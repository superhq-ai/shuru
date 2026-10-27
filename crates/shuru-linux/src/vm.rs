use std::net::TcpStream;
use std::os::unix::io::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::configuration::VirtualMachineConfiguration;
use crate::error::{Result, VzError};
use crate::kvm::{self, layout};

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
    guest_cid: u64,
    state_tx: Sender<VmState>,
    state_rx: Receiver<VmState>,
    running: Arc<AtomicBool>,
}

impl VirtualMachine {
    pub fn new(config: &VirtualMachineConfiguration) -> Self {
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
            guest_cid: layout::GUEST_CID,
            mounts: inner.mounts.clone(),
            has_balloon: inner.has_balloon,
        };

        match kvm::KvmVm::create(create_config) {
            Ok(kvm_vm) => {
                let cid = kvm_vm.guest_cid;
                VirtualMachine {
                    vm: Mutex::new(Some(kvm_vm)),
                    guest_cid: cid,
                    state_tx,
                    state_rx,
                    running,
                }
            }
            Err(e) => {
                eprintln!("shuru: failed to create KVM VM: {}", e);
                VirtualMachine {
                    vm: Mutex::new(None),
                    guest_cid: layout::GUEST_CID,
                    state_tx,
                    state_rx,
                    running,
                }
            }
        }
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

    /// Connect to a vsock port on the guest via AF_VSOCK.
    pub fn connect_to_vsock_port(&self, port: u32) -> Result<TcpStream> {
        let sock = unsafe { libc::socket(layout::AF_VSOCK, libc::SOCK_STREAM, 0) };
        if sock < 0 {
            return Err(VzError::new(format!(
                "failed to create vsock socket: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Use non-blocking connect + poll to implement a 2-second timeout.
        // SO_SNDTIMEO doesn't work for AF_VSOCK on some kernels.
        unsafe {
            let flags = libc::fcntl(sock, libc::F_GETFL);
            libc::fcntl(sock, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let addr = libc::sockaddr_vm {
            svm_family: layout::AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: self.guest_cid as u32,
            svm_zero: [0u8; 4],
        };

        let ret = unsafe {
            libc::connect(
                sock,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };

        if ret < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() != Some(libc::EINPROGRESS) {
                unsafe { libc::close(sock) };
                return Err(VzError::new(format!("vsock connect failed: {}", errno)));
            }

            // Wait for connect to complete with 2s timeout
            let mut pfd = libc::pollfd {
                fd: sock,
                events: libc::POLLOUT,
                revents: 0,
            };
            let poll_ret = unsafe { libc::poll(&mut pfd, 1, 2000) };
            if poll_ret <= 0 {
                unsafe { libc::close(sock) };
                return Err(VzError::new("vsock connect timed out"));
            }

            // Check if connect succeeded
            let mut err: libc::c_int = 0;
            let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            unsafe {
                libc::getsockopt(
                    sock,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut err as *mut _ as *mut libc::c_void,
                    &mut len,
                );
            }
            if err != 0 {
                unsafe { libc::close(sock) };
                return Err(VzError::new(format!(
                    "vsock connect failed: {}",
                    std::io::Error::from_raw_os_error(err)
                )));
            }
        }

        // Restore blocking mode for normal I/O
        unsafe {
            let flags = libc::fcntl(sock, libc::F_GETFL);
            libc::fcntl(sock, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }

        Ok(unsafe { TcpStream::from_raw_fd(sock) })
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
