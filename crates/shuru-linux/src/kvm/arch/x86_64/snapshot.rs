//! The x86_64 part of a snapshot. Per vCPU: the CPUID the guest saw, the
//! TSC rate, the registers, the FPU and vector state, the local APIC, the
//! MSRs KVM migrates and the pending events. Per VM: the two PICs, the
//! IOAPIC, the PIT and kvmclock. The kernel structures are kept as their
//! bytes, as KVM hands them out.

use std::io;

use kvm_bindings::{
    kvm_clock_data, kvm_cpuid_entry2, kvm_debugregs, kvm_irqchip, kvm_lapic_state, kvm_mp_state,
    kvm_msr_entry, kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs, kvm_xsave,
    CpuId, Msrs, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE,
    KVM_MAX_CPUID_ENTRIES, KVM_MAX_MSR_ENTRIES,
};
use kvm_ioctls::{VcpuFd, VmFd};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct VcpuState {
    /// function, index, flags, eax, ebx, ecx, edx.
    pub cpuid: Vec<[u32; 7]>,
    pub tsc_khz: u32,
    pub mp_state: u32,
    pub regs: Vec<u8>,
    pub sregs: Vec<u8>,
    pub xsave: Vec<u8>,
    pub xcrs: Vec<u8>,
    pub debug_regs: Vec<u8>,
    pub lapic: Vec<u8>,
    pub msrs: Vec<(u32, u64)>,
    pub events: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct IrqchipState {
    pub pic_master: Vec<u8>,
    pub pic_slave: Vec<u8>,
    pub ioapic: Vec<u8>,
    pub pit: Vec<u8>,
    pub clock: Vec<u8>,
}

fn error(what: impl std::fmt::Display, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

fn bytes<T>(value: &T) -> Vec<u8> {
    // SAFETY: the KVM structures are plain data.
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }.to_vec()
}

fn from_bytes<T>(bytes: &[u8], what: &str) -> io::Result<T> {
    if bytes.len() != size_of::<T>() {
        return Err(io::Error::other(format!("{what} has the wrong size")));
    }
    // SAFETY: the length matches, the KVM structures are plain data and
    // all zeroes is a valid value of each.
    unsafe {
        let mut value: T = std::mem::zeroed();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), &mut value as *mut T as *mut u8, bytes.len());
        Ok(value)
    }
}

// ===========================================================================
// MSRs
// ===========================================================================

