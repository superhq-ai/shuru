// Derived from voxl (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo,
// under the MIT license: see LICENSE-voxl.

//! x264 through the C side in x264.c.

use anyhow::{bail, Result};

use crate::convert::I420;

#[repr(C)]
struct RawEncoder {
    _private: [u8; 0],
}

extern "C" {
    fn screen_enc_open(
        width: i32,
        height: i32,
        fps: i32,
        threads: i32,
        crf: f32,
        max_kbps: i32,
    ) -> *mut RawEncoder;
    fn screen_enc_avc(e: *mut RawEncoder, out: *mut u8) -> i32;
    fn screen_enc_set_rate(e: *mut RawEncoder, crf: f32, max_kbps: i32, buf_kbit: i32);
    #[allow(clippy::too_many_arguments)]
    fn screen_enc_encode(
        e: *mut RawEncoder,
        y: *mut u8,
        u: *mut u8,
        v: *mut u8,
        stride_y: i32,
        stride_uv: i32,
        pts: i64,
        idr: i32,
        mb_info: *mut u8,
        out: *mut *const u8,
        keyframe: *mut i32,
    ) -> i32;
    fn screen_enc_close(e: *mut RawEncoder);
}

pub struct Encoder {
    raw: *mut RawEncoder,
    fps: u32,
    pts: i64,
}

// The encoder is used from one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, threads: u32, crf: f32, max_kbps: u32) -> Result<Self> {
        let raw = unsafe {
            screen_enc_open(
                width as i32,
                height as i32,
                fps as i32,
                threads as i32,
                crf,
                max_kbps as i32,
            )
        };
        if raw.is_null() {
            bail!("x264 would not open an encoder for {width}x{height}");
        }
        Ok(Encoder { raw, fps, pts: 0 })
    }

    /// The WebCodecs codec string of the stream, such as `avc1.42c01f`.
    pub fn codec(&self) -> Result<String> {
        let mut avc = [0u8; 3];
        if unsafe { screen_enc_avc(self.raw, avc.as_mut_ptr()) } != 0 {
            bail!("x264 gave no SPS");
        }
        Ok(format!("avc1.{:02x}{:02x}{:02x}", avc[0], avc[1], avc[2]))
    }

    /// Rate control with a buffer of about 1.25 frames at `max_kbps`.
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        self.set_rate_buf(crf, max_kbps, max_kbps / self.fps.max(1) * 5 / 4);
    }

    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, buf_kbit: u32) {
        unsafe { screen_enc_set_rate(self.raw, crf, max_kbps as i32, buf_kbit as i32) }
    }

    /// Encodes `pic` and appends the access unit to `out`; true for a
    /// keyframe. `mb_info` has one byte a macroblock, 1 for one that did
    /// not change; x264 is done with it when this returns.
    pub fn encode(
        &mut self,
        pic: &I420,
        ts_ms: i64,
        idr: bool,
        mb_info: Option<&mut [u8]>,
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        self.pts = ts_ms.max(self.pts + 1);
        let mut data: *const u8 = std::ptr::null();
        let mut key = 0;
        let size = unsafe {
            screen_enc_encode(
                self.raw,
                pic.y.as_ptr() as *mut u8,
                pic.u.as_ptr() as *mut u8,
                pic.v.as_ptr() as *mut u8,
                pic.width as i32,
                pic.stride_uv() as i32,
                self.pts,
                idr as i32,
                mb_info.map_or(std::ptr::null_mut(), |m| m.as_mut_ptr()),
                &mut data,
                &mut key,
            )
        };
        if size < 0 {
            bail!("x264 failed to encode a frame");
        }
        if size > 0 && !data.is_null() {
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, size as usize) });
        }
        Ok(key != 0)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { screen_enc_close(self.raw) }
    }
}
