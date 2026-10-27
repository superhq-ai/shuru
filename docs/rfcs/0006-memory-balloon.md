# RFC 0006: Memory balloon

- Status: Implemented
- Date: 2026-10-27
- Scope: shuru-linux, shuru-darwin, shuru-vm, shuru-sdk
- Depends on: nothing

## Summary

Every VM gets a virtio memory balloon. On a Linux host the guest kernel
reports pages it has freed and the host discards them, so an idle sandbox
costs the host what the guest is actually using rather than what it has
touched since boot. On both hosts a caller can set a memory target and the
guest gives up or takes back the difference.

```rust
let sb = AsyncSandbox::boot(config).await?;
sb.set_memory_target(1024 * 1024 * 1024).await?;
```

Nothing changes for `shuru run`. The device is attached, the guest driver
binds it, and free memory flows back on its own.

## Motivation

A hosted fleet of sandboxes is sold on memory. Each VM is configured with
2 GB, and before this change the host paid for every page the guest ever
touched: a build that used 1.5 GB and finished left 1.5 GB resident in the
host process until the VM stopped, even though the guest had freed it.
Overcommit is the whole business, and overcommit without a way to get
memory back is a crash waiting for a busy hour.

The two standard answers are the virtio balloon and free page reporting,
and every serious hypervisor ships both. Firecracker, Cloud Hypervisor and
QEMU offer `VIRTIO_BALLOON_F_REPORTING`; Apple offers the traditional
balloon through `VZVirtioTraditionalMemoryBalloonDevice`. The guest kernel
already had `CONFIG_VIRTIO_BALLOON=y`, which pulls in page reporting, so
the only missing piece was the device on the host side.

## Design

### The device

`shuru-linux` gains a virtio-mmio balloon backend (device ID 5) that offers
three features:

- `VIRTIO_F_VERSION_1`.
- `VIRTIO_BALLOON_F_REPORTING`. The guest hands over runs of free pages of
  `page_reporting_order` and above (order 10, 4 MB, on this kernel) a
  couple of seconds after they free up. The host answers each run with
  `madvise(MADV_DONTNEED)` on the matching host range, which drops the
  pages from the process. Reporting happens in rounds two seconds apart, a
  sixteenth of each free list per round, so a freed gigabyte drains in
  under ten seconds.
- `VIRTIO_BALLOON_F_DEFLATE_ON_OOM`. A guest under memory pressure may
  take ballooned pages back without asking, which is the safe default for
  a host that set a target and walked away.

The inflate queue carries arrays of 4 KiB page numbers; consecutive pages
are discarded as one run, and only whole host pages go, since a 4 KiB
guest page on a 64 KiB host page may share that page with memory still in
use. The deflate queue is acknowledged and nothing else: a discarded page
reads as zero when the guest touches it again, so giving memory back costs
one page fault per page and no host work.

Queue indices follow the negotiated features. The spec lists stats,
free page hints and reporting as queues 2, 3 and 4, but each exists only
when its feature is negotiated, and a queue that does not exist takes no
index. The Linux driver, Cloud Hypervisor, crosvm and QEMU all number the
optional queues this way, in spec order, skipping the absent ones. The
backend builds its layout from the features the driver accepted at
`FEATURES_OK`, so with only reporting on offer the reporting queue is
index 2, and if stats are added later it moves to 3 on its own. The first
build hardcoded index 4, the guest kicked index 2, and its reporting
worker waited forever for an acknowledgement that never came (visible as
a kworker parked in `virtballoon_free_page_report`).

### The target

`BalloonControl` is the host handle. `set_memory_target(bytes)` turns the
difference between the configured size and the target into a page count,
writes it to the device config and raises a config change interrupt; the
guest driver allocates that many pages and sends them up the inflate
queue. The guest writes back how many pages it has actually given up,
which `ballooned_bytes` reads.

On macOS the same call reaches `VZVirtioTraditionalMemoryBalloonDevice`
through `targetVirtualMachineMemorySize`, on the VM queue as the framework
requires. The framework offers no free page reporting, so on a Mac only
the target moves memory. That is fine: a Mac is one user running a few
sandboxes, and the hosted fleet is Linux.

### Where it lives

- `shuru-linux` 0.3.2: `kvm/balloon.rs`, `VirtualMachine::set_memory_target`
  and `ballooned_bytes`. The device is attached when the configuration
  carries a `VirtioMemoryBalloonDevice`.
- `shuru-darwin` 0.2.2: `VirtualMachine::set_memory_target`.
- `shuru-vm` 0.5.2: attaches the balloon on both platforms and exposes
  `Sandbox::set_memory_target`.
- `shuru-sdk` 0.7.1: `AsyncSandbox::set_memory_target`, and an example
  (`cargo run -p shuru-sdk --example balloon`) that prints host and guest
  memory at each step.

## What was measured

a1.metal, Ubuntu 24.04 host kernel 7.0.0-1014-aws, 2 GB guest, resident
size of the host process:

| step | host resident | guest MemFree |
|---|---|---|
| booted | 23 MB | 1968 MB |
| guest fills 1 GB of tmpfs | 1109 MB | 933 MB |
| 2 s after the guest frees it | 855 MB | 1975 MB |
| 5 s | 599 MB | 1975 MB |
| 7 s | 343 MB | 1975 MB |
| 9 s | 87 MB | 1975 MB |
| target 1 GB, 1 s later | 84 MB | 951 MB |
| target 2 GB, 1 s later | 84 MB | 1887 MB |

The 1 GB came back without anyone asking, about 256 MB per two second
round.
Setting the target afterwards found nothing resident left to take, which
is the point: reporting had already done the work. On macOS the target
moved guest MemFree from 1970 MB to 947 MB and back to 1894 MB; the host
side is not visible from the process there.

The guest reporting worker is idle after a run and the balloon device
shows the three negotiated feature bits, see the evidence log.

## Not in this RFC

- `VIRTIO_BALLOON_F_STATS_VQ`, guest memory statistics for the host. A
  hosted scheduler will want them; they are a small addition on the same
  device.
- Memory hotplug (`virtio-mem`). The balloon moves memory inside a fixed
  size; growing a VM past its configured size is a different device.
- A `shuru run` flag. Nobody sets a target by hand on a laptop.
