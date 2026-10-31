use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

/// A host port exposed to the guest via host.shuru.internal.
#[derive(Debug, Clone)]
pub struct ExposeHostMapping {
    /// Port on the host (127.0.0.1:host_port).
    pub host_port: u16,
    /// Port the guest connects to (host.shuru.internal:guest_port).
    pub guest_port: u16,
}

/// A token bucket: `burst` at once, refilled at `per_second`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub per_second: u32,
    pub burst: u32,
}

/// Bounds on new connections from the guest. A SYN over either bound is
/// held back, not refused, so the guest retransmits it and slows down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnectionRate {
    /// Every new connection.
    pub connections: Rate,
    /// Connections to an address and port the guest has not reached
    /// lately. Scans touch a new one with every SYN; real work returns to
    /// the same few.
    pub destinations: Rate,
}

/// Configuration for the proxy engine.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// IPv4 DNS server used for upstream queries.
    pub dns_resolver: Ipv4Addr,
    /// Secrets to inject. Key is the env var name visible to the guest.
    /// The guest gets a random placeholder token; the proxy substitutes
    /// the real value only when the request targets an allowed host.
    pub secrets: HashMap<String, SecretConfig>,
    /// Network access rules.
    pub network: NetworkConfig,
    /// Host ports exposed to the guest via host.shuru.internal.
    pub expose_host: Vec<ExposeHostMapping>,
    /// Working directory for secret provider commands. Normally the directory
    /// holding the resolved shuru.json, so relative paths in a config behave
    /// the same wherever shuru is invoked from. None inherits the process cwd.
    pub config_dir: Option<PathBuf>,
    /// Refuse connections to anything but public addresses: no loopback,
    /// private, link-local or shared ranges, so a guest on a shared host
    /// cannot reach the host, its network or a metadata service. Turns
    /// `expose_host` off.
    pub public_only: bool,
    /// Destination ports the guest is refused outright, as a closed port.
    pub deny_ports: Vec<u16>,
    /// How fast the guest may open connections. None leaves it unbounded.
    pub connection_rate: Option<ConnectionRate>,
    /// Where each connection the guest opened is recorded. None keeps no
    /// record.
    pub egress_log: Option<crate::egress::EgressLog>,
    /// PEM roots for verifying upstream servers when relaying secrets.
    /// None reads the system store from disk.
    pub upstream_roots: Option<Vec<u8>>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            dns_resolver: Ipv4Addr::new(8, 8, 8, 8),
            secrets: HashMap::new(),
            network: NetworkConfig::default(),
            expose_host: Vec::new(),
            config_dir: None,
            public_only: false,
            deny_ports: Vec::new(),
            connection_rate: None,
            egress_log: None,
            upstream_roots: None,
        }
    }
}

/// A secret that the proxy injects into HTTP requests.
///
/// The value comes from exactly one source: a host environment variable
/// (`from`), a command that mints it (`command`), or a literal (`value`).
#[derive(Debug, Clone, Default)]
pub struct SecretConfig {
    /// Host environment variable to read the real value from.
    pub from: Option<String>,
    /// Command to run to mint the value, as argv. Never passed to a shell.
    /// Re-run once the minted value approaches its expiry.
    pub command: Option<Vec<String>>,
    /// Lifetime to assume when a command reports no expiry of its own.
    pub ttl: Option<Duration>,
    /// Domain patterns where this secret may be sent (e.g., "api.openai.com").
    /// The proxy only substitutes the placeholder on requests to these hosts.
    pub hosts: Vec<String>,
    /// If set, use this value directly instead of reading from the host env var.
    pub value: Option<String>,
}

impl SecretConfig {
    /// Secret backed by a host environment variable.
    pub fn from_env(var: impl Into<String>, hosts: Vec<String>) -> Self {
        Self {
            from: Some(var.into()),
            hosts,
            ..Default::default()
        }
    }

    /// Secret minted by running a command.
    pub fn from_command(command: Vec<String>, hosts: Vec<String>) -> Self {
        Self {
            command: Some(command),
            hosts,
            ..Default::default()
        }
    }

    /// True if this secret is bound to `domain`.
    pub fn matches_domain(&self, domain: &str) -> bool {
        self.hosts
            .iter()
            .any(|pattern| domain_matches(pattern, domain))
    }
}

