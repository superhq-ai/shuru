//! `shuru-node run` boots one jailed VM, runs a command in it and stops it,
//! the way a node runs each sandbox.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use shuru_node::cgroup::{Limits, Sandbox};
use shuru_node::control::{self, Disk, NetdConfig, Request, Response, Secret, VmmConfig};
use shuru_node::firewall::{self, NETD_UID_BASE};
use shuru_node::jail::{self, Handoff, Jail};
use shuru_proto::hello::Identity;
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
    /// Picks the uids and the cgroup. One VM per slot at a time.
    #[arg(long, default_value_t = 0)]
    slot: u32,
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

struct Network {
    netd: Jailed,
    vm_end: OwnedFd,
    mac: [u8; 6],
    placeholders: HashMap<String, String>,
    ca_cert_pem: String,
}

fn start_network(args: &RunArgs, netd: Jailed) -> Result<Network> {
    let secrets = args
        .secrets
        .iter()
        .map(|spec| secret(spec))
        .collect::<Result<_>>()?;
    let upstream_roots = fs::read_to_string(&args.ca_bundle)
        .with_context(|| format!("{}", args.ca_bundle.display()))?;
    let (vm_end, host_end) = shuru_proxy::create_socketpair()?;
    let (vm_end, host_end) =
        unsafe { (OwnedFd::from_raw_fd(vm_end), OwnedFd::from_raw_fd(host_end)) };
    let config = NetdConfig {
        dns_resolver: args.dns,
        allow: args.allow.clone(),
        secrets,
        upstream_roots,
    };
    let response = netd
        .configure(&Request::ConfigureNetwork(config), &[host_end.as_raw_fd()])
        .context("shuru-netd failed")?;
    let Response::Network {
        placeholders,
        ca_cert_pem,
    } = response
    else {
        bail!("shuru-netd answered {response:?}");
    };
    let mut mac = [0u8; 6];
    getrandom::fill(&mut mac).map_err(|e| anyhow::anyhow!("{e}"))?;
    mac[0] = (mac[0] & 0xfe) | 0x02;
    Ok(Network {
        netd,
        vm_end,
        mac,
        placeholders,
        ca_cert_pem,
    })
}

fn run(args: RunArgs) -> Result<i32> {
    let started = Instant::now();
    if unsafe { libc::geteuid() } != 0 {
        bail!("shuru-node runs as root");
    }
    let bin_dir = match &args.bin_dir {
        Some(dir) => dir.clone(),
        None => std::env::current_exe()?
            .parent()
            .context("no binary directory")?
            .to_path_buf(),
    };
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
    if args.allow_net {
        firewall::install()?;
    }

    let dir = RunDir::create(args.slot)?;
    let vsock_listener = UnixListener::bind(dir.vsock())?;
    let mut sandbox = Sandbox::create(
        &args.slot.to_string(),
        &Limits {
            guest_memory_bytes: args.memory << 20,
            cpus: args.cpus as u64,
            pids: SANDBOX_PIDS,
        },
    )
    .context("creating the sandbox cgroup")?;

    let network = if args.allow_net {
        let netd = spawn(Spawn {
            name: "netd",
            uid: NETD_UID_BASE + args.slot,
            nofile: NETD_NOFILE,
            host_network: true,
            dir: &dir,
            sandbox: &mut sandbox,
            bin_dir: &bin_dir,
            null: &null,
            seccomp_log: args.seccomp_log,
        })?;
        Some(start_network(&args, netd)?)
    } else {
        None
    };
    let networked = started.elapsed();

    let vmm = spawn(Spawn {
        name: "vmm",
        uid: VMM_UID_BASE + args.slot,
        nofile: VMM_NOFILE,
        host_network: false,
        dir: &dir,
        sandbox: &mut sandbox,
        bin_dir: &bin_dir,
        null: &null,
        seccomp_log: args.seccomp_log,
    })?;
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

    let config = VmmConfig {
        cpus: args.cpus,
        memory_bytes: args.memory << 20,
        command_line,
        initrd: true,
        disk: Some(Disk { read_only: false }),
        serial: true,
        network_mac: network.as_ref().map(|network| network.mac),
        vsock_connections: VSOCK_CONNECTIONS,
        balloon: true,
    };
    let mut fds = vec![
        kvm.as_raw_fd(),
        kernel.as_raw_fd(),
        initrd.as_raw_fd(),
        disk.as_raw_fd(),
        serial.as_raw_fd(),
    ];
    if let Some(network) = &network {
        fds.push(network.vm_end.as_raw_fd());
    }
    fds.push(vsock_listener.as_raw_fd());
    vmm.configure(&Request::Configure(config), &fds)
        .context("shuru-vmm failed")?;
    drop((kvm, kernel, initrd, disk, serial, vsock_listener));
    let ready = started.elapsed();

    let mut env = HashMap::new();
    if let Some(network) = &network {
        if !network.placeholders.is_empty() {
            let guest = connect_guest(&dir.vsock(), &identity, &vmm)?;
            let ca = HashMap::from([("SHURU_PROXY_CA".to_string(), network.ca_cert_pem.clone())]);
            let argv = ["sh", "-c", INSTALL_CA].map(str::to_string).to_vec();
            if exec(guest, argv, ca)? != 0 {
                bail!("installing the proxy CA in the guest failed");
            }
            env = network.placeholders.clone();
        }
    }
    let guest = connect_guest(&dir.vsock(), &identity, &vmm)?;
    let answered = started.elapsed();
    let code = exec(guest, args.command, env)?;

    if args.time {
        let peak = sandbox
            .memory_peak()
            .map_or("unknown".into(), |bytes| format!("{} MiB", bytes >> 20));
        eprintln!(
            "shuru-node: network {} ms, jailed {} ms, VM running {} ms, guest answered {} ms, done {} ms, peak memory {peak}",
            networked.as_millis(),
            spawned.as_millis(),
            ready.as_millis(),
            answered.as_millis(),
            started.elapsed().as_millis(),
        );
    }
    vmm.stop();
    if let Some(network) = network {
        network.netd.stop();
    }
    Ok(code)
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
