// Derived from voxl (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo,
// under the MIT license: see LICENSE-voxl.

//! The pipeline: damage driven capture, I420, x264, viewers, with flow
//! control and bitrate from their acks.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;

use crate::capture::{Band, Capturer};
use crate::convert::I420;
use crate::encoder::Encoder;
use crate::hub::{wait_any, Client, Hub, Inflight, Out, Snapshot};
use crate::input::InputEvent;

#[derive(Clone)]
pub struct VideoConfig {
    pub fps: u32,
    pub max_kbps: u32,
    pub min_kbps: u32,
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    /// Quality of the pass that sharpens a screen gone still.
    pub paintover_crf: f32,
    pub paintover_burst: u32,
    pub paintover_delay: Duration,
}

/// Size of the header in front of each access unit.
pub const HEADER: usize = 16;

/// Bytes a viewer may have waiting to be written before it is resynced
/// with a keyframe, for viewers that do not ack.
const MAX_QUEUED: usize = 8 << 20;

/// Frames kept for acks of one viewer.
const MAX_INFLIGHT: usize = 600;

struct Job {
    buf: Box<I420>,
    idx: usize,
    force_idr: bool,
    paintover: bool,
    /// Rows changed in this capture.
    band: Option<Band>,
    /// Macroblocks to encode; None for all of them.
    mb_dirty: Option<Vec<u8>>,
    captured_at: Instant,
}

/// Opens the display and the encoder and starts capture. The hub it
/// returns is how the server reaches the stream.
pub fn start(display: &str, cfg: VideoConfig, input: mpsc::Sender<InputEvent>) -> Result<Arc<Hub>> {
    let cap = Capturer::new(display)?;
    let (w, h) = (cap.width, cap.height);
    let enc = Encoder::new(w, h, cfg.fps, cfg.threads, cfg.crf, cfg.max_kbps)?;
    let hub = Hub::new(w, h, enc.codec()?, input)?;
    eprintln!(
        "shuru-screen: {w}x{h} at {} fps, {}, {} encoder thread(s)",
        cfg.fps, hub.codec, cfg.threads
    );
    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(1);
    let (ret_tx, ret_rx) = mpsc::channel::<(usize, Box<I420>)>();
    let (h2, c2) = (hub.clone(), cfg.clone());
    std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || encode_loop(enc, job_rx, ret_tx, h2, c2))?;
    let h3 = hub.clone();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut cap = cap;
            let e = capture_loop(&mut cap, cfg, h3, job_tx, ret_rx).unwrap_err();
            eprintln!("shuru-screen: capture stopped: {e:#}");
            std::process::exit(1);
        })?;
    Ok(hub)
}

fn flow_ok(clients: &[Arc<Client>], interval_ms: f64) -> bool {
    for c in clients {
        if !c.acks.load(Ordering::Relaxed) {
            continue;
        }
        let (n, age) = c.backlog();
        if n == 0 {
            continue;
        }
        let rtt = c.rtt_ms();
        // Enough frames in flight to cover a round trip and decode, never
        // more than about rtt + 120 ms of queue.
        let max_n = (((rtt + 40.0) / interval_ms).ceil() as usize + 2).clamp(3, 40);
        let max_age = rtt as u128 + 120;
        if age > 4000 {
            // A stalled viewer (hidden tab, dead socket) stops holding
            // everyone back.
            c.resync();
            continue;
        }
        if n >= max_n || age > max_age {
            return false;
        }
    }
    true
}

/// Delay based bitrate: back off toward the delivered rate when acks show
/// queueing, probe upward slowly while the path is clear.
struct RateCtl {
    target: f64,
    max: f64,
    min: f64,
    last_eval: Instant,
    last_dec: Instant,
    last_inc: Instant,
    /// Ramp up multiplicatively until the first congestion.
    slow_start: bool,
    /// Delivered rate at the last congestion: the likely capacity.
    ceiling: f64,
    ceiling_at: Instant,
}

const START_KBPS: f64 = 8000.0;

