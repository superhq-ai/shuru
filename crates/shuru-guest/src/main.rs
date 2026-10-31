#[cfg(any(target_os = "linux", test))]
mod identity;

#[cfg(any(target_os = "linux", test))]
mod ext4;

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
mod children;
#[cfg(target_os = "linux")]
mod services;

#[cfg(target_os = "linux")]
mod sessions;

#[cfg(target_os = "linux")]
mod disk;

#[cfg(target_os = "linux")]
mod files;

#[cfg(target_os = "linux")]
mod ports;

#[cfg(target_os = "linux")]
mod guest {
    use std::io::{Read, Write};
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use shuru_proto::frame;
    use shuru_proto::{
        ChmodRequest, ChownRequest, CopyRequest, DiscardRequest, DownloadProgress, DownloadRequest,
        ExecRequest, ForwardRequest, ForwardResponse, FsOkResponse, MkdirRequest, MountRequest,
        MountResponse, ReadDirRequest, ReadFileRequest, RemoveRequest, RenameRequest, StatRequest,
        StatResponse, WatchEvent, WatchRequest, WriteFileRequest,
    };
    use shuru_proto::{VSOCK_PORT, VSOCK_PORT_FORWARD};

    // _IO('R', 0x07): reseed the kernel CRNG now rather than on its timer.
    const RNDRESEEDCRNG: libc::Ioctl = 0x5207;
    const FORWARD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const FORWARD_STALL: std::time::Duration = std::time::Duration::from_secs(60);

    /// Running execs, by the id the host started them with.
    static EXECS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<u64, std::sync::Arc<Scope>>>,
    > = std::sync::LazyLock::new(Default::default);
    static NEXT_EXEC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    /// What a kill of an exec reaches: its cgroup, or with none the process
    /// group of its process until that is reaped.
    enum Scope {
        Cgroup(crate::cgroup::Cgroup),
        Group(crate::children::Watch),
    }

    impl Scope {
        fn signal(&self, signal: libc::c_int) {
            match self {
                Scope::Cgroup(cgroup) => cgroup.signal(signal),
                Scope::Group(leader) => crate::children::signal(*leader, true, signal),
            }
        }
    }

