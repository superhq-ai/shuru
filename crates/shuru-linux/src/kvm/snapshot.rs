//! What a paused VM is made of besides its memory and disk, and how a new
//! VM takes it back (RFC 0009): vCPU registers, the GICv3 and the devices.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use kvm_bindings::{
    kvm_device_attr, kvm_mp_state, kvm_vcpu_events, RegList, KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
    KVM_DEV_ARM_VGIC_GRP_DIST_REGS, KVM_DEV_ARM_VGIC_GRP_REDIST_REGS,
};
use kvm_ioctls::{DeviceFd, VcpuFd};
use serde::{Deserialize, Serialize};
use vm_memory::{
    FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestRegionMmap, MmapRegion,
};

use super::layout::DRAM_BASE;

pub const FORMAT_VERSION: u32 = 1;

/// Interrupt lines the GIC is created with: 32 private, the rest shared.
pub const NR_IRQS: u32 = 128;
const PRIVATE_IRQS: u32 = 32;

#[derive(Serialize, Deserialize)]
pub struct SnapshotState {
    pub version: u32,
    pub cpu_count: usize,
    pub memory_bytes: u64,
    pub vcpus: Vec<VcpuState>,
    pub gic: GicState,
    /// One entry per device on the MMIO bus, in bus order.
    pub devices: Vec<DeviceState>,
}

#[derive(Serialize, Deserialize)]
pub struct VcpuState {
    /// Every register `KVM_GET_REG_LIST` names, none wider than 128 bits
    /// since SVE is not enabled.
    pub regs: Vec<(u64, u128)>,
    pub mp_state: u32,
    pub events: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct GicState {
    pub dist: Vec<(u32, u32)>,
    /// Per vCPU: the SGI and PPI frame of its redistributor.
    pub redist: Vec<Vec<(u32, u32)>>,
    /// Per vCPU: the CPU interface system registers.
    pub icc: Vec<Vec<(u64, u64)>>,
}

#[derive(Serialize, Deserialize)]
pub enum DeviceState {
    Uart {
        lcr: u32,
        cr: u32,
        imsc: u32,
        ibrd: u32,
        fbrd: u32,
    },
    Rtc {
        mr: u32,
        cr: u32,
        imsc: u32,
    },
    Virtio(VirtioState),
}

#[derive(Serialize, Deserialize)]
pub struct VirtioState {
    pub device_id: u32,
    pub status: u32,
    pub drv_features: u64,
    pub dev_features_sel: u32,
    pub drv_features_sel: u32,
    pub queue_sel: u32,
    pub activated: bool,
    pub interrupt_status: u32,
    pub queues: Vec<QueueState>,
    pub backend: BackendState,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct QueueState {
    pub size: u16,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    pub last_avail_idx: u16,
}

#[derive(Serialize, Deserialize)]
pub enum BackendState {
    Block,
    /// The next ring index of each queue the device serves itself.
    Vsock {
        rx: u16,
        tx: u16,
        event: u16,
    },
    Balloon {
        target_pages: u32,
        actual_pages: u32,
    },
    /// The next receive ring index. The transmit queue is the transport's.
    Net {
        rx: u16,
    },
}

fn error(what: impl std::fmt::Display, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

pub fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the device cannot be snapshotted",
    )
}

pub fn mismatch() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the snapshot does not match the devices",
    )
}

// ===========================================================================
// Memory
// ===========================================================================

/// Memory for a restored guest: the saved memory mapped private, so each
/// page comes from the file on first touch and guest writes never reach it.
pub fn private_memory(file: File, size: u64) -> io::Result<GuestMemoryMmap> {
    if file.metadata()?.len() != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the saved memory has the wrong size",
        ));
    }
    let region = MmapRegion::build(
        Some(FileOffset::new(file, 0)),
        size as usize,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_NORESERVE,
    )
    .map_err(io::Error::other)?;
    let region = GuestRegionMmap::new(region, GuestAddress(DRAM_BASE)).map_err(io::Error::other)?;
    GuestMemoryMmap::from_regions(vec![region]).map_err(io::Error::other)
}

