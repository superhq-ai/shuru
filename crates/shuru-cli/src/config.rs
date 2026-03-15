use std::collections::HashMap;

use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Default, Deserialize)]
pub(crate) struct ShuruConfig {
    pub cpus: Option<usize>,
    pub memory: Option<u64>,
    pub disk_size: Option<u64>,
    pub allow_net: Option<bool>,
    pub ports: Option<Vec<String>>,
    pub mounts: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    pub secrets: Option<HashMap<String, SecretEntry>>,
    pub network: Option<NetworkEntry>,
}

/// A secret to inject via the proxy.
/// Example: `{ "value": "sk-your-openai-key", "hosts": ["api.openai.com"] }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretEntry {
    /// Literal secret value held on the host.
    pub value: String,
    /// Domains where this secret may be sent.
    pub hosts: Vec<String>,
}

/// Network access policy.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct NetworkEntry {
    /// Allowed domain patterns. Empty or absent = allow all.
    pub allow: Option<Vec<String>>,
}

impl ShuruConfig {
    /// Convert config sections into a ProxyConfig for shuru-proxy.
    pub fn to_proxy_config(&self) -> shuru_proxy::config::ProxyConfig {
        let mut proxy = shuru_proxy::config::ProxyConfig::default();

        if let Some(ref secrets) = self.secrets {
            for (name, entry) in secrets {
                proxy.secrets.insert(
                    name.clone(),
                    shuru_proxy::config::SecretConfig {
                        value: entry.value.clone(),
                        hosts: entry.hosts.clone(),
                    },
                );
            }
        }

        if let Some(ref network) = self.network {
            if let Some(ref allow) = network.allow {
                proxy.network.allow = allow.clone();
            }
        }

        proxy
    }
}

pub(crate) fn load_config(config_flag: Option<&str>) -> Result<ShuruConfig> {
    let path = match config_flag {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from("shuru.json"),
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let cfg: ShuruConfig = serde_json::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
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

    #[test]
    fn parses_literal_secret_entries() {
        let cfg: ShuruConfig = serde_json::from_str(
            r#"{
                "secrets": {
                    "API_KEY": {
                        "value": "sk-test",
                        "hosts": ["api.openai.com"]
                    }
                }
            }"#,
        )
        .expect("config should parse");

        let proxy = cfg.to_proxy_config();
        let secret = proxy.secrets.get("API_KEY").expect("secret should exist");
        assert_eq!(secret.value, "sk-test");
        assert_eq!(secret.hosts, vec!["api.openai.com"]);
    }

    #[test]
    fn rejects_legacy_from_secret_entries() {
        let err = serde_json::from_str::<ShuruConfig>(
            r#"{
                "secrets": {
                    "API_KEY": {
                        "from": "OPENAI_API_KEY",
                        "hosts": ["api.openai.com"]
                    }
                }
            }"#,
        )
        .expect_err("legacy secret schema should fail");

        assert!(err.to_string().contains("unknown field `from`"));
    }
}
