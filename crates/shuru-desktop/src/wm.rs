//! The window manager: one thread that owns the X connection, manages
//! windows, draws the strip, panel and overlays, and answers commands
//! from the API.

mod manage;
mod overlay;

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, NONE};

use crate::apps::App;
use crate::chrome::{self, BORDER, MARGIN, RIPPLE, STRIP};
use crate::draw::{self, Canvas, Font};

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_TAKE_FOCUS,
        WM_STATE,
        UTF8_STRING,
        _NET_SUPPORTED,
        _NET_SUPPORTING_WM_CHECK,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CLOSE_WINDOW,
        _NET_NUMBER_OF_DESKTOPS,
        _NET_CURRENT_DESKTOP,
        _NET_DESKTOP_GEOMETRY,
        _NET_DESKTOP_VIEWPORT,
        _NET_WORKAREA,
        _NET_FRAME_EXTENTS,
        _NET_REQUEST_FRAME_EXTENTS,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_DESKTOP,
        _NET_WM_STATE,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_FOCUSED,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_WINDOW_TYPE_TOOLBAR,
        _NET_WM_WINDOW_TYPE_SPLASH,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_MENU,
        _NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
        _NET_WM_WINDOW_TYPE_POPUP_MENU,
        _NET_WM_WINDOW_TYPE_TOOLTIP,
        _NET_WM_WINDOW_TYPE_NOTIFICATION,
        _NET_WM_WINDOW_TYPE_COMBO,
        _NET_WM_WINDOW_TYPE_DND,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    Agent,
    Person,
}

type Reply = oneshot::Sender<Result<Value, (u16, String)>>;

pub enum Cmd {
    Windows(Reply),
    Focus(u32, Reply),
    Close(u32, Reply),
    Launch {
        app: String,
        args: Vec<String>,
        timeout: Duration,
        reply: Reply,
    },
    Overlay {
        who: Option<Option<Who>>,
        ripple: Option<(i16, i16)>,
        reply: Reply,
    },
    Exited {
        pid: u32,
        status: String,
        ok: bool,
    },
}

/// The last time the screen changed, for `/settle`.
pub struct Settle {
    start: Instant,
    last_ms: AtomicU64,
}

impl Settle {
    fn changed(&self) {
        self.last_ms
            .store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// How long the screen has been still.
    pub fn quiet(&self) -> Duration {
        let now = self.start.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_ms.load(Ordering::Relaxed)))
    }
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::Sender<Cmd>,
    wake: Arc<OwnedFd>,
}

impl Handle {
    fn send(&self, cmd: Cmd) -> bool {
        if self.tx.send(cmd).is_err() {
            return false;
        }
        let one = 1u64.to_ne_bytes();
        unsafe { libc::write(self.wake.as_raw_fd(), one.as_ptr().cast(), 8) };
        true
    }

    pub async fn call(&self, make: impl FnOnce(Reply) -> Cmd) -> Result<Value, (u16, String)> {
        let (tx, rx) = oneshot::channel();
        if !self.send(make(tx)) {
            return Err((503, "the window manager has stopped".into()));
        }
        rx.await
            .unwrap_or_else(|_| Err((503, "the window manager has stopped".into())))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Normal,
    Dialog,
    Utility,
    Splash,
    Dock,
    Popup,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Normal => "normal",
            Kind::Dialog => "dialog",
            Kind::Utility => "utility",
            Kind::Splash => "splash",
            Kind::Dock => "dock",
            Kind::Popup => "popup",
        }
    }

    /// Kinds that take the screen and the keyboard.
    fn is_app(self) -> bool {
        matches!(self, Kind::Normal | Kind::Dialog | Kind::Utility)
    }
}

struct Client {
    instance: String,
    class: String,
    title: String,
    pid: Option<u32>,
    kind: Kind,
    parent: Option<Window>,
    delete: bool,
    take_focus: bool,
    input: bool,
    fullscreen: bool,
    /// The configured app it belongs to.
    app: Option<usize>,
    /// Windows with the same key are one app.
    key: String,
    geom: (i16, i16, u16, u16, u16),
}

struct Pending {
    app: App,
    pid: u32,
    deadline: Instant,
    reply: Option<Reply>,
}

type R = (i16, i16, u16, u16);

fn within(inner: R, outer: R) -> bool {
    inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.0 as i32 + inner.2 as i32 <= outer.0 as i32 + outer.2 as i32
        && inner.1 as i32 + inner.3 as i32 <= outer.1 as i32 + outer.3 as i32
}

