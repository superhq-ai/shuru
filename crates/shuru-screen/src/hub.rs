// Vendored from voxl 0.3.5 (https://github.com/tembo/voxl), Copyright (c)
// 2026 Tembo, under the MIT license: see LICENSE-voxl. Pinned to v0.3.5.
// Stripped to the Linux streaming core: voxl's macOS display switching,
// dynamic resize, audio and clipboard state are dropped, the per-task gate
// is extracted into gate.rs, and a thin layer (our cursor/pointer greeting
// and JSON hello) sits over voxl's ack-based flow control unchanged.

//! Shared state between the video producer and the websocket clients: voxl's
//! ack-based `Client` flow control, plus shuru's cursor/pointer greeting.

use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::cursor::CursorImage;
use crate::still::Snapshot;

pub enum Out {
    Bin(Bytes),
    Text(String),
}

pub struct Inflight {
    pub id: u32,
    pub sent: Instant,
    pub bytes: usize,
}

pub struct Client {
    pub id: u64,
    /// The viewer id this stream carries (`/stream?viewer=`), for the gate.
    pub viewer: Option<String>,
    pub tx: mpsc::UnboundedSender<Out>,
    /// Whether this client is watching the video (all shuru clients are).
    pub video: AtomicBool,
    pub inflight: Mutex<VecDeque<Inflight>>,
    /// Slow-rising minimum RTT (ms), derived from ack timing in `on_ack` (0 until the
    /// first ack: flow uses a 40 ms default until then).
    pub min_rtt_ms: AtomicU32,
    /// Exponentially-weighted estimate of delivered bytes/sec, from acks.
    pub delivered: Mutex<RateEst>,
    pub needs_keyframe: AtomicBool,
    /// Has received a keyframe since (re)starting video.
    pub started: AtomicBool,
    pub delay: Mutex<DelayEst>,
}

/// Send->ack delay tracking: `base` is the (slowly expiring) minimum, the excess over
/// it is queueing somewhere on the path (network buffers or a slow decoder).
pub struct DelayEst {
    pub base_ms: f64,
    pub base_at: Instant,
    pub queue_ms: f64,
    /// Slow-rising minimum send->ack time (ms): a lower bound on path RTT, fed to
    /// `Client::min_rtt_ms` so `flow_ok`/`RateCtl` use a real estimate instead of the
    /// hardcoded 40 ms default (the resync build sends no ping). `f64::MAX` until the
    /// first ack.
    pub rtt_min_ms: f64,
}

/// A frame at or below this size may refresh the clean-path baseline once it has gone
/// stale; a larger one may not.
///
/// UPSTREAM-VOXL CANDIDATE (voxl 0.3.5 `hub.rs` bug 1): voxl refreshes `base_ms` from
/// *any* sample once `base_at` is >= 10 s old. After an idle moment the first change is a
/// large frame whose ack time is mostly serialization, so adopting it as the baseline
/// makes `queue_ms = ms - base_ms` collapse to ~0 and blinds `RateCtl`'s `q > 30`
/// congestion test for up to 10 s. Gating the staleness refresh to small frames (a
/// genuinely lower sample always still refreshes, regardless of size) keeps the baseline
/// a clean-path minimum. See docs/findings/voxl-bugs.md #1.
const BASE_REFRESH_MAX_BYTES: usize = 8 * 1024;

impl DelayEst {
    /// Folds one send->ack sample (`ms`, for a frame of `bytes` bytes) into the delay and
    /// RTT estimates. Pure given the current state and the sample, so it is unit-tested
    /// directly. Returns the RTT estimate (ms, >= 1) to publish to `min_rtt_ms`.
    pub fn observe(&mut self, ms: f64, bytes: usize) -> u32 {
        // base_ms is the clean-path send->ack minimum; the excess over it is queueing.
        // Only a genuinely lower sample, or a *small* frame after the staleness window,
        // may move it (bug 1 fix: a big post-idle frame must not reset the baseline).
        if ms < self.base_ms
            || (self.base_at.elapsed().as_secs() >= 10 && bytes <= BASE_REFRESH_MAX_BYTES)
        {
            self.base_ms = ms;
            self.base_at = Instant::now();
        }
        let qd = ms - self.base_ms;
        self.queue_ms = self.queue_ms * 0.7 + qd * 0.3;
        // RTT estimate: the send->ack minimum is a path-RTT lower bound. Snap down to any
        // new low instantly; otherwise drift up at most 0.5 ms per ack (never past the
        // current sample), so a lucky low reading is not pinned forever yet one slow ack
        // cannot inflate the floor (an inflated RTT would loosen `flow_ok`'s queue bound).
        self.rtt_min_ms = if ms < self.rtt_min_ms {
            ms
        } else {
            (self.rtt_min_ms + 0.5).min(ms)
        };
        self.rtt_min_ms.round().clamp(1.0, 2000.0) as u32
    }
}

