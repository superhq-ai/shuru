//! A person at the desktop: clicks on the panel, the rail and title bars,
//! moving, snapping and resizing windows, and the keys of the desktop.

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::{CURRENT_TIME, NONE};

use super::{Place, Target, Wm, R};
use crate::chrome::{self, Light, BAR_H, PANEL_H};

const DRAG_START: i32 = 4;
const DOUBLE_CLICK_MS: u32 = 400;

#[derive(Default)]
pub(super) struct Keys {
    tab: Option<Keycode>,
    grave: Option<Keycode>,
    alts: Vec<Keycode>,
    supers: Vec<Keycode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Snap {
    Max,
    Left,
    Right,
}

pub(super) enum Drag {
    Move {
        win: Window,
        /// The pointer in the card.
        off: (i16, i16),
        start: (i16, i16),
        started: bool,
        snap: Option<Snap>,
    },
    Resize {
        win: Window,
        edges: [bool; 4],
        start: (i16, i16),
        card: R,
    },
    Light {
        win: Window,
        light: Light,
    },
}

impl Drag {
    pub(super) fn window(&self) -> Window {
        match self {
            Drag::Move { win, .. } | Drag::Resize { win, .. } | Drag::Light { win, .. } => *win,
        }
    }
}

impl Wm {
    fn grab_pointer(&self, cursor: Cursor) -> Result<bool> {
        let reply = self
            .conn
            .grab_pointer(
                false,
                self.root,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                cursor,
                CURRENT_TIME,
            )?
            .reply()?;
        Ok(reply.status == GrabStatus::SUCCESS)
    }

    pub(super) fn end_drag(&mut self) -> Result<()> {
        if self.drag.take().is_none() {
            return Ok(());
        }
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        self.show_snap(None)?;
        self.restack()
    }

    pub(super) fn button_press(&mut self, e: &ButtonPressEvent) -> Result<()> {
        if self.pop.is_some() {
            return self.pop_press(e);
        }
        let w = e.event;
        if let Some(slot) = self.note_slot(w) {
            return self.note_press(slot, e.event_x, e.event_y);
        }
        if w == self.ours.panel {
            if e.detail == 1 {
                if let Some(hit) = self.panel_hit(e.event_x as f32, e.event_y as f32) {
                    self.panel_click(hit)?;
                }
            }
        } else if w == self.ours.rail {
            self.show_tooltip(None)?;
            let hit = self.rail_hit(e.event_x as f32, e.root_y as f32);
            if let Some(i) = hit {
                self.rail_click(i, e.detail)?;
            }
        } else if let Some(&client) = self.frames.get(&w) {
            if e.detail == 1 {
                self.frame_press(client, w, e)?;
            }
        } else {
            self.conn.allow_events(Allow::REPLAY_POINTER, e.time)?;
            let app = self.clients.get(&w).is_some_and(|c| c.kind.is_app());
            if app && self.focused != Some(w) {
                self.focus(w)?;
            }
        }
        Ok(())
    }

    pub(super) fn front_target(&self) -> Option<Target> {
        let c = self.clients.get(&self.top_of(self.focused?))?;
        Some(match c.app {
            Some(i) => Target::App(i),
            None => Target::Key(c.key.clone()),
        })
    }

    pub(super) fn light_click(&mut self, win: Window, light: Light) -> Result<()> {
        match light {
            Light::Close => {
                self.close(win)?;
                Ok(())
            }
            Light::Minimize => self.minimize(win),
            Light::Maximize => self.toggle_maximized(win),
        }
    }

    fn title_hit(&self, win: Window, x: f32, y: f32) -> Option<Light> {
        let tree = self.bar_trees.get(&win)?;
        let c = self.clients.get(&win)?;
        let (w, h) = tree.size();
        let id = tree.hit_node(crate::draw::Rect::new(-1.0, -1.0, w, h), x, y)?;
        chrome::title_light(win, c.parent.is_some() || c.kind != super::Kind::Normal, id)
    }