/// Whether `pid` is `ancestor` or runs under it.
fn descends(pid: u32, ancestor: u32) -> bool {
    let mut p = pid;
    for _ in 0..16 {
        if p == ancestor {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else {
            return false;
        };
        // The fields after the name, which is in parentheses.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            return false;
        };
        match rest.split_whitespace().nth(1).and_then(|v| v.parse().ok()) {
            Some(pp) if pp > 1 => p = pp,
            _ => return false,
        }
    }
    false
}

struct Ours {
    check: Window,
    strip: Window,
    panel: Window,
    badge: Window,
    border: [Window; 4],
    cursor: Window,
    ripple: Window,
    gc: Gcontext,
}

pub struct Wm {
    conn: RustConnection,
    display: String,
    root: Window,
    depth: u8,
    sw: u16,
    sh: u16,
    atoms: Atoms,
    ours: Ours,
    font: Font,
    apps: Vec<App>,
    clients: HashMap<Window, Client>,
    /// Managed windows, most recently focused first.
    mru: Vec<Window>,
    /// Managed windows in the order they mapped.
    order: Vec<Window>,
    focused: Option<Window>,
    /// Override redirect windows of apps that are mapped.
    foreign: Vec<Window>,
    who: Option<Who>,
    strip_shown: bool,
    panel_shown: bool,
    panel_rect: R,
    panel_spans: Vec<(f32, f32)>,
    panel_targets: Vec<PanelTarget>,
    panel_hover: Option<usize>,
    badge_rect: R,
    badge_done: Option<(f32, f32)>,
    cursor_ok: bool,
    cursor_rect: R,
    cursor_hot: (i16, i16),
    cursor_moved: bool,
    /// Warps make no raw motion, so the pointer is also looked at now and
    /// then.
    cursor_poll: Instant,
    cursor_px: Option<Canvas>,
    cursor_shown: bool,
    ripple_person: bool,
    ripple_until: Option<Instant>,
    ripple_rect: R,
    /// Where our windows were lately, so the damage they leave is not
    /// taken for a change of the screen.
    recent: Vec<(Instant, R)>,
    pending: Vec<Pending>,
    settle: Arc<Settle>,
    rx: mpsc::Receiver<Cmd>,
    handle: Handle,
    wake: Arc<OwnedFd>,
    alt_tab: Option<(Vec<Window>, usize)>,
    keys: Keys,
}

#[derive(Default)]
struct Keys {
    tab: Option<Keycode>,
    alts: Vec<Keycode>,
}

enum PanelTarget {
    App(usize),
    Key(String),
}

pub fn start(display: &str, apps: Vec<App>) -> Result<(Handle, Arc<Settle>)> {
    let (conn, screen_num) =
        RustConnection::connect(Some(display)).map_err(|e| anyhow!("X display {display}: {e}"))?;
    let screen = conn.setup().roots[screen_num].clone();
    let root = screen.root;
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(
            EventMask::SUBSTRUCTURE_REDIRECT
                | EventMask::SUBSTRUCTURE_NOTIFY
                | EventMask::PROPERTY_CHANGE,
        ),
    )?
    .check()
    .map_err(|e| match e {
        ReplyError::X11Error(_) => anyhow!("another window manager runs on {display}"),
        e => anyhow!(e),
    })?;
    let atoms = Atoms::new(&conn)?.reply()?;
    conn.damage_query_version(1, 1)?.reply().context("DAMAGE")?;
    conn.shape_query_version()?.reply().context("SHAPE")?;
    let cursor_ok = conn
        .xfixes_query_version(4, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some()
        && conn
            .xinput_xi_query_version(2, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();

    let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if efd < 0 {
        bail!("eventfd: {}", std::io::Error::last_os_error());
    }
    let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(efd) });
    let (tx, rx) = mpsc::channel();
    let handle = Handle {
        tx,
        wake: wake.clone(),
    };
    let settle = Arc::new(Settle {
        start: Instant::now(),
        last_ms: AtomicU64::new(0),
    });

    let ours = make_ours(&conn, &screen)?;
    let mut wm = Wm {
        conn,
        display: display.to_string(),
        root,
        depth: screen.root_depth,
        sw: screen.width_in_pixels,
        sh: screen.height_in_pixels,
        atoms,
        ours,
        font: Font::new(),
        apps,
        clients: HashMap::new(),
        mru: Vec::new(),
        order: Vec::new(),
        focused: None,
        foreign: Vec::new(),
        who: None,
        strip_shown: false,
        panel_shown: false,
        panel_rect: (0, 0, 0, 0),
        panel_spans: Vec::new(),
        panel_targets: Vec::new(),
        panel_hover: None,
        badge_rect: (0, 0, 0, 0),
        badge_done: None,
        cursor_ok,
        cursor_rect: (0, 0, 0, 0),
        cursor_hot: (0, 0),
        cursor_moved: false,
        cursor_poll: Instant::now(),
        cursor_px: None,
        cursor_shown: false,
        ripple_person: false,
        ripple_until: None,
        ripple_rect: (0, 0, 0, 0),
        recent: Vec::new(),
        pending: Vec::new(),
        settle: settle.clone(),
        rx,
        handle: handle.clone(),
        wake,
        alt_tab: None,
        keys: Keys::default(),
    };
    wm.setup()?;
    std::thread::Builder::new()
        .name("wm".into())
        .spawn(move || {
            if let Err(e) = wm.run() {
                eprintln!("shuru-desktop: {e:#}");
                std::process::exit(1);
            }
        })?;
    Ok((handle, settle))
}

