pub mod arch;
pub mod balloon;
pub mod devices;
pub mod snapshot;
pub mod virtio_fs;
mod vmgenid;
pub mod vsock;

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::os::unix::thread::JoinHandleExt;

use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap};

use self::arch::Platform;
use self::devices::*;
use self::snapshot::SnapshotState;
use crate::error::VzError;

// ===========================================================================
// VM creation
// ===========================================================================

pub struct KvmVm {
    #[allow(dead_code)]
    pub kvm: Kvm,
    #[allow(dead_code)]
    pub vm_fd: Arc<VmFd>,
    pub mem: GuestMemoryMmap,
    memory_bytes: u64,
    pub bus: Arc<MmioBus>,
    /// I/O ports, on x86 only.
    ports: Arc<MmioBus>,
    pub vcpus: Vec<VcpuFd>,
    pub vcpu_threads: Vec<std::thread::JoinHandle<VcpuFd>>,
    pub running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    pub vsock: Option<Arc<vsock::VsockControl>>,
    pub balloon: Option<Arc<balloon::BalloonControl>>,
    platform: Platform,
}

/// Where the guest comes from.
pub enum Boot {
    Kernel {
        kernel: File,
        initrd: Option<File>,
        command_line: String,
    },
    /// A snapshot taken with `KvmVm::snapshot`, its memory in `memory`.
    Snapshot { memory: File, state: SnapshotState },
}

pub struct VmCreateConfig {
    /// An open /dev/kvm; opened here when absent.
    pub kvm: Option<std::os::fd::OwnedFd>,
    pub cpu_count: usize,
    pub memory_bytes: u64,
    pub boot: Boot,
    pub serial_write_fd: Option<i32>,
    pub serial_read_fd: Option<i32>,
    pub disk: Option<File>,
    pub disk_read_only: bool,
    pub network_fd: Option<i32>,
    pub network_mac: Option<[u8; 6]>,
    pub has_vsock: bool,
    pub vsock_connections: usize,
    /// Virtio-fs mounts: (tag, host_path, read_only).
    pub mounts: Vec<(String, String, bool)>,
    pub has_balloon: bool,
}

