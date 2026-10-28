# RFC 0009: Linux VM snapshots

- Status: Draft, first slice built
- Date: 2026-10-27, updated 2026-10-28
- Scope: shuru-linux, shuru-guest, shuru-proto, shuru-proxy, guest kernel config, `shuru-vmm`, `shuru-node`
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
cold boot plus setup by a wide margin. The first is now measured and holds;
see Measured so far.

## Design

### What a snapshot is

A directory on the node data volume (XFS), mode 0700, every file 0600:

```
snap-<id>/
  memory      guest memory, a file the size of guest RAM
  state       vCPU, interrupt controller and device state, JSON with a
              format version
  disk        the working disk, a reflink clone
  node.json   what the node needs to restore: the release and guest
              protocol that took it, vCPU count, memory size, the guest
              identity, and for a networked VM the egress policy, the
              guest MAC and the proxy session
```

`memory` and `disk` are reflink clones where possible, so a snapshot of a
VM that was itself restored from a snapshot shares every unchanged extent
with its parent on disk.

`state` is JSON. It is a few tens of kilobytes (19 KB for 2 vCPUs and five
devices), so encoding size and speed do not matter, and a readable format
makes a mismatch after an upgrade easy to see. Besides the registers a
restore writes back, it holds the CPU ID registers each vCPU saw:
`MIDR_EL1`, `REVIDR_EL1` and the `ID_AA64` processor, debug, instruction
set and memory model feature registers. The guest picked code paths from
those at boot. A restore refuses a snapshot whose state format, guest
protocol or CPU ID registers differ from what this node offers, and says
which. Image hashes are not recorded.

A snapshot holds everything the guest held: its memory, its identity and,
for a networked VM, the proxy CA key. Anyone who can read it can resume the
guest or intercept the guest TLS it relays, so it gets the same protection
as the guest itself. Secret values are not in it; they come from the
environment of the node at each restore.

### Guest memory

Guest memory stays anonymous memory from `vm-memory` for a cold boot, as
before this RFC. An earlier draft used a `memfd` mapped shared, so saving
could find the touched pages with `SEEK_DATA`. On a1.metal that made cold
boot about 60 ms slower (385 to 405 ms against 327 to 334 ms on the same
instance), because every first touch of guest memory went through shmem.
It is gone.

A restored VM maps `memory` with `MAP_PRIVATE`. Unwritten pages stay in the
page cache, shared by every VM restored from the same file. A guest write
gives that VM a private copy of one page.

Both mappings are private, so one rule finds what to save: the pages the
VM wrote, by the guest or by a device, are anonymous, and
`/proc/PID/pagemap` marks them present and not file backed, or swapped
out. The VMM reads its pagemap through a descriptor the node opens (the
jail has no `/proc`) and writes those pages:

- for a cold-booted VM, into an empty sparse file, so pages never touched
  stay holes. Free page reporting (RFC 0006) keeps the resident set close
  to what the guest uses; a 2 GB guest that ran one command saves 67 MB;
- for a restored VM, over a reflink clone of the `memory` it was restored
  from, so a sandbox idle since its last resume costs almost nothing to
  pause.

Free page reporting keeps `MADV_DONTNEED` for both. On a cold-booted VM a
reported page reads as zero when touched again; on a restored VM it reverts
to the file content, which stays in the page cache, shared and reclaimable.
The guest does not rely on the content of a reported page, because Shuru
does not offer page poisoning (RFC 0006).

### Who may share a snapshot

Restores of one snapshot share its unchanged pages in the host page cache.
Shared physical pages are what cross-VM cache attacks need: Flush+Reload
recovered AES keys across VMs that shared pages through deduplication
(Irazoqui and others, "Wait a minute! A fast, Cross-VM attack on AES",
RAID 2014), and page cache sharing alone leaks access patterns between
processes (Gruss and others, "Page Cache Attacks", CCS 2019). Two rules
follow:

- **Snapshot memory is shared only within one tenant.** Every VM restored
  from one `memory` file belongs to the same owner. A template offered to
  many tenants is reflinked into a separate `memory` file per tenant. The
  clones share blocks on disk, but each file has its own page cache, so no
  physical page is shared across tenants.
- **A template offered to many tenants is built clean.** It is made from
  public inputs by a repeatable process, with no secrets bound and no
  network session, so its memory holds nothing about any tenant and no
  proxy CA its restores would all trust. Tenant secrets are bound after
  restore.

