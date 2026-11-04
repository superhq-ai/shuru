//! A small software rasteriser: anti-aliased rounded rectangles, rings and
//! text into an 0RGB buffer that goes to a window as a ZPixmap.

use std::collections::HashMap;

pub const BG: u32 = 0x0c0c0e;
pub const SURFACE: u32 = 0x141416;
pub const STRIP_BG: u32 = 0x111113;
pub const RAISED: u32 = 0x232327;
pub const HOVER: u32 = 0x1b1b1e;
pub const HAIRLINE: u32 = 0x26262b;
pub const TEXT: u32 = 0xf2f2f3;
pub const DIM: u32 = 0x9d9da2;
pub const AMBER: u32 = 0xe8b04a;
pub const GREEN: u32 = 0x4cc777;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }

    fn inset(self, d: f32) -> Self {
        Rect::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

fn mix(under: u32, over: u32, a: f32) -> u32 {
    if a >= 1.0 {
        return over;
    }
    let ch = |s: u32| {
        let u = ((under >> s) & 0xff) as f32;
        let o = ((over >> s) & 0xff) as f32;
        ((u + (o - u) * a).round() as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}

/// Signed distance from a point to a rounded rectangle.
fn rounded_sd(px: f32, py: f32, b: Rect, r: f32) -> f32 {
    let (hw, hh) = (b.w / 2.0, b.h / 2.0);
    let qx = (px - (b.x + hw)).abs() - (hw - r);
    let qy = (py - (b.y + hh)).abs() - (hh - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

impl Canvas {
    pub fn new(w: usize, h: usize, bg: u32) -> Self {
        Canvas {
            w,
            h,
            px: vec![bg; w * h],
        }
    }

    pub fn blend(&mut self, x: usize, y: usize, color: u32, a: f32) {
        if a <= 0.0 || x >= self.w || y >= self.h {
            return;
        }
        let p = &mut self.px[y * self.w + x];
        *p = mix(*p, color, a);
    }

    pub fn rounded_rect(&mut self, b: Rect, r: f32, color: u32) {
        let r = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
        let x0 = b.x.floor().max(0.0) as usize;
        let y0 = b.y.floor().max(0.0) as usize;
        let x1 = ((b.x + b.w).ceil().max(0.0) as usize).min(self.w);
        let y1 = ((b.y + b.h).ceil().max(0.0) as usize).min(self.h);
        for j in y0..y1 {
            for i in x0..x1 {
                let d = rounded_sd(i as f32 + 0.5, j as f32 + 0.5, b, r);
                self.blend(i, j, color, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// A filled rounded rectangle with a one pixel border.
    pub fn bordered(&mut self, b: Rect, r: f32, fill: u32, border: u32) {
        self.rounded_rect(b, r, border);
        self.rounded_rect(b.inset(1.0), r - 1.0, fill);
    }

    /// The outline of a rounded rectangle, `width` wide, centred on its edge.
    pub fn stroke_rect(&mut self, b: Rect, r: f32, width: f32, color: u32) {
        let r = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
        let pad = width;
        let x0 = (b.x - pad).floor().max(0.0) as usize;
        let y0 = (b.y - pad).floor().max(0.0) as usize;
        let x1 = ((b.x + b.w + pad).ceil().max(0.0) as usize).min(self.w);
        let y1 = ((b.y + b.h + pad).ceil().max(0.0) as usize).min(self.h);
        for j in y0..y1 {
            for i in x0..x1 {
                let d = rounded_sd(i as f32 + 0.5, j as f32 + 0.5, b, r);
                self.blend(i, j, color, (width / 2.0 + 0.5 - d.abs()).clamp(0.0, 1.0));
            }
        }
    }

    /// A line from (x0, y0) to (x1, y1) with round ends.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: u32) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        let pad = width;
        let bx0 = (x0.min(x1) - pad).floor().max(0.0) as usize;
        let by0 = (y0.min(y1) - pad).floor().max(0.0) as usize;
        let bx1 = ((x0.max(x1) + pad).ceil().max(0.0) as usize).min(self.w);
        let by1 = ((y0.max(y1) + pad).ceil().max(0.0) as usize).min(self.h);
        for j in by0..by1 {
            for i in bx0..bx1 {
                let (px, py) = (i as f32 + 0.5, j as f32 + 0.5);
                let t = (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0);
                let d = ((px - x0 - t * dx).powi(2) + (py - y0 - t * dy).powi(2)).sqrt();
                self.blend(i, j, color, (width / 2.0 + 0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// The outline of an ellipse around (cx, cy).
    pub fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, width: f32, color: u32) {
        let x0 = (cx - rx - width).floor().max(0.0) as usize;
        let y0 = (cy - ry - width).floor().max(0.0) as usize;
        let x1 = ((cx + rx + width).ceil().max(0.0) as usize).min(self.w);
        let y1 = ((cy + ry + width).ceil().max(0.0) as usize).min(self.h);
        for j in y0..y1 {
            for i in x0..x1 {
                let (dx, dy) = ((i as f32 + 0.5 - cx) / rx, (j as f32 + 0.5 - cy) / ry);
                let d = ((dx * dx + dy * dy).sqrt() - 1.0) * rx.min(ry);
                self.blend(i, j, color, (width / 2.0 + 0.5 - d.abs()).clamp(0.0, 1.0));
            }
        }
    }

    /// Coverage of a ring of `width` centred on radius `r` around (cx, cy).
    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, color: u32) {
        for j in 0..self.h {
            for i in 0..self.w {
                let d = ((i as f32 + 0.5 - cx).powi(2) + (j as f32 + 0.5 - cy).powi(2)).sqrt();
                let a = (width / 2.0 + 0.5 - (d - r).abs()).clamp(0.0, 1.0);
                self.blend(i, j, color, a);
            }
        }
    }

    /// The pixels as a ZPixmap of 32 bits a pixel, little endian.
    pub fn bytes(&self) -> Vec<u8> {
        self.px.iter().flat_map(|p| p.to_le_bytes()).collect()
    }
}

/// Runs of set pixels a row at a time, as rectangles for SHAPE.
pub fn mask_rects(w: usize, h: usize, set: impl Fn(usize, usize) -> bool) -> Vec<(i16, i16, u16)> {
    let mut out = Vec::new();
    for y in 0..h {
        let mut x = 0;
        while x < w {
            if !set(x, y) {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && set(x, y) {
                x += 1;
            }
            out.push((start as i16, y as i16, (x - start) as u16));
        }
    }
    out
}

/// Inter Medium, cut to Latin; see fonts/LICENSE-Inter.txt.
const FONT: &[u8] = include_bytes!("../fonts/Inter-Medium.ttf");

pub struct Font {
    font: fontdue::Font,
    glyphs: HashMap<(char, u32), (fontdue::Metrics, Vec<u8>)>,
}

impl Font {
    pub fn new() -> Self {
        let font = fontdue::Font::from_bytes(FONT, fontdue::FontSettings::default())
            .expect("the embedded font parses");
        Font {
            font,
            glyphs: HashMap::new(),
        }
    }

    fn glyph(&mut self, c: char, size: f32) -> &(fontdue::Metrics, Vec<u8>) {
        let key = (c, (size * 10.0) as u32);
        let font = &self.font;
        self.glyphs
            .entry(key)
            .or_insert_with(|| font.rasterize(c, size))
    }

    pub fn width(&mut self, text: &str, size: f32) -> f32 {
        text.chars()
            .map(|c| self.glyph(c, size).0.advance_width)
            .sum()
    }

    /// `text` cut with an ellipsis to fit `max` pixels.
    pub fn fit(&mut self, text: &str, size: f32, max: f32) -> String {
        if self.width(text, size) <= max {
            return text.to_string();
        }
        let ell = self.width("\u{2026}", size);
        let mut out = String::new();
        let mut w = 0.0;
        for c in text.chars() {
            let cw = self.glyph(c, size).0.advance_width;
            if w + cw + ell > max {
                break;
            }
            w += cw;
            out.push(c);
        }
        out.push('\u{2026}');
        out
    }

    /// Draws `text` with its left end at `x`, vertically centred on `mid`.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: f32,
        mid: f32,
        size: f32,
        color: u32,
    ) -> f32 {
        let cap = self
            .font
            .horizontal_line_metrics(size)
            .map_or(size * 0.7, |m| m.ascent * 0.73);
        let baseline = (mid + cap / 2.0).round();
        let mut pen = x;
        for c in text.chars() {
            let (m, bitmap) = self.glyph(c, size).clone();
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.height as f32 - m.ymin as f32) as i32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = bitmap[row * m.width + col] as f32 / 255.0;
                    let (px, py) = (gx + col as i32, gy + row as i32);
                    if px >= 0 && py >= 0 {
                        canvas.blend(px as usize, py as usize, color, a);
                    }
                }
            }
            pen += m.advance_width;
        }
        pen - x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_rect_covers_the_middle_not_the_corner() {
        let mut c = Canvas::new(20, 20, 0);
        c.rounded_rect(Rect::new(0.0, 0.0, 20.0, 20.0), 8.0, 0xffffff);
        assert_eq!(c.px[10 * 20 + 10], 0xffffff);
        assert_eq!(c.px[0], 0);
    }

    #[test]
    fn mask_rects_are_row_runs() {
        let r = mask_rects(5, 2, |x, y| y == 1 && (x == 1 || x == 2 || x == 4));
        assert_eq!(r, vec![(1, 1, 2), (4, 1, 1)]);
    }

    #[test]
    fn text_is_measured_and_cut() {
        let mut f = Font::new();
        let w = f.width("Terminal", 13.0);
        assert!(w > 30.0 && w < 80.0, "{w}");
        let cut = f.fit("a long window title that does not fit", 13.0, 60.0);
        assert!(cut.ends_with('\u{2026}'));
        assert!(f.width(&cut, 13.0) <= 60.0);
        let mut c = Canvas::new(80, 20, 0);
        f.draw(&mut c, "Hi", 2.0, 10.0, 13.0, 0xffffff);
        assert!(c.px.iter().any(|&p| p != 0));
    }
}
