//! Resolution of secret values, including provider commands and a provider
//! of the embedder that mint short lived credentials, fetched again as those
//! credentials expire.
//!
//! See docs/rfcs/0002-refreshable-secrets.md and 0013-live-secrets.md.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::{ProxyConfig, Refused, Scope, SecretConfig, SecretProvider};

/// Mint a command value again once it comes within this long of expiring,
/// so rotation happens ahead of the first request that would have failed.
const COMMAND_REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// The same for a provided value, which crosses more on its way.
const PROVIDED_REFRESH_WINDOW: Duration = Duration::from_secs(300);

/// After a failed mint, how long before another is tried.
const RETRY_PAUSE: Duration = Duration::from_secs(30);

/// How long a provided value without an expiry is kept.
const PROVIDED_MAX_AGE: Duration = Duration::from_secs(3600);

/// A provider that has not produced output in this long is killed.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on provider stderr carried into an error message.
const STDERR_LIMIT: usize = 4096;

/// Resolves secrets to their real values for substitution.
///
/// Environment backed secrets are read on every call, which is free and keeps
/// the pre-existing behaviour of picking up whatever the host env holds.
/// Minted secrets are cached until they approach expiry.
pub struct SecretResolver {
    table: RwLock<Arc<HashMap<String, SecretConfig>>>,
    placeholders: HashMap<String, String>,
    provider: Option<Arc<dyn SecretProvider>>,
    config_dir: Option<PathBuf>,
    /// One slot per minted secret, so a mint holds up only its own secret.
    slots: std::sync::Mutex<HashMap<String, Arc<Mutex<Slot>>>>,
    #[cfg(test)]
    skew: std::sync::Mutex<Duration>,
}

#[derive(Default)]
struct Slot {
    entry: Option<CacheEntry>,
    /// When the last mint failed, if it did since the last success.
    failed_at: Option<Instant>,
}

struct CacheEntry {
    value: String,
    /// None means the value is permanent.
    expires_at: Option<Instant>,
    /// How long before `expires_at` a new value is minted.
    window: Duration,
}

impl CacheEntry {
    /// `window` at most, and never more than half the lifetime, so a value
    /// that lives a few minutes is not minted again on every connection.
    fn new(value: String, now: Instant, expires_at: Option<Instant>, window: Duration) -> Self {
        let window = match expires_at {
            Some(at) => window.min(at.saturating_duration_since(now) / 2),
            None => window,
        };
        Self {
            value,
            expires_at,
            window,
        }
    }

