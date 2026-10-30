//! The sandboxes on this host. Each keeps a directory under the state
//! directory with its settings and working disk. A running sandbox has a
//! thread of its own that boots the VM, holds it and stops it; the jailed
//! processes are its children and die with it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use shuru_proto::hello::Identity;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::http::ApiError;
use super::hub::Hubs;
use super::ServeArgs;
use crate::vm::{self, Host, KernelBoot, NetworkPolicy, Vm};
use shuru_node::cgroup;

const RECORD: &str = "sandbox.json";
const DISK: &str = "disk";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Guest connections for exec sessions; the rest of what the VM takes
/// goes to short calls, and one stays for the shutdown.
const STREAMS: usize = vm::VSOCK_CONNECTIONS * 3 / 4;
const CALLS: usize = vm::VSOCK_CONNECTIONS - STREAMS - 1;
/// More creates, deletes and boots than ever run at once.
const OPS: u32 = 1 << 20;

/// What `sandbox.json` holds.
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub created_at: u64,
    pub cpus: usize,
    pub memory_mib: u64,
}

/// Where the guest agent of a running sandbox answers, the budgets its
/// connections come from, and its exec sessions with clients.
#[derive(Clone)]
pub struct GuestAddr {
    pub vsock: PathBuf,
    pub identity: Identity,
    pub streams: Arc<Semaphore>,
    pub calls: Arc<Semaphore>,
    pub hubs: Arc<Hubs>,
}

struct Running {
    guest: GuestAddr,
    slot: u32,
    boot: u64,
    alive: Arc<AtomicBool>,
    /// Stops the VM, after shutting the guest down when true.
    stop: mpsc::Sender<bool>,
    thread: JoinHandle<()>,
}

impl Running {
    async fn stop(self, graceful: bool) {
        let _ = self.stop.send(graceful);
        let thread = self.thread;
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
    }
}

pub struct Entry {
    pub record: Record,
    dir: PathBuf,
    /// Held while the VM boots, stops or is replaced.
    vm: tokio::sync::Mutex<Option<Running>>,
    running: Arc<AtomicBool>,
    deleted: AtomicBool,
    last_running_at: AtomicU64,
    ended: Mutex<Option<Ended>>,
}

/// The last run of a VM to stop: its boot, when it stopped, in
/// milliseconds, and the CPU time it used in all.
#[derive(Clone, Copy)]
struct Ended {
    boot: u64,
    at_ms: u64,
    cpu_usec: u64,
}

/// What a VM of a sandbox has used. `boot` tells one run of the VM from
/// the next, and is `None` for a sandbox that has not run since serve
/// started; `ended_ms` is when the run stopped.
pub struct RunUsage {
    pub running: bool,
    pub boot: Option<u64>,
    pub ended_ms: Option<u64>,
    pub usage: cgroup::Usage,
}

impl Entry {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn last_running_at(&self) -> Option<u64> {
        if self.is_running() {
            return Some(unix_now());
        }
        match self.last_running_at.load(Ordering::Relaxed) {
            0 => None,
            at => Some(at),
        }
    }

    /// What the running VM has used since it booted or, once it stopped,
    /// what the last one used in all, so a meter misses nothing at the end
    /// of a run.
    pub async fn usage(&self) -> Result<RunUsage, ApiError> {
        let vm = self.vm.lock().await;
        if let Some(running) = vm
            .as_ref()
            .filter(|running| running.alive.load(Ordering::Acquire))
        {
            let usage = cgroup::usage(&running.slot.to_string())
                .map_err(|e| ApiError::internal(format!("reading the sandbox cgroup: {e}")))?;
            return Ok(RunUsage {
                running: true,
                boot: Some(running.boot),
                ended_ms: None,
                usage,
            });
        }
        drop(vm);
        let ended = *self.ended.lock().unwrap_or_else(|e| e.into_inner());
        Ok(RunUsage {
            running: false,
            boot: ended.map(|ended| ended.boot),
            ended_ms: ended.map(|ended| ended.at_ms),
            usage: cgroup::Usage {
                cpu_usec: ended.map_or(0, |ended| ended.cpu_usec),
                memory_bytes: 0,
            },
        })
    }

    /// The guest of a sandbox that is running, without booting one that is
    /// not.
    pub async fn guest_if_running(&self) -> Option<GuestAddr> {
        let vm = self.vm.lock().await;
        vm.as_ref()
            .filter(|running| running.alive.load(Ordering::Acquire))
            .map(|running| running.guest.clone())
    }
}