The page cache is charged to the cgroup that first reads a page, so a
restored VM shows a small peak (5 to 9 MiB here) while the shared pages
count elsewhere. Host memory accounting has to count snapshot page cache
per tenant for the same reason.

### vCPU and interrupt state (arm64)

Per vCPU, with the VM paused:

- every register in `KVM_GET_REG_LIST`, read with `KVM_GET_ONE_REG`: core,
  floating point and SIMD, system registers including `MPIDR_EL1`, and
  the timer registers;
- `KVM_GET_MP_STATE`, so vCPUs the guest powered off through PSCI stay off;
- `KVM_GET_VCPU_EVENTS`, for a pending SError.

The GICv3 through its device attributes: distributor registers,
redistributor registers per vCPU and CPU interface system registers.
Shuru does not create an ITS, so there are no pending tables in guest
memory to flush.

Restore order follows the kernel requirements: create the vCPUs, create
and initialise the GIC, initialise each vCPU and restore its registers
with `MPIDR_EL1` first, then restore the GIC (CPU interface, then
redistributors, then the distributor with its control register last), and
the devices last.

### Time

The guest virtual counter must not jump forward by the time a snapshot sat
on disk, or every timer fires at once and the watchdogs report stalls.
Restoring the saved virtual counter register makes KVM set the offset, so
the guest counter continues from where it stopped and the guest monotonic
clock does not count the gap. QEMU does the same by default.

Wall time comes from the host. On the first connection after a restore the
node sends a restore frame (`RESTORE_REQ`) carrying the host
`CLOCK_REALTIME`, and the agent steps the guest clock before answering.
A restored guest read the same second as the host in every run.

### Clones must not share randomness

Every VM restored from one snapshot starts with identical kernel random
state. Two sandboxes from one template would generate the same keys.

- The guest kernel has a VM generation ID device (`CONFIG_VMGENID`, device
  tree compatible `microsoft,vmgenid`, in Linux since 6.10). Its 16 bytes
  live in a read-only page of their own outside guest RAM, which the VMM
  fills with fresh random bytes for every VM and never saves. A restore
  raises its edge interrupt before the vCPUs start, and the driver sees a
  new ID and reseeds the kernel random number generator in the interrupt
  handler as soon as they run.
- The restore frame also carries 32 bytes from the host random number
  generator. The agent writes them to `/dev/urandom` and forces a reseed
  (`RNDRESEEDCRNG`), so the reseed does not rest on one mechanism.

Three restores of one snapshot read different bytes from `/dev/urandom`
and counted one vmgenid interrupt each.

Process state is a different matter. A program that seeded its own
generator before the snapshot keeps that seed in every clone. Shuru cannot
fix that from outside. The documentation will say so plainly: take
templates before starting processes that hold keys, or restart them after
restore.

### Identity

Every restore of one snapshot starts with the token and instance ID of RFC
0008 that the snapshot was taken with, so anyone holding the snapshot could
reach every restore of it. The restore frame carries a new token and
instance ID. The node connects with the saved identity, sends the frame on
that connection, and from the answer on the guest accepts only the new
one. A snapshot of a restored VM records the rotated identity; a third
generation restore that connects with it shows the guest switched.

An agent from before this RFC skips the unknown frame and never answers.
The node waits two seconds and stops the restore, rather than run a guest
whose identity, clock and randomness it could not reset.

### Devices

The virtio-mmio transport saves features, status, interrupt status and
each queue (size, ready, ring addresses, last available index). Backends
add their own state:

| device | saved | on restore |
|---|---|---|
| block | nothing beyond the queues | attaches the snapshot `disk` clone |
| net | the receive ring index | a new proxy with the saved session (below) and the saved MAC; open guest TCP connections to the outside are lost |
| balloon | target and actual pages | keeps the target; reporting continues |
| vsock | the index of each queue | sends a transport reset event; the host connects only after the guest has posted its event buffer again |
| console, rtc | their registers | resumes |
| vmgenid | nothing | a new ID and an interrupt |
| virtio-fs | refuses to snapshot | hosted nodes do not attach it; a local VM with mounts cannot be snapshotted |

The guest drops its vsock connections on the reset event in a work queue.
A host connection accepted before that runs would be dropped with them, so
the device holds new host connections until the guest posts its event
buffer again, which it does after the reset.

