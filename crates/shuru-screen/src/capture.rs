// Derived from voxl (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo,
// under the MIT license: see LICENSE-voxl.

//! X11 capture: MIT-SHM GetImage into a frame of our own, driven by
//! XDamage. The server copies the pixels between requests, so a frame
//! never holds half of one draw and half of the next. Reading the
//! framebuffer of Xvfb in place (`-shmem`) would race its drawing.

use std::os::fd::{AsFd, BorrowedFd};
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::convert::I420;

struct ShmBuf {
    ptr: *mut u8,
    len: usize,
    shmid: i32,
}

unsafe impl Send for ShmBuf {}

impl ShmBuf {
    fn new(len: usize) -> Result<Self> {
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                bail!("shmget: {}", std::io::Error::last_os_error());
            }
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                let e = std::io::Error::last_os_error();
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                bail!("shmat: {e}");
            }
            Ok(ShmBuf {
                ptr: ptr as *mut u8,
                len,
                shmid,
            })
        }
    }

    fn as_slice(&self) -> &[u8] {
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

impl Band {
    pub fn union(a: Option<Band>, b: Band) -> Band {
        match a {
            None => b,
            Some(a) => Band {
                y0: a.y0.min(b.y0),
                y1: a.y1.max(b.y1),
            },
        }
    }
}

pub struct Capturer {
    conn: RustConnection,
    root: u32,
    pub width: u32,
    pub height: u32,
    stride: usize,
    seg: u32,
    buf: ShmBuf,
    pub damaged: bool,
    /// When the first damage since the last take came in.
    pub damaged_at: Option<Instant>,
    /// Rows touched since the last take.
    acc: Option<(u32, u32)>,
    /// Macroblocks (16x16) touched since the last take, row major.
    mb_dirty: Vec<u8>,
    mb_cols: usize,
    /// The screen changed size: capture must be set up again.
    pub resized: bool,
}

impl Capturer {
    pub fn new(display: &str) -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(Some(display))
            .map_err(|e| anyhow!("X display {display}: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        // Even sizes for 4:2:0; an odd last row or column is left out.
        let width = screen.width_in_pixels as u32 & !1;
        let height = screen.height_in_pixels as u32 & !1;
        if screen.root_depth != 24 && screen.root_depth != 32 {
            bail!("unsupported root depth {}", screen.root_depth);
        }
        conn.extension_information(shm::X11_EXTENSION_NAME)?
            .ok_or_else(|| anyhow!("the X server has no MIT-SHM"))?;
        conn.shm_query_version()?.reply()?;
        conn.damage_query_version(1, 1)?.reply()?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.randr_query_version(1, 2)?.reply()?;
        conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)?;
        let stride = width as usize * 4;
        let seg = conn.generate_id()?;
        let buf = ShmBuf::new(stride * height as usize)?;
        conn.shm_attach(seg, buf.shmid as u32, false)?.check()?;
        // Removed once both sides detach.
        unsafe { libc::shmctl(buf.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        let damage = conn.generate_id()?;
        conn.damage_create(damage, root, damage::ReportLevel::RAW_RECTANGLES)?;
        conn.flush()?;
        Ok(Capturer {
            conn,
            root,
            width,
            height,
            stride,
            seg,
            buf,
            damaged: true,
            damaged_at: None,
            acc: None,
            mb_dirty: vec![0; (width as usize).div_ceil(16) * (height as usize).div_ceil(16)],
            mb_cols: (width as usize).div_ceil(16),
            resized: false,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// Takes in the X events queued so far.
    pub fn pump(&mut self) -> Result<()> {
        while let Some(ev) = self.conn.poll_for_event()? {
            let e = match ev {
                Event::DamageNotify(e) => e,
                Event::RandrScreenChangeNotify(e) => {
                    if (e.width as u32 & !1, e.height as u32 & !1) != (self.width, self.height) {
                        self.resized = true;
                    }
                    continue;
                }
                _ => continue,
            };
            let top = e.area.y.max(0) as u32;
            let bot = (e.area.y as i32 + e.area.height as i32).clamp(0, self.height as i32) as u32;
            if bot <= top {
                continue;
            }
            let left = e.area.x.max(0) as u32;
            let right = (e.area.x as i32 + e.area.width as i32).clamp(0, self.width as i32) as u32;
            let (x0, x1) = (left as usize / 16, (right as usize).div_ceil(16));
            for row in (top as usize / 16)..(bot as usize).div_ceil(16) {
                self.mb_dirty[row * self.mb_cols + x0..row * self.mb_cols + x1].fill(1);
            }
            self.acc = Some(match self.acc {
                Some((a, b)) => (a.min(top), b.max(bot)),
                None => (top, bot),
            });
            self.damaged = true;
            self.damaged_at.get_or_insert_with(Instant::now);
        }
        Ok(())
    }

    /// The rows damaged since the last take. Raw rectangles carry their
    /// areas, so this never waits on the X server.
    pub fn take_damage(&mut self) -> Option<Band> {
        self.damaged = false;
        self.damaged_at = None;
        let (y0, y1) = self.acc.take()?;
        // Even rows for 4:2:0.
        Some(Band {
            y0: y0 & !1,
            y1: ((y1 + 1) & !1).min(self.height),
        })
    }

    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let n = self.mb_dirty.len();
        std::mem::replace(&mut self.mb_dirty, vec![0; n])
    }

    pub fn mb_cols(&self) -> usize {
        self.mb_cols
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    pub fn convert(&self, dst: &mut I420, band: Band, threads: usize) {
        crate::convert::convert_band(
            self.pixels(),
            self.stride,
            dst,
            band.y0 as usize,
            band.y1 as usize,
            threads,
        );
    }

    /// The screen now as packed BGRX. The server is grabbed for the copy,
    /// a few ms, so nothing draws into the pixels while they are read.
    pub fn snapshot(&mut self) -> Result<Vec<u8>> {
        self.conn.grab_server()?;
        let shot = self.snapshot_grabbed();
        self.conn.ungrab_server()?;
        self.conn.flush()?;
        shot
    }

    fn snapshot_grabbed(&mut self) -> Result<Vec<u8>> {
        // Damage for everything drawn before the grab comes ahead of this reply.
        self.conn.get_input_focus()?.reply()?;
        self.pump()?;
        self.grab(self.full())?;
        let (row, h) = (self.width as usize * 4, self.height as usize);
        let mut out = Vec::with_capacity(row * h);
        for line in self.pixels().chunks_exact(self.stride).take(h) {
            out.extend_from_slice(&line[..row]);
        }
        Ok(out)
    }

    /// The cursor and where it is, for stills. When the server refuses
    /// the image, `last` at the pointer.
    pub fn cursor(
        &self,
        last: Option<crate::cursor::CursorImage>,
    ) -> Option<crate::cursor::CursorImage> {
        if let Ok(c) = crate::cursor::CursorImage::fetch(&self.conn) {
            return Some(c);
        }
        let mut c = last?;
        let p = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        (c.x, c.y) = (p.root_x as i32, p.root_y as i32);
        Some(c)
    }

    fn pixels(&self) -> &[u8] {
        &self.buf.as_slice()[..self.stride * self.height as usize]
    }

    /// Fetches a full width band into the shm frame.
    pub fn grab(&self, band: Band) -> Result<()> {
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
            )?
            .reply()?;
        Ok(())
    }
}
