use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

// Re-exports
pub use shuru_proto::{DirEntry, ReadDirResponse, StatResponse, WatchEvent};
pub use shuru_proxy::config::{ExposeHostMapping, NetworkConfig, ProxyConfig, SecretConfig};
pub use shuru_vm::{default_data_dir, MountConfig};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A raw bidirectional byte stream to a TCP port inside the guest,
/// established via [`AsyncSandbox::dial_guest`].
///
/// Implements blocking [`Read`]/[`Write`] over the host-side (vsock-backed)
/// connection. For async use, take the inner stream with
/// [`GuestStream::into_inner`], set it non-blocking, and wrap it with
/// `tokio::net::TcpStream::from_std`.
pub struct GuestStream(TcpStream);

impl GuestStream {
    /// Consume the wrapper and return the underlying host-side stream.
    pub fn into_inner(self) -> TcpStream {
        self.0
    }

    /// Clone the stream. Both handles reference the same guest connection.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(GuestStream)
    }
}

impl Read for GuestStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for GuestStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Storage backend for the VM's root disk.
#[derive(Debug, Clone, Default)]
pub enum StorageMode {
    /// Direct disk image attachment (CoW clone on macOS, copy on Linux).
    #[default]
    Direct,
    /// Content-addressable chunk store via NBD. Requires the `cas` feature.
    #[cfg(feature = "cas")]
    Cas {
        /// Directory for the CAS chunk store. Defaults to `<data_dir>/cas`.
        cas_dir: Option<String>,
    },
}

/// Configuration for booting a sandbox VM.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Data directory containing kernel, rootfs, initramfs.
    /// Defaults to `~/.local/share/shuru`.
    pub data_dir: Option<String>,
    /// Number of CPUs. Default: 2.
    pub cpus: usize,
    /// Memory in MB. Default: 2048.
    pub memory_mb: u64,
    /// Disk size in MB. Default: 4096.
    pub disk_size_mb: u64,
    /// Host → guest directory mounts (VirtioFS).
    pub mounts: Vec<MountConfig>,
    /// Enable networking via proxy.
    pub allow_net: bool,
    /// Secrets for proxy injection.
    pub secrets: HashMap<String, SecretConfig>,
    /// Allowed domain patterns for network access.
    pub allowed_hosts: Vec<String>,
    /// Port forwards (host → guest).
    pub ports: Vec<shuru_proto::PortMapping>,
    /// Host ports exposed to the guest via host.shuru.internal.
    pub expose_host: Vec<ExposeHostMapping>,
    /// Boot from a named checkpoint instead of base rootfs.
    pub from: Option<String>,
    /// Storage backend mode. Default: Direct (flat file with CoW).
    pub storage: StorageMode,
    /// Default environment variables for all commands.
    /// Merged with proxy placeholders; caller extra_env overrides both.
    pub env: HashMap<String, String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            cpus: 2,
            memory_mb: 2048,
            disk_size_mb: 4096,
            mounts: vec![],
            allow_net: false,
            secrets: HashMap::new(),
            allowed_hosts: vec![],
            ports: vec![],
            expose_host: vec![],
            from: None,
            storage: StorageMode::default(),
            env: HashMap::new(),
        }
    }
}

/// Result of executing a command in the VM.
#[derive(Debug)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Events from an interactive shell session.
#[derive(Debug)]
pub enum ShellEvent {
    /// Terminal output bytes (PTY stdout).
    Output(Vec<u8>),
    /// Process exited with code.
    Exit(i32),
    /// Error from guest.
    Error(String),
}

/// Writer half of a shell session. Cloneable — used to send input and resize.
#[derive(Clone)]
pub struct ShellWriter {
    writer: Arc<std::sync::Mutex<TcpStream>>,
}

impl ShellWriter {
    /// Send input bytes (keystrokes) to the shell.
    pub fn send_input(&self, data: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut w = self.writer.lock().unwrap();
        shuru_proto::frame::write_frame(&mut *w, shuru_proto::frame::STDIN, data)?;
        w.flush()?;
        Ok(())
    }

    /// Send a terminal resize event.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        let payload = shuru_proto::frame::resize_payload(rows, cols);
        shuru_proto::frame::write_frame(&mut *w, shuru_proto::frame::RESIZE, &payload)?;
        Ok(())
    }
}

