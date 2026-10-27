pub mod balloon;
pub mod devices;
pub mod layout;
pub mod virtio_fs;

use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::os::unix::thread::JoinHandleExt;

use kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3;
use kvm_ioctls::{DeviceFd, Kvm, VcpuFd, VmFd};
use linux_loader::loader::{pe::PE, KernelLoader};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use self::devices::*;
use self::layout::*;
use crate::error::VzError;

// Use the actual constants from kvm-bindings (not our hand-defined ones)
use kvm_bindings::{KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_DEV_ARM_VGIC_GRP_CTRL};

// ===========================================================================
// VM creation
// ===========================================================================

pub struct KvmVm {
    #[allow(dead_code)]
    pub kvm: Kvm,
    pub vm_fd: Arc<VmFd>,
    pub mem: GuestMemoryMmap,
    pub bus: Arc<Mutex<MmioBus>>,
    pub vcpus: Vec<VcpuFd>,
    pub vcpu_threads: Vec<std::thread::JoinHandle<()>>,
    pub running: Arc<AtomicBool>,
    pub guest_cid: u64,
    pub balloon: Option<Arc<balloon::BalloonControl>>,
    #[allow(dead_code)]
    _gic: DeviceFd,
}

pub struct VmCreateConfig {
    pub cpu_count: usize,
    pub memory_bytes: u64,
    pub kernel_path: String,
    pub initrd_path: Option<String>,
    pub command_line: String,
    pub serial_write_fd: Option<i32>,
    pub serial_read_fd: Option<i32>,
    pub disk_path: Option<String>,
    pub disk_read_only: bool,
    pub network_fd: Option<i32>,
    pub network_mac: Option<[u8; 6]>,
    pub has_vsock: bool,
    pub guest_cid: u64,
    /// Virtio-fs mounts: (tag, host_path, read_only).
    pub mounts: Vec<(String, String, bool)>,
    pub has_balloon: bool,
}

