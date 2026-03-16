use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossbeam_channel::Receiver;

use shuru_darwin::terminal;
use shuru_darwin::network::FileHandleNetworkAttachment;
use shuru_darwin::*;

use shuru_proto::{
    frame, ChmodRequest, CopyRequest, ExecRequest, ForwardRequest, ForwardResponse, FsOkResponse,
    MkdirRequest, MountRequest, MountResponse, PortMapping, ReadDirRequest, ReadDirResponse,
    ReadFileRequest, RemoveRequest, RenameRequest, StatRequest, StatResponse, WatchRequest,
    WriteFileRequest, WriteFileResponse, VSOCK_PORT, VSOCK_PORT_FORWARD,
};

// --- Mount types ---

#[derive(Debug, Clone)]
pub struct MountConfig {
    pub host_path: String,
    pub guest_path: String,
}

// --- VmConfigBuilder ---

pub struct VmConfigBuilder {
    kernel: Option<String>,
    rootfs: Option<String>,
    initrd: Option<String>,
    cpus: usize,
    memory_mb: u64,
    console: bool,
    verbose: bool,
    network_fd: Option<i32>,
    mounts: Vec<MountConfig>,
}

impl VmConfigBuilder {
    pub(crate) fn new() -> Self {
        VmConfigBuilder {
            kernel: None,
            rootfs: None,
            initrd: None,
            cpus: 2,
            memory_mb: 2048,
            console: true,
            verbose: false,
            network_fd: None,
            mounts: Vec::new(),
        }
    }

    /// When false, serial console stdin is disconnected and stdout goes to
    /// stderr. This prevents the serial console from consuming host stdin
    /// in exec/shell mode.
    pub fn console(mut self, enabled: bool) -> Self {
        self.console = enabled;
        self
    }

    /// When true, serial console output (kernel dmesg, initramfs) is shown
    /// even in non-console mode. Default is false (quiet).
    pub fn verbose(mut self, enabled: bool) -> Self {
        self.verbose = enabled;
        self
    }

    pub fn kernel(mut self, path: impl Into<String>) -> Self {
        self.kernel = Some(path.into());
        self
    }

    pub fn rootfs(mut self, path: impl Into<String>) -> Self {
        self.rootfs = Some(path.into());
        self
    }

    pub fn initrd(mut self, path: impl Into<String>) -> Self {
        self.initrd = Some(path.into());
        self
    }

    pub fn cpus(mut self, n: usize) -> Self {
        self.cpus = n;
        self
    }

    pub fn memory_mb(mut self, mb: u64) -> Self {
        self.memory_mb = mb;
        self
    }

    /// Attach a network device via a socketpair fd for proxy-based networking.
    pub fn network_fd(mut self, fd: i32) -> Self {
        self.network_fd = Some(fd);
        self
    }

    /// Add a host directory mount (virtio-fs).
    pub fn mount(mut self, config: MountConfig) -> Self {
        self.mounts.push(config);
        self
    }

