//! The Settings window: X11 input, painting and the AT-SPI bridge.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{mpsc as sync, Arc};

use accesskit::{Action, ActionData, ActionRequest, NodeId, TreeId, TreeInfo, TreeUpdate};
use anyhow::{bail, Result};
use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

use super::{Preferences, ROOT};
use crate::draw::{self, Canvas, Font, Rect};
use crate::locale::{tr, Msg};
use crate::ui::widgets::Paint;
use crate::ui::{BarHit, Thumb, Tree};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING, WM_PROTOCOLS, WM_DELETE_WINDOW,
        _NET_WM_NAME, _NET_WM_PID, _NET_WM_WINDOW_TYPE, _NET_WM_WINDOW_TYPE_NORMAL, _NET_ACTIVE_WINDOW,
    }
}

const WIDTH: u16 = 640;
const MOST_HEIGHT: u16 = 720;
/// Room the panel, our title bar and a margin take from the screen.
const SCREEN_ROOM: u16 = 32 + 46 + 32;
/// One click of the wheel or one arrow press on the scrollbar.
const STEP: f32 = 48.0;

mod key {
    pub const TAB: u32 = 0xff09;
    pub const LEFT_TAB: u32 = 0xfe20;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const HOME: u32 = 0xff50;
    pub const END: u32 = 0xff57;
    pub const RETURN: u32 = 0xff0d;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const SPACE: u32 = 0x20;
    pub const ESCAPE: u32 = 0xff1b;
}

pub(super) fn run(display: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (trees, tree_rx) = mpsc::unbounded_channel();
    let (actions, mut action_rx) = mpsc::unbounded_channel::<ActionRequest>();
    let (tx, rx) = sync::channel();
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        bail!("Settings eventfd: {}", std::io::Error::last_os_error());
    }
    let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
    let notify = wake.clone();
    let display = display.to_string();
    runtime.block_on(async move {
        let publisher = tokio::spawn(crate::atspi::run(
            tr(Msg::Settings).into(),
            tree_rx,
            actions,
        ));
        let forwarder = tokio::spawn(async move {
            while let Some(request) = action_rx.recv().await {
                if tx.send(request).is_err() {
                    break;
                }
                let one = 1u64.to_ne_bytes();
                unsafe { libc::write(notify.as_raw_fd(), one.as_ptr().cast(), 8) };
            }
        });
        let out =
            tokio::task::spawn_blocking(move || Client::open(&display, trees, rx, wake)?.run())
                .await?;
        forwarder.abort();
        publisher.abort();
        out
    })
}

/// The keysyms of each keycode, read once.
struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    fn read(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Self {
            min,
            per: reply.keysyms_per_keycode as usize,
            syms: reply.keysyms,
        })
    }

    fn sym(&self, code: u8) -> u32 {
        let i = code.saturating_sub(self.min) as usize * self.per;
        self.syms.get(i).copied().unwrap_or(0)
    }
}

/// What a paint shows, to skip painting when nothing changed.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    scroll: f32,
    hovered: Option<u64>,
    selected: Option<usize>,
    pointer: bool,
    thumb: Thumb,
}

struct Client {
    conn: RustConnection,
    window: Window,
    root: Window,
    gc: Gcontext,
    depth: u8,
    atoms: Atoms,
    width: u16,
    height: u16,
    font: Font,
    keymap: Keymap,
    preferences: Preferences,
    tree: Tree,
    trees: mpsc::UnboundedSender<crate::atspi::Snapshot>,
    actions: sync::Receiver<ActionRequest>,
    wake: Arc<OwnedFd>,
    watch: Option<crate::appearance::Watch>,
    origin: (i16, i16),
    focused: bool,
    last: Option<crate::atspi::Snapshot>,
}

