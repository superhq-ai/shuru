//! Which new connections the guest may open, decided on its SYN: ports
//! refused outright, and a pace for the rest. The bytes it sends out, in
//! all and at a pace. And the record of what it opened, for tracing an
//! abuse report to a sandbox.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use lru::LruCache;
use serde_json::json;
use smoltcp::wire::Ipv4Address;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::config::{ConnectionRate, ProxyConfig, Rate};

/// How long a destination counts as reached.
const DESTINATION_MEMORY: Duration = Duration::from_secs(600);
/// Destinations remembered at once. One forgotten early only costs a
/// token of `destinations` when it comes back.
const DESTINATIONS_KEPT: usize = 4096;
/// SYNs held back or refused are counted and written at most this often,
/// so a scan costs a line per interval, not one per SYN.
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// The longest host name kept, as DNS allows.
const HOST_MAX: usize = 253;
/// How far the egress pace lets the guest run ahead before it holds bytes
/// back. Timers keep about a millisecond, so a hold for every payload
/// would leave far slower than the pace.
const EGRESS_SLACK: Duration = Duration::from_millis(10);

/// Where the record of what a guest opened goes: one JSON line per
/// connection, and the SYNs held back or refused per interval. `close`
/// writes what is still open, so a sandbox that stops leaves its record.
#[derive(Clone)]
pub struct EgressLog {
    sandbox: Arc<str>,
    state: Arc<Mutex<State>>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

struct State {
    next: u64,
    open: HashMap<u64, Open>,
    turned_away: TurnedAway,
}

struct Open {
    dst: SocketAddr,
    at: SystemTime,
    since: Instant,
    tally: Arc<Tally>,
}

/// SYNs turned away since `since`.
struct TurnedAway {
    since: SystemTime,
    held: u64,
    refused: BTreeMap<u16, u64>,
}

impl TurnedAway {
    fn since(since: SystemTime) -> Self {
        TurnedAway {
            since,
            held: 0,
            refused: BTreeMap::new(),
        }
    }
}

impl EgressLog {
    pub fn new(sandbox: &str, out: impl Write + Send + 'static) -> Self {
        EgressLog {
            sandbox: sandbox.into(),
            state: Arc::new(Mutex::new(State {
                next: 0,
                open: HashMap::new(),
                turned_away: TurnedAway::since(SystemTime::now()),
            })),
            out: Arc::new(Mutex::new(Box::new(out))),
        }
    }

    pub fn stderr(sandbox: &str) -> Self {
        Self::new(sandbox, std::io::stderr())
    }

    /// Writes every connection still open, marked `open`, and the SYNs
    /// turned away since the last report. For a sandbox about to stop.
    pub fn close(&self) {
        let (open, turned) = {
            let mut state = self.state();
            let open: Vec<Open> = state.open.drain().map(|(_, open)| open).collect();
            let turned =
                std::mem::replace(&mut state.turned_away, TurnedAway::since(SystemTime::now()));
            (open, turned)
        };
        for open in &open {
            self.connection(open, open.since.elapsed(), None);
        }
        self.turned_away(turned);
    }

    pub(crate) fn open(&self, dst: SocketAddr, tally: Arc<Tally>) -> Opened {
        let mut state = self.state();
        let id = state.next;
        state.next += 1;
        state.open.insert(
            id,
            Open {
                dst,
                at: SystemTime::now(),
                since: Instant::now(),
                tally,
            },
        );
        Opened {
            log: self.clone(),
            id,
            ok: false,
        }
    }

    fn ended(&self, id: u64, ok: bool) {
        let open = self.state().open.remove(&id);
        if let Some(open) = open {
            self.connection(&open, open.since.elapsed(), Some(ok));
        }
    }

    fn refused(&self, port: u16) {
        *self.state().turned_away.refused.entry(port).or_default() += 1;
    }

    fn held(&self) {
        self.state().turned_away.held += 1;
    }