/// The MSRs of `list` that `vcpu` can read, with their values. KVM stops a
/// batch at the first MSR it cannot read, so the rest go in the next.
fn read_msrs(vcpu: &VcpuFd, list: &[u32]) -> io::Result<Vec<(u32, u64)>> {
    let mut values = Vec::with_capacity(list.len());
    let mut rest = list;
    while !rest.is_empty() {
        let batch = &rest[..rest.len().min(KVM_MAX_MSR_ENTRIES)];
        let entries: Vec<_> = batch
            .iter()
            .map(|&index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut msrs =
            Msrs::from_entries(&entries).map_err(|e| error("MSR list", format!("{e:?}")))?;
        let read = vcpu
            .get_msrs(&mut msrs)
            .map_err(|e| error("KVM_GET_MSRS", e))?;
        values.extend(msrs.as_slice()[..read].iter().map(|e| (e.index, e.data)));
        // The one after those read could not be, and is left out.
        rest = &rest[(read + 1).min(rest.len())..];
    }
    Ok(values)
}

fn write_msrs(vcpu: &VcpuFd, values: &[(u32, u64)]) -> io::Result<()> {
    for batch in values.chunks(KVM_MAX_MSR_ENTRIES) {
        let entries: Vec<_> = batch
            .iter()
            .map(|&(index, data)| kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        let msrs = Msrs::from_entries(&entries).map_err(|e| error("MSR list", format!("{e:?}")))?;
        let written = vcpu.set_msrs(&msrs).map_err(|e| error("KVM_SET_MSRS", e))?;
        if written != batch.len() {
            return Err(io::Error::other(format!(
                "KVM_SET_MSRS: MSR {:#x} was refused",
                batch[written].0
            )));
        }
    }
    Ok(())
}

// ===========================================================================
// CPUID
// ===========================================================================

fn cpuid_entries(cpuid: &CpuId) -> Vec<[u32; 7]> {
    cpuid
        .as_slice()
        .iter()
        .map(|e| [e.function, e.index, e.flags, e.eax, e.ebx, e.ecx, e.edx])
        .collect()
}

fn cpuid_from(entries: &[[u32; 7]]) -> io::Result<CpuId> {
    let entries: Vec<_> = entries
        .iter()
        .map(
            |&[function, index, flags, eax, ebx, ecx, edx]| kvm_cpuid_entry2 {
                function,
                index,
                flags,
                eax,
                ebx,
                ecx,
                edx,
                ..Default::default()
            },
        )
        .collect();
    CpuId::from_entries(&entries).map_err(|e| error("CPUID list", format!("{e:?}")))
}

/// The registers that say which features the guest may use, by leaf and
/// subleaf: eax, ebx, ecx, edx.
const FEATURE_LEAVES: [(u32, u32, [bool; 4]); 7] = [
    (0x1, 0, [false, false, true, true]),
    (0x7, 0, [false, true, true, true]),
    (0x7, 1, [true, false, false, true]),
    (0xd, 0, [true, false, false, true]),
    (0xd, 1, [true, false, false, false]),
    (0x8000_0001, 0, [false, false, true, true]),
    (0x8000_0008, 0, [false, true, false, false]),
];

/// Bits that are not features of the CPU: the hypervisor bit, and those
/// that mirror what the guest kernel turned on in CR4 (OSXSAVE, OSPKE).
fn not_features(function: u32, index: u32, register: usize) -> u32 {
    match (function, index, register) {
        (0x1, 0, 2) => 1 << 31 | 1 << 27,
        (0x7, 0, 2) => 1 << 4,
        _ => 0,
    }
}

fn leaf(entries: &[[u32; 7]], function: u32, index: u32) -> [u32; 4] {
    entries
        .iter()
        .find(|e| e[0] == function && e[1] == index)
        .map(|e| [e[3], e[4], e[5], e[6]])
        .unwrap_or_default()
}

/// Refuses a snapshot whose guest saw another CPU model or vendor, or a
/// feature this host cannot give it. Features this host has on top are
/// fine: the guest chose its code paths without them.
pub fn check_cpu(saved: &[[u32; 7]], here: &CpuId) -> io::Result<()> {
    let here = cpuid_entries(here);
    let other = |what: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the snapshot was taken on another CPU: {what}"),
        )
    };
    let (saved_vendor, here_vendor) = (leaf(saved, 0, 0), leaf(&here, 0, 0));
    if saved_vendor[1..] != here_vendor[1..] {
        return Err(other("the vendor differs".into()));
    }
    let (saved_model, here_model) = (leaf(saved, 1, 0)[0], leaf(&here, 1, 0)[0]);
    if saved_model != here_model {
        return Err(other(format!("the signature was {saved_model:#x}")));
    }
    for (function, index, registers) in FEATURE_LEAVES {
        let (saved, here) = (leaf(saved, function, index), leaf(&here, function, index));
        for (i, compared) in registers.into_iter().enumerate() {
            let missing = saved[i] & !here[i] & !not_features(function, index, i);
            if compared && missing != 0 {
                return Err(other(format!(
                    "leaf {function:#x}.{index} register {} had {missing:#x}",
                    ["eax", "ebx", "ecx", "edx"][i]
                )));
            }
        }
    }
    Ok(())
}

// ===========================================================================
// vCPU
// ===========================================================================

