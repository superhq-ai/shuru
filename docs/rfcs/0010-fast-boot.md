# RFC 0010: Fast boot and sealed disks

- Status: Draft
- Date: 2026-10-27
- Scope: shuru-guest, shuru-proto, shuru-linux, shuru-vm, shuru-sdk, shuru-cli, `scripts/prepare-rootfs.sh`, `kernel/shuru_defconfig`, OS image
- Depends on: nothing
- Bears on: RFC 0007 (hosted Shuru), RFC 0009 (Linux VM snapshots)

## Summary

Two changes that need each other.

**Sealed disks.** Every saved disk (checkpoint, recipe build, fork of a
running sandbox) is taken from a guest that froze its root filesystem
first. One guest call, one host function, used by every path that saves a
disk. A saved disk has its ext4 journal flushed and `needs_recovery` cleared.
Orphan cleanup can still be needed for open, unlinked files. It contains
completed filesystem writes, not application buffers or process state.
A guest that cannot seal leaves no checkpoint.

**Fast boot.** The kernel handles journal replay; ordinary boots need no
userspace fsck or offline resize. A small error gate refuses known damage,
and the kernel still validates the filesystem as it mounts. The initramfs
shrinks to one uncompressed executable, shuru-guest installed as `/init`,
which mounts the disk, grows the filesystem when the disk is bigger,
sets up the network and becomes the agent. Busybox, e2fsck, resize2fs and
the shell script go.

The earlier measurements suggest about 350 ms of removable work from the
0.65 s Linux boot. That is a target, not a measured result of this change.
The new mount, error checks and resize costs must be measured on both hosts.

## Motivation

### Where a boot goes today

a1.metal, 2 vCPU, 2 GB guest, 0.10.0 OS image, host page cache warm,
seven runs:

| stage | time |
|---|---|
| VM created to guest kernel start, plus connect and hello | about 210 ms, not yet broken down |
| kernel start to `Run /init` | 142 to 146 ms, of which about 105 ms unpacks the 3.8 MB gzip initramfs |
| `/init` to `EXT4-fs (vda): mounted` | 247 to 261 ms: `e2fsck -p` and `resize2fs` on `/dev/vda` |
| root mounted to the agent answering | 35 to 45 ms |
| VM created to first exec answered | 644 to 664 ms |

A cold host cache adds about 100 ms. `shuru run -- true` takes 0.70 to
0.75 s end to end, against 0.44 to 0.49 s on a Mac.

### Why fsck and resize run at all

The image is a 1 GB ext4 made by a default `mkfs.ext4`: it has a journal,
`metadata_csum`, `resize_inode` and `64bit`.

- **resize2fs** came with configurable disk sizes (8503e0e). The default
  disk is 4096 MB and `WorkDisk::clone_from` extends the cloned file
  without touching the filesystem in it, so every run from the base grows
  the same 1 GB to 4 GB again.
- **e2fsck** came with checkpoint resizing (8f9db5d). A checkpoint is
  saved from a guest that was powered off without unmounting its root, so
  its journal still holds work (`needs_recovery` is set; the `docker`
  checkpoint on this machine has it). Offline resize2fs will not touch a
  filesystem in that state, or one not checked since it was last mounted,
  so e2fsck ran first to replay the journal and stamp the check.

Neither does anything for the kernel, which replays a journal on its own
when it mounts. They exist only because resizing was done offline.

### Saved disks are not taken the same way twice

There are three ways to save a disk today and each has different
guarantees:

| path | what happens before the clone | result |
|---|---|---|
| `shuru checkpoint create` and `update` | services stopped, `sync`, power off. If the guest does not answer in 90 s it is powered off anyway and the clone still happens | journal not flushed; after a timeout, writes still in the guest page cache are missing |
| `run --stdio` checkpoint (TypeScript SDK) | `sync` run as an exec while everything keeps running, then the clone, then stop | writes between the `sync` and the clone race the clone |
| `AsyncSandbox::checkpoint` (Rust SDK) | nothing; the VM keeps running | whatever had reached the disk; the existing checkpoint is deleted before the new one is written |

A reflink can capture a block-level crash state. A streaming copy of a
changing disk can mix states from different times; journaling does not
make that safe. Even a crash-consistent clone can omit writes still in
the guest cache.

