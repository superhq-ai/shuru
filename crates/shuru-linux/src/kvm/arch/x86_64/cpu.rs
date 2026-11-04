//! vCPU setup for the 64-bit boot protocol: the CPUID the guest sees, the
//! MSRs, the local APIC wiring, and for the boot vCPU long mode with the
//! first GiB identity mapped, the kernel entry and the zero page in rsi.

use kvm_bindings::{
    kvm_cpuid_entry2, kvm_fpu, kvm_lapic_state, kvm_msr_entry, kvm_regs, kvm_segment, CpuId, Msrs,
    KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
};
use kvm_ioctls::VcpuFd;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::layout::*;
use crate::error::VzError;

fn kvm_error(what: &str) -> impl Fn(kvm_ioctls::Error) -> VzError + '_ {
    move |e| VzError::new(format!("{what}: {e}"))
}

// ===========================================================================
// CPUID
// ===========================================================================

const LEAF_FEATURES: u32 = 1;
const LEAF_CACHE: u32 = 4;
const LEAF_TOPOLOGY: u32 = 0xb;
const LEAF_TOPOLOGY_V2: u32 = 0x1f;
const LEAF_AMD_SIZES: u32 = 0x8000_0008;
const LEAF_AMD_TOPOLOGY: u32 = 0x8000_001e;

const FEATURES_ECX_HYPERVISOR: u32 = 1 << 31;
const FEATURES_EDX_HTT: u32 = 1 << 28;

const LEVEL_SMT: u32 = 1;
const LEVEL_CORE: u32 = 2;

/// Bits of the APIC ID that number `count` CPUs.
fn id_bits(count: u32) -> u32 {
    u32::BITS - count.saturating_sub(1).leading_zeros()
}

/// What KVM offers, with the topology of `cpu_count` single-thread cores
/// in one package and `cpu` as the APIC ID.
pub fn cpuid_for(supported: &CpuId, cpu: u32, cpu_count: u32) -> Result<CpuId, VzError> {
    let mut cpuid = supported.clone();
    let bits = id_bits(cpu_count);
    for entry in cpuid.as_mut_slice() {
        match entry.function {
            LEAF_FEATURES => {
                entry.ebx = (entry.ebx & 0x0000_ffff) | (cpu << 24) | (cpu_count.min(255) << 16);
                entry.ecx |= FEATURES_ECX_HYPERVISOR;
                if cpu_count > 1 {
                    entry.edx |= FEATURES_EDX_HTT;
                } else {
                    entry.edx &= !FEATURES_EDX_HTT;
                }
            }
            LEAF_CACHE => {
                // Cores in the package, and the threads sharing this cache:
                // all of them for L3, one below.
                let level = (entry.eax >> 5) & 0x7;
                let sharing = if level >= 3 { cpu_count - 1 } else { 0 };
                entry.eax = (entry.eax & 0x3fff) | (sharing << 14) | ((cpu_count - 1) << 26);
            }
            LEAF_AMD_SIZES => {
                entry.ecx = (entry.ecx & !0xf0ff) | (bits << 12) | (cpu_count - 1);
            }
            LEAF_AMD_TOPOLOGY => {
                entry.eax = cpu;
                entry.ebx = cpu & 0xff;
                entry.ecx = 0;
            }
            _ => {}
        }
    }
    // KVM passes on what the host reports, which on a nested host can stop
    // at the thread level, and the guest then puts each CPU in a package of
    // its own. So the extended topology leaves are written out whole.
    for leaf in [LEAF_TOPOLOGY, LEAF_TOPOLOGY_V2] {
        if !supported.as_slice().iter().any(|e| e.function == leaf) {
            continue;
        }
        cpuid.retain(|e| e.function != leaf);
        for entry in topology(leaf, cpu, cpu_count) {
            cpuid
                .push(entry)
                .map_err(|e| VzError::new(format!("CPUID list: {e:?}")))?;
        }
    }
    Ok(cpuid)
}

/// The SMT and core levels of an extended topology leaf, then the
/// terminating invalid level.
fn topology(leaf: u32, cpu: u32, cpu_count: u32) -> [kvm_cpuid_entry2; 3] {
    let level = |index: u32, eax: u32, ebx: u32, kind: u32| kvm_cpuid_entry2 {
        function: leaf,
        index,
        flags: KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
        eax,
        ebx,
        ecx: (kind << 8) | index,
        edx: cpu,
        ..Default::default()
    };
    [
        level(0, 0, 1, LEVEL_SMT),
        level(1, id_bits(cpu_count), cpu_count, LEVEL_CORE),
        level(2, 0, 0, 0),
    ]
}

