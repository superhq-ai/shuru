pub mod balloon;
pub mod devices;
pub mod layout;
pub mod snapshot;
pub mod virtio_fs;
pub mod vmgenid;
pub mod vsock;

use std::fs::File;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::os::unix::thread::JoinHandleExt;

use kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3;
use kvm_ioctls::{DeviceFd, Kvm, VcpuFd, VmFd};
use linux_loader::loader::{pe::PE, KernelLoader};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use self::devices::*;
use self::layout::*;
use self::snapshot::SnapshotState;
use crate::error::VzError;

// Use the actual constants from kvm-bindings (not our hand-defined ones)
use kvm_bindings::{KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_DEV_ARM_VGIC_GRP_CTRL};

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
    pub vcpus: Vec<VcpuFd>,
    pub vcpu_threads: Vec<std::thread::JoinHandle<VcpuFd>>,
    pub running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    pub vsock: Option<Arc<vsock::VsockControl>>,
    pub balloon: Option<Arc<balloon::BalloonControl>>,
    gic: DeviceFd,
    _vmgenid: vmgenid::VmGenId,
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

        let mem = match &config.boot {
            Boot::Kernel { .. } => GuestMemoryMmap::<()>::from_ranges(&[(
                GuestAddress(DRAM_BASE),
                config.memory_bytes as usize,
            )])
            .map_err(io::Error::other),
            Boot::Snapshot { memory, .. } => memory
                .try_clone()
                .and_then(|file| snapshot::private_memory(file, config.memory_bytes)),
        }
        .map_err(|e| VzError::new(format!("failed to create guest memory: {}", e)))?;

        let host_addr = mem
            .get_host_address(GuestAddress(DRAM_BASE))
            .map_err(|e| VzError::new(format!("failed to get host address: {}", e)))?;

        let mem_region = kvm_bindings::kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: DRAM_BASE,
            memory_size: config.memory_bytes,
            userspace_addr: host_addr as u64,
            flags: 0,
        };
        unsafe {
            vm_fd
                .set_user_memory_region(mem_region)
                .map_err(|e| VzError::new(format!("KVM_SET_USER_MEMORY_REGION: {}", e)))?;
        }

        let vmgenid = vmgenid::VmGenId::new(&vm_fd)
            .map_err(|e| VzError::new(format!("failed to set up vmgenid: {e}")))?;

        // Build MMIO bus
        let mut bus = MmioBus::new();
        let mut virtio_idx: u32 = 0;

        // PL011 UART
        bus.add(
            UART_BASE,
            UART_SIZE,
            Box::new(Mutex::new(Pl011::new(
                config.serial_write_fd,
                config.serial_read_fd,
            ))),
        );

        // PL031 RTC - needed so the guest boots with the host wall clock
        // (otherwise TLS certs are "not yet valid" since 1970-01-01).
        bus.add(RTC_BASE, RTC_SIZE, Box::new(Mutex::new(Pl031::new())));

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

        // Create vCPUs (must exist before GIC init)
        let mut vcpus = Vec::with_capacity(config.cpu_count);
        for i in 0..config.cpu_count {
            vcpus.push(
                vm_fd
                    .create_vcpu(i as u64)
                    .map_err(|e| VzError::new(format!("KVM_CREATE_VCPU({}): {}", i, e)))?,
            );
        }

        // GIC-v3
        let gic = create_gic(&vm_fd, config.cpu_count as u64)?;

        match config.boot {
            Boot::Kernel {
                mut kernel,
                initrd,
                command_line,
            } => {
                let (entry_addr, initrd) =
                    load_kernel(&mem, &mut kernel, initrd, config.memory_bytes)?;
                for (i, vcpu) in vcpus.iter().enumerate() {
                    init_vcpu(&vm_fd, vcpu, i, entry_addr)?;
                }
                let fdt_data = generate_fdt(
                    config.cpu_count,
                    config.memory_bytes,
                    &command_line,
                    initrd,
                    virtio_idx,
                )?;
                let fdt_addr = DRAM_BASE + config.memory_bytes - FDT_MAX_SIZE;
                mem.write_slice(&fdt_data, GuestAddress(fdt_addr))
                    .map_err(|e| VzError::new(format!("failed to write FDT: {}", e)))?;
                // x0 = FDT address for the boot CPU
                if let Some(vcpu) = vcpus.first() {
                    set_reg(vcpu, REG_X0, fdt_addr)?;
                }
            }
            Boot::Snapshot { state, .. } => {
                restore(&vm_fd, &vcpus, &gic, &bus, &state, config.memory_bytes)
                    .map_err(|e| VzError::new(format!("failed to restore the snapshot: {e}")))?;
            }
        }

        Ok(KvmVm {
            kvm,
            vm_fd,
            mem,
            memory_bytes: config.memory_bytes,
            bus: Arc::new(bus),
            vcpus,
            vcpu_threads: Vec::new(),
            running: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            vsock: vsock_control,
            balloon: balloon_control,
            gic,
            _vmgenid: vmgenid,
        })
    }

    pub fn start(&mut self) -> Result<(), VzError> {
        self.running.store(true, Ordering::Release);

        let vcpus = std::mem::take(&mut self.vcpus);
        for vcpu in vcpus {
            let bus = self.bus.clone();
            let running = self.running.clone();
            let paused = self.paused.clone();

            self.vcpu_threads.push(
                std::thread::Builder::new()
                    .name("shuru-vcpu".into())
                    .spawn(move || vcpu_run_loop(vcpu, bus, running, paused))
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
        let devices = self
            .bus
            .devices
            .iter()
            .map(|entry| entry.device.save())
            .collect::<io::Result<Vec<_>>>()?;
        let vcpus = self
            .vcpus
            .iter()
            .map(snapshot::save_vcpu)
            .collect::<io::Result<Vec<_>>>()?;
        let gic = snapshot::save_gic(&self.gic, &vcpus)?;
        snapshot::save_memory(pagemap, &self.mem, self.memory_bytes, memory_out)?;
        Ok(SnapshotState {
            version: snapshot::FORMAT_VERSION,
            cpu_count: vcpus.len(),
            memory_bytes: self.memory_bytes,
            vcpus,
            gic,
            devices,
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

// ===========================================================================
// GIC-v3
// ===========================================================================

fn create_gic(vm_fd: &VmFd, _vcpu_count: u64) -> Result<DeviceFd, VzError> {
    let mut gic_device = kvm_bindings::kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
        fd: 0,
        flags: 0,
    };
    let gic_fd = vm_fd
        .create_device(&mut gic_device)
        .map_err(|e| VzError::new(format!("KVM_CREATE_DEVICE(GIC): {}", e)))?;

    let dist_addr: u64 = GIC_DIST_BASE;
    gic_fd
        .set_device_attr(&kvm_bindings::kvm_device_attr {
            group: KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_DIST as u64,
            addr: &dist_addr as *const u64 as u64,
            flags: 0,
        })
        .map_err(|e| VzError::new(format!("GIC set dist addr: {}", e)))?;

    let redist_addr: u64 = GIC_REDIST_BASE;
    gic_fd
        .set_device_attr(&kvm_bindings::kvm_device_attr {
            group: KVM_DEV_ARM_VGIC_GRP_ADDR,
            attr: kvm_bindings::KVM_VGIC_V3_ADDR_TYPE_REDIST as u64,
            addr: &redist_addr as *const u64 as u64,
            flags: 0,
        })
        .map_err(|e| VzError::new(format!("GIC set redist addr: {}", e)))?;

    // Configure the number of IRQs (SPIs + SGIs + PPIs).
    // Must be at least 64 (32 SGI/PPI + 32 SPI minimum).
    // We use 128 to have room for our devices (SPI 16-23 = IRQ 48-55).
    let nr_irqs: u32 = 128;
    gic_fd
        .set_device_attr(&kvm_bindings::kvm_device_attr {
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            attr: 0,
            addr: &nr_irqs as *const u32 as u64,
            flags: 0,
        })
        .map_err(|e| VzError::new(format!("GIC set nr_irqs: {}", e)))?;

    gic_fd
        .set_device_attr(&kvm_bindings::kvm_device_attr {
            group: KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr: kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
            addr: 0,
            flags: 0,
        })
        .map_err(|e| VzError::new(format!("GIC init: {}", e)))?;

    Ok(gic_fd)
}

// ===========================================================================
// Kernel and snapshot
// ===========================================================================

/// Loads the kernel and the initrd. Returns the entry point and where the
/// initrd went.
fn load_kernel(
    mem: &GuestMemoryMmap,
    kernel: &mut File,
    initrd: Option<File>,
    memory_bytes: u64,
) -> Result<(u64, Option<(u64, u64)>), VzError> {
    // kernel_offset must be 2MB-aligned. The PE loader adds text_offset
    // from the image header internally, so pass DRAM_BASE (1 GiB aligned).
    let kernel_result = PE::load(mem, Some(GuestAddress(DRAM_BASE)), kernel, None)
        .map_err(|e| VzError::new(format!("failed to load kernel: {}", e)))?;
    let entry_addr = kernel_result.kernel_load.raw_value();

    let Some(mut initrd) = initrd else {
        return Ok((entry_addr, None));
    };
    let mut initrd_data = Vec::new();
    initrd
        .read_to_end(&mut initrd_data)
        .map_err(|e| VzError::new(format!("failed to read initrd: {}", e)))?;
    // Place after FDT reservation, page-aligned
    let load_addr =
        DRAM_BASE + memory_bytes - FDT_MAX_SIZE - ((initrd_data.len() as u64 + 0xFFF) & !0xFFF);
    mem.write_slice(&initrd_data, GuestAddress(load_addr))
        .map_err(|e| VzError::new(format!("failed to write initrd: {}", e)))?;
    Ok((entry_addr, Some((load_addr, initrd_data.len() as u64))))
}

/// Puts a snapshot into a VM built with the same devices: vCPUs, then the
/// GIC, whose redistributors are found by vCPU affinity, then the devices,
/// whose interrupt lines need the GIC.
fn restore(
    vm_fd: &VmFd,
    vcpus: &[VcpuFd],
    gic: &DeviceFd,
    bus: &MmioBus,
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
    {
        return Err(snapshot::mismatch());
    }
    for (vcpu, saved) in vcpus.iter().zip(&state.vcpus) {
        vcpu_init(vm_fd, vcpu, false).map_err(|e| io::Error::other(e.to_string()))?;
        snapshot::check_cpu(vcpu, saved)?;
        snapshot::restore_vcpu(vcpu, saved)?;
    }
    snapshot::restore_gic(gic, &state.gic, &state.vcpus)?;
    for (entry, saved) in bus.devices.iter().zip(&state.devices) {
        entry.device.restore(saved)?;
    }
    vmgenid::VmGenId::notify(vm_fd)
}

// ===========================================================================
// vCPU init
// ===========================================================================

fn vcpu_init(vm_fd: &VmFd, vcpu: &VcpuFd, power_off: bool) -> Result<(), VzError> {
    let mut kvi = kvm_bindings::kvm_vcpu_init::default();
    vm_fd
        .get_preferred_target(&mut kvi)
        .map_err(|e| VzError::new(format!("KVM_ARM_PREFERRED_TARGET: {}", e)))?;

    kvi.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
    if power_off {
        kvi.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
    }

    vcpu.vcpu_init(&kvi)
        .map_err(|e| VzError::new(format!("KVM_ARM_VCPU_INIT: {}", e)))
}

fn init_vcpu(vm_fd: &VmFd, vcpu: &VcpuFd, cpu_id: usize, entry_addr: u64) -> Result<(), VzError> {
    vcpu_init(vm_fd, vcpu, cpu_id > 0).map_err(|e| VzError::new(format!("vCPU {cpu_id}: {e}")))?;

    if cpu_id == 0 {
        set_reg(vcpu, REG_PC, entry_addr)?;
        set_reg(vcpu, REG_PSTATE, PSTATE_FAULT_BITS_64)?;
    }
    Ok(())
}

fn set_reg(vcpu: &VcpuFd, reg_id: u64, value: u64) -> Result<(), VzError> {
    vcpu.set_one_reg(reg_id, &value.to_le_bytes())
        .map_err(|e| VzError::new(format!("KVM_SET_ONE_REG(0x{:x}): {}", reg_id, e)))?;
    Ok(())
}

// ===========================================================================
// FDT
// ===========================================================================

fn generate_fdt(
    cpu_count: usize,
    mem_size: u64,
    cmdline: &str,
    initrd: Option<(u64, u64)>,
    virtio_device_count: u32,
) -> Result<Vec<u8>, VzError> {
    use vm_fdt::FdtWriter;

    let mut fdt = FdtWriter::new().map_err(|e| VzError::new(format!("FdtWriter::new: {}", e)))?;

    let root = fdt.begin_node("").unwrap();
    fdt.property_string("compatible", "linux,dummy-virt")
        .unwrap();
    fdt.property_u32("#address-cells", 2).unwrap();
    fdt.property_u32("#size-cells", 2).unwrap();
    fdt.property_u32("interrupt-parent", 1).unwrap();

    // /chosen
    let chosen = fdt.begin_node("chosen").unwrap();
    fdt.property_string("bootargs", cmdline).unwrap();
    fdt.property_string("stdout-path", "/pl011@9000000")
        .unwrap();
    if let Some((addr, size)) = initrd {
        fdt.property_u64("linux,initrd-start", addr).unwrap();
        fdt.property_u64("linux,initrd-end", addr + size).unwrap();
    }
    fdt.end_node(chosen).unwrap();

    // /memory
    let mem_node = fdt.begin_node(&format!("memory@{:x}", DRAM_BASE)).unwrap();
    fdt.property_string("device_type", "memory").unwrap();
    fdt.property_array_u64("reg", &[DRAM_BASE, mem_size])
        .unwrap();
    fdt.end_node(mem_node).unwrap();

    // /cpus
    let cpus = fdt.begin_node("cpus").unwrap();
    fdt.property_u32("#address-cells", 1).unwrap();
    fdt.property_u32("#size-cells", 0).unwrap();
    for i in 0..cpu_count {
        let cpu = fdt.begin_node(&format!("cpu@{}", i)).unwrap();
        fdt.property_string("device_type", "cpu").unwrap();
        fdt.property_string("compatible", "arm,arm-v8").unwrap();
        fdt.property_string("enable-method", "psci").unwrap();
        fdt.property_u32("reg", i as u32).unwrap();
        fdt.end_node(cpu).unwrap();
    }
    fdt.end_node(cpus).unwrap();

    // /psci
    let psci = fdt.begin_node("psci").unwrap();
    fdt.property_string_list(
        "compatible",
        vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()],
    )
    .unwrap();
    fdt.property_string("method", "hvc").unwrap();
    fdt.end_node(psci).unwrap();

    // /intc (GIC-v3)
    let redist_size = cpu_count as u64 * GIC_REDIST_SIZE_PER_CPU;
    let gic = fdt
        .begin_node(&format!("intc@{:x}", GIC_DIST_BASE))
        .unwrap();
    fdt.property_string("compatible", "arm,gic-v3").unwrap();
    fdt.property_u32("#interrupt-cells", 3).unwrap();
    fdt.property_null("interrupt-controller").unwrap();
    fdt.property_u32("phandle", 1).unwrap();
    fdt.property_array_u64(
        "reg",
        &[GIC_DIST_BASE, GIC_DIST_SIZE, GIC_REDIST_BASE, redist_size],
    )
    .unwrap();
    fdt.end_node(gic).unwrap();

    // /timer
    let timer = fdt.begin_node("timer").unwrap();
    fdt.property_string("compatible", "arm,armv8-timer")
        .unwrap();
    fdt.property_null("always-on").unwrap();
    fdt.property_array_u32(
        "interrupts",
        &[
            1, 13, 8, // secure phys
            1, 14, 8, // non-secure phys
            1, 11, 8, // virtual
            1, 10, 8, // hypervisor
        ],
    )
    .unwrap();
    fdt.end_node(timer).unwrap();

    // /pclk (fixed clock for PL011)
    let pclk = fdt.begin_node("pclk").unwrap();
    fdt.property_string("compatible", "fixed-clock").unwrap();
    fdt.property_u32("#clock-cells", 0).unwrap();
    fdt.property_u32("clock-frequency", 24_000_000).unwrap();
    fdt.property_u32("phandle", 2).unwrap();
    fdt.end_node(pclk).unwrap();

    // /pl011
    let uart = fdt.begin_node(&format!("pl011@{:x}", UART_BASE)).unwrap();
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl011".into(), "arm,primecell".into()],
    )
    .unwrap();
    fdt.property_array_u64("reg", &[UART_BASE, UART_SIZE])
        .unwrap();
    fdt.property_array_u32("interrupts", &[0, UART_SPI, 4])
        .unwrap();
    fdt.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])
        .unwrap();
    fdt.property_array_u32("clocks", &[2, 2]).unwrap();
    fdt.end_node(uart).unwrap();

    // /pl031 RTC (no IRQ — we only expose the data register, no alarm)
    let rtc = fdt.begin_node(&format!("pl031@{:x}", RTC_BASE)).unwrap();
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl031".into(), "arm,primecell".into()],
    )
    .unwrap();
    fdt.property_array_u64("reg", &[RTC_BASE, RTC_SIZE])
        .unwrap();
    fdt.property_string("clock-names", "apb_pclk").unwrap();
    fdt.property_u32("clocks", 2).unwrap();
    fdt.end_node(rtc).unwrap();

    let vmgenid = fdt
        .begin_node(&format!("vmgenid@{:x}", VMGENID_BASE))
        .unwrap();
    fdt.property_string("compatible", "microsoft,vmgenid")
        .unwrap();
    fdt.property_array_u64("reg", &[VMGENID_BASE, VMGENID_SIZE])
        .unwrap();
    fdt.property_array_u32("interrupts", &[0, VMGENID_SPI, 1]) // SPI, edge rising
        .unwrap();
    fdt.end_node(vmgenid).unwrap();

    // Virtio MMIO devices
    for i in 0..virtio_device_count {
        let base = VIRTIO_MMIO_BASE + (i as u64) * VIRTIO_MMIO_GAP;
        let spi = VIRTIO_SPI_BASE + i;
        let node = fdt.begin_node(&format!("virtio_mmio@{:x}", base)).unwrap();
        fdt.property_string("compatible", "virtio,mmio").unwrap();
        fdt.property_array_u64("reg", &[base, VIRTIO_MMIO_SIZE])
            .unwrap();
        fdt.property_array_u32("interrupts", &[0, spi, 4]).unwrap(); // SPI, level-high
        fdt.end_node(node).unwrap();
    }

    fdt.end_node(root).unwrap();
    fdt.finish()
        .map_err(|e| VzError::new(format!("FDT finish: {}", e)))
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
    let spi = VIRTIO_SPI_BASE + *virtio_idx;
    let base = VIRTIO_MMIO_BASE + (*virtio_idx as u64) * VIRTIO_MMIO_GAP;
    let device = VirtioMmioDevice::new(backend, base, spi, vm_fd.clone(), mem.clone())
        .map_err(|e| VzError::new(format!("failed to set up virtio device: {e}")))?;
    bus.add(base, VIRTIO_MMIO_SIZE, Box::new(device));
    *virtio_idx += 1;
    Ok(())
}

/// Runs until the guest stops or the VM pauses, and hands the vCPU back.
fn vcpu_run_loop(
    mut vcpu: VcpuFd,
    bus: Arc<MmioBus>,
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
