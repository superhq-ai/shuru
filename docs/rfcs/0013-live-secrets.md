# RFC 0013: Live and provided secrets on a node

- Status: Draft
- Date: 2026-11-02
- Scope: shuru-proxy, shuru-node (serve, netd)
- Depends on: RFC 0002 (refreshable secrets), RFC 0008 (node runtime), RFC 0012 (guest API)

## Summary

Four changes to how a sandbox on a node gets its secrets.

1. `PUT config` reaches a running sandbox. Netd takes the new values and
   hosts of the secrets the guest booted with, and stops substituting
   those that were removed, without a reboot. A secret with a new name
   still waits for the next boot.
2. A secret can be provided: given with its hosts and no value. Netd asks
   serve for the value when the guest first talks to a bound host, serve
   asks a provider on a unix socket, and netd keeps the value in memory
   until shortly before it expires.
3. A placeholder inside `Authorization: Basic` is substituted. Git over
   HTTPS sends its token that way, base64 encoded, so a placeholder
   there never matched before.
4. A secret can be limited to the `Authorization` header, so a bound host
   that stores what it is sent cannot be made to keep the value.

## Motivation

A sandbox on a node lives for days. The credentials agents want there
are short lived: a GitHub App installation token lasts one hour. RFC
0002 solved this for `shuru run` with a provider command on the host.
On a node that does not fit. Netd runs jailed under seccomp and has no
business starting processes, and the value comes from whoever runs the
node, not from the host.

Values set through `PUT config` reach only the next boot today, so a
rotated key or a revoked one stays in effect for as long as the guest
runs. Revocation is the case that matters: taking a secret away must
stop it being sent now, not after a park.

GitHub App tokens are the motivating case for all four. The token is
minted per hour, it must be revocable at once, `git push` over HTTPS
sends it as the password of Basic auth, and GitHub is a host where the
guest can write a value and read it back.

## Live config

`PUT /v1/sandboxes/{name}/config` saves `config.json` as before. When the
sandbox runs with a network, serve then hands netd the new secrets over
its control connection:

```json
{ "op": "update_secrets", "secrets": { "NAME": { "value": "...", "hosts": ["..."] } } }
```

Netd swaps its table in one step:

- A secret the guest booted with takes its new value, hosts and scope.
- A secret that is gone is no longer substituted. Its placeholder, if
  the guest still sends it, goes upstream as it is and is refused there.
- A secret with a name the guest did not boot with is kept for the next
  boot. The guest has no placeholder for it, and a sandbox that booted
  with no secrets does not trust the proxy CA. Netd answers the names it
  could not take, and serve logs them.

New connections use the new table at once. An open relay resolves its
substitutions again before the next request head once 5 seconds have
passed, as in RFC 0002, so a change or a removal reaches it within that.
It never does so in the middle of a body, so an upload is not held up by
a fetch. If that resolve fails, the relay closes the connection rather
than carry on with what it had, which might include a secret that is
gone.

A provided value cached for a secret that changed or went is dropped. A
fetch already under way for it finishes for the connection waiting on
it, and its value is not kept.

`PUT config` answers 204 once the file is saved and, for a running
sandbox, netd has the table. If netd does not take it, the answer is
500 and the file is still saved, so the next boot has it and the caller
may send it again. Sending the same config twice is harmless.

### Order

Two `PUT config` calls may race on the way to a node. Create and `PUT
config` may carry `config_version`, a number the caller raises with each
change. Serve keeps it in `config.json` and shows it as `config_version`
in get and list. A `PUT config` whose version is lower than the one kept
changes nothing and answers 204. A `PUT config` without one applies and
keeps the version there was, so it never lowers it. A caller that sends
versions can then compare the version a sandbox holds with its own, and
send the config again where it is behind.

## Provided secrets

A secret without a value names its provider instead:

```json
{ "secrets": { "GH_TOKEN": { "provider": true, "hosts": ["api.github.com", "github.com"] } } }
```

Serve takes these only when run with `--secret-provider <socket>`, the
path of a unix socket where something that runs the node answers for
values; without it they are refused with 400. A provided value is never
written to `config.json` or the disk of the node.

Netd gets a way to ask only when the sandbox boots with a provided
secret. A valued secret that a live update turns into a provided one, on
a sandbox that booted with none, fails closed until the next boot: its
hosts are refused, and the old value is not sent.

### Provider

Serve asks the provider with one HTTP/1.1 request per value, on a
connection of its own:

```
POST /v1/secrets
{ "sandbox": "<name>", "secret": "GH_TOKEN" }
```

The provider answers 200 with the value and, optionally, when it
expires:

```json
{ "value": "ghs_...", "expires_at": "2026-11-02T18:36:00Z" }
```

- The value is at most 32 KiB, without control characters, since it goes
  into a header.
- An `expires_at` in the past is a failure. Without one the value is kept
  for an hour at most; a provider is asked again after that.
- 403 and 404 are refusals: the provider says this sandbox may not have
  the secret. Any other answer is a failure that may pass. The `message`
  of a JSON error body is kept for the log either way.
- Serve gives up after 25 seconds.

