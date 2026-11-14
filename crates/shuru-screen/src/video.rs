// Vendored from voxl 0.3.5 (https://github.com/tembo/voxl), Copyright (c)
// 2026 Tembo, under the MIT license: see LICENSE-voxl. Pinned to v0.3.5.
// Stripped to Linux: voxl's macOS display switching and dynamic resize are
// removed, snapshots use shuru's still::Snapshot, and two shallow integration
// hooks (keyframe-on-reconnect via Client::resync in the server, and
// input-quiet sharpening here) sit behind the hello capability.

//! Video pipeline: damage-driven capture -> I420 -> x264 -> clients, with ack-based flow control.

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::{Encoded, Encoder, EncoderConfig, Planes};
use crate::hub::wait_any;
use crate::hub::{Hub, Inflight, Out};
use bytes::Bytes;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct VideoConfig {
    pub display: Option<String>,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    pub max_kbps: u32,
    pub paintover_crf: f32,
    pub paintover_burst: u32,
    pub paintover_delay_ms: u64,
    pub min_kbps: u32,
    pub coalesce: Duration,
    pub stripes: u32,
    pub adaptive_threads: bool,
}

struct Job {
    buf: Box<I420>,
    idx: usize,
    force_idr: bool,
    paintover: bool,
    /// Last pass of a paint-over burst.
    paintover_final: bool,
    /// Rows that changed in this capture.
    band: Option<Band>,
    /// Per-macroblock change map for this capture (None: analyse everything).
    mb_dirty: Option<Arc<Vec<u8>>>,
    captured_at: Instant,
    marker_ms: i64,
}

fn union(a: Option<Band>, b: Band) -> Band {
    match a {
        None => b,
        Some(a) => Band {
            y0: a.y0.min(b.y0),
            y1: a.y1.max(b.y1),
        },
    }
}

/// One capture/encode setup for the current screen size.
struct Pipeline {
    cap: Capturer,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<(usize, Box<I420>)>,
    encode: std::thread::JoinHandle<()>,
}

/// Runs the pipeline. On Linux the screen size is fixed; a geometry change (an
/// RandR resize underneath us) ends the capture loop and the pipeline is rebuilt
/// at the new size, with clients getting the new geometry and a keyframe.
pub fn spawn(cfg: VideoConfig, hub: Arc<Hub>) -> Result<(), String> {
    let mut p = build(&cfg, &hub)?;
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || loop {
            let Pipeline {
                mut cap,
                job_tx,
                ret_rx,
                encode,
            } = p;
            if let Err(e) = capture_loop(&mut cap, cfg.clone(), hub.clone(), job_tx, ret_rx) {
                if cap.geometry_changed() {
                    log::info!("capture geometry changed: {e}");
                } else {
                    log::error!("capture loop ended: {e}");
                    std::process::exit(1);
                }
            }
            drop(cap);
            let _ = encode.join();
            p = match build(&cfg, &hub) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("video restart failed: {e}");
                    std::process::exit(1);
                }
            };
        })
        .unwrap();
    Ok(())
}

fn build(cfg: &VideoConfig, hub: &Arc<Hub>) -> Result<Pipeline, String> {
    let cap = Capturer::new(cfg.display.as_deref())?;
    let (w, h) = (cap.width, cap.height);
    hub.width.store(w, Ordering::Relaxed);
    hub.height.store(h, Ordering::Relaxed);
    log::info!(
        "capturing {}x{} @ {} fps, preset {} threads {}",
        w,
        h,
        cfg.fps,
        cfg.preset,
        cfg.threads
    );

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
    let (ret_tx, ret_rx) = mpsc::channel::<(usize, Box<I420>)>();
    let layout = stripe_layout(h, cfg.stripes);
    let mut stripes = Vec::new();
    for &(y, sh) in &layout {
        let fixed = if cfg.adaptive_threads {
            1
        } else {
            (cfg.threads / layout.len() as u32).max(1)
        };
        let ecfg = EncoderConfig {
            width: w,
            height: sh,
            fps: cfg.fps,
            preset: cfg.preset.clone(),
            threads: fixed,
            crf: cfg.crf,
            max_kbps: cfg.max_kbps,
        };
        let max_threads = if cfg.adaptive_threads {
            (cfg.threads / layout.len() as u32).max(1)
        } else {
            1
        };
        stripes.push(Stripe {
            y: y as usize,
            h: sh as usize,
            enc: Encoder::new(&ecfg)?,
            cfg: ecfg,
            max_threads,
            applied_kbps: 0,
            applied_crf: -1.0,
        });
    }
    *hub.stripes.lock() = layout;
    let hub2 = hub.clone();
    let (crf, po_crf) = (cfg.crf, cfg.paintover_crf);
    let encode = std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || encode_loop(stripes, job_rx, ret_tx, hub2, crf, po_crf))
        .unwrap();
    // New geometry (and a keyframe) for clients already connected; no-op at startup.
    hub.restart_clients();
    Ok(Pipeline {
        cap,
        job_tx,
        ret_rx,
        encode,
    })
}

/// Whether a client is over the in-flight bound `flow_ok` enforces: more frames queued
/// than ~one RTT plus decode covers, or an oldest-unacked age past ~rtt+120 ms. A client
/// with nothing in flight is never over the bound.
fn over_flow_bound(c: &crate::hub::Client, interval_ms: f64) -> bool {
    let (n, age) = c.backlog();
    if n == 0 {
        return false;
    }
    let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
        0 => 40,
        r => r,
    } as f64;
    let max_n = (((rtt + 40.0) / interval_ms).ceil() as usize + 2).clamp(3, 40);
    let max_age = rtt as u128 + 120;
    n >= max_n || age > max_age
}

/// Bug 2 fix (UPSTREAM-VOXL CANDIDATE, voxl 0.3.5 `video.rs` bug 2): before a forced
/// keyframe that is about to bypass `flow_ok`, drop the in-flight backlog of any viewer
/// already over the flow bound, so the IDR (the single largest frame) resyncs the viewer
/// instead of stacking on a full queue. This is the same resync move `flow_ok` already
/// makes for a stalled viewer below. Returns how many viewers were cleared.
/// See docs/findings/voxl-bugs.md #2.
fn clear_backlogged_inflight(clients: &[Arc<crate::hub::Client>], interval_ms: f64) -> usize {
    let mut cleared = 0;
    for c in clients {
        if over_flow_bound(c, interval_ms) {
            c.inflight.lock().clear();
            cleared += 1;
        }
    }
    cleared
}