    fn report(&self) {
        let turned = std::mem::replace(
            &mut self.state().turned_away,
            TurnedAway::since(SystemTime::now()),
        );
        self.turned_away(turned);
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self, line: serde_json::Value) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{line}");
    }

    fn connection(&self, open: &Open, lasted: Duration, ok: Option<bool>) {
        let mut line = json!({
            "egress": "connection",
            "sandbox": &*self.sandbox,
            "at": rfc3339(open.at),
            "dst": open.dst.to_string(),
            "host": open.tally.host.get(),
            "up": open.tally.up.load(Ordering::Relaxed),
            "down": open.tally.down.load(Ordering::Relaxed),
            "ms": lasted.as_millis() as u64,
        });
        match ok {
            Some(ok) => line["ok"] = ok.into(),
            None => line["open"] = true.into(),
        }
        self.write(line);
    }

    fn turned_away(&self, turned: TurnedAway) {
        if turned.held == 0 && turned.refused.is_empty() {
            return;
        }
        self.write(json!({
            "egress": "turned_away",
            "sandbox": &*self.sandbox,
            "at": rfc3339(turned.since),
            "held": turned.held,
            "refused": turned.refused,
        }));
    }
}

impl std::fmt::Debug for EgressLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressLog")
            .field("sandbox", &self.sandbox)
            .finish_non_exhaustive()
    }
}

/// A connection in the record until it ends. Dropped without `end`, as
/// when its task panics, it is written as failed.
pub(crate) struct Opened {
    log: EgressLog,
    id: u64,
    ok: bool,
}

impl Opened {
    pub(crate) fn end(mut self, ok: bool) {
        self.ok = ok;
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.log.ended(self.id, self.ok);
    }
}

