# RFC 0002: Refreshable secrets

- Status: Implemented
- Date: 2026-08-04
- Scope: shuru-proxy, shuru-cli config, shuru-sdk public types
- Issue: superhq-ai/shuru#40

## Summary

Let a secret be backed by a command instead of a host environment variable.
The proxy runs that command to mint the real value, caches it in memory, and
re-runs it as the value expires. The placeholder in the guest never changes,
so a rotation is invisible inside the VM.

This makes short lived credentials usable for long running sandbox work. The
motivating case is a GitHub App installation token, which dies after one hour
and currently strands any agent task that outlives it.

It also narrows where substitution happens. Secrets are now substituted in the
request head, meaning the request line and any header value, and no longer
anywhere in the byte stream. Appendix A works through why.

## Motivation

Secrets resolve from the host environment of the shuru process, which is
frozen at exec. A VM that runs for ninety minutes against a sixty minute token
fails with an auth error after most of the work is already done. The only
workaround today is to split the task across VM boots using checkpoints.

The alternative users reach for otherwise is a long lived personal access
token, which is strictly worse: broader scope, no expiry, and a much larger
blast radius if the host is compromised.

Refreshing on the host side preserves the property the proxy exists to
provide. Real credentials stay outside the VM, the guest holds a placeholder
it cannot redeem anywhere except the bound hosts, and the guest cannot retain
anything useful even in the window where it holds a live session.

## Non-goals

- A control socket or `shuru secret set` command. That needs a per VM control
  plane, which does not exist and should not be motivated by secrets alone.
- File watching. Secrets at rest plus partial write races, for no benefit over
  a command.
- A CLI flag for command backed secrets. The `--secret NAME=ENV@hosts` syntax
  does not extend to argv without quoting hazards. Config file only.
- Refreshing on an upstream 401. Expiry driven refresh covers the reported
  case; reactive refresh is future work.
- Substituting inside request bodies. See appendix A.
- Redacting secrets out of responses. See appendix B.

## Background

Secrets flow through three points.

`ProxyConfig.secrets` maps a guest visible env var name to a `SecretConfig`
with `from` (host env var), `hosts` (domains where substitution is allowed),
and an optional literal `value`.

At proxy start, one random placeholder per secret is generated and handed to
the guest as the value of that env var at exec time.

At TLS connection setup the SNI is matched against the bound hosts, and the
resulting placeholder to value pairs are applied to the guest to upstream
stream.

The value was already re-read per connection rather than cached at launch. It
simply never changed, because the process environment cannot change. Mutating
it is not an option either: `std::env::set_var` is unsafe in a multithreaded
process under the 2024 edition, and the proxy runs on its own threads.

Prior art converges on one answer for refresh. AWS `credential_process` runs a
command that prints JSON with an `Expiration` field and re-runs it when the
clock passes that time. Git credential helpers added `password_expiry_utc` in
2.41 for exactly the GitHub App case. Docker credential helpers pass the
target host in on stdin but have no expiry concept, so they pay a subprocess
spawn on every operation. This design takes the AWS shape, since the bound
hosts are already known from config and there is nothing to pass in.

## Design

### Config surface

`SecretEntry` gains `command` and `ttl`, and `from` becomes optional:

```json
{
  "allow_net": true,
  "secrets": {
    "GITHUB_TOKEN": {
      "command": ["./scripts/mint-installation-token.sh"],
      "hosts": ["api.github.com", "github.com"]
    },
    "API_KEY": {
      "from": "OPENAI_API_KEY",
      "hosts": ["api.openai.com"]
    }
  }
}
```

Exactly one of `from` or `command` must be set. Both, or neither, is a config
error reported at load time rather than at first request.

`command` is an argv array, not a shell string. No shell sits between shuru
and a credential, and there is no quoting hazard for paths with spaces.

`ttl` is an optional duration string (`"45m"`, `"3600s"`) used only when the
command reports no expiry of its own.

### Provider contract

The command runs with stdin closed, inheriting the host environment of the
shuru process, with the working directory set to the directory holding the
resolved config file so relative paths behave the same wherever shuru is
invoked from.

On success it exits zero having written one JSON object to stdout:

