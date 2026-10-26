# Changelog

## 0.8.0

### Guest services

An image can declare long running processes in
`/etc/shuru/services/<name>.json`. The guest init starts them at boot,
waits for their readiness check before running the command, and stops them
with SIGTERM before the VM powers off. The host now asks the guest to shut
down over vsock, which also syncs the disk, so checkpoints are cloned from
a consistent image on every stop path. `Sandbox::stop()` is graceful and
`Sandbox::kill()` is the old hard stop. See docs/rfcs/0004-guest-services.md.

### Recipes

`shuru run --recipe REF` and `shuru checkpoint create NAME --recipe REF`
build a checkpoint from a recipe: a directory with `recipe.json` and shell
steps, fetched from GitHub at an exact commit or read from a local path.
`shuru recipe list` shows the official catalog in superhq-ai/recipes and
`shuru recipe show REF` prints a recipe before it is built. Checkpoints built
this way record their source and revision, visible in `shuru checkpoint
list`, and a changed recipe rebuilds on the next run. `--checkpoint NAME`
on `run` picks the checkpoint name instead of the recipe name. Third-party
recipes ask for confirmation or `--yes`. The catalog ships `docker` and
`browser`, headless Chromium as a guest service with the DevTools protocol
on port 9222, which the Browser Use example now uses. See
docs/rfcs/0005-recipes.md.

### checkpoint update

`shuru checkpoint update NAME ...` takes the same arguments as `create` and
replaces an existing checkpoint, or creates it when missing. The new
checkpoint is built under a staging name and swapped in only when the
command exits 0, so a failed update leaves the old one in place.
`--from NAME` layers the command onto the existing checkpoint and
`--recipe REF` rebuilds from a recipe. Closes #44.

### Docker Engine runs inside the VM

The `docker` recipe installs pinned docker-ce 29.9.2 and containerd 2.4.2,
declares dockerd as a guest service, and pre-pulls one image, so
`shuru checkpoint create docker --recipe docker` followed by
`shuru run --from docker -- docker run ...` just works, as does
`shuru run --recipe docker` on its own. Containers get the same boundary as
any guest process: no egress without `--allow-net`, the host allowlist
applied by the proxy, host ports only through `-p`.

To support it, the guest kernel gains cgroup v2 controllers, cgroup BPF,
seccomp filters, veth, and the netfilter targets dockerd installs, and moves
to Linux 6.12.114. The guest init now mounts cgroup2 on /sys/fs/cgroup and
tmpfs on /run and /dev/shm. `/run` is therefore no longer persisted into
checkpoints.

`SHURU_DATA_DIR` overrides the data directory so builds and tests can use
their own kernel, initramfs and checkpoints. `scripts/build-kernel-hf.sh`
builds the kernel on a Hugging Face CPU job.

See docs/rfcs/0003-docker-on-shuru.md.

## 0.7.0

### Secrets can be refreshed while the VM runs

A secret can now be backed by a command that mints it, instead of a host
environment variable that is frozen when the VM boots:

```json
"secrets": {
  "GITHUB_TOKEN": {
    "command": ["./scripts/mint-installation-token.sh"],
    "hosts": ["api.github.com", "github.com"]
  }
}
```

The command prints `{"version":1,"value":"...","expires_at":"..."}` and the
proxy re-runs it as the value nears expiry, so credentials with short
lifetimes (GitHub App installation tokens last an hour) no longer strand a
long running task. The placeholder inside the guest never changes, so the
rotation is invisible to the VM and the real value still never enters it.
Set `ttl` for a command that cannot report an expiry of its own.

Values are held in memory only, resolution is single flight so a burst of
requests mints once, and a command that fails while a live value is cached
keeps serving that value. A command that fails with nothing usable cached
refuses the connection instead of sending a placeholder upstream.

See docs/rfcs/0002-refreshable-secrets.md.

### Secrets are substituted in the request head

Substitution used to be a blind byte replace over the whole stream, which had
two consequences. A placeholder straddling two reads was forwarded
unsubstituted, so the secret never got injected: reproduced on a large upload
where 3 of 2000 placeholders leaked. And a secret in a request body broke the
declared body length, hanging the request.

