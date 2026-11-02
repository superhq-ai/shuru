# RFC 0004: Guest services

- Status: Implemented
- Date: 2026-10-26
- Scope: shuru-guest, shuru-proto, shuru-vm, shuru-cli, Docker checkpoint script
- Depends on: RFC 0003

## Summary

Let an image declare long running processes that the guest starts at boot,
waits on before running the user command, and stops cleanly before the VM
goes away. A service is one JSON file under `/etc/shuru/services/` in the
rootfs or checkpoint. Init owns the lifecycle. The host asks the guest to
shut down over vsock before it stops the VM, and the guest answers only
after every service has exited and the disk is synced.

With this in place `shuru run --from docker -- docker run ...` just works.
The `with-dockerd` wrapper from RFC 0003 goes away.

## Motivation

The guest has no init system. PID 1 is shuru-init, which mounts
filesystems, brings up networking and serves the vsock control protocol.
Nothing starts daemons and nothing stops them. RFC 0003 worked around this
with a shell wrapper that started dockerd around each command, which has
four problems:

- Every daemon needs its own copy of the wrapper and users must remember to
  type it. `shuru run --from docker -- docker ps` fails with a socket error.
- Shutdown runs only on the happy path. An interrupted `shuru run`, a stop
  from the SDK, or a host side error leaves the daemon without SIGTERM and
  the disk without a sync. For a checkpoint build that means a cloned disk
  with half written state.
- The host already hard stops the VM. There is no cooperative shutdown at
  all, so even the `sync` before a checkpoint clone is a separate exec that
  can be skipped by any error between the command and the stop.
- Startup cost is paid per command instead of per boot.

Other sandboxes solve this at the platform level. E2B templates take a
start command and a ready command, and the ready command is retried until
it exits 0 before the sandbox is handed over. Sprites declare services that
start on boot and expose a lifecycle state. Shuru should offer the same
contract: declare once in the image, the runtime handles start, readiness
and stop.

## Non-goals

- Dependencies between services. All services start in parallel.
- Per service users or cgroups. Services run as root in the root cgroup,
  the same as the user command today.
- Services that depend on host mounts. Mounts arrive with the first
  command, after the services have started.
- Changing the on disk format of checkpoints.

## Design

### Service definition

A service is a JSON file at `/etc/shuru/services/<name>.json`. The name is
the file stem and must match `[a-z0-9][a-z0-9-]*`.

```json
{
  "exec": ["dockerd"],
  "ready": ["docker", "info"],
  "ready_timeout_secs": 60,
  "stop_timeout_secs": 30,
  "env": {"DOCKER_RAMDISK": "1"},
  "cwd": "/"
}
```

| field | required | default | meaning |
|---|---|---|---|
| `exec` | yes | | argv of the process to run. Resolved on the init PATH. |
| `ready` | no | none | argv retried every 250 ms until it exits 0. Without it the service is ready once it has run for 500 ms. |
| `ready_timeout_secs` | no | 30 | give up on `ready` after this long. |
| `stop_timeout_secs` | no | 15 | time between SIGTERM and SIGKILL at shutdown. |
| `env` | no | `{}` | extra environment for `exec` and `ready`, over the env from the host. |
| `cwd` | no | `/` | working directory for `exec`. |
| `restart` | no | `"no"` | `"always"` starts the service again after it exits. |

Files that fail to parse are reported on the console and skipped. They do
not block the boot or the user command, because a broken file in an image
should be debuggable with `shuru run`.

### Lifecycle in the guest

After networking is up, init creates the vsock listeners and waits for the
env of the services from the host, then reads the services directory and
spawns every service with it. Each gets its own process group and its
stdout and stderr appended to `/run/shuru/log/<name>.log`. The log lives
on tmpfs so it never lands in a checkpoint.

### Env from the host

A service gets the same env as a command: the env the host gives the
sandbox and the placeholders of its secrets, never a secret value. The
host sends it once, in `SERVICE_ENV_REQ`, on its first connection after
the boot, and the guest answers `SERVICE_ENV_RESP` once the services are
spawned. The env of the service file is over it, and the env of init is
under it.

- The first env starts the services. Any later one is refused with an
  ERROR, so the env of a service, and of each restart of it, stays the
  one it booted with. An env changed later reaches services at the next
  boot.
- Commands wait for the services to start, like they wait for them to be
  ready, so the first command after a boot never runs before them.
