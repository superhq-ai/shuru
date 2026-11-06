//! What the desktop draws itself: the panel, the rail, title bars, window
//! corners, menus, the click ripple and the cursor. Pure pixels, no X.

use std::f32::consts::{FRAC_PI_2, PI};

use crate::draw::{self, theme, Canvas, Font, Image, Rect};
use crate::glyphs;
use crate::locale::{tr, Msg};

mod faces;
pub use faces::bell;
use faces::{Agent, PanelBell, PanelControl, RailTile, TitleSurface, WindowLight};

pub const PANEL_H: u16 = 32;
pub const RAIL_W: u16 = 64;
pub const BAR_H: u16 = 30;
/// The corner radius of a window card.
pub const RADIUS: f32 = 12.0;
/// The corner piece of a frame: the radius and the ring.
pub const CORNER: u16 = 13;
pub const RIPPLE: u16 = 44;
pub const TOOLTIP_H: u16 = 22;

/// Rail items: 48 px at x 8, the first at y 40, 52 px apart.
pub const ITEM: u16 = 48;
pub const ITEM_X: i16 = 8;
pub const ITEM_TOP: i16 = 40;
pub const ITEM_PITCH: i16 = 52;

const TEXT_SIZE: f32 = 13.0;
const SMALL_SIZE: f32 = 12.0;
const CONTROL_Y: f32 = 4.0;
const CONTROL_H: f32 = 24.0;

/// Where windows go: right of the rail, under the panel, 8 px from the
/// bottom and right edges.
pub fn work_area(sw: u16, sh: u16) -> (i16, i16, u16, u16) {
    (
        RAIL_W as i16,
        PANEL_H as i16 + 8,
        sw.saturating_sub(RAIL_W + 8),
        sh.saturating_sub(PANEL_H + 16),
    )
}

/// The screen y of rail item `i`.
pub fn item_y(i: usize) -> i16 {
    ITEM_TOP + ITEM_PITCH * i as i16
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHit {
    Apps,
    App,
    Notes,
}

/// What the notification control on the panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bell {
    /// A notification came since the center was last open.
    pub unseen: bool,
    /// Do not disturb is on.
    pub quiet: bool,
}

/// The width of the notification control.
pub const BELL_W: f32 = 32.0;

pub struct PanelView<'a> {
    pub app: Option<(&'a str, Option<&'a Image>)>,
    /// None, the agent (false) or a person (true).
    pub person: Option<bool>,
    pub hover: Option<PanelHit>,
    pub bell: Bell,
}

/// The spans of the panel controls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelLayout {
    pub apps: (f32, f32),
    pub app: Option<(f32, f32)>,
    /// The notification control, at the end of the panel.
    pub notes: (f32, f32),
}

const MARK_X: f32 = 8.0;
const LABEL_X: f32 = 35.0;

pub fn panel_layout(font: &mut Font, w: usize, view: &PanelView) -> PanelLayout {
    let apps_w = (LABEL_X + font.width(tr(Msg::Applications), TEXT_SIZE) + 8.0).round();
    let apps = (4.0, apps_w);
    let app = view.app.map(|(name, _)| {
        let x = apps.0 + apps.1 + 4.0;
        (x, (32.0 + font.width(name, TEXT_SIZE) + 8.0).round())
    });
    let notes = (w as f32 - 4.0 - BELL_W, BELL_W);
    PanelLayout { apps, app, notes }
}

/// The Applications control at (x, y) of its pill: the hq mark and label.
pub fn apps_label(c: &mut Canvas, font: &mut Font, x: f32, y: f32) {
    c.glyph(
        &glyphs::MARK,
        (x + MARK_X) as i32,
        (y + 7.0) as i32,
        theme().text,
        1.0,
    );
    font.draw(
        c,
        tr(Msg::Applications),
        x + LABEL_X,
        y + 12.0,
        TEXT_SIZE,
        theme().text,
    );
}

/// The app item at (x, y) of its pill: the 16 px icon and the name.
pub fn app_label(
    c: &mut Canvas,
    font: &mut Font,
    x: f32,
    y: f32,
    name: &str,
    icon: Option<&Image>,
) {
    app_icon(c, font, icon, name, x + 8.0, y + 4.0, 16, 1.0);
    font.draw(c, name, x + 32.0, y + 12.0, TEXT_SIZE, theme().text);
}

/// An app icon, or for an app without one a tile with its initial.
#[allow(clippy::too_many_arguments)]
pub fn app_icon(
    c: &mut Canvas,
    font: &mut Font,
    icon: Option<&Image>,
    name: &str,
    x: f32,
    y: f32,
    size: usize,
    opacity: f32,
) {
    match icon {
        Some(img) => c.image(img, x as i32, y as i32, opacity),
        None => {
            let s = size as f32;
            c.rounded_rect(
                Rect::new(x, y, s, s),
                s / 4.0,
                draw::mix(theme().bg, theme().raised, opacity),
            );
            let initial: String = name.chars().take(1).flat_map(char::to_uppercase).collect();
            let fs = (s * 0.55).round();
            let tw = font.width(&initial, fs);
            font.draw(
                c,
                &initial,
                x + (s - tw) / 2.0,
                y + s / 2.0,
                fs,
                draw::mix(theme().bg, theme().text, opacity),
            );
        }
    }
}

