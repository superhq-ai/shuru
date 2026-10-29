//! Resolution of secret values, including provider commands that mint short
//! lived credentials and are re-run as those credentials expire.
//!
//! See docs/rfcs/0002-refreshable-secrets.md.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::{ProxyConfig, SecretConfig};

/// Re-mint a value once it comes within this long of expiring, so rotation
/// happens ahead of the first request that would have failed.
const REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// A provider that has not produced output in this long is killed.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on provider stderr carried into an error message.
const STDERR_LIMIT: usize = 4096;

/// Resolves secrets to their real values for substitution.
///
/// Environment backed secrets are read on every call, which is free and keeps
/// the pre-existing behaviour of picking up whatever the host env holds.
/// Command backed secrets are cached until they approach expiry.
pub struct SecretResolver {
    config: Arc<ProxyConfig>,
    placeholders: HashMap<String, String>,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    value: String,
    /// None means the provider reported no expiry, so the value is permanent.
    expires_at: Option<Instant>,
}

impl CacheEntry {
    /// Usable without re-running the provider.
    fn is_fresh(&self, now: Instant) -> bool {
        match self.expires_at {
            None => true,
            Some(at) => now + REFRESH_WINDOW < at,
        }
    }

    /// Past the refresh window but not yet dead, so still worth serving if a
    /// refresh attempt fails.
    fn is_usable(&self, now: Instant) -> bool {
        match self.expires_at {
            None => true,
            Some(at) => now < at,
        }
    }
}

/// A value obtained from a provider command.
struct Minted {
    value: String,
    expires_at: Option<Instant>,
}

/// The JSON a provider command writes to stdout.
#[derive(Deserialize)]
struct ProviderOutput {
    version: u32,
    value: String,
    #[serde(default)]
    expires_at: Option<String>,
}