/// Reader half of a shell session. Receives output events asynchronously.
pub struct ShellReader {
    output_rx: mpsc::UnboundedReceiver<ShellEvent>,
}

impl ShellReader {
    /// Receive the next shell event. Returns `None` when the session ends.
    pub async fn recv(&mut self) -> Option<ShellEvent> {
        self.output_rx.recv().await
    }
}

/// Handle to an interactive shell session with PTY support.
pub struct ShellHandle {
    writer: ShellWriter,
    reader: ShellReader,
    _reader_thread: std::thread::JoinHandle<()>,
}

impl ShellHandle {
    /// Split into writer (cloneable, for input) and reader (for output).
    pub fn split(self) -> (ShellWriter, ShellReader) {
        // Leak the thread handle so it runs to completion
        std::mem::forget(self._reader_thread);
        (self.writer, self.reader)
    }
}

/// Reader half of a file watch session. Receives filesystem events asynchronously.
pub struct WatchReceiver {
    event_rx: mpsc::UnboundedReceiver<WatchEvent>,
}

impl WatchReceiver {
    /// Receive the next watch event. Returns `None` when the watch ends.
    pub async fn recv(&mut self) -> Option<WatchEvent> {
        self.event_rx.recv().await
    }

    /// Try to receive a buffered event without blocking.
    pub fn try_recv(&mut self) -> Result<WatchEvent, tokio::sync::mpsc::error::TryRecvError> {
        self.event_rx.try_recv()
    }
}

