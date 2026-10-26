---
name: shuru
description: Run commands in an isolated Linux microVM sandbox using the shuru CLI. Use when the user asks to execute untrusted code, install packages safely, test in a clean environment, run Docker containers or a headless browser inside a VM, or needs Linux-specific tooling on macOS.
---

# Sandboxed Execution with Shuru

Shuru boots an ephemeral Linux microVM (Debian, ARM64) on macOS, with experimental support on Linux ARM64 hosts. Each `shuru run` gets a fresh disk clone and all changes are discarded on exit. Use it whenever you need to run commands in isolation from the host.

## Core Workflow

The pattern is: **run in sandbox, mount to share files, checkpoint to persist state, recipe for a ready-made environment**.

```bash
# 1. Run a command in a fresh VM
shuru run -- echo "hello from the sandbox"

# 2. Mount the project directory so the VM can access host files
shuru run --mount ./src:/workspace -- ls /workspace

# 3. If the command needs network access (install packages, fetch data)
shuru run --allow-net -- sh -c 'apt-get install -y curl && curl https://example.com'

# 4. If setup is expensive, save a checkpoint and reuse it
shuru checkpoint create node-env --allow-net -- apt-get install -y nodejs npm
shuru run --from node-env --mount .:/workspace -- node /workspace/app.js

# 5. For Docker or a browser, use a recipe from the official catalog
shuru run --recipe docker -- docker run --rm hello-world
shuru run --recipe browser -p 9222:9222 -- sleep infinity
```

## Command Chaining

Chain commands with `sh -c` when you need multiple steps:

```bash
shuru run --allow-net -- sh -c 'apt-get install -y python3 python3-pip && python3 -c "print(1+1)"'

shuru run --mount .:/workspace -- sh -c 'cd /workspace && ls -la && cat README.md'
```

## Essential Commands

### Run

```bash
shuru run [flags] [-- command...]

# Interactive shell (default when no command given)
shuru run

# Run a single command
shuru run -- whoami

# With resources
shuru run --cpus 4 --memory 4096 --disk-size 8192 -- make -j4

# With networking + port forwarding
shuru run --allow-net -p 8080:80 -- nginx -g 'daemon off;'

# Multiple mounts
shuru run --mount ./src:/src --mount ./data:/data -- ls /src /data

# From a checkpoint
shuru run --from myenv -- npm test

# From a recipe: builds the checkpoint on first use, reuses it after
shuru run --recipe docker -- docker ps
```

### Checkpoints

```bash
# Create: boots VM, runs command, saves disk on exit
shuru checkpoint create <name> [flags] [-- command...]

# Stack: create from an existing checkpoint
shuru checkpoint create with-deps --from base-env --allow-net -- npm install

# Update: rebuild in place; the old checkpoint stays until the new build succeeds
shuru checkpoint update base-env --allow-net -- apt-get install -y build-essential git

# Layer onto the existing checkpoint instead of starting over
shuru checkpoint update base-env --from base-env --allow-net -- apt-get install -y jq

# Build a checkpoint from a recipe
shuru checkpoint create docker --recipe docker

# List all checkpoints (shows actual disk usage and recipe source)
shuru checkpoint list

# Delete
shuru checkpoint delete <name>
```

`create` refuses an existing name. Use `update` to replace a checkpoint; it builds under a staging name and swaps in only when the command exits 0, so a failed update leaves the old one intact.

### Recipes

