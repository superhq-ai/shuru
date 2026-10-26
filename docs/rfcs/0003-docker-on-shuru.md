# RFC 0003: Docker Engine inside a Shuru VM

- Status: Implemented
- Date: 2026-10-26
- Scope: kernel/shuru_defconfig, shuru-guest init, shuru-vm data dir, scripts, shuru-cli integration tests

## Summary

Run an unmodified Docker Engine inside a Shuru microVM and start one small,
maintained, non-root HTTP server image pinned by digest:

    nginxinc/nginx-unprivileged:1.31.7-alpine-slim
    @sha256:a8cb0f33d51fa107c96fd2bd3a71aaf564ec131e9d2bdf8700944f0ed8b170d7

Docker is not baked into the base rootfs. It is installed into a checkpoint
with the existing `shuru checkpoint create` flow, pinned to exact package
versions from the Docker apt repository, with the image pre-pulled. The guest
kernel gains the options a container runtime needs, and the guest init mounts
cgroup v2 and the tmpfs directories any container runtime assumes.

Nothing about the sandbox boundary changes. Containers sit inside the VM, the
VM sits inside the existing proxy, port forwarding, and mount policy. Docker
isolation inside the VM stays at its defaults: seccomp on, no privileged
containers, no host socket, no host networking.

## Motivation

Agents increasingly need to run a service exactly as it ships, which means a
container image. Today that requires either translating a Dockerfile into apt
commands by hand or giving the agent the host Docker socket, which is the
opposite of what Shuru exists for. A VM that can run dockerd gives the agent
the real tool while the host keeps the same boundary it has for everything
else Shuru runs.

## Non-goals

- Docker Desktop style integration (host CLI talking to a VM daemon). The
  daemon socket stays inside the guest.
- Rootless dockerd. The guest already runs everything as root inside a VM;
  rootless would add complexity without changing the host boundary.
- Swarm, IPVS, overlay networks, VXLAN. Not enabled in the kernel.
- GPU or device passthrough.
- Nested virtualization.

## Design

### Kernel

`kernel/shuru_defconfig` grows a "Container runtime support" section. The
required set is what moby's `contrib/check-config.sh` lists, minus IPVS:

- cgroup v2 controllers runc uses: cpu with bandwidth (`CFS_BANDWIDTH`),
  cpuset, io (`BLK_CGROUP`), memory and pids were already on.
- `CGROUP_BPF` and `BPF_SYSCALL`: on cgroup v2, device access rules are an
  eBPF program. Without it runc refuses to start a non-userns container.
  `BPF_UNPRIV_DEFAULT_OFF` keeps bpf() root-only inside the guest.
- `SECCOMP_FILTER`: the default Docker seccomp profile.
- `KEYS`: runc joins a session keyring per container.
- `VETH`, `BRIDGE_NETFILTER`, and the xt matches and targets dockerd installs
  through iptables-nft (`addrtype`, `conntrack`, `comment`, `mark`, DNAT,
  MASQUERADE, raw and mangle tables, plus the ip6 equivalents). `NFT_COMPAT`
  is what lets iptables-nft load xt extensions at all.
- `EXT4_FS_SECURITY` for overlay2 security xattrs.
- `IKCONFIG_PROC` so `check-config.sh` can audit the running guest.

The kernel version default moves from 6.12.17 to 6.12.114, the current 6.12
LTS release. Both build scripts honour `SHURU_DATA_DIR` so a build can land
somewhere other than the installed assets. `scripts/build-kernel-hf.sh` builds
the same defconfig on a Hugging Face CPU job for hosts without Docker or disk.

### Guest init

`mount_filesystems` in shuru-guest additionally mounts:

- `tmpfs` on `/run` (with `/run/lock`). Pid files and sockets no longer leak
  into checkpoints, which otherwise leaves dockerd refusing to start from a
  checkpoint because of a stale `/run/docker.pid`.
- `tmpfs` on `/dev/shm`, mode 1777.
- `cgroup2` on `/sys/fs/cgroup` with `nsdelegate,memory_recursiveprot`, and
  every available controller enabled in the root `cgroup.subtree_control`.

These are what a normal init does and they apply to every sandbox, not only
Docker ones. The behaviour change is that `/run` is no longer persisted into
checkpoints.

