//! Idle park: a running sandbox nothing has used for `--idle-park` is
//! stopped as `stop` stops it. Its guest is in use while
//! - a request that uses it, or a connection upgraded to it, is open,
//! - an exec session runs, which only the guest knows of once no client
//!   is attached,
//! - the VM uses at least `BUSY_CPU` of CPU time in a second, which
//!   catches processes started outside a session,
//! - a connection the guest opened is still open and the guest sent on it
//!   within `WAIT_LIMIT`, or the remote sent on it within the idle time.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use shuru_proxy::FlowAge;

use super::exec;
use super::sandboxes::{Entry, Node};
use crate::vm;

const TICK: Duration = Duration::from_secs(1);
/// How often a guest that had a session running is asked again, and how
/// long it has to answer.
const SESSION_POLL: Duration = Duration::from_secs(5);
/// CPU time in a second above which a VM counts as in use. A quiet guest
/// and its VMM stay well below it.
const BUSY_CPU: Duration = Duration::from_millis(20);
/// How long an open connection counts as in use after the guest last sent
/// on it. Who sent last says nothing: TLS session tickets and HTTP/2
/// settings reach the guest after its request, so a guest waiting on a slow
/// answer looks answered. An idle keep-alive holds a sandbox up to this.
const WAIT_LIMIT: Duration = Duration::from_secs(10 * 60);

pub struct Activity {
    state: Mutex<State>,
}

struct State {
    /// Requests and connections open.
    busy: usize,
    /// When the last of them ended, the VM booted, or the guest was last
    /// seen in use.
    idle_since: Instant,
    /// The last look at the guest found a session running or a connection
    /// in use.
    working: bool,
    checking: bool,
    /// The CPU time of the VM at the last tick, and when it was read.
    cpu: Option<(Instant, u64)>,
}

impl Default for Activity {
    fn default() -> Self {
        Activity {
            state: Mutex::new(State {
                busy: 0,
                idle_since: Instant::now(),
                working: false,
                checking: false,
                cpu: None,
            }),
        }
    }
}

impl Activity {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts as activity until dropped.
    pub fn busy(self: &Arc<Self>) -> Busy {
        self.state().busy += 1;
        Busy(self.clone())
    }

    /// Starts the clock again, as a boot does.
    pub fn restart(&self) {
        let mut state = self.state();
        state.idle_since = Instant::now();
        state.working = false;
        state.cpu = None;
    }

    /// Takes the CPU time of the VM, counting the time since the last
    /// reading as use if it spent at least `BUSY_CPU` a second.
    pub fn saw_cpu(&self, cpu_usec: u64, now: Instant) {
        let mut state = self.state();
        if let Some((at, before)) = state.cpu.replace((now, cpu_usec)) {
            if busy_cpu(
                cpu_usec.saturating_sub(before),
                now.saturating_duration_since(at),
            ) {
                state.idle_since = state.idle_since.max(now);
            }
        }
    }

    /// Whether the guest is due a look at its sessions and connections at
    /// `now`. The caller holds the check until it is done, so one runs at a
    /// time.
    fn due(self: &Arc<Self>, now: Instant, after: Duration) -> Option<Checking> {
        let mut state = self.state();
        let wait = if state.working {
            SESSION_POLL.min(after)
        } else {
            after
        };
        if state.checking
            || state.busy > 0
            || now.saturating_duration_since(state.idle_since) < wait
        {
            return None;
        }
        state.checking = true;
        Some(Checking(self.clone()))
    }

    fn saw_work(&self, working: bool, now: Instant) {
        let mut state = self.state();
        state.working = working;
        if working {
            state.idle_since = state.idle_since.max(now);
        }
    }

    /// Whether nothing has used the sandbox for `after`.
    pub fn idle(&self, now: Instant, after: Duration) -> bool {
        let state = self.state();
        state.busy == 0
            && !state.working
            && now.saturating_duration_since(state.idle_since) >= after
    }
}

pub struct Busy(Arc<Activity>);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.busy -= 1;
        if state.busy == 0 {
            state.idle_since = Instant::now();
        }
    }
}

