//! A jailed VM and what it takes to boot one: the sandbox directory, the
//! jailed shuru-vmm and shuru-netd, the guest network and the guest
//! connection. Shared by `run`, `restore` and `serve`.

use std::cell::Cell;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use shuru_node::cgroup::{Limits, Sandbox};
use shuru_node::control::{self, Boot, Disk, NetdConfig, Request, Response, Secret, VmmConfig};
use shuru_node::firewall::{self, NETD_UID_BASE};
use shuru_node::jail::{self, Handoff, Jail};
use shuru_proto::frame;
use shuru_proto::hello::Identity;

const RUN_DIR: &str = "/run/shuru";
/// Each slot is one byte of this file, held with an open file description
/// lock. The lock goes with the last descriptor of the open, whether the
/// holder closes it or dies, so no file is left behind per slot.
const SLOT_LOCKS: &str = "slots.lock";
const VMM_UID_BASE: u32 = 100_000;
pub const VSOCK_CONNECTIONS: usize = 128;
const VMM_NOFILE: u64 = 512;
// 256 proxied connections, each with a guest and an upstream side.
const NETD_NOFILE: u64 = 1024;
const NPROC: u64 = 64;
const SANDBOX_PIDS: u64 = 2 * NPROC;
const GUEST_PORT: u32 = 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// The sandbox directory: a control socket per jailed process, the vsock
/// socket, and the empty mount point for their roots. Only root can enter
/// it. It holds the lock on its slot, which every command that runs a VM
/// takes, until the VM and its cgroup are gone.
pub struct RunDir {
    path: PathBuf,
    _lock: File,
}

impl RunDir {
    /// Takes the slot, clearing what a VM that is gone left in it.
    pub fn create(slot: u32) -> Result<Self> {
        let lock = lock_slot(slot)?;
        clear_slot(slot)?;
        let path = slot_dir(slot);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("{}", path.display()))?;
        let dir = RunDir { path, _lock: lock };
        fs::DirBuilder::new().mode(0o555).create(dir.root())?;
        Ok(dir)
    }

    fn root(&self) -> PathBuf {
        self.path.join("root")
    }

    fn control(&self, name: &str) -> PathBuf {
        self.path.join(format!("{name}.sock"))
    }

    pub fn vsock(&self) -> PathBuf {
        self.path.join("vsock.sock")
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        remove_slot_dir(&self.path);
    }
}

fn slot_dir(slot: u32) -> PathBuf {
    Path::new(RUN_DIR).join(format!("sbx-{slot}"))
}

fn remove_slot_dir(path: &Path) {
    for name in ["vmm.sock", "netd.sock", "vsock.sock"] {
        let _ = fs::remove_file(path.join(name));
    }
    let _ = fs::remove_dir(path.join("root"));
    let _ = fs::remove_dir(path);
}

fn lock_slot(slot: u32) -> Result<File> {
    lock_slot_in(Path::new(RUN_DIR), slot)
}

fn lock_slot_in(base: &Path, slot: u32) -> Result<File> {
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(base)?;
    let path = base.join(SLOT_LOCKS);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("{}", path.display()))?;
    let byte = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: libc::off_t::from(slot),
        l_len: 1,
        l_pid: 0,
    };
    if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_OFD_SETLK, &byte) } != 0 {
        let e = io::Error::last_os_error();
        if matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
            bail!("slot {slot} is in use");
        }
        return Err(e).with_context(|| format!("locking slot {slot}"));
    }
    Ok(lock)
}

/// Removes what a VM that is gone left in a slot whose lock is held: its
/// cgroup, refused while it still holds a process, and its directory.
fn clear_slot(slot: u32) -> Result<()> {
    shuru_node::cgroup::remove_empty(&slot.to_string())
        .with_context(|| format!("slot {slot} has a cgroup still in use"))?;
    let path = slot_dir(slot);
    if fs::symlink_metadata(&path).is_ok() {
        remove_slot_dir(&path);
        if fs::symlink_metadata(&path).is_ok() {
            bail!("slot {slot} keeps {}", path.display());
        }
    }
    Ok(())
}

/// Clears a slot no one holds, so a VM can start in it.
pub fn reclaim_slot(slot: u32) -> Result<()> {
    let _lock = lock_slot(slot)?;
    clear_slot(slot)
}

/// A jailed process and its control connection. Killed and reaped when
/// dropped.
pub struct Jailed {
    pub pid: i32,
    reaped: Cell<bool>,
    control: UnixStream,
}

impl Jailed {
    pub fn exited(&self) -> bool {
        let mut status = 0;
        if !self.reaped.get()
            && unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid
        {
            self.reaped.set(true);
        }
        self.reaped.get()
    }

