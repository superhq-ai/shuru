//! `shuru-node run` boots one jailed VM, runs a command in it and stops it,
//! the way a node runs each sandbox. `shuru-node restore` does the same
//! from a snapshot `run --snapshot-to` took.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use shuru_node::cgroup::{Limits, Sandbox};
use shuru_node::control::{self, Boot, Disk, NetdConfig, Request, Response, Secret, VmmConfig};
use shuru_node::firewall::{self, NETD_UID_BASE};
use shuru_node::jail::{self, Handoff, Jail};
use shuru_proto::hello::{Identity, Restore, PROTOCOL_VERSION};
use shuru_proto::{frame, ExecRequest};

const RUN_DIR: &str = "/run/shuru";
const VMM_UID_BASE: u32 = 100_000;
const VSOCK_CONNECTIONS: usize = 128;
const VMM_NOFILE: u64 = 512;
// 256 proxied connections, each with a guest and an upstream side.
const NETD_NOFILE: u64 = 1024;
const NPROC: u64 = 64;
const SANDBOX_PIDS: u64 = 2 * NPROC;
const GUEST_PORT: u32 = 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);
const INSTALL_CA: &str =
    "printf '%s' \"$SHURU_PROXY_CA\" > /usr/local/share/ca-certificates/shuru-proxy.crt \
    && update-ca-certificates --fresh > /dev/null 2>&1";

#[derive(Parser)]
#[command(name = "shuru-node", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot a jailed VM, run one command in it and stop the VM.
    Run(RunArgs),
    /// Restore a jailed VM from a snapshot, run one command in it and stop
    /// the VM.
    Restore(RestoreArgs),
}

#[derive(clap::Args)]
struct Common {
    /// Picks the uids and the cgroup. One VM per slot at a time.
    #[arg(long, default_value_t = 0)]
    slot: u32,
    /// The directory holding the statically linked shuru-vmm and
    /// shuru-netd. Defaults to the one holding this binary.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
    /// Show the guest console, guest token included.
    #[arg(long)]
    verbose: bool,
    /// Log calls the seccomp filters forbid instead of killing.
    #[arg(long)]
    seccomp_log: bool,
    /// Print timings and peak memory to stderr.
    #[arg(long)]
    time: bool,
}

#[derive(clap::Args)]
struct RunArgs {
    #[arg(long)]
    kernel: PathBuf,
    #[arg(long)]
    initrd: PathBuf,
    /// The working disk. The guest writes to it.
    #[arg(long)]
    disk: PathBuf,
    #[arg(long, default_value_t = 2)]
    cpus: usize,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = 2048)]
    memory: u64,
    /// Give the guest a network through a jailed shuru-netd. It reaches
    /// public addresses only.
    #[arg(long)]
    allow_net: bool,
    /// A domain the guest may reach, wildcards allowed. Without any, every
    /// public address.
    #[arg(long = "allow", requires = "allow_net")]
    allow: Vec<String>,
    /// NAME@HOST[,HOST]: the guest sees a placeholder in $NAME, and the
    /// value of $NAME here is sent in its place to those hosts only.
    #[arg(long = "secret", requires = "allow_net")]
    secrets: Vec<String>,
    #[arg(long, default_value = "8.8.8.8")]
    dns: Ipv4Addr,
    /// Roots for checking the hosts secrets are sent to.
    #[arg(long, default_value = "/etc/ssl/certs/ca-certificates.crt")]
    ca_bundle: PathBuf,
    /// After the command, snapshot the VM into this new directory: its
    /// memory, its state and a copy of the working disk.
    #[arg(long)]
    snapshot_to: Option<PathBuf>,
    #[command(flatten)]
    common: Common,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(clap::Args)]