struct Checking(Arc<Activity>);

impl Drop for Checking {
    fn drop(&mut self) {
        self.0.state().checking = false;
    }
}

/// Looks at the running sandboxes every second until shutdown starts.
pub async fn watch(node: Arc<Node>, after: Duration) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if node.is_closing() {
            return;
        }
        let now = Instant::now();
        for entry in node.entries() {
            if !entry.is_running() {
                continue;
            }
            if let Some(cpu_usec) = entry.cpu_usec() {
                entry.activity().saw_cpu(cpu_usec, now);
            }
            if let Some(checking) = entry.activity().due(now, after) {
                tokio::spawn(check(node.clone(), entry, after, checking));
            }
        }
    }
}

/// Stops the sandbox unless its guest still runs a session or uses a
/// connection. A guest or netd that cannot say is taken to be in use, and
/// is asked again, so a busy guest does not lose its work.
async fn check(node: Arc<Node>, entry: Arc<Entry>, after: Duration, _checking: Checking) {
    let Some(addr) = entry.guest_if_running().await else {
        return;
    };
    let name = &entry.record.name;
    let running = match tokio::time::timeout(SESSION_POLL, exec::sessions(&addr)).await {
        Ok(Ok(sessions)) => sessions.iter().any(|session| session.is_active),
        Ok(Err(e)) => {
            eprintln!(
                "shuru-node: listing the sessions of sandbox {name}: {}",
                e.message()
            );
            true
        }
        Err(_) => {
            eprintln!("shuru-node: sandbox {name} did not list its sessions in time");
            true
        }
    };
    let working = running || on_the_network(&entry, after).await;
    entry.activity().saw_work(working, Instant::now());
    if working {
        return;
    }
    if let Err(e) = node.stop_idle(&entry, after).await {
        if !node.is_closing() {
            eprintln!("shuru-node: parking sandbox {name}: {}", e.message());
        }
    }
}

async fn on_the_network(entry: &Entry, after: Duration) -> bool {
    let Some(netd) = entry.netd_if_running().await else {
        return false;
    };
    let name = &entry.record.name;
    let asked = tokio::task::spawn_blocking(move || {
        let netd = netd.lock().unwrap_or_else(|e| e.into_inner());
        vm::flows(&netd)
    })
    .await;
    match asked {
        Ok(Ok(flows)) => flows.iter().any(|flow| in_use(flow, after)),
        Ok(Err(e)) => {
            eprintln!("shuru-node: listing the connections of sandbox {name}: {e:#}");
            true
        }
        Err(e) => {
            eprintln!("shuru-node: listing the connections of sandbox {name}: {e}");
            true
        }
    }
}

fn in_use(flow: &FlowAge, after: Duration) -> bool {
    flow.from_guest_ms < WAIT_LIMIT.as_millis() as u64
        || flow
            .to_guest_ms
            .is_some_and(|to| to < after.as_millis() as u64)
}

