//! The window manager: one thread that owns the X connection, frames and
//! stacks windows, draws the panel, the rail, menus and overlays, and
//! answers commands from the API.

mod input;
mod manage;
mod menu;
mod notes;
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
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::errors::ReplyError;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::appearance;
use crate::apps::App;
use crate::bus;
use crate::chrome::{self, Light, PanelHit, PanelLayout, RAIL_W, RIPPLE};
use crate::draw::{self, Font, Image};
use crate::wallpaper;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_TAKE_FOCUS,
        WM_STATE,
        MANAGER,
        _XSETTINGS_S0,
        _XSETTINGS_SETTINGS,
        WM_CHANGE_STATE,
        UTF8_STRING,
        CLIPBOARD,
        INCR,
        TARGETS,
        SHURU_PASTE,
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
        _NET_WM_MOVERESIZE,
        _NET_WM_STATE,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_FOCUSED,
        _NET_WM_STATE_ABOVE,
        COMPOUND_TEXT,
        _NET_WM_STATE_DEMANDS_ATTENTION,
        _NET_WM_STATE_MODAL,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_ALLOWED_ACTIONS,
        _NET_WM_ACTION_MOVE,
        _NET_WM_ACTION_RESIZE,
        _NET_WM_ACTION_MINIMIZE,
        _NET_WM_ACTION_MAXIMIZE_HORZ,
        _NET_WM_ACTION_MAXIMIZE_VERT,
        _NET_WM_ACTION_FULLSCREEN,
        _NET_WM_ACTION_CLOSE,
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
        _MOTIF_WM_HINTS,
        _GTK_FRAME_EXTENTS,
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
    Tree(Reply),
    TreeAction {
        id: u64,
        action: String,
        value: Option<String>,
        reply: Reply,
    },
    Focus {
        id: u32,
        maximize: bool,
        reply: Reply,
    },
    Close(u32, Reply),
    Launch {
        app: String,
        args: Vec<String>,
        timeout: Duration,
        maximize: bool,
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
    /// Reads the appearance, or sets it to dark or light first.
    Appearance(Option<bool>, Reply),
    /// Reads the wallpaper, or sets it to a named picture first.
    Wallpaper(Option<String>, Reply),
    Notify(Box<bus::Note>, Reply),
    CloseNote(u32, Reply),
    Notifications(Reply),
    NoteAction {
        id: u32,
        key: String,
        reply: Reply,
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

    /// Kinds that take the keyboard.
    fn is_app(self) -> bool {
        matches!(self, Kind::Normal | Kind::Dialog | Kind::Utility)
    }

    /// Kinds we put in a frame.
    fn framed(self) -> bool {
        !matches!(self, Kind::Dock | Kind::Popup)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Maximized,
    Floating,
}

/// A rectangle of the screen.
type R = (i16, i16, u16, u16);

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
    /// The app entry it belongs to.
    app: Option<usize>,
    /// Windows with the same key are one app.
    key: String,
    framed: bool,
    frame: Window,
    /// The resize band round a floating frame.
    grip: Window,
    corners: Vec<(chrome::Corner, Window)>,
    /// The ring over a client that draws past its card.
    edges: Vec<Window>,
    /// The shadow a client draws round its card, as _GTK_FRAME_EXTENTS
    /// gives it: left, right, top, bottom.
    shadow: [u16; 4],
    /// Whether we draw its title bar.
    bar: bool,
    place: Place,
    /// Floating in a half of the work area.
    snapped: bool,
    minimized: bool,
    /// The visible window, our bar included and the ring not.
    card: R,
    /// The card for the size it asked for when it mapped.
    natural: (u16, u16),
    /// Where a floating window goes back to.
    floating: Option<R>,
    min: (u16, u16),
    /// The largest client size it takes, 0 for no limit.
    max: (u16, u16),
    /// The size steps it takes from a base size, as WM_NORMAL_HINTS give
    /// them.
    base: (u16, u16),
    inc: (u16, u16),
    /// Kept over the other windows.
    above: bool,
    /// Asks for attention, by _NET_WM_STATE or the urgency hint.
    attention: bool,
    /// Blocks input to its parent while it is open.
    modal: bool,
    /// Unmap events we caused and do not mean a withdraw.
    unmaps: u32,
    hover: bool,
    pressed: Option<Light>,
}

impl Client {
    fn bar_h(&self) -> u16 {
        if self.bar && !self.fullscreen {
            chrome::BAR_H
        } else {
            0
        }
    }

    /// A top level window of an app, as opposed to its dialogs.
    fn is_main(&self) -> bool {
        self.kind == Kind::Normal && self.parent.is_none()
    }
}

/// An app of the rail: a configured one, or one found for a window.
struct Entry {
    app: App,
    pinned: bool,
    small: Option<Image>,
    large: Option<Image>,
}

struct Pending {
    app: App,
    pid: u32,
    deadline: Instant,
    maximize: bool,
    reply: Option<Reply>,
}

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
    panel: Window,
    rail: Window,
    empty: Window,
    tooltip: Window,
    pop: Window,
    snap: Window,
    cursor: Window,
    ripple: Window,
    notes: [Window; notes::SHOWN],
    gc: Gcontext,
}

