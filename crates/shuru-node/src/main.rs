//! `shuru-node run` boots one jailed VM, runs a command in it and stops it,
//! the way a node runs each sandbox.

use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use shuru_node::cgroup::{self, Limits};
use shuru_node::control::{self, Disk, Request, Response, VmmConfig};
use shuru_node::jail::{self, Handoff, Jail};
use shuru_proto::hello::Identity;
use shuru_proto::{frame, ExecRequest};

const RUN_DIR: &str = "/run/shuru";
const VMM_UID_BASE: u32 = 100_000;
const VSOCK_CONNECTIONS: usize = 128;
const NOFILE: u64 = 512;
const NPROC: u64 = 64;
const GUEST_PORT: u32 = 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Picks the VMM uid and the cgroup. One VM per slot at a time.
    #[arg(long, default_value_t = 0)]
    slot: u32,
    /// The statically linked shuru-vmm. Defaults to the one next to this
    /// binary.
    #[arg(long)]
    vmm: Option<PathBuf>,
    /// Show the guest console, guest token included.
    #[arg(long)]
    verbose: bool,
    /// Log VMM calls outside the seccomp allowlist instead of killing it.
    #[arg(long)]
    seccomp_log: bool,
    /// Print boot timings and peak memory to stderr.
    #[arg(long)]
    time: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn main() {
    let Command::Run(args) = Cli::parse().command;
    match run(args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("shuru-node: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The sandbox directory: the two sockets, and the empty mount point for
/// the VMM root. Only root can enter it.
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

    fn control(&self) -> PathBuf {
        self.0.join("control.sock")
    }

    fn vsock(&self) -> PathBuf {
        self.0.join("vsock.sock")
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.control());
        let _ = fs::remove_file(self.vsock());
        let _ = fs::remove_dir(self.root());
        let _ = fs::remove_dir(&self.0);
    }
}

/// The jailed VMM, killed and reaped when dropped.
struct Vmm {
    pid: i32,
    reaped: Cell<bool>,
}

impl Vmm {
    fn exited(&self) -> bool {
        let mut status = 0;
        if !self.reaped.get()
            && unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid
        {
            self.reaped.set(true);
        }
        self.reaped.get()
    }
}

impl Drop for Vmm {
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

fn run(args: RunArgs) -> Result<i32> {
    let started = Instant::now();
    if unsafe { libc::geteuid() } != 0 {
        bail!("shuru-node runs as root");
    }
    let vmm_path = match &args.vmm {
        Some(path) => path.clone(),
        None => std::env::current_exe()?.with_file_name("shuru-vmm"),
    };
    let binary = File::open(&vmm_path).with_context(|| format!("{}", vmm_path.display()))?;
    let kvm: OwnedFd = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .context("/dev/kvm")?
        .into();
    let open = |path: &Path| File::open(path).with_context(|| format!("{}", path.display()));
    let kernel = open(&args.kernel)?;
    let initrd = open(&args.initrd)?;
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.disk)
        .with_context(|| format!("{}", args.disk.display()))?;
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let serial: OwnedFd = if args.verbose {
        io::stderr().as_fd().try_clone_to_owned()?
    } else {
        null.try_clone()?.into()
    };

    let dir = RunDir::create(args.slot)?;
    let control_listener: OwnedFd = UnixListener::bind(dir.control())?.into();
    let vsock_listener = UnixListener::bind(dir.vsock())?;
    let mut sandbox = cgroup::Sandbox::create(
        &args.slot.to_string(),
        &Limits {
            guest_memory_bytes: args.memory << 20,
            cpus: args.cpus as u64,
            pids: NPROC,
        },
    )
    .context("creating the sandbox cgroup")?;
    let procs = sandbox.child("vmm")?;

    let uid = VMM_UID_BASE + args.slot;
    let root = CString::new(dir.root().as_os_str().as_bytes())?;
    let mut argv: Vec<&CStr> = vec![c"shuru-vmm"];
    if args.seccomp_log {
        argv.push(c"--seccomp-log");
    }
    let pid = jail::spawn(
        &Jail {
            uid,
            gid: uid,
            cgroup_procs: &procs,
            root: &root,
            nofile: NOFILE,
            nproc: NPROC,
        },
        &binary,
        &argv,
        &Handoff {
            keep: &control_listener,
            null: &null,
            stderr: io::stderr().as_raw_fd(),
        },
    )?;
    let vmm = Vmm {
        pid,
        reaped: Cell::new(false),
    };
    drop(control_listener);
    let spawned = started.elapsed();

    let mut identity = Identity {
        token: [0; 32],
        instance: [0; 16],
    };
    getrandom::fill(&mut identity.token).map_err(|e| anyhow::anyhow!("{e}"))?;
    getrandom::fill(&mut identity.instance).map_err(|e| anyhow::anyhow!("{e}"))?;
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let quiet = if args.verbose { "" } else { " quiet" };
    let command_line = format!(
        "console=ttyAMA0 root=/dev/vda rw{quiet} shuru.token={} shuru.instance={}",
        hex(&identity.token),
        hex(&identity.instance)
    );

    let control = UnixStream::connect(dir.control())?;
    let config = VmmConfig {
        cpus: args.cpus,
        memory_bytes: args.memory << 20,
        command_line,
        initrd: true,
        disk: Some(Disk { read_only: false }),
        serial: true,
        network_mac: None,
        vsock_connections: VSOCK_CONNECTIONS,
        balloon: true,
    };
    let fds = [
        kvm.as_raw_fd(),
        kernel.as_raw_fd(),
        initrd.as_raw_fd(),
        disk.as_raw_fd(),
        serial.as_raw_fd(),
        vsock_listener.as_raw_fd(),
    ];
    control::send(&control, &Request::Configure(config), &fds)?;
    drop((kvm, kernel, initrd, disk, serial, vsock_listener));
    match control::receive::<Response>(&control, 0)? {
        Some((Response::Ok, _)) => {}
        Some((Response::Error { message }, _)) => bail!("the VMM failed: {message}"),
        _ => bail!("the VMM exited while starting"),
    }
    let ready = started.elapsed();

    let guest = connect_guest(&dir.vsock(), &identity, &vmm)?;
    let answered = started.elapsed();
    let code = exec(guest, args.command)?;

    if args.time {
        let peak = sandbox
            .memory_peak()
            .map_or("unknown".into(), |bytes| format!("{} MiB", bytes >> 20));
        eprintln!(
            "shuru-node: jailed {} ms, VM running {} ms, guest answered {} ms, done {} ms, peak memory {peak}",
            spawned.as_millis(),
            ready.as_millis(),
            answered.as_millis(),
            started.elapsed().as_millis(),
        );
    }
    let _ = control::send(&control, &Request::Stop, &[]);
    drop(vmm);
    Ok(code)
}

/// Connects to the guest agent through the VMM listener, retrying until
/// the agent listens.
fn connect_guest(path: &Path, identity: &Identity, vmm: &Vmm) -> Result<UnixStream> {
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

fn exec(mut guest: UnixStream, argv: Vec<String>) -> Result<i32> {
    let request = ExecRequest {
        argv,
        env: Default::default(),
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
