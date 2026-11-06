//! The menus: the Applications menu under its panel control, the app menu
//! under the panel app item or beside a rail item, and the notification
//! center under its control at the end of the panel. Each is one shaped
//! window: the open pill and the sheet it flows into.

use anyhow::Result;
use cosmic_text::Edit;
use serde_json::{json, Value};
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::{Target, Wm, R};
use crate::chrome::{self, Connected, PanelHit, Side, ITEM, PANEL_H};
use crate::draw::{theme, Canvas, Font, Rect};
use crate::glyphs;
use crate::locale::{self, tr, Msg};
use crate::ui::kit::switch::{Knob, TRACK_H, TRACK_W};
use crate::ui::text::Field;
use crate::ui::widgets::{Content, Ink, Paint, LABEL_SIZE, SMALL_SIZE, TEXT_SIZE};
use crate::ui::{style, Builder, Node, Style, Tree};
use accesskit::Role;

const APPS_W: f32 = 320.0;
const APP_W: f32 = 280.0;
const APP_MAX_W: f32 = 420.0;
const NOTES_W: f32 = 360.0;
const SHEET_PAD: f32 = 5.0;
const SHEET_TOP: f32 = 36.0;
const PILL_Y: f32 = 4.0;
const MAX_WINDOWS: usize = 8;
/// Where the text of a notification starts in its row, past its icon.
const NOTE_TEXT_X: f32 = 40.0;

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
    /// A click on a notification in the center.
    OpenNote(u32),
    NoteAction(u32, String),
    Dismiss(u32),
    ClearAll,
    ToggleQuiet,
}

impl Act {
    /// Whether the menu stays open after: acts that change the center.
    fn keeps_open(&self) -> bool {
        matches!(self, Act::Dismiss(_) | Act::ClearAll | Act::ToggleQuiet)
    }
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
    /// A setting turned on or off in place.
    Toggle {
        name: String,
        on: bool,
        act: Act,
    },
    /// A notification, with a button that dismisses it.
    Note {
        app: String,
        summary: String,
        body: String,
        entry: Option<usize>,
        act: Act,
        dismiss: Act,
    },
    /// An action under the row it belongs to.
    Sub {
        name: String,
        act: Act,
    },
    /// A line that says the menu has nothing to list.
    Empty(String),
    Sep,
    Footer,
}

impl PRow {
    fn act(&self) -> Option<&Act> {
        match self {
            PRow::Item { act, .. }
            | PRow::Action { act, .. }
            | PRow::Toggle { act, .. }
            | PRow::Note { act, .. }
            | PRow::Sub { act, .. } => Some(act),
            _ => None,
        }
    }

    /// The act of a node of the row: a button in it, else the row.
    fn act_for(&self, id: u64) -> Option<Act> {
        match self {
            PRow::Note { dismiss, .. } if id == act_id(dismiss) => Some(dismiss.clone()),
            _ => self.act().cloned(),
        }
    }
}