    fn frame_press(&mut self, client: Window, w: Window, e: &ButtonPressEvent) -> Result<()> {
        let Some(c) = self.clients.get(&client) else {
            return Ok(());
        };
        let card = c.card;
        let is_grip = w == c.grip;
        let bar = c.bar_h() > 0;
        if self.focused != Some(client) {
            self.focus(client)?;
        }
        let (x, y) = (e.root_x, e.root_y);
        if is_grip {
            let edges = Self::edges_at(card, x, y);
            if self.grab_pointer(self.edge_cursor(edges))? {
                self.drag = Some(Drag::Resize {
                    win: client,
                    edges,
                    start: (x, y),
                    card,
                });
            }
            return Ok(());
        }
        let (cx, cy) = ((x - card.0) as f32, (y - card.1) as f32);
        if !bar || cy < 0.0 || cy >= BAR_H as f32 {
            return Ok(());
        }
        if let Some(light) = self.title_hit(client, cx, cy) {
            if self.grab_pointer(self.cursors.arrow)? {
                self.drag = Some(Drag::Light { win: client, light });
                if let Some(c) = self.clients.get_mut(&client) {
                    c.pressed = Some(light);
                }
                self.draw_frame(client)?;
            }
            return Ok(());
        }
        let double = self
            .last_bar_click
            .is_some_and(|(lw, t)| lw == client && e.time.wrapping_sub(t) < DOUBLE_CLICK_MS);
        if double {
            self.last_bar_click = None;
            if self.clients.get(&client).is_some_and(|c| c.is_main()) {
                self.toggle_maximized(client)?;
            }
            return Ok(());
        }
        self.last_bar_click = Some((client, e.time));
        self.start_move(client, x, y)
    }

    fn start_move(&mut self, win: Window, x: i16, y: i16) -> Result<()> {
        let Some(c) = self.clients.get(&win) else {
            return Ok(());
        };
        let off = (x - c.card.0, y - c.card.1);
        if self.grab_pointer(self.cursors.moving)? {
            self.drag = Some(Drag::Move {
                win,
                off,
                start: (x, y),
                started: false,
                snap: None,
            });
            self.drop_if_released()?;
        }
        Ok(())
    }

    /// A move or resize asked for after the button went up ends at once.
    fn drop_if_released(&mut self) -> Result<()> {
        let p = self.conn.query_pointer(self.root)?.reply()?;
        let buttons = u16::from(KeyButMask::BUTTON1)
            | u16::from(KeyButMask::BUTTON2)
            | u16::from(KeyButMask::BUTTON3);
        if u16::from(p.mask) & buttons == 0 {
            self.end_drag()?;
        }
        Ok(())
    }

    /// `_NET_WM_MOVERESIZE` from a window that draws its own title bar.
    pub(super) fn move_resize_request(&mut self, win: Window, d: [u32; 5]) -> Result<()> {
        let Some(c) = self.clients.get(&win).filter(|c| c.framed && !c.fullscreen) else {
            return Ok(());
        };
        let (x, y) = (d[0] as i16, d[1] as i16);
        let (card, maximized) = (c.card, c.place == Place::Maximized);
        if d[2] == 11 {
            return self.end_drag();
        }
        if self.drag.is_some() {
            return Ok(());
        }
        if self.focused != Some(win) {
            self.focus(win)?;
        }
        if d[2] == 8 {
            return self.start_move(win, x, y);
        }
        // Top left, top, top right, right, bottom right, bottom, bottom
        // left, left.
        let edges = match d[2] {
            0 => [true, false, true, false],
            1 => [false, false, true, false],
            2 => [false, true, true, false],
            3 => [false, true, false, false],
            4 => [false, true, false, true],
            5 => [false, false, false, true],
            6 => [true, false, false, true],
            7 => [true, false, false, false],
            _ => return Ok(()),
        };
        if maximized {
            return Ok(());
        }
        if self.grab_pointer(self.edge_cursor(edges))? {
            self.drag = Some(Drag::Resize {
                win,
                edges,
                start: (x, y),
                card,
            });
            self.drop_if_released()?;
        }
        Ok(())
    }

    fn snap_target(&self, x: i16, y: i16) -> Option<(Snap, R)> {
        let (wx, wy, ww, wh) = chrome::work_area(self.sw, self.sh);
        let half = (ww - 8) / 2;
        if y < wy {
            Some((Snap::Max, (wx, wy, ww, wh)))
        } else if x < wx + 8 {
            Some((Snap::Left, (wx, wy, half, wh)))
        } else if x >= wx + ww as i16 - 8 {
            Some((Snap::Right, (wx + (ww - half) as i16, wy, half, wh)))
        } else {
            None
        }
    }

