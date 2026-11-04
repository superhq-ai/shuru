//! The menus: the Applications menu under its panel control, and the app
//! menu under the panel app item or beside a rail item. Each is one shaped
//! window: the open pill and the sheet it flows into.

use anyhow::Result;
use x11rb::protocol::xproto::*;
use x11rb::CURRENT_TIME;

use super::{Target, Wm, R};
use crate::chrome::{self, Connected, PanelHit, Row, RowState, Side, ITEM, PANEL_H, SHEET_PAD};
use crate::draw::{Canvas, Rect};
use crate::locale::{self, tr, Msg};

const APPS_W: f32 = 320.0;
const APP_W: f32 = 280.0;
const SHEET_TOP: f32 = 36.0;
const PILL_Y: f32 = 4.0;
const MAX_WINDOWS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Anchor {
    Panel,
    Rail(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Act {
    Activate(Target),
    Focus(Window),
    Url(String),
    Action(usize, usize),
    Launch(usize),
    Hide(Target),
    Quit(Target),
}

enum PRow {
    Search,
    Section(String),
    Item {
        name: String,
        entry: Option<usize>,
        detail: Option<String>,
        current: bool,
        minimized: bool,
        act: Act,
    },
    Action {
        name: String,
        act: Act,
    },
    Sep,
    Footer,
}

impl PRow {
    fn act(&self) -> Option<&Act> {
        match self {
            PRow::Item { act, .. } | PRow::Action { act, .. } => Some(act),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum What {
    Apps,
    App(Target, Anchor),
}

pub(super) struct Pop {
    what: What,
    pub rect: R,
    shape: Connected,
    rows: Vec<PRow>,
    query: String,
    sel: Option<usize>,
    hover: Option<usize>,
}

/// A window title without the trailing " - App" most apps add.
fn window_name(title: &str, app: &str) -> String {
    let short = [" - ", " \u{2014} ", " \u{2013} "]
        .iter()
        .find_map(|sep| title.strip_suffix(app)?.strip_suffix(sep))
        .unwrap_or(title);
    if short.is_empty() {
        app.to_string()
    } else {
        short.to_string()
    }
}

/// Whether a query reads as an address to open in the browser.
fn is_url(q: &str) -> bool {
    if q.is_empty() || q.contains(char::is_whitespace) {
        return false;
    }
    if q.contains("://") || q.starts_with("localhost") {
        return true;
    }
    let host = q.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    host.rsplit_once('.').is_some_and(|(name, tld)| {
        !name.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
    })
}

fn keysym_char(sym: u32) -> Option<char> {
    match sym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(sym),
        0x0100_0000..=0x0110_ffff => char::from_u32(sym - 0x0100_0000),
        _ => None,
    }
}

impl Wm {
    fn row_height_sum(rows: &[PRow]) -> f32 {
        let fake: Vec<Row> = rows.iter().map(|r| Self::shape_row(r)).collect();
        chrome::sheet_height(&fake)
    }

    /// A stand in of the right height for measuring.
    fn shape_row(r: &PRow) -> Row<'static> {
        let state = RowState::Rest;
        match r {
            PRow::Search => Row::Search {
                query: "",
                placeholder: "",
            },
            PRow::Section(_) => Row::Section(""),
            PRow::Item { .. } => Row::Item {
                name: "",
                icon: None,
                icon_name: "",
                detail: None,
                current: false,
                minimized: false,
                state,
            },
            PRow::Action { .. } => Row::Action {
                name: "",
                hint: None,
                state,
            },
            PRow::Sep => Row::Sep,
            PRow::Footer => Row::Footer,
        }
    }

    fn window_row(&self, w: Window, current: bool) -> PRow {
        let c = &self.clients[&w];
        let app = self.app_name(c);
        PRow::Item {
            name: window_name(&c.title, &app),
            entry: c.app,
            detail: None,
            current,
            minimized: c.minimized,
            act: Act::Focus(w),
        }
    }

    fn browser(&self) -> Option<usize> {
        self.entries.iter().position(|e| e.app.is_browser())
    }

    fn apps_rows(&self, query: &str) -> Vec<PRow> {
        let q = query.trim().to_lowercase();
        let mut rows = vec![PRow::Search];
        if is_url(query.trim()) {
            if let Some(b) = self.browser() {
                rows.push(PRow::Item {
                    name: query.trim().to_string(),
                    entry: Some(b),
                    detail: Some(locale::open_in(&self.entries[b].app.name)),
                    current: false,
                    minimized: false,
                    act: Act::Url(query.trim().to_string()),
                });
            }
        }
        let apps: Vec<PRow> = self
            .rail_targets()
            .into_iter()
            .filter_map(|t| match t {
                Target::App(i) => Some(i),
                Target::Key(_) => None,
            })
            .filter(|&i| {
                let a = &self.entries[i].app;
                q.is_empty()
                    || a.name.to_lowercase().contains(&q)
                    || a.id.to_lowercase().contains(&q)
                    || a.generic
                        .as_ref()
                        .is_some_and(|g| g.to_lowercase().contains(&q))
            })
            .map(|i| PRow::Item {
                name: self.entries[i].app.name.clone(),
                entry: Some(i),
                detail: self.entries[i].app.generic.clone(),
                current: false,
                minimized: false,
                act: Act::Activate(Target::App(i)),
            })
            .collect();
        if !apps.is_empty() {
            rows.push(PRow::Section(tr(Msg::Applications).into()));
            rows.extend(apps);
        }
        let windows: Vec<PRow> = self
            .mru
            .iter()
            .copied()
            .filter(|w| {
                let c = &self.clients[w];
                c.is_main()
                    && (q.is_empty()
                        || c.title.to_lowercase().contains(&q)
                        || self.app_name(c).to_lowercase().contains(&q))
            })
            .take(MAX_WINDOWS)
            .map(|w| self.window_row(w, false))
            .collect();
        if !windows.is_empty() {
            rows.push(PRow::Section(tr(Msg::OpenWindows).into()));
            rows.extend(windows);
        }
        if rows.len() == 1 {
            rows.push(PRow::Section(tr(Msg::NoMatches).into()));
        }
        rows.push(PRow::Footer);
        rows
    }

    fn app_rows(&self, t: &Target) -> Vec<PRow> {
        let name = self.target_name(t);
        let windows = self.windows_of(t);
        let entry = match t {
            Target::App(i) => Some(*i),
            Target::Key(_) => None,
        };
        let mut rows = Vec::new();
        let actions: Vec<PRow> = entry
            .map(|i| {
                self.entries[i]
                    .app
                    .actions
                    .iter()
                    .enumerate()
                    .map(|(k, a)| PRow::Action {
                        name: a.name.clone(),
                        act: Act::Action(i, k),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if windows.is_empty() {
            rows.push(PRow::Action {
                name: tr(Msg::Open).into(),
                act: Act::Activate(t.clone()),
            });
            if !actions.is_empty() {
                rows.push(PRow::Sep);
                rows.extend(actions);
            }
            return rows;
        }
        if !actions.is_empty() {
            rows.extend(actions);
        } else if let Some(i) = entry {
            rows.push(PRow::Action {
                name: tr(Msg::NewWindow).into(),
                act: Act::Launch(i),
            });
        }
        if !rows.is_empty() {
            rows.push(PRow::Sep);
        }
        rows.push(PRow::Section(tr(Msg::OpenWindows).into()));
        let focused_top = self.focused.map(|f| self.top_of(f));
        for &w in windows.iter().take(MAX_WINDOWS) {
            rows.push(self.window_row(w, focused_top == Some(w)));
        }
        rows.push(PRow::Sep);
        rows.push(PRow::Action {
            name: locale::hide(&name),
            act: Act::Hide(t.clone()),
        });
        rows.push(PRow::Action {
            name: locale::quit(&name),
            act: Act::Quit(t.clone()),
        });
        rows
    }

    fn first_act(rows: &[PRow]) -> Option<usize> {
        rows.iter().position(|r| r.act().is_some())
    }

    pub(super) fn open_apps_menu(&mut self) -> Result<()> {
        let was = self.pop.as_ref().map(|p| p.what.clone());
        self.close_pop()?;
        if was == Some(What::Apps) {
            return Ok(());
        }
        let rows = self.apps_rows("");
        let sel = Self::first_act(&rows);
        self.show_pop(What::Apps, rows, String::new(), sel)
    }

    pub(super) fn open_app_menu(&mut self, t: Target, anchor: Anchor) -> Result<()> {
        let what = What::App(t.clone(), anchor);
        let was = self.pop.as_ref().map(|p| p.what.clone());
        self.close_pop()?;
        if was == Some(what.clone()) {
            return Ok(());
        }
        let rows = self.app_rows(&t);
        self.show_pop(what, rows, String::new(), None)
    }

    /// The pill and sheet of a menu on the screen.
    fn pop_geometry(&self, what: &What, height: f32) -> Option<(Rect, Rect, f32, Side)> {
        match what {
            What::Apps => {
                let (x, w) = self.panel_layout.apps;
                Some((
                    Rect::new(x, PILL_Y, w, 24.0),
                    Rect::new(x, SHEET_TOP, APPS_W, height),
                    6.0,
                    Side::Below,
                ))
            }
            What::App(_, Anchor::Panel) => {
                let (x, w) = self.panel_layout.app?;
                Some((
                    Rect::new(x, PILL_Y, w, 24.0),
                    Rect::new(x, SHEET_TOP, APP_W.max(w), height),
                    6.0,
                    Side::Below,
                ))
            }
            What::App(_, Anchor::Rail(i)) => {
                let item = ITEM as f32;
                let height = height.max(item + 20.0);
                let iy = chrome::item_y(*i) as f32;
                let pill = Rect::new(chrome::ITEM_X as f32, iy, item, item);
                let x = chrome::RAIL_W as f32;
                let bottom = self.sh as f32 - 8.0;
                if iy + height <= bottom || iy + item - height < SHEET_TOP + 4.0 {
                    Some((pill, Rect::new(x, iy, APP_W, height), 12.0, Side::Right))
                } else {
                    Some((
                        pill,
                        Rect::new(x, iy + item - height, APP_W, height),
                        12.0,
                        Side::RightUp,
                    ))
                }
            }
        }
    }

    fn show_pop(
        &mut self,
        what: What,
        rows: Vec<PRow>,
        query: String,
        sel: Option<usize>,
    ) -> Result<()> {
        self.show_tooltip(None)?;
        let height = Self::row_height_sum(&rows);
        let Some((pill, sheet, pill_r, side)) = self.pop_geometry(&what, height) else {
            return Ok(());
        };
        let x0 = pill.x.min(sheet.x);
        let y0 = pill.y.min(sheet.y);
        let x1 = (pill.x + pill.w).max(sheet.x + sheet.w);
        let y1 = (pill.y + pill.h).max(sheet.y + sheet.h);
        let rect = (x0 as i16, y0 as i16, (x1 - x0) as u16, (y1 - y0) as u16);
        let local = |r: Rect| Rect::new(r.x - x0, r.y - y0, r.w, r.h);
        let shape = Connected {
            pill: local(pill),
            sheet: local(sheet),
            pill_r,
            side,
        };
        let had = self.pop.is_some();
        if let Some(old) = self.pop.as_ref().map(|p| p.rect) {
            self.moved(old);
        }
        self.pop = Some(Pop {
            what,
            rect,
            shape,
            rows,
            query,
            sel,
            hover: None,
        });
        self.conn.configure_window(
            self.ours.pop,
            &ConfigureWindowAux::new()
                .x(rect.0 as i32)
                .y(rect.1 as i32)
                .width(rect.2 as u32)
                .height(rect.3 as u32),
        )?;
        let (_, mask) = chrome::connected(rect.2 as usize, rect.3 as usize, &shape);
        self.shape(self.ours.pop, &mask)?;
        self.conn.map_window(self.ours.pop)?;
        if !had {
            self.panel_hover = None;
            self.rail_hover = None;
            self.draw_panel()?;
            self.draw_rail()?;
            self.grab_cursor = self.cursors.arrow;
            self.conn.grab_pointer(
                false,
                self.root,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                self.cursors.arrow,
                CURRENT_TIME,
            )?;
            self.conn.grab_keyboard(
                false,
                self.root,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?;
        }
        self.restack()?;
        self.draw_pop()
    }

    pub(super) fn close_pop(&mut self) -> Result<()> {
        let Some(p) = self.pop.take() else {
            return Ok(());
        };
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        self.conn.ungrab_keyboard(CURRENT_TIME)?;
        self.conn.unmap_window(self.ours.pop)?;
        self.moved(p.rect);
        Ok(())
    }

    pub(super) fn draw_pop(&mut self) -> Result<()> {
        let Some(p) = self.pop.take() else {
            return Ok(());
        };
        let (w, h) = (p.rect.2 as usize, p.rect.3 as usize);
        let (mut c, _) = chrome::connected(w, h, &p.shape);
        self.pop_pill(&mut c, &p);
        let rows: Vec<Row> = p
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let state = if p.sel == Some(i) {
                    RowState::Selected
                } else if p.hover == Some(i) {
                    RowState::Hover
                } else {
                    RowState::Rest
                };
                match r {
                    PRow::Search => Row::Search {
                        query: &p.query,
                        placeholder: tr(Msg::Search),
                    },
                    PRow::Section(s) => Row::Section(s),
                    PRow::Item {
                        name,
                        entry,
                        detail,
                        current,
                        minimized,
                        ..
                    } => Row::Item {
                        name,
                        icon: entry.and_then(|e| self.entries[e].small.as_ref()),
                        icon_name: entry
                            .map_or(name.as_str(), |e| self.entries[e].app.name.as_str()),
                        detail: detail.as_deref(),
                        current: *current,
                        minimized: *minimized,
                        state,
                    },
                    PRow::Action { name, .. } => Row::Action {
                        name,
                        hint: None,
                        state,
                    },
                    PRow::Sep => Row::Sep,
                    PRow::Footer => Row::Footer,
                }
            })
            .collect();
        chrome::sheet(&mut c, &mut self.font, p.shape.sheet, &rows);
        let out = self.put(self.ours.pop, &c, 0, 0);
        self.pop = Some(p);
        out
    }

    /// What the open pill shows: the panel control or the rail item.
    fn pop_pill(&mut self, c: &mut Canvas, p: &Pop) {
        let pill = p.shape.pill;
        match &p.what {
            What::Apps => chrome::apps_label(c, &mut self.font, pill.x, pill.y),
            What::App(t, Anchor::Panel) => {
                let name = self.target_name(t);
                let icon = Self::icon_of(&self.entries, t, false);
                chrome::app_label(c, &mut self.font, pill.x, pill.y, &name, icon);
            }
            What::App(t, Anchor::Rail(_)) => {
                let (name, windows, front, attention) = self.rail_item(t);
                let item = chrome::RailItem {
                    name: &name,
                    icon: Self::icon_of(&self.entries, t, true),
                    windows,
                    front,
                    attention,
                };
                chrome::rail_icon(c, &mut self.font, &item, pill.x, pill.y);
            }
        }
    }

    /// The row under a point of the menu window, one that acts or not.
    fn any_row_at(p: &Pop, x: f32, y: f32) -> Option<usize> {
        let s = p.shape.sheet;
        if x < s.x + SHEET_PAD || x >= s.x + s.w - SHEET_PAD {
            return None;
        }
        let mut top = s.y + SHEET_PAD;
        for (i, r) in p.rows.iter().enumerate() {
            let h = Self::shape_row(r).height();
            if y >= top && y < top + h {
                return Some(i);
            }
            top += h;
        }
        None
    }

    /// The row under a point of the menu window.
    fn row_at(p: &Pop, x: f32, y: f32) -> Option<usize> {
        Self::any_row_at(p, x, y).filter(|&i| p.rows[i].act().is_some())
    }

    pub(super) fn pop_motion(&mut self, x: i16, y: i16) -> Result<()> {
        let Some(p) = &self.pop else {
            return Ok(());
        };
        let (px, py) = ((x - p.rect.0) as f32, (y - p.rect.1) as f32);
        let hover = Self::row_at(p, px, py);
        let search = Self::any_row_at(p, px, py).is_some_and(|i| matches!(p.rows[i], PRow::Search));
        let cursor = if search {
            self.cursors.text
        } else {
            self.cursors.arrow
        };
        let changed = hover != p.hover;
        self.set_grab_cursor(cursor)?;
        if changed {
            if let Some(p) = &mut self.pop {
                p.hover = hover;
            }
            self.draw_pop()?;
        }
        Ok(())
    }

    pub(super) fn pop_press(&mut self, e: &ButtonPressEvent) -> Result<()> {
        let Some(p) = &self.pop else {
            return Ok(());
        };
        let (lx, ly) = ((e.root_x - p.rect.0) as f32, (e.root_y - p.rect.1) as f32);
        let pill = p.shape.pill;
        let on_pill = lx >= pill.x && lx < pill.x + pill.w && ly >= pill.y && ly < pill.y + pill.h;
        if let Some(i) = Self::row_at(p, lx, ly) {
            if e.detail == 1 {
                let act = p.rows[i].act().cloned();
                self.close_pop()?;
                if let Some(a) = act {
                    self.run_act(a)?;
                }
            }
            return Ok(());
        }
        let inside = ly >= p.shape.sheet.y
            && ly < p.shape.sheet.y + p.shape.sheet.h
            && lx >= p.shape.sheet.x
            && lx < p.shape.sheet.x + p.shape.sheet.w;
        if inside {
            return Ok(());
        }
        self.close_pop()?;
        if on_pill {
            return Ok(());
        }
        let (x, y) = (e.root_x, e.root_y);
        if y < PANEL_H as i16 && e.detail == 1 {
            match self.panel_layout.hit(x as f32, y as f32) {
                Some(PanelHit::Apps) => self.open_apps_menu()?,
                Some(PanelHit::App) => {
                    if let Some(t) = self.front_target() {
                        self.open_app_menu(t, Anchor::Panel)?;
                    }
                }
                _ => {}
            }
        } else if x < chrome::RAIL_W as i16 {
            if let Some(i) = chrome::rail_hit(self.rail_targets.len(), x as f32, y as f32) {
                let t = self.rail_targets[i].clone();
                match e.detail {
                    1 if self.in_front(&t) => self.open_app_menu(t, Anchor::Panel)?,
                    1 => self.activate(&t)?,
                    3 => self.open_app_menu(t, Anchor::Rail(i))?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn run_act(&mut self, act: Act) -> Result<()> {
        match act {
            Act::Activate(t) => self.activate(&t)?,
            Act::Focus(w) => self.focus(w)?,
            Act::Url(u) => {
                if let Some(b) = self.browser() {
                    let app = self.entries[b].app.clone();
                    let argv = app.command(&[u]);
                    self.start_app(app, argv);
                }
            }
            Act::Action(i, k) => {
                let app = self.entries[i].app.clone();
                let argv = app.action_command(k);
                self.start_app(app, argv);
            }
            Act::Launch(i) => {
                let app = self.entries[i].app.clone();
                let argv = app.command(&[]);
                self.start_app(app, argv);
            }
            Act::Hide(t) => {
                for w in self.windows_of(&t) {
                    self.minimize(w)?;
                }
            }
            Act::Quit(t) => {
                for w in self.windows_of(&t) {
                    self.close(w)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn pop_key(&mut self, e: &KeyPressEvent) -> Result<()> {
        let syms = self
            .conn
            .get_keyboard_mapping(e.detail, 1)?
            .reply()?
            .keysyms;
        let state = u16::from(e.state);
        let shift = state & u16::from(ModMask::SHIFT) != 0;
        let ctrl = state & u16::from(ModMask::CONTROL) != 0;
        let base = syms.first().copied().unwrap_or(0);
        let sym = match syms.get(1).copied() {
            Some(s) if shift && s != 0 => s,
            _ => base,
        };
        if self.is_super(e.detail) {
            return self.close_pop();
        }
        let Some(p) = &self.pop else {
            return Ok(());
        };
        let acts: Vec<usize> = (0..p.rows.len())
            .filter(|&i| p.rows[i].act().is_some())
            .collect();
        let at = p.sel.and_then(|s| acts.iter().position(|&a| a == s));
        match base {
            0xff1b => return self.close_pop(),
            0xff0d | 0xff8d => {
                let act = p.sel.and_then(|i| p.rows[i].act().cloned());
                if act.is_some() {
                    self.close_pop()?;
                    if let Some(a) = act {
                        self.run_act(a)?;
                    }
                }
                return Ok(());
            }
            0xff52 | 0xff54 if !acts.is_empty() => {
                let n = acts.len();
                let next = match (at, base == 0xff54) {
                    (None, true) => 0,
                    (None, false) => n - 1,
                    (Some(i), true) => (i + 1) % n,
                    (Some(i), false) => (i + n - 1) % n,
                };
                let sel = Some(acts[next]);
                if let Some(p) = &mut self.pop {
                    p.sel = sel;
                }
                return self.draw_pop();
            }
            _ => {}
        }
        if p.what != What::Apps || ctrl {
            return Ok(());
        }
        let mut query = p.query.clone();
        if base == 0xff08 {
            if query.pop().is_none() {
                return Ok(());
            }
        } else if let Some(ch) = keysym_char(sym) {
            query.push(ch);
        } else {
            return Ok(());
        }
        let rows = self.apps_rows(&query);
        let sel = Self::first_act(&rows);
        self.show_pop(What::Apps, rows, query, sel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_name_drops_the_app_suffix() {
        assert_eq!(window_name("Fares - Chromium", "Chromium"), "Fares");
        assert_eq!(window_name("Chromium", "Chromium"), "Chromium");
        assert_eq!(window_name("", "Files"), "Files");
        assert_eq!(window_name("notes.md", "Text Editor"), "notes.md");
    }

    #[test]
    fn addresses_read_as_urls() {
        assert!(is_url("example.com"));
        assert!(is_url("example.com/a?b=c"));
        assert!(is_url("https://x"));
        assert!(is_url("localhost:8080"));
        assert!(is_url("northwindrail.example:443/x"));
        assert!(!is_url("chromium"));
        assert!(!is_url("text editor"));
        assert!(!is_url("v1.2"));
        assert!(!is_url(""));
    }

    #[test]
    fn keysyms_become_text() {
        assert_eq!(keysym_char(0x61), Some('a'));
        assert_eq!(keysym_char(0xe9), Some('\u{e9}'));
        assert_eq!(keysym_char(0x0100_20ac), Some('\u{20ac}'));
        assert_eq!(keysym_char(0xff0d), None);
    }
}