/// Our own windows: override redirect, kept out of the client list.
fn make_ours(conn: &RustConnection, screen: &Screen) -> Result<Ours> {
    let root = screen.root;
    let make = |mask: EventMask, bg: u32| -> Result<Window> {
        let w = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            w,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(bg)
                .event_mask(mask),
        )?;
        Ok(w)
    };
    let clicks = EventMask::EXPOSURE
        | EventMask::BUTTON_PRESS
        | EventMask::POINTER_MOTION
        | EventMask::LEAVE_WINDOW;
    let ours = Ours {
        check: make(EventMask::NO_EVENT, draw::BG)?,
        strip: make(EventMask::EXPOSURE, draw::STRIP_BG)?,
        panel: make(clicks, draw::BG)?,
        badge: make(
            EventMask::EXPOSURE | EventMask::BUTTON_PRESS,
            draw::STRIP_BG,
        )?,
        border: [
            make(EventMask::NO_EVENT, draw::AMBER)?,
            make(EventMask::NO_EVENT, draw::AMBER)?,
            make(EventMask::NO_EVENT, draw::AMBER)?,
            make(EventMask::NO_EVENT, draw::AMBER)?,
        ],
        cursor: make(EventMask::EXPOSURE, 0)?,
        ripple: make(EventMask::EXPOSURE, draw::BG)?,
        gc: conn.generate_id()?,
    };
    conn.create_gc(ours.gc, root, &CreateGCAux::new().graphics_exposures(0))?;
    // Overlays let every click through.
    for w in [ours.cursor, ours.ripple].into_iter().chain(ours.border) {
        conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            w,
            0,
            0,
            &[],
        )?;
    }
    Ok(ours)
}

/// The value of a property, or nothing for a window that is gone.
fn prop(conn: &RustConnection, win: Window, atom: Atom) -> Option<GetPropertyReply> {
    conn.get_property(false, win, atom, AtomEnum::ANY, 0, 4096)
        .ok()?
        .reply()
        .ok()
        .filter(|r| r.type_ != NONE)
}

fn prop32(conn: &RustConnection, win: Window, atom: Atom) -> Vec<u32> {
    prop(conn, win, atom)
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default()
}

impl Wm {
    fn setup(&mut self) -> Result<()> {
        let (conn, root, a) = (&self.conn, self.root, &self.atoms);
        let font = conn.generate_id()?;
        conn.open_font(font, b"cursor")?;
        let arrow = conn.generate_id()?;
        // XC_left_ptr.
        conn.create_glyph_cursor(arrow, font, font, 68, 69, 0, 0, 0, 0xffff, 0xffff, 0xffff)?;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new()
                .background_pixel(draw::BG)
                .cursor(arrow),
        )?;
        conn.clear_area(false, root, 0, 0, 0, 0)?;

