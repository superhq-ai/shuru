# RFC 0011: Host memory bounds

- Status: Draft
- Date: 2026-10-28
- Scope: shuru-proxy, shuru-linux, shuru-vm, shuru-cli, shuru-sdk
- Depends on: nothing
- Bears on: RFC 0007 (hosted Shuru), RFC 0008 (node runtime)

## Summary

`--memory` sets how much RAM the guest gets. It says nothing about the host
processes that serve the guest: the VMM device code, the network proxy, the
CLI stdio server and the SDK. Each of them copies guest data into its own
heap, and before this change several of them kept copying for as long as
the guest kept producing. A guest that wrote to a socket nobody read, or
printed to a stdout nobody consumed, grew the host process by gigabytes in
seconds.

This RFC states the rule those processes follow from now on, records what
was changed to meet it, lists the ceilings that remain per VM, and sets out
the cgroup envelope that hosted nodes put around each sandbox.

## The rule

What a guest does never changes how much host memory serves it. Three
mechanisms keep that true:

1. **Streams apply backpressure.** Data the consumer has not taken stays in
   the guest. The host holds a fixed window per stream and stops reading
   from the guest when the window is full; the guest then blocks on its own
   socket, pipe or vsock buffer, inside its own RAM.
2. **Buffered calls have a size limit.** A call that returns everything at
   once (exec output, file read) fails past a fixed size and names the
   streaming alternative, as Node does with `maxBuffer`.
3. **Tables have a count limit.** Anything the guest can make the host
   allocate per item (sockets, connections, open files, inodes, DNS queries
   in flight) has a cap, and hitting it fails that item, not the process.

A fourth rule sits next to these: guest input never panics a host process.
Release builds use `panic = "abort"`, so a guest-triggered panic in the VMM
takes the whole sandbox down with it.

## What changed

| Path | Before | After |
|---|---|---|
| Proxy, guest to upstream | every read from smoltcp queued for the engine | 128 KiB in flight per connection; the stack reads from the guest only as the engine consumes |
| Proxy, upstream to guest | every upstream read queued for the stack | 128 KiB of permits per connection; the upstream reader waits for the guest |
| Proxy, closed sockets | removed only on the guest side close path | host closes drain with a 60 s timeout, then are removed; unanswered listeners go after 5 s |
| Proxy, stale commands | a reused socket handle could receive the data of a dead connection | connection ids carry a serial; stale commands are dropped |
| Proxy, DNS | a task per query, unbounded | 64 queries in flight, the rest dropped |
| Proxy, chunked bodies | a chunk size line could grow without end | lines past 4 KiB switch the connection to a plain tunnel |
| CLI stdio, spawn and watch events | unbounded channel to the writer thread | 64 events; a client that stops reading stalls the guest |
| CLI stdio and Rust SDK, exec | all output held | 64 MiB per stream, then the call fails |
| CLI stdio and Rust SDK, read_file | the whole file held | 64 MiB, then the call fails |
| Rust SDK, shell and watch events | unbounded channels | 64 events, reader threads block |
| Linux VMM, virtqueues | queue size, ring addresses and chains taken from the guest as given | size within the device maximum, rings inside guest memory, avail distance at most the size, chains checked for loops, range and flags |
| Linux VMM, net TX | a frame buffer sized by the guest chain | one fixed buffer of the largest valid frame; longer chains are dropped |
| Linux VMM, balloon | inflate length from the guest | 256 page numbers per descriptor |
| Linux VMM, virtio-fs | reads and writes sized by the guest, unbounded handle and inode tables, names read until a NUL | 128 KiB per read or write, 512 open files, 262144 inodes, names stop at PATH_MAX |

The proxy also fixes a race found while testing the above: smoltcp could
read a guest SYN straight from the socketpair while it polled, skipping the
pass that adds a listener for it, and answered with a reset. A loop of short
connections failed somewhere between 150 and 475 connections in. smoltcp
now only sees frames that were inspected.

## Measured

macOS, release build, `footprint` physical memory of the `shuru` process.
The old build is the commit before these changes.

| Scenario | Old build | New build |
|---|---|---|
| Host floods a socket the guest never reads | 1.3 GB after 3 s, still growing | 6 MB flat |
| Guest floods a socket the host never reads | not run | 8 MB flat |
| 600 short connections in a row | refused after 150 to 475 | 600 of 600, four runs |
| Guest `cat /dev/zero` over stdio spawn, client never reads | 2 GB after 5 s | 5 MB flat |
| stdio exec printing 2 GiB | holds it all | fails at 64 MiB with an error, server keeps serving |
| stdio read_file of a 1 GiB file | holds it all | fails at 64 MiB with an error |

Linux on an a1.metal (Graviton, KVM), release build, new build only.
Overhead is the resident memory of the `shuru` process outside the guest
RAM mapping, read from `/proc/PID/smaps`, peak over the run.

| Scenario | Host overhead |
|---|---|
| Idle guest | 6 MB |
| Host floods a socket the guest never reads | 7 MB |
| Guest floods a socket the host never reads | 7 MB |
| 600 short connections in a row | 8 to 11 MB, 600 of 600 |
| 400 connections the host floods, guest never reads | 199 MB |
| Guest `cat /dev/zero` over stdio spawn, client never reads | 6 MB |
| stdio exec printing 2 GiB, then read_file of 1 GiB | 33 to 45 MB, both fail at 64 MiB, server keeps serving |
| Kill a process that stopped reading 8 MiB of stdin | 11 MB, gone after 2.1 s |
| Client hangs up on a running exec | 30 to 38 MB, nothing left running |

