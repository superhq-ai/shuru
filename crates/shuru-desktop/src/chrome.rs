//! What the desktop draws itself: the panel, the rail, title bars, window
//! corners, menus, the click ripple and the cursor. Pure pixels, no X.

use std::f32::consts::{FRAC_PI_2, PI};

use crate::draw::{self, theme, Canvas, Font, Image, Raised, Rect};
use crate::glyphs;
use crate::locale::{tr, Msg};

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

fn mid_of(y: f32, h: f32) -> f32 {
    y + h / 2.0
}

fn dot(c: &mut Canvas, x: f32, cy: f32, d: f32, color: u32) {
    c.rounded_rect(Rect::new(x, cy - d / 2.0, d, d), d / 2.0, color);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHit {
    Apps,
    App,
}

pub struct PanelView<'a> {
    pub app: Option<(&'a str, Option<&'a Image>)>,
    /// None, the agent (false) or a person (true).
    pub person: Option<bool>,
    pub hover: Option<PanelHit>,
}

/// The spans of the panel controls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelLayout {
    pub apps: (f32, f32),
    pub app: Option<(f32, f32)>,
}

impl PanelLayout {
    pub fn hit(&self, x: f32, y: f32) -> Option<PanelHit> {
        if !(CONTROL_Y..CONTROL_Y + CONTROL_H).contains(&y) {
            return None;
        }
        let inside = |s: (f32, f32)| x >= s.0 && x < s.0 + s.1;
        if inside(self.apps) {
            Some(PanelHit::Apps)
        } else if self.app.is_some_and(inside) {
            Some(PanelHit::App)
        } else {
            None
        }
    }
}

const MARK_X: f32 = 8.0;
const LABEL_X: f32 = 35.0;

pub fn panel_layout(font: &mut Font, view: &PanelView) -> PanelLayout {
    let apps_w = (LABEL_X + font.width(tr(Msg::Applications), TEXT_SIZE) + 8.0).round();
    let apps = (4.0, apps_w);
    let app = view.app.map(|(name, _)| {
        let x = apps.0 + apps.1 + 4.0;
        (x, (32.0 + font.width(name, TEXT_SIZE) + 8.0).round())
    });
    PanelLayout { apps, app }
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
pub fn panel(font: &mut Font, w: usize, view: &PanelView, layout: &PanelLayout) -> Canvas {
    let mut c = Canvas::new(w, PANEL_H as usize, theme().bg);
    let pill = |c: &mut Canvas, s: (f32, f32), hit: PanelHit| {
        if view.hover == Some(hit) {
            let r = Rect::new(s.0, CONTROL_Y, s.1, CONTROL_H);
            c.rounded_rect(r, 6.0, theme().hover);
            c.ring_in(r, 6.0, theme().hairline);
        }
    };
    pill(&mut c, layout.apps, PanelHit::Apps);
    apps_label(&mut c, font, layout.apps.0, CONTROL_Y);
    if let (Some(s), Some((name, icon))) = (layout.app, view.app) {
        pill(&mut c, s, PanelHit::App);
        app_label(&mut c, font, s.0, CONTROL_Y, name, icon);
    }
    // Who has control shows only for the agent: a person in control sees
    // it on the page, and the screen shows it as amber frames.
    if view.person == Some(false) {
        let mid = mid_of(CONTROL_Y, CONTROL_H);
        let label = tr(Msg::Agent);
        let x = w as f32 - 4.0 - font.width(label, TEXT_SIZE) - 16.0;
        dot(&mut c, x, mid, 8.0, theme().green);
        font.draw(&mut c, label, x + 16.0, mid, TEXT_SIZE, theme().text);
    }
    c
}

pub struct RailItem<'a> {
    pub name: &'a str,
    pub icon: Option<&'a Image>,
    pub windows: usize,
    pub front: bool,
    /// One of its windows asks for attention.
    pub attention: bool,
}