        let check = self.ours.check;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            &[check],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            check,
            a._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            &[check],
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            check,
            a._NET_WM_NAME,
            a.UTF8_STRING,
            b"shuru-desktop",
        )?;
        let supported = [
            a._NET_SUPPORTED,
            a._NET_SUPPORTING_WM_CHECK,
            a._NET_CLIENT_LIST,
            a._NET_CLIENT_LIST_STACKING,
            a._NET_ACTIVE_WINDOW,
            a._NET_CLOSE_WINDOW,
            a._NET_NUMBER_OF_DESKTOPS,
            a._NET_CURRENT_DESKTOP,
            a._NET_DESKTOP_GEOMETRY,
            a._NET_DESKTOP_VIEWPORT,
            a._NET_WORKAREA,
            a._NET_FRAME_EXTENTS,
            a._NET_REQUEST_FRAME_EXTENTS,
            a._NET_WM_NAME,
            a._NET_WM_PID,
            a._NET_WM_DESKTOP,
            a._NET_WM_STATE,
            a._NET_WM_STATE_FULLSCREEN,
            a._NET_WM_STATE_FOCUSED,
            a._NET_WM_WINDOW_TYPE,
            a._NET_WM_WINDOW_TYPE_NORMAL,
            a._NET_WM_WINDOW_TYPE_DIALOG,
            a._NET_WM_WINDOW_TYPE_UTILITY,
            a._NET_WM_WINDOW_TYPE_SPLASH,
            a._NET_WM_WINDOW_TYPE_DOCK,
        ];
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_SUPPORTED,
            AtomEnum::ATOM,
            &supported,
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_NUMBER_OF_DESKTOPS,
            AtomEnum::CARDINAL,
            &[1],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_CURRENT_DESKTOP,
            AtomEnum::CARDINAL,
            &[0],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_DESKTOP_VIEWPORT,
            AtomEnum::CARDINAL,
            &[0, 0],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_DESKTOP_GEOMETRY,
            AtomEnum::CARDINAL,
            &[self.sw as u32, self.sh as u32],
        )?;
        let (x, y, w, h) = chrome::work_area(self.sw, self.sh);
        conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_WORKAREA,
            AtomEnum::CARDINAL,
            &[x as u32, y as u32, w as u32, h as u32],
        )?;

        let damage = conn.generate_id()?;
        conn.damage_create(damage, root, damage::ReportLevel::RAW_RECTANGLES)?;

        if self.cursor_ok {
            conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
            conn.xinput_xi_select_events(
                root,
                &[xinput::EventMask {
                    // All devices: raw events of XTEST come from its slave.
                    deviceid: 0,
                    mask: vec![
                        xinput::XIEventMask::RAW_MOTION | xinput::XIEventMask::RAW_BUTTON_PRESS,
                    ],
                }],
            )?;
            conn.xfixes_hide_cursor(root)?;
        } else {
            eprintln!("shuru-desktop: no XFixes 4 or XInput 2, the cursor overlay is off");
        }
        self.grab_keys()?;
        let conn = &self.conn;

        let (sx, sy) = (MARGIN, MARGIN);
        let sw = self.sw - 2 * MARGIN as u16;
        conn.configure_window(
            self.ours.strip,
            &ConfigureWindowAux::new()
                .x(sx as i32)
                .y(sy as i32)
                .width(sw as u32)
                .height(STRIP as u32),
        )?;
        let t = BORDER as u32;
        let (w, h) = (self.sw as u32, self.sh as u32);
        let edges = [
            (0, 0, w, t),
            (0, h as i32 - t as i32, w, t),
            (0, 0, t, h),
            (w as i32 - t as i32, 0, t, h),
        ];
        for (win, (x, y, ew, eh)) in self.ours.border.iter().zip(edges) {
            conn.configure_window(
                *win,
                &ConfigureWindowAux::new().x(x).y(y).width(ew).height(eh),
            )?;
        }
        conn.configure_window(
            self.ours.ripple,
            &ConfigureWindowAux::new()
                .width(RIPPLE as u32)
                .height(RIPPLE as u32),
        )?;
        let (_, rm) = chrome::ripple(false);
        self.shape(self.ours.ripple, &rm)?;

        // Windows that were there before us.
        let tree = self.conn.query_tree(root)?.reply()?;
        for w in tree.children {
            let Ok(attr) = self.conn.get_window_attributes(w)?.reply() else {
                continue;
            };
            if attr.map_state != MapState::VIEWABLE || self.is_ours(w) {
                continue;
            }
            if attr.override_redirect {
                self.foreign.push(w);
            } else {
                self.manage(w)?;
            }
        }
        if self.cursor_ok {
            self.update_cursor_image()?;
        }
        self.chrome()?;
        self.conn.flush()?;
        Ok(())
    }
}
