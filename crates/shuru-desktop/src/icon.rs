//! App icons: the Icon key of a `.desktop` file looked up in the hicolor
//! and Adwaita themes and in /usr/share/pixmaps, PNG only. The size asked
//! for is used when there is one; otherwise the next larger size is scaled
//! down by area, or the largest smaller one scaled up. A guest whose apps
//! ship only SVG icons renders them to PNG beforehand.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::draw::Image;

const THEMES: [&str; 2] = ["hicolor", "Adwaita"];
const MAX_FILE: u64 = 4 << 20;

fn data_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_DATA_HOME") {
        Some(h) if !h.is_empty() => out.push(PathBuf::from(h)),
        _ => out.extend(home.as_ref().map(|h| h.join(".local/share"))),
    }
    let data = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    out.extend(data.split(':').map(PathBuf::from));
    out
}

/// The PNG files for `name` with the size of the directory they are in;
/// 0 for a size that comes from the file.
fn candidates(name: &str) -> Vec<(u32, PathBuf)> {
    let file = format!("{name}.png");
    let mut out = Vec::new();
    for dir in data_dirs() {
        for theme in THEMES {
            let Ok(sizes) = std::fs::read_dir(dir.join("icons").join(theme)) else {
                continue;
            };
            for s in sizes.flatten() {
                let n = s.file_name();
                let Some(size) = n
                    .to_str()
                    .and_then(|n| n.split_once('x'))
                    .filter(|(w, h)| w == h)
                    .and_then(|(w, _)| w.parse::<u32>().ok())
                else {
                    continue;
                };
                let Ok(contexts) = std::fs::read_dir(s.path()) else {
                    continue;
                };
                for c in contexts.flatten() {
                    let p = c.path().join(&file);
                    if p.is_file() {
                        out.push((size, p));
                    }
                }
            }
        }
        let p = dir.join("pixmaps").join(&file);
        if p.is_file() {
            out.push((0, p));
        }
    }
    out
}

/// The icon `name` at `size` by `size` pixels.
pub fn load(name: &str, size: u32) -> Option<Image> {
    let mut found: Vec<(u32, PathBuf)> = if name.starts_with('/') {
        if !name.ends_with(".png") {
            return None;
        }
        vec![(0, PathBuf::from(name))]
    } else {
        candidates(name)
    };
    // Exact, then larger from the smallest, then smaller from the largest.
    found.sort_by_key(|(s, _)| match *s {
        s if s == size => (0, 0),
        0 => (1, 0),
        s if s > size => (1, s),
        s => (2, u32::MAX - s),
    });
    found
        .iter()
        .find_map(|(_, p)| decode(p))
        .map(|img| scale(&img, size as usize))
}

fn decode(path: &Path) -> Option<Image> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let mut dec = png::Decoder::new(File::open(path).ok()?);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || w > 1024 || h > 1024 {
        return None;
    }
    let n = info.color_type.samples();
    let px = buf[..info.buffer_size()]
        .chunks_exact(n)
        .map(|p| {
            let (r, g, b, a) = match n {
                4 => (p[0], p[1], p[2], p[3]),
                3 => (p[0], p[1], p[2], 255),
                2 => (p[0], p[0], p[0], p[1]),
                _ => (p[0], p[0], p[0], 255),
            };
            (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
        })
        .collect();
    Some(Image { w, h, px })
}

/// Scales to `size` square by the area each pixel covers, in
/// premultiplied alpha.
pub fn scale(img: &Image, size: usize) -> Image {
    if img.w == size && img.h == size {
        return img.clone();
    }
    let (fx, fy) = (img.w as f32 / size as f32, img.h as f32 / size as f32);
    let mut px = Vec::with_capacity(size * size);
    for y in 0..size {
        let (y0, y1) = (y as f32 * fy, (y + 1) as f32 * fy);
        for x in 0..size {
            let (x0, x1) = (x as f32 * fx, (x + 1) as f32 * fx);
            let mut acc = [0.0f32; 4];
            let mut area = 0.0;
            for sy in y0.floor() as usize..(y1.ceil() as usize).min(img.h) {
                let wy = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0);
                for sx in x0.floor() as usize..(x1.ceil() as usize).min(img.w) {
                    let wx = (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                    let p = img.px[sy * img.w + sx];
                    let a = (p >> 24) as f32 / 255.0;
                    let k = wx * wy;
                    acc[0] += k * a;
                    acc[1] += k * a * ((p >> 16) & 0xff) as f32;
                    acc[2] += k * a * ((p >> 8) & 0xff) as f32;
                    acc[3] += k * a * (p & 0xff) as f32;
                    area += k;
                }
            }
            let a = acc[0] / area.max(1e-6);
            let c = |v: f32| {
                if acc[0] > 0.0 {
                    (v / acc[0]).round().clamp(0.0, 255.0) as u32
                } else {
                    0
                }
            };
            let alpha = (a * 255.0).round().clamp(0.0, 255.0) as u32;
            px.push(alpha << 24 | c(acc[1]) << 16 | c(acc[2]) << 8 | c(acc[3]));
        }
    }
    Image {
        w: size,
        h: size,
        px,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_averages_by_area() {
        let img = Image {
            w: 2,
            h: 2,
            px: vec![0xffff0000, 0xffff0000, 0x00000000, 0x00000000],
        };
        let s = scale(&img, 1);
        assert_eq!(s.px[0] >> 24, 128);
        assert_eq!(s.px[0] & 0xffffff, 0xff0000);
    }

    #[test]
    fn decodes_a_png() {
        let dir = std::env::temp_dir().join(format!("shuru-desktop-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dot.png");
        {
            let f = File::create(&path).unwrap();
            let mut enc = png::Encoder::new(f, 4, 4);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[0x80; 64]).unwrap();
        }
        let img = load(path.to_str().unwrap(), 2).unwrap();
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.px[0], 0x80808080);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