/// The panel: 32 px on BG.
#[cfg(test)]
pub fn panel(font: &mut Font, w: usize, view: &PanelView, layout: &PanelLayout) -> Canvas {
    let tree = panel_tree(font, w, view, layout);
    let mut c = Canvas::new(w, PANEL_H as usize, theme().bg);
    let icons = |_| view.app.and_then(|(_, icon)| icon);
    tree.paint(
        &mut c,
        &mut crate::ui::widgets::Paint {
            font,
            icons: &icons,
            field: None,
        },
        Rect::new(0.0, 0.0, w as f32, PANEL_H as f32),
    );
    c
}

pub const PANEL_ID: u64 = 10;
pub const APPLICATIONS_ID: u64 = 11;
pub const FRONT_APP_ID: u64 = 12;
pub const AGENT_ID: u64 = 13;
pub const NOTIFICATIONS_ID: u64 = 14;
pub const RAIL_ID: u64 = 20;

pub fn panel_tree(
    font: &mut Font,
    w: usize,
    view: &PanelView,
    layout: &PanelLayout,
) -> crate::ui::Tree {
    use crate::ui::widgets::Content;
    use crate::ui::{style, Builder, Node};
    use accesskit::Role;
    let mut b = Builder::new(
        Node::new(
            Content::Box,
            style()
                .row()
                .width(w as f32)
                .height(PANEL_H as f32)
                .pad(CONTROL_Y, 4.0, CONTROL_Y, 4.0),
        )
        .semantic(PANEL_ID, Role::Toolbar, tr(Msg::Panel)),
    );
    b.add(
        0,
        Node::new(
            PanelControl::content(None, view.hover == Some(PanelHit::Apps)),
            style().width(layout.apps.1).height(CONTROL_H),
        )
        .semantic(APPLICATIONS_ID, Role::Button, tr(Msg::Applications)),
    );
    if let (Some((name, _)), Some((_, width))) = (view.app, layout.app) {
        b.add(
            0,
            Node::new(
                PanelControl::content(Some(name.into()), view.hover == Some(PanelHit::App)),
                style()
                    .width(width)
                    .height(CONTROL_H)
                    .margin(0.0, 0.0, 0.0, 4.0),
            )
            .semantic(FRONT_APP_ID, Role::Button, name),
        );
    }
    b.add(0, Node::new(Content::Box, style().grow()));
    if view.person == Some(false) {
        let width = font.width(tr(Msg::Agent), TEXT_SIZE) + 16.0;
        b.add(
            0,
            Node::new(Agent::content(), style().width(width).height(CONTROL_H)).semantic(
                AGENT_ID,
                Role::Label,
                tr(Msg::Agent),
            ),
        );
    }
    let mut bell = Node::new(
        PanelBell::content(view.bell, view.hover == Some(PanelHit::Notes)),
        style()
            .width(layout.notes.1)
            .height(CONTROL_H)
            .margin(0.0, 0.0, 0.0, 4.0),
    )
    .semantic(NOTIFICATIONS_ID, Role::Button, tr(Msg::Notifications));
    if let Some(s) = bell.semantic.as_mut() {
        s.set_has_popup(accesskit::HasPopup::Menu);
        if view.bell.quiet {
            s.set_description(tr(Msg::DoNotDisturb));
        }
    }
    b.add(0, bell);
    b.finish(font)
}

pub struct RailItem<'a> {
    pub name: &'a str,
    pub icon: Option<&'a Image>,
    pub windows: usize,
    pub front: bool,
    /// One of its windows asks for attention.
    pub attention: bool,
}

pub fn rail_tree(
    font: &mut Font,
    items: &[RailItem],
    ids: &[u64],
    hover: Option<usize>,
    h: usize,
    top: i16,
) -> crate::ui::Tree {
    use crate::ui::widgets::Content;
    use crate::ui::{style, Builder, Node};
    use accesskit::Role;
    let mut b = Builder::new(
        Node::new(
            Content::Box,
            style().column().width(RAIL_W as f32).height(h as f32).pad(
                (ITEM_TOP - top) as f32,
                8.0,
                0.0,
                8.0,
            ),
        )
        .semantic(RAIL_ID, Role::Toolbar, tr(Msg::Applications)),
    );
    for (i, item) in items.iter().enumerate() {
        let mut node = Node::new(
            RailTile {
                name: item.name.into(),
                entry: i,
                windows: item.windows,
                front: item.front,
                attention: item.attention,
                hover: hover == Some(i),
            }
            .content(),
            style()
                .width(ITEM as f32)
                .height(ITEM as f32)
                .margin(0.0, 0.0, 4.0, 0.0),
        )
        .semantic(ids[i], Role::Button, item.name);
        node.semantic.as_mut().unwrap().set_selected(item.front);
        b.add(0, node);
    }
    b.finish(font)
}

/// The icon and running dots of a rail item whose box is at (x, y).
pub fn rail_icon(c: &mut Canvas, font: &mut Font, item: &RailItem, x: f32, y: f32) {
    app_icon(c, font, item.icon, item.name, x + 8.0, y + 8.0, 32, 1.0);
    if item.attention {
        c.rounded_rect(Rect::new(x + 37.0, y + 5.0, 6.0, 6.0), 3.0, theme().text);
    }
    let n = item.windows.min(3);
    if n > 0 {
        let total = 6.0 * n as f32 + 3.0 * (n - 1) as f32;
        let top = y + (ITEM as f32 - total) / 2.0;
        for k in 0..n {
            c.rounded_rect(
                Rect::new(x + 1.0, top + 9.0 * k as f32, 6.0, 6.0),
                3.0,
                theme().text,
            );
        }
    }
}

