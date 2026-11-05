//! A small software rasteriser: anti-aliased rounded rectangles, rings,
//! filled outlines, masks, images and text into an 0RGB buffer that goes to
//! a window as a ZPixmap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use cosmic_text::{
    fontdb, Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache,
    SwashContent, Weight, Wrap,
};

use crate::glyphs::Glyph;

/// A control of the raised family: a fill, a 1 px ring a step off it, a
/// 1 px highlight inside the top and a 1 px shadow line under it.
#[derive(Clone, Copy)]
pub struct Raised {
    pub fill: u32,
    pub ring: u32,
    pub hi: u32,
    pub shadow: u32,
}

/// The colors of the shell in one appearance.
pub struct Theme {
    /// The desktop, the panel and the rail.
    pub bg: u32,
    /// Menus and notifications.
    pub surface: u32,
    /// Our title bars.
    pub winbar: u32,
    pub hover: u32,
    /// Tooltips and app tiles.
    pub raised: u32,
    /// The keyboard row of a menu.
    pub selected: u32,
    /// The app in front on the rail.
    pub rail_active: u32,
    pub hairline: u32,
    /// The focused window, menus, tooltips.
    pub hairline_hi: u32,
    /// The snap preview.
    pub snap: u32,
    pub text: u32,
    pub dim: u32,
    pub amber: u32,
    pub green: u32,
    /// The top highlight of a connected popover pill.
    pub pill_hi: u32,
    pub keycap: Raised,
    /// The line under each window light.
    pub light_drop: u32,
    /// All three lights of a window that is not focused.
    pub light_idle: Raised,
}

/// A window light of a focused window, the same in both appearances.
#[derive(Clone, Copy)]
pub struct Lamp {
    pub fill: u32,
    pub ring: u32,
    pub hi: u32,
    /// The glyph on hover.
    pub glyph: u32,
}

pub const CLOSE: Lamp = Lamp {
    fill: 0xe2574f,
    ring: 0xc4463f,
    hi: 0xec7a73,
    glyph: 0x4d0f0b,
};
pub const MINIMIZE: Lamp = Lamp {
    fill: 0xe8c547,
    ring: 0xc9a737,
    hi: 0xf1d677,
    glyph: 0x5a4205,
};
pub const MAXIMIZE: Lamp = Lamp {
    fill: 0x4cc777,
    ring: 0x3aa863,
    hi: 0x74d696,
    glyph: 0x0d4223,
};

impl Lamp {
    pub fn raised(self, drop: u32) -> Raised {
        Raised {
            fill: self.fill,
            ring: self.ring,
            hi: self.hi,
            shadow: drop,
        }
    }
}

pub static DARK: Theme = Theme {
    bg: 0x0c0c0e,
    surface: 0x141416,
    winbar: 0x1c1c1f,
    hover: 0x1b1b1e,
    raised: 0x232327,
    selected: 0x232327,
    rail_active: 0x232327,
    hairline: 0x26262b,
    hairline_hi: 0x3a3a40,
    snap: 0x18181b,
    text: 0xf2f2f3,
    dim: 0x9d9da2,
    amber: 0xe8b04a,
    green: 0x4cc777,
    pill_hi: 0x1f1f23,
    keycap: Raised {
        fill: 0x232327,
        ring: 0x34343a,
        hi: 0x2c2c31,
        shadow: 0x050506,
    },
    light_drop: 0x0c0c0e,
    light_idle: Raised {
        fill: 0x3a3a40,
        ring: 0x34343a,
        hi: 0x46464c,
        shadow: 0x0c0c0e,
    },
};