A recipe is a versioned description of a checkpoint: a `recipe.json` plus shell steps, fetched from GitHub at an exact commit or read from a local directory. The official catalog is [superhq-ai/recipes](https://github.com/superhq-ai/recipes) and currently ships `docker` and `browser`.

```bash
shuru recipe list                                   # the official catalog
shuru recipe show docker                            # metadata, services and every step, without booting
shuru run --recipe docker -- docker ps              # build on first use, reuse after
shuru checkpoint create docker --recipe docker      # build explicitly under that name
shuru run --recipe docker --checkpoint ci -- ...    # pick the checkpoint name
shuru checkpoint update docker --recipe docker      # force a rebuild at the current revision
shuru run --recipe owner/repo/path@v1 --yes -- ...  # third-party, pinned to a tag
shuru run --recipe ./my-recipe -- ...               # local directory while developing one
```

Rules that matter when scripting:

- A bare name such as `docker` always means the official catalog. A local directory must be written as a path (`./dir`, `../dir`, `/abs`, `~/dir`).
- Official and local recipes build without prompting. Third-party GitHub recipes ask for confirmation, so pass `--yes` in non-interactive runs.
- References resolve to a commit SHA that is recorded next to the checkpoint. A changed recipe rebuilds on the next `run --recipe`; an unchanged one boots the existing checkpoint in seconds.
- Builds use the network allowlist declared by the recipe, never the project `shuru.json`. The first build of `docker` or `browser` takes a few minutes.

See [references/recipes.md](references/recipes.md) for the `recipe.json` format and guest services.

### Other Commands

```bash
# Download/update OS image
shuru init
shuru init --force    # re-download even if up to date

# Upgrade CLI + OS image
shuru upgrade
```

## Common Patterns

### Dev Environment Setup

Create a checkpoint with all dependencies pre-installed, then use it for fast runs:

```bash
# One-time setup
shuru checkpoint create python-dev --allow-net -- sh -c 'apt-get install -y python3 python3-pip && pip install pytest requests'

# Fast subsequent runs
shuru run --from python-dev --mount .:/workspace -- sh -c 'cd /workspace && pytest'
```

### Docker Inside the VM

The `docker` recipe installs Docker Engine, runs `dockerd` as a guest service, and pre-pulls `nginxinc/nginx-unprivileged:1.31.7-alpine-slim`. The command starts only after `docker info` answers.

```bash
# Build once (a few minutes), then every run boots in seconds
shuru checkpoint create docker --recipe docker

# Serve the pre-pulled image on host port 8080; no --allow-net needed
shuru run --from docker -p 8080:8080 -- \
  docker run --rm -p 8080:8080 nginxinc/nginx-unprivileged:1.31.7-alpine-slim

# Pulling other images needs network
shuru run --from docker --allow-net -- docker run --rm python:3.12-alpine python3 -c 'print(1)'

# Build an image from a mounted project
shuru run --from docker --mount .:/app -- sh -c 'cd /app && docker build -t app . && docker run --rm app'
```

Containers get the same boundary as any guest process: no egress without `--allow-net`, the host allowlist applies, and host ports only open through `-p`. Keep Docker at its defaults inside the VM; do not use `--privileged` or mount the Docker socket.

### Headless Browser (CDP)

The `browser` recipe runs Chromium headless as an unprivileged user with the DevTools protocol on port 9222 inside the VM. Forward that port and point any CDP client (Playwright, Puppeteer, browser-use) at `http://127.0.0.1:9222`.

```bash
# Keep the VM alive while a client on the host drives the browser
shuru run --recipe browser -p 9222:9222 -- sleep infinity

# In another terminal
curl -s http://127.0.0.1:9222/json/version
```

Chromium stays inside the microVM; only its CDP port reaches the host. Add `--allow-net` when the browser must load real websites, and `--allow-host` to restrict which ones. The DevTools HTTP endpoint ignores HTTP/1.0 requests, so use a normal HTTP/1.1 client.

### Testing Untrusted Code

Run untrusted scripts with no network access and no host filesystem access:

```bash
# Fully isolated: no --allow-net, no --mount
shuru run -- sh -c 'echo "malicious script here" && rm -rf / 2>/dev/null; echo "host is safe"'
```

### Build and Test

Mount source, build inside the VM, results appear on host via the mount:

```bash
shuru run --mount .:/workspace --cpus 4 --memory 4096 -- sh -c '
  cd /workspace
  apt-get install -y build-essential
  make -j4
  make test
'
```

### Port Forwarding for Web Servers

```bash
shuru run --allow-net --from node-env -p 3000:3000 --mount .:/app -- sh -c '
  cd /app && node server.js
'
# Access at http://localhost:3000 on the host
```

### Stacking Checkpoints

Build environments incrementally:

```bash
shuru checkpoint create base --allow-net -- apt-get install -y build-essential git curl
shuru checkpoint create node --from base --allow-net -- apt-get install -y nodejs npm
shuru checkpoint create project --from node --allow-net --mount .:/app -- sh -c 'cd /app && npm install'
# Now "project" has OS deps + Node + node_modules baked in
shuru run --from project --mount .:/app -- sh -c 'cd /app && npm test'
```

## Project Config (shuru.json)

Place `shuru.json` in the project root to avoid repeating flags:

```json
{
  "cpus": 2,
  "memory": 2048,
  "disk_size": 4096,
  "allow_net": true,
  "ports": ["8080:80"],
  "mounts": ["./src:/workspace"],
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

CLI flags override config values. When `secrets` are configured, the guest receives placeholder tokens and the proxy substitutes real values on HTTPS requests to allowed hosts. A secret can also be minted by a `command` and refreshed before it expires. See [references/config.md](references/config.md) for all fields.

## Important Constraints

- **Networking is off by default.** You must pass `--allow-net` to install packages or make HTTP requests. Port forwarding with `-p` works without it.
- **The guest is Debian Linux (aarch64).** Use `apt-get install` for packages.
- **Ephemeral by default.** Everything is discarded on exit unless you checkpoint.
- **Mounts are read-only by default.** Guest writes go to a tmpfs overlay and are discarded on exit. Use `:rw` suffix + `--allow-host-writes` to write to the host.
- **No systemd in the guest.** Long-running daemons are declared as guest services (see references/recipes.md); `systemctl` does not exist.
- **Apple Silicon macOS**, plus experimental Linux ARM64 hosts with `/dev/kvm`.
- **Default resources:** 2 CPUs, 2048 MB RAM, 4096 MB disk. Override with `--cpus`, `--memory`, `--disk-size`.

## Deep-Dive Documentation

- [references/checkpoints.md](references/checkpoints.md): checkpoint lifecycle, update, stacking, disk usage
- [references/recipes.md](references/recipes.md): recipe.json format, references, trust, guest services
- [references/config.md](references/config.md): shuru.json fields and resolution order
- [references/networking.md](references/networking.md): allow-net, port forwarding, proxy behavior