fn flow_ok(hub: &Hub, interval_ms: f64) -> bool {
    for c in hub.video_clients() {
        let (n, age) = c.backlog();
        if n == 0 {
            continue;
        }
        if age > 4000 {
            // Stalled client (hidden tab, dead socket): stop letting it gate everyone.
            log::warn!(
                "client {} stalled ({} frames, {} ms); resyncing",
                c.id,
                n,
                age
            );
            c.inflight.lock().clear();
            c.needs_keyframe.store(true, Ordering::Relaxed);
            c.started.store(false, Ordering::Relaxed);
            continue;
        }
        if over_flow_bound(&c, interval_ms) {
            return false;
        }
    }
    true
}

/// Delay-based bitrate adaptation: back off toward the measured delivery rate when
/// acks show queueing, probe upward slowly when the path is clear.
struct RateCtl {
    target: f64,
    max: f64,
    min: f64,
    last_eval: Instant,
    last_dec: Instant,
    last_inc: Instant,
    /// Ramp multiplicatively until the first sign of congestion (like TCP slow start),
    /// so a session neither starts by flooding a slow link nor crawls up on a fast one.
    slow_start: bool,
    /// Delivery rate observed at the last congestion: the path's likely capacity.
    ceiling: f64,
    ceiling_at: Instant,
}

/// Starting point for a new session's bitrate cap (kbit/s).
const START_KBPS: f64 = 8000.0;

impl RateCtl {
    fn new(max: f64, min: f64) -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        RateCtl {
            target: START_KBPS.min(max),
            max,
            min,
            last_eval: past,
            last_dec: past,
            last_inc: past,
            slow_start: true,
            ceiling: f64::MAX,
            ceiling_at: past,
        }
    }

    fn update(&mut self, hub: &Hub, clients: &[Arc<crate::hub::Client>]) {
        if self.last_eval.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_eval = Instant::now();
        let mut congested = false;
        let mut clear = true;
        let mut rate = f64::MAX;
        let mut delivered = 0f64;
        for c in clients {
            if !c.started.load(Ordering::Relaxed) {
                continue;
            }
            let q = c.delay.lock().queue_ms;
            let (_, age) = c.backlog();
            let rtt = match c.min_rtt_ms.load(Ordering::Relaxed) {
                0 => 40,
                r => r,
            } as f64;
            if q > 30.0 || age as f64 > rtt + 80.0 {
                congested = true;
                let r = c.delivered.lock().rate_bps / 1000.0;
                if r > 0.0 {
                    rate = rate.min(r);
                }
            }
            if q > 15.0 {
                clear = false;
            }
            delivered = delivered.max(c.delivered.lock().rate_bps / 1000.0);
        }
        let now = Instant::now();
        if congested && self.last_dec.elapsed() >= Duration::from_millis(300) {
            let mut t = self.target * 0.85;
            // Delivery rate only says something about capacity when we were actually
            // pushing near the target; congestion from one big frame on an idle screen
            // would otherwise pin the ceiling at a few kbps.
            if rate.is_finite() && rate >= self.target * 0.5 {
                self.ceiling = rate;
                self.ceiling_at = now;
                t = t.min(rate * 0.85);
            }
            self.target = t.max(self.min);
            self.last_dec = now;
            self.slow_start = false;
            log::debug!(
                "rate: congested, target {:.0} kbps (delivered {:.0})",
                self.target,
                rate
            );
        } else if self.slow_start
            && !congested
            && clear
            && self.last_inc.elapsed() >= Duration::from_millis(250)
        {
            // Only probe upward once the content actually presses against the cap.
            if delivered >= self.target * 0.5 {
                self.target = (self.target * 1.5).min(self.max);
                self.last_inc = now;
                log::debug!(
                    "rate: slow start, target {:.0} kbps (delivered {:.0})",
                    self.target,
                    delivered
                );
            }
        } else if !congested
            && clear
            && self.last_dec.elapsed() >= Duration::from_secs(1)
            && delivered >= self.target * 0.5
        {
            // Only raise the cap while the content actually uses it: an idle screen says
            // nothing about the path, and an inflated cap overshoots the next burst.
            // Paths change; forget a capacity estimate that hasn't been confirmed lately.
            if self.ceiling_at.elapsed() >= Duration::from_secs(15) {
                self.ceiling = f64::MAX;
            }
            // Recover quickly to just under the known capacity, then probe past it gently
            // so a busy link isn't driven into the queue every couple of seconds.
            if self.target < self.ceiling * 0.9
                && self.last_inc.elapsed() >= Duration::from_millis(200)
            {
                self.target = (self.target * 1.06 + 100.0)
                    .min(self.ceiling * 0.9)
                    .min(self.max);
                self.last_inc = now;
            } else if self.target >= self.ceiling * 0.9
                && self.last_inc.elapsed() >= Duration::from_millis(1000)
            {
                self.target = (self.target * 1.02).min(self.max);
                self.last_inc = now;
            }
        }
        hub.target_kbps.store(self.target as u32, Ordering::Relaxed);
    }
}

/// Regional sharpening. Blocks encoded while they changed carry motion-level quality;
/// once a block has been still for a while it is re-encoded at paint-over quality,
/// even while other parts of the screen keep moving (a playing video would otherwise
/// keep the frame-level paint-over from ever running).
struct Refiner {
    epoch: Instant,
    changed_ms: Vec<u32>,
    pending: Vec<bool>,
    last_refine: Instant,
    cols: usize,
}

const REFINE_STILL_MS: u32 = 300;
const REFINE_EVERY: Duration = Duration::from_millis(250);

impl Refiner {
    fn new(blocks: usize, cols: usize) -> Self {
        Refiner {
            epoch: Instant::now(),
            changed_ms: vec![0; blocks],
            pending: vec![false; blocks],
            last_refine: Instant::now(),
            cols,
        }
    }
    fn now(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }
    fn mark(&mut self, dirty: &[u8]) {
        let now = self.now();
        for (i, &d) in dirty.iter().enumerate() {
            if d != 0 {
                self.pending[i] = true;
                self.changed_ms[i] = now;
            }
        }
    }
    fn mark_all(&mut self) {
        let now = self.now();
        self.pending.fill(true);
        self.changed_ms.fill(now);
    }
    fn clear_all(&mut self) {
        self.pending.fill(false);
    }
    /// Every block not yet sharpened (1 = encode) and the row band they span.
    fn pending_map(&self) -> Option<(Vec<u8>, Band)> {
        let (mut r0, mut r1) = (usize::MAX, 0);
        let map: Vec<u8> = self
            .pending
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                if p {
                    r0 = r0.min(i / self.cols);
                    r1 = r1.max(i / self.cols + 1);
                }
                p as u8
            })
            .collect();
        (r1 > r0).then(|| {
            (
                map,
                Band {
                    y0: (r0 * 16) as u32,
                    y1: (r1 * 16) as u32,
                },
            )
        })
    }
    /// Blocks due for sharpening (1 = refine) and the row band they span.
    fn due(&mut self) -> Option<(Vec<u8>, Band)> {
        if self.last_refine.elapsed() < REFINE_EVERY {
            return None;
        }
        let now = self.now();
        let mut map = vec![0u8; self.pending.len()];
        let (mut r0, mut r1, mut any) = (usize::MAX, 0, false);
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i] && now.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS {
                *m = 1;
                self.pending[i] = false;
                let row = i / self.cols;
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
                any = true;
            }
        }
        if !any {
            return None;
        }
        self.last_refine = Instant::now();
        Some((
            map,
            Band {
                y0: (r0 * 16) as u32,
                y1: (r1 * 16) as u32,
            },
        ))
    }
}