pub static LIGHT: Theme = Theme {
    bg: 0xeeeef0,
    surface: 0xfbfbfc,
    winbar: 0xf5f5f7,
    hover: 0xe3e3e7,
    raised: 0xe6e6ea,
    selected: 0xd6d6db,
    rail_active: 0xb4b4bb,
    hairline: 0xdadade,
    hairline_hi: 0xc2c2c9,
    snap: 0xe2e2e6,
    text: 0x18181b,
    dim: 0x5c5c63,
    amber: 0xa86a00,
    green: 0x18864a,
    pill_hi: 0xffffff,
    keycap: Raised {
        fill: 0xffffff,
        ring: 0xc8c8ce,
        hi: 0xffffff,
        shadow: 0xd6d6db,
    },
    light_drop: 0xdcdce0,
    light_idle: Raised {
        fill: 0xd4d4d9,
        ring: 0xc4c4ca,
        hi: 0xe2e2e6,
        shadow: 0xdcdce0,
    },
};

static IS_DARK: AtomicBool = AtomicBool::new(false);

/// The theme of the current appearance.
pub fn theme() -> &'static Theme {
    if IS_DARK.load(Ordering::Relaxed) {
        &DARK
    } else {
        &LIGHT
    }
}

pub fn set_dark(dark: bool) {
    IS_DARK.store(dark, Ordering::Relaxed);
}

pub fn is_dark() -> bool {
    IS_DARK.load(Ordering::Relaxed)
}

impl Raised {
    /// Pressed: `fill` with `ring` and nothing else.
    pub fn pressed(fill: u32, ring: u32) -> Raised {
        Raised {
            fill,
            ring,
            hi: fill,
            shadow: NONE,
        }
    }
}

/// No shadow line.
pub const NONE: u32 = 0xff00_0000;

