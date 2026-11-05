//! Shapes rasterised by tiny-skia over only the pixels they touch, then
//! blended into a canvas, so a clip on the canvas holds for them too.

use tiny_skia::{FillRule, Mask, Path, PathBuilder, Transform};

use crate::draw::{Canvas, Raised, Rect, NONE};

/// The whole pixels around `b`, one more on each side for antialiasing.
#[derive(Clone, Copy)]
struct Area {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Area {
    fn around(b: Rect) -> Option<Self> {
        let x = (b.x - 1.0).floor() as i32;
        let y = (b.y - 1.0).floor() as i32;
        let w = ((b.x + b.w + 1.0).ceil() as i32 - x).max(0) as u32;
        let h = ((b.y + b.h + 1.0).ceil() as i32 - y).max(0) as u32;
        (w > 0 && h > 0).then_some(Self { x, y, w, h })
    }

    /// How much of each pixel of the area the rounded rectangle covers.
    fn cover(self, b: Rect, r: f32) -> Option<Mask> {
        let mut mask = Mask::new(self.w, self.h)?;
        let path = rounded_path(b, r)?;
        let shift = Transform::from_translate(-self.x as f32, -self.y as f32);
        mask.fill_path(&path, FillRule::Winding, true, shift);
        Some(mask)
    }

    fn blend(self, c: &mut Canvas, color: u32, alpha: impl Fn(usize) -> f32) {
        for j in 0..self.h as usize {
            for i in 0..self.w as usize {
                let (x, y) = (self.x + i as i32, self.y + j as i32);
                if x >= 0 && y >= 0 {
                    let a = alpha(j * self.w as usize + i).clamp(0.0, 1.0);
                    c.blend(x as usize, y as usize, color, a);
                }
            }
        }
    }
}

fn rounded_path(b: Rect, r: f32) -> Option<Path> {
    if b.w <= 0.0 || b.h <= 0.0 {
        return None;
    }
    let (l, t, rt, bt) = (b.x, b.y, b.x + b.w, b.y + b.h);
    let a = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
    let k = 0.552_284_8 * a;
    let mut p = PathBuilder::new();
    p.move_to(l + a, t);
    p.line_to(rt - a, t);
    p.cubic_to(rt - a + k, t, rt, t + a - k, rt, t + a);
    p.line_to(rt, bt - a);
    p.cubic_to(rt, bt - a + k, rt - a + k, bt, rt - a, bt);
    p.line_to(l + a, bt);
    p.cubic_to(l + a - k, bt, l, bt - a + k, l, bt - a);
    p.line_to(l, t + a);
    p.cubic_to(l, t + a - k, l + a - k, t, l + a, t);
    p.close();
    p.finish()
}

fn alpha(m: &Mask, i: usize) -> f32 {
    m.data()[i] as f32 / 255.0
}

pub fn rounded(c: &mut Canvas, b: Rect, r: f32, color: u32) {
    let Some(area) = Area::around(b) else {
        return;
    };
    if let Some(m) = area.cover(b, r) {
        area.blend(c, color, |i| alpha(&m, i));
    }
}

/// A 1 px ring inside the edge of a rounded rectangle.
pub fn ring(c: &mut Canvas, b: Rect, r: f32, color: u32) {
    let Some(area) = Area::around(b) else {
        return;
    };
    let (Some(outer), inner) = (
        area.cover(b, r),
        area.cover(b.inset(1.0), (r - 1.0).max(0.0)),
    ) else {
        return;
    };
    area.blend(c, color, |i| {
        alpha(&outer, i) - inner.as_ref().map_or(0.0, |m| alpha(m, i))
    });
}

/// A control of the raised family, as `Canvas::raised` draws it.
pub fn raised(c: &mut Canvas, b: Rect, r: f32, s: Raised) {
    let Some(area) = Area::around(Rect::new(b.x, b.y, b.w, b.h + 2.0)) else {
        return;
    };
    let Some(base) = area.cover(b, r) else {
        return;
    };
    let shifted = |dy: f32| area.cover(Rect::new(b.x, b.y + dy, b.w, b.h), r);
    if s.shadow != NONE {
        if let Some(down) = shifted(1.0) {
            area.blend(c, s.shadow, |i| alpha(&down, i) - alpha(&base, i));
        }
    }
    area.blend(c, s.fill, |i| alpha(&base, i));
    if s.hi != s.fill {
        if let Some(lower) = shifted(2.0) {
            area.blend(c, s.hi, |i| alpha(&base, i) * (1.0 - alpha(&lower, i)));
        }
    }
    ring(c, b, r, s.ring);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_stay_inside_the_clip() {
        let mut c = Canvas::new(40, 40, 0);
        c.clip = Some((0, 0, 40, 20));
        rounded(&mut c, Rect::new(5.0, 5.0, 30.0, 30.0), 6.0, 0xffffff);
        assert_eq!(c.px[10 * 40 + 20], 0xffffff);
        assert_eq!(c.px[30 * 40 + 20], 0);
    }

    #[test]
    fn a_ring_leaves_the_middle() {
        let mut c = Canvas::new(20, 20, 0);
        ring(&mut c, Rect::new(2.0, 2.0, 16.0, 16.0), 4.0, 0xffffff);
        assert_eq!(c.px[10 * 20 + 2], 0xffffff);
        assert_eq!(c.px[10 * 20 + 10], 0);
    }
}
