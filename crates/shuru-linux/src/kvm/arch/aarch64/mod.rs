//! arm64 guests: a GICv3, PSCI, a devicetree, the PL011 and PL031.

mod devices;
pub mod layout;
mod snapshot;

use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

use kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3;
use kvm_bindings::{KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_DEV_ARM_VGIC_GRP_CTRL};
use kvm_ioctls::{DeviceFd, Kvm, VcpuFd, VmFd};
use linux_loader::loader::{pe::PE, KernelLoader};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use self::devices::{Pl011, Pl031};
pub use self::layout::DRAM_BASE;
use self::layout::*;
pub use self::snapshot::{GicState as IrqchipState, VcpuState};
use crate::error::VzError;
use crate::kvm::devices::MmioBus;
use crate::kvm::vmgenid;

/// The guest console and the early console that prints before it.
pub const CONSOLE: &str = "console=ttyAMA0";
pub const EARLYCON: &str = "earlycon=pl011,mmio,0x09000000";

/// Guest RAM: one range at `DRAM_BASE`.
pub fn memory_ranges(memory_bytes: u64) -> Vec<(u64, u64)> {
    vec![(DRAM_BASE, memory_bytes)]
}

/// Where virtio device `index` sits and the interrupt line it raises.
pub fn virtio_slot(index: u32) -> (u64, u64, u32) {
    let base = VIRTIO_MMIO_BASE + index as u64 * VIRTIO_MMIO_GAP;
    (base, VIRTIO_MMIO_SIZE, spi_to_irq(VIRTIO_SPI_BASE + index))
}

/// What the VM needs before its vCPUs exist. Nothing on arm64.
pub fn prepare(_vm_fd: &VmFd) -> Result<(), VzError> {
    Ok(())
}

/// The UART and the RTC, which gives the guest the wall clock at boot
/// (otherwise TLS certificates are not yet valid at 1970-01-01).
pub fn add_devices(
    _vm_fd: &Arc<VmFd>,
    bus: &mut MmioBus,
    _ports: &mut MmioBus,
    serial_write_fd: Option<i32>,
    serial_read_fd: Option<i32>,
) {
    bus.add(
        UART_BASE,
        UART_SIZE,
        Box::new(Mutex::new(Pl011::new(serial_write_fd, serial_read_fd))),
    );
    bus.add(RTC_BASE, RTC_SIZE, Box::new(Mutex::new(Pl031::new())));
}

/// A port write that resets the machine. arm64 has no ports.
pub fn is_reset(_port: u16, _data: &[u8]) -> bool {
    false
}

/// The interrupt controller and the generation ID, made once the vCPUs
/// exist.
pub struct Platform {
    gic: DeviceFd,
    _vmgenid: vmgenid::VmGenId,
}

impl Platform {
    pub fn new(_kvm: &Kvm, vm_fd: &VmFd, cpu_count: usize) -> Result<Self, VzError> {
        let vmgenid = vmgenid::VmGenId::new(vm_fd)
            .map_err(|e| VzError::new(format!("failed to set up vmgenid: {e}")))?;
        Ok(Platform {
            gic: create_gic(vm_fd, cpu_count as u64)?,
            _vmgenid: vmgenid,
        })
    }

    pub fn save(
        &self,
        _vm_fd: &VmFd,
        vcpus: &[VcpuFd],
    ) -> io::Result<(Vec<VcpuState>, IrqchipState)> {
        let states = vcpus
            .iter()
            .map(snapshot::save_vcpu)
            .collect::<io::Result<Vec<_>>>()?;
        let gic = snapshot::save_gic(&self.gic, &states)?;
        Ok((states, gic))
    }

    /// vCPUs first, then the GIC, whose redistributors are found by vCPU
    /// affinity.
    pub fn restore(
        &self,
        vm_fd: &VmFd,
        vcpus: &[VcpuFd],
        states: &[VcpuState],
        gic: &IrqchipState,
    ) -> io::Result<()> {
        for (vcpu, saved) in vcpus.iter().zip(states) {
            vcpu_init(vm_fd, vcpu, false).map_err(|e| io::Error::other(e.to_string()))?;
            snapshot::check_cpu(vcpu, saved)?;
            snapshot::restore_vcpu(vcpu, saved)?;
        }
        snapshot::restore_gic(&self.gic, gic, states)
    }

    /// Tells a restored guest that its generation changed.
    pub fn restored(&self, vm_fd: &VmFd) -> io::Result<()> {
        vmgenid::VmGenId::notify(vm_fd)
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
    let nr_irqs: u32 = NR_IRQS;
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
// Boot
// ===========================================================================

/// Loads the kernel and the initrd, points the boot vCPU at the kernel and
/// hands it the devicetree. The other vCPUs wait for PSCI.
#[allow(clippy::too_many_arguments)]
pub fn boot(
    _kvm: &Kvm,
    vm_fd: &VmFd,
    mem: &GuestMemoryMmap,
    vcpus: &[VcpuFd],
    kernel: &mut File,
    initrd: Option<File>,
    command_line: &str,
    memory_bytes: u64,
    virtio_count: u32,
) -> Result<(), VzError> {
    let (entry_addr, initrd) = load_kernel(mem, kernel, initrd, memory_bytes)?;
    for (i, vcpu) in vcpus.iter().enumerate() {
        init_vcpu(vm_fd, vcpu, i, entry_addr)?;
    }
    let fdt_data = generate_fdt(
        vcpus.len(),
        memory_bytes,
        command_line,
        initrd,
        virtio_count,
    )?;
    let fdt_addr = DRAM_BASE + memory_bytes - FDT_MAX_SIZE;
    mem.write_slice(&fdt_data, GuestAddress(fdt_addr))
        .map_err(|e| VzError::new(format!("failed to write FDT: {}", e)))?;
    // x0 = FDT address for the boot CPU
    if let Some(vcpu) = vcpus.first() {
        set_reg(vcpu, REG_X0, fdt_addr)?;
    }
    Ok(())
}

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