/// A tooltip: 22 px, RAISED, a HAIRLINE_HI ring outside, and its shape.
pub fn tooltip(font: &mut Font, text: &str) -> (Canvas, Canvas) {
    let w = (font.width(text, TEXT_SIZE) + 16.0).ceil();
    let h = TOOLTIP_H as f32;
    let b = Rect::new(1.0, 1.0, w, h);
    let mut c = Canvas::new(w as usize + 2, h as usize + 2, theme().bg);
    c.rounded_rect(b, 5.0, theme().raised);
    c.ring_out(b, 5.0, theme().hairline_hi);
    font.draw(&mut c, text, 9.0, 1.0 + h / 2.0, TEXT_SIZE, theme().text);
    let mut m = Canvas::new(c.w, c.h, 0);
    m.rounded_rect(b.inset(-1.0), 6.0, 0xffffff);
    (c, m)
}

/// The width of a notification card inside its ring.
pub const NOTE_W: usize = 340;
const NOTE_PAD_X: f32 = 12.0;
const NOTE_PAD_Y: f32 = 10.0;
const NOTE_TEXT_X: f32 = NOTE_PAD_X + 20.0 + 10.0;
const NOTE_CLOSE: f32 = 18.0;

/// The close button of a card, in card coordinates.
pub fn note_close() -> Rect {
    Rect::new(
        1.0 + NOTE_W as f32 - NOTE_PAD_X - NOTE_CLOSE,
        1.0 + NOTE_PAD_Y,
        NOTE_CLOSE,
        NOTE_CLOSE,
    )
}

/// A notification card: the app icon, the summary, up to two lines of body
/// and a close button. Answers the card and its shape.
pub fn note(
    font: &mut Font,
    icon: Option<&Image>,
    app: &str,
    summary: &str,
    body: &str,
) -> (Canvas, Canvas) {
    let t = theme();
    let text_w = NOTE_W as f32 - NOTE_TEXT_X - 10.0 - NOTE_CLOSE - NOTE_PAD_X;
    let body = font.wrap(body, SMALL_SIZE, text_w, 2);
    let title = if summary.is_empty() { app } else { summary };
    let text_h = 16.0 + body.len() as f32 * 15.0 + if body.is_empty() { 0.0 } else { 2.0 };
    let h = 2.0 * NOTE_PAD_Y + text_h.max(20.0);
    let b = Rect::new(1.0, 1.0, NOTE_W as f32, h);
    let mut c = Canvas::new(NOTE_W + 2, h as usize + 2, t.bg);
    c.rounded_rect(b, 10.0, t.surface);
    c.ring_out(b, 10.0, t.hairline_hi);
    app_icon(
        &mut c,
        font,
        icon,
        app,
        1.0 + NOTE_PAD_X,
        1.0 + NOTE_PAD_Y,
        20,
        1.0,
    );
    let x = 1.0 + NOTE_TEXT_X;
    let title = font.fit(title, TEXT_SIZE, text_w);
    font.draw(&mut c, &title, x, 1.0 + NOTE_PAD_Y + 8.0, TEXT_SIZE, t.text);
    for (i, line) in body.iter().enumerate() {
        let mid = 1.0 + NOTE_PAD_Y + 16.0 + 2.0 + 7.5 + 15.0 * i as f32;
        font.draw(&mut c, line, x, mid, SMALL_SIZE, t.dim);
    }
    let close = note_close();
    c.glyph(
        &glyphs::CLOSE,
        (close.x + 1.0) as i32,
        (close.y + 1.0) as i32,
        t.dim,
        1.0,
    );
    let mut m = Canvas::new(c.w, c.h, 0);
    m.rounded_rect(b.inset(-1.0), 11.0, 0xffffff);
    (c, m)
}

/// The line on an empty desktop.
pub fn empty(font: &mut Font, w: usize) -> Canvas {
    let mut c = Canvas::new(w, 20, theme().bg);
    let tw = font.width(tr(Msg::NoWindows), TEXT_SIZE);
    font.draw(
        &mut c,
        tr(Msg::NoWindows),
        ((w as f32 - tw) / 2.0).round(),
        10.0,
        TEXT_SIZE,
        theme().dim,
    );
    c
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Light {
    Close,
    Minimize,
    Maximize,
}

/// The lights of a bar and their x in the card.
pub fn lights(close_only: bool) -> &'static [(Light, f32)] {
    if close_only {
        &[(Light::Close, 10.0)]
    } else {
        &[
            (Light::Close, 10.0),
            (Light::Minimize, 28.0),
            (Light::Maximize, 46.0),
        ]
    }
}

pub const LIGHT_Y: f32 = 9.0;
pub const LIGHT_D: f32 = 12.0;

/// Whether (x, y) of a card is over the group of lights.
pub fn over_lights(close_only: bool, x: f32, y: f32) -> bool {
    let right = lights(close_only)
        .last()
        .map_or(0.0, |(_, lx)| lx + LIGHT_D);
    (LIGHT_Y - 3.0..LIGHT_Y + LIGHT_D + 3.0).contains(&y) && (7.0..right + 3.0).contains(&x)
}