### Docker checkpoint

`scripts/prepare-docker-checkpoint.sh` creates a checkpoint (default name
`docker`) by running, inside a VM with a strict host allowlist:

1. Enable the trixie-security and trixie-updates archives (debootstrap only
   lists main) and `apt-get upgrade`.
2. Add the Docker apt repository with its signing key verified against a
   pinned SHA-256, then install `docker-ce`, `docker-ce-cli` and
   `containerd.io` at exact pinned versions.
3. Write `/etc/docker/daemon.json` with `no-new-privileges: true`. Everything
   else stays at Docker defaults (iptables on, userland proxy on, seccomp
   default profile, overlay2, json-file logs).
4. Declare dockerd as a guest service in `/etc/shuru/services/docker.json`
   (RFC 0004), with `docker info` as the readiness check and a 30 second
   stop timeout. The guest init starts it at boot and stops it before power
   off. An earlier revision of this RFC used a `with-dockerd` wrapper
   script for the same job; RFC 0004 replaced it.
5. Boot from that intermediate checkpoint and `docker pull` the pinned image
   so the final checkpoint runs it without network.

The checkpoint is the unit users reuse: `shuru run --from docker -p 8080:8080
-- docker run ...`.

### Data dir isolation

`default_data_dir()` in shuru-vm reads `SHURU_DATA_DIR` before falling back
to `~/.local/share/shuru`. This is what lets the whole verification run with
its own kernel, initramfs, checkpoints and instances without touching the
installed assets, and what the integration tests use.

### How traffic flows

- Host `127.0.0.1:P` to container: existing vsock forward to guest
  `127.0.0.1:P`, answered by docker-proxy, which relays to the container.
- Container egress: container to docker0, MASQUERADE in the guest, eth0, the
  smoltcp proxy on the host. The proxy sees ordinary guest TCP and DNS, so the
  domain allowlist and DNS pinning apply unchanged. Without `--allow-net`
  there is no eth0 and containers have no egress at all.
- Container DNS: the default bridge copies the guest `/etc/resolv.conf`,
  which points at 10.0.0.1, the proxy. A container that sets `--dns 8.8.8.8`
  sends UDP to an address the proxy does not serve, so it gets nothing.

## Security

What stays the same: the host boundary. Containers run as processes of a
guest kernel inside the VM. The only paths out are the ones Shuru already
exposes to any guest process: the proxy (policy applied), explicit port
forwards (host dials in, never out), and explicit mounts (read-only overlay
by default).

What this adds inside the guest: bpf() and keyctl() syscalls, seccomp, and
netfilter surface, all reachable only by root in the guest. The container to
guest-root boundary is Docker's own and is left at its defaults. The guest
root to host boundary is unchanged.

Residual risks and limits:

- A container escape to guest root gains what any sandbox command already
  has. That is the same as today.
- The host disk only resets on exit. A long lived VM running a malicious
  container has the same egress policy as a malicious `curl` in the guest.
- `--privileged`, `--cap-add`, and `-v /var/run/docker.sock` remain possible
  for the guest user, as in any Docker install. They do not cross the VM
  boundary, but the test suite never uses them and the docs say not to.
- Vulnerability posture is a snapshot. The RFC appendix records what was
  scanned and what was found on the date of the run.

## Testing

`crates/shuru-cli/tests/docker.rs`, `#[ignore]` like the mount tests, boots
real VMs from the Docker checkpoint and checks:

1. dockerd starts, reports cgroup v2, seccomp, overlay2; the pinned image runs
   and `id -u` inside it is 101.
2. HTTP through `-p`: a page served by the container is fetched from the host.
3. Egress is blocked without `--allow-net`.
4. With `--allow-net --allow-host example.com`, the container fetches
   example.com but a non-listed domain, a raw IP, and a custom DNS server all
   fail.
5. The container cannot reach a listener on the host loopback.
6. A container bind-mounting a read-only host mount cannot write to the host.
7. `--memory`, `--cpus`, `--pids-limit` are reflected in the container cgroup
   and a memory hog is OOM-killed.
8. A named volume survives container stop/start and a dockerd restart, and
   survives a clean VM stop/start via checkpoint.