    pub fn build(self) -> Result<Sandbox> {
        let kernel_path = self.kernel.context("kernel path is required")?;
        let rootfs_path = self.rootfs.context("rootfs path is required")?;

        if !VirtualMachine::supported() {
            bail!("Virtualization is not supported on this machine");
        }

        let boot_loader = LinuxBootLoader::new_with_kernel(&kernel_path);
        if let Some(ref initrd) = self.initrd {
            boot_loader.set_initrd(initrd);
        }

        let cmdline = if self.verbose {
            "console=hvc0 root=/dev/vda rw"
        } else {
            "console=hvc0 root=/dev/vda rw quiet"
        };
        boot_loader.set_command_line(cmdline);

        let memory_bytes = self.memory_mb * 1024 * 1024;
        let config = VirtualMachineConfiguration::new(&boot_loader, self.cpus, memory_bytes);

        let dev_null; // keep the File alive so the fd stays valid
        let serial_attachment = if self.console {
            FileHandleSerialAttachment::new(
                std::io::stdin().as_raw_fd(),
                std::io::stdout().as_raw_fd(),
            )
        } else if self.verbose {
            FileHandleSerialAttachment::new_write_only(std::io::stderr().as_raw_fd())
        } else {
            dev_null = std::fs::File::open("/dev/null")
                .map_err(|e| anyhow::anyhow!("failed to open /dev/null: {}", e))?;
            FileHandleSerialAttachment::new_write_only(dev_null.as_raw_fd())
        };
        let serial = VirtioConsoleSerialPort::new_with_attachment(&serial_attachment);
        config.set_serial_ports(&[serial]);

        let disk_attachment = DiskImageAttachment::new_with_options(
            &rootfs_path,
            false,
            DiskImageCachingMode::Cached,
            DiskImageSynchronizationMode::Fsync,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create disk attachment: {}", e))?;
        let block_device = VirtioBlockDevice::new(&disk_attachment);
        config.set_storage_devices(&[&block_device]);

        if let Some(fd) = self.network_fd {
            let net_attachment = FileHandleNetworkAttachment::new(fd);
            let net_device = VirtioNetworkDevice::new_with_attachment(&net_attachment);
            net_device.set_mac_address(&MACAddress::random_local());
            config.set_network_devices(&[net_device]);
        }

        // Set up directory sharing devices (virtio-fs) and mount metadata
        let mut fs_devices: Vec<VirtioFileSystemDevice> = Vec::new();
        let mut mount_requests: Vec<MountRequest> = Vec::new();

        for (i, m) in self.mounts.iter().enumerate() {
            let tag = format!("mount{}", i);
            // Host directory is always read-only from the VM side.
            // Overlay mode uses tmpfs upper layer inside the guest.
            let shared_dir = SharedDirectory::new(&m.host_path, true);
            fs_devices.push(VirtioFileSystemDevice::new(&tag, &shared_dir));
            mount_requests.push(MountRequest {
                tag,
                guest_path: m.guest_path.clone(),
            });
        }

        if !fs_devices.is_empty() {
            config.set_directory_sharing_devices(&fs_devices);
        }

        let socket_device = VirtioSocketDevice::new();
        config.set_socket_devices(&[socket_device]);

        config.set_entropy_devices(&[VirtioEntropyDevice::new()]);
        config.set_memory_balloon_devices(&[VirtioMemoryBalloonDevice::new()]);

        config
            .validate()
            .map_err(|e| anyhow::anyhow!("VM configuration invalid: {}", e))?;

        Ok(Sandbox {
            vm: Arc::new(VirtualMachine::new(&config)),
            mounts: Mutex::new(mount_requests),
        })
    }
}

// --- Sandbox ---

pub struct Sandbox {
    vm: Arc<VirtualMachine>,
    mounts: Mutex<Vec<MountRequest>>,
}

impl Sandbox {
    pub fn builder() -> VmConfigBuilder {
        VmConfigBuilder::new()
    }

    pub fn start(&self) -> Result<()> {
        self.vm
            .start()
            .map_err(|e| anyhow::anyhow!("Failed to start VM: {}", e))
    }

    pub fn stop(&self) -> Result<()> {
        self.vm
            .stop()
            .map_err(|e| anyhow::anyhow!("Failed to stop VM: {}", e))
    }

    pub fn state_channel(&self) -> Receiver<VmState> {
        self.vm.state_channel()
    }

    /// Send pending mount requests over an established vsock connection.
    /// Drains the mount list so subsequent calls are no-ops.
    fn send_mount_requests(
        &self,
        writer: &mut impl Write,
        reader: &mut impl Read,
    ) -> Result<()> {
        let mounts = std::mem::take(&mut *self.mounts.lock().unwrap());
        for req in &mounts {
            frame::send_json(writer, frame::MOUNT_REQ, &req)
                .context("sending mount request")?;
            let (_msg_type, payload) = frame::read_frame(reader)
                .context("reading mount response")?
                .context("guest closed connection during mount init")?;
            let resp: MountResponse = match serde_json::from_slice(&payload) {
                Ok(r) => r,
                Err(_) => {
                    bail!(
                        "guest does not support directory mounts. \
                         Run `shuru upgrade` and recreate the checkpoint to enable --mount."
                    );
                }
            };
            if !resp.ok {
                bail!(
                    "mount failed: {} -> {}: {}",
                    req.tag,
                    req.guest_path,
                    resp.error.unwrap_or_else(|| "unknown error".into())
                );
            }
        }
        Ok(())
    }