/// Pointer cursors from the cursor theme.
struct Cursors {
    arrow: Cursor,
    moving: Cursor,
    /// Over the controls of the panel and the rail.
    pointer: Cursor,
    /// Over the search field.
    text: Cursor,
    /// By edge: left, right, top, bottom, then the corners clockwise from
    /// the top left.
    sides: [Cursor; 8],
}

/// What the rail shows: an app entry, or windows of no known app.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    App(usize),
    Key(String),
}

pub struct Wm {
    conn: RustConnection,
    display: String,
    root: Window,
    depth: u8,
    visual: Visualid,
    sw: u16,
    sh: u16,
    atoms: Atoms,
    ours: Ours,
    cursors: Cursors,
    /// The cursor each of our windows was last given.
    cursor_of: HashMap<Window, Cursor>,
    /// The cursor of the pointer grab a menu holds.
    grab_cursor: Cursor,
    font: Font,
    entries: Vec<Entry>,
    /// Classes looked up in the XDG dirs, and what was found.
    looked_up: HashMap<String, Option<usize>>,
    clients: HashMap<Window, Client>,
    /// Frames of clients, by frame and by corner piece.
    frames: HashMap<Window, Window>,
    /// Managed windows, most recently focused first.
    mru: Vec<Window>,
    /// Managed windows in the order they mapped.
    order: Vec<Window>,
    /// Framed windows from the bottom of the stack up.
    stack: Vec<Window>,
    focused: Option<Window>,
    /// Override redirect windows of apps that are mapped.
    foreign: Vec<Window>,
    who: Option<Who>,
    bar_trees: HashMap<Window, crate::ui::Tree>,
    panel_tree: Option<crate::ui::Tree>,
    rail_tree: Option<crate::ui::Tree>,
    panel_layout: PanelLayout,
    panel_hover: Option<PanelHit>,
    rail_targets: Vec<Target>,
    rail_hover: Option<usize>,
    rail_h: u16,
    tooltip_for: Option<usize>,
    tooltip_rect: R,
    empty_shown: bool,
    empty_rect: R,
    pop: Option<menu::Pop>,
    clipboard: String,
    drag: Option<input::Drag>,
    snap_rect: Option<R>,
    last_bar_click: Option<(Window, Timestamp)>,
    cursor_ok: bool,
    cursor_rect: R,
    cursor_hot: (i16, i16),
    cursor_moved: bool,
    /// Warps make no raw motion, so the pointer is also looked at now and
    /// then.
    cursor_poll: Instant,
    /// When the pointer last moved or the server last said anything; the
    /// polling slows down a while after, so an idle desktop rarely wakes.
    cursor_active: Instant,
    cursor_px: Option<draw::Canvas>,
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
    /// Alt+Tab or Alt+`: the windows it walks and where it is.
    switching: Option<(Vec<Window>, usize)>,
    keys: input::Keys,
    /// The appearance setting, watched.
    watch: Option<appearance::Watch>,
    /// The wallpaper setting and the picture it names, read once.
    wallpaper: Option<wallpaper::Choice>,
    wall: Option<draw::Canvas>,
    xsettings_serial: u32,
    notes: notes::Notes,
    last_accessibility: Option<accesskit::TreeUpdate>,
    bus: Option<tokio::sync::mpsc::UnboundedSender<bus::Event>>,
}