fn rfc3339(at: SystemTime) -> String {
    OffsetDateTime::from(at)
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// The bytes the guest sent out, in all, and the pace they may leave at.
pub struct Egress {
    bytes: AtomicU64,
    pace: Option<Mutex<Bucket>>,
}

impl Egress {
    pub(crate) fn new(rate: Option<Rate>, now: Instant) -> Self {
        Egress {
            bytes: AtomicU64::new(0),
            pace: rate
                .filter(|rate| rate.per_second > 0)
                .map(|rate| Mutex::new(Bucket::new(rate, now))),
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Spends `n` bytes of the pace, and answers how long to hold them
    /// back before they leave.
    pub(crate) fn spend(&self, n: usize, now: Instant) -> Option<Duration> {
        let mut bucket = self
            .pace
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        bucket.owe(n as f64, now, EGRESS_SLACK)
    }

    pub(crate) fn sent(&self, n: usize) {
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Bytes each way on one connection, and the host its TLS ClientHello
/// named, filled in as the relay runs.
#[derive(Default)]
pub(crate) struct Tally {
    pub(crate) up: AtomicU64,
    pub(crate) down: AtomicU64,
    host: OnceLock<String>,
}

impl Tally {
    /// The guest chooses the name, so only what a host name may hold is
    /// kept, lowercased and cut to length.
    pub(crate) fn set_host(&self, host: &str) {
        let clean: String = host
            .chars()
            .take(HOST_MAX)
            .map(|c| match c {
                'a'..='z' | '0'..='9' | '.' | '-' | '_' | '*' => c,
                'A'..='Z' => c.to_ascii_lowercase(),
                _ => '?',
            })
            .collect();
        let _ = self.host.set(clean);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Open,
    /// Answered as a closed port.
    Refuse,
    /// Held back; the guest sends the SYN again.
    Wait,
}

pub(crate) struct Gate {
    deny_ports: HashSet<u16>,
    rate: Option<Paced>,
    log: Option<EgressLog>,
    next_report: Instant,
}

struct Paced {
    connections: Bucket,
    destinations: Bucket,
    reached: LruCache<(Ipv4Address, u16), Instant>,
}

impl Gate {
    pub(crate) fn new(config: &ProxyConfig, now: Instant) -> Self {
        Gate {
            deny_ports: config.deny_ports.iter().copied().collect(),
            rate: config.connection_rate.map(|rate: ConnectionRate| Paced {
                connections: Bucket::new(rate.connections, now),
                destinations: Bucket::new(rate.destinations, now),
                reached: LruCache::new(NonZeroUsize::new(DESTINATIONS_KEPT).expect("non-zero")),
            }),
            log: config.egress_log.clone(),
            next_report: now + REPORT_EVERY,
        }
    }

    pub(crate) fn admit(&mut self, dst: (Ipv4Address, u16), now: Instant) -> Verdict {
        let verdict = self.decide(dst, now);
        if let Some(log) = &self.log {
            match verdict {
                Verdict::Open => {}
                Verdict::Refuse => log.refused(dst.1),
                Verdict::Wait => log.held(),
            }
        }
        verdict
    }

    /// Writes the SYNs turned away since the last report, once
    /// `REPORT_EVERY` has passed and there were some.
    pub(crate) fn report(&mut self, now: Instant) {
        let Some(log) = &self.log else {
            return;
        };
        if now < self.next_report {
            return;
        }
        log.report();
        self.next_report = now + REPORT_EVERY;
    }

    fn decide(&mut self, dst: (Ipv4Address, u16), now: Instant) -> Verdict {
        if self.deny_ports.contains(&dst.1) {
            return Verdict::Refuse;
        }
        let Some(paced) = &mut self.rate else {
            return Verdict::Open;
        };
        let new = paced
            .reached
            .get(&dst)
            .is_none_or(|at| now.duration_since(*at) >= DESTINATION_MEMORY);
        paced.connections.refill(now);
        paced.destinations.refill(now);
        if !paced.connections.has() || (new && !paced.destinations.has()) {
            return Verdict::Wait;
        }
        paced.connections.take();
        if new {
            paced.destinations.take();
        }
        paced.reached.put(dst, now);
        Verdict::Open
    }
}

struct Bucket {
    rate: Rate,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: Rate, now: Instant) -> Self {
        Bucket {
            rate,
            tokens: f64::from(rate.burst),
            at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * f64::from(self.rate.per_second))
            .min(f64::from(self.rate.burst));
        self.at = now;
    }

    fn has(&self) -> bool {
        self.tokens >= 1.0
    }

    fn take(&mut self) {
        self.tokens -= 1.0;
    }

    /// Takes `n` even past empty, and answers how long until the debt is
    /// paid back, once it is more than `slack` of the rate.
    fn owe(&mut self, n: f64, now: Instant, slack: Duration) -> Option<Duration> {
        self.refill(now);
        self.tokens -= n;
        let wait = -self.tokens / f64::from(self.rate.per_second);
        (wait > slack.as_secs_f64()).then(|| Duration::from_secs_f64(wait))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(connections: (u32, u32), destinations: (u32, u32), now: Instant) -> Gate {
        let config = ProxyConfig {
            deny_ports: vec![25, 587],
            connection_rate: Some(ConnectionRate {
                connections: Rate {
                    per_second: connections.0,
                    burst: connections.1,
                },
                destinations: Rate {
                    per_second: destinations.0,
                    burst: destinations.1,
                },
            }),
            ..Default::default()
        };
        Gate::new(&config, now)
    }

    fn host(last: u8) -> Ipv4Address {
        Ipv4Address::new(93, 184, 216, last)
    }

    #[test]
    fn denied_ports_are_refused_and_spend_nothing() {
        let now = Instant::now();
        let mut gate = gate((1, 1), (1, 1), now);
        assert_eq!(gate.admit((host(1), 25), now), Verdict::Refuse);
        assert_eq!(gate.admit((host(1), 587), now), Verdict::Refuse);
        assert_eq!(gate.admit((host(1), 443), now), Verdict::Open);
    }

    #[test]
    fn without_a_rate_everything_else_opens() {
        let now = Instant::now();
        let mut gate = Gate::new(&ProxyConfig::default(), now);
        for port in 1..=2000 {
            assert_eq!(gate.admit((host(1), port), now), Verdict::Open);
        }
    }

    #[test]
    fn a_scan_waits_once_new_destinations_run_out() {
        let now = Instant::now();
        let mut gate = gate((1000, 1000), (2, 5), now);
        for port in 1..=5 {
            assert_eq!(gate.admit((host(1), port), now), Verdict::Open);
        }
        assert_eq!(gate.admit((host(1), 6), now), Verdict::Wait);
        assert_eq!(gate.admit((host(2), 443), now), Verdict::Wait);
        // Half a second at two a second brings one back.
        let later = now + Duration::from_millis(500);
        assert_eq!(gate.admit((host(2), 443), later), Verdict::Open);
        assert_eq!(gate.admit((host(3), 443), later), Verdict::Wait);
    }

    #[test]
    fn a_destination_reached_lately_costs_no_destination_token() {
        let now = Instant::now();
        let mut gate = gate((1000, 1000), (1, 1), now);
        assert_eq!(gate.admit((host(1), 443), now), Verdict::Open);
        for _ in 0..100 {
            assert_eq!(gate.admit((host(1), 443), now), Verdict::Open);
        }
        assert_eq!(gate.admit((host(2), 443), now), Verdict::Wait);
    }

    #[test]
    fn a_destination_counts_as_new_again_after_the_memory() {
        let now = Instant::now();
        let mut gate = gate((1000, 1000), (1, 1), now);
        assert_eq!(gate.admit((host(1), 443), now), Verdict::Open);
        let later = now + DESTINATION_MEMORY;
        // The bucket refilled, so the return opens, but it spends the token.
        assert_eq!(gate.admit((host(1), 443), later), Verdict::Open);
        assert_eq!(gate.admit((host(2), 443), later), Verdict::Wait);
    }

    #[test]
    fn a_flood_to_one_destination_waits_on_connections() {
        let now = Instant::now();
        let mut gate = gate((10, 3), (100, 100), now);
        for _ in 0..3 {
            assert_eq!(gate.admit((host(1), 22), now), Verdict::Open);
        }
        assert_eq!(gate.admit((host(1), 22), now), Verdict::Wait);
        let later = now + Duration::from_millis(100);
        assert_eq!(gate.admit((host(1), 22), later), Verdict::Open);
    }

    /// A writer the test reads back.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Lines {
        fn read(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[test]
    fn a_connection_is_one_line_with_its_sandbox_destination_host_and_bytes() {
        let lines = Lines::default();
        let log = EgressLog::new("sbx-1", lines.clone());
        let tally = Tally::default();
        tally.up.store(512, Ordering::Relaxed);
        tally.down.store(4096, Ordering::Relaxed);
        tally.set_host("Example.COM");
        let open = Open {
            dst: "93.184.216.34:443".parse().unwrap(),
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_000_000),
            since: Instant::now(),
            tally: Arc::new(tally),
        };
        log.connection(&open, Duration::from_millis(1234), Some(true));
        let lines = lines.read();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            json!({
                "egress": "connection",
                "sandbox": "sbx-1",
                "at": "2026-10-26T07:33:20Z",
                "dst": "93.184.216.34:443",
                "host": "example.com",
                "up": 512,
                "down": 4096,
                "ms": 1234,
                "ok": true,
            })
        );
    }

    #[test]
    fn closing_writes_what_is_still_open_and_what_was_turned_away() {
        let lines = Lines::default();
        let log = EgressLog::new("sbx-1", lines.clone());
        let ended = log.open("93.184.216.34:443".parse().unwrap(), Arc::default());
        let still = log.open("93.184.216.35:443".parse().unwrap(), Arc::default());
        ended.end(true);
        log.refused(25);
        log.close();
        drop(still);
        log.close();
        let read = lines.read();
        assert_eq!(read.len(), 3, "{read:?}");
        assert_eq!(read[0]["dst"], "93.184.216.34:443");
        assert_eq!(read[0]["ok"], true);
        assert_eq!(read[1]["dst"], "93.184.216.35:443");
        assert_eq!(read[1]["open"], true);
        assert!(read[1].get("ok").is_none());
        assert_eq!(read[2]["egress"], "turned_away");
        assert_eq!(read[2]["refused"], json!({"25": 1}));
    }

    #[test]
    fn a_connection_dropped_without_an_end_is_written_as_failed() {
        let lines = Lines::default();
        let log = EgressLog::new("sbx-1", lines.clone());
        drop(log.open("93.184.216.34:443".parse().unwrap(), Arc::default()));
        assert_eq!(lines.read()[0]["ok"], false);
    }

    #[test]
    fn a_host_keeps_only_what_a_host_name_may_hold() {
        let tally = Tally::default();
        tally.set_host("evil.com\n{\"egress\":\"fake\"}");
        assert_eq!(tally.host.get().unwrap(), "evil.com???egress???fake??");
        let long = Tally::default();
        long.set_host(&"a".repeat(400));
        assert_eq!(long.host.get().unwrap().len(), HOST_MAX);
    }

    #[test]
    fn turned_away_syns_are_reported_once_per_interval() {
        let lines = Lines::default();
        let now = Instant::now();
        let config = ProxyConfig {
            deny_ports: vec![25],
            connection_rate: Some(ConnectionRate {
                connections: Rate {
                    per_second: 1,
                    burst: 1,
                },
                destinations: Rate {
                    per_second: 1,
                    burst: 1,
                },
            }),
            egress_log: Some(EgressLog::new("sbx-1", lines.clone())),
            ..Default::default()
        };
        let mut gate = Gate::new(&config, now);
        assert_eq!(gate.admit((host(1), 443), now), Verdict::Open);
        for _ in 0..3 {
            assert_eq!(gate.admit((host(2), 443), now), Verdict::Wait);
        }
        assert_eq!(gate.admit((host(1), 25), now), Verdict::Refuse);
        gate.report(now + Duration::from_secs(1));
        assert!(lines.read().is_empty(), "not before the interval");
        gate.report(now + REPORT_EVERY);
        let read = lines.read();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0]["egress"], "turned_away");
        assert_eq!(read[0]["held"], 3);
        assert_eq!(read[0]["refused"], json!({"25": 1}));
        gate.report(now + REPORT_EVERY * 3);
        assert_eq!(lines.read().len(), 1, "nothing to say, nothing written");
    }

    #[test]
    fn waiting_spends_no_tokens() {
        let now = Instant::now();
        let mut gate = gate((1, 1), (100, 100), now);
        assert_eq!(gate.admit((host(1), 443), now), Verdict::Open);
        for _ in 0..50 {
            assert_eq!(gate.admit((host(2), 443), now), Verdict::Wait);
        }
        let later = now + Duration::from_secs(1);
        assert_eq!(gate.admit((host(2), 443), later), Verdict::Open);
    }

    #[test]
    fn egress_leaves_at_its_pace_after_the_burst() {
        let now = Instant::now();
        let rate = Rate {
            per_second: 1000,
            burst: 1000,
        };
        let egress = Egress::new(Some(rate), now);
        assert_eq!(egress.spend(1000, now), None);
        assert_eq!(egress.spend(500, now), Some(Duration::from_millis(500)));
        // Every connection waits out the debt of all of them.
        assert_eq!(egress.spend(500, now), Some(Duration::from_secs(1)));
        let later = now + Duration::from_secs(3);
        assert_eq!(egress.spend(1000, later), None, "paid back and full again");
        // A debt within the slack is let through and paid back later.
        assert_eq!(egress.spend(5, later), None);
        assert_eq!(egress.spend(10, later), Some(Duration::from_millis(15)));
    }

    #[test]
    fn egress_without_a_pace_is_only_counted() {
        let now = Instant::now();
        let egress = Egress::new(None, now);
        assert_eq!(egress.spend(usize::MAX, now), None);
        egress.sent(1500);
        egress.sent(500);
        assert_eq!(egress.bytes(), 2000);
        let zero = Rate {
            per_second: 0,
            burst: 0,
        };
        assert_eq!(Egress::new(Some(zero), now).spend(1, now), None);
    }
}
