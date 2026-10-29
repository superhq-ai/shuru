//! Starting the process of a session: in the session cgroup, its own
//! session and process group, with pipes or a terminal. Every descriptor is
//! created close-on-exec, so a process started by another thread at the
//! same time inherits none.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use shuru_proto::{frame, ExecRequest};

use crate::cgroup::{self, Cgroup};

pub struct Spawned {
    pub pid: i32,
    pub pidfd: OwnedFd,
    /// Stdin, or for a tty session a second descriptor of the terminal.
    pub stdin: OwnedFd,
    pub outputs: Vec<(u8, OwnedFd)>,
}

/// Starts the process. On an error the cgroup may hold what was started,
/// for the caller to kill.
pub fn spawn(
    exec: &ExecRequest,
    cgroup: &Cgroup,
    on_exit: crate::children::OnExit,
) -> io::Result<Spawned> {
    let procs_file = cgroup.procs()?;
    let procs = procs_file.as_raw_fd();
    let mut cmd = Command::new(&exec.argv[0]);
    cmd.args(&exec.argv[1..]).envs(&exec.env);
    if let Some(cwd) = &exec.cwd {
        cmd.current_dir(cwd);
    }
    if exec.tty.unwrap_or(false) {
        let (master, slave) = open_pty(exec.rows.unwrap_or(24), exec.cols.unwrap_or(80))?;
        let stdin = OwnedFd::from(master.try_clone()?);
        if !exec.env.contains_key("TERM") {
            cmd.env("TERM", "xterm-256color");
        }
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        unsafe {
            cmd.pre_exec(move || {
                cgroup::join(procs)?;
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let (child, pidfd) = crate::children::spawn(&mut cmd, on_exit)?;
        // The command holds our copies of the terminal; the process has its own.
        drop(cmd);
        Ok(Spawned {
            pid: child.id() as i32,
            pidfd,
            stdin,
            outputs: vec![(frame::STDOUT, OwnedFd::from(master))],
        })
    } else {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(move || {
                cgroup::join(procs)?;
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let (mut child, pidfd) = crate::children::spawn(&mut cmd, on_exit)?;
        let pid = child.id() as i32;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            return Err(io::Error::other("missing pipes"));
        };
        let spawned = Spawned {
            pid,
            pidfd,
            stdin: stdin.into(),
            outputs: vec![
                (frame::STDOUT, stdout.into()),
                (frame::STDERR, stderr.into()),
            ],
        };
        let nonblocking = std::iter::once(&spawned.stdin)
            .chain(spawned.outputs.iter().map(|(_, fd)| fd))
            .try_for_each(set_nonblocking);
        nonblocking?;
        Ok(spawned)
    }
}

/// A terminal pair, the master nonblocking, through TIOCGPTPEER so the
/// slave is never looked up by path.
fn open_pty(rows: u16, cols: u16) -> io::Result<(File, File)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    let master = unsafe { libc::open(c"/dev/ptmx".as_ptr(), flags | libc::O_NONBLOCK) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let unlock: libc::c_int = 0;
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSPTLCK, &unlock) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, flags) };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, unsafe { File::from_raw_fd(slave) }))
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
