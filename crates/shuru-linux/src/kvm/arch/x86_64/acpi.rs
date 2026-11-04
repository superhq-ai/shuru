//! The ACPI tables of a hardware-reduced platform: the CPUs and the IOAPIC
//! in the MADT, and in the DSDT COM1, the virtio devices, the vmgenid
//! device and the generic event device that tells the guest its generation
//! changed. A hardware-reduced guest has no legacy PIC and maps no ISA
//! line by itself, so every line in use is a resource of a DSDT device.
//! The tables sit together from `ACPI_START`, where the kernel finds the
//! RSDP.

use acpi_tables::aml::{
    Arg, Device, EISAName, Equal, If, Interrupt, Memory32Fixed, Method, Name, Notify, Package,
    Path, ResourceTemplate, IO,
};
use acpi_tables::fadt::{FADTBuilder, Flags};
use acpi_tables::rsdp::Rsdp;
use acpi_tables::sdt::Sdt;
use acpi_tables::xsdt::XSDT;
use acpi_tables::Aml;

use super::layout::*;
use super::virtio_slot;

/// One local APIC entry each, with IDs below the 255 of the xAPIC.
pub const MAX_CPUS: usize = 32;

const OEM_ID: [u8; 6] = *b"SHURU ";
const OEM_REVISION: u32 = 1;

// IA-PC boot architecture flags: no VGA, no MSI, no ASPM, no CMOS RTC.
const IAPC_BOOT_ARCH: u16 = 1 << 2 | 1 << 3 | 1 << 4 | 1 << 5;

// MADT entries.
const MADT_LOCAL_APIC: u8 = 0;
const MADT_IO_APIC: u8 = 1;
const MADT_LOCAL_APIC_NMI: u8 = 4;
const LOCAL_APIC_ENABLED: u32 = 1;
const ALL_CPUS: u8 = 0xff;
const LINT1: u8 = 1;

const NOTIFY_DEVICE_CHECK: u8 = 0x80;

/// The tables, from the RSDP at `ACPI_START` on.
pub fn tables(cpu_count: usize, virtio_count: u32) -> Vec<u8> {
    let mut out = vec![0; Rsdp::len()];
    let mut place = |table: &dyn Aml| {
        let at = ACPI_START + out.len() as u64;
        table.to_aml_bytes(&mut out);
        // Tables start on 8 bytes.
        out.resize(out.len().next_multiple_of(8), 0);
        at
    };
    let dsdt = place(&dsdt(virtio_count));
    let fadt = place(&fadt(dsdt));
    let madt = place(&madt(cpu_count));
    let mut xsdt = XSDT::new(OEM_ID, *b"SHURXSDT", OEM_REVISION);
    xsdt.add_entry(fadt);
    xsdt.add_entry(madt);
    let xsdt = place(&xsdt);
    let mut rsdp = Vec::new();
    Rsdp::new(OEM_ID, xsdt).to_aml_bytes(&mut rsdp);
    out[..rsdp.len()].copy_from_slice(&rsdp);
    out
}

fn fadt(dsdt: u64) -> impl Aml {
    let mut fadt = FADTBuilder::new(OEM_ID, *b"SHURFACP", OEM_REVISION)
        .dsdt_64(dsdt)
        .flag(Flags::HwReducedAcpi);
    fadt.iapc_boot_arch = IAPC_BOOT_ARCH.into();
    fadt.hypervisor_vendor_identity = u64::from_le_bytes(*b"SHURUKVM").into();
    fadt.finalize()
}

/// A local APIC per vCPU, its ID the vCPU index, and the IOAPIC.
fn madt(cpu_count: usize) -> Sdt {
    let mut madt = Sdt::new(*b"APIC", 36, 4, OEM_ID, *b"SHURAPIC", OEM_REVISION);
    let mut entries = Vec::new();
    entries.extend(APIC_ADDR.to_le_bytes());
    // No flags: the 8259 PICs are left masked.
    entries.extend(0u32.to_le_bytes());
    for cpu in 0..cpu_count as u8 {
        entries.extend([MADT_LOCAL_APIC, 8, cpu, cpu]);
        entries.extend(LOCAL_APIC_ENABLED.to_le_bytes());
    }
    entries.extend([MADT_IO_APIC, 12, 0, 0]);
    entries.extend(IOAPIC_ADDR.to_le_bytes());
    entries.extend(0u32.to_le_bytes());
    entries.extend([MADT_LOCAL_APIC_NMI, 6, ALL_CPUS, 0, 0, LINT1]);
    madt.append_slice(&entries);
    madt
}