pub struct RateEst {
    pub window_start: Instant,
    pub bytes: usize,
    pub rate_bps: f64,
}

impl Client {
    pub fn send(&self, m: Out) {
        let _ = self.tx.send(m);
    }

    pub fn on_ack(&self, id: u32) {
        let mut q = self.inflight.lock();
        let mut acked = 0;
        let mut sample = None;
        while let Some(f) = q.front() {
            if wrapping_le(f.id, id) {
                acked += f.bytes;
                if f.id == id {
                    sample = Some((f.sent.elapsed().as_secs_f64() * 1000.0, f.bytes));
                }
                q.pop_front();
            } else {
                break;
            }
        }
        drop(q);
        if let Some((ms, bytes)) = sample {
            let rtt = self.delay.lock().observe(ms, bytes);
            // Feed the ack-derived RTT back so flow_ok/RateCtl stop defaulting to 40 ms.
            self.min_rtt_ms.store(rtt, Ordering::Relaxed);
        }
        let mut d = self.delivered.lock();
        d.bytes += acked;
        let el = d.window_start.elapsed().as_secs_f64();
        if el >= 0.5 {
            let r = d.bytes as f64 * 8.0 / el;
            d.rate_bps = if d.rate_bps == 0.0 {
                r
            } else {
                d.rate_bps * 0.6 + r * 0.4
            };
            d.bytes = 0;
            d.window_start = Instant::now();
        }
    }

    /// Frames in flight and age of the oldest unacknowledged one.
    pub fn backlog(&self) -> (usize, u128) {
        let q = self.inflight.lock();
        (
            q.len(),
            q.front().map(|f| f.sent.elapsed().as_millis()).unwrap_or(0),
        )
    }

    /// Ask for a fresh keyframe (a reconnecting or decoder-stuck viewer).
    pub fn resync(&self) {
        self.needs_keyframe.store(true, Ordering::Relaxed);
        self.started.store(false, Ordering::Relaxed);
        self.inflight.lock().clear();
    }
}

/// A non-blocking, close-on-exec pipe for waking a thread that polls file descriptors
/// (`pipe2` with those flags, which macOS lacks).
pub fn wake_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    let (r, w) = nix::unistd::pipe()?;
    for fd in [&r, &w] {
        fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok((r, w))
}

/// Something `wait_any` can wait for: a file descriptor on Unix.
pub type Waitable<'a> = std::os::fd::BorrowedFd<'a>;

/// Wakes a thread blocked in `wait_any`: a pipe on Unix.
pub struct Wake {
    r: std::os::fd::OwnedFd,
    w: std::os::fd::OwnedFd,
}

impl Wake {
    pub fn new() -> std::io::Result<Self> {
        let (r, w) = wake_pipe()?;
        Ok(Wake { r, w })
    }

    pub fn signal(&self) {
        let _ = nix::unistd::write(&self.w, &[1u8]);
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!(nix::unistd::read(&self.r, &mut buf), Ok(n) if n > 0) {}
    }

    pub fn waitable(&self) -> Waitable<'_> {
        std::os::fd::AsFd::as_fd(&self.r)
    }
}

/// Blocks until one of `w` is ready or `timeout_ms` passes (negative: no timeout).
pub fn wait_any(w: &[Waitable], timeout_ms: i32) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let mut p: Vec<PollFd> = w
        .iter()
        .map(|&fd| PollFd::new(fd, PollFlags::POLLIN))
        .collect();
    let _ = poll(
        &mut p,
        PollTimeout::try_from(timeout_ms).unwrap_or(PollTimeout::NONE),
    );
}

fn wrapping_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

#[derive(Default)]
pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub grab_us: AtomicU64,
    pub convert_us: AtomicU64,
    pub flow_blocked: AtomicU64,
    pub paintovers: AtomicU64,
    pub keyframes: AtomicU64,
    pub refines: AtomicU64,
    pub damage_wait_us: AtomicU64,
    pub send_lat_us: AtomicU64,
}