- A host that sends no env, an older one or a raw console boot, gets the
  services started without it after 5 s. An image without service files
  starts at once and accepts a later env as a no op.
- The host connects within a few milliseconds of the guest listening, and
  the env is one frame, so services start about that much later than
  before.

Each service has one of five states: starting, ready, restarting, failed or stopped. A
thread per service runs the ready check until it passes or the timeout is
hit. The zombie reaper already in init records the exit of any service pid,
so a daemon that dies at any point moves to failed with its exit status.
A service without a ready check is ready once it has run 500 ms, so one
that exits at once fails the first command instead of letting it run, and
init reaps any exited service itself before it decides whether a command
runs.

Before spawning a user command, in both the piped and the tty exec paths,
init waits until no service is still starting. If every service is ready
the command runs as before. If any service is failed, init answers the exec
with an ERROR frame that names the service, the reason, and the last lines
of its log, and does not run the command. File, mount, watch and port
forward requests are never gated, so the log of a failed service can always
be read from the host.

### Restarting

A service with `"restart": "always"` is started again after it exits, or
after a spawn that fails. The first wait is 1 s and each next one doubles,
up to 60 s. A run that lasted 60 s or more was healthy, so the wait after
it is 1 s again. Each run gets a new ready check, and a ready check that
times out leaves the process running: only an exit restarts it.

Such a service holds commands only until it is first ready, or until its
`ready_timeout_secs` has passed since boot, and never fails them. After
that, a crash or a restart in progress is in its log, not in the exec of
an unrelated command. A service without `restart` keeps the rule above:
it holds commands while starting and fails every command once it has
failed.

At shutdown a service waiting to restart has no process, and the restart
does not happen. The spawn of a restart takes the same lock as the start
of a shutdown, so a process started just before it is stopped like the
others.

### Logs

Init checks every 5 s. A log over 1 MiB is copied to `<name>.log.1`,
replacing the older copy, and emptied. Services write with `O_APPEND`, so
they carry on at the start of the emptied file. Lines written between the
copy and the truncate are lost. Sixteen services with both files at the cap
use half of `/run`.

### Shutdown handshake

Two new frames on the control connection:

- `SHUTDOWN_REQ` (0x60), empty payload.
- `SHUTDOWN_RESP` (0x61), JSON `ShutdownResponse { stopped: [name], killed: [name] }`.

On `SHUTDOWN_REQ` the guest sends SIGTERM to every running service, waits
up to each one's `stop_timeout_secs` for it to exit, sends SIGKILL to the
process group of any that remain, calls `sync`, and then replies. Services
are stopped in parallel so the wall clock cost is the slowest service, not
the sum. Once a shutdown has started, new exec requests are refused.

On the host, `Sandbox::stop()` becomes a graceful stop. It opens a control
connection, sends `SHUTDOWN_REQ`, waits for the reply with a read timeout,
and then performs the hard stop it did before. Anything that goes wrong on
the way, including an old guest that does not know the frame, falls through
to the hard stop so a stop can never hang. `Sandbox::kill()` is the old
behaviour for callers that want it.

Because the handshake includes `sync`, the `sync_before_stop` flag in the
CLI and the separate `sync` exec go away. Checkpoint creation and plain runs
share one path, and the SDK gets the same shutdown through its existing
`stop()` and `Drop`.

### Docker checkpoint

The Docker recipe in `superhq-ai/recipes` (RFC 0005) installs Docker and
declares `/etc/shuru/services/docker.json` in its first step. Its second
step boots from that result, where the service model has already started
dockerd, and runs `docker pull` for the pinned image. The build itself is
the first consumer of the feature.

The `daemon.json` from RFC 0003 is unchanged. The `with-dockerd` helper is
no longer installed.

### Traffic and isolation

Nothing in this RFC changes networking or mounts. Services run inside the
VM with the same proxy, allowlist and mount rules as the user command.

## Security

- Services come from the image. Anything that can write to
  `/etc/shuru/services/` in a checkpoint could already run arbitrary code
  in that checkpoint, so this adds no new trust boundary.
- Service logs are on tmpfs under `/run`, which is size capped at 64 MB,
  and each is kept near 1 MiB with one older copy. A service that writes
  faster than the check runs can still fill `/run` and get write errors,
  but never the disk or the checkpoint.
- Shutdown is bounded. The guest waits at most `stop_timeout_secs` per
  service and the host waits at most 90 seconds for the reply before it
  hard stops. A hung service cannot hold a VM open.
