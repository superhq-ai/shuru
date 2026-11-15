// Vendored from voxl 0.3.5 (https://github.com/tembo/voxl), Copyright (c)
// 2026 Tembo, under the MIT license: see LICENSE-voxl. Pinned to v0.3.5.

//! X11 screen capture: MIT-SHM GetImage into a persistent BGRX frame, driven by XDamage.

use std::os::fd::{AsFd, BorrowedFd};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

pub static DAMAGE_EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct ShmBuf {
    pub ptr: *mut u8,
    pub len: usize,
    shmid: i32,
}

unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn new(len: usize) -> Result<Self, String> {
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return Err("shmget failed".into());
            }
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                return Err("shmat failed".into());
            }
            Ok(ShmBuf {
                ptr: ptr as *mut u8,
                len,
                shmid,
            })
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.ptr as *const _);
            libc::shmctl(self.shmid, libc::IPC_RMID, std::ptr::null_mut());
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

pub struct Capturer {
    pub conn: RustConnection,
    root: u32,
    pub width: u32,
    pub height: u32,
    resize_pending: bool,
    pub stride: usize,
    seg: u32,
    buf: ShmBuf,
    /// Kept for the connection's lifetime; raw-rectangle events need no further requests.
    _damage: u32,
    pub damaged: bool,
    /// When the first damage since the last take arrived.
    pub damaged_at: Option<std::time::Instant>,
    /// Rows touched since the last take (raw damage rectangles, no server round trip).
    acc: Option<(u32, u32)>,
    /// Macroblocks (16x16) touched since the last take, row-major.
    mb_dirty: Vec<u8>,
    pub mb_cols: usize,
    /// Areas (x0, y0, x1, y1) changed since the video thread last took them, for `/wait`.
    pub changes: Vec<(u32, u32, u32, u32)>,
}

