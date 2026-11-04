//! What a paused VM is made of besides its memory and disk, and how a new
//! VM takes it back (RFC 0009): vCPU registers, the interrupt controller
//! and the devices. The first two are in the arch module.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use serde::{Deserialize, Serialize};
use vm_memory::{
    FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestRegionMmap, MmapRegion,
};

use super::arch::{IrqchipState, VcpuState};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct SnapshotState {
    pub version: u32,
    pub cpu_count: usize,
    pub memory_bytes: u64,
    pub vcpus: Vec<VcpuState>,
    #[serde(rename = "gic")]
    pub irqchip: IrqchipState,
    /// One entry per device on the MMIO bus, in bus order.
    pub devices: Vec<DeviceState>,
    /// The same for the I/O ports, which only x86_64 has.
    #[serde(default)]
    pub ports: Vec<DeviceState>,
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
    Serial {
        ier: u8,
        lcr: u8,
        mcr: u8,
        scr: u8,
        fifo: bool,
        divisor: u16,
        thre_pending: bool,
    },
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
/// The file holds the RAM ranges back to back.
pub fn private_memory(file: File, ranges: &[(u64, u64)]) -> io::Result<GuestMemoryMmap> {
    let size: u64 = ranges.iter().map(|&(_, len)| len).sum();
    if file.metadata()?.len() != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the saved memory has the wrong size",
        ));
    }
    let mut regions = Vec::with_capacity(ranges.len());
    let mut offset = 0;
    for &(start, len) in ranges {
        let region = MmapRegion::build(
            Some(FileOffset::new(file.try_clone()?, offset)),
            len as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_NORESERVE,
        )
        .map_err(io::Error::other)?;
        regions.push(GuestRegionMmap::new(region, GuestAddress(start)).map_err(io::Error::other)?);
        offset += len;
    }
    GuestMemoryMmap::from_regions(regions).map_err(io::Error::other)
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
    ranges: &[(u64, u64)],
    out: &File,
) -> io::Result<u64> {
    let mut saved = 0;
    let mut file_offset = 0;
    for &(start, len) in ranges {
        let base = mem
            .get_host_address(GuestAddress(start))
            .map_err(io::Error::other)?;
        saved += save_range(pagemap, base, len, out, file_offset)?;
        file_offset += len;
    }
    Ok(saved)
}

fn save_range(
    pagemap: &File,
    base: *mut u8,
    size: u64,
    out: &File,
    file_offset: u64,
) -> io::Result<u64> {
    const PRESENT: u64 = 1 << 63;
    const SWAPPED: u64 = 1 << 62;
    const FILE: u64 = 1 << 61;
    let changed = |entry: u64| entry & SWAPPED != 0 || entry & (PRESENT | FILE) == PRESENT;

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
            write_at(
                out,
                unsafe { base.add(offset as usize) },
                file_offset + offset,
                len,
            )?;
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