- Readiness failure fails closed for exec. A command never runs against a
  half started service, unless that service asks to be restarted, which
  says the image accepts commands running while it comes back.
- A service that exits at once and restarts forever costs one spawn a
  minute once its wait has reached the cap.
- The env from the host adds nothing a command does not already get:
  plain env and secret placeholders, never a secret value. The host sends
  it and reads one fixed answer with a 10 s timeout, so a guest cannot
  hold the boot of the host. The guest takes it once, before the services
  start, so a later connection cannot change what a restart inherits.
- The env of a running service is in its memory, so a memory snapshot
  taken while it runs carries it. Placeholders fail closed after a
  restore, since each run of the network gets new ones, but a credential
  put in plain env instead of a secret travels with the snapshot. Keep
  credentials in secrets.

Residual risks:

- SIGKILL after the stop timeout can still leave a service with unflushed
  state. The sync that follows only covers what reached the page cache.
- The host hard stop on timeout is the same crash semantics as before. The
  improvement is that it is now the exception rather than the rule.

## Testing

`crates/shuru-cli/tests/services.rs` builds throwaway checkpoints from tiny
shell services and checks, against a real VM:

- a service with a delayed ready check gates the user command, a second
  service without a ready check is up immediately, and both are stopped
  at shutdown with their logs under `/run/shuru/log`
- a service that exits before its ready check passes fails the exec with
  an error naming it and the tail of its log, and the command never runs
- SIGTERM reaches a service at shutdown and the disk is synced before the
  checkpoint is cloned, proven by a marker written from the TERM trap and
  read back from a derived checkpoint
- a service that ignores SIGTERM is killed after its stop timeout and the
  CLI reports it

`crates/shuru-cli/tests/docker.rs` drops every `with-dockerd` and runs
plain `docker` commands. The nine RFC 0003 tests pass unchanged otherwise.

## Rollout

The guest and host change together in one release. The env from the
host came with protocol version 6 of the connection hello, so a host and
guest that disagree on it refuse to talk instead of waiting out timeouts.
An old host against a new guest never sends `SHUTDOWN_REQ` and behaves as
today. An old guest
skips unknown frames without answering, so a new host against it waits out
the shutdown timeout and then hard stops. Mixed versions degrade to the old
behaviour at the cost of a slow stop, and the asset version check keeps the
two in step for stock installs.

## Appendix A: Evidence

Captured on 2026-10-26 against real VMs on an M-series Mac with 8 GB RAM,
kernel 6.12.114 (sha256 `bd0550e9...`), isolated data dir, codesigned debug
CLI. Full `--nocapture` output is in
`docs/rfcs/0004-guest-services-evidence.log`; the Docker suite rerun is in
`docs/rfcs/0003-docker-on-shuru-evidence.log`.

Service suite, four tests, one run:

```
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.49s
```

Lines that show each behaviour, from that log:

```
shuru-guest: service slow ready
GATED_OK
fast-log=fast-started
slow-alive=2
shuru-guest: services stopped in 0.1s (stopped: ["fast", "slow"], killed: [])
shuru: stopped services: fast, slow

guest error: service bad exited with status 3
--- /run/shuru/log/bad.log ---
booting bad service
giving up

shuru-guest: services stopped in 1.0s (stopped: ["trap"], killed: [])
shuru: stopped services: trap
got-term

shuru: services ignored SIGTERM and were killed: stubborn
```

Docker checkpoint build from the docker recipe (RFC 0005). The service
file is written in step one, so dockerd is already up when step two pulls:

```
shuru: building recipe docker from <recipes-checkout>/docker@dcbbf2c (2 steps)
shuru: step 1/2: install.sh
shuru: checkpoint 'docker-step1' saved
shuru: step 2/2: pull.sh
Digest: sha256:a8cb0f33d51fa107c96fd2bd3a71aaf564ec131e9d2bdf8700944f0ed8b170d7
shuru: checkpoint 'docker' saved
shuru: checkpoint 'docker-step1' deleted
real    2m3.869s
```

RFC 0003 Docker suite, nine tests, against that checkpoint with no
`with-dockerd` anywhere:

```
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 78.79s
```

The same nine tests took 125.51s with the wrapper, because every script
paid a daemon start and stop. The checkpoint persistence test additionally
asserts `shuru: stopped services: docker` on the create side, which is the
handshake running before the clone.
