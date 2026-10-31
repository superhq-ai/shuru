//! Idle park: a running sandbox nothing has used for `--idle-park` is
//! stopped as `stop` stops it. Requests that use its guest and upgraded
//! connections to it count, and so do exec sessions whose process still
//! runs, which only the guest knows of once no client is attached.
//! Managed services do not count.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::exec;
use super::sandboxes::{Entry, Node};

const TICK: Duration = Duration::from_secs(1);
/// How often a guest that had a session running is asked again, and how
/// long it has to answer.
const SESSION_POLL: Duration = Duration::from_secs(5);

pub struct Activity {
    state: Mutex<State>,
}

struct State {
    /// Requests and connections open.
    busy: usize,
    /// When the last of them ended, the VM booted, or a session was last
    /// seen running.
    idle_since: Instant,
    /// The last look at the guest found a session running.
    sessions: bool,
    checking: bool,
}

impl Default for Activity {
    fn default() -> Self {
        Activity {
            state: Mutex::new(State {
                busy: 0,
                idle_since: Instant::now(),
                sessions: false,
                checking: false,
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
        state.sessions = false;
    }

    /// Whether the guest is due a look at its sessions at `now`. The
    /// caller holds the check until it is done, so one runs at a time.
    fn due(self: &Arc<Self>, now: Instant, after: Duration) -> Option<Checking> {
        let mut state = self.state();
        let wait = if state.sessions {
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

    fn saw_sessions(&self, running: bool, now: Instant) {
        let mut state = self.state();
        state.sessions = running;
        if running {
            state.idle_since = state.idle_since.max(now);
        }
    }

    /// Whether nothing has used the sandbox for `after`.
    pub fn idle(&self, now: Instant, after: Duration) -> bool {
        let state = self.state();
        state.busy == 0
            && !state.sessions
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
            if let Some(checking) = entry.activity().due(now, after) {
                tokio::spawn(check(node.clone(), entry, after, checking));
            }
        }
    }
}

/// Stops the sandbox unless its guest still runs a session. A guest that
/// cannot say is taken to run one, and is asked again, so a busy guest
/// does not lose its work.
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
    entry.activity().saw_sessions(running, Instant::now());
    if running {
        return;
    }
    if let Err(e) = node.stop_idle(&entry, after).await {
        if !node.is_closing() {
            eprintln!("shuru-node: parking sandbox {name}: {}", e.message());
        }
    }
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
        activity.saw_sessions(true, seen);
        assert!(!activity.idle(seen + AFTER, AFTER));
        // Asked again sooner than the idle time while a session runs.
        assert!(activity.due(seen + SESSION_POLL / 2, AFTER).is_none());
        assert!(activity.due(seen + SESSION_POLL, AFTER).is_some());
        // Gone by the next look: idle from when it was last seen.
        activity.saw_sessions(false, seen + SESSION_POLL);
        assert!(!activity.idle(seen + SESSION_POLL, AFTER));
        assert!(activity.idle(seen + AFTER, AFTER));
    }

    #[test]
    fn a_boot_restarts_the_clock() {
        let activity = Arc::new(Activity::default());
        activity.saw_sessions(true, Instant::now());
        activity.restart();
        let booted = Instant::now();
        assert!(!activity.idle(booted, AFTER));
        assert!(activity.idle(booted + AFTER, AFTER));
    }
}
