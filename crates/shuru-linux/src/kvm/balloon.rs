//! Virtio memory balloon (device ID 5).
//!
//! Memory goes back to the host two ways:
//!
//! - Free page reporting (`VIRTIO_BALLOON_F_REPORTING`). The guest kernel
//!   hands over runs of free pages a couple of seconds after they free up
//!   and the host discards them with `MADV_DONTNEED`. Nothing has to ask
//!   and the guest keeps its full memory size.
//! - A memory target. The host lowers how much memory the guest keeps,
//!   the guest driver allocates the difference and sends the page numbers
//!   over the inflate queue, and the host discards those pages too.
//!   Raising the target gives them back through the deflate queue.
//!
//! A discarded page reads as zero when the guest touches it again, so the
//! only cost to the guest is a page fault. The guest may also deflate on
//! its own under memory pressure (`VIRTIO_BALLOON_F_DEFLATE_ON_OOM`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use kvm_ioctls::VmFd;
use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use super::devices::{
    avail_idx, avail_ring_entry, read_desc, write_used, VirtioBackend, VirtioQueueState,
    VIRTIO_F_VERSION_1, VRING_DESC_F_NEXT,
};

const VIRTIO_BALLOON_F_STATS_VQ: u64 = 1 << 1;
const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u64 = 1 << 2;
const VIRTIO_BALLOON_F_FREE_PAGE_HINT: u64 = 1 << 3;
const VIRTIO_BALLOON_F_REPORTING: u64 = 1 << 5;

const OFFERED_FEATURES: u64 =
    VIRTIO_F_VERSION_1 | VIRTIO_BALLOON_F_DEFLATE_ON_OOM | VIRTIO_BALLOON_F_REPORTING;

/// Balloon pages are always 4 KiB, whatever the guest page size.
const PAGE_SHIFT: u32 = 12;
const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

const VIRTIO_MMIO_INT_CONFIG: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QueueRole {
    Inflate,
    Deflate,
    Stats,
    FreePageHint,
    Reporting,
}

/// The optional queues in spec order. Each exists only when its feature is
/// negotiated, and a queue that does not exist takes no index, so the ones
/// after it move up. The spec numbers them 2, 3 and 4, which is their
/// position only when all three are negotiated.
const OPTIONAL_QUEUES: [(u64, QueueRole); 3] = [
    (VIRTIO_BALLOON_F_STATS_VQ, QueueRole::Stats),
    (VIRTIO_BALLOON_F_FREE_PAGE_HINT, QueueRole::FreePageHint),
    (VIRTIO_BALLOON_F_REPORTING, QueueRole::Reporting),
];

struct QueueLayout {
    roles: Vec<QueueRole>,
}

impl QueueLayout {
    fn for_features(features: u64) -> Self {
        let mut roles = vec![QueueRole::Inflate, QueueRole::Deflate];
        roles.extend(
            OPTIONAL_QUEUES
                .iter()
                .filter(|(bit, _)| features & bit != 0)
                .map(|(_, role)| *role),
        );
        QueueLayout { roles }
    }

    fn role(&self, idx: u16) -> Option<QueueRole> {
        self.roles.get(usize::from(idx)).copied()
    }

    fn len(&self) -> usize {
        self.roles.len()
    }
}

struct IrqLine {
    vm_fd: Arc<VmFd>,
    irq: u32,
    status: Arc<AtomicU32>,
}

/// Host side of the balloon: set a target, read what the guest gave up.
pub struct BalloonControl {
    total_bytes: u64,
    target_pages: AtomicU32,
    actual_pages: AtomicU32,
    irq: Mutex<Option<IrqLine>>,
}

impl BalloonControl {
    /// How much memory the guest should keep. Rounded down to a page and
    /// clamped to the configured size; the guest responds in its own time.
    pub fn set_memory_target(&self, bytes: u64) {
        let keep = bytes.min(self.total_bytes);
        let pages = ((self.total_bytes - keep) / PAGE_SIZE) as u32;
        self.target_pages.store(pages, Ordering::Release);
        if let Some(line) = self.irq.lock().unwrap().as_ref() {
            line.status
                .fetch_or(VIRTIO_MMIO_INT_CONFIG, Ordering::Release);
            let _ = line.vm_fd.set_irq_line(line.irq, true);
        }
    }

    /// Bytes the guest has handed over through the target so far. Free
    /// page reporting is not counted: those pages still belong to the guest.
    pub fn ballooned_bytes(&self) -> u64 {
        self.actual_pages.load(Ordering::Acquire) as u64 * PAGE_SIZE
    }
}

pub struct BalloonBackend {
    control: Arc<BalloonControl>,
    host_page: u64,
    layout: QueueLayout,
}

impl BalloonBackend {
    pub fn new(total_bytes: u64) -> Self {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        BalloonBackend {
            control: Arc::new(BalloonControl {
                total_bytes,
                target_pages: AtomicU32::new(0),
                actual_pages: AtomicU32::new(0),
                irq: Mutex::new(None),
            }),
            host_page: if page > 0 { page as u64 } else { PAGE_SIZE },
            layout: QueueLayout::for_features(OFFERED_FEATURES),
        }
    }