    pub fn configure(&self, request: &Request, fds: &[i32]) -> Result<Response> {
        control::send(&self.control, request, fds)?;
        match control::receive::<Response>(&self.control, 0)? {
            Some((Response::Error { message }, _)) => bail!("{message}"),
            Some((response, _)) => Ok(response),
            None => bail!("exited while starting"),
        }
    }

    pub fn stop(self) {
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
    host: &'a Host,
}

fn spawn(spawn: Spawn) -> Result<Jailed> {
    let name = format!("shuru-{}", spawn.name);
    let path = spawn.host.bin_dir.join(&name);
    let binary = File::open(&path).with_context(|| format!("{}", path.display()))?;
    let listener: OwnedFd = UnixListener::bind(spawn.dir.control(spawn.name))?.into();
    let procs = spawn.sandbox.child(spawn.name)?;
    let root = CString::new(spawn.dir.root().as_os_str().as_bytes())?;
    let argv0 = CString::new(name.as_str())?;
    let mut argv = vec![argv0.as_c_str()];
    if spawn.host.seccomp_log {
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
            null: &spawn.host.null,
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
pub struct NetworkPolicy {
    pub allow: Vec<String>,
    /// NAME@HOST[,HOST], valued from the environment at each start.
    pub secrets: Vec<String>,
    pub dns: Ipv4Addr,
}

pub struct Network {
    pub netd: Jailed,
    pub vm_end: OwnedFd,
    pub mac: [u8; 6],
    pub session: shuru_proxy::Session,
}

/// Configures `netd`. A restore passes the session and MAC the guest was
/// saved with: it trusts that CA, holds those placeholders and has the
/// gateway in its neighbour table under that MAC.
pub fn start_network(
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
pub struct Host {
    bin_dir: PathBuf,
    pub kvm: OwnedFd,
    null: File,
    pub serial: OwnedFd,
    seccomp_log: bool,
}

impl Host {
    /// `verbose` shows the guest console, guest token included.
    /// `seccomp_log` logs the calls the filters forbid instead of killing.
    pub fn open(bin_dir: Option<&Path>, verbose: bool, seccomp_log: bool) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("shuru-node runs as root");
        }
        let bin_dir = match bin_dir {
            Some(dir) => dir.to_path_buf(),
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
        let serial = if verbose {
            io::stderr().as_fd().try_clone_to_owned()?
        } else {
            null.try_clone()?.into()
        };
        Ok(Host {
            bin_dir,
            kvm,
            null,
            serial,
            seccomp_log,
        })
    }

    pub fn sandbox(&self, slot: u32, memory_bytes: u64, cpus: usize) -> Result<Sandbox> {
        Sandbox::create(
            &slot.to_string(),
            &Limits {
                guest_memory_bytes: memory_bytes,
                cpus: cpus as u64,
                pids: SANDBOX_PIDS,
            },
        )
        .context("creating the sandbox cgroup")
    }

    pub fn spawn_netd(&self, slot: u32, dir: &RunDir, sandbox: &mut Sandbox) -> Result<Jailed> {
        firewall::install()?;
        spawn(Spawn {
            name: "netd",
            uid: NETD_UID_BASE + slot,
            nofile: NETD_NOFILE,
            host_network: true,
            dir,
            sandbox,
            host: self,
        })
    }

    pub fn spawn_vmm(&self, slot: u32, dir: &RunDir, sandbox: &mut Sandbox) -> Result<Jailed> {
        spawn(Spawn {
            name: "vmm",
            uid: VMM_UID_BASE + slot,
            nofile: VMM_NOFILE,
            host_network: false,
            dir,
            sandbox,
            host: self,
        })
    }
}

pub fn open(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("{}", path.display()))
}

/// A kernel to boot in a fresh jail.
pub struct KernelBoot<'a> {
    pub slot: u32,
    pub kernel: &'a Path,
    pub initrd: &'a Path,
    /// The working disk. The guest writes to it.
    pub disk: &'a Path,
    pub cpus: usize,
    pub memory_bytes: u64,
    /// A network through a jailed shuru-netd, reaching public addresses
    /// only.
    pub network: Option<&'a NetworkPolicy>,
    pub ca_bundle: &'a Path,
    pub verbose: bool,
}

/// A running VM. Dropping it kills the VMM before netd, and both before
/// their cgroup and directory go.
pub struct Vm {
    pub vmm: Jailed,
    pub network: Option<Network>,
    pub sandbox: Sandbox,
    pub dir: RunDir,
    pub identity: Identity,
}

impl Vm {
    /// Returns once the VMM runs the kernel, before the guest agent
    /// answers. `times` gets when the network, the jails and the VM were
    /// ready, counted from `started`.
    pub fn boot(
        host: &Host,
        spec: &KernelBoot,
        started: Instant,
        times: &mut Vec<(&'static str, Duration)>,
    ) -> Result<Vm> {
        let kernel = open(spec.kernel)?;
        let initrd = open(spec.initrd)?;
        let disk = OpenOptions::new()
            .read(true)
            .write(true)
            .open(spec.disk)
            .with_context(|| format!("{}", spec.disk.display()))?;

        let dir = RunDir::create(spec.slot)?;
        let vsock_listener = UnixListener::bind(dir.vsock())?;
        let mut sandbox = host.sandbox(spec.slot, spec.memory_bytes, spec.cpus)?;

        let network = match spec.network {
            Some(policy) => {
                let netd = host.spawn_netd(spec.slot, &dir, &mut sandbox)?;
                Some(start_network(policy, spec.ca_bundle, netd, None)?)
            }
            None => None,
        };
        times.push(("network", started.elapsed()));

        let vmm = host.spawn_vmm(spec.slot, &dir, &mut sandbox)?;
        times.push(("jailed", started.elapsed()));

        let identity = new_identity()?;
        let quiet = if spec.verbose { "" } else { " quiet" };
        let command_line = format!(
            "console=ttyAMA0 root=/dev/vda rw{quiet} shuru.token={} shuru.instance={}",
            hex(&identity.token),
            hex(&identity.instance)
        );
        let config = VmmConfig {
            cpus: spec.cpus,
            memory_bytes: spec.memory_bytes,
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
        times.push(("VM running", started.elapsed()));
        Ok(Vm {
            vmm,
            network,
            sandbox,
            dir,
            identity,
        })
    }

    /// A new guest connection, waiting for the agent to listen.
    pub fn connect(&self) -> Result<UnixStream> {
        connect_guest(&self.dir.vsock(), &self.identity, &self.vmm)
    }

    pub fn stop(self) {
        let Vm {
            vmm,
            network,
            sandbox,
            dir,
            ..
        } = self;
        vmm.stop();
        if let Some(network) = network {
            network.netd.stop();
        }
        drop((sandbox, dir));
    }
}

pub fn new_identity() -> Result<Identity> {
    let mut identity = Identity {
        token: [0; 32],
        instance: [0; 16],
    };
    getrandom::fill(&mut identity.token).map_err(|e| anyhow::anyhow!("{e}"))?;
    getrandom::fill(&mut identity.instance).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(identity)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex<const N: usize>(text: &str) -> Result<[u8; N]> {
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
pub fn connect_guest(path: &Path, identity: &Identity, vmm: &Jailed) -> Result<UnixStream> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if vmm.exited() {
            bail!("the VMM exited before the guest answered");
        }
        if Instant::now() > deadline {
            bail!("the guest did not answer within {BOOT_TIMEOUT:?}");
        }
        if let Some(stream) = try_connect_guest(path, identity)? {
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn try_connect_guest(path: &Path, identity: &Identity) -> Result<Option<UnixStream>> {
    try_connect_guest_port(path, identity, GUEST_PORT)
}

pub fn try_connect_guest_port(
    path: &Path,
    identity: &Identity,
    port: u32,
) -> Result<Option<UnixStream>> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("CONNECT {port}\n").as_bytes())?;
    if !read_line(&mut stream)?.is_some_and(|line| line.starts_with("OK ")) {
        return Ok(None);
    }
    identity
        .connect(&mut stream)
        .context("the guest did not answer the connection hello")?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(Some(stream))
}

/// Asks the guest to stop its services and sync its disk, waiting at most
/// `timeout` for the answer.
pub fn shut_down_guest(guest: &mut UnixStream, timeout: Duration) -> Result<()> {
    guest.set_read_timeout(Some(timeout))?;
    guest.set_write_timeout(Some(timeout))?;
    frame::write_frame(guest, frame::SHUTDOWN_REQ, &[])?;
    match frame::read_frame(guest)? {
        Some((frame::SHUTDOWN_RESP, _)) => Ok(()),
        Some((frame::ERROR, payload)) => bail!("{}", String::from_utf8_lossy(&payload)),
        Some((kind, _)) => bail!("the guest answered the shutdown with frame {kind:#x}"),
        None => bail!("the guest closed the connection"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_held_until_its_lock_is_closed() {
        let base = std::env::temp_dir().join(format!("shuru-slots-{}", std::process::id()));
        let first = lock_slot_in(&base, 1000).unwrap();
        assert!(lock_slot_in(&base, 1000).is_err());
        let other = lock_slot_in(&base, 1001).unwrap();
        drop(first);
        let again = lock_slot_in(&base, 1000).unwrap();
        drop((again, other));
        let names: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [SLOT_LOCKS]);
        fs::remove_dir_all(&base).unwrap();
    }
}