pub struct BarView<'a> {
    pub title: &'a str,
    pub focused: bool,
    pub ring: u32,
    pub close_only: bool,
    pub maximized: bool,
    pub hover: bool,
    pub pressed: Option<Light>,
}

pub fn title_node_id(window: u32, light: Option<Light>) -> u64 {
    ((window as u64) << 32)
        | match light {
            None => 0xb400,
            Some(Light::Close) => 0xb401,
            Some(Light::Minimize) => 0xb402,
            Some(Light::Maximize) => 0xb403,
        }
}

pub fn title_light(window: u32, close_only: bool, id: u64) -> Option<Light> {
    lights(close_only)
        .iter()
        .find_map(|(light, _)| (title_node_id(window, Some(*light)) == id).then_some(*light))
}

pub fn title_tree(font: &mut Font, window: u32, w: usize, v: &BarView) -> crate::ui::Tree {
    use crate::ui::widgets::{Content, Ink};
    use crate::ui::{style, Builder, Node};
    use accesskit::Role;
    let mut b = Builder::new(
        Node::new(
            TitleSurface::content(v.ring),
            style().width(w as f32 + 2.0).height(BAR_H as f32 + 1.0),
        )
        .semantic(title_node_id(window, None), Role::TitleBar, v.title),
    );
    for &(light, x) in lights(v.close_only) {
        let verb = tr(match light {
            Light::Close => Msg::Close,
            Light::Minimize => Msg::Minimize,
            Light::Maximize if v.maximized => Msg::Restore,
            Light::Maximize => Msg::Maximize,
        });
        let button = b.add(
            0,
            Node::new(
                Content::Box,
                style()
                    .at(x - 2.0, LIGHT_Y - 2.0)
                    .width(LIGHT_D + 6.0)
                    .height(LIGHT_D + 6.0),
            )
            .semantic(
                title_node_id(window, Some(light)),
                Role::Button,
                &format!("{verb} {}", v.title),
            ),
        );
        b.add(
            button,
            Node::new(
                WindowLight {
                    light,
                    lit: v.focused || v.hover,
                    hover: v.hover,
                    pressed: v.pressed == Some(light),
                    maximized: v.maximized,
                }
                .content(),
                style().at(3.0, 3.0).width(LIGHT_D).height(LIGHT_D),
            ),
        );
    }
    let width = w as f32 - 148.0;
    if width > 8.0 && !v.title.is_empty() {
        b.add(
            0,
            Node::new(
                Content::CenteredText {
                    text: v.title.into(),
                    size: SMALL_SIZE,
                    ink: if v.focused { Ink::Text } else { Ink::Dim },
                },
                style().at(75.0, 10.0).width(width).height(12.0),
            ),
        );
    }
    b.finish(font)
}

#[cfg(test)]
pub fn title_bar(font: &mut Font, w: usize, v: &BarView) -> Canvas {
    let tree = title_tree(font, 100, w, v);
    let mut c = Canvas::new(w + 2, BAR_H as usize + 1, theme().bg);
    tree.paint(
        &mut c,
        &mut crate::ui::widgets::Paint {
            font,
            icons: &|_| None,
            field: None,
        },
        Rect::new(0.0, 0.0, w as f32 + 2.0, BAR_H as f32 + 1.0),
    );
    c
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A corner of a frame: the ring round the card corner over the client,
/// and its shape: the pixels the card does not cover.
pub fn corner(which: Corner, ring: u32) -> (Canvas, Canvas) {
    let s = CORNER as usize;
    let big = 4.0 * s as f32;
    let outer = Rect::new(0.0, 0.0, big, big);
    let inner = outer.inset(1.0);
    let mut c = Canvas::new(s, s, theme().bg);
    let mut m = Canvas::new(s, s, 0);
    for j in 0..s {
        for i in 0..s {
            let (x, y) = match which {
                Corner::TopLeft => (i, j),
                Corner::TopRight => (s - 1 - i, j),
                Corner::BottomLeft => (i, s - 1 - j),
                Corner::BottomRight => (s - 1 - i, s - 1 - j),
            };
            let co = draw::coverage(x, y, outer, RADIUS + 1.0);
            let ci = draw::coverage(x, y, inner, RADIUS);
            c.px[j * s + i] = draw::mix(draw::mix(theme().bg, ring, co), theme().winbar, ci);
            if co >= 0.5 && ci < 0.5 {
                m.px[j * s + i] = 0xffffff;
            }
        }
    }
    (c, m)
}

/// The rows of a rounded rectangle `w` by `h` as SHAPE rectangles.
pub fn rounded_shape(w: u16, h: u16, r: f32) -> Vec<(i16, i16, u16, u16)> {
    let b = Rect::new(0.0, 0.0, w as f32, h as f32);
    let ri = r.ceil() as u16;
    if h <= 2 * ri || w <= 2 * ri {
        return draw::mask_rects(w as usize, h as usize, |x, y| {
            draw::coverage(x, y, b, r) >= 0.5
        })
        .into_iter()
        .map(|(x, y, rw)| (x, y, rw, 1))
        .collect();
    }
    let mut out = Vec::new();
    for y in (0..ri).chain(h - ri..h) {
        let first = (0..w / 2).find(|&x| draw::coverage(x as usize, y as usize, b, r) >= 0.5);
        if let Some(x) = first {
            out.push((x as i16, y as i16, w - 2 * x, 1));
        }
    }
    out.push((0, ri as i16, w, h - 2 * ri));
    out.sort_by_key(|r| r.1);
    out
}

/// The top and bottom bands of the snap preview, `r` rows each: SNAP
/// with a HAIRLINE_HI ring inside the edge.
pub fn snap_band(w: usize, h: usize, top: bool) -> Canvas {
    let r = RADIUS as usize;
    let mut c = Canvas::new(w, r, theme().bg);
    let b = Rect::new(
        0.0,
        if top { 0.0 } else { r as f32 - h as f32 },
        w as f32,
        h as f32,
    );
    c.rounded_rect(b, RADIUS, theme().snap);
    c.ring_in(b, RADIUS, theme().hairline_hi);
    c
}

/// Which way a connected popover runs from its pill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Under a panel item, left edges aligned.
    Below,
    /// Under a panel item at the end of the panel, right edges aligned.
    BelowEnd,
    /// Right of a rail item, top edges aligned.
    Right,
    /// Right of a rail item, bottom edges aligned.
    RightUp,
}