    pub fn control(&self) -> Arc<BalloonControl> {
        self.control.clone()
    }

    /// Give the host pages fully inside `[gpa, gpa + len)` back. A guest
    /// page smaller than a host page cannot go alone, since the rest of the
    /// host page may still be in use.
    fn discard(&self, mem: &GuestMemoryMmap, gpa: u64, len: u64) {
        let mask = !(self.host_page - 1);
        let start = (gpa + self.host_page - 1) & mask;
        let end = gpa.saturating_add(len) & mask;
        if end <= start {
            return;
        }
        let len = (end - start) as usize;
        if !mem.check_range(GuestAddress(start), len) {
            return;
        }
        if let Ok(ptr) = mem.get_host_address(GuestAddress(start)) {
            unsafe {
                libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DONTNEED);
            }
        }
    }

    /// An inflate buffer is an array of 4 KiB page numbers. Consecutive
    /// pages are discarded as one run.
    fn discard_pfns(&self, mem: &GuestMemoryMmap, addr: u64, len: u32) {
        let mut run: Option<(u64, u64)> = None;
        for i in 0..(len / 4) as u64 {
            let pfn = match mem.read_obj::<u32>(GuestAddress(addr + i * 4)) {
                Ok(pfn) => pfn as u64,
                Err(_) => break,
            };
            let gpa = pfn << PAGE_SHIFT;
            run = match run {
                Some((start, run_len)) if start + run_len == gpa => {
                    Some((start, run_len + PAGE_SIZE))
                }
                Some((start, run_len)) => {
                    self.discard(mem, start, run_len);
                    Some((gpa, PAGE_SIZE))
                }
                None => Some((gpa, PAGE_SIZE)),
            };
        }
        if let Some((start, run_len)) = run {
            self.discard(mem, start, run_len);
        }
    }

    fn drain(&self, role: QueueRole, q: &mut VirtioQueueState, mem: &GuestMemoryMmap) {
        if q.size == 0 {
            return;
        }
        let current = avail_idx(mem, q.avail_addr);
        while q.last_avail_idx != current {
            let head = avail_ring_entry(mem, q.avail_addr, q.size, q.last_avail_idx);
            let mut idx = head;
            loop {
                let desc = read_desc(mem, q.desc_addr, idx);
                match role {
                    QueueRole::Inflate => self.discard_pfns(mem, desc.addr, desc.len),
                    QueueRole::Deflate => {}
                    QueueRole::Reporting => self.discard(mem, desc.addr, desc.len as u64),
                    QueueRole::Stats | QueueRole::FreePageHint => {
                        unreachable!("the balloon does not offer stats or free page hints")
                    }
                }
                if desc.flags & VRING_DESC_F_NEXT == 0 {
                    break;
                }
                idx = desc.next;
            }
            write_used(mem, q.used_addr, q.size, q.last_avail_idx, head as u32, 0);
            q.last_avail_idx = q.last_avail_idx.wrapping_add(1);
        }
    }
}

impl VirtioBackend for BalloonBackend {
    fn device_id(&self) -> u32 {
        5
    }

    fn device_features(&self) -> u64 {
        OFFERED_FEATURES
    }

    fn queue_count(&self) -> usize {
        QueueLayout::for_features(OFFERED_FEATURES).len()
    }

    fn queue_max_size(&self) -> u16 {
        128
    }

    fn config_read(&self, offset: u64) -> u32 {
        match offset {
            0 => self.control.target_pages.load(Ordering::Acquire),
            4 => self.control.actual_pages.load(Ordering::Acquire),
            _ => 0,
        }
    }

    fn config_write(&mut self, offset: u64, value: u32) {
        if offset == 4 {
            self.control.actual_pages.store(value, Ordering::Release);
        }
    }

    fn features_ok(&mut self, features: u64) {
        self.layout = QueueLayout::for_features(features);
    }

    fn activate(
        &mut self,
        _: &[VirtioQueueState],
        _: &GuestMemoryMmap,
        vm_fd: &Arc<VmFd>,
        irq: u32,
        interrupt_status: Arc<AtomicU32>,
    ) {
        *self.control.irq.lock().unwrap() = Some(IrqLine {
            vm_fd: vm_fd.clone(),
            irq,
            status: interrupt_status,
        });
    }

    fn process_queue(
        &mut self,
        queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
        _: &Arc<VmFd>,
        _: u32,
    ) {
        let Some(role) = self.layout.role(queue_idx) else {
            return;
        };
        if let Some(q) = queues.get_mut(usize::from(queue_idx)) {
            if q.ready {
                self.drain(role, q, mem);
            }
        }
    }

    fn reset(&mut self) {
        *self.control.irq.lock().unwrap() = None;
        self.control.actual_pages.store(0, Ordering::Release);
        self.layout = QueueLayout::for_features(OFFERED_FEATURES);
    }
}