    /// Usable without minting again.
    fn is_fresh(&self, now: Instant) -> bool {
        match self.expires_at {
            None => true,
            Some(at) => now + self.window < at,
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

/// A placeholder, the value it stands for and where it may be swapped.
/// No Debug, so a value cannot reach a log.
pub(crate) struct Substitution {
    pub placeholder: String,
    pub value: String,
    pub scope: Scope,
}

/// A value obtained from a provider command or the provider.
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
    pub fn new(config: &ProxyConfig, placeholders: HashMap<String, String>) -> Self {
        Self {
            table: RwLock::new(Arc::new(config.secrets.clone())),
            placeholders,
            provider: config.provider.clone(),
            config_dir: config.config_dir.clone(),
            slots: std::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            skew: std::sync::Mutex::new(Duration::ZERO),
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        return Instant::now() + *self.skew.lock().unwrap();
        #[cfg(not(test))]
        Instant::now()
    }

    #[cfg(test)]
    fn advance(&self, by: Duration) {
        *self.skew.lock().unwrap() += by;
    }

    /// Replaces the secrets. Those the guest has no placeholder for wait for
    /// the next boot and are returned by name; values cached for secrets that
    /// changed or went are dropped. Connections pick the new table up from
    /// their next resolve.
    pub fn update(&self, secrets: HashMap<String, SecretConfig>) -> Vec<String> {
        let mut pending: Vec<String> = secrets
            .keys()
            .filter(|name| !self.placeholders.contains_key(*name))
            .cloned()
            .collect();
        pending.sort();
        let secrets: HashMap<_, _> = secrets
            .into_iter()
            .filter(|(name, _)| self.placeholders.contains_key(name))
            .collect();
        let mut table = self.table.write().unwrap_or_else(|e| e.into_inner());
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|name, _| {
                secrets
                    .get(name)
                    .is_some_and(|new| table.get(name) == Some(new))
            });
        *table = Arc::new(secrets);
        pending
    }

    /// Placeholder to real value pairs for every secret bound to `domain`.
    ///
    /// Returns an error if a bound minted secret cannot be resolved, so the
    /// caller can fail the connection rather than forward a placeholder that
    /// the upstream will reject with a confusing auth error.
    pub(crate) async fn substitutions_for_domain(&self, domain: &str) -> Result<Vec<Substitution>> {
        let table = self.table.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut substitutions = Vec::new();
        for (name, secret) in table.iter() {
            if !secret.matches_domain(domain) {
                continue;
            }
            let Some(placeholder) = self.placeholders.get(name) else {
                continue;
            };
            if let Some(value) = self.resolve(name, secret).await? {
                substitutions.push(Substitution {
                    placeholder: placeholder.clone(),
                    value,
                    scope: secret.scope,
                });
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
        if secret.provided {
            let Some(provider) = &self.provider else {
                bail!("resolving secret '{name}': no provider");
            };
            return self
                .cached(name, PROVIDED_REFRESH_WINDOW, |now| async move {
                    let provided = provider.provide(name).await?;
                    if provided.value.is_empty() {
                        bail!("the provider gave an empty value");
                    }
                    // It goes into a header, where a line break would end it.
                    if provided.value.bytes().any(|b| b < 0x20 || b == 0x7f) {
                        bail!("the provider gave a value with control characters");
                    }
                    let lives = provided
                        .expires_in
                        .unwrap_or(PROVIDED_MAX_AGE)
                        .min(PROVIDED_MAX_AGE);
                    Ok(Minted {
                        value: provided.value,
                        expires_at: Some(now + lives),
                    })
                })
                .await
                .map(Some);
        }
        match &secret.command {
            Some(command) => self
                .cached(name, COMMAND_REFRESH_WINDOW, |now| {
                    run_provider(command, secret.ttl, self.config_dir.as_deref(), now)
                })
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    fn slot(&self, name: &str) -> Arc<Mutex<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    /// Serve `name` from its slot, minting when the cached value is missing
    /// or near expiry.
    ///
    /// The slot lock is held across the mint, so concurrent connections to
    /// one expired secret wait on a single mint instead of each running their
    /// own. After a failure no mint is tried for `RETRY_PAUSE`: a live value
    /// is served meanwhile, and without one the connection fails at once.
    async fn cached<F, Fut>(&self, name: &str, window: Duration, mint: F) -> Result<String>
    where
        F: FnOnce(Instant) -> Fut,
        Fut: std::future::Future<Output = Result<Minted>>,
    {
        let slot = self.slot(name);
        let mut slot = slot.lock().await;
        let now = self.now();

        if let Some(entry) = &slot.entry {
            if entry.is_fresh(now) {
                return Ok(entry.value.clone());
            }
        }
        if let Some(failed) = slot.failed_at {
            if now < failed + RETRY_PAUSE {
                if let Some(entry) = slot.entry.as_ref().filter(|e| e.is_usable(now)) {
                    return Ok(entry.value.clone());
                }
                bail!(
                    "resolving secret '{name}': the last attempt failed {} s ago",
                    now.duration_since(failed).as_secs()
                );
            }
        }

        match mint(now).await {
            Ok(minted) => {
                let value = minted.value.clone();
                slot.entry = Some(CacheEntry::new(
                    minted.value,
                    now,
                    minted.expires_at,
                    window,
                ));
                slot.failed_at = None;
                Ok(value)
            }
            Err(e) => {
                slot.failed_at = Some(now);
                if e.downcast_ref::<Refused>().is_some() {
                    slot.entry = None;
                }
                if let Some(entry) = slot.entry.as_ref().filter(|e| e.is_usable(now)) {
                    warn!("secret '{name}': refresh failed, serving cached value: {e:#}");
                    return Ok(entry.value.clone());
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
    now: Instant,
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

    parse_provider_output(&output.stdout, ttl, now)
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
        let entry =
            |expires_at| CacheEntry::new("v".into(), now, expires_at, COMMAND_REFRESH_WINDOW);

        let permanent = entry(None);
        assert!(permanent.is_fresh(now) && permanent.is_usable(now));

        let hour = entry(Some(now + Duration::from_secs(3600)));
        assert!(hour.is_fresh(now));
        assert!(hour.is_fresh(now + Duration::from_secs(3539)));
        assert!(!hour.is_fresh(now + Duration::from_secs(3541)));
        assert!(hour.is_usable(now + Duration::from_secs(3599)));
        assert!(!hour.is_usable(now + Duration::from_secs(3600)));
    }

    #[test]
    fn the_window_is_at_most_half_the_lifetime() {
        let now = Instant::now();
        let short = CacheEntry::new(
            "v".into(),
            now,
            Some(now + Duration::from_secs(120)),
            PROVIDED_REFRESH_WINDOW,
        );
        assert!(
            short.is_fresh(now),
            "a new value is fresh, however short lived"
        );
        assert!(short.is_fresh(now + Duration::from_secs(59)));
        assert!(!short.is_fresh(now + Duration::from_secs(61)));
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

    /// Inside the refresh window once 20 seconds have passed.
    fn expiring_soon() -> String {
        let soon = time::OffsetDateTime::now_utc() + time::Duration::seconds(30);
        soon.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn resolver_for(secret: SecretConfig) -> SecretResolver {
        resolver_with(secret, None)
    }

    fn resolver_with(
        secret: SecretConfig,
        provider: Option<Arc<dyn SecretProvider>>,
    ) -> SecretResolver {
        let mut config = ProxyConfig {
            provider,
            ..Default::default()
        };
        config.secrets.insert("TOKEN".to_string(), secret);
        let placeholders =
            HashMap::from([("TOKEN".to_string(), "shuru_tok_placeholder".to_string())]);
        SecretResolver::new(&config, placeholders)
    }

    async fn resolve_one(resolver: &SecretResolver, domain: &str) -> Result<Vec<(String, String)>> {
        Ok(pairs(resolver.substitutions_for_domain(domain).await?))
    }

    fn pairs(substitutions: Vec<Substitution>) -> Vec<(String, String)> {
        substitutions
            .into_iter()
            .map(|s| (s.placeholder, s.value))
            .collect()
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
        let again = resolve_one(&resolver, "api.github.com").await.unwrap();
        resolver.advance(Duration::from_secs(20));
        let second = resolve_one(&resolver, "api.github.com").await.unwrap();

        assert_eq!(first[0].1, "token-1");
        assert_eq!(again[0].1, "token-1", "fresh for half its life");
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
                pairs(
                    resolver
                        .substitutions_for_domain("api.github.com")
                        .await
                        .unwrap(),
                )
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
        resolver.advance(Duration::from_secs(20));
        let second = resolve_one(&resolver, "api.github.com").await.unwrap();
        let third = resolve_one(&resolver, "api.github.com").await.unwrap();

        assert_eq!(first[0].1, "token-1");
        assert_eq!(second[0].1, "token-1", "should serve the still-live value");
        assert_eq!(third[0].1, "token-1");
        assert_eq!(provider.calls(), 2, "one refresh, then a pause");

        // Dead now, and still inside the pause: refused without a mint.
        resolver.advance(Duration::from_secs(15));
        assert!(resolve_one(&resolver, "api.github.com").await.is_err());
        assert_eq!(provider.calls(), 2);

        resolver.advance(Duration::from_secs(20));
        assert!(resolve_one(&resolver, "api.github.com").await.is_err());
        assert_eq!(provider.calls(), 3, "tried again after the pause");
    }

    /// Provides `value-N` on the Nth call, for an hour, or fails while
    /// `failing` is set, or refuses while `refusing` is.
    #[derive(Debug, Default)]
    struct FakeProvider {
        calls: std::sync::atomic::AtomicU32,
        failing: std::sync::atomic::AtomicBool,
        refusing: std::sync::atomic::AtomicBool,
        value: std::sync::Mutex<Option<String>>,
        lives: Option<Duration>,
        delay: Duration,
    }

    impl SecretProvider for FakeProvider {
        fn provide<'a>(
            &'a self,
            name: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<crate::config::Provided>> + Send + 'a>,
        > {
            use std::sync::atomic::Ordering;
            Box::pin(async move {
                assert_eq!(name, "TOKEN");
                tokio::time::sleep(self.delay).await;
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if self.failing.load(Ordering::SeqCst) {
                    bail!("the provider is down");
                }
                if self.refusing.load(Ordering::SeqCst) {
                    return Err(Refused("not connected".into()).into());
                }
                let value = self.value.lock().unwrap().clone();
                Ok(crate::config::Provided {
                    value: value.unwrap_or_else(|| format!("value-{n}")),
                    expires_in: self.lives.or(Some(Duration::from_secs(3600))),
                })
            })
        }
    }

    fn provided() -> SecretConfig {
        SecretConfig::from_provider(vec!["api.github.com".into()])
    }

    #[tokio::test]
    async fn provided_value_is_minted_once_for_concurrent_connections() {
        let provider = Arc::new(FakeProvider {
            delay: Duration::from_millis(50),
            ..Default::default()
        });
        let resolver = Arc::new(resolver_with(provided(), Some(provider.clone())));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let resolver = resolver.clone();
            tasks.push(tokio::spawn(async move {
                pairs(
                    resolver
                        .substitutions_for_domain("api.github.com")
                        .await
                        .unwrap(),
                )
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap()[0].1, "value-1");
        }
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provided_value_is_minted_again_five_minutes_before_it_expires() {
        let provider = Arc::new(FakeProvider::default());
        let resolver = resolver_with(provided(), Some(provider.clone()));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-1"
        );
        resolver.advance(Duration::from_secs(3290));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-1"
        );
        resolver.advance(Duration::from_secs(20));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-2"
        );
    }

    #[tokio::test]
    async fn provided_value_outlives_a_failing_provider_until_it_expires() {
        use std::sync::atomic::Ordering;
        let provider = Arc::new(FakeProvider::default());
        let resolver = resolver_with(provided(), Some(provider.clone()));
        resolve_one(&resolver, "api.github.com").await.unwrap();
        provider.failing.store(true, Ordering::SeqCst);
        resolver.advance(Duration::from_secs(3400));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-1"
        );
        resolver.advance(Duration::from_secs(200));
        let err = resolve_one(&resolver, "api.github.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("TOKEN"));
    }

    #[tokio::test]
    async fn a_refusal_drops_the_cached_value_at_once() {
        use std::sync::atomic::Ordering;
        let provider = Arc::new(FakeProvider::default());
        let resolver = resolver_with(provided(), Some(provider.clone()));
        resolve_one(&resolver, "api.github.com").await.unwrap();
        provider.refusing.store(true, Ordering::SeqCst);
        resolver.advance(Duration::from_secs(3400));
        let err = resolve_one(&resolver, "api.github.com").await.unwrap_err();
        assert!(format!("{err:#}").contains("not connected"));
        // Still dead inside the pause, though the value had time left.
        assert!(resolve_one(&resolver, "api.github.com").await.is_err());
    }

    #[tokio::test]
    async fn a_provided_value_is_kept_an_hour_at_most() {
        let provider = Arc::new(FakeProvider {
            lives: Some(Duration::from_secs(86_400)),
            ..Default::default()
        });
        let resolver = resolver_with(provided(), Some(provider.clone()));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-1"
        );
        resolver.advance(Duration::from_secs(3400));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-2"
        );
    }

    #[tokio::test]
    async fn a_provided_value_with_a_line_break_is_refused() {
        let provider = Arc::new(FakeProvider::default());
        *provider.value.lock().unwrap() = Some("ok\r\nX-Injected: 1".into());
        let resolver = resolver_with(provided(), Some(provider));
        assert!(resolve_one(&resolver, "api.github.com").await.is_err());
    }

    #[tokio::test]
    async fn substitutions_carry_the_scope_of_their_secret() {
        let mut secret = provided();
        secret.scope = Scope::Authorization;
        let resolver = resolver_with(secret, Some(Arc::new(FakeProvider::default())));
        let got = resolver
            .substitutions_for_domain("api.github.com")
            .await
            .unwrap();
        assert_eq!(got[0].scope, Scope::Authorization);
    }

    #[tokio::test]
    async fn provided_without_a_provider_fails_closed() {
        let resolver = resolver_with(provided(), None);
        assert!(resolve_one(&resolver, "api.github.com").await.is_err());
    }

    #[tokio::test]
    async fn an_update_changes_values_and_takes_secrets_away() {
        let resolver = resolver_for(SecretConfig {
            value: Some("old".into()),
            hosts: vec!["api.github.com".into()],
            ..Default::default()
        });
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "old"
        );

        let pending = resolver.update(HashMap::from([
            (
                "TOKEN".to_string(),
                SecretConfig {
                    value: Some("new".into()),
                    hosts: vec!["api.github.com".into(), "github.com".into()],
                    ..Default::default()
                },
            ),
            (
                "LATER".to_string(),
                SecretConfig {
                    value: Some("x".into()),
                    hosts: vec!["github.com".into()],
                    ..Default::default()
                },
            ),
        ]));
        assert_eq!(pending, vec!["LATER".to_string()]);
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "new"
        );
        assert_eq!(
            resolve_one(&resolver, "github.com").await.unwrap(),
            vec![("shuru_tok_placeholder".to_string(), "new".to_string())]
        );

        assert!(resolver.update(HashMap::new()).is_empty());
        assert!(resolve_one(&resolver, "api.github.com")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_update_drops_a_cached_value_only_when_its_secret_changed() {
        let provider = Arc::new(FakeProvider::default());
        let resolver = resolver_with(provided(), Some(provider.clone()));
        resolve_one(&resolver, "api.github.com").await.unwrap();

        resolver.update(HashMap::from([("TOKEN".to_string(), provided())]));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-1"
        );

        let mut wider = provided();
        wider.hosts.push("github.com".into());
        resolver.update(HashMap::from([("TOKEN".to_string(), wider)]));
        assert_eq!(
            resolve_one(&resolver, "api.github.com").await.unwrap()[0].1,
            "value-2"
        );
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