impl KvmVm {
    pub fn create(config: VmCreateConfig) -> Result<Self, VzError> {
        let kvm =
            Kvm::new().map_err(|e| VzError::new(format!("failed to open /dev/kvm: {}", e)))?;
        let vm_fd = kvm
            .create_vm()
            .map_err(|e| VzError::new(format!("failed to create VM: {}", e)))?;
        let vm_fd = Arc::new(vm_fd);

        // Guest memory
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(
            GuestAddress(DRAM_BASE),
            config.memory_bytes as usize,
        )])
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

        // Load kernel (PE format for aarch64)
        let mut kernel_file = File::open(&config.kernel_path)
            .map_err(|e| VzError::new(format!("failed to open kernel: {}", e)))?;
        // kernel_offset must be 2MB-aligned. The PE loader adds text_offset
        // from the image header internally, so pass DRAM_BASE (1 GiB aligned).
        let kernel_result = PE::load(&mem, Some(GuestAddress(DRAM_BASE)), &mut kernel_file, None)
            .map_err(|e| VzError::new(format!("failed to load kernel: {}", e)))?;
        let entry_addr = kernel_result.kernel_load.raw_value();

        // Load initrd
        let mut initrd_addr: Option<u64> = None;
        let mut initrd_size: u64 = 0;
        if let Some(ref initrd_path) = config.initrd_path {
            let mut initrd_data = Vec::new();
            File::open(initrd_path)
                .and_then(|mut f| f.read_to_end(&mut initrd_data))
                .map_err(|e| VzError::new(format!("failed to read initrd: {}", e)))?;

            // Place after FDT reservation, page-aligned
            let load_addr = DRAM_BASE + config.memory_bytes
                - FDT_MAX_SIZE
                - ((initrd_data.len() as u64 + 0xFFF) & !0xFFF);
            mem.write_slice(&initrd_data, GuestAddress(load_addr))
                .map_err(|e| VzError::new(format!("failed to write initrd: {}", e)))?;
            initrd_addr = Some(load_addr);
            initrd_size = initrd_data.len() as u64;
        }

        // Build MMIO bus
        let mut bus = MmioBus::new();
        let mut virtio_idx: u32 = 0;

        // PL011 UART
        bus.add(
            UART_BASE,
            UART_SIZE,
            Box::new(Pl011::new(config.serial_write_fd, config.serial_read_fd)),
        );

        // PL031 RTC - needed so the guest boots with the host wall clock
        // (otherwise TLS certs are "not yet valid" since 1970-01-01).
        bus.add(RTC_BASE, RTC_SIZE, Box::new(Pl031::new()));

        // Virtio block
        if let Some(ref disk_path) = config.disk_path {
            let blk = BlockBackend::new(disk_path, config.disk_read_only)
                .map_err(|e| VzError::new(format!("failed to open disk: {}", e)))?;
            let spi = VIRTIO_SPI_BASE + virtio_idx;
            let base = VIRTIO_MMIO_BASE + (virtio_idx as u64) * VIRTIO_MMIO_GAP;
            bus.add(
                base,
                VIRTIO_MMIO_SIZE,
                Box::new(VirtioMmioDevice::new(
                    Box::new(blk),
                    spi,
                    vm_fd.clone(),
                    mem.clone(),
                )),
            );
            virtio_idx += 1;
        }

        // Vhost-vsock
        let guest_cid = config.guest_cid;
        if config.has_vsock {
            let spi = VIRTIO_SPI_BASE + virtio_idx;
            let base = VIRTIO_MMIO_BASE + (virtio_idx as u64) * VIRTIO_MMIO_GAP;
            bus.add(
                base,
                VIRTIO_MMIO_SIZE,
                Box::new(VirtioMmioDevice::new(
                    Box::new(VhostVsockBackend::new(guest_cid)),
                    spi,
                    vm_fd.clone(),
                    mem.clone(),
                )),
            );
            virtio_idx += 1;
        }

        // Virtio-fs mounts (one device per mount)
        for (tag, host_path, read_only) in &config.mounts {
            let fs = virtio_fs::VirtioFsBackend::new(tag, host_path, *read_only).map_err(|e| {
                VzError::new(format!(
                    "failed to set up virtio-fs for tag '{}': {}",
                    tag, e
                ))
            })?;
            let spi = VIRTIO_SPI_BASE + virtio_idx;
            let base = VIRTIO_MMIO_BASE + (virtio_idx as u64) * VIRTIO_MMIO_GAP;
            bus.add(
                base,
                VIRTIO_MMIO_SIZE,
                Box::new(VirtioMmioDevice::new(
                    Box::new(fs),
                    spi,
                    vm_fd.clone(),
                    mem.clone(),
                )),
            );
            virtio_idx += 1;
        }

        // Virtio net
        if let Some(net_fd) = config.network_fd {
            let mac = config
                .network_mac
                .unwrap_or([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
            let spi = VIRTIO_SPI_BASE + virtio_idx;
            let base = VIRTIO_MMIO_BASE + (virtio_idx as u64) * VIRTIO_MMIO_GAP;
            bus.add(
                base,
                VIRTIO_MMIO_SIZE,
                Box::new(VirtioMmioDevice::new(
                    Box::new(NetBackend::new(net_fd, mac)),
                    spi,
                    vm_fd.clone(),
                    mem.clone(),
                )),
            );
            virtio_idx += 1;
        }

        // Virtio balloon
        let mut balloon_control = None;
        if config.has_balloon {
            let backend = balloon::BalloonBackend::new(config.memory_bytes);
            balloon_control = Some(backend.control());
            let spi = VIRTIO_SPI_BASE + virtio_idx;
            let base = VIRTIO_MMIO_BASE + (virtio_idx as u64) * VIRTIO_MMIO_GAP;
            bus.add(
                base,
                VIRTIO_MMIO_SIZE,
                Box::new(VirtioMmioDevice::new(
                    Box::new(backend),
                    spi,
                    vm_fd.clone(),
                    mem.clone(),
                )),
            );
            virtio_idx += 1;
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

        // Init vCPU registers
        for (i, vcpu) in vcpus.iter().enumerate() {
            init_vcpu(&vm_fd, vcpu, i, entry_addr)?;
        }

        // FDT
        let fdt_data = generate_fdt(
            config.cpu_count,
            config.memory_bytes,
            &config.command_line,
            initrd_addr,
            initrd_size,
            virtio_idx,
        )?;
        let fdt_addr = DRAM_BASE + config.memory_bytes - FDT_MAX_SIZE;
        mem.write_slice(&fdt_data, GuestAddress(fdt_addr))
            .map_err(|e| VzError::new(format!("failed to write FDT: {}", e)))?;

        // Set x0 = FDT address for boot CPU
        if let Some(vcpu) = vcpus.first() {
            set_reg(vcpu, REG_X0, fdt_addr)?;
        }

        Ok(KvmVm {
            kvm,
            vm_fd,
            mem,
            bus: Arc::new(Mutex::new(bus)),
            vcpus,
            vcpu_threads: Vec::new(),
            running: Arc::new(AtomicBool::new(false)),
            guest_cid,
            balloon: balloon_control,
            _gic: gic,
        })
    }

    pub fn start(&mut self) -> Result<(), VzError> {
        self.running.store(true, Ordering::Release);

        let vcpus = std::mem::take(&mut self.vcpus);
        for vcpu in vcpus {
            let bus = self.bus.clone();
            let running = self.running.clone();

            self.vcpu_threads.push(
                std::thread::Builder::new()
                    .name("shuru-vcpu".into())
                    .spawn(move || vcpu_run_loop(vcpu, bus, running))
                    .map_err(|e| VzError::new(format!("failed to spawn vCPU: {}", e)))?,
            );
        }
        Ok(())
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
// vCPU init
// ===========================================================================

fn init_vcpu(vm_fd: &VmFd, vcpu: &VcpuFd, cpu_id: usize, entry_addr: u64) -> Result<(), VzError> {
    let mut kvi = kvm_bindings::kvm_vcpu_init::default();
    vm_fd
        .get_preferred_target(&mut kvi)
        .map_err(|e| VzError::new(format!("KVM_ARM_PREFERRED_TARGET: {}", e)))?;

    kvi.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
    if cpu_id > 0 {
        kvi.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
    }

    vcpu.vcpu_init(&kvi)
        .map_err(|e| VzError::new(format!("KVM_ARM_VCPU_INIT({}): {}", cpu_id, e)))?;

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
    initrd_addr: Option<u64>,
    initrd_size: u64,
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
    if let Some(addr) = initrd_addr {
        fdt.property_u64("linux,initrd-start", addr).unwrap();
        fdt.property_u64("linux,initrd-end", addr + initrd_size)
            .unwrap();
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

fn vcpu_run_loop(mut vcpu: VcpuFd, bus: Arc<Mutex<MmioBus>>, running: Arc<AtomicBool>) {
    // Install empty handler for SIGRTMIN so KVM_RUN returns EINTR on stop
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = empty_signal_handler as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGRTMIN(), &sa, std::ptr::null_mut());
    }

    while running.load(Ordering::Acquire) {
        match vcpu.run() {
            Ok(exit_reason) => {
                use kvm_ioctls::VcpuExit;
                match exit_reason {
                    VcpuExit::MmioRead(addr, data) => {
                        // data is &mut [u8] — write device response directly
                        bus.lock().unwrap().handle_read(addr, data);
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        bus.lock().unwrap().handle_write(addr, data);
                    }
                    VcpuExit::SystemEvent(..) | VcpuExit::Shutdown => {
                        running.store(false, Ordering::Release);
                        return;
                    }
                    VcpuExit::Hlt => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    VcpuExit::InternalError => {
                        eprintln!("shuru: KVM internal error");
                        running.store(false, Ordering::Release);
                        return;
                    }
                    _ => {} // other exits — continue
                }
            }
            Err(e) => {
                if e.errno() != libc::EAGAIN && e.errno() != libc::EINTR {
                    eprintln!("shuru: KVM_RUN error: {}", e);
                    running.store(false, Ordering::Release);
                    return;
                }
                // EINTR from our signal → check running flag and loop
            }
        }
    }
}

extern "C" fn empty_signal_handler(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // Empty — just used to interrupt KVM_RUN
}