    /// Run a command non-interactively over vsock, streaming output to the
    /// provided writers. Returns the guest process exit code.
    pub fn exec(
        &self,
        argv: &[impl AsRef<str>],
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> Result<i32> {
        self.exec_with_env(argv, &HashMap::new(), stdout, stderr)
    }

    pub fn exec_with_env(
        &self,
        argv: &[impl AsRef<str>],
        env: &HashMap<String, String>,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> Result<i32> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = ExecRequest {
            argv: argv.iter().map(|s| s.as_ref().to_string()).collect(),
            env: env.clone(),
            tty: None,
            rows: None,
            cols: None,
            cwd: None,
        };
        frame::send_json(&mut writer, frame::EXEC_REQ, &req)?;

        let mut exit_code = 0;

        loop {
            match frame::read_frame(&mut reader).context("reading vsock response")? {
                Some((frame::STDOUT, payload)) => {
                    stdout.write_all(&payload)?;
                }
                Some((frame::STDERR, payload)) => {
                    stderr.write_all(&payload)?;
                }
                Some((frame::EXIT, payload)) => {
                    exit_code = frame::parse_exit_code(&payload).unwrap_or(0);
                    break;
                }
                Some((frame::ERROR, payload)) => {
                    let msg = String::from_utf8_lossy(&payload);
                    write!(stderr, "guest error: {}", msg)?;
                    exit_code = 1;
                    break;
                }
                Some(_) => {} // unknown type, skip
                None => break, // EOF
            }
        }

        Ok(exit_code)
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = ReadFileRequest { path: path.to_string() };
        frame::send_json(&mut writer, frame::READ_FILE_REQ, &req)?;

        match frame::read_frame(&mut reader).context("reading read_file response")? {
            Some((frame::READ_FILE_RESP, payload)) => Ok(payload),
            Some((frame::ERROR, payload)) => {
                bail!("{}", String::from_utf8_lossy(&payload));
            }
            Some((other, _)) => {
                bail!("unexpected frame type 0x{:02x} in read_file response", other);
            }
            None => bail!("guest closed connection during read_file"),
        }
    }

    pub fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = WriteFileRequest {
            path: path.to_string(),
            len: content.len() as u64,
        };
        frame::send_json(&mut writer, frame::WRITE_FILE_REQ, &req)?;
        frame::write_frame(&mut writer, frame::WRITE_FILE_DATA, content)?;

        let (_msg_type, payload) = frame::read_frame(&mut reader)
            .context("reading write_file response")?
            .context("guest closed connection during write_file")?;

        let resp: WriteFileResponse = serde_json::from_slice(&payload)
            .context("parsing write_file response")?;

        if !resp.ok {
            bail!(
                "write_file failed: {}",
                resp.error.unwrap_or_else(|| "unknown error".into())
            );
        }

        Ok(())
    }

    /// Send a request and expect FS_OK_RESP or ERROR. Used by void fs ops.
    fn void_fs_op(&self, req_frame: u8, req: &impl serde::Serialize) -> Result<()> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        self.send_mount_requests(&mut writer, &mut reader)?;

        frame::send_json(&mut writer, req_frame, req)?;

        match frame::read_frame(&mut reader).context("reading fs op response")? {
            Some((frame::FS_OK_RESP, payload)) => {
                let resp: FsOkResponse =
                    serde_json::from_slice(&payload).context("parsing fs ok response")?;
                if !resp.ok {
                    bail!("{}", resp.error.unwrap_or_else(|| "unknown error".into()));
                }
                Ok(())
            }
            Some((frame::ERROR, payload)) => {
                bail!("{}", String::from_utf8_lossy(&payload));
            }
            Some((other, _)) => {
                bail!("unexpected frame type 0x{:02x}", other);
            }
            None => bail!("guest closed connection"),
        }
    }

    pub fn mkdir(&self, path: &str, recursive: bool) -> Result<()> {
        self.void_fs_op(
            frame::MKDIR_REQ,
            &MkdirRequest { path: path.to_string(), recursive },
        )
    }

