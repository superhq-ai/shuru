// Vendored from voxl 0.3.5 (https://github.com/tembo/voxl), Copyright (c)
// 2026 Tembo, under the MIT license: see LICENSE-voxl. Pinned to v0.3.5.

//! Thin safe wrapper around libx264 tuned for low-latency desktop streaming.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use std::ffi::CString;
use std::ptr;

#[derive(Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub crf: f32,
    /// VBV cap in kbit/s (0 = uncapped).
    pub max_kbps: u32,
}

pub struct Encoder {
    h: *mut ffi::x264_t,
    param: ffi::x264_param_t,
    pic: ffi::x264_picture_t,
    pts: i64,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Encoder {}

pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

impl Encoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, String> {
        unsafe {
            let mut param: ffi::x264_param_t = std::mem::zeroed();
            let preset = CString::new(cfg.preset.as_str()).unwrap();
            let tune = c"zerolatency";
            if ffi::x264_param_default_preset(&mut param, preset.as_ptr(), tune.as_ptr()) < 0 {
                return Err(format!("bad x264 preset {}", cfg.preset));
            }
            param.i_width = cfg.width as i32;
            param.i_height = cfg.height as i32;
            param.i_csp = ffi::X264_CSP_I420 as i32;
            param.i_fps_num = cfg.fps;
            param.i_fps_den = 1;
            param.i_timebase_num = 1;
            param.i_timebase_den = 1000;
            // VFR input costs a frame of encoder delay (duration estimation).
            param.b_vfr_input = 0;
            param.i_threads = cfg.threads as i32;
            param.b_sliced_threads = (cfg.threads > 1) as i32;
            param.i_slice_count = cfg.threads as i32;
            param.i_keyint_max = ffi::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = if std::env::var("VOXL_X264_INFO").as_deref() == Ok("debug") {
                // Per-frame QP, macroblock types and size.
                ffi::X264_LOG_DEBUG
            } else if std::env::var("VOXL_X264_INFO").is_ok() {
                ffi::X264_LOG_INFO
            } else {
                ffi::X264_LOG_WARNING
            } as i32;
            param.rc.i_rc_method = ffi::X264_RC_CRF as i32;
            param.rc.f_rf_constant = cfg.crf;
            param.rc.i_lookahead = 0;
            // ultrafast's diamond search only follows motion of a few pixels, so a dragged
            // window or a fast scroll would be coded as new pixels every frame. UMH finds
            // those shifts (a third of the bits for a window drag) for about the same CPU.
            param.analyse.i_me_method = ffi::X264_ME_UMH as i32;
            // shuru divergence (upstream candidate): widen UMH's search from voxl's 64
            // to 256. A fast scroll or flick shifts content by far more than 64 px a
            // frame; the wider search finds the true vector instead of coding the move
            // as new pixels. Measured ~13 kB vs ~29 kB on a 240-row flick for ~1 ms more
            // CPU. Worth proposing to voxl.
            param.analyse.i_me_range = 256;
            // Per-macroblock "unchanged" hints from X damage let x264 skip analysis.
            param.analyse.b_mb_info = (std::env::var("VOXL_NO_MBINFO").is_err()) as i32;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if cfg.max_kbps > 0 {
                param.rc.i_vbv_max_bitrate = cfg.max_kbps as i32;
                // ~1.25 frames of buffer keeps single-frame spikes bounded.
                param.rc.i_vbv_buffer_size = vbv_buffer_kbit(cfg.max_kbps, cfg.fps) as i32;
            }
            // sRGB content (BT.709 primaries, sRGB transfer) in BT.601 limited range,
            // matching libyuv ARGBToI420 and the macOS NV12 capture.
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 13; // IEC 61966-2-1: every capture path delivers sRGB
            param.vui.i_colmatrix = 6;
            param.vui.b_fullrange = 0;
            let profile = c"high";
            ffi::x264_param_apply_profile(&mut param, profile.as_ptr());

            let h = ffi::x264_encoder_open(&mut param);
            if h.is_null() {
                return Err("x264_encoder_open failed".into());
            }
            let mut pic: ffi::x264_picture_t = std::mem::zeroed();
            ffi::x264_picture_init(&mut pic);
            pic.img.i_csp = ffi::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(Encoder {
                h,
                param,
                pic,
                pts: 0,
                width: cfg.width,
                height: cfg.height,
            })
        }
    }