Substitution now applies to the request line and any header value, whatever
the header is named, so a custom `X-My-Vendor-Token` needs no configuration
and a credential in a query parameter is covered. Bodies stream through
untouched, so no declared length is ever invalidated. HTTP framing is read
but never rewritten, only to find where the next request begins on a reused
connection, and anything ambiguous falls back to a plain byte tunnel.

This is also less work than before, since only heads are scanned rather than
every byte of every body.

Behaviour change: a placeholder in a request body is no longer substituted.
That never worked for any real credential length, since placeholders are a
fixed 30 bytes and a differing length broke framing, but the failure moves
from a hung request to a clean 401.

A live connection also re-resolves its secrets every 5 seconds, so a rotation
reaches a connection that is already open instead of waiting for it to
reconnect.

### Crates

- `shuru-proxy` 0.3.0: breaking. `SecretConfig.from` is now
  `Option<String>`, with `SecretConfig::from_env` and
  `SecretConfig::from_command` as the constructors. Adds `command`, `ttl`
  and `config_dir`.
- `shuru-sdk` 0.4.0: breaking by re-export of the above.

## 0.6.5

### Direct disk mode is now the default

Plain `shuru run` boots in about 0.45s warm, down from about 2s. The root
disk is an APFS clonefile of the base image attached directly, instead of
going through the CAS NBD server. The CAS path is opt-in via
`SHURU_STORAGE=cas`, and booting `--from` a CAS (`.idx`) checkpoint always
uses NBD automatically since the chunk store is the only place its content
exists. This also fixes a bug where `SHURU_STORAGE=direct` combined with a
`.idx` checkpoint booted an empty placeholder disk.

Consequence: `shuru checkpoint create` now produces `.ext4` clonefile
checkpoints by default; set `SHURU_STORAGE=cas` to produce `.idx` chunk
store checkpoints. Both formats boot with `--from` and no env var.

### Fix: checkpoint data loss on late guest writes

`checkpoint create` stopped the VM without flushing the guest page cache,
so writes made shortly before the stop could silently vanish from the saved
checkpoint, and journaled images replayed journal recovery plus orphan
inode cleanup on every boot from the checkpoint. The guest now runs `sync`
before the VM is stopped.

### Boot path

- vsock readiness poll tightened from 200ms to 10ms granularity (same 10s
  total timeout), removing up to 190ms of detection slack per boot
- unused virtio memory balloon device removed from the VM configuration
- kernel cmdline gains `init=/usr/bin/shuru-init`, so booting without an
  initramfs lands on the correct init
- `SHURU_BOOT_TRACE=1` prints per-stage boot timings for the host side

### Guest (`shuru-guest` 0.3.6)

- boot stage logs now carry monotonic timestamps
- PID 1 stdio is hardened: a missing or broken console can no longer make
  init writes fail

### Crates

- `shuru-vm` 0.3.8: builder no longer configures a balloon device; adds
  `Sandbox::wait_ready()`
- `shuru-sdk` 0.3.8: dependency bump

## 0.6.1

### Linux backend: correct wall clock and working directory mounts

Two gaps in the experimental Linux KVM backend are closed. Both were verified
live on an AWS `a1.metal` ARM64 instance.

### Linux backend (`shuru-linux` 0.2.0)

- **PL031 RTC.** Exposes host wall clock to the guest via a minimal PL031
  MMIO device + FDT node. Fixes guests booting with clock pinned at
  1970-01-01, which previously broke every TLS handshake inside the sandbox
  ("certificate not yet valid"). Kernel already had `CONFIG_RTC_DRV_PL031`
  and `CONFIG_RTC_HCTOSYS`, so the system clock is seeded from `/dev/rtc0`
  at boot.
- **In-process virtio-fs passthrough.** `--mount` now works on Linux. The
  VMM speaks the FUSE wire protocol directly over the virtio-fs queue and
  forwards ops to the host filesystem via libc/std::fs. No external
  `virtiofsd` daemon or extra crate required. Covers INIT, LOOKUP, FORGET
  (single + batch), GETATTR, SETATTR, OPEN/READ/WRITE/RELEASE, OPENDIR and
  READDIR(PLUS), CREATE, UNLINK/MKDIR/RMDIR/RENAME(2), STATFS, FLUSH,
  FSYNC(DIR), ACCESS, and xattr stubs. Read-only mounts reject writes with
  EROFS at the op layer.

### VM (`shuru-vm` 0.3.6)