/// Copies the pages the guest has that `out` lacks to the same offsets in
/// `out`: for a booted guest an empty file as long as the memory, for a
/// restored one a copy of the file it was restored from. Both mappings are
/// private, so those are the anonymous pages, written by the guest or a
/// device: present and not file backed, or swapped out. Pages the guest
/// never wrote stay holes or saved pages. `pagemap` is the
/// /proc/PID/pagemap of this process. Returns the bytes copied.
pub fn save_memory(
    pagemap: &File,
    mem: &GuestMemoryMmap,
    size: u64,
    out: &File,
) -> io::Result<u64> {
    const PRESENT: u64 = 1 << 63;
    const SWAPPED: u64 = 1 << 62;
    const FILE: u64 = 1 << 61;
    let changed = |entry: u64| entry & SWAPPED != 0 || entry & (PRESENT | FILE) == PRESENT;

    let base = mem
        .get_host_address(GuestAddress(DRAM_BASE))
        .map_err(io::Error::other)?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let pages = size / page;
    let first = base as u64 / page;
    let mut entries = vec![0u8; 4096 * 8];
    let mut saved = 0;
    let mut index = 0;
    while index < pages {
        let count = (pages - index).min(4096);
        let bytes = &mut entries[..count as usize * 8];
        pagemap.read_exact_at(bytes, (first + index) * 8)?;
        let entry = |i: u64| {
            let at = i as usize * 8;
            u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap())
        };
        let mut i = 0;
        while i < count {
            if !changed(entry(i)) {
                i += 1;
                continue;
            }
            let start = i;
            while i < count && changed(entry(i)) {
                i += 1;
            }
            let offset = (index + start) * page;
            let len = (i - start) * page;
            // SAFETY: the run lies inside the guest memory mapping.
            write_at(out, unsafe { base.add(offset as usize) }, offset, len)?;
            saved += len;
        }
        index += count;
    }
    Ok(saved)
}

fn write_at(out: &File, mut from: *const u8, mut offset: u64, mut len: u64) -> io::Result<()> {
    while len > 0 {
        let chunk = len.min(1 << 30) as usize;
        let n = unsafe { libc::pwrite(out.as_raw_fd(), from.cast(), chunk, offset as libc::off_t) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        // SAFETY: n is at most chunk, which is within the range.
        from = unsafe { from.add(n as usize) };
        offset += n as u64;
        len -= n as u64;
    }
    Ok(())
}

// ===========================================================================
// vCPU
// ===========================================================================

const KVM_REG_SIZE_SHIFT: u64 = 52;
const KVM_REG_SIZE_MASK: u64 = 0x00f0_0000_0000_0000;
// The most a kvm-bindings RegList holds.
const MAX_REGS: usize = 500;

const fn sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

/// A 64-bit KVM system register.
const fn sysreg64(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    0x6030_0000_0013_0000 | sysreg(op0, op1, crn, crm, op2)
}

const MPIDR_EL1: u64 = sysreg64(3, 0, 0, 0, 5);

/// What the guest saw of the CPU: its identity and the features it chose
/// code paths for at boot.
const CPU_ID_REGS: [(&str, u64); 11] = [
    ("MIDR_EL1", sysreg64(3, 0, 0, 0, 0)),
    ("REVIDR_EL1", sysreg64(3, 0, 0, 0, 6)),
    ("ID_AA64PFR0_EL1", sysreg64(3, 0, 0, 4, 0)),
    ("ID_AA64PFR1_EL1", sysreg64(3, 0, 0, 4, 1)),
    ("ID_AA64DFR0_EL1", sysreg64(3, 0, 0, 5, 0)),
    ("ID_AA64ISAR0_EL1", sysreg64(3, 0, 0, 6, 0)),
    ("ID_AA64ISAR1_EL1", sysreg64(3, 0, 0, 6, 1)),
    ("ID_AA64ISAR2_EL1", sysreg64(3, 0, 0, 6, 2)),
    ("ID_AA64MMFR0_EL1", sysreg64(3, 0, 0, 7, 0)),
    ("ID_AA64MMFR1_EL1", sysreg64(3, 0, 0, 7, 1)),
    ("ID_AA64MMFR2_EL1", sysreg64(3, 0, 0, 7, 2)),
];

/// Refuses a snapshot taken on a CPU that differs from what `vcpu`, newly
/// initialised, offers here.
pub fn check_cpu(vcpu: &VcpuFd, state: &VcpuState) -> io::Result<()> {
    for (name, id) in CPU_ID_REGS {
        let Some(&(_, saved)) = state.regs.iter().find(|(reg, _)| *reg == id) else {
            continue;
        };
        let mut bytes = [0u8; 8];
        let here = vcpu
            .get_one_reg(id, &mut bytes)
            .map(|_| u64::from_le_bytes(bytes) as u128);
        if here.ok() != Some(saved) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the snapshot was taken on another CPU: {name} was {saved:#x}"),
            ));
        }
    }
    Ok(())
}

