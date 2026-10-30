# shuru

Local-first microVM sandbox for AI agents on macOS, with experimental Linux support.

Shuru boots lightweight Linux VMs for AI agents. On macOS it uses Apple's Virtualization.framework. On Linux it uses a KVM backend that is now available as an experimental release build for ARM64 hosts. Every sandbox is ephemeral: the rootfs resets on every run, giving agents a disposable environment to execute code, install packages, and run tools without touching your host.

> [!WARNING]
> **Experimental Linux support.** Linux builds are available for testing, but they are not ready for production use yet. Expect rough edges, missing polish, and compatibility gaps.

## Requirements

- macOS 14 (Sonoma) or later on Apple Silicon
- Linux ARM64 with KVM access (`/dev/kvm`) for experimental testing only

On Linux, keep the data directory (`~/.local/share/shuru` or `SHURU_DATA_DIR`) on a filesystem with reflinks, XFS with `reflink=1` or btrfs. Every boot and every checkpoint is then a copy-on-write clone that costs no disk and no time, as on APFS. On ext4 shuru falls back to copying the disk image, which turns a one second boot into ten and makes each checkpoint a full copy.

## Install

```sh
brew tap superhq-ai/tap && brew install shuru
```

Or via the install script:

```sh
curl -fsSL https://raw.githubusercontent.com/superhq-ai/shuru/main/install.sh | sh
```

The install script supports macOS on Apple Silicon and experimental Linux ARM64. Linux users can also download the `linux-aarch64` release tarball manually from GitHub Releases if they prefer.

> [!NOTE]
> Homebrew remains macOS-only. Linux installs via the script are still experimental and not ready for production use.

## Usage

```sh
# Interactive shell
shuru run

# Run a command
shuru run -- echo hello

# With network access
shuru run --allow-net

# Restrict to specific hosts
shuru run --allow-net --allow-host api.openai.com --allow-host registry.npmjs.org

# Use a specific DNS resolver
shuru run --allow-net --dns-resolver 1.1.1.1 -- curl https://example.com

# Custom resources
shuru run --cpus 4 --memory 4096 --disk-size 8192 -- make -j4
```

Disks default to 4096 MB, which is also the minimum. Larger disks grow in
the guest at boot and cannot shrink below their source image. Sparse files
and reflinks keep unused capacity from becoming an allocation. Checkpoints
freeze ext4 before copying and replace the saved disk only after the seal
is confirmed.

### Directory mounts

Share host directories into the VM using VirtioFS. By default the host directory is read-only; guest writes go to a tmpfs overlay layer (discarded when the VM exits). Append `:rw` to make the mount read-write — guest writes go directly to the host filesystem.

```sh
# Mount a directory (guest can read, writes go to overlay — host is untouched)
shuru run --mount ./src:/workspace -- touch /workspace/test.txt
ls ./src/test.txt   # not found — write stayed in the overlay

# Read-write mount (guest writes land on host, requires --allow-host-writes)
shuru run --allow-host-writes --mount ./src:/workspace:rw -- touch /workspace/test.txt
ls ./src/test.txt   # found — write went to host

# Multiple mounts
shuru run --mount ./src:/workspace --mount ./data:/data -- sh
```