impl KvmVm {
    pub fn create(mut config: VmCreateConfig) -> Result<Self, VzError> {
        let kvm = match config.kvm.take() {
            // SAFETY: the descriptor is an open /dev/kvm handed over to us.
            Some(fd) => unsafe {
                use std::os::fd::{FromRawFd, IntoRawFd};
                Kvm::from_raw_fd(fd.into_raw_fd())
            },
            None => {
                Kvm::new().map_err(|e| VzError::new(format!("failed to open /dev/kvm: {}", e)))?
            }
        };
        let vm_fd = kvm
            .create_vm()
            .map_err(|e| VzError::new(format!("failed to create VM: {}", e)))?;
        let vm_fd = Arc::new(vm_fd);

        arch::prepare(&vm_fd)?;

        let ranges = arch::memory_ranges(config.memory_bytes);
        let mem = match &config.boot {
            Boot::Kernel { .. } => GuestMemoryMmap::<()>::from_ranges(
                &ranges
                    .iter()
                    .map(|&(start, len)| (GuestAddress(start), len as usize))
                    .collect::<Vec<_>>(),
            )
            .map_err(io::Error::other),
            Boot::Snapshot { memory, .. } => memory
                .try_clone()
                .and_then(|file| snapshot::private_memory(file, &ranges)),
        }
        .map_err(|e| VzError::new(format!("failed to create guest memory: {}", e)))?;

        for (slot, &(start, len)) in ranges.iter().enumerate() {
            let host_addr = mem
                .get_host_address(GuestAddress(start))
                .map_err(|e| VzError::new(format!("failed to get host address: {}", e)))?;
            let mem_region = kvm_bindings::kvm_userspace_memory_region {
                slot: slot as u32,
                guest_phys_addr: start,
                memory_size: len,
                userspace_addr: host_addr as u64,
                flags: 0,
            };
            unsafe {
                vm_fd
                    .set_user_memory_region(mem_region)
                    .map_err(|e| VzError::new(format!("KVM_SET_USER_MEMORY_REGION: {}", e)))?;
            }
        }

        let mut bus = MmioBus::new();
        let mut ports = MmioBus::new();
        let mut virtio_idx: u32 = 0;
        arch::add_devices(
            &vm_fd,
            &mut bus,
            &mut ports,
            config.serial_write_fd,
            config.serial_read_fd,
        );

        // Virtio block
        if let Some(disk) = config.disk.take() {
            let blk = BlockBackend::new(disk, config.disk_read_only)
                .map_err(|e| VzError::new(format!("failed to open disk: {}", e)))?;
            add_virtio(&mut bus, &mut virtio_idx, Box::new(blk), &vm_fd, &mem)?;
        }

        let mut vsock_control = None;
        if config.has_vsock {
            let backend = vsock::VsockBackend::new(config.vsock_connections)
                .map_err(|e| VzError::new(format!("failed to create vsock device: {e}")))?;
            vsock_control = Some(backend.control());
            add_virtio(&mut bus, &mut virtio_idx, Box::new(backend), &vm_fd, &mem)?;
        }

        // Virtio-fs mounts (one device per mount)
        for (tag, host_path, read_only) in &config.mounts {
            let fs = virtio_fs::VirtioFsBackend::new(tag, host_path, *read_only).map_err(|e| {
                VzError::new(format!(
                    "failed to set up virtio-fs for tag '{}': {}",
                    tag, e
                ))
            })?;
            add_virtio(&mut bus, &mut virtio_idx, Box::new(fs), &vm_fd, &mem)?;
        }

        // Virtio net
        if let Some(net_fd) = config.network_fd {
            let mac = config
                .network_mac
                .unwrap_or([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
            add_virtio(
                &mut bus,
                &mut virtio_idx,
                Box::new(NetBackend::new(net_fd, mac)),
                &vm_fd,
                &mem,
            )?;
        }

        // Virtio balloon
        let mut balloon_control = None;
        if config.has_balloon {
            let backend = balloon::BalloonBackend::new(config.memory_bytes);
            balloon_control = Some(backend.control());
            add_virtio(&mut bus, &mut virtio_idx, Box::new(backend), &vm_fd, &mem)?;
        }

        // The arm64 GIC needs the vCPUs to exist first
        let mut vcpus = Vec::with_capacity(config.cpu_count);
        for i in 0..config.cpu_count {
            vcpus.push(
                vm_fd
                    .create_vcpu(i as u64)
                    .map_err(|e| VzError::new(format!("KVM_CREATE_VCPU({}): {}", i, e)))?,
            );
        }

        let platform = Platform::new(&kvm, &vm_fd, config.cpu_count)?;

        match config.boot {
            Boot::Kernel {
                mut kernel,
                initrd,
                command_line,
            } => arch::boot(
                &kvm,
                &vm_fd,
                &mem,
                &vcpus,
                &mut kernel,
                initrd,
                &command_line,
                config.memory_bytes,
                virtio_idx,
            )?,
            Boot::Snapshot { state, .. } => {
                restore(
                    &vm_fd,
                    &vcpus,
                    &platform,
                    &bus,
                    &ports,
                    &state,
                    config.memory_bytes,
                )
                .map_err(|e| VzError::new(format!("failed to restore the snapshot: {e}")))?;
            }
        }

        Ok(KvmVm {
            kvm,
            vm_fd,
            mem,
            memory_bytes: config.memory_bytes,
            bus: Arc::new(bus),
            ports: Arc::new(ports),
            vcpus,
            vcpu_threads: Vec::new(),
            running: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            vsock: vsock_control,
            balloon: balloon_control,
            platform,
        })
    }

    pub fn start(&mut self) -> Result<(), VzError> {
        self.running.store(true, Ordering::Release);

        let vcpus = std::mem::take(&mut self.vcpus);
        for vcpu in vcpus {
            let bus = self.bus.clone();
            let ports = self.ports.clone();
            let running = self.running.clone();
            let paused = self.paused.clone();

            self.vcpu_threads.push(
                std::thread::Builder::new()
                    .name("shuru-vcpu".into())
                    .spawn(move || vcpu_run_loop(vcpu, bus, ports, running, paused))
                    .map_err(|e| VzError::new(format!("failed to spawn vCPU: {}", e)))?,
            );
        }
        Ok(())
    }

    /// Takes every vCPU out of the guest and back from its thread.
    fn pause(&mut self) -> Result<(), VzError> {
        self.paused.store(true, Ordering::Release);
        for handle in std::mem::take(&mut self.vcpu_threads) {
            // A signal that lands before the thread enters KVM_RUN does
            // not stop it, so keep sending until it returns.
            while !handle.is_finished() {
                unsafe {
                    libc::pthread_kill(handle.as_pthread_t() as libc::pthread_t, libc::SIGRTMIN());
                }
                std::thread::sleep(Duration::from_micros(500));
            }
            let vcpu = handle
                .join()
                .map_err(|_| VzError::new("a vCPU thread panicked"))?;
            self.vcpus.push(vcpu);
        }
        if !self.running.load(Ordering::Acquire) {
            return Err(VzError::new("the guest stopped"));
        }
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), VzError> {
        for entry in &self.bus.devices {
            entry.device.resume();
        }
        self.paused.store(false, Ordering::Release);
        self.start()
    }

    /// Pauses the VM and saves it: the guest memory into `memory_out` and
    /// the rest into the returned state. The VM stays paused. See
    /// `snapshot::save_memory` for `memory_out` and `pagemap`.
    pub fn snapshot(
        &mut self,
        memory_out: &File,
        pagemap: &File,
    ) -> Result<SnapshotState, VzError> {
        self.pause()?;
        self.save(memory_out, pagemap).map_err(|e| {
            let _ = self.resume();
            VzError::new(format!("snapshot failed: {e}"))
        })
    }

    fn save(&self, memory_out: &File, pagemap: &File) -> io::Result<SnapshotState> {
        let saved = |bus: &MmioBus| {
            bus.devices
                .iter()
                .map(|entry| entry.device.save())
                .collect::<io::Result<Vec<_>>>()
        };
        let devices = saved(&self.bus)?;
        let ports = saved(&self.ports)?;
        let (vcpus, irqchip) = self.platform.save(&self.vm_fd, &self.vcpus)?;
        snapshot::save_memory(
            pagemap,
            &self.mem,
            &arch::memory_ranges(self.memory_bytes),
            memory_out,
        )?;
        Ok(SnapshotState {
            version: snapshot::FORMAT_VERSION,
            cpu_count: vcpus.len(),
            memory_bytes: self.memory_bytes,
            vcpus,
            irqchip,
            devices,
            ports,
        })
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        // Send a signal to unblock KVM_RUN on each vCPU thread
        for handle in &self.vcpu_threads {
            unsafe {
                libc::pthread_kill(handle.as_pthread_t() as libc::pthread_t, libc::SIGRTMIN());
            }
        }
        for handle in self.vcpu_threads.drain(..) {
            let _ = handle.join();
        }
        if let Some(vsock) = &self.vsock {
            vsock.deactivate();
        }
    }
}

impl Drop for KvmVm {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Puts a snapshot into a VM built with the same devices: vCPUs and the
/// interrupt controller, then the devices, whose interrupt lines need it.
fn restore(
    vm_fd: &VmFd,
    vcpus: &[VcpuFd],
    platform: &Platform,
    bus: &MmioBus,
    ports: &MmioBus,
    state: &SnapshotState,
    memory_bytes: u64,
) -> io::Result<()> {
    if state.version != snapshot::FORMAT_VERSION {
        return Err(io::Error::other(format!(
            "snapshot format {} is not {}",
            state.version,
            snapshot::FORMAT_VERSION
        )));
    }
    if state.cpu_count != vcpus.len()
        || state.vcpus.len() != vcpus.len()
        || state.memory_bytes != memory_bytes
        || state.devices.len() != bus.devices.len()
        || state.ports.len() != ports.devices.len()
    {
        return Err(snapshot::mismatch());
    }
    platform.restore(vm_fd, vcpus, &state.vcpus, &state.irqchip)?;
    let devices = bus.devices.iter().chain(&ports.devices);
    for (entry, saved) in devices.zip(state.devices.iter().chain(&state.ports)) {
        entry.device.restore(saved)?;
    }
    platform.restored(vm_fd)
}

// ===========================================================================
// vCPU run loop
// ===========================================================================

fn add_virtio(
    bus: &mut MmioBus,
    virtio_idx: &mut u32,
    backend: Box<dyn VirtioBackend>,
    vm_fd: &Arc<VmFd>,
    mem: &GuestMemoryMmap,
) -> Result<(), VzError> {
    let (base, size, irq) = arch::virtio_slot(*virtio_idx);
    let device = VirtioMmioDevice::new(backend, base, irq, vm_fd.clone(), mem.clone())
        .map_err(|e| VzError::new(format!("failed to set up virtio device: {e}")))?;
    bus.add(base, size, Box::new(device));
    *virtio_idx += 1;
    Ok(())
}

/// Runs until the guest stops or the VM pauses, and hands the vCPU back.
fn vcpu_run_loop(
    mut vcpu: VcpuFd,
    bus: Arc<MmioBus>,
    ports: Arc<MmioBus>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> VcpuFd {
    // Install empty handler for SIGRTMIN so KVM_RUN returns EINTR on stop
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = empty_signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGRTMIN(), &sa, std::ptr::null_mut());
    }

    while running.load(Ordering::Acquire) {
        // With immediate_exit set, KVM_RUN still finishes the MMIO read the
        // guest waits on before it returns, so the saved state is whole.
        if paused.load(Ordering::Acquire) {
            vcpu.set_kvm_immediate_exit(1);
        }
        match vcpu.run() {
            Ok(exit_reason) => {
                use kvm_ioctls::VcpuExit;
                match exit_reason {
                    VcpuExit::MmioRead(addr, data) => {
                        // data is &mut [u8] — write device response directly
                        bus.handle_read(addr, data);
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        bus.handle_write(addr, data);
                    }
                    VcpuExit::IoIn(port, data) => ports.handle_read(port as u64, data),
                    VcpuExit::IoOut(port, data) => {
                        if arch::is_reset(port, data) {
                            running.store(false, Ordering::Release);
                            break;
                        }
                        ports.handle_write(port as u64, data);
                    }
                    VcpuExit::SystemEvent(..) | VcpuExit::Shutdown => {
                        running.store(false, Ordering::Release);
                        break;
                    }
                    VcpuExit::Hlt => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    VcpuExit::InternalError => {
                        eprintln!("shuru: KVM internal error");
                        running.store(false, Ordering::Release);
                        break;
                    }
                    _ => {} // other exits — continue
                }
            }
            Err(e) => {
                if e.errno() != libc::EAGAIN && e.errno() != libc::EINTR {
                    eprintln!("shuru: KVM_RUN error: {}", e);
                    running.store(false, Ordering::Release);
                    break;
                }
                // EINTR from our signal: stopping, pausing, or neither.
                if e.errno() == libc::EINTR && paused.load(Ordering::Acquire) {
                    vcpu.set_kvm_immediate_exit(0);
                    break;
                }
            }
        }
    }
    vcpu
}

extern "C" fn empty_signal_handler(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // Empty — just used to interrupt KVM_RUN
}