struct Config {
    kernel: PathBuf,
    initrd: PathBuf,
    base_disk: PathBuf,
    cpus: usize,
    memory_mib: u64,
    network: Option<NetworkPolicy>,
    ca_bundle: PathBuf,
    verbose: bool,
}

pub struct Node {
    config: Config,
    host: Host,
    dir: PathBuf,
    sandboxes: Mutex<BTreeMap<String, Arc<Entry>>>,
    slots: Mutex<BTreeSet<u32>>,
    last_boot: AtomicU64,
    /// Set once shutdown starts; no operation starts after it.
    closing: AtomicBool,
    /// A permit per create, delete or boot in flight, which shutdown waits
    /// out.
    ops: Arc<Semaphore>,
    _lock: File,
}

impl Node {
    pub fn open(args: &ServeArgs, host: Host) -> Result<Node> {
        if args.memory == 0 || args.cpus == 0 {
            bail!("--cpus and --memory must be above zero");
        }
        let last_slot = args
            .first_slot
            .checked_add(args.max_running)
            .filter(|last| *last <= 100_000)
            .context("slots must stay below 100000")?;
        for path in [&args.kernel, &args.initrd, &args.disk] {
            fs::metadata(path).with_context(|| format!("{}", path.display()))?;
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&args.state_dir)
            .with_context(|| format!("{}", args.state_dir.display()))?;
        let lock = lock(&args.state_dir.join("serve.lock"))?;
        let dir = args.state_dir.join("sandboxes");
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)?;

        let mut slots = BTreeSet::new();
        for slot in args.first_slot..last_slot {
            match vm::reclaim_slot(slot) {
                Ok(()) => {
                    slots.insert(slot);
                }
                Err(e) => eprintln!("shuru-node: skipping slot {slot}: {e:#}"),
            }
        }

