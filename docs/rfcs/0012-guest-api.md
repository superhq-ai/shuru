# RFC 0012: Guest API for long-lived sandboxes

- Status: Draft
- Date: 2026-10-29
- Scope: shuru-guest, shuru-proto, shuru-node, shuru-netd
- Depends on: RFC 0004 (guest services), RFC 0006 (memory balloon), RFC 0008 (node runtime), RFC 0009 (Linux VM snapshots)

## Summary

A sandbox that lives for days and is driven over the network needs more
from the guest than one command per connection. This RFC adds exec
sessions that outlive the connection and can be attached again, a fuller
filesystem API, port forwarding and port events, managed services,
checkpoints, and network, privilege and resource policies, and the guest
protocol frames for them. `shuru-node` (the agent below) maps them onto an
HTTP and WebSocket API shape that agents already know.

## Motivation

Today `shuru-node run` boots one jailed VM, runs one command and stops it
(RFC 0008), and `shuru-node restore` does the same from a snapshot
(RFC 0009). A long-lived sandbox needs the opposite: it lives for days,
receives many requests, and survives snapshots with its processes intact.

The guest is also built around one command per connection. A command dies
when its connection drops, its output is gone if nobody was reading, and
nothing lists what is running. An agent driving a sandbox over the
network drops connections all the time.

## Design

### The agent

`shuru-node serve` keeps sandboxes on one host and serves the API for
them, HTTP and WebSocket on a unix socket that only root can reach. It
does no authentication of its own; whatever exposes the API beyond the
host brings that.

```
POST   /v1/sandboxes                  { "name": "web-1" }
GET    /v1/sandboxes                  ?prefix= &max_results= &continuation_token=
GET    /v1/sandboxes/{name}
DELETE /v1/sandboxes/{name}
```

Each sandbox keeps a directory under the state directory
(`/var/lib/shuru/sandboxes/<id>/`) with `sandbox.json` (id, name, size)
and its working disk, a reflink copy of the base disk the agent is given.
Create writes the directory under another name, syncs it and renames it
into place, as a snapshot is written (RFC 0009), then boots the VM.

A sandbox is `running` while its VM is up and `cold` otherwise. A cold
sandbox keeps its disk and boots on the next request that needs its
guest. A restart of the agent finds every sandbox cold. On SIGTERM the
agent shuts every guest down, which stops its services and syncs its
disk, then stops the VMs.

Each running VM has a thread in the agent that boots it, holds it and
stops it. The jailed VMM and netd are children of that thread and are
killed when it exits (`PR_SET_PDEATHSIG`), so a VM never outlives an
agent that crashed. VMs run in slots from `--first-slot`, one VM per slot
as with `run --slot`; at start the agent frees the slots a crashed run
left, and skips any whose cgroup still holds a process.

### Exec sessions (guest)

The guest agent keeps a session table. A session is one process started
through exec:

| field | meaning |
|---|---|
| `id` | decimal string, unique per boot of the guest agent |
| `command`, `workdir`, `tty` | as started |
| `created` | Unix time |
| `is_active` | the process is running |
| `last_activity` | time of the last input or output |
| `bytes_per_second` | output rate over the last ten seconds |

Each session keeps its output in a ring buffer of 1 MiB. Output is a
sequence of chunks, each tagged stdout or stderr, and every byte has an
offset counted from the start of the session across both streams. An
attach with `output_offset` replays what is still buffered from that
offset, then follows live output. A session survives every disconnect.
After it exits, its exit code and buffer stay for ten minutes, so a client
that lost its connection still learns how it ended; then the session is
dropped. At most the 64 most recently ended sessions are kept, so a burst
of short sessions drops the oldest sooner.

Up to 16 clients may attach to a session, and 256 in all. An attachment
that arrives while the session has no owner becomes its owner: its stdin
and resize messages reach the process, and `session_info` says
`is_owner: true`. When the owner detaches, the next attachment to send
input becomes the owner. With `max_run_after_disconnect`, a session is
sent SIGHUP each time it has had no attachment for that long; without it,
a session runs until it exits or is killed.

Each session runs in a cgroup of its own, and starts in its own session
and process group. Kill sends the signal to every process in the cgroup
and the cgroups below it, those that left the process group included,
waits `timeout` (default 10 seconds, at most a day) and then kills the
cgroup, reporting each step. It completes once the process has exited and
the cgroup is empty. SIGKILL goes through `cgroup.kill`. Any other signal
goes to each member through a pidfd, opened before the guest checks that
the process is still in the cgroup, so a signal never reaches a process
that took a reused pid. A cgroup that a process outlives is removed once
that process exits. Plain execs get a cgroup of their own too, so killing
one reaches what it started even after its first process is gone.

The exit code on the wire is one byte, as the API defines it: the exit
status, or 128 plus the signal number for a process a signal ended.

New guest protocol frames, on new connections from the agent (guest
protocol 4):