Evidence from the real run is in the appendix.

## Rollout

The base OS image for the next release carries the new kernel and init, so
the checkpoint script works with stock assets. Until then, `SHURU_KERNEL` and
`SHURU_INITRD` point at a locally built pair. No behaviour changes for users
who never create the checkpoint, apart from `/run` being tmpfs.

## Appendix A: Evidence

Recorded on 2026-10-26 on a MacBook with an Apple M-series CPU, macOS 14.4.1,
8 GB RAM. All VM output below comes from real boots of the worktree CLI with
`SHURU_DATA_DIR=~/.local/share/shuru-docker-dev`, which holds its own kernel,
initramfs and checkpoint. The installed `~/.local/share/shuru` assets were not
modified.

### Kernel

Built with `scripts/build-kernel-hf.sh` on a Hugging Face `cpu-upgrade` job
(job `6ade955e1e112550ce7f01dc`, `hf jobs ssh --dry-run` printed the ssh
target). Source tarball verified against kernel.org `sha256sums.asc`.

    Image-6.12.114  sha256 bd0550e93ab88a9977af6c5f4ba231aeeff143cd874980a99b247ca0e2a0e633  9320960 bytes

Guest boot on the new kernel:

    6.12.114
    tmpfs on /run type tmpfs (rw,relatime,size=65536k,mode=755)
    tmpfs on /dev/shm type tmpfs (rw,relatime,size=262144k)
    cgroup2 on /sys/fs/cgroup type cgroup2 (rw,relatime,nsdelegate,memory_recursiveprot)
    subtree_control: cpuset cpu io memory pids

moby `contrib/check-config.sh` run inside the guest against `/proc/config.gz`:
every "Generally Necessary" item enabled except `NETFILTER_XT_MATCH_IPVS`
(swarm routing mesh only). Optional items left out are swarm overlay, IPVS,
SELinux, AppArmor, btrfs, macvlan, ipvlan, ftp conntrack helpers.

### Checkpoint

`scripts/prepare-docker-checkpoint.sh` ran inside a VM allowed to reach only
deb.debian.org, security.debian.org, download.docker.com, `*.docker.io`,
`*.docker.com`, `*.cloudflarestorage.com`:

    Docker version 29.9.2, build a82e528
    containerd containerd v2.4.2 b7a25261e029d68978bbf290df0a62fc819b3c51
    runc version 1.5.2
    Digest: sha256:a8cb0f33d51fa107c96fd2bd3a71aaf564ec131e9d2bdf8700944f0ed8b170d7
    nginxinc/nginx-unprivileged   <none>   sha256:a8cb0f33...   a8cb0f33d51f   6 days ago   30.3MB

Image config read from the registry before pulling: `User: 101`,
`ExposedPorts: 8080/tcp`, arm64 manifest
`sha256:d99276c6bdeded8a96168938a0f1dfa158c492410019d0d3766884d0919d1b45`.

### Runtime checks from the integration suite

Final run with the `with-dockerd` wrapper, each test booting one VM from
the rebuilt checkpoint:

    test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 125.51s

Rerun after RFC 0004 moved dockerd to a guest service, same nine tests:

    test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 78.78s

Raw output of the rerun is in `docs/rfcs/0003-docker-on-shuru-evidence.log`.
Excerpts, identical between the two runs:

    cgroupfs cgroup-v2 storage=overlayfs kernel=6.12.114 security=[name=seccomp,profile=builtin name=cgroupns name=no-new-privileges]
    docker=29.9.2 containerd=v2.4.2 runc=1.5.2
    digest=nginxinc/nginx-unprivileged@sha256:a8cb0f33... user=101
    uid=101
    nginx-master-Uid: 101 101 101 101
    privileged=false
    seccomp=[]
    no-new-privs=NoNewPrivs: 1
    seccomp-mode=Seccomp: 2

HTTP through `-p`: a page written in the guest and served by the container
came back on the host port with `HTTP/1.1 200` and `Server: nginx`; the VM
shut down after the container logged the probe request.

Egress without `--allow-net`:

    guest-eth0=Device "eth0" does not exist
    EGRESS_BLOCKED
    IP_BLOCKED

