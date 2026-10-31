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
DELETE /v1/sandboxes/{name}           ?if_stopped=
POST   /v1/sandboxes/{name}/stop
POST   /v1/sandboxes/{name}/park         ?if_stopped=
GET    /v1/sandboxes/{name}/usage
PUT    /v1/sandboxes/{name}/config       { "env": {...}, "secrets": {...} }
POST   /v1/sandboxes/{name}/checkpoint
POST   /v1/sandboxes/{name}/restore      { "disk": "<state_dir>/import/<file>" }
```

`stop` is another addition: it shuts the guest down, so its disk is
synced, and stops the VM, leaving the sandbox cold. It answers 204, and
does nothing to a sandbox that is not running. A host stops a sandbox it
no longer wants to run, such as one whose owner has run out of credit.

`park` is another: it stops the sandbox as `stop` does, then, under the
lock that boots its VM, makes a reflink copy of its disk at
`<state_dir>/parked/<name>.disk`, written under another name, synced and
renamed over any copy there. It answers 200 with `disk`, the path of the
copy, `boot`, the boot of the last run (0 if it has not run since it was
made or imported), and `size`, the size of the disk in bytes. The copy
belongs to the caller, which tells by `boot` whether the disk changed
since a copy it stored before.

A host that saw a sandbox stopped and wants to park or delete it only
while it stays stopped passes `if_stopped=true`. Under the same lock, a
sandbox whose VM runs by then, because a request booted it, is left
running and the call answers 409, so the request that woke it is not cut
off. Looking first and acting after would leave that request a window.

Create takes two more fields. `disk` names a file under
`<state_dir>/import/` to make the sandbox from, in place of a copy of
the base disk; it is renamed into the sandbox directory, so it must be on
the same filesystem, and a path that leads anywhere else, links
followed, is refused with 400. `start: false` leaves the new sandbox
cold; the default is `true`. A name that exists answers 409, and a create
that fails leaves the file where it was.

With `--idle-park <seconds>` (default 30, 0 turns it off) the agent stops
a running sandbox, as `stop` does, once nothing has used it for that
long. Requests that use the guest (exec, attach, kill, the filesystem,
proxy and the ports watch) count while they are in flight, upgraded
connections while they are open, and exec sessions while their process
runs. The agent asks the guest for its sessions before it stops a
sandbox, and every 5 seconds while one runs, so the clock restarts from
the last time one was seen. What a host polls or manages (get, usage,
the session list, stop, park, delete, config, checkpoint and restore)
does not count, nor do managed services.

`usage` is an addition to the Sprites API, for whatever meters the host:
`running`, `boot`, a number for the run of the VM that grows with every
start, and the CPU time (`cpu_usec`) and memory (`memory_bytes`) the
cgroup of that run shows. Once the VM stops, `cpu_usec` is what its last
run used in all and `ended` when it stopped, so a meter misses nothing at
the end of a run; `boot` and `ended` are in milliseconds, and `boot` is
null for a sandbox that has not run since serve started.

Each sandbox keeps a directory under the state directory
(`/var/lib/shuru/sandboxes/<id>/`) with `sandbox.json` (id, name, size,
and the boot of its last run, written before the VM runs so a restart
still knows it), `config.json` (its env and secrets, below) and its
working disk, a reflink copy of the base disk the agent is given.
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

### Env and secrets

A sandbox can be given env, plain values handed to every exec as they
are, and secrets, which the guest only sees as placeholders. Create takes
both, and `PUT config` replaces both:

```json
{
  "env": { "LOG_LEVEL": "debug" },
  "secrets": { "OPENAI_API_KEY": { "value": "...", "hosts": ["api.openai.com"] } }
}
```

Names match `[A-Z_][A-Z0-9_]{0,127}`. A value is at most 32 KiB, without
NUL. A secret has 1 to 20 hosts, each a domain of two labels or more, or
`*.` and one, compared without case. Names, values and hosts come to at
most 256 KiB in all, so the env still fits one exec frame. Anything else
is refused with 400, and so are secrets on an agent run without
`--allow-net`. `PUT config` answers 204, or 404 for a sandbox that does
not exist. Get and list show `env` and `secrets` as sorted lists of names.
No answer, error or log line carries a value.

The agent keeps both in `config.json` in the sandbox directory, mode
0600, written under another name, synced and renamed, as `sandbox.json`
is. It is the only place a secret value is written; a value never reaches
the guest disk.

At each boot netd gets the secrets of the sandbox with their values,
besides the network policy of the agent, as `shuru-node run --secret`
gives them from its environment. The proxy makes a placeholder for each
and swaps it for the value on requests to its hosts only. A sandbox with
secrets has the session CA installed in the guest after boot, before it
counts as running, since the proxy signs for those hosts with it.

Every exec, HTTP or WebSocket, gets its env in this order, each over the
one before: the sandbox env, the placeholders under the secret names, the
env of the request. Env changes reach the next exec. Secret changes reach
the next boot, since netd holds the secrets for the run; until then the
running guest keeps the placeholders it booted with.

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

`PORT_EVENT` tells attached clients of a port opened or closed by a
process of the session, as the JSON the API sends.

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
- `fs/stat` is an addition to the Sprites API: one entry for any path,
  shaped as in `fs/list`. The Sprites SDK stats a path by listing it and
  taking the first entry, which for a directory is its first child;
  `fs/list` stays as Sprites has it, so `readdir` stays right;
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

`proxy` opens a TCP connection from inside the guest to the host and
port in its first message, through the existing `FWD_REQ` path, answers
`{"status":"connected","target":"host:port"}` or a `status` of `error`
with a `message`, then carries raw bytes. `host` is usually `localhost`;
any other name resolves inside the guest, and its egress goes through
netd like any other.

`ports/watch` asks the guest for a `PORTS_WATCH_REQ` stream. The guest
reads `/proc/net/tcp` and `/proc/net/tcp6` every 500 ms, while a port
watch or an attached exec session wants them, for listening sockets,
maps each socket inode to a process through `/proc/*/fd`, sends the full
list first and then `port_opened` and `port_closed` with `address`,
`port` and `pid`. The same scan sends `PORT_EVENT` to attached exec
sessions whose cgroup holds the process that owns the new socket.

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
keep running across it. Services start fresh only when the guest boots,
as it does after a checkpoint restore.

### Checkpoints

A checkpoint is a copy of the disk only, so a restore restarts the
sandbox and running processes do not come back. The agent gives a host
two pieces; where checkpoints are kept, and how they are numbered and
listed, is up to the host.

```
POST /v1/sandboxes/{name}/checkpoint   200 { "disk": "<state_dir>/checkpoints/<name>.<random>.disk", "size": bytes }
POST /v1/sandboxes/{name}/restore      { "disk": "<state_dir>/import/<file>" }   200 the sandbox
```

`checkpoint` holds the lock that boots and stops the VM, so neither runs
meanwhile. A running sandbox is asked to `sync`, then its disk gets a
reflink copy (`FICLONE`) while the VM runs on. The copy is the disk as a
crash at that instant would leave it, which the ext4 journal recovers
from. A disk in use is only copied by sharing its blocks, since a plain
copy could mix blocks from before and after a write; on a filesystem
without reflinks only a stopped sandbox can be copied, which goes as
`park` copies. The copy belongs to the caller, which deletes it.
`checkpoints/` is made with mode 0700, and what an earlier run left in it
is removed when the agent starts.

`restore` takes a file under `import/` by the rules create has, else 400.
Under the same lock it stops the VM as `stop` does, moves `last_boot` to
a new boot and saves it, then renames the file over the sandbox disk and
syncs the directory. The boot moves first, so a crash between the two
costs a host at most a copy it did not need, never a restored disk under
the boot before. The sandbox is left cold and boots on the next request;
the new boot tells a host that the next `park` copies a disk it has not
stored.

Neither counts as activity, as `stop` and `park` do not.

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
3. **Checkpoints.** Disk copies of a running or stopped sandbox, and
   restore in place.
4. **Policies and services.** The netd rule list with live reload,
   privileges and resources, managed services with `needs` and restart.

## What will be measured

| case | measured as | target |
|---|---|---|
| exec of `true` on a running sandbox, through the agent | request to response end | under 10 ms on the node |
| checkpoint of a running sandbox | time the VM lock is held | under 50 ms for 100 MB written since the last sync |
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
