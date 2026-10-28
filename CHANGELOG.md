# Changelog

## Unreleased

### Fast boot

`shuru run -- true` on an M-series Mac goes from about 300 ms to about 160
ms. The guest agent is now `/init` in an uncompressed initramfs and mounts
the root itself. The old initramfs gunzip, `e2fsck` and offline
`resize2fs` are gone from the boot path. The kernel replays an old journal
when it mounts, and a larger disk grows online with `EXT4_IOC_RESIZE_FS`.
Networking is configured by the agent. The rootfs no longer carries a copy
of the agent.

The kernel tick goes from 100 Hz to 1000 Hz. virtio_pci_modern sleeps one
millisecond at a time while a device resets, and on the asynchronous Apple
reset each of those sleeps cost two jiffies, 10 to 20 ms per device at 100
Hz. Guest uptime when the command starts drops from 150 ms to 70 ms.

On APFS, growing a clone of a fragmented file costs time in proportion to
its extent count, 0.5 to 1.5 s for the base image built by
`prepare-rootfs.sh`. The base is now rewritten sequentially and extended to
4096 MB once, after download or on the first boot that finds it short, and
a clone then costs well under a millisecond.

### Sealed checkpoints

Every checkpoint path now freezes ext4 in the guest before the host copies
the disk, so a saved disk is clean and needs no journal replay. A seal
carries an id and a lease; the guest thaws itself when the lease runs out,
and the host publishes the copy only after a matching thaw. A failed save
keeps the previous checkpoint. A save that died leaves a staging directory
that the next save of the same name reuses. CAS checkpoints are written as
standalone indexes, so replacing one cannot form a parent cycle or depend
on the base image path.

The root is mounted `errors=remount-ro`. A disk with recorded ext4 errors
refuses to boot and refuses to seal.

### Freed space goes back to the host

A seal runs `FITRIM` for free extents of 1 MiB and up, and `trim` is
available on a running sandbox from the CLI stdio protocol and both SDKs.
The Linux block device supports discard and write zeroes, mapped to
`fallocate` hole punching, with every range bounds checked.

### Disk size

Disks default to 4096 MB, which is now also the minimum. A disk never
shrinks below its source.

### macOS: a VM that powers off during boot no longer hangs the host

Connecting to the guest waited forever if the VM stopped while the connect
was pending, because the framework never calls the completion handler. It
now notices the stopped VM and fails.

### Watch events carry their names again

`file_change` notifications sent `"2"` and `"3"` instead of `"modify"` and
`"delete"`, and the guest reported every new file as modified. Events are
`create`, `modify`, `delete` and `rename` again, a rename arrives once for
each path, and `shuru_proto::watch_kind::name` maps a kind to its name.

### Files of 1 MB and up can be read and written

`read_file` and `write_file` sent the whole file in one frame, and frames
stop at 1 MB, so larger files failed with a broken pipe. Contents now
travel in 256 KiB frames. A failed write reports the guest error, and the
TypeScript SDK no longer rescans a long response line for every chunk it
receives, which made a 64 MB read take 1.7 s instead of 0.5 s.

### Host memory no longer grows with what the guest does

A guest could grow the host shuru process without limit: by writing to a
socket the remote never read, by flooding a socket the guest never read, or
by printing to a spawned process stdout the client never consumed. The old
build reached 2 GB in five seconds on the last case. The proxy now holds a
fixed window per connection each way and its smoltcp buffers are halved to
512 KiB and 256 KiB at the same measured throughput. stdio and SDK event
queues are bounded, and buffered exec output and file reads fail past 64 MiB
with an error that points at streaming instead. On Linux the VMM validates virtqueue
sizes, ring addresses and descriptor chains, and caps virtio-fs handles,
inodes and request sizes, so a guest cannot loop, crash or inflate it either.
RFC 0011 has the rule, the remaining per-VM ceilings and the cgroup envelope
for hosted nodes.

### Kill reaches a process that stopped reading stdin

Stdin and the kill signal shared one connection, so a process that stopped
reading stdin left the kill queued behind stdin nobody would take, and the
SDK kept piling stdin up in the shuru process. A kill now sends SIGTERM as
before and, if the process is still running two seconds later, SIGKILL on a
connection of its own. The guest also kills a process group when the host
hangs up on it. In the TypeScript SDK, `write()` on a spawned process now
returns a promise that resolves once the data reaches the VM; up to 1 MiB
is in flight and the rest waits in the calling process. The stdio server
refuses more than 4 MiB of unacknowledged stdin per process. A single write
over 1 MiB used to break the stream; it is now split.

### Short connections no longer get refused at random

The network proxy added a listener for each guest SYN it inspected, but
smoltcp also read frames straight from the guest while it polled. A SYN that
arrived during a poll skipped inspection and was answered with a reset, so a
loop of short connections failed with "Connection refused" somewhere between
150 and 475 connections in. smoltcp now only sees inspected frames, and 600
back to back connections succeed.