    /// SHURU: the stream's WebCodecs codec string, such as `avc1.640028`: the
    /// SPS's profile, constraint flags and level. The Mac and web viewers
    /// configure their decoders with it and refuse a hello without one.
    pub fn codec(&self) -> Option<String> {
        const NAL_SPS: i32 = 7;
        unsafe {
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n = 0;
            if ffi::x264_encoder_headers(self.h, &mut nals, &mut n) < 0 || nals.is_null() {
                return None;
            }
            let sps = std::slice::from_raw_parts(nals, n as usize)
                .iter()
                .find(|nal| nal.i_type == NAL_SPS)?;
            avc_codec(std::slice::from_raw_parts(
                sps.p_payload,
                sps.i_payload as usize,
            ))
        }
    }

    /// Reconfigure rate control on the fly (buffer of ~1.25 frames at `max_kbps`).
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        self.set_rate_buf(
            crf,
            max_kbps,
            vbv_buffer_kbit(max_kbps, self.param.i_fps_num),
        );
    }

    /// Reconfigure rate control with an explicit VBV buffer (kbit).
    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, buf_kbit: u32) {
        unsafe {
            self.param.rc.f_rf_constant = crf;
            if max_kbps > 0 {
                self.param.rc.i_vbv_max_bitrate = max_kbps as i32;
                self.param.rc.i_vbv_buffer_size = buf_kbit.max(1) as i32;
            }
            ffi::x264_encoder_reconfig(self.h, &mut self.param);
        }
    }

    /// Encode one picture. `force_idr` requests a keyframe, `qp` (if set) forces the frame QP.
    /// Appends Annex-B output to `out`.
    /// `mb_info` (one byte per macroblock, X264_MBINFO_CONSTANT for unchanged ones) lets
    /// x264 skip analysis of static areas; it is consumed before this returns.
    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        qp: Option<i32>,
        mb_info: Option<&mut [u8]>,
        out: &mut Vec<u8>,
    ) -> Result<Encoded, String> {
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if force_idr {
                ffi::X264_TYPE_IDR as i32
            } else {
                ffi::X264_TYPE_AUTO as i32
            };
            self.pic.i_qpplus1 = qp.map(|q| q + 1).unwrap_or(0);
            self.pic.prop.mb_info = mb_info.map(|m| m.as_mut_ptr()).unwrap_or(ptr::null_mut());
            self.pic.prop.mb_info_free = None;
            // Strictly increasing pts even if the wall clock stalls.
            self.pts = ts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = std::mem::zeroed();
            let size =
                ffi::x264_encoder_encode(self.h, &mut nals, &mut n, &mut self.pic, &mut pic_out);
            if size < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            if size > 0 && !nals.is_null() {
                // NAL payloads are contiguous in memory.
                let first = &*nals;
                out.extend_from_slice(std::slice::from_raw_parts(first.p_payload, size as usize));
            }
            if std::env::var("VOXL_DEBUG_MARKER").is_ok() {
                log::info!(
                    "x264 out {} bytes, delayed {}, pts in {} out {}",
                    size,
                    ffi::x264_encoder_delayed_frames(self.h),
                    self.pts,
                    pic_out.i_pts
                );
            }
            Ok(Encoded {
                keyframe: pic_out.b_keyframe != 0,
                // Undocumented as an output, but x264 has filled it in for years and
                // FFmpeg's libx264 wrapper reads it the same way.
                qp: (pic_out.i_qpplus1 > 0).then(|| (pic_out.i_qpplus1 - 1) as f32),
                crf_avg: pic_out.prop.f_crf_avg as f32,
            })
        }
    }
}

/// A VBV buffer of 1.25 frames at `max_kbps`, rounded up: x264 treats a buffer under
/// 1.1 frames as a single-frame VBV, which may lower rows of a frame below its starting
/// QP (video.rs's MotionQuality relies on it not doing that).
fn vbv_buffer_kbit(max_kbps: u32, fps: u32) -> u32 {
    (max_kbps * 5).div_ceil(4 * fps.max(1))
}