#[derive(Clone, Copy, Debug, PartialEq)]
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

    pub fn inset(self, d: f32) -> Self {
        Rect::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }

    fn shift(self, dx: f32, dy: f32) -> Self {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

pub fn mix(under: u32, over: u32, a: f32) -> u32 {
    if a >= 1.0 {
        return over;
    }
    if a <= 0.0 {
        return under;
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

/// How much of the pixel at (i, j) a rounded rectangle covers.
pub fn coverage(i: usize, j: usize, b: Rect, r: f32) -> f32 {
    if b.w <= 0.0 || b.h <= 0.0 {
        return 0.0;
    }
    let r = r.min(b.w / 2.0).min(b.h / 2.0).max(0.0);
    (0.5 - rounded_sd(i as f32 + 0.5, j as f32 + 0.5, b, r)).clamp(0.0, 1.0)
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

    fn span(&self, b: Rect, pad: f32) -> (usize, usize, usize, usize) {
        let x0 = (b.x - pad).floor().max(0.0) as usize;
        let y0 = (b.y - pad).floor().max(0.0) as usize;
        let x1 = ((b.x + b.w + pad).ceil().max(0.0) as usize).min(self.w);
        let y1 = ((b.y + b.h + pad).ceil().max(0.0) as usize).min(self.h);
        (x0, y0, x1, y1)
    }

    /// Paints `color` over every pixel by `cover(i, j)` within `b`.
    fn paint(&mut self, b: Rect, color: u32, cover: impl Fn(usize, usize) -> f32) {
        let (x0, y0, x1, y1) = self.span(b, 1.0);
        for j in y0..y1 {
            for i in x0..x1 {
                let a = cover(i, j);
                self.blend(i, j, color, a);
            }
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        for j in y.min(self.h)..(y + h).min(self.h) {
            let row = j * self.w;
            self.px[row + x.min(self.w)..row + (x + w).min(self.w)].fill(color);
        }
    }

    pub fn rounded_rect(&mut self, b: Rect, r: f32, color: u32) {
        self.paint(b, color, |i, j| coverage(i, j, b, r));
    }

    /// A 1 px ring inside the edge of a rounded rectangle.
    pub fn ring_in(&mut self, b: Rect, r: f32, color: u32) {
        let inner = b.inset(1.0);
        self.paint(b, color, |i, j| {
            coverage(i, j, b, r) - coverage(i, j, inner, (r - 1.0).max(0.0))
        });
    }

    /// A 1 px ring outside the edge of a rounded rectangle.
    pub fn ring_out(&mut self, b: Rect, r: f32, color: u32) {
        let outer = b.inset(-1.0);
        self.paint(outer, color, |i, j| {
            coverage(i, j, outer, r + 1.0) - coverage(i, j, b, r)
        });
    }

    /// A control of the raised family.
    pub fn raised(&mut self, b: Rect, r: f32, s: Raised) {
        if s.shadow != NONE {
            let down = b.shift(0.0, 1.0);
            self.paint(down, s.shadow, |i, j| {
                coverage(i, j, down, r) - coverage(i, j, b, r)
            });
        }
        self.rounded_rect(b, r, s.fill);
        if s.hi != s.fill {
            let lower = b.shift(0.0, 2.0);
            self.paint(b, s.hi, |i, j| {
                coverage(i, j, b, r) * (1.0 - coverage(i, j, lower, r))
            });
        }
        self.ring_in(b, r, s.ring);
    }

    /// An alpha mask in `color`, its top left at (x, y).
    pub fn glyph(&mut self, g: &Glyph, x: i32, y: i32, color: u32, opacity: f32) {
        for row in 0..g.h {
            for col in 0..g.w {
                let a = g.alpha[row * g.w + col] as f32 / 255.0 * opacity;
                let (px, py) = (x + col as i32, y + row as i32);
                if px >= 0 && py >= 0 {
                    self.blend(px as usize, py as usize, color, a);
                }
            }
        }
    }

    /// An image of straight alpha ARGB pixels, its top left at (x, y).
    pub fn image(&mut self, img: &Image, x: i32, y: i32, opacity: f32) {
        for row in 0..img.h {
            for col in 0..img.w {
                let p = img.px[row * img.w + col];
                let a = (p >> 24) as f32 / 255.0 * opacity;
                let (px, py) = (x + col as i32, y + row as i32);
                if px >= 0 && py >= 0 {
                    self.blend(px as usize, py as usize, p & 0xffffff, a);
                }
            }
        }
    }

    /// Coverage of a ring of `width` centered on radius `r` around (cx, cy).
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

/// An image in straight alpha ARGB.
#[derive(Clone, Debug)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

/// How much of each pixel of a `w` by `h` grid a closed outline covers,
/// by the even-odd rule, from four sub-rows a pixel row.
pub fn fill_coverage(w: usize, h: usize, pts: &[(f32, f32)]) -> Vec<f32> {
    const SUB: usize = 4;
    let mut cov = vec![0.0f32; w * h];
    let mut xs: Vec<f32> = Vec::new();
    for j in 0..h {
        for s in 0..SUB {
            let y = j as f32 + (s as f32 + 0.5) / SUB as f32;
            xs.clear();
            for k in 0..pts.len() {
                let (a, b) = (pts[k], pts[(k + 1) % pts.len()]);
                if (a.1 <= y) != (b.1 <= y) {
                    xs.push(a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0));
                }
            }
            xs.sort_by(f32::total_cmp);
            for k in 0..xs.len() / 2 {
                let (x0, x1) = (
                    xs[2 * k].clamp(0.0, w as f32),
                    xs[2 * k + 1].clamp(0.0, w as f32),
                );
                let row = &mut cov[j * w..(j + 1) * w];
                let (i0, i1) = (x0.floor() as usize, x1.ceil() as usize);
                for (i, c) in row.iter_mut().enumerate().take(i1).skip(i0) {
                    let l = x0.max(i as f32);
                    let r = x1.min(i as f32 + 1.0);
                    if r > l {
                        *c += (r - l) / SUB as f32;
                    }
                }
            }
        }
    }
    cov
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

/// Inter Medium, cut to Latin; see fonts/LICENSE-Inter.txt. Other scripts
/// fall back to the fonts the system has.
const FONT: &[u8] = include_bytes!("../fonts/Inter-Medium.ttf");

/// Shaped lines and fitted strings kept between redraws.
const LINES: usize = 512;
/// Rasterized glyphs kept between redraws.
const GLYPHS: usize = 4096;

/// A shaped line: its glyphs with their pen positions, and its width.
struct Line {
    glyphs: Vec<(CacheKey, i32, i32)>,
    width: f32,
    baseline: f32,
}

/// Text through cosmic-text: shaping, bidi and fallback to system fonts.
pub struct Font {
    pub(crate) fonts: FontSystem,
    pub(crate) swash: SwashCache,
    lines: HashMap<(String, u32), Line>,
    fits: HashMap<(String, u32, u32), String>,
}

impl Font {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_data(FONT.to_vec());
        let family = db.faces().next().unwrap().families[0].0.clone();
        db.set_sans_serif_family(family);
        Self {
            fonts: FontSystem::new_with_locale_and_db("en-US".into(), db),
            swash: SwashCache::new(),
            lines: HashMap::new(),
            fits: HashMap::new(),
        }
    }
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_data(FONT.to_vec());
        let family = db.faces().next().map(|f| f.families[0].0.clone());
        db.load_system_fonts();
        if let Some(family) = family {
            db.set_sans_serif_family(family);
        }
        let locale = crate::locale::messages_locale();
        let locale = locale
            .split(['.', '@'])
            .next()
            .map(|l| l.replace('_', "-"))
            .filter(|l| !l.is_empty() && l != "C" && l != "POSIX")
            .unwrap_or_else(|| "en-US".to_string());
        Font {
            fonts: FontSystem::new_with_locale_and_db(locale, db),
            swash: SwashCache::new(),
            lines: HashMap::new(),
            fits: HashMap::new(),
        }
    }

    fn line(&mut self, text: &str, size: f32) -> &Line {
        let key = (text.to_string(), size.to_bits());
        if !self.lines.contains_key(&key) {
            if self.lines.len() >= LINES {
                self.lines.clear();
            }
            let line = self.shape(text, size);
            self.lines.insert(key.clone(), line);
        }
        &self.lines[&key]
    }

    fn shape(&mut self, text: &str, size: f32) -> Line {
        let mut buf = Buffer::new(&mut self.fonts, Metrics::new(size, size));
        buf.set_wrap(Wrap::None);
        buf.set_size(None, None);
        let attrs = Attrs::new()
            .family(Family::SansSerif)
            .weight(Weight::MEDIUM);
        buf.set_text(text, &attrs, Shaping::Advanced, None);
        buf.shape_until_scroll(&mut self.fonts, false);
        let mut line = Line {
            glyphs: Vec::new(),
            width: 0.0,
            baseline: size * 0.8,
        };
        if let Some(run) = buf.layout_runs().next() {
            line.width = run.line_w;
            line.baseline = run.line_y - run.line_top;
            for g in run.glyphs {
                let p = g.physical((0.0, 0.0), 1.0);
                line.glyphs.push((p.cache_key, p.x, p.y));
            }
        }
        line
    }

    pub fn width(&mut self, text: &str, size: f32) -> f32 {
        self.line(text, size).width
    }

    /// `text` cut with an ellipsis to fit `max` pixels, by shaped width.
    pub fn fit(&mut self, text: &str, size: f32, max: f32) -> String {
        if self.width(text, size) <= max {
            return text.to_string();
        }
        let key = (text.to_string(), size.to_bits(), max.to_bits());
        if let Some(s) = self.fits.get(&key) {
            return s.clone();
        }
        let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
        let (mut lo, mut hi) = (0, ends.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let cut = format!("{}\u{2026}", text[..ends[mid - 1]].trim_end());
            if self.shape(&cut, size).width <= max {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let out = if lo == 0 {
            "\u{2026}".to_string()
        } else {
            format!("{}\u{2026}", text[..ends[lo - 1]].trim_end())
        };
        if self.fits.len() >= LINES {
            self.fits.clear();
        }
        self.fits.insert(key, out.clone());
        out
    }

    /// `text` in at most `lines` lines of `max` pixels, broken after a
    /// space where one fits and anywhere in a word that does not, the last
    /// line cut with an ellipsis.
    pub fn wrap(&mut self, text: &str, size: f32, max: f32, lines: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        while !rest.is_empty() && out.len() < lines {
            if out.len() + 1 == lines || self.shape(&rest, size).width <= max {
                out.push(self.fit(&rest, size, max));
                break;
            }
            let chars: Vec<(usize, char)> = rest.char_indices().collect();
            let (mut lo, mut hi) = (1, chars.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                let end = chars.get(mid).map_or(rest.len(), |c| c.0);
                if self.shape(&rest[..end], size).width <= max {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            let mut end = chars.get(lo).map_or(rest.len(), |c| c.0);
            if let Some(space) = rest[..end].rfind(' ').filter(|&i| i > 0) {
                if chars.get(lo).is_some_and(|c| c.1 != ' ') {
                    end = space;
                }
            }
            out.push(rest[..end].trim_end().to_string());
            rest = rest[end..].trim_start().to_string();
        }
        out
    }

    /// Draws `text` with its left end at `x`, centered in a line box of
    /// the font size on `mid` the way a browser centers it. Answers the
    /// width drawn.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: f32,
        mid: f32,
        size: f32,
        color: u32,
    ) -> f32 {
        let line = self.line(text, size);
        let (glyphs, width) = (line.glyphs.clone(), line.width);
        let baseline = (mid - size / 2.0 + line.baseline).round() as i32;
        let x0 = x.round() as i32;
        if self.swash.image_cache.len() >= GLYPHS {
            self.swash.image_cache.clear();
        }
        for (key, gx, gy) in glyphs {
            let Some(img) = self.swash.get_image(&mut self.fonts, key) else {
                continue;
            };
            let (w, h) = (img.placement.width as i32, img.placement.height as i32);
            let left = x0 + gx + img.placement.left;
            let top = baseline + gy - img.placement.top;
            for row in 0..h {
                for col in 0..w {
                    let (px, py) = (left + col, top + row);
                    if px < 0 || py < 0 {
                        continue;
                    }
                    let i = (row * w + col) as usize;
                    match img.content {
                        SwashContent::Color => {
                            let p = &img.data[i * 4..i * 4 + 4];
                            let rgb = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
                            canvas.blend(px as usize, py as usize, rgb, p[3] as f32 / 255.0);
                        }
                        SwashContent::Mask => {
                            let a = img.data[i] as f32 / 255.0;
                            canvas.blend(px as usize, py as usize, color, a);
                        }
                        SwashContent::SubpixelMask => {}
                    }
                }
            }
        }
        width
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
    fn rings_sit_inside_and_outside_the_edge() {
        let b = Rect::new(2.0, 2.0, 10.0, 10.0);
        let mut c = Canvas::new(14, 14, 0);
        c.ring_in(b, 0.0, 0xffffff);
        assert_eq!(c.px[2 * 14 + 6], 0xffffff);
        assert_eq!(c.px[14 + 6], 0);
        let mut c = Canvas::new(14, 14, 0);
        c.ring_out(b, 0.0, 0xffffff);
        assert_eq!(c.px[14 + 6], 0xffffff);
        assert_eq!(c.px[2 * 14 + 6], 0);
    }

    #[test]
    fn a_square_outline_covers_its_pixels() {
        let pts = [(1.0, 1.0), (3.0, 1.0), (3.0, 3.0), (1.0, 3.0)];
        let cov = fill_coverage(4, 4, &pts);
        assert!((cov[4 + 1] - 1.0).abs() < 1e-4);
        assert_eq!(cov[0], 0.0);
        let half = [(0.5, 0.0), (4.0, 0.0), (4.0, 4.0), (0.5, 4.0)];
        assert!((fill_coverage(4, 4, &half)[0] - 0.5).abs() < 1e-4);
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
        let lines = f.wrap(
            "one two three four five six seven eight nine ten",
            13.0,
            90.0,
            2,
        );
        assert_eq!(lines.len(), 2);
        assert!(!lines[0].ends_with(' '));
        assert!(lines[1].ends_with('\u{2026}'));
        assert!(lines.iter().all(|l| f.width(l, 13.0) <= 90.0));
        assert_eq!(f.wrap("short", 13.0, 90.0, 2), vec!["short"]);
    }
}
