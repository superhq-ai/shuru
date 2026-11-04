//! Our own windows: the strip, the panel, the badge, the border, the
//! click ripple and the cursor.

use std::time::{Duration, Instant};

use anyhow::Result;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::*;

use super::{within, Kind, PanelTarget, Who, Wm, R};
use crate::chrome::{
    self, PanelItem, BADGE_H, BADGE_INSET, BORDER, MARGIN, PANEL_H, PANEL_LIFT, RIPPLE, STRIP,
};
use crate::draw::{self, Canvas};

impl Wm {
    pub(super) fn moved(&mut self, old: R) {
        if old.2 > 0 && old.3 > 0 {
            self.recent.push((Instant::now(), old));
        }
    }

    fn strip_rect(&self) -> R {
        (MARGIN, MARGIN, self.sw - 2 * MARGIN as u16, STRIP as u16)
    }

    fn border_rects(&self) -> [R; 4] {
        let t = BORDER;
        [
            (0, 0, self.sw, t),
            (0, (self.sh - t) as i16, self.sw, t),
            (0, 0, t, self.sh),
            ((self.sw - t) as i16, 0, t, self.sh),
        ]
    }

    /// Whether damage falls inside one of our own windows, now or lately.
    pub(super) fn own_damage(&self, r: R) -> bool {
        let mut ours = Vec::with_capacity(12);
        if self.strip_shown {
            ours.push(self.strip_rect());
        }
        if self.panel_shown {
            ours.push(self.panel_rect);
        }
        if self.who.is_some() {
            ours.push(self.badge_rect);
        }
        if self.who == Some(Who::Person) {
            ours.extend(self.border_rects());
        }
        if self.cursor_shown {
            ours.push(self.cursor_rect);
        }
        if self.ripple_until.is_some() {
            ours.push(self.ripple_rect);
        }
        ours.iter()
            .chain(self.recent.iter().map(|(_, r)| r))
            .any(|&o| within(r, o))
    }

    pub(super) fn raise(&self, w: Window) -> Result<()> {
        self.conn
            .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        Ok(())
    }

    pub(super) fn raise_overlays(&self) -> Result<()> {
        if self.who.is_some() {
            self.raise(self.ours.badge)?;
        }
        if self.who == Some(Who::Person) {
            for b in self.ours.border {
                self.raise(b)?;
            }
        }
        if self.ripple_until.is_some() {
            self.raise(self.ours.ripple)?;
        }
        if self.cursor_shown {
            self.raise(self.ours.cursor)?;
        }
        Ok(())
    }

    /// The strip and panel over the windows, menus of apps over them, the
    /// overlays over everything.
    fn restack(&self) -> Result<()> {
        if self.strip_shown {
            self.raise(self.ours.strip)?;
        }
        if self.panel_shown {
            self.raise(self.ours.panel)?;
        }
        for &f in &self.foreign {
            self.raise(f)?;
        }
        self.raise_overlays()
    }

    /// Shows, hides and redraws the strip, panel and badge for the focused
    /// window, then restacks.
    pub(super) fn chrome(&mut self) -> Result<()> {
        let fullscreen = |w: Option<u32>| {
            w.and_then(|w| self.clients.get(&w))
                .is_some_and(|c| c.fullscreen)
        };
        let full = fullscreen(self.focused) || fullscreen(self.focused.map(|f| self.top_of(f)));
        let strip = !full && self.focused.is_some();
        if strip != self.strip_shown {
            self.strip_shown = strip;
            if strip {
                self.conn.map_window(self.ours.strip)?;
            } else {
                self.conn.unmap_window(self.ours.strip)?;
                self.moved(self.strip_rect());
            }
        }
        self.draw_strip()?;
        self.layout_panel(!full)?;
        self.place_badge()?;
        self.restack()
    }