- `VmCreateConfig` gains a `mounts: Vec<(tag, host_path, read_only)>` field
  wired through from the existing `set_directory_sharing_devices()` plumbing.

### CLI (`shuru-cli` 0.6.1)

- Picks up the Linux backend fixes; no CLI-facing changes.

## 0.6.0

### Experimental Linux ARM64 support

Shuru now ships Linux ARM64 CLI builds using a KVM-based backend, alongside the
existing macOS builds. Setup guide: https://shuru.run/linux

Linux support is experimental, not production-ready yet. Homebrew remains
macOS-only; on Linux, use the install script.

### Linux backend (`shuru-linux` 0.1.0, new)

- Initial release of the KVM-based backend crate, published to crates.io
- Mirrors the `shuru-darwin` API surface (`VirtualMachine`, `VmState`, `VzError`,
  network attachment, terminal)

### VM (`shuru-vm` 0.3.5)

- `shuru-linux` dependency wired in unconditionally on Linux targets (no longer
  behind a `shuru-linux` feature flag)

### CLI (`shuru-cli` 0.6.0)

- `shuru upgrade` selects the correct tarball per host (`darwin-aarch64` or
  `linux-aarch64`) instead of hard-coding darwin

### Installer and CI

- `install.sh` detects Linux ARM64 and installs the matching tarball; prints an
  experimental-support warning on Linux
- Release workflow builds both `darwin-aarch64` and `linux-aarch64` CLI tarballs
  (adds `ubuntu-24.04-arm` runner for the Linux job)
- Crates-publish workflow includes `shuru-linux`

## 0.5.5

### Store (`shuru-store` 0.1.1)

- Content-addressable chunk store with BLAKE3 hashing and local filesystem backend
- NBD (Network Block Device) server for serving VM disks from the chunk store
- `ChunkIndex` with parent-chain resolution for delta-only checkpoints
- Lazy ingestion: chunks read from flat rootfs on first access, no upfront conversion
- S3 chunk store backend

### CLI (`shuru-cli` 0.5.5)

- CAS-backed VM disks via NBD: `shuru run` now uses the chunk store by default
- `SHURU_STORAGE=direct` env var to fall back to flat file mode
- Checkpoints saved as `.idx` (CAS delta index) when CAS is active, `.ext4` otherwise
- `checkpoint list` shows storage type and size for CAS checkpoints
- `--expose-host` flag for forwarding host ports to the guest via `host.shuru.internal`
- `--disk-size` flag to set the VM disk size

### VM (`shuru-vm` 0.3.3)

- NBD storage support: `SandboxBuilder::nbd_uri()` for attaching NBD-backed block devices
- `download()` method on `Sandbox` for downloading and extracting archives inside the guest
- Port forwarding for host-exposed ports via vsock

### Rust SDK (`shuru-sdk` 0.3.3)

- CAS storage support behind the `cas` feature flag
- `StorageMode` enum: `Direct` (default, flat file with CoW) or `Cas { cas_dir }` for chunk store
- Checkpoints saved as `.idx` when CAS is active, `.ext4` otherwise
- `download()` method with progress reporting
- `open_watch()` for inotify-backed filesystem change events
- `discard_overlay()` to revert file changes in overlay mounts
- File management: `read_dir()`, `mkdir()`, `rename()`, `chmod()`, `remove()`
- `expose_host` config for forwarding host ports to the guest
- `open_shell()` gains `cwd` and `extra_env` parameters

### Guest (`shuru-guest` 0.3.2)

- Download handler: fetch URLs, optionally extract `.tar.gz` archives, with progress reporting
- File management ops: `mkdir`, `read_dir`, `stat`, `remove`, `rename`, `copy`, `chmod`
- Filesystem watching via `inotify` with recursive directory support
- Overlay discard support for reverting file changes

### Protocol (`shuru-proto` 0.3.2)

- `Download`, `DownloadProgress` types for in-guest downloads
- `ReadDir`, `Mkdir`, `Rename`, `Chmod`, `Remove`, `DiscardOverlay` request/response types
- `DOWNLOAD_REQ`, `DOWNLOAD_PROGRESS` frame types

### Proxy (`shuru-proxy` 0.2.3)

- `ExposeHostMapping` config and DNS interception for `host.shuru.internal`
- Exposed host ports resolved to `127.0.0.1` on the host side

### Darwin (`shuru-darwin` 0.1.1)