        let node = Node {
            config: Config {
                kernel: args.kernel.clone(),
                initrd: args.initrd.clone(),
                base_disk: args.disk.clone(),
                cpus: args.cpus,
                memory_mib: args.memory,
                network: args.allow_net.then(|| NetworkPolicy {
                    allow: Vec::new(),
                    secrets: Vec::new(),
                    dns: args.dns,
                }),
                ca_bundle: args.ca_bundle.clone(),
                verbose: args.verbose,
            },
            host,
            dir,
            sandboxes: Mutex::new(BTreeMap::new()),
            slots: Mutex::new(slots),
            last_boot: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            ops: Arc::new(Semaphore::new(OPS as usize)),
            _lock: lock,
        };
        node.load()?;
        Ok(node)
    }

    /// Reads the sandboxes an earlier run left. None of them runs yet; the
    /// first request for one boots it.
    fn load(&self) -> Result<()> {
        let mut sandboxes = self.sandboxes();
        for item in fs::read_dir(&self.dir)? {
            let item = item?;
            let file_name = item.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with('.') {
                // A create or a delete that did not finish.
                if file_name.ends_with(".partial") || file_name.ends_with(".deleted") {
                    let _ = fs::remove_dir_all(item.path());
                }
                continue;
            }
            let record = match read_record(&item.path()) {
                Ok(record) => record,
                Err(e) => {
                    eprintln!("shuru-node: skipping {}: {e:#}", item.path().display());
                    continue;
                }
            };
            if record.id != file_name
                || !valid_name(&record.name)
                || sandboxes.contains_key(&record.name)
            {
                eprintln!("shuru-node: skipping {}", item.path().display());
                continue;
            }
            let entry = Entry::new(record, item.path());
            sandboxes.insert(entry.record.name.clone(), Arc::new(entry));
        }
        Ok(())
    }

    fn sandboxes(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Entry>>> {
        self.sandboxes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn slots(&self) -> MutexGuard<'_, BTreeSet<u32>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, name: &str) -> Result<Arc<Entry>, ApiError> {
        self.sandboxes()
            .get(name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("no sandbox {name}")))
    }

    /// Sandboxes in name order, those after `after` that start with
    /// `prefix`, at most `max` of them. The flag says whether more follow.
    pub fn list(&self, prefix: &str, after: Option<&str>, max: usize) -> (Vec<Arc<Entry>>, bool) {
        let sandboxes = self.sandboxes();
        let mut matching = sandboxes
            .range::<str, _>((
                after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                std::ops::Bound::Unbounded,
            ))
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(_, entry)| entry.clone());
        let page: Vec<_> = matching.by_ref().take(max).collect();
        let more = matching.next().is_some();
        (page, more)
    }

    /// Creates a sandbox with its own copy of the base disk and boots it.
    /// Runs to the end even if the caller goes away.
    pub async fn create(self: &Arc<Self>, name: String) -> Result<Arc<Entry>, ApiError> {
        if !valid_name(&name) {
            return Err(ApiError::bad_request(
                "a name is 1 to 63 lowercase letters, digits and dashes, not starting or ending with a dash",
            ));
        }
        let op = self.op()?;
        let node = self.clone();
        tokio::spawn(async move { node.create_now(name, op).await })
            .await
            .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Admits an operation that shutdown waits for, unless it has begun.
    fn op(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        let closing = || ApiError::unavailable("shuru-node is shutting down");
        let permit = self
            .ops
            .clone()
            .try_acquire_owned()
            .map_err(|_| closing())?;
        if self.closing.load(Ordering::SeqCst) {
            return Err(closing());
        }
        Ok(permit)
    }

    async fn create_now(
        self: Arc<Self>,
        name: String,
        _op: OwnedSemaphorePermit,
    ) -> Result<Arc<Entry>, ApiError> {
        let mut token = [0u8; 16];
        getrandom::fill(&mut token).map_err(|e| ApiError::internal(format!("{e}")))?;
        let record = Record {
            id: format!("sbx-{}", vm::hex(&token)),
            name: name.clone(),
            created_at: unix_now(),
            cpus: self.config.cpus,
            memory_mib: self.config.memory_mib,
        };
        let entry = Arc::new(Entry::new(record.clone(), self.dir.join(&record.id)));
        // Held until the sandbox runs, so nothing reaches it half made.
        let mut vm = entry.vm.lock().await;
        {
            let mut sandboxes = self.sandboxes();
            if sandboxes.contains_key(&name) {
                return Err(ApiError::conflict(format!("sandbox {name} already exists")));
            }
            sandboxes.insert(name.clone(), entry.clone());
        }
        let made = {
            let dir = self.dir.clone();
            let base = self.config.base_disk.clone();
            tokio::task::spawn_blocking(move || make(&dir, &base, &record))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|made| made)
        };
        let started = match made {
            Ok(()) => self.start(&entry, &mut vm).await,
            Err(e) => Err(ApiError::internal(format!("creating the sandbox: {e:#}"))),
        };
        if let Err(e) = started {
            if let Err(e) = self.bury(&entry).await {
                eprintln!("shuru-node: removing sandbox {}: {e:#}", entry.record.id);
            }
            return Err(e);
        }
        drop(vm);
        Ok(entry)
    }

    /// Stops the VM and removes the sandbox with its disk. Runs to the end
    /// even if the caller goes away.
    pub async fn delete(self: &Arc<Self>, name: &str) -> Result<(), ApiError> {
        let op = self.op()?;
        let entry = self.get(name)?;
        let node = self.clone();
        tokio::spawn(async move {
            let _op = op;
            let mut vm = entry.vm.lock().await;
            if entry.deleted.load(Ordering::Acquire) {
                return Err(ApiError::not_found(format!(
                    "no sandbox {}",
                    entry.record.name
                )));
            }
            if let Some(running) = vm.take() {
                running.stop(false).await;
            }
            node.bury(&entry)
                .await
                .map_err(|e| ApiError::internal(format!("removing the sandbox: {e:#}")))
        })
        .await
        .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Removes the sandbox of `entry`, whose VM is stopped: renames its
    /// directory aside, which a crash leaves for the next start to finish,
    /// then forgets the name and removes the directory.
    async fn bury(&self, entry: &Arc<Entry>) -> Result<()> {
        let dir = entry.dir.clone();
        let parent = self.dir.clone();
        let grave = self.dir.join(format!(".{}.deleted", entry.record.id));
        let buried = {
            let grave = grave.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                match fs::rename(&dir, &grave) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
                File::open(&parent)?.sync_all()?;
                Ok(())
            })
            .await?
        };
        buried?;
        entry.deleted.store(true, Ordering::Release);
        {
            // A new create may have taken the name since a failed one.
            let mut sandboxes = self.sandboxes();
            let name = &entry.record.name;
            if sandboxes
                .get(name)
                .is_some_and(|named| Arc::ptr_eq(named, entry))
            {
                sandboxes.remove(name);
            }
        }
        if let Err(e) = tokio::task::spawn_blocking(move || fs::remove_dir_all(&grave)).await? {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("shuru-node: removing {}: {e}", entry.record.id);
            }
        }
        Ok(())
    }

    /// The guest of the sandbox, booting it first if it is not running.
    /// A boot runs to the end even if the caller goes away, so the lock on
    /// the VM is never let go while one is under way.
    pub async fn guest(self: &Arc<Self>, entry: &Arc<Entry>) -> Result<GuestAddr, ApiError> {
        if let Some(guest) = entry.guest_if_running().await {
            return Ok(guest);
        }
        let op = self.op()?;
        let node = self.clone();
        let entry = entry.clone();
        tokio::spawn(async move {
            let _op = op;
            let mut vm = entry.vm.lock().await;
            if entry.deleted.load(Ordering::Acquire) {
                return Err(ApiError::not_found(format!(
                    "no sandbox {}",
                    entry.record.name
                )));
            }
            if let Some(running) = vm.as_ref() {
                if running.alive.load(Ordering::Acquire) {
                    return Ok(running.guest.clone());
                }
            }
            node.start(&entry, &mut vm).await
        })
        .await
        .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Boots the VM of `entry` on a thread of its own. The thread stops the
    /// VM when told to, when the VM exits, or when nobody is left to take it
    /// because the caller went away while it booted.
    async fn start(
        self: &Arc<Self>,
        entry: &Arc<Entry>,
        vm: &mut Option<Running>,
    ) -> Result<GuestAddr, ApiError> {
        if let Some(gone) = vm.take() {
            gone.stop(false).await;
        }
        let slot = self
            .slots()
            .pop_first()
            .ok_or_else(|| ApiError::unavailable("this host runs as many sandboxes as it may"))?;
        let boot = self.next_boot();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let alive = entry.running.clone();
        let thread = {
            let node = self.clone();
            let entry = entry.clone();
            let alive = alive.clone();
            std::thread::Builder::new()
                .name(format!("vm-{}", entry.record.name))
                .spawn(move || node.hold(&entry, slot, boot, alive, ready_tx, stop_rx))
        };
        let thread = match thread {
            Ok(thread) => thread,
            Err(e) => {
                self.slots().insert(slot);
                return Err(ApiError::internal(format!("starting the VM thread: {e}")));
            }
        };
        let guest = match ready_rx.await {
            Ok(Ok(guest)) => guest,
            Ok(Err(e)) => {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
                return Err(ApiError::internal(format!(
                    "booting sandbox {}: {e:#}",
                    entry.record.name
                )));
            }
            Err(_) => return Err(ApiError::internal("the VM thread ended")),
        };
        entry.last_running_at.store(unix_now(), Ordering::Relaxed);
        *vm = Some(Running {
            guest: guest.clone(),
            slot,
            boot,
            alive,
            stop: stop_tx,
            thread,
        });
        Ok(guest)
    }

    /// A number for a VM start, above that of any start before it: the
    /// time in milliseconds, or one past the last when the clock has not
    /// moved on.
    fn next_boot(&self) -> u64 {
        let now = unix_millis();
        let next = |last: u64| now.max(last + 1);
        self.last_boot
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                Some(next(last))
            })
            .map_or(now, next)
    }

    fn hold(
        &self,
        entry: &Entry,
        slot: u32,
        boot: u64,
        alive: Arc<AtomicBool>,
        ready: tokio::sync::oneshot::Sender<Result<GuestAddr>>,
        stop: mpsc::Receiver<bool>,
    ) {
        let name = &entry.record.name;
        let vm = match self.boot(entry, slot) {
            Ok(vm) => vm,
            Err(e) => {
                self.release(slot);
                let _ = ready.send(Err(e));
                return;
            }
        };
        let guest = GuestAddr {
            vsock: vm.dir.vsock(),
            identity: vm.identity,
            streams: Arc::new(Semaphore::new(STREAMS)),
            calls: Arc::new(Semaphore::new(CALLS)),
            hubs: Arc::default(),
        };
        alive.store(true, Ordering::Release);
        let graceful = if ready.send(Ok(guest)).is_err() {
            false
        } else {
            loop {
                match stop.recv_timeout(WATCH_INTERVAL) {
                    Ok(graceful) => break graceful,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break false,
                    Err(mpsc::RecvTimeoutError::Timeout) if vm.vmm.exited() => {
                        eprintln!("shuru-node: the VM of sandbox {name} exited");
                        break false;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        };
        alive.store(false, Ordering::Release);
        if graceful {
            if let Err(e) = shut_down(&vm) {
                eprintln!("shuru-node: shutting down sandbox {name}: {e:#}");
            }
        }
        if let Some(used) = vm.stop() {
            *entry.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ended {
                boot,
                at_ms: unix_millis(),
                cpu_usec: used.cpu_usec,
            });
        }
        self.release(slot);
    }

    /// Hands `slot` back once nothing of its last VM is left, or keeps it
    /// out of use.
    fn release(&self, slot: u32) {
        match vm::reclaim_slot(slot) {
            Ok(()) => {
                self.slots().insert(slot);
            }
            Err(e) => eprintln!("shuru-node: taking slot {slot} out of use: {e:#}"),
        }
    }

    fn boot(&self, entry: &Entry, slot: u32) -> Result<Vm> {
        let config = &self.config;
        let disk = entry.dir.join(DISK);
        let vm = Vm::boot(
            &self.host,
            &KernelBoot {
                slot,
                kernel: &config.kernel,
                initrd: &config.initrd,
                disk: &disk,
                cpus: entry.record.cpus,
                memory_bytes: entry.record.memory_mib << 20,
                network: config.network.as_ref(),
                ca_bundle: &config.ca_bundle,
                verbose: config.verbose,
            },
            Instant::now(),
            &mut Vec::new(),
        )?;
        drop(vm.connect()?);
        Ok(vm)
    }

    /// Waits for the creates, deletes and boots under way, then shuts every
    /// running guest down, so its disk is synced, and stops the VMs.
    pub async fn shut_down(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let _drained = self.ops.acquire_many(OPS).await;
        let entries: Vec<_> = self.sandboxes().values().cloned().collect();
        let mut stopping = tokio::task::JoinSet::new();
        for entry in entries {
            stopping.spawn(async move {
                if let Some(running) = entry.vm.lock().await.take() {
                    running.stop(true).await;
                }
            });
        }
        while stopping.join_next().await.is_some() {}
    }
}

impl Entry {
    fn new(record: Record, dir: PathBuf) -> Entry {
        Entry {
            record,
            dir,
            vm: tokio::sync::Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            deleted: AtomicBool::new(false),
            last_running_at: AtomicU64::new(0),
            ended: Mutex::new(None),
        }
    }
}

fn shut_down(vm: &Vm) -> Result<()> {
    let mut guest = vm::try_connect_guest(&vm.dir.vsock(), &vm.identity)?
        .context("the guest did not take the connection")?;
    vm::shut_down_guest(&mut guest, SHUTDOWN_TIMEOUT)
}

/// Takes the lock on the state directory, so one `serve` uses it at a
/// time.
fn lock(path: &Path) -> Result<File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("{}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another shuru-node serve holds {}", path.display());
    }
    Ok(file)
}

/// Writes the sandbox directory under another name, syncs it and renames
/// it into place, so a crash never leaves one that looks complete.
fn make(dir: &Path, base_disk: &Path, record: &Record) -> Result<()> {
    let staging = dir.join(format!(".{}.partial", record.id));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .with_context(|| format!("{}", staging.display()))?;
    let made = (|| {
        let disk = crate::clone_file(base_disk, &staging.join(DISK))?;
        let file = crate::create_private(&staging.join(RECORD))?;
        serde_json::to_writer(&file, record)?;
        for file in [&disk, &file] {
            file.sync_all()?;
        }
        File::open(&staging)?.sync_all()?;
        fs::rename(&staging, dir.join(&record.id))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if made.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    made
}

fn read_record(dir: &Path) -> Result<Record> {
    let file = vm::open(&dir.join(RECORD))?;
    serde_json::from_reader(file).context("reading the sandbox record")
}

fn valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for name in ["a", "web-1", "0", &"a".repeat(63)] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "-a", "a-", "A", "a_b", "a.b", "a/b", &"a".repeat(64)] {
            assert!(!valid_name(name), "{name}");
        }
    }
}