    fn drag_motion(&mut self, x: i16, y: i16) -> Result<()> {
        match self.drag {
            Some(Drag::Move {
                win,
                off,
                start,
                started,
                ..
            }) => {
                if !started {
                    let (dx, dy) = ((x - start.0) as i32, (y - start.1) as i32);
                    if dx * dx + dy * dy < DRAG_START * DRAG_START {
                        return Ok(());
                    }
                    self.unsnap(win, off)?;
                }
                let Some(c) = self.clients.get(&win) else {
                    return self.end_drag();
                };
                let off = match self.drag {
                    Some(Drag::Move { off, .. }) => off,
                    _ => off,
                };
                let mut card = c.card;
                card.0 = x - off.0;
                card.1 = (y - off.1).max(PANEL_H as i16);
                let snap = if c.is_main() {
                    self.snap_target(x, y)
                } else {
                    None
                };
                if let Some(c) = self.clients.get_mut(&win) {
                    c.card = card;
                }
                if let Some(Drag::Move {
                    snap: s, started, ..
                }) = &mut self.drag
                {
                    *s = snap.map(|(k, _)| k);
                    *started = true;
                }
                self.layout(win)?;
                self.show_snap(snap.map(|(_, r)| r))?;
            }
            Some(Drag::Resize {
                win,
                edges,
                start,
                card,
            }) => {
                let (dx, dy) = (x - start.0, y - start.1);
                let (mw, mh) = self.min_card(win);
                let (l, t) = (card.0 as i32, card.1 as i32);
                let (r, b) = (l + card.2 as i32, t + card.3 as i32);
                let mut n = (l, t, r, b);
                if edges[0] {
                    n.0 = (l + dx as i32).min(r - mw as i32);
                }
                if edges[1] {
                    n.2 = (r + dx as i32).max(l + mw as i32);
                }
                if edges[2] {
                    n.1 = (t + dy as i32).clamp(PANEL_H as i32, b - mh as i32);
                }
                if edges[3] {
                    n.3 = (b + dy as i32).max(t + mh as i32);
                }
                let (w, h) = self.hinted(win, ((n.2 - n.0) as u16, (n.3 - n.1) as u16));
                if edges[0] {
                    n.0 = n.2 - w as i32;
                } else {
                    n.2 = n.0 + w as i32;
                }
                if edges[2] {
                    n.1 = n.3 - h as i32;
                } else {
                    n.3 = n.1 + h as i32;
                }
                if let Some(c) = self.clients.get_mut(&win) {
                    c.card = (
                        n.0 as i16,
                        n.1 as i16,
                        (n.2 - n.0) as u16,
                        (n.3 - n.1) as u16,
                    );
                    c.snapped = false;
                }
                self.layout(win)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The first move of a maximized or snapped window: it takes its
    /// floating size back under the pointer.
    fn unsnap(&mut self, win: Window, off: (i16, i16)) -> Result<()> {
        let float = self.float_card(win);
        let Some(c) = self.clients.get_mut(&win) else {
            return Ok(());
        };
        if c.place == Place::Maximized || c.snapped {
            let fx = (off.0 as i32 * float.2 as i32 / c.card.2.max(1) as i32) as i16;
            c.place = Place::Floating;
            c.snapped = false;
            c.card = (c.card.0 + off.0 - fx, c.card.1, float.2, float.3);
            if let Some(Drag::Move { off: o, .. }) = &mut self.drag {
                o.0 = fx;
            }
        }
        if let Some(Drag::Move { started, .. }) = &mut self.drag {
            *started = true;
        }
        Ok(())
    }

    pub(super) fn button_release(&mut self, e: &ButtonReleaseEvent) -> Result<()> {
        self.pop_release();
        let Some(drag) = self.drag.take() else {
            return Ok(());
        };
        self.drag = Some(drag);
        let (x, y) = (e.root_x, e.root_y);
        match self.drag {
            Some(Drag::Move {
                win, snap, started, ..
            }) => {
                self.end_drag()?;
                if !started {
                    return Ok(());
                }
                let before = self.clients.get(&win).map(|c| c.card);
                match snap {
                    Some(Snap::Max) => {
                        self.set_maximized(win, true)?;
                    }
                    Some(s) => {
                        if let Some((_, r)) = self.snap_target(x, y).filter(|(k, _)| *k == s) {
                            if let Some(c) = self.clients.get_mut(&win) {
                                c.floating = before;
                                c.card = r;
                                c.snapped = true;
                            }
                            self.layout(win)?;
                        }
                    }
                    None => {
                        if let Some(c) = self.clients.get_mut(&win) {
                            c.floating = Some(c.card);
                        }
                        self.layout(win)?;
                    }
                }
                self.chrome()?;
            }
            Some(Drag::Resize { win, .. }) => {
                self.end_drag()?;
                if let Some(c) = self.clients.get_mut(&win) {
                    c.floating = Some(c.card);
                }
            }
            Some(Drag::Light { win, light }) => {
                self.end_drag()?;
                let over = self.clients.get(&win).and_then(|c| {
                    self.title_hit(win, (x - c.card.0) as f32, (y - c.card.1) as f32)
                });
                if let Some(c) = self.clients.get_mut(&win) {
                    c.pressed = None;
                }
                self.draw_frame(win)?;
                if over == Some(light) {
                    self.light_click(win, light)?;
                }
            }
            None => {}
        }
        Ok(())
    }

    pub(super) fn motion(&mut self, e: &MotionNotifyEvent) -> Result<()> {
        if self.drag.is_some() {
            return self.drag_motion(e.root_x, e.root_y);
        }
        if self.pop.is_some() {
            return self.pop_motion(e.root_x, e.root_y);
        }
        let w = e.event;
        if w == self.ours.panel {
            let hover = self.panel_hit(e.event_x as f32, e.event_y as f32);
            let cursor = self.control_cursor(hover.is_some());
            self.set_cursor(w, cursor)?;
            if hover != self.panel_hover {
                self.panel_hover = hover;
                self.draw_panel()?;
            }
        } else if w == self.ours.rail {
            let hover = self.rail_hit(e.event_x as f32, e.root_y as f32);
            let cursor = self.control_cursor(hover.is_some());
            self.set_cursor(w, cursor)?;
            if hover != self.rail_hover {
                self.rail_hover = hover;
                self.draw_rail()?;
            }
            self.show_tooltip(hover)?;
        } else if let Some(&client) = self.frames.get(&w) {
            let Some(c) = self.clients.get(&client) else {
                return Ok(());
            };
            if w == c.grip {
                let edges = Self::edges_at(c.card, e.root_x, e.root_y);
                let cursor = self.edge_cursor(edges);
                return self.set_cursor(w, cursor);
            }
            let close_only = c.parent.is_some() || c.kind != super::Kind::Normal;
            let hover = c.bar_h() > 0
                && chrome::over_lights(
                    close_only,
                    (e.root_x - c.card.0) as f32,
                    (e.root_y - c.card.1) as f32,
                );
            if hover != c.hover {
                if let Some(c) = self.clients.get_mut(&client) {
                    c.hover = hover;
                }
                self.draw_frame(client)?;
            }
        }
        Ok(())
    }

    fn control_cursor(&self, on: bool) -> Cursor {
        if on {
            self.cursors.pointer
        } else {
            self.cursors.arrow
        }
    }

    /// Gives a window a cursor, when it does not have it already.
    pub(super) fn set_cursor(&mut self, w: Window, cursor: Cursor) -> Result<()> {
        if self.cursor_of.get(&w) != Some(&cursor) {
            self.cursor_of.insert(w, cursor);
            self.conn
                .change_window_attributes(w, &ChangeWindowAttributesAux::new().cursor(cursor))?;
        }
        Ok(())
    }

    /// Changes the cursor of the pointer grab a menu holds.
    pub(super) fn set_grab_cursor(&mut self, cursor: Cursor) -> Result<()> {
        if self.grab_cursor != cursor {
            self.grab_cursor = cursor;
            self.conn.change_active_pointer_grab(
                cursor,
                CURRENT_TIME,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            )?;
        }
        Ok(())
    }

    pub(super) fn leave(&mut self, w: Window) -> Result<()> {
        if self.drag.is_some() || self.pop.is_some() {
            return Ok(());
        }
        if w == self.ours.panel {
            if self.panel_hover.take().is_some() {
                self.draw_panel()?;
            }
        } else if w == self.ours.rail {
            self.show_tooltip(None)?;
            if self.rail_hover.take().is_some() {
                self.draw_rail()?;
            }
        } else if let Some(&client) = self.frames.get(&w) {
            if let Some(c) = self.clients.get_mut(&client).filter(|c| c.hover) {
                c.hover = false;
                self.draw_frame(client)?;
            }
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
        self.keys = Keys::default();
        for (i, syms) in map.keysyms.chunks(per).enumerate() {
            let code = min + i as u8;
            let first = |k: &mut Option<Keycode>, sym: u32| {
                if syms.first() == Some(&sym) && k.is_none() {
                    *k = Some(code);
                }
            };
            first(&mut self.keys.tab, 0xff09);
            first(&mut self.keys.grave, 0x60);
            if syms.contains(&0xffe9) || syms.contains(&0xffea) {
                self.keys.alts.push(code);
            }
            if syms.contains(&0xffeb) || syms.contains(&0xffec) {
                self.keys.supers.push(code);
            }
        }
        self.conn.ungrab_key(Grab::ANY, self.root, ModMask::ANY)?;
        let (lock, num) = (u16::from(ModMask::LOCK), u16::from(ModMask::M2));
        let m1 = u16::from(ModMask::M1);
        let mut grabs: Vec<(Keycode, u16)> = Vec::new();
        for code in [self.keys.tab, self.keys.grave].into_iter().flatten() {
            grabs.push((code, m1));
        }
        for &code in &self.keys.supers {
            grabs.push((code, 0));
        }
        for (code, mods) in grabs {
            for extra in [0, lock, num, lock | num] {
                self.conn.grab_key(
                    true,
                    self.root,
                    ModMask::from(mods | extra),
                    code,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn key_press(&mut self, e: &KeyPressEvent) -> Result<()> {
        if self.pop.is_some() {
            return self.pop_key(e);
        }
        let alt = u16::from(e.state) & u16::from(ModMask::M1) != 0;
        if self.keys.supers.contains(&e.detail) && self.switching.is_none() {
            return self.open_apps_menu();
        }
        if !alt {
            return Ok(());
        }
        let apps = Some(e.detail) == self.keys.tab;
        let windows = Some(e.detail) == self.keys.grave;
        if !apps && !windows {
            return Ok(());
        }
        if self.switching.is_none() {
            let list = if apps {
                self.app_cycle()
            } else {
                self.window_cycle()
            };
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
            self.switching = Some((list, 0));
        }
        let back = u16::from(e.state) & u16::from(ModMask::SHIFT) != 0;
        let next = self.switching.as_mut().map(|(list, i)| {
            *i = if back {
                (*i + list.len() - 1) % list.len()
            } else {
                (*i + 1) % list.len()
            };
            list[*i]
        });
        if let Some(w) = next {
            self.focus(w)?;
        }
        Ok(())
    }

    pub(super) fn is_super(&self, code: Keycode) -> bool {
        self.keys.supers.contains(&code)
    }

    pub(super) fn key_release(&mut self, e: &KeyReleaseEvent) -> Result<()> {
        if self.switching.is_some() && self.keys.alts.contains(&e.detail) {
            self.switching = None;
            self.conn.ungrab_keyboard(CURRENT_TIME)?;
        }
        Ok(())
    }

    /// Alt+Tab: one window an app, the most recent of each, apps in the
    /// order they were last used.
    fn app_cycle(&self) -> Vec<Window> {
        let mut seen: Vec<&str> = Vec::new();
        let mut list = Vec::new();
        for &w in &self.mru {
            let c = &self.clients[&w];
            if c.kind.is_app() && !seen.contains(&c.key.as_str()) {
                seen.push(&c.key);
                list.push(w);
            }
        }
        list
    }

    /// Alt+`: the windows of the app in front that are not minimized.
    fn window_cycle(&self) -> Vec<Window> {
        let Some(key) = self
            .focused
            .and_then(|f| self.clients.get(&f))
            .map(|c| c.key.clone())
        else {
            return Vec::new();
        };
        self.mru
            .iter()
            .copied()
            .filter(|w| {
                let c = &self.clients[w];
                c.key == key && c.is_main() && !c.minimized
            })
            .collect()
    }
}