impl SecretResolver {
    pub fn new(config: Arc<ProxyConfig>, placeholders: HashMap<String, String>) -> Self {
        Self {
            config,
            placeholders,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Placeholder to real value pairs for every secret bound to `domain`.
    ///
    /// Returns an error if a bound command backed secret cannot be resolved,
    /// so the caller can fail the connection rather than forward a placeholder
    /// that the upstream will reject with a confusing auth error.
    pub async fn substitutions_for_domain(&self, domain: &str) -> Result<Vec<(String, String)>> {
        let mut substitutions = Vec::new();
        for (name, secret) in &self.config.secrets {
            if !secret.matches_domain(domain) {
                continue;
            }
            let Some(placeholder) = self.placeholders.get(name) else {
                continue;
            };
            if let Some(value) = self.resolve(name, secret).await? {
                substitutions.push((placeholder.clone(), value));
            }
        }
        Ok(substitutions)
    }

    async fn resolve(&self, name: &str, secret: &SecretConfig) -> Result<Option<String>> {
        if let Some(value) = &secret.value {
            return Ok(Some(value.clone()));
        }
        if let Some(var) = &secret.from {
            // A missing variable skips substitution rather than failing the
            // connection, matching the behaviour before providers existed.
            return Ok(std::env::var(var).ok());
        }
        match &secret.command {
            Some(command) => self
                .resolve_command(name, command, secret.ttl)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Serve `name` from cache, running the provider when the cached value is
    /// missing or near expiry.
    ///
    /// The cache lock is deliberately held across the provider run. That makes
    /// concurrent connections to one expired secret wait on a single process
    /// instead of each spawning their own.
    async fn resolve_command(
        &self,
        name: &str,
        command: &[String],
        ttl: Option<Duration>,
    ) -> Result<String> {
        let mut cache = self.cache.lock().await;

        if let Some(entry) = cache.get(name) {
            if entry.is_fresh(Instant::now()) {
                return Ok(entry.value.clone());
            }
        }

        match run_provider(command, ttl, self.config.config_dir.as_deref()).await {
            Ok(minted) => {
                let value = minted.value.clone();
                cache.insert(
                    name.to_string(),
                    CacheEntry {
                        value: minted.value,
                        expires_at: minted.expires_at,
                    },
                );
                Ok(value)
            }
            Err(e) => {
                if let Some(entry) = cache.get(name) {
                    if entry.is_usable(Instant::now()) {
                        warn!("secret '{name}': refresh failed, serving cached value: {e:#}");
                        return Ok(entry.value.clone());
                    }
                }
                Err(e.context(format!("resolving secret '{name}'")))
            }
        }
    }
}

/// Run a provider command and parse what it minted.
async fn run_provider(
    command: &[String],
    ttl: Option<Duration>,
    cwd: Option<&std::path::Path>,
) -> Result<Minted> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("command is empty"))?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let output = tokio::time::timeout(PROVIDER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            anyhow!(
                "'{program}' timed out after {}s",
                PROVIDER_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("running '{program}'"))?;

    if !output.status.success() {
        bail!(
            "'{program}' exited with {}: {}",
            output.status,
            truncated_stderr(&output.stderr)
        );
    }

    parse_provider_output(&output.stdout, ttl, Instant::now())
        .with_context(|| format!("output of '{program}'"))
}

/// Parse provider stdout into a value and an absolute expiry.
///
/// `now` is taken by the caller so the expiry lands on the same clock reading
/// used elsewhere in the resolve, and so tests can pin it.
fn parse_provider_output(stdout: &[u8], ttl: Option<Duration>, now: Instant) -> Result<Minted> {
    // Parse failures never quote stdout: it holds the credential.
    let parsed: ProviderOutput =
        serde_json::from_slice(stdout).map_err(|e| anyhow!("not valid JSON ({e})"))?;

    if parsed.version != 1 {
        bail!("unsupported version {}, expected 1", parsed.version);
    }

    let value = parsed.value.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        bail!("value is empty");
    }

    let expires_at = match parsed.expires_at {
        Some(ref stamp) => Some(now + remaining_until(stamp)?),
        None => ttl.map(|ttl| now + ttl),
    };

    Ok(Minted { value, expires_at })
}

/// Time left until an RFC3339 instant, as of now.
fn remaining_until(stamp: &str) -> Result<Duration> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    let expires = OffsetDateTime::parse(stamp, &Rfc3339)
        .map_err(|e| anyhow!("expires_at '{stamp}' is not RFC3339 ({e})"))?;
    let remaining = expires - OffsetDateTime::now_utc();
    if !remaining.is_positive() {
        bail!("expires_at '{stamp}' is already in the past");
    }
    Ok(remaining.unsigned_abs())
}

fn truncated_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "<no stderr>".to_string();
    }
    match text.char_indices().nth(STDERR_LIMIT) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minted(json: &str, ttl: Option<Duration>) -> Result<Minted> {
        parse_provider_output(json.as_bytes(), ttl, Instant::now())
    }

    #[test]
    fn parses_value_and_expiry() {
        let far_future = "2099-01-01T00:00:00Z";
        let out = minted(
            &format!(r#"{{"version":1,"value":"ghs_abc","expires_at":"{far_future}"}}"#),
            None,
        )
        .unwrap();
        assert_eq!(out.value, "ghs_abc");
        assert!(out.expires_at.is_some());
    }

    #[test]
    fn trailing_newline_is_trimmed() {
        let out = minted(r#"{"version":1,"value":"ghs_abc\n"}"#, None).unwrap();
        assert_eq!(out.value, "ghs_abc");
    }

    #[test]
    fn expiry_falls_back_to_ttl_then_to_never() {
        let with_ttl = minted(
            r#"{"version":1,"value":"v"}"#,
            Some(Duration::from_secs(600)),
        )
        .unwrap();
        assert!(with_ttl.expires_at.is_some());

        let without_ttl = minted(r#"{"version":1,"value":"v"}"#, None).unwrap();
        assert!(without_ttl.expires_at.is_none());
    }

    #[test]
    fn reported_expiry_wins_over_ttl() {
        // A ttl of an hour against a reported expiry a few seconds out: the
        // reported one must win, so the entry is already inside the window.
        let soon = time::OffsetDateTime::now_utc() + time::Duration::seconds(5);
        let stamp = soon
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let now = Instant::now();
        let out = parse_provider_output(
            format!(r#"{{"version":1,"value":"v","expires_at":"{stamp}"}}"#).as_bytes(),
            Some(Duration::from_secs(3600)),
            now,
        )
        .unwrap();
        assert!(out.expires_at.unwrap() < now + Duration::from_secs(60));
    }

    #[test]
    fn rejects_bad_output() {
        assert!(minted(r#"{"version":2,"value":"v"}"#, None).is_err());
        assert!(minted(r#"{"version":1,"value":""}"#, None).is_err());
        assert!(minted(r#"{"version":1}"#, None).is_err());
        assert!(minted("not json at all", None).is_err());
        assert!(minted(
            r#"{"version":1,"value":"v","expires_at":"yesterday"}"#,
            None
        )
        .is_err());
        assert!(minted(
            r#"{"version":1,"value":"v","expires_at":"2000-01-01T00:00:00Z"}"#,
            None
        )
        .is_err());
    }

    #[test]
    fn parse_error_never_quotes_stdout() {
        let secret = "ghs_supersecretvalue";
        // `.err()` rather than `unwrap_err()`: Minted has no Debug impl, by
        // design, so a credential cannot reach a panic message.
        let err = minted(&format!(r#"{{"version":9,"value":"{secret}"}}"#), None)
            .err()
            .unwrap()
            .to_string();
        assert!(!err.contains(secret), "error leaked the credential: {err}");
    }

    #[test]
    fn cache_entry_freshness() {
        let now = Instant::now();

        let permanent = CacheEntry {
            value: "v".into(),
            expires_at: None,
        };
        assert!(permanent.is_fresh(now) && permanent.is_usable(now));

        let inside_window = CacheEntry {
            value: "v".into(),
            expires_at: Some(now + Duration::from_secs(30)),
        };
        assert!(!inside_window.is_fresh(now));
        assert!(inside_window.is_usable(now));

        let dead = CacheEntry {
            value: "v".into(),
            expires_at: Some(now - Duration::from_secs(1)),
        };
        assert!(!dead.is_fresh(now) && !dead.is_usable(now));

        let healthy = CacheEntry {
            value: "v".into(),
            expires_at: Some(now + Duration::from_secs(3600)),
        };
        assert!(healthy.is_fresh(now));
    }

    /// A provider script plus the file it records its call count in.
    struct TestProvider {
        dir: std::path::PathBuf,
        script: std::path::PathBuf,
        counter: std::path::PathBuf,
    }

    impl TestProvider {
        fn new(body: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SEQ: AtomicU32 = AtomicU32::new(0);

            let dir = std::env::temp_dir().join(format!(
                "shuru-secrets-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();

            let script = dir.join("provider.sh");
            std::fs::write(&script, body).unwrap();

            Self {
                counter: dir.join("count"),
                script,
                dir,
            }
        }

        /// argv for a secret backed by this provider, minting values that
        /// expire at `stamp`. The shell reads the script: executing it can
        /// fail with ETXTBSY while a test on another thread forks with the
        /// script still open for writing.
        fn argv(&self, stamp: &str) -> Vec<String> {
            vec![
                "/bin/sh".to_string(),
                self.script.to_str().unwrap().to_string(),
                self.counter.to_str().unwrap().to_string(),
                stamp.to_string(),
            ]
        }

        fn calls(&self) -> u32 {
            std::fs::read_to_string(&self.counter)
                .map(|s| s.trim().parse().unwrap())
                .unwrap_or(0)
        }
    }

    impl Drop for TestProvider {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Mints token-N on the Nth call, expiring at the stamp in argv.
    const COUNTING_PROVIDER: &str = r#"#!/bin/sh
n=$(cat "$1" 2>/dev/null || echo 0)
n=$((n + 1))
printf '%s' "$n" > "$1"
printf '{"version":1,"value":"token-%s","expires_at":"%s"}' "$n" "$2"
"#;

    /// Succeeds once, then fails every later call.
    const FAILS_AFTER_FIRST: &str = r#"#!/bin/sh
n=$(cat "$1" 2>/dev/null || echo 0)
n=$((n + 1))
printf '%s' "$n" > "$1"
if [ "$n" -gt 1 ]; then
  echo "mint failed" >&2
  exit 1
fi
printf '{"version":1,"value":"token-%s","expires_at":"%s"}' "$n" "$2"
"#;

    const ALWAYS_FAILS: &str = r#"#!/bin/sh
echo "no credentials available" >&2
exit 1
"#;

    /// Far enough out to stay fresh for the whole test.
    const STABLE: &str = "2099-01-01T00:00:00Z";

    /// Inside the refresh window, so every resolve re-mints.
    fn expiring_soon() -> String {
        let soon = time::OffsetDateTime::now_utc() + time::Duration::seconds(30);
        soon.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn resolver_for(secret: SecretConfig) -> SecretResolver {
        let mut config = ProxyConfig::default();
        config.secrets.insert("TOKEN".to_string(), secret);
        let placeholders =
            HashMap::from([("TOKEN".to_string(), "shuru_tok_placeholder".to_string())]);
        SecretResolver::new(Arc::new(config), placeholders)
    }

    async fn resolve_one(resolver: &SecretResolver, domain: &str) -> Result<Vec<(String, String)>> {
        resolver.substitutions_for_domain(domain).await
    }

    #[tokio::test]
    async fn fresh_value_is_served_from_cache() {
        let provider = TestProvider::new(COUNTING_PROVIDER);
        let resolver = resolver_for(SecretConfig::from_command(
            provider.argv(STABLE),
            vec!["api.github.com".into()],
        ));

        let first = resolve_one(&resolver, "api.github.com").await.unwrap();
        let second = resolve_one(&resolver, "api.github.com").await.unwrap();

        assert_eq!(
            first,
            vec![("shuru_tok_placeholder".into(), "token-1".into())]
        );
        assert_eq!(second, first);
        assert_eq!(provider.calls(), 1, "cached value should not re-run");
    }

    #[tokio::test]
    async fn value_near_expiry_is_reminted() {
        let provider = TestProvider::new(COUNTING_PROVIDER);
        let resolver = resolver_for(SecretConfig::from_command(
            provider.argv(&expiring_soon()),
            vec!["api.github.com".into()],
        ));

        let first = resolve_one(&resolver, "api.github.com").await.unwrap();
        let second = resolve_one(&resolver, "api.github.com").await.unwrap();

        assert_eq!(first[0].1, "token-1");
        assert_eq!(second[0].1, "token-2");
        assert_eq!(first[0].0, second[0].0, "placeholder must not change");
        assert_eq!(provider.calls(), 2);
    }

    #[tokio::test]
    async fn concurrent_resolves_mint_once() {
        let provider = TestProvider::new(COUNTING_PROVIDER);
        let resolver = Arc::new(resolver_for(SecretConfig::from_command(
            provider.argv(STABLE),
            vec!["api.github.com".into()],
        )));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let resolver = resolver.clone();
            tasks.push(tokio::spawn(async move {
                resolver
                    .substitutions_for_domain("api.github.com")
                    .await
                    .unwrap()
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap()[0].1, "token-1");
        }

        assert_eq!(
            provider.calls(),
            1,
            "single flight should collapse the burst"
        );
    }

    #[tokio::test]
    async fn failing_provider_fails_closed() {
        let provider = TestProvider::new(ALWAYS_FAILS);
        let resolver = resolver_for(SecretConfig::from_command(
            provider.argv(STABLE),
            vec!["api.github.com".into()],
        ));

        let err = resolve_one(&resolver, "api.github.com")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("TOKEN"), "error should name the secret: {err}");
    }

    #[tokio::test]
    async fn failed_refresh_falls_back_to_a_live_cached_value() {
        let provider = TestProvider::new(FAILS_AFTER_FIRST);
        let resolver = resolver_for(SecretConfig::from_command(
            provider.argv(&expiring_soon()),
            vec!["api.github.com".into()],
        ));

        let first = resolve_one(&resolver, "api.github.com").await.unwrap();
        let second = resolve_one(&resolver, "api.github.com").await.unwrap();

        assert_eq!(first[0].1, "token-1");
        assert_eq!(second[0].1, "token-1", "should serve the still-live value");
        assert_eq!(provider.calls(), 2, "refresh should have been attempted");
    }

    #[tokio::test]
    async fn unbound_domain_never_runs_the_provider() {
        let provider = TestProvider::new(COUNTING_PROVIDER);
        let resolver = resolver_for(SecretConfig::from_command(
            provider.argv(STABLE),
            vec!["api.github.com".into()],
        ));

        assert!(resolve_one(&resolver, "evil.example.com")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn missing_env_var_skips_substitution() {
        let resolver = resolver_for(SecretConfig::from_env(
            "SHURU_TEST_VAR_THAT_DOES_NOT_EXIST",
            vec!["api.openai.com".into()],
        ));

        assert!(resolve_one(&resolver, "api.openai.com")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn empty_command_is_an_error_not_a_panic() {
        let resolver = resolver_for(SecretConfig::from_command(
            Vec::new(),
            vec!["api.github.com".into()],
        ));
        assert!(resolve_one(&resolver, "api.github.com").await.is_err());
    }

    #[test]
    fn stderr_is_truncated() {
        let long = "x".repeat(STDERR_LIMIT * 2);
        let out = truncated_stderr(long.as_bytes());
        assert!(out.ends_with("..."));
        assert!(out.len() < long.len());
        assert_eq!(truncated_stderr(b"  "), "<no stderr>");
    }
}
