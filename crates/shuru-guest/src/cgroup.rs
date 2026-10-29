//! Cgroups the guest starts processes in: one per exec session and one per
//! plain exec. A cgroup holds every process started from it, those that
//! left the process group included. SIGKILL goes through `cgroup.kill`.
//! Any other signal goes to each member through a pidfd, opened before the
//! check that the process is still in the cgroup, so it never reaches a
//! later process that took a reused pid. A cgroup that a process outlives
//! is removed once that process is gone.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const MOUNT: &str = "/sys/fs/cgroup";
const ROOT: &str = "/sys/fs/cgroup/shuru";

/// Dropped cgroups that still held a process.
static STALE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn stale() -> MutexGuard<'static, Vec<PathBuf>> {
    STALE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn init() -> io::Result<()> {
    match fs::create_dir(ROOT) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

pub struct Cgroup {
    dir: PathBuf,
}

impl Cgroup {
    pub fn create(name: &str) -> io::Result<Cgroup> {
        let dir = Path::new(ROOT).join(name);
        if let Err(e) = fs::create_dir(&dir) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
            // Left with this name by an earlier run; taken over once empty.
            remove_tree(&dir)?;
            fs::create_dir(&dir)?;
        }
        Ok(Cgroup { dir })
    }

    /// Its `cgroup.procs`, for a child to `join` before exec.
    pub fn procs(&self) -> io::Result<OwnedFd> {
        let file = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(self.dir.join("cgroup.procs"))?;
        Ok(file.into())
    }

    /// Whether any process is left, in it or below it. One that cannot be
    /// read counts as populated, so a kill never completes on a guess.
    pub fn populated(&self) -> bool {
        match fs::read_to_string(self.dir.join("cgroup.events")) {
            Ok(events) => !events.lines().any(|line| line == "populated 0"),
            Err(_) => true,
        }
    }

    /// Signals every process in it or below it.
    pub fn signal(&self, signal: libc::c_int) {
        if signal == libc::SIGKILL {
            let _ = fs::write(self.dir.join("cgroup.kill"), "1");
            return;
        }
        let Ok(path) = self.dir.strip_prefix(MOUNT) else {
            return;
        };
        let path = Path::new("/").join(path);
        for dir in tree(&self.dir) {
            let Ok(procs) = fs::read_to_string(dir.join("cgroup.procs")) else {
                continue;
            };
            for pid in procs.lines().filter_map(|line| line.parse().ok()) {
                signal_member(pid, &path, signal);
            }
        }
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        if remove_tree(&self.dir).is_err() {
            stale().push(std::mem::take(&mut self.dir));
            crate::children::wake_reaper();
        }
    }
}

/// Moves the calling process into the cgroup whose `cgroup.procs` is open
/// at `procs`. Only a system call, so a forked child can make it.
pub fn join(procs: RawFd) -> io::Result<()> {
    if unsafe { libc::write(procs, b"0".as_ptr() as *const libc::c_void, 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn has_stale() -> bool {
    !stale().is_empty()
}

/// Removes the dropped cgroups that have emptied since.
pub fn retry_stale() {
    stale().retain(|dir| remove_tree(dir).is_err());
}

/// Signals `pid` if it is in the cgroup at `path` or below it. The pidfd
/// names the process that had the pid when it was opened; if that process
/// is gone by the check, the signal fails rather than reach another.
fn signal_member(pid: i32, path: &Path, signal: libc::c_int) {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    if fd < 0 {
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let Ok(cgroups) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
        return;
    };
    let inside = cgroups
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .any(|member| Path::new(member).starts_with(path));
    if inside {
        let info: *const libc::siginfo_t = std::ptr::null();
        unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), signal, info, 0) };
    }
}

/// A cgroup and every one below it, each before those below it.
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut all = vec![dir.to_path_buf()];
    let mut i = 0;
    while i < all.len() {
        if let Ok(entries) = fs::read_dir(&all[i]) {
            let below: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            all.extend(below);
        }
        i += 1;
    }
    all
}

/// Removes a cgroup and those below it once no process is left in any of
/// them. One still in use keeps every cgroup below it, which a process in
/// it may yet join.
fn remove_tree(dir: &Path) -> io::Result<()> {
    match fs::read_to_string(dir.join("cgroup.events")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(events) if !events.lines().any(|line| line == "populated 0") => {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        Ok(_) => {}
    }
    for dir in tree(dir).iter().rev() {
        match fs::remove_dir(dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn a_dropped_cgroup_goes_once_it_empties() {
        init().unwrap();
        let cgroup = Cgroup::create("test-dropped").unwrap();
        let dir = cgroup.dir().to_path_buf();
        fs::create_dir(dir.join("below")).unwrap();
        let procs = cgroup.procs().unwrap();
        let fd = procs.as_raw_fd();
        let mut cmd = Command::new("sleep");
        cmd.arg("300");
        unsafe { cmd.pre_exec(move || join(fd)) };
        let mut child = cmd.spawn().unwrap();
        drop(cgroup);
        assert!(dir.join("below").exists());
        child.kill().unwrap();
        child.wait().unwrap();
        retry_stale();
        assert!(!dir.exists());
    }
}