### What the initramfs still does that matters

Every boot it copies its own shuru-init over `/usr/bin/shuru-init` on the
disk. That is how a checkpoint built by an older image gets the agent that
matches the host, which the connection hello depends on. Booting the disk
directly would run the agent the checkpoint was saved with. So the
initramfs stays as the way the agent reaches the guest, and everything
else leaves it.

## What is supported

**Supported**

- **Ephemeral runs.** The disk is discarded at exit. Nothing is sealed.
- **Checkpoint at stop.** `shuru checkpoint create` and `update`, recipe
  builds, the TypeScript SDK. The guest services of RFC 0004 (daemons an
  image declares, such as dockerd, supervised by the agent inside the
  guest) get their SIGTERM and stop timeout, then the seal, the clone and
  the power off. Other processes are not signalled; the seal captures
  what they had written.
- **Checkpoint of a running sandbox.** The Rust SDK today, and forks of a
  running sandbox in RFC 0007. Seal, clone, thaw; the sandbox carries on.
  Services are not stopped. What they had written is on the disk and what
  they held in memory is not, the same contract as a cloud disk snapshot:
  a database recovers as it would after a power cut.
- **Forks at the same size or larger.** The guest grows the filesystem
  once, at boot, and every disk saved afterwards keeps the new size.
- **Large free extents offered for discard,** while a sandbox runs and
  before every save. Reclamation depends on the backing storage.
- **Checkpoints from before this change.** The kernel replays their
  journal when it mounts them. No code for it; the first seal after that
  makes them clean.

**Not supported**

- **Saving from a guest that did not seal.** Hung, panicked, killed,
  powered itself off, timed out: no checkpoint, an error saying which, and
  on `update` the old checkpoint untouched. A disk of unknown state is not
  saved under a name.
- **Shrinking a disk, or a disk smaller than 4096 MB.** See Sizes below.
- **Memory or process state in a checkpoint.** RFC 0009.
- **Repairing a damaged filesystem at boot.** The filesystem is set to
  `errors=remount-ro`, so damage the kernel finds stops writes and the run
  fails loudly instead of spreading it. A model where damage is normal is
  the wrong model. On 6.12 the option shuts the filesystem down: reads
  fail as well as writes, so no program on the root runs again. The agent
  lives in memory and still answers, refusing a seal with the error. Read
  access after an error arrives with `EXT4_FLAGS_EMERGENCY_RO` in 6.16.
- **A running disk that survives a host crash.** The durable state of a
  sandbox is its last saved disk. The journal would make a crashed disk
  consistent, so this can be offered later without a new design; it is
  not promised now.

## Design

### Seal and thaw

`shuru-proto` gains `Seal` and `Thaw` here, and `Trim` (below).

- `Seal { id: u64, stop_services: bool, lease_ms: u64 }`. The lease is
  between 1000 and 300000 ms. A second active seal is refused. Stop the
  declared services if requested, check root health, call `syncfs` on the
  root descriptor, trim, then `FIFREEZE`. Check health again after freezing
  and require `needs_recovery` to be clear before answering `Sealed`.
- `Thaw { id: u64 }`. Release only that seal with `FITHAW`. Answer `Thawed`
  if the copy finished within its lease, otherwise `Expired`.

The guest starts the monotonic lease after freeze returns. An independent
thread thaws at the deadline and retains the expired state for the next
matching Thaw. The host must receive `Thawed` before publication. A lost
reply, wrong id, timeout or `Expired` discards the private copy. A lease
cannot cancel a blocked freeze ioctl: the host times out, saves nothing,
and a freeze that eventually completes still gets its lease watchdog.

Freeze waits for ordinary writers, shared writable page faults and
internal filesystem writers, then syncs and flushes the journal. Reads
that need atime updates can also wait. The control and watchdog paths must
not write the root, touch writable root mappings, or wait for locks held
by blocked writers. Runtime state and service logs live on `/run` tmpfs;
the executable remains on initramfs. Existing request threads and guest
processes can block in the kernel until thaw without blocking control.