The 400 connection row is the cost of a stalled connection at the current
sizes: about 0.5 MB each, the smoltcp send buffer and the proxy window full
toward a guest that does not read.

## Ceilings that remain per VM

These are fixed, independent of guest behaviour, and some are large.

| Item | Ceiling |
|---|---|
| smoltcp sockets | 512 sockets of 512 KiB receive plus 256 KiB send, 384 MiB if every one is full |
| Proxy windows | 128 KiB each way per connection, 128 MiB across 512 sockets |
| Linux vsock | 1024 connections of 256 KiB, 256 MiB |
| virtio-fs | 512 open files, 262144 inodes |
| Buffered calls | 64 MiB per stream per call; the stdio server runs one at a time, SDK callers choose their own concurrency |

The smoltcp and vsock numbers are worst cases that need hundreds of
connections each holding full windows. They are fine on a laptop. On a
hosted node they set the per-sandbox overhead budget, so the node runtime
lowers them (see below).

The smoltcp buffers were halved from 1 MiB and 512 KiB. They span only the
hop between guest and host, under a millisecond; the remote round trip is
the host kernel socket to the upstream. On an M-series Mac a single stream
measured 1.8 to 2.0 Gbit/s down and 8.8 up at the new sizes against 2.0 to
2.1 and 7 to 9 before; halving again to 256 KiB and 128 KiB lost a fifth.

## Stdin and kill

Stdin for a spawned process used to be fire and forget. When the process
stopped reading, stdin queued in the stdio server without limit, and the
KILL frame that shares the exec connection sat behind it: the guest agent
was blocked writing to the full stdin pipe and never read the next frame.

- `input` is now a request, answered once the data is written to the exec
  connection. The TypeScript SDK keeps up to 1 MiB in flight per process
  and queues the rest in the calling process, the way a Node stream
  buffers. The stdio server refuses more than 4 MiB of unanswered stdin
  per process. Writes are split into frames under the 1 MiB frame limit.
- A kill sends SIGTERM in band as before. If the process has not exited
  two seconds later, the host sends a `KILL_REQ` naming the exec id on a
  connection of its own, and the guest SIGKILLs the process group. The
  guest forgets an id after the child exits but before it is reaped, so a
  late kill cannot reach a reused pid.
- The guest also SIGKILLs the process group when the host hangs up on a
  piped exec, so a client that dies leaves nothing running. On macOS this
  does not fire while host data is still queued for the guest, which is why
  the kill needs its own connection.

## Hosted: the cgroup envelope

Bounded buffers make the host overhead of a sandbox small and predictable.
They do not make it zero, and a bug in any one of them would reopen the
problem. Hosted nodes therefore put each sandbox in its own cgroup v2,
the same way the Firecracker jailer does:

- One cgroup per sandbox holds the VMM process, its device threads, the
  proxy and anything else spawned for that sandbox.
- `memory.max` is the guest RAM plus a fixed host allowance. Guest RAM is
  anonymous memory mapped by the VMM, so it is charged to the same cgroup,
  and pages the balloon returns (RFC 0006) are uncharged.
- `memory.swap.max` is 0, so a sandbox cannot spill into node swap.
- `memory.high` sits a little under `memory.max`, so a sandbox near its
  limit is throttled and reclaimed before the OOM killer runs.
- `pids.max` caps threads, since device threads and per-connection tasks
  are the other thing the guest can make the host create.
- When the OOM killer does run, it kills that sandbox and nothing else. The
  node runtime reports it as an OOM exit, not a crash.

This was run on an a1.metal with a systemd scope standing in for the node
runtime: `MemoryMax=1280M`, `MemoryHigh=1216M`, `MemorySwapMax=0`, guest
RAM 1024 MiB, so a 256 MiB allowance. The guest touched 900 MiB of its RAM,
then opened 400 connections the host flooded and never read them. Over
two runs the cgroup peaked at 1122 MB and 1239 MB. The second run crossed
`memory.high` and was throttled 790 times; neither run saw a `max` or
`oom` event, and both sandboxes exited normally. Guest RAM is charged to
the scope, as expected, so 256 MiB of allowance is already tight for a
sandbox that fills its RAM and stalls 400 connections.

The host allowance is sized from the ceilings above, and at the current sizes
the sum is too large to grant: 384 MiB of smoltcp buffers, 128 MiB of proxy
windows and 256 MiB of vsock buffers. The measured idle overhead is 6 MB
and a busy sandbox stays near 10 MB; only stalled connections cost real
memory, about 0.5 MB each. Hosted builds therefore lower the counts rather
than the buffer sizes, which would cost throughput:

| Item | Hosted limit | Worst case |
|---|---|---|
| smoltcp sockets | 256 | 192 MiB buffers, 64 MiB windows |
| Linux vsock connections | 128 | 32 MiB |

That gives an allowance of 320 MiB per sandbox: 288 MiB of ceilings plus
32 MiB for code, stacks and the TLS state of the proxy. A guest that goes
past it is throttled at `memory.high` and then killed alone, which is the
outcome the envelope exists for. RFC 0008 carries the final numbers.

macOS has no cgroups. Local sandboxes rely on the bounds alone, which is
the right trade for a laptop where the user owns both sides.

## Testing

- Proxy unit tests cover receive only yielding inspected frames.
- Linux unit tests cover rejected queues, chain loops, out of range
  descriptors, foreign flags, oversized net TX chains, virtio-fs name and
  handle caps.
- The pressure scenarios above run against a real VM with servers on the
  host reached through `--expose-host`.
- The TS SDK integration suite and the CLI real VM suite pass unchanged.