// ===========================================================================
// MSRs and the local APIC
// ===========================================================================

const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

const MISC_ENABLE_FAST_STRING: u64 = 1;
// MTRRs on, memory write-back unless a range says otherwise.
const MTRR_ENABLE_WRITE_BACK: u64 = (1 << 11) | 6;

pub fn set_msrs(vcpu: &VcpuFd) -> Result<(), VzError> {
    let entries = [
        (MSR_IA32_SYSENTER_CS, 0),
        (MSR_IA32_SYSENTER_ESP, 0),
        (MSR_IA32_SYSENTER_EIP, 0),
        (MSR_STAR, 0),
        (MSR_CSTAR, 0),
        (MSR_KERNEL_GS_BASE, 0),
        (MSR_SYSCALL_MASK, 0),
        (MSR_LSTAR, 0),
        (MSR_IA32_TSC, 0),
        (MSR_IA32_MISC_ENABLE, MISC_ENABLE_FAST_STRING),
        (MSR_MTRR_DEF_TYPE, MTRR_ENABLE_WRITE_BACK),
    ]
    .map(|(index, data)| kvm_msr_entry {
        index,
        data,
        ..Default::default()
    });
    let msrs =
        Msrs::from_entries(&entries).map_err(|e| VzError::new(format!("MSR list: {e:?}")))?;
    let set = vcpu.set_msrs(&msrs).map_err(kvm_error("KVM_SET_MSRS"))?;
    if set != entries.len() {
        return Err(VzError::new(format!(
            "KVM_SET_MSRS: MSR {:#x} was refused",
            entries[set].index
        )));
    }
    Ok(())
}

const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

fn lapic_reg(lapic: &kvm_lapic_state, offset: usize) -> u32 {
    let bytes: [u8; 4] = std::array::from_fn(|i| lapic.regs[offset + i] as u8);
    u32::from_le_bytes(bytes)
}

fn set_lapic_reg(lapic: &mut kvm_lapic_state, offset: usize, value: u32) {
    for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
        lapic.regs[offset + i] = byte as _;
    }
}

/// LINT0 takes the legacy PIC as an external interrupt, LINT1 the NMI.
pub fn set_lint(vcpu: &VcpuFd) -> Result<(), VzError> {
    let mut lapic = vcpu.get_lapic().map_err(kvm_error("KVM_GET_LAPIC"))?;
    for (offset, mode) in [(APIC_LVT0, APIC_MODE_EXTINT), (APIC_LVT1, APIC_MODE_NMI)] {
        let value = lapic_reg(&lapic, offset);
        set_lapic_reg(&mut lapic, offset, (value & !0x700) | (mode << 8));
    }
    vcpu.set_lapic(&lapic).map_err(kvm_error("KVM_SET_LAPIC"))
}

// ===========================================================================
// Long mode for the boot vCPU
// ===========================================================================

const CR0_PE: u64 = 1;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

const PAGE_PRESENT_WRITABLE: u64 = 0x3;
const PAGE_HUGE: u64 = 0x80;

/// A GDT descriptor from its flags, base and limit.
const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((base as u64 & 0xff00_0000) << 32)
        | ((flags as u64 & 0xf0ff) << 40)
        | ((limit as u64 & 0x000f_0000) << 32)
        | ((base as u64 & 0x00ff_ffff) << 16)
        | (limit as u64 & 0x0000_ffff)
}

const GDT: [u64; 4] = [
    0,
    gdt_entry(0xa09b, 0, 0xfffff), // 64-bit code
    gdt_entry(0xc093, 0, 0xfffff), // data
    gdt_entry(0x808b, 0, 0xfffff), // TSS
];

/// The segment register form of GDT entry `index`.
fn segment(index: usize) -> kvm_segment {
    let entry = GDT[index];
    let bit = |n: u32| ((entry >> n) & 1) as u8;
    let g = bit(55);
    let limit = ((entry & 0xffff) | (((entry >> 48) & 0xf) << 16)) as u32;
    let present = bit(47);
    kvm_segment {
        base: ((entry >> 16) & 0xff_ffff) | ((entry >> 56) << 24),
        limit: if g == 1 { (limit << 12) | 0xfff } else { limit },
        selector: (index * 8) as u16,
        type_: ((entry >> 40) & 0xf) as u8,
        present,
        dpl: ((entry >> 45) & 0x3) as u8,
        db: bit(54),
        s: bit(44),
        l: bit(53),
        g,
        avl: bit(52),
        unusable: u8::from(present == 0),
        padding: 0,
    }
}

fn write(mem: &GuestMemoryMmap, at: u64, value: u64) -> Result<(), VzError> {
    mem.write_obj(value, GuestAddress(at))
        .map_err(|e| VzError::new(format!("failed to write boot tables: {e}")))
}