fn act_id(act: &Act) -> u64 {
    node_id(&format!("{act:?}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum What {
    Apps,
    App(Target, Anchor),
    Notes,
}

pub(super) struct Pop {
    what: What,
    pub rect: R,
    shape: Connected,
    rows: Vec<PRow>,
    query: String,
    tree: Tree,
    field: Option<Field>,
}

#[derive(Debug, PartialEq)]
enum ShellClick {
    Panel(PanelHit),
    Rail(usize),
}

fn shell_click(id: u64, has_front: bool, targets: &[Target]) -> Option<ShellClick> {
    match id {
        chrome::APPLICATIONS_ID => Some(ShellClick::Panel(PanelHit::Apps)),
        chrome::FRONT_APP_ID if has_front => Some(ShellClick::Panel(PanelHit::App)),
        chrome::NOTIFICATIONS_ID => Some(ShellClick::Panel(PanelHit::Notes)),
        _ => targets
            .iter()
            .position(|t| Wm::rail_node_id(t) == id)
            .map(ShellClick::Rail),
    }
}

impl Pop {
    pub(super) fn is_apps(&self) -> bool {
        self.what == What::Apps
    }
    pub(super) fn is_panel_app(&self) -> bool {
        matches!(self.what, What::App(_, Anchor::Panel))
    }
    pub(super) fn is_rail_app(&self, target: &Target) -> bool {
        matches!(&self.what, What::App(t, _) if t == target)
    }
    pub(super) fn is_notes(&self) -> bool {
        self.what == What::Notes
    }
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

/// A stable id for a node agents see, from what it is.
fn node_id(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hash);
    hash.finish()
}

/// The tree of a menu: a sheet of rows that sizes to them within the
/// widths of `sheet`. `icon_name` names the app of an entry.
fn menu_tree(
    font: &mut Font,
    sheet: Style,
    id: &str,
    name: &str,
    rows: &[PRow],
    icon_name: &dyn Fn(Option<usize>, &str) -> String,
) -> Tree {
    let text = |text: &str, size: f32, ink: Ink| Content::Text {
        text: text.to_string(),
        size,
        ink,
    };
    let sheet = sheet
        .column()
        .pad(SHEET_PAD, SHEET_PAD, SHEET_PAD, SHEET_PAD);
    let mut b =
        Builder::new(Node::new(Content::Box, sheet).semantic(node_id(id), Role::Menu, name));
    for r in rows {
        match r {
            PRow::Search => {
                let row = b.row(Node::new(Content::Box, style().height(30.0)));
                let well = b.add(
                    row,
                    Node::new(
                        Content::Well,
                        style()
                            .row()
                            .center()
                            .grow()
                            .height(28.0)
                            .pad(0.0, 12.0, 0.0, 8.0),
                    ),
                );
                b.add(
                    well,
                    Node::new(Content::Glyph(&glyphs::SEARCH, Ink::Dim), style()),
                );
                b.add(
                    well,
                    Node::new(
                        Content::Field {
                            placeholder: tr(Msg::Search).to_string(),
                        },
                        style().grow().height(18.0).margin(0.0, 0.0, 0.0, 8.0),
                    )
                    .semantic(
                        node_id("search"),
                        Role::SearchInput,
                        tr(Msg::Search),
                    ),
                );
            }
            PRow::Section(label) => {
                let row = b.row(
                    Node::new(
                        Content::Box,
                        style().row().center().height(22.0).pad(2.0, 0.0, 0.0, 8.0),
                    )
                    .semantic(
                        node_id(&format!("section:{label}")),
                        Role::Heading,
                        label,
                    ),
                );
                b.add(
                    row,
                    Node::new(text(label, LABEL_SIZE, Ink::Dim), style().grow()),
                );
            }
            PRow::Item {
                name,
                entry,
                detail,
                current,
                minimized,
                act,
            } => {
                let row = b.row(
                    Node::new(
                        Content::Choice,
                        style().row().center().height(26.0).pad(0.0, 8.0, 0.0, 8.0),
                    )
                    .semantic(act_id(act), Role::MenuItem, name),
                );
                let opacity = if *minimized { 0.5 } else { 1.0 };
                b.add(
                    row,
                    Node::new(
                        Content::AppIcon {
                            entry: *entry,
                            name: icon_name(*entry, name),
                            opacity,
                        },
                        style(),
                    ),
                );
                let ink = if *minimized { Ink::Dim } else { Ink::Text };
                let gap = style().margin(0.0, 0.0, 0.0, 8.0);
                b.add(
                    row,
                    Node::new(text(name, TEXT_SIZE, ink), gap.clone().grow()),
                );
                let detail = if *minimized {
                    Some(tr(Msg::Minimized))
                } else {
                    detail.as_deref()
                };
                if let Some(d) = detail {
                    b.add(row, Node::new(text(d, SMALL_SIZE, Ink::Dim), gap.clone()));
                }
                if *current {
                    b.add(
                        row,
                        Node::new(Content::Dot, style().margin(0.0, 2.0, 0.0, 8.0)),
                    );
                }
            }
            PRow::Action { name, act } | PRow::Sub { name, act } => {
                let left = if matches!(r, PRow::Sub { .. }) {
                    NOTE_TEXT_X
                } else {
                    8.0
                };
                let row = b.row(
                    Node::new(
                        Content::Choice,
                        style().row().center().height(24.0).pad(0.0, 8.0, 0.0, left),
                    )
                    .semantic(act_id(act), Role::MenuItem, name),
                );
                b.add(
                    row,
                    Node::new(text(name, TEXT_SIZE, Ink::Text), style().grow()),
                );
            }
            PRow::Toggle { name, on, act } => {
                let mut node = Node::new(
                    Content::Choice,
                    style().row().center().height(32.0).pad(0.0, 8.0, 0.0, 8.0),
                )
                .semantic(act_id(act), Role::MenuItemCheckBox, name);
                if let Some(s) = node.semantic.as_mut() {
                    s.set_toggled(if *on {
                        accesskit::Toggled::True
                    } else {
                        accesskit::Toggled::False
                    });
                }
                let row = b.row(node);
                b.add(
                    row,
                    Node::new(text(name, TEXT_SIZE, Ink::Text), style().grow()),
                );
                b.add(
                    row,
                    Node::new(Knob::content(*on), style().width(TRACK_W).height(TRACK_H)),
                );
            }
            PRow::Note {
                app,
                summary,
                body,
                entry,
                act,
                dismiss,
            } => {
                let title = if summary.is_empty() { app } else { summary };
                let mut node = Node::new(
                    Content::Choice,
                    style().row().center().pad(7.0, 4.0, 7.0, 8.0),
                )
                .semantic(act_id(act), Role::MenuItem, title);
                if let Some(s) = node.semantic.as_mut() {
                    let about = if body.is_empty() {
                        app.clone()
                    } else {
                        format!("{app}: {body}")
                    };
                    s.set_description(about);
                }
                let row = b.row(node);
                b.add(
                    row,
                    Node::new(
                        Content::AppIcon {
                            entry: *entry,
                            name: icon_name(*entry, app),
                            opacity: 1.0,
                        },
                        style(),
                    ),
                );
                let lines = b.add(
                    row,
                    Node::new(
                        Content::Box,
                        style()
                            .column()
                            .grow()
                            .margin(0.0, 8.0, 0.0, NOTE_TEXT_X - 8.0 - 16.0),
                    ),
                );
                b.add(lines, Node::new(text(app, LABEL_SIZE, Ink::Dim), style()));
                b.add(
                    lines,
                    Node::new(
                        text(title, TEXT_SIZE, Ink::Text),
                        style().margin(4.0, 0.0, 0.0, 0.0),
                    ),
                );
                if !body.is_empty() {
                    b.add(
                        lines,
                        Node::new(
                            text(body, SMALL_SIZE, Ink::Dim),
                            style().margin(4.0, 0.0, 0.0, 0.0),
                        ),
                    );
                }
                let close = b.add(
                    row,
                    Node::new(
                        Content::Choice,
                        style()
                            .row()
                            .center()
                            .width(24.0)
                            .height(24.0)
                            .pad(0.0, 0.0, 0.0, 4.0),
                    )
                    .semantic(
                        act_id(dismiss),
                        Role::Button,
                        &locale::dismiss(title),
                    ),
                );
                b.add(
                    close,
                    Node::new(Content::Glyph(&glyphs::CLOSE, Ink::Dim), style()),
                );
            }
            PRow::Empty(label) => {
                let row = b.row(
                    Node::new(
                        Content::Box,
                        style().row().center().height(40.0).pad(0.0, 8.0, 0.0, 8.0),
                    )
                    .semantic(
                        node_id(&format!("empty:{label}")),
                        Role::Label,
                        label,
                    ),
                );
                b.add(
                    row,
                    Node::new(text(label, TEXT_SIZE, Ink::Dim), style().grow()),
                );
            }
            PRow::Sep => {
                let row = b.row(Node::new(
                    Content::Box,
                    style().column().height(9.0).pad(4.0, 3.0, 0.0, 3.0),
                ));
                b.add(row, Node::new(Content::Rule, style().height(1.0)));
            }
            PRow::Footer => {
                // The hairline runs from edge to edge of the sheet.
                let row = b.row(Node::new(
                    Content::Box,
                    style()
                        .column()
                        .height(29.0)
                        .pad(5.0, 0.0, 0.0, 0.0)
                        .margin(0.0, 1.0 - SHEET_PAD, 0.0, 1.0 - SHEET_PAD),
                ));
                b.add(row, Node::new(Content::Rule, style().height(1.0)));
                let keys = b.add(
                    row,
                    Node::new(
                        Content::Box,
                        style()
                            .row()
                            .center()
                            .height(18.0)
                            .margin(5.0, 0.0, 0.0, 0.0)
                            .pad(0.0, 0.0, 0.0, 12.0),
                    ),
                );
                let key = style().margin(0.0, 5.0, 0.0, 0.0);
                let label = style().margin(0.0, 18.0, 0.0, 0.0);
                let hints: [(Content, Style); 7] = [
                    (Content::Key(Some(&glyphs::UP), ""), key.clone()),
                    (Content::Key(Some(&glyphs::DOWN), ""), key.clone()),
                    (text(tr(Msg::Move), SMALL_SIZE, Ink::Dim), label.clone()),
                    (Content::Key(Some(&glyphs::ENTER), ""), key.clone()),
                    (text(tr(Msg::Open), SMALL_SIZE, Ink::Dim), label),
                    (Content::Key(None, "Esc"), key),
                    (text(tr(Msg::Close), SMALL_SIZE, Ink::Dim), style()),
                ];
                for (content, style) in hints {
                    b.add(keys, Node::new(content, style));
                }
            }
        }
    }
    b.finish(font)
}

impl Wm {
    /// The tree of a menu of this desktop.
    fn pop_tree(&mut self, what: &What, rows: &[PRow]) -> Tree {
        let (sheet, name) = match what {
            What::Apps => (style().width(APPS_W), tr(Msg::Applications).to_string()),
            What::App(t, anchor) => {
                let least = match anchor {
                    Anchor::Panel => APP_W.max(self.panel_layout.app.map_or(0.0, |(_, w)| w)),
                    Anchor::Rail(_) => APP_W,
                };
                let sheet = style().min_width(least).max_width(APP_MAX_W.max(least));
                (sheet, self.target_name(t))
            }
            What::Notes => (style().width(NOTES_W), tr(Msg::Notifications).to_string()),
        };
        let entries = &self.entries;
        let icon_name = |entry: Option<usize>, name: &str| {
            entry.map_or(name.to_string(), |e| entries[e].app.name.clone())
        };
        let id = format!("menu:{what:?}");
        menu_tree(&mut self.font, sheet, &id, &name, rows, &icon_name)
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

    /// The entry of the default browser, else of the first browser.
    fn browser(&self) -> Option<usize> {
        let id = crate::defaults::Defaults::from_env().browser();
        self.entries
            .iter()
            .position(|e| Some(crate::defaults::desktop_id(&e.app)) == id)
            .or_else(|| self.entries.iter().position(|e| e.app.is_browser()))
    }

    /// Adds the default browser as an entry when it is not one yet, so
    /// addresses typed in the launcher open in it.
    fn add_default_browser(&mut self) {
        let Some(id) = crate::defaults::Defaults::from_env().browser() else {
            return;
        };
        if self
            .entries
            .iter()
            .any(|e| crate::defaults::desktop_id(&e.app) == id)
        {
            return;
        }
        if let Ok(app) = crate::apps::load(id.trim_end_matches(".desktop")) {
            if app.is_browser() {
                self.entries.push(super::Entry::new(app, false));
            }
        }
    }

    fn launcher_targets(&self) -> Vec<Target> {
        let mut targets = self.rail_targets();
        if let Some(i) = self.entries.iter().position(|e| e.app.id == "settings") {
            let target = Target::App(i);
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
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
            .launcher_targets()
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

    /// The notification center: do not disturb, then the notifications
    /// newest first, each with its actions, then Clear all.
    fn notes_rows(&self) -> Vec<PRow> {
        let mut rows = vec![
            PRow::Toggle {
                name: tr(Msg::DoNotDisturb).into(),
                on: self.notes.quiet,
                act: Act::ToggleQuiet,
            },
            PRow::Sep,
            PRow::Section(tr(Msg::Notifications).into()),
        ];
        let mut any = false;
        for r in self.notes.listed() {
            any = true;
            rows.push(PRow::Note {
                app: r.app.clone(),
                summary: r.summary.clone(),
                body: r.body.split_whitespace().collect::<Vec<_>>().join(" "),
                entry: r.entry,
                act: Act::OpenNote(r.id),
                dismiss: Act::Dismiss(r.id),
            });
            for (key, label) in r.actions.iter().filter(|(k, _)| k != "default") {
                rows.push(PRow::Sub {
                    name: label.clone(),
                    act: Act::NoteAction(r.id, key.clone()),
                });
            }
        }
        if any {
            rows.push(PRow::Sep);
            rows.push(PRow::Action {
                name: tr(Msg::ClearAll).into(),
                act: Act::ClearAll,
            });
        } else {
            rows.push(PRow::Empty(tr(Msg::NoNotifications).into()));
        }
        rows
    }

    pub(super) fn open_notes(&mut self) -> Result<()> {
        let was = self.pop.as_ref().map(|p| p.what.clone());
        self.close_pop()?;
        if was == Some(What::Notes) {
            return Ok(());
        }
        self.notes.unseen = false;
        let rows = self.notes_rows();
        self.show_pop(What::Notes, rows, String::new(), None)
    }

    /// Rebuilds the notification center when it is open, keeping the
    /// keyboard where it was.
    pub(super) fn refresh_notes(&mut self) -> Result<()> {
        let Some(p) = self.pop.as_ref().filter(|p| p.is_notes()) else {
            return Ok(());
        };
        let was = p.tree.selected;
        let rows = self.notes_rows();
        let sel = was.and_then(|s| {
            let s = s.min(rows.len() - 1);
            (s..rows.len())
                .chain((0..s).rev())
                .find(|&i| rows[i].act().is_some())
        });
        self.show_pop(What::Notes, rows, String::new(), sel)
    }

    /// Does what a row or a button in it does, and closes the menu unless
    /// the act changes what the menu shows.
    fn choose(&mut self, act: Act) -> Result<()> {
        if act.keeps_open() {
            self.run_act(act)?;
            self.refresh_notes()
        } else {
            self.close_pop()?;
            self.run_act(act)
        }
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
        self.add_default_browser();
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
    fn pop_geometry(
        &self,
        what: &What,
        width: f32,
        height: f32,
    ) -> Option<(Rect, Rect, f32, Side)> {
        match what {
            What::Apps => {
                let (x, w) = self.panel_layout.apps;
                Some((
                    Rect::new(x, PILL_Y, w, 24.0),
                    Rect::new(x, SHEET_TOP, width, height),
                    6.0,
                    Side::Below,
                ))
            }
            What::App(_, Anchor::Panel) => {
                let (x, w) = self.panel_layout.app?;
                Some((
                    Rect::new(x, PILL_Y, w, 24.0),
                    Rect::new(x, SHEET_TOP, width, height),
                    6.0,
                    Side::Below,
                ))
            }
            What::App(_, Anchor::Rail(i)) => {
                let item = ITEM as f32;
                let iy = chrome::item_y(*i) as f32;
                let pill = Rect::new(chrome::ITEM_X as f32, iy, item, item);
                let x = chrome::RAIL_W as f32;
                let bottom = self.sh as f32 - 8.0;
                if iy + height <= bottom || iy + item - height < SHEET_TOP + 4.0 {
                    Some((pill, Rect::new(x, iy, width, height), 12.0, Side::Right))
                } else {
                    Some((
                        pill,
                        Rect::new(x, iy + item - height, width, height),
                        12.0,
                        Side::RightUp,
                    ))
                }
            }
            What::Notes => {
                let (x, w) = self.panel_layout.notes;
                Some((
                    Rect::new(x, PILL_Y, w, 24.0),
                    Rect::new(x + w - width, SHEET_TOP, width, height),
                    6.0,
                    Side::BelowEnd,
                ))
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
        let mut tree = self.pop_tree(&what, &rows);
        let (width, full) = tree.size();
        let height = full.min(self.sh as f32 - SHEET_TOP - 8.0);
        let Some((pill, sheet, pill_r, side)) = self.pop_geometry(&what, width, height) else {
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
        let old_rect = self.pop.as_ref().map(|p| p.rect);
        let field = self
            .pop
            .as_mut()
            .and_then(|p| p.field.take())
            .or_else(|| (what == What::Apps).then(|| Field::new(&mut self.font)));
        if let Some(old) = old_rect.filter(|old| *old != rect) {
            self.moved(old);
        }
        tree.select(sel, shape.sheet.h);
        if what == What::Apps {
            tree.focused = Some(0);
            tree.set_value(0, &query);
        }
        self.pop = Some(Pop {
            what,
            rect,
            shape,
            rows,
            query,
            tree,
            field,
        });
        if old_rect != Some(rect) {
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
        }
        self.draw_pop()?;
        self.draw_panel()?;
        self.draw_rail()?;
        if !had {
            self.conn.map_window(self.ours.pop)?;
        }
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
        self.restack()
    }

    pub(super) fn close_pop(&mut self) -> Result<()> {
        let Some(p) = self.pop.take() else {
            return Ok(());
        };
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        self.conn.ungrab_keyboard(CURRENT_TIME)?;
        self.conn.unmap_window(self.ours.pop)?;
        self.moved(p.rect);
        self.draw_panel()?;
        self.draw_rail()?;
        Ok(())
    }

    pub(super) fn draw_pop(&mut self) -> Result<()> {
        let Some(mut p) = self.pop.take() else {
            return Ok(());
        };
        let (w, h) = (p.rect.2 as usize, p.rect.3 as usize);
        let (mut c, _) = chrome::connected(w, h, &p.shape);
        self.pop_pill(&mut c, &p);
        let entries = &self.entries;
        let icons = |e: usize| entries[e].small.as_ref();
        let mut paint = Paint {
            font: &mut self.font,
            icons: &icons,
            field: p.field.as_mut(),
        };
        p.tree.paint(&mut c, &mut paint, p.shape.sheet);
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
            What::Notes => {
                let bell = chrome::Bell {
                    unseen: false,
                    quiet: self.notes.quiet,
                };
                chrome::bell(c, pill, bell, theme().surface);
            }
        }
    }

    /// The row under a point of the menu window, one that acts or not.
    fn any_row_at(p: &Pop, x: f32, y: f32) -> Option<usize> {
        p.tree.hit(p.shape.sheet, x, y)
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
        if p.field.as_ref().is_some_and(|f| f.dragging) {
            let s = p.shape.sheet;
            let fx = p.tree.field_bounds((s.x, s.y)).map_or(0.0, |b| b.x);
            self.pop.as_mut().unwrap().field.as_mut().unwrap().pointer(
                &mut self.font,
                px - fx,
                true,
            );
            return self.draw_pop();
        }
        let hover = Self::row_at(p, px, py);
        let search = Self::any_row_at(p, px, py).is_some_and(|i| matches!(p.rows[i], PRow::Search));
        let cursor = if search {
            self.cursors.text
        } else {
            self.cursors.arrow
        };
        let changed = hover.is_some() && (hover != p.tree.selected || !p.tree.pointer);
        self.set_grab_cursor(cursor)?;
        if changed {
            if let Some(p) = &mut self.pop {
                p.tree.select(hover, p.shape.sheet.h);
                p.tree.pointer = true;
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
        if e.detail == 1
            && Self::any_row_at(p, lx, ly).is_some_and(|i| matches!(p.rows[i], PRow::Search))
        {
            let s = p.shape.sheet;
            let fx = p.tree.field_bounds((s.x, s.y)).map_or(0.0, |b| b.x);
            let p = self.pop.as_mut().unwrap();
            let field = p.field.as_mut().unwrap();
            field.pointer(&mut self.font, lx - fx, false);
            field.dragging = true;
            return self.draw_pop();
        }
        let pill = p.shape.pill;
        let on_pill = lx >= pill.x && lx < pill.x + pill.w && ly >= pill.y && ly < pill.y + pill.h;
        if let Some(i) = Self::row_at(p, lx, ly) {
            if e.detail == 1 {
                let act = match p.tree.hit_node(p.shape.sheet, lx, ly) {
                    Some(id) => p.rows[i].act_for(id),
                    None => p.rows[i].act().cloned(),
                };
                if let Some(a) = act {
                    self.choose(a)?;
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
            if let Some(hit) = self.panel_hit(x as f32, y as f32) {
                self.panel_click(hit)?;
            }
        } else if x < chrome::RAIL_W as i16 {
            if let Some(i) = self.rail_hit(x as f32, y as f32) {
                self.rail_click(i, e.detail)?;
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
            Act::OpenNote(id) => self.open_note(id)?,
            Act::NoteAction(id, key) => {
                self.note_action(id, &key)?;
            }
            Act::Dismiss(id) => {
                self.close_note(id, super::notes::DISMISSED)?;
            }
            Act::ClearAll => self.clear_notes()?,
            Act::ToggleQuiet => {
                let quiet = !self.notes.quiet;
                if let Err(e) = crate::dnd::write(quiet) {
                    eprintln!("shuru-desktop: do not disturb: {e}");
                }
                self.set_quiet(quiet)?;
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
        if ctrl
            && matches!(base, 0x63 | 0x78 | 0x76)
            && self.pop.as_ref().is_some_and(|p| p.what == What::Apps)
        {
            if base == 0x76 {
                let field = self.pop.as_mut().unwrap().field.as_mut().unwrap();
                if field.paste_pending {
                    return Ok(());
                }
                field.paste_pending = true;
                self.conn.convert_selection(
                    self.ours.pop,
                    self.atoms.CLIPBOARD,
                    self.atoms.UTF8_STRING,
                    self.atoms.SHURU_PASTE,
                    CURRENT_TIME,
                )?;
            } else {
                let field = self.pop.as_mut().unwrap().field.as_mut().unwrap();
                if let Some(text) = field.editor.copy_selection() {
                    self.clipboard = text;
                    self.conn.set_selection_owner(
                        self.ours.check,
                        self.atoms.CLIPBOARD,
                        CURRENT_TIME,
                    )?;
                    if base == 0x78 {
                        field.editor.delete_selection();
                        self.refresh_query()?;
                    }
                }
            }
            return Ok(());
        }
        let Some(p) = &self.pop else {
            return Ok(());
        };
        // Space types in the search field of Applications, else activates.
        let activate = matches!(base, 0xff0d | 0xff8d) || (base == 0x20 && p.what != What::Apps);
        match base {
            0xff1b => return self.close_pop(),
            _ if activate => {
                if let Some(a) = p.tree.selected.and_then(|i| p.rows[i].act().cloned()) {
                    self.choose(a)?;
                }
                return Ok(());
            }
            // Delete and BackSpace dismiss the notification under the keyboard.
            0xffff | 0xff08 if p.what == What::Notes => {
                let dismiss = p.tree.selected.and_then(|i| match &p.rows[i] {
                    PRow::Note { dismiss, .. } => Some(dismiss.clone()),
                    _ => None,
                });
                if let Some(a) = dismiss {
                    self.choose(a)?;
                }
                return Ok(());
            }
            0xff52 | 0xff54 => {
                if let Some(p) = &mut self.pop {
                    p.tree.step(base == 0xff54, p.shape.sheet.h);
                }
                return self.draw_pop();
            }
            _ => {}
        }
        if p.what != What::Apps {
            return Ok(());
        }
        let Some(p) = &mut self.pop else {
            return Ok(());
        };
        if !p
            .field
            .as_mut()
            .unwrap()
            .key(&mut self.font, base, keysym_char(sym), shift, ctrl)
        {
            return Ok(());
        }
        self.refresh_query()
    }

    pub(super) fn pop_release(&mut self) {
        if let Some(field) = self.pop.as_mut().and_then(|p| p.field.as_mut()) {
            field.dragging = false;
        }
    }

    pub(super) fn accessibility_tree(&self) -> accesskit::TreeUpdate {
        let mut nodes = Vec::new();
        let mut children = Vec::new();
        let mut focus = accesskit::NodeId(1);
        for (tree, view, origin, focused) in self.shell_surfaces() {
            let (mut surface, selected) = tree.accesskit(view, origin);
            children.push(accesskit::NodeId(tree.root_id()));
            nodes.append(&mut surface);
            if focused {
                focus = selected;
            }
        }
        let mut root = accesskit::Node::new(Role::Window);
        root.set_label("SuperHQ desktop");
        root.set_bounds(accesskit::Rect::new(
            0.0,
            0.0,
            self.sw as f64,
            self.sh as f64,
        ));
        root.set_children(children);
        nodes.push((accesskit::NodeId(1), root));
        crate::atspi::text_runs(&mut nodes);
        if let Some(field) = self.pop.as_ref().and_then(|p| p.field.as_ref()) {
            let (anchor, focus) = field.selection_indices();
            for (_, node) in &mut nodes {
                if node.role() == Role::SearchInput {
                    if let Some(&run) = node.children().first() {
                        node.set_text_selection(accesskit::TextSelection {
                            anchor: accesskit::TextPosition {
                                node: run,
                                character_index: anchor,
                            },
                            focus: accesskit::TextPosition {
                                node: run,
                                character_index: focus,
                            },
                        });
                    }
                }
            }
        }
        accesskit::TreeUpdate {
            nodes,
            tree: Some(accesskit::TreeInfo::new(accesskit::NodeId(1))),
            tree_id: accesskit::TreeId::ROOT,
            focus,
        }
    }

    fn shell_surfaces(&self) -> Vec<(&Tree, Rect, (i16, i16), bool)> {
        let mut surfaces = Vec::new();
        if let Some(t) = &self.panel_tree {
            surfaces.push((
                t,
                Rect::new(0.0, 0.0, self.sw as f32, PANEL_H as f32),
                (0, 0),
                false,
            ));
        }
        if let Some(t) = &self.rail_tree {
            surfaces.push((
                t,
                Rect::new(0.0, 0.0, chrome::RAIL_W as f32, self.rail_h as f32),
                (0, PANEL_H as i16),
                false,
            ));
        }
        for win in &self.stack {
            if let (Some(tree), Some(c)) = (
                self.bar_trees.get(win),
                self.clients
                    .get(win)
                    .filter(|c| c.framed && !c.fullscreen && !c.minimized && c.bar_h() > 0),
            ) {
                let (w, h) = tree.size();
                surfaces.push((
                    tree,
                    Rect::new(0.0, 0.0, w, h),
                    (c.card.0 - 1, c.card.1 - 1),
                    false,
                ));
            }
        }
        if let Some(p) = &self.pop {
            surfaces.push((&p.tree, p.shape.sheet, (p.rect.0, p.rect.1), true));
        }
        surfaces
    }

    pub(super) fn shell_tree(&self) -> Value {
        Value::Array(
            self.shell_surfaces()
                .into_iter()
                .flat_map(|(tree, view, origin, _)| {
                    tree.json(view, origin)
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .collect(),
        )
    }

    pub(super) fn rail_node_id(target: &Target) -> u64 {
        node_id(&format!("rail:{target:?}"))
    }

    pub(super) fn panel_hit(&self, x: f32, y: f32) -> Option<PanelHit> {
        let id = self.panel_tree.as_ref()?.hit_node(
            Rect::new(0.0, 0.0, self.sw as f32, PANEL_H as f32),
            x,
            y,
        )?;
        match id {
            chrome::APPLICATIONS_ID => Some(PanelHit::Apps),
            chrome::FRONT_APP_ID => Some(PanelHit::App),
            chrome::NOTIFICATIONS_ID => Some(PanelHit::Notes),
            _ => None,
        }
    }

    pub(super) fn rail_hit(&self, x: f32, screen_y: f32) -> Option<usize> {
        let id = self.rail_tree.as_ref()?.hit_node(
            Rect::new(
                0.0,
                PANEL_H as f32,
                chrome::RAIL_W as f32,
                self.rail_h as f32,
            ),
            x,
            screen_y,
        )?;
        self.rail_targets
            .iter()
            .position(|t| Self::rail_node_id(t) == id)
    }

    pub(super) fn panel_click(&mut self, hit: PanelHit) -> Result<()> {
        match hit {
            PanelHit::Apps => self.open_apps_menu(),
            PanelHit::App => {
                if let Some(t) = self.front_target() {
                    self.open_app_menu(t, Anchor::Panel)?;
                }
                Ok(())
            }
            PanelHit::Notes => self.open_notes(),
        }
    }

    pub(super) fn rail_click(&mut self, i: usize, button: u8) -> Result<()> {
        self.show_tooltip(None)?;
        let t = self.rail_targets[i].clone();
        match button {
            1 if self.in_front(&t) => self.open_app_menu(t, Anchor::Panel),
            1 => self.activate(&t),
            3 => self.open_app_menu(t, Anchor::Rail(i)),
            _ => Ok(()),
        }
    }

    pub(super) fn tree_action(
        &mut self,
        id: u64,
        action: &str,
        value: Option<String>,
    ) -> std::result::Result<Value, (u16, String)> {
        if self.clients.iter().any(|(&win, c)| {
            chrome::title_node_id(win, None) == id
                && c.framed
                && !c.fullscreen
                && !c.minimized
                && c.bar_h() > 0
        }) {
            return Err((400, "unsupported action".into()));
        }
        let title = self.clients.iter().find_map(|(&win, c)| {
            if !c.framed || c.fullscreen || c.minimized || c.bar_h() == 0 {
                return None;
            }
            let close_only = c.parent.is_some() || c.kind != super::Kind::Normal;
            chrome::title_light(win, close_only, id).map(|light| (win, light))
        });
        if let Some((win, light)) = title {
            if action != "click" {
                return Err((400, "unsupported action".into()));
            }
            return self
                .focus(win)
                .and_then(|()| self.light_click(win, light))
                .map(|()| json!({"ok":true}))
                .map_err(|e| (500, e.to_string()));
        }
        if let Some(click) = shell_click(id, self.panel_layout.app.is_some(), &self.rail_targets) {
            if action != "click" {
                return Err((400, "unsupported action".into()));
            }
            let out = self.close_pop().and_then(|()| match click {
                ShellClick::Panel(hit) => self.panel_click(hit),
                ShellClick::Rail(i) => self.rail_click(i, 1),
            });
            return out
                .map(|()| json!({"ok": true}))
                .map_err(|e| (500, e.to_string()));
        }
        if [chrome::PANEL_ID, chrome::RAIL_ID, chrome::AGENT_ID].contains(&id) {
            return Err((400, "unsupported action".into()));
        }
        let p = self.pop.as_mut().ok_or((404, "no open UI".into()))?;
        let wanted = match action {
            "click" => accesskit::Action::Click,
            "focus" => accesskit::Action::Focus,
            "set_value" => accesskit::Action::SetValue,
            _ => return Err((400, "unknown action".into())),
        };
        if id == p.tree.root_id() {
            return Err((400, "unsupported action".into()));
        }
        let row = p.tree.row_of(id).ok_or((404, "no such node".into()))?;
        if !p.tree.supports(row, wanted) {
            return Err((400, "unsupported action".into()));
        }
        let out = match wanted {
            accesskit::Action::Click => {
                let act = p.rows[row]
                    .act_for(id)
                    .ok_or((400, "unsupported action".into()))?;
                self.choose(act)
            }
            accesskit::Action::Focus => {
                p.tree.focused = Some(row);
                if p.rows[row].act().is_some() {
                    p.tree.pointer = false;
                    p.tree.select(Some(row), p.shape.sheet.h);
                }
                self.draw_pop()
            }
            _ => {
                let value = value.ok_or((400, "value is required".into()))?;
                let field = p.field.as_mut().ok_or((400, "unsupported action".into()))?;
                field.select_all(&mut self.font);
                field.insert(&value);
                self.refresh_query()
            }
        };
        out.map(|()| json!({"ok":true}))
            .map_err(|e| (500, e.to_string()))
    }

    pub(super) fn clipboard_request(&self, e: &SelectionRequestEvent) -> Result<()> {
        let property = if e.property == x11rb::NONE {
            e.target
        } else {
            e.property
        };
        let mut accepted = false;
        if e.selection == self.atoms.CLIPBOARD {
            if e.target == self.atoms.UTF8_STRING {
                self.conn.change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    self.atoms.UTF8_STRING,
                    self.clipboard.as_bytes(),
                )?;
                accepted = true;
            } else if e.target == self.atoms.TARGETS {
                self.conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[self.atoms.TARGETS, self.atoms.UTF8_STRING],
                )?;
                accepted = true;
            }
        }
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if accepted { property } else { x11rb::NONE },
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, event)?;
        Ok(())
    }

    pub(super) fn clipboard_paste(&mut self, e: &SelectionNotifyEvent) -> Result<()> {
        if e.requestor != self.ours.pop || e.selection != self.atoms.CLIPBOARD {
            return Ok(());
        }
        let Some(field) = self
            .pop
            .as_mut()
            .and_then(|p| p.field.as_mut())
            .filter(|f| f.paste_pending)
        else {
            return Ok(());
        };
        if e.property == x11rb::NONE {
            field.paste_pending = false;
            return Ok(());
        }
        if e.property != self.atoms.SHURU_PASTE {
            return Ok(());
        }
        let reply = self
            .conn
            .get_property(true, self.ours.pop, e.property, AtomEnum::ANY, 0, u32::MAX)?
            .reply()?;
        if reply.type_ == self.atoms.INCR {
            field.paste_chunks = Some(Vec::new());
            return Ok(());
        }
        field.paste_pending = false;
        if reply.type_ == self.atoms.UTF8_STRING && reply.format == 8 {
            if let Ok(text) = std::str::from_utf8(&reply.value) {
                field.insert(text);
                self.refresh_query()?;
            }
        }
        Ok(())
    }

    pub(super) fn clipboard_chunk(&mut self, e: &PropertyNotifyEvent) -> Result<()> {
        if e.state != Property::NEW_VALUE {
            return Ok(());
        }
        let Some(field) = self
            .pop
            .as_mut()
            .and_then(|p| p.field.as_mut())
            .filter(|f| f.paste_chunks.is_some())
        else {
            return Ok(());
        };
        let reply = self
            .conn
            .get_property(true, self.ours.pop, e.atom, AtomEnum::ANY, 0, u32::MAX)?
            .reply()?;
        if reply.type_ != self.atoms.UTF8_STRING || reply.format != 8 {
            field.paste_chunks = None;
            field.paste_pending = false;
            return Ok(());
        }
        if reply.value.is_empty() {
            let bytes = field.paste_chunks.take().unwrap();
            field.paste_pending = false;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                field.insert(text);
                self.refresh_query()?;
            }
        } else {
            field
                .paste_chunks
                .as_mut()
                .unwrap()
                .extend_from_slice(&reply.value);
        }
        Ok(())
    }

    fn refresh_query(&mut self) -> Result<()> {
        let query = self
            .pop
            .as_ref()
            .and_then(|p| p.field.as_ref())
            .map(Field::value)
            .unwrap_or_default();
        if self.pop.as_ref().is_some_and(|p| p.query == query) {
            return self.draw_pop();
        }
        let rows = self.apps_rows(&query);
        let sel = Self::first_act(&rows);
        self.show_pop(What::Apps, rows, query, sel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(width: f32) -> Style {
        style().width(width)
    }

    fn launcher() -> Vec<PRow> {
        vec![
            PRow::Search,
            PRow::Section("Applications".into()),
            PRow::Item {
                name: "Terminal".into(),
                entry: None,
                detail: Some("Shell".into()),
                current: true,
                minimized: false,
                act: Act::Launch(0),
            },
            PRow::Sep,
            PRow::Action {
                name: "Quit".into(),
                act: Act::Launch(1),
            },
            PRow::Footer,
        ]
    }

    #[test]
    fn shell_clicks_route_by_stable_identity() {
        let terminal = Target::Key("terminal".into());
        let browser = Target::App(0);
        let id = Wm::rail_node_id(&terminal);
        assert_eq!(
            shell_click(chrome::APPLICATIONS_ID, false, &[]),
            Some(ShellClick::Panel(PanelHit::Apps))
        );
        assert_eq!(shell_click(chrome::FRONT_APP_ID, false, &[]), None);
        assert_eq!(
            shell_click(chrome::FRONT_APP_ID, true, &[]),
            Some(ShellClick::Panel(PanelHit::App))
        );
        assert_eq!(
            shell_click(id, false, &[terminal.clone(), browser.clone()]),
            Some(ShellClick::Rail(0))
        );
        assert_eq!(
            shell_click(id, false, &[browser, terminal]),
            Some(ShellClick::Rail(1))
        );
        assert_eq!(shell_click(id, false, &[]), None);
    }

    #[test]
    fn a_sheet_fits_its_rows_and_paints_them() {
        let mut font = Font::for_test();
        let names = |_: Option<usize>, n: &str| n.to_string();
        let mut tree = menu_tree(
            &mut font,
            sheet(APPS_W),
            "apps",
            "Apps",
            &launcher(),
            &names,
        );
        assert_eq!(
            tree.size(),
            (APPS_W, 10.0 + 30.0 + 22.0 + 26.0 + 9.0 + 24.0 + 29.0)
        );
        let field = tree.field_bounds((0.0, 0.0)).unwrap();
        assert_eq!(
            field,
            Rect::new(
                SHEET_PAD + 32.0,
                SHEET_PAD + 5.0,
                APPS_W - 2.0 * SHEET_PAD - 44.0,
                18.0
            )
        );
        tree.select(Some(2), 200.0);
        let (w, h) = tree.size();
        let mut c = Canvas::new(w as usize, h as usize, crate::draw::theme().surface);
        let icons = |_: usize| None;
        let mut paint = Paint {
            font: &mut font,
            icons: &icons,
            field: None,
        };
        tree.paint(&mut c, &mut paint, Rect::new(0.0, 0.0, w, h));
        assert!(c.px.contains(&crate::draw::theme().selected));
        assert!(c.px.contains(&crate::draw::theme().raised));
    }

    fn center() -> Vec<PRow> {
        vec![
            PRow::Toggle {
                name: "Do not disturb".into(),
                on: true,
                act: Act::ToggleQuiet,
            },
            PRow::Sep,
            PRow::Section("Notifications".into()),
            PRow::Note {
                app: "Chromium".into(),
                summary: "Build finished".into(),
                body: "All 93 tests passed".into(),
                entry: None,
                act: Act::OpenNote(7),
                dismiss: Act::Dismiss(7),
            },
            PRow::Sub {
                name: "View log".into(),
                act: Act::NoteAction(7, "log".into()),
            },
            PRow::Sep,
            PRow::Action {
                name: "Clear all".into(),
                act: Act::ClearAll,
            },
        ]
    }

    #[test]
    fn the_center_routes_a_dismiss_button_apart_from_its_row() {
        let rows = center();
        let note = &rows[3];
        assert_eq!(
            note.act_for(act_id(&Act::Dismiss(7))),
            Some(Act::Dismiss(7))
        );
        assert_eq!(
            note.act_for(act_id(&Act::OpenNote(7))),
            Some(Act::OpenNote(7))
        );
        assert!(Act::Dismiss(7).keeps_open());
        assert!(Act::ToggleQuiet.keeps_open());
        assert!(Act::ClearAll.keeps_open());
        assert!(!Act::OpenNote(7).keeps_open());
        assert!(!Act::NoteAction(7, "log".into()).keeps_open());
    }

    #[test]
    fn the_center_describes_its_rows() {
        let mut font = Font::for_test();
        let names = |_: Option<usize>, n: &str| n.to_string();
        let rows = center();
        let mut tree = locale::with_lang(locale::Lang::En, || {
            menu_tree(
                &mut font,
                sheet(NOTES_W),
                "notes",
                "Notifications",
                &rows,
                &names,
            )
        });
        let quiet = tree.semantic(act_id(&Act::ToggleQuiet)).unwrap();
        assert_eq!(quiet.role(), Role::MenuItemCheckBox);
        assert_eq!(quiet.toggled(), Some(accesskit::Toggled::True));
        assert!(quiet.supports_action(accesskit::Action::Click));
        let note = tree.semantic(act_id(&Act::OpenNote(7))).unwrap();
        assert_eq!(note.label(), Some("Build finished"));
        assert_eq!(note.description(), Some("Chromium: All 93 tests passed"));
        let dismiss = tree.semantic(act_id(&Act::Dismiss(7))).unwrap();
        assert_eq!(dismiss.role(), Role::Button);
        assert_eq!(dismiss.label(), Some("Dismiss Build finished"));
        assert!(dismiss.supports_action(accesskit::Action::Click));
        let (w, h) = tree.size();
        let view = Rect::new(0.0, 0.0, w, h);
        let row = tree.row_of(act_id(&Act::Dismiss(7))).unwrap();
        assert_eq!(row, 3, "the button belongs to the row of its notification");
        let b = tree.row_bounds(row, (0.0, 0.0));
        assert_eq!(
            tree.hit_node(view, b.x + b.w - 14.0, b.y + b.h / 2.0),
            Some(act_id(&Act::Dismiss(7)))
        );
        assert_eq!(
            tree.hit_node(view, b.x + 60.0, b.y + b.h / 2.0),
            Some(act_id(&Act::OpenNote(7)))
        );
        let walked: Vec<usize> = (0..5)
            .map(|_| {
                tree.step(true, h);
                tree.selected.unwrap()
            })
            .collect();
        assert_eq!(walked, [0, 3, 4, 6, 0], "arrows skip what does not act");
    }

    #[test]
    fn an_empty_center_says_so() {
        let mut font = Font::for_test();
        let names = |_: Option<usize>, n: &str| n.to_string();
        let rows = vec![PRow::Empty("No notifications".into())];
        let tree = menu_tree(
            &mut font,
            sheet(NOTES_W),
            "notes",
            "Notifications",
            &rows,
            &names,
        );
        let json = tree.json(Rect::new(0.0, 0.0, NOTES_W, 100.0), (0, 0));
        assert!(json
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["role"] == "label" && n["name"] == "No notifications"));
    }

    #[test]
    fn a_one_item_menu_is_one_row_tall() {
        let mut font = Font::for_test();
        let names = |_: Option<usize>, n: &str| n.to_string();
        let rows = [PRow::Action {
            name: "Quit".into(),
            act: Act::Launch(0),
        }];
        let s = style().min_width(APP_W).max_width(APP_MAX_W);
        let tree = menu_tree(&mut font, s, "app", "Files", &rows, &names);
        assert_eq!(tree.size(), (APP_W, 24.0 + 2.0 * SHEET_PAD));
    }

    #[test]
    fn an_app_menu_widens_for_a_long_action() {
        let mut font = Font::for_test();
        let names = |_: Option<usize>, n: &str| n.to_string();
        let name = "Open a new private window in the background";
        let rows = [PRow::Action {
            name: name.into(),
            act: Act::Launch(0),
        }];
        let s = style().min_width(APP_W).max_width(APP_MAX_W);
        let tree = menu_tree(&mut font, s, "app", "Files", &rows, &names);
        let want = (font.width(name, TEXT_SIZE) + 16.0 + 2.0 * SHEET_PAD).clamp(APP_W, APP_MAX_W);
        assert!(
            (tree.size().0 - want).abs() < 0.01,
            "{} vs {want}",
            tree.size().0
        );
    }

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