/// Saves a vCPU that is not running. The MP state goes first, since
/// reading it can change the local APIC, and the events last.
pub fn save_vcpu(vcpu: &VcpuFd, msr_list: &[u32]) -> io::Result<VcpuState> {
    let mp_state = vcpu
        .get_mp_state()
        .map_err(|e| error("KVM_GET_MP_STATE", e))?
        .mp_state;
    let regs = vcpu.get_regs().map_err(|e| error("KVM_GET_REGS", e))?;
    let sregs = vcpu.get_sregs().map_err(|e| error("KVM_GET_SREGS", e))?;
    let xsave = vcpu.get_xsave().map_err(|e| error("KVM_GET_XSAVE", e))?;
    let xcrs = vcpu.get_xcrs().map_err(|e| error("KVM_GET_XCRS", e))?;
    let debug_regs = vcpu
        .get_debug_regs()
        .map_err(|e| error("KVM_GET_DEBUGREGS", e))?;
    let lapic = vcpu.get_lapic().map_err(|e| error("KVM_GET_LAPIC", e))?;
    let tsc_khz = vcpu
        .get_tsc_khz()
        .map_err(|e| error("KVM_GET_TSC_KHZ", e))?;
    let cpuid = vcpu
        .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| error("KVM_GET_CPUID2", e))?;
    let msrs = read_msrs(vcpu, msr_list)?;
    let events = vcpu
        .get_vcpu_events()
        .map_err(|e| error("KVM_GET_VCPU_EVENTS", e))?;
    Ok(VcpuState {
        cpuid: cpuid_entries(&cpuid),
        tsc_khz,
        mp_state,
        regs: bytes(&regs),
        sregs: bytes(&sregs),
        xsave: bytes(&xsave),
        xcrs: bytes(&xcrs),
        debug_regs: bytes(&debug_regs),
        lapic: bytes(&lapic),
        msrs,
        events: bytes(&events),
    })
}

/// Restores a vCPU. The registers clear pending exceptions, so the events
/// come after them; the special registers set the APIC base, so the local
/// APIC comes after them; the TSC deadline MSR only takes with the local
/// APIC set up, so the MSRs come after it.
pub fn restore_vcpu(vcpu: &VcpuFd, state: &VcpuState) -> io::Result<()> {
    vcpu.set_cpuid2(&cpuid_from(&state.cpuid)?)
        .map_err(|e| error("KVM_SET_CPUID2", e))?;
    let here = vcpu
        .get_tsc_khz()
        .map_err(|e| error("KVM_GET_TSC_KHZ", e))?;
    if here != state.tsc_khz {
        vcpu.set_tsc_khz(state.tsc_khz).map_err(|e| {
            error(
                format_args!(
                    "the snapshot was taken with a TSC of {} kHz, this host has {here} kHz and cannot scale",
                    state.tsc_khz
                ),
                e,
            )
        })?;
    }
    vcpu.set_mp_state(kvm_mp_state {
        mp_state: state.mp_state,
    })
    .map_err(|e| error("KVM_SET_MP_STATE", e))?;
    vcpu.set_regs(&from_bytes::<kvm_regs>(&state.regs, "regs")?)
        .map_err(|e| error("KVM_SET_REGS", e))?;
    vcpu.set_sregs(&from_bytes::<kvm_sregs>(&state.sregs, "sregs")?)
        .map_err(|e| error("KVM_SET_SREGS", e))?;
    vcpu.set_xsave(&from_bytes::<kvm_xsave>(&state.xsave, "xsave")?)
        .map_err(|e| error("KVM_SET_XSAVE", e))?;
    vcpu.set_xcrs(&from_bytes::<kvm_xcrs>(&state.xcrs, "xcrs")?)
        .map_err(|e| error("KVM_SET_XCRS", e))?;
    vcpu.set_debug_regs(&from_bytes::<kvm_debugregs>(
        &state.debug_regs,
        "debug registers",
    )?)
    .map_err(|e| error("KVM_SET_DEBUGREGS", e))?;
    vcpu.set_lapic(&from_bytes::<kvm_lapic_state>(&state.lapic, "lapic")?)
        .map_err(|e| error("KVM_SET_LAPIC", e))?;
    write_msrs(vcpu, &state.msrs)?;
    vcpu.set_vcpu_events(&from_bytes::<kvm_vcpu_events>(&state.events, "events")?)
        .map_err(|e| error("KVM_SET_VCPU_EVENTS", e))?;
    Ok(())
}

// ===========================================================================
// Interrupt controllers, PIT and clock
// ===========================================================================

fn get_chip(vm_fd: &VmFd, chip_id: u32) -> io::Result<Vec<u8>> {
    let mut chip = kvm_irqchip {
        chip_id,
        ..Default::default()
    };
    vm_fd
        .get_irqchip(&mut chip)
        .map_err(|e| error(format_args!("KVM_GET_IRQCHIP {chip_id}"), e))?;
    Ok(bytes(&chip))
}

