//! A normal X11 client with retained layout, input and accessibility.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{mpsc as sync, Arc};

use accesskit::{Action, ActionRequest, NodeId, TreeId, TreeInfo, TreeUpdate};
use anyhow::{bail, Result};
use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

use super::Preferences;
use crate::draw::{self, Canvas, Font, Rect};
use crate::ui::widgets::Paint;
use crate::ui::Tree;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING, WM_PROTOCOLS, WM_DELETE_WINDOW,
        _NET_WM_NAME, _NET_WM_PID, _NET_WM_WINDOW_TYPE, _NET_WM_WINDOW_TYPE_NORMAL, _NET_ACTIVE_WINDOW,
    }
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
            "SuperHQ Settings".into(),
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
        let atoms = Atoms::new(&conn)?.reply()?;
        let window = conn.generate_id()?;
        let gc = conn.generate_id()?;
        let preferences = Preferences::load();
        draw::set_dark(preferences.dark);
        let (width, height) = (680, 700);
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            width,
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
        conn.change_property8(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_NAME,
            atoms.UTF8_STRING,
            b"SuperHQ Settings",
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"SuperHQ Settings",
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
        let tree = preferences.tree(&mut font, width as f32);
        conn.map_window(window)?;
        conn.flush()?;
        Ok(Self {
            conn,
            window,
            root,
            gc,
            depth,
            atoms,
            width,
            height,
            font,
            preferences,
            tree,
            trees,
            actions,
            wake,
            watch: crate::appearance::Watch::new(),
            origin: (0, 0),
            focused: false,
            last: None,
        })
    }

    fn view(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    fn rebuild(&mut self) {
        let selected = self.tree.focused;
        let scroll = self.tree.scroll;
        self.tree = self.preferences.tree(&mut self.font, self.width as f32);
        self.tree.scroll = scroll.min((self.tree.size().1 - self.height as f32).max(0.0));
        if let Some(row) = selected {
            self.tree.select(Some(row), self.height as f32);
        }
    }

    fn action(&mut self, id: u64, action: Action) -> Result<()> {
        let Some(row) = self.tree.row_of(id) else {
            return Ok(());
        };
        if !self.tree.supports(row, action) {
            return Ok(());
        }
        let activate = ClientMessageEvent::new(
            32,
            self.window,
            self.atoms._NET_ACTIVE_WINDOW,
            [2, CURRENT_TIME, 0, 0, 0],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            activate,
        )?;
        match action {
            Action::Click => {
                if let Err(e) = self.preferences.click(id) {
                    self.preferences.status = e.to_string();
                }
                draw::set_dark(self.preferences.dark);
                self.rebuild();
                self.tree.select(Some(row), self.height as f32);
            }
            Action::Focus => {
                self.tree.pointer = false;
                self.tree.select(Some(row), self.height as f32);
            }
            _ => {}
        }
        Ok(())
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
        let (mut nodes, focus) = self.tree.accesskit(view, self.origin);
        if let Some((_, root)) = nodes.iter_mut().find(|(id, _)| *id == NodeId(1)) {
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
                tree: Some(TreeInfo::new(NodeId(1))),
                tree_id: TreeId::ROOT,
                focus: if self.focused { focus } else { NodeId(1) },
            },
            focused: self.focused,
        };
        if self.last.as_ref() != Some(&snapshot) {
            let _ = self.trees.send(snapshot.clone());
            self.last = Some(snapshot);
        }
        self.conn.flush()?;
        Ok(())
    }

    fn key(&mut self, event: KeyPressEvent) -> Result<()> {
        let mapping = self.conn.get_keyboard_mapping(event.detail, 1)?.reply()?;
        let key = mapping.keysyms.first().copied().unwrap_or(0);
        match key {
            0xff09 | 0xff54 => self.tree.step(
                u16::from(event.state) & u16::from(KeyButMask::SHIFT) == 0,
                self.height as f32,
            ),
            0xfe20 | 0xff52 => self.tree.step(false, self.height as f32),
            0xff0d | 0x20 => {
                if let Some(id) = self.tree.selected_id() {
                    self.action(id, Action::Click)?;
                }
            }

            _ => {}
        }
        Ok(())
    }

    fn run(mut self) -> Result<()> {
        self.paint()?;
        loop {
            let mut changed = false;
            while let Some(event) = self.conn.poll_for_event()? {
                changed = true;
                match event {
                    Event::ClientMessage(e)
                        if e.type_ == self.atoms.WM_PROTOCOLS
                            && e.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW =>
                    {
                        return Ok(())
                    }
                    Event::DestroyNotify(e) if e.window == self.window => return Ok(()),
                    Event::ConfigureNotify(e) => {
                        self.width = e.width.max(1);
                        self.height = e.height.max(1);
                        let translated = self
                            .conn
                            .translate_coordinates(self.window, self.root, 0, 0)?
                            .reply()?;
                        self.origin = (translated.dst_x, translated.dst_y);
                        self.rebuild();
                    }
                    Event::ButtonPress(e) => match e.detail {
                        1 => {
                            if let Some(id) =
                                self.tree
                                    .hit_node(self.view(), e.event_x as f32, e.event_y as f32)
                            {
                                self.action(id, Action::Click)?;
                            }
                        }
                        4 | 5 => {
                            self.tree.scroll = (self.tree.scroll
                                + if e.detail == 4 { -48.0 } else { 48.0 })
                            .clamp(0.0, (self.tree.size().1 - self.height as f32).max(0.0));
                        }
                        _ => {}
                    },
                    Event::MotionNotify(e) => {
                        let row = self
                            .tree
                            .hit(self.view(), e.event_x as f32, e.event_y as f32)
                            .filter(|r| self.tree.supports(*r, Action::Click));
                        self.tree.pointer = true;
                        self.tree.select(row, self.height as f32);
                    }
                    Event::LeaveNotify(_) => {
                        self.tree.pointer = true;
                        self.tree.select(None, self.height as f32);
                    }
                    Event::KeyPress(e) => self.key(e)?,
                    Event::FocusIn(_) => self.focused = true,
                    Event::FocusOut(_) => self.focused = false,
                    _ => {}
                }
            }
            while let Ok(request) = self.actions.try_recv() {
                self.action(request.target_node.0, request.action)?;
                changed = true;
            }
            if self.watch.as_ref().is_some_and(|w| w.drain()) {
                self.preferences.dark = crate::appearance::read();
                draw::set_dark(self.preferences.dark);
                self.rebuild();
                changed = true;
            }
            if changed {
                self.paint()?;
            }
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