The provider decides whether this sandbox may have this secret now.
Serve asks only for names that are provided secrets of the sandbox it
names, and only when netd of that sandbox asks.

### Netd asks

Netd gets a second descriptor with its configuration: one end of a
socket pair whose other end serve keeps for the run. Messages on it are
framed as on the control connection, a u32 little endian length and a
JSON body:

```json
{ "ask": 7, "secret": "GH_TOKEN" }
{ "ask": 7, "value": "ghs_...", "expires_in_ms": 3540000 }
{ "ask": 8, "error": "the provider answered 503: busy" }
{ "ask": 9, "error": "the provider answered 403: not connected", "refused": true }
```

The expiry travels as a duration, so the two clocks need not agree.
Asks are answered as they finish, in any order. Netd waits 30 seconds
for an answer.

### Caching

Each secret has its own entry, so a provided value being fetched holds
up connections to its hosts and no others. Concurrent connections that
need it wait for one fetch.

A cached value is used until it comes within its refresh window of
expiring: 5 minutes for a provided value, 60 seconds for a provider
command, and never more than half of its lifetime, so a value that
lives a few minutes is not fetched on every connection. A hosted fetch
crosses the node, the control plane and the provider, so it gets the
larger window.

When a fetch fails, a value that has not yet expired is used and a
warning is logged. Another fetch is tried no sooner than 30 seconds
later. With no value left, the connection to the bound host is refused,
as a failed provider command refuses it in RFC 0002: the guest sees a
dropped TLS connection, never a request sent without its credential.
The 30 second pause applies to provider commands too.

A refusal is not a failure that may pass: the cached value is dropped at
once and the connection refused. So a provider revokes a value by
refusing the next fetch, without waiting for it to expire.

A guest causes at most one fetch per secret per refresh window, and one
per 30 seconds while fetches fail. It cannot name the secret it wants,
only reach a host a secret is bound to.

## Basic auth

After the placeholders are swapped in a request head, netd looks at each
`Authorization` header whose value is `Basic` and a base64 token. If the
decoded token holds the placeholder of a secret bound to the host, the
placeholder is swapped there and the token encoded again. A token that
does not decode, or holds no placeholder, is left as it was.

This is the same rule as for any header: only secrets bound to the host
of the connection, only in the request head. It does not widen where a
value can go. It covers git, which sends `username:token`, and any client
that puts a token in Basic auth.

## Scope

By default a placeholder is swapped anywhere in the request head: the
request line and every header, as RFC 0002 made it. That suits keys
passed as `?key=` or in a header of the vendor. It also means that a
bound host where the guest can write and then read, such as GitHub, can
be made to keep the value: the guest names a file after the placeholder,
or puts it in a title, and reads it back.

A secret given `"in": "authorization"` is swapped only inside the value
of an `Authorization` header, Basic included, and nowhere else in the
head. No request a host stores keeps that header, so the guest can no
longer make the host write the value down. `gh` sends `Authorization:
token` and git sends Basic, so both work with the scope. `"in": "head"`
is the default.

## Restores

A restored guest keeps the placeholders of its session. Its config may
have changed since the snapshot, so the session secrets and the config
are matched as a live update matches them: a secret of the session takes
its value from the config, one the config no longer has is not
substituted, and one the session never had waits for the next cold boot.
A restore no longer fails when the two differ.

## Security

- Provided values live in netd memory only. They are not in
  `config.json`, not on the guest disk, not in a snapshot (a snapshot
  keeps the session, which holds placeholders, not values).
- Whoever runs the node secures the provider socket. Serve runs as root
  and only connects to it; nothing in the guest or netd can reach it.
- Netd can ask only for names in its own table, and serve checks the
  name against the config of the sandbox before asking. A netd taken over
  by its guest could ask for the secrets of its own sandbox, which it
  would substitute anyway, and no others.
- No value appears in a log, an answer of serve or an error message.
  `Debug` of every type that holds one leaves it out.
- What a scope does not stop: a value that leaves some other way stays
  good until it expires or the provider revokes it at its origin. A park
  or a removal stops the sandbox sending it, not anyone else.

## Not in this RFC

- Secrets with new names on a running sandbox. They need the CA in the
  guest and a placeholder in the exec env, which a later change can add.
- Fetching again when an upstream answers 401. Expiry is what drives a
  fetch, as in RFC 0002.
- Provided secrets for `shuru run` on a Mac. A provider command already
  covers that.

## Testing

- shuru-proxy: Basic tokens with a placeholder, without one, not base64,
  bound to another host, split across reads; the authorization scope in
  the request line, other headers, Bearer, token and Basic; a table swap
  that changes a value, removes a secret and adds one; a relay that
  closes when its resolve fails; a provided secret fetched once for
  concurrent connections, refreshed inside its window, kept through a
  failed fetch until it expires, dropped at once on a refusal; the 30
  second pause; control characters refused.
- shuru-node: `PUT config` on a running sandbox changes what netd sends
  and a removal stops it, checked on a1.metal against a real upstream;
  versions; provided secrets refused without `--secret-provider`; a fake
  provider socket answering values, refusals, errors and slow answers;
  a restore whose config changed since the snapshot.