A networked guest keeps two things from its proxy: the proxy CA in its
trust store and the secret placeholders in its environment. The proxy
session (the CA certificate, its key and the placeholders) is saved in
`node.json`, and the restore starts the network process with it. The
proxy signs new leaf certificates with the saved key under the same name,
and hands out the saved CA certificate, so the guest trust store still
holds. The egress policy and the secret names and hosts are saved too;
the secret values are read from the environment at restore, so a secret
can change between restores. The gateway MAC is fixed, so the guest
neighbour table stays right.

### Taking a snapshot

1. Stop the vCPUs: signal every vCPU thread out of `KVM_RUN` until it
   returns.
2. Save the devices. Transport devices finish the requests the guest made
   before the pause; net stops its receive thread; vsock stops serving.
3. Save vCPU and GIC state.
4. Write memory through the pagemap, as above.
5. Reflink the working disk into `disk`, write `state` and `node.json`.
6. Resume the vCPUs, or stop the VM when the snapshot is for a pause.

If saving fails the VM resumes. Memory and disk come from the same paused
instant, and dirty guest page cache is part of memory, so the guest does
not need to `sync` first. This is why a snapshot skips the freeze that
disk-only checkpoints take (RFC 0010): a checkpoint keeps the disk and
drops memory, so the filesystem must be consistent on its own; a snapshot
keeps both.

The files go into a staging directory beside the target, `.NAME.partial`.
Each file and the directory are synced, the directory is renamed into
place with `RENAME_NOREPLACE`, and the parent is synced. A crash
mid-snapshot leaves only the staging directory, which the next snapshot to
that name removes, so a directory under the final name is always complete.
A target that already exists is refused before the VM starts, not after
the command ran. The sync is most of the time a snapshot takes on EBS (see
below); the VM is already stopped or resumed by then, so it delays only
the reply.

### Restoring

1. Read `node.json` and `state`; check the guest protocol, the format
   version and that the shape matches. Start reading `memory` ahead.
2. Map `memory` private, reflink `disk` into a new working disk.
3. For a networked VM, start the network process with the saved session.
4. Create the VM and vCPUs, check the CPU ID registers against the saved
   ones, restore GIC and vCPU state, restore devices, raise the vmgenid
   interrupt.
5. Start the vCPUs. Connect over vsock with the saved identity and send
   the restore frame: new identity, time, seed.

### Snapshots across releases

A snapshot holds the guest kernel and agent of the release that took it,
running, in `memory`, and its device state in the encoding of that VMM.
A newer host restoring it does not boot the new kernel; it resumes the old
one, so it has to read the old device state and talk to the old agent.
Refusing every snapshot from an earlier release would make each upgrade
break every template and every paused sandbox. Instead:

- `node.json` records the release and the guest protocol version, and
  `state` is versioned. Today a restore refuses any other guest protocol
  or state format; the window here is what replaces that refusal. The VMM
  reads the encodings of the last few releases (four to start) and
  refuses older ones.
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
  reboot with the reason, never as a successful restore. The node itself
  only refuses, with the reason; the cold boot is up to its caller.

Disks need none of this. A working disk is a block level clone, so
replacing the base image never changes a disk cloned from it, and an old
disk boots with the current kernel and agent because the agent lives in
the initramfs (RFC 0010).

### Snapshots that leave the node

A snapshot is a self-contained directory, and `memory` and `disk` are
plain files. Restore needs them on local XFS. Where a snapshot is kept when
it is not on the host, and how it gets back, is up to whoever runs the host
and is outside this RFC.

Fetching pages on demand through `userfaultfd` would let a restore start
before the whole `memory` file has arrived; it is left for later because
the local version is the one to measure first.

### Prefetching the working set

Pages loaded on first touch cost a page fault each, and when the snapshot
is not in the page cache each fault waits on a read from the volume. E2B
documents that templates restore faster than live snapshots because they
prefetch.

A restore reads the saved memory ahead. A thread in the node walks the
data ranges of `memory` (`SEEK_DATA` and `SEEK_HOLE`, so pages the guest
never touched are skipped) and calls `posix_fadvise(POSIX_FADV_WILLNEED)`
on each, so the guest finds its pages in the page cache instead of faulting
each one in from the volume. `--no-prefetch` turns it off. With the page
cache dropped it brings the first answer from 171 to 197 ms down to 142 to
149 ms; with the snapshot cached it changes nothing. It also delays the
restore of VM state by about 30 ms, because the reads compete with reading
`state` and the VMM binary. Starting it once the VMM has read `state` may
win that back; it is not measured yet.

