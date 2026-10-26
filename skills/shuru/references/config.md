# Project Config (shuru.json)

Place `shuru.json` in the project root (or pass `--config <path>`). All fields are optional.

## Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cpus` | number | 2 | Number of CPU cores |
| `memory` | number | 2048 | Memory in MB |
| `disk_size` | number | 4096 | Disk size in MB |
| `allow_net` | boolean | false | Enable networking |
| `allow_host_writes` | boolean | false | Allow `:rw` mounts to write to host filesystem |
| `ports` | string[] | [] | Port forwards, `"HOST:GUEST"` format |
| `mounts` | string[] | [] | Directory mounts, `"HOST:GUEST[:ro\|:rw]"` format (default: ro) |
| `command` | string[] | ["/bin/sh"] | Default command to run |
| `secrets` | object | {} | Secrets to inject via proxy (see below) |
| `network` | object | {} | Network access policy (see below) |

## Resolution Order

CLI flags take priority over config values. Config values take priority over hardcoded defaults.

```
CLI flag > shuru.json > default
```

For example, `shuru run --cpus 4` with `{"cpus": 2}` in shuru.json uses 4 CPUs.

## Secrets

Secrets let the guest use API keys without exposing the real values. The guest receives a random placeholder token; the proxy substitutes the real value only on HTTPS requests to allowed hosts.

```json
{
  "allow_net": true,
  "secrets": {
    "API_KEY": {
      "from": "OPENAI_API_KEY",
      "hosts": ["api.openai.com"]
    }
  }
}
```

- `from`: host environment variable containing the real value
- `hosts`: domains where the proxy will substitute the placeholder with the real value

The guest sees `$API_KEY=shuru_tok_...`. The real secret is never placed in the VM. Bind a secret only to hosts you trust with it: responses are not rewritten, so a bound host that reflects the request shows the real value to the guest.

Substitution applies to the request head: the request line and any header
value, whatever the header is named. A custom `X-My-Vendor-Token` works with
no extra configuration, and a credential passed as a query parameter is
covered too. Request bodies are forwarded untouched, so a placeholder placed
in a body reaches the upstream unsubstituted and the request fails to
authenticate.

### Refreshing short lived credentials

A secret can be minted by a command instead of read from the environment.
The proxy runs the command, caches what it returns, and runs it again as the
value approaches expiry. Use this for credentials that expire sooner than
the task takes to finish, such as GitHub App installation tokens, which last
an hour.

```json
{
  "allow_net": true,
  "secrets": {
    "GITHUB_TOKEN": {
      "command": ["./scripts/mint-installation-token.sh"],
      "hosts": ["api.github.com", "github.com"]
    }
  }
}
```

- `command`: argv array, never passed to a shell. Runs with the working
  directory set to the folder holding `shuru.json`, so relative paths work
  wherever `shuru` is invoked from.
- `ttl`: optional lifetime to assume when the command reports no expiry.
  Accepts `"45m"`, `"3600s"`, `"2h"`, or a bare number of seconds.

Set exactly one of `from` or `command` on a secret.

The command writes one JSON object to stdout:

```json
{ "version": 1, "value": "ghs_...", "expires_at": "2026-08-04T18:36:00Z" }
```

- `version` must be `1`.
- `value` is the credential.
- `expires_at` is optional RFC3339. When present the proxy re-runs the
  command about a minute before it expires. When absent, `ttl` applies. With
  neither, the value is minted once and never refreshed.

A non-zero exit, unparseable output, or a run longer than 10 seconds is a
failure. If a previously minted value is still live the proxy keeps serving
it and logs a warning; otherwise the connection is refused, rather than
sending a placeholder upstream that would come back as a confusing auth
error.

The placeholder in the guest never changes when a value is refreshed, so
rotation is invisible inside the VM. Minted values are held in memory only.

## Network Policy

Restrict which domains the guest can reach:

```json
{
  "allow_net": true,
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org", "*.github.com"]
  }
}
```

- Empty or absent `allow` list means all domains are allowed.
- Supports wildcards: `*.example.com` matches `api.example.com` but not `example.com`.
- DNS queries for blocked domains return REFUSED.

## Example

```json
{
  "cpus": 4,
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true,
  "ports": ["3000:3000", "8080:80"],
  "mounts": [".:/workspace"],
  "command": ["/bin/sh", "-c", "cd /workspace && sh"],
  "secrets": {
    "API_KEY": {
      "from": "OPENAI_API_KEY",
      "hosts": ["api.openai.com"]
    }
  },
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org"]
  }
}
```

With this config, `shuru run` boots a VM that can only reach `api.openai.com` and `registry.npmjs.org`, with the OpenAI API key injected securely via the proxy.