- `VZNetworkBlockDeviceStorageDeviceAttachment` support for NBD-backed disks

### TypeScript SDK (`@superhq/shuru` 0.4.2)

- `exposeHost` option in `StartOptions` for forwarding host ports to the guest

## 0.5.4

### CLI (`shuru-cli` 0.5.4)

- Read-write mount support: `--mount ./src:/workspace:rw` (default remains read-only overlay)
- `--allow-host-writes` flag required for `:rw` mounts (same opt-in pattern as `--allow-net`)
- Mount host paths restricted to current working directory; `/` rejected as mount source or CWD
- BoringSSL for upstream TLS in proxy (Chrome-like TLS fingerprint passes Cloudflare)
- ALPN aligned to HTTP/1.1 only on MITM connections
- DNS AAAA queries return empty NOERROR (IPv4-only VM, fixes musl getaddrinfo)
- Removed rewrite rules from proxy

### VM (`shuru-vm` 0.3.2)

- `MountConfig` gains `read_only: bool` field. Consumers must add this field (`true` preserves existing behavior).
- `validate_checkpoint_name()` rejects path traversal in checkpoint names (fixes #17)

### Guest (`shuru-guest` 0.3.1)

- Direct VirtioFS mount for `:rw` mounts (skips overlay)

### Protocol (`shuru-proto` 0.3.1)

- `MountRequest.read_only` field with `serde(default = true)` for backward compatibility

### Proxy (`shuru-proxy` 0.2.2)

- Upstream TLS switched from rustls to BoringSSL (Cloudflare JA3/JA4 fingerprint compatibility)
- Guest-side ALPN set to HTTP/1.1 only (matches upstream, prevents protocol mismatch)
- DNS: empty NOERROR for AAAA queries instead of forwarding (IPv4-only stack)

### SDK (`shuru-sdk` 0.3.2)

- Re-exports updated `MountConfig` with `read_only` field
- Checkpoint name validation on boot and save

### TypeScript SDK (`@superhq/shuru` 0.4.1)

- `allowHostWrites` option in `StartOptions`
- `mounts` values support `:rw` suffix (e.g. `{ "./src": "/workspace:rw" }`)

## 0.4.1

### CLI (`shuru-cli` 0.4.1)

- Fixed `--allow-net` having no effect in `--stdio` mode. Proxy networking now works via the SDK.
- Secret environment variables are now injected into exec/spawn calls in stdio mode
- CA certificate installation for MITM proxying in stdio mode

### SDK (`@superhq/shuru` 0.3.1)

- `exec()` and `spawn()` now accept `string | string[]`. Array form passes argv directly with no shell interpretation.
- Added `shell` option to `ExecOptions` and `SpawnOptions` to override the default shell (e.g. `/bin/bash` instead of `sh`)
- New exported type: `ExecOptions`

## 0.4.0

### Streaming spawn, kill, and file watching

Full streaming I/O across the guest, CLI, and SDK - spawn long-running processes, stream stdout/stderr in real-time, kill processes, write to stdin, and watch files for changes.

#### Guest (`shuru-guest` 0.2.0)

- Streaming piped exec: dedicated threads for stdout, stderr, and stdin relay with mpsc channel for frame serialization (no interleaved writes)
- `cwd` support in both piped and TTY exec modes
- Guest-side file watching via raw `libc::inotify` with recursive directory traversal, auto-watching new subdirectories, and `poll(2)` for clean shutdown on vsock hangup
- New frame types: `KILL`, `WATCH_REQ`, `WATCH_EVENT`

#### CLI (`shuru-cli` 0.4.0)

- Rewrote `stdio.rs` from synchronous request-response to concurrent multiplexed architecture
- Main thread reads stdin JSON-RPC, dedicated event thread writes notifications to stdout
- Per-process std::threads relay vsock frames as JSON-RPC `output`/`exit` notifications
- New methods: `spawn` (returns pid, streams in background), `kill`, `input` (stdin forwarding), `watch` (file change events)
- `SharedWriter` (`Arc<Mutex<Stdout>>`) for thread-safe output from multiple process threads
- `ProcessHandle` with `mpsc::Sender<ProcessInput>` for stdin/kill forwarding to the correct vsock connection
- Backward-compatible: `exec`, `read_file`, `write_file`, `checkpoint` unchanged

#### Protocol (`shuru-proto` 0.2.0)

- Added `KILL` (0x07), `WATCH_REQ` (0x30), `WATCH_EVENT` (0x31) frame types
- Added `cwd` field to `ExecRequest` (backward-compatible `Option`)
- Added `WatchRequest` and `WatchEvent` types

#### VM (`shuru-vm` 0.2.0)

- `open_exec()`: connect vsock for streaming, returns raw `TcpStream` for caller-managed I/O
- `open_watch()`: connect vsock for file watching, returns stream emitting `WATCH_EVENT` frames

#### SDK (`@superhq/shuru` 0.3.0)

- `sandbox.spawn(command, opts?)` — real-time stdout/stderr streaming via `SandboxProcess` handle
- `sandbox.watch(path, handler, opts?)` — guest-side inotify file change events
- `SandboxProcess`: `.on("stdout" | "stderr" | "exit")`, `.write()`, `.kill()`, `.exited`, `.pid`
- `SpawnOptions` (`cwd`, `env`), `WatchOptions` (`recursive`), `FileChangeEvent` type
- JSON-RPC notification dispatch for `output`, `exit`, `file_change` in `ShuruProcess`
- Unit tests (13) with mock shuru binary: spawn streaming, kill, watch, concurrent operations
- Integration tests (12) against real VM: streaming, stdin, kill, file creation/modification/deletion, recursive watch, concurrent watch+spawn

## 0.3.3

- Added `--secret` and `--allow-host` CLI flags for inline proxy config (no `shuru.json` required)
- Replaced `shuru.epoch` cmdline hack with proper PL031 RTC, now, the kernel sets wall clock at boot automatically
- Added `libatomic1` to rootfs
- SDK: `secrets` and `network` options now map to CLI flags directly (no temp config files)

## 0.3.2

- Fixed proxy corrupting large HTTP responses (e.g. `apt-get update`) due to dropped bytes when smoltcp TX buffer was full

## 0.3.1

- Fixed TLS certificate validation failures by syncing guest clock from host via kernel cmdline

## 0.3.0

### Custom minimal kernel, faster boot

Boot time reduced from ~5s to ~1s by replacing the Debian cloud kernel with a custom minimal Linux 6.12.x kernel.

- Custom kernel built from `kernel/shuru_defconfig` with all VirtIO drivers built-in (~8MB, no loadable modules)
- Simplified initramfs with no module loading, no DHCP, no /dev/vda polling
- Quiet boot by default, use `--verbose` to see kernel output

### Proxy-based networking

All guest network traffic now flows through a userspace proxy on the host. No NAT device, no direct internet access.

- Domain allowlists via `shuru.json`
- Secret injection: API keys stay on host, placeholder tokens swapped at proxy
- MITM TLS only when secrets need to be injected; blind-tunneled otherwise
- Fixed placeholder token collision with atomic counter
- Instance directory cleanup on error and PID reuse

**Note:** Existing checkpoints created with 0.2.x will continue to work.

## 0.2.0

### Breaking: Guest OS migrated from Alpine Linux to Debian

The guest VM now runs **Debian 13 (trixie)** instead of Alpine Linux 3.21. This is a breaking change for existing checkpoints and workflows that use `apk`.

**Why:** Alpine's musl libc is incompatible with many tools that assume glibc (e.g., Claude Code, VS Code server, many pre-built binaries). Debian's glibc resolves this and aligns with the standard environment developers expect.

**What changed:**

- **Package manager:** `apk add` -> `apt-get install -y`
- **Package names:** Some differ between Alpine and Debian (e.g., `build-base` → `build-essential`, `py3-pip` → `python3-pip`)
- **Kernel:** Alpine `linux-virt` -> Debian `linux-image-cloud-arm64`
- **Pre-installed tools:** `curl`, `git`, `jq`, `less`, `procps`, `openssh-client`, `iproute2`, `xz-utils`

**Migration guide:**

1. Run `shuru upgrade` to get the new CLI and OS image.
2. Recreate any checkpoints using `apt-get` instead of `apk`:

```bash
# Before (Alpine)
shuru checkpoint create myenv --allow-net -- apk add nodejs npm

# After (Debian)
shuru checkpoint create myenv --allow-net -- apt-get install -y nodejs npm
```

3. Existing Alpine checkpoints will continue to boot (same kernel architecture, same init path), but new VMs start from Debian.