pub fn start(
    display: &str,
    apps: Vec<App>,
    bus: tokio::sync::mpsc::UnboundedSender<bus::Event>,
) -> Result<(Handle, Arc<Settle>)> {
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

    draw::set_dark(appearance::read());
    let ours = make_ours(&conn, &screen)?;
    let cursors = make_cursors(&conn, screen_num)?;
    let mut entries: Vec<_> = apps.into_iter().map(|app| Entry::new(app, true)).collect();
    if !entries.iter().any(|e| e.app.id == "settings") {
        if let Ok(app) = crate::apps::load("settings") {
            entries.push(Entry::new(app, false));
        }
    }
    let mut wm = Wm {
        conn,
        display: display.to_string(),
        root,
        depth: screen.root_depth,
        visual: screen.root_visual,
        sw: screen.width_in_pixels,
        sh: screen.height_in_pixels,
        atoms,
        ours,
        cursors,
        cursor_of: HashMap::new(),
        grab_cursor: NONE,
        font: Font::new(),
        entries,
        looked_up: HashMap::new(),
        clients: HashMap::new(),
        frames: HashMap::new(),
        mru: Vec::new(),
        order: Vec::new(),
        stack: Vec::new(),
        focused: None,
        foreign: Vec::new(),
        who: None,
        bar_trees: HashMap::new(),
        panel_tree: None,
        rail_tree: None,
        panel_layout: PanelLayout::default(),
        panel_hover: None,
        rail_targets: Vec::new(),
        rail_hover: None,
        rail_h: 0,
        tooltip_for: None,
        tooltip_rect: (0, 0, 0, 0),
        empty_shown: false,
        empty_rect: (0, 0, 0, 0),
        pop: None,
        clipboard: String::new(),
        drag: None,
        snap_rect: None,
        last_bar_click: None,
        cursor_ok,
        cursor_rect: (0, 0, 0, 0),
        cursor_hot: (0, 0),
        cursor_moved: false,
        cursor_poll: Instant::now(),
        cursor_active: Instant::now(),
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
        switching: None,
        keys: input::Keys::default(),
        watch: appearance::Watch::new(),
        wallpaper: None,
        wall: None,
        xsettings_serial: 0,
        notes: notes::Notes::new(crate::dnd::read()),
        bus: Some(bus),
        last_accessibility: None,
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

impl Entry {
    fn new(app: App, pinned: bool) -> Entry {
        let icon = |size| app.icon.as_deref().and_then(|i| crate::icon::load(i, size));
        Entry {
            small: icon(16),
            large: icon(32),
            app,
            pinned,
        }
    }
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
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::LEAVE_WINDOW;
    let ours = Ours {
        check: make(EventMask::NO_EVENT, draw::theme().bg)?,
        panel: make(clicks, draw::theme().bg)?,
        rail: make(clicks, draw::theme().bg)?,
        empty: make(EventMask::EXPOSURE, draw::theme().bg)?,
        tooltip: make(EventMask::EXPOSURE, draw::theme().raised)?,
        pop: make(
            clicks | EventMask::KEY_PRESS | EventMask::PROPERTY_CHANGE,
            draw::theme().surface,
        )?,
        snap: make(EventMask::EXPOSURE, draw::theme().snap)?,
        cursor: make(EventMask::EXPOSURE, 0)?,
        ripple: make(EventMask::EXPOSURE, draw::theme().bg)?,
        notes: [
            make(
                EventMask::EXPOSURE | EventMask::BUTTON_PRESS,
                draw::theme().surface,
            )?,
            make(
                EventMask::EXPOSURE | EventMask::BUTTON_PRESS,
                draw::theme().surface,
            )?,
            make(
                EventMask::EXPOSURE | EventMask::BUTTON_PRESS,
                draw::theme().surface,
            )?,
        ],
        gc: conn.generate_id()?,
    };
    conn.create_gc(ours.gc, root, &CreateGCAux::new().graphics_exposures(0))?;
    // Overlays let every click through.
    for w in [
        ours.cursor,
        ours.ripple,
        ours.tooltip,
        ours.snap,
        ours.empty,
    ] {
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

/// The cursor theme when neither XCURSOR_THEME nor Xcursor.theme names one.
const CURSOR_THEME: &str = "Adwaita";

fn make_cursors(conn: &RustConnection, screen: usize) -> Result<Cursors> {
    if std::env::var_os("XCURSOR_THEME").is_none() {
        std::env::set_var("XCURSOR_THEME", CURSOR_THEME);
    }
    let db = x11rb::resource_manager::new_from_default(conn)?;
    let themed = x11rb::cursor::Handle::new(conn, screen, &db)?.reply().ok();
    let font = conn.generate_id()?;
    conn.open_font(font, b"cursor")?;
    // A cursor by its theme names, else the cursor font glyph.
    let load = |names: &[&str], glyph: u16| -> Result<Cursor> {
        if let Some(h) = &themed {
            for name in names {
                let c = h.load_cursor(conn, name)?;
                if c != NONE {
                    return Ok(c);
                }
            }
        }
        let c = conn.generate_id()?;
        conn.create_glyph_cursor(
            c,
            font,
            font,
            glyph,
            glyph + 1,
            0,
            0,
            0,
            0xffff,
            0xffff,
            0xffff,
        )?;
        Ok(c)
    };
    Ok(Cursors {
        arrow: load(&["default", "left_ptr"], 68)?,
        moving: load(&["move", "fleur"], 52)?,
        pointer: load(&["pointer", "hand2"], 60)?,
        text: load(&["text", "xterm"], 152)?,
        sides: [
            load(&["w-resize", "left_side"], 70)?,
            load(&["e-resize", "right_side"], 96)?,
            load(&["n-resize", "top_side"], 138)?,
            load(&["s-resize", "bottom_side"], 16)?,
            load(&["nw-resize", "top_left_corner"], 134)?,
            load(&["ne-resize", "top_right_corner"], 136)?,
            load(&["se-resize", "bottom_right_corner"], 14)?,
            load(&["sw-resize", "bottom_left_corner"], 12)?,
        ],
    })
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

fn shadow_of(conn: &RustConnection, win: Window, atom: Atom) -> [u16; 4] {
    let v = prop32(conn, win, atom);
    let mut out = [0; 4];
    if v.len() == 4 {
        for (o, x) in out.iter_mut().zip(v) {
            *o = x.min(64) as u16;
        }
    }
    out
}

impl Wm {
    fn setup(&mut self) -> Result<()> {
        let (conn, root, a) = (&self.conn, self.root, &self.atoms);
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new()
                .background_pixel(draw::theme().bg)
                .cursor(self.cursors.arrow),
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
            a._NET_WM_MOVERESIZE,
            a._NET_WM_STATE,
            a._NET_WM_STATE_FULLSCREEN,
            a._NET_WM_STATE_FOCUSED,
            a._NET_WM_STATE_ABOVE,
            a._NET_WM_STATE_DEMANDS_ATTENTION,
            a._NET_WM_STATE_MODAL,
            a._NET_WM_STATE_HIDDEN,
            a._NET_WM_STATE_MAXIMIZED_VERT,
            a._NET_WM_STATE_MAXIMIZED_HORZ,
            a._NET_WM_ALLOWED_ACTIONS,
            a._NET_WM_ACTION_MOVE,
            a._NET_WM_ACTION_RESIZE,
            a._NET_WM_ACTION_MINIMIZE,
            a._NET_WM_ACTION_MAXIMIZE_HORZ,
            a._NET_WM_ACTION_MAXIMIZE_VERT,
            a._NET_WM_ACTION_FULLSCREEN,
            a._NET_WM_ACTION_CLOSE,
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

        self.fit_screen()?;
        if self
            .conn
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_some()
        {
            self.conn
                .randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)?;
        }
        self.conn.configure_window(
            self.ours.ripple,
            &ConfigureWindowAux::new()
                .width(RIPPLE as u32)
                .height(RIPPLE as u32),
        )?;
        let (_, rm) = chrome::ripple(false);
        self.shape(self.ours.ripple, &rm)?;
        self.conn.map_window(self.ours.panel)?;
        self.conn.map_window(self.ours.rail)?;

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
        self.conn
            .set_screen_saver(0, 0, Blanking::NOT_PREFERRED, Exposures::DEFAULT)?;
        let (xs, manager) = (self.atoms._XSETTINGS_S0, self.atoms.MANAGER);
        self.conn
            .set_selection_owner(self.ours.check, xs, CURRENT_TIME)?;
        let ev =
            ClientMessageEvent::new(32, root, manager, [CURRENT_TIME, xs, self.ours.check, 0, 0]);
        self.conn
            .send_event(false, root, EventMask::STRUCTURE_NOTIFY, ev)?;
        self.publish_appearance()?;
        self.set_wallpaper(wallpaper::read())?;
        self.chrome()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Sizes the panel, the rail and the empty desktop to the screen and
    /// publishes its geometry and work area.
    pub(super) fn fit_screen(&mut self) -> Result<()> {
        let (root, a) = (self.root, &self.atoms);
        self.conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_DESKTOP_GEOMETRY,
            AtomEnum::CARDINAL,
            &[self.sw as u32, self.sh as u32],
        )?;
        let (x, y, w, h) = chrome::work_area(self.sw, self.sh);
        self.conn.change_property32(
            PropMode::REPLACE,
            root,
            a._NET_WORKAREA,
            AtomEnum::CARDINAL,
            &[x as u32, y as u32, w as u32, h as u32],
        )?;
        let panel_h = chrome::PANEL_H;
        self.rail_h = self.sh.saturating_sub(panel_h);
        self.conn.configure_window(
            self.ours.panel,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(self.sw as u32)
                .height(panel_h as u32),
        )?;
        self.conn.configure_window(
            self.ours.rail,
            &ConfigureWindowAux::new()
                .x(0)
                .y(panel_h as i32)
                .width(RAIL_W as u32)
                .height(self.rail_h as u32),
        )?;
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        self.empty_rect = (wx, wy + wh as i16 / 2 - 10, ww, 20);
        let e = self.empty_rect;
        self.conn.configure_window(
            self.ours.empty,
            &ConfigureWindowAux::new()
                .x(e.0 as i32)
                .y(e.1 as i32)
                .width(e.2 as u32)
                .height(e.3 as u32),
        )?;
        Ok(())
    }
}