/// What x264 made of one picture.
pub struct Encoded {
    pub keyframe: bool,
    /// The QP x264 started the frame at, rounded (None if x264 didn't say). VBV may
    /// have raised rows of the frame above it; x264 doesn't report the average.
    pub qp: Option<f32>,
    /// x264's "average effective CRF": the CRF the frame was encoded with plus what VBV
    /// added to its average QP, before the frame and within it.
    pub crf_avg: f32,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// cargo test --release encode_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn encode_cost() {
        let (w, h) = (2560usize, 1600usize);
        let mbs = (w / 16) * (h / 16);
        let threads: u32 = std::env::var("T")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(1);
        #[cfg(unix)]
        let cpu = || unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut r);
            (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64 * 1e3
                + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e3
        };
        // Process CPU time isn't wired up on Windows; wall times still print.
        #[cfg(windows)]
        let cpu = || 0.0;
        for (label, hint) in [("no hints", false), ("hints", true)] {
            let mut enc = Encoder::new(&EncoderConfig {
                width: w as u32,
                height: h as u32,
                fps: 60,
                preset: "ultrafast".into(),
                threads,
                crf: 24.0,
                max_kbps: 30000,
            })
            .unwrap();
            let mut y = vec![0u8; w * h];
            let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
            for (i, p) in y.iter_mut().enumerate() {
                *p = ((i % w) ^ (i / w)) as u8;
            }
            let mut out = Vec::new();
            let mut total = std::time::Duration::ZERO;
            let mut cpu_ms = 0.0;
            for f in 0..120 {
                // A 640x360 region changes every frame; the rest stays still.
                for row in 0..360 {
                    for x in 0..640 {
                        y[row * w + x] = (x + row + f * 7) as u8;
                    }
                }
                let mut info: Vec<u8> = (0..mbs)
                    .map(|i| ((i % (w / 16)) >= 40 || (i / (w / 16)) >= 23) as u8)
                    .collect();
                let planes = Planes {
                    y: &y,
                    u: &u,
                    v: &v,
                    stride_y: w,
                    stride_uv: w / 2,
                };
                out.clear();
                let t = std::time::Instant::now();
                let c0 = cpu();
                enc.encode(
                    &planes,
                    f as i64 * 16,
                    f == 0,
                    None,
                    if hint { Some(&mut info) } else { None },
                    &mut out,
                )
                .unwrap();
                if f >= 20 {
                    total += t.elapsed();
                    cpu_ms += cpu() - c0;
                }
            }
            println!(
                "{threads}t {label}: {:.2} ms/frame wall, {:.2} ms CPU, {} bytes last",
                total.as_secs_f64() * 1000.0 / 100.0,
                cpu_ms / 100.0,
                out.len()
            );
        }
    }
}

/// SHURU: `avc1.PPCCLL` from an Annex B SPS: the three bytes after the start
/// code and the NAL header.
fn avc_codec(sps: &[u8]) -> Option<String> {
    let start = sps.windows(3).position(|w| w == [0, 0, 1])? + 3;
    let b = sps.get(start + 1..start + 4)?;
    Some(format!("avc1.{:02x}{:02x}{:02x}", b[0], b[1], b[2]))
}

#[cfg(test)]
mod codec_tests {
    #[test]
    fn the_codec_string_is_read_from_the_sps() {
        let sps = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
        assert_eq!(super::avc_codec(&sps).as_deref(), Some("avc1.640028"));
        assert_eq!(super::avc_codec(&[0, 0, 1, 0x67, 0x64]), None);
    }

    #[test]
    fn a_real_encoder_names_its_codec_as_the_viewers_expect() {
        let enc = super::Encoder::new(&super::EncoderConfig {
            width: 1280,
            height: 720,
            fps: 30,
            preset: "ultrafast".into(),
            threads: 1,
            crf: 23.0,
            max_kbps: 0,
        })
        .unwrap();
        let codec = enc.codec().unwrap();
        // The Mac and web viewers accept only ^avc1\.[0-9a-f]{6}$.
        let hex = codec.strip_prefix("avc1.").unwrap_or_default();
        assert!(
            hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()),
            "{codec}"
        );
    }
}