This freezes the ext4 superblock, including overlay upper files on it.
It does not freeze an independent filesystem on a loop-backed file, and
raw block writes bypass the filesystem. Freezing an outer filesystem before
syncing an inner loop filesystem can deadlock. The shipped kernel disables
loop devices and swap. Custom kernels with these dependencies need their
own quiescing protocol; they are not covered by this seal contract.
Dockerd overlay2 data is covered, but daemon memory and writes to other
filesystems are not.

Before mount the guest reads only the fixed ext4 superblock on `/dev/vda`
and rejects `s_state & EXT4_ERROR_FS`. Mount read-only with journal replay
enabled, check again, then remount read-write with `errors=remount-ro`.
This is an error gate, not an integrity scan. At seal, reject a read-only
root, the persisted error flag, an increase from the boot value of
`/sys/fs/ext4/vda/errors_count`, or a failed sync/freeze. A historical error
count alone is not an error flag. The kernel still validates checksums and
mount features. No host code reads ext4 metadata.

While sealed the agent refuses requests that would write to the root, and
it writes nothing there itself (its state is on the `/run` tmpfs).
Writes to other mounts (tmpfs, virtiofs) are unaffected and are not part
of the disk anyway.

Freezing, not a read-only remount, because a remount fails with `EBUSY`
while any process holds a file open for writing. That would make
checkpoints of a running sandbox impossible and checkpoints at stop
depend on killing every stray process first. Freeze can still block on
I/O or filesystem dependencies. A host timeout
is a failed save, never permission to copy an unsealed disk.

### One host function

`shuru-vm` gets `Sandbox::save_disk(dest, after, copy)` with
`AfterSave::Thaw` or `AfterSave::PowerOff`. The copy callback writes either
the work disk clone or the CAS index:

1. Serialize saves for this sandbox and seal with a fresh id. A stop save
   asks the guest to stop its declared services.
2. Reserve `dest.partial` as the staging directory and hold an exclusive
   `flock` on it. A held lock means another save of `dest` is running, and
   this one fails. A directory with no lock holder was left by a save that
   died, and is reused. Clone into its new `disk` file, by reflink or sparse
   copy, or write the CAS index. Sync the staged file.
3. Thaw and require the matching `Thawed` answer. A failure removes only
   the staging directory owned by this operation and leaves `dest` intact.
4. For PowerOff, stop the VM. Then atomically rename the staged disk onto
   `dest` and sync its parent directory. Remove the empty staging directory.

Both paths validate by thawing before publication. On the stop path this
briefly allows writes to the original disk before power off; the completed
clone is independent and stays sealed. Keeping the guest frozen through
power off would require another acknowledgement protocol to prove that its
lease had not already expired. Renaming before validation is incorrect.
A directory sync failure after rename reports uncertain durability; it
cannot restore the old name. Interrupted staging directories are left for
explicit operator cleanup, never silently reused.

The CLI checkpoint commands, recipes, stdio, Rust SDK and CAS all use this
function. No running disk can be saved by a caller skipping the seal. CAS
indexes merge inherited hashes and ingest remaining flat-base chunks when
saving, so neither a mutable parent name nor a replaced base image changes
a saved child. The first save can therefore read the remaining base image.
CAS chunks are published atomically before the index. Unsupported NBD trim
remains disabled. NBD attachment is currently available only on macOS;
the existing Linux backend rejects it. A name cannot switch between `.idx`
and `.ext4` in place; use a
new name, so an older alternate file cannot silently shadow the new disk.

The agent comes from the image initramfs. HELLO_OK now carries the 16-byte
instance id followed by protocol version 3 as a little-endian u32. The host
rejects the old 16-byte reply or a different version before sending work.
An explicitly supplied old initramfs cannot silently bypass this contract.

### The initramfs is the agent

The image ships `initramfs.cpio`: an uncompressed newc archive holding
`/init`, the static musl shuru-guest binary (2.3 MB), and the directories
it mounts on. Unpacking an uncompressed archive is a copy.

shuru-guest starts as `/init` and runs a first stage when it finds itself
on the initramfs:

1. Require PID 1 on ramfs or tmpfs. Mount devtmpfs on `/dev`, perform
   the error gate above and mount `/dev/vda` on `/newroot`.
2. Grow the filesystem only if the device is larger (below).
3. Unmount the initramfs `/dev` and delete `/init` and the initial `/dev`
   contents while the old root is still reachable. Never traverse the
   `/newroot` mount during cleanup.