    fn uptime_ms() -> f64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6
    }

    /// Boot stage log with a monotonic timestamp. Uses a raw write so a
    /// missing or broken console can never panic PID 1.
    fn stage(msg: &str) {
        let s = format!("shuru-guest[{:7.1}ms]: {}\n", uptime_ms(), msg);
        unsafe { libc::write(2, s.as_ptr() as *const libc::c_void, s.len()) };
    }

    /// Ensure fds 0-2 exist so stdio writes never hit EBADF. When the VM has
    /// no console device the kernel starts PID 1 with no open fds.
    fn ensure_stdio() {
        unsafe {
            if libc::fcntl(2, libc::F_GETFD) >= 0 {
                return;
            }
            let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            if fd >= 0 {
                for t in 0..3 {
                    libc::dup2(fd, t);
                }
                if fd > 2 {
                    libc::close(fd);
                }
            }
        }
    }

    fn mount_fs(source: &str, target: &str, fstype: &str, data: Option<&str>) -> bool {
        mount_fs_with_flags(source, target, fstype, 0, data)
    }

    fn mount_fs_with_flags(
        source: &str,
        target: &str,
        fstype: &str,
        flags: libc::c_ulong,
        data: Option<&str>,
    ) -> bool {
        use std::ffi::CString;

        let c_source = CString::new(source).unwrap();
        let c_target = CString::new(target).unwrap();
        let c_fstype = CString::new(fstype).unwrap();

        let data_ptr = data.map(|d| CString::new(d).unwrap());
        let ret = unsafe {
            libc::mount(
                c_source.as_ptr(),
                c_target.as_ptr(),
                c_fstype.as_ptr(),
                flags,
                data_ptr
                    .as_ref()
                    .map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
            )
        };
        if ret != 0 {
            eprintln!(
                "shuru-guest: failed to mount {} on {}: {}",
                source,
                target,
                std::io::Error::last_os_error()
            );
            return false;
        }
        true
    }

    fn mount_filesystems() {
        assert!(mount_fs("proc", "/proc", "proc", None));
        assert!(mount_fs("sysfs", "/sys", "sysfs", None));
        assert!(mount_fs("devtmpfs", "/dev", "devtmpfs", None));
        std::fs::create_dir_all("/dev/pts").ok();
        mount_fs(
            "devpts",
            "/dev/pts",
            "devpts",
            Some("newinstance,ptmxmode=0666"),
        );
        mount_fs("tmpfs", "/tmp", "tmpfs", None);
        // Runtime state lives on tmpfs so pid files and sockets never land in
        // a checkpoint. A stale /run/docker.pid otherwise blocks dockerd on
        // the next boot.
        // Both are guest RAM, so cap them well below the VM memory.
        assert!(mount_fs(
            "tmpfs",
            "/run",
            "tmpfs",
            Some("mode=0755,size=64m")
        ));
        std::fs::create_dir_all("/run/lock").ok();
        std::fs::create_dir_all("/dev/shm").ok();
        mount_fs("tmpfs", "/dev/shm", "tmpfs", Some("mode=1777,size=256m"));
        mount_cgroup2();
    }

    /// Unified cgroup hierarchy with every controller delegated below the
    /// root, which is what runc and containerd expect to find.
    fn mount_cgroup2() {
        if !mount_fs(
            "cgroup2",
            "/sys/fs/cgroup",
            "cgroup2",
            Some("nsdelegate,memory_recursiveprot,favordynmods"),
        ) {
            return;
        }
        let controllers = match std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers") {
            Ok(c) => c,
            Err(_) => return,
        };
        for name in controllers.split_whitespace() {
            if let Err(e) = std::fs::write(
                "/sys/fs/cgroup/cgroup.subtree_control",
                format!("+{}", name),
            ) {
                eprintln!(
                    "shuru-guest: failed to enable cgroup controller {}: {}",
                    name, e
                );
            }
        }
    }

    fn process_mount(req: &MountRequest) -> MountResponse {
        if let Err(e) = std::fs::create_dir_all(&req.guest_path) {
            return MountResponse {
                tag: req.tag.clone(),
                ok: false,
                error: Some(format!(
                    "failed to create mount point {}: {}",
                    req.guest_path, e
                )),
            };
        }

        let result = if req.read_only {
            mount_overlay(&req.tag, &req.guest_path)
        } else {
            mount_direct(&req.tag, &req.guest_path)
        };
        match result {
            Ok(()) => MountResponse {
                tag: req.tag.clone(),
                ok: true,
                error: None,
            },
            Err(msg) => MountResponse {
                tag: req.tag.clone(),
                ok: false,
                error: Some(msg),
            },
        }
    }

    fn mount_overlay(tag: &str, guest_path: &str) -> Result<(), String> {
        let virtiofs_dir = format!("/mnt/.virtiofs/{}", tag);
        let overlay_dir = format!("/mnt/.overlay/{}", tag);
        let upper_dir = format!("{}/upper", overlay_dir);
        let work_dir = format!("{}/work", overlay_dir);

        std::fs::create_dir_all(&virtiofs_dir)
            .and_then(|_| std::fs::create_dir_all(&upper_dir))
            .and_then(|_| std::fs::create_dir_all(&work_dir))
            .map_err(|e| format!("failed to create staging dirs: {}", e))?;

        if !mount_fs(tag, &virtiofs_dir, "virtiofs", None) {
            return Err(format!("failed to mount virtiofs device '{}'", tag));
        }

        if !mount_fs("tmpfs", &overlay_dir, "tmpfs", None) {
            return Err(format!("failed to mount tmpfs for overlay on '{}'", tag));
        }

        // Re-create upper/work after tmpfs mount
        std::fs::create_dir_all(&upper_dir)
            .and_then(|_| std::fs::create_dir_all(&work_dir))
            .map_err(|e| format!("failed to create overlay dirs after tmpfs: {}", e))?;

        let overlay_opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            virtiofs_dir, upper_dir, work_dir
        );
        if !mount_fs("overlay", guest_path, "overlay", Some(&overlay_opts)) {
            return Err(format!("failed to mount overlay at {}", guest_path));
        }

        eprintln!("shuru-guest: mounted {} -> {} (overlay)", tag, guest_path);
        Ok(())
    }

    fn mount_direct(tag: &str, guest_path: &str) -> Result<(), String> {
        if !mount_fs(tag, guest_path, "virtiofs", None) {
            return Err(format!(
                "failed to mount virtiofs device '{}' at {}",
                tag, guest_path
            ));
        }
        eprintln!("shuru-guest: mounted {} -> {} (direct rw)", tag, guest_path);
        Ok(())
    }

    fn bring_up_interface(sock: i32, name: &[u8]) {
        unsafe {
            let mut ifr: libc::ifreq = std::mem::zeroed();
            let copy_len = name.len().min(libc::IFNAMSIZ);
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                ifr.ifr_name.as_mut_ptr().cast::<u8>(),
                copy_len,
            );

            let display_name = String::from_utf8_lossy(&name[..name.len().saturating_sub(1)]);
            if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) < 0 {
                eprintln!("shuru-guest: failed to get {} flags", display_name);
                return;
            }

            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            if libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) < 0 {
                eprintln!("shuru-guest: failed to bring up {}", display_name);
            }
        }
    }

    // --- Networking setup ---
    fn setup_networking() {
        unsafe {
            let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if sock < 0 {
                panic!("network socket: {}", std::io::Error::last_os_error());
            }
            bring_up_interface(sock, b"lo\0");
            let mut ifr: libc::ifreq = std::mem::zeroed();
            std::ptr::copy_nonoverlapping(c"eth0".as_ptr(), ifr.ifr_name.as_mut_ptr(), 5);
            if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) < 0 {
                libc::close(sock);
                return;
            }
            fn address(bytes: [u8; 4]) -> libc::sockaddr {
                let addr = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: 0,
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(bytes),
                    },
                    sin_zero: [0; 8],
                };
                unsafe { std::mem::transmute(addr) }
            }
            for (op, addr) in [
                (libc::SIOCSIFADDR, [10, 0, 0, 2]),
                (libc::SIOCSIFNETMASK, [255, 255, 255, 0]),
            ] {
                ifr.ifr_ifru.ifru_addr = address(addr);
                if libc::ioctl(sock, op as _, &ifr) < 0 {
                    panic!("network address: {}", std::io::Error::last_os_error());
                }
            }
            bring_up_interface(sock, b"eth0\0");
            let mut route: libc::rtentry = std::mem::zeroed();
            route.rt_dst = address([0; 4]);
            route.rt_genmask = address([0; 4]);
            route.rt_gateway = address([10, 0, 0, 1]);
            route.rt_flags = (libc::RTF_UP | libc::RTF_GATEWAY) as _;
            if libc::ioctl(sock, libc::SIOCADDRT as _, &route) < 0 {
                panic!("network route: {}", std::io::Error::last_os_error());
            }
            libc::close(sock);
        }
        std::fs::write("/etc/resolv.conf", "nameserver 10.0.0.1\n").expect("configure DNS");
    }

    fn create_vsock_listener(port: u32) -> i32 {
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                panic!(
                    "shuru-guest: failed to create vsock socket: {}",
                    std::io::Error::last_os_error()
                );
            }

            #[repr(C)]
            struct SockaddrVm {
                svm_family: libc::sa_family_t,
                svm_reserved1: u16,
                svm_port: u32,
                svm_cid: u32,
                svm_flags: u8,
                svm_zero: [u8; 3],
            }

            let addr = SockaddrVm {
                svm_family: libc::AF_VSOCK as libc::sa_family_t,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: libc::VMADDR_CID_ANY,
                svm_flags: 0,
                svm_zero: [0; 3],
            };

            let ret = libc::bind(
                fd,
                &addr as *const SockaddrVm as *const libc::sockaddr,
                std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
            );
            if ret < 0 {
                panic!(
                    "shuru-guest: failed to bind vsock on port {}: {}",
                    port,
                    std::io::Error::last_os_error()
                );
            }

            let ret = libc::listen(fd, 1);
            if ret < 0 {
                panic!(
                    "shuru-guest: failed to listen on vsock: {}",
                    std::io::Error::last_os_error()
                );
            }

            fd
        }
    }

    /// Write a binary frame using `writev` for a single atomic syscall.
    /// Used by the PTY poll loop where we have a raw fd instead of a std Write.
    fn write_frame_fd(fd: i32, msg_type: u8, payload: &[u8]) {
        let len = 1u32 + payload.len() as u32;
        let len_bytes = len.to_be_bytes();
        let type_byte = [msg_type];
        let iov = [
            libc::iovec {
                iov_base: len_bytes.as_ptr() as *mut libc::c_void,
                iov_len: 4,
            },
            libc::iovec {
                iov_base: type_byte.as_ptr() as *mut libc::c_void,
                iov_len: 1,
            },
            libc::iovec {
                iov_base: payload.as_ptr() as *mut libc::c_void,
                iov_len: payload.len(),
            },
        ];
        unsafe {
            libc::writev(fd, iov.as_ptr(), 3);
        }
    }

    fn authenticate(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let timeout = Some(std::time::Duration::from_secs(2));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        crate::identity::current().accept(stream)?;
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)
    }

    /// Sets the wall clock and reseeds the RNG, then takes the new identity.
    fn restore(restore: &shuru_proto::hello::Restore) -> std::io::Result<()> {
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        now.tv_sec = restore.realtime_ns.div_euclid(1_000_000_000) as _;
        now.tv_nsec = restore.realtime_ns.rem_euclid(1_000_000_000) as _;
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &now) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut random = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/urandom")?;
        random.write_all(&restore.seed)?;
        if unsafe { libc::ioctl(random.as_raw_fd(), RNDRESEEDCRNG) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        crate::identity::set(restore.identity);
        Ok(())
    }

    fn handle_connection(fd: i32) {
        // SAFETY: fd is a valid socket from accept()
        let mut stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        if authenticate(&mut stream).is_err() {
            return;
        }
        let _ = stream.set_nodelay(true);
        let mut reader = stream.try_clone().expect("failed to clone stream");
        let mut writer = stream;

        // Ends on EOF or a read error.
        while let Ok(Some((msg_type, payload))) = frame::read_frame(&mut reader) {
            if crate::disk::busy() && !matches!(msg_type, frame::THAW_REQ | frame::SEAL_REQ) {
                let _ = frame::write_frame(&mut writer, frame::ERROR, b"root seal is active");
                return;
            }
            match msg_type {
                frame::SEAL_REQ | frame::THAW_REQ => {
                    let result = if msg_type == frame::SEAL_REQ {
                        serde_json::from_slice::<shuru_proto::SealRequest>(&payload)
                            .map_err(std::io::Error::other)
                            .and_then(crate::disk::seal)
                    } else {
                        serde_json::from_slice::<shuru_proto::ThawRequest>(&payload)
                            .map_err(std::io::Error::other)
                            .and_then(|r| crate::disk::thaw(r.id))
                    };
                    match result {
                        Ok(resp) => {
                            let _ = frame::send_json(&mut writer, msg_type + 1, &resp);
                        }
                        Err(e) => {
                            let _ = frame::write_frame(
                                &mut writer,
                                frame::ERROR,
                                e.to_string().as_bytes(),
                            );
                        }
                    }
                }
                frame::KILL_REQ => {
                    if let Ok(req) = serde_json::from_slice::<shuru_proto::KillRequest>(&payload) {
                        let scope = EXECS.lock().unwrap().get(&req.id).cloned();
                        if let Some(scope) = scope {
                            scope.signal(libc::SIGKILL);
                        }
                    }
                    let _ = frame::write_frame(&mut writer, frame::KILL_RESP, &[]);
                }
                frame::TRIM_REQ => match crate::disk::trim() {
                    Ok(resp) => {
                        let _ = frame::send_json(&mut writer, frame::TRIM_RESP, &resp);
                    }
                    Err(e) => {
                        let _ =
                            frame::write_frame(&mut writer, frame::ERROR, e.to_string().as_bytes());
                    }
                },
                frame::MOUNT_REQ => {
                    let mount_req: MountRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid mount request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    let resp = process_mount(&mount_req);
                    let _ = frame::send_json(&mut writer, frame::MOUNT_RESP, &resp);
                }
                frame::EXEC_REQ => {
                    let req: ExecRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid exec request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };

                    if req.argv.is_empty() {
                        let _ = frame::write_frame(&mut writer, frame::ERROR, b"empty argv");
                        continue;
                    }

                    // Services declared by the image must be ready before a
                    // command runs against them (RFC 0004).
                    if let Err(msg) = crate::services::wait_ready() {
                        let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                        continue;
                    }

                    if req.tty.unwrap_or(false) {
                        // TTY mode: hand off the raw fd
                        let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(&writer);
                        // Prevent TcpStream from closing the fd on drop
                        std::mem::forget(writer);
                        std::mem::forget(reader);
                        handle_tty_exec(raw_fd, &req);
                        return;
                    }

                    // Non-TTY streaming mode: takes ownership of streams
                    handle_piped_exec(&req, reader, writer);
                    return;
                }
                frame::SESSION_START => {
                    let req: shuru_proto::SessionStart = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid session request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    if req.exec.argv.is_empty() {
                        let _ = frame::write_frame(&mut writer, frame::ERROR, b"empty argv");
                        continue;
                    }
                    if let Err(msg) = crate::services::wait_ready() {
                        let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                        continue;
                    }
                    crate::sessions::start(req, writer);
                    return;
                }
                frame::SESSION_ATTACH => {
                    match serde_json::from_slice::<shuru_proto::SessionAttach>(&payload) {
                        Ok(req) => crate::sessions::attach(req, writer),
                        Err(e) => {
                            let msg = format!("invalid attach request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                        }
                    }
                    return;
                }
                frame::SESSION_LIST => {
                    let list = crate::sessions::list();
                    let _ = frame::send_json(&mut writer, frame::SESSION_LIST_RESP, &list);
                }
                frame::SESSION_KILL => {
                    match serde_json::from_slice::<shuru_proto::SessionKill>(&payload) {
                        Ok(req) => crate::sessions::kill(req, &mut writer),
                        Err(e) => {
                            let msg = format!("invalid kill request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                        }
                    }
                }
                frame::WATCH_REQ => {
                    let req: WatchRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid watch request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    if req.detail {
                        crate::files::watch(&req, writer);
                    } else {
                        handle_watch(&req, writer);
                    }
                    return;
                }
                frame::PORTS_WATCH_REQ => {
                    crate::ports::watch(writer);
                    return;
                }
                frame::READ_FILE_REQ => {
                    let req: ReadFileRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid read_file request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_read_file(&req, &mut writer);
                }
                frame::WRITE_FILE_REQ => {
                    let req: WriteFileRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid write_file request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    crate::files::write(&req, &mut reader, &mut writer);
                }
                frame::MKDIR_REQ => {
                    let req: MkdirRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid mkdir request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_mkdir(&req, &mut writer);
                }
                frame::READ_DIR_REQ => {
                    let req: ReadDirRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid read_dir request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    crate::files::read_dir(&req, &mut writer);
                }
                frame::STAT_REQ => {
                    let req: StatRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid stat request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_stat(&req, &mut writer);
                }
                frame::REMOVE_REQ => {
                    let req: RemoveRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid remove request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_remove(&req, &mut writer);
                }
                frame::DISCARD_REQ => {
                    let req: DiscardRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid discard request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_discard(&req, &mut writer);
                }
                frame::RENAME_REQ => {
                    let req: RenameRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid rename request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_rename(&req, &mut writer);
                }
                frame::COPY_REQ => {
                    let req: CopyRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid copy request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    crate::files::copy(&req, &mut writer);
                }
                frame::CHMOD_REQ => {
                    let req: ChmodRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid chmod request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    crate::files::chmod(&req, &mut writer);
                }
                frame::CHOWN_REQ => {
                    let req: ChownRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid chown request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    crate::files::chown(&req, &mut writer);
                }
                frame::DOWNLOAD_REQ => {
                    let req: DownloadRequest = match serde_json::from_slice(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = format!("invalid download request: {}", e);
                            let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                            continue;
                        }
                    };
                    handle_download(&req, &mut writer);
                }
                frame::SHUTDOWN_REQ => {
                    let resp = crate::services::shutdown();
                    match crate::disk::sync() {
                        Ok(()) => {
                            let _ = frame::send_json(&mut writer, frame::SHUTDOWN_RESP, &resp);
                        }
                        Err(e) => {
                            let _ = frame::write_frame(
                                &mut writer,
                                frame::ERROR,
                                e.to_string().as_bytes(),
                            );
                        }
                    }
                }
                frame::RESTORE_REQ => {
                    let result = shuru_proto::hello::Restore::decode(&payload)
                        .ok_or_else(|| std::io::Error::other("invalid restore request"))
                        .and_then(|r| restore(&r));
                    let _ = match result {
                        Ok(()) => frame::write_frame(&mut writer, frame::RESTORE_RESP, &[]),
                        Err(e) => {
                            frame::write_frame(&mut writer, frame::ERROR, e.to_string().as_bytes())
                        }
                    };
                }
                _ => {} // unknown type, skip
            }
        }
    }

    fn handle_read_file(req: &ReadFileRequest, writer: &mut impl Write) {
        let fail = |writer: &mut _, e: std::io::Error| {
            let msg = format!("read_file {}: {}", req.path, e);
            let _ = frame::write_frame(writer, frame::ERROR, msg.as_bytes());
        };
        let mut file = match std::fs::File::open(&req.path) {
            Ok(f) => f,
            Err(e) => return fail(writer, e),
        };
        let mut buf = vec![0u8; frame::FILE_CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(n) => {
                    if frame::write_frame(writer, frame::READ_FILE_RESP, &buf[..n]).is_err()
                        || n == 0
                    {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return fail(writer, e),
            }
        }
    }

    fn send_fs_ok(writer: &mut impl Write) {
        let resp = FsOkResponse {
            ok: true,
            ..Default::default()
        };
        let _ = frame::send_json(writer, frame::FS_OK_RESP, &resp);
    }

    fn send_fs_err(writer: &mut impl Write, msg: String) {
        let _ = frame::write_frame(writer, frame::ERROR, msg.as_bytes());
    }

    fn handle_mkdir(req: &MkdirRequest, writer: &mut impl Write) {
        let result = if req.recursive {
            std::fs::create_dir_all(&req.path)
        } else {
            std::fs::create_dir(&req.path)
        };
        match result {
            Ok(()) => send_fs_ok(writer),
            Err(e) => send_fs_err(writer, format!("mkdir {}: {}", req.path, e)),
        }
    }

    fn handle_download(req: &DownloadRequest, writer: &mut impl Write) {
        let response = match ureq::get(&req.url).call() {
            Ok(r) => r,
            Err(e) => {
                send_fs_err(writer, format!("download failed: {e}"));
                return;
            }
        };

        let total_bytes = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        let mut body = response.into_body().into_reader();

        if req.extract {
            // Stream through gzip + tar, reporting progress.
            // `strip_components` mirrors `tar --strip-components=N` — the caller
            // knows whether their tarball is directory-wrapped (Node, Pi → strip 1)
            // or flat (Codex → strip 0). The guest doesn't inspect structure.
            let progress_writer = DownloadProgressWriter {
                writer,
                bytes: 0,
                total_bytes,
                last_report: 0,
            };
            let tee = TeeReader::new(&mut body, progress_writer);
            let decoder = flate2::read::GzDecoder::new(tee);
            let mut archive = tar::Archive::new(decoder);

            if let Err(e) = std::fs::create_dir_all(&req.path) {
                send_fs_err(writer, format!("mkdir {}: {e}", req.path));
                return;
            }

            let dest = std::path::Path::new(&req.path);
            let strip = req.strip_components as usize;
            let entries = match archive.entries() {
                Ok(e) => e,
                Err(e) => {
                    send_fs_err(writer, format!("read tar entries: {e}"));
                    return;
                }
            };
            for entry in entries {
                let mut entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        send_fs_err(writer, format!("tar entry: {e}"));
                        return;
                    }
                };
                let path = match entry.path() {
                    Ok(p) => p.into_owned(),
                    Err(e) => {
                        send_fs_err(writer, format!("tar path: {e}"));
                        return;
                    }
                };
                let out_rel: std::path::PathBuf = path.components().skip(strip).collect();
                if out_rel.as_os_str().is_empty() {
                    continue; // e.g. the top-level directory entry when strip >= 1
                }
                let out_path = dest.join(&out_rel);
                if let Some(parent) = out_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = entry.unpack(&out_path) {
                    send_fs_err(writer, format!("extract {}: {e}", out_rel.display()));
                    return;
                }
            }
        } else {
            // Download to a single file
            if let Some(parent) = std::path::Path::new(&req.path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut file = match std::fs::File::create(&req.path) {
                Ok(f) => f,
                Err(e) => {
                    send_fs_err(writer, format!("create {}: {e}", req.path));
                    return;
                }
            };

            let mut buf = [0u8; 65536];
            let mut bytes_downloaded: u64 = 0;
            let mut last_report: u64 = 0;
            loop {
                match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if file.write_all(&buf[..n]).is_err() {
                            send_fs_err(writer, format!("write {}: I/O error", req.path));
                            return;
                        }
                        bytes_downloaded += n as u64;
                        if bytes_downloaded - last_report >= 65536 {
                            last_report = bytes_downloaded;
                            let progress = DownloadProgress {
                                bytes_downloaded,
                                total_bytes,
                            };
                            let _ = frame::send_json(writer, frame::DOWNLOAD_PROGRESS, &progress);
                        }
                    }
                    Err(e) => {
                        send_fs_err(writer, format!("download read: {e}"));
                        return;
                    }
                }
            }
        }

        send_fs_ok(writer);
    }

    /// Reader wrapper that reports download progress through the frame protocol.
    struct DownloadProgressWriter<'a, W: Write> {
        writer: &'a mut W,
        bytes: u64,
        total_bytes: Option<u64>,
        last_report: u64,
    }

    /// Tee reader: reads from inner, writes to writer (for progress tracking).
    struct TeeReader<'a, R: Read, W: Write> {
        inner: R,
        progress: DownloadProgressWriter<'a, W>,
    }

    impl<'a, R: Read, W: Write> TeeReader<'a, R, W> {
        fn new(inner: R, progress: DownloadProgressWriter<'a, W>) -> Self {
            Self { inner, progress }
        }
    }

    impl<'a, R: Read, W: Write> Read for TeeReader<'a, R, W> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.progress.bytes += n as u64;
            if self.progress.bytes - self.progress.last_report >= 65536 {
                self.progress.last_report = self.progress.bytes;
                let progress = DownloadProgress {
                    bytes_downloaded: self.progress.bytes,
                    total_bytes: self.progress.total_bytes,
                };
                let _ = frame::send_json(self.progress.writer, frame::DOWNLOAD_PROGRESS, &progress);
            }
            Ok(n)
        }
    }

    fn handle_stat(req: &StatRequest, writer: &mut impl Write) {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(&req.path) {
            Ok(m) => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let resp = StatResponse {
                    size: m.len(),
                    mode: m.mode(),
                    mtime,
                    is_dir: m.is_dir(),
                    is_file: m.is_file(),
                    is_symlink: m.file_type().is_symlink(),
                };
                let _ = frame::send_json(writer, frame::STAT_RESP, &resp);
            }
            Err(e) => send_fs_err(writer, format!("stat {}: {}", req.path, e)),
        }
    }

    fn handle_remove(req: &RemoveRequest, writer: &mut impl Write) {
        let is_dir = std::fs::symlink_metadata(&req.path).is_ok_and(|m| m.is_dir());
        let result = if req.recursive && is_dir {
            std::fs::remove_dir_all(&req.path)
        } else if req.recursive {
            std::fs::remove_file(&req.path)
        } else {
            std::fs::remove_file(&req.path).or_else(|_| std::fs::remove_dir(&req.path))
        };
        match result {
            Ok(()) => send_fs_ok(writer),
            Err(e) => send_fs_err(writer, format!("remove {}: {}", req.path, e)),
        }
    }

    fn handle_discard(req: &DiscardRequest, writer: &mut impl Write) {
        // Find the overlay upper dir for the mount point containing this path
        let mount_point = if req.path.starts_with("/workspace") {
            "/workspace"
        } else {
            return send_fs_err(writer, format!("discard: unsupported path {}", req.path));
        };

        let upper_dir = match resolve_overlay_upper(mount_point) {
            Some((upper, _, _)) => upper,
            None => {
                return send_fs_err(writer, "discard: not an overlay mount".to_string());
            }
        };

        // Map /workspace/foo/bar.rs → {upper_dir}/foo/bar.rs
        let rel = req.path.strip_prefix(mount_point).unwrap_or(&req.path);
        let upper_path = format!("{}{}", upper_dir, rel);

        // The entry might be:
        //  1. A modified/created file or dir in the upper → remove it
        //  2. A whiteout (char device 0,0) for a deleted file → remove it
        //  3. Nothing at this exact path, but a parent dir is opaque
        //     (whole directory was deleted) → remove the opaque xattr
        //     from the nearest opaque ancestor so the lower entry shows through

        let upper = std::path::Path::new(&upper_path);
        let result = if upper.is_dir() {
            std::fs::remove_dir_all(&upper_path)
        } else if upper.exists() || upper.symlink_metadata().is_ok() {
            // exists() returns false for whiteouts/special files, but
            // symlink_metadata succeeds for any inode including char devices
            std::fs::remove_file(&upper_path)
        } else {
            // Nothing at this path in upper — check for an opaque parent dir.
            // Walk up from the target to find the nearest opaque directory and
            // remove it entirely so the whole subtree from lower is restored.
            match find_opaque_ancestor(&upper_dir, rel) {
                Some(opaque_dir) => std::fs::remove_dir_all(&opaque_dir),
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("nothing to discard at {}", upper_path),
                )),
            }
        };

        match result {
            Ok(()) => {
                // Drop the kernel's dentry/inode cache so the overlay VFS
                // re-reads from lower+upper after our direct upper-dir mutation.
                let _ = std::fs::write("/proc/sys/vm/drop_caches", b"2");
                send_fs_ok(writer);
            }
            Err(e) => send_fs_err(writer, format!("discard {}: {}", upper_path, e)),
        }
    }

    /// Walk up from `rel` (e.g. "/src/main.rs") looking for an opaque directory
    /// in the upper dir. Returns the full upper path of the opaque dir if found.
    fn find_opaque_ancestor(upper_dir: &str, rel: &str) -> Option<String> {
        let mut current = rel;
        loop {
            let (parent, _) = current.rsplit_once('/')?;
            if parent.is_empty() {
                return None; // reached root, nothing to do
            }
            let candidate = format!("{}{}", upper_dir, parent);
            let p = std::path::Path::new(&candidate);
            if p.is_dir() && is_overlay_opaque(&candidate) {
                return Some(candidate);
            }
            current = parent;
        }
    }

    /// Check if a directory has the overlay opaque xattr (trusted.overlay.opaque=y).
    fn is_overlay_opaque(path: &str) -> bool {
        use std::ffi::CString;
        let Ok(c_path) = CString::new(path) else {
            return false;
        };
        let attr = c"trusted.overlay.opaque";
        let mut buf = [0u8; 2];
        let ret = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                attr.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        ret == 1 && buf[0] == b'y'
    }

    fn handle_rename(req: &RenameRequest, writer: &mut impl Write) {
        match std::fs::rename(&req.old_path, &req.new_path) {
            Ok(()) => send_fs_ok(writer),
            Err(e) => send_fs_err(
                writer,
                format!("rename {} -> {}: {}", req.old_path, req.new_path, e),
            ),
        }
    }

    fn handle_piped_exec(
        req: &ExecRequest,
        vsock_reader: std::net::TcpStream,
        vsock_writer: std::net::TcpStream,
    ) {
        let mut cmd = Command::new(&req.argv[0]);
        if req.argv.len() > 1 {
            cmd.args(&req.argv[1..]);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        if let Some(ref cwd) = req.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let name = format!(
            "exec-{}",
            NEXT_EXEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let cgroup = match crate::cgroup::Cgroup::create(&name) {
            Ok(cgroup) => Some(cgroup),
            Err(e) => {
                eprintln!("shuru-guest: exec without a cgroup: {}", e);
                None
            }
        };
        let procs = match cgroup.as_ref().map(|c| c.procs()).transpose() {
            Ok(procs) => procs,
            Err(e) => {
                let msg = format!("failed to spawn: {}", e);
                let mut w = vsock_writer;
                let _ = frame::write_frame(&mut w, frame::ERROR, msg.as_bytes());
                return;
            }
        };
        let procs_fd = procs.as_ref().map(|fd| fd.as_raw_fd());
        // Its own process group too, which a terminal program may expect.
        unsafe {
            cmd.pre_exec(move || {
                if let Some(fd) = procs_fd {
                    crate::cgroup::join(fd)?;
                }
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        match crate::children::spawn_waited(&mut cmd) {
            Ok((mut child, leader, exit)) => {
                drop(procs);
                let scope = std::sync::Arc::new(match cgroup {
                    Some(cgroup) => Scope::Cgroup(cgroup),
                    None => Scope::Group(leader),
                });
                if let Some(id) = req.id {
                    EXECS.lock().unwrap().insert(id, scope.clone());
                }

                // Channel serializes all frame writes to prevent interleaving
                let (tx, rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();

                // Writer thread: drains channel, writes frames to vsock
                let mut frame_writer = vsock_writer;
                let writer_thread = std::thread::spawn(move || {
                    for (frame_type, payload) in rx {
                        if frame::write_frame(&mut frame_writer, frame_type, &payload).is_err() {
                            break;
                        }
                    }
                });

                // Thread: child stdout -> STDOUT frames
                let child_stdout = child.stdout.take().unwrap();
                let tx_stdout = tx.clone();
                let stdout_thread = std::thread::spawn(move || {
                    let mut stdout = child_stdout;
                    let mut buf = [0u8; 8192];
                    loop {
                        match stdout.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if tx_stdout.send((frame::STDOUT, buf[..n].to_vec())).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });

                // Thread: child stderr -> STDERR frames
                let child_stderr = child.stderr.take().unwrap();
                let tx_stderr = tx.clone();
                let stderr_thread = std::thread::spawn(move || {
                    let mut stderr = child_stderr;
                    let mut buf = [0u8; 8192];
                    loop {
                        match stderr.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if tx_stderr.send((frame::STDERR, buf[..n].to_vec())).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });

                // The input thread below can block writing to a child that
                // never reads its stdin, and then cannot see the host hang
                // up. Watch for that separately and kill the group.
                let hangup_watch = match (vsock_reader.try_clone(), std::io::pipe()) {
                    (Ok(conn), Ok((done_rx, done_tx))) => {
                        let scope = scope.clone();
                        let t = std::thread::spawn(move || kill_on_hangup(conn, &scope, done_rx));
                        Some((t, done_tx))
                    }
                    _ => None,
                };

                // Thread: vsock STDIN/KILL frames -> child stdin
                let child_stdin = child.stdin.take().unwrap();
                let input_scope = scope.clone();
                let input_thread = std::thread::spawn(move || {
                    let mut stdin = child_stdin;
                    let mut reader = vsock_reader;
                    loop {
                        match frame::read_frame(&mut reader) {
                            Ok(Some((frame::STDIN, data))) => {
                                if stdin.write_all(&data).is_err() {
                                    break;
                                }
                                let _ = stdin.flush();
                            }
                            Ok(Some((frame::KILL, _))) => {
                                input_scope.signal(libc::SIGTERM);
                                break;
                            }
                            _ => break,
                        }
                    }
                });

                // Wait for output to drain, then wait for child
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                let status = exit.wait();
                if let Some(id) = req.id {
                    EXECS.lock().unwrap().remove(&id);
                }
                if let Some((t, done_tx)) = hangup_watch {
                    drop(done_tx);
                    let _ = t.join();
                }
                let exit_code = if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    -1
                };

                let _ = tx.send((frame::EXIT, frame::exit_payload(exit_code).to_vec()));
                drop(tx);
                let _ = writer_thread.join();

                // Input thread will exit when vsock closes or we drop
                drop(input_thread);
            }
            Err(e) => {
                let msg = format!("failed to spawn: {}", e);
                let mut w = vsock_writer;
                let _ = frame::write_frame(&mut w, frame::ERROR, msg.as_bytes());
            }
        }
    }

    /// Kill the exec when the host hangs up. Returns without killing once
    /// the write end of `done` is closed.
    fn kill_on_hangup(conn: std::net::TcpStream, scope: &Scope, done: std::io::PipeReader) {
        use std::os::unix::io::AsRawFd;
        let mut fds = [
            libc::pollfd {
                fd: done.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: conn.as_raw_fd(),
                events: libc::POLLRDHUP,
                revents: 0,
            },
        ];
        loop {
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return;
            }
            if fds[0].revents != 0 {
                return;
            }
            if fds[1].revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0 {
                scope.signal(libc::SIGKILL);
                return;
            }
        }
    }

    /// Find the overlay upper dir for a mount point by parsing /proc/mounts.
    /// Returns (watch_path, prefix_to_strip, prefix_to_add) if overlay found.
    fn resolve_overlay_upper(mount_point: &str) -> Option<(String, String, String)> {
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
        for line in mounts.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }
            if parts[0] != "overlay" || parts[1] != mount_point {
                continue;
            }
            // Parse mount options to find upperdir=...
            for opt in parts[3].split(',') {
                if let Some(upper) = opt.strip_prefix("upperdir=") {
                    return Some((
                        upper.to_string(),       // watch this path
                        upper.to_string(),       // strip this prefix from events
                        mount_point.to_string(), // replace with this
                    ));
                }
            }
        }
        None
    }

    fn handle_watch(req: &WatchRequest, mut writer: std::net::TcpStream) {
        use notify::{RecursiveMode, Watcher};

        // Watch the overlay mount directly. Linux >= 5.11 propagates inotify
        // events from the upper layer through to the merged mount, so we see
        // creates/modifies/deletes with proper file paths in the caller's
        // namespace — no whiteout char devices, no path rewriting.
        let watch_dir = req.path.clone();

        let (tx, rx) = std::sync::mpsc::channel();

        let mut watcher =
            match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            }) {
                Ok(w) => w,
                Err(_) => return,
            };

        if watcher
            .watch(std::path::Path::new(&watch_dir), RecursiveMode::Recursive)
            .is_err()
        {
            return;
        }

        let gitignore = {
            let mut builder = ignore::gitignore::GitignoreBuilder::new(&req.path);
            let _ = builder.add_line(None, ".git");
            builder.add(format!("{}/.gitignore", req.path));
            builder.build().unwrap_or_else(|_| {
                let mut b = ignore::gitignore::GitignoreBuilder::new(&req.path);
                let _ = b.add_line(None, ".git");
                b.build().unwrap()
            })
        };

        loop {
            // Block until next event (or timeout to check vsock hangup)
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(event) => {
                    // Drain buffered events
                    let mut events = vec![event];
                    while let Ok(ev) = rx.try_recv() {
                        events.push(ev);
                    }

                    for event in events {
                        // Access events (open/read/close) are noise for diff
                        // tracking and agent hooks alike — only real content
                        // changes matter.
                        if matches!(event.kind, notify::EventKind::Access(_)) {
                            continue;
                        }
                        // A rename already arrived as its From and To halves.
                        if matches!(
                            event.kind,
                            notify::EventKind::Modify(notify::event::ModifyKind::Name(
                                notify::event::RenameMode::Both
                            ))
                        ) {
                            continue;
                        }
                        for path in &event.paths {
                            if gitignore.matched(path, path.is_dir()).is_ignore() {
                                continue;
                            }
                            if path.is_dir() {
                                continue;
                            }
                            let path_str = match path.to_str() {
                                Some(p) => p,
                                None => continue,
                            };
                            let kind = match event.kind {
                                notify::EventKind::Create(_) => shuru_proto::watch_kind::CREATE,
                                notify::EventKind::Remove(_) => shuru_proto::watch_kind::DELETE,
                                notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
                                    shuru_proto::watch_kind::RENAME
                                }
                                _ => shuru_proto::watch_kind::MODIFY,
                            };
                            let evt = WatchEvent {
                                kind,
                                path: path_str.to_string(),
                            };
                            if frame::write_frame(&mut writer, frame::WATCH_EVENT, &evt.encode())
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if crate::files::hung_up(&writer) {
                break;
            }
        }
    }

    fn handle_tty_exec(vsock_fd: i32, req: &ExecRequest) {
        unsafe {
            // Set up initial winsize
            let ws = libc::winsize {
                ws_row: req.rows.unwrap_or(24),
                ws_col: req.cols.unwrap_or(80),
                ws_xpixel: 0,
                ws_ypixel: 0,
            };

            // Allocate PTY pair
            let mut master: libc::c_int = 0;
            let mut slave: libc::c_int = 0;
            if libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &ws as *const libc::winsize as *mut libc::winsize,
            ) < 0
            {
                write_frame_fd(vsock_fd, frame::ERROR, b"openpty failed");
                libc::close(vsock_fd);
                return;
            }

            let exit = crate::children::Exit::default();
            let status = exit.clone();
            let forked = crate::children::start(
                || {
                    let pid = libc::fork();
                    if pid == 0 {
                        tty_child(master, slave, vsock_fd, req);
                    }
                    if pid < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(pid)
                    }
                },
                Box::new(move |s| status.set(s)),
            );
            let Ok(child) = forked else {
                write_frame_fd(vsock_fd, frame::ERROR, b"fork failed");
                libc::close(master);
                libc::close(slave);
                libc::close(vsock_fd);
                return;
            };
            // === PARENT ===
            libc::close(slave);
            pty_poll_loop(vsock_fd, master, child, &exit);
            libc::close(master);
            libc::close(vsock_fd);
        }
    }

    /// The child side of a tty exec, between fork and exec.
    unsafe fn tty_child(master: i32, slave: i32, vsock_fd: i32, req: &ExecRequest) -> ! {
        use std::ffi::CString;

        libc::close(master);
        libc::close(vsock_fd);
        libc::setsid();
        libc::ioctl(slave, libc::TIOCSCTTY, 0);
        libc::dup2(slave, 0);
        libc::dup2(slave, 1);
        libc::dup2(slave, 2);
        if slave > 2 {
            libc::close(slave);
        }

        // Close any other inherited fds
        for fd in 3..1024 {
            libc::close(fd);
        }

        // Change directory if requested
        if let Some(ref cwd) = req.cwd {
            if let Ok(dir) = CString::new(cwd.as_str()) {
                libc::chdir(dir.as_ptr());
            }
        }

        // Set environment
        for (k, v) in &req.env {
            if let Ok(var) = CString::new(format!("{}={}", k, v)) {
                libc::putenv(var.into_raw());
            }
        }
        if !req.env.contains_key("TERM") {
            let term = CString::new("TERM=xterm-256color").unwrap();
            libc::putenv(term.into_raw());
        }
        if !req.env.contains_key("PATH") {
            let path =
                CString::new("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                    .unwrap();
            libc::putenv(path.into_raw());
        }

        // Build argv and exec
        let c_args: Vec<CString> = req
            .argv
            .iter()
            .map(|s| CString::new(s.as_str()).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();
        let c_argv: Vec<*const libc::c_char> = c_args
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        libc::execvp(c_argv[0], c_argv.as_ptr());

        // If execvp returns, it failed - print error to the PTY
        let err = std::io::Error::last_os_error();
        let msg = format!("shuru: {}: {}\n", req.argv[0], err);
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::_exit(127);
    }

    fn pty_poll_loop(
        vsock_fd: i32,
        master_fd: i32,
        child: crate::children::Watch,
        exit: &crate::children::Exit,
    ) {
        let mut vsock_buf: Vec<u8> = Vec::new();
        let mut read_buf = [0u8; 4096];

        loop {
            let mut fds = [
                libc::pollfd {
                    fd: vsock_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: master_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }

            // Check vsock for binary frames (stdin, resize)
            if fds[0].revents & libc::POLLIN != 0 {
                let n = unsafe {
                    libc::read(
                        vsock_fd,
                        read_buf.as_mut_ptr() as *mut libc::c_void,
                        read_buf.len(),
                    )
                };
                if n <= 0 {
                    crate::children::signal(child, false, libc::SIGHUP);
                    break;
                }
                vsock_buf.extend_from_slice(&read_buf[..n as usize]);

                // Process complete binary frames
                while let Some((msg_type, payload_start, total_len)) = frame::try_parse(&vsock_buf)
                {
                    let payload = &vsock_buf[payload_start..total_len];
                    match msg_type {
                        frame::STDIN => unsafe {
                            libc::write(
                                master_fd,
                                payload.as_ptr() as *const libc::c_void,
                                payload.len(),
                            );
                        },
                        frame::RESIZE => {
                            if let Some((rows, cols)) = frame::parse_resize(payload) {
                                unsafe {
                                    let ws = libc::winsize {
                                        ws_row: rows,
                                        ws_col: cols,
                                        ws_xpixel: 0,
                                        ws_ypixel: 0,
                                    };
                                    libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
                                }
                            }
                        }
                        _ => {}
                    }
                    vsock_buf.drain(..total_len);
                }
            }

            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                crate::children::signal(child, false, libc::SIGHUP);
                break;
            }

            // Check PTY master for output — send raw bytes as STDOUT frames
            if fds[1].revents & libc::POLLIN != 0 {
                let n = unsafe {
                    libc::read(
                        master_fd,
                        read_buf.as_mut_ptr() as *mut libc::c_void,
                        read_buf.len(),
                    )
                };
                if n > 0 {
                    write_frame_fd(vsock_fd, frame::STDOUT, &read_buf[..n as usize]);
                }
            }

            if fds[1].revents & libc::POLLHUP != 0 {
                // Child closed PTY — drain remaining output
                loop {
                    let n = unsafe {
                        libc::read(
                            master_fd,
                            read_buf.as_mut_ptr() as *mut libc::c_void,
                            read_buf.len(),
                        )
                    };
                    if n <= 0 {
                        break;
                    }
                    write_frame_fd(vsock_fd, frame::STDOUT, &read_buf[..n as usize]);
                }
                break;
            }
        }

        let status = exit.wait();

        let exit_code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            1
        };

        write_frame_fd(vsock_fd, frame::EXIT, &frame::exit_payload(exit_code));
    }

    fn forward_accept_loop(listener_fd: i32) {
        loop {
            let client_fd = unsafe {
                libc::accept4(
                    listener_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };

            if client_fd < 0 {
                continue;
            }

            std::thread::spawn(move || {
                handle_forward_connection(client_fd);
            });
        }
    }

    fn handle_forward_connection(fd: i32) {
        let mut stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        if authenticate(&mut stream).is_err() {
            return;
        }
        let _ = stream.set_nodelay(true);

        // Read the forward request frame
        let (_msg_type, payload) = match frame::read_frame(&mut stream) {
            Ok(Some(f)) => f,
            _ => return,
        };

        let req: ForwardRequest = match serde_json::from_slice(&payload) {
            Ok(r) => r,
            Err(e) => {
                let resp = ForwardResponse {
                    status: "error".into(),
                    message: Some(format!("invalid request: {}", e)),
                };
                let _ = frame::send_json(&mut stream, frame::FWD_RESP, &resp);
                return;
            }
        };

        let host = req.host.as_deref().unwrap_or("127.0.0.1");
        let tcp_stream = match forward_connect(host, req.port) {
            Ok(s) => s,
            Err(e) => {
                let resp = ForwardResponse {
                    status: "error".into(),
                    message: Some(format!("connect to {host}:{}: {e}", req.port)),
                };
                let _ = frame::send_json(&mut stream, frame::FWD_RESP, &resp);
                return;
            }
        };

        // Send success response
        let resp = ForwardResponse {
            status: "ok".into(),
            message: None,
        };
        if frame::send_json(&mut stream, frame::FWD_RESP, &resp).is_err() {
            return;
        }

        // Bidirectional relay between vsock and TCP
        forward_relay(stream, tcp_stream, req.whole_close);
    }

    fn forward_connect(host: &str, port: u16) -> std::io::Result<std::net::TcpStream> {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
        // Loopback whatever the image has in /etc/hosts (RFC 6761).
        let addrs: Vec<SocketAddr> = if host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        {
            vec![
                (Ipv4Addr::LOCALHOST, port).into(),
                (Ipv6Addr::LOCALHOST, port).into(),
            ]
        } else {
            (host, port).to_socket_addrs()?.collect()
        };
        let mut last = None;
        for addr in addrs {
            match std::net::TcpStream::connect_timeout(&addr, FORWARD_CONNECT_TIMEOUT) {
                Ok(stream) => return Ok(stream),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no address found")
        }))
    }

    fn forward_relay(vsock: std::net::TcpStream, tcp: std::net::TcpStream, whole_close: bool) {
        let mut vsock_read = vsock.try_clone().expect("clone vsock");
        let mut tcp_write = tcp.try_clone().expect("clone tcp");
        let mut tcp_read = tcp;
        let mut vsock_write = vsock;
        // The host gives up on a stalled target, so the guest does too.
        if whole_close {
            let _ = tcp_write.set_write_timeout(Some(FORWARD_STALL));
        }

        let t1 = std::thread::spawn(move || {
            let _ = std::io::copy(&mut vsock_read, &mut tcp_write);
            // Shutting down both ways also ends the read of the other thread.
            let how = if whole_close {
                std::net::Shutdown::Both
            } else {
                std::net::Shutdown::Write
            };
            let _ = tcp_write.shutdown(how);
        });
        let t2 = std::thread::spawn(move || {
            let _ = std::io::copy(&mut tcp_read, &mut vsock_write);
            let _ = vsock_write.shutdown(std::net::Shutdown::Write);
        });
        let _ = t1.join();
        let _ = t2.join();
    }

    extern "C" fn sigterm_handler(_: libc::c_int) {
        unsafe {
            libc::sync();
            libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
        }
    }

    pub fn run() -> ! {
        ensure_stdio();
        stage("starting as PID 1");

        // Set PATH early so all child processes inherit it
        std::env::set_var(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );

        if let Err(e) = crate::disk::boot() {
            stage(&format!("root boot failed: {e}"));
            unsafe {
                libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
            }
            loop {
                std::thread::park();
            }
        }
        mount_filesystems();
        if let Err(e) = crate::disk::check_root() {
            stage(&format!("root boot failed: {e}"));
            unsafe {
                libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
            }
            loop {
                std::thread::park();
            }
        }
        crate::disk::start_watchdog();
        ensure_stdio();
        stage("filesystems mounted");

        let boot_identity = std::fs::read_to_string("/proc/cmdline")
            .and_then(|cmdline| crate::identity::parse(&cmdline))
            .unwrap_or_else(|_| {
                eprintln!("shuru-guest: missing or invalid guest identity");
                std::process::exit(1);
            });
        crate::identity::set(boot_identity);

        // Set hostname
        let hostname = b"shuru\0";
        unsafe {
            libc::sethostname(hostname.as_ptr() as *const libc::c_char, 5);
        }

        setup_networking();
        stage("networking ready");

        if let Err(e) = crate::cgroup::init() {
            eprintln!("shuru-guest: no exec cgroups: {}", e);
        }
        crate::children::start_reaper().expect("start the reaper");

        let started = crate::services::start_all();
        if started > 0 {
            stage(&format!("{} service(s) starting", started));
        }

        // Register signal handlers (PID 1 has no default signal dispositions)
        unsafe {
            libc::signal(
                libc::SIGTERM,
                sigterm_handler as *const () as libc::sighandler_t,
            );
            libc::signal(
                libc::SIGINT,
                sigterm_handler as *const () as libc::sighandler_t,
            );
        }

        let listener_fd = create_vsock_listener(VSOCK_PORT);
        stage(&format!("vsock listening on port {}", VSOCK_PORT));

        let fwd_listener_fd = create_vsock_listener(VSOCK_PORT_FORWARD);
        stage(&format!(
            "port forward listener on port {}",
            VSOCK_PORT_FORWARD
        ));
        std::thread::spawn(move || {
            forward_accept_loop(fwd_listener_fd);
        });

        loop {
            let client_fd = unsafe {
                libc::accept4(
                    listener_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };

            if client_fd < 0 {
                continue;
            }

            eprintln!("shuru-guest: accepted vsock connection");

            std::thread::spawn(move || {
                handle_connection(client_fd);
            });
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    guest::run();

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("shuru-guest is a Linux-only binary meant to run inside a VM");
        std::process::exit(1);
    }
}
