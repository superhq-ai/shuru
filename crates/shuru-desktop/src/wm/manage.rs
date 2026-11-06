//! Managing windows: framing, placing, stacking, focusing, closing, and
//! the API commands.

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::{
    descends, prop, prop32, shadow_of, Client, Cmd, Entry, Kind, Pending, Place, Target, Who, Wm, R,
};
use crate::appearance;
use crate::apps::{self, App};
use crate::chrome::{self, Corner, BAR_H, CORNER};
use crate::draw;
use crate::draw::theme;

const CURSOR_POLL: Duration = Duration::from_millis(100);
const CURSOR_IDLE: Duration = Duration::from_secs(2);
const CURSOR_POLL_IDLE: Duration = Duration::from_secs(1);
/// The resize band outside a floating window, the ring included.
pub(super) const GRIP: i16 = 7;
const MIN_W: u16 = 160;
const MIN_H: u16 = 80;

/// The frame round a card: one pixel more on each side for the ring.
fn frame_of(card: R) -> R {
    (card.0 - 1, card.1 - 1, card.2 + 2, card.3 + 2)
}

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
            let update = self.accessibility_tree();
            if self.last_accessibility.as_ref() != Some(&update) {
                if let Some(bus) = &self.bus {
                    let _ = bus.send(crate::bus::Event::Tree(Box::new(update.clone())));
                }
                self.last_accessibility = Some(update);
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
                libc::pollfd {
                    fd: self.watch.as_ref().map_or(-1, |w| w.fd()),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 3, timeout) };
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
            let changes = match &self.watch {
                Some(w) if fds[2].revents != 0 => w.drain(),
                _ => Default::default(),
            };
            if changes.appearance {
                self.set_appearance(appearance::read())?;
            }
            if changes.language && crate::locale::reload() {
                self.set_language()?;
            }
            if changes.do_not_disturb {
                self.set_quiet(crate::dnd::read())?;
            }
        }
    }

    fn events(&mut self) -> Result<bool> {
        let mut any = false;
        while let Some(ev) = self.conn.poll_for_event()? {
            any = true;
            if self.cursor_active.elapsed() >= CURSOR_IDLE {
                self.cursor_poll = Instant::now();
            }
            self.cursor_active = Instant::now();
            self.event(ev)?;
        }
        Ok(any)
    }

    fn event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::MapRequest(e) => match self.clients.get(&e.window) {
                Some(c) if c.minimized => self.focus(e.window)?,
                Some(_) => {
                    self.conn.map_window(e.window)?;
                }
                None => self.manage(e.window)?,
            },
            Event::ConfigureRequest(e) => self.configure_request(&e)?,
            Event::UnmapNotify(e) => {
                if let Some(c) = self.clients.get_mut(&e.window) {
                    if c.unmaps > 0 {
                        c.unmaps -= 1;
                    } else {
                        self.unmanage(e.window, false)?;
                    }
                }
            }
            Event::DestroyNotify(e) => {
                self.foreign.retain(|&x| x != e.window);
                if self.clients.contains_key(&e.window) {
                    self.unmanage(e.window, true)?;
                }
            }
            Event::MapNotify(e) if e.event == self.root => {
                if e.override_redirect
                    && !self.is_ours(e.window)
                    && !self.foreign.contains(&e.window)
                {
                    self.foreign.push(e.window);
                    self.restack()?;
                }
            }
            Event::PropertyNotify(e)
                if e.window == self.ours.pop && e.atom == self.atoms.SHURU_PASTE =>
            {
                self.clipboard_chunk(&e)?
            }
            Event::PropertyNotify(e)
                if e.atom == self.atoms._GTK_FRAME_EXTENTS
                    && self.clients.contains_key(&e.window) =>
            {
                let shadow = shadow_of(&self.conn, e.window, e.atom);
                if let Some(c) = self.clients.get_mut(&e.window) {
                    if c.shadow != shadow {
                        c.shadow = shadow;
                        self.layout(e.window)?;
                    }
                }
            }
            Event::PropertyNotify(e)
                if e.atom == u32::from(AtomEnum::WM_HINTS)
                    && self.clients.contains_key(&e.window) =>
            {
                let hints = prop32(&self.conn, e.window, e.atom);
                self.set_attention(e.window, urgent(&hints))?;
            }
            Event::PropertyNotify(e) => {
                let names = [self.atoms._NET_WM_NAME, u32::from(AtomEnum::WM_NAME)];
                if names.contains(&e.atom) && self.clients.contains_key(&e.window) {
                    let title = self.title(e.window);
                    if let Some(c) = self.clients.get_mut(&e.window) {
                        c.title = title;
                    }
                    self.draw_frame(e.window)?;
                    if self.pop.is_some() {
                        self.draw_pop()?;
                    }
                }
            }
            Event::ClientMessage(e) => self.client_message(&e)?,
            Event::RandrScreenChangeNotify(e) if e.root == self.root => {
                self.screen_changed(e.width, e.height)?
            }
            Event::Expose(e) if e.count == 0 => self.redraw(e.window)?,
            Event::ButtonPress(e) => self.button_press(&e)?,
            Event::ButtonRelease(e) => self.button_release(&e)?,
            Event::MotionNotify(e) => self.motion(&e)?,
            Event::LeaveNotify(e) => self.leave(e.event)?,
            Event::SelectionRequest(e) => self.clipboard_request(&e)?,
            Event::SelectionNotify(e) => self.clipboard_paste(&e)?,
            Event::KeyPress(e) => self.key_press(&e)?,
            Event::KeyRelease(e) => self.key_release(&e)?,
            Event::MappingNotify(_) => self.grab_keys()?,
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
        [
            o.check, o.panel, o.rail, o.empty, o.tooltip, o.pop, o.snap, o.cursor, o.ripple,
        ]
        .contains(&w)
            || o.notes.contains(&w)
            || self.frames.contains_key(&w)
    }

    fn title(&self, w: Window) -> String {
        if let Some(r) = prop(&self.conn, w, self.atoms._NET_WM_NAME) {
            return String::from_utf8_lossy(&r.value).into_owned();
        }
        prop(&self.conn, w, AtomEnum::WM_NAME.into())
            .map(|r| match r.type_ {
                t if t == self.atoms.UTF8_STRING => String::from_utf8_lossy(&r.value).into_owned(),
                t if t == self.atoms.COMPOUND_TEXT => compound_text(&r.value),
                _ => r.value.iter().map(|&b| b as char).collect(),
            })
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

    /// The app entry for a window class: a configured app, or one found in
    /// the XDG dirs, which joins the rail while it has windows.
    fn entry_for(&mut self, instance: &str, class: &str) -> Option<usize> {
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.app.owns(instance, class))
        {
            return Some(i);
        }
        let key = format!("{instance}\0{class}");
        if let Some(found) = self.looked_up.get(&key) {
            return *found;
        }
        let found = apps::owner(instance, class).map(|app| {
            self.entries.push(Entry::new(app, false));
            self.entries.len() - 1
        });
        self.looked_up.insert(key, found);
        found
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
        let Ok(geom) = self.conn.get_geometry(win)?.reply() else {
            return Ok(());
        };
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
        let states = prop32(conn, win, a._NET_WM_STATE);
        let fullscreen = states.contains(&a._NET_WM_STATE_FULLSCREEN);
        let above = states.contains(&a._NET_WM_STATE_ABOVE);
        let modal = parent.is_some() && states.contains(&a._NET_WM_STATE_MODAL);
        let attention = states.contains(&a._NET_WM_STATE_DEMANDS_ATTENTION) || urgent(&hints);
        let motif = prop32(conn, win, a._MOTIF_WM_HINTS);
        let undecorated = motif.len() > 2 && motif[0] & 2 != 0 && motif[2] == 0;
        let csd = prop(conn, win, a._GTK_FRAME_EXTENTS).is_some();
        let shadow = shadow_of(conn, win, a._GTK_FRAME_EXTENTS);
        let normal = prop32(conn, win, AtomEnum::WM_NORMAL_HINTS.into());
        let size_hint = |flag: u32, i: usize| {
            (normal.len() > i + 1 && normal[0] & flag != 0).then(|| {
                (
                    normal[i].min(u16::MAX as u32) as u16,
                    normal[i + 1].min(u16::MAX as u32) as u16,
                )
            })
        };
        let min = size_hint(16, 5).map_or((0, 0), |(w, h)| {
            (
                w.saturating_sub(shadow[0] + shadow[1]),
                h.saturating_sub(shadow[2] + shadow[3]),
            )
        });
        let max = size_hint(32, 7).map_or((0, 0), |(w, h)| {
            (
                w.saturating_sub(shadow[0] + shadow[1]),
                h.saturating_sub(shadow[2] + shadow[3]),
            )
        });
        let inc = size_hint(64, 9).unwrap_or((0, 0));
        let base = size_hint(256, 15).unwrap_or(min);
        let kind = self.kind_of(&types, parent.is_some());
        let parent_client = parent.and_then(|p| self.clients.get(&p));
        let parent_card = parent_client.map(|p| p.card);
        let parent_app = parent_client.map(|p| (p.app, p.key.clone()));
        let app = match parent_app {
            Some((app, _)) => app,
            None => self.entry_for(&instance, &class),
        };
        let key = match (parent_app, app) {
            (Some((_, k)), _) => k,
            (None, Some(i)) => format!("app:{i}"),
            _ if !class.is_empty() => class.to_lowercase(),
            _ if !instance.is_empty() => instance.to_lowercase(),
            _ => format!("window:{win}"),
        };
        let title = self.title(win);
        let a = &self.atoms;
        let (delete, take_focus) = (
            protocols.contains(&a.WM_DELETE_WINDOW),
            protocols.contains(&a.WM_TAKE_FOCUS),
        );
        let framed = kind.framed();
        let bar = framed && kind != Kind::Splash && !undecorated && !csd;
        let bar_h = if bar { BAR_H } else { 0 };
        let natural = (
            geom.width.saturating_sub(shadow[0] + shadow[1]).max(1),
            geom.height.saturating_sub(shadow[2] + shadow[3]).max(1) + bar_h,
        );
        let fixed = min.0 > 0 && min == max;
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        let floats = self
            .pending
            .iter()
            .find(|p| pid.is_some_and(|pid| descends(pid, p.pid)) || p.app.owns(&instance, &class))
            .is_some_and(|p| !p.maximize);
        let (place, card) = if kind == Kind::Normal && parent.is_none() && !fixed && !floats {
            (Place::Maximized, (wx, wy, ww, wh))
        } else {
            let over = parent_card.unwrap_or((wx, wy, ww, wh));
            let w = natural.0.clamp(1, ww);
            let h = natural.1.clamp(1, wh);
            (Place::Floating, self.centered((w, h), over))
        };

        let (mut frame, mut grip, mut corners, mut edges) = (win, NONE, Vec::new(), Vec::new());
        let mut unmaps = 0;
        if framed {
            frame = self.conn.generate_id()?;
            let fr = frame_of(card);
            self.conn.create_window(
                self.depth,
                frame,
                self.root,
                fr.0,
                fr.1,
                fr.2,
                fr.3,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &CreateWindowAux::new()
                    .background_pixel(theme().hairline)
                    .border_pixel(theme().hairline)
                    .event_mask(
                        EventMask::SUBSTRUCTURE_REDIRECT
                            | EventMask::SUBSTRUCTURE_NOTIFY
                            | EventMask::EXPOSURE
                            | EventMask::BUTTON_PRESS
                            | EventMask::BUTTON_RELEASE
                            | EventMask::POINTER_MOTION
                            | EventMask::LEAVE_WINDOW,
                    ),
            )?;
            grip = self.conn.generate_id()?;
            self.conn.create_window(
                0,
                grip,
                self.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(
                    EventMask::BUTTON_PRESS | EventMask::POINTER_MOTION | EventMask::LEAVE_WINDOW,
                ),
            )?;
            let which: &[Corner] = if bar {
                &[Corner::BottomLeft, Corner::BottomRight]
            } else {
                &[
                    Corner::TopLeft,
                    Corner::TopRight,
                    Corner::BottomLeft,
                    Corner::BottomRight,
                ]
            };
            for &k in which {
                let cw = self.conn.generate_id()?;
                self.conn.create_window(
                    self.depth,
                    cw,
                    frame,
                    0,
                    0,
                    CORNER,
                    CORNER,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    self.visual,
                    &CreateWindowAux::new()
                        .background_pixel(theme().hairline)
                        .event_mask(EventMask::EXPOSURE),
                )?;
                let (_, mask) = chrome::corner(k, theme().hairline);
                self.shape(cw, &mask)?;
                corners.push((k, cw));
                self.frames.insert(cw, win);
            }
            for _ in 0..if bar { 3 } else { 4 } {
                let ew = self.conn.generate_id()?;
                self.conn.create_window(
                    self.depth,
                    ew,
                    frame,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    self.visual,
                    &CreateWindowAux::new().background_pixel(theme().hairline),
                )?;
                edges.push(ew);
            }
            self.frames.insert(frame, win);
            self.frames.insert(grip, win);
            self.conn.change_save_set(SetMode::INSERT, win)?;
            if attr.map_state == MapState::VIEWABLE {
                unmaps += 1;
            }
            self.conn.reparent_window(
                win,
                frame,
                1 - shadow[0] as i16,
                1 + bar_h as i16 - shadow[2] as i16,
            )?;
            self.conn
                .configure_window(win, &ConfigureWindowAux::new().border_width(0))?;
        }
        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let wm_state = self.atoms.WM_STATE;
        self.conn
            .change_property32(PropMode::REPLACE, win, wm_state, wm_state, &[1, 0])?;
        let desktop = self.atoms._NET_WM_DESKTOP;
        self.conn
            .change_property32(PropMode::REPLACE, win, desktop, AtomEnum::CARDINAL, &[0])?;
        let a = &self.atoms;
        let allowed = [
            a._NET_WM_ACTION_MOVE,
            a._NET_WM_ACTION_RESIZE,
            a._NET_WM_ACTION_MINIMIZE,
            a._NET_WM_ACTION_MAXIMIZE_HORZ,
            a._NET_WM_ACTION_MAXIMIZE_VERT,
            a._NET_WM_ACTION_FULLSCREEN,
            a._NET_WM_ACTION_CLOSE,
        ];
        let allowed_atom = a._NET_WM_ALLOWED_ACTIONS;
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            allowed_atom,
            AtomEnum::ATOM,
            &allowed,
        )?;
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
                framed,
                frame,
                grip,
                corners,
                edges,
                shadow,
                bar,
                place,
                snapped: false,
                minimized: false,
                card,
                natural,
                floating: None,
                min,
                max,
                base,
                inc,
                above,
                attention,
                modal,
                unmaps,
                hover: false,
                pressed: None,
            },
        );
        self.order.push(win);
        self.layout(win)?;
        let pending = self.pending_for(win);
        let c = &self.clients[&win];
        let take = c.kind.is_app()
            && (pending.is_some()
                || match self.focused.and_then(|f| self.clients.get(&f)) {
                    None => true,
                    Some(f) => f.key == c.key,
                });
        self.conn.map_window(win)?;
        if framed {
            for &(_, cw) in &self.clients[&win].corners {
                self.conn.map_window(cw)?;
            }
            self.conn.map_window(frame)?;
            if take {
                self.stack.push(win);
            } else {
                let at = self
                    .focused
                    .map(|f| self.top_of(f))
                    .and_then(|f| self.stack.iter().position(|&s| s == f))
                    .unwrap_or(self.stack.len());
                self.stack.insert(at, win);
            }
        }
        if kind.is_app() {
            self.grab_clicks(win)?;
        }
        if take {
            self.mru.insert(0, win);
            self.focus(win)?;
        } else {
            self.mru.push(win);
            if kind.is_app() {
                eprintln!("shuru-desktop: window {win} mapped in the background");
            }
            self.chrome()?;
        }
        if let Some(i) = pending {
            self.answer_pending(i, win)?;
        }
        self.client_list()
    }

    /// A card of `size` centered over `over`, inside the work area.
    fn centered(&self, size: (u16, u16), over: R) -> R {
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        let (w, h) = (size.0.min(ww) as i32, size.1.min(wh) as i32);
        let x = (over.0 as i32 + (over.2 as i32 - w) / 2)
            .clamp(wx as i32, (wx as i32 + ww as i32 - w).max(wx as i32));
        let y = (over.1 as i32 + (over.3 as i32 - h) / 2)
            .clamp(wy as i32, (wy as i32 + wh as i32 - h).max(wy as i32));
        (x as i16, y as i16, w as u16, h as u16)
    }

    /// Where a window that has not floated yet goes when it is restored:
    /// the size it asked for, centered, or two thirds of the work area when
    /// it asked for about all of it.
    pub(super) fn float_card(&self, w: Window) -> R {
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        let Some(c) = self.clients.get(&w) else {
            return (wx, wy, ww, wh);
        };
        if let Some(f) = c.floating {
            return f;
        }
        let (mut fw, mut fh) = c.natural;
        if fw as u32 * 10 >= ww as u32 * 9 || fh as u32 * 10 >= wh as u32 * 9 || fw < MIN_W {
            fw = (ww as u32 * 2 / 3) as u16;
            fh = (wh as u32 * 2 / 3) as u16;
        }
        self.centered((fw, fh), (wx, wy, ww, wh))
    }

    fn answer_pending(&mut self, i: usize, win: Window) -> Result<()> {
        let p = self.pending.remove(i);
        if p.maximize {
            self.set_maximized(win, true)?;
        }
        if let Some(r) = p.reply {
            let _ = r.send(Ok(self.info(win)));
        }
        Ok(())
    }

    fn pending_for(&self, win: Window) -> Option<usize> {
        let c = self.clients.get(&win)?;
        self.pending.iter().position(|p| {
            c.pid.is_some_and(|pid| descends(pid, p.pid)) || p.app.owns(&c.instance, &c.class)
        })
    }

    /// Puts the frame, the client, the corners and the grip where the card
    /// of a window says.
    pub(super) fn layout(&mut self, win: Window) -> Result<()> {
        let Some(c) = self.clients.get(&win) else {
            return Ok(());
        };
        if !c.framed {
            return Ok(());
        }
        let bar = c.bar_h();
        let (frame, client) = if c.fullscreen {
            let s = (0, 0, self.sw, self.sh);
            (s, s)
        } else {
            let f = frame_of(c.card);
            let [l, r, t, b] = c.shadow;
            (
                f,
                (
                    1 - l as i16,
                    1 + bar as i16 - t as i16,
                    c.card.2 + l + r,
                    c.card.3.saturating_sub(bar).max(1) + t + b,
                ),
            )
        };
        self.conn.configure_window(
            c.frame,
            &ConfigureWindowAux::new()
                .x(frame.0 as i32)
                .y(frame.1 as i32)
                .width(frame.2 as u32)
                .height(frame.3 as u32),
        )?;
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(client.0 as i32)
                .y(client.1 as i32)
                .width(client.2 as u32)
                .height(client.3 as u32),
        )?;
        if c.fullscreen {
            self.conn
                .shape_mask(shape::SO::SET, shape::SK::BOUNDING, c.frame, 0, 0, NONE)?;
        } else {
            let rects: Vec<Rectangle> =
                chrome::rounded_shape(frame.2, frame.3, chrome::RADIUS + 1.0)
                    .into_iter()
                    .map(|(x, y, width, height)| Rectangle {
                        x,
                        y,
                        width,
                        height,
                    })
                    .collect();
            self.conn.shape_rectangles(
                shape::SO::SET,
                shape::SK::BOUNDING,
                ClipOrdering::YX_BANDED,
                c.frame,
                0,
                0,
                &rects,
            )?;
        }
        let top = if c.bar { 1 + bar as i16 } else { 0 };
        let (fw, fh) = (frame.2 as i16, frame.3 as i16);
        let lines = [
            (0, top, 1, fh - top),
            (fw - 1, top, 1, fh - top),
            (0, fh - 1, fw, 1),
            (0, 0, fw, 1),
        ];
        for (&ew, &(x, y, w, h)) in c.edges.iter().zip(&lines) {
            self.conn.configure_window(
                ew,
                &ConfigureWindowAux::new()
                    .x(x as i32)
                    .y(y as i32)
                    .width(w.max(1) as u32)
                    .height(h.max(1) as u32)
                    .stack_mode(StackMode::ABOVE),
            )?;
            if c.fullscreen {
                self.conn.unmap_window(ew)?;
            } else {
                self.conn.map_window(ew)?;
            }
        }
        let s = CORNER as i16;
        for &(k, cw) in &c.corners {
            let (x, y) = match k {
                Corner::TopLeft => (0, 0),
                Corner::TopRight => (frame.2 as i16 - s, 0),
                Corner::BottomLeft => (0, frame.3 as i16 - s),
                Corner::BottomRight => (frame.2 as i16 - s, frame.3 as i16 - s),
            };
            self.conn.configure_window(
                cw,
                &ConfigureWindowAux::new()
                    .x(x as i32)
                    .y(y as i32)
                    .stack_mode(StackMode::ABOVE),
            )?;
            if c.fullscreen {
                self.conn.unmap_window(cw)?;
            } else {
                self.conn.map_window(cw)?;
            }
        }
        let gripped = c.place == Place::Floating && !c.fullscreen && !c.minimized;
        if gripped {
            let g = GRIP;
            self.conn.configure_window(
                c.grip,
                &ConfigureWindowAux::new()
                    .x((c.card.0 - g) as i32)
                    .y((c.card.1 - g) as i32)
                    .width((c.card.2 + 2 * g as u16) as u32)
                    .height((c.card.3 + 2 * g as u16) as u32)
                    .sibling(c.frame)
                    .stack_mode(StackMode::BELOW),
            )?;
            self.conn.map_window(c.grip)?;
        } else {
            self.conn.unmap_window(c.grip)?;
        }
        let extents = [1, 1, 1 + bar as u32, 1];
        let frame_atom = self.atoms._NET_FRAME_EXTENTS;
        self.conn.change_property32(
            PropMode::REPLACE,
            win,
            frame_atom,
            AtomEnum::CARDINAL,
            if c.fullscreen { &[0; 4] } else { &extents },
        )?;
        self.net_state(win)?;
        self.notify_geometry(win)?;
        self.draw_frame(win)
    }

    /// The synthetic ConfigureNotify ICCCM asks for after we move a window.
    fn notify_geometry(&self, win: Window) -> Result<()> {
        let Some(c) = self.clients.get(&win) else {
            return Ok(());
        };
        let (x, y, width, height) = if !c.framed {
            match self.conn.get_geometry(win)?.reply() {
                Ok(g) => (g.x, g.y, g.width, g.height),
                Err(_) => return Ok(()),
            }
        } else if c.fullscreen {
            (0, 0, self.sw, self.sh)
        } else {
            let bar = c.bar_h();
            let [l, r, t, b] = c.shadow;
            (
                c.card.0 - l as i16,
                c.card.1 + bar as i16 - t as i16,
                c.card.2 + l + r,
                c.card.3 - bar + t + b,
            )
        };
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
            border_width: 0,
            override_redirect: false,
        };
        self.conn
            .send_event(false, win, EventMask::STRUCTURE_NOTIFY, ev)?;
        Ok(())
    }

    fn configure_request(&mut self, e: &ConfigureRequestEvent) -> Result<()> {
        let Some(c) = self.clients.get(&e.window).filter(|c| c.framed) else {
            self.conn
                .configure_window(e.window, &ConfigureWindowAux::from_configure_request(e))?;
            return Ok(());
        };
        if c.fullscreen || c.place == Place::Maximized {
            return self.notify_geometry(e.window);
        }
        let bar = c.bar_h();
        let [l, r, t, b] = c.shadow;
        let m = e.value_mask;
        let mut card = c.card;
        if m.contains(ConfigWindow::WIDTH) {
            card.2 = e.width.saturating_sub(l + r).max(1);
        }
        if m.contains(ConfigWindow::HEIGHT) {
            card.3 = e
                .height
                .saturating_sub(t + b)
                .saturating_add(bar)
                .max(bar + 1);
        }
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        card.2 = card.2.min(ww);
        card.3 = card.3.min(wh);
        if c.kind == Kind::Normal && c.parent.is_none() {
            if m.contains(ConfigWindow::X) {
                card.0 = e.x + l as i16;
            }
            if m.contains(ConfigWindow::Y) {
                card.1 = e.y - bar as i16 + t as i16;
            }
            card.0 = card.0.clamp(wx - card.2 as i16 + 80, wx + ww as i16 - 80);
            card.1 = card.1.clamp(wy, wy + wh as i16 - 40);
        } else {
            let over = c
                .parent
                .and_then(|p| self.clients.get(&p))
                .map(|p| p.card)
                .unwrap_or((wx, wy, ww, wh));
            card = self.centered((card.2, card.3), over);
        }
        if let Some(c) = self.clients.get_mut(&e.window) {
            c.card = card;
        }
        self.layout(e.window)
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

    /// The window and the dialogs over it, bottom first.
    fn group(&self, top: Window) -> Vec<Window> {
        let mut group: Vec<Window> = self
            .stack
            .iter()
            .copied()
            .filter(|&x| x != top && self.top_of(x) == top)
            .collect();
        group.insert(0, top);
        group
    }

    /// Raises the window and its dialogs, restores them if minimized, and
    /// gives it the keyboard.
    pub(super) fn focus(&mut self, w: Window) -> Result<()> {
        if !self.clients.contains_key(&w) {
            return Ok(());
        }
        let top = self.top_of(w);
        let group = self.group(top);
        for &g in &group {
            if self.clients[&g].minimized {
                self.restore(g)?;
            }
        }
        self.stack.retain(|x| !group.contains(x));
        self.stack
            .extend(group.iter().filter(|g| self.clients[g].framed));
        let target = if w == top {
            self.mru
                .iter()
                .copied()
                .find(|&x| x != top && self.top_of(x) == top && self.clients[&x].kind.is_app())
                .unwrap_or(top)
        } else {
            w
        };
        let target = self
            .order
            .iter()
            .copied()
            .find(|x| {
                let c = &self.clients[x];
                c.modal && c.parent == Some(target) && !c.minimized
            })
            .unwrap_or(target);
        if self.clients[&target].attention {
            if let Some(c) = self.clients.get_mut(&target) {
                c.attention = false;
            }
        }
        self.mru.retain(|&x| x != target);
        self.mru.insert(0, target);
        let old = self
            .focused
            .filter(|&o| o != target && self.clients.contains_key(&o));
        if let Some(old) = old {
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
        if let Some(old) = old {
            self.net_state(old)?;
            self.ring(old)?;
        }
        self.net_state(target)?;
        self.ring(target)?;
        self.client_list()?;
        self.chrome()
    }

    /// The color of the ring round a frame.
    pub(super) fn ring_color(&self, w: Window) -> u32 {
        if self.focused != Some(w) {
            theme().hairline
        } else if self.who == Some(Who::Person) {
            theme().amber
        } else {
            theme().hairline_hi
        }
    }

    /// Repaints the ring and bar of a frame in its color.
    pub(super) fn ring(&mut self, w: Window) -> Result<()> {
        let Some(c) = self.clients.get(&w).filter(|c| c.framed) else {
            return Ok(());
        };
        let color = self.ring_color(w);
        for &win in std::iter::once(&c.frame).chain(&c.edges) {
            self.conn.change_window_attributes(
                win,
                &ChangeWindowAttributesAux::new().background_pixel(color),
            )?;
            self.conn.clear_area(false, win, 0, 0, 0, 0)?;
        }
        self.draw_frame(w)
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

    fn unmanage(&mut self, w: Window, destroyed: bool) -> Result<()> {
        self.foreign.retain(|&x| x != w);
        self.bar_trees.remove(&w);
        let Some(c) = self.clients.remove(&w) else {
            return Ok(());
        };
        self.mru.retain(|&x| x != w);
        self.order.retain(|&x| x != w);
        self.stack.retain(|&x| x != w);
        if self.drag.as_ref().is_some_and(|d| d.window() == w) {
            self.end_drag()?;
        }
        // The window may be gone already; errors for it are dropped.
        if !destroyed {
            let wm_state = self.atoms.WM_STATE;
            self.conn
                .change_property32(PropMode::REPLACE, w, wm_state, wm_state, &[0, 0])?;
        }
        if c.framed {
            if !destroyed {
                let bar = c.bar_h() as i16;
                self.conn
                    .reparent_window(w, self.root, c.card.0, c.card.1 + bar)?;
                self.conn.change_save_set(SetMode::DELETE, w)?;
            }
            self.frames.retain(|_, client| *client != w);
            self.conn.destroy_window(c.frame)?;
            self.conn.destroy_window(c.grip)?;
            self.moved(frame_of(c.card));
        }
        if self.focused == Some(w) {
            self.focused = None;
            let next = c
                .parent
                .filter(|p| self.clients.get(p).is_some_and(|p| !p.minimized))
                .or_else(|| self.next_focus(Some(&c.key)));
            match next {
                Some(n) => self.focus(n)?,
                None => self.focus_nothing()?,
            }
        } else {
            self.chrome()?;
        }
        self.client_list()
    }

    /// The most recent visible window, of the app with `key` if it has one.
    fn next_focus(&self, key: Option<&str>) -> Option<Window> {
        let visible = |x: &Window| {
            let c = &self.clients[x];
            c.kind.is_app() && !c.minimized
        };
        key.and_then(|k| {
            self.mru
                .iter()
                .copied()
                .find(|x| visible(x) && self.clients[x].key == k)
        })
        .or_else(|| self.mru.iter().copied().find(visible))
    }

    fn focus_nothing(&mut self) -> Result<()> {
        let old = self.focused.take();
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, self.root, CURRENT_TIME)?;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms._NET_ACTIVE_WINDOW,
            AtomEnum::WINDOW,
            &[NONE],
        )?;
        if let Some(o) = old {
            self.grab_clicks(o)?;
            self.net_state(o)?;
            self.ring(o)?;
        }
        self.chrome()
    }

    fn client_list(&self) -> Result<()> {
        let (framed, full) = self.stacked();
        let mut stacking = framed;
        if let Some(t) = full {
            stacking.extend(self.group(t));
        }
        stacking.extend(self.order.iter().filter(|w| !self.clients[w].framed));
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

    /// Stacks the frames in order, then the panel, the rail, a fullscreen
    /// window, menus of apps, and our overlays on top.
    /// The framed windows from the bottom up, windows kept above over the
    /// rest, without a fullscreen group in front, and that group.
    fn stacked(&self) -> (Vec<Window>, Option<Window>) {
        let full = self
            .focused
            .map(|f| self.top_of(f))
            .filter(|t| self.clients.get(t).is_some_and(|c| c.fullscreen));
        let above = |w: &Window| self.clients[&self.top_of(*w)].above;
        let list = self
            .stack
            .iter()
            .filter(|w| !above(w))
            .chain(self.stack.iter().filter(|w| above(w)))
            .copied()
            .filter(|&w| Some(self.top_of(w)) != full)
            .collect();
        (list, full)
    }

    pub(super) fn restack(&self) -> Result<()> {
        let (stack, full) = self.stacked();
        let dragged = self.drag.as_ref().map(|d| d.window());
        let mut order = Vec::with_capacity(self.stack.len() * 2 + 12);
        for w in stack {
            let c = &self.clients[&w];
            if self.snap_rect.is_some() && dragged == Some(w) {
                order.push(self.ours.snap);
            }
            if c.place == Place::Floating {
                order.push(c.grip);
            }
            order.push(c.frame);
        }
        order.push(self.ours.panel);
        order.push(self.ours.rail);
        if let Some(t) = full {
            order.extend(self.group(t).iter().map(|g| self.clients[g].frame));
        }
        order.extend(
            self.order
                .iter()
                .filter(|w| !self.clients[w].framed)
                .copied(),
        );
        order.extend(&self.foreign);
        order.extend(self.ours.notes);
        order.extend([
            self.ours.tooltip,
            self.ours.pop,
            self.ours.ripple,
            self.ours.cursor,
        ]);
        for w in order {
            self.raise(w)?;
        }
        self.conn.configure_window(
            self.ours.empty,
            &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
        )?;
        self.client_list()
    }

    pub(super) fn raise(&self, w: Window) -> Result<()> {
        self.conn
            .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        Ok(())
    }

    fn net_state(&self, w: Window) -> Result<()> {
        let Some(c) = self.clients.get(&w) else {
            return Ok(());
        };
        let a = &self.atoms;
        let mut state = Vec::new();
        if c.fullscreen {
            state.push(a._NET_WM_STATE_FULLSCREEN);
        }
        if c.place == Place::Maximized {
            state.extend([
                a._NET_WM_STATE_MAXIMIZED_VERT,
                a._NET_WM_STATE_MAXIMIZED_HORZ,
            ]);
        }
        if c.minimized {
            state.push(a._NET_WM_STATE_HIDDEN);
        }
        if c.above {
            state.push(a._NET_WM_STATE_ABOVE);
        }
        if c.attention {
            state.push(a._NET_WM_STATE_DEMANDS_ATTENTION);
        }
        if c.modal {
            state.push(a._NET_WM_STATE_MODAL);
        }
        if self.focused == Some(w) {
            state.push(a._NET_WM_STATE_FOCUSED);
        }
        self.conn.change_property32(
            PropMode::REPLACE,
            w,
            a._NET_WM_STATE,
            AtomEnum::ATOM,
            &state,
        )?;
        Ok(())
    }

    pub(super) fn close(&self, w: Window) -> Result<&'static str> {
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

    /// Hides a window and its dialogs; the rail keeps its dot.
    pub(super) fn minimize(&mut self, w: Window) -> Result<()> {
        let top = self.top_of(w);
        if !self.clients.get(&top).is_some_and(|c| c.framed) {
            return Ok(());
        }
        for g in self.group(top) {
            let Some(c) = self.clients.get_mut(&g) else {
                continue;
            };
            if c.minimized || !c.framed {
                continue;
            }
            c.minimized = true;
            let (frame, grip, card) = (c.frame, c.grip, c.card);
            self.conn.unmap_window(frame)?;
            self.conn.unmap_window(grip)?;
            self.moved(frame_of(card));
            let wm_state = self.atoms.WM_STATE;
            self.conn
                .change_property32(PropMode::REPLACE, g, wm_state, wm_state, &[3, 0])?;
            self.net_state(g)?;
        }
        let focused_top = self.focused.map(|f| self.top_of(f));
        if focused_top == Some(top) {
            let key = self.clients[&top].key.clone();
            match self.next_focus(Some(&key)) {
                Some(n) => self.focus(n)?,
                None => self.focus_nothing()?,
            }
        } else {
            self.chrome()?;
        }
        Ok(())
    }

    fn restore(&mut self, w: Window) -> Result<()> {
        let Some(c) = self.clients.get_mut(&w) else {
            return Ok(());
        };
        if !c.minimized {
            return Ok(());
        }
        c.minimized = false;
        let frame = c.frame;
        let wm_state = self.atoms.WM_STATE;
        self.conn
            .change_property32(PropMode::REPLACE, w, wm_state, wm_state, &[1, 0])?;
        self.layout(w)?;
        self.conn.map_window(frame)?;
        Ok(())
    }

    pub(super) fn set_maximized(&mut self, w: Window, on: bool) -> Result<()> {
        let float = self.float_card(w);
        let area = chrome::work_area(self.sw, self.sh);
        let Some(c) = self.clients.get_mut(&w) else {
            return Ok(());
        };
        if !c.framed || (c.place == Place::Maximized) == on {
            return Ok(());
        }
        if on {
            if !c.snapped {
                c.floating = Some(c.card);
            }
            c.snapped = false;
            c.place = Place::Maximized;
            c.card = area;
        } else {
            c.place = Place::Floating;
            c.card = float;
        }
        self.layout(w)?;
        self.restack()
    }

    pub(super) fn toggle_maximized(&mut self, w: Window) -> Result<()> {
        let on = self
            .clients
            .get(&w)
            .is_some_and(|c| c.place == Place::Floating);
        self.set_maximized(w, on)
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
            // From a pager or from the app in front; another app asking
            // for the screen is refused.
            let same = self
                .focused
                .and_then(|f| self.clients.get(&f))
                .is_none_or(|f| f.key == c.key);
            if d[0] == 2 || same || pending.is_some() {
                self.focus(e.window)?;
                if let Some(i) = pending {
                    self.answer_pending(i, e.window)?;
                }
            }
        } else if e.type_ == a._NET_WM_STATE {
            let has = |atom: u32| d[1] == atom || d[2] == atom;
            let (fs, mv, mh) = (
                a._NET_WM_STATE_FULLSCREEN,
                a._NET_WM_STATE_MAXIMIZED_VERT,
                a._NET_WM_STATE_MAXIMIZED_HORZ,
            );
            let (st_above, st_attention, st_modal) = (
                a._NET_WM_STATE_ABOVE,
                a._NET_WM_STATE_DEMANDS_ATTENTION,
                a._NET_WM_STATE_MODAL,
            );
            let Some(c) = self.clients.get(&e.window) else {
                return Ok(());
            };
            let on = |now: bool| match d[0] {
                0 => false,
                1 => true,
                _ => !now,
            };
            let (full, maxed) = (c.fullscreen, c.place == Place::Maximized);
            let (above, attention, modal) = (c.above, c.attention, c.modal);
            if has(st_above) {
                if let Some(c) = self.clients.get_mut(&e.window) {
                    c.above = on(above);
                }
                self.net_state(e.window)?;
                self.restack()?;
            }
            if has(st_modal) {
                if let Some(c) = self.clients.get_mut(&e.window) {
                    c.modal = c.parent.is_some() && on(modal);
                }
                self.net_state(e.window)?;
            }
            if has(st_attention) {
                self.set_attention(e.window, on(attention))?;
            }
            if has(fs) {
                self.set_fullscreen(e.window, on(full))?;
            }
            if has(mv) || has(mh) {
                self.set_maximized(e.window, on(maxed))?;
            }
        } else if e.type_ == a.WM_CHANGE_STATE {
            if d[0] == 3 {
                self.minimize(e.window)?;
            }
        } else if e.type_ == a._NET_CLOSE_WINDOW {
            self.close(e.window)?;
        } else if e.type_ == a._NET_WM_MOVERESIZE {
            self.move_resize_request(e.window, d)?;
        } else if e.type_ == a._NET_REQUEST_FRAME_EXTENTS {
            let frame = a._NET_FRAME_EXTENTS;
            self.conn.change_property32(
                PropMode::REPLACE,
                e.window,
                frame,
                AtomEnum::CARDINAL,
                &[1, 1, 1 + BAR_H as u32, 1],
            )?;
        }
        Ok(())
    }

    /// Marks a window that asks for attention on the rail, until it gets
    /// the focus.
    fn set_attention(&mut self, w: Window, on: bool) -> Result<()> {
        let on = on && self.focused != Some(w);
        let Some(c) = self.clients.get_mut(&w) else {
            return Ok(());
        };
        if c.attention == on {
            return Ok(());
        }
        c.attention = on;
        self.net_state(w)?;
        self.draw_rail()
    }

    fn set_fullscreen(&mut self, w: Window, on: bool) -> Result<()> {
        let Some(c) = self.clients.get_mut(&w) else {
            return Ok(());
        };
        if c.fullscreen == on {
            return Ok(());
        }
        c.fullscreen = on;
        self.layout(w)?;
        self.chrome()
    }

    fn info(&self, w: Window) -> Value {
        let Some(c) = self.clients.get(&w) else {
            return Value::Null;
        };
        let state = if c.minimized {
            "minimized"
        } else if c.place == Place::Maximized {
            "maximized"
        } else {
            "floating"
        };
        let (x, y, width, height) = if c.fullscreen {
            (0, 0, self.sw, self.sh)
        } else if c.framed {
            c.card
        } else {
            (0, 0, 0, 0)
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
            "state": state,
            "geometry": { "x": x, "y": y, "width": width, "height": height },
        })
    }

    pub(super) fn app_name(&self, c: &Client) -> String {
        match c.app {
            Some(i) => self.entries[i].app.name.clone(),
            None if !c.class.is_empty() => c.class.clone(),
            None => c.instance.clone(),
        }
    }

    fn command(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Tree(reply) => {
                let _ = reply.send(Ok(self.shell_tree()));
            }
            Cmd::TreeAction {
                id,
                action,
                value,
                reply,
            } => {
                let out = self.tree_action(id, &action, value);
                let _ = reply.send(out);
            }
            Cmd::Windows(reply) => {
                let list: Vec<Value> = self.mru.iter().map(|&w| self.info(w)).collect();
                let _ = reply.send(Ok(Value::Array(list)));
            }
            Cmd::Focus {
                id,
                maximize,
                reply,
            } => {
                let out = match self.clients.get(&id) {
                    None => Err((404, format!("no window {id}"))),
                    Some(c) if !c.kind.is_app() => {
                        Err((400, format!("window {id} is a {}", c.kind.as_str())))
                    }
                    Some(c) => {
                        let main = c.is_main();
                        self.focus(id)?;
                        if maximize && main {
                            self.set_maximized(id, true)?;
                        }
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
                maximize,
                reply,
            } => {
                let configured: Vec<App> = self.entries.iter().map(|e| e.app.clone()).collect();
                let Some(found) = apps::find(&configured, &app) else {
                    let _ = reply.send(Err((404, format!("no app {app}"))));
                    return Ok(());
                };
                match found
                    .command(&args)
                    .and_then(|argv| self.spawn(&argv, &found.id))
                {
                    Ok(pid) => self.pending.push(Pending {
                        app: found,
                        pid,
                        deadline: Instant::now() + timeout,
                        maximize,
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
            Cmd::Appearance(set, reply) => {
                if let Some(dark) = set {
                    if let Err(e) = appearance::write(dark) {
                        let _ = reply.send(Err((500, format!("writing the appearance: {e}"))));
                        return Ok(());
                    }
                    self.set_appearance(dark)?;
                }
                let _ = reply.send(Ok(
                    json!({ "appearance": appearance::name(draw::is_dark()) }),
                ));
            }
            Cmd::Notify(note, reply) => {
                let id = self.notify(*note)?;
                let _ = reply.send(Ok(json!({ "id": id })));
            }
            Cmd::CloseNote(id, reply) => {
                self.close_note(id, super::notes::CLOSED)?;
                let _ = reply.send(Ok(json!({ "ok": true })));
            }
            Cmd::Notifications(reply) => {
                let _ = reply.send(Ok(self.notes_json()));
            }
            Cmd::NoteAction { id, key, reply } => {
                let answer = if self.note_action(id, &key)? {
                    Ok(json!({ "ok": true }))
                } else {
                    Err((404, format!("no open notification {id} with action {key}")))
                };
                let _ = reply.send(answer);
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

    pub(super) fn spawn(&self, argv: &[String], id: &str) -> Result<u32> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env("DISPLAY", &self.display)
            .env("LANG", crate::locale::messages_locale())
            .env("LC_ALL", crate::locale::messages_locale())
            .env("QT_ACCESSIBILITY", "1")
            .env("GNOME_ACCESSIBILITY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting {}", argv[0]))?;
        let pid = child.id();
        eprintln!("shuru-desktop: launched {id} as pid {pid}");
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

    /// Starts an app for a person: no reply waits for its window.
    pub(super) fn start_app(&mut self, app: App, argv: Result<Vec<String>>) {
        match argv.and_then(|argv| self.spawn(&argv, &app.id)) {
            Ok(pid) => self.pending.push(Pending {
                app,
                pid,
                deadline: Instant::now() + Duration::from_secs(30),
                maximize: false,
                reply: None,
            }),
            Err(e) => eprintln!("shuru-desktop: {e:#}"),
        }
    }

    /// The windows of a rail target, most recent first.
    pub(super) fn windows_of(&self, t: &Target) -> Vec<Window> {
        let key = match t {
            Target::App(i) => format!("app:{i}"),
            Target::Key(k) => k.clone(),
        };
        self.mru
            .iter()
            .copied()
            .filter(|w| {
                let c = &self.clients[w];
                c.key == key && c.is_main()
            })
            .collect()
    }

    /// A target that is not running starts; one that is comes to the front
    /// with its most recent window.
    pub(super) fn activate(&mut self, t: &Target) -> Result<()> {
        match self.windows_of(t).first() {
            Some(&w) => self.focus(w),
            None => {
                if let Target::App(i) = t {
                    let app = self.entries[*i].app.clone();
                    let argv = app.command(&[]);
                    self.start_app(app, argv);
                }
                Ok(())
            }
        }
    }

    fn timers(&mut self) -> Result<()> {
        let now = Instant::now();
        if self.cursor_shown && self.cursor_poll <= now {
            self.cursor_moved = true;
            let idle = self.cursor_active.elapsed() >= CURSOR_IDLE;
            self.cursor_poll = now + if idle { CURSOR_POLL_IDLE } else { CURSOR_POLL };
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
        self.expire_notes(now)?;
        Ok(())
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .iter()
            .map(|p| p.deadline)
            .chain(self.ripple_until)
            .chain(self.cursor_shown.then_some(self.cursor_poll))
            .chain(self.notes.deadline())
            .min()
    }

    /// The screen changed size: the shell follows, maximized and snapped
    /// windows fill their new places and floating ones stay on screen.
    fn screen_changed(&mut self, w: u16, h: u16) -> Result<()> {
        if (w, h) == (self.sw, self.sh) {
            return Ok(());
        }
        self.sw = w;
        self.sh = h;
        self.close_pop()?;
        self.fit_screen()?;
        let (wx, wy, ww, wh) = chrome::work_area(w, h);
        let wins: Vec<Window> = self.clients.keys().copied().collect();
        for win in wins {
            let snap = self.clients[&win].snapped;
            let card = self.clients[&win].card;
            let fitted = if snap {
                let left = card.0 <= wx + 1;
                let half = ww / 2;
                let x = if left { wx } else { wx + half as i16 };
                (x, wy, if left { half } else { ww - half }, wh)
            } else {
                let cw = card.2.min(ww);
                let ch = card.3.min(wh);
                let x = card.0.clamp(wx, wx + (ww - cw) as i16);
                let y = card.1.clamp(wy, wy + (wh - ch) as i16);
                (x, y, cw, ch)
            };
            if let Some(c) = self.clients.get_mut(&win) {
                match c.place {
                    Place::Maximized => c.card = (wx, wy, ww, wh),
                    Place::Floating => c.card = fitted,
                }
                if let Some(f) = c.floating.as_mut() {
                    let cw = f.2.min(ww);
                    let ch = f.3.min(wh);
                    *f = (
                        f.0.clamp(wx, wx + (ww - cw) as i16),
                        f.1.clamp(wy, wy + (wh - ch) as i16),
                        cw,
                        ch,
                    );
                }
            }
            if self.clients[&win].framed {
                self.layout(win)?;
            }
        }
        self.layout_notes()?;
        self.settle.changed();
        self.chrome()
    }

    /// A card size the client takes: within its largest size and on its
    /// size steps.
    pub(super) fn hinted(&self, w: Window, (cw, ch): (u16, u16)) -> (u16, u16) {
        let Some(c) = self.clients.get(&w) else {
            return (cw, ch);
        };
        let bar = c.bar_h();
        let fit = |v: u16, max: u16, base: u16, inc: u16| {
            let v = if max > 0 { v.min(max) } else { v };
            if inc > 1 && v > base {
                base + (v - base) / inc * inc
            } else {
                v
            }
        };
        let (mw, mh) = self.min_card(w);
        let width = fit(cw, c.max.0, c.base.0, c.inc.0).max(mw.min(cw));
        let height =
            (fit(ch.saturating_sub(bar), c.max.1, c.base.1, c.inc.1) + bar).max(mh.min(ch));
        (width, height)
    }

    /// The smallest card a window may have.
    pub(super) fn min_card(&self, w: Window) -> (u16, u16) {
        self.clients.get(&w).map_or((MIN_W, MIN_H), |c| {
            (c.min.0.max(MIN_W), (c.min.1 + c.bar_h()).max(MIN_H))
        })
    }
}

/// Whether WM_HINTS carry the urgency hint.
fn urgent(hints: &[u32]) -> bool {
    hints.first().is_some_and(|f| f & 256 != 0)
}

/// COMPOUND_TEXT as far as it can be read without charset tables: ASCII,
/// Latin-1, and the UTF-8 segments Xutf8 clients write. A run of text in
/// another charset becomes one U+FFFD.
fn compound_text(v: &[u8]) -> String {
    let mut out = String::new();
    let mut utf8 = Vec::new();
    let (mut in_utf8, mut gl_ok, mut gr_ok, mut i) = (false, true, true, 0);
    let unknown = |out: &mut String| {
        if !out.ends_with('\u{fffd}') {
            out.push('\u{fffd}');
        }
    };
    while i < v.len() {
        if v[i] == 0x1b {
            if in_utf8 {
                out.push_str(&String::from_utf8_lossy(&utf8));
                utf8.clear();
            }
            let rest = &v[i + 1..];
            let n = rest
                .iter()
                .position(|b| (0x30..=0x7e).contains(b))
                .map_or(rest.len(), |p| p + 1);
            let seq = &rest[..n];
            in_utf8 = seq == b"%G";
            match seq {
                b"(B" => gl_ok = true,
                b"-A" => gr_ok = true,
                b"%@" | b"%G" => {}
                [b'(', ..] | [b'$', b'(', ..] | [b'$', b'B'] | [b'$', b'A'] => gl_ok = false,
                _ => gr_ok = false,
            }
            i += 1 + n;
            continue;
        }
        let b = v[i];
        if in_utf8 {
            utf8.push(b);
        } else if (b < 0x80 && (gl_ok || b < 0x20)) || (b >= 0xa0 && gr_ok) {
            out.push(b as char);
        } else {
            unknown(&mut out);
        }
        i += 1;
    }
    out.push_str(&String::from_utf8_lossy(&utf8));
    out
}

#[cfg(test)]
mod tests {
    use super::compound_text;

    #[test]
    fn compound_text_reads_latin_and_utf8_segments() {
        let mut v = b"Release \x1b%G".to_vec();
        v.extend("\u{1f680}".as_bytes());
        v.extend(b"\x1b%@ caf\xe9");
        assert_eq!(compound_text(&v), "Release \u{1f680} caf\u{e9}");
        assert_eq!(
            compound_text(b"log: \x1b$(A99=(\x1b(B ok"),
            "log: \u{fffd} ok"
        );
    }
}