4. `chdir("/newroot")`, move `.` onto `/` with `MS_MOVE`, `chroot(".")`,
   `chdir("/")`. No exec is needed: musl is static and PID 1 continues in
   the same process. Close old-root directory and device descriptors.
5. Mount proc, sysfs, devtmpfs and runtime tmpfs, then carry on as the agent.

Unlinking does not free the executable while mappings and the executable
reference hold its inode. Account for the binary backing pages for the
life of the VM; the old 2.3 MB size is only a measurement of the old build.
Deleting after chroot would instead address the new disk.

There is no exec and no copy onto the disk. The agent never lives on the
guest writable disk, and the image stops installing `/usr/bin/shuru-init`.
Copies inside old checkpoints stay as dead files.

`CONFIG_BLK_DEV_INITRD` and `CONFIG_DEVTMPFS` are already set, and an
initramfs is always run as `/init`, so `init=/usr/bin/shuru-init` is
dropped from the command line.

### Kernel tick

The guest kernel moves from `CONFIG_HZ_100` to `CONFIG_HZ_1000`. A virtio
PCI reset writes 0 to `device_status` and must wait until it reads back 0
(virtio 1.2, PCI transport); `vp_reset` in
`drivers/virtio/virtio_pci_modern.c` polls with `msleep(1)`. Apple devices
finish a reset asynchronously, so each device sleeps once, and at 100 Hz
`msleep(1)` is two ticks, 10 to 20 ms. On a Mac the four devices took
about 100 ms of every boot this way.

Measured on an M-series Mac, the 0.8.0 CLI and image, 6.12.114 built from
this defconfig with only `CONFIG_HZ` changed, median of eight warm boots:

| | 100 Hz | 1000 Hz |
|---|---|---|
| `shuru run -- true` wall | 300 ms | 214 ms |
| guest uptime at the command | 150 ms | 70 ms |
| `hvc0` enabled (end of device probe) | 113 ms | 25 ms |
| `Run /init` | 118 ms | 41 ms |

The guest is tickless when idle (`CONFIG_NO_HZ_IDLE`), so an idle vCPU
takes no ticks either way; a busy one takes 1000 timer interrupts a second
instead of 100. Linux hosts use virtio-mmio, whose reset path differs, so
the effect there is measured, not assumed.

### Growing the filesystem

Online resize is one ioctl, and the kernel does the work on the mounted
filesystem:

1. `BLKGETSIZE64` on `/dev/vda` for the device size.
2. `statfs` for the block size and the guest-read superblock for the total
   block count. `statfs.f_blocks` excludes overhead and cannot replace the
   latter. Honor the `64bit` feature when reading the high count.
3. Skip resize when counts match. Otherwise use `EXT4_IOC_RESIZE_FS` with
   a pointer to a u64 block count, then verify the resulting total.

Equal counts return success from `ext4_resize_fs`, but the ioctl still
reads the final device block and flushes the journal. Avoiding it matters.
Growth requires CAP_SYS_RESOURCE, a writable primary superblock, no ext4
error state and a readable last block. `sparse_super2` is unsupported.
The usual journal, metadata_csum, resize_inode, 64bit and flex_bg features
support growth; reserved GDT exhaustion can convert resize_inode to meta_bg.
64bit does not remove the 32-bit inode count limit. The last partial group
can be rounded down if too small for metadata; verification refuses an
incomplete requested growth rather than silently reporting the wrong size.

Growth adds group tables, bitmaps and backup metadata and flushes the
journal. With 4 KiB blocks and normal 32768-block groups, 4 GiB to 8 GiB
adds 32 groups; to 64 GiB adds 480. These are not constant-time
operations and no latency estimate is claimed until measured.

The root is mounted with `noinit_itable`. Without it the lazyinit thread
writes zeros over the inode tables of every new group after a grow, 48 MB
for 1 GB to 4 GB, competing with the first commands of the run. Those
tables sit in space the disk file never wrote, which reads as zero
already, and the checksummed group descriptors record which inodes are
unused. The filesystem keeps its new
size in later checkpoints. Online growth needs no fsck timestamp.

