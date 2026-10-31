//! What a sandbox is given besides its disk: env, plain values handed to
//! every exec as they are, and secrets, which the guest only sees as
//! placeholders that the proxy swaps for the value on requests to the
//! bound hosts. Kept in `config.json` in the sandbox directory, the one
//! place a secret value is written.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shuru_node::control;

use super::http::ApiError;

pub const CONFIG: &str = "config.json";
const MAX_NAME: usize = 128;
const MAX_VALUE: usize = 32 * 1024;
const MAX_HOSTS: usize = 20;
/// Names, values and hosts in all, so the env still fits an exec frame and
/// the secrets the netd configuration, next to the CA bundle.
const MAX_TOTAL: usize = 256 * 1024;

#[derive(Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: BTreeMap<String, BoundSecret>,
}

#[derive(Serialize, Deserialize)]
pub struct BoundSecret {
    pub value: String,
    /// Lowercase, each a domain or `*.` and a domain.
    pub hosts: Vec<String>,
}

impl SandboxConfig {
    /// The `env` and `secrets` of a request body, checked. No message
    /// carries a value, and none carries a name before it is checked.
    pub fn parse(env: Option<Value>, secrets: Option<Value>) -> Result<Self, ApiError> {
        let config = SandboxConfig {
            env: match env {
                Some(env) => parse_env(env)?,
                None => BTreeMap::new(),
            },
            secrets: match secrets {
                Some(secrets) => parse_secrets(secrets)?,
                None => BTreeMap::new(),
            },
        };
        let env = config
            .env
            .iter()
            .map(|(name, value)| name.len() + value.len());
        let secrets = config.secrets.iter().map(|(name, secret)| {
            name.len() + secret.value.len() + secret.hosts.iter().map(String::len).sum::<usize>()
        });
        if env.chain(secrets).sum::<usize>() > MAX_TOTAL {
            return Err(ApiError::bad_request(format!(
                "env and secrets: over {} KiB in all",
                MAX_TOTAL / 1024
            )));
        }
        Ok(config)
    }

    /// The secrets as netd takes them.
    pub fn netd_secrets(&self) -> HashMap<String, control::Secret> {
        self.secrets
            .iter()
            .map(|(name, secret)| {
                let secret = control::Secret {
                    value: secret.value.clone(),
                    hosts: secret.hosts.clone(),
                };
                (name.clone(), secret)
            })
            .collect()
    }
}

fn parse_env(env: Value) -> Result<BTreeMap<String, String>, ApiError> {
    let Value::Object(env) = env else {
        return Err(ApiError::bad_request(
            "env: expected an object of names to values",
        ));
    };
    env.into_iter()
        .map(|(name, value)| {
            check_name("env", &name)?;
            let Value::String(value) = value else {
                return Err(ApiError::bad_request(format!(
                    "env.{name}: expected a string"
                )));
            };
            check_value(&format!("env.{name}"), &value)?;
            Ok((name, value))
        })
        .collect()
}

fn parse_secrets(secrets: Value) -> Result<BTreeMap<String, BoundSecret>, ApiError> {
    let Value::Object(secrets) = secrets else {
        return Err(ApiError::bad_request(
            "secrets: expected an object of names to secrets",
        ));
    };
    secrets
        .into_iter()
        .map(|(name, secret)| {
            check_name("secrets", &name)?;
            let at = format!("secrets.{name}");
            let Value::Object(mut fields) = secret else {
                return Err(ApiError::bad_request(format!(
                    "{at}: expected an object with value and hosts"
                )));
            };
            let value = match fields.remove("value") {
                Some(Value::String(value)) => value,
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "{at}.value: expected a string"
                    )))
                }
            };
            check_value(&format!("{at}.value"), &value)?;
            let hosts = match fields.remove("hosts") {
                Some(Value::Array(hosts)) => hosts,
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "{at}.hosts: expected a list of hosts"
                    )))
                }
            };
            if !(1..=MAX_HOSTS).contains(&hosts.len()) {
                return Err(ApiError::bad_request(format!(
                    "{at}.hosts: expected 1 to {MAX_HOSTS} hosts"
                )));
            }
            let hosts = hosts
                .into_iter()
                .map(|host| match host {
                    Value::String(host) if valid_host(&host) => Ok(host.to_ascii_lowercase()),
                    _ => Err(ApiError::bad_request(format!(
                        "{at}.hosts: a host is a domain, such as api.example.com, or *. and a domain"
                    ))),
                })
                .collect::<Result<_, _>>()?;
            if !fields.is_empty() {
                return Err(ApiError::bad_request(format!(
                    "{at}: expected only value and hosts"
                )));
            }
            Ok((name, BoundSecret { value, hosts }))
        })
        .collect()
}

fn check_name(what: &str, name: &str) -> Result<(), ApiError> {
    if valid_name(name) {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "{what}: a name is 1 to {MAX_NAME} of A to Z, digits and _, not starting with a digit"
    )))
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=MAX_NAME).contains(&bytes.len())
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

fn check_value(at: &str, value: &str) -> Result<(), ApiError> {
    if value.len() > MAX_VALUE || value.contains('\0') {
        return Err(ApiError::bad_request(format!(
            "{at}: a value is at most {} KiB, without NUL",
            MAX_VALUE / 1024
        )));
    }
    Ok(())
}