struct RestoreArgs {
    /// A directory `run --snapshot-to` wrote.
    #[arg(long)]
    from: PathBuf,
    /// Where the working disk goes, a copy of the saved one. It must not
    /// exist.
    #[arg(long)]
    disk: PathBuf,
    /// Roots for checking the hosts secrets are sent to, for a snapshot
    /// with a network.
    #[arg(long, default_value = "/etc/ssl/certs/ca-certificates.crt")]
    ca_bundle: PathBuf,
    /// After the command, snapshot the VM again into this new directory.
    /// Its memory starts as a copy of the restored one.
    #[arg(long)]
    snapshot_to: Option<PathBuf>,
    /// Do not read the saved memory ahead; pages load on first touch only.
    #[arg(long)]
    no_prefetch: bool,
    #[command(flatten)]
    common: Common,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn main() {
    let result = match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::Restore(args) => restore(args),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("shuru-node: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The sandbox directory: a control socket per jailed process, the vsock
/// socket, and the empty mount point for their roots. Only root can enter
/// it.
struct RunDir(PathBuf);

impl RunDir {
    fn create(slot: u32) -> Result<Self> {
        let base = Path::new(RUN_DIR);
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(base)?;
        let path = base.join(format!("sbx-{slot}"));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("slot {slot} is in use: {}", path.display()))?;
        let dir = RunDir(path);
        fs::DirBuilder::new().mode(0o555).create(dir.root())?;
        Ok(dir)
    }

    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    fn control(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.sock"))
    }

    fn vsock(&self) -> PathBuf {
        self.0.join("vsock.sock")
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        for name in ["vmm", "netd"] {
            let _ = fs::remove_file(self.control(name));
        }
        let _ = fs::remove_file(self.vsock());
        let _ = fs::remove_dir(self.root());
        let _ = fs::remove_dir(&self.0);
    }
}

/// A jailed process and its control connection. Killed and reaped when
/// dropped.
struct Jailed {
    pid: i32,
    reaped: Cell<bool>,
    control: UnixStream,
}

impl Jailed {
    fn exited(&self) -> bool {
        let mut status = 0;
        if !self.reaped.get()
            && unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid
        {
            self.reaped.set(true);
        }
        self.reaped.get()
    }

    fn configure(&self, request: &Request, fds: &[i32]) -> Result<Response> {
        control::send(&self.control, request, fds)?;
        match control::receive::<Response>(&self.control, 0)? {
            Some((Response::Error { message }, _)) => bail!("{message}"),
            Some((response, _)) => Ok(response),
            None => bail!("exited while starting"),
        }
    }

    fn stop(self) {
        let _ = control::send(&self.control, &Request::Stop, &[]);
    }
}

impl Drop for Jailed {
    // Once reaped the pid may belong to someone else.
    fn drop(&mut self) {
        if self.reaped.get() {
            return;
        }
        let mut status = 0;
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, &mut status, 0);
        }
    }
}

struct Spawn<'a> {
    name: &'static str,
    uid: u32,
    nofile: u64,
    host_network: bool,
    dir: &'a RunDir,
    sandbox: &'a mut Sandbox,
    bin_dir: &'a Path,
    null: &'a File,
    seccomp_log: bool,
}

fn spawn(spawn: Spawn) -> Result<Jailed> {
    let name = format!("shuru-{}", spawn.name);
    let path = spawn.bin_dir.join(&name);
    let binary = File::open(&path).with_context(|| format!("{}", path.display()))?;
    let listener: OwnedFd = UnixListener::bind(spawn.dir.control(spawn.name))?.into();
    let procs = spawn.sandbox.child(spawn.name)?;
    let root = CString::new(spawn.dir.root().as_os_str().as_bytes())?;
    let argv0 = CString::new(name.as_str())?;
    let mut argv = vec![argv0.as_c_str()];
    if spawn.seccomp_log {
        argv.push(c"--seccomp-log");
    }
    let pid = jail::spawn(
        &Jail {
            uid: spawn.uid,
            gid: spawn.uid,
            cgroup_procs: &procs,
            root: &root,
            nofile: spawn.nofile,
            nproc: NPROC,
            host_network: spawn.host_network,
        },
        &binary,
        &argv,
        &Handoff {
            keep: &listener,
            null: spawn.null,
            stderr: io::stderr().as_raw_fd(),
        },
    )
    .with_context(|| format!("starting {name}"))?;
    let reaped = Cell::new(false);
    let control = match UnixStream::connect(spawn.dir.control(spawn.name)) {
        Ok(control) => control,
        Err(e) => {
            drop(Jailed {
                pid,
                reaped,
                control: UnixStream::pair()?.0,
            });
            return Err(e.into());
        }
    };
    Ok(Jailed {
        pid,
        reaped,
        control,
    })
}

/// `NAME@HOST[,HOST]`, valued from the environment of this process.
fn secret(spec: &str) -> Result<(String, Secret)> {
    let (name, hosts) = spec
        .split_once('@')
        .with_context(|| format!("--secret {spec}: expected NAME@HOST[,HOST]"))?;
    let value =
        std::env::var(name).with_context(|| format!("--secret {spec}: ${name} is not set"))?;
    let hosts = hosts.split(',').map(str::to_string).collect();
    Ok((name.to_string(), Secret { value, hosts }))
}