```json
{
  "version": 1,
  "value": "ghs_...",
  "expires_at": "2026-08-04T18:36:00Z"
}
```

- `version` must be 1. Unknown versions are a resolve failure, which reserves
  room to change the shape later.
- `value` is the credential, used verbatim. A trailing newline is trimmed.
- `expires_at` is optional RFC3339. Present means refresh at that time. Absent
  falls back to `ttl`. Absent with no `ttl` means fetch once and never
  refresh, matching `credential_process`.

A non-zero exit, unparseable stdout, an empty `value`, or a timeout is a
resolve failure. The timeout is 10 seconds and the process is killed on
expiry.

Stderr is captured, truncated to 4 KB, and surfaced only in the resolve
failure message. Stdout is never logged at any level, since it holds the
credential, and `Minted` deliberately has no `Debug` implementation so a
credential cannot reach a panic message.

### Resolution and caching

`SecretResolver` owns what `ProxyConfig` cannot, since the engine holds the
config as an immutable `Arc<ProxyConfig>`:

```rust
pub struct SecretResolver {
    config: Arc<ProxyConfig>,
    placeholders: HashMap<String, String>,
    cache: Mutex<HashMap<String, CacheEntry>>,
}
```

Env backed secrets keep reading `std::env::var` with no caching, which is free
and preserves earlier behaviour exactly.

Command backed secrets resolve against the cache. A cached entry with no
expiry, or an expiry more than the refresh window away, is returned as is;
otherwise the command runs and the result is stored. The refresh window is 60
seconds, so a value is re-minted shortly before it dies rather than at the
first request that fails.

Concurrent connections must not each spawn a process. The cache mutex is held
across the fetch, so the first caller runs the command and the rest wait and
observe the fresh entry. The mutex is per resolver rather than per secret,
which serialises two different secrets in the rare case they lapse together.
That is acceptable against a 10 second worst case and avoids a map of
in-flight futures.

### Where substitution applies

Substitution is scoped to the request head: the request line and every header
value. It is scoped by position, not by header name, so any header carries a
secret, including one this project has never heard of. A user writing
`X-My-Vendor-Token: $API_KEY` needs no configuration and there is no provider
table to maintain. The request line covers credentials passed as query
parameters, such as the Google `?key=` form.

Bodies stream through untouched.

This is what makes the relay deterministic. At an arbitrary byte offset, "no
more bytes yet" and "the request ended" are indistinguishable without a clock.
A head that has not reached its terminating CRLF CRLF is unambiguously an
unfinished request, so holding it is correct rather than a guess, and the
upstream is waiting on those bytes regardless.

Framing is read, never rewritten, and only to find where a body ends and the
next head begins on a reused connection. Bodies are never modified, so no
declared length is ever invalidated and nothing has to be recomputed. Framing
follows the RFC 9112 section 6.3 algorithm, and anything that section calls
out as a possible smuggling attempt is treated as ambiguous rather than
resolved: both `Content-Length` and `Transfer-Encoding` present, conflicting
`Content-Length` values, an unparseable head, `CONNECT`, or an `Upgrade`.
Ambiguity stops substitution for the rest of the connection and degrades to a
plain byte tunnel, which is the pre-existing behaviour rather than a guessed
boundary. An intermediary that never re-frames cannot desynchronise its
upstream by re-framing wrongly, so the smuggling exposure of a rewriting proxy
does not arise.

A head is bounded at 64 KiB. A larger one is released and the connection
tunnelled, so nothing buffers without bound.

This is also less work per byte than replacing over the whole stream, which
scanned every byte of every body once per secret. Only heads are scanned now:
roughly 200x less on an 80 KB upload, 2500x less on 1 MB.

### Refresh granularity

Values resolve at TLS connection setup, and a live relay re-resolves every 5
seconds as it forwards, so a rotation reaches a connection that is already
open rather than waiting for it to reconnect. Fresh values apply from the next
request head.

The relay can simply await the resolver, because it is already an async loop.
An earlier draft assumed this needed a lock free read plus a background
refresh task; it does not. While the cached value is live the re-resolve is a
mutex and a map lookup, and a re-mint costs that connection what it would have
cost at setup, bounded by the same provider timeout.

## Failure handling

