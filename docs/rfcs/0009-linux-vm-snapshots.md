# RFC 0009: Linux VM snapshots

- Status: Draft
- Date: 2026-10-27, updated 2026-10-28
- Scope: shuru-linux, shuru-guest, shuru-proto, guest kernel config, `shuru-vmm`, `shuru-node`
- Depends on: RFC 0008 (node runtime)
- Required by: hosted Shuru

## Summary

A running VM on a Linux host can be saved to a snapshot: guest memory,
vCPU and interrupt controller state, device state and the working disk,
captured at one instant. A snapshot restores into a new VM by mapping the
memory file instead of reading it, so restore cost depends on the size of
the device state, not the size of guest memory, and pages load when the
guest touches them. Many sandboxes can restore from one snapshot at once
and share its unchanged pages in the host page cache.

One mechanism serves three uses in hosted Shuru: templates (set a sandbox up
once, start every new one from that point), pause and resume of idle
sandboxes, and forks of a running sandbox.

## Motivation

### The macOS attempt lost to a cold boot, and why

RFC 0001 saved and restored machine state on macOS. A cold boot took 0.44
to 0.49 s; restoring took about 0.63 s. Two things made restore lose:

- Apple restore reads and decrypts the whole state file before the guest
  runs. The cost grows with the state, and it is not under our control.
- The snapshot was taken at the first vsock handshake, the earliest point
  a guest can answer. It held nothing a cold boot does not reach in 0.3 s,
  so there was nothing for restore to skip.

The snapshot paid full price and skipped almost no work.

### What is different on Linux

- **Restore can be lazy.** We own the VMM. Mapping a memory file with
  `MAP_PRIVATE` makes restore a handful of system calls; the kernel pages
  memory in from the page cache on first touch and copies a page only when
  the guest writes it. Firecracker restores this way, and its published
  restore times are in milliseconds.
- **Cold boot is slower here.** On a1.metal, `shuru run -- true` takes
  412 to 431 ms with the 0.11.0 release, down from 1.25 to 1.77 s in the
  0.9.2 cycle, and the guest reaches the command 160 to 170 ms after the
  kernel starts. The same command takes 140 to 160 ms on an M-series Mac.
  There is more to skip than on a Mac, though far less than when this RFC
  was first drafted.
- **The real win is warm state.** A template snapshot is taken after the
  setup a user cares about: packages installed, an interpreter with its
  imports loaded, a dev server listening, a browser started. A cold boot
  cannot skip that work at any speed. This is the case the macOS attempt
  never measured.

So this RFC makes two claims to test, not assume: restoring a bare base
snapshot beats a cold boot on Linux, and restoring a warm template beats
cold boot plus setup by a wide margin.

## Design

### What a snapshot is

A directory on the node data volume (XFS):

```
snap-<id>/
  manifest.json   format version, release and guest protocol version,
                  CPU fingerprint, guest image hashes, memory size,
                  vCPU count, device list, parent snapshot
  state.bin       vCPU, interrupt controller, timer and device state,
                  versioned binary encoding
  memory          guest memory, a sparse file the size of guest RAM
  disk            the working disk, a reflink clone
```

`memory` and `disk` are reflink clones where possible, so a snapshot of a
VM that was itself restored from a snapshot shares every unchanged extent
with its parent on disk.

### Guest memory

Guest memory today is anonymous memory from `vm-memory`. Under this RFC it
comes from one of two places:

- **Cold boot:** a `memfd` of guest RAM size, mapped shared. Saving writes
  only the resident ranges of the memfd to `memory`, using `SEEK_DATA` and
  `SEEK_HOLE` to skip what was never touched. Free page reporting (RFC
  0006) keeps the resident set close to what the guest actually uses, so a
  2 GB guest using 300 MB writes about 300 MB.
- **Restore:** `memory` mapped `MAP_PRIVATE`. Unwritten pages stay in the
  page cache, shared by every VM restored from the same file. A guest
  write gives that VM a private copy of one page.

Saving a VM that was restored from a snapshot writes only what it changed.
Pages the guest wrote are anonymous copies; pages it did not write still
map the parent file. `/proc/self/pagemap` tells the two apart, so the
snapshot is a reflink clone of the parent `memory` with the changed pages
written over it. Pausing a sandbox that has been idle since its last
resume costs almost nothing.

