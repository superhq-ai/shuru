//! x86_64 memory map for the KVM virtual machine.
//!
//! RAM starts at 0. Above 3 GiB is the hole for the virtio devices, the
//! IOAPIC and the local APICs; RAM past 3 GiB continues at 4 GiB.

pub const DRAM_BASE: u64 = 0;
pub const MMIO_HOLE_START: u64 = 0xc000_0000;
pub const HIGH_RAM_START: u64 = 0x1_0000_0000;

// Boot structures in low memory.
pub const GDT_START: u64 = 0x500;
pub const IDT_START: u64 = 0x520;
pub const ZERO_PAGE_START: u64 = 0x7000;
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
pub const PML4_START: u64 = 0x9000;
pub const PDPTE_START: u64 = 0xa000;
pub const PDE_START: u64 = 0xb000;
pub const CMDLINE_START: u64 = 0x2_0000;
/// The kernel's COMMAND_LINE_SIZE on x86, with the terminating zero.
pub const CMDLINE_MAX: usize = 2048;
/// The extended BIOS data area. Low RAM ends here.
pub const EBDA_START: u64 = 0x9_fc00;
/// The ACPI tables, from the RSDP on, in the BIOS area below 1 MiB, where
/// the kernel looks for the RSDP.
pub const ACPI_START: u64 = 0xe_0000;
pub const HIMEM_START: u64 = 0x10_0000;

// The vmgenid page, read only, in the hole below the virtio devices. KVM
// slots 0 and 1 are RAM.
pub const VMGENID_BASE: u64 = 0xcfff_f000;
pub const VMGENID_SIZE: u64 = 0x1000;
pub const VMGENID_SLOT: u32 = 2;

// Virtio MMIO devices, a page each, in the hole.
pub const VIRTIO_MMIO_BASE: u64 = 0xd000_0000;
pub const VIRTIO_MMIO_SIZE: u64 = 0x1000;

pub const IOAPIC_ADDR: u32 = 0xfec0_0000;
pub const APIC_ADDR: u32 = 0xfee0_0000;
// Pages KVM needs on Intel, at the top of the hole.
pub const IDENTITY_MAP_ADDR: u64 = 0xfffb_c000;
pub const TSS_ADDR: u64 = 0xfffb_d000;

// Interrupt lines on the IOAPIC.
pub const IOAPIC_PINS: u32 = 24;
/// The ACPI generic event device, which the guest turns into a vmgenid
/// notify. Edge triggered, as COM2 would be.
pub const VMGENID_IRQ: u32 = 3;
pub const SERIAL_IRQ: u32 = 4;
pub const VIRTIO_IRQ_BASE: u32 = 5;

// COM1.
pub const SERIAL_PORT: u64 = 0x3f8;
pub const SERIAL_PORT_SIZE: u64 = 8;
// The keyboard controller command port; 0xfe there resets the machine.
pub const I8042_COMMAND_PORT: u16 = 0x64;
pub const I8042_RESET: u8 = 0xfe;