impl RateCtl {
    fn new(max: f64, min: f64) -> Self {
        let past = Instant::now() - Duration::from_secs(10);
        RateCtl {
            target: START_KBPS.min(max).max(min),
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

    fn update(&mut self, hub: &Hub, clients: &[Arc<Client>]) {
        if self.last_eval.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_eval = Instant::now();
        let mut congested = false;
        let mut clear = true;
        let mut rate = f64::MAX;
        let mut delivered = 0f64;
        for c in clients {
            if !c.started.load(Ordering::Relaxed) || !c.acks.load(Ordering::Relaxed) {
                continue;
            }
            let q = c.delay.lock().unwrap().queue_ms;
            let (_, age) = c.backlog();
            let d = c.delivered.lock().unwrap().rate_bps / 1000.0;
            if q > 30.0 || age as f64 > c.rtt_ms() + 80.0 {
                congested = true;
                if d > 0.0 {
                    rate = rate.min(d);
                }
            }
            if q > 15.0 {
                clear = false;
            }
            delivered = delivered.max(d);
        }
        let now = Instant::now();
        if congested && self.last_dec.elapsed() >= Duration::from_millis(300) {
            let mut t = self.target * 0.85;
            // The delivered rate only tells the capacity when we pushed near
            // the target; one big frame on an idle screen does not.
            if rate.is_finite() && rate >= self.target * 0.5 {
                self.ceiling = rate;
                self.ceiling_at = now;
                t = t.min(rate * 0.85);
            }
            self.target = t.max(self.min);
            self.last_dec = now;
            self.slow_start = false;
        } else if self.slow_start
            && !congested
            && clear
            && self.last_inc.elapsed() >= Duration::from_millis(250)
        {
            if delivered >= self.target * 0.5 {
                self.target = (self.target * 1.5).min(self.max);
                self.last_inc = now;
            }
        } else if !congested
            && clear
            && self.last_dec.elapsed() >= Duration::from_secs(1)
            && delivered >= self.target * 0.5
        {
            // Raise the cap only while the content uses it, and forget a
            // capacity not confirmed lately.
            if self.ceiling_at.elapsed() >= Duration::from_secs(15) {
                self.ceiling = f64::MAX;
            }
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

/// Regional sharpening. Blocks sent while they moved carry motion quality;
/// once a block has been still a while it is sent again at paint-over
/// quality, even while other parts of the screen keep moving.
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

    fn band_of(&self, map: &[u8]) -> Option<Band> {
        let (mut r0, mut r1) = (usize::MAX, 0);
        for (i, _) in map.iter().enumerate().filter(|(_, &m)| m != 0) {
            r0 = r0.min(i / self.cols);
            r1 = r1.max(i / self.cols + 1);
        }
        (r1 > r0).then_some(Band {
            y0: (r0 * 16) as u32,
            y1: (r1 * 16) as u32,
        })
    }

    /// Every block not sharpened yet (1 to encode) and the rows they span.
    fn pending_map(&self) -> Option<(Vec<u8>, Band)> {
        let map: Vec<u8> = self.pending.iter().map(|&p| p as u8).collect();
        let band = self.band_of(&map)?;
        Some((map, band))
    }

    /// Blocks due for sharpening (1 to refine) and the rows they span.
    fn due(&mut self) -> Option<(Vec<u8>, Band)> {
        if self.last_refine.elapsed() < REFINE_EVERY {
            return None;
        }
        let now = self.now();
        let mut map = vec![0u8; self.pending.len()];
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i] && now.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS {
                *m = 1;
                self.pending[i] = false;
            }
        }
        let band = self.band_of(&map)?;
        self.last_refine = Instant::now();
        Some((map, band))
    }
}

fn answer_snapshots(cap: &mut Capturer, hub: &Hub) -> Result<()> {
    let waiting = std::mem::take(&mut *hub.snapshots.lock().unwrap());
    if waiting.is_empty() {
        return Ok(());
    }
    // Damage stays pending, so the stream still sends what changed. The
    // PNG is made off this thread.
    let shot = Arc::new(Snapshot {
        width: cap.width as usize,
        height: cap.height as usize,
        bgrx: cap.snapshot()?,
    });
    for tx in waiting {
        let _ = tx.send(shot.clone());
    }
    Ok(())
}

fn capture_loop(
    cap: &mut Capturer,
    cfg: VideoConfig,
    hub: Arc<Hub>,
    job_tx: mpsc::SyncSender<Job>,
    ret_rx: mpsc::Receiver<(usize, Box<I420>)>,
) -> Result<()> {
    let (w, h) = (cap.width as usize, cap.height as usize);
    // Two frames: one converts while the encoder has the other. Each keeps
    // the rows it has not been brought up to date with.
    let mut pool: Vec<Option<Box<I420>>> = vec![
        Some(Box::new(I420::new(w, h))),
        Some(Box::new(I420::new(w, h))),
    ];
    let mut stale: [Option<Band>; 2] = [Some(cap.full()), Some(cap.full())];
    let mut next_idx = 0usize;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    // Allow a slightly early capture, so jitter in the source does not
    // alias into dropped frames.
    let min_interval = interval.mul_f64(0.85);
    let interval_ms = interval.as_secs_f64() * 1000.0;
    let mut last_capture = Instant::now() - interval;
    let mut last_motion = Instant::now();
    let mut paintover_left = 0u32;
    let mut rc = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
    hub.target_kbps.store(rc.target as u32, Ordering::Relaxed);
    let mut had_clients = false;
    let mut full_next = true;
    let mut refiner = Refiner::new(cap.mb_cols() * h.div_ceil(16), cap.mb_cols());

    loop {
        cap.pump()?;
        hub.waker.drain();
        answer_snapshots(cap, &hub)?;
        while let Ok((i, b)) = ret_rx.try_recv() {
            pool[i] = Some(b);
        }
        let clients = hub.clients();
        if clients.is_empty() {
            if had_clients {
                rc = RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64);
                hub.target_kbps.store(rc.target as u32, Ordering::Relaxed);
            }
            had_clients = false;
            wait_any(&[cap.fd(), hub.waker.fd()], 500);
            continue;
        }
        rc.update(&hub, &clients);
        let want_key = clients.iter().any(|c| {
            c.needs_keyframe.load(Ordering::Relaxed) && c.queued() < MAX_QUEUED / 2
        });
        if !had_clients {
            had_clients = true;
            // Our copy may be stale: take everything once.
            cap.grab(cap.full())?;
            stale = [Some(cap.full()), Some(cap.full())];
            cap.damaged = true;
            full_next = true;
        }
        let paintover_due = paintover_left > 0 && last_motion.elapsed() >= cfg.paintover_delay;
        if !cap.damaged && !want_key && !paintover_due {
            let timeout = if paintover_left == 0 {
                100
            } else {
                cfg.paintover_delay
                    .saturating_sub(last_motion.elapsed())
                    .as_millis() as i32
                    + 1
            };
            wait_any(&[cap.fd(), hub.waker.fd()], timeout);
            continue;
        }
        let since = last_capture.elapsed();
        if since < min_interval {
            let rem = min_interval - since;
            wait_any(&[hub.waker.fd()], (rem.as_micros() as i32 + 999) / 1000);
            continue;
        }
        if !want_key && !flow_ok(&clients, interval_ms) {
            wait_any(&[cap.fd(), hub.waker.fd()], 4);
            continue;
        }
        // The encoder may still hold the frame we want.
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
        let mut mb_dirty = (!full_next).then(|| cap.take_mb_map());
        if full_next {
            cap.take_mb_map();
            full_next = false;
        }
        match &mb_dirty {
            Some(m) => refiner.mark(m),
            None => refiner.mark_all(),
        }
        let mut band = None;
        if cap.damaged {
            band = cap.take_damage();
            if let Some(b) = band {
                cap.grab(b)?;
            }
        }
        let is_motion = band.is_some();
        if !is_motion && !want_key && !paintover_due {
            continue;
        }
        // Sharpen only what went out at motion quality.
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
        let mut buf = pool[idx]
            .take()
            .ok_or_else(|| anyhow!("no frame to convert into"))?;
        let todo = match (stale[idx], band) {
            (p, Some(b)) => Some(Band::union(p, b)),
            (p, None) => p,
        };
        if let Some(b) = band {
            stale[1 - idx] = Some(Band::union(stale[1 - idx], b));
        }
        stale[idx] = None;
        if let Some(t) = todo {
            cap.convert(&mut buf, t, cfg.convert_threads);
        }

        let mut refine = None;
        let paintover = if is_motion {
            last_motion = Instant::now();
            paintover_left = cfg.paintover_burst;
            // Sharpen blocks gone still on this frame too.
            refine = if want_key { None } else { refiner.due() };
            refine.is_some()
        } else if paintover_due {
            paintover_left -= 1;
            true
        } else {
            false
        };
        if want_key {
            for c in clients.iter().filter(|c| c.queued() < MAX_QUEUED / 2) {
                c.needs_keyframe.store(false, Ordering::Relaxed);
            }
            // A keyframe is squeezed into the cap; sharpen all of it once still.
            refiner.mark_all();
        }
        let frame_paintover = paintover && refine.is_none();
        if frame_paintover && paintover_left == 0 {
            refiner.clear_all();
        }
        let mb_map = if want_key {
            None
        } else if frame_paintover {
            paint_map.map(|(m, b)| {
                band = Some(Band::union(band, b));
                m
            })
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(Band::union(band, rband));
            let mut m = mb_dirty.take().unwrap_or_else(|| vec![1; rmap.len()]);
            for (d, r) in m.iter_mut().zip(rmap) {
                *d |= r;
            }
            Some(m)
        } else {
            mb_dirty.take()
        };
        last_capture = t0;
        next_idx = 1 - idx;
        let job = Job {
            buf,
            idx,
            force_idr: want_key,
            paintover,
            mb_dirty: mb_map,
            band,
            captured_at: origin,
        };
        if job_tx.send(job).is_err() {
            return Err(anyhow!("the encoder stopped"));
        }
    }
}

fn encode_loop(
    mut enc: Encoder,
    rx: mpsc::Receiver<Job>,
    ret: mpsc::Sender<(usize, Box<I420>)>,
    hub: Arc<Hub>,
    cfg: VideoConfig,
) {
    let epoch = Instant::now();
    let mut frame_id: u32 = 0;
    let mut last_ts = 0u64;
    let (mut applied_kbps, mut applied_crf) = (0u32, -1f32);
    while let Ok(job) = rx.recv() {
        if !job.force_idr && !job.paintover && job.band.is_none() {
            let _ = ret.send((job.idx, job.buf));
            continue;
        }
        let target = hub.target_kbps.load(Ordering::Relaxed);
        let crf = if job.paintover { cfg.paintover_crf } else { cfg.crf };
        let cap = if job.paintover {
            target.saturating_mul(2)
        } else {
            target
        };
        if job.force_idr {
            // A keyframe is what a new viewer sees first: up to about 8
            // frames of buffer at 4x the cap.
            let boost = cap.saturating_mul(4).max(16_000);
            enc.set_rate_buf(crf, boost, boost / cfg.fps.max(1) * 8);
            applied_kbps = 0;
        } else {
            let moved = applied_kbps == 0
                || (cap as f64 - applied_kbps as f64).abs() > applied_kbps as f64 * 0.04;
            if crf != applied_crf || moved {
                enc.set_rate(crf, cap);
                applied_crf = crf;
                applied_kbps = cap;
            }
        }
        // x264 wants 1 on macroblocks that did not change.
        let mut info = job
            .mb_dirty
            .as_ref()
            .map(|d| d.iter().map(|&x| (x == 0) as u8).collect::<Vec<u8>>());
        let mut out = Vec::with_capacity(64 << 10);
        out.resize(HEADER, 0);
        let ts_ms = job.captured_at.duration_since(epoch).as_millis() as i64;
        let r = enc.encode(&job.buf, ts_ms, job.force_idr, info.as_deref_mut(), &mut out);
        let _ = ret.send((job.idx, job.buf));
        let key = match r {
            Ok(key) if out.len() > HEADER => key,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("shuru-screen: {e:#}");
                continue;
            }
        };
        frame_id = frame_id.wrapping_add(1);
        let ts_us = (job.captured_at.duration_since(epoch).as_micros() as u64).max(last_ts + 1);
        last_ts = ts_us;
        out[0] = 1;
        out[1] = key as u8 | (job.paintover as u8) << 1;
        out[4..8].copy_from_slice(&frame_id.to_be_bytes());
        out[8..16].copy_from_slice(&ts_us.to_be_bytes());
        let size = out.len();
        let msg = Bytes::from(out);
        let now = Instant::now();
        for c in hub.clients() {
            if c.queued() > MAX_QUEUED {
                if c.started.load(Ordering::Relaxed) {
                    c.resync();
                }
                continue;
            }
            if !c.started.load(Ordering::Relaxed) {
                if !key {
                    continue;
                }
                c.started.store(true, Ordering::Relaxed);
                c.inflight.lock().unwrap().clear();
            }
            {
                let mut q = c.inflight.lock().unwrap();
                if q.len() >= MAX_INFLIGHT {
                    q.pop_front();
                }
                q.push_back(Inflight {
                    id: frame_id,
                    sent: now,
                    bytes: size,
                });
            }
            c.send(Out::Bin(msg.clone()));
        }
    }
}