| frame | payload | answer |
|---|---|---|
| `SESSION_START` (0x70) | `ExecRequest` plus `max_run_after_disconnect` in seconds | `SESSION_INFO` (0x72), then output frames as `EXEC_REQ` does today |
| `SESSION_ATTACH` (0x71) | `{ id, output_offset }` | `SESSION_INFO`, buffered output, then live output |
| `SESSION_LIST` (0x73) | none | `SESSION_LIST_RESP` (0x74), the session table |
| `SESSION_KILL` (0x75) | `{ id, signal, timeout }` | `SESSION_KILL_EVENT` (0x76) frames, the last `complete` with the exit code, or `error` |

`STDIN`, `STDOUT`, `STDERR`, `RESIZE`, `KILL` and `EXIT` keep their
meaning on an attached connection. `STDIN_EOF` (0x0A) closes the stdin of
a piped session; a terminal has no end of input, so it is ignored there.
Closing the connection only detaches. After `EXIT` the guest closes the
connection. `EXEC_REQ` stays for the local runtime.

`SESSION_INFO` carries the `output_offset` the following output starts
at: the one asked for, or the start of the buffer when those bytes are
gone, so a client can tell exactly what it missed. While anything is
attached, output waits for the slowest attachment, so an attached client
gets every byte and a process that writes faster than its clients read is
slowed down, as through a pipe. Output is only lost while nothing is
attached; the buffer then keeps the newest 1 MiB, or less when output
switches between stdout and stderr more than 16384 times within it.
Input waits in a queue
of 256 KiB; while it is full the guest stops reading the owner, and other
sessions are not held up. At most 128 sessions run at once; a start past
that gets an `ERROR`. Kill events are the JSON the
API streams as NDJSON, so the agent passes them through.

`PORT_EVENT` arrives with ports (slice 2): a port opened by a process of
an attached session.

The agent turns these into the two exec forms of the API. HTTP exec
starts a session, sends the request body as stdin when `stdin=true`, and
writes the response as frames of one stream byte and the data. The
WebSocket form maps one to one onto the frames above.

The agent holds one guest connection per session that has clients, however
many there are, and fans its output out to them. It closes that connection
when the last client leaves, so `max_run_after_disconnect` counts from
then. That keeps a sandbox
within its vsock connection cap, and it keeps slow clients out of the
guest: the agent reads at the pace of the fastest client, and a client
that falls more than 1 MiB behind it is closed with the offset it reached,
to attach again from there. A client whose `output_offset` was cut to the
start of the buffer is told so by a `gap` message with the bytes skipped,
before any output, instead of having to compare offsets.

### Files (guest)

The API filesystem operations map onto existing frames, with these
additions:

- every path is resolved against `workingDir` by the agent before it is
  sent, and must be absolute after that;
- `fs/write` takes `mode` and `mkdir` (create parent directories);
- `fs/list` returns `type`, `size`, `mode`, `modTime` and `isDir` per
  entry, which `READ_DIR_RESP` gains;
- `fs/copy` gains `preserveAttrs`; `fs/chmod` gains `recursive`;
- `CHOWN_REQ` is new: `{ path, uid, gid, recursive }`;
- `asRoot`: operations run as the default user unless `asRoot` is
  set, in which case they run as root. The guest agent switches its file
  system uid and gid per request (`setfsuid`, `setfsgid`) on the thread
  that serves it. The default user is set by the image, and is root when
  the image sets none.

`fs/watch` is the existing `WATCH_REQ`, sent as JSON messages with
`type`, `path`, `event`, `timestamp`, `size` and `isDir`.

Size limits are the API ones: 16 MiB for a file write, 10 MiB for exec
stdin over HTTP. Larger transfers use exec with stdin over the WebSocket.

### Ports and URL traffic

`proxy` opens a TCP connection from inside the guest to the host and port
in its first message, through the existing `FWD_REQ` path, then carries
raw bytes. `host` is usually `localhost`; any other name resolves inside
the guest, and its egress goes through netd like any other.

`ports/watch` asks the guest for a `PORTS_WATCH_REQ` stream. The guest
reads `/proc/net/tcp` and `/proc/net/tcp6` every 500 ms for listening
sockets, maps each socket inode to a process through `/proc/*/fd`, sends
the full list first and then `port_opened` and `port_closed` with
`address`, `port` and `pid`. The same scan sends `PORT_EVENT` to attached
exec sessions whose cgroup holds the process that owns the new socket.

### Services (guest, extends RFC 0004)

RFC 0004 services are declared by the image and start at boot. The API
makes them managed at run time and adds what that needs:

- `PUT services/{name}` writes `/etc/shuru/services/<name>.json` with
  `cmd`, `args`, `needs`, `env`, `dir` and `http_port`, then starts it.
  The file is on the guest disk, so the definition survives snapshots and
  reboots.
- `needs` orders start: a service starts after the services it needs are
  ready. A cycle is refused when the service is written.