### Linux: the guest console works

The Linux backend emulates a PL011 serial port and boots the guest with
`console=ttyAMA0`, but the kernel was built without the PL011 driver, so a
Linux guest printed nothing, `--verbose` showed no kernel log and
`--console` had no console to attach to. The driver is now built in.

The Linux bootloader also dropped `quiet` from every command line. With a
working console that would print the whole kernel log through the emulated
UART on every boot, about 230 ms on a1.metal, only to discard it. Linux now
keeps `quiet` unless `--verbose` is set, as macOS does.

### Checkpoint saves report stopped services again

`shuru checkpoint create --verbose` prints the services that were stopped
before the save, and every save prints services that ignored SIGTERM.

## 0.10.0

### Linux: a VM can no longer reach another VM

On a Linux host every VM asked vhost-vsock for guest CID 3. vhost CIDs are
global to the host kernel, so the second VM got EADDRINUSE, the error was
printed and ignored, and its host side connected to CID 3 anyway: the first
VM. Reproduced on a1.metal with 0.9.3, a second `shuru run` executed inside
the first sandbox and printed its marker file and kernel boot_id. macOS was
not affected.

The Linux backend now emulates virtio-vsock in the VMM instead of using
vhost-vsock. Each VM has its own device and its own CID space, the host end
of every connection is a socketpair inside the shuru process, and no other
process on the host can reach the guest agent. Host data stays in that
socket until the guest posts receive buffers, then one read puts it
straight into guest memory across as many buffers as the credit the guest
grants allows, so nothing queues in the VMM. Guest data goes from guest
memory to the socket in one write and is only copied when the socket is
full. Connections opened by the guest are refused. The vhost_vsock module
is no longer needed.

### Linux: devices no longer share a lock

Every virtio device used to sit behind one lock, and disk and shared folder
requests ran on the vCPU that rang the doorbell while holding it, so one
busy device stalled every vCPU touching any other. Each device now has its
own lock and its own thread. Doorbells go to an eventfd inside KVM, so
ringing one no longer exits to the VMM, and interrupts fire only when
buffers were used and the guest asked for them. Raising and acknowledging
an interrupt now happen under one lock, which closes a window where an
acknowledgement could lower the line over a newly raised reason.

### The guest checks who is connecting

Every boot gets a random 32 byte token and a 16 byte instance id on the
kernel command line. The guest agent requires the token as the first frame
of every connection, on the control channel and on port forwards, and
answers with the instance id, which the host checks. A connection that
reaches the wrong VM is refused instead of served, and a guest booted
without a token stops. The token is redacted from `--verbose` and
`--console` output. This applies on macOS too.

The protocol version is 2. An OS image older than this release does not
answer the hello, and `shuru upgrade` fetches the matching one.

Measured on a1.metal against 0.9.3 on the same host, three runs each:

| | 0.9.3 (vhost) | 0.10.0 |
|---|---|---|
| exec round trip p50 | 1.6 ms | 1.8 ms |
| 16 parallel execs | 35 ms | 36 ms |
| guest to host, 128 MB | 117 to 191 MB/s | 434 to 451 MB/s |
| host to guest, 128 MB | 353 to 366 MB/s | 315 to 360 MB/s |
| shared folder read, 128 MB | 241 to 245 MB/s | 343 to 384 MB/s |

### Crates

- `shuru-proto` 0.5.0: `hello::Identity`, the `HELLO` and `HELLO_OK`
  frames, protocol version 2.
- `shuru-linux` 0.4.0: breaking. Userspace virtio-vsock,
  `VirtualMachine::new` returns a `Result`, `GUEST_CID` and `AF_VSOCK` are
  gone. `kvm::devices`: the bus dispatches to `MmioHandler`, devices take
  an `Interrupt`, and `VirtioBackend::process_queue` returns whether it
  used a buffer.
- `shuru-vm` 0.6.0 and `shuru-sdk` 0.8.0: the hello on every connection.
  They need a 0.10.0 OS image.
- `shuru-guest` 0.5.0.

## 0.9.3

### Guest memory goes back to the host

Every VM now carries a virtio memory balloon (RFC 0006). On a Linux host
the guest kernel reports the pages it has freed and the host discards
them, so an idle sandbox costs what the guest is using, not what it has
touched since boot. Measured on a1.metal: a guest that filled and freed
1 GB took the host process from 1109 MB resident back to 87 MB within
seven seconds, with nobody asking.

A caller can also set a memory target. The guest gives up the difference
through the balloon and takes it back when the target rises, on Linux and
on macOS. `shuru run` is unchanged.

### Crates

- `shuru-linux` 0.3.2: the balloon backend, `set_memory_target` and
  `ballooned_bytes` on `VirtualMachine`.