    pub fn read_dir(&self, path: &str) -> Result<ReadDirResponse> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = ReadDirRequest { path: path.to_string() };
        frame::send_json(&mut writer, frame::READ_DIR_REQ, &req)?;

        match frame::read_frame(&mut reader).context("reading read_dir response")? {
            Some((frame::READ_DIR_RESP, payload)) => {
                Ok(serde_json::from_slice(&payload).context("parsing read_dir response")?)
            }
            Some((frame::ERROR, payload)) => {
                bail!("{}", String::from_utf8_lossy(&payload));
            }
            Some((other, _)) => {
                bail!("unexpected frame type 0x{:02x} in read_dir response", other);
            }
            None => bail!("guest closed connection during read_dir"),
        }
    }

    pub fn stat(&self, path: &str) -> Result<StatResponse> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = StatRequest { path: path.to_string() };
        frame::send_json(&mut writer, frame::STAT_REQ, &req)?;

        match frame::read_frame(&mut reader).context("reading stat response")? {
            Some((frame::STAT_RESP, payload)) => {
                Ok(serde_json::from_slice(&payload).context("parsing stat response")?)
            }
            Some((frame::ERROR, payload)) => {
                bail!("{}", String::from_utf8_lossy(&payload));
            }
            Some((other, _)) => {
                bail!("unexpected frame type 0x{:02x} in stat response", other);
            }
            None => bail!("guest closed connection during stat"),
        }
    }

    pub fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        self.void_fs_op(
            frame::REMOVE_REQ,
            &RemoveRequest { path: path.to_string(), recursive },
        )
    }

    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        self.void_fs_op(
            frame::RENAME_REQ,
            &RenameRequest { old_path: old_path.to_string(), new_path: new_path.to_string() },
        )
    }

    pub fn copy(&self, src: &str, dst: &str, recursive: bool) -> Result<()> {
        self.void_fs_op(
            frame::COPY_REQ,
            &CopyRequest { src: src.to_string(), dst: dst.to_string(), recursive },
        )
    }

    pub fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        self.void_fs_op(
            frame::CHMOD_REQ,
            &ChmodRequest { path: path.to_string(), mode },
        )
    }

    /// Open a vsock connection for streaming exec. Returns the raw stream
    /// after sending mounts + ExecRequest. Caller manages I/O (reads
    /// STDOUT/STDERR/EXIT frames, writes STDIN/KILL frames).
    pub fn open_exec(
        &self,
        argv: &[impl AsRef<str>],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<TcpStream> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream.try_clone()?;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = ExecRequest {
            argv: argv.iter().map(|s| s.as_ref().to_string()).collect(),
            env: env.clone(),
            tty: None,
            rows: None,
            cols: None,
            cwd: cwd.map(|s| s.to_string()),
        };
        frame::send_json(&mut writer, frame::EXEC_REQ, &req)?;

        Ok(stream)
    }

    /// Open a vsock connection for an interactive shell with PTY support.
    /// Like `open_exec` but with `tty=true`. Returns the raw stream after
    /// sending mounts + ExecRequest. Caller manages I/O using the binary
    /// frame protocol (STDIN/STDOUT/RESIZE/EXIT frames).
    pub fn open_shell(
        &self,
        argv: &[impl AsRef<str>],
        env: &HashMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<TcpStream> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream.try_clone()?;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = ExecRequest {
            argv: argv.iter().map(|s| s.as_ref().to_string()).collect(),
            env: env.clone(),
            tty: Some(true),
            rows: Some(rows),
            cols: Some(cols),
            cwd: None,
        };
        frame::send_json(&mut writer, frame::EXEC_REQ, &req)?;

        Ok(stream)
    }

    /// Open a vsock connection for file watching. Returns a stream that
    /// emits WATCH_EVENT frames until the connection is closed.
    pub fn open_watch(&self, path: &str, recursive: bool) -> Result<TcpStream> {
        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream.try_clone()?;

        self.send_mount_requests(&mut writer, &mut reader)?;

        let req = WatchRequest {
            path: path.to_string(),
            recursive,
        };
        frame::send_json(&mut writer, frame::WATCH_REQ, &req)?;

        Ok(stream)
    }

    /// Run an interactive shell session with PTY support.
    /// Puts the host terminal in raw mode, relays I/O bidirectionally over
    /// vsock, and handles SIGWINCH for window resize.
    /// Returns the guest process exit code.
    pub fn shell(
        &self,
        argv: &[impl AsRef<str>],
        env: &HashMap<String, String>,
    ) -> Result<i32> {
        let stdin_fd = std::io::stdin().as_raw_fd();
        let (rows, cols) = terminal::terminal_size(stdin_fd);

        let stream = self.connect_vsock()?;
        let mut writer = stream.try_clone()?;
        let mut reader = stream;

        // Mount phase (sync, before raw mode)
        self.send_mount_requests(&mut writer, &mut reader)?;

        // Send ExecRequest with tty=true
        let req = ExecRequest {
            argv: argv.iter().map(|s| s.as_ref().to_string()).collect(),
            env: env.clone(),
            tty: Some(true),
            rows: Some(rows),
            cols: Some(cols),
            cwd: None,
        };
        frame::send_json(&mut writer, frame::EXEC_REQ, &req)?;

        // Enter raw mode - TerminalState restores on drop
        let _raw_guard = terminal::TerminalState::enter_raw_mode(stdin_fd);

        // Set up kqueue-based stdin relay (zero-latency I/O multiplexing)
        let (relay, shutdown_signal) =
            terminal::StdinRelay::new(stdin_fd).expect("failed to init stdin relay");

        let exit_code = Arc::new(Mutex::new(0i32));

        // Thread A: stdin → vsock (kqueue blocks until data/resize/shutdown)
        let mut vsock_writer = writer.try_clone()?;
        let stdin_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match relay.wait() {
                    terminal::StdinEvent::Ready => {
                        let n = terminal::read_raw(stdin_fd, &mut buf);
                        if n == 0 {
                            break;
                        }
                        if frame::write_frame(&mut vsock_writer, frame::STDIN, &buf[..n]).is_err()
                        {
                            break;
                        }
                    }
                    terminal::StdinEvent::Resize => {
                        let (rows, cols) = terminal::terminal_size(stdin_fd);
                        let payload = frame::resize_payload(rows, cols);
                        if frame::write_frame(&mut vsock_writer, frame::RESIZE, &payload).is_err()
                        {
                            break;
                        }
                    }
                    terminal::StdinEvent::Shutdown => break,
                }
            }
        });

        // Thread B: vsock -> stdout (read binary frames, write raw output)
        // Uses BufWriter + deferred flush to batch rapid TUI updates into
        // fewer terminal writes, preventing visible tearing/flickering.
        let exit_code_b = exit_code.clone();
        let vsock_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut stdout = BufWriter::new(std::io::stdout());
            loop {
                match frame::read_frame(&mut reader) {
                    Ok(Some((frame::STDOUT, payload))) => {
                        let _ = stdout.write_all(&payload);
                        // Only flush to the terminal when no more data is
                        // already buffered from the vsock. This batches
                        // rapid sequential messages (e.g. a full TUI
                        // screen redraw) into a single terminal write.
                        if reader.buffer().is_empty() {
                            let _ = stdout.flush();
                        }
                    }
                    Ok(Some((frame::EXIT, payload))) => {
                        let _ = stdout.flush();
                        *exit_code_b.lock().unwrap() =
                            frame::parse_exit_code(&payload).unwrap_or(0);
                        break;
                    }
                    Ok(Some((frame::ERROR, payload))) => {
                        let _ = stdout.flush();
                        let msg = String::from_utf8_lossy(&payload);
                        let _ = std::io::stderr()
                            .write_all(format!("guest error: {}\r\n", msg).as_bytes());
                        *exit_code_b.lock().unwrap() = 1;
                        break;
                    }
                    Ok(Some(_)) => {} // unknown type, skip
                    Ok(None) | Err(_) => break,
                }
            }
            let _ = stdout.flush();
            shutdown_signal.signal();
        });

        // Wait for threads
        let _ = vsock_thread.join();
        let _ = stdin_thread.join();

        // Terminal restored by _raw_guard drop
        // SIGWINCH restored by StdinRelay drop
        let code = *exit_code.lock().unwrap();
        Ok(code)
    }

    /// Start port forwarding proxies. Returns a handle that stops all
    /// listeners when dropped.
    pub fn start_port_forwarding(&self, forwards: &[PortMapping]) -> Result<PortForwardHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut listeners = Vec::new();

        for mapping in forwards {
            let addr = format!("127.0.0.1:{}", mapping.host_port);
            let tcp_listener = TcpListener::bind(&addr)
                .with_context(|| format!("Failed to bind port {}", mapping.host_port))?;
            tcp_listener.set_nonblocking(true)?;

            let guest_port = mapping.guest_port;
            let vm = Arc::clone(&self.vm);
            let stop_flag = stop.clone();

            eprintln!(
                "shuru: forwarding 127.0.0.1:{} -> guest:{}",
                mapping.host_port, mapping.guest_port
            );

            let handle = std::thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    match tcp_listener.accept() {
                        Ok((tcp_stream, _)) => {
                            // macOS accept() inherits non-blocking from the
                            // listener — force blocking for the relay.
                            let _ = tcp_stream.set_nonblocking(false);
                            let vm = Arc::clone(&vm);
                            std::thread::spawn(move || {
                                if let Err(e) = handle_forward_connection(tcp_stream, &vm, guest_port) {
                                    tracing::debug!("port forward error: {}", e);
                                }
                            });
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(e) => {
                            if !stop_flag.load(Ordering::Relaxed) {
                                tracing::debug!("accept error on port forward listener: {}", e);
                            }
                            break;
                        }
                    }
                }
            });

            listeners.push(handle);
        }

        Ok(PortForwardHandle {
            stop,
            threads: listeners,
        })
    }

    fn connect_vsock(&self) -> Result<TcpStream> {
        let state_rx = self.vm.state_channel();
        for attempt in 1..=50 {
            // Check if VM died (e.g. guest mount failure -> reboot POWER_OFF)
            if let Ok(state) = state_rx.try_recv() {
                match state {
                    VmState::Stopped => {
                        bail!("VM stopped during startup - check boot output above for errors")
                    }
                    VmState::Error => bail!("VM encountered an error during startup"),
                    _ => {}
                }
            }
            match self.vm.connect_to_vsock_port(VSOCK_PORT) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Err(e) => {
                    if attempt == 50 {
                        bail!("Failed to connect to guest after {} attempts: {}", attempt, e);
                    }
                    tracing::debug!("vsock connect attempt {} failed: {}", attempt, e);
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        unreachable!()
    }
}

// --- Port forwarding ---

/// Handle returned by `start_port_forwarding`. Signals all listener threads
/// to stop and joins them when dropped.
pub struct PortForwardHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for PortForwardHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn handle_forward_connection(
    tcp_stream: TcpStream,
    vm: &VirtualMachine,
    guest_port: u16,
) -> Result<()> {
    let mut vsock_stream = vm
        .connect_to_vsock_port(VSOCK_PORT_FORWARD)
        .map_err(|e| anyhow::anyhow!("vsock connect for port forward: {}", e))?;
    let _ = vsock_stream.set_nodelay(true);

    // Send forward request
    let req = ForwardRequest { port: guest_port };
    frame::send_json(&mut vsock_stream, frame::FWD_REQ, &req)?;

    // Read response frame
    let (_msg_type, payload) = frame::read_frame(&mut vsock_stream)
        .context("reading forward response")?
        .context("guest closed connection during forward handshake")?;
    let resp: ForwardResponse =
        serde_json::from_slice(&payload).context("parsing forward response")?;

    if resp.status != "ok" {
        bail!(
            "guest refused forward: {}",
            resp.message.unwrap_or_default()
        );
    }

    // Bidirectional relay between TCP and vsock
    relay(tcp_stream, vsock_stream);
    Ok(())
}

fn relay(a: TcpStream, b: TcpStream) {
    let mut a_read = a.try_clone().expect("clone tcp stream");
    let mut b_write = b.try_clone().expect("clone vsock stream");
    let mut b_read = b;
    let mut a_write = a;

    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a_read, &mut b_write);
        let _ = b_write.shutdown(Shutdown::Write);
    });
    let t2 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut b_read, &mut a_write);
        let _ = a_write.shutdown(Shutdown::Write);
    });
    let _ = t1.join();
    let _ = t2.join();
}
