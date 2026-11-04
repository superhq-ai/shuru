// Derived from voxl (https://github.com/tembo/voxl), Copyright (c) 2026 Tembo,
// under the MIT license: see LICENSE-voxl.

//! State shared by the capture and encode threads and the viewers.

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::input::InputEvent;

pub enum Out {
    Bin(Bytes),
    Text(String),
}

impl Out {
    pub fn len(&self) -> usize {
        match self {
            Out::Bin(b) => b.len(),
            Out::Text(t) => t.len(),
        }
    }
}

pub struct Inflight {
    pub id: u32,
    pub sent: Instant,
    pub bytes: usize,
}

/// One open stream.
pub struct Client {
    pub id: u64,
    pub viewer: Option<String>,
    tx: mpsc::UnboundedSender<Out>,
    /// Bytes handed to the writer and not yet on the socket.
    queued: Arc<AtomicUsize>,
    /// Frames sent and not yet acknowledged.
    pub inflight: Mutex<VecDeque<Inflight>>,
    /// Has acknowledged a frame: only such clients steer flow and bitrate.
    pub acks: AtomicBool,
    pub delivered: Mutex<RateEst>,
    pub delay: Mutex<DelayEst>,
    pub needs_keyframe: AtomicBool,
    /// Has had a keyframe since the stream (re)started for it.
    pub started: AtomicBool,
}

/// Send to ack delay: `base_ms` is a slowly expiring minimum, the excess
/// over it is queueing on the path or in the decoder.
pub struct DelayEst {
    pub base_ms: f64,
    pub base_at: Instant,
    pub queue_ms: f64,
}

pub struct RateEst {
    pub window_start: Instant,
    pub bytes: usize,
    pub rate_bps: f64,
}

impl Client {
    pub fn send(&self, m: Out) {
        self.queued.fetch_add(m.len(), Ordering::Relaxed);
        let _ = self.tx.send(m);
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub fn resync(&self) {
        self.inflight.lock().unwrap().clear();
        self.started.store(false, Ordering::Relaxed);
        self.needs_keyframe.store(true, Ordering::Relaxed);
    }

    pub fn on_ack(&self, id: u32) {
        self.acks.store(true, Ordering::Relaxed);
        let mut q = self.inflight.lock().unwrap();
        let mut acked = 0;
        let mut sample = None;
        while let Some(f) = q.front() {
            if !wrapping_le(f.id, id) {
                break;
            }
            acked += f.bytes;
            if f.id == id {
                sample = Some(f.sent.elapsed().as_secs_f64() * 1000.0);
            }
            q.pop_front();
        }
        drop(q);
        if let Some(ms) = sample {
            let mut e = self.delay.lock().unwrap();
            if ms < e.base_ms || e.base_at.elapsed().as_secs() >= 10 {
                e.base_ms = ms;
                e.base_at = Instant::now();
            }
            let qd = ms - e.base_ms;
            e.queue_ms = e.queue_ms * 0.7 + qd * 0.3;
        }
        let mut d = self.delivered.lock().unwrap();
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

    /// Frames in flight and the age of the oldest, in ms.
    pub fn backlog(&self) -> (usize, u128) {
        let q = self.inflight.lock().unwrap();
        (
            q.len(),
            q.front().map(|f| f.sent.elapsed().as_millis()).unwrap_or(0),
        )
    }

    /// The smallest send to ack time seen lately, or 40 ms before any.
    pub fn rtt_ms(&self) -> f64 {
        let base = self.delay.lock().unwrap().base_ms;
        if base.is_finite() {
            base.max(1.0)
        } else {
            40.0
        }
    }
}

fn wrapping_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

/// Wakes the capture thread out of `wait_any`.
pub struct Wake {
    r: OwnedFd,
    w: OwnedFd,
}

impl Wake {
    fn new() -> std::io::Result<Self> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe {
            Wake {
                r: OwnedFd::from_raw_fd(fds[0]),
                w: OwnedFd::from_raw_fd(fds[1]),
            }
        })
    }