fn reg_size(id: u64) -> usize {
    1 << ((id & KVM_REG_SIZE_MASK) >> KVM_REG_SIZE_SHIFT)
}

/// Saves a vCPU that is not running.
pub fn save_vcpu(vcpu: &VcpuFd) -> io::Result<VcpuState> {
    let mut list = RegList::new(MAX_REGS).map_err(|e| error("register list", e))?;
    vcpu.get_reg_list(&mut list)
        .map_err(|e| error("KVM_GET_REG_LIST", e))?;
    let mut regs = Vec::with_capacity(list.as_slice().len());
    for &id in list.as_slice() {
        let size = reg_size(id);
        if size > 16 {
            return Err(io::Error::other(format!(
                "register {id:#x} is {size} bytes wide"
            )));
        }
        let mut bytes = [0u8; 16];
        vcpu.get_one_reg(id, &mut bytes[..size])
            .map_err(|e| error(format_args!("KVM_GET_ONE_REG {id:#x}"), e))?;
        regs.push((id, u128::from_le_bytes(bytes)));
    }
    let mp_state = vcpu
        .get_mp_state()
        .map_err(|e| error("KVM_GET_MP_STATE", e))?
        .mp_state;
    let events = vcpu
        .get_vcpu_events()
        .map_err(|e| error("KVM_GET_VCPU_EVENTS", e))?;
    // SAFETY: kvm_vcpu_events is plain data.
    let events = unsafe {
        std::slice::from_raw_parts(
            &events as *const kvm_vcpu_events as *const u8,
            size_of::<kvm_vcpu_events>(),
        )
    }
    .to_vec();
    Ok(VcpuState {
        regs,
        mp_state,
        events,
    })
}

/// Restores a vCPU after `KVM_ARM_VCPU_INIT`. `MPIDR_EL1` goes first, so
/// the GIC created afterwards addresses the redistributor the vCPU had.
pub fn restore_vcpu(vcpu: &VcpuFd, state: &VcpuState) -> io::Result<()> {
    let ordered = state
        .regs
        .iter()
        .filter(|(id, _)| *id == MPIDR_EL1)
        .chain(state.regs.iter().filter(|(id, _)| *id != MPIDR_EL1));
    for &(id, value) in ordered {
        let size = reg_size(id);
        if size > 16 {
            return Err(io::Error::other(format!(
                "register {id:#x} is {size} bytes wide"
            )));
        }
        vcpu.set_one_reg(id, &value.to_le_bytes()[..size])
            .map_err(|e| error(format_args!("KVM_SET_ONE_REG {id:#x}"), e))?;
    }
    vcpu.set_mp_state(kvm_mp_state {
        mp_state: state.mp_state,
    })
    .map_err(|e| error("KVM_SET_MP_STATE", e))?;
    if state.events.len() != size_of::<kvm_vcpu_events>() {
        return Err(io::Error::other("vCPU events have the wrong size"));
    }
    let mut events = kvm_vcpu_events::default();
    // SAFETY: the length matches and kvm_vcpu_events is plain data.
    unsafe {
        std::ptr::copy_nonoverlapping(
            state.events.as_ptr(),
            &mut events as *mut kvm_vcpu_events as *mut u8,
            state.events.len(),
        );
    }
    vcpu.set_vcpu_events(&events)
        .map_err(|e| error("KVM_SET_VCPU_EVENTS", e))?;
    Ok(())
}

/// The affinity of a vCPU in the form the GIC device attributes take.
fn affinity(state: &VcpuState) -> io::Result<u64> {
    let mpidr = state
        .regs
        .iter()
        .find(|(id, _)| *id == MPIDR_EL1)
        .map(|(_, value)| *value as u64)
        .ok_or_else(|| io::Error::other("no MPIDR_EL1 in the vCPU state"))?;
    Ok(((mpidr >> 8) & 0xff00_0000) | (mpidr & 0x00ff_ffff))
}