It runs before the agent listens, so the first request always sees the
size the caller asked for. Growing happens once per size change, never on
a plain boot. A failure stops the boot with the kernel error on the
console, because a disk smaller than asked for is a wrong disk.

Growth happens in the guest kernel. The host never parses a filesystem a
guest wrote.

### Sizes live with the disk

A disk is its size. A run or fork asking for more extends the file, as
now, and the guest grows the filesystem once; a disk saved afterwards
keeps that size. A request smaller than its source disk is rejected, including CAS.

4096 MB becomes the smallest disk. A configuration asking for less is
refused with a message naming the minimum.

The base image still ships as a 1 GB filesystem, and the host prepares the
file once: `WorkDisk::prepare_base` rewrites it sequentially into a new
file, extends that file to 4096 MB and renames it over the base. The
installer runs it after unpacking, and a boot from the base runs it when
the base is still short, which covers SDK users and bases built locally.
A base at least 4096 MB long is taken as prepared, so every later boot
pays one `stat`. It never applies to `--rootfs` or to a checkpoint.

The file layout is the reason. On APFS, extending a clone, or writing
into the space beyond its source, costs time in proportion to the extent
count of the source file. Measured on an M-series Mac, extending a clone
of the 1 GB base to 4 GB:

| source | extend |
|---|---|
| base built by `prepare-rootfs.sh` | 550 to 1480 ms |
| base unpacked from the 0.8.0 release | 26 to 250 ms |
| the same bytes written sequentially | 12 to 90 ms |
| sequential and already 4 GB | nothing to extend; clone 0.2 ms |

This was the largest single cost of a base boot on the Mac, larger than
the guest kernel. With the base prepared, the guest grows its filesystem
online in about 6 ms and nothing else is paid per boot. The filesystem is
not grown on the host, since the host does not run resize tools on images.
A fixed 4096 MB image built that way would also not need preparing, but
the file a tar unpacks is laid out by the unpacker, so the host would
prepare it anyway.

A disk never shrinks. Its size is a ceiling, not an allocation: a 16 GB
disk does not allocate all 16 GB up front. Metadata, unreclaimed extents
and filesystem allocation units add to the live file payload. Shrinking would mean an offline
resize, either on the host (parsing a guest filesystem) or in a guest
stage with the root unmounted, with a full check first and a failure when
the data does not fit, all to lower a capacity limit. What
does cost the host is space the guest freed and the host still holds,
which the next section fixes.

### Freed space goes back to the host

Today a file deleted in the guest keeps its blocks on the host for the
life of the disk, and in every checkpoint saved from it. A sandbox that
builds and deletes 20 GB holds 20 GB.

- **Linux host.** The virtio-blk backend offers `VIRTIO_BLK_F_DISCARD` and
  `VIRTIO_BLK_F_WRITE_ZEROES`. A discard, or a write zeroes with the unmap
  flag, becomes `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)` on
  the disk file; a write zeroes without it becomes `FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE`. A failed host operation
  fails the request; write zeroes must never acknowledge nonzero data.
  Ranges and segment counts come from the guest, so each is checked
  against the disk size and the advertised limits before it reaches the
  host. Bad bounds fail with `VIRTIO_BLK_S_IOERR`; unsupported flag bits
  use `VIRTIO_BLK_S_UNSUPP`, as required by the spec. On a
  reflinked disk a punched range drops this disk reference to the shared
  extents and leaves the base and other clones alone.
- **macOS host.** The framework already offers discard to the guest
  (measured: `discard_max_bytes` 4 GiB); nothing in the guest used it.
- **Guest.** Batched trim, not the `discard` mount option. With `discard`,
  ext4 sends one discard per freed extent when the journal commits and
  holds those blocks until the discard finishes. A build that churns
  small files turns into a stream of small hole punches: commit latency
  in the guest, a host disk file split into many small extents (slower
  I/O and slower clones afterwards), and holes punched in blocks that are
  allocated again a moment later. `FITRIM` walks the free space instead
  and sends large merged ranges, skips ranges below a minimum length, and
  skips fully trimmed groups until newly freed blocks clear that state
  or a smaller minlen requires another pass. This tracking is in memory
  and resets across mounts. Use minlen 1 MiB; the kernel can raise it to
  device granularity. Trim byte counts are submitted free ranges, not
  proof of reclaimed host allocation.