/// What the guest may reach and which secrets it is given, kept with a
/// snapshot so a restore gets the same.
#[derive(Clone, Serialize, Deserialize)]
struct NetworkPolicy {
    allow: Vec<String>,
    /// NAME@HOST[,HOST], valued from the environment at each start.
    secrets: Vec<String>,
    dns: Ipv4Addr,
}

struct Network {
    netd: Jailed,
    vm_end: OwnedFd,
    mac: [u8; 6],
    session: shuru_proxy::Session,
}

/// Configures `netd`. A restore passes the session and MAC the guest was
/// saved with: it trusts that CA, holds those placeholders and has the
/// gateway in its neighbour table under that MAC.
fn start_network(
    policy: &NetworkPolicy,
    ca_bundle: &Path,
    netd: Jailed,
    saved: Option<(shuru_proxy::Session, [u8; 6])>,
) -> Result<Network> {
    let secrets = policy
        .secrets
        .iter()
        .map(|spec| secret(spec))
        .collect::<Result<_>>()?;
    let upstream_roots =
        fs::read_to_string(ca_bundle).with_context(|| format!("{}", ca_bundle.display()))?;
    let (vm_end, host_end) = shuru_proxy::create_socketpair()?;
    let (vm_end, host_end) =
        unsafe { (OwnedFd::from_raw_fd(vm_end), OwnedFd::from_raw_fd(host_end)) };
    let (session, mac) = match saved {
        Some((session, mac)) => (Some(session), mac),
        None => {
            let mut mac = [0u8; 6];
            getrandom::fill(&mut mac).map_err(|e| anyhow::anyhow!("{e}"))?;
            mac[0] = (mac[0] & 0xfe) | 0x02;
            (None, mac)
        }
    };
    let config = NetdConfig {
        dns_resolver: policy.dns,
        allow: policy.allow.clone(),
        secrets,
        upstream_roots,
        session,
    };
    let response = netd
        .configure(&Request::ConfigureNetwork(config), &[host_end.as_raw_fd()])
        .context("shuru-netd failed")?;
    let Response::Network { session } = response else {
        bail!("shuru-netd answered {response:?}");
    };
    Ok(Network {
        netd,
        vm_end,
        mac,
        session,
    })
}

/// What every sandbox needs from the host, opened before the jail.
struct Host {
    bin_dir: PathBuf,
    kvm: OwnedFd,
    null: File,
    serial: OwnedFd,
}

impl Host {
    fn open(common: &Common) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("shuru-node runs as root");
        }
        let bin_dir = match &common.bin_dir {
            Some(dir) => dir.clone(),
            None => std::env::current_exe()?
                .parent()
                .context("no binary directory")?
                .to_path_buf(),
        };
        let kvm = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .context("/dev/kvm")?
            .into();
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        let serial = if common.verbose {
            io::stderr().as_fd().try_clone_to_owned()?
        } else {
            null.try_clone()?.into()
        };
        Ok(Host {
            bin_dir,
            kvm,
            null,
            serial,
        })
    }

    fn sandbox(&self, common: &Common, memory_bytes: u64, cpus: usize) -> Result<Sandbox> {
        Sandbox::create(
            &common.slot.to_string(),
            &Limits {
                guest_memory_bytes: memory_bytes,
                cpus: cpus as u64,
                pids: SANDBOX_PIDS,
            },
        )
        .context("creating the sandbox cgroup")
    }

    fn spawn_netd(&self, common: &Common, dir: &RunDir, sandbox: &mut Sandbox) -> Result<Jailed> {
        firewall::install()?;
        spawn(Spawn {
            name: "netd",
            uid: NETD_UID_BASE + common.slot,
            nofile: NETD_NOFILE,
            host_network: true,
            dir,
            sandbox,
            bin_dir: &self.bin_dir,
            null: &self.null,
            seccomp_log: common.seccomp_log,
        })
    }

    fn spawn_vmm(&self, common: &Common, dir: &RunDir, sandbox: &mut Sandbox) -> Result<Jailed> {
        spawn(Spawn {
            name: "vmm",
            uid: VMM_UID_BASE + common.slot,
            nofile: VMM_NOFILE,
            host_network: false,
            dir,
            sandbox,
            bin_dir: &self.bin_dir,
            null: &self.null,
            seccomp_log: common.seccomp_log,
        })
    }
}

