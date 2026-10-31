//! The sandboxes on this host. Each keeps a directory under the state
//! directory with its settings, its config and its working disk. A running
//! sandbox has a thread of its own that boots the VM, holds it and stops
//! it; the jailed processes are its children and die with it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Write;
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

use super::config::{self, SandboxConfig};
use super::guest;
use super::http::ApiError;
use super::hub::Hubs;
use super::idle::{Activity, Busy};
use super::ServeArgs;
use crate::vm::{self, Host, KernelBoot, NetworkPolicy, Vm};
use shuru_node::cgroup;

const RECORD: &str = "sandbox.json";
const DISK: &str = "disk";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
/// Guest connections for exec sessions; the rest of what the VM takes
/// goes to short calls, and one stays for the shutdown.
const STREAMS: usize = vm::VSOCK_CONNECTIONS * 3 / 4;
const CALLS: usize = vm::VSOCK_CONNECTIONS - STREAMS - 1;
/// More creates, deletes and boots than ever run at once.
const OPS: u32 = 1 << 20;

/// The settings of a sandbox.
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub created_at: u64,
    pub cpus: usize,
    pub memory_mib: u64,
}

/// What `sandbox.json` holds: the record, and the boot of the last run,
/// written before the VM runs, so a restart still knows whether the disk
/// may have changed since.
#[derive(Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    record: Record,
    #[serde(default)]
    last_boot: u64,
}

/// Where the disk of a new sandbox comes from.
enum NewDisk {
    /// A copy of the base disk.
    Base(PathBuf),
    /// A file under the import directory, moved in.
    Import(PathBuf),
}

/// A copy of the disk of a stopped sandbox, for the caller to keep.
pub struct Parked {
    pub disk: PathBuf,
    pub boot: u64,
    pub size: u64,
}

/// A copy of the disk of a sandbox, running or not, for the caller to
/// keep.
pub struct Checkpoint {
    pub disk: PathBuf,
    pub size: u64,
}

/// Where the guest agent of a running sandbox answers, the budgets its
/// connections come from, its exec sessions with clients, and the
/// placeholders its secrets were given at boot.
#[derive(Clone)]
pub struct GuestAddr {
    pub vsock: PathBuf,
    pub identity: Identity,
    pub streams: Arc<Semaphore>,
    pub calls: Arc<Semaphore>,
    pub hubs: Arc<Hubs>,
    pub placeholders: Arc<HashMap<String, String>>,
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
    /// The boot of the last run, 0 if none since the sandbox was made or
    /// imported.
    last_boot: AtomicU64,
    ended: Mutex<Option<Ended>>,
    activity: Arc<Activity>,
    config: Mutex<Arc<SandboxConfig>>,
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

    pub fn activity(&self) -> &Arc<Activity> {
        &self.activity
    }

    /// Keeps the sandbox from an idle park until dropped.
    pub fn busy(&self) -> Busy {
        self.activity.busy()
    }

