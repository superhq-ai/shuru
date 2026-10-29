//! Every child the guest starts. A child is registered in the same critical
//! section that starts it, and unregistered in the one that reaps it, and
//! only this module reaps. So a wait status always reaches the one waiting
//! for it, never a later process that got the same pid. Each registration
//! has a serial number of its own, so a signal sent through `signal` cannot
//! reach such a process either.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

pub type OnExit = Box<dyn FnOnce(libc::c_int) + Send>;

#[derive(Default)]
struct Watched {
    serial: u64,
    children: HashMap<i32, (u64, OnExit)>,
}

impl Watched {
    fn insert(&mut self, pid: i32, on_exit: OnExit) -> Watch {
        self.serial += 1;
        self.children.insert(pid, (self.serial, on_exit));
        Watch {
            pid,
            serial: self.serial,
        }
    }
}

/// A registered child, until it is reaped.
#[derive(Clone, Copy)]
pub struct Watch {
    pid: i32,
    serial: u64,
}

static WATCHED: LazyLock<Mutex<Watched>> = LazyLock::new(Default::default);

/// The end of the pipe SIGCHLD writes to.
static REAPER_WAKE: AtomicI32 = AtomicI32::new(-1);

const STALE_RETRY: Duration = Duration::from_secs(1);

fn watched() -> MutexGuard<'static, Watched> {
    WATCHED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The wait status of a child, once it is reaped.
#[derive(Clone, Default)]
pub struct Exit(Arc<(Mutex<Option<libc::c_int>>, Condvar)>);

impl Exit {
    fn slot(&self) -> MutexGuard<'_, Option<libc::c_int>> {
        self.0 .0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, status: libc::c_int) {
        *self.slot() = Some(status);
        self.0 .1.notify_all();
    }

    pub fn get(&self) -> Option<libc::c_int> {
        *self.slot()
    }

    pub fn wait(&self) -> libc::c_int {
        let mut slot = self.slot();
        loop {
            if let Some(status) = *slot {
                return status;
            }
            slot = self.0 .1.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Starts a child with `start`, which returns its pid, and gives `on_exit`
/// its wait status once it is reaped.
pub fn start(start: impl FnOnce() -> io::Result<i32>, on_exit: OnExit) -> io::Result<Watch> {
    let mut watched = watched();
    let pid = start()?;
    Ok(watched.insert(pid, on_exit))
}

/// Spawns `cmd` and gives `on_exit` its wait status once it is reaped.
pub fn spawn_with(cmd: &mut Command, on_exit: OnExit) -> io::Result<(Child, Watch)> {
    let mut watched = watched();
    let child = cmd.spawn()?;
    let watch = watched.insert(child.id() as i32, on_exit);
    Ok((child, watch))
}

/// Spawns `cmd`, its wait status to come in the returned `Exit`.
pub fn spawn_waited(cmd: &mut Command) -> io::Result<(Child, Watch, Exit)> {
    let exit = Exit::default();
    let status = exit.clone();
    let (child, watch) = spawn_with(cmd, Box::new(move |s| status.set(s)))?;
    Ok((child, watch, exit))
}

/// Spawns `cmd` with a pidfd that turns readable when it exits.
pub fn spawn(cmd: &mut Command, on_exit: OnExit) -> io::Result<(Child, OwnedFd)> {
    let mut watched = watched();
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    if fd < 0 {
        let err = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    watched.insert(pid, on_exit);
    Ok((child, unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Signals a child, or with `group` the process group it leads, while it
/// is not yet reaped and so still holds its pid.
pub fn signal(child: Watch, group: bool, signal: libc::c_int) {
    let watched = watched();
    let registered = watched.children.get(&child.pid);
    if registered.is_some_and(|(serial, _)| *serial == child.serial) {
        let pid = child.pid;
        unsafe { libc::kill(if group { -pid } else { pid }, signal) };
    }
}

/// Reaps the children that have exited, unless a start holds the lock;
/// then it returns false at once.
pub fn reap_watched() -> bool {
    let mut done = Vec::new();
    {
        let mut watched = match WATCHED.try_lock() {
            Ok(watched) => watched,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        let pids: Vec<i32> = watched.children.keys().copied().collect();
        for pid in pids {
            let mut status = 0;
            if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                if let Some((_, on_exit)) = watched.children.remove(&pid) {
                    done.push((on_exit, status));
                }
            }
        }
    }
    for (on_exit, status) in done {
        on_exit(status);
    }
    true
}

/// Reaps every child that has exited, as PID 1 must: its own, and the
/// orphans it adopted.
fn reap_all() {
    let mut done = Vec::new();
    {
        let mut watched = watched();
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if let Some((_, on_exit)) = watched.children.remove(&pid) {
                done.push((on_exit, status));
            }
        }
    }
    for (on_exit, status) in done {
        on_exit(status);
    }
}

/// Reaps from a thread of its own, woken through a pipe by SIGCHLD. The
/// handler is in place before the first pass, so a child that exits after
/// any pass has written to the pipe. The same thread removes dropped
/// cgroups once their last process is gone, retrying each second.
pub fn start_reaper() -> io::Result<()> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // A full pipe already holds a wakeup.
    if unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    REAPER_WAKE.store(write.into_raw_fd(), Ordering::Release);
    let handler = on_sigchld as *const () as libc::sighandler_t;
    if unsafe { libc::signal(libc::SIGCHLD, handler) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    std::thread::Builder::new()
        .name("reaper".into())
        .spawn(move || {
            let mut buf = [0u8; 64];
            let mut next_retry = Instant::now();
            loop {
                reap_all();
                let now = Instant::now();
                if now >= next_retry && crate::cgroup::has_stale() {
                    crate::cgroup::retry_stale();
                    next_retry = now + STALE_RETRY;
                }
                let timeout = if crate::cgroup::has_stale() {
                    next_retry.saturating_duration_since(now).as_millis() as libc::c_int + 1
                } else {
                    -1
                };
                let mut woken = libc::pollfd {
                    fd: read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut woken, 1, timeout) } > 0 {
                    let buf = buf.as_mut_ptr() as *mut libc::c_void;
                    unsafe { libc::read(read.as_raw_fd(), buf, 64) };
                }
            }
        })?;
    Ok(())
}

/// Wakes the reaper, to retry dropped cgroups from now on.
pub fn wake_reaper() {
    on_sigchld(0);
}

extern "C" fn on_sigchld(_: libc::c_int) {
    let fd = REAPER_WAKE.load(Ordering::Acquire);
    unsafe {
        let errno = *libc::__errno_location();
        libc::write(fd, [1u8].as_ptr() as *const libc::c_void, 1);
        *libc::__errno_location() = errno;
    }
}