// ===========================================================================
// GICv3
// ===========================================================================

const GICD_CTLR: u32 = 0x0000;
const GICD_IGROUPR: u32 = 0x0080;
const GICD_ISENABLER: u32 = 0x0100;
const GICD_ISPENDR: u32 = 0x0200;
const GICD_ISACTIVER: u32 = 0x0300;
const GICD_IPRIORITYR: u32 = 0x0400;
const GICD_ICFGR: u32 = 0x0C00;
const GICD_IROUTER: u32 = 0x6000;

const GICR_SGI_BASE: u32 = 0x1_0000;

/// The shared interrupt part of the distributor, in restore order. The
/// set-enable, set-pending and set-active registers only set bits, which is
/// enough because a new GIC starts with all of them clear. `GICD_CTLR`
/// goes last.
fn dist_offsets() -> Vec<u32> {
    let spis = PRIVATE_IRQS..NR_IRQS;
    let mut offsets = Vec::new();
    for base in [GICD_IGROUPR, GICD_ISENABLER, GICD_ISPENDR, GICD_ISACTIVER] {
        offsets.extend(spis.clone().step_by(32).map(|irq| base + irq / 8));
    }
    offsets.extend(spis.clone().step_by(4).map(|irq| GICD_IPRIORITYR + irq));
    offsets.extend(spis.clone().step_by(16).map(|irq| GICD_ICFGR + irq / 4));
    for irq in spis {
        offsets.push(GICD_IROUTER + irq * 8);
        offsets.push(GICD_IROUTER + irq * 8 + 4);
    }
    offsets.push(GICD_CTLR);
    offsets
}

/// The SGI and PPI frame of one redistributor.
fn redist_offsets() -> Vec<u32> {
    let mut offsets: Vec<u32> = [0x0080, 0x0100, 0x0200, 0x0300]
        .into_iter()
        .chain((0..8).map(|i| 0x0400 + i * 4))
        .chain([0x0C00, 0x0C04])
        .collect();
    for offset in &mut offsets {
        *offset += GICR_SGI_BASE;
    }
    offsets
}

const ICC_SRE_EL1: u64 = sysreg(3, 0, 12, 12, 5);
const ICC_CTLR_EL1: u64 = sysreg(3, 0, 12, 12, 4);
const ICC_PMR_EL1: u64 = sysreg(3, 0, 4, 6, 0);
const ICC_BPR0_EL1: u64 = sysreg(3, 0, 12, 8, 3);
const ICC_BPR1_EL1: u64 = sysreg(3, 0, 12, 12, 3);
const ICC_IGRPEN0_EL1: u64 = sysreg(3, 0, 12, 12, 6);
const ICC_IGRPEN1_EL1: u64 = sysreg(3, 0, 12, 12, 7);

/// The CPU interface registers in restore order. The active priority
/// registers past what the GIC implements fail to read and are skipped.
fn icc_registers() -> Vec<u64> {
    let mut registers = vec![
        ICC_SRE_EL1,
        ICC_CTLR_EL1,
        ICC_PMR_EL1,
        ICC_BPR0_EL1,
        ICC_BPR1_EL1,
    ];
    registers.extend((0..4).map(|n| sysreg(3, 0, 12, 8, 4 + n)));
    registers.extend((0..4).map(|n| sysreg(3, 0, 12, 9, n)));
    registers.extend([ICC_IGRPEN0_EL1, ICC_IGRPEN1_EL1]);
    registers
}

fn attr<T>(group: u32, attr: u64, value: &mut T) -> kvm_device_attr {
    kvm_device_attr {
        group,
        attr,
        addr: value as *mut T as u64,
        flags: 0,
    }
}

fn get32(gic: &DeviceFd, group: u32, at: u64) -> io::Result<u32> {
    let mut value = 0u32;
    gic.get_device_attr(&mut attr(group, at, &mut value))
        .map_err(|e| error(format_args!("GIC read {group}:{at:#x}"), e))?;
    Ok(value)
}

