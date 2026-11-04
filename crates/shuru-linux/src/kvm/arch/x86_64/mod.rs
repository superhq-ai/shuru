//! x86_64 guests: the in-kernel PIC, IOAPIC and PIT, hardware-reduced
//! ACPI tables that list the CPUs and devices, the 64-bit boot protocol and
//! COM1. The wall clock comes from kvmclock.

mod acpi;
mod cpu;
pub mod layout;
mod serial;
mod snapshot;

use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

use kvm_bindings::{kvm_pit_config, CpuId, KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use linux_loader::loader::bootparam::{boot_e820_entry, boot_params};
use linux_loader::loader::{elf::Elf, KernelLoader};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

pub use self::layout::DRAM_BASE;
use self::layout::*;
use crate::error::VzError;
use crate::kvm::devices::MmioBus;
use crate::kvm::vmgenid::VmGenId;

/// The guest console and the early console that prints before it.
pub const CONSOLE: &str = "console=ttyS0";
pub const EARLYCON: &str = "earlycon=uart8250,io,0x3f8";

/// Guest RAM: up to the hole below 4 GiB, the rest above it.
pub fn memory_ranges(memory_bytes: u64) -> Vec<(u64, u64)> {
    let low = memory_bytes.min(MMIO_HOLE_START);
    let mut ranges = vec![(DRAM_BASE, low)];
    if memory_bytes > low {
        ranges.push((HIGH_RAM_START, memory_bytes - low));
    }
    ranges
}

/// Where virtio device `index` sits and the interrupt line it raises.
pub fn virtio_slot(index: u32) -> (u64, u64, u32) {
    let base = VIRTIO_MMIO_BASE + index as u64 * VIRTIO_MMIO_SIZE;
    (base, VIRTIO_MMIO_SIZE, VIRTIO_IRQ_BASE + index)
}

/// The interrupt controllers and the PIT, which must exist before the
/// vCPUs, and the pages KVM keeps for itself on Intel.
pub fn prepare(vm_fd: &VmFd) -> Result<(), VzError> {
    vm_fd
        .set_identity_map_address(IDENTITY_MAP_ADDR)
        .map_err(|e| VzError::new(format!("KVM_SET_IDENTITY_MAP_ADDR: {e}")))?;
    vm_fd
        .set_tss_address(TSS_ADDR as usize)
        .map_err(|e| VzError::new(format!("KVM_SET_TSS_ADDR: {e}")))?;
    vm_fd
        .create_irq_chip()
        .map_err(|e| VzError::new(format!("KVM_CREATE_IRQCHIP: {e}")))?;
    let pit = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm_fd
        .create_pit2(pit)
        .map_err(|e| VzError::new(format!("KVM_CREATE_PIT2: {e}")))
}

/// COM1. Reads of ports nothing answers return zero, which also tells
/// the kernel the keyboard controller is ready for the reset command.
pub fn add_devices(
    vm_fd: &Arc<VmFd>,
    _bus: &mut MmioBus,
    ports: &mut MmioBus,
    serial_write_fd: Option<i32>,
    _serial_read_fd: Option<i32>,
) {
    ports.add(
        SERIAL_PORT,
        SERIAL_PORT_SIZE,
        Box::new(Mutex::new(serial::Serial::new(
            vm_fd.clone(),
            serial_write_fd,
        ))),
    );
}

/// The keyboard controller reset, which is how the guest powers off: it
/// reboots with reboot=k, and the VM ends there.
pub fn is_reset(port: u16, data: &[u8]) -> bool {
    port == I8042_COMMAND_PORT && data.first() == Some(&I8042_RESET)
}

pub use snapshot::{IrqchipState, VcpuState};

/// The vmgenid page, and what a snapshot needs from KVM: the MSRs it
/// migrates, and the CPUID a guest booted here would see, to check a
/// snapshot against.
pub struct Platform {
    msrs: Vec<u32>,
    cpuid: CpuId,
    _vmgenid: VmGenId,
}

impl Platform {
    pub fn new(kvm: &Kvm, vm_fd: &VmFd, cpu_count: usize) -> Result<Self, VzError> {
        let vmgenid = VmGenId::new(vm_fd)
            .map_err(|e| VzError::new(format!("failed to set up vmgenid: {e}")))?;
        let msrs = kvm
            .get_msr_index_list()
            .map_err(|e| VzError::new(format!("KVM_GET_MSR_INDEX_LIST: {e}")))?;
        let supported = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| VzError::new(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;
        Ok(Platform {
            msrs: msrs.as_slice().to_vec(),
            cpuid: cpu::cpuid_for(&supported, 0, cpu_count as u32)?,
            _vmgenid: vmgenid,
        })
    }

    pub fn save(
        &self,
        vm_fd: &VmFd,
        vcpus: &[VcpuFd],
    ) -> io::Result<(Vec<VcpuState>, IrqchipState)> {
        let states = vcpus
            .iter()
            .map(|vcpu| snapshot::save_vcpu(vcpu, &self.msrs))
            .collect::<io::Result<_>>()?;
        Ok((states, snapshot::save_vm(vm_fd)?))
    }

    pub fn restore(
        &self,
        vm_fd: &VmFd,
        vcpus: &[VcpuFd],
        states: &[VcpuState],
        irqchip: &IrqchipState,
    ) -> io::Result<()> {
        for state in states {
            snapshot::check_cpu(&state.cpuid, &self.cpuid)?;
        }
        snapshot::restore_vm(vm_fd, irqchip)?;
        for (vcpu, state) in vcpus.iter().zip(states) {
            snapshot::restore_vcpu(vcpu, state)?;
        }
        Ok(())
    }

    pub fn restored(&self, vm_fd: &VmFd) -> io::Result<()> {
        VmGenId::notify(vm_fd)
    }
}

// ===========================================================================
// Boot
// ===========================================================================

/// Loads the kernel (an uncompressed ELF vmlinux) and the initrd, writes
/// the zero page, the command line and the ACPI tables, and sets up every
/// vCPU. Only the boot vCPU gets registers: the others wait in KVM for the
/// startup IPI.
#[allow(clippy::too_many_arguments)]
pub fn boot(
    kvm: &Kvm,
    _vm_fd: &VmFd,
    mem: &GuestMemoryMmap,
    vcpus: &[VcpuFd],
    kernel: &mut File,
    initrd: Option<File>,
    command_line: &str,
    memory_bytes: u64,
    virtio_count: u32,
) -> Result<(), VzError> {
    if vcpus.len() > acpi::MAX_CPUS {
        return Err(VzError::new(format!(
            "at most {} vCPUs on x86_64",
            acpi::MAX_CPUS
        )));
    }
    if VIRTIO_IRQ_BASE + virtio_count > IOAPIC_PINS {
        return Err(VzError::new(format!(
            "at most {} virtio devices on x86_64",
            IOAPIC_PINS - VIRTIO_IRQ_BASE
        )));
    }

    let loaded = Elf::load(mem, None, kernel, Some(GuestAddress(HIMEM_START)))
        .map_err(|e| VzError::new(format!("failed to load kernel: {e}")))?;
    let low_end = memory_bytes.min(MMIO_HOLE_START);
    let initrd = match initrd {
        Some(mut initrd) => {
            let mut data = Vec::new();
            initrd
                .read_to_end(&mut data)
                .map_err(|e| VzError::new(format!("failed to read initrd: {e}")))?;
            let at = (low_end - data.len() as u64) & !0xfff;
            if at < loaded.kernel_end {
                return Err(VzError::new("the initrd does not fit below the kernel"));
            }
            mem.write_slice(&data, GuestAddress(at))
                .map_err(|e| VzError::new(format!("failed to write initrd: {e}")))?;
            Some((at, data.len() as u64))
        }
        None => None,
    };

    let command_line = full_command_line(command_line)?;
    mem.write_slice(command_line.as_bytes(), GuestAddress(CMDLINE_START))
        .and_then(|()| mem.write_obj(0u8, GuestAddress(CMDLINE_START + command_line.len() as u64)))
        .map_err(|e| VzError::new(format!("failed to write the command line: {e}")))?;

    let params = zero_page(command_line.len(), initrd, memory_bytes);
    mem.write_obj(params, GuestAddress(ZERO_PAGE_START))
        .map_err(|e| VzError::new(format!("failed to write the zero page: {e}")))?;
    mem.write_slice(
        &acpi::tables(vcpus.len(), virtio_count),
        GuestAddress(ACPI_START),
    )
    .map_err(|e| VzError::new(format!("failed to write the ACPI tables: {e}")))?;

    let supported = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VzError::new(format!("KVM_GET_SUPPORTED_CPUID: {e}")))?;
    for (i, vcpu) in vcpus.iter().enumerate() {
        let cpuid = cpu::cpuid_for(&supported, i as u32, vcpus.len() as u32)?;
        vcpu.set_cpuid2(&cpuid)
            .map_err(|e| VzError::new(format!("KVM_SET_CPUID2: {e}")))?;
        cpu::set_msrs(vcpu)?;
        cpu::set_lint(vcpu)?;
    }
    cpu::set_boot_registers(&vcpus[0], mem, loaded.kernel_load.raw_value())
}

/// The caller's arguments, then the reset that stands in for power off,
/// and no probing for PCI, which the kernel has only for ACPI.
fn full_command_line(base: &str) -> Result<String, VzError> {
    let line = format!("{base} reboot=k pci=off 8250.nr_uarts=1");
    if line.len() >= CMDLINE_MAX {
        return Err(VzError::new(format!(
            "the kernel command line is {} bytes, the most is {}",
            line.len(),
            CMDLINE_MAX - 1
        )));
    }
    Ok(line)
}

const E820_RAM: u32 = 1;
const BOOT_FLAG: u16 = 0xaa55;
const HDRS_MAGIC: u32 = 0x5372_6448;
const LOADER_UNDEFINED: u8 = 0xff;
const KERNEL_ALIGNMENT: u32 = 0x0100_0000;

fn zero_page(
    command_line_len: usize,
    initrd: Option<(u64, u64)>,
    memory_bytes: u64,
) -> boot_params {
    let mut params = boot_params::default();
    params.hdr.type_of_loader = LOADER_UNDEFINED;
    params.hdr.boot_flag = BOOT_FLAG;
    params.hdr.header = HDRS_MAGIC;
    params.hdr.kernel_alignment = KERNEL_ALIGNMENT;
    params.hdr.cmd_line_ptr = CMDLINE_START as u32;
    params.hdr.cmdline_size = command_line_len as u32;
    params.acpi_rsdp_addr = ACPI_START;
    if let Some((at, size)) = initrd {
        params.hdr.ramdisk_image = at as u32;
        params.hdr.ramdisk_size = size as u32;
    }

    let mut ram = vec![(0, EBDA_START)];
    for (start, len) in memory_ranges(memory_bytes) {
        let start = start.max(HIMEM_START);
        let end = if start == HIMEM_START {
            len
        } else {
            start + len
        };
        if end > start {
            ram.push((start, end - start));
        }
    }
    for (i, &(addr, size)) in ram.iter().enumerate() {
        params.e820_table[i] = boot_e820_entry {
            addr,
            size,
            type_: E820_RAM,
        };
    }
    params.e820_entries = ram.len() as u8;
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_splits_at_the_hole() {
        assert_eq!(memory_ranges(2 << 30), vec![(0, 2 << 30)]);
        assert_eq!(
            memory_ranges(4 << 30),
            vec![
                (0, MMIO_HOLE_START),
                (HIGH_RAM_START, (4 << 30) - MMIO_HOLE_START)
            ]
        );
    }

    #[test]
    fn e820_skips_low_holes() {
        let params = zero_page(10, None, 4 << 30);
        let table: Vec<_> = params.e820_table[..params.e820_entries as usize]
            .iter()
            .map(|e| (e.addr, e.size))
            .collect();
        assert_eq!(
            table,
            vec![
                (0, EBDA_START),
                (HIMEM_START, MMIO_HOLE_START - HIMEM_START),
                (HIGH_RAM_START, (4 << 30) - MMIO_HOLE_START),
            ]
        );
    }

    #[test]
    fn command_line_ends_in_the_reset() {
        let line = full_command_line("console=ttyS0").unwrap();
        assert_eq!(line, "console=ttyS0 reboot=k pci=off 8250.nr_uarts=1");
    }
}