- `shuru-darwin` 0.2.2: `set_memory_target` through the framework balloon.
- `shuru-vm` 0.5.2: the balloon is attached on both platforms;
  `Sandbox::set_memory_target`.
- `shuru-sdk` 0.7.1: `AsyncSandbox::set_memory_target` and the `balloon`
  example.

## 0.9.2

### The working disk can no longer outlive its run

Every run clones the base image, and until now the clone was a named file
under `instances/<pid>` that the process deleted on a clean exit. A run
ended by SIGTERM, a kill, a crash or an SDK program dying left the clone
behind, and `shuru prune` could not tell: SDK directories were never
matched, and a reused PID looked alive. Disk usage grew with every
interrupted run.

The clone is anonymous now. On Linux it is created with `O_TMPFILE` and
never has a name; on macOS the framework needs a path, so the clone is
named for the two syscalls between creation and open, then unlinked and
attached through `/dev/fd`. The kernel frees the blocks when the last
descriptor closes, however the process ends. Checkpoints are cloned out of
the open descriptor, instantly on APFS, XFS and btrfs. The `instances`
directory, the PID naming and `shuru prune` are gone; `shuru upgrade`
removes a leftover `instances` directory from earlier versions.

Measured on macOS: a run killed with SIGKILL while writing 600 MB returned
the space to the volume within seconds and left no file behind.

### Crates

- `shuru-darwin` 0.2.1: `clone_path` and `clone_fd` (APFS clones).
- `shuru-linux` 0.3.1: `open_anonymous` and `reflink`.
- `shuru-vm` 0.5.1: `WorkDisk`, the anonymous clone, with `attach_path`
  and `save_to`. Stays free of unsafe code.
- `shuru-sdk` 0.7.0: breaking. `AsyncSandbox::instance_dir` is removed,
  there is no directory any more. The Linux SDK path now reflinks instead
  of copying the base image.

## 0.9.1

### CLI downloads trust the platform certificate store

`shuru upgrade`, `shuru init` and `shuru recipe` verified TLS against a
bundled root list, so a corporate TLS interceptor such as Netskope failed
with `UnknownIssuer` (#27). They now use the platform verifier: the system
keychain on macOS, the system store plus `SSL_CERT_FILE` and `SSL_CERT_DIR`
on Linux. The connection the proxy opens to a host a secret is bound to
already honours `SSL_CERT_FILE` and `SSL_CERT_DIR` on both platforms, and
that is the way to trust an interceptor there; documented in the README.

### Docs

The secrets section now says what responses from a bound host do: they come
back untouched, so a host that reflects the request shows the real value to
the guest. Bind a secret only to hosts you trust with it.

## 0.9.0

### Network throughput inside the VM, about 80 times faster

The proxy netstack that backs `--allow-net` capped every guest connection
at about 1 MB/s whatever the host could do. Contributed by rsclarke in #35:
the smoltcp poll loop no longer sleeps 1 ms between passes but waits on the
host socket, the proxy command channel and the stack timer; the TCP buffers
grow so window scaling can fill the pipe; and the VM network attachment
runs at a 65535 byte IP-MTU so the stack handles one 64 KiB frame per
syscall instead of forty at 1500 bytes. `apt-get`, `npm install` and
release downloads inside the VM now run close to host speed.

On top of that: per-connection buffers are 1 MiB receive and 512 KiB send
rather than 4 MiB each way, with a ceiling of 512 live sockets, so a guest
opening hundreds of connections costs the host a bounded amount of memory.
The host socket buffers are read back after being set and a socket too
small to carry one frame is an error rather than a silent stall. The Linux
backend receives frames at the new size.

### Crates

- `shuru-proxy` 0.4.0: larger frames and buffers, `DrainStatus` on the
  device, `create_socketpair` fails when the kernel caps the buffers.
- `shuru-darwin` 0.2.0 and `shuru-linux` 0.3.0: breaking.
  `FileHandleNetworkAttachment::new` takes the IP-MTU.
- `shuru-vm` 0.5.0 and `shuru-sdk` 0.6.0: pick up the above.

## 0.8.1

### Linux: reflink clones instead of copies

The Linux backend copied the full disk image for every boot and every
checkpoint, where macOS has always used an APFS clonefile. The clone now
goes through the FICLONE ioctl, so on XFS with `reflink=1` or btrfs a boot
and a checkpoint are copy-on-write and cost no disk. Measured on an
a1.metal host against the same 0.8.0 image: boot 1.3s instead of 9s,
`checkpoint create` 1.3s instead of 51s, `run --from` 1.0s instead of 35s,
and a checkpoint allocates 0 bytes instead of 4 GB. Filesystems without
reflinks still work through the old copy, with a one-time note naming the
directory so the fix is obvious.

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