fn dsdt(virtio_count: u32) -> Sdt {
    let mut aml = Vec::new();
    Device::new(
        Path::new("_SB_.COM1"),
        vec![
            &Name::new(Path::new("_HID"), &EISAName::new("PNP0501")),
            &Name::new(
                Path::new("_CRS"),
                &ResourceTemplate::new(vec![
                    &IO::new(
                        SERIAL_PORT as u16,
                        SERIAL_PORT as u16,
                        1,
                        SERIAL_PORT_SIZE as u8,
                    ),
                    &Interrupt::new(true, true, false, false, SERIAL_IRQ),
                ]),
            ),
        ],
    )
    .to_aml_bytes(&mut aml);
    for index in 0..virtio_count {
        virtio_device(index).to_aml_bytes(&mut aml);
    }
    let addr = Package::new(vec![&(VMGENID_BASE as u32), &((VMGENID_BASE >> 32) as u32)]);
    Device::new(
        Path::new("_SB_.VGEN"),
        vec![
            &Name::new(Path::new("_HID"), &"VMGENCTR"),
            &Name::new(Path::new("_CID"), &"VM_Gen_Counter"),
            &Name::new(Path::new("_DDN"), &"VM_Gen_Counter"),
            &Name::new(Path::new("ADDR"), &addr),
        ],
    )
    .to_aml_bytes(&mut aml);
    Device::new(
        Path::new("_SB_.GED_"),
        vec![
            &Name::new(Path::new("_HID"), &"ACPI0013"),
            &Name::new(
                Path::new("_CRS"),
                &ResourceTemplate::new(vec![&Interrupt::new(
                    true,
                    true,
                    false,
                    false,
                    VMGENID_IRQ,
                )]),
            ),
            &Method::new(
                Path::new("_EVT"),
                1,
                true,
                vec![&If::new(
                    &Equal::new(&Arg(0), &VMGENID_IRQ),
                    vec![&Notify::new(
                        &Path::new("\\_SB_.VGEN"),
                        &NOTIFY_DEVICE_CHECK,
                    )],
                )],
            ),
        ],
    )
    .to_aml_bytes(&mut aml);
    let mut dsdt = Sdt::new(*b"DSDT", 36, 2, OEM_ID, *b"SHURDSDT", OEM_REVISION);
    dsdt.append_slice(&aml);
    dsdt
}

/// A virtio-mmio device the kernel's virtio_mmio driver claims by its ID.
fn virtio_device(index: u32) -> impl Aml {
    let (base, size, irq) = virtio_slot(index);
    VirtioDevice {
        name: format!("_SB_.V{index:03X}"),
        index,
        memory: Memory32Fixed::new(true, base as u32, size as u32),
        interrupt: Interrupt::new(true, false, false, false, irq),
    }
}

struct VirtioDevice {
    name: String,
    index: u32,
    memory: Memory32Fixed,
    interrupt: Interrupt,
}

impl Aml for VirtioDevice {
    fn to_aml_bytes(&self, sink: &mut dyn acpi_tables::AmlSink) {
        Device::new(
            Path::new(&self.name),
            vec![
                &Name::new(Path::new("_HID"), &"LNRO0005"),
                &Name::new(Path::new("_UID"), &self.index),
                &Name::new(
                    Path::new("_CRS"),
                    &ResourceTemplate::new(vec![&self.memory, &self.interrupt]),
                ),
            ],
        )
        .to_aml_bytes(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(bytes: &[u8]) -> u8 {
        bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b))
    }

    fn table(bytes: &[u8], at: u64) -> &[u8] {
        let offset = (at - ACPI_START) as usize;
        let len = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        &bytes[offset..offset + len as usize]
    }

    #[test]
    fn tables_link_up_and_check_out() {
        let bytes = tables(MAX_CPUS, IOAPIC_PINS - VIRTIO_IRQ_BASE);
        assert!(ACPI_START + bytes.len() as u64 <= HIMEM_START);
        assert_eq!(&bytes[..8], b"RSD PTR ");
        assert_eq!(sum(&bytes[..20]), 0);
        assert_eq!(sum(&bytes[..Rsdp::len()]), 0);
        let xsdt_at = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let xsdt = table(&bytes, xsdt_at);
        assert_eq!(&xsdt[..4], b"XSDT");
        assert_eq!(sum(xsdt), 0);
        let mut signatures = Vec::new();
        for entry in xsdt[36..].chunks(8) {
            let entry = table(&bytes, u64::from_le_bytes(entry.try_into().unwrap()));
            assert_eq!(sum(entry), 0);
            signatures.push(entry[..4].to_vec());
            if &entry[..4] == b"FACP" {
                let dsdt_at = u64::from_le_bytes(entry[140..148].try_into().unwrap());
                let dsdt = table(&bytes, dsdt_at);
                assert_eq!(&dsdt[..4], b"DSDT");
                assert_eq!(sum(dsdt), 0);
            }
        }
        assert_eq!(signatures, [b"FACP".to_vec(), b"APIC".to_vec()]);
    }

    #[test]
    fn madt_lists_every_cpu() {
        let madt = madt(4);
        let mut entries = &madt.as_slice()[44..];
        let mut kinds = Vec::new();
        while let [kind, len, ..] = *entries {
            kinds.push(kind);
            entries = &entries[len as usize..];
        }
        assert_eq!(kinds, [0, 0, 0, 0, MADT_IO_APIC, MADT_LOCAL_APIC_NMI]);
    }
}