- **When.** Two triggers, both decided by the host. Seal runs `FITRIM` on
  `/` before `FIFREEZE`, including checkpoints from before this change.
  Small free extents, concurrent frees after trim and storage that ignores
  discard can still occupy host blocks. Unsupported trim is an explicit
  nonfatal result; other ioctl failures fail seal. And a `Trim` request lets the host
  ask a running sandbox to give space back: the SDK exposes it, and the
  node runtime sends it under disk pressure. There is no timer in the
  guest. Policy stays on the host, which is the
  side that knows when space matters.
- **CAS storage.** The NBD export advertises trim only if `shuru-store`
  handles `NBD_CMD_TRIM`; until it does, discards there are not offered
  and the disk behaves as it does today.

The Linux configuration layout follows virtio 1.2 section 5.2.4: capacity
at byte 0, discard limits at 36 and 40, alignment at 44, zero limits at 48
and 52, and `write_zeroes_may_unmap` at 56. Advertise 2097152 sectors per
range, 32 ranges, discard alignment 8 sectors and zero-unmap support. Read
only disks advertise neither destructive feature. Requests 11 and 13 use
a 16-byte header, packed 16-byte little-endian ranges and a writable status
byte. The header sector is zero. Range bit 0 permits unmap only for zero;
all discard flags and other zero flag bits must be zero.

Bound descriptor traversal by the queue size, reject cycles and indirect
chains, validate direction and guest memory spans, cap data segment sizes,
and copy all range records before validating all of them. Check counts,
capacity subtraction and signed fallocate byte offsets before any mutation.
Unsupported host filesystems may refuse fallocate. A real I/O failure can
leave earlier validated ranges completed, as the spec permits.

The backend does not offer `VIRTIO_BLK_F_FLUSH`, as before. A write
completes once it is in the host page cache, which survives the guest and
the VMM process and is what a checkpoint clone reads; only a host crash
loses it, and a running disk does not promise to survive one (see What is
supported). Syncing to stable storage on every guest flush would put a host
`fdatasync` on every journal commit and every guest `fsync`, the hot path of
package installs and git, to buy a guarantee nothing relies on. This is
the default of Firecracker (`Unsafe` cache type) and QEMU `cache=unsafe`.
Durability starts at publication: the checkpoint writer syncs the new file
and its directory.

### The image

`prepare-rootfs.sh` gains `-e remount-ro` on `mkfs.ext4`, and the image is
unmounted cleanly before it is packed, so the base is sealed by
construction. The image stays 1024 MB. The rootfs no longer installs
shuru-init.

### Network

shuru-guest already brings up `lo` and checks for `eth0`. It gains the
rest of what the initramfs did when `eth0` exists: `SIOCSIFADDR`
10.0.0.2, `SIOCSIFNETMASK` 255.255.255.0, `SIOCADDRT` default route via
10.0.0.1, and `/etc/resolv.conf` pointing at 10.0.0.1. With no `eth0` it
skips all of it.

## Security

- No host filesystem parser or performance telemetry endpoint. Discard
  and zero extend the existing virtio-blk request surface with bounded
  standard operations. The guest reads filesystem metadata; the host
  handles only block ranges and file clones.
- A guest that lies about sealing hurts only its own checkpoint, which
  only its own kernel reads later.
- Network settings in the guest were never a boundary; the proxy, the
  allowlist and secret substitution on the host are.
- The agent is read from the image on every boot, never from the guest
  writable disk.

## Compatibility

- The OS image changes, so this ships as a new OS release; the CLI already
  refuses an image whose VERSION does not match.
- Old checkpoints boot: the agent comes from the new initramfs, the kernel
  replays their journal, and the equal-size resize ioctl is skipped, and growth happens once.
- macOS boots the same initramfs through `VZLinuxBootLoader`.

## What will be measured

On a1.metal and an M-series Mac, each in the evidence log:

