//! Which new connections the guest may open, decided on its SYN: ports
//! refused outright, and a pace for the rest.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use lru::LruCache;
use smoltcp::wire::Ipv4Address;

use crate::config::{ConnectionRate, ProxyConfig, Rate};

/// How long a destination counts as reached.
const DESTINATION_MEMORY: Duration = Duration::from_secs(600);
/// Destinations remembered at once. One forgotten early only costs a
/// token of `destinations` when it comes back.
const DESTINATIONS_KEPT: usize = 4096;

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
        }
    }

    pub(crate) fn admit(&mut self, dst: (Ipv4Address, u16), now: Instant) -> Verdict {
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
