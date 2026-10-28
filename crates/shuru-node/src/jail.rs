//! Starts a process in the jail RFC 0008 describes: its own mount, pid,
//! ipc, uts and cgroup namespaces, a network namespace unless it is the
//! egress path, an empty read-only root, no capabilities, an unprivileged
//! uid and only the descriptors it is given.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

pub struct Jail<'a> {
    pub uid: u32,
    pub gid: u32,
    /// `cgroup.procs` of the cgroup to run in, opened for writing.
    pub cgroup_procs: &'a File,
    /// An empty directory the root tmpfs is mounted on.
    pub root: &'a CStr,
    pub nofile: u64,
    pub nproc: u64,
    /// Keep the host network namespace.
    pub host_network: bool,
}

/// What the child inherits: fd 3 is `keep`, 0 and 1 are `null`, 2 is
/// `stderr`. Everything else is closed.
pub struct Handoff<'a> {
    pub keep: &'a OwnedFd,
    pub null: &'a File,
    pub stderr: RawFd,
}

const CGROUP: i32 = 0;
const MOUNTS: i32 = 1;
const CREDENTIALS: i32 = 2;
const LIMITS: i32 = 3;
const DESCRIPTORS: i32 = 4;
const EXEC: i32 = 5;
const STEPS: [&str; 6] = [
    "joining the cgroup",
    "building the root",
    "dropping privileges",
    "setting limits",
    "placing descriptors",
    "executing the binary",
];

/// Executes `binary` in the jail and returns its pid. The binary must be
/// statically linked, since the root it runs in is empty, and the caller
/// must be single threaded.
pub fn spawn(jail: &Jail, binary: &File, argv: &[&CStr], handoff: &Handoff) -> io::Result<i32> {
    let mut argv: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv.push(ptr::null());
    let envp: [*const libc::c_char; 1] = [ptr::null()];
    let options = CString::new("mode=0555,size=4k").unwrap();

    let mut pipe = [0; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (mut report_read, report_write) =
        unsafe { (File::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };

    let mut flags = libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::SIGCHLD;
    if !jail.host_network {
        flags |= libc::CLONE_NEWNET;
    }
    // Without a stack the child runs on a copy of ours, like fork.
    let pid = unsafe { libc::syscall(libc::SYS_clone, flags as libc::c_ulong, 0, 0, 0, 0) };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let report = report_write.as_raw_fd();
        unsafe {
            child(
                jail,
                binary.as_raw_fd(),
                &argv,
                &envp,
                &options,
                handoff,
                report,
            )
        }
    }
    let pid = pid as i32;
    drop(report_write);

    let mut report = [0u8; 8];
    let mut read = 0;
    while read < report.len() {
        match report_read.read(&mut report[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if read == 0 {
        return Ok(pid);
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let step = i32::from_le_bytes(report[..4].try_into().unwrap());
    let error = io::Error::from_raw_os_error(i32::from_le_bytes(report[4..].try_into().unwrap()));
    let step = STEPS
        .get(step as usize)
        .copied()
        .unwrap_or("an unknown step");
    Err(io::Error::new(
        error.kind(),
        format!("jail failed {step}: {error}"),
    ))
}

// Runs between clone and exec, so it makes system calls only: no
// allocation, no locks, and no libc wrappers that assume other threads.
unsafe fn child(
    jail: &Jail,
    binary: RawFd,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
    options: &CStr,
    handoff: &Handoff,
    report: RawFd,
) -> ! {
    let mut report = report;
    macro_rules! check {
        ($step:expr, $call:expr) => {
            if ($call as i64) < 0 {
                fail(report, $step);
            }
        };
    }

    check!(
        CGROUP,
        libc::write(jail.cgroup_procs.as_raw_fd(), b"0".as_ptr().cast(), 1)
    );
    // After joining, so the namespace root is the sandbox cgroup.
    check!(CGROUP, libc::unshare(libc::CLONE_NEWCGROUP));

    check!(
        MOUNTS,
        libc::mount(
            ptr::null(),
            c"/".as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            ptr::null(),
        )
    );
    check!(
        MOUNTS,
        libc::mount(
            c"tmpfs".as_ptr(),
            jail.root.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RDONLY,
            options.as_ptr().cast(),
        )
    );
    check!(MOUNTS, libc::chdir(jail.root.as_ptr()));
    check!(
        MOUNTS,
        libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr())
    );
    check!(MOUNTS, libc::umount2(c".".as_ptr(), libc::MNT_DETACH));
    check!(MOUNTS, libc::chdir(c"/".as_ptr()));

    // Past the last capability the kernel answers EINVAL.
    for cap in 0..64 {
        if libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) < 0 {
            if cap > 0 && *libc::__errno_location() == libc::EINVAL {
                break;
            }
            fail(report, CREDENTIALS);
        }
    }
    check!(
        CREDENTIALS,
        libc::syscall(libc::SYS_setgroups, 0, ptr::null::<libc::gid_t>())
    );
    check!(
        CREDENTIALS,
        libc::syscall(libc::SYS_setresgid, jail.gid, jail.gid, jail.gid)
    );
    check!(
        CREDENTIALS,
        libc::syscall(libc::SYS_setresuid, jail.uid, jail.uid, jail.uid)
    );
    check!(
        CREDENTIALS,
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0
        )
    );
    // Leaving uid 0 already cleared them; this makes it explicit.
    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
    check!(
        CREDENTIALS,
        libc::syscall(libc::SYS_capset, &header, data.as_ptr())
    );
    check!(
        CREDENTIALS,
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
    );
    for (resource, limit) in [
        (libc::RLIMIT_NOFILE, jail.nofile),
        (libc::RLIMIT_NPROC, jail.nproc),
        (libc::RLIMIT_CORE, 0),
    ] {
        let limit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        check!(LIMITS, libc::setrlimit(resource, &limit));
    }

    // Lift everything above the numbers handed out before placing it.
    let keep = libc::fcntl(handoff.keep.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 16);
    let binary = libc::fcntl(binary, libc::F_DUPFD_CLOEXEC, 16);
    let null = libc::fcntl(handoff.null.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 16);
    let stderr = libc::fcntl(handoff.stderr, libc::F_DUPFD_CLOEXEC, 16);
    let lifted = libc::fcntl(report, libc::F_DUPFD_CLOEXEC, 16);
    for fd in [keep, binary, null, stderr, lifted] {
        check!(DESCRIPTORS, fd);
    }
    report = lifted;
    check!(DESCRIPTORS, libc::dup2(null, 0));
    check!(DESCRIPTORS, libc::dup2(null, 1));
    check!(DESCRIPTORS, libc::dup2(stderr, 2));
    check!(DESCRIPTORS, libc::dup2(keep, 3));
    check!(DESCRIPTORS, libc::dup3(binary, 4, libc::O_CLOEXEC));
    check!(DESCRIPTORS, libc::dup3(report, 5, libc::O_CLOEXEC));
    report = 5;
    check!(
        DESCRIPTORS,
        libc::syscall(libc::SYS_close_range, 6, u32::MAX, 0)
    );

    libc::syscall(
        libc::SYS_execveat,
        4,
        c"".as_ptr(),
        argv.as_ptr(),
        envp.as_ptr(),
        libc::AT_EMPTY_PATH,
    );
    fail(report, EXEC)
}

unsafe fn fail(report: RawFd, step: i32) -> ! {
    let errno = *libc::__errno_location();
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&step.to_le_bytes());
    bytes[4..].copy_from_slice(&errno.to_le_bytes());
    libc::write(report, bytes.as_ptr().cast(), bytes.len());
    libc::_exit(127)
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}