Mounts can also be set in `shuru.json` (see [Config file](#config-file)).

> [!NOTE]
> Directory mounts require checkpoints created on v0.1.11+. Existing checkpoints work normally for all other features. Run `shuru upgrade` to get the latest version.

### Port forwarding

Forward host ports to guest ports over vsock. Works without `--allow-net` — the guest needs no network device.

```sh
# Install python3 into a checkpoint, then serve with port forwarding
shuru checkpoint create py --allow-net -- apt-get install -y python3
shuru run --from py -p 8080:8000 -- python3 -m http.server 8000

# From the host (in another terminal)
curl http://127.0.0.1:8080/

# Multiple ports
shuru run -p 8080:80 -p 8443:443 -- nginx
```

Port forwards can also be set in `shuru.json` (see [Config file](#config-file)).

### Checkpoints

Checkpoints save the disk state so you can reuse an environment across runs.

```sh
# Set up an environment and save it
shuru checkpoint create myenv --allow-net -- sh -c 'apt-get install -y python3 gcc'

# Run from a checkpoint (ephemeral -- changes are discarded)
shuru run --from myenv -- python3 script.py

# Branch from an existing checkpoint
shuru checkpoint create myenv2 --from myenv --allow-net -- sh -c 'pip install numpy'

# Rebuild in place: the old checkpoint stays until the new build succeeds
shuru checkpoint update myenv --allow-net -- sh -c 'apt-get install -y python3 gcc make'

# Or layer onto the existing one
shuru checkpoint update myenv --from myenv --allow-net -- sh -c 'pip install numpy'

# List and delete
shuru checkpoint list
shuru checkpoint delete myenv
```

### Secrets

Secrets keep API keys on the host. The guest receives a random placeholder token; the proxy substitutes the real value only on HTTPS requests to the specified hosts. The real secret is never placed in the VM. Bind a secret only to hosts you trust with it: responses come back untouched, so a bound host that reflects the request, as echo and debug endpoints do, shows the real value to the guest.

```sh
# Inject a secret via CLI
shuru run --allow-net --secret API_KEY=OPENAI_API_KEY@api.openai.com -- curl https://api.openai.com/v1/models

# Multiple secrets
shuru run --allow-net \
  --secret API_KEY=OPENAI_API_KEY@api.openai.com \
  --secret GH_TOKEN=GITHUB_TOKEN@api.github.com \
  -- sh
```

Format: `NAME=ENV_VAR@host1,host2` — `NAME` is the env var the guest sees, `ENV_VAR` is the host env var with the real value, and hosts are where the proxy substitutes it.

Secrets can also be set in `shuru.json` (see [Config file](#config-file)).

### Config file

Shuru loads `shuru.json` from the current directory (or `--config PATH`). All fields are optional; CLI flags take precedence.

```json
{
  "cpus": 4,
  "memory": 4096,
  "disk_size": 8192,
  "allow_net": true,
  "ports": ["8080:80"],
  "mounts": ["./src:/workspace", "./data:/data"],
  "command": ["python", "script.py"],
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

The `network.allow` list restricts which hosts the guest can reach. Omit it to allow all hosts.

### Environment variables

Per-machine settings live in the environment rather than in `shuru.json`, so
a project file never decides where assets live or how checkpoints are stored
on someone else. None of these can grant a permission; networking, mounts and
host writes stay on flags and in the project file.

| Variable | Effect |
|---|---|
| `SHURU_DATA_DIR` | Where assets, checkpoints and instance data live. Default `~/.local/share/shuru`. |
| `SHURU_KERNEL`, `SHURU_INITRD`, `SHURU_ROOTFS` | Boot from these files instead of the installed assets. Same as `--kernel`, `--initrd` and `--rootfs`, which win when both are given. |
| `SHURU_STORAGE` | `cas` saves new checkpoints as `.idx` indexes into the chunk store under the data dir. Unset or any other value keeps the default flat `.ext4` copies. A `.idx` checkpoint always boots through the chunk store. |
| `SHURU_BOOT_TRACE` | When set, prints boot timings to stderr. |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | Extra roots for the proxy when it opens its own TLS connection to a host a secret is bound to. Needed behind a TLS interceptor such as Netskope, since that connection does not read the macOS keychain. CLI downloads use the platform store and need this only on Linux. |

## SDK

Use shuru programmatically from TypeScript with the [`@superhq/shuru`](https://www.npmjs.com/package/@superhq/shuru) package.

```sh
bun add @superhq/shuru
```

```ts
import { Sandbox } from "@superhq/shuru";

const sb = await Sandbox.start({ from: "python-env" });

const result = await sb.exec("python3 -c 'print(1+1)'");
console.log(result.stdout); // "2\n"

await sb.checkpoint("after-run"); // saves disk state and stops the VM
```

See the [SDK README](packages/sdk/README.md) for full API docs.

## Agent Skill

Shuru ships as an [agent skill](https://agentskills.io) so AI agents (Claude Code, Cursor, Copilot, etc.) can use it automatically.

```sh
# Install via Vercel's skills CLI
npx skills add superhq-ai/shuru

# Or manually copy into your project
cp -r skills/shuru .claude/skills/shuru
```

Once installed, agents will use `shuru run` whenever they need sandboxed execution.

## Browser Use

The `browser` recipe installs Chromium from Debian and runs it headless as a
guest service, under an unprivileged user, with the DevTools protocol on
port 9222 inside the VM. Forward that port and point any CDP client at it:

```sh
shuru run --recipe browser -p 9222:9222 -- sleep infinity
curl -s http://127.0.0.1:9222/json/version
```

The Browser Use example does exactly that. Chromium stays inside the
microVM and only its CDP port reaches the host:

```sh
OPENAI_API_KEY=... \
uv run --with browser-use --with openai examples/browser-use.py
```

## Recipes

A recipe is a versioned description of a checkpoint: a `recipe.json` with
metadata and services, plus shell steps that run in fresh boots. The official
catalog lives in [superhq-ai/recipes](https://github.com/superhq-ai/recipes).

```sh
shuru recipe list                                  # the official catalog
shuru recipe show docker                           # metadata, services and every step
shuru checkpoint create docker --recipe docker     # build the checkpoint under that name
shuru run --from docker -- docker ps               # and start from it like any other
shuru run --recipe docker -- docker ps             # or both at once: builds on first use, reuses after
shuru checkpoint create mydocker --recipe docker   # the same recipe under a name of your own
shuru checkpoint update docker --recipe docker     # force a rebuild at the current revision
shuru run --recipe docker --checkpoint ci -- ...   # the one-liner under a name of your own
shuru run --recipe owner/repo/path@v1 --yes -- ... # third-party, pinned to a tag
shuru run --recipe ./my-recipe -- ...              # local directory while developing
```

References resolve to an exact commit, which is printed and recorded next to
the checkpoint. `shuru checkpoint list` shows where each one came from, and a
changed recipe rebuilds on the next `shuru run --recipe`. Builds use the
network allowlist the recipe declares, never the project `shuru.json`. See
docs/rfcs/0005-recipes.md.

## Docker

Docker Engine runs inside the VM through the `docker` recipe. Build the
checkpoint once, then start from it; after that containers run with
networking disabled:

```sh
shuru checkpoint create docker --recipe docker

# Serve the pre-pulled nginx-unprivileged image on host port 8080
shuru run --from docker -p 8080:8080 -- \
  docker run --rm -p 8080:8080 nginxinc/nginx-unprivileged:1.31.7-alpine-slim
curl http://127.0.0.1:8080/
```

`shuru run --recipe docker -- ...` does both: it builds the checkpoint on
first use and reuses it afterwards.

dockerd is a guest service: it starts when the VM boots, the command waits
until `docker info` answers, and the daemon is stopped and the disk synced
before the VM powers off. Pulling other images needs `--allow-net`, and the
usual `--allow-host` allowlist applies to container traffic too: containers
reach the internet only through the Shuru proxy, and nothing at all without
`--allow-net`.

Any image can declare services the same way. A JSON file under
`/etc/shuru/services/` names the command to run, an optional readiness
check, and the stop timeout:

```json
{"exec": ["dockerd"], "ready": ["docker", "info"], "stop_timeout_secs": 30}
```

See docs/rfcs/0004-guest-services.md for the full contract.

Docker isolation inside the VM stays at its defaults (seccomp, no new
privileges). Do not use `--privileged` or mount the Docker socket into a
container; neither crosses the VM boundary, but there is no reason to weaken
the inner layer. See docs/rfcs/0003-docker-on-shuru.md for the full design,
limits, and verification evidence.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for release notes and breaking changes.

## Support

<a href="https://buymeacoffee.com/harshdoesdev" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-yellow.png" alt="Buy Me A Coffee" height="40"></a>

## Bugs

File issues at [github.com/superhq-ai/shuru/issues](https://github.com/superhq-ai/shuru/issues).

## Star History

<a href="https://star-history.dera.page/#superhq-ai/shuru&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://star-history.dera.page/svg?repos=superhq-ai/shuru&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://star-history.dera.page/svg?repos=superhq-ai/shuru&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://star-history.dera.page/svg?repos=superhq-ai/shuru&type=date&legend=top-left" />
 </picture>
</a>