fn capture_loop(
    cap: &mut Capturer,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<(usize, Box<I420>)>,
) -> Result<(), String> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    let mut pool: Vec<Option<Box<I420>>> = vec![
        Some(Box::new(I420::new(w, h))),
        Some(Box::new(I420::new(w, h))),
    ];
    let mut pending: [Option<Band>; 2] = [Some(cap.full()), Some(cap.full())];
    let mut next_idx = 0usize;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow slight early capture so source jitter doesn't alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
    let mut last_motion = Instant::now();
    let mut paintover_left = 0u32;
    let mut rc = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
    hub.target_kbps.store(rc.target as u32, Ordering::Relaxed);
    let mut had_clients = false;
    let mut full_next = true;
    let mut refiner = Refiner::new(w.div_ceil(16) * h.div_ceil(16), w.div_ceil(16));
    let debug_marker = std::env::var("VOXL_DEBUG_MARKER").is_ok();

    loop {
        if cap.needs_rebuild() {
            return Ok(());
        }
        cap.pump()?;
        if cap.needs_rebuild() {
            return Ok(());
        }
        cap.changes.clear();
        hub.drain_wake();
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        if !snaps.is_empty() {
            // The whole current screen, whether or not anyone is watching. Damage
            // stays pending, so the stream still sends what changed. Only the pixels
            // are copied here (the framebuffer is live); the PNG is made off this thread.
            // The capturer takes the damage for exactly these pixels with them, so the
            // screenshot's seq covers what it shows and nothing drawn after it.
            let bgrx = cap.snapshot()?;
            cap.changes.clear();
            let shot = Arc::new(crate::still::Snapshot {
                width: w,
                height: h,
                bgrx,
                // The cursor shown now, so the still can draw it in (stills only).
                cursor: hub.cursor_image.lock().clone(),
            });
            for tx in snaps {
                let _ = tx.send(shot.clone());
            }
        }
        while let Ok((i, b)) = ret_rx.try_recv() {
            pool[i] = Some(b);
        }
        let clients = hub.video_clients();
        if clients.is_empty() {
            if had_clients {
                rc = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
                hub.target_kbps.store(rc.target as u32, Ordering::Relaxed);
            }
            had_clients = false;
            wait_any(&[cap.fd(), hub.waker.waitable()], 500);
            continue;
        }
        rc.update(&hub, &clients);
        let want_key = clients
            .iter()
            .any(|c| c.needs_keyframe.load(Ordering::Relaxed));
        if !had_clients {
            had_clients = true;
            // Our shm copy may be stale: re-grab everything once.
            cap.grab(cap.full())?;
            pending = [Some(cap.full()), Some(cap.full())];
            cap.damaged = true;
            full_next = true;
        }
        // Shallow hook (behind the hello `sharpenOnInputQuiet` capability): hold the
        // paint-over off while the viewer is still driving input, so a burst of scroll
        // or drag events doesn't trip a sharpen between frames; sharpen once input goes
        // quiet. A plain viewer never sets the flag, so this is a no-op for it.
        let input_quiet = !hub.sharpen_on_input_quiet()
            || hub
                .now_ms()
                .saturating_sub(hub.last_input_ms.load(Ordering::Relaxed))
                >= cfg.paintover_delay_ms;
        let paintover_due = paintover_left > 0
            && last_motion.elapsed() >= Duration::from_millis(cfg.paintover_delay_ms)
            && input_quiet;
        if !cap.damaged && !want_key && !paintover_due {
            let timeout = if paintover_left == 0 {
                100
            } else {
                cfg.paintover_delay_ms
                    .saturating_sub(last_motion.elapsed().as_millis() as u64) as i32
                    + 1
            };
            wait_any(&[cap.fd(), hub.waker.waitable()], timeout);
            continue;
        }
        let since = last_capture.elapsed();
        if since < min_interval {
            let rem = min_interval - since;
            wait_any(
                &[hub.waker.waitable()],
                (rem.as_micros() as i32 + 999) / 1000,
            );
            continue;
        }
        // Apps present a frame as a burst of puts; give the burst a moment to land so
        // the capture holds one whole frame instead of its first tiles.
        if let Some(t) = cap.damaged_at {
            let el = t.elapsed();
            if el < cfg.coalesce {
                let rem = cfg.coalesce - el;
                std::thread::sleep(rem);
                continue;
            }
        }
        if !flow_ok(&hub, interval_ms) {
            if want_key {
                // Bug 2 (upstream-voxl candidate): a forced keyframe bypasses the flow
                // bound, so on a slow link a reconnect-in-motion would stack an IDR on an
                // already-deep queue. Clear the backlogged viewers first so the IDR
                // resyncs them instead of growing the backlog. See voxl-bugs.md #2.
                let cleared = clear_backlogged_inflight(&clients, interval_ms);
                if cleared > 0 {
                    log::debug!("forced keyframe under backlog: resynced {cleared} viewer(s)");
                }
            } else {
                hub.stats.flow_blocked.fetch_add(1, Ordering::Relaxed);
                wait_any(&[cap.fd(), hub.waker.waitable()], 4);
                continue;
            }
        }
        // Need a free buffer (the encoder may still hold the other).
        if pool[next_idx].is_none() {
            match ret_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((i, b)) => pool[i] = Some(b),
                Err(_) => continue,
            }
            if pool[next_idx].is_none() {
                continue;
            }
        }

        let t0 = Instant::now();
        let origin = cap.damaged_at.unwrap_or(t0);
        hub.stats
            .damage_wait_us
            .fetch_add((t0 - origin).as_micros() as u64, Ordering::Relaxed);
        let mut band = None;
        let mut mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        match &mb_dirty {
            Some(m) => refiner.mark(m),
            None => refiner.mark_all(),
        }
        if cap.damaged {
            band = cap.take_damage()?;
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let t1 = Instant::now();
        if debug_marker {
            log::info!(
                "cap dt {:.1} ms band {:?} grab {} us",
                last_capture.elapsed().as_secs_f64() * 1000.0,
                band,
                (t1 - t0).as_micros()
            );
        }
        let marker_ms = match cap.bgrx() {
            Some((px, stride)) if debug_marker => marker_probe(px, stride, "grab"),
            _ => -1,
        };
        let is_motion = band.is_some();
        if !is_motion && !want_key && !paintover_due {
            // Damage was empty after all.
            continue;
        }
        // Sharpen only what was sent at motion quality; nothing pending, nothing to send.
        let paint_map = if paintover_due && !is_motion && !want_key {
            match refiner.pending_map() {
                Some(m) => Some(m),
                None => {
                    paintover_left = 0;
                    continue;
                }
            }
        } else {
            None
        };
        let idx = next_idx;
        let mut buf = pool[idx].take().unwrap();
        let todo = match (pending[idx], band) {
            (p, Some(b)) => Some(union(p, b)),
            (p, None) => p,
        };
        if let Some(b) = band {
            pending[1 - idx] = Some(union(pending[1 - idx], b));
        }
        pending[idx] = None;
        if let Some(t) = todo {
            cap.convert(&mut buf, t.y0 as usize, t.y1 as usize, cfg.convert_threads);
        }
        let t2 = Instant::now();
        hub.stats
            .grab_us
            .fetch_add((t1 - t0).as_micros() as u64, Ordering::Relaxed);
        hub.stats
            .convert_us
            .fetch_add((t2 - t1).as_micros() as u64, Ordering::Relaxed);
        hub.stats.captured.fetch_add(1, Ordering::Relaxed);

        let mut refine = None;
        let paintover = if is_motion {
            last_motion = Instant::now();
            paintover_left = cfg.paintover_burst;
            // Piggyback sharpening of blocks that have gone still on this frame.
            refine = if want_key { None } else { refiner.due() };
            refine.is_some()
        } else if paintover_due {
            paintover_left -= 1;
            hub.stats.paintovers.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        };
        if want_key {
            for c in &clients {
                c.needs_keyframe.store(false, Ordering::Relaxed);
            }
            // A keyframe is squeezed into the rate cap; sharpen all of it once still.
            refiner.mark_all();
        }
        let frame_paintover = paintover && refine.is_none();
        if frame_paintover && paintover_left == 0 {
            refiner.clear_all();
        }
        // A refinement frame encodes what moved plus what is due; everything else stays skipped.
        let mb_map = if want_key {
            None
        } else if frame_paintover {
            paint_map.map(|(m, b)| {
                band = Some(match band {
                    Some(x) => union(Some(x), b),
                    None => b,
                });
                Arc::new(m)
            })
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(match band {
                Some(b) => union(Some(b), rband),
                None => rband,
            });
            let mut m = mb_dirty.take().unwrap_or_else(|| vec![1; rmap.len()]);
            for (d, r) in m.iter_mut().zip(rmap) {
                *d |= r;
            }
            hub.stats.refines.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(m))
        } else {
            mb_dirty.take().map(Arc::new)
        };
        last_capture = t0;
        next_idx = 1 - idx;
        if job_tx
            .send(Job {
                buf,
                idx,
                force_idr: want_key,
                paintover,
                paintover_final: frame_paintover && paintover_left == 0,
                mb_dirty: mb_map,
                band,
                captured_at: origin,
                marker_ms,
            })
            .is_err()
        {
            return Err("encoder gone".into());
        }
    }
}