/// Handle to a filesystem watch session. Drop to stop watching.
pub struct WatchHandle {
    pub receiver: WatchReceiver,
    _reader_thread: std::thread::JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// Internal command protocol (async ↔ VM thread)
// ---------------------------------------------------------------------------

enum SandboxCmd {
    Exec {
        argv: Vec<String>,
        reply: oneshot::Sender<Result<ExecResult>>,
    },
    ReadFile {
        path: String,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    WriteFile {
        path: String,
        content: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    ReadDir {
        path: String,
        reply: oneshot::Sender<Result<ReadDirResponse>>,
    },
    Mkdir {
        path: String,
        recursive: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Rename {
        old_path: String,
        new_path: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Chmod {
        path: String,
        mode: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    Download {
        url: String,
        path: String,
        extract: bool,
        strip_components: u32,
        progress_tx: std::sync::mpsc::Sender<shuru_proto::DownloadProgress>,
        reply: oneshot::Sender<Result<()>>,
    },
    OpenShell {
        argv: Option<Vec<String>>,
        rows: u16,
        cols: u16,
        cwd: Option<String>,
        extra_env: HashMap<String, String>,
        reply: oneshot::Sender<Result<TcpStream>>,
    },
    Watch {
        path: String,
        recursive: bool,
        reply: oneshot::Sender<Result<TcpStream>>,
    },
    Remove {
        path: String,
        recursive: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    DiscardOverlay {
        path: String,
        reply: oneshot::Sender<Result<()>>,
    },
    AddPortForward {
        mapping: shuru_proto::PortMapping,
        reply: oneshot::Sender<Result<()>>,
    },
    DialGuest {
        port: u16,
        reply: oneshot::Sender<Result<TcpStream>>,
    },
    Checkpoint {
        name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Stop {
        reply: oneshot::Sender<Result<()>>,
    },
}

// ---------------------------------------------------------------------------
// AsyncSandbox — the main public interface
// ---------------------------------------------------------------------------

/// Async wrapper around a Shuru VM sandbox.
///
/// All VM operations are dispatched to a dedicated OS thread that owns
/// the sandbox. This avoids Send/Sync constraints from the Apple
/// Virtualization framework's Objective-C objects.
pub struct AsyncSandbox {
    cmd_tx: std::sync::mpsc::Sender<SandboxCmd>,
}

impl AsyncSandbox {
    /// Boot a new sandbox VM with the given configuration.
    pub async fn boot(config: SandboxConfig) -> Result<Self> {
        let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name("shuru-vm".into())
            .spawn(move || match boot_vm(config) {
                Ok(booted) => {
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    run_vm_loop(
                        booted.sandbox,
                        booted.work_disk,
                        &booted.data_dir,
                        cmd_rx,
                        booted.proxy_handle,
                        booted.fwd_handle,
                        #[cfg(feature = "cas")]
                        booted.nbd_handle,
                        booted.default_env,
                    );
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;

        ready_rx.await??;

        Ok(Self { cmd_tx })
    }

    /// Execute a command and wait for the result.
    pub async fn exec(&self, argv: &[&str]) -> Result<ExecResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Exec {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Execute a command string via the given shell (defaults to `/bin/sh`).
    pub async fn exec_shell(&self, command: &str) -> Result<ExecResult> {
        self.exec_in("/bin/sh", command).await
    }

    /// Execute a command string via a specific shell.
    ///
    /// Use `exec_in("bash", cmd)` when you need login profile (PATH etc.),
    /// or `exec_in("/bin/sh", cmd)` for basic POSIX shell.
    pub async fn exec_in(&self, shell: &str, command: &str) -> Result<ExecResult> {
        self.exec(&[shell, "-c", command]).await
    }

    /// Spawn an interactive PTY session.
    /// If `argv` is None, opens a login shell (`bash -l`).
    /// If `argv` is Some, runs that command directly (no shell wrapper).
    /// If `cwd` is Some, the process starts in that directory.
    pub async fn open_shell(
        &self,
        rows: u16,
        cols: u16,
        cwd: Option<&str>,
        argv: Option<&[&str]>,
        extra_env: HashMap<String, String>,
    ) -> Result<ShellHandle> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::OpenShell {
                argv: argv.map(|a| a.iter().map(|s| s.to_string()).collect()),
                rows,
                cols,
                cwd: cwd.map(|s| s.to_string()),
                extra_env,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        let stream = reply_rx.await??;

        // Split the stream for bidirectional I/O
        let writer_stream = stream.try_clone()?;
        let reader_stream = stream;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let reader_thread = std::thread::Builder::new()
            .name("shuru-shell-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader_stream);
                loop {
                    match shuru_proto::frame::read_frame(&mut reader) {
                        Ok(Some((shuru_proto::frame::STDOUT, payload))) => {
                            if event_tx.send(ShellEvent::Output(payload)).is_err() {
                                break;
                            }
                        }
                        Ok(Some((shuru_proto::frame::EXIT, payload))) => {
                            let code = shuru_proto::frame::parse_exit_code(&payload).unwrap_or(0);
                            let _ = event_tx.send(ShellEvent::Exit(code));
                            break;
                        }
                        Ok(Some((shuru_proto::frame::ERROR, payload))) => {
                            let msg = String::from_utf8_lossy(&payload).to_string();
                            let _ = event_tx.send(ShellEvent::Error(msg));
                            break;
                        }
                        Ok(Some(_)) => {} // skip unknown frame types
                        Ok(None) | Err(_) => break,
                    }
                }
            })?;

        Ok(ShellHandle {
            writer: ShellWriter {
                writer: Arc::new(std::sync::Mutex::new(writer_stream)),
            },
            reader: ShellReader {
                output_rx: event_rx,
            },
            _reader_thread: reader_thread,
        })
    }

    /// Read a file from the VM. Returns raw bytes.
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::ReadFile {
                path: path.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Write a file to the VM.
    pub async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::WriteFile {
                path: path.to_string(),
                content: content.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Discard overlay changes for a file. Removes it from the overlay upper dir,
    /// revealing the original host version from the lower layer.
    pub async fn discard_overlay(&self, path: &str) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::DiscardOverlay {
                path: path.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Add a port forward to a running sandbox.
    pub async fn add_port_forward(&self, host_port: u16, guest_port: u16) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::AddPortForward {
                mapping: shuru_proto::PortMapping {
                    host_port,
                    guest_port,
                },
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Open a raw byte stream to a TCP port listening inside the guest.
    ///
    /// This is the ingress counterpart to outbound proxying: it lets host-side
    /// code reach a server running in the sandbox without binding a local
    /// listener (unlike [`add_port_forward`](Self::add_port_forward)). The
    /// returned [`GuestStream`] is a blocking bidirectional pipe to
    /// `127.0.0.1:<port>` inside the guest, suitable for bridging the guest
    /// service onto any transport (for example, a tunnel to the public
    /// internet). It works whether or not `--allow-net` is enabled.
    ///
    /// For async I/O, take the inner stream with [`GuestStream::into_inner`],
    /// set it non-blocking, and wrap it with `tokio::net::TcpStream::from_std`.
    pub async fn dial_guest(&self, port: u16) -> Result<GuestStream> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::DialGuest {
                port,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        let stream = reply_rx.await??;
        Ok(GuestStream(stream))
    }

    /// Remove a file or directory inside the VM.
    pub async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Remove {
                path: path.to_string(),
                recursive,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// List directory contents in the VM.
    pub async fn read_dir(&self, path: &str) -> Result<ReadDirResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::ReadDir {
                path: path.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Create a directory inside the VM. If `recursive` is true, creates
    /// parent directories as needed (like `mkdir -p`).
    pub async fn mkdir(&self, path: &str, recursive: bool) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Mkdir {
                path: path.to_string(),
                recursive,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Rename a file or directory inside the VM.
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Rename {
                old_path: old_path.to_string(),
                new_path: new_path.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Change file permissions inside the VM.
    pub async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Chmod {
                path: path.to_string(),
                mode,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Download a URL into the sandbox. If `extract` is true, the download
    /// is treated as a .tar.gz and extracted to `path` as a directory.
    /// `strip_components` mirrors `tar --strip-components=N` — set to `1`
    /// for tarballs wrapped in a single top-level directory (Node, Pi)
    /// and `0` for flat tarballs (single binary at root).
    /// Returns a channel that receives progress updates.
    pub async fn download(
        &self,
        url: &str,
        path: &str,
        extract: bool,
        strip_components: u32,
    ) -> Result<(
        tokio::sync::oneshot::Receiver<Result<()>>,
        std::sync::mpsc::Receiver<shuru_proto::DownloadProgress>,
    )> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
        self.cmd_tx
            .send(SandboxCmd::Download {
                url: url.to_string(),
                path: path.to_string(),
                extract,
                strip_components,
                progress_tx,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        Ok((reply_rx, progress_rx))
    }

    /// Save the current rootfs state as a named checkpoint (CoW clone).
    /// Future VMs can boot from this checkpoint via `SandboxConfig::from`.
    pub async fn checkpoint(&self, name: &str) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Checkpoint {
                name: name.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        reply_rx.await?
    }

    /// Watch a path inside the VM for filesystem changes (inotify-backed).
    /// Returns a handle whose receiver emits `WatchEvent`s. Drop the handle
    /// to stop watching.
    pub async fn open_watch(&self, path: &str, recursive: bool) -> Result<WatchHandle> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(SandboxCmd::Watch {
                path: path.to_string(),
                recursive,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("VM thread exited"))?;
        let stream = reply_rx.await??;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let reader_thread = std::thread::Builder::new()
            .name("shuru-watch-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stream);
                loop {
                    match shuru_proto::frame::read_frame(&mut reader) {
                        Ok(Some((shuru_proto::frame::WATCH_EVENT, payload))) => {
                            if let Some(event) = WatchEvent::decode(&payload) {
                                if event_tx.send(event).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Some(_)) => {} // skip unknown frame types
                        Ok(None) | Err(_) => break,
                    }
                }
            })?;

        Ok(WatchHandle {
            receiver: WatchReceiver { event_rx },
            _reader_thread: reader_thread,
        })
    }

    /// Stop the VM and clean up resources.
    pub async fn stop(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.cmd_tx.send(SandboxCmd::Stop { reply: reply_tx });
        reply_rx.await.unwrap_or(Ok(()))
    }
}

impl Drop for AsyncSandbox {
    fn drop(&mut self) {
        // Signal the VM thread to stop
        let (reply_tx, _) = oneshot::channel();
        let _ = self.cmd_tx.send(SandboxCmd::Stop { reply: reply_tx });
    }
}

// ---------------------------------------------------------------------------
// Internal: VM boot & command loop (runs on dedicated OS thread)
// ---------------------------------------------------------------------------

struct BootedVm {
    sandbox: shuru_vm::Sandbox,
    work_disk: shuru_vm::WorkDisk,
    data_dir: String,
    proxy_handle: Option<shuru_proxy::ProxyHandle>,
    fwd_handle: Option<shuru_vm::PortForwardHandle>,
    #[cfg(feature = "cas")]
    nbd_handle: Option<shuru_store::NbdHandle>,
    default_env: HashMap<String, String>,
}

fn boot_vm(config: SandboxConfig) -> Result<BootedVm> {
    let default_env = config.env.clone();
    let data_dir = config.data_dir.unwrap_or_else(shuru_vm::default_data_dir);

    // Resolve asset paths
    let kernel_path = format!("{}/Image", data_dir);
    let rootfs_path = format!("{}/rootfs.ext4", data_dir);
    let initrd_path_str = format!("{}/initramfs.cpio.gz", data_dir);

    if !std::path::Path::new(&kernel_path).exists() {
        bail!(
            "Kernel not found at {}. Run `shuru init` to download.",
            kernel_path
        );
    }

    // Determine rootfs source (checkpoint or base)
    let checkpoints_dir = format!("{}/checkpoints", data_dir);
    #[allow(unused_mut, unused_variables)]
    let mut cas_index: Option<String> = None;

    let source = match &config.from {
        Some(name) => {
            shuru_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;

            // Check .idx (CAS) first, then .ext4 (legacy)
            #[cfg(feature = "cas")]
            {
                let idx_path = format!("{}/{}.idx", checkpoints_dir, name);
                let ext4_path = format!("{}/{}.ext4", checkpoints_dir, name);
                if std::path::Path::new(&idx_path).exists() {
                    cas_index = Some(idx_path.clone());
                    idx_path
                } else if std::path::Path::new(&ext4_path).exists() {
                    ext4_path
                } else {
                    bail!("Checkpoint '{}' not found", name);
                }
            }

            #[cfg(not(feature = "cas"))]
            {
                let path = format!("{}/{}.ext4", checkpoints_dir, name);
                if !std::path::Path::new(&path).exists() {
                    bail!("Checkpoint '{}' not found", name);
                }
                path
            }
        }
        None => {
            if !std::path::Path::new(&rootfs_path).exists() {
                bail!(
                    "Rootfs not found at {}. Run `shuru init` to download.",
                    rootfs_path
                );
            }
            rootfs_path
        }
    };

    // Determine whether to use NBD-backed CAS storage
    #[cfg(feature = "cas")]
    let use_nbd = cas_index.is_some() || matches!(config.storage, StorageMode::Cas { .. });
    #[cfg(not(feature = "cas"))]
    let use_nbd = false;

    // A CAS run is served over NBD, so its disk is only a placeholder.
    let work_disk = if use_nbd {
        shuru_vm::WorkDisk::empty(&data_dir)?
    } else {
        shuru_vm::WorkDisk::clone_from(&source, &data_dir, config.disk_size_mb)?
    };

    // Start CAS NBD server if needed
    #[cfg(feature = "cas")]
    let nbd_handle = if use_nbd {
        std::fs::create_dir_all(format!("{}/cas", data_dir))?;
        let socket_path = format!(
            "{}/cas/nbd-{}-{}.sock",
            data_dir,
            std::process::id(),
            work_disk.as_raw_fd()
        );
        let cas_dir_str = match &config.storage {
            StorageMode::Cas { cas_dir } => cas_dir
                .clone()
                .unwrap_or_else(|| format!("{}/cas", data_dir)),
            _ => format!("{}/cas", data_dir),
        };
        let index_path = if let Some(ref idx) = cas_index {
            idx.clone()
        } else {
            let source_hash = blake3::hash(source.as_bytes()).to_hex();
            format!("{}/cas/indexes/{}.idx", data_dir, &source_hash[..16])
        };
        let target_size = config.disk_size_mb * 1024 * 1024;
        Some(shuru_store::start_cas_nbd_server(
            &source,
            &cas_dir_str,
            &index_path,
            &socket_path,
            target_size,
        )?)
    } else {
        None
    };

    let initrd_path = if std::path::Path::new(&initrd_path_str).exists() {
        Some(initrd_path_str)
    } else {
        None
    };

    // Set up proxy networking if enabled
    let (vm_fd, proxy_handle) = if config.allow_net {
        let mut proxy_config = ProxyConfig::default();
        proxy_config.secrets = config.secrets;
        proxy_config.network.allow = config.allowed_hosts;
        proxy_config.expose_host = config.expose_host;

        let (vm_fd, host_fd) = shuru_proxy::create_socketpair()?;
        let handle = shuru_proxy::start(host_fd, proxy_config)?;
        (Some(vm_fd), Some(handle))
    } else {
        (None, None)
    };

    // Build the VM
    let mut builder = shuru_vm::Sandbox::builder()
        .kernel(&kernel_path)
        .rootfs(work_disk.attach_path())
        .cpus(config.cpus)
        .memory_mb(config.memory_mb)
        .console(false); // No serial console in SDK mode

    if let Some(fd) = vm_fd {
        builder = builder.network_fd(fd);
    }
    #[cfg(feature = "cas")]
    if let Some(ref handle) = nbd_handle {
        builder = builder.nbd_uri(handle.uri());
    }
    if let Some(ref initrd) = initrd_path {
        builder = builder.initrd(initrd);
    }
    for m in &config.mounts {
        builder = builder.mount(m.clone());
    }

    let sandbox = builder.build()?;

    info!(
        "booting VM ({}cpus, {}MB RAM, {}MB disk, storage={:?})",
        config.cpus, config.memory_mb, config.disk_size_mb, config.storage
    );

    sandbox.start()?;

    // Start port forwarding
    let fwd_handle = if !config.ports.is_empty() {
        Some(sandbox.start_port_forwarding(&config.ports)?)
    } else {
        None
    };

    // Inject CA cert and secret placeholders when proxy is active
    if let Some(ref handle) = proxy_handle {
        if !handle.placeholders.is_empty() {
            sandbox.write_file(
                "/usr/local/share/ca-certificates/shuru-proxy.crt",
                &handle.ca_cert_pem,
            )?;
            sandbox.exec(
                &["update-ca-certificates", "--fresh"],
                &mut std::io::sink(),
                &mut std::io::sink(),
            )?;
        }
    }

    info!("VM ready");

    Ok(BootedVm {
        sandbox,
        work_disk,
        data_dir,
        proxy_handle,
        fwd_handle,
        #[cfg(feature = "cas")]
        nbd_handle,
        default_env,
    })
}

fn run_vm_loop(
    sandbox: shuru_vm::Sandbox,
    work_disk: shuru_vm::WorkDisk,
    data_dir: &str,
    cmd_rx: std::sync::mpsc::Receiver<SandboxCmd>,
    proxy_handle: Option<shuru_proxy::ProxyHandle>,
    _fwd_handle: Option<shuru_vm::PortForwardHandle>,
    #[cfg(feature = "cas")] nbd_handle: Option<shuru_store::NbdHandle>,
    default_env: HashMap<String, String>,
) {
    // Base env: config defaults, then proxy placeholders on top
    let mut env = default_env;
    if let Some(ref handle) = proxy_handle {
        env.extend(handle.placeholders.clone());
    }

    // Keep proxy_handle alive for the lifetime of the VM
    let _proxy = proxy_handle;
    // Additional port forward handles added at runtime
    let mut extra_fwd_handles: Vec<shuru_vm::PortForwardHandle> = Vec::new();

    // Wrap in Arc so read-type commands (ReadFile/ReadDir) can fan out to
    // worker threads and run concurrently — each `sandbox.read_file` opens
    // its own vsock connection, so there's no contention to serialize.
    let sandbox = Arc::new(sandbox);

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            SandboxCmd::Exec { argv, reply } => {
                let result = exec_command(&sandbox, &argv, &env);
                let _ = reply.send(result);
            }
            // Reads open their own vsock connection per call and don't mutate
            // state, so fan them out onto worker threads — otherwise a burst
            // of UI stats requests serializes behind the dispatcher.
            SandboxCmd::ReadFile { path, reply } => {
                let sb = sandbox.clone();
                std::thread::spawn(move || {
                    let _ = reply.send(sb.read_file(&path));
                });
            }
            SandboxCmd::ReadDir { path, reply } => {
                let sb = sandbox.clone();
                std::thread::spawn(move || {
                    let _ = reply.send(sb.read_dir(&path));
                });
            }
            SandboxCmd::WriteFile {
                path,
                content,
                reply,
            } => {
                let _ = reply.send(sandbox.write_file(&path, &content));
            }
            SandboxCmd::Mkdir {
                path,
                recursive,
                reply,
            } => {
                let _ = reply.send(sandbox.mkdir(&path, recursive));
            }
            SandboxCmd::Rename {
                old_path,
                new_path,
                reply,
            } => {
                let _ = reply.send(sandbox.rename(&old_path, &new_path));
            }
            SandboxCmd::Chmod { path, mode, reply } => {
                let _ = reply.send(sandbox.chmod(&path, mode));
            }
            SandboxCmd::Download {
                url,
                path,
                extract,
                strip_components,
                progress_tx,
                reply,
            } => {
                let result = sandbox.download(&url, &path, extract, strip_components, |p| {
                    let _ = progress_tx.send(p);
                });
                let _ = reply.send(result);
            }
            SandboxCmd::OpenShell {
                argv,
                rows,
                cols,
                cwd,
                extra_env,
                reply,
            } => {
                let default_argv = ["/bin/bash".to_string(), "-l".to_string()];
                let shell_argv: Vec<String> = argv.unwrap_or_else(|| default_argv.to_vec());
                let shell_argv_refs: Vec<&str> = shell_argv.iter().map(|s| s.as_str()).collect();
                let mut merged_env = env.clone();
                merged_env.extend(extra_env);
                let result = sandbox.open_shell_with_cwd(
                    &shell_argv_refs,
                    &merged_env,
                    rows,
                    cols,
                    cwd.as_deref(),
                );
                let _ = reply.send(result);
            }
            SandboxCmd::Watch {
                path,
                recursive,
                reply,
            } => {
                let result = sandbox.open_watch(&path, recursive);
                let _ = reply.send(result);
            }
            SandboxCmd::Remove {
                path,
                recursive,
                reply,
            } => {
                let _ = reply.send(sandbox.remove(&path, recursive));
            }
            SandboxCmd::DiscardOverlay { path, reply } => {
                let _ = reply.send(sandbox.discard_overlay(&path));
            }
            SandboxCmd::AddPortForward { mapping, reply } => {
                let result = sandbox.start_port_forwarding(&[mapping]);
                match result {
                    Ok(handle) => {
                        extra_fwd_handles.push(handle);
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            SandboxCmd::DialGuest { port, reply } => {
                // Handshake blocks on the guest — run off the command loop.
                let sb = sandbox.clone();
                std::thread::spawn(move || {
                    let _ = reply.send(sb.connect_forward(port));
                });
            }
            SandboxCmd::Checkpoint { name, reply } => {
                let result = (|| -> Result<()> {
                    shuru_vm::validate_checkpoint_name(&name).map_err(|e| anyhow::anyhow!(e))?;
                    let checkpoints_dir = format!("{}/checkpoints", data_dir);
                    std::fs::create_dir_all(&checkpoints_dir)?;

                    // CAS path: save as .idx via NbdHandle
                    #[cfg(feature = "cas")]
                    if let Some(ref handle) = nbd_handle {
                        let index_path = format!("{}/{}.idx", checkpoints_dir, name);
                        if std::path::Path::new(&index_path).exists() {
                            std::fs::remove_file(&index_path)?;
                        }
                        handle.save_checkpoint(&index_path)?;
                        info!("checkpoint '{}' saved (CAS)", name);
                        return Ok(());
                    }

                    // Direct path: save as .ext4 via CoW clone
                    let checkpoint_path = format!("{}/{}.ext4", checkpoints_dir, name);
                    if std::path::Path::new(&checkpoint_path).exists() {
                        std::fs::remove_file(&checkpoint_path)?;
                    }
                    work_disk.save_to(&checkpoint_path)?;
                    info!("checkpoint '{}' saved", name);
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            SandboxCmd::Stop { reply } => {
                let _ = reply.send(sandbox.stop());
                break;
            }
        }
    }

    // Ensure cleanup
    let _ = sandbox.stop();
}

fn exec_command(
    sandbox: &shuru_vm::Sandbox,
    argv: &[String],
    env: &HashMap<String, String>,
) -> Result<ExecResult> {
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    let exit_code = sandbox.exec_with_env(&argv_refs, env, &mut stdout_buf, &mut stderr_buf)?;

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        exit_code,
    })
}
