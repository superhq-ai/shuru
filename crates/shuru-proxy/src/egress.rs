//! Which new connections the guest may open, decided on its SYN: ports
//! refused outright, and a pace for the rest. And the record of what it
//! opened, for tracing an abuse report to a sandbox.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// Where the record of what a guest opened goes: one JSON line per
/// connection, and the SYNs held back or refused per interval.
#[derive(Clone)]
pub struct EgressLog {
    sandbox: Arc<str>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl EgressLog {
    pub fn new(sandbox: &str, out: impl Write + Send + 'static) -> Self {
        EgressLog {
            sandbox: sandbox.into(),
            out: Arc::new(Mutex::new(Box::new(out))),
        }
    }

    pub fn stderr(sandbox: &str) -> Self {
        Self::new(sandbox, std::io::stderr())
    }

    fn write(&self, line: serde_json::Value) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{line}");
    }

    pub(crate) fn connection(
        &self,
        dst: SocketAddr,
        opened: SystemTime,
        lasted: Duration,
        tally: &Tally,
        ok: bool,
    ) {
        self.write(json!({
            "egress": "connection",
            "sandbox": &*self.sandbox,
            "at": rfc3339(opened),
            "dst": dst.to_string(),
            "host": tally.host.get(),
            "up": tally.up.load(Ordering::Relaxed),
            "down": tally.down.load(Ordering::Relaxed),
            "ms": lasted.as_millis() as u64,
            "ok": ok,
        }));
    }

    fn turned_away(&self, since: SystemTime, held: u64, refused: &BTreeMap<u16, u64>) {
        self.write(json!({
            "egress": "turned_away",
            "sandbox": &*self.sandbox,
            "at": rfc3339(since),
            "held": held,
            "refused": refused,
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

fn rfc3339(at: SystemTime) -> String {
    OffsetDateTime::from(at)
        .format(&Rfc3339)
        .unwrap_or_default()
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

/// SYNs turned away since `since`.
struct TurnedAway {
    since: (Instant, SystemTime),
    held: u64,
    refused: BTreeMap<u16, u64>,
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
    turned_away: TurnedAway,
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
            turned_away: TurnedAway {
                since: (now, SystemTime::now()),
                held: 0,
                refused: BTreeMap::new(),
            },
        }
    }

    pub(crate) fn admit(&mut self, dst: (Ipv4Address, u16), now: Instant) -> Verdict {
        let verdict = self.decide(dst, now);
        match verdict {
            Verdict::Open => {}
            Verdict::Refuse => *self.turned_away.refused.entry(dst.1).or_default() += 1,
            Verdict::Wait => self.turned_away.held += 1,
        }
        verdict
    }

    /// Writes the SYNs turned away since the last report, once
    /// `REPORT_EVERY` has passed and there were some.
    pub(crate) fn report(&mut self, now: Instant) {
        let Some(log) = &self.log else {
            return;
        };
        let turned = &mut self.turned_away;
        if now.saturating_duration_since(turned.since.0) < REPORT_EVERY {
            return;
        }
        if turned.held > 0 || !turned.refused.is_empty() {
            log.turned_away(turned.since.1, turned.held, &turned.refused);
        }
        *turned = TurnedAway {
            since: (now, SystemTime::now()),
            held: 0,
            refused: BTreeMap::new(),
        };
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
        let dst: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let opened = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_000_000);
        log.connection(dst, opened, Duration::from_millis(1234), &tally, true);
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
}