/// Split the screen into horizontal stripes, each an independent H.264 stream.
/// x264 pays ~9 ms of CPU per 2560x1600 frame even when nothing changed, and sliced
/// threading triples that; independent single-threaded stripes are encoded in
/// parallel (low latency) and only when they changed (low CPU).
fn stripe_layout(height: u32, n: u32) -> Vec<(u32, u32)> {
    let n = n.clamp(1, 16);
    let each = (height.div_ceil(n) + 15) & !15;
    let mut v = Vec::new();
    let mut y = 0;
    while y < height {
        let h = each.min(height - y);
        v.push((y, h));
        y += h;
    }
    v
}

struct Stripe {
    y: usize,
    h: usize,
    enc: Encoder,
    cfg: EncoderConfig,
    /// Sliced threads to fan out to under sustained heavy motion (1 = never).
    max_threads: u32,
    applied_kbps: u32,
    applied_crf: f32,
}

const HDR: usize = 16;

struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SendPtr<T> {}

struct StripeJob {
    /// Valid until the worker replies: the encode loop waits for every stripe.
    buf: SendPtr<I420>,
    ts: i64,
    force_idr: bool,
    /// A paint-over or refinement pass (`crf` is the paint-over CRF).
    paintover: bool,
    crf: f32,
    cap: u32,
    mb_dirty: Option<Arc<Vec<u8>>>,
    mb_cols: usize,
}

type StripeResult = (usize, Option<(bool, Vec<u8>)>);

/// x264's sliced threading costs ~3x the CPU of one thread on desktop content (and on
/// small updates is even slower), so a stripe encodes single-threaded and only fans out
/// while sustained heavy motion would otherwise blow the frame budget.
struct ThreadGovernor {
    threads: u32,
    ewma_ms: f64,
    heavy: u32,
    light_since: Option<Instant>,
    budget_ms: f64,
    /// One-thread encode time as a function of changed macroblocks, learned from
    /// single-threaded frames, to tell when one thread would fit again.
    fit: CostFit,
    /// Predicted one-thread time while threaded.
    predicted_ms: f64,
}