impl Client {
    fn open(
        display: &str,
        trees: mpsc::UnboundedSender<crate::atspi::Snapshot>,
        actions: sync::Receiver<ActionRequest>,
        wake: Arc<OwnedFd>,
    ) -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(Some(display))?;
        let screen = &conn.setup().roots[screen_num];
        let (root, depth) = (screen.root, screen.root_depth);
        let height = MOST_HEIGHT.min(screen.height_in_pixels.saturating_sub(SCREEN_ROOM).max(240));
        let atoms = Atoms::new(&conn)?.reply()?;
        let window = conn.generate_id()?;
        let gc = conn.generate_id()?;
        let preferences = Preferences::load();
        draw::set_dark(preferences.dark);
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            WIDTH,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(draw::theme().bg)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::KEY_PRESS
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::LEAVE_WINDOW
                        | EventMask::FOCUS_CHANGE,
                ),
        )?;
        conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"superhq-settings\0SuperHQSettings\0",
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &[atoms.WM_DELETE_WINDOW],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_PID,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[atoms._NET_WM_WINDOW_TYPE_NORMAL],
        )?;
        let mut font = Font::new();
        let tree = preferences.tree(&mut font, WIDTH as f32);
        let keymap = Keymap::read(&conn)?;
        let client = Self {
            conn,
            window,
            root,
            gc,
            depth,
            atoms,
            width: WIDTH,
            height,
            font,
            keymap,
            preferences,
            tree,
            trees,
            actions,
            wake,
            watch: crate::appearance::Watch::new(),
            origin: (0, 0),
            focused: false,
            last: None,
        };
        client.set_title()?;
        client.conn.map_window(window)?;
        client.conn.flush()?;
        Ok(client)
    }

    fn set_title(&self) -> Result<()> {
        let title = tr(Msg::Settings).as_bytes();
        self.conn.change_property8(
            PropMode::REPLACE,
            self.window,
            self.atoms._NET_WM_NAME,
            self.atoms.UTF8_STRING,
            title,
        )?;
        self.conn.change_property8(
            PropMode::REPLACE,
            self.window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title,
        )?;
        Ok(())
    }

    fn view(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    fn h(&self) -> f32 {
        self.height as f32
    }

    fn look(&self) -> Look {
        Look {
            scroll: self.tree.scroll,
            hovered: self.tree.hovered,
            selected: self.tree.selected,
            pointer: self.tree.pointer,
            thumb: self.tree.thumb,
        }
    }

    /// Builds the tree again, keeping the scroll and the focused control,
    /// or moving the focus to `focus`.
    fn rebuild(&mut self, focus: Option<u64>) {
        let keep = focus.or_else(|| self.tree.selected_id());
        let (scroll, pointer) = (self.tree.scroll, self.tree.pointer);
        self.tree = self.preferences.tree(&mut self.font, self.width as f32);
        self.tree.scroll_to(scroll, self.h());
        self.tree.pointer = pointer;
        if let Some(row) = keep.and_then(|id| self.tree.row_of(id)) {
            let height = self.h();
            if focus.is_some() {
                self.tree.select(Some(row), height);
            } else {
                // The same row, without scrolling to it.
                self.tree.selected = Some(row);
                self.tree.focused = Some(row);
            }
        }
    }

    fn activate(&mut self) -> Result<()> {
        let message = ClientMessageEvent::new(
            32,
            self.window,
            self.atoms._NET_ACTIVE_WINDOW,
            [2, CURRENT_TIME, 0, 0, 0],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            message,
        )?;
        Ok(())
    }

    fn click(&mut self, id: u64) {
        if !self.tree.node_supports(id, Action::Click) {
            return;
        }
        let focus = self.preferences.click(id).or(Some(id));
        draw::set_dark(self.preferences.dark);
        self.rebuild(focus);
    }

    /// An action from an assistive tool.
    fn action(&mut self, request: ActionRequest) -> Result<()> {
        let id = request.target_node.0;
        let h = self.h();
        match request.action {
            Action::Click => {
                self.activate()?;
                self.tree.pointer = false;
                self.click(id);
            }
            Action::Focus | Action::ScrollIntoView => {
                if self.tree.node_supports(id, request.action) {
                    if request.action == Action::Focus {
                        self.activate()?;
                    }
                    self.tree.pointer = false;
                    let row = self.tree.row_of(id);
                    self.tree.select(row, h);
                }
            }
            Action::SetValue if id == self.tree.scrollbar_id() => {
                if let Some(ActionData::NumericValue(v)) = request.data {
                    self.tree.scroll_to(v as f32, h);
                }
            }
            Action::ScrollDown if id == ROOT => self.tree.scroll_to(self.tree.scroll + h, h),
            Action::ScrollUp if id == ROOT => self.tree.scroll_to(self.tree.scroll - h, h),
            _ => {}
        }
        Ok(())
    }

    fn key(&mut self, event: KeyPressEvent) {
        let shift = u16::from(event.state) & u16::from(KeyButMask::SHIFT) != 0;
        let h = self.h();
        let row = self.tree.focused;
        match self.keymap.sym(event.detail) {
            key::TAB if !shift => self.tree.step(true, h),
            key::TAB | key::LEFT_TAB => self.tree.step(false, h),
            key::DOWN => self.tree.step(true, h),
            key::UP => self.tree.step(false, h),
            k @ (key::LEFT | key::RIGHT) => {
                if let Some(next) = row.and_then(|r| self.tree.radio_step(r, k == key::RIGHT)) {
                    self.tree.pointer = false;
                    self.click(next);
                }
            }
            key::RETURN | key::KP_ENTER | key::SPACE => {
                if let Some(id) = self.tree.selected_id() {
                    self.tree.pointer = false;
                    self.click(id);
                }
            }
            key::ESCAPE => {
                if let Some(open) = self.preferences.open {
                    self.tree.pointer = false;
                    self.click(open);
                }
            }
            key::PAGE_DOWN => self.tree.scroll_to(self.tree.scroll + h - STEP, h),
            key::PAGE_UP => self.tree.scroll_to(self.tree.scroll - h + STEP, h),
            key::HOME => self.tree.scroll_to(0.0, h),
            key::END => self.tree.scroll_to(f32::MAX, h),
            _ => {}
        }
    }

    fn press(&mut self, e: ButtonPressEvent) {
        let (x, y, h) = (e.event_x as f32, e.event_y as f32, self.h());
        match e.detail {
            1 => match self.tree.bar_hit(self.view(), x, y) {
                Some(BarHit::Thumb(grab)) => self.tree.thumb = Thumb::Held(grab),
                Some(BarHit::Track(down)) => {
                    let page = if down { h - STEP } else { STEP - h };
                    self.tree.scroll_to(self.tree.scroll + page, h);
                }
                None => {
                    if let Some(id) = self.tree.hit_node(self.view(), x, y) {
                        self.tree.pointer = true;
                        self.click(id);
                    }
                }
            },
            4 => self.tree.scroll_to(self.tree.scroll - STEP, h),
            5 => self.tree.scroll_to(self.tree.scroll + STEP, h),
            _ => {}
        }
    }

    fn motion(&mut self, x: f32, y: f32) {
        let view = self.view();
        if let Thumb::Held(grab) = self.tree.thumb {
            self.tree.drag_thumb(view, y, grab);
            return;
        }
        self.tree.thumb = match self.tree.bar_hit(view, x, y) {
            Some(BarHit::Thumb(_)) => Thumb::Hover,
            _ => Thumb::Rest,
        };
        let over_bar = self.tree.thumb != Thumb::Rest;
        self.tree.hovered = self.tree.hit_node(view, x, y).filter(|_| !over_bar);
        let row = self
            .tree
            .hit(view, x, y)
            .filter(|&r| !over_bar && self.tree.actionable(r));
        if row.is_some() || self.tree.pointer {
            self.tree.pointer = true;
            self.tree.selected = row;
        }
    }

    fn paint(&mut self) -> Result<()> {
        let view = self.view();
        let mut c = Canvas::new(self.width as usize, self.height as usize, draw::theme().bg);
        self.tree.paint(
            &mut c,
            &mut Paint {
                font: &mut self.font,
                icons: &|_| None,
                field: None,
            },
            view,
        );
        let bytes = c.bytes();
        let rows = (200_000 / (c.w * 4)).max(1);
        for y in (0..c.h).step_by(rows) {
            let count = rows.min(c.h - y);
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                self.window,
                self.gc,
                self.width,
                count as u16,
                0,
                y as i16,
                0,
                self.depth,
                &bytes[y * c.w * 4..(y + count) * c.w * 4],
            )?;
        }
        self.publish();
        self.conn.flush()?;
        Ok(())
    }

    /// Sends the tree to the accessibility bus when it changed.
    fn publish(&mut self) {
        let view = self.view();
        let (mut nodes, focus) = self.tree.accesskit(view, self.origin);
        if let Some((_, root)) = nodes.iter_mut().find(|(id, _)| *id == NodeId(ROOT)) {
            root.set_bounds(accesskit::Rect::new(
                self.origin.0 as f64,
                self.origin.1 as f64,
                self.origin.0 as f64 + self.width as f64,
                self.origin.1 as f64 + self.height as f64,
            ));
        }
        let snapshot = crate::atspi::Snapshot {
            tree: TreeUpdate {
                nodes,
                tree: Some(TreeInfo::new(NodeId(ROOT))),
                tree_id: TreeId::ROOT,
                focus: if self.focused { focus } else { NodeId(ROOT) },
            },
            focused: self.focused,
        };
        if self.last.as_ref() != Some(&snapshot) {
            let _ = self.trees.send(snapshot.clone());
            self.last = Some(snapshot);
        }
    }

    /// Follows a change of the settings files, made here or elsewhere.
    fn settings_changed(&mut self, changes: crate::appearance::Changes) -> Result<()> {
        if changes.appearance {
            self.preferences.dark = crate::appearance::read();
            draw::set_dark(self.preferences.dark);
        }
        if changes.language {
            crate::locale::reload();
            self.preferences.lang = crate::locale::lang();
            self.set_title()?;
        }
        if changes.do_not_disturb {
            self.preferences.quiet = crate::dnd::read();
        }
        self.rebuild(None);
        Ok(())
    }

    fn run(mut self) -> Result<()> {
        self.paint()?;
        loop {
            let before = self.look();
            let mut changed = false;
            while let Some(event) = self.conn.poll_for_event()? {
                match event {
                    Event::ClientMessage(e)
                        if e.type_ == self.atoms.WM_PROTOCOLS
                            && e.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW =>
                    {
                        return Ok(())
                    }
                    Event::DestroyNotify(e) if e.window == self.window => return Ok(()),
                    Event::Expose(_) => changed = true,
                    Event::ConfigureNotify(e) => {
                        let at = self
                            .conn
                            .translate_coordinates(self.window, self.root, 0, 0)?
                            .reply()?;
                        self.origin = (at.dst_x, at.dst_y);
                        if (e.width, e.height) != (self.width, self.height) {
                            self.width = e.width.max(1);
                            self.height = e.height.max(1);
                            self.rebuild(None);
                            changed = true;
                        }
                    }
                    Event::ButtonPress(e) => {
                        self.press(e);
                        changed = true;
                    }
                    Event::ButtonRelease(e) => {
                        if matches!(self.tree.thumb, Thumb::Held(_)) {
                            self.tree.thumb = Thumb::Rest;
                            self.motion(e.event_x as f32, e.event_y as f32);
                        }
                    }
                    Event::MotionNotify(e) => self.motion(e.event_x as f32, e.event_y as f32),
                    Event::LeaveNotify(_) => {
                        if !matches!(self.tree.thumb, Thumb::Held(_)) {
                            self.tree.thumb = Thumb::Rest;
                            self.tree.hovered = None;
                            if self.tree.pointer {
                                self.tree.selected = None;
                            }
                        }
                    }
                    Event::KeyPress(e) => {
                        self.key(e);
                        changed = true;
                    }
                    Event::FocusIn(_) => {
                        self.focused = true;
                        self.publish();
                    }
                    Event::FocusOut(_) => {
                        self.focused = false;
                        self.publish();
                    }
                    _ => {}
                }
            }
            while let Ok(request) = self.actions.try_recv() {
                self.action(request)?;
                changed = true;
            }
            if let Some(changes) = self.watch.as_ref().map(|w| w.drain()) {
                if changes.any() {
                    self.settings_changed(changes)?;
                    changed = true;
                }
            }
            if changed || self.look() != before {
                self.paint()?;
            }
            self.conn.flush()?;
            let mut fds = [
                libc::pollfd {
                    fd: self.conn.stream().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.watch.as_ref().map_or(-1, |w| w.fd()),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e.into());
                }
            }
            if fds[1].revents != 0 {
                let mut value = [0u8; 8];
                unsafe { libc::read(self.wake.as_raw_fd(), value.as_mut_ptr().cast(), 8) };
            }
        }
    }
}
