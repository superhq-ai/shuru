use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Default, Deserialize)]
pub(crate) struct ShuruConfig {
    pub cpus: Option<usize>,
    pub memory: Option<u64>,
    pub disk_size: Option<u64>,
    pub allow_net: Option<bool>,
    pub allow_host_writes: Option<bool>,
    pub ports: Option<Vec<String>>,
    pub mounts: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    pub secrets: Option<HashMap<String, SecretEntry>>,
    pub network: Option<NetworkEntry>,
    /// Host ports to expose to the guest (e.g. "3000:8080" or "5432").
    pub expose_host: Option<Vec<String>>,
    /// Directory holding this config, used as the working directory for
    /// secret provider commands. Set on load, never deserialized.
    #[serde(skip)]
    pub config_dir: Option<PathBuf>,
}

/// A secret to inject via the proxy. The value comes from either a host
/// environment variable or a command that mints it, never both.
///
/// Example: `{ "from": "OPENAI_API_KEY", "hosts": ["api.openai.com"] }`
/// Example: `{ "command": ["./mint.sh"], "hosts": ["api.github.com"] }`
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SecretEntry {
    /// Host environment variable containing the real value.
    pub from: Option<String>,
    /// Command that mints the value, as argv. Re-run as the value expires.
    pub command: Option<Vec<String>>,
    /// Lifetime to assume when the command reports no expiry of its own.
    /// Accepts "45m", "3600s", "2h", or a bare number of seconds.
    pub ttl: Option<String>,
    /// Domains where this secret may be sent.
    pub hosts: Vec<String>,
}

impl SecretEntry {
    fn to_secret_config(&self, name: &str) -> Result<shuru_proxy::config::SecretConfig> {
        let ttl = match self.ttl {
            Some(ref ttl) => Some(
                parse_duration(ttl)
                    .map_err(|e| anyhow::anyhow!("secret '{name}': invalid ttl '{ttl}': {e}"))?,
            ),
            None => None,
        };

        match (&self.from, &self.command) {
            (Some(_), Some(_)) => {
                bail!("secret '{name}': set either 'from' or 'command', not both")
            }
            (None, None) => bail!("secret '{name}': needs either 'from' or 'command'"),
            (Some(from), None) => {
                if ttl.is_some() {
                    bail!("secret '{name}': 'ttl' only applies to 'command' secrets");
                }
                Ok(shuru_proxy::config::SecretConfig::from_env(
                    from.clone(),
                    self.hosts.clone(),
                ))
            }
            (None, Some(command)) => {
                if command.is_empty() {
                    bail!("secret '{name}': 'command' is empty");
                }
                Ok(shuru_proxy::config::SecretConfig {
                    ttl,
                    ..shuru_proxy::config::SecretConfig::from_command(
                        command.clone(),
                        self.hosts.clone(),
                    )
                })
            }
        }
    }
}

/// Parse "45m", "3600s", "2h", or a bare number of seconds.
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (digits, multiplier) = match s.strip_suffix(['s', 'm', 'h']) {
        Some(digits) => (
            digits,
            match s.as_bytes()[s.len() - 1] {
                b's' => 1,
                b'm' => 60,
                _ => 3600,
            },
        ),
        None => (s, 1),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("expected a number optionally suffixed with s, m, or h"))?;
    if value == 0 {
        bail!("must be greater than zero");
    }
    Ok(Duration::from_secs(value * multiplier))
}

/// Network access policy.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct NetworkEntry {
    /// Allowed domain patterns. Empty or absent = allow all.
    pub allow: Option<Vec<String>>,
}

impl ShuruConfig {
    /// Reject a malformed config up front, so `shuru run` fails the same way
    /// whether or not networking is enabled.
    pub fn validate(&self) -> Result<()> {
        if let Some(size) = self.disk_size {
            shuru_vm::validate_disk_size(size)?;
        }
        if let Some(ref secrets) = self.secrets {
            for (name, entry) in secrets {
                entry.to_secret_config(name)?;
            }
        }
        Ok(())
    }

    /// Convert config sections into a ProxyConfig for shuru-proxy.
    pub fn to_proxy_config(&self) -> Result<shuru_proxy::config::ProxyConfig> {
        let mut proxy = shuru_proxy::config::ProxyConfig {
            config_dir: self.config_dir.clone(),
            ..Default::default()
        };

        if let Some(ref secrets) = self.secrets {
            for (name, entry) in secrets {
                proxy
                    .secrets
                    .insert(name.clone(), entry.to_secret_config(name)?);
            }
        }

        if let Some(ref network) = self.network {
            if let Some(ref allow) = network.allow {
                proxy.network.allow = allow.clone();
            }
        }

        if let Some(ref expose) = self.expose_host {
            for s in expose {
                if let Ok(mapping) = parse_expose_host(s) {
                    proxy.expose_host.push(mapping);
                }
            }
        }

        Ok(proxy)
    }
}

