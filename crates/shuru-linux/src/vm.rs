use std::fs::{File, OpenOptions};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::configuration::VirtualMachineConfiguration;
use crate::error::{Result, VzError};
use crate::kvm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Stopped = 0,
    Running = 1,
    Error = 3,
    // Paused/Starting/etc. kept as discriminants for Darwin compatibility
    // but not constructed on Linux until pause/resume is implemented.
    Unknown = -1,
}

/// What a VM starts from.
pub enum VmImage {
    Kernel {
        kernel: File,
        initrd: Option<File>,
        command_line: String,
    },
    /// The memory and state files `VirtualMachine::snapshot` wrote. The
    /// devices must be the ones the snapshot was taken with.
    Snapshot { memory: File, state: File },
}

/// Everything a VM needs, already open, for a process that may not open
/// files itself. There is no virtio-fs: it needs host paths.
pub struct VmDescriptors {
    pub kvm: OwnedFd,
    pub image: VmImage,
    pub disk: Option<File>,
    pub disk_read_only: bool,
    pub serial_write: Option<OwnedFd>,
    pub serial_read: Option<OwnedFd>,
    pub network: Option<OwnedFd>,
    pub network_mac: Option<[u8; 6]>,
    pub cpu_count: usize,
    pub memory_bytes: u64,
    /// The vsock connection cap, or no vsock device.
    pub vsock: Option<usize>,
    pub balloon: bool,
}

pub struct VirtualMachine {
    vm: Mutex<Option<kvm::KvmVm>>,
    state_tx: Sender<VmState>,
    state_rx: Receiver<VmState>,
    running: Arc<AtomicBool>,
    // Descriptors the devices use by number.
    _held: Vec<OwnedFd>,
}

impl VirtualMachine {
    pub fn new(config: &VirtualMachineConfiguration) -> Result<Self> {
        let inner = config.inner.borrow();
        let open = |path: &str, what: &str| {
            File::open(path).map_err(|e| VzError::new(format!("failed to open {}: {}", what, e)))
        };
        let kernel = open(&inner.kernel_path, "kernel")?;
        let initrd = inner
            .initrd_path
            .as_deref()
            .map(|p| open(p, "initrd"))
            .transpose()?;
        let disk = inner
            .disk_path
            .as_deref()
            .map(|p| {
                OpenOptions::new()
                    .read(true)
                    .write(!inner.disk_read_only)
                    .open(p)
                    .map_err(|e| VzError::new(format!("failed to open disk: {}", e)))
            })
            .transpose()?;

        let create_config = kvm::VmCreateConfig {
            kvm: None,
            cpu_count: inner.cpu_count,
            memory_bytes: inner.memory_size,
            boot: kvm::Boot::Kernel {
                kernel,
                initrd,
                command_line: inner.command_line.clone(),
            },
            serial_write_fd: inner.serial_write_fd,
            serial_read_fd: inner.serial_read_fd,
            disk,
            disk_read_only: inner.disk_read_only,
            network_fd: inner.network_fd,
            network_mac: inner.network_mac,
            has_vsock: inner.has_socket,
            vsock_connections: kvm::vsock::MAX_CONNECTIONS,
            mounts: inner.mounts.clone(),
            has_balloon: inner.has_balloon,
        };
        Self::create(create_config, Vec::new())
    }

    pub fn from_descriptors(descriptors: VmDescriptors) -> Result<Self> {
        let raw = |fd: &Option<OwnedFd>| fd.as_ref().map(|fd| fd.as_raw_fd());
        let boot = match descriptors.image {
            VmImage::Kernel {
                kernel,
                initrd,
                command_line,
            } => kvm::Boot::Kernel {
                kernel,
                initrd,
                command_line,
            },
            VmImage::Snapshot { memory, state } => kvm::Boot::Snapshot {
                memory,
                state: serde_json::from_reader(std::io::BufReader::new(state))
                    .map_err(|e| VzError::new(format!("bad snapshot state: {e}")))?,
            },
        };
        let create_config = kvm::VmCreateConfig {
            kvm: Some(descriptors.kvm),
            cpu_count: descriptors.cpu_count,
            memory_bytes: descriptors.memory_bytes,
            boot,
            serial_write_fd: raw(&descriptors.serial_write),
            serial_read_fd: raw(&descriptors.serial_read),
            disk: descriptors.disk,
            disk_read_only: descriptors.disk_read_only,
            network_fd: raw(&descriptors.network),
            network_mac: descriptors.network_mac,
            has_vsock: descriptors.vsock.is_some(),
            vsock_connections: descriptors.vsock.unwrap_or(0),
            mounts: Vec::new(),
            has_balloon: descriptors.balloon,
        };
        let held = [
            descriptors.serial_write,
            descriptors.serial_read,
            descriptors.network,
        ]
        .into_iter()
        .flatten()
        .collect();
        Self::create(create_config, held)
    }