/// Exponentially weighted least squares fit of encode ms = a + b * changed MBs.
#[derive(Default)]
struct CostFit {
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl CostFit {
    fn add(&mut self, x: f64, y: f64) {
        let k = 0.995;
        self.w = self.w * k + 1.0;
        self.x = self.x * k + x;
        self.y = self.y * k + y;
        self.xx = self.xx * k + x * x;
        self.xy = self.xy * k + x * y;
    }
    fn predict(&self, x: f64) -> Option<f64> {
        if self.w < 30.0 {
            return None;
        }
        let var = self.xx * self.w - self.x * self.x;
        if var <= f64::EPSILON * self.w * self.w {
            return Some(self.y / self.w);
        }
        let b = (self.xy * self.w - self.x * self.y) / var;
        let a = (self.y - b * self.x) / self.w;
        Some(a + b * x)
    }
}

impl ThreadGovernor {
    fn want(&mut self, enc_ms: f64, changed_mbs: f64, max: u32) -> Option<u32> {
        self.ewma_ms = self.ewma_ms * 0.8 + enc_ms * 0.2;
        if self.threads == 1 {
            self.fit.add(changed_mbs, enc_ms);
            // Before one thread runs out of frame budget: encode times vary from frame to
            // frame, and at ~90% of the budget on average enough frames miss it to drop
            // the frame rate.
            self.heavy = if self.ewma_ms > self.budget_ms * 0.65 {
                self.heavy + 1
            } else {
                0
            };
            if self.heavy >= 20 {
                self.predicted_ms = self.ewma_ms;
                return Some(max);
            }
            return None;
        }
        // Without enough one-thread history, assume one thread is no faster than these.
        let one = self.fit.predict(changed_mbs).unwrap_or(enc_ms);
        self.predicted_ms = self.predicted_ms * 0.8 + one * 0.2;
        if self.predicted_ms < self.budget_ms * 0.5 {
            // One thread would fit with room to spare (switching reopens the encoder,
            // which costs a keyframe, so not on a brief lull).
            let since = *self.light_since.get_or_insert_with(Instant::now);
            (since.elapsed() >= Duration::from_secs(2)).then_some(1)
        } else {
            self.light_since = None;
            None
        }
    }
}

/// x264's CRF raises the QP of frames it judges complex, and its quick half-resolution
/// estimate can't follow a dragged window or a fast scroll: the moved area counts as new
/// pixels and the QP lands 7-10 steps above `--crf`, although the real search codes the
/// move as cheap motion vectors (a few KB a frame). Big moves went blocky, with nothing
/// saved that the bitrate cap needed. Motion frames that come out that cheap get a CRF
/// lowered by what the estimate adds, so they stay at the configured quality. Video and
/// new content really are complex, and x264's higher QP there saves real bits, so those
/// are left alone, as are the lower QPs x264 picks for small updates (typing) and
/// paint-overs. VBV still raises the QP when the link is short.
#[derive(Default)]
struct MotionQuality {
    /// QP steps x264's complexity estimate adds to the next frame (negative for simple
    /// ones). x264 blurs it over the frames it encodes, paint-overs included, so each
    /// frame's is a good guess at the next one's.
    complexity: f32,
    /// log2 of the bits per changed macroblock of recent motion frames, scaled to the
    /// configured CRF. Averaged in the log domain: one frame of new content (a menu
    /// opening mid-drag) shouldn't outweigh the run of cheap frames around it.
    mb_bits_log2: f32,
    /// QP steps taken off the CRF of motion frames now.
    applied: f32,
}

/// Most QP steps taken off a motion frame's CRF.
const MAX_MOTION_EXCESS: f32 = 12.0;

/// Bits per changed macroblock (at the configured CRF) under which motion counts as
/// cheap: a dragged window or a scrolled page costs 10-30, video and new content 50 and
/// more.
const CHEAP_MB_BITS: f32 = 40.0;

impl MotionQuality {
    /// The CRF to encode a motion frame with, for the configured `crf`.
    fn crf(&mut self, crf: f32) -> f32 {
        let want = if self.mb_bits_log2 < CHEAP_MB_BITS.log2() {
            self.complexity.clamp(0.0, MAX_MOTION_EXCESS)
        } else {
            0.0
        };
        // Whole steps, and only once the estimate has moved a full step: every change
        // reconfigures x264.
        if (want - self.applied).abs() >= 1.0 {
            self.applied = want.round();
        }
        (crf - self.applied).max(0.0)
    }

    /// Notes what x264 made of a frame it was given `crf` for. `size` is set for a
    /// motion frame (not a paint-over).
    fn note(&mut self, e: &Encoded, crf: f32, size: Option<MotionSize>) {
        // A keyframe's QP isn't picked from the complexity estimate.
        let (false, Some(qp)) = (e.keyframe, e.qp) else {
            return;
        };
        self.observe(crf, qp, e);
        // Small updates cost more per macroblock (headers, a cursor's edges) and never
        // get x264's QP up; only big ones tell what kind of motion this is.
        if let Some(m) = size.filter(|m| m.changed * 16 >= m.mbs) {
            // Halving the quantizer step (6 QP) about doubles the bits.
            let b = (m.bits.max(1) as f32 / m.changed.max(1) as f32).log2() + (qp - m.crf) / 6.0;
            self.mb_bits_log2 = self.mb_bits_log2 * 0.7 + b * 0.3;
        }
    }