fn busy_cpu(used_usec: u64, over: Duration) -> bool {
    !over.is_zero() && u128::from(used_usec) * 1_000_000 >= BUSY_CPU.as_micros() * over.as_micros()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AFTER: Duration = Duration::from_secs(30);

    #[test]
    fn idle_once_nothing_is_open_for_long_enough() {
        let activity = Arc::new(Activity::default());
        let start = Instant::now();
        assert!(!activity.idle(start, AFTER));
        assert!(activity.idle(start + AFTER, AFTER));
        assert!(activity.due(start + AFTER, AFTER).is_some());
    }

    #[test]
    fn busy_holds_off_the_clock_and_restarts_it() {
        let activity = Arc::new(Activity::default());
        let first = activity.busy();
        let second = activity.busy();
        let later = Instant::now() + AFTER * 2;
        assert!(!activity.idle(later, AFTER));
        assert!(activity.due(later, AFTER).is_none());
        drop(first);
        assert!(!activity.idle(later, AFTER));
        drop(second);
        let ended = Instant::now();
        assert!(!activity.idle(ended + AFTER / 2, AFTER));
        assert!(activity.idle(ended + AFTER, AFTER));
    }

    #[test]
    fn one_check_at_a_time() {
        let activity = Arc::new(Activity::default());
        let at = Instant::now() + AFTER;
        let checking = activity.due(at, AFTER);
        assert!(checking.is_some());
        assert!(activity.due(at, AFTER).is_none());
        drop(checking);
        assert!(activity.due(at, AFTER).is_some());
    }

    #[test]
    fn a_running_session_restarts_the_clock_when_it_ends() {
        let activity = Arc::new(Activity::default());
        let start = Instant::now();
        let seen = start + AFTER;
        activity.saw_work(true, seen);
        assert!(!activity.idle(seen + AFTER, AFTER));
        // Asked again sooner than the idle time while a session runs.
        assert!(activity.due(seen + SESSION_POLL / 2, AFTER).is_none());
        assert!(activity.due(seen + SESSION_POLL, AFTER).is_some());
        // Gone by the next look: idle from when it was last seen.
        activity.saw_work(false, seen + SESSION_POLL);
        assert!(!activity.idle(seen + SESSION_POLL, AFTER));
        assert!(activity.idle(seen + AFTER, AFTER));
    }

    #[test]
    fn cpu_use_restarts_the_clock() {
        let activity = Arc::new(Activity::default());
        let start = Instant::now();
        activity.saw_cpu(1_000, start);
        let tick = start + Duration::from_secs(1);
        // Under the floor: a quiet guest.
        activity.saw_cpu(1_000 + 5_000, tick);
        assert!(activity.idle(start + AFTER, AFTER));
        let busy = tick + Duration::from_secs(1);
        activity.saw_cpu(6_000 + 200_000, busy);
        assert!(!activity.idle(start + AFTER, AFTER));
        assert!(!activity.idle(busy + AFTER / 2, AFTER));
        assert!(activity.idle(busy + AFTER, AFTER));
    }

    #[test]
    fn the_first_cpu_reading_after_a_boot_is_a_baseline() {
        let activity = Arc::new(Activity::default());
        activity.restart();
        let booted = Instant::now();
        activity.saw_cpu(5_000_000, booted + Duration::from_secs(1));
        assert!(activity.idle(booted + AFTER, AFTER));
    }

    #[test]
    fn busy_cpu_is_a_rate() {
        let second = Duration::from_secs(1);
        assert!(!busy_cpu(19_999, second));
        assert!(busy_cpu(20_000, second));
        assert!(!busy_cpu(39_000, second * 2));
        assert!(busy_cpu(40_000, second * 2));
        assert!(!busy_cpu(1_000_000, Duration::ZERO));
    }

    fn flow(from_guest_s: u64, to_guest_s: Option<u64>) -> FlowAge {
        FlowAge {
            from_guest_ms: from_guest_s * 1000,
            to_guest_ms: to_guest_s.map(|s| s * 1000),
        }
    }

    #[test]
    fn a_connection_the_guest_sent_on_is_in_use_up_to_the_limit() {
        assert!(in_use(&flow(120, None), AFTER));
        // Session tickets came after the request.
        assert!(in_use(&flow(120, Some(119)), AFTER));
        assert!(!in_use(&flow(WAIT_LIMIT.as_secs(), None), AFTER));
        assert!(!in_use(&flow(WAIT_LIMIT.as_secs(), Some(60)), AFTER));
    }

    #[test]
    fn a_remote_still_sending_keeps_its_connection_in_use() {
        let quiet = WAIT_LIMIT.as_secs() + 60;
        assert!(in_use(&flow(quiet, Some(5)), AFTER));
        assert!(!in_use(&flow(quiet, Some(40)), AFTER));
    }

    #[test]
    fn a_boot_restarts_the_clock() {
        let activity = Arc::new(Activity::default());
        activity.saw_work(true, Instant::now());
        activity.restart();
        let booted = Instant::now();
        assert!(!activity.idle(booted, AFTER));
        assert!(activity.idle(booted + AFTER, AFTER));
    }
}