/// A pill and the sheet it flows into, in window coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Connected {
    pub pill: Rect,
    pub sheet: Rect,
    pub pill_r: f32,
    pub side: Side,
}

const FILLET: f32 = 10.0;
const SHEET_R: f32 = 10.0;

impl Connected {
    /// The outline grown by `d`, as points of window coordinates. The edge
    /// at `d` 0 runs on pixel centers.
    pub fn outline(&self, d: f32) -> Vec<(f32, f32)> {
        // Laid out as for Below: u across, v down from the pill.
        let to = |x: f32, y: f32| match self.side {
            Side::Below => (x, y),
            Side::BelowEnd => (-x, y),
            Side::Right => (y, x),
            Side::RightUp => (-y, x),
        };
        let back = |u: f32, v: f32| match self.side {
            Side::Below => (u, v),
            Side::BelowEnd => (-u, v),
            Side::Right => (v, u),
            Side::RightUp => (v, -u),
        };
        let boxed = |r: Rect| {
            let (a, b) = (to(r.x, r.y), to(r.x + r.w, r.y + r.h));
            (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
        };
        let (pl, pt, pr_, _) = boxed(self.pill);
        let (sl, st, sr, sb) = boxed(self.sheet);
        let (l, r, t) = (pl + 0.5, pr_ - 0.5, pt + 0.5);
        let (s, s_l, s_r, b) = (st + 0.5, sl + 0.5, sr - 0.5, sb - 0.5);
        let rp = self.pill_r;
        let mut pts = Vec::new();
        let mut arc = |cu: f32, cv: f32, rad: f32, a0: f32, a1: f32| {
            let n = (rad.abs() * 1.5).ceil().max(2.0) as usize;
            for k in 0..=n {
                let a = a0 + (a1 - a0) * k as f32 / n as f32;
                pts.push(back(cu + rad * a.cos(), cv + rad * a.sin()));
            }
        };
        arc(l + rp, t + rp, rp + d, PI, 1.5 * PI);
        arc(r - rp, t + rp, rp + d, 1.5 * PI, 2.0 * PI);
        if s_r > r + 0.5 {
            arc(r + FILLET, s - FILLET, FILLET - d, PI, FRAC_PI_2);
            arc(s_r - SHEET_R, s + SHEET_R, SHEET_R + d, 1.5 * PI, 2.0 * PI);
        }
        if s_r < r - 0.5 {
            let neck = ((r - s_r) / 2.0).min(6.0);
            arc(r - neck, s - neck, neck + d, 0.0, FRAC_PI_2);
            arc(s_r + neck, s + neck, neck - d, 1.5 * PI, PI);
        }
        arc(s_r - SHEET_R, b - SHEET_R, SHEET_R + d, 0.0, FRAC_PI_2);
        arc(s_l + SHEET_R, b - SHEET_R, SHEET_R + d, FRAC_PI_2, PI);
        pts
    }
}

/// The connected shape on a `w` by `h` canvas: SURFACE inside, a 1 px
/// HAIRLINE_HI outline, and the shape mask.
pub fn connected(w: usize, h: usize, shape: &Connected) -> (Canvas, Canvas) {
    let fill = draw::fill_coverage(w, h, &shape.outline(0.0));
    let outer = draw::fill_coverage(w, h, &shape.outline(0.5));
    let inner = draw::fill_coverage(w, h, &shape.outline(-0.5));
    let mut c = Canvas::new(w, h, theme().bg);
    let mut m = Canvas::new(w, h, 0);
    for i in 0..w * h {
        let p = draw::mix(theme().bg, theme().surface, fill[i]);
        c.px[i] = draw::mix(p, theme().hairline_hi, outer[i] - inner[i]);
        if outer[i] >= 0.5 {
            m.px[i] = 0xffffff;
        }
    }
    if matches!(shape.side, Side::Below | Side::BelowEnd) {
        let p = shape.pill;
        let y = p.y as usize + 1;
        for x in (p.x + shape.pill_r + 1.0) as usize..(p.x + p.w - shape.pill_r) as usize {
            c.px[y * w + x] = theme().pill_hi;
        }
    }
    (c, m)
}

/// A ring around a click, and its shape.
pub fn ripple(person: bool) -> (Canvas, Canvas) {
    let s = RIPPLE as usize;
    let mid = s as f32 / 2.0;
    let mut c = Canvas::new(s, s, theme().bg);
    c.ring(mid, mid, 15.0, 7.0, theme().bg);
    c.ring(
        mid,
        mid,
        15.0,
        3.5,
        if person { theme().amber } else { theme().text },
    );
    let mut mask = Canvas::new(s, s, 0);
    mask.ring(mid, mid, 15.0, 7.0, 0xffffff);
    (c, mask)
}

/// A cursor image from XFixes, premultiplied ARGB, as pixels and shape.
pub fn cursor(w: usize, h: usize, argb: &[u32]) -> (Canvas, Canvas) {
    let mut c = Canvas::new(w, h, 0);
    let mut mask = Canvas::new(w, h, 0);
    for (i, &p) in argb.iter().take(w * h).enumerate() {
        let a = p >> 24;
        if a < 128 {
            continue;
        }
        let un = |s: u32| (((p >> s) & 0xff) * 255 / a).min(255) << s;
        c.px[i] = un(16) | un(8) | un(0);
        mask.px[i] = 0xffffff;
    }
    (c, mask)
}

/// Whether a mask pixel is in the shape.
pub fn inside(mask: &Canvas, x: usize, y: usize) -> bool {
    (mask.px[y * mask.w + x] & 0xff) >= 128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_work_area_is_right_of_the_rail_under_the_panel() {
        assert_eq!(work_area(1280, 800), (64, 40, 1208, 752));
    }

    #[test]
    fn the_panel_lays_out_its_controls() {
        let mut f = Font::for_test();
        let view = PanelView {
            app: Some(("Chromium", None)),
            person: Some(true),
            hover: None,
            bell: Bell::default(),
        };
        let l = panel_layout(&mut f, 1280, &view);
        assert_eq!(l.apps.0, 4.0);
        let (ax, _) = l.app.unwrap();
        assert_eq!(ax, l.apps.0 + l.apps.1 + 4.0);
        let c = panel(&mut f, 1280, &view, &l);
        assert_eq!((c.w, c.h), (1280, 32));
    }

    #[test]
    fn the_panel_shows_no_control_for_a_person() {
        let mut f = Font::for_test();
        let mut view = PanelView {
            app: None,
            person: Some(true),
            hover: None,
            bell: Bell::default(),
        };
        let l = panel_layout(&mut f, 1280, &view);
        let person = panel(&mut f, 1280, &view, &l);
        view.person = None;
        let nobody = panel(&mut f, 1280, &view, &l);
        assert!(person.px == nobody.px);
        view.person = Some(false);
        let agent = panel(&mut f, 1280, &view, &l);
        assert!(agent.px != nobody.px);
    }

    #[test]
    fn a_rounded_shape_cuts_the_corners_only() {
        let rects = rounded_shape(100, 60, 13.0);
        let full: u32 = rects.iter().map(|r| r.2 as u32 * r.3 as u32).sum();
        assert!(full < 6000 && full > 5800, "{full}");
        assert!(rects.iter().all(|r| r.0 >= 0 && r.0 as u16 + r.2 <= 100));
    }

    #[test]
    fn a_corner_shows_the_ring_and_hides_the_outside() {
        let (c, m) = corner(Corner::TopLeft, 0xffffff);
        assert!(!inside(&m, 0, 0));
        assert!(!inside(&m, 12, 12));
        assert!(inside(&m, 12, 0));
        assert!(c.px[12] & 0xff >= 0xf0);
        let (_, m) = corner(Corner::BottomRight, 0xffffff);
        assert!(!inside(&m, 12, 12));
        assert!(inside(&m, 0, 12));
    }

    #[test]
    fn a_popover_at_the_end_of_the_panel_aligns_right_edges() {
        let shape = Connected {
            pill: Rect::new(160.0, 0.0, 40.0, 24.0),
            sheet: Rect::new(0.0, 32.0, 200.0, 100.0),
            pill_r: 6.0,
            side: Side::BelowEnd,
        };
        let (c, m) = connected(200, 132, &shape);
        assert!(inside(&m, 180, 12));
        assert!(inside(&m, 180, 28));
        assert!(!inside(&m, 50, 12));
        assert!(inside(&m, 50, 80));
        assert!(!inside(&m, 0, 131));
        assert_eq!(c.px[60 * 200 + 50], theme().surface);
    }

    #[test]
    fn a_connected_popover_joins_pill_and_sheet() {
        let shape = Connected {
            pill: Rect::new(0.0, 0.0, 100.0, 24.0),
            sheet: Rect::new(0.0, 32.0, 200.0, 100.0),
            pill_r: 6.0,
            side: Side::Below,
        };
        let (c, m) = connected(200, 132, &shape);
        assert!(inside(&m, 50, 12));
        assert!(inside(&m, 50, 28));
        assert!(!inside(&m, 150, 12));
        assert!(inside(&m, 150, 80));
        assert!(!inside(&m, 199, 131));
        assert_eq!(c.px[0], theme().bg);
        assert_eq!(c.px[60 * 200 + 50], theme().surface);
        assert_eq!(c.px[60 * 200], theme().hairline_hi);
        let side = Connected {
            pill: Rect::new(0.0, 0.0, 48.0, 48.0),
            sheet: Rect::new(56.0, 0.0, 200.0, 120.0),
            pill_r: 12.0,
            side: Side::Right,
        };
        let (_, m) = connected(256, 120, &side);
        assert!(inside(&m, 24, 24));
        assert!(inside(&m, 52, 10));
        assert!(!inside(&m, 24, 100));
        assert!(inside(&m, 100, 100));
        let up = Connected {
            pill: Rect::new(0.0, 72.0, 48.0, 48.0),
            sheet: Rect::new(56.0, 0.0, 200.0, 120.0),
            pill_r: 12.0,
            side: Side::RightUp,
        };
        let (_, m) = connected(256, 120, &up);
        assert!(inside(&m, 24, 96));
        assert!(!inside(&m, 24, 20));
        assert!(inside(&m, 100, 20));
    }

    #[test]
    fn a_note_wraps_its_body() {
        let mut f = Font::for_test();
        let (c, m) = note(
            &mut f,
            None,
            "Chromium",
            "Download complete",
            "fares-2026-11-14.pdf, 182 KB, in Downloads",
        );
        assert_eq!(c.w, NOTE_W + 2);
        assert_eq!(c.h, 55);
        assert_eq!((m.w, m.h), (c.w, c.h));
    }

    #[test]
    fn a_title_bar_has_lights_and_a_ring() {
        let mut f = Font::for_test();
        let v = BarView {
            title: "Terminal",
            focused: true,
            ring: theme().hairline_hi,
            close_only: false,
            maximized: false,
            hover: false,
            pressed: None,
        };
        let c = title_bar(&mut f, 400, &v);
        assert_eq!((c.w, c.h), (402, 31));
        assert_eq!(c.px[200], theme().hairline_hi);
        assert_eq!(c.px[402 * 20 + 401], theme().hairline_hi);
        assert_eq!(c.px[402 * 3 + 300], theme().winbar);
        assert_eq!(c.px[402 * 15 + 17], draw::CLOSE.fill);
    }

    #[test]
    fn cursor_shape_follows_alpha() {
        let (c, m) = cursor(2, 1, &[0xff80_4020, 0x1000_0000]);
        assert_eq!(c.px[0], 0x804020);
        assert!(inside(&m, 0, 0));
        assert!(!inside(&m, 1, 0));
    }

    #[test]
    fn a_short_rail_sheet_keeps_the_pill_and_row_visible() {
        for side in [Side::Right, Side::RightUp] {
            let sheet_y = if side == Side::Right { 0.0 } else { 14.0 };
            let shape = Connected {
                pill: Rect::new(0.0, 0.0, 48.0, 48.0),
                sheet: Rect::new(56.0, sheet_y, 280.0, 34.0),
                pill_r: 12.0,
                side,
            };
            let (_, mask) = connected(336, 48, &shape);
            assert!(inside(&mask, 24, 24));
            assert!(inside(&mask, 200, sheet_y as usize + 17));
            assert!(!inside(
                &mask,
                200,
                if side == Side::Right { 47 } else { 0 }
            ));
        }
    }

    #[test]
    fn ripple_is_a_ring() {
        let (_, m) = ripple(false);
        let mid = RIPPLE as usize / 2;
        assert!(!inside(&m, mid, mid));
        assert!(inside(&m, mid + 15, mid));
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    #[test]
    fn panel_paint_keeps_the_original_pixels() {
        let mut font = Font::for_test();
        for hover in [
            None,
            Some(PanelHit::Apps),
            Some(PanelHit::App),
            Some(PanelHit::Notes),
        ] {
            let view = PanelView {
                app: Some(("Terminal", None)),
                person: Some(false),
                hover,
                bell: Bell::default(),
            };
            let layout = panel_layout(&mut font, 1280, &view);
            let actual = panel(&mut font, 1280, &view, &layout);
            let mut expected = Canvas::new(1280, 32, theme().bg);
            for (span, hit) in [
                (layout.apps, PanelHit::Apps),
                (layout.app.unwrap(), PanelHit::App),
                (layout.notes, PanelHit::Notes),
            ] {
                if hover == Some(hit) {
                    let b = Rect::new(span.0, 4.0, span.1, 24.0);
                    expected.rounded_rect(b, 6.0, theme().hover);
                    expected.ring_in(b, 6.0, theme().hairline);
                }
            }
            apps_label(&mut expected, &mut font, layout.apps.0, 4.0);
            app_label(
                &mut expected,
                &mut font,
                layout.app.unwrap().0,
                4.0,
                "Terminal",
                None,
            );
            bell(
                &mut expected,
                Rect::new(layout.notes.0, 4.0, layout.notes.1, 24.0),
                Bell::default(),
                if hover == Some(PanelHit::Notes) {
                    theme().hover
                } else {
                    theme().bg
                },
            );
            let x = layout.notes.0 - 4.0 - font.width(tr(Msg::Agent), TEXT_SIZE) - 16.0;
            expected.rounded_rect(Rect::new(x, 12.0, 8.0, 8.0), 4.0, theme().green);
            font.draw(
                &mut expected,
                tr(Msg::Agent),
                x + 16.0,
                16.0,
                TEXT_SIZE,
                theme().text,
            );
            assert_eq!(actual.px, expected.px);
        }
    }

    #[test]
    fn panel_nodes_keep_control_sizes_and_hit_targets() {
        let mut font = Font::for_test();
        let view = PanelView {
            app: Some(("Terminal", None)),
            person: Some(false),
            hover: None,
            bell: Bell::default(),
        };
        let layout = panel_layout(&mut font, 1280, &view);
        let mut tree = panel_tree(&mut font, 1280, &view, &layout);
        let bounds = Rect::new(0.0, 0.0, 1280.0, 32.0);
        assert_eq!(tree.size(), (1280.0, 32.0));
        assert_eq!(tree.hit_node(bounds, 5.0, 5.0), Some(APPLICATIONS_ID));
        assert_eq!(
            tree.hit_node(bounds, layout.app.unwrap().0 + 1.0, 5.0),
            Some(FRONT_APP_ID)
        );
        assert_eq!(tree.hit_node(bounds, 5.0, 29.0), None);
        assert_eq!(layout.notes, (1280.0 - 4.0 - BELL_W, BELL_W));
        assert_eq!(
            tree.hit_node(bounds, layout.notes.0 + 2.0, 10.0),
            Some(NOTIFICATIONS_ID)
        );
        tree.set_expanded(APPLICATIONS_ID, true);
        let json = tree.json(bounds, (0, 0));
        let apps = json
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == APPLICATIONS_ID)
            .unwrap();
        assert_eq!(apps["role"], "button");
        assert_eq!(apps["actions"], serde_json::json!(["click"]));
        assert_eq!(apps["bounds"]["width"], layout.apps.1);
        assert_eq!(apps["states"]["expanded"], true);
        assert!(json
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Agent"));
    }

    #[test]
    fn rail_nodes_keep_the_pitch_and_screen_coordinates() {
        let mut font = Font::for_test();
        let items: Vec<_> = (0..3)
            .map(|i| RailItem {
                name: "Terminal",
                icon: None,
                windows: i,
                front: i == 0,
                attention: i == 2,
            })
            .collect();
        let tree = rail_tree(&mut font, &items, &[101, 202, 303], None, 768, 32);
        assert_eq!(tree.size(), (64.0, 768.0));
        let view = Rect::new(0.0, 32.0, 64.0, 768.0);
        for (i, id) in [101, 202, 303].into_iter().enumerate() {
            assert_eq!(tree.hit_node(view, 30.0, item_y(i) as f32), Some(id));
        }
        assert_eq!(tree.hit_node(view, 30.0, 90.0), None);
        let json = tree.json(Rect::new(0.0, 0.0, 64.0, 768.0), (0, 32));
        let first = &json[0];
        assert_eq!(first["id"], 101);
        assert_eq!(
            first["bounds"],
            serde_json::json!({"x": 8.0, "y": 40.0, "width": 48.0, "height": 48.0})
        );
        assert_eq!(first["states"]["selected"], true);
    }

    #[test]
    fn rail_paint_keeps_the_original_pixels() {
        let mut font = Font::for_test();
        for front in [false, true] {
            for hover in [None, Some(0)] {
                let items = [RailItem {
                    name: "Terminal",
                    icon: None,
                    windows: 3,
                    front,
                    attention: true,
                }];
                let tree = rail_tree(&mut font, &items, &[101], hover, 200, 32);
                let mut actual = Canvas::new(64, 200, theme().bg);
                tree.paint(
                    &mut actual,
                    &mut crate::ui::widgets::Paint {
                        font: &mut font,
                        icons: &|_| None,
                        field: None,
                    },
                    Rect::new(0.0, 0.0, 64.0, 200.0),
                );
                let mut expected = Canvas::new(64, 200, theme().bg);
                let b = Rect::new(8.0, 8.0, 48.0, 48.0);
                if front {
                    expected.rounded_rect(b, 12.0, theme().rail_active);
                } else if hover.is_some() {
                    expected.rounded_rect(b, 12.0, theme().hover);
                }
                rail_icon(&mut expected, &mut font, &items[0], 8.0, 8.0);
                assert_eq!(actual.px, expected.px);
            }
        }
    }
}

#[cfg(test)]
mod title_tree_tests {
    use super::*;

    #[test]
    fn title_buttons_have_stable_ids_names_and_real_hit_targets() {
        let mut font = Font::for_test();
        let mut view = BarView {
            title: "Terminal",
            focused: true,
            ring: theme().hairline_hi,
            close_only: false,
            maximized: false,
            hover: false,
            pressed: None,
        };
        let tree = title_tree(&mut font, 42, 400, &view);
        assert_eq!(tree.size(), (402.0, 31.0));
        let bounds = Rect::new(-1.0, -1.0, 402.0, 31.0);
        for &(light, x) in lights(false) {
            let id = tree.hit_node(bounds, x, LIGHT_Y).unwrap();
            assert_eq!(id, title_node_id(42, Some(light)));
            assert_eq!(title_light(42, false, id), Some(light));
        }
        let json = tree.json(Rect::new(0.0, 0.0, 402.0, 31.0), (100, 200));
        assert_eq!(json[0]["name"], "Close Terminal");
        assert_eq!(json[0]["actions"], serde_json::json!(["click"]));
        assert_eq!(json[3]["role"], "title_bar");
        view.maximized = true;
        let restored = title_tree(&mut font, 42, 400, &view).json(bounds, (0, 0));
        assert_eq!(restored[2]["name"], "Restore Terminal");
        assert_eq!(restored[2]["id"], json[2]["id"]);
        view.close_only = true;
        let dialog = title_tree(&mut font, 42, 400, &view);
        assert_eq!(dialog.hit_node(bounds, 50.0, 15.0), None);
        assert_eq!(
            title_light(42, true, title_node_id(42, Some(Light::Maximize))),
            None
        );
    }
}