| Condition | Behaviour |
| --- | --- |
| Both or neither of `from` and `command` set | Config error at load, names the secret |
| Command fails, no cached value | Fail the connection with a clear error |
| Command fails, unexpired value cached | Serve the cached value, log under warn |
| Command fails, cached value already expired | Fail the connection |
| Env var missing (`from` secrets) | Skip substitution, unchanged behaviour |
| Ambiguous or unparseable HTTP framing | Stop substituting, tunnel the connection |

Command backed secrets fail closed. Forwarding a placeholder to an upstream
produces a 401 that reads like a scope problem, and there is no reason to
spend a request on a value known to be wrong. The failure is logged at warn
with the provider stderr, since the guest only sees a dropped TLS connection.

Env backed secrets keep the existing skip behaviour so nothing that works
today starts failing.

## Security

The credential still never enters the VM by way of the proxy. The placeholder
is generated once per proxy and injected at exec, so a refresh changes only
the host side substitution table and the guest observes nothing.

Refreshing narrows the window. A guest that exfiltrates a live session gains
nothing durable, because the value it was transiting expires on schedule and
the replacement is minted outside the VM.

Narrowing substitution to the head also removes a way for a guest to have the
proxy write a credential into arbitrary data it controls: a placeholder
appearing in a body is now left alone.

New surface is one subprocess spawned by the host, from an argv array in a
config file the user controls, with no guest input reaching it. A hostile
`shuru.json` can already run arbitrary code on `shuru run`, so this adds no
capability that did not exist.

Substitution is best effort against a cooperative guest and is not a
containment boundary. A guest that base64s the placeholder, splits it across
fields, or gzips the body defeats literal matching by construction. The host
allowlist is what actually contains the credential. Appendix B records a
related defect that is not addressed here.

## Testing

Unit, in `secrets.rs`: expiry math and the refresh window, the
`expires_at` over `ttl` over never precedence, provider output parsing
including version mismatch and garbage stdout, single flight, fail closed and
fallback to a live cached value, and that a parse error never quotes stdout.

Unit, in `substitute.rs`: substitution in arbitrary header names and in the
request line, at every chunk split point, bodies forwarded byte for byte,
a second and third request on a reused connection, chunked bodies with
extensions and trailers, and that each ambiguous framing case degrades to a
tunnel with bytes released intact.

Verified end to end against a live endpoint that echoes the request:

- A length-changing secret substituted into a header, which previously hung
  when the same value sat in a body.
- A 40 KB POST body forwarded untouched with the header substituted.
- A placeholder in a body carried through verbatim, returning 200 rather than
  hanging.
- Five requests on one connection, five substitutions, no placeholder leaked.
- A rotation picked up mid-connection: 614 tokens took the first minted value
  and 386 the second, on one connection.

## Rollout

Additive for configuration. Existing configs using `from` behave as before.

`SecretConfig` is re-exported from shuru-sdk, so the field changes are
breaking for SDK consumers building it as a struct literal. It gains
`Default`, with `SecretConfig::from_env` and `SecretConfig::from_command` as
the constructors. shuru-proxy goes to 0.3.0 and shuru-sdk to 0.4.0.

Behaviour change: a secret placed in a request body is no longer substituted.
Appendix A shows this never worked for any real credential length, so no
working configuration changes, but the failure mode moves from a hung request
to a clean 401.

## Appendix A: why substitution is head scoped

### Body substitution never worked

Byte substitution arrived with proxy based networking in c2b046f (v0.3.0,
2026-03-10) as a blind replace over the guest to upstream stream, commented
only "Replace placeholder tokens with real values". No design named bodies as
a target, and no documentation ever mentioned them.

It is therefore a side effect rather than a feature, and it never functioned,
because placeholders are a fixed 30 bytes and framing breaks the moment the
replacement is any other length. Nothing real is 30 bytes: a GitHub token is
40, a legacy OpenAI key 51, an Anthropic key about 108, an AWS access key id
20 and its secret 40, a Stripe key about 107.

Measured through a real VM, holding everything constant except the length of
the minted value:

| Request shape | Length equals placeholder | Length differs |
| --- | --- | --- |
| Secret in a header | 200 | 200 |
| Body with `Content-Length` | 200 | fails |
| Body with `Transfer-Encoding: chunked` | 200 | fails |
| Body over 1 KB, so `Expect: 100-continue` | 200 | fails |