pub struct Hub {
    clients: Mutex<Vec<Arc<Client>>>,
    next_id: AtomicU64,
    /// A monotonic epoch for the millisecond clock the input-quiet hook shares.
    epoch: Instant,
    pub stats: Stats,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Encoded stripes as (y, height).
    pub stripes: Mutex<Vec<(u32, u32)>>,
    /// SHURU: the WebCodecs codec string (`avc1.PPCCLL`) the viewers decode with.
    pub codec: Mutex<String>,
    /// Adaptive bitrate cap applied by the encoder (kbit/s).
    pub target_kbps: AtomicU32,
    /// The last cursor message (shuru JSON), replayed to new clients.
    pub cursor: Mutex<Option<String>>,
    /// The last cursor image, for stills when the server refuses one.
    pub cursor_image: Mutex<Option<CursorImage>>,
    /// The last pointer position sent.
    pub pointer: Mutex<Option<(i32, i32)>>,
    /// Wakes the capture thread (acks, joins, keyframe requests).
    pub waker: Wake,
    pub input: Mutex<Option<std::sync::mpsc::Sender<crate::input::InputEvent>>>,
    /// Screenshot requests, answered by the capture thread with the current screen.
    pub snapshots: Mutex<Vec<tokio::sync::oneshot::Sender<Arc<Snapshot>>>>,
    /// Shallow integration hook (behind the hello `sharpenOnInputQuiet` capability):
    /// the last time input was applied, in epoch millis, so the video thread can start
    /// the paint-over countdown when the user goes quiet, not only when pixels settle.
    pub last_input_ms: AtomicU64,
    /// How many connected viewers negotiated the input-quiet sharpening hook.
    sharpen_subs: AtomicUsize,
    /// How many connected viewers negotiated the keyframe-on-lease hook.
    lease_subs: AtomicUsize,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Hub {
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            epoch: Instant::now(),
            stats: Stats::default(),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            target_kbps: AtomicU32::new(0),
            stripes: Mutex::new(Vec::new()),
            codec: Mutex::new(String::new()),
            cursor: Mutex::new(None),
            cursor_image: Mutex::new(None),
            pointer: Mutex::new(None),
            waker: Wake::new().expect("wake"),
            input: Mutex::new(None),
            snapshots: Mutex::new(Vec::new()),
            last_input_ms: AtomicU64::new(0),
            sharpen_subs: AtomicUsize::new(0),
            lease_subs: AtomicUsize::new(0),
        })
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    /// Milliseconds since this hub was made, the clock the input-quiet hook shares.
    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Records that input was just applied (for the input-quiet sharpening hook).
    pub fn note_input(&self) {
        self.last_input_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    /// Whether the input-quiet sharpening hook is active (a capable viewer is connected).
    pub fn sharpen_on_input_quiet(&self) -> bool {
        self.sharpen_subs.load(Ordering::Relaxed) > 0
    }

    /// Whether the keyframe-on-lease hook is active (a capable viewer is connected).
    pub fn keyframe_on_lease(&self) -> bool {
        self.lease_subs.load(Ordering::Relaxed) > 0
    }

    /// A viewer negotiated these hooks on connect; the counters undo on disconnect so a
    /// plain voxl viewer, which negotiates nothing, is never affected.
    pub fn add_caps(&self, sharpen: bool, lease: bool) {
        if sharpen {
            self.sharpen_subs.fetch_add(1, Ordering::Relaxed);
        }
        if lease {
            self.lease_subs.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn remove_caps(&self, sharpen: bool, lease: bool) {
        if sharpen {
            self.sharpen_subs.fetch_sub(1, Ordering::Relaxed);
        }
        if lease {
            self.lease_subs.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn drain_wake(&self) {
        self.waker.drain();
    }

    /// A new stream. It gets the hello, the cursor and the pointer before any frame;
    /// the server sends the gate state separately (the gate lives outside the hub).
    pub fn add_client(
        &self,
        viewer: Option<String>,
        tx: mpsc::UnboundedSender<Out>,
    ) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            viewer,
            tx,
            video: AtomicBool::new(true),
            inflight: Mutex::new(VecDeque::new()),
            min_rtt_ms: AtomicU32::new(0),
            delivered: Mutex::new(RateEst {
                window_start: Instant::now(),
                bytes: 0,
                rate_bps: 0.0,
            }),
            needs_keyframe: AtomicBool::new(true),
            started: AtomicBool::new(false),
            delay: Mutex::new(DelayEst {
                base_ms: f64::MAX,
                base_at: Instant::now(),
                queue_ms: 0.0,
                rtt_min_ms: f64::MAX,
            }),
        });
        // Under the client lock, so no broadcast slips in between.
        let mut clients = self.clients.lock();
        c.send(Out::Text(self.hello()));
        if let Some(m) = self.cursor.lock().clone() {
            c.send(Out::Text(m));
        }
        if let Some((x, y)) = *self.pointer.lock() {
            c.send(Out::Text(pointer_message(x, y)));
        }
        clients.push(c.clone());
        drop(clients);
        self.wake();
        c
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.wake();
    }

    pub fn clients(&self) -> Vec<Arc<Client>> {
        self.clients.lock().clone()
    }

    pub fn video_clients(&self) -> Vec<Arc<Client>> {
        self.clients
            .lock()
            .iter()
            .filter(|c| c.video.load(Ordering::Relaxed))
            .cloned()
            .collect()
    }

    /// Sends `m` to every stream.
    pub fn broadcast(&self, m: &str) {
        for c in self.clients.lock().iter() {
            c.send(Out::Text(m.to_string()));
        }
    }

    /// The `hello` message: the stream's geometry, codec and stripe layout.
    pub fn hello(&self) -> String {
        serde_json::json!({
            "type": "hello",
            "codec": self.codec.lock().clone(),
            "format": "annexb",
            "width": self.width.load(Ordering::Relaxed),
            "height": self.height.load(Ordering::Relaxed),
            "stripes": *self.stripes.lock(),
        })
        .to_string()
    }

    /// After the stream restarts (new size): every client needs the new geometry and
    /// a keyframe, and acks for the old stream's frames no longer apply.
    pub fn restart_clients(&self) {
        let hello = self.hello();
        for c in self.clients.lock().iter() {
            c.inflight.lock().clear();
            c.started.store(false, Ordering::Relaxed);
            c.needs_keyframe.store(true, Ordering::Relaxed);
            c.send(Out::Text(hello.clone()));
        }
    }
}

pub fn pointer_message(x: i32, y: i32) -> String {
    serde_json::json!({"type": "pointer", "x": x, "y": y}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn delay(base_ms: f64, base_age: Duration, queue_ms: f64) -> DelayEst {
        DelayEst {
            base_ms,
            base_at: Instant::now() - base_age,
            queue_ms,
            rtt_min_ms: base_ms,
        }
    }

    /// Fix 1: the RTT estimate tracks the send->ack minimum instead of defaulting to
    /// 40 ms, snaps down to any new low, and is not inflated by one slow ack.
    #[test]
    fn rtt_tracks_ack_minimum() {
        let mut e = DelayEst {
            base_ms: f64::MAX,
            base_at: Instant::now(),
            queue_ms: 0.0,
            rtt_min_ms: f64::MAX,
        };
        // First ack sets the floor.
        assert_eq!(e.observe(25.0, 1000), 25);
        // A much slower ack barely moves the floor (drifts up <= 0.5 ms).
        let r = e.observe(120.0, 1000);
        assert!(r <= 26, "one slow ack jumped the RTT floor to {r} ms");
        // Several more slow acks still only creep upward, nowhere near the sample.
        for _ in 0..3 {
            e.observe(120.0, 1000);
        }
        assert!(e.rtt_min_ms < 30.0, "RTT crept to {} ms", e.rtt_min_ms);
        // A lower ack snaps the floor straight down.
        assert_eq!(e.observe(11.0, 1000), 11);
    }

    /// Fix 2 / voxl bug 1: after the 10 s staleness window a *big* post-idle frame must
    /// not reset the baseline, or `queue_ms` collapses and the `q > 30` congestion test
    /// goes blind. A small frame (or any lower sample) may still refresh it.
    #[test]
    fn big_post_idle_frame_does_not_blind_congestion() {
        // Baseline set low on a clean path, now stale (> 10 s old).
        let mut e = delay(20.0, Duration::from_secs(11), 0.0);
        // The first post-idle change is a big frame that acks slow (serialization).
        e.observe(140.0, 64 * 1024);
        assert!(
            e.base_ms <= 20.0 + f64::EPSILON,
            "big frame reset base_ms to {} (baseline polluted)",
            e.base_ms
        );
        assert!(
            e.queue_ms > 30.0,
            "queue_ms collapsed to {} ms: congestion would be invisible",
            e.queue_ms
        );

        // A small frame after the same staleness window is still allowed to refresh.
        let mut small = delay(20.0, Duration::from_secs(11), 0.0);
        small.observe(35.0, 2000);
        assert!(
            (small.base_ms - 35.0).abs() < 1e-6,
            "small frame failed to refresh base_ms: {}",
            small.base_ms
        );
    }
}