Reading every saved page is right while the saved memory is about what a
restore touches. For a template whose saved memory is much larger than its
working set, the node would record which pages a restored VM touches in
its first second (`mincore` on the mapping), store that list beside the
snapshot, and read those ranges first.

## Measured so far

a1.metal, 2 vCPU and 2 GB guests, jailed VMM under kill-mode seccomp,
memory and disks on XFS over EBS gp3, 2026-10-28. Times are from the start
of `shuru-node` to the guest answering its first connection, unless noted.

| case | result |
|---|---|
| cold boot, main and this branch, same instance, same kernel and agent | 328 to 338 ms and 330 to 341 ms |
| cold boot with the vmgenid kernel | 326 to 338 ms |
| snapshot of a cold-booted guest after one command | 35 ms, 67 MB written; 176 to 186 ms (one 418 ms) once the files are synced and renamed into place |
| snapshot into a directory that exists | refused in 17 ms, before the VM starts |
| restore of a snapshot with another guest protocol, or with `MIDR_EL1` edited in `state` | refused with the reason; the untouched snapshot still restores |
| restore, snapshot in the page cache | VM restored 13 to 19 ms, guest answered 26 to 30 ms, identity rotated 1 ms later, `true` done 55 to 58 ms |
| restore, page cache dropped | 210 to 300 ms; 40 ms with only `memory` cached; the first restore after the instance came up took 1.85 s |
| snapshot of a restored guest | 15 to 64 ms, changed pages only |
| restore from the third generation | memory-only state (a file in `/dev/shm`) intact through each generation |
| restore of a networked guest | network process with the saved session 45 to 53 ms, guest answered 65 to 72 ms; TLS through the reloaded CA and secret substitution work, and a secret changed between restores reaches the guest |
| restore peak cgroup memory | 5 to 9 MiB warm, 20 to 32 MiB cold |
| restore, page cache dropped, prefetch against `--no-prefetch`, alternating, four each | guest answered 142 to 149 ms against 171 to 197 ms; `true` done 203 to 205 ms (one 337 ms) against 240 to 265 ms; no difference when cached |
| warm template: Python with numpy 2.2.4 imported, waiting on a FIFO, 104 MB of saved memory | restore to the computed answer 93 to 97 ms, 122 to 133 ms wall; cold boot plus the same import and sum 1.12 s (2.04 s uncached) |
| 50 concurrent restores from one snapshot, each running `sleep 3`, two runs | all 50 answered, median 122 and 143 ms, slowest 199 and 225 ms; wall 3.3 s; host available memory down 64 and 217 MiB |
| 50 concurrent cold boots, same command | all 50 answered, median 1.56 s, slowest 1.86 s; wall 5.0 s; host available memory down 3.3 GiB |

A bare base snapshot restores more than ten times faster than a cold boot
when it is in the page cache, and a template with its work loaded restores
in under a tenth of what it takes to boot and load it. Fifty restores share
one copy of the saved pages in the page cache, so together they cost the
host about a fifteenth of the memory of fifty cold boots. Uncached, the
reads from the volume decide the time, which prefetching shortens.

In the template restore, 39 ms of the 93 is the reflink of a disk that apt
wrote to, which has many more extents than the base image.

## What will be measured

| case | measured as | target |
|---|---|---|
| restore of a warm template with a Node dev server listening, or a headless browser started | restore call to the warm work answering | under 150 ms, against cold boot plus setup |
| pause of a cold-booted 2 GB guest using 300 MB | bytes written, time | about 300 MB |
| restore of a snapshot taken by the previous release | restore call to first exec reply | same as a same-release restore |
| resume of a snapshot past the window | what the caller sees | a reported reboot from its disk, files intact |

Every number goes in the evidence log next to the cold boot figures from
the same host and build.

## Not in this RFC

- macOS. The framework restore stays what it is; local runs keep cold
  boots. Revisit only if Apple changes how restore loads memory.
- Snapshots of a VM with virtio-fs mounts.
- Live migration between nodes.
- Storing snapshots off the host and moving them between hosts.
- On-demand page fetching (`userfaultfd`).
- Snapshot compression and encryption at rest.