Free page reporting has to follow the backing. On a shared memfd,
`MADV_DONTNEED` only unmaps and the page stays in the file, so the balloon
backend switches to `MADV_REMOVE`, which punches the hole. On a restored
VM, `MADV_DONTNEED` on the private mapping drops the private copy and the
page reverts to the file content, which stays in the page cache shared and
reclaimable. The guest does not rely on the content of a reported page,
because Shuru does not offer page poisoning (RFC 0006).

### vCPU and interrupt state (arm64)

Per vCPU, with the VM paused:

- every register in `KVM_GET_REG_LIST`, read with `KVM_GET_ONE_REG`: core,
  floating point and SIMD, system registers including `MPIDR_EL1`, and
  the timer registers;
- `KVM_GET_MP_STATE`, so vCPUs the guest powered off through PSCI stay off;
- `KVM_GET_VCPU_EVENTS`, for a pending SError.

The GICv3 through its device attributes: distributor registers,
redistributor registers per vCPU, CPU interface system registers and line
level information. Shuru does not create an ITS, so there are no pending
tables in guest memory to flush.

Restore order follows the kernel requirements: create vCPUs, initialise
them, restore `MPIDR_EL1`, create and initialise the GIC, restore its
state, then restore the rest of each vCPU.

### Time

The guest virtual counter must not jump forward by the time a snapshot sat
on disk, or every timer fires at once. On restore the VMM sets the counter
offset (`KVM_ARM_SET_COUNTER_OFFSET`, host kernel 6.4 and later) so the
guest counter continues from where it stopped. The guest then learns the
real time from the host over the first connection after restore and steps
`CLOCK_REALTIME`, which also fixes the stale clock RFC 0001 corrected with
an exec of `date`.

### Clones must not share randomness

Every VM restored from one snapshot starts with identical kernel random
state. Two sandboxes from one template would generate the same keys.

The guest kernel gets a VM generation ID device (`CONFIG_VMGENID`, device
tree compatible `microsoft,vmgenid`, supported by Linux since 6.10). The
VMM writes a new random generation ID into the device on every restore and
raises its interrupt. The kernel reseeds its random number generator
before anything else in the guest runs again, and announces the change to
userspace with a uevent.

Process state is a different matter. A program that seeded its own
generator before the snapshot keeps that seed in every clone. Shuru cannot
fix that from outside. The documentation will say so plainly: take
templates before starting processes that hold keys, or restart them after
restore. The guest agent also rotates the per-boot token and instance ID
from RFC 0008 on the first connection after restore.

### Devices

The virtio-mmio transport saves features, status, interrupt status and
each queue (size, ready, ring addresses, last available index). Backends
add their own state:

| device | saved | on restore |
|---|---|---|
| block | nothing beyond the queues | attaches the snapshot `disk` clone |
| net | nothing | a new proxy (RFC 0008); open guest TCP connections to the outside are reset |
| balloon | target and actual pages | keeps the target; reporting continues |
| vsock | nothing | sends a transport reset event, the guest drops connections, the host reconnects |
| console, rng | nothing | resumes |
| virtio-fs | refuses to snapshot | hosted nodes do not attach it; a local VM with mounts cannot be snapshotted |

### Taking a snapshot

1. Stop the vCPUs: signal every vCPU thread out of `KVM_RUN` and wait.
2. Stop device threads from pulling new work (net rx, vsock).
3. Save vCPU, GIC and device state.
4. Write memory: the resident ranges for a cold-booted VM, the changed
   pages over a reflink of the parent for a restored one.
5. Reflink the working disk descriptor into `disk`.
6. `fsync` the files, write `manifest.json` last, rename the staging
   directory into place.
7. Resume the vCPUs, or stop the VM when the snapshot is for a pause.

Memory and disk come from the same paused instant, and dirty guest page
cache is part of memory, so the guest does not need to `sync` first. This
is why a snapshot skips the freeze that disk-only checkpoints take (RFC
0010): a checkpoint keeps the disk and drops memory, so the filesystem
must be consistent on its own; a snapshot keeps both.

### Restoring

