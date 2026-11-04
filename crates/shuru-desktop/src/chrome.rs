//! What the desktop draws itself: the strip above a window, the panel,
//! the badge, the click ripple and the cursor. Pure pixels, no X.

use crate::draw::{
    Canvas, Font, Rect, AMBER, BG, DIM, GREEN, HAIRLINE, HOVER, RAISED, STRIP_BG, SURFACE, TEXT,
};

pub const MARGIN: i16 = 12;
pub const STRIP: i16 = 28;
pub const PANEL_H: u16 = 44;
/// Space between the bottom of the panel and the bottom of the window.
pub const PANEL_LIFT: i16 = 2;
pub const BORDER: u16 = 2;
pub const RIPPLE: u16 = 44;
pub const BADGE_H: usize = 22;
/// Space the badge keeps from the right end of the strip.
pub const BADGE_INSET: i16 = 5;

const TEXT_SIZE: f32 = 13.0;
const SMALL_SIZE: f32 = 12.0;

/// Where top level windows go, border included: the screen less the
/// margins and the strip. The panel floats over the bottom of it.
pub fn work_area(sw: u16, sh: u16) -> (i16, i16, u16, u16) {
    let top = MARGIN + STRIP;
    (
        MARGIN,
        top,
        sw.saturating_sub(2 * MARGIN as u16),
        sh.saturating_sub((top + MARGIN) as u16),
    )
}

/// The strip: the top edge of the frame round the window, with the app
/// name in it. `reserve` is kept free at the right for the badge.
pub fn strip(
    font: &mut Font,
    w: usize,
    app: Option<&str>,
    title: Option<&str>,
    reserve: f32,
) -> Canvas {
    let h = STRIP as usize;
    let mut c = Canvas::new(w, h, STRIP_BG);
    for x in 0..w {
        c.px[x] = HAIRLINE;
    }
    for y in 0..h {
        c.px[y * w] = HAIRLINE;
        c.px[y * w + w - 1] = HAIRLINE;
    }
    let mid = h as f32 / 2.0 + 0.5;
    let Some(app) = app else {
        return c;
    };
    let left = 10.0;
    let room = w as f32 - reserve - left;
    let app = font.fit(app, TEXT_SIZE, room);
    let used = font.draw(&mut c, &app, left, mid, TEXT_SIZE, TEXT);
    if let Some(t) = title.filter(|t| !t.is_empty() && *t != app) {
        let x = left + used + 10.0;
        if room + left - x > 24.0 {
            let t = font.fit(t, TEXT_SIZE, room + left - x);
            font.draw(&mut c, &t, x, mid, TEXT_SIZE, DIM);
        }
    }
    c
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Globe,
    Terminal,
    Folder,
    Page,
}

pub struct PanelItem {
    pub icon: Icon,
    pub running: bool,
    pub focused: bool,
}

const PANEL_PAD: f32 = 4.0;
const ITEM_W: f32 = 50.0;

/// The width of the panel and the span of each item in it.
pub fn panel_layout(items: &[PanelItem]) -> (usize, Vec<(f32, f32)>) {
    let spans: Vec<_> = (0..items.len())
        .map(|i| (PANEL_PAD + i as f32 * (ITEM_W + 4.0), ITEM_W))
        .collect();
    let w = spans
        .last()
        .map_or(PANEL_H as f32, |&(x, w)| x + w + PANEL_PAD);
    (w.ceil() as usize, spans)
}

fn icon(c: &mut Canvas, icon: Icon, cx: f32, cy: f32, color: u32) {
    let s = 1.6;
    match icon {
        Icon::Globe => {
            c.ellipse(cx, cy, 9.0, 9.0, s, color);
            c.ellipse(cx, cy, 4.0, 9.0, s, color);
            c.line(cx - 9.0, cy, cx + 9.0, cy, s, color);
        }
        Icon::Terminal => {
            c.line(cx - 9.0, cy - 5.0, cx - 4.0, cy, s, color);
            c.line(cx - 4.0, cy, cx - 9.0, cy + 5.0, s, color);
            c.line(cx, cy + 6.0, cx + 9.0, cy + 6.0, s, color);
        }
        Icon::Folder => {
            c.stroke_rect(Rect::new(cx - 9.5, cy - 4.5, 19.0, 13.0), 2.0, s, color);
            c.line(cx - 9.5, cy - 4.0, cx - 9.5, cy - 6.5, s, color);
            c.line(cx - 9.5, cy - 7.5, cx - 3.5, cy - 7.5, s, color);
            c.line(cx - 3.5, cy - 7.5, cx - 1.5, cy - 4.5, s, color);
        }
        Icon::Page => {
            c.stroke_rect(Rect::new(cx - 7.0, cy - 9.0, 14.0, 18.0), 2.0, s, color);
            for dy in [-4.0, 0.0, 4.0] {
                c.line(cx - 3.5, cy + dy, cx + 3.5, cy + dy, s, color);
            }
        }
    }
}

