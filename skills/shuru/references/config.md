# Project Config (shuru.json)

Place `shuru.json` in the project root (or pass `--config <path>`). All fields are optional.

## Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cpus` | number | 2 | Number of CPU cores |
| `memory` | number | 2048 | Memory in MB |
| `disk_size` | number | 4096 | Disk size in MB |
| `allow_net` | boolean | false | Enable networking |
| `ports` | string[] | [] | Port forwards, `"HOST:GUEST"` format |
| `mounts` | string[] | [] | Directory mounts, `"HOST:GUEST"` format |
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
      "value": "sk-your-openai-key",
      "hosts": ["api.openai.com"]
    }
  }
}
```

- `value`: literal secret value held on the host
- `hosts`: domains where the proxy will substitute the placeholder with the real value

The guest sees `$API_KEY=shuru_tok_...`. The real secret never enters the VM.

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
      "value": "sk-your-openai-key",
      "hosts": ["api.openai.com"]
    }
  },
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org"]
  }
}
```

With this config, `shuru run` boots a VM that can only reach `api.openai.com` and `registry.npmjs.org`, with the OpenAI API key injected securely via the proxy.