    pub fn config(&self) -> Arc<SandboxConfig> {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
    parked: PathBuf,
    import: PathBuf,
    checkpoints: PathBuf,
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
        let parked = args.state_dir.join("parked");
        let import = args.state_dir.join("import");
        let checkpoints = args.state_dir.join("checkpoints");
        for dir in [&dir, &parked, &import, &checkpoints] {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(dir)
                .with_context(|| format!("{}", dir.display()))?;
        }
        let parked = fs::canonicalize(parked)?;
        let import = fs::canonicalize(import)?;
        let checkpoints = fs::canonicalize(checkpoints)?;
        for item in fs::read_dir(&parked)? {
            let path = item?.path();
            if path.extension().is_some_and(|ext| ext == "partial") {
                let _ = fs::remove_file(path);
            }
        }
        // Copies handed to callers of an earlier run, which are gone.
        for item in fs::read_dir(&checkpoints)? {
            let _ = fs::remove_file(item?.path());
        }

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
            parked,
            import,
            checkpoints,
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
    /// first request for one boots it. Boots go on from the last any of them
    /// had, should the clock have gone back.
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
            let read = read_record(&item.path())
                .and_then(|stored| Ok((stored, config::read(&item.path())?)));
            let (Stored { record, last_boot }, config) = match read {
                Ok(read) => read,
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
            self.last_boot.fetch_max(last_boot, Ordering::Relaxed);
            let entry = Entry::new(record, item.path(), last_boot, config);
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

    pub fn entries(&self) -> Vec<Arc<Entry>> {
        self.sandboxes().values().cloned().collect()
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
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

    /// Creates a sandbox with its own copy of the base disk, or with `disk`
    /// moved in from the import directory, and boots it if `start`. Runs to
    /// the end even if the caller goes away.
    pub async fn create(
        self: &Arc<Self>,
        name: String,
        disk: Option<&str>,
        config: SandboxConfig,
        start: bool,
    ) -> Result<Arc<Entry>, ApiError> {
        if !valid_name(&name) {
            return Err(ApiError::bad_request(
                "a name is 1 to 63 lowercase letters, digits and dashes, not starting or ending with a dash",
            ));
        }
        self.check_config(&config)?;
        let disk = match disk {
            Some(disk) => NewDisk::Import(import_path(&self.import, disk)?),
            None => NewDisk::Base(self.config.base_disk.clone()),
        };
        let op = self.op()?;
        let node = self.clone();
        tokio::spawn(async move { node.create_now(name, disk, config, start, op).await })
            .await
            .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Secrets reach a guest only through the proxy of its network.
    fn check_config(&self, config: &SandboxConfig) -> Result<(), ApiError> {
        if !config.secrets.is_empty() && self.config.network.is_none() {
            return Err(ApiError::bad_request(
                "secrets: this host gives sandboxes no network (--allow-net)",
            ));
        }
        Ok(())
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
        disk: NewDisk,
        config: SandboxConfig,
        start: bool,
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
        let entry = Arc::new(Entry::new(
            record.clone(),
            self.dir.join(&record.id),
            0,
            config,
        ));
        // Held until the sandbox runs, so nothing reaches it half made.
        let mut vm = entry.vm.lock().await;
        {
            let mut sandboxes = self.sandboxes();
            if sandboxes.contains_key(&name) {
                return Err(ApiError::conflict(format!("sandbox {name} already exists")));
            }
            sandboxes.insert(name.clone(), entry.clone());
        }
        let imported = match &disk {
            NewDisk::Import(path) => Some(path.clone()),
            NewDisk::Base(_) => None,
        };
        let made = {
            let dir = self.dir.clone();
            let config = entry.config();
            tokio::task::spawn_blocking(move || make(&dir, &disk, &record, &config))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|made| made)
        };
        let started = match made {
            Ok(()) if start => self.start(&entry, &mut vm).await.map(drop),
            Ok(()) => Ok(()),
            Err(e) => Err(ApiError::internal(format!("creating the sandbox: {e:#}"))),
        };
        if let Err(e) = started {
            if let Some(path) = imported {
                // Back where it was, so the caller still has it.
                let _ = fs::rename(entry.dir.join(DISK), path);
            }
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
            present(&entry)?;
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

    /// Shuts the guest down, so its disk is synced, and stops the VM. The
    /// sandbox and its disk stay; the next request that needs the guest
    /// boots it again. Stopping a sandbox that is not running does nothing.
    pub async fn stop(self: &Arc<Self>, name: &str) -> Result<(), ApiError> {
        let op = self.op()?;
        let entry = self.get(name)?;
        stop_when(op, entry, |_| true).await.map(drop)
    }

    /// Stops the VM of a sandbox nothing has used for `after`, as `stop`
    /// does. It looks again with the lock on the VM held, so a request that
    /// came in meanwhile keeps it running. True means it stopped.
    pub async fn stop_idle(
        self: &Arc<Self>,
        entry: &Arc<Entry>,
        after: Duration,
    ) -> Result<bool, ApiError> {
        let op = self.op()?;
        stop_when(op, entry.clone(), move |entry| {
            entry.activity.idle(Instant::now(), after)
        })
        .await
    }

    /// Stops the VM as `stop` does, then, with the lock on the VM still
    /// held so nothing boots it meanwhile, copies the disk to
    /// `parked/<name>.disk` in place of any copy there.
    pub async fn park(self: &Arc<Self>, name: &str) -> Result<Parked, ApiError> {
        let op = self.op()?;
        let entry = self.get(name)?;
        let parked = self.parked.clone();
        tokio::spawn(async move {
            let _op = op;
            let mut vm = entry.vm.lock().await;
            present(&entry)?;
            if let Some(running) = vm.take() {
                running.stop(true).await;
            }
            let boot = entry.last_boot.load(Ordering::Acquire);
            let disk = entry.dir.join(DISK);
            let name = entry.record.name.clone();
            let (disk, size) =
                tokio::task::spawn_blocking(move || park_copy(&disk, &parked, &name))
                    .await
                    .map_err(|e| ApiError::internal(format!("{e}")))?
                    .map_err(|e| ApiError::internal(format!("parking the disk: {e:#}")))?;
            drop(vm);
            Ok(Parked { disk, boot, size })
        })
        .await
        .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Replaces the env and secrets of the sandbox. Env reaches the next
    /// exec; secrets reach the next boot, since netd holds them for the
    /// run.
    pub async fn configure(
        self: &Arc<Self>,
        name: &str,
        config: SandboxConfig,
    ) -> Result<(), ApiError> {
        self.check_config(&config)?;
        let op = self.op()?;
        let entry = self.get(name)?;
        tokio::spawn(async move {
            let _op = op;
            // Held so the sandbox directory is in place and stays.
            let _vm = entry.vm.lock().await;
            present(&entry)?;
            let config = Arc::new(config);
            let saved = {
                let dir = entry.dir.clone();
                let config = config.clone();
                tokio::task::spawn_blocking(move || replace_json(&dir, config::CONFIG, &*config))
                    .await
                    .map_err(|e| ApiError::internal(format!("{e}")))?
            };
            saved.map_err(|e| ApiError::internal(format!("saving the config: {e:#}")))?;
            *entry.config.lock().unwrap_or_else(|e| e.into_inner()) = config;
            Ok(())
        })
        .await
        .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Copies the disk to `checkpoints/<name>.<random>.disk` for the
    /// caller, who deletes it, with the lock on the VM held so nothing
    /// boots or stops it meanwhile. A running sandbox syncs first and keeps
    /// running; its copy is the disk as a crash at that instant would leave
    /// it, which the ext4 journal recovers from.
    pub async fn checkpoint(self: &Arc<Self>, name: &str) -> Result<Checkpoint, ApiError> {
        let op = self.op()?;
        let entry = self.get(name)?;
        let checkpoints = self.checkpoints.clone();
        tokio::spawn(async move {
            let _op = op;
            let vm = entry.vm.lock().await;
            present(&entry)?;
            let live = vm
                .as_ref()
                .filter(|running| running.alive.load(Ordering::Acquire))
                .map(|running| running.guest.clone());
            if let Some(addr) = &live {
                tokio::time::timeout(SYNC_TIMEOUT, guest::sync(addr))
                    .await
                    .map_err(|_| ApiError::internal("the guest did not sync in time"))??;
            }
            let disk = entry.dir.join(DISK);
            let name = entry.record.name.clone();
            let live = live.is_some();
            let (disk, size) = tokio::task::spawn_blocking(move || {
                checkpoint_copy(&disk, &checkpoints, &name, live)
            })
            .await
            .map_err(|e| ApiError::internal(format!("{e}")))?
            .map_err(|e| ApiError::internal(format!("copying the disk: {e:#}")))?;
            drop(vm);
            Ok(Checkpoint { disk, size })
        })
        .await
        .map_err(|e| ApiError::internal(format!("{e}")))?
    }

    /// Stops the VM as `stop` does, then, with the lock on the VM still
    /// held, renames `disk` from the import directory over the sandbox
    /// disk. The sandbox is left cold at a new boot, so a host sees that
    /// its disk changed; the next request boots it.
    pub async fn restore(self: &Arc<Self>, name: &str, disk: &str) -> Result<Arc<Entry>, ApiError> {
        let disk = import_path(&self.import, disk)?;
        let op = self.op()?;
        let entry = self.get(name)?;
        let node = self.clone();
        tokio::spawn(async move {
            let _op = op;
            let mut vm = entry.vm.lock().await;
            present(&entry)?;
            if let Some(running) = vm.take() {
                running.stop(true).await;
            }
            let boot = node.next_boot();
            entry.last_boot.store(boot, Ordering::Release);
            let restored = {
                let dir = entry.dir.clone();
                let record = entry.record.clone();
                tokio::task::spawn_blocking(move || restore_disk(&dir, record, boot, &disk))
                    .await
                    .map_err(|e| ApiError::internal(format!("{e}")))?
            };
            restored.map_err(|e| ApiError::internal(format!("restoring the disk: {e:#}")))?;
            drop(vm);
            Ok(entry)
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
            present(&entry)?;
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
        let saved = {
            let dir = entry.dir.clone();
            let record = entry.record.clone();
            tokio::task::spawn_blocking(move || save_boot(&dir, record, boot))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|saved| saved)
        };
        if let Err(e) = saved {
            self.slots().insert(slot);
            return Err(ApiError::internal(format!(
                "saving the sandbox record: {e:#}"
            )));
        }
        entry.last_boot.store(boot, Ordering::Release);
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
        entry.activity.restart();
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

    fn next_boot(&self) -> u64 {
        boot_after(&self.last_boot, unix_millis())
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
            placeholders: Arc::new(
                vm.network
                    .as_ref()
                    .map(|network| network.session.placeholders.clone())
                    .unwrap_or_default(),
            ),
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

    /// Boots the VM with the node network and the secrets of the sandbox.
    /// A guest given secrets trusts the proxy CA before it counts as
    /// running.
    fn boot(&self, entry: &Entry, slot: u32) -> Result<Vm> {
        let config = &self.config;
        let disk = entry.dir.join(DISK);
        let secrets = entry.config().netd_secrets();
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
                secrets: &secrets,
                ca_bundle: &config.ca_bundle,
                verbose: config.verbose,
            },
            Instant::now(),
            &mut Vec::new(),
        )?;
        drop(vm.connect()?);
        if let Some(network) = &vm.network {
            if !network.session.placeholders.is_empty() {
                vm.install_ca(&network.session.ca_cert_pem)?;
            }
        }
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
    fn new(record: Record, dir: PathBuf, last_boot: u64, config: SandboxConfig) -> Entry {
        Entry {
            record,
            dir,
            vm: tokio::sync::Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            deleted: AtomicBool::new(false),
            last_running_at: AtomicU64::new(0),
            last_boot: AtomicU64::new(last_boot),
            ended: Mutex::new(None),
            activity: Arc::default(),
            config: Mutex::new(Arc::new(config)),
        }
    }
}

/// A number for a VM start, or a restore, above that of any before it:
/// `now` in milliseconds, or one past `last` when the clock has not moved
/// on.
fn boot_after(last: &AtomicU64, now: u64) -> u64 {
    let mut seen = last.load(Ordering::Relaxed);
    loop {
        let boot = now.max(seen + 1);
        match last.compare_exchange_weak(seen, boot, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return boot,
            Err(newer) => seen = newer,
        }
    }
}

/// Refuses a sandbox deleted while its caller waited for the lock on its
/// VM.
fn present(entry: &Entry) -> Result<(), ApiError> {
    if entry.deleted.load(Ordering::Acquire) {
        return Err(ApiError::not_found(format!(
            "no sandbox {}",
            entry.record.name
        )));
    }
    Ok(())
}

/// Stops the VM of `entry` as `Node::stop` does, if `when` still holds
/// once the lock on the VM is held. Runs to the end even if the caller goes
/// away.
async fn stop_when(
    op: OwnedSemaphorePermit,
    entry: Arc<Entry>,
    when: impl FnOnce(&Entry) -> bool + Send + 'static,
) -> Result<bool, ApiError> {
    tokio::spawn(async move {
        let _op = op;
        let mut vm = entry.vm.lock().await;
        present(&entry)?;
        if !when(&entry) {
            return Ok(false);
        }
        if let Some(running) = vm.take() {
            running.stop(true).await;
        }
        Ok(true)
    })
    .await
    .map_err(|e| ApiError::internal(format!("{e}")))?
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
/// it into place, so a crash never leaves one that looks complete. An
/// imported disk goes back where it was if this fails.
fn make(dir: &Path, disk: &NewDisk, record: &Record, config: &SandboxConfig) -> Result<()> {
    let staging = dir.join(format!(".{}.partial", record.id));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .with_context(|| format!("{}", staging.display()))?;
    let made = (|| {
        let file = crate::create_private(&staging.join(RECORD))?;
        let stored = Stored {
            record: record.clone(),
            last_boot: 0,
        };
        serde_json::to_writer(&file, &stored)?;
        let config_file = crate::create_private(&staging.join(config::CONFIG))?;
        (&config_file).write_all(&serde_json::to_vec(config)?)?;
        let disk = match disk {
            NewDisk::Base(base) => crate::clone_file(base, &staging.join(DISK))?,
            NewDisk::Import(path) => {
                fs::rename(path, staging.join(DISK))
                    .with_context(|| format!("moving {} in", path.display()))?;
                vm::open(&staging.join(DISK))?
            }
        };
        for file in [&disk, &file, &config_file] {
            file.sync_all()?;
        }
        File::open(&staging)?.sync_all()?;
        fs::rename(&staging, dir.join(&record.id))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if made.is_err() {
        if let NewDisk::Import(path) = disk {
            let _ = fs::rename(staging.join(DISK), path);
        }
        let _ = fs::remove_dir_all(&staging);
    }
    made
}

fn read_record(dir: &Path) -> Result<Stored> {
    let file = vm::open(&dir.join(RECORD))?;
    serde_json::from_reader(file).context("reading the sandbox record")
}

/// Puts `boot` in `sandbox.json` in place of the last one.
fn save_boot(dir: &Path, record: Record, boot: u64) -> Result<()> {
    let stored = Stored {
        record,
        last_boot: boot,
    };
    replace_json(dir, RECORD, &stored)
}

/// Writes `value` as `dir/name`, readable by root only, through a file
/// synced under another name, so a crash leaves the old one or the new.
fn replace_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let partial = dir.join(format!(".{name}.partial"));
    let _ = fs::remove_file(&partial);
    let written = (|| {
        let file = crate::create_private(&partial)?;
        (&file).write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&partial, dir.join(name))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&partial);
    }
    written
}

/// Puts `boot` in the record of the sandbox in `dir`, then renames `from`
/// over its disk. The boot moves first, so a crash between the two leaves
/// at worst a new boot on the old disk, which only costs a host a copy it
/// did not need, never the restored disk under the boot before.
fn restore_disk(dir: &Path, record: Record, boot: u64, from: &Path) -> Result<()> {
    save_boot(dir, record, boot)?;
    vm::open(from)?.sync_all()?;
    fs::rename(from, dir.join(DISK)).with_context(|| format!("moving {} in", from.display()))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// The file `disk` names, which must be under the import directory once
/// links are followed.
fn import_path(import: &Path, disk: &str) -> Result<PathBuf, ApiError> {
    let refused =
        || ApiError::bad_request(format!("disk: expected a file under {}", import.display()));
    let path = fs::canonicalize(disk).map_err(|_| refused())?;
    if !path.starts_with(import) {
        return Err(refused());
    }
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(path),
        _ => Err(refused()),
    }
}

/// Copies `disk` to `<parked>/<name>.disk` through a file synced under
/// another name, so a crash never leaves a cut copy there. The answer is
/// the copy and its size.
fn park_copy(disk: &Path, parked: &Path, name: &str) -> Result<(PathBuf, u64)> {
    let target = parked.join(format!("{name}.disk"));
    let partial = parked.join(format!(".{name}.disk.partial"));
    let _ = fs::remove_file(&partial);
    let copied = (|| {
        let copy = crate::clone_file(disk, &partial)?;
        copy.sync_all()?;
        let size = copy.metadata()?.len();
        fs::rename(&partial, &target)?;
        File::open(parked)?.sync_all()?;
        Ok(size)
    })();
    match copied {
        Ok(size) => Ok((target, size)),
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Copies `disk` to `<checkpoints>/<name>.<random>.disk`. A `live` disk,
/// which its VM writes meanwhile, is only copied by sharing its blocks,
/// which takes all of them at one instant; a plain copy could mix blocks
/// from before and after a write. The answer is the copy and its size.
fn checkpoint_copy(
    disk: &Path,
    checkpoints: &Path,
    name: &str,
    live: bool,
) -> Result<(PathBuf, u64)> {
    let mut token = [0u8; 8];
    getrandom::fill(&mut token).map_err(|e| anyhow::anyhow!("{e}"))?;
    let target = checkpoints.join(format!("{name}.{}.disk", vm::hex(&token)));
    let copied = (|| {
        let copy = if live {
            let from = vm::open(disk)?;
            let copy = crate::create_private(&target)?;
            if !shuru_linux::reflink(&copy, &from)? {
                bail!("the state directory cannot share blocks between files, which copying the disk of a running sandbox takes");
            }
            copy
        } else {
            crate::clone_file(disk, &target)?
        };
        Ok(copy.metadata()?.len())
    })();
    match copied {
        Ok(size) => Ok((target, size)),
        Err(e) => {
            let _ = fs::remove_file(&target);
            Err(e)
        }
    }
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

    /// A fresh directory of its own for `test`.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shuru-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn imports_only_files_under_the_import_directory() {
        let state = scratch("import");
        let import = state.join("import");
        fs::create_dir_all(import.join("deeper")).unwrap();
        for file in ["import/a.disk", "import/deeper/b.disk", "outside.disk"] {
            fs::write(state.join(file), b"disk").unwrap();
        }
        std::os::unix::fs::symlink(state.join("outside.disk"), import.join("link.disk")).unwrap();

        let ok = |path: PathBuf| import_path(&import, path.to_str().unwrap()).unwrap();
        assert_eq!(ok(import.join("a.disk")), import.join("a.disk"));
        assert_eq!(
            ok(import.join("deeper/b.disk")),
            import.join("deeper/b.disk")
        );
        assert_eq!(ok(import.join("deeper/../a.disk")), import.join("a.disk"));
        for refused in [
            state.join("outside.disk"),
            import.join("../outside.disk"),
            import.join("link.disk"),
            import.join("missing.disk"),
            import.join("deeper"),
            import.clone(),
        ] {
            let e = import_path(&import, refused.to_str().unwrap()).unwrap_err();
            assert_eq!(e.code(), "bad_request", "{}", refused.display());
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_park_replaces_the_copy_before() {
        let state = scratch("park");
        let parked = state.join("parked");
        fs::create_dir_all(&parked).unwrap();
        let disk = state.join("disk");
        fs::write(&disk, b"first").unwrap();
        let (copy, size) = park_copy(&disk, &parked, "web-1").unwrap();
        assert_eq!(copy, parked.join("web-1.disk"));
        assert_eq!(size, 5);

        // A crash left a cut copy, and the disk changed since.
        fs::write(parked.join(".web-1.disk.partial"), b"cut").unwrap();
        fs::write(&disk, b"second run").unwrap();
        let (copy, size) = park_copy(&disk, &parked, "web-1").unwrap();
        assert_eq!(fs::read(&copy).unwrap(), b"second run");
        assert_eq!(size, 10);
        assert_eq!(fs::read(&disk).unwrap(), b"second run");
        let names: Vec<_> = fs::read_dir(&parked)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["web-1.disk"]);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn an_import_moves_the_disk_in_or_leaves_it() {
        let state = scratch("make");
        let sandboxes = state.join("sandboxes");
        fs::create_dir_all(&sandboxes).unwrap();
        let imported = state.join("import.disk");
        fs::write(&imported, b"stored").unwrap();
        let record = |id: &str| Record {
            id: id.into(),
            name: "web-1".into(),
            created_at: 1,
            cpus: 2,
            memory_mib: 2048,
        };

        let config = secret_config();
        make(
            &sandboxes,
            &NewDisk::Import(imported.clone()),
            &record("sbx-a"),
            &config,
        )
        .unwrap();
        assert!(!imported.exists());
        let dir = sandboxes.join("sbx-a");
        assert_eq!(fs::read(dir.join(DISK)).unwrap(), b"stored");
        assert_eq!(read_record(&dir).unwrap().last_boot, 0);
        assert_private(&dir.join(config::CONFIG));
        assert_eq!(config::read(&dir).unwrap().secrets["TOKEN"].value, "s3cret");

        // The directory exists already, so the make fails at its last step.
        fs::write(&imported, b"again").unwrap();
        fs::create_dir_all(sandboxes.join("sbx-b/taken")).unwrap();
        assert!(make(
            &sandboxes,
            &NewDisk::Import(imported.clone()),
            &record("sbx-b"),
            &config,
        )
        .is_err());
        assert_eq!(fs::read(&imported).unwrap(), b"again");
        assert!(!sandboxes.join(".sbx-b.partial").exists());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn the_last_boot_is_kept_in_the_record() {
        let dir = scratch("boot");
        let record = Record {
            id: "sbx-a".into(),
            name: "web-1".into(),
            created_at: 1,
            cpus: 2,
            memory_mib: 2048,
        };
        // A record written before boots were kept.
        fs::write(dir.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(read_record(&dir).unwrap().last_boot, 0);
        save_boot(&dir, record.clone(), 17).unwrap();
        save_boot(&dir, record, 18).unwrap();
        let stored = read_record(&dir).unwrap();
        assert_eq!(
            (stored.record.name.as_str(), stored.last_boot),
            ("web-1", 18)
        );
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [RECORD]);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn secret_config() -> SandboxConfig {
        SandboxConfig::parse(
            Some(serde_json::json!({ "A": "1" })),
            Some(serde_json::json!({
                "TOKEN": { "value": "s3cret", "hosts": ["api.example.com"] }
            })),
        )
        .unwrap()
    }

    fn assert_private(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", path.display());
    }

    fn record() -> Record {
        Record {
            id: "sbx-a".into(),
            name: "web-1".into(),
            created_at: 1,
            cpus: 2,
            memory_mib: 2048,
        }
    }

    #[test]
    fn a_config_replaces_the_one_before() {
        let dir = scratch("config");
        replace_json(&dir, config::CONFIG, &SandboxConfig::default()).unwrap();
        assert!(config::read(&dir).unwrap().secrets.is_empty());
        // A crash left a cut copy.
        fs::write(dir.join(format!(".{}.partial", config::CONFIG)), b"{").unwrap();
        replace_json(&dir, config::CONFIG, &secret_config()).unwrap();
        let read = config::read(&dir).unwrap();
        assert_eq!(read.env["A"], "1");
        assert_eq!(read.secrets["TOKEN"].hosts, ["api.example.com"]);
        assert_private(&dir.join(config::CONFIG));
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [config::CONFIG]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn boots_only_move_forward() {
        let last = AtomicU64::new(0);
        assert_eq!(boot_after(&last, 1000), 1000);
        // The clock went back, or has not moved.
        assert_eq!(boot_after(&last, 400), 1001);
        assert_eq!(boot_after(&last, 1001), 1002);
        assert_eq!(boot_after(&last, 5000), 5000);
        assert_eq!(last.load(Ordering::Relaxed), 5000);
    }

    #[test]
    fn a_restore_moves_the_boot_then_the_disk() {
        let state = scratch("restore");
        let dir = state.join("sbx-a");
        let import = state.join("import");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&import).unwrap();
        save_boot(&dir, record(), 17).unwrap();
        fs::write(dir.join(DISK), b"before").unwrap();
        fs::write(import.join("v3.disk"), b"checkpoint").unwrap();

        let from = import_path(&import, import.join("v3.disk").to_str().unwrap()).unwrap();
        restore_disk(&dir, record(), 18, &from).unwrap();
        assert_eq!(fs::read(dir.join(DISK)).unwrap(), b"checkpoint");
        assert!(!from.exists());
        assert_eq!(read_record(&dir).unwrap().last_boot, 18);

        // Gone before the move: the boot still moved, the disk stays.
        assert!(restore_disk(&dir, record(), 19, &from).is_err());
        assert_eq!(fs::read(dir.join(DISK)).unwrap(), b"checkpoint");
        assert_eq!(read_record(&dir).unwrap().last_boot, 19);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_checkpoint_is_a_copy_of_its_own() {
        let state = scratch("checkpoint");
        let checkpoints = state.join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        let disk = state.join("disk");
        fs::write(&disk, b"running").unwrap();

        let (first, size) = checkpoint_copy(&disk, &checkpoints, "web-1", false).unwrap();
        let (second, _) = checkpoint_copy(&disk, &checkpoints, "web-1", false).unwrap();
        assert_ne!(first, second);
        assert_eq!(size, 7);
        for copy in [&first, &second] {
            let file_name = copy.file_name().unwrap().to_str().unwrap();
            assert!(file_name.starts_with("web-1.") && file_name.ends_with(".disk"));
            assert_eq!(copy.parent(), Some(checkpoints.as_path()));
            assert_eq!(fs::read(copy).unwrap(), b"running");
            assert_private(copy);
        }
        fs::write(&disk, b"changed").unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"running");

        // A live disk is copied only where blocks can be shared, and a
        // refusal leaves nothing behind.
        let before = fs::read_dir(&checkpoints).unwrap().count();
        match checkpoint_copy(&disk, &checkpoints, "web-1", true) {
            Ok((copy, _)) => assert_eq!(fs::read(copy).unwrap(), b"changed"),
            Err(_) => assert_eq!(fs::read_dir(&checkpoints).unwrap().count(), before),
        }
        fs::remove_dir_all(&state).unwrap();
    }
}
