//! Managing windows: placing, focusing, closing, and the API commands.

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::{descends, prop, prop32, Client, Cmd, Kind, Pending, Who, Wm};
use crate::apps::{self, App};
use crate::chrome;

const CURSOR_POLL: Duration = Duration::from_millis(100);

impl Wm {
    pub(super) fn run(mut self) -> Result<()> {
        loop {
            let mut busy = true;
            while busy {
                busy = self.events()?;
                while let Ok(cmd) = self.rx.try_recv() {
                    self.command(cmd)?;
                    busy = true;
                }
                self.timers()?;
                if self.cursor_moved {
                    self.cursor_moved = false;
                    self.move_cursor()?;
                    busy = true;
                }
            }
            self.conn.flush()?;
            let timeout = self.next_deadline().map_or(-1, |d| {
                d.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(60_000) as i32
                    + 1
            });
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
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    bail!("poll: {e}");
                }
            }
            if fds[1].revents != 0 {
                let mut buf = [0u8; 8];
                unsafe { libc::read(self.wake.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
            }
        }
    }

    fn events(&mut self) -> Result<bool> {
        let mut any = false;
        while let Some(ev) = self.conn.poll_for_event()? {
            any = true;
            self.event(ev)?;
        }
        Ok(any)
    }

    fn event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::MapRequest(e) => self.manage(e.window)?,
            Event::ConfigureRequest(e) => self.configure_request(&e)?,
            Event::UnmapNotify(e) if e.event == self.root => self.unmanage(e.window)?,
            Event::DestroyNotify(e) => self.unmanage(e.window)?,
            Event::MapNotify(e) if e.event == self.root => {
                if e.override_redirect
                    && !self.is_ours(e.window)
                    && !self.foreign.contains(&e.window)
                {
                    self.foreign.push(e.window);
                    self.raise_overlays()?;
                }
            }
            Event::PropertyNotify(e) => {
                let names = [self.atoms._NET_WM_NAME, u32::from(AtomEnum::WM_NAME)];
                if names.contains(&e.atom) && self.clients.contains_key(&e.window) {
                    let title = self.title(e.window);
                    if let Some(c) = self.clients.get_mut(&e.window) {
                        c.title = title;
                    }
                    if self.focused == Some(e.window) {
                        self.draw_strip()?;
                    }
                }
            }
            Event::ClientMessage(e) => self.client_message(&e)?,
            Event::Expose(e) if e.count == 0 => self.redraw(e.window)?,
            Event::ButtonPress(e) => self.button(&e)?,
            Event::MotionNotify(e) if e.event == self.ours.panel => {
                let hover = chrome::panel_hit(&self.panel_spans, e.event_x as f32);
                if hover != self.panel_hover {
                    self.panel_hover = hover;
                    self.draw_panel()?;
                }
            }
            Event::LeaveNotify(e) if e.event == self.ours.panel => {
                if self.panel_hover.take().is_some() {
                    self.draw_panel()?;
                }
            }
            Event::KeyPress(e) => self.key_press(&e)?,
            Event::KeyRelease(e) => {
                if self.alt_tab.is_some() && self.keys.alts.contains(&e.detail) {
                    self.alt_tab = None;
                    self.conn.ungrab_keyboard(CURRENT_TIME)?;
                }
            }
            Event::DamageNotify(e) => {
                let r = (e.area.x, e.area.y, e.area.width, e.area.height);
                if !self.own_damage(r) {
                    self.settle.changed();
                }
            }
            Event::XfixesCursorNotify(_) => self.update_cursor_image()?,
            Event::XinputRawMotion(_) => self.cursor_moved = true,
            Event::XinputRawButtonPress(e)
                if (1..=3).contains(&e.detail) && self.who != Some(Who::Person) =>
            {
                let pointer = self.conn.query_pointer(self.root)?.reply();
                if let Ok(p) = pointer {
                    self.show_ripple(p.root_x, p.root_y, false)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn is_ours(&self, w: Window) -> bool {
        let o = &self.ours;
        [o.check, o.strip, o.panel, o.badge, o.cursor, o.ripple].contains(&w)
            || o.border.contains(&w)
    }

    fn title(&self, w: Window) -> String {
        if let Some(r) = prop(&self.conn, w, self.atoms._NET_WM_NAME) {
            return String::from_utf8_lossy(&r.value).into_owned();
        }
        prop(&self.conn, w, AtomEnum::WM_NAME.into())
            .map(|r| r.value.iter().map(|&b| b as char).collect())
            .unwrap_or_default()
    }

    fn kind_of(&self, types: &[u32], transient: bool) -> Kind {
        let a = &self.atoms;
        for t in types {
            if *t == a._NET_WM_WINDOW_TYPE_NORMAL {
                break;
            }
            if *t == a._NET_WM_WINDOW_TYPE_DIALOG {
                return Kind::Dialog;
            }
            if [a._NET_WM_WINDOW_TYPE_UTILITY, a._NET_WM_WINDOW_TYPE_TOOLBAR].contains(t) {
                return Kind::Utility;
            }
            if *t == a._NET_WM_WINDOW_TYPE_SPLASH {
                return Kind::Splash;
            }
            if [a._NET_WM_WINDOW_TYPE_DOCK, a._NET_WM_WINDOW_TYPE_DESKTOP].contains(t) {
                return Kind::Dock;
            }
            let popups = [
                a._NET_WM_WINDOW_TYPE_MENU,
                a._NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
                a._NET_WM_WINDOW_TYPE_POPUP_MENU,
                a._NET_WM_WINDOW_TYPE_TOOLTIP,
                a._NET_WM_WINDOW_TYPE_NOTIFICATION,
                a._NET_WM_WINDOW_TYPE_COMBO,
                a._NET_WM_WINDOW_TYPE_DND,
            ];
            if popups.contains(t) {
                return Kind::Popup;
            }
        }
        if transient {
            Kind::Dialog
        } else {
            Kind::Normal
        }
    }

    pub(super) fn manage(&mut self, win: Window) -> Result<()> {
        if self.clients.contains_key(&win) || self.is_ours(win) {
            return Ok(());
        }
        let Ok(attr) = self.conn.get_window_attributes(win)?.reply() else {
            return Ok(());
        };
        if attr.override_redirect {
            self.conn.map_window(win)?;
            return Ok(());
        }
        let conn = &self.conn;
        let a = &self.atoms;
        let (instance, class) = prop(conn, win, AtomEnum::WM_CLASS.into())
            .map(|r| {
                let mut parts = r
                    .value
                    .split(|&b| b == 0)
                    .map(|p| String::from_utf8_lossy(p).into_owned());
                (
                    parts.next().unwrap_or_default(),
                    parts.next().unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        let pid = prop32(conn, win, a._NET_WM_PID).first().copied();
        let types = prop32(conn, win, a._NET_WM_WINDOW_TYPE);
        let parent = prop32(conn, win, AtomEnum::WM_TRANSIENT_FOR.into())
            .first()
            .copied()
            .filter(|p| self.clients.contains_key(p));
        let protocols = prop32(conn, win, a.WM_PROTOCOLS);
        let hints = prop32(conn, win, AtomEnum::WM_HINTS.into());
        // The input hint, when the hints set it.
        let input = !(hints.len() > 1 && hints[0] & 1 != 0 && hints[1] == 0);
        let fullscreen = prop32(conn, win, a._NET_WM_STATE).contains(&a._NET_WM_STATE_FULLSCREEN);
        let kind = self.kind_of(&types, parent.is_some());
        let parent_client = parent.and_then(|p| self.clients.get(&p));
        let app = match parent_client {
            Some(p) => p.app,
            None => self.apps.iter().position(|ap| ap.owns(&instance, &class)),
        };
        let key = match (parent_client, app) {
            (Some(p), _) => p.key.clone(),
            (None, Some(i)) => format!("app:{i}"),
            _ if !class.is_empty() => class.to_lowercase(),
            _ if !instance.is_empty() => instance.to_lowercase(),
            _ => format!("window:{win}"),
        };
        let title = self.title(win);
        let (delete, take_focus) = (
            protocols.contains(&a.WM_DELETE_WINDOW),
            protocols.contains(&a.WM_TAKE_FOCUS),
        );
        let (wm_state, frame, desktop) = (a.WM_STATE, a._NET_FRAME_EXTENTS, a._NET_WM_DESKTOP);
        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        self.conn
            .change_property32(PropMode::REPLACE, win, wm_state, wm_state, &[1, 0])?;
        self.conn
            .change_property32(PropMode::REPLACE, win, frame, AtomEnum::CARDINAL, &[0; 4])?;
        self.conn
            .change_property32(PropMode::REPLACE, win, desktop, AtomEnum::CARDINAL, &[0])?;
        self.clients.insert(
            win,
            Client {
                instance,
                class,
                title,
                pid,
                kind,
                parent,
                delete,
                take_focus,
                input,
                fullscreen,
                app,
                key,
                geom: (0, 0, 0, 0, 0),
            },
        );
        self.order.push(win);
        self.place(win, None)?;
        let pending = self.pending_for(win);
        let c = &self.clients[&win];
        let take = c.kind.is_app()
            && (pending.is_some()
                || match self.focused.and_then(|f| self.clients.get(&f)) {
                    None => true,
                    Some(f) => f.key == c.key,
                });
        let kind = c.kind;
        self.conn.map_window(win)?;
        if kind.is_app() {
            self.grab_clicks(win)?;
        }
        if take {
            self.mru.insert(0, win);
            self.focus(win)?;
        } else {
            self.mru.push(win);
            match kind {
                // Listed, not raised: under everything.
                k if k.is_app() => {
                    self.conn.configure_window(
                        win,
                        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
                    )?;
                    eprintln!("shuru-desktop: window {win} mapped in the background");
                }
                Kind::Popup | Kind::Splash => self.raise_overlays()?,
                _ => {}
            }
            self.chrome()?;
        }
        if let Some(i) = pending {
            self.answer_pending(i, win);
        }
        self.client_list()
    }

    fn answer_pending(&mut self, i: usize, win: Window) {
        let p = self.pending.remove(i);
        if let Some(r) = p.reply {
            let _ = r.send(Ok(self.info(win)));
        }
    }

    fn pending_for(&self, win: Window) -> Option<usize> {
        let c = self.clients.get(&win)?;
        self.pending.iter().position(|p| {
            c.pid.is_some_and(|pid| descends(pid, p.pid)) || p.app.owns(&c.instance, &c.class)
        })
    }

    /// Puts a window where its kind goes: top level windows fill the work
    /// area, dialogs are centred over their parent.
    fn place(&mut self, win: Window, size: Option<(u16, u16)>) -> Result<()> {
        let Some(c) = self.clients.get(&win) else {
            return Ok(());
        };
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        let geom = if c.fullscreen {
            (0, 0, self.sw, self.sh, 0)
        } else {
            match c.kind {
                Kind::Normal => (wx, wy, ww - 2, wh - 2, 1),
                Kind::Dialog | Kind::Utility | Kind::Splash => {
                    let (w, h) = match size {
                        Some(s) => s,
                        None => match self.conn.get_geometry(win)?.reply() {
                            Ok(g) => (g.width, g.height),
                            Err(_) => return Ok(()),
                        },
                    };
                    let bw: u16 = if c.kind == Kind::Splash { 0 } else { 1 };
                    let w = w.clamp(1, ww - 2 * bw);
                    let h = h.clamp(1, wh - 2 * bw);
                    let over = c
                        .parent
                        .and_then(|p| self.clients.get(&p))
                        .map(|p| {
                            (
                                p.geom.0,
                                p.geom.1,
                                p.geom.2 + 2 * p.geom.4,
                                p.geom.3 + 2 * p.geom.4,
                            )
                        })
                        .unwrap_or((wx, wy, ww, wh));
                    let (ow, oh) = (w as i32 + 2 * bw as i32, h as i32 + 2 * bw as i32);
                    let x = (over.0 as i32 + (over.2 as i32 - ow) / 2)
                        .clamp(wx as i32, (wx as i32 + ww as i32 - ow).max(wx as i32));
                    let y = (over.1 as i32 + (over.3 as i32 - oh) / 2)
                        .clamp(wy as i32, (wy as i32 + wh as i32 - oh).max(wy as i32));
                    (x as i16, y as i16, w, h, bw)
                }
                Kind::Dock | Kind::Popup => return Ok(()),
            }
        };
        let (x, y, w, h, bw) = geom;
        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().border_pixel(crate::draw::HAIRLINE),
        )?;
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(x as i32)
                .y(y as i32)
                .width(w as u32)
                .height(h as u32)
                .border_width(bw as u32),
        )?;
        if let Some(c) = self.clients.get_mut(&win) {
            c.geom = geom;
        }
        self.notify_geometry(win)
    }

    /// The synthetic ConfigureNotify ICCCM asks for after we move a window.
    fn notify_geometry(&self, win: Window) -> Result<()> {
        let Some(c) = self.clients.get(&win) else {
            return Ok(());
        };
        let (x, y, width, height, border_width) = c.geom;
        let ev = ConfigureNotifyEvent {
            response_type: CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: win,
            window: win,
            above_sibling: NONE,
            x,
            y,
            width,
            height,
            border_width,
            override_redirect: false,
        };
        self.conn
            .send_event(false, win, EventMask::STRUCTURE_NOTIFY, ev)?;
        Ok(())
    }

    fn configure_request(&mut self, e: &ConfigureRequestEvent) -> Result<()> {
        let known = self
            .clients
            .get(&e.window)
            .map(|c| (c.kind, c.geom, c.fullscreen));
        match known {
            Some((Kind::Dialog | Kind::Utility | Kind::Splash, g, false)) => {
                let w = if e.value_mask.contains(ConfigWindow::WIDTH) {
                    e.width
                } else {
                    g.2
                };
                let h = if e.value_mask.contains(ConfigWindow::HEIGHT) {
                    e.height
                } else {
                    g.3
                };
                self.place(e.window, Some((w, h)))
            }
            Some((Kind::Dock | Kind::Popup, _, false)) | None => {
                self.conn
                    .configure_window(e.window, &ConfigureWindowAux::from_configure_request(e))?;
                Ok(())
            }
            // The size of a top level window is ours.
            Some(_) => self.notify_geometry(e.window),
        }
    }

    pub(super) fn top_of(&self, mut w: Window) -> Window {
        for _ in 0..16 {
            match self.clients.get(&w).and_then(|c| c.parent) {
                Some(p) if self.clients.contains_key(&p) => w = p,
                _ => break,
            }
        }
        w
    }

    /// Puts the window and its dialogs on screen and gives it the keyboard.
    pub(super) fn focus(&mut self, w: Window) -> Result<()> {
        if !self.clients.contains_key(&w) {
            return Ok(());
        }
        let top = self.top_of(w);
        let mut group: Vec<Window> = self
            .mru
            .iter()
            .rev()
            .copied()
            .filter(|&x| x != top && self.top_of(x) == top)
            .collect();
        group.insert(0, top);
        for g in &group {
            self.raise(*g)?;
        }
        let target = if w == top {
            self.mru
                .iter()
                .copied()
                .find(|&x| x != top && self.top_of(x) == top && self.clients[&x].kind.is_app())
                .unwrap_or(top)
        } else {
            w
        };
        self.mru.retain(|&x| x != target);
        self.mru.insert(0, target);
        if let Some(old) = self
            .focused
            .filter(|&o| o != target && self.clients.contains_key(&o))
        {
            self.grab_clicks(old)?;
        }
        self.conn
            .ungrab_button(ButtonIndex::ANY, target, ModMask::ANY)?;
        let c = &self.clients[&target];
        if c.input {
            self.conn
                .set_input_focus(InputFocus::POINTER_ROOT, target, CURRENT_TIME)?;
        }
        if c.take_focus {
            let ev = ClientMessageEvent::new(
                32,
                target,
                self.atoms.WM_PROTOCOLS,
                [self.atoms.WM_TAKE_FOCUS, CURRENT_TIME, 0, 0, 0],
            );
            self.conn
                .send_event(false, target, EventMask::NO_EVENT, ev)?;
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms._NET_ACTIVE_WINDOW,
            AtomEnum::WINDOW,
            &[target],
        )?;
        self.focused = Some(target);
        self.client_list()?;
        self.chrome()
    }

    /// A click on a window that is not focused focuses it, then goes on
    /// to it.
    fn grab_clicks(&self, w: Window) -> Result<()> {
        self.conn.grab_button(
            false,
            w,
            EventMask::BUTTON_PRESS,
            GrabMode::SYNC,
            GrabMode::ASYNC,
            NONE,
            NONE,
            ButtonIndex::ANY,
            ModMask::ANY,
        )?;
        Ok(())
    }

    fn unmanage(&mut self, w: Window) -> Result<()> {
        self.foreign.retain(|&x| x != w);
        let Some(c) = self.clients.remove(&w) else {
            return Ok(());
        };
        self.mru.retain(|&x| x != w);
        self.order.retain(|&x| x != w);
        // The window may be gone already; errors for it are dropped.
        let wm_state = self.atoms.WM_STATE;
        self.conn
            .change_property32(PropMode::REPLACE, w, wm_state, wm_state, &[0, 0])?;
        if self.focused == Some(w) {
            self.focused = None;
            let next = c
                .parent
                .filter(|p| self.clients.contains_key(p))
                .or_else(|| {
                    self.mru
                        .iter()
                        .copied()
                        .find(|x| self.clients[x].key == c.key && self.clients[x].kind.is_app())
                })
                .or_else(|| {
                    self.mru
                        .iter()
                        .copied()
                        .find(|x| self.clients[x].kind.is_app())
                });
            match next {
                Some(n) => self.focus(n)?,
                None => {
                    self.conn
                        .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        self.root,
                        self.atoms._NET_ACTIVE_WINDOW,
                        AtomEnum::WINDOW,
                        &[NONE],
                    )?;
                    self.chrome()?;
                }
            }
        } else {
            self.chrome()?;
        }
        self.client_list()
    }

    fn client_list(&self) -> Result<()> {
        let stacking: Vec<u32> = self.mru.iter().rev().copied().collect();
        let a = &self.atoms;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            a._NET_CLIENT_LIST,
            AtomEnum::WINDOW,
            &self.order,
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            a._NET_CLIENT_LIST_STACKING,
            AtomEnum::WINDOW,
            &stacking,
        )?;
        Ok(())
    }

    fn close(&self, w: Window) -> Result<&'static str> {
        let Some(c) = self.clients.get(&w) else {
            return Ok("gone");
        };
        if c.delete {
            let ev = ClientMessageEvent::new(
                32,
                w,
                self.atoms.WM_PROTOCOLS,
                [self.atoms.WM_DELETE_WINDOW, CURRENT_TIME, 0, 0, 0],
            );
            self.conn.send_event(false, w, EventMask::NO_EVENT, ev)?;
            Ok("delete")
        } else {
            self.conn.kill_client(w)?;
            Ok("kill")
        }
    }