fn open(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("{}", path.display()))
}

fn print_times(sandbox: &Sandbox, times: &[(&str, Duration)]) {
    let peak = sandbox
        .memory_peak()
        .map_or("unknown".into(), |bytes| format!("{} MiB", bytes >> 20));
    let times: Vec<String> = times
        .iter()
        .map(|(what, at)| format!("{what} {} ms", at.as_millis()))
        .collect();
    eprintln!("shuru-node: {}, peak memory {peak}", times.join(", "));
}

fn run(args: RunArgs) -> Result<i32> {
    let started = Instant::now();
    let host = Host::open(&args.common)?;
    snapshot_target(args.snapshot_to.as_deref())?;
    let kernel = open(&args.kernel)?;
    let initrd = open(&args.initrd)?;
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.disk)
        .with_context(|| format!("{}", args.disk.display()))?;
    let policy = args.allow_net.then(|| NetworkPolicy {
        allow: args.allow.clone(),
        secrets: args.secrets.clone(),
        dns: args.dns,
    });

    let dir = RunDir::create(args.common.slot)?;
    let vsock_listener = UnixListener::bind(dir.vsock())?;
    let mut sandbox = host.sandbox(&args.common, args.memory << 20, args.cpus)?;

    let network = match &policy {
        Some(policy) => {
            let netd = host.spawn_netd(&args.common, &dir, &mut sandbox)?;
            Some(start_network(policy, &args.ca_bundle, netd, None)?)
        }
        None => None,
    };
    let networked = started.elapsed();

    let vmm = host.spawn_vmm(&args.common, &dir, &mut sandbox)?;
    let spawned = started.elapsed();

    let identity = new_identity()?;
    let quiet = if args.common.verbose { "" } else { " quiet" };
    let command_line = format!(
        "console=ttyAMA0 root=/dev/vda rw{quiet} shuru.token={} shuru.instance={}",
        hex(&identity.token),
        hex(&identity.instance)
    );

    let config = VmmConfig {
        cpus: args.cpus,
        memory_bytes: args.memory << 20,
        boot: Boot::Kernel {
            command_line,
            initrd: true,
        },
        disk: Some(Disk { read_only: false }),
        serial: true,
        network_mac: network.as_ref().map(|network| network.mac),
        vsock_connections: VSOCK_CONNECTIONS,
        balloon: true,
    };
    let mut fds = vec![
        host.kvm.as_raw_fd(),
        kernel.as_raw_fd(),
        initrd.as_raw_fd(),
        disk.as_raw_fd(),
        host.serial.as_raw_fd(),
    ];
    if let Some(network) = &network {
        fds.push(network.vm_end.as_raw_fd());
    }
    fds.push(vsock_listener.as_raw_fd());
    vmm.configure(&Request::Configure(config), &fds)
        .context("shuru-vmm failed")?;
    drop((host, kernel, initrd, disk, vsock_listener));
    let ready = started.elapsed();

    let mut env = HashMap::new();
    if let Some(network) = &network {
        if !network.session.placeholders.is_empty() {
            let guest = connect_guest(&dir.vsock(), &identity, &vmm)?;
            let ca = network.session.ca_cert_pem.clone();
            let ca = HashMap::from([("SHURU_PROXY_CA".to_string(), ca)]);
            let argv = ["sh", "-c", INSTALL_CA].map(str::to_string).to_vec();
            if exec(guest, argv, ca)? != 0 {
                bail!("installing the proxy CA in the guest failed");
            }
            env = network.session.placeholders.clone();
        }
    }
    let guest = connect_guest(&dir.vsock(), &identity, &vmm)?;
    let answered = started.elapsed();
    let code = exec(guest, args.command, env)?;
    let done = started.elapsed();

    let mut times = vec![
        ("network", networked),
        ("jailed", spawned),
        ("VM running", ready),
        ("guest answered", answered),
        ("done", done),
    ];
    if let Some(to) = &args.snapshot_to {
        let info = SnapshotInfo {
            release: RELEASE.into(),
            protocol: PROTOCOL_VERSION,
            cpus: args.cpus,
            memory_bytes: args.memory << 20,
            token: hex(&identity.token),
            instance: hex(&identity.instance),
            network: network
                .as_ref()
                .zip(policy)
                .map(|(network, policy)| SavedNetwork {
                    policy,
                    mac: hex(&network.mac),
                    session: network.session.clone(),
                }),
        };
        save_snapshot(&vmm, to, &args.disk, &info, None)?;
        times.push(("snapshot", started.elapsed() - done));
    }
    if args.common.time {
        print_times(&sandbox, &times);
    }
    vmm.stop();
    if let Some(network) = network {
        network.netd.stop();
    }
    Ok(code)
}