impl Capturer {
    pub fn new(display: Option<&str>) -> Result<Self, String> {
        let (conn, screen_num) =
            RustConnection::connect(display).map_err(|e| format!("X connect: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        // Subscribe before querying geometry so a resize during initialization is not lost.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| e.to_string())?;
        let geometry = conn
            .get_geometry(root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let width = u32::from(geometry.width);
        let height = u32::from(geometry.height);
        if screen.root_depth != 24 && screen.root_depth != 32 {
            return Err(format!("unsupported root depth {}", screen.root_depth));
        }
        conn.extension_information(shm::X11_EXTENSION_NAME)
            .map_err(|e| e.to_string())?
            .ok_or("no MIT-SHM")?;
        conn.shm_query_version()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.damage_query_version(1, 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let stride = width as usize * 4;
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        // shuru divergence (upstream candidate): always copy the screen with MIT-SHM
        // GetImage into a frame of our own, never voxl's `direct` in-place read of the
        // Xvfb `-shmem` framebuffer. Reading the live framebuffer races the server's
        // drawing and tears (~25% of frames at 60 Hz); the extra copy is cheap and the
        // result never holds half of one draw and half of the next. Worth proposing to
        // voxl as an opt-out of direct mode.
        let buf = ShmBuf::new(stride * height as usize)?;
        conn.shm_attach(seg, buf.shmid as u32, false)
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())?;
        // Mark for removal now; it stays alive while attached.
        unsafe { libc::shmctl(buf.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        log::info!("capturing via MIT-SHM GetImage");
        let damage = conn.generate_id().map_err(|e| e.to_string())?;
        conn.damage_create(damage, root, damage::ReportLevel::RAW_RECTANGLES)
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        Ok(Capturer {
            conn,
            root,
            width,
            height,
            resize_pending: false,
            stride,
            seg,
            buf,
            _damage: damage,
            damaged: true,
            damaged_at: None,
            acc: None,
            mb_dirty: vec![0; (width as usize).div_ceil(16) * (height as usize).div_ceil(16)],
            mb_cols: (width as usize).div_ceil(16),
            changes: Vec::new(),
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// Drain queued X events; returns true if damage was reported.
    pub fn pump(&mut self) -> Result<(), String> {
        while let Some(ev) = self.conn.poll_for_event().map_err(|e| e.to_string())? {
            if let Event::ConfigureNotify(e) = &ev {
                if e.window == self.root
                    && (u32::from(e.width), u32::from(e.height)) != (self.width, self.height)
                {
                    self.resize_pending = true;
                }
            }
            if let Event::DamageNotify(e) = ev {
                DAMAGE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if std::env::var("VOXL_DEBUG_DAMAGE").is_ok() {
                    log::info!(
                        "dmg {:?} x{} y{} {}x{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_micros()
                            % 1_000_000,
                        e.area.x,
                        e.area.y,
                        e.area.width,
                        e.area.height
                    );
                }
                let top = (e.area.y as i32).clamp(0, self.height as i32) as u32;
                let bot =
                    (e.area.y as i32 + e.area.height as i32).clamp(0, self.height as i32) as u32;
                if bot > top {
                    let left = (e.area.x as i32).clamp(0, self.width as i32) as u32;
                    let right =
                        (e.area.x as i32 + e.area.width as i32).clamp(0, self.width as i32) as u32;
                    if right <= left {
                        continue;
                    }
                    let (x0, x1) = (left as usize / 16, (right as usize).div_ceil(16));
                    for row in (top as usize / 16)..(bot as usize).div_ceil(16) {
                        self.mb_dirty[row * self.mb_cols + x0..row * self.mb_cols + x1].fill(1);
                    }
                    self.changes.push((left, top, right, bot));
                    self.acc = Some(match self.acc {
                        Some((a, b)) => (a.min(top), b.max(bot)),
                        None => (top, bot),
                    });
                    self.damaged = true;
                    self.damaged_at.get_or_insert_with(std::time::Instant::now);
                }
            }
        }
        Ok(())
    }

    /// Take accumulated damage as a row band. Raw-rectangle reporting delivers the
    /// areas in the events themselves, so this never waits on the X server.
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.damaged = false;
        self.damaged_at = None;
        let Some((y0, y1)) = self.acc.take() else {
            return Ok(None);
        };
        // Align to even rows for 4:2:0.
        Ok(Some(Band {
            y0: y0 & !1,
            y1: ((y1 + 1) & !1).min(self.height),
        }))
    }

    /// Take the dirty-macroblock map accumulated with the current damage.
    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let n = self.mb_dirty.len();
        std::mem::replace(&mut self.mb_dirty, vec![0; n])
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    /// Convert rows [y0, y1) of the current frame into `dst`.
    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        crate::convert::convert_band(self.pixels(), self.stride, dst, y0, y1, threads);
    }

    /// The current frame as packed BGRX (`width * 4` bytes per row), for screenshots,
    /// with the damage for exactly these pixels taken into `changes`. Nothing draws while
    /// the server is grabbed (a few ms for the copy), so a repaint can't land in the
    /// pixels without its damage, or after them with it.
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        self.conn.grab_server().map_err(|e| e.to_string())?;
        let shot = self.snapshot_grabbed();
        // Release the server even if the copy failed.
        self.conn.ungrab_server().map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())?;
        shot
    }

    fn snapshot_grabbed(&mut self) -> Result<Vec<u8>, String> {
        // Damage for everything drawn before the grab arrives ahead of this reply.
        self.conn
            .get_input_focus()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        self.pump()?;
        self.grab(self.full())?;
        let (row, h) = (self.width as usize * 4, self.height as usize);
        let mut out = Vec::with_capacity(row * h);
        for line in self.pixels().chunks_exact(self.stride).take(h) {
            out.extend_from_slice(&line[..row]);
        }
        Ok(out)
    }

    /// The current frame as BGRX pixels and their stride (debug marker probe).
    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        Some((self.pixels(), self.stride))
    }

    /// Whether a root configure event changed the capture dimensions.
    pub fn needs_rebuild(&self) -> bool {
        self.resize_pending
    }

    /// A resize can race a GetImage request before its configure event is processed.
    pub fn geometry_changed(&self) -> bool {
        self.conn
            .get_geometry(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|g| (u32::from(g.width), u32::from(g.height)) != (self.width, self.height))
    }

    /// Current frame pixels (BGRX, `stride` bytes per row).
    fn pixels(&self) -> &[u8] {
        &self.buf.as_slice()[..self.stride * self.height as usize]
    }

    /// Grab a full-width band into the shm frame at its natural offset.
    pub fn grab(&self, band: Band) -> Result<(), String> {
        let h = band.y1 - band.y0;
        if h == 0 {
            return Ok(());
        }
        let off = band.y0 as usize * self.stride;
        self.conn
            .shm_get_image(
                self.root,
                0,
                band.y0 as i16,
                self.width as u16,
                h as u16,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                self.seg,
                off as u32,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