    fn client_message(&mut self, e: &ClientMessageEvent) -> Result<()> {
        let d = e.data.as_data32();
        let a = &self.atoms;
        if e.type_ == a._NET_ACTIVE_WINDOW {
            let Some(c) = self.clients.get(&e.window) else {
                return Ok(());
            };
            if !c.kind.is_app() {
                return Ok(());
            }
            let pending = self.pending_for(e.window);
            // From a pager or from the app on screen; another app asking
            // for the screen is refused.
            let same = self
                .focused
                .and_then(|f| self.clients.get(&f))
                .is_none_or(|f| f.key == c.key);
            if d[0] == 2 || same || pending.is_some() {
                self.focus(e.window)?;
                if let Some(i) = pending {
                    self.answer_pending(i, e.window);
                }
            }
        } else if e.type_ == a._NET_WM_STATE {
            let fs = a._NET_WM_STATE_FULLSCREEN;
            if d[1] == fs || d[2] == fs {
                if let Some(c) = self.clients.get(&e.window) {
                    let on = match d[0] {
                        0 => false,
                        1 => true,
                        _ => !c.fullscreen,
                    };
                    self.set_fullscreen(e.window, on)?;
                }
            }
        } else if e.type_ == a._NET_CLOSE_WINDOW {
            self.close(e.window)?;
        } else if e.type_ == a._NET_REQUEST_FRAME_EXTENTS {
            let frame = a._NET_FRAME_EXTENTS;
            self.conn.change_property32(
                PropMode::REPLACE,
                e.window,
                frame,
                AtomEnum::CARDINAL,
                &[0; 4],
            )?;
        }
        Ok(())
    }