/// What a snapshot directory holds besides the memory, state and disk:
/// what took it, the VM shape and the guest identity at the snapshot,
/// which every restore replaces.
#[derive(Serialize, Deserialize)]
struct SnapshotInfo {
    release: String,
    /// The guest protocol of the agent running in the saved memory.
    protocol: u32,
    cpus: usize,
    memory_bytes: u64,
    token: String,
    instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network: Option<SavedNetwork>,
}

/// The guest network, with the proxy CA key. Secret values are not kept.
#[derive(Clone, Serialize, Deserialize)]
struct SavedNetwork {
    policy: NetworkPolicy,
    mac: String,
    session: shuru_proxy::Session,
}

const SNAPSHOT_INFO: &str = "node.json";
const RELEASE: &str = env!("CARGO_PKG_VERSION");

/// A file only root can read, which must not exist yet.
fn create_private(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("{}", path.display()))
}

/// `dst` gets the blocks of `src`, shared when the filesystem can.
fn clone_file(src: &Path, dst: &Path) -> Result<File> {
    let from = open(src)?;
    let to = create_private(dst)?;
    if !shuru_linux::reflink(&to, &from)? {
        io::copy(&mut &from, &mut &to)?;
    }
    Ok(to)
}

/// Refuses a snapshot directory that exists before the VM starts, rather
/// than after the command ran.
fn snapshot_target(dir: Option<&Path>) -> Result<()> {
    match dir {
        Some(dir) if fs::symlink_metadata(dir).is_ok() => {
            bail!("{} already exists", dir.display())
        }
        _ => Ok(()),
    }
}

