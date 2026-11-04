//! Stills: the screen now as a PNG, scaled down on request.

use anyhow::Result;

use crate::hub::Snapshot;

/// The snapshot as a PNG at most `width` wide, keeping its aspect.
pub fn png(shot: &Snapshot, width: Option<usize>) -> Result<Vec<u8>> {
    let (w, h) = (shot.width, shot.height);
    let ow = width.unwrap_or(w).clamp(1, w);
    let oh = ((h * ow + w / 2) / w).max(1);
    let rgb = if ow == w {
        shot.bgrx
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect()
    } else {
        scale(&shot.bgrx, w, h, ow, oh)
    };
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, ow as u32, oh as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(&rgb)?;
    Ok(out)
}

/// BGRX at `w`x`h` to RGB at `ow`x`oh`, each pixel the mean of the source
/// pixels it covers.
fn scale(src: &[u8], w: usize, h: usize, ow: usize, oh: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(ow * oh * 3);
    let cols: Vec<(usize, usize)> = (0..ow)
        .map(|x| (x * w / ow, ((x + 1) * w / ow).max(x * w / ow + 1)))
        .collect();
    let mut acc = vec![[0u32; 3]; ow];
    for y in 0..oh {
        let (y0, y1) = (y * h / oh, ((y + 1) * h / oh).max(y * h / oh + 1));
        acc.iter_mut().for_each(|a| *a = [0; 3]);
        for row in src.chunks_exact(w * 4).take(y1).skip(y0) {
            for (a, &(x0, x1)) in acc.iter_mut().zip(&cols) {
                for p in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    a[0] += p[2] as u32;
                    a[1] += p[1] as u32;
                    a[2] += p[0] as u32;
                }
            }
        }
        for (a, &(x0, x1)) in acc.iter().zip(&cols) {
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            out.extend(a.iter().map(|&c| ((c + n / 2) / n) as u8));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_to_the_width_asked() {
        let (w, h) = (64, 40);
        let shot = Snapshot {
            width: w,
            height: h,
            bgrx: vec![200; w * h * 4],
        };
        let png = png(&shot, Some(16)).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!((info.width, info.height), (16, 10));
    }

    #[test]
    fn mean_of_the_covered_pixels() {
        // 2x1 to 1x1: one black pixel and one white.
        let src = [0, 0, 0, 0, 255, 255, 255, 0];
        assert_eq!(scale(&src, 2, 1, 1, 1), vec![128, 128, 128]);
    }
}
