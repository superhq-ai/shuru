// Derived from voxl (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo,
// under the MIT license: see LICENSE-voxl.

//! BGRX to I420 by libyuv, split across threads by row bands.

extern "C" {
    fn ARGBToI420(
        src_argb: *const u8,
        src_stride_argb: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        width: i32,
        height: i32,
    ) -> i32;
}

pub struct I420 {
    pub width: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        I420 {
            width,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    pub fn stride_uv(&self) -> usize {
        self.width.div_ceil(2)
    }
}

struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    fn get(&self) -> *mut u8 {
        self.0
    }
}

/// Runs `run(a, b)` over row chunks of [y0, y1) on up to `threads`
/// threads; chunk edges stay even for 4:2:0.
fn split_rows(y0: usize, y1: usize, threads: usize, run: impl Fn(usize, usize) + Sync) {
    let rows = y1 - y0;
    if rows == 0 {
        return;
    }
    let n = threads.clamp(1, (rows / 64).max(1));
    if n == 1 {
        run(y0, y1);
        return;
    }
    let chunk = (rows.div_ceil(n) + 1) & !1;
    std::thread::scope(|s| {
        let mut a = y0;
        while a < y1 {
            let b = (a + chunk).min(y1);
            let run = &run;
            if b == y1 {
                run(a, b);
            } else {
                s.spawn(move || run(a, b));
            }
            a = b;
        }
    });
}

/// Converts rows [y0, y1), even aligned, of `src` into `dst`.
pub fn convert_band(
    src: &[u8],
    src_stride: usize,
    dst: &mut I420,
    y0: usize,
    y1: usize,
    threads: usize,
) {
    if y1 <= y0 {
        return;
    }
    let w = dst.width;
    let suv = dst.stride_uv();
    let (yp, up, vp) = (
        SendPtr(dst.y.as_mut_ptr()),
        SendPtr(dst.u.as_mut_ptr()),
        SendPtr(dst.v.as_mut_ptr()),
    );
    let src_p = SendPtr(src.as_ptr() as *mut u8);
    let run = |a: usize, b: usize| unsafe {
        ARGBToI420(
            src_p.get().add(a * src_stride),
            src_stride as i32,
            yp.get().add(a * w),
            w as i32,
            up.get().add((a / 2) * suv),
            suv as i32,
            vp.get().add((a / 2) * suv),
            suv as i32,
            w as i32,
            (b - a) as i32,
        );
    };
    split_rows(y0, y1, threads, run);
}