/// Saves the VM into `dir`, which must not exist. A restored VM passes the
/// memory it was restored from as `base`; it only writes what changed. The
/// files are written and synced under another name, then renamed into
/// place, so a crash never leaves a directory that looks complete.
fn save_snapshot(
    vmm: &Jailed,
    dir: &Path,
    disk: &Path,
    info: &SnapshotInfo,
    base: Option<&Path>,
) -> Result<()> {
    snapshot_target(Some(dir))?;
    let name = dir
        .file_name()
        .context("the snapshot directory needs a name")?;
    let parent = match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let staging = parent.join(format!(".{}.partial", name.to_string_lossy()));
    // What an earlier crash left behind.
    let _ = fs::remove_dir_all(&staging);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .with_context(|| format!("{}", staging.display()))?;
    if let Err(e) = write_snapshot(vmm, &staging, disk, info, base) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let from = CString::new(staging.as_os_str().as_bytes())?;
    let to = CString::new(dir.as_os_str().as_bytes())?;
    let renamed = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if renamed != 0 {
        let error = io::Error::last_os_error();
        let _ = fs::remove_dir_all(&staging);
        return Err(error).with_context(|| format!("{}", dir.display()));
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn write_snapshot(
    vmm: &Jailed,
    dir: &Path,
    disk: &Path,
    info: &SnapshotInfo,
    base: Option<&Path>,
) -> Result<()> {
    let memory_path = dir.join("memory");
    let memory = match base {
        Some(base) => {
            clone_file(base, &memory_path)?;
            OpenOptions::new().write(true).open(&memory_path)?
        }
        None => {
            let memory = create_private(&memory_path)?;
            memory.set_len(info.memory_bytes)?;
            memory
        }
    };
    let state = create_private(&dir.join("state"))?;
    // The jail has no /proc. The VMM reads its page flags through this.
    let pagemap = open(Path::new(&format!("/proc/{}/pagemap", vmm.pid)))?;
    vmm.configure(
        &Request::Snapshot { resume: false },
        &[memory.as_raw_fd(), state.as_raw_fd(), pagemap.as_raw_fd()],
    )
    .context("snapshot failed")?;
    // The VM is paused, so the disk is as the saved guest left it.
    let disk = clone_file(disk, &dir.join("disk"))?;
    let node = create_private(&dir.join(SNAPSHOT_INFO))?;
    serde_json::to_writer(&node, info)?;
    for file in [&memory, &state, &disk, &node] {
        file.sync_all()?;
    }
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Starts reading the saved pages of `memory` into the page cache, so a
/// restored guest finds them there instead of faulting each one in from
/// the disk. Only the data ranges are read; holes are pages the guest
/// never touched.
fn prefetch(memory: File) {
    let read_ahead = move || {
        let fd = memory.as_raw_fd();
        let mut offset = 0;
        loop {
            let data = unsafe { libc::lseek(fd, offset, libc::SEEK_DATA) };
            if data < 0 {
                return;
            }
            let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
            if hole <= data {
                return;
            }
            unsafe { libc::posix_fadvise(fd, data, hole - data, libc::POSIX_FADV_WILLNEED) };
            offset = hole;
        }
    };
    let _ = std::thread::Builder::new()
        .name("prefetch".into())
        .spawn(read_ahead);
}

fn restore(args: RestoreArgs) -> Result<i32> {
    let started = Instant::now();
    let host = Host::open(&args.common)?;
    snapshot_target(args.snapshot_to.as_deref())?;
    let info: SnapshotInfo = serde_json::from_reader(open(&args.from.join(SNAPSHOT_INFO))?)
        .context("reading the snapshot")?;
    // The agent in the saved memory answers only its own protocol.
    if info.protocol != PROTOCOL_VERSION {
        bail!(
            "the snapshot was taken by shuru-node {} with guest protocol {}; this one speaks {}",
            info.release,
            info.protocol,
            PROTOCOL_VERSION
        );
    }
    let identity = Identity {
        token: unhex(&info.token)?,
        instance: unhex(&info.instance)?,
    };
    clone_file(&args.from.join("disk"), &args.disk)?;
    let disk = OpenOptions::new().read(true).write(true).open(&args.disk)?;
    let memory = open(&args.from.join("memory"))?;
    if !args.no_prefetch {
        prefetch(memory.try_clone()?);
    }
    let state = open(&args.from.join("state"))?;
    let cloned = started.elapsed();

    let dir = RunDir::create(args.common.slot)?;
    let vsock_listener = UnixListener::bind(dir.vsock())?;
    let mut sandbox = host.sandbox(&args.common, info.memory_bytes, info.cpus)?;
    let network = match &info.network {
        Some(saved) => {
            let netd = host.spawn_netd(&args.common, &dir, &mut sandbox)?;
            let saved_session = (saved.session.clone(), unhex(&saved.mac)?);
            Some(start_network(
                &saved.policy,
                &args.ca_bundle,
                netd,
                Some(saved_session),
            )?)
        }
        None => None,
    };
    let vmm = host.spawn_vmm(&args.common, &dir, &mut sandbox)?;
    let spawned = started.elapsed();

    let config = VmmConfig {
        cpus: info.cpus,
        memory_bytes: info.memory_bytes,
        boot: Boot::Snapshot,
        disk: Some(Disk { read_only: false }),
        serial: true,
        network_mac: network.as_ref().map(|network| network.mac),
        vsock_connections: VSOCK_CONNECTIONS,
        balloon: true,
    };
    let mut fds = vec![
        host.kvm.as_raw_fd(),
        disk.as_raw_fd(),
        host.serial.as_raw_fd(),
    ];
    if let Some(network) = &network {
        fds.push(network.vm_end.as_raw_fd());
    }
    fds.extend([
        vsock_listener.as_raw_fd(),
        memory.as_raw_fd(),
        state.as_raw_fd(),
    ]);
    vmm.configure(&Request::Configure(config), &fds)
        .context("shuru-vmm failed")?;
    drop((host, disk, vsock_listener, memory, state));
    let ready = started.elapsed();

    let mut guest = connect_guest(&dir.vsock(), &identity, &vmm)?;
    let answered = started.elapsed();
    let identity = rotate(&mut guest)?;
    let rotated = started.elapsed();
    let env = network
        .as_ref()
        .map(|network| network.session.placeholders.clone())
        .unwrap_or_default();
    let code = exec(guest, args.command, env)?;
    let done = started.elapsed();
    let mut times = vec![
        ("disk cloned", cloned),
        ("jailed", spawned),
        ("VM restored", ready),
        ("guest answered", answered),
        ("identity rotated", rotated),
        ("done", done),
    ];
    if let Some(to) = &args.snapshot_to {
        let info = SnapshotInfo {
            release: RELEASE.into(),
            token: hex(&identity.token),
            instance: hex(&identity.instance),
            ..info
        };
        let base = args.from.join("memory");
        save_snapshot(&vmm, to, &args.disk, &info, Some(&base))?;
        times.push(("snapshot", started.elapsed() - done));
    }
    if args.common.time {
        print_times(&sandbox, &times);
    }
    vmm.stop();
    if let Some(network) = network {
        network.netd.stop();
    }
    Ok(code)
}

fn new_identity() -> Result<Identity> {
    let mut identity = Identity {
        token: [0; 32],
        instance: [0; 16],
    };
    getrandom::fill(&mut identity.token).map_err(|e| anyhow::anyhow!("{e}"))?;
    getrandom::fill(&mut identity.instance).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(identity)
}

/// Gives a restored guest an identity no other restore of the snapshot
/// has, the current time and fresh seed material for its RNG. Until this
/// answers, anyone holding the snapshot could reach the guest.
fn rotate(guest: &mut UnixStream) -> Result<Identity> {
    let mut restore = Restore {
        identity: new_identity()?,
        realtime_ns: 0,
        seed: [0; 32],
    };
    getrandom::fill(&mut restore.seed).map_err(|e| anyhow::anyhow!("{e}"))?;
    restore.realtime_ns = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos()
        .try_into()?;
    guest.set_read_timeout(Some(Duration::from_secs(2)))?;
    frame::write_frame(guest, frame::RESTORE_REQ, &restore.encode())?;
    match frame::read_frame(guest) {
        Ok(Some((frame::RESTORE_RESP, _))) => {}
        Ok(Some((frame::ERROR, payload))) => {
            bail!(
                "the guest refused the restore: {}",
                String::from_utf8_lossy(&payload)
            )
        }
        Ok(Some((kind, _))) => bail!("the guest answered the restore with frame {kind:#x}"),
        Ok(None) => bail!("the guest closed the connection"),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            bail!("the guest agent predates restore support")
        }
        Err(e) => return Err(e).context("reading the restore answer"),
    }
    guest.set_read_timeout(None)?;
    Ok(restore.identity)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    if text.len() != 2 * N || !text.is_ascii() {
        bail!("bad hex in the snapshot");
    }
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16)?;
    }
    Ok(bytes)
}