fn set32(gic: &DeviceFd, group: u32, at: u64, mut value: u32) -> io::Result<()> {
    gic.set_device_attr(&attr(group, at, &mut value))
        .map_err(|e| error(format_args!("GIC write {group}:{at:#x}"), e))
}

pub fn save_gic(gic: &DeviceFd, vcpus: &[VcpuState]) -> io::Result<GicState> {
    let dist = dist_offsets()
        .into_iter()
        .map(|offset| {
            get32(gic, KVM_DEV_ARM_VGIC_GRP_DIST_REGS, offset as u64).map(|v| (offset, v))
        })
        .collect::<io::Result<_>>()?;
    let mut redist = Vec::with_capacity(vcpus.len());
    let mut icc = Vec::with_capacity(vcpus.len());
    for vcpu in vcpus {
        let mpidr = affinity(vcpu)? << 32;
        redist.push(
            redist_offsets()
                .into_iter()
                .map(|offset| {
                    get32(gic, KVM_DEV_ARM_VGIC_GRP_REDIST_REGS, mpidr | offset as u64)
                        .map(|v| (offset, v))
                })
                .collect::<io::Result<_>>()?,
        );
        let mut registers = Vec::new();
        for register in icc_registers() {
            let mut value = 0u64;
            let mut at = attr(
                KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
                mpidr | register,
                &mut value,
            );
            if gic.get_device_attr(&mut at).is_ok() {
                registers.push((register, value));
            }
        }
        icc.push(registers);
    }
    Ok(GicState { dist, redist, icc })
}

/// Restores the GIC after every vCPU has its `MPIDR_EL1` back.
pub fn restore_gic(gic: &DeviceFd, state: &GicState, vcpus: &[VcpuState]) -> io::Result<()> {
    if state.redist.len() != vcpus.len() || state.icc.len() != vcpus.len() {
        return Err(io::Error::other("GIC state does not match the vCPUs"));
    }
    for (i, vcpu) in vcpus.iter().enumerate() {
        let mpidr = affinity(vcpu)? << 32;
        for &(register, mut value) in &state.icc[i] {
            gic.set_device_attr(&attr(
                KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
                mpidr | register,
                &mut value,
            ))
            .map_err(|e| error(format_args!("GIC write ICC {register:#x}"), e))?;
        }
        for &(offset, value) in &state.redist[i] {
            set32(
                gic,
                KVM_DEV_ARM_VGIC_GRP_REDIST_REGS,
                mpidr | offset as u64,
                value,
            )?;
        }
    }
    for &(offset, value) in &state.dist {
        set32(gic, KVM_DEV_ARM_VGIC_GRP_DIST_REGS, offset as u64, value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_encodings() {
        assert_eq!(MPIDR_EL1, 0x6030_0000_0013_c005);
        assert_eq!(CPU_ID_REGS[0].1, 0x6030_0000_0013_c000);
        assert_eq!(CPU_ID_REGS[2].1, 0x6030_0000_0013_c020);
        assert_eq!(reg_size(MPIDR_EL1), 8);
        assert_eq!(reg_size(0x6040_0000_0010_0054), 16);
        assert_eq!(ICC_SRE_EL1, 0xc665);
        assert_eq!(ICC_PMR_EL1, 0xc230);
    }

    #[test]
    fn distributor_covers_the_shared_interrupts_once() {
        let offsets = dist_offsets();
        let unique: std::collections::BTreeSet<_> = offsets.iter().collect();
        assert_eq!(unique.len(), offsets.len());
        assert_eq!(offsets.last(), Some(&GICD_CTLR));
        assert!(offsets.contains(&(GICD_ISENABLER + 4)));
        assert!(!offsets.contains(&GICD_ISENABLER));
        assert!(offsets.contains(&(GICD_IROUTER + 127 * 8 + 4)));
        assert!(offsets.contains(&(GICD_ICFGR + 0x1C)));
    }

    #[test]
    fn affinity_packs_aff3_above_aff2() {
        let state = VcpuState {
            regs: vec![(
                MPIDR_EL1,
                0x8000_0000 | (5 << 32) | (3 << 16) | (2 << 8) | 1,
            )],
            mp_state: 0,
            events: Vec::new(),
        };
        assert_eq!(
            affinity(&state).unwrap(),
            (5 << 24) | (3 << 16) | (2 << 8) | 1
        );
    }
}