Egress with `--allow-net --allow-host example.com`:

    <title>Example Domain</title>
    ALLOWED_OK
    DOMAIN_BLOCKED      (www.iana.org, DNS refused by the proxy)
    IP_BLOCKED          (1.1.1.1, not DNS-pinned)
    DNS_BLOCKED         (--dns 8.8.8.8, UDP not relayed)

Host loopback listener on a random port, with and without `--allow-net`:
`GATEWAY_BLOCKED`, `HOSTNAME_BLOCKED`, and the host listener accepted no
connection.

Read-only host mount bind-mounted into a container: the container read the
host file, wrote `escaped.txt` into the guest overlay, and the file never
appeared on the host. `-v /:/vmroot` shows no `/Users`, the VM root only.

Resource limits:

    memory.max=67108864
    cpu.max=50000 100000
    pids.max=20
    oom-exit=137          (32 MB limit, 200 MB allocation)
    (fork beyond --pids-limit 5 fails, "forked-all" never printed)

Named volume: data written through `docker exec` survived `docker stop` and
`docker start`, survived stopping and restarting dockerd, and survived a
clean VM stop and start through `shuru checkpoint create --from docker`.

### Vulnerability posture

Image, Trivy 0.76 with the 2026-10-25 database, `linux/arm64`:

    alpine 3.24.2   Total: 3 (UNKNOWN: 2, MEDIUM: 1, HIGH: 0, CRITICAL: 0)
    musl 1.2.6-r2 -> fixed in 1.2.6-r3 (CVE-2026-107891, no severity yet)
    zlib 1.3.2-r0 -> fixed in 1.3.2-r1 (CVE-2026-85091, medium, heap overflow)

Both fixes landed in Alpine after the 2026-10-19 image build. There is no
newer `nginx-unprivileged` build yet. Residual: a medium zlib issue in a
process that runs as uid 101 under seccomp inside a VM. Re-run
`prepare-docker-checkpoint.sh` with a newer digest when the image rebuilds.

Guest Debian layer, `debsecan --suite trixie --only-fixed` inside the VM:
36 fixable package entries (openssl, curl, expat, pcre2, brotli, xz) before
enabling the security archive, 0 after. 294 entries remain open with no
Debian fix available; that is the normal trixie backlog of low and
unimportant issues and is not actionable from this repo.

Runtime components, GitHub advisory database queried for the exact
versions, with a control query against runc 1.1.11 returning 5 advisories:

    containerd/containerd/v2 2.4.2   0 advisories   (latest release, 2026-10-13)
    moby 29.9.2                      0 advisories   (latest release, 2026-10-20)
    opencontainers/runc 1.5.2        0 advisories   (latest release, 2026-09-25)

Kernel 6.12.114 is the current 6.12 LTS release at build time.

Not scanned: the proxy and CLI Rust dependencies (unchanged by this RFC) and
the kernel against per-CVE data (LTS point release is the mitigation).

### Reproduction

    # kernel (HF job) and initramfs into an isolated data dir
    export SHURU_DATA_DIR=~/.local/share/shuru-docker-dev
    cargo build -p shuru-guest --target aarch64-unknown-linux-musl --release
    ./scripts/build-kernel-hf.sh               # or ./scripts/build-kernel.sh with Docker
    ln -s ~/.local/share/shuru/rootfs.ext4 $SHURU_DATA_DIR/rootfs.ext4
    ./scripts/prepare-rootfs.sh                # only builds the initramfs here

    # CLI: sign a copy, cargo test relinks target/debug/shuru
    cargo build -p shuru-cli
    cp target/debug/shuru /tmp/shuru-signed
    codesign --entitlements shuru.entitlements --force -s - /tmp/shuru-signed

    export SHURU_KERNEL=$SHURU_DATA_DIR/Image SHURU_INITRD=$SHURU_DATA_DIR/initramfs.cpio.gz
    export SHURU_ROOTFS=~/.local/share/shuru/rootfs.ext4
    SHURU_BIN=/tmp/shuru-signed ./scripts/prepare-docker-checkpoint.sh
    SHURU_BIN=/tmp/shuru-signed SHURU_TEST_NET=1 \
      cargo test -p shuru-cli --test docker -- --ignored --test-threads=1