    pub fn signal(&self) {
        use std::os::fd::AsRawFd;
        let _ = unsafe { libc::write(self.w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    pub fn drain(&self) {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.r.as_fd()
    }
}

/// Blocks until one of `fds` is readable or `timeout_ms` passes.
pub fn wait_any(fds: &[BorrowedFd], timeout_ms: i32) {
    use std::os::fd::AsRawFd;
    let mut p: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
}

/// Who may send input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Shut,
    Agent,
    Viewer(String),
}

/// The screen as captured: BGRX, rows packed.
pub struct Snapshot {
    pub width: usize,
    pub height: usize,
    pub bgrx: Vec<u8>,
}

pub struct Hub {
    clients: Mutex<Vec<Arc<Client>>>,
    next_id: AtomicU64,
    pub width: u32,
    pub height: u32,
    pub codec: String,
    /// The bitrate cap the encoder applies, kbit/s.
    pub target_kbps: AtomicU32,
    pub waker: Wake,
    pub input: std_mpsc::Sender<InputEvent>,
    pub gate: Mutex<Gate>,
    /// Stills asked for, answered by the capture thread.
    pub snapshots: Mutex<Vec<oneshot::Sender<Arc<Snapshot>>>>,
}

impl Hub {
    pub fn new(
        width: u32,
        height: u32,
        codec: String,
        input: std_mpsc::Sender<InputEvent>,
    ) -> std::io::Result<Arc<Self>> {
        Ok(Arc::new(Hub {
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            width,
            height,
            codec,
            target_kbps: AtomicU32::new(0),
            waker: Wake::new()?,
            input,
            gate: Mutex::new(Gate::Shut),
            snapshots: Mutex::new(Vec::new()),
        }))
    }

    pub fn wake(&self) {
        self.waker.signal();
    }

    /// A new stream; `queued` is shared with its writer.
    pub fn add_client(
        &self,
        viewer: Option<String>,
        tx: mpsc::UnboundedSender<Out>,
        queued: Arc<AtomicUsize>,
    ) -> Arc<Client> {
        let c = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            viewer,
            tx,
            queued,
            inflight: Mutex::new(VecDeque::new()),
            acks: AtomicBool::new(false),
            delivered: Mutex::new(RateEst {
                window_start: Instant::now(),
                bytes: 0,
                rate_bps: 0.0,
            }),
            delay: Mutex::new(DelayEst {
                base_ms: f64::INFINITY,
                base_at: Instant::now(),
                queue_ms: 0.0,
            }),
            needs_keyframe: AtomicBool::new(true),
            started: AtomicBool::new(false),
        });
        self.clients.lock().unwrap().push(c.clone());
        self.wake();
        c
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().unwrap().retain(|c| c.id != id);
        self.wake();
    }

    pub fn clients(&self) -> Vec<Arc<Client>> {
        self.clients.lock().unwrap().clone()
    }

    pub fn hello(&self) -> String {
        serde_json::json!({
            "type": "hello",
            "codec": self.codec,
            "format": "annexb",
            "width": self.width,
            "height": self.height,
        })
        .to_string()
    }

    /// Whether input from `viewer` lands now.
    pub fn may_input(&self, viewer: Option<&str>) -> bool {
        match (&*self.gate.lock().unwrap(), viewer) {
            (Gate::Viewer(open), Some(v)) => open == v,
            _ => false,
        }
    }

    pub fn gate_message(&self, viewer: Option<&str>) -> String {
        serde_json::json!({"type": "gate", "input": self.may_input(viewer)}).to_string()
    }

    /// Opens the gate to someone else, or shuts it. Keys and buttons held
    /// by whoever had it are let go.
    pub fn set_gate(&self, gate: Gate) {
        {
            let mut g = self.gate.lock().unwrap();
            if *g == gate {
                return;
            }
            *g = gate;
        }
        let _ = self.input.send(InputEvent::ReleaseAll);
        for c in self.clients() {
            c.send(Out::Text(self.gate_message(c.viewer.as_deref())));
        }
    }
}
