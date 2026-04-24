# Networking

Networking is **off by default**. Pass `--allow-net` to enable it.

## How It Works

All guest network traffic goes through a userspace proxy on the host (no NAT, no direct internet access). The proxy:

- Resolves DNS on the host and relays responses
- Tunnels TCP connections (HTTP and HTTPS) to the real internet
- Optionally performs MITM on HTTPS to inject secrets (only when `secrets` are configured)
- Enforces domain allowlists when `network.allow` is set

ICMP (ping) is not supported — only TCP traffic is proxied.

## Enabling Network Access

```bash
shuru run --allow-net -- sh -c 'apt-get install -y curl && curl https://example.com'
```

Or set it in `shuru.json`:

```json
{
  "allow_net": true
}
```

## Domain Allowlist

Restrict which domains the guest can reach:

```json
{
  "allow_net": true,
  "network": {
    "allow": ["api.openai.com", "registry.npmjs.org", "*.github.com"]
  }
}
```

DNS queries for blocked domains return REFUSED. Omit `network.allow` to allow all domains.

## Secret Injection

See [config.md](config.md#secrets) for details on injecting API keys via the proxy.

## Port Forwarding

Forward host ports to guest ports with `-p HOST:GUEST`. Port forwarding uses vsock and works **without** `--allow-net`:

```bash
# Forward host 8080 to guest 80
shuru run -p 8080:80 -- python3 -m http.server 80

# With networking too
shuru run --allow-net -p 3000:3000 -p 5432:5432 -- sh -c 'start-services.sh'
```

Access forwarded services at `localhost:HOST_PORT` on the host machine.

Port forwards can also be set in `shuru.json`:

```json
{
  "ports": ["8080:80", "3000:3000"]
}
```

CLI `-p` flags are merged with config ports (not replaced).

## Kulfi Tunnels

Attach Kulfi exposure to an existing forwarded host port:

```bash
shuru run -p 3000:3000 --kulfi 3000:http -- python3 -m http.server 3000
shuru run -p 2222:22 --kulfi 2222:tcp:9001 -- /usr/sbin/sshd -D
```

`--kulfi` takes `HOST_PORT:http|tcp[:BRIDGE_PORT]` and requires a matching `-p/--port` host port.

For HTTP, shuru starts a localhost bridge and prints:

- a local URL like `http://127.0.0.1:<port>` for immediate testing
- the Kulfi identity
- the bridge-dependent public URL `https://<id52>.<domain>`

The public URL only works when a Kulfi HTTP bridge is available. The current integration is designed so the local bridge path works even when the default public bridge domain is down.

## Without Networking

When `--allow-net` is not set, the VM has no network device. DNS resolution, HTTP requests, and package installs will fail. This is the intended default for maximum isolation.

To install packages, either:
1. Use `--allow-net` during the run
2. Create a checkpoint with packages pre-installed, then run without networking:

```bash
shuru checkpoint create with-tools --allow-net -- apt-get install -y curl jq python3
shuru run --from with-tools -- python3 script.py   # no --allow-net needed
```