    fn create(create_config: kvm::VmCreateConfig, held: Vec<OwnedFd>) -> Result<Self> {
        let (state_tx, state_rx) = bounded(1);
        let kvm_vm = kvm::KvmVm::create(create_config)?;
        Ok(VirtualMachine {
            vm: Mutex::new(Some(kvm_vm)),
            state_tx,
            state_rx,
            running: Arc::new(AtomicBool::new(false)),
            _held: held,
        })
    }

    pub fn supported() -> bool {
        // Check if /dev/kvm is accessible
        std::path::Path::new("/dev/kvm").exists()
    }

    pub fn start(&self) -> Result<()> {
        let mut vm = self.vm.lock().unwrap();
        let kvm_vm = vm
            .as_mut()
            .ok_or_else(|| VzError::new("VM was not created successfully"))?;

        kvm_vm
            .start()
            .map_err(|e| VzError::new(format!("failed to start VM: {}", e)))?;

        self.running.store(true, Ordering::Release);

        // Notify state change
        let _ = self.state_tx.try_send(VmState::Running);

        // Spawn a monitor thread that watches the running flag
        let running = kvm_vm.running.clone();
        let state_tx = self.state_tx.clone();
        let vm_running = self.running.clone();
        std::thread::Builder::new()
            .name("shuru-vm-monitor".into())
            .spawn(move || {
                // Poll until vCPUs stop
                while running.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                vm_running.store(false, Ordering::Release);
                let _ = state_tx.try_send(VmState::Stopped);
            })
            .ok();

        Ok(())
    }

    /// Pauses the VM and writes it out: the guest memory into `memory` and
    /// the rest into `state`. With `resume` the guest runs on, otherwise it
    /// stays paused until stopped. See `KvmVm::snapshot` for `memory` and
    /// `pagemap`.
    pub fn snapshot(
        &self,
        memory: &File,
        state: &File,
        pagemap: &File,
        resume: bool,
    ) -> Result<()> {
        let mut vm = self.vm.lock().unwrap();
        let kvm_vm = vm
            .as_mut()
            .ok_or_else(|| VzError::new("VM was not created successfully"))?;
        let saved = kvm_vm.snapshot(memory, pagemap)?;
        let written = serde_json::to_writer(std::io::BufWriter::new(state), &saved)
            .map_err(|e| VzError::new(format!("failed to write the snapshot state: {e}")));
        if resume || written.is_err() {
            kvm_vm.resume()?;
        }
        written
    }

    pub fn stop(&self) -> Result<()> {
        let mut vm = self.vm.lock().unwrap();
        if let Some(kvm_vm) = vm.as_mut() {
            kvm_vm.stop();
        }
        self.running.store(false, Ordering::Release);
        let _ = self.state_tx.try_send(VmState::Stopped);
        Ok(())
    }

    /// How much memory the guest should keep. The balloon driver in the
    /// guest hands the rest to the host, and gets it back when the target
    /// rises again. Fails without a balloon device.
    pub fn set_memory_target(&self, bytes: u64) -> Result<()> {
        let vm = self.vm.lock().unwrap();
        let control = vm
            .as_ref()
            .and_then(|v| v.balloon.clone())
            .ok_or_else(|| VzError::new("no memory balloon device"))?;
        control.set_memory_target(bytes);
        Ok(())
    }

    /// Bytes the guest has handed over in response to the memory target.
    pub fn ballooned_bytes(&self) -> u64 {
        self.vm
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|v| v.balloon.as_ref().map(|b| b.ballooned_bytes()))
            .unwrap_or(0)
    }

    pub fn state_channel(&self) -> Receiver<VmState> {
        self.state_rx.clone()
    }

    pub fn can_start(&self) -> bool {
        self.vm.lock().unwrap().is_some()
    }

    pub fn can_stop(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn can_pause(&self) -> bool {
        false
    }
    pub fn can_resume(&self) -> bool {
        false
    }

    pub fn can_request_stop(&self) -> bool {
        self.can_stop()
    }

    fn vsock(&self) -> Result<Arc<kvm::vsock::VsockControl>> {
        self.vm
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|vm| vm.vsock.clone())
            .ok_or_else(|| VzError::new("no vsock device"))
    }

    pub fn connect_to_vsock_port(&self, port: u32) -> Result<TcpStream> {
        self.vsock()?
            .connect(port)
            .map_err(|e| VzError::new(format!("vsock connect failed: {e}")))
    }

    /// Accepts host connects on `listener` for as long as the VM lives. A
    /// client writes `CONNECT <guest port>\n` and reads `OK <host port>\n`
    /// once the guest accepts; a refused connect closes the socket.
    pub fn serve_vsock(&self, listener: UnixListener) -> Result<()> {
        self.vsock()?
            .serve(listener)
            .map_err(|e| VzError::new(format!("vsock listener failed: {e}")))
    }

    pub fn state(&self) -> VmState {
        if self.vm.lock().unwrap().is_none() {
            return VmState::Error;
        }
        if self.running.load(Ordering::Acquire) {
            VmState::Running
        } else {
            VmState::Stopped
        }
    }
}