/// A domain of two labels or more, or `*.` and one.
fn valid_host(host: &str) -> bool {
    let domain = host.strip_prefix("*.").unwrap_or(host);
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// The env of an exec: the sandbox env, then the placeholders of its
/// secrets, then the env of the request, each over the one before.
pub fn exec_env(
    sandbox: &BTreeMap<String, String>,
    placeholders: &HashMap<String, String>,
    request: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = sandbox
        .iter()
        .chain(placeholders)
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    env.extend(request);
    env
}

/// The config in `dir`, empty for a sandbox made before configs were kept.
pub fn read(dir: &Path) -> Result<SandboxConfig> {
    let file = match File::open(dir.join(CONFIG)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(SandboxConfig::default()),
        Err(e) => return Err(anyhow!("{CONFIG}: {e}")),
    };
    // Only where it failed: the message serde gives can quote a value.
    serde_json::from_reader(BufReader::new(file)).map_err(|e| {
        anyhow!(
            "{CONFIG} is not valid at line {} column {}",
            e.line(),
            e.column()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(env: Value, secrets: Value) -> Result<SandboxConfig, ApiError> {
        SandboxConfig::parse(Some(env), Some(secrets))
    }

    #[test]
    fn names() {
        for name in ["A", "_", "PATH", "_X1", "A_B_9", &"A".repeat(128)] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "a", "1A", "A-B", "A.B", "Ä", "A B", &"A".repeat(129)] {
            assert!(!valid_name(name), "{name}");
        }
    }

    #[test]
    fn hosts() {
        for host in [
            "api.openai.com",
            "*.example.com",
            "a-b.c0.io",
            "API.Example.com",
        ] {
            assert!(valid_host(host), "{host}");
        }
        for host in [
            "",
            "localhost",
            "*",
            "*.com",
            "**.example.com",
            "a.*.com",
            "-a.com",
            "a-.com",
            "a..com",
            "example.com.",
            "a_b.com",
            "https://a.com",
            "a.com:443",
            &format!("{}.com", "a".repeat(64)),
        ] {
            assert!(!valid_host(host), "{host}");
        }
    }

    #[test]
    fn a_config_is_checked() {
        let config = parse(
            json!({ "B": "2", "A": "1" }),
            json!({ "TOKEN": { "value": "s3cret", "hosts": ["API.example.com", "*.example.org"] } }),
        )
        .unwrap();
        assert_eq!(config.env.keys().collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(
            config.secrets["TOKEN"].hosts,
            ["api.example.com", "*.example.org"]
        );
        assert!(SandboxConfig::parse(None, None).unwrap().env.is_empty());

        let refused = [
            (json!([]), json!({})),
            (json!({ "a": "1" }), json!({})),
            (json!({ "A": 1 }), json!({})),
            (json!({ "A": "x".repeat(MAX_VALUE + 1) }), json!({})),
            (json!({ "A": "a\u{0}b" }), json!({})),
            (json!({}), json!({ "T": "s3cret" })),
            (json!({}), json!({ "T": { "value": "s3cret" } })),
            (
                json!({}),
                json!({ "T": { "value": "s3cret", "hosts": [] } }),
            ),
            (
                json!({}),
                json!({ "T": { "value": 7, "hosts": ["a.com"] } }),
            ),
            (
                json!({}),
                json!({ "T": { "value": "s3cret", "hosts": ["localhost"] } }),
            ),
            (
                json!({}),
                json!({ "T": { "value": "s3cret", "hosts": vec!["a.com"; MAX_HOSTS + 1] } }),
            ),
            (
                json!({}),
                json!({ "T": { "value": "s3cret", "hosts": ["a.com"], "host": "b.com" } }),
            ),
        ];
        for (env, secrets) in refused {
            let e = parse(env.clone(), secrets.clone()).err().unwrap();
            assert_eq!(e.code(), "bad_request", "{env} {secrets}");
            assert!(!e.message().contains("s3cret"), "{}", e.message());
        }
    }

    #[test]
    fn the_total_is_bounded() {
        let env: serde_json::Map<String, Value> = (0..9)
            .map(|i| (format!("V{i}"), json!("x".repeat(MAX_VALUE))))
            .collect();
        let e = parse(Value::Object(env), json!({})).err().unwrap();
        assert_eq!(e.code(), "bad_request");
    }

    #[test]
    fn a_config_round_trips() {
        let config = parse(
            json!({ "A": "1" }),
            json!({ "T": { "value": "s3cret", "hosts": ["a.example.com"] } }),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("shuru-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read(&dir).unwrap().secrets.is_empty());
        std::fs::write(dir.join(CONFIG), serde_json::to_vec(&config).unwrap()).unwrap();
        let read = read(&dir).unwrap();
        assert_eq!(read.env["A"], "1");
        assert_eq!(read.secrets["T"].value, "s3cret");
        assert_eq!(read.secrets["T"].hosts, ["a.example.com"]);
        let netd = read.netd_secrets();
        assert_eq!(netd["T"].value, "s3cret");

        std::fs::write(dir.join(CONFIG), br#"{"env":"s3cret"}"#).unwrap();
        let e = super::read(&dir).err().unwrap();
        assert!(!format!("{e:#}").contains("s3cret"), "{e:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exec_env_order() {
        let sandbox = BTreeMap::from([
            ("A".to_string(), "sandbox".to_string()),
            ("B".to_string(), "sandbox".to_string()),
            ("C".to_string(), "sandbox".to_string()),
        ]);
        let placeholders = HashMap::from([
            ("B".to_string(), "placeholder".to_string()),
            ("C".to_string(), "placeholder".to_string()),
        ]);
        let request = HashMap::from([
            ("C".to_string(), "request".to_string()),
            ("D".to_string(), "request".to_string()),
        ]);
        let env = exec_env(&sandbox, &placeholders, request);
        let mut env: Vec<_> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        env.sort();
        assert_eq!(
            env,
            [
                ("A", "sandbox"),
                ("B", "placeholder"),
                ("C", "request"),
                ("D", "request"),
            ]
        );
    }
}
