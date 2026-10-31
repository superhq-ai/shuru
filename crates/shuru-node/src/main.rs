//! `shuru-node run` boots one jailed VM, runs a command in it and stops it,
//! the way a node runs each sandbox. `shuru-node restore` does the same
//! from a snapshot `run --snapshot-to` took. `shuru-node serve` keeps
//! sandboxes running and serves the sandbox API for them.

mod serve;
mod vm;

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use shuru_node::cgroup::Sandbox;
use shuru_node::control::{Boot, Disk, Request, VmmConfig};
use shuru_proto::hello::{Identity, Restore, PROTOCOL_VERSION};
use shuru_proto::{frame, ExecRequest};
use vm::{
    connect_guest, hex, new_identity, open, start_network, try_connect_guest, unhex, Host, Jailed,
    KernelBoot, NetworkPolicy, RunDir, Vm, VSOCK_CONNECTIONS,
};

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
    /// Keep sandboxes and serve the sandbox API for them on a unix socket.
    Serve(serve::ServeArgs),
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
    /// While the command runs, each connection to this new unix socket is
    /// a fresh guest connection, past the hello, for speaking the guest
    /// protocol from outside.
    #[arg(long)]
    guest_socket: Option<PathBuf>,
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
        Command::Serve(args) => serve::serve(args).map(|()| 0),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("shuru-node: {e:#}");
            std::process::exit(1);
        }
    }
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
    let host = open_host(&args.common)?;
    snapshot_target(args.snapshot_to.as_deref())?;
    let policy = args.allow_net.then(|| NetworkPolicy {
        allow: args.allow.clone(),
        secrets: args.secrets.clone(),
        dns: args.dns,
    });
    let mut times = Vec::new();
    let vm = Vm::boot(
        &host,
        &KernelBoot {
            slot: args.common.slot,
            kernel: &args.kernel,
            initrd: &args.initrd,
            disk: &args.disk,
            cpus: args.cpus,
            memory_bytes: args.memory << 20,
            network: policy.as_ref(),
            secrets: &HashMap::new(),
            ca_bundle: &args.ca_bundle,
            verbose: args.common.verbose,
        },
        started,
        &mut times,
    )?;
    drop(host);

    let mut env = HashMap::new();
    if let Some(network) = &vm.network {
        if !network.session.placeholders.is_empty() {
            vm.install_ca(&network.session.ca_cert_pem)?;
            env = network.session.placeholders.clone();
        }
    }
    let guest = vm.connect()?;
    times.push(("guest answered", started.elapsed()));
    let _socket = serve_guest(&args.common, vm.dir.vsock(), vm.identity)?;
    let code = exec(guest, args.command, env)?;
    let done = started.elapsed();
    times.push(("done", done));

    if let Some(to) = &args.snapshot_to {
        let info = SnapshotInfo {
            release: RELEASE.into(),
            protocol: PROTOCOL_VERSION,
            cpus: args.cpus,
            memory_bytes: args.memory << 20,
            token: hex(&vm.identity.token),
            instance: hex(&vm.identity.instance),
            network: vm
                .network
                .as_ref()
                .zip(policy)
                .map(|(network, policy)| SavedNetwork {
                    policy,
                    mac: hex(&network.mac),
                    session: network.session.clone(),
                }),
        };
        save_snapshot(&vm.vmm, to, &args.disk, &info, None)?;
        times.push(("snapshot", started.elapsed() - done));
    }
    if args.common.time {
        print_times(&vm.sandbox, &times);
    }
    vm.stop();
    Ok(code)
}

fn open_host(common: &Common) -> Result<Host> {
    Host::open(
        common.bin_dir.as_deref(),
        common.verbose,
        common.seccomp_log,
    )
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
    let host = open_host(&args.common)?;
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
    let slot = args.common.slot;
    let mut sandbox = host.sandbox(slot, info.memory_bytes, info.cpus)?;
    let network = match &info.network {
        Some(saved) => {
            let netd = host.spawn_netd(slot, &dir, &mut sandbox)?;
            let saved_session = (saved.session.clone(), unhex(&saved.mac)?);
            Some(start_network(
                &saved.policy,
                &HashMap::new(),
                &args.ca_bundle,
                netd,
                Some(saved_session),
            )?)
        }
        None => None,
    };
    let vmm = host.spawn_vmm(slot, &dir, &mut sandbox)?;
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
    let _socket = serve_guest(&args.common, dir.vsock(), identity)?;
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

/// Removes the `--guest-socket` path when the run ends.
struct GuestSocket(PathBuf);

impl Drop for GuestSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn serve_guest(common: &Common, vsock: PathBuf, identity: Identity) -> Result<Option<GuestSocket>> {
    let Some(path) = &common.guest_socket else {
        return Ok(None);
    };
    let listener = UnixListener::bind(path).with_context(|| format!("{}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let vsock = vsock.clone();
            std::thread::spawn(move || {
                if let Ok(Some(guest)) = try_connect_guest(&vsock, &identity) {
                    relay(client, guest);
                }
            });
        }
    });
    Ok(Some(GuestSocket(path.clone())))
}

fn relay(a: UnixStream, b: UnixStream) {
    let copy = |mut from: UnixStream, mut to: UnixStream| {
        let _ = io::copy(&mut from, &mut to);
        let _ = to.shutdown(std::net::Shutdown::Write);
    };
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let t = std::thread::spawn(move || copy(a2, b2));
    copy(b, a);
    let _ = t.join();
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