/// Connects to the guest agent through the VMM listener, retrying until
/// the agent listens.
fn connect_guest(path: &Path, identity: &Identity, vmm: &Jailed) -> Result<UnixStream> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if vmm.exited() {
            bail!("the VMM exited before the guest answered");
        }
        if Instant::now() > deadline {
            bail!("the guest did not answer within {BOOT_TIMEOUT:?}");
        }
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(format!("CONNECT {GUEST_PORT}\n").as_bytes())?;
        if read_line(&mut stream)?.is_some_and(|line| line.starts_with("OK ")) {
            identity
                .connect(&mut stream)
                .context("the guest did not answer the connection hello")?;
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Reads up to a newline one byte at a time, so nothing after it is lost.
fn read_line(stream: &mut UnixStream) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0];
    while line.len() < 32 {
        if stream.read(&mut byte)? == 0 {
            return Ok(None);
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8(line).ok());
        }
        line.push(byte[0]);
    }
    Ok(None)
}

fn exec(mut guest: UnixStream, argv: Vec<String>, env: HashMap<String, String>) -> Result<i32> {
    let request = ExecRequest {
        argv,
        env,
        tty: None,
        rows: None,
        cols: None,
        cwd: None,
        id: None,
    };
    frame::send_json(&mut guest, frame::EXEC_REQ, &request)?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    loop {
        match frame::read_frame(&mut guest).context("reading from the guest")? {
            Some((frame::STDOUT, payload)) => stdout.write_all(&payload)?,
            Some((frame::STDERR, payload)) => stderr.write_all(&payload)?,
            Some((frame::EXIT, payload)) => {
                stdout.flush()?;
                return Ok(frame::parse_exit_code(&payload).unwrap_or(1));
            }
            Some((frame::ERROR, payload)) => {
                bail!("guest error: {}", String::from_utf8_lossy(&payload))
            }
            Some(_) => {}
            None => bail!("the guest closed the connection"),
        }
    }
}