    fn put(&self, win: Window, c: &Canvas) -> Result<()> {
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
                0,
                y0 as i16,
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

    pub(super) fn redraw(&mut self, w: Window) -> Result<()> {
        if w == self.ours.strip {
            self.draw_strip()
        } else if w == self.ours.panel {
            self.draw_panel()
        } else if w == self.ours.badge {
            self.draw_badge()
        } else if w == self.ours.cursor {
            match &self.cursor_px {
                Some(c) => self.put(w, c),
                None => Ok(()),
            }
        } else if w == self.ours.ripple {
            let (c, _) = chrome::ripple(self.ripple_person);
            self.put(w, &c)
        } else {
            Ok(())
        }
    }

    pub(super) fn draw_strip(&mut self) -> Result<()> {
        if !self.strip_shown {
            return Ok(());
        }
        let reserve = match self.who {
            Some(who) => {
                let (badge, _, _) = chrome::badge(&mut self.font, who == Who::Person);
                badge.w as f32 + BADGE_INSET as f32 + 10.0
            }
            None => 0.0,
        };
        let focused = self.focused.and_then(|f| self.clients.get(&f));
        let app = focused.map(|c| self.app_name(c));
        let title = focused.map(|c| c.title.clone());
        let w = self.strip_rect().2 as usize;
        let c = chrome::strip(&mut self.font, w, app.as_deref(), title.as_deref(), reserve);
        self.put(self.ours.strip, &c)
    }

    fn panel_items(&self) -> (Vec<PanelItem>, Vec<PanelTarget>) {
        let fkey = self
            .focused
            .and_then(|f| self.clients.get(&f))
            .map(|c| c.key.as_str());
        let mut items = Vec::new();
        let mut targets = Vec::new();
        for (i, app) in self.apps.iter().enumerate() {
            let key = format!("app:{i}");
            items.push(PanelItem {
                icon: app.icon(),
                running: self
                    .clients
                    .values()
                    .any(|c| c.key == key && c.kind.is_app()),
                focused: fkey == Some(key.as_str()),
            });
            targets.push(PanelTarget::App(i));
        }
        let mut seen: Vec<&str> = Vec::new();
        for w in &self.order {
            let c = &self.clients[w];
            if c.app.is_some() || c.kind != Kind::Normal || seen.contains(&c.key.as_str()) {
                continue;
            }
            seen.push(&c.key);
            items.push(PanelItem {
                icon: chrome::Icon::Page,
                running: true,
                focused: fkey == Some(c.key.as_str()),
            });
            targets.push(PanelTarget::Key(c.key.clone()));
        }
        (items, targets)
    }

    fn layout_panel(&mut self, visible: bool) -> Result<()> {
        let (items, targets) = self.panel_items();
        self.panel_targets = targets;
        if !visible || items.is_empty() {
            if self.panel_shown {
                self.panel_shown = false;
                self.conn.unmap_window(self.ours.panel)?;
                self.moved(self.panel_rect);
            }
            return Ok(());
        }
        let (w, spans) = chrome::panel_layout(&items);
        let x = ((self.sw as i32 - w as i32) / 2) as i16;
        let y = self.sh as i16 - MARGIN - 1 - PANEL_LIFT - PANEL_H as i16;
        let rect = (x, y, w as u16, PANEL_H);
        if rect != self.panel_rect {
            self.moved(self.panel_rect);
            self.conn.configure_window(
                self.ours.panel,
                &ConfigureWindowAux::new()
                    .x(x as i32)
                    .y(y as i32)
                    .width(w as u32)
                    .height(PANEL_H as u32),
            )?;
            let (_, mask) = chrome::panel(&items, w, &spans, None);
            self.shape(self.ours.panel, &mask)?;
            self.panel_rect = rect;
        }
        self.panel_spans = spans;
        if !self.panel_shown {
            self.panel_shown = true;
            self.conn.map_window(self.ours.panel)?;
        }
        self.draw_panel()
    }

    pub(super) fn draw_panel(&mut self) -> Result<()> {
        if !self.panel_shown {
            return Ok(());
        }
        let (items, _) = self.panel_items();
        let (c, _) = chrome::panel(
            &items,
            self.panel_rect.2 as usize,
            &self.panel_spans,
            self.panel_hover,
        );
        self.put(self.ours.panel, &c)
    }

    pub(super) fn set_who(&mut self, who: Option<Who>) -> Result<()> {
        if who == self.who {
            return Ok(());
        }
        let was = self.who;
        self.who = who;
        if was == Some(Who::Person) {
            for b in self.ours.border {
                self.conn.unmap_window(b)?;
            }
            for r in self.border_rects() {
                self.moved(r);
            }
        }
        if who == Some(Who::Person) {
            for b in self.ours.border {
                self.conn.map_window(b)?;
            }
        }
        match who {
            Some(_) => {
                self.place_badge()?;
                self.conn.map_window(self.ours.badge)?;
            }
            None => {
                self.conn.unmap_window(self.ours.badge)?;
                self.moved(self.badge_rect);
            }
        }
        let name = match who {
            Some(Who::Agent) => "the agent",
            Some(Who::Person) => "a person",
            None => "nobody",
        };
        eprintln!("shuru-desktop: in control: {name}");
        self.draw_strip()?;
        self.raise_overlays()
    }

    fn place_badge(&mut self) -> Result<()> {
        let Some(who) = self.who else {
            return Ok(());
        };
        let (_, mask, done) = chrome::badge(&mut self.font, who == Who::Person);
        let w = mask.w as u16;
        let (x, y) = if self.strip_shown {
            (
                self.sw as i16 - MARGIN - BADGE_INSET - w as i16,
                MARGIN + (STRIP - BADGE_H as i16) / 2,
            )
        } else {
            (self.sw as i16 - MARGIN - w as i16, MARGIN)
        };
        let rect = (x, y, w, BADGE_H as u16);
        if rect != self.badge_rect {
            self.moved(self.badge_rect);
            self.conn.configure_window(
                self.ours.badge,
                &ConfigureWindowAux::new()
                    .x(x as i32)
                    .y(y as i32)
                    .width(w as u32)
                    .height(BADGE_H as u32),
            )?;
            self.badge_rect = rect;
            self.shape(self.ours.badge, &mask)?;
        }
        self.badge_done = done;
        self.draw_badge()
    }

    fn draw_badge(&mut self) -> Result<()> {
        let Some(who) = self.who else {
            return Ok(());
        };
        let (c, _, _) = chrome::badge(&mut self.font, who == Who::Person);
        self.put(self.ours.badge, &c)
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
        self.put(self.ours.ripple, &c)?;
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
        self.put(self.ours.cursor, &c)?;
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
        self.moved(self.cursor_rect);
        self.cursor_rect.0 = x;
        self.cursor_rect.1 = y;
        self.conn.configure_window(
            self.ours.cursor,
            &ConfigureWindowAux::new().x(x as i32).y(y as i32),
        )?;
        Ok(())
    }
}
