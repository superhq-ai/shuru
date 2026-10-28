//! Per sandbox cgroups under `shuru.slice`: limits on `sbx-<id>`, processes
//! in its children, so the envelope holds whatever the split between them.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ROOT: &str = "/sys/fs/cgroup";
const CONTROLLERS: &str = "+cpu +memory +pids";
const PERIOD_US: u64 = 100_000;
/// Host memory a VM may use beyond its guest RAM (RFC 0011).
pub const OVERHEAD_BYTES: u64 = 320 << 20;
const HIGH_BELOW_MAX: u64 = 64 << 20;

pub struct Limits {
    pub guest_memory_bytes: u64,
    pub cpus: u64,
    pub pids: u64,
}

pub struct Sandbox {
    path: PathBuf,
    children: Vec<PathBuf>,
}

impl Sandbox {
    pub fn create(id: &str, limits: &Limits) -> io::Result<Self> {
        let slice = Path::new(ROOT).join("shuru.slice");
        write(&Path::new(ROOT).join("cgroup.subtree_control"), CONTROLLERS)?;
        match fs::create_dir(&slice) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        write(&slice.join("cgroup.subtree_control"), CONTROLLERS)?;

        let path = slice.join(format!("sbx-{id}"));
        fs::create_dir(&path)?;
        let sandbox = Sandbox {
            path,
            children: Vec::new(),
        };
        let max = limits.guest_memory_bytes + OVERHEAD_BYTES;
        sandbox.set("memory.max", &max.to_string())?;
        sandbox.set("memory.high", &(max - HIGH_BELOW_MAX).to_string())?;
        match sandbox.set("memory.swap.max", "0") {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
        sandbox.set(
            "cpu.max",
            &format!("{} {PERIOD_US}", limits.cpus * PERIOD_US),
        )?;
        sandbox.set("pids.max", &limits.pids.to_string())?;
        Ok(sandbox)
    }

    fn set(&self, file: &str, value: &str) -> io::Result<()> {
        write(&self.path.join(file), value)
            .map_err(|e| io::Error::new(e.kind(), format!("{file}: {e}")))
    }

    /// Creates a child cgroup and opens its `cgroup.procs` for a process to
    /// join.
    pub fn child(&mut self, name: &str) -> io::Result<File> {
        let path = self.path.join(name);
        fs::create_dir(&path)?;
        self.children.push(path.clone());
        OpenOptions::new()
            .write(true)
            .open(path.join("cgroup.procs"))
    }

    pub fn memory_peak(&self) -> Option<u64> {
        fs::read_to_string(self.path.join("memory.peak"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for Sandbox {
    // A cgroup goes away only once its last process is reaped, which can
    // lag the exit of a pid namespace init by a moment.
    fn drop(&mut self) {
        for path in self.children.iter().rev().chain([&self.path]) {
            for _ in 0..100 {
                match fs::remove_dir(path) {
                    Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    _ => break,
                }
            }
        }
    }
}

fn write(path: &Path, value: &str) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value.as_bytes())
}