Headers are immune because framing describes the body, not the head. Every
body framing fails once a rewrite changes the byte count: with
`Content-Length` the upstream waits for bytes that never arrive, and with
chunked the emitted chunk size no longer matches its data.

### No comparable proxy offers it

- Modal, in its credential-injection recipe, does not substitute at all. The
  proxy composes the header itself, via a Caddy
  `header_up x-api-key {env.ANTHROPIC_API_KEY}`.
- Hermes iron-proxy substitutes "the proxy token wherever it appears in a
  matched location", and enumerates them: `Authorization`, `x-api-key`,
  `api-key`, `x-goog-api-key`, and the Google `?key=` form. Headers and query
  parameters, never bodies.

The tools that do rewrite bodies are tokenization vaults such as VGS, where
that is the entire product and it is done with structured payload operations
over a fully parsed message.

mitmproxy documents the underlying constraint plainly: with streaming enabled
"the response body cannot be modified by the usual means", because "if the
transfer encoding is not chunked, you cannot simply change the content
length", and "it is recommended not to stream messages you need to modify".

### Options rejected

- **A timeout on held-back fragments.** Shipped briefly. Any timer can be
  beaten by a slow enough stream, since "no bytes for N seconds" is not the
  same fact as "the request ended". Measured: at 50ms a 4 KB/s throttled
  upload released 6 of 1000 placeholders early; at 2s, none. Tuned to observed
  throughput rather than derived, and it does nothing for framing.
- **Scaling that wait to fragment length.** Narrows both failure modes and is
  still guessing at the same missing fact.
- **Re-framing everything to chunked.** Legal per RFC 9112 section 7.1.4, and
  it makes length changes free, but it rewrites requests that did not ask for
  it and breaks any upstream requiring an exact `Content-Length`, notably S3
  style signed uploads.
- **A buffering relay that rewrites `Content-Length`.** Rejected on cost. It
  introduces request smuggling exposure, which RFC 9112 section 11.2
  describes as exploiting "differences in protocol parsing among various
  recipients"; it gives an untrusted guest a memory lever; it turns uploads
  into store and forward; and its over-cap path is incoherent, since a
  placeholder detected after earlier bytes are already upstream leaves a
  truncated request sent, which for a non-idempotent POST may have committed
  a side effect. Even done perfectly it still fails for signed and compressed
  bodies. All of that to preserve something that never worked.
- **Length-matched placeholders**, generated to be exactly as long as the
  secret so substitution preserves framing by construction. Cheap and
  tempting, but entropy falls with the secret: 80 bits today, 40 against a 20
  byte AWS key id, and under 10 bytes there is no room for the `shuru_tok_`
  marker at all. A short placeholder starts colliding with ordinary request
  text, and a collision writes a real credential into unrelated data, which is
  worse than not substituting. Viable only with a length floor.

## Appendix B: a separate defect, not fixed here

Substitution is one directional. The proxy replaces the placeholder with the
real value on the way out and does nothing on the way back, so an upstream
that reflects the request hands the credential to the guest:

```
guest env holds: shuru_tok_18c8ae1dd13ae8680000
"authorization":"Bearer sk-live-REAL-CREDENTIAL-DO-NOT-LEAK"
```

The guest printed the real value. This weakens the claim that the real secret
never enters the VM, with the precondition that a host the secret is bound to
must reflect it, which a debug endpoint or a verbose error response can do.
Scoping substitution to the head does not address it, since a reflected header
carries the value just the same.

The fix is response side redaction, replacing the real value with the
placeholder on the way back, and it inherits the framing problem in the
response direction. This wants its own issue and its own design.

## Future work

- Reactive refresh on an upstream 401, covering providers that revoke early.
- Policy driven header injection, where the config states that a host gets
  `Authorization: Bearer <secret>` and the guest never holds a placeholder at
  all. It has none of the framing risk and none of the transformation limits,
  because the proxy composes the header rather than finding and rewriting
  something the guest wrote. Complementary to substitution rather than a
  replacement, since it does not cover query parameters or bodies.
- A `shuru secret set` control command, once a general per VM control plane
  exists for port exposure and network policy changes.