1. Check the manifest: format version, release, and the CPU fingerprint
   (`MIDR_EL1` and the ID registers KVM exposes). Nodes in one pool run
   identical hardware. A snapshot this host cannot resume is not
   attempted; it falls back as described under Snapshots across releases.
2. Map `memory` private, reflink `disk` into a new working disk.
3. Create the VM and vCPUs, restore GIC and vCPU state, set the counter
   offset, restore devices, write a new VM generation ID.
4. Start the vCPUs. Connect over vsock, rotate the token, send the time.

### Snapshots across releases

A snapshot holds the guest kernel and agent of the release that took it,
running, in `memory`, and its device state in the encoding of that VMM.
A newer host restoring it does not boot the new kernel; it resumes the old
one, so it has to read the old device state and talk to the old agent.
Refusing every snapshot from an earlier release would make each upgrade
break every template and every paused sandbox. Instead:

- The manifest records the release and the guest protocol version.
  `state.bin` is versioned per section, and the VMM reads the encodings of
  the last few releases (four to start) and refuses older ones.
- The host keeps speaking the guest protocol versions of those releases.
  The hello reply gains the guest protocol version, and the host uses the
  matching framing on that connection. Until now a protocol change could
  drop older agents, as protocol 3 did in 0.11.0. From the first release
  with snapshots, a change keeps the older versions in the window
  answerable.
- A device added in a later release is absent from an older snapshot;
  the restored VM runs without it until its next cold boot. A device whose
  state encoding changed reads the old encoding.
- Past the window, or on a CPU fingerprint mismatch, a resume becomes a
  cold boot of the snapshot `disk` with the current kernel and image.
  Files survive; processes and memory do not. The host reports it as a
  reboot with the reason, never as a successful restore.

Disks need none of this. A working disk is a block level clone, so
replacing the base image never changes a disk cloned from it, and an old
disk boots with the current kernel and agent because the agent lives in
the initramfs (RFC 0010).

### Snapshots that leave the node

A snapshot is a self-contained directory: the manifest names everything
restore checks, and `memory` and `disk` are plain files. Restore needs
them on local XFS. Where a snapshot is kept when it is not on the host,
and how it gets back, is up to whoever runs the host and is outside this
RFC.

Fetching pages on demand through `userfaultfd` would let a restore start
before the whole `memory` file has arrived; it is left for later because
the local version is the one to measure first.

### Prefetching the working set

Pages loaded on first touch cost a page fault each, and E2B documents that
templates restore faster than live snapshots because they prefetch. For
templates, the agent records which pages a restored VM touches in its
first second (`mincore` on the mapping), stores that list beside the
snapshot, and asks the kernel to read those ranges ahead
(`MADV_WILLNEED`) at the next restore. It is an optimisation and lands
only if the measurements show page faults matter.

## What will be measured

On a1.metal, 2 vCPU and 2 GB guests, against the 412 to 431 ms cold boot
of 0.11.0:

| case | measured as | target |
|---|---|---|
| restore of a base snapshot taken at first handshake | restore call to first exec reply | under 100 ms |
| restore of a warm template (Python with numpy imported, a Node dev server listening, a headless browser started) | restore call to the warm work answering | under 150 ms, against cold boot plus setup |
| 50 concurrent restores from one snapshot | wall time, host memory used | shared pages counted once |
| pause of a sandbox restored from a template and idle since | pause call to VM gone, bytes written | under 100 ms, changed pages only |
| pause of a cold-booted 2 GB guest using 300 MB | bytes written, time | about 300 MB |
| restore of a snapshot taken by the previous release | restore call to first exec reply | same as a same-release restore |
| resume of a snapshot past the window | what the caller sees | a reported reboot from its disk, files intact |

Every number goes in the evidence log next to the cold boot figures from
the same host and build. If restore of the base snapshot does not beat a
cold boot, the RFC says so and templates carry the feature alone.

## Not in this RFC

- macOS. The framework restore stays what it is; local runs keep cold
  boots. Revisit only if Apple changes how restore loads memory.
- Snapshots of a VM with virtio-fs mounts.
- Live migration between nodes.
- Storing snapshots off the host and moving them between hosts.
- On-demand page fetching (`userfaultfd`).
- Snapshot compression and encryption at rest.
