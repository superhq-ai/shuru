# RFC 0015: Run as a user

- Status: Draft
- Date: 2026-11-05
- Scope: shuru-proto, shuru-guest, shuru-vm, shuru-cli, shuru-sdk, the TypeScript SDK, shuru-node (serve)
- Depends on: RFC 0004 (guest services), RFC 0012 (guest API)

## Summary

An exec, a guest service and a sandbox can name the user a process runs
as. Until now every process the guest started ran as root. With this, a
desktop and an agent that share files run as one user, and a caller drops
root without wrapping its command in `runuser`.

## Shape

One type, with the type the protocol already has for a user, `Owner` (an
id, or a name looked up in the guest):

```rust
/// Who a process runs as. Root when neither is set.
pub struct RunAs {
    /// The user, with the groups and the home of its passwd entry.
    pub user: Option<Owner>,
    /// The one group to run with, in place of the groups of the user.
    pub group: Option<Owner>,
}
```

It is flat in the JSON wherever it goes, as `user` and `group`:

| where | how |
|---|---|
| `ExecRequest` | `{"argv": ["id"], "user": "desktop"}` |
| a service file (RFC 0004) | `"user": "desktop", "group": 44` |
| `shuru run` | `--user desktop` or `--user desktop:video`, or `"user"` in `shuru.json` |
| `shuru run --stdio` | `user` and `group` on `exec` and `spawn`, over the one of `--user` |
| the Rust SDK | `SandboxConfig::run_as` |
| the TypeScript SDK | `user` on the start options, `user` and `group` on `exec` and `spawn` |
| serve | `user` and `group` in the sandbox config (create and `PUT config`), and as exec query parameters over it |

A request that names neither takes the default of its sandbox; one that
names `root` runs as root. What shuru itself runs (the proxy CA install,
a recipe build) runs as root whatever the default is, so `Sandbox::exec`
stays root and the calls that run what a user asked for take a `RunAs`.

A service runs as the user of its file and never takes the default of
the sandbox. The image declares its services and knows what they need: a
Docker daemon must be root, whoever the commands run as.

## In the guest

The account is looked up in `/etc/passwd` and `/etc/group` before the
fork, on each start, so a user made after boot is found by the next
command or the next restart of a service. A name matches a name first, as
chown does; a number given as a name, or an id, then matches an id. A
user or a group with no entry is an error that names it and nothing runs,
for a name and an id alike: the home, the shell and the groups all come
from the entry.

Between the fork and the exec, after the cgroup is joined: `setgroups`
(the user and the groups that list it, or the one `group`), `setgid`,
`setuid`, then the working directory, entered as the user. Then:

- `HOME`, `USER`, `LOGNAME` and `SHELL` from the entry, unless the
  request sets them;
- the working directory is the home when the request names none, and
  the root when the home is missing, as `/nonexistent` of `nobody`; a
  directory the request names that cannot be entered is an error;
- a terminal belongs to the user, in the `tty` group with mode 0620, as
  login leaves it, so `tty`, `mesg` and programs that check it work.

## Old guests

An old guest would ignore the fields and run the command as root, more
than was asked, and by the time it answers the command has run. The
fields come with protocol version 7: a host and a guest that disagree on
the version refuse each other at the connection hello, as since RFC 0012,
so no request reaches a guest that would ignore them. A memory snapshot
of an older guest is refused on restore for the same reason, as for every
protocol bump; a disk checkpoint boots the guest of the host that runs it.

## Not changed

Who may exec at all. A process that runs as a user with sudo can become
root; the boundary is the VM, as before. This is about which user owns
what a process writes and which files it can open.

## Tests

- An exec as a name runs with its uid, its groups and its home; `id` and
  `pwd` say so, and a user without a home starts in the root.
- `group` replaces the groups of the user.
- An unknown user fails the exec with an error that names it, and nothing
  runs.
- A terminal exec as a user has its terminal owned by that user.
- A service file takes `user` and `group`; a bad value fails the file.
- `RunAs` parses `user` and `user:group`, is flat in an exec request, is
  left out of it for root, and falls back to the default only when it
  names neither.
- The user of a serve sandbox config is kept, and an old config reads as
  root.