/// Network access policy.
#[derive(Debug, Clone, Default)]
pub struct NetworkConfig {
    /// Allowed domain patterns. Empty = allow all.
    /// Supports wildcards: "*.openai.com", "registry.npmjs.org".
    pub allow: Vec<String>,
}

impl NetworkConfig {
    /// Returns true if a domain allowlist is configured.
    pub fn has_allowlist(&self) -> bool {
        !self.allow.is_empty()
    }
}

impl ProxyConfig {
    /// Check if a domain is allowed by the network policy.
    /// Empty allowlist means all domains are allowed.
    pub fn is_domain_allowed(&self, domain: &str) -> bool {
        if self.network.allow.is_empty() {
            return true;
        }
        self.network
            .allow
            .iter()
            .any(|pattern| domain_matches(pattern, domain))
    }

    /// Look up whether a connection to the gateway IP on `guest_port` should
    /// be forwarded to a host port. Returns the host port if matched.
    pub fn exposed_host_port(&self, dst_ip: Ipv4Addr, guest_port: u16) -> Option<u16> {
        const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
        if dst_ip != GATEWAY {
            return None;
        }
        self.expose_host
            .iter()
            .find(|m| m.guest_port == guest_port)
            .map(|m| m.host_port)
    }
}

/// False for loopback, private, link-local, shared, multicast, documentation
/// and reserved addresses.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..128).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..32).contains(&b))
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => is_public(IpAddr::V4(v4)),
            // Global unicast, minus the IANA special block and documentation.
            None => {
                let [a, b, ..] = ip.segments();
                a & 0xe000 == 0x2000 && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
            }
        },
    }
}

/// Simple wildcard domain matching.
/// "*" matches any domain (catch-all).
/// "*.example.com" matches "api.example.com" but not "example.com".
/// "example.com" matches exactly "example.com".
pub(crate) fn domain_matches(pattern: &str, domain: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix("*.") {
        domain.ends_with(suffix)
            && domain.len() > suffix.len()
            && domain.as_bytes()[domain.len() - suffix.len() - 1] == b'.'
    } else {
        pattern == domain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exposed_host_port() {
        use std::net::Ipv4Addr;
        let config = ProxyConfig {
            expose_host: vec![
                ExposeHostMapping {
                    host_port: 3000,
                    guest_port: 8080,
                },
                ExposeHostMapping {
                    host_port: 5432,
                    guest_port: 5432,
                },
            ],
            ..Default::default()
        };
        // Gateway IP match
        assert_eq!(
            config.exposed_host_port(Ipv4Addr::new(10, 0, 0, 1), 8080),
            Some(3000)
        );
        assert_eq!(
            config.exposed_host_port(Ipv4Addr::new(10, 0, 0, 1), 5432),
            Some(5432)
        );
        // No mapping for this port
        assert_eq!(
            config.exposed_host_port(Ipv4Addr::new(10, 0, 0, 1), 9999),
            None
        );
        // Non-gateway IP
        assert_eq!(
            config.exposed_host_port(Ipv4Addr::new(1, 2, 3, 4), 8080),
            None
        );
    }

    #[test]
    fn test_domain_matching() {
        assert!(domain_matches("*", "anything.com"));
        assert!(domain_matches("*", "api.example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("example.com", "api.example.com"));
        assert!(domain_matches("*.example.com", "api.example.com"));
        assert!(domain_matches("*.example.com", "deep.api.example.com"));
        assert!(!domain_matches("*.example.com", "example.com"));
        assert!(!domain_matches("*.example.com", "notexample.com"));
    }

    #[test]
    fn test_has_allowlist() {
        let empty = NetworkConfig::default();
        assert!(!empty.has_allowlist());

        let with_entries = NetworkConfig {
            allow: vec!["api.example.com".into()],
        };
        assert!(with_entries.has_allowlist());
    }

    #[test]
    fn public_addresses() {
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700::1111",
            "2001:4860::8888",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        for private in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.170",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "2001:db8::1",
            "ff02::1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
    }

    #[test]
    fn default_dns_resolver_is_google_public_dns() {
        assert_eq!(
            ProxyConfig::default().dns_resolver,
            Ipv4Addr::new(8, 8, 8, 8)
        );
    }
}