    /// x264 starts from QP0 = `crf` + complexity. VBV may raise that before the frame
    /// (to `qp`) and row by row within it (to an average x264 doesn't report), never
    /// below QP0 or, with a VBV buffer over 1.1 frames, below the frame's starting QP,
    /// and `e.crf_avg` is `crf` + average - QP0. So the complexity lies between
    /// `qp - e.crf_avg` (no row raised) and `qp - crf` (nothing raised before the
    /// frame), give or take the rounding of `qp`: one value when VBV stayed out, a range
    /// when it didn't. Then the last estimate is kept while it fits, else moved to the
    /// nearest end, so a frame VBV squeezed doesn't throw away what earlier frames showed.
    fn observe(&mut self, crf: f32, qp: f32, e: &Encoded) {
        let lo = qp - 0.5 - e.crf_avg;
        let hi = qp + 0.5 - crf;
        self.complexity = if hi - lo < 1.25 {
            (lo + hi) / 2.0
        } else {
            self.complexity.max(lo).min(hi)
        };
    }
}

/// How big a motion frame came out, for MotionQuality.
#[derive(Clone, Copy)]
struct MotionSize {
    bits: usize,
    /// Macroblocks that changed, of the stripe's `mbs`.
    changed: usize,
    mbs: usize,
    /// The configured CRF.
    crf: f32,
}

fn stripe_worker(
    i: usize,
    mut s: Stripe,
    rx: mpsc::Receiver<StripeJob>,
    done: mpsc::Sender<StripeResult>,
    debug: bool,
) {
    let mut gov = ThreadGovernor {
        threads: 1,
        ewma_ms: 0.0,
        heavy: 0,
        light_since: None,
        budget_ms: 1000.0 / s.cfg.fps as f64,
        fit: CostFit::default(),
        predicted_ms: 0.0,
    };
    let mut motion = MotionQuality::default();
    while let Ok(job) = rx.recv() {
        let buf = unsafe { &*job.buf.0 };
        let is_motion = !job.force_idr && !job.paintover;
        let crf = if is_motion {
            motion.crf(job.crf)
        } else {
            job.crf
        };
        if job.force_idr {
            // A keyframe is what a new viewer sees first; don't starve it to the
            // steady-state frame budget (up to ~8 frames' worth at 4x the cap).
            let boost = job.cap.saturating_mul(4).max(16_000);
            s.enc
                .set_rate_buf(job.crf, boost, boost / s.cfg.fps.max(1) * 8);
            s.applied_kbps = 0;
        } else {
            let moved = s.applied_kbps == 0
                || (job.cap as f64 - s.applied_kbps as f64).abs() > s.applied_kbps as f64 * 0.04;
            if crf != s.applied_crf || moved {
                s.enc.set_rate(crf, job.cap);
                s.applied_crf = crf;
                s.applied_kbps = job.cap;
            }
        }
        let suv = buf.stride_uv();
        let planes = Planes {
            y: &buf.y[s.y * buf.width..],
            u: &buf.u[(s.y / 2) * suv..],
            v: &buf.v[(s.y / 2) * suv..],
            stride_y: buf.width,
            stride_uv: suv,
        };
        let mut out = Vec::with_capacity(64 << 10);
        out.resize(HDR, 0);
        // x264 wants X264_MBINFO_CONSTANT (1) on unchanged macroblocks of this stripe.
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let rows = s.h.div_ceil(16);
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols]
                .iter()
                .map(|&x| (x == 0) as u8)
                .collect::<Vec<u8>>()
        });
        // Changed macroblocks in this stripe (all of them without a damage map).
        let stripe_mbs = s.h.div_ceil(16) * job.mb_cols;
        let changed = info
            .as_ref()
            .map_or(stripe_mbs, |m| m.iter().filter(|&&c| c == 0).count());
        let te = Instant::now();
        let r = s.enc.encode(
            &planes,
            job.ts,
            job.force_idr,
            None,
            info.as_deref_mut(),
            &mut out,
        );
        if debug {
            let cpu_now = thread_cpu_us();
            log::info!(
                "stripe y{} encode {} us wall, thread cpu total {} us, {} bytes",
                s.y,
                te.elapsed().as_micros(),
                cpu_now,
                out.len()
            );
        }
        if s.max_threads > 1 {
            if let Some(t) = gov.want(
                te.elapsed().as_secs_f64() * 1000.0,
                changed as f64,
                s.max_threads,
            ) {
                match Encoder::new(&EncoderConfig {
                    threads: t,
                    ..s.cfg.clone()
                }) {
                    Ok(e) => {
                        log::info!(
                            "stripe {i}: encoder now {t} thread(s) (encode {:.1} ms)",
                            gov.ewma_ms
                        );
                        // A fresh encoder opens with an IDR on its next frame, and
                        // its rate control starts over.
                        s.enc = e;
                        motion = MotionQuality::default();
                        s.applied_kbps = 0;
                        gov.threads = t;
                        gov.heavy = 0;
                        gov.light_since = None;
                    }
                    Err(e) => log::error!("encoder reopen failed: {e}"),
                }
            }
        }
        let res = match r {
            Ok(e) => {
                let size = is_motion.then(|| MotionSize {
                    bits: (out.len() - HDR) * 8,
                    changed,
                    mbs: stripe_mbs,
                    crf: job.crf,
                });
                motion.note(&e, crf, size);
                (out.len() > HDR).then_some((e.keyframe, out))
            }
            Err(e) => {
                log::error!("{e}");
                None
            }
        };
        if done.send((i, res)).is_err() {
            return;
        }
    }
}

