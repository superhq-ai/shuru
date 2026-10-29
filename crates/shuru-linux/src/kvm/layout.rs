//! aarch64 memory map for the KVM virtual machine.
//!
//! Below DRAM_BASE is MMIO space for devices.
//! DRAM starts at 1 GiB and extends for the configured memory size.

// RAM
pub const DRAM_BASE: u64 = 0x4000_0000; // 1 GiB

// GIC v3
pub const GIC_DIST_BASE: u64 = 0x0800_0000;
pub const GIC_DIST_SIZE: u64 = 0x1_0000;
pub const GIC_REDIST_BASE: u64 = 0x080A_0000;
pub const GIC_REDIST_SIZE_PER_CPU: u64 = 0x2_0000;

// PL011 UART
pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 0x1000;
pub const UART_SPI: u32 = 1;

// PL031 RTC - provides wall clock at boot so guests start with correct time
// (required for TLS cert validation; without it the guest clock sits at Unix epoch).
pub const RTC_BASE: u64 = 0x0901_0000;
pub const RTC_SIZE: u64 = 0x1000;

// The 16 byte VM generation ID, in a page of its own.
pub const VMGENID_BASE: u64 = 0x0902_0000;
pub const VMGENID_SIZE: u64 = 0x1000;
pub const VMGENID_SPI: u32 = 2;

// Virtio MMIO devices
pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_MMIO_SIZE: u64 = 0x200;
pub const VIRTIO_MMIO_GAP: u64 = 0x200;
pub const VIRTIO_SPI_BASE: u32 = 16;

// IRQ helpers
pub const SPI_OFFSET: u32 = 32;

// KVM ARM IRQ encoding for KVM_IRQ_LINE:
//   bits[24..27] = type (0=CPU/SGI, 1=SPI, 2=PPI)
//   bits[0..9]   = intid (for SPI: SPI_number + 32)
const KVM_ARM_IRQ_TYPE_SPI: u32 = 1 << 24;

pub const fn spi_to_irq(spi: u32) -> u32 {
    KVM_ARM_IRQ_TYPE_SPI | (spi + SPI_OFFSET)
}

// FDT is placed at the top of RAM, page-aligned.
pub const FDT_MAX_SIZE: u64 = 0x20_0000;

// KVM register encoding for aarch64
pub const fn arm64_core_reg(offset_bytes: u64) -> u64 {
    0x6030_0000_0010_0000u64 | (offset_bytes / 4)
}

pub const REG_X0: u64 = arm64_core_reg(0);
pub const REG_PC: u64 = arm64_core_reg(256);
pub const REG_PSTATE: u64 = arm64_core_reg(264);

/// EL1h with DAIF masked
pub const PSTATE_FAULT_BITS_64: u64 = 0x3c5;

// KVM GIC constants (hand-defined because kvm-bindings uses these from the
// crate but we also reference them directly for GIC setup)
pub const KVM_ARM_VCPU_PSCI_0_2: u32 = 2;
pub const KVM_ARM_VCPU_POWER_OFF: u32 = 0;