/// Puts the boot vCPU in long mode at `entry`, the first GiB identity
/// mapped with 2 MiB pages, as the kernel's 64-bit entry expects.
pub fn set_boot_registers(vcpu: &VcpuFd, mem: &GuestMemoryMmap, entry: u64) -> Result<(), VzError> {
    for (i, descriptor) in GDT.iter().enumerate() {
        write(mem, GDT_START + i as u64 * 8, *descriptor)?;
    }
    write(mem, IDT_START, 0)?;
    write(mem, PML4_START, PDPTE_START | PAGE_PRESENT_WRITABLE)?;
    write(mem, PDPTE_START, PDE_START | PAGE_PRESENT_WRITABLE)?;
    for i in 0..512u64 {
        write(
            mem,
            PDE_START + i * 8,
            (i << 21) | PAGE_HUGE | PAGE_PRESENT_WRITABLE,
        )?;
    }

    let mut sregs = vcpu.get_sregs().map_err(kvm_error("KVM_GET_SREGS"))?;
    sregs.gdt.base = GDT_START;
    sregs.gdt.limit = (GDT.len() * 8 - 1) as u16;
    sregs.idt.base = IDT_START;
    sregs.idt.limit = 7;
    sregs.cs = segment(1);
    let data = segment(2);
    (sregs.ds, sregs.es, sregs.fs, sregs.gs, sregs.ss) = (data, data, data, data, data);
    sregs.tr = segment(3);
    sregs.cr3 = PML4_START;
    sregs.cr4 |= CR4_PAE;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
    vcpu.set_sregs(&sregs).map_err(kvm_error("KVM_SET_SREGS"))?;

    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu).map_err(kvm_error("KVM_SET_FPU"))?;

    let regs = kvm_regs {
        rflags: 0x2,
        rip: entry,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi: ZERO_PAGE_START,
        ..Default::default()
    };
    vcpu.set_regs(&regs).map_err(kvm_error("KVM_SET_REGS"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdt_segments() {
        let code = segment(1);
        assert_eq!(
            (code.selector, code.type_, code.l, code.db),
            (0x08, 0xb, 1, 0)
        );
        assert_eq!(
            (code.present, code.s, code.g, code.limit),
            (1, 1, 1, 0xffff_ffff)
        );
        let data = segment(2);
        assert_eq!(
            (data.selector, data.type_, data.l, data.db),
            (0x10, 0x3, 0, 1)
        );
        let tss = segment(3);
        assert_eq!((tss.selector, tss.type_, tss.s), (0x18, 0xb, 0));
        assert_eq!(GDT[1], 0x00af_9b00_0000_ffff);
    }

    #[test]
    fn topology_is_one_package_of_single_thread_cores() {
        let host = |function, index, ecx| kvm_cpuid_entry2 {
            function,
            index,
            ecx,
            ebx: 1,
            ..Default::default()
        };
        // A nested host that reports the thread level only.
        let supported = CpuId::from_entries(&[
            host(LEAF_FEATURES, 0, 0),
            host(LEAF_TOPOLOGY, 0, LEVEL_SMT << 8),
        ])
        .unwrap();
        let cpuid = cpuid_for(&supported, 2, 4).unwrap();
        let leaf = |index| {
            *cpuid
                .as_slice()
                .iter()
                .find(|e| e.function == LEAF_TOPOLOGY && e.index == index)
                .unwrap()
        };
        let (smt, core, end) = (leaf(0), leaf(1), leaf(2));
        assert_eq!(
            (smt.eax, smt.ebx, smt.ecx >> 8, smt.edx),
            (0, 1, LEVEL_SMT, 2)
        );
        assert_eq!(
            (core.eax, core.ebx, core.ecx >> 8, core.edx),
            (2, 4, LEVEL_CORE, 2)
        );
        assert_eq!(end.ecx >> 8, 0);
        assert!(!cpuid
            .as_slice()
            .iter()
            .any(|e| e.function == LEAF_TOPOLOGY_V2));
        let features = cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == LEAF_FEATURES)
            .unwrap();
        assert_eq!(features.ebx >> 24, 2);
        assert_ne!(features.ecx & FEATURES_ECX_HYPERVISOR, 0);
    }

    #[test]
    fn apic_id_bits() {
        assert_eq!(id_bits(1), 0);
        assert_eq!(id_bits(2), 1);
        assert_eq!(id_bits(3), 2);
        assert_eq!(id_bits(4), 2);
        assert_eq!(id_bits(16), 4);
        assert_eq!(id_bits(17), 5);
    }
}