| case | measure | expect |
|---|---|---|
| `shuru run -- true` | wall time | targets: a1.metal under 0.40 s; Mac under 0.20 s |
| 100 Hz against 1000 Hz on a1.metal | guest uptime at `Run /init` | the virtio-mmio effect of the tick |
| same | guest uptime at `Run /init`, root mounted, first exec | `/init` under 50 ms |
| VM created to guest kernel start | host side timestamps | the 210 ms split into kernel load, memory setup, disk clone, connect |
| boot that grows 4 GB to 8 GB, and to 64 GB | time in stage one | paid once per size change |
| seal during a sustained write (`fio`) | seal time; the clone booted | superblock clean, no journal replay, file contents match up to the seal |
| checkpoint at stop and of a running sandbox | both paths through `save_disk` | the same clean result on both hosts |
| guest hung before the seal | `checkpoint update` | error, old checkpoint bytes unchanged |
| write 2 GB, delete it, `Trim` | host allocated blocks of the disk file, before and after | substantial reclamation of large free extents, both hosts |
| a small file churning build (`npm install`, a kernel build) with `discard` mounted, against `Trim` after it | guest wall time, host extent count of the disk file, hole punch calls | confirms batched trim over the mount option |
| `FITRIM` in a seal, fresh disk and a pre-change checkpoint | seal time | |
| discard ranges outside the disk, too many segments | request status | `IOERR`, file untouched |
| a pre-change checkpoint | boot | kernel replays the journal; next save is clean |
| seal with no thaw before the deadline | guest state, next `Thaw` | guest writable again, `Expired`, no checkpoint |
| disk with the errors flag set | boot, seal | both refused with the message |
| first boot after install, and later boots, Mac and a1.metal | `prepare_base` time, clone time | a few seconds once, then under 1 ms |

## Alternatives considered

- **Read-only remount instead of freeze.** Fails with `EBUSY` on any open
  writer, so it needs every process killed first and cannot serve a
  running sandbox.
- **Keep e2fsck, skip only resize.** Every checkpoint boot pays a check of
  a filesystem the journal already made consistent.
- **No initramfs, boot the disk.** Checkpoints would keep the agent they
  were saved with and fail the hello after an upgrade.
- **resize2fs on the host when a disk is cloned.** The host would parse
  filesystems that guests wrote.
- **Kernel `ip=`.** Needs `CONFIG_IP_PNP`; the host must leave it off when
  no network device is attached or the kernel waits for one, and boot can
  wait on link state each VMM reports differently. Both failures are
  stalls that show only in dmesg.
- **Snapshots to skip the boot.** RFC 0009. With a cold boot near 0.3 s a
  bare snapshot has little to skip; templates of warm state are what is
  left for it.

## Source checks

The review used Linux v6.12 sources. The build script pins 6.12.114;
repeat runtime checks on that exact built kernel before accepting timing
or compatibility results. The stable tag was not retrievable in this review.

- `fs/super.c`, `freeze_super` and `thaw_super_locked`: freeze ordering and
  the inability to cancel an ioctl by expiring a userspace deadline.
- `fs/ext4/super.c`, `ext4_freeze`, `ext4_unfreeze`, `ext4_handle_error`
  and `ext4_clear_journal_err`: recovery flag and filesystem errors.
- `fs/ext4/ioctl.c`, `__ext4_ioctl`: resize journal flush and trim support;
  `fs/ext4/resize.c`, `ext4_resize_begin`, `ext4_resize_fs` and
  `setup_new_flex_group_blocks`: limits and metadata work.
- `fs/ext4/mballoc.c`, `ext4_trim_fs`, `ext4_trim_all_free` and
  `ext4_try_to_trim_range`: minlen and per-mount trimmed group state.
- `fs/ext4/sysfs.c`, `errors_count`; `fs/ext4/ext4.h`, `ext4_super_block`:
  error count versus state flag and fixed superblock fields.
- `fs/overlayfs/util.c`, `ovl_start_write`; `fs/overlayfs/file.c`,
  `ovl_write_iter`: upper filesystem writes participate in freeze.
- BusyBox `util-linux/switch_root.c`, `switch_root_main`: old-root cleanup
  precedes MS_MOVE and chroot. The static agent omits the final exec.
- [Virtio 1.2 sections 5.2.3 to 5.2.6.2](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html):
  block features, configuration layout, ranges, statuses and flush.

Source files are available under
[Linux v6.12](https://github.com/torvalds/linux/tree/v6.12) and
[BusyBox switch_root](https://github.com/mirror/busybox/blob/master/util-linux/switch_root.c).