/// The panel and its shape.
pub fn panel(
    items: &[PanelItem],
    w: usize,
    spans: &[(f32, f32)],
    hover: Option<usize>,
) -> (Canvas, Canvas) {
    let h = PANEL_H as usize;
    let whole = Rect::new(0.0, 0.0, w as f32, h as f32);
    let mut c = Canvas::new(w, h, BG);
    c.bordered(whole, 12.0, SURFACE, HAIRLINE);
    let ih = h as f32 - 2.0 * PANEL_PAD;
    for (i, (item, &(x, iw))) in items.iter().zip(spans).enumerate() {
        let fill = if item.focused {
            Some(RAISED)
        } else if hover == Some(i) {
            Some(HOVER)
        } else {
            None
        };
        if let Some(f) = fill {
            c.rounded_rect(Rect::new(x, PANEL_PAD, iw, ih), 9.0, f);
        }
        let color = if item.running || hover == Some(i) {
            TEXT
        } else {
            DIM
        };
        icon(&mut c, item.icon, x + iw / 2.0, h as f32 / 2.0 - 1.0, color);
        if item.running {
            c.rounded_rect(
                Rect::new(x + iw / 2.0 - 1.5, h as f32 - 8.5, 3.0, 3.0),
                1.5,
                color,
            );
        }
    }
    let mut mask = Canvas::new(w, h, 0);
    mask.rounded_rect(whole, 12.0, 0xffffff);
    (c, mask)
}

/// The item under `x` in the panel.
pub fn panel_hit(spans: &[(f32, f32)], x: f32) -> Option<usize> {
    spans.iter().position(|&(sx, sw)| x >= sx && x < sx + sw)
}

/// The who badge: a pill with a dot, and for a person a Done button that
/// hands control back. The pixels, the shape, and the span of Done.
pub fn badge(font: &mut Font, person: bool) -> (Canvas, Canvas, Option<(f32, f32)>) {
    let label = if person { "You" } else { "Agent" };
    let pill = (10.0 + 7.0 + 7.0 + font.width(label, SMALL_SIZE) + 10.0).ceil();
    let done = (12.0 + font.width("Done", SMALL_SIZE) + 12.0).ceil();
    let w = if person { pill + 4.0 + done } else { pill } as usize;
    let h = BADGE_H;
    let r = h as f32 / 2.0;
    let mut c = Canvas::new(w, h, STRIP_BG);
    let mut mask = Canvas::new(w, h, 0);
    let first = Rect::new(0.0, 0.0, pill, h as f32);
    c.bordered(first, r, SURFACE, HAIRLINE);
    mask.rounded_rect(first, r, 0xffffff);
    let dot = if person { AMBER } else { GREEN };
    c.rounded_rect(Rect::new(10.0, r - 3.5, 7.0, 7.0), 3.5, dot);
    font.draw(&mut c, label, 24.0, r, SMALL_SIZE, TEXT);
    let mut span = None;
    if person {
        let x = pill + 4.0;
        let b = Rect::new(x, 0.0, done, h as f32);
        c.bordered(b, r, SURFACE, HAIRLINE);
        mask.rounded_rect(b, r, 0xffffff);
        font.draw(&mut c, "Done", x + 12.0, r, SMALL_SIZE, TEXT);
        span = Some((x, done));
    }
    (c, mask, span)
}

/// A ring around a click, and its shape.
pub fn ripple(person: bool) -> (Canvas, Canvas) {
    let s = RIPPLE as usize;
    let mid = s as f32 / 2.0;
    let mut c = Canvas::new(s, s, BG);
    c.ring(mid, mid, 15.0, 7.0, BG);
    c.ring(mid, mid, 15.0, 3.5, if person { AMBER } else { TEXT });
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
    fn work_area_leaves_room_for_the_strip() {
        assert_eq!(work_area(1280, 800), (12, 40, 1256, 748));
    }

    #[test]
    fn panel_hits_the_item_under_the_pointer() {
        let items = vec![
            PanelItem {
                icon: Icon::Terminal,
                running: true,
                focused: true,
            },
            PanelItem {
                icon: Icon::Globe,
                running: false,
                focused: false,
            },
        ];
        let (w, spans) = panel_layout(&items);
        assert_eq!(w, 112);
        assert_eq!(panel_hit(&spans, spans[0].0 + 1.0), Some(0));
        assert_eq!(panel_hit(&spans, spans[1].0 + 1.0), Some(1));
        assert_eq!(panel_hit(&spans, 0.0), None);
        let (c, m) = panel(&items, w, &spans, Some(1));
        assert_eq!(c.w, w);
        assert!(!inside(&m, 0, 0));
        assert!(inside(&m, w / 2, 20));
    }

    #[test]
    fn a_person_gets_a_done_button() {
        let mut f = Font::new();
        let (a, _, none) = badge(&mut f, false);
        let (p, _, done) = badge(&mut f, true);
        assert!(none.is_none());
        let (x, w) = done.unwrap();
        assert!(x > 0.0 && x + w <= p.w as f32);
        assert!(p.w > a.w);
    }

    #[test]
    fn cursor_shape_follows_alpha() {
        let (c, m) = cursor(2, 1, &[0xff80_4020, 0x1000_0000]);
        assert_eq!(c.px[0], 0x804020);
        assert!(inside(&m, 0, 0));
        assert!(!inside(&m, 1, 0));
    }

    #[test]
    fn ripple_is_a_ring() {
        let (_, m) = ripple(false);
        let mid = RIPPLE as usize / 2;
        assert!(!inside(&m, mid, mid));
        assert!(inside(&m, mid + 15, mid));
    }
}