fn set_chip(vm_fd: &VmFd, saved: &[u8], chip_id: u32) -> io::Result<()> {
    let chip: kvm_irqchip = from_bytes(saved, "irqchip")?;
    if chip.chip_id != chip_id {
        return Err(io::Error::other("the irqchip state is out of order"));
    }
    vm_fd
        .set_irqchip(&chip)
        .map_err(|e| error(format_args!("KVM_SET_IRQCHIP {chip_id}"), e))
}

pub fn save_vm(vm_fd: &VmFd) -> io::Result<IrqchipState> {
    let pit = vm_fd.get_pit2().map_err(|e| error("KVM_GET_PIT2", e))?;
    let mut clock = vm_fd.get_clock().map_err(|e| error("KVM_GET_CLOCK", e))?;
    // What the flags report holds for this host only.
    clock.flags = 0;
    Ok(IrqchipState {
        pic_master: get_chip(vm_fd, KVM_IRQCHIP_PIC_MASTER)?,
        pic_slave: get_chip(vm_fd, KVM_IRQCHIP_PIC_SLAVE)?,
        ioapic: get_chip(vm_fd, KVM_IRQCHIP_IOAPIC)?,
        pit: bytes(&pit),
        clock: bytes(&clock),
    })
}

/// Restores the VM part before the vCPUs, so they find their interrupt
/// lines and clock as they were.
pub fn restore_vm(vm_fd: &VmFd, state: &IrqchipState) -> io::Result<()> {
    set_chip(vm_fd, &state.pic_master, KVM_IRQCHIP_PIC_MASTER)?;
    set_chip(vm_fd, &state.pic_slave, KVM_IRQCHIP_PIC_SLAVE)?;
    set_chip(vm_fd, &state.ioapic, KVM_IRQCHIP_IOAPIC)?;
    vm_fd
        .set_pit2(&from_bytes::<kvm_pit_state2>(&state.pit, "pit")?)
        .map_err(|e| error("KVM_SET_PIT2", e))?;
    vm_fd
        .set_clock(&from_bytes::<kvm_clock_data>(&state.clock, "clock")?)
        .map_err(|e| error("KVM_SET_CLOCK", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpuid(entries: &[[u32; 7]]) -> CpuId {
        cpuid_from(entries).unwrap()
    }

    const VENDOR: [u32; 7] = [0, 0, 0, 0x20, 0x756e_6547, 0x6c65_746e, 0x4965_6e69];

    #[test]
    fn cpu_check() {
        let saved = vec![
            VENDOR,
            [1, 0, 0, 0xa06d1, 0, 0x8800_0001, 0x10],
            [7, 0, 1, 0, 0x5, 0, 0],
        ];
        // More features here, and no hypervisor or OSXSAVE bit, is fine.
        let here = cpuid(&[
            VENDOR,
            [1, 0, 0, 0xa06d1, 0, 0x3, 0x30],
            [7, 0, 1, 0, 0x7, 0, 0],
        ]);
        check_cpu(&saved, &here).unwrap();
        let fewer = cpuid(&[
            VENDOR,
            [1, 0, 0, 0xa06d1, 0, 0x1, 0x10],
            [7, 0, 1, 0, 0x4, 0, 0],
        ]);
        assert!(check_cpu(&saved, &fewer).is_err());
        let model = cpuid(&[
            VENDOR,
            [1, 0, 0, 0xa06d2, 0, 0x1, 0x10],
            [7, 0, 1, 0, 0x5, 0, 0],
        ]);
        assert!(check_cpu(&saved, &model).is_err());
        let amd = cpuid(&[
            [0, 0, 0, 0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65],
            [1, 0, 0, 0xa06d1, 0, 0x1, 0x10],
            [7, 0, 1, 0, 0x5, 0, 0],
        ]);
        assert!(check_cpu(&saved, &amd).is_err());
    }

    #[test]
    fn plain_data_round_trips() {
        let regs = kvm_regs {
            rip: 0x1234,
            rflags: 2,
            ..Default::default()
        };
        let back: kvm_regs = from_bytes(&bytes(&regs), "regs").unwrap();
        assert_eq!((back.rip, back.rflags), (0x1234, 2));
        assert!(from_bytes::<kvm_regs>(&[0; 3], "regs").is_err());
    }
}
