//! Virtual machine generation ID, the `microsoft,vmgenid` devicetree
//! device. The guest kernel reads 16 bytes from a page outside its RAM and
//! rereads them on an edge interrupt; when they change it reseeds its RNG
//! in the interrupt handler. Every VM gets a fresh page, and a restore
//! raises the interrupt, so no two restores of one snapshot share RNG state.

use std::io;

use kvm_ioctls::VmFd;
use vm_memory::mmap::MmapRegion;

use super::layout::{spi_to_irq, VMGENID_BASE, VMGENID_SIZE, VMGENID_SPI};

const SLOT: u32 = 1;

pub struct VmGenId {
    // Mapped into the guest until the VM is gone.
    _page: MmapRegion<()>,
}

impl VmGenId {
    pub fn new(vm_fd: &VmFd) -> io::Result<Self> {
        let page = MmapRegion::<()>::new(VMGENID_SIZE as usize).map_err(io::Error::other)?;
        // SAFETY: the region is VMGENID_SIZE bytes, private to this process.
        let id = unsafe { std::slice::from_raw_parts_mut(page.as_ptr(), 16) };
        // Requests this small are never cut short.
        if unsafe { libc::getrandom(id.as_mut_ptr().cast(), id.len(), 0) } != id.len() as isize {
            return Err(io::Error::last_os_error());
        }
        let region = kvm_bindings::kvm_userspace_memory_region {
            slot: SLOT,
            guest_phys_addr: VMGENID_BASE,
            memory_size: VMGENID_SIZE,
            userspace_addr: page.as_ptr() as u64,
            flags: kvm_bindings::KVM_MEM_READONLY,
        };
        // SAFETY: the page outlives the VM, which holds it in `_page`.
        unsafe { vm_fd.set_user_memory_region(region) }.map_err(io::Error::from)?;
        Ok(VmGenId { _page: page })
    }

    /// Tells a restored guest that its generation changed.
    pub fn notify(vm_fd: &VmFd) -> io::Result<()> {
        let irq = spi_to_irq(VMGENID_SPI);
        vm_fd.set_irq_line(irq, true).map_err(io::Error::from)?;
        vm_fd.set_irq_line(irq, false).map_err(io::Error::from)
    }
}
