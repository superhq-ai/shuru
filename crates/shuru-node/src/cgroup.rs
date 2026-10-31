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

    pub fn usage(&self) -> io::Result<Usage> {
        read_usage(&self.path)
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

/// CPU time and memory a sandbox cgroup has used.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    pub cpu_usec: u64,
    pub memory_bytes: u64,
}

pub fn usage(id: &str) -> io::Result<Usage> {
    read_usage(
        &Path::new(ROOT)
            .join("shuru.slice")
            .join(format!("sbx-{id}")),
    )
}

fn read_usage(path: &Path) -> io::Result<Usage> {
    Ok(Usage {
        cpu_usec: cpu_usec(&fs::read_to_string(path.join("cpu.stat"))?)?,
        memory_bytes: number(&fs::read_to_string(path.join("memory.current"))?)?,
    })
}

fn cpu_usec(stat: &str) -> io::Result<u64> {
    let value = stat
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cpu.stat has no usage_usec"))?;
    number(value)
}

fn number(value: &str) -> io::Result<u64> {
    value
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{value:?}: {e}")))
}

/// Removes the cgroup of sandbox `id` that a VM which is gone left behind.
/// Refuses one that still holds a process.
pub fn remove_empty(id: &str) -> io::Result<()> {
    let path = Path::new(ROOT)
        .join("shuru.slice")
        .join(format!("sbx-{id}"));
    match fs::read_to_string(path.join("cgroup.events")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(events) if !events.lines().any(|line| line == "populated 0") => {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        Ok(_) => {}
    }
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir(entry.path())?;
        }
    }
    fs::remove_dir(&path)
}

fn write(path: &Path, value: &str) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_time_is_read_from_cpu_stat() {
        let stat = "usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\n";
        assert_eq!(cpu_usec(stat).unwrap(), 1234567);
        assert!(cpu_usec("user_usec 1\n").is_err());
        assert_eq!(number("4096\n").unwrap(), 4096);
        assert!(number("max\n").is_err());
    }
}
