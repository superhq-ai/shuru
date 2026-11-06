//! Our own windows: the panel, the rail, the title bars and corners of
//! frames, the empty desktop line, the tooltip, the snap preview, the
//! click ripple and the cursor.

use std::time::{Duration, Instant};

use anyhow::Result;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

use super::{within, Entry, Place, Target, Who, Wm, R};
use crate::appearance;
use crate::chrome::{
    self, BarView, PanelView, RailItem, CORNER, ITEM, PANEL_H, RAIL_W, RIPPLE, TOOLTIP_H,
};
use crate::draw::{self, theme, Canvas};

impl Wm {
    pub(super) fn moved(&mut self, old: R) {
        if old.2 > 0 && old.3 > 0 {
            self.recent.push((Instant::now(), old));
        }
    }

    /// The parts of the screen we draw: panel, rail, overlays, menus, and
    /// the bars, rings and corners of frames.
    fn own_rects(&self) -> Vec<R> {
        let mut ours = vec![
            (0, 0, self.sw, PANEL_H),
            (0, PANEL_H as i16, RAIL_W, self.rail_h),
        ];
        let mut add = |shown: bool, r: R| {
            if shown {
                ours.push(r);
            }
        };
        add(self.empty_shown, self.empty_rect);
        add(self.tooltip_for.is_some(), self.tooltip_rect);
        add(self.cursor_shown, self.cursor_rect);
        add(self.ripple_until.is_some(), self.ripple_rect);
        if let Some(p) = &self.pop {
            ours.push(p.rect);
        }
        if let Some(s) = self.snap_rect {
            ours.push(s);
        }
        ours.extend(self.note_rects());
        for c in self.clients.values() {
            if !c.framed || c.fullscreen || c.minimized {
                continue;
            }
            let (x, y, w, h) = c.card;
            let s = CORNER;
            ours.extend([
                (x - 1, y - 1, w + 2, c.bar_h() + 1),
                (x - 1, y - 1, 1, h + 2),
                (x + w as i16, y - 1, 1, h + 2),
                (x - 1, y + h as i16, w + 2, 1),
                (x - 1, y - 1, s, s),
                (x + w as i16 + 1 - s as i16, y - 1, s, s),
                (x - 1, y + h as i16 + 1 - s as i16, s, s),
                (
                    x + w as i16 + 1 - s as i16,
                    y + h as i16 + 1 - s as i16,
                    s,
                    s,
                ),
            ]);
        }
        ours
    }

    /// Whether damage falls inside one of our own windows, now or lately.
    pub(super) fn own_damage(&self, r: R) -> bool {
        self.own_rects()
            .iter()
            .chain(self.recent.iter().map(|(_, r)| r))
            .any(|&o| within(r, o))
    }

