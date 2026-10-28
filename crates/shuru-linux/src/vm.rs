use std::net::TcpStream;
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

pub struct VirtualMachine {
    vm: Mutex<Option<kvm::KvmVm>>,
    state_tx: Sender<VmState>,
    state_rx: Receiver<VmState>,
    running: Arc<AtomicBool>,
}

impl VirtualMachine {
    pub fn new(config: &VirtualMachineConfiguration) -> Result<Self> {
        let inner = config.inner.borrow();
        let (state_tx, state_rx) = bounded(1);
        let running = Arc::new(AtomicBool::new(false));

        let create_config = kvm::VmCreateConfig {
            cpu_count: inner.cpu_count,
            memory_bytes: inner.memory_size,
            kernel_path: inner.kernel_path.clone(),
            initrd_path: inner.initrd_path.clone(),
            command_line: inner.command_line.clone(),
            serial_write_fd: inner.serial_write_fd,
            serial_read_fd: inner.serial_read_fd,
            disk_path: inner.disk_path.clone(),
            disk_read_only: inner.disk_read_only,
            network_fd: inner.network_fd,
            network_mac: inner.network_mac,
            has_vsock: inner.has_socket,
            mounts: inner.mounts.clone(),
            has_balloon: inner.has_balloon,
        };

        let kvm_vm = kvm::KvmVm::create(create_config)?;
        Ok(VirtualMachine {
            vm: Mutex::new(Some(kvm_vm)),
            state_tx,
            state_rx,
            running,
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

    pub fn connect_to_vsock_port(&self, port: u32) -> Result<TcpStream> {
        let control = self
            .vm
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|vm| vm.vsock.clone())
            .ok_or_else(|| VzError::new("no vsock device"))?;
        control
            .connect(port)
            .map_err(|e| VzError::new(format!("vsock connect failed: {e}")))
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