/// Parse "HOST_PORT:GUEST_PORT" or "PORT" into an ExposeHostMapping.
pub(crate) fn parse_expose_host(s: &str) -> Result<shuru_proxy::config::ExposeHostMapping> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            let port: u16 = parts[0]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid port: '{}'", parts[0]))?;
            Ok(shuru_proxy::config::ExposeHostMapping {
                host_port: port,
                guest_port: port,
            })
        }
        2 => {
            let host_port: u16 = parts[0]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid host port: '{}'", parts[0]))?;
            let guest_port: u16 = parts[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid guest port: '{}'", parts[1]))?;
            Ok(shuru_proxy::config::ExposeHostMapping {
                host_port,
                guest_port,
            })
        }
        _ => bail!("expected HOST_PORT:GUEST_PORT or PORT format"),
    }
}

pub(crate) fn load_config(config_flag: Option<&str>) -> Result<ShuruConfig> {
    let path = match config_flag {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from("shuru.json"),
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let mut cfg: ShuruConfig = serde_json::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
            // Secret providers run relative to the config, not to wherever
            // shuru happened to be invoked from.
            cfg.config_dir = path
                .canonicalize()
                .ok()
                .and_then(|p| p.parent().map(PathBuf::from));
            cfg.validate()
                .map_err(|e| anyhow::anyhow!("{}: {}", path.display(), e))?;
            Ok(cfg)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if config_flag.is_some() {
                bail!("Config file not found: {}", path.display());
            }
            Ok(ShuruConfig::default())
        }
        Err(e) => bail!("Failed to read {}: {}", path.display(), e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(json: &str) -> Result<ShuruConfig> {
        let cfg: ShuruConfig = serde_json::from_str(json)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// ShuruConfig has no Debug impl, so unwrap_err is unavailable.
    fn rejection(json: &str) -> String {
        config_from(json)
            .err()
            .expect("config should have been rejected")
            .to_string()
    }

    #[test]
    fn parses_duration_suffixes() {
        assert_eq!(parse_duration("45m").unwrap(), Duration::from_secs(2700));
        assert_eq!(parse_duration("3600s").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration(" 90 ").unwrap(), Duration::from_secs(90));

        assert!(parse_duration("").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("10d").is_err());
        assert!(parse_duration("-5m").is_err());
    }

    #[test]
    fn accepts_either_source() {
        let env = config_from(
            r#"{"secrets":{"API_KEY":{"from":"OPENAI_API_KEY","hosts":["api.openai.com"]}}}"#,
        )
        .unwrap();
        let proxy = env.to_proxy_config().unwrap();
        assert_eq!(
            proxy.secrets["API_KEY"].from.as_deref(),
            Some("OPENAI_API_KEY")
        );

        let command = config_from(
            r#"{"secrets":{"GH":{"command":["./mint.sh"],"ttl":"45m","hosts":["api.github.com"]}}}"#,
        )
        .unwrap();
        let proxy = command.to_proxy_config().unwrap();
        assert_eq!(
            proxy.secrets["GH"].command.as_deref(),
            Some(["./mint.sh".to_string()].as_slice())
        );
        assert_eq!(proxy.secrets["GH"].ttl, Some(Duration::from_secs(2700)));
    }

    #[test]
    fn rejects_ambiguous_or_incomplete_secrets() {
        let both = rejection(
            r#"{"secrets":{"GH":{"from":"TOKEN","command":["./mint.sh"],"hosts":["api.github.com"]}}}"#,
        );
        assert!(both.contains("not both"), "{both}");

        let neither = rejection(r#"{"secrets":{"GH":{"hosts":["api.github.com"]}}}"#);
        assert!(neither.contains("either 'from' or 'command'"), "{neither}");

        let empty = rejection(r#"{"secrets":{"GH":{"command":[],"hosts":["api.github.com"]}}}"#);
        assert!(empty.contains("empty"), "{empty}");
    }

    #[test]
    fn rejects_ttl_on_an_env_secret() {
        let err = rejection(
            r#"{"secrets":{"GH":{"from":"TOKEN","ttl":"45m","hosts":["api.github.com"]}}}"#,
        );
        assert!(err.contains("only applies to 'command'"), "{err}");
    }

    #[test]
    fn reports_the_offending_secret_by_name() {
        let err = rejection(
            r#"{"secrets":{"GITHUB_TOKEN":{"command":["./mint.sh"],"ttl":"soon","hosts":["api.github.com"]}}}"#,
        );
        assert!(err.contains("GITHUB_TOKEN"), "{err}");
    }
}