/// The rail: 64 px wide from the panel down, item `i` at screen y
/// `item_y(i)`. `top` is the screen y of the rail window.
pub fn rail(
    font: &mut Font,
    items: &[RailItem],
    hover: Option<usize>,
    h: usize,
    top: i16,
) -> Canvas {
    let mut c = Canvas::new(RAIL_W as usize, h, theme().bg);
    for (i, item) in items.iter().enumerate() {
        let y = (item_y(i) - top) as f32;
        let b = Rect::new(ITEM_X as f32, y, ITEM as f32, ITEM as f32);
        if item.front {
            c.rounded_rect(b, 12.0, theme().rail_active);
        } else if hover == Some(i) {
            c.rounded_rect(b, 12.0, theme().hover);
        }
        rail_icon(&mut c, font, item, ITEM_X as f32, y);
    }
    c
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

/// The rail item under a point of the rail window.
pub fn rail_hit(count: usize, x: f32, screen_y: f32) -> Option<usize> {
    if !(ITEM_X as f32..(ITEM_X + ITEM as i16) as f32).contains(&x) {
        return None;
    }
    (0..count).find(|&i| {
        let y = item_y(i) as f32;
        screen_y >= y && screen_y < y + ITEM as f32
    })
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

/// The light at (x, y) of a card.
pub fn light_hit(close_only: bool, x: f32, y: f32) -> Option<Light> {
    if !(LIGHT_Y - 3.0..LIGHT_Y + LIGHT_D + 3.0).contains(&y) {
        return None;
    }
    lights(close_only)
        .iter()
        .find(|(_, lx)| x >= lx - 3.0 && x < lx + LIGHT_D + 3.0)
        .map(|(l, _)| *l)
}

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

/// Our title bar for a card `w` wide, with the ring round it and the top
/// corners: the top `BAR_H + 1` rows of a frame `w + 2` wide.
pub fn title_bar(font: &mut Font, w: usize, v: &BarView) -> Canvas {
    let fw = w + 2;
    let mut c = Canvas::new(fw, BAR_H as usize + 1, theme().bg);
    let card = Rect::new(1.0, 1.0, w as f32, 4.0 * RADIUS);
    c.rounded_rect(card.inset(-1.0), RADIUS + 1.0, v.ring);
    c.rounded_rect(card, RADIUS, theme().winbar);
    for x in [0, fw - 1] {
        for y in RADIUS as usize..c.h {
            c.px[y * fw + x] = v.ring;
        }
    }
    for &(light, lx) in lights(v.close_only) {
        let b = Rect::new(1.0 + lx, 1.0 + LIGHT_Y, LIGHT_D, LIGHT_D);
        let lamp = match light {
            Light::Close => draw::CLOSE,
            Light::Minimize => draw::MINIMIZE,
            Light::Maximize => draw::MAXIMIZE,
        };
        let lit = v.focused || v.hover;
        let style = if v.pressed == Some(light) {
            Raised::pressed(lamp.ring, lamp.ring)
        } else if lit {
            lamp.raised(theme().light_drop)
        } else {
            theme().light_idle
        };
        c.raised(b, LIGHT_D / 2.0, style);
        if v.hover {
            let g = match light {
                Light::Close => &glyphs::LIGHT_CLOSE,
                Light::Minimize => &glyphs::LIGHT_MIN,
                Light::Maximize if v.maximized => &glyphs::LIGHT_RESTORE,
                Light::Maximize => &glyphs::LIGHT_MAX,
            };
            c.glyph(g, b.x as i32, b.y as i32, lamp.glyph, 1.0);
        }
    }
    let max = w as f32 - 148.0;
    if max > 8.0 && !v.title.is_empty() {
        let t = font.fit(v.title, SMALL_SIZE, max);
        let tw = font.width(&t, SMALL_SIZE);
        let color = if v.focused { theme().text } else { theme().dim };
        font.draw(
            &mut c,
            &t,
            (1.0 + (w as f32 - tw) / 2.0).round(),
            1.0 + 15.0,
            SMALL_SIZE,
            color,
        );
    }
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
            Side::Right => (y, x),
            Side::RightUp => (-y, x),
        };
        let back = |u: f32, v: f32| match self.side {
            Side::Below => (u, v),
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
    if shape.side == Side::Below {
        let p = shape.pill;
        let y = p.y as usize + 1;
        for x in (p.x + shape.pill_r + 1.0) as usize..(p.x + p.w - shape.pill_r) as usize {
            c.px[y * w + x] = theme().pill_hi;
        }
    }
    (c, m)
}

#[cfg(test)]
use crate::ui::widgets::sheet_height;
pub use crate::ui::widgets::{Row, RowState};

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
    fn rail_items_are_hit_by_their_box() {
        assert_eq!(rail_hit(3, 30.0, 41.0), Some(0));
        assert_eq!(rail_hit(3, 30.0, 90.0), None);
        assert_eq!(rail_hit(3, 30.0, 92.0), Some(1));
        assert_eq!(rail_hit(3, 2.0, 50.0), None);
        assert_eq!(rail_hit(1, 30.0, 100.0), None);
    }

    #[test]
    fn the_panel_lays_out_its_controls() {
        let mut f = Font::new();
        let view = PanelView {
            app: Some(("Chromium", None)),
            person: Some(true),
            hover: None,
        };
        let l = panel_layout(&mut f, &view);
        assert_eq!(l.apps.0, 4.0);
        let (ax, aw) = l.app.unwrap();
        assert_eq!(ax, l.apps.0 + l.apps.1 + 4.0);
        assert_eq!(l.hit(10.0, 10.0), Some(PanelHit::Apps));
        assert_eq!(l.hit(ax + 2.0, 10.0), Some(PanelHit::App));
        assert_eq!(l.hit(ax + aw + 40.0, 10.0), None);
        assert_eq!(l.hit(ax + 2.0, 30.0), None);
        let c = panel(&mut f, 1280, &view, &l);
        assert_eq!((c.w, c.h), (1280, 32));
    }

    #[test]
    fn the_panel_shows_no_control_for_a_person() {
        let mut f = Font::new();
        let mut view = PanelView {
            app: None,
            person: Some(true),
            hover: None,
        };
        let l = panel_layout(&mut f, &view);
        let person = panel(&mut f, 1280, &view, &l);
        view.person = None;
        let nobody = panel(&mut f, 1280, &view, &l);
        assert!(person.px == nobody.px);
        view.person = Some(false);
        let agent = panel(&mut f, 1280, &view, &l);
        assert!(agent.px != nobody.px);
    }

    #[test]
    fn lights_are_hit_by_position() {
        assert_eq!(light_hit(false, 12.0, 15.0), Some(Light::Close));
        assert_eq!(light_hit(false, 30.0, 15.0), Some(Light::Minimize));
        assert_eq!(light_hit(false, 50.0, 15.0), Some(Light::Maximize));
        assert_eq!(light_hit(true, 50.0, 15.0), None);
        assert_eq!(light_hit(false, 200.0, 15.0), None);
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
    fn sheets_fit_their_rows() {
        let mut f = Font::new();
        let rows = [
            Row::Search {
                query: "",
                placeholder: "Search",
            },
            Row::Section("Applications"),
            Row::Item {
                name: "Terminal",
                icon: None,
                icon_name: "Terminal",
                detail: Some("Shell"),
                current: false,
                minimized: false,
                state: RowState::Selected,
            },
            Row::Sep,
            Row::Action {
                name: "Quit",
                hint: None,
                state: RowState::Hover,
            },
            Row::Footer,
        ];
        let h = sheet_height(&rows);
        assert_eq!(h, 10.0 + 30.0 + 22.0 + 26.0 + 9.0 + 24.0 + 29.0);
        let mut c = Canvas::new(320, h as usize, theme().surface);
        let tree = crate::ui::Tree::new(
            rows.iter()
                .enumerate()
                .map(|(i, r)| {
                    (
                        i as u64,
                        accesskit::Role::MenuItem,
                        String::new(),
                        r.height(),
                    )
                })
                .collect(),
            320.0,
        );
        let sheet = Rect::new(0.0, 0.0, 320.0, h);
        crate::ui::widgets::sheet(&mut c, &mut f, sheet, &rows, &tree.bounds(sheet));
        assert!(c.px.contains(&theme().raised));
    }

    #[test]
    fn a_note_wraps_its_body() {
        let mut f = Font::new();
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
        let mut f = Font::new();
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