    pub(super) fn put(&self, win: Window, c: &Canvas, x: i16, y: i16) -> Result<()> {
        if c.w == 0 || c.h == 0 {
            return Ok(());
        }
        let rows = (200_000 / (c.w * 4)).max(1);
        let bytes = c.bytes();
        for y0 in (0..c.h).step_by(rows) {
            let n = rows.min(c.h - y0);
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                win,
                self.ours.gc,
                c.w as u16,
                n as u16,
                x,
                y + y0 as i16,
                0,
                self.depth,
                &bytes[y0 * c.w * 4..(y0 + n) * c.w * 4],
            )?;
        }
        Ok(())
    }

    pub(super) fn shape(&self, win: Window, mask: &Canvas) -> Result<()> {
        let rects: Vec<Rectangle> =
            draw::mask_rects(mask.w, mask.h, |x, y| chrome::inside(mask, x, y))
                .into_iter()
                .map(|(x, y, width)| Rectangle {
                    x,
                    y,
                    width,
                    height: 1,
                })
                .collect();
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::YX_BANDED,
            win,
            0,
            0,
            &rects,
        )?;
        Ok(())
    }

    pub(super) fn place_window(&self, w: Window, r: R) -> Result<()> {
        self.conn.configure_window(
            w,
            &ConfigureWindowAux::new()
                .x(r.0 as i32)
                .y(r.1 as i32)
                .width(r.2 as u32)
                .height(r.3 as u32),
        )?;
        Ok(())
    }

    pub(super) fn redraw(&mut self, w: Window) -> Result<()> {
        let o = &self.ours;
        if w == o.panel {
            self.draw_panel()
        } else if w == o.rail {
            self.draw_rail()
        } else if w == o.empty {
            self.draw_empty()
        } else if w == o.tooltip {
            self.draw_tooltip()
        } else if w == o.pop {
            self.draw_pop()
        } else if w == o.snap {
            self.draw_snap()
        } else if w == o.cursor {
            match &self.cursor_px {
                Some(c) => self.put(w, c, 0, 0),
                None => Ok(()),
            }
        } else if let Some(slot) = self.note_slot(w) {
            self.expose_note(slot)
        } else if w == o.ripple {
            let (c, _) = chrome::ripple(self.ripple_person);
            self.put(w, &c, 0, 0)
        } else if let Some(&client) = self.frames.get(&w) {
            self.draw_frame(client)
        } else {
            Ok(())
        }
    }

    /// Our bar and the corners of a frame.
    pub(super) fn draw_frame(&mut self, win: Window) -> Result<()> {
        let ring = self.ring_color(win);
        let Some(c) = self.clients.get(&win) else {
            return Ok(());
        };
        if !c.framed || c.fullscreen {
            return Ok(());
        }
        let (frame, corners) = (c.frame, c.corners.clone());
        if c.bar_h() > 0 {
            let view = BarView {
                title: &c.title,
                focused: self.focused == Some(win),
                ring,
                close_only: c.parent.is_some() || c.kind != super::Kind::Normal,
                maximized: c.place == Place::Maximized,
                hover: c.hover,
                pressed: c.pressed,
            };
            let tree = chrome::title_tree(&mut self.font, win, c.card.2 as usize, &view);
            let (w, h) = tree.size();
            let mut canvas = Canvas::new(w as usize, h as usize, theme().bg);
            tree.paint(
                &mut canvas,
                &mut crate::ui::widgets::Paint {
                    font: &mut self.font,
                    icons: &|_| None,
                    field: None,
                },
                draw::Rect::new(0.0, 0.0, w, h),
            );
            self.bar_trees.insert(win, tree);
            self.put(frame, &canvas, 0, 0)?;
        }
        for (k, cw) in corners {
            let (canvas, _) = chrome::corner(k, ring);
            self.put(cw, &canvas, 0, 0)?;
        }
        Ok(())
    }

    /// Shows, hides and redraws the panel, rail, empty line and toast,
    /// then restacks.
    /// Switches the shell to dark or light and redraws all of it.
    pub(super) fn set_appearance(&mut self, dark: bool) -> Result<()> {
        if dark == draw::is_dark() {
            return Ok(());
        }
        draw::set_dark(dark);
        let t = draw::theme();
        let o = &self.ours;
        let bgs = [
            (self.root, t.bg),
            (o.panel, t.bg),
            (o.rail, t.bg),
            (o.empty, t.bg),
            (o.ripple, t.bg),
            (o.tooltip, t.raised),
            (o.pop, t.surface),
            (o.snap, t.snap),
            (o.notes[0], t.surface),
            (o.notes[1], t.surface),
            (o.notes[2], t.surface),
        ];
        for (w, px) in bgs {
            self.conn.change_window_attributes(
                w,
                &ChangeWindowAttributesAux::new().background_pixel(px),
            )?;
        }
        self.conn.clear_area(false, self.root, 0, 0, 0, 0)?;
        let o = &self.ours;
        let ours = [o.panel, o.rail, o.empty, o.tooltip, o.pop, o.snap, o.ripple];
        for w in ours {
            self.redraw(w)?;
        }
        let framed: Vec<Window> = self
            .clients
            .iter()
            .filter(|(_, c)| c.framed)
            .map(|(&w, _)| w)
            .collect();
        for w in framed {
            self.ring(w)?;
        }
        self.layout_notes()?;
        self.publish_appearance()?;
        self.bus_send(crate::bus::Event::Appearance(dark));
        self.settle.changed();
        Ok(())
    }

    /// Hands the appearance to running GTK apps by XSETTINGS, and to the
    /// rest by GNOME settings and settings.ini.
    pub(super) fn publish_appearance(&mut self) -> Result<()> {
        let dark = draw::is_dark();
        self.publish_theme(appearance::theme(dark))?;
        appearance::propagate(dark);
        Ok(())
    }

    pub(super) fn publish_theme(&mut self, name: &str) -> Result<()> {
        self.xsettings_serial += 1;
        let value = appearance::xsettings(self.xsettings_serial, name);
        let prop = self.atoms._XSETTINGS_SETTINGS;
        self.conn
            .change_property8(PropMode::REPLACE, self.ours.check, prop, prop, &value)?;
        Ok(())
    }

    pub(super) fn chrome(&mut self) -> Result<()> {
        self.draw_panel()?;
        self.draw_rail()?;
        let empty = !self.clients.values().any(|c| c.framed && c.is_main());
        if empty != self.empty_shown {
            self.empty_shown = empty;
            if empty {
                self.conn.map_window(self.ours.empty)?;
            } else {
                self.conn.unmap_window(self.ours.empty)?;
                self.moved(self.empty_rect);
            }
        }
        self.restack()
    }

    fn draw_empty(&mut self) -> Result<()> {
        if !self.empty_shown {
            return Ok(());
        }
        let c = chrome::empty(&mut self.font, self.empty_rect.2 as usize);
        self.put(self.ours.empty, &c, 0, 0)
    }

    /// The front app for the panel: its name and small icon.
    fn front(&self) -> Option<(String, Option<usize>)> {
        let c = self.clients.get(&self.top_of(self.focused?))?;
        Some((self.app_name(c), c.app))
    }

    pub(super) fn draw_panel(&mut self) -> Result<()> {
        let front = self.front();
        let view = PanelView {
            app: front
                .as_ref()
                .map(|(n, i)| (n.as_str(), i.and_then(|i| self.entries[i].small.as_ref()))),
            person: self.who.map(|w| w == Who::Person),
            hover: self.panel_hover,
        };
        let layout = chrome::panel_layout(&mut self.font, &view);
        let mut tree = chrome::panel_tree(&mut self.font, self.sw as usize, &view, &layout);
        if let Some(p) = &self.pop {
            tree.set_expanded(chrome::APPLICATIONS_ID, p.is_apps());
            tree.set_expanded(chrome::FRONT_APP_ID, p.is_panel_app());
        }
        let mut c = Canvas::new(self.sw as usize, PANEL_H as usize, theme().bg);
        let icons = |_| view.app.and_then(|(_, icon)| icon);
        tree.paint(
            &mut c,
            &mut crate::ui::widgets::Paint {
                font: &mut self.font,
                icons: &icons,
                field: None,
            },
            draw::Rect::new(0.0, 0.0, self.sw as f32, PANEL_H as f32),
        );
        self.panel_tree = Some(tree);
        self.panel_layout = layout;
        self.put(self.ours.panel, &c, 0, 0)
    }

    /// The rail: pinned apps, then apps and windows that run.
    pub(super) fn rail_targets(&self) -> Vec<Target> {
        let mut out: Vec<Target> = (0..self.entries.len())
            .filter(|&i| self.entries[i].pinned)
            .map(Target::App)
            .collect();
        for w in &self.order {
            let c = &self.clients[w];
            if !c.is_main() {
                continue;
            }
            let t = match c.app {
                Some(i) => Target::App(i),
                None => Target::Key(c.key.clone()),
            };
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }

    pub(super) fn target_name(&self, t: &Target) -> String {
        match t {
            Target::App(i) => self.entries[*i].app.name.clone(),
            Target::Key(k) => self
                .order
                .iter()
                .map(|w| &self.clients[w])
                .find(|c| &c.key == k)
                .map(|c| self.app_name(c))
                .unwrap_or_else(|| k.clone()),
        }
    }

    pub(super) fn icon_of<'a>(
        entries: &'a [Entry],
        t: &Target,
        large: bool,
    ) -> Option<&'a draw::Image> {
        match t {
            Target::App(i) if large => entries[*i].large.as_ref(),
            Target::App(i) => entries[*i].small.as_ref(),
            Target::Key(_) => None,
        }
    }

    /// Whether the app of a target is the one in front.
    pub(super) fn in_front(&self, t: &Target) -> bool {
        let Some(f) = self.focused.and_then(|f| self.clients.get(&f)) else {
            return false;
        };
        match t {
            Target::App(i) => f.key == format!("app:{i}"),
            Target::Key(k) => &f.key == k,
        }
    }

    pub(super) fn rail_item(&self, t: &Target) -> (String, usize, bool, bool) {
        let windows = self.windows_of(t);
        let attention = windows
            .iter()
            .any(|w| self.clients.get(w).is_some_and(|c| c.attention));
        (
            self.target_name(t),
            windows.len(),
            self.in_front(t),
            attention,
        )
    }

    pub(super) fn draw_rail(&mut self) -> Result<()> {
        self.rail_targets = self.rail_targets();
        let data: Vec<(String, usize, bool, bool)> = self
            .rail_targets
            .iter()
            .map(|t| self.rail_item(t))
            .collect();
        let items: Vec<RailItem> = self
            .rail_targets
            .iter()
            .zip(&data)
            .map(|(t, (name, windows, front, attention))| RailItem {
                name,
                icon: Self::icon_of(&self.entries, t, true),
                windows: *windows,
                front: *front,
                attention: *attention,
            })
            .collect();
        let ids: Vec<_> = self.rail_targets.iter().map(Self::rail_node_id).collect();
        let mut tree = chrome::rail_tree(
            &mut self.font,
            &items,
            &ids,
            self.rail_hover,
            self.rail_h as usize,
            PANEL_H as i16,
        );
        if let Some(p) = &self.pop {
            for (i, id) in ids.iter().enumerate() {
                tree.set_expanded(*id, p.is_rail_app(&self.rail_targets[i]));
            }
        }
        let icons = |i: usize| items[i].icon;
        let mut c = Canvas::new(RAIL_W as usize, self.rail_h as usize, theme().bg);
        tree.paint(
            &mut c,
            &mut crate::ui::widgets::Paint {
                font: &mut self.font,
                icons: &icons,
                field: None,
            },
            draw::Rect::new(0.0, 0.0, RAIL_W as f32, self.rail_h as f32),
        );
        self.rail_tree = Some(tree);
        self.put(self.ours.rail, &c, 0, 0)
    }

    pub(super) fn show_tooltip(&mut self, i: Option<usize>) -> Result<()> {
        if i == self.tooltip_for {
            return Ok(());
        }
        if self.tooltip_for.take().is_some() {
            self.conn.unmap_window(self.ours.tooltip)?;
            self.moved(self.tooltip_rect);
        }
        let Some(i) = i.filter(|&i| i < self.rail_targets.len()) else {
            return Ok(());
        };
        self.tooltip_for = Some(i);
        let name = self.target_name(&self.rail_targets[i]);
        let (c, mask) = chrome::tooltip(&mut self.font, &name);
        let y = chrome::item_y(i) + (ITEM as i16 - TOOLTIP_H as i16) / 2 - 1;
        self.tooltip_rect = (RAIL_W as i16 - 1, y, c.w as u16, c.h as u16);
        self.place_window(self.ours.tooltip, self.tooltip_rect)?;
        self.shape(self.ours.tooltip, &mask)?;
        self.conn.map_window(self.ours.tooltip)?;
        self.raise(self.ours.tooltip)?;
        self.raise_top()?;
        self.put(self.ours.tooltip, &c, 0, 0)
    }

    fn draw_tooltip(&mut self) -> Result<()> {
        let Some(i) = self.tooltip_for else {
            return Ok(());
        };
        let name = self.target_name(&self.rail_targets[i]);
        let (c, _) = chrome::tooltip(&mut self.font, &name);
        self.put(self.ours.tooltip, &c, 0, 0)
    }

    /// The snap preview under the dragged window, or none.
    pub(super) fn show_snap(&mut self, r: Option<R>) -> Result<()> {
        if r == self.snap_rect {
            return Ok(());
        }
        if let Some(old) = self.snap_rect.take() {
            self.conn.unmap_window(self.ours.snap)?;
            self.moved(old);
        }
        let Some(r) = r else {
            return Ok(());
        };
        self.snap_rect = Some(r);
        self.place_window(self.ours.snap, r)?;
        let rects: Vec<Rectangle> = chrome::rounded_shape(r.2, r.3, chrome::RADIUS)
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
            self.ours.snap,
            0,
            0,
            &rects,
        )?;
        self.conn.map_window(self.ours.snap)?;
        self.restack()?;
        self.draw_snap()
    }

    fn draw_snap(&mut self) -> Result<()> {
        let Some((_, _, w, h)) = self.snap_rect else {
            return Ok(());
        };
        let (w, h) = (w as usize, h as usize);
        let band = chrome::RADIUS as usize;
        self.put(self.ours.snap, &chrome::snap_band(w, h, true), 0, 0)?;
        self.put(
            self.ours.snap,
            &chrome::snap_band(w, h, false),
            0,
            (h - band) as i16,
        )?;
        self.conn.change_gc(
            self.ours.gc,
            &ChangeGCAux::new().foreground(theme().hairline_hi),
        )?;
        let side = (h - 2 * band) as u16;
        self.conn.poly_fill_rectangle(
            self.ours.snap,
            self.ours.gc,
            &[
                Rectangle {
                    x: 0,
                    y: band as i16,
                    width: 1,
                    height: side,
                },
                Rectangle {
                    x: w as i16 - 1,
                    y: band as i16,
                    width: 1,
                    height: side,
                },
            ],
        )?;
        Ok(())
    }

    pub(super) fn set_who(&mut self, who: Option<Who>) -> Result<()> {
        if who == self.who {
            return Ok(());
        }
        self.who = who;
        let name = match who {
            Some(Who::Agent) => "the agent",
            Some(Who::Person) => "a person",
            None => "nobody",
        };
        eprintln!("shuru-desktop: in control: {name}");
        if let Some(f) = self.focused {
            self.ring(f)?;
        }
        self.chrome()
    }

    /// The ripple and the cursor over everything else.
    fn raise_top(&self) -> Result<()> {
        if self.ripple_until.is_some() {
            self.raise(self.ours.ripple)?;
        }
        if self.cursor_shown {
            self.raise(self.ours.cursor)?;
        }
        Ok(())
    }

    pub(super) fn show_ripple(&mut self, x: i16, y: i16, person: bool) -> Result<()> {
        let half = (RIPPLE / 2) as i16;
        if self.ripple_until.is_some() {
            self.moved(self.ripple_rect);
        }
        self.ripple_rect = (x - half, y - half, RIPPLE, RIPPLE);
        self.ripple_person = person;
        self.conn.configure_window(
            self.ours.ripple,
            &ConfigureWindowAux::new()
                .x((x - half) as i32)
                .y((y - half) as i32)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.ours.ripple)?;
        let (c, _) = chrome::ripple(person);
        self.put(self.ours.ripple, &c, 0, 0)?;
        self.ripple_until = Some(Instant::now() + Duration::from_millis(500));
        if self.cursor_shown {
            self.raise(self.ours.cursor)?;
        }
        Ok(())
    }

    pub(super) fn update_cursor_image(&mut self) -> Result<()> {
        let Ok(img) = self.conn.xfixes_get_cursor_image()?.reply() else {
            return Ok(());
        };
        let (w, h) = (img.width as usize, img.height as usize);
        if w == 0 || h == 0 {
            if self.cursor_shown {
                self.cursor_shown = false;
                self.conn.unmap_window(self.ours.cursor)?;
                self.moved(self.cursor_rect);
            }
            return Ok(());
        }
        let (c, mask) = chrome::cursor(w, h, &img.cursor_image);
        self.cursor_hot = (img.xhot as i16, img.yhot as i16);
        let rect = (
            img.x - self.cursor_hot.0,
            img.y - self.cursor_hot.1,
            w as u16,
            h as u16,
        );
        self.moved(self.cursor_rect);
        self.cursor_rect = rect;
        self.conn.configure_window(
            self.ours.cursor,
            &ConfigureWindowAux::new()
                .x(rect.0 as i32)
                .y(rect.1 as i32)
                .width(w as u32)
                .height(h as u32)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.shape(self.ours.cursor, &mask)?;
        if !self.cursor_shown {
            self.cursor_shown = true;
            self.conn.map_window(self.ours.cursor)?;
        }
        self.put(self.ours.cursor, &c, 0, 0)?;
        self.cursor_px = Some(c);
        Ok(())
    }

    pub(super) fn move_cursor(&mut self) -> Result<()> {
        if !self.cursor_shown {
            return Ok(());
        }
        let Ok(p) = self.conn.query_pointer(self.root)?.reply() else {
            return Ok(());
        };
        let (x, y) = (p.root_x - self.cursor_hot.0, p.root_y - self.cursor_hot.1);
        if (x, y) == (self.cursor_rect.0, self.cursor_rect.1) {
            return Ok(());
        }
        self.cursor_active = Instant::now();
        self.moved(self.cursor_rect);
        self.cursor_rect.0 = x;
        self.cursor_rect.1 = y;
        self.conn.configure_window(
            self.ours.cursor,
            &ConfigureWindowAux::new().x(x as i32).y(y as i32),
        )?;
        Ok(())
    }

    /// The band of a floating card under (x, y): left, right, top, bottom.
    pub(super) fn edges_at(card: R, x: i16, y: i16) -> [bool; 4] {
        let (cx, cy, cw, ch) = (card.0, card.1, card.2 as i16, card.3 as i16);
        let zone = 20;
        let (l, r) = (x < cx, x >= cx + cw);
        let (t, b) = (y < cy, y >= cy + ch);
        let near_l = x < cx + zone;
        let near_r = x >= cx + cw - zone;
        let near_t = y < cy + zone;
        let near_b = y >= cy + ch - zone;
        [
            l || ((t || b) && near_l),
            r || ((t || b) && near_r),
            t || ((l || r) && near_t),
            b || ((l || r) && near_b),
        ]
    }

    /// The cursor for a set of edges.
    pub(super) fn edge_cursor(&self, e: [bool; 4]) -> Cursor {
        let s = &self.cursors.sides;
        match e {
            [true, _, true, _] => s[4],
            [_, true, true, _] => s[5],
            [_, true, _, true] => s[6],
            [true, _, _, true] => s[7],
            [true, ..] => s[0],
            [_, true, ..] => s[1],
            [_, _, true, _] => s[2],
            [_, _, _, true] => s[3],
            _ => self.cursors.arrow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_of_the_band_resize_two_edges() {
        let card = (100, 100, 400, 300);
        assert_eq!(Wm::edges_at(card, 95, 200), [true, false, false, false]);
        assert_eq!(Wm::edges_at(card, 95, 105), [true, false, true, false]);
        assert_eq!(Wm::edges_at(card, 110, 95), [true, false, true, false]);
        assert_eq!(Wm::edges_at(card, 300, 95), [false, false, true, false]);
        assert_eq!(Wm::edges_at(card, 505, 395), [false, true, false, true]);
        assert_eq!(Wm::edges_at(card, 300, 405), [false, false, false, true]);
    }
}