fn encode_loop(
    stripes_in: Vec<Stripe>,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::Sender<(usize, Box<I420>)>,
    hub: Arc<Hub>,
    crf: f32,
    po_crf: f32,
) {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let n = stripes_in.len();
    let debug_stripes = std::env::var("VOXL_DEBUG_STRIPES").is_ok();
    // Geometry and paint-over bookkeeping stay here; encoders move to their workers.
    let mut stripes: Vec<(usize, usize, bool)> =
        stripes_in.iter().map(|s| (s.y, s.h, false)).collect();
    let (done_tx, done_rx) = mpsc::channel::<StripeResult>();
    let workers: Vec<(mpsc::Sender<StripeJob>, std::thread::JoinHandle<()>)> = stripes_in
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let (tx, rx) = mpsc::channel();
            let done = done_tx.clone();
            let h = std::thread::Builder::new()
                .name(format!("enc-{i}"))
                .spawn(move || stripe_worker(i, s, rx, done, debug_stripes))
                .unwrap();
            (tx, h)
        })
        .collect();
    while let Ok(job) = rx.recv() {
        let t0 = Instant::now();
        let selected: Vec<bool> = stripes
            .iter()
            .map(|&(y, h, touched)| {
                job.force_idr
                    || (job.paintover && touched)
                    || job
                        .band
                        .is_some_and(|b| (b.y0 as usize) < y + h && (b.y1 as usize) > y)
            })
            .collect();
        if !selected.iter().any(|&x| x) {
            let _ = ret.send((job.idx, job.buf));
            continue;
        }
        let target = hub.target_kbps.load(Ordering::Relaxed);
        let want_crf = if job.paintover { po_crf } else { crf };
        // Each stripe may use up to twice its area share of the budget; the delay-based
        // controller reins the total in if several stripes move at once.
        let share = ((target as u64 * 2) / n as u64).max(target as u64 / n as u64 + 1) as u32;
        let ts = job.captured_at.duration_since(epoch).as_millis() as i64;
        // Persistent per-stripe workers keep each encoder's state warm in one core's cache.
        let buf_ptr = SendPtr(&*job.buf as *const I420);
        let mut pending = 0;
        for (i, &on) in selected.iter().enumerate() {
            if on {
                let cap = if job.paintover {
                    share.saturating_mul(2)
                } else {
                    share
                };
                let _ = workers[i].0.send(StripeJob {
                    buf: buf_ptr,
                    ts,
                    force_idr: job.force_idr,
                    paintover: job.paintover,
                    crf: want_crf,
                    cap,
                    mb_dirty: job.mb_dirty.clone(),
                    mb_cols: job.buf.width.div_ceil(16),
                });
                pending += 1;
            }
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        for _ in 0..pending {
            if let Ok((i, r)) = done_rx.recv() {
                results[i] = r;
            }
        }
        let _ = ret.send((job.idx, job.buf));
        for (y, h, touched) in stripes.iter_mut() {
            if job.paintover_final {
                *touched = false;
            }
            if job
                .band
                .is_some_and(|b| (b.y0 as usize) < *y + *h && (b.y1 as usize) > *y)
            {
                *touched = true;
            }
        }
        let parts: Vec<(usize, bool, Vec<u8>)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|(k, o)| (i, k, o)))
            .collect();
        if parts.is_empty() {
            continue;
        }
        let enc_us = t0.elapsed().as_micros() as u64;
        let all_key = parts.len() == n && parts.iter().all(|p| p.1);
        let total: usize = parts.iter().map(|p| p.2.len()).sum();
        hub.stats.encode_us.fetch_add(enc_us, Ordering::Relaxed);
        hub.stats.encoded.fetch_add(1, Ordering::Relaxed);
        hub.stats.bytes.fetch_add(total as u64, Ordering::Relaxed);
        if all_key {
            hub.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        frame_id = frame_id.wrapping_add(1);
        // Server-side pipeline latency (capture -> sent) in 1/10 ms, for client stats.
        let mut pipe = (job.captured_at.elapsed().as_micros() / 100).min(u32::MAX as u128) as u32;
        if job.marker_ms >= 0 {
            // Debug: wall-clock send time instead of pipeline latency.
            pipe = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32;
        }
        let nparts = parts.len() as u8;
        let msgs: Vec<Bytes> = parts
            .into_iter()
            .map(|(i, key, mut out)| {
                // type, flags, frame id, pipe, stripe, stripe count, parts in frame, reserved, y
                out[0] = 0x01;
                out[1] = (key as u8) | if job.paintover { 2 } else { 0 };
                out[2..6].copy_from_slice(&frame_id.to_be_bytes());
                out[6..10].copy_from_slice(&pipe.to_be_bytes());
                out[10] = i as u8;
                out[11] = n as u8;
                out[12] = nparts;
                out[13] = 0;
                out[14..16].copy_from_slice(&(stripes[i].0 as u16).to_be_bytes());
                Bytes::from(out)
            })
            .collect();
        let now = Instant::now();
        for c in hub.video_clients() {
            if !c.started.load(Ordering::Relaxed) {
                if !all_key {
                    continue;
                }
                c.started.store(true, Ordering::Relaxed);
                c.inflight.lock().clear();
            }
            {
                let mut q = c.inflight.lock();
                if q.len() > 600 {
                    log::warn!("client {} hopelessly behind; dropping", c.id);
                    drop(q);
                    c.send(Out::Text("{\"t\":\"close\"}".into()));
                    continue;
                }
                q.push_back(Inflight {
                    id: frame_id,
                    sent: now,
                    bytes: total,
                });
            }
            for m in &msgs {
                c.send(Out::Bin(m.clone()));
            }
        }
        hub.stats.send_lat_us.fetch_add(
            job.captured_at.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        if job.marker_ms >= 0 {
            log::info!(
                "marker sent age {} ms (encode {} us)",
                marker_age(job.marker_ms),
                enc_us
            );
        }
    }
}

