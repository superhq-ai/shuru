# RFC 0008: Linux node runtime

- Status: Draft. The vsock device and the connection hello shipped in 0.10.0 (PR #56); host memory bounds in 0.11.0 (RFC 0011).
- Date: 2026-10-27, updated 2026-10-28
- Scope: shuru-linux, shuru-proto, shuru-guest, new `shuru-vmm` and `shuru-node` binaries
- Depends on: nothing
- Required by: hosted Shuru, RFC 0009 (Linux VM snapshots)

## Summary

Every VM on a Linux host runs in its own process, under its own user, in
its own namespaces and cgroup, behind a seccomp filter, and holding nothing
but the file descriptors it was handed. The host reaches the guest through
a vsock device implemented inside that process and exposed as a unix
socket only the node agent can open, and every connection proves which VM
it reached. The network proxy, which holds real secret values, runs in a
separate process from the VM.

The immediate reason is a bug: on Linux, a second sandbox started while
another is running executes its commands inside the first VM. The larger
reason is that a hosted node runs strangers next to each other, and today
the only wall between them is the hardware virtualization boundary.

## Motivation

### Two sandboxes on one Linux host share one guest

Reproduced on a1.metal with the released 0.9.3 CLI (evidence log): VM A
writes a marker to its own `/tmp` and stays up; a second `shuru run`
started ten seconds later prints that marker and the kernel boot ID of VM A.
VM B never ran anything. Nothing was logged.

The cause is that every VM asks vhost-vsock for guest CID 3. CIDs are
global to the host kernel, the kernel refuses the duplicate, and the host
side, which connects to CID 3 within milliseconds, reaches whichever VM
owns it. The guest protocol carries no identity, so nothing notices.

Handing out unique CIDs would stop the collision and leave the design
problem in place:

- Any process on the host that can create an `AF_VSOCK` socket can connect
  to any guest agent port, and the agent executes what it is sent as root.
  vsock ignores network namespaces, so jailing processes does not contain
  this.
- vhost-vsock keeps its state in the host kernel, where a snapshot cannot
  reach it (RFC 0009).
- Every local Linux user needs the `vhost_vsock` module loaded and access
  to `/dev/vhost-vsock`.

macOS is not affected. Virtualization.framework gives every VM its own
vsock device and the host reaches it through that VM object only.

### The VM runs inside the caller

The SDK and the CLI run the VMM on threads in their own process: vCPU
loops, virtio device emulation, the virtio-fs server and the network
proxy. A guest that finds a bug in descriptor parsing lands in a process
with the caller user, the caller files, every other VM that process
started, and, through the proxy, the real values of every secret. There
are no limits on memory, CPU, processes or disk bandwidth beyond what the
guest was configured with.

On a laptop that process belongs to the person who started the sandbox,
and the in-process model is acceptable. On a hosted node it is not: one
tenant escaping device emulation would reach every other tenant on the
machine.

## Design

### Process layout on a node

```
shuru-node (root, one per host)
 |  control channel to the hosted control plane
 |  data API: exec, files, ports
 |
 +-- per sandbox, uid 100000 + slot, cgroup shuru.slice/sbx-<id>
      |
      +-- shuru-vmm      namespaces: mnt pid net ipc uts cgroup
      |   seccomp: kill on anything outside the allowlist
      |   fds in: /dev/kvm, disk, kernel, initrd, memory file,
      |           net socketpair end, vsock listener, control listener
      |
      +-- shuru-netd     uid 200000 + slot, own cgroup
          seccomp: socket I/O only, no exec, no open
          fds in: the other net socketpair end, egress policy, secrets
```

`shuru-node` is the only privileged process. It creates every resource a
VM needs, opens every file, and passes descriptors to two unprivileged
children per sandbox.

### shuru-vmm

A thin binary over the `shuru-linux` library. It receives its
configuration as one message on its control socket, builds the VM from the
descriptors it was given, and never opens a path or creates a socket after
that. Control operations on the same socket:

- `start`, `stop`
- `pause`, `resume`
- `snapshot`, `restore` (RFC 0009)
- `set_memory_target`, `stats` (vCPU time, guest memory resident, balloon
  state, virtio queue counters)

It exits when the guest powers off or when its control peer asks.

The control socket is a listening unix socket in a directory owned by the
agent, so a restarted agent reconnects to running VMs instead of losing
them. VMMs run in their own systemd scope and are not killed with the
agent.

The `shuru` CLI and SDK on a local Linux machine keep calling the library
in process, as they do now. The library is the same in both places; only
the process wrapper and the jail differ.

### The jail

For each sandbox the agent:

1. Allocates a slot. The slot gives the VMM uid and gid `100000 + slot`
   and the proxy uid and gid `200000 + slot`. A slot is not reused until
   both processes have exited and the cgroup is gone.
2. Creates the cgroup `shuru.slice/sbx-<id>`, with the VMM and the proxy
   in child cgroups below it, and on the parent:
   - `memory.max` = guest memory + 320 MiB, `memory.high` 64 MiB below
     that, and `memory.swap.max` = 0, so an overrun throttles and then
     kills this sandbox only. RFC 0011 measured the allowance on a1.metal:
     a 1024 MiB guest filling its RAM while 400 connections flooded it
     peaked at 1122 and 1239 MB in a 1280 MB envelope. 320 MiB covers 256
     proxy sockets (192 MiB of smoltcp buffers and 64 MiB of relay
     windows), 128 vsock connections (32 MiB) and the base process. Those
     numbers were taken with the VMM and the proxy in one process; the
     split between the two child cgroups is measured when they separate,
     and the parent limit holds either way.
   - `cpu.max` = vCPU count times the period, and a `cpu.weight` by plan.
   - `pids.max` sized to the vCPU and device threads.
   - `io.max` on the node data device, by plan.
3. Forks the VMM child, which unshares mount, pid, network, ipc, uts and
   cgroup namespaces, pivots into an empty tmpfs root holding only the
   read-only VMM binary, sets `no_new_privs`, drops every capability from
   all sets including the bounding set, sets rlimits (`NOFILE`, `NPROC`,
   `CORE` = 0), switches to the slot uid, closes every descriptor not on
   the hand-off list and executes the VMM.
4. The VMM installs its seccomp filter on all threads (`TSYNC`) after
   building the VM and before the first `KVM_RUN`. The filter is an
   allowlist built with the rust-vmm `seccompiler` crate: `ioctl` limited to
   the KVM requests in use, `read`, `write`, `epoll`, `eventfd`, `futex`,
   `mmap` and `munmap` on existing descriptors, `madvise` with
   `MADV_DONTNEED` only, `accept4` on the handed-over listeners, signal and
   exit calls. `open`, `openat`, `socket`, `connect`, `execve`, `ptrace`
   and everything else kill the process. A development build can run the
   filter in log mode to find missing calls.
5. Starts the proxy child the same way with its own uid and cgroup. It
   keeps network access, because it is the egress path, and its filter
   denies `execve`, `open` and `ptrace`.

User namespaces are not used. The agent is root on a machine dedicated to
this, real uids are simpler to reason about, and unprivileged user
namespaces widen the host kernel surface a guest escape could reach.

### Guest channel: vsock inside the VMM

`shuru-linux` gains a virtio-vsock device (device ID 19) implemented in the
VMM, replacing vhost-vsock everywhere, including local Linux runs.

- The device speaks the standard virtio-vsock packet protocol on its rx,
  tx and event queues: connection requests, responses, resets, shutdowns,
  data and credit updates, with per-connection credit flow control. The
  guest driver is the one the kernel already has; nothing changes in the
  guest image.
- The guest CID stays 3. It now names this VM inside this device only, so
  there is no host-wide namespace to collide in.
- As shipped in 0.10.0, the host end of each connection is a socketpair
  inside the process that runs the VM, which is all the CLI and SDK need.
  The receive path is pull based: host data stays in the host socket
  until the guest posts rx buffers, then one `readv` fills as many as the
  guest credit allows.
- Under `shuru-vmm` the agent is a different process, so the VMM also
  takes a listening unix socket. A client connects, writes
  `CONNECT <port>\n`, and gets `OK\n` once the guest accepted, after which
  the stream carries bytes in both directions. This is the convention
  Firecracker uses, so existing tooling understands it.
- Guest-initiated connections to the host are refused with a reset.
  Nothing uses them today, and a guest has no business opening one.
- Connections are capped per VM: 1024 locally, 128 on a hosted node,
  given to `shuru-vmm` with its configuration. The proxy takes its socket
  cap the same way: 512 locally, 256 hosted (RFC 0011).
- On restore from a snapshot the device sends a transport reset event, the
  guest drops its connections, and the host reconnects (RFC 0009).

On the node only the agent can open the socket: it lives in an
agent-owned directory with mode 0700. On a laptop it lives in the run
directory of the process that started the VM.

### Guest identity on every connection

Every connection to the guest agent opens with a hello:

1. The host sends `HELLO { token }`, 32 random bytes generated by the host
   for this VM and delivered on the kernel command line.
2. The guest checks the token in constant time, closes the connection on a
   mismatch, and replies `HELLO_OK { instance_id }`.
3. The host checks the instance ID against the VM it meant to reach and
   drops the connection otherwise.

The token does not defend against the guest; the guest belongs to the
sandbox owner and can read its own command line. It defends against the
host reaching the wrong guest, which is exactly the failure above, and
against any host process other than the one that started the VM. After a
snapshot restore the host sends a new token and instance ID over the
first connection, so clones of one snapshot never share either (RFC 0009).

This shipped in `shuru-proto` as protocol version 2 (0.10.0). The guest
image and the host binary come from the same release and are already
locked together by the `VERSION` file, so there is no mixed-version window
to support. macOS uses the same hello for uniformity. Serial output
redacts the token, and a guest booted without one stops instead of
serving. The rotation after restore is not built yet; it lands with RFC
0009.

### Network

The proxy (`shuru-proxy`) moves out of the VMM into `shuru-netd`, one per
sandbox. The VMM holds one end of the frame socketpair and never sees a
secret value. The proxy holds the other end, the egress policy and the
secrets bound to this sandbox only.

Egress is enforced twice. The proxy refuses link-local, loopback, private
and VPC ranges after resolving a name, so a guest cannot reach the
instance metadata service, the agent, or other nodes. The host firewall
(nftables) drops the same ranges by the proxy uid (`meta skuid`), which
also gives per-sandbox byte counters and rate limits without any proxy
code.

On a laptop the proxy stays in process; the secrets it holds belong to
the person running it.

### shuru-node

The agent is the only component with root:

- slots, cgroups, jails, uids, firewall rules;
- working disks (the descriptor-held clones from 0.9.2, on XFS) and
  snapshot files (RFC 0009);
- the control channel to the hosted control plane;
- the data API that SDK clients reach (exec, PTY, files, watch, port
  connections), served by opening the VM vsock socket and speaking the
  existing guest protocol.

It never parses guest-controlled data with privileges beyond what the
data API needs, and its own data API endpoint is authenticated with
tokens from the hosted control plane.

## What will be measured

Measured on a1.metal for PR #56, against 0.9.3 with vhost-vsock on the
same host:

- The reproduction in the evidence log fails: two concurrent VMs each see
  only their own marker and boot ID, six VMs at once give six boot IDs.
- Exec round trip p50 1.8 ms against 1.6 ms (target: within 1 ms of vhost).
- Guest to host 434 to 451 MB/s against 117 to 191 MB/s; host to guest 315
  to 360 MB/s against 353 to 366 MB/s (target: at least 200 MB/s).

Measured for RFC 0011: host overhead per VM is 6 to 11 MB outside guest
RAM in every pressure scenario except 400 stalled connections (about 200
MB), and a sandbox in the cgroup envelope above survives its worst case.

Still to measure, before hosted ships:

- Boot to exec ready, jailed against unjailed. Target: within 20 ms.
- The full SDK test suite with the seccomp filter in log mode, then in
  kill mode.
- Density: idle 256 MB sandboxes on one host until memory runs out, with
  free page reporting on (RFC 0006).

## Alternatives considered

- **Unique CIDs with vhost-vsock.** Fixes the collision, leaves every
  guest reachable from every host process and leaves vsock state outside
  any snapshot. Rejected.
- **Firecracker as the hosted VMM.** It has a jailer, a seccomp allowlist,
  userspace vsock and snapshots today, and years of production use. It
  would also mean two VMMs: ours for local Linux and macOS parity, and
  Firecracker for hosted, each with its own device set, boot path and
  failure modes against one guest image. The pieces this RFC adds are
  bounded: one virtio device, one jail routine, one filter. Keeping one
  VMM is the recommendation, with the caveat that ours is younger and
  unaudited; a fuzzing harness for the virtio queue and descriptor
  parsing is part of this work.
- **gVisor or containers instead of VMs.** A different isolation model
  from the one Shuru is built on. Out of scope.

## Not in this RFC

- The hosted control plane, the public API and the node data API wire
  format.
- Snapshots (RFC 0009).
- virtio-fs on hosted nodes. Hosted sandboxes have no host directory to
  share; the device is not attached there.
- Changes to macOS beyond the protocol hello.
