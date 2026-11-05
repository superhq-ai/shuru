//! Software paint boundary, composited into the X11 0RGB buffer.

use crate::draw::{Canvas, Rect};
use tiny_skia::{Paint, PathBuilder, Pixmap, Transform};

pub fn rounded(c: &mut Canvas, b: Rect, radius: f32, color: u32) {
    let x = b.x.floor().max(0.0) as usize;
    let y = b.y.floor().max(0.0) as usize;
    let w = ((b.x + b.w).ceil() as usize).min(c.w).saturating_sub(x);
    let h = ((b.y + b.h).ceil() as usize).min(c.h).saturating_sub(y);
    let Some(mut pixmap) = Pixmap::new(w as u32, h as u32) else {
        return;
    };
    let (l, t) = (b.x - x as f32, b.y - y as f32);
    let (r, d) = (l + b.w, t + b.h);
    let a = radius.min(b.w / 2.0).min(b.h / 2.0);
    let k = 0.5522848 * a;
    let mut path = PathBuilder::new();
    path.move_to(l + a, t);
    path.line_to(r - a, t);
    path.cubic_to(r - a + k, t, r, t + a - k, r, t + a);
    path.line_to(r, d - a);
    path.cubic_to(r, d - a + k, r - a + k, d, r - a, d);
    path.line_to(l + a, d);
    path.cubic_to(l + a - k, d, l, d - a + k, l, d - a);
    path.line_to(l, t + a);
    path.cubic_to(l, t + a - k, l + a - k, t, l + a, t);
    path.close();
    let mut paint = Paint::default();
    paint.set_color_rgba8((color >> 16) as u8, (color >> 8) as u8, color as u8, 255);
    pixmap.fill_path(
        &path.finish().unwrap(),
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        None,
    );
    for (i, pixel) in pixmap.pixels().iter().enumerate() {
        c.blend(x + i % w, y + i / w, color, pixel.alpha() as f32 / 255.0);
    }
}

pub fn ring(c: &mut Canvas, b: Rect, r: f32, color: u32) {
    let mut outer = Canvas::new(c.w, c.h, 0);
    let mut inner = Canvas::new(c.w, c.h, 0);
    rounded(&mut outer, b, r, 0xffffff);
    rounded(&mut inner, b.inset(1.0), (r - 1.0).max(0.0), 0xffffff);
    composite(c, b, color, |i| {
        ((outer.px[i] & 255) as f32 - (inner.px[i] & 255) as f32) / 255.0
    });
}

fn composite(c: &mut Canvas, b: Rect, color: u32, alpha: impl Fn(usize) -> f32) {
    for y in b.y.max(0.0) as usize..((b.y + b.h + 1.0).ceil() as usize).min(c.h) {
        for x in b.x.max(0.0) as usize..((b.x + b.w + 1.0).ceil() as usize).min(c.w) {
            c.blend(x, y, color, alpha(y * c.w + x).max(0.0));
        }
    }
}

pub fn raised(c: &mut Canvas, b: Rect, r: f32, s: crate::draw::Raised) {
    let mut base = Canvas::new(c.w, c.h, 0);
    rounded(&mut base, b, r, 0xffffff);
    let mut offset = Canvas::new(c.w, c.h, 0);
    if s.shadow != crate::draw::NONE {
        rounded(
            &mut offset,
            Rect::new(b.x, b.y + 1.0, b.w, b.h),
            r,
            0xffffff,
        );
        composite(c, Rect::new(b.x, b.y, b.w, b.h + 1.0), s.shadow, |i| {
            ((offset.px[i] & 255) as f32 - (base.px[i] & 255) as f32) / 255.0
        });
    }
    rounded(c, b, r, s.fill);
    if s.hi != s.fill {
        offset.px.fill(0);
        rounded(
            &mut offset,
            Rect::new(b.x, b.y + 2.0, b.w, b.h),
            r,
            0xffffff,
        );
        composite(c, b, s.hi, |i| {
            (base.px[i] & 255) as f32 / 255.0 * (1.0 - (offset.px[i] & 255) as f32 / 255.0)
        });
    }
    ring(c, b, r, s.ring);
}