/// Debug: decode the bench loadgen marker at (300,150) and log its age.
/// CPU time this thread has used, in microseconds.
fn thread_cpu_us() -> i64 {
    use nix::time::{clock_gettime, ClockId};
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(|t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
        .unwrap_or(0)
}

fn marker_probe(buf: &[u8], stride: usize, what: &str) -> i64 {
    let mut v: u64 = 0;
    let (mx, my) = std::env::var("VOXL_DEBUG_MARKER")
        .ok()
        .and_then(|s| {
            s.split_once(',')
                .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        })
        .unwrap_or((300, 150));
    for b in 0..40 {
        let x = mx + (b % 20) * 24 + 12;
        let y = my + (b / 20) * 24 + 12;
        let i = y * stride + x * 4;
        if buf[i + 2] > 128 {
            v |= 1 << b;
        }
    }
    let ms = (v & 0xffffff) as i64;
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    log::info!("marker {} frame {} age {} ms", what, v >> 24, now - ms);
    ms
}

fn marker_age(ms: i64) -> i64 {
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64)
        & 0xffffff;
    now - ms
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 1280;
    const H: usize = 768;

    /// What the screen shows in a test sequence.
    #[derive(Clone, Copy)]
    enum Scene {
        /// A window of text-like detail dragged 48 px a frame across a gradient desktop.
        Drag,
        /// Noise over the whole screen, new every frame: content that really is complex.
        Noise,
    }

    fn draw(y: &mut [u8], scene: Scene, frame: usize) {
        match scene {
            Scene::Drag => {
                let (ww, wh) = (W / 2, H / 2);
                let x0 = 48 * frame % (W - ww);
                let y0 = H / 4;
                for row in 0..H {
                    let line = &mut y[row * W..(row + 1) * W];
                    line.fill((40 + row * 120 / H) as u8);
                    if (y0..y0 + wh).contains(&row) {
                        let r = row - y0;
                        for (x, p) in line[x0..x0 + ww].iter_mut().enumerate() {
                            // Lines of "glyphs": dark strokes on white, a gap between lines.
                            let glyph =
                                ((x / 9) as u64 * 2654435761 + (r / 18) as u64 * 40503) >> 7;
                            let ink = r % 18 < 12 && (glyph >> (x % 9 + r % 18 % 6)) & 1 == 1;
                            *p = if ink { 30 } else { 235 };
                        }
                    }
                }
            }
            Scene::Noise => {
                let mut state = (frame as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15);
                for p in y.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *p = 64 + (state % 128) as u8;
                }
            }
        }
    }

    /// One frame as a stripe worker saw it: the CRF it handed x264 and what came back.
    struct Frame {
        crf: f32,
        e: Encoded,
        complexity: f32,
    }

    /// Encodes frames the way a stripe worker does, under a bitrate cap of `max_kbps`
    /// (0: none), with or without MotionQuality.
    struct Run {
        enc: Encoder,
        motion: MotionQuality,
        correct: bool,
        crf: f32,
        max_kbps: u32,
        applied: f32,
        y: Vec<u8>,
        n: usize,
    }

    impl Run {
        fn new(correct: bool, crf: f32, max_kbps: u32) -> Run {
            Run {
                enc: Encoder::new(&EncoderConfig {
                    width: W as u32,
                    height: H as u32,
                    fps: 60,
                    preset: "ultrafast".into(),
                    threads: 1,
                    crf,
                    max_kbps,
                })
                .unwrap(),
                motion: MotionQuality::default(),
                correct,
                crf,
                max_kbps,
                applied: crf,
                y: vec![0; W * H],
                n: 0,
            }
        }

        /// Encodes the next frame of `scene` as a motion frame, or the current picture
        /// again as a paint-over at `paintover_crf`.
        fn frame(&mut self, scene: Scene, paintover_crf: Option<f32>) -> Frame {
            let motion = paintover_crf.is_none();
            if motion {
                draw(&mut self.y, scene, self.n);
            }
            let c = match paintover_crf {
                Some(c) => c,
                None if self.correct => self.motion.crf(self.crf),
                None => self.crf,
            };
            if c != self.applied {
                self.enc.set_rate(c, self.max_kbps);
                self.applied = c;
            }
            let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
            let planes = Planes {
                y: &self.y,
                u: &u,
                v: &v,
                stride_y: W,
                stride_uv: W / 2,
            };
            let mut out = Vec::new();
            let e = self
                .enc
                .encode(
                    &planes,
                    self.n as i64 * 16,
                    self.n == 0,
                    None,
                    None,
                    &mut out,
                )
                .unwrap();
            let mbs = W / 16 * H / 16;
            let size = motion.then_some(MotionSize {
                bits: out.len() * 8,
                changed: mbs,
                mbs,
                crf: self.crf,
            });
            self.motion.note(&e, c, size);
            self.n += 1;
            Frame {
                crf: c,
                complexity: self.motion.complexity,
                e,
            }
        }

        /// 40 motion frames of `scene`; returns the last 30, once rate control settled.
        fn motion(mut self, scene: Scene) -> Vec<Frame> {
            (0..40).map(|_| self.frame(scene, None)).skip(10).collect()
        }
    }

    fn mean(v: impl Iterator<Item = f32>) -> f32 {
        let (sum, n) = v.fold((0.0, 0), |(s, n), x| (s + x, n + 1));
        sum / n as f32
    }

    fn qp(f: &Frame) -> f32 {
        f.e.qp.unwrap()
    }

    /// Bug 2: a forced keyframe under backlog clears the viewer's in-flight queue first,
    /// so the IDR replaces the backlog instead of stacking the largest frame on a full
    /// queue. A viewer within the flow bound is left untouched.
    #[test]
    fn forced_keyframe_clears_backlogged_inflight() {
        let hub = Hub::new();
        let interval_ms = 1000.0 / 30.0;

        // A viewer buried well over the flow bound (default 40 ms RTT).
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let behind = hub.add_client(Some("behind".into()), tx);
        {
            let mut q = behind.inflight.lock();
            for id in 0..50u32 {
                q.push_back(Inflight {
                    id,
                    sent: Instant::now() - Duration::from_millis(500),
                    bytes: 4000,
                });
            }
        }
        assert!(over_flow_bound(&behind, interval_ms));
        assert!(!flow_ok(&hub, interval_ms), "a buried viewer should fail flow_ok");

        // A viewer comfortably within the bound.
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        let ok = hub.add_client(Some("ok".into()), tx2);
        ok.inflight.lock().push_back(Inflight {
            id: 0,
            sent: Instant::now(),
            bytes: 100,
        });
        assert!(!over_flow_bound(&ok, interval_ms));

        let clients = vec![behind.clone(), ok.clone()];
        let cleared = clear_backlogged_inflight(&clients, interval_ms);
        assert_eq!(cleared, 1, "only the backlogged viewer should be cleared");
        assert_eq!(
            behind.backlog().0,
            0,
            "backlog not cleared before the forced keyframe"
        );
        assert_eq!(ok.backlog().0, 1, "an in-bound viewer must be left alone");
    }

    #[test]
    fn dragged_window_stays_at_crf() {
        let crf = 24.0;
        let plain = mean(Run::new(false, crf, 0).motion(Scene::Drag).iter().map(qp));
        let corrected = mean(Run::new(true, crf, 0).motion(Scene::Drag).iter().map(qp));
        // The scene has to show the problem for the check below to mean anything.
        assert!(
            plain > crf + 3.0,
            "x264 coded the drag at QP {plain:.1} without the correction (CRF {crf})"
        );
        assert!(
            corrected < crf + 1.0,
            "drag frames at QP {corrected:.1} for CRF {crf}"
        );
    }

    /// Under a bitrate cap VBV raises rows within a frame, which the frame's starting QP
    /// doesn't show; the estimate must not take that for a simple frame and drop the
    /// correction.
    #[test]
    fn capped_drag_keeps_its_estimate() {
        let crf = 24.0;
        // Without a cap the complexity is exactly what x264 added to the QP.
        let plain = Run::new(false, crf, 0).motion(Scene::Drag);
        let complexity = mean(plain.iter().map(|f| qp(f) - f.crf));
        let capped = Run::new(true, crf, 20_000).motion(Scene::Drag);
        let start_qp_says = capped
            .iter()
            .map(|f| qp(f) - f.e.crf_avg)
            .fold(f32::MAX, f32::min);
        assert!(
            start_qp_says < complexity - 4.0,
            "VBV never raised rows enough to matter (lowest {start_qp_says:.1}, complexity {complexity:.1})"
        );
        let lowest = capped.iter().map(|f| f.complexity).fold(f32::MAX, f32::min);
        assert!(
            lowest > complexity - 3.0,
            "estimate fell to {lowest:.1} under the cap, complexity {complexity:.1}"
        );
    }

    /// Content that really is complex keeps x264's QP: lowering it would only spend bits.
    #[test]
    fn complex_content_is_left_alone() {
        let crf = 24.0;
        let plain = mean(Run::new(false, crf, 0).motion(Scene::Noise).iter().map(qp));
        let corrected = Run::new(true, crf, 0).motion(Scene::Noise);
        assert!(
            plain > crf + 3.0,
            "x264 didn't raise the QP for noise ({plain:.1}, CRF {crf})"
        );
        let lowered = corrected.iter().filter(|f| f.crf != crf).count();
        assert_eq!(lowered, 0, "{lowered} noise frames got a lowered CRF");
    }

    /// Once the screen goes still the paint-overs show x264 forgetting the drag, so the
    /// next motion frame isn't handed the drag's correction.
    #[test]
    fn paintovers_wind_the_correction_down() {
        let crf = 24.0;
        let mut run = Run::new(true, crf, 0);
        for _ in 0..40 {
            run.frame(Scene::Drag, None);
        }
        let during = crf - run.motion.crf(crf);
        for _ in 0..3 {
            run.frame(Scene::Drag, Some(16.0));
        }
        let after = crf - run.motion.crf(crf);
        assert!(
            after <= during / 2.0,
            "correction {after} after the paint-overs, {during} during the drag"
        );
    }
}