    fn set_fullscreen(&mut self, w: Window, on: bool) -> Result<()> {
        let Some(c) = self.clients.get_mut(&w) else {
            return Ok(());
        };
        c.fullscreen = on;
        let state: &[u32] = if on {
            &[self.atoms._NET_WM_STATE_FULLSCREEN]
        } else {
            &[]
        };
        self.conn.change_property32(
            PropMode::REPLACE,
            w,
            self.atoms._NET_WM_STATE,
            AtomEnum::ATOM,
            state,
        )?;
        self.place(w, None)?;
        self.chrome()
    }

    fn info(&self, w: Window) -> Value {
        let Some(c) = self.clients.get(&w) else {
            return Value::Null;
        };
        json!({
            "id": w,
            "app": self.app_name(c),
            "class": c.class,
            "title": c.title,
            "pid": c.pid,
            "focused": self.focused == Some(w),
            "kind": c.kind.as_str(),
            "fullscreen": c.fullscreen,
        })
    }

    pub(super) fn app_name(&self, c: &Client) -> String {
        match c.app {
            Some(i) => self.apps[i].name.clone(),
            None if !c.class.is_empty() => c.class.clone(),
            None => c.instance.clone(),
        }
    }

    fn command(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Windows(reply) => {
                let list: Vec<Value> = self.mru.iter().map(|&w| self.info(w)).collect();
                let _ = reply.send(Ok(Value::Array(list)));
            }
            Cmd::Focus(id, reply) => {
                let out = match self.clients.get(&id) {
                    None => Err((404, format!("no window {id}"))),
                    Some(c) if !c.kind.is_app() => {
                        Err((400, format!("window {id} is a {}", c.kind.as_str())))
                    }
                    Some(_) => {
                        self.focus(id)?;
                        Ok(self.info(id))
                    }
                };
                let _ = reply.send(out);
            }
            Cmd::Close(id, reply) => {
                let out = if self.clients.contains_key(&id) {
                    let how = self.close(id)?;
                    Ok(json!({ "id": id, "closing": true, "how": how }))
                } else {
                    Err((404, format!("no window {id}")))
                };
                let _ = reply.send(out);
            }
            Cmd::Launch {
                app,
                args,
                timeout,
                reply,
            } => {
                let Some(found) = apps::find(&self.apps, &app) else {
                    let _ = reply.send(Err((404, format!("no app {app}"))));
                    return Ok(());
                };
                match self.spawn(&found, &args) {
                    Ok(pid) => self.pending.push(Pending {
                        app: found,
                        pid,
                        deadline: Instant::now() + timeout,
                        reply: Some(reply),
                    }),
                    Err(e) => {
                        let _ = reply.send(Err((500, format!("{e:#}"))));
                    }
                }
            }
            Cmd::Overlay { who, ripple, reply } => {
                if let Some(who) = who {
                    self.set_who(who)?;
                }
                if let Some((x, y)) = ripple {
                    self.show_ripple(x, y, false)?;
                }
                let _ = reply.send(Ok(json!({ "who": self.who })));
            }
            Cmd::Exited { pid, status, ok } => {
                if ok {
                    return Ok(());
                }
                if let Some(i) = self.pending.iter().position(|p| p.pid == pid) {
                    let p = self.pending.remove(i);
                    if let Some(r) = p.reply {
                        let msg =
                            format!("{} exited with {status} before a window mapped", p.app.id);
                        let _ = r.send(Err((502, msg)));
                    }
                }
            }
        }
        Ok(())
    }

    fn spawn(&self, app: &App, args: &[String]) -> Result<u32> {
        let argv = app.command(args)?;
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env("DISPLAY", &self.display)
            .env("QT_ACCESSIBILITY", "1")
            .env("GNOME_ACCESSIBILITY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting {}", argv[0]))?;
        let pid = child.id();
        eprintln!("shuru-desktop: launched {} as pid {pid}", app.id);
        let handle = self.handle.clone();
        std::thread::Builder::new()
            .name("reap".into())
            .spawn(move || {
                if let Ok(status) = child.wait() {
                    handle.send(Cmd::Exited {
                        pid,
                        status: status.to_string(),
                        ok: status.success(),
                    });
                }
            })?;
        Ok(pid)
    }

    fn timers(&mut self) -> Result<()> {
        let now = Instant::now();
        if self.cursor_shown && self.cursor_poll <= now {
            self.cursor_moved = true;
            self.cursor_poll = now + CURSOR_POLL;
        }
        if self.ripple_until.is_some_and(|t| t <= now) {
            self.ripple_until = None;
            self.conn.unmap_window(self.ours.ripple)?;
            self.moved(self.ripple_rect);
        }
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].deadline > now {
                i += 1;
                continue;
            }
            let p = self.pending.remove(i);
            if let Some(r) = p.reply {
                let msg = format!("no window from {} (pid {}) in time", p.app.id, p.pid);
                let _ = r.send(Err((504, msg)));
            }
        }
        self.recent
            .retain(|(t, _)| now.duration_since(*t) < Duration::from_secs(1));
        Ok(())
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .iter()
            .map(|p| p.deadline)
            .chain(self.ripple_until)
            .chain(self.cursor_shown.then_some(self.cursor_poll))
            .min()
    }

    pub(super) fn panel_click(&mut self, i: usize) -> Result<()> {
        let key = match self.panel_targets.get(i) {
            Some(super::PanelTarget::App(a)) => format!("app:{a}"),
            Some(super::PanelTarget::Key(k)) => k.clone(),
            None => return Ok(()),
        };
        let running = self
            .mru
            .iter()
            .copied()
            .find(|w| self.clients[w].key == key && self.clients[w].kind.is_app());
        match (running, self.panel_targets.get(i)) {
            (Some(w), _) => self.focus(w)?,
            (None, Some(super::PanelTarget::App(a))) => {
                let app = self.apps[*a].clone();
                match self.spawn(&app, &[]) {
                    Ok(pid) => self.pending.push(Pending {
                        app,
                        pid,
                        deadline: Instant::now() + Duration::from_secs(30),
                        reply: None,
                    }),
                    Err(e) => eprintln!("shuru-desktop: {e:#}"),
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn grab_keys(&mut self) -> Result<()> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()?;
        let per = (map.keysyms_per_keycode as usize).max(1);
        for (i, syms) in map.keysyms.chunks(per).enumerate() {
            let code = min + i as u8;
            if syms.contains(&0xff09) && self.keys.tab.is_none() {
                self.keys.tab = Some(code);
            }
            if syms.contains(&0xffe9) || syms.contains(&0xffea) {
                self.keys.alts.push(code);
            }
        }
        if let Some(tab) = self.keys.tab {
            let m1 = u16::from(ModMask::M1);
            let (lock, num) = (u16::from(ModMask::LOCK), u16::from(ModMask::M2));
            for extra in [0, lock, num, lock | num] {
                self.conn.grab_key(
                    true,
                    self.root,
                    ModMask::from(m1 | extra),
                    tab,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }
        Ok(())
    }

    /// Alt+Tab walks the apps, most recent first, while Alt is held.
    fn key_press(&mut self, e: &KeyPressEvent) -> Result<()> {
        if Some(e.detail) != self.keys.tab || u16::from(e.state) & u16::from(ModMask::M1) == 0 {
            return Ok(());
        }
        if self.alt_tab.is_none() {
            let mut seen: Vec<&str> = Vec::new();
            let mut list = Vec::new();
            for &w in &self.mru {
                let c = &self.clients[&w];
                if c.kind.is_app() && !seen.contains(&c.key.as_str()) {
                    seen.push(&c.key);
                    list.push(w);
                }
            }
            if list.len() < 2 {
                return Ok(());
            }
            self.conn.grab_keyboard(
                false,
                self.root,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?;
            self.alt_tab = Some((list, 0));
        }
        let next = self.alt_tab.as_mut().map(|(list, i)| {
            *i = (*i + 1) % list.len();
            list[*i]
        });
        if let Some(w) = next {
            self.focus(w)?;
        }
        Ok(())
    }

    fn button(&mut self, e: &ButtonPressEvent) -> Result<()> {
        if e.event == self.ours.panel {
            if e.detail == 1 {
                if let Some(i) = chrome::panel_hit(&self.panel_spans, e.event_x as f32) {
                    self.panel_click(i)?;
                }
            }
        } else if e.event == self.ours.badge {
            let x = e.event_x as f32;
            if e.detail == 1
                && self
                    .badge_done
                    .is_some_and(|(dx, dw)| x >= dx && x < dx + dw)
            {
                self.set_who(Some(Who::Agent))?;
            }
        } else {
            self.conn.allow_events(Allow::REPLAY_POINTER, e.time)?;
            let app = self.clients.get(&e.event).is_some_and(|c| c.kind.is_app());
            if app && self.focused != Some(e.event) {
                self.focus(e.event)?;
            }
        }
        Ok(())
    }
}