- A service that exits is started again after one second, doubling to at
  most 60 seconds while it keeps failing. RFC 0004 left restart out;
  long-lived sandboxes need it.
- `start`, `stop` and `restart` stream NDJSON events (`started`,
  `stopping`, `stopped`, `stdout`, `stderr`, `exit`, `error`, `complete`)
  for `duration` seconds after the action, read from the service log.
- `logs` returns the last `lines` lines, then follows for `duration`.
  Logs stay under `/run/shuru/log/<name>.log`, each line with a timestamp
  and a stream tag.

A snapshot restore brings the processes back as they were, so services
keep running across it. Services start fresh only when the guest boots.

### Checkpoints

A checkpoint is an RFC 0009 snapshot of the running sandbox, taken with
`resume: true`: the VM pauses while its changed pages are written, then
continues. Checkpoints are numbered `v0`, `v1` and so on per sandbox;
`v0` is taken right after the first boot. Each holds a `comment`, its
creation time, `source_id` (the checkpoint the sandbox last restored
from), and `health`, which is `ok` when its files pass the RFC 0009
checks and names the problem otherwise.

Create and restore answer with NDJSON events, `info` lines while they
run, then `complete` or `error`.

Restore stops the running VM without saving it, restores the checkpoint
into a new working disk and memory, rotates the guest identity and raises
the vmgenid interrupt (RFC 0009). Exec sessions running at the time of the
checkpoint come back as sessions; their old attachments are gone and
clients attach again.

### Policies

**Network.** netd enforces a rule list instead of the current allow list:

```json
{ "rules": [ { "include": "defaults" }, { "domain": "*.npmjs.org", "action": "allow" }, { "domain": "bad.example", "action": "deny" } ] }
```

- An empty rule list means no enforcement.
- `include: "defaults"` adds a list of common development domains
  (package registries, source hosts, container registries, model APIs),
  kept in the node configuration with a version.
- A domain matches exactly, as a subdomain wildcard (`*.example.com`) or
  as `*`. The most specific match decides: exact, then the longest
  wildcard, then `*`. No match denies.
- A denied name gets a DNS `REFUSED`. A connection to a raw IP is allowed
  only if an allowed name resolved to it. Private and local addresses are
  always refused (`public_only`, RFC 0008).
- A new policy reaches the running netd over its control socket and
  applies at once. netd remembers the name each connection was opened
  for and closes the ones the new policy denies.
- The agent writes the policy to `/.shuru/policy/network.json` in the
  guest for programs to read. Changing that file changes nothing.

**Privileges.** `profile` (`minimal`, `standard`, `privileged`, or empty
for unrestricted) sets the capability bounding set of every process the
guest agent starts for exec and services; `noNewPrivileges` sets
`PR_SET_NO_NEW_PRIVS` on them; `devices` lists the device nodes they may
open, enforced with a device cgroup inside the guest (the guest kernel
has `CGROUP_DEVICE` and `CGROUP_BPF`). The capabilities in each profile
are listed in the node configuration, so they can be tuned without a
guest change. The policy goes to
the guest agent over a new `POLICY_REQ` frame and applies to processes
started after it.

**Resources.** `memory.limit_mb` below the guest RAM size inflates the
balloon to the difference (RFC 0006). `autoscale` keeps free page
reporting on, which is the default anyway; `autoscale: false` turns it
off. Deleting the policy deflates the balloon.

## Implementation order

Each slice ends with the measurements below on a1.metal, kill-mode
seccomp, before the next starts.

1. **Guest sessions.** The session table, the new frames, buffered
   output with offsets, attach, list, kill through session cgroups. Tested
   through `shuru-node run` with a session client, including a dropped
   connection during output and an attach after exit.
2. **Exec, files and ports in the agent.** `shuru-node serve` with
   create, list, get and delete, HTTP and WebSocket exec, the filesystem,
   `proxy`, `ports/watch`. One guest connection per session, fanned out
   to its clients.
3. **Checkpoints.** Create with `resume: true`, list, get, restore in
   place.
4. **Policies and services.** The netd rule list with live reload,
   privileges and resources, managed services with `needs` and restart.

## What will be measured

| case | measured as | target |
|---|---|---|
| exec of `true` on a running sandbox, through the agent | request to response end | under 10 ms on the node |
| checkpoint of a running sandbox | guest paused time | under 50 ms for 100 MB changed |
| 50 MB of output to an attached client | bytes lost, throughput | none, at the vsock rate |
| attach after a dropped connection during 50 MB of output | bytes lost | none within the buffer |
| network policy change with open connections | time until denied ones close | under 1 s |

## Not in this RFC

- Placing sandboxes on hosts, moving them between hosts, and keeping
  them anywhere but the host that runs them.
- The SDKs.
- Multiplexing several operations over one WebSocket.
- An API from inside the guest (holding a sandbox awake, checkpoints from
  inside).
- Live migration between nodes.
