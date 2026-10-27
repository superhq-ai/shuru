use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use kvm_ioctls::VmFd;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use super::layout;

// ===========================================================================
// MMIO device trait + bus
// ===========================================================================

pub trait MmioDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]);
    fn write(&mut self, offset: u64, data: &[u8]);
}

pub struct MmioEntry {
    pub base: u64,
    pub size: u64,
    pub device: Box<dyn MmioDevice>,
}

pub struct MmioBus {
    pub devices: Vec<MmioEntry>,
}

impl MmioBus {
    pub fn new() -> Self {
        MmioBus {
            devices: Vec::new(),
        }
    }

    pub fn add(&mut self, base: u64, size: u64, device: Box<dyn MmioDevice>) {
        self.devices.push(MmioEntry { base, size, device });
    }

    pub fn handle_read(&mut self, addr: u64, data: &mut [u8]) {
        for entry in &mut self.devices {
            if addr >= entry.base && addr < entry.base + entry.size {
                entry.device.read(addr - entry.base, data);
                return;
            }
        }
        for b in data.iter_mut() {
            *b = 0;
        }
    }

    pub fn handle_write(&mut self, addr: u64, data: &[u8]) {
        for entry in &mut self.devices {
            if addr >= entry.base && addr < entry.base + entry.size {
                entry.device.write(addr - entry.base, data);
                return;
            }
        }
    }
}

// ===========================================================================
// PL011 UART (minimal — transmit + basic RX for console mode)
// ===========================================================================

const UARTDR: u64 = 0x000;
const UARTFR: u64 = 0x018;
const UARTIBRD: u64 = 0x024;
const UARTFBRD: u64 = 0x028;
const UARTLCR_H: u64 = 0x02C;
const UARTCR: u64 = 0x030;
const UARTIMSC: u64 = 0x038;
const UARTICR: u64 = 0x048;

const UARTPERIPHID0: u64 = 0xFE0;
const UARTPERIPHID1: u64 = 0xFE4;
const UARTPERIPHID2: u64 = 0xFE8;
const UARTPERIPHID3: u64 = 0xFEC;
const UARTPCELLID0: u64 = 0xFF0;
const UARTPCELLID1: u64 = 0xFF4;
const UARTPCELLID2: u64 = 0xFF8;
const UARTPCELLID3: u64 = 0xFFC;

const UARTFR_TXFE: u32 = 1 << 7;
const UARTFR_RXFE: u32 = 1 << 4;

pub struct Pl011 {
    write_fd: Option<RawFd>,
    #[allow(dead_code)]
    read_fd: Option<RawFd>,
    lcr: u32,
    cr: u32,
    imsc: u32,
    ibrd: u32,
    fbrd: u32,
}

impl Pl011 {
    pub fn new(write_fd: Option<RawFd>, read_fd: Option<RawFd>) -> Self {
        Pl011 {
            write_fd,
            read_fd,
            lcr: 0,
            cr: 0x0300,
            imsc: 0,
            ibrd: 0,
            fbrd: 0,
        }
    }

    fn read_u32(&self, offset: u64) -> u32 {
        match offset {
            UARTDR => 0,
            UARTFR => UARTFR_TXFE | UARTFR_RXFE,
            UARTIBRD => self.ibrd,
            UARTFBRD => self.fbrd,
            UARTLCR_H => self.lcr,
            UARTCR => self.cr,
            UARTIMSC => self.imsc,
            UARTPERIPHID0 => 0x11,
            UARTPERIPHID1 => 0x10,
            UARTPERIPHID2 => 0x34,
            UARTPERIPHID3 => 0x00,
            UARTPCELLID0 => 0x0D,
            UARTPCELLID1 => 0xF0,
            UARTPCELLID2 => 0x05,
            UARTPCELLID3 => 0xB1,
            _ => 0,
        }
    }

    fn write_u32(&mut self, offset: u64, val: u32) {
        match offset {
            UARTDR => {
                if let Some(fd) = self.write_fd {
                    let byte = (val & 0xFF) as u8;
                    unsafe {
                        libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
                    }
                }
            }
            UARTIBRD => self.ibrd = val,
            UARTFBRD => self.fbrd = val,
            UARTLCR_H => self.lcr = val,
            UARTCR => self.cr = val,
            UARTIMSC => self.imsc = val,
            UARTICR => {}
            _ => {}
        }
    }
}

impl MmioDevice for Pl011 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let val = self.read_u32(offset);
        let len = data.len().min(4);
        data[..len].copy_from_slice(&val.to_le_bytes()[..len]);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        self.write_u32(offset, u32::from_le_bytes(bytes));
    }
}

// ===========================================================================
// PL031 RTC (read-only wall clock from host, no alarm/IRQ support)
// ===========================================================================

const RTCDR: u64 = 0x000; // Data register: seconds since Unix epoch
const RTCMR: u64 = 0x004; // Match register
const RTCLR: u64 = 0x008; // Load register
const RTCCR: u64 = 0x00C; // Control register (bit 0: enable)
const RTCIMSC: u64 = 0x010; // Interrupt mask
const RTCRIS: u64 = 0x014; // Raw interrupt status
const RTCMIS: u64 = 0x018; // Masked interrupt status
const RTCICR: u64 = 0x01C; // Interrupt clear

const RTCPERIPHID0: u64 = 0xFE0;
const RTCPERIPHID1: u64 = 0xFE4;
const RTCPERIPHID2: u64 = 0xFE8;
const RTCPERIPHID3: u64 = 0xFEC;
const RTCPCELLID0: u64 = 0xFF0;
const RTCPCELLID1: u64 = 0xFF4;
const RTCPCELLID2: u64 = 0xFF8;
const RTCPCELLID3: u64 = 0xFFC;

pub struct Pl031 {
    mr: u32,
    cr: u32,
    imsc: u32,
}

impl Pl031 {
    pub fn new() -> Self {
        Pl031 {
            mr: 0,
            cr: 1, // enabled
            imsc: 0,
        }
    }

    fn now_unix(&self) -> u32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0)
    }
}

impl MmioDevice for Pl031 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let val: u32 = match offset {
            RTCDR => self.now_unix(),
            RTCMR => self.mr,
            RTCLR => self.now_unix(),
            RTCCR => self.cr,
            RTCIMSC => self.imsc,
            RTCRIS => 0,
            RTCMIS => 0,
            // PrimeCell ID: PL031, revision 1
            RTCPERIPHID0 => 0x31,
            RTCPERIPHID1 => 0x10,
            RTCPERIPHID2 => 0x04,
            RTCPERIPHID3 => 0x00,
            RTCPCELLID0 => 0x0D,
            RTCPCELLID1 => 0xF0,
            RTCPCELLID2 => 0x05,
            RTCPCELLID3 => 0xB1,
            _ => 0,
        };
        let len = data.len().min(4);
        data[..len].copy_from_slice(&val.to_le_bytes()[..len]);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let val = u32::from_le_bytes(bytes);
        match offset {
            RTCMR => self.mr = val,
            RTCLR => {} // guest attempts to set time - ignore, host clock is authoritative
            RTCCR => self.cr = val & 1,
            RTCIMSC => self.imsc = val & 1,
            RTCICR => {}
            _ => {}
        }
    }
}

// ===========================================================================
// Virtio MMIO transport (v2)
// ===========================================================================

const VIRTIO_MMIO_MAGIC: u64 = 0x000;
const VIRTIO_MMIO_VERSION: u64 = 0x004;
const VIRTIO_MMIO_DEVICE_ID: u64 = 0x008;
const VIRTIO_MMIO_VENDOR_ID: u64 = 0x00C;
const VIRTIO_MMIO_DEV_FEATURES: u64 = 0x010;
const VIRTIO_MMIO_DEV_FEATURES_SEL: u64 = 0x014;
const VIRTIO_MMIO_DRV_FEATURES: u64 = 0x020;
const VIRTIO_MMIO_DRV_FEATURES_SEL: u64 = 0x024;
const VIRTIO_MMIO_QUEUE_SEL: u64 = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX: u64 = 0x034;
const VIRTIO_MMIO_QUEUE_NUM: u64 = 0x038;
const VIRTIO_MMIO_QUEUE_READY: u64 = 0x044;
const VIRTIO_MMIO_QUEUE_NOTIFY: u64 = 0x050;
const VIRTIO_MMIO_INTERRUPT_STATUS: u64 = 0x060;
const VIRTIO_MMIO_INTERRUPT_ACK: u64 = 0x064;
const VIRTIO_MMIO_STATUS: u64 = 0x070;
const VIRTIO_MMIO_QUEUE_DESC_LOW: u64 = 0x080;
const VIRTIO_MMIO_QUEUE_DESC_HIGH: u64 = 0x084;
const VIRTIO_MMIO_QUEUE_DRIVER_LOW: u64 = 0x090;
const VIRTIO_MMIO_QUEUE_DRIVER_HIGH: u64 = 0x094;
const VIRTIO_MMIO_QUEUE_DEVICE_LOW: u64 = 0x0A0;
const VIRTIO_MMIO_QUEUE_DEVICE_HIGH: u64 = 0x0A4;
const VIRTIO_MMIO_CONFIG_GENERATION: u64 = 0x0FC;
const VIRTIO_MMIO_CONFIG: u64 = 0x100;

const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;

#[derive(Clone)]
pub struct VirtioQueueState {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    pub last_avail_idx: u16,
}

impl VirtioQueueState {
    fn new(max_size: u16) -> Self {
        VirtioQueueState {
            max_size,
            size: 0,
            ready: false,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
        }
    }
}

pub trait VirtioBackend: Send {
    fn device_id(&self) -> u32;
    fn device_features(&self) -> u64;
    fn queue_count(&self) -> usize;
    fn queue_max_size(&self) -> u16;
    fn config_read(&self, offset: u64) -> u32;
    fn config_write(&mut self, offset: u64, value: u32);
    fn activate(
        &mut self,
        queues: &[VirtioQueueState],
        mem: &GuestMemoryMmap,
        vm_fd: &Arc<VmFd>,
        irq: u32,
        interrupt_status: Arc<AtomicU32>,
    );
    fn process_queue(
        &mut self,
        queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
        vm_fd: &Arc<VmFd>,
        irq: u32,
    );
    fn reset(&mut self);
}

pub struct VirtioMmioDevice {
    backend: Box<dyn VirtioBackend>,
    queues: Vec<VirtioQueueState>,
    dev_features_sel: u32,
    drv_features: u64,
    drv_features_sel: u32,
    queue_sel: u32,
    status: u32,
    interrupt_status: Arc<AtomicU32>,
    vm_fd: Arc<VmFd>,
    mem: GuestMemoryMmap,
    irq: u32,
    activated: bool,
}

impl VirtioMmioDevice {
    pub fn new(
        backend: Box<dyn VirtioBackend>,
        spi: u32,
        vm_fd: Arc<VmFd>,
        mem: GuestMemoryMmap,
    ) -> Self {
        let queue_count = backend.queue_count();
        let queue_max = backend.queue_max_size();
        let queues = (0..queue_count)
            .map(|_| VirtioQueueState::new(queue_max))
            .collect();

        VirtioMmioDevice {
            backend,
            queues,
            dev_features_sel: 0,
            drv_features: 0,
            drv_features_sel: 0,
            queue_sel: 0,
            status: 0,
            interrupt_status: Arc::new(AtomicU32::new(0)),
            vm_fd,
            mem,
            // KVM_IRQ_LINE on ARM: SPI N maps to intid N+32
            irq: layout::spi_to_irq(spi),
            activated: false,
        }
    }

    fn selected_queue(&self) -> Option<&VirtioQueueState> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut VirtioQueueState> {
        self.queues.get_mut(self.queue_sel as usize)
    }
}

impl MmioDevice for VirtioMmioDevice {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let val: u32 = match offset {
            VIRTIO_MMIO_MAGIC => 0x7472_6976,
            VIRTIO_MMIO_VERSION => 2,
            VIRTIO_MMIO_DEVICE_ID => self.backend.device_id(),
            VIRTIO_MMIO_VENDOR_ID => 0x554D_4551,
            VIRTIO_MMIO_DEV_FEATURES => {
                let features = self.backend.device_features();
                if self.dev_features_sel == 0 {
                    features as u32
                } else {
                    (features >> 32) as u32
                }
            }
            VIRTIO_MMIO_QUEUE_NUM_MAX => self.selected_queue().map_or(0, |q| q.max_size as u32),
            VIRTIO_MMIO_QUEUE_READY => self.selected_queue().map_or(0, |q| q.ready as u32),
            VIRTIO_MMIO_INTERRUPT_STATUS => self.interrupt_status.load(Ordering::Acquire),
            VIRTIO_MMIO_STATUS => self.status,
            VIRTIO_MMIO_CONFIG_GENERATION => 0,
            o if o >= VIRTIO_MMIO_CONFIG => self.backend.config_read(o - VIRTIO_MMIO_CONFIG),
            _ => 0,
        };
        let len = data.len().min(4);
        data[..len].copy_from_slice(&val.to_le_bytes()[..len]);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let val = u32::from_le_bytes(bytes);

        match offset {
            VIRTIO_MMIO_DEV_FEATURES_SEL => self.dev_features_sel = val,
            VIRTIO_MMIO_DRV_FEATURES => {
                if self.drv_features_sel == 0 {
                    self.drv_features = (self.drv_features & 0xFFFF_FFFF_0000_0000) | val as u64;
                } else {
                    self.drv_features =
                        (self.drv_features & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_DRV_FEATURES_SEL => self.drv_features_sel = val,
            VIRTIO_MMIO_QUEUE_SEL => self.queue_sel = val,
            VIRTIO_MMIO_QUEUE_NUM => {
                if let Some(q) = self.selected_queue_mut() {
                    q.size = val as u16;
                }
            }
            VIRTIO_MMIO_QUEUE_READY => {
                if let Some(q) = self.selected_queue_mut() {
                    q.ready = val == 1;
                }
            }
            VIRTIO_MMIO_QUEUE_NOTIFY => {
                self.backend.process_queue(
                    val as u16,
                    &mut self.queues,
                    &self.mem,
                    &self.vm_fd,
                    self.irq,
                );
                self.interrupt_status.fetch_or(1, Ordering::Release);
                let _ = self.vm_fd.set_irq_line(self.irq, true);
            }
            VIRTIO_MMIO_INTERRUPT_ACK => {
                self.interrupt_status.fetch_and(!val, Ordering::AcqRel);
                if self.interrupt_status.load(Ordering::Acquire) == 0 {
                    let _ = self.vm_fd.set_irq_line(self.irq, false);
                }
            }
            VIRTIO_MMIO_STATUS => {
                self.status = val;
                if val == 0 {
                    self.backend.reset();
                    self.activated = false;
                    self.interrupt_status.store(0, Ordering::Release);
                }
                if !self.activated
                    && (val & VIRTIO_STATUS_DRIVER_OK) != 0
                    && (val & VIRTIO_STATUS_FEATURES_OK) != 0
                {
                    self.activated = true;
                    self.backend.activate(
                        &self.queues,
                        &self.mem,
                        &self.vm_fd,
                        self.irq,
                        self.interrupt_status.clone(),
                    );
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.desc_addr = (q.desc_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.desc_addr = (q.desc_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_DRIVER_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.avail_addr = (q.avail_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DRIVER_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.avail_addr = (q.avail_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_DEVICE_LOW => {
                if let Some(q) = self.selected_queue_mut() {
                    q.used_addr = (q.used_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DEVICE_HIGH => {
                if let Some(q) = self.selected_queue_mut() {
                    q.used_addr = (q.used_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            o if o >= VIRTIO_MMIO_CONFIG => {
                self.backend.config_write(o - VIRTIO_MMIO_CONFIG, val);
            }
            _ => {}
        }
    }
}

// ===========================================================================
// Virtio descriptor chain helpers
// ===========================================================================

pub(super) const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

#[derive(Clone, Copy)]
pub(super) struct VringDesc {
    pub(super) addr: u64,
    pub(super) len: u32,
    pub(super) flags: u16,
    pub(super) next: u16,
}

pub fn avail_idx(mem: &GuestMemoryMmap, avail_addr: u64) -> u16 {
    mem.read_obj::<u16>(GuestAddress(avail_addr + 2))
        .unwrap_or(0)
}

pub fn avail_ring_entry(mem: &GuestMemoryMmap, avail_addr: u64, queue_size: u16, idx: u16) -> u16 {
    let offset = 4 + (idx % queue_size) as u64 * 2;
    mem.read_obj::<u16>(GuestAddress(avail_addr + offset))
        .unwrap_or(0)
}

pub(super) fn read_desc(mem: &GuestMemoryMmap, desc_addr: u64, idx: u16) -> VringDesc {
    let off = idx as u64 * 16;
    VringDesc {
        addr: mem.read_obj(GuestAddress(desc_addr + off)).unwrap_or(0),
        len: mem.read_obj(GuestAddress(desc_addr + off + 8)).unwrap_or(0),
        flags: mem
            .read_obj(GuestAddress(desc_addr + off + 12))
            .unwrap_or(0),
        next: mem
            .read_obj(GuestAddress(desc_addr + off + 14))
            .unwrap_or(0),
    }
}

pub fn write_used(
    mem: &GuestMemoryMmap,
    used_addr: u64,
    queue_size: u16,
    used_idx: u16,
    desc_id: u32,
    len: u32,
) {
    let ring_off = 4 + (used_idx % queue_size) as u64 * 8;
    let _ = mem.write_obj(desc_id, GuestAddress(used_addr + ring_off));
    let _ = mem.write_obj(len, GuestAddress(used_addr + ring_off + 4));
    let _ = mem.write_obj(used_idx.wrapping_add(1), GuestAddress(used_addr + 2));
}

// ===========================================================================
// Virtio block backend (device ID 2)
// ===========================================================================

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
pub(super) const VIRTIO_F_VERSION_1: u64 = 1 << 32;

pub struct BlockBackend {
    disk: File,
    capacity: u64,
    read_only: bool,
    buf: Vec<u8>,
}

impl BlockBackend {
    pub fn new(path: &str, read_only: bool) -> std::io::Result<Self> {
        let disk = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let capacity = disk.metadata()?.len() / 512;
        Ok(BlockBackend {
            disk,
            capacity,
            read_only,
            buf: vec![0u8; 65536],
        })
    }
}

impl VirtioBackend for BlockBackend {
    fn device_id(&self) -> u32 {
        2
    }

    fn device_features(&self) -> u64 {
        let mut f = VIRTIO_F_VERSION_1;
        if self.read_only {
            f |= 1 << 5;
        }
        f
    }

    fn queue_count(&self) -> usize {
        1
    }
    fn queue_max_size(&self) -> u16 {
        256
    }

    fn config_read(&self, offset: u64) -> u32 {
        match offset {
            0 => self.capacity as u32,
            4 => (self.capacity >> 32) as u32,
            _ => 0,
        }
    }

    fn config_write(&mut self, _: u64, _: u32) {}

    fn activate(
        &mut self,
        _: &[VirtioQueueState],
        _: &GuestMemoryMmap,
        _: &Arc<VmFd>,
        _: u32,
        _: Arc<AtomicU32>,
    ) {
    }

    fn process_queue(
        &mut self,
        _queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
        _vm_fd: &Arc<VmFd>,
        _irq: u32,
    ) {
        let q = match queues.get_mut(0) {
            Some(q) if q.ready => q,
            _ => return,
        };

        let current_avail = avail_idx(mem, q.avail_addr);

        while q.last_avail_idx != current_avail {
            let head = avail_ring_entry(mem, q.avail_addr, q.size, q.last_avail_idx);

            let header_desc = read_desc(mem, q.desc_addr, head);
            let req_type = mem
                .read_obj::<u32>(GuestAddress(header_desc.addr))
                .unwrap_or(u32::MAX);
            let sector = mem
                .read_obj::<u64>(GuestAddress(header_desc.addr + 8))
                .unwrap_or(0);

            // Walk the descriptor chain inline — no Vec allocation.
            // Chain: header → data desc(s) → status (last, no NEXT flag).
            let mut disk_offset = sector * 512;
            let mut total_data_len: u32 = 0;
            let mut status = VIRTIO_BLK_S_OK;
            let mut status_addr: Option<u64> = None;
            let mut next = header_desc.next;
            let mut has_next = header_desc.flags & VRING_DESC_F_NEXT != 0;

            while has_next {
                let desc = read_desc(mem, q.desc_addr, next);
                let is_last = desc.flags & VRING_DESC_F_NEXT == 0;

                if is_last {
                    status_addr = Some(desc.addr);
                } else if status == VIRTIO_BLK_S_OK {
                    let len = desc.len as usize;
                    if self.buf.len() < len {
                        self.buf.resize(len, 0);
                    }
                    let buf = &mut self.buf[..len];

                    match req_type {
                        VIRTIO_BLK_T_IN => {
                            if self.disk.seek(SeekFrom::Start(disk_offset)).is_err()
                                || self.disk.read_exact(buf).is_err()
                            {
                                status = VIRTIO_BLK_S_IOERR;
                            } else {
                                let _ = mem.write_slice(buf, GuestAddress(desc.addr));
                            }
                        }
                        VIRTIO_BLK_T_OUT if !self.read_only => {
                            if mem.read_slice(buf, GuestAddress(desc.addr)).is_err()
                                || self.disk.seek(SeekFrom::Start(disk_offset)).is_err()
                                || self.disk.write_all(buf).is_err()
                            {
                                status = VIRTIO_BLK_S_IOERR;
                            }
                        }
                        _ => {
                            status = VIRTIO_BLK_S_IOERR;
                        }
                    }
                    disk_offset += len as u64;
                    total_data_len += desc.len;
                }

                next = desc.next;
                has_next = !is_last;
            }

            if let Some(addr) = status_addr {
                let _ = mem.write_obj(status, GuestAddress(addr));
            }
            write_used(
                mem,
                q.used_addr,
                q.size,
                q.last_avail_idx,
                head as u32,
                total_data_len + 1,
            );
            q.last_avail_idx = q.last_avail_idx.wrapping_add(1);
        }
    }

    fn reset(&mut self) {}
}

// ===========================================================================
// Virtio net backend (device ID 1) — socketpair with RX polling thread
// ===========================================================================

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_HDR_SIZE: usize = 12;

pub struct NetBackend {
    fd: RawFd,
    mac: [u8; 6],
    rx_running: Arc<AtomicBool>,
    rx_thread: Option<std::thread::JoinHandle<()>>,
}

impl NetBackend {
    pub fn new(fd: RawFd, mac: [u8; 6]) -> Self {
        NetBackend {
            fd,
            mac,
            rx_running: Arc::new(AtomicBool::new(false)),
            rx_thread: None,
        }
    }
}

impl VirtioBackend for NetBackend {
    fn device_id(&self) -> u32 {
        1
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC
    }

    fn queue_count(&self) -> usize {
        2
    }
    fn queue_max_size(&self) -> u16 {
        256
    }

    fn config_read(&self, offset: u64) -> u32 {
        match offset {
            0 => u32::from_le_bytes([self.mac[0], self.mac[1], self.mac[2], self.mac[3]]),
            4 => u16::from_le_bytes([self.mac[4], self.mac[5]]) as u32,
            _ => 0,
        }
    }

    fn config_write(&mut self, _: u64, _: u32) {}

    fn activate(
        &mut self,
        queues: &[VirtioQueueState],
        mem: &GuestMemoryMmap,
        vm_fd: &Arc<VmFd>,
        irq: u32,
        interrupt_status: Arc<AtomicU32>,
    ) {
        // Set fd to non-blocking
        unsafe {
            let flags = libc::fcntl(self.fd, libc::F_GETFL);
            libc::fcntl(self.fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Spawn RX polling thread
        let rx_q = match queues.first() {
            Some(q) if q.ready => q.clone(),
            _ => return,
        };

        self.rx_running.store(true, Ordering::Release);
        let running = self.rx_running.clone();
        let fd = self.fd;
        let mem = mem.clone();
        let vm_fd = vm_fd.clone();

        self.rx_thread = Some(
            std::thread::Builder::new()
                .name("shuru-net-rx".into())
                .spawn(move || {
                    net_rx_loop(fd, mem, rx_q, vm_fd, irq, interrupt_status, running);
                })
                .expect("failed to spawn net-rx thread"),
        );
    }

    fn process_queue(
        &mut self,
        queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
        _vm_fd: &Arc<VmFd>,
        _irq: u32,
    ) {
        if queue_idx != 1 {
            return; // RX handled by thread
        }
        let q = match queues.get_mut(1) {
            Some(q) if q.ready => q,
            _ => return,
        };

        let current_avail = avail_idx(mem, q.avail_addr);
        while q.last_avail_idx != current_avail {
            let head = avail_ring_entry(mem, q.avail_addr, q.size, q.last_avail_idx);

            let mut packet = Vec::new();
            let mut desc = read_desc(mem, q.desc_addr, head);
            loop {
                let mut buf = vec![0u8; desc.len as usize];
                let _ = mem.read_slice(&mut buf, GuestAddress(desc.addr));
                packet.extend_from_slice(&buf);
                if desc.flags & VRING_DESC_F_NEXT == 0 {
                    break;
                }
                desc = read_desc(mem, q.desc_addr, desc.next);
            }

            // Skip virtio-net header, send raw Ethernet frame
            if packet.len() > VIRTIO_NET_HDR_SIZE {
                let frame = &packet[VIRTIO_NET_HDR_SIZE..];
                unsafe {
                    libc::send(
                        self.fd,
                        frame.as_ptr() as *const _,
                        frame.len(),
                        libc::MSG_NOSIGNAL,
                    );
                }
            }

            write_used(mem, q.used_addr, q.size, q.last_avail_idx, head as u32, 0);
            q.last_avail_idx = q.last_avail_idx.wrapping_add(1);
        }
    }

    fn reset(&mut self) {
        self.rx_running.store(false, Ordering::Release);
        if let Some(h) = self.rx_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for NetBackend {
    fn drop(&mut self) {
        self.reset();
    }
}

/// RX loop: poll socketpair → push frames into guest RX queue → inject interrupt.
fn net_rx_loop(
    fd: RawFd,
    mem: GuestMemoryMmap,
    rx_q: VirtioQueueState,
    vm_fd: Arc<VmFd>,
    irq: u32,
    interrupt_status: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
) {
    let mut last_avail = rx_q.last_avail_idx;
    let mut used_idx: u16 = 0;
    // One Ethernet frame at the 65535-byte IP-MTU the proxy stack uses:
    // 14 bytes of header plus the IP payload. A datagram longer than the
    // buffer would be truncated by recv.
    let mut buf = vec![0u8; 14 + 65535];

    while running.load(Ordering::Acquire) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ret <= 0 {
            continue;
        }

        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
        if n <= 0 {
            continue;
        }
        let frame = &buf[..n as usize];

        // Check for available RX descriptors
        let current_avail = avail_idx(&mem, rx_q.avail_addr);
        if last_avail == current_avail {
            continue; // drop frame — no buffers
        }

        let head = avail_ring_entry(&mem, rx_q.avail_addr, rx_q.size, last_avail);
        let desc = read_desc(&mem, rx_q.desc_addr, head);

        let total = VIRTIO_NET_HDR_SIZE + frame.len();
        if (desc.flags & VRING_DESC_F_WRITE) != 0 && desc.len as usize >= total {
            // Write virtio-net header (zeros) + frame
            let header = [0u8; VIRTIO_NET_HDR_SIZE];
            let _ = mem.write_slice(&header, GuestAddress(desc.addr));
            let _ = mem.write_slice(frame, GuestAddress(desc.addr + VIRTIO_NET_HDR_SIZE as u64));

            write_used(
                &mem,
                rx_q.used_addr,
                rx_q.size,
                used_idx,
                head as u32,
                total as u32,
            );
            used_idx = used_idx.wrapping_add(1);
            last_avail = last_avail.wrapping_add(1);

            // Signal the guest
            interrupt_status.fetch_or(1, Ordering::Release);
            let _ = vm_fd.set_irq_line(irq, true);
        }
    }
}

// ===========================================================================
// Vhost-vsock backend (device ID 19)
// ===========================================================================

pub struct VhostVsockBackend {
    guest_cid: u64,
    vhost_fd: Option<RawFd>,
    kick_evts: Vec<EventFd>,
    call_evts: Vec<EventFd>,
    irq_running: Arc<AtomicBool>,
    irq_thread: Option<std::thread::JoinHandle<()>>,
}

impl VhostVsockBackend {
    pub fn new(guest_cid: u64) -> Self {
        VhostVsockBackend {
            guest_cid,
            vhost_fd: None,
            kick_evts: Vec::new(),
            call_evts: Vec::new(),
            irq_running: Arc::new(AtomicBool::new(false)),
            irq_thread: None,
        }
    }
}

// Vhost ioctl encoding
const VHOST_VIRTIO: u64 = 0xAF;

const fn vhost_io(nr: u64) -> u64 {
    (VHOST_VIRTIO << 8) | nr
}

const fn vhost_iow_size(nr: u64, size: usize) -> u64 {
    (1u64 << 30) | ((size as u64) << 16) | (VHOST_VIRTIO << 8) | nr
}

const VHOST_SET_OWNER: u64 = vhost_io(0x01);
const VHOST_SET_FEATURES: u64 = vhost_iow_size(0x00, 8); // sizeof(u64)
const VHOST_SET_MEM_TABLE: u64 = vhost_iow_size(0x03, 8); // sizeof(vhost_memory header)
const VHOST_SET_VRING_NUM: u64 = vhost_iow_size(0x10, 8); // sizeof(vhost_vring_state)
const VHOST_SET_VRING_ADDR: u64 = vhost_iow_size(0x11, 40); // sizeof(vhost_vring_addr) = 4+4+8+8+8+8
const VHOST_SET_VRING_BASE: u64 = vhost_iow_size(0x12, 8);
const VHOST_SET_VRING_KICK: u64 = vhost_iow_size(0x20, 8); // sizeof(vhost_vring_file)
const VHOST_SET_VRING_CALL: u64 = vhost_iow_size(0x21, 8);
const VHOST_VSOCK_SET_GUEST_CID: u64 = vhost_iow_size(0x60, 8);
// _IOW(0xAF, 0x61, int) — starts/stops the vhost worker
const VHOST_VSOCK_SET_RUNNING: u64 = vhost_iow_size(0x61, 4);

#[repr(C)]
struct VhostVringState {
    index: u32,
    num: u32,
}

#[repr(C)]
struct VhostVringAddr {
    index: u32,
    flags: u32,
    desc_user_addr: u64,
    used_user_addr: u64,
    avail_user_addr: u64,
    log_guest_addr: u64,
}

#[repr(C)]
struct VhostVringFile {
    index: u32,
    fd: i32,
}

#[repr(C)]
struct VhostMemoryHeader {
    nregions: u32,
    _padding: u32,
}

#[repr(C)]
struct VhostMemoryRegion {
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
    _flags_padding: u64,
}

#[repr(C)]
struct VhostMemTable {
    header: VhostMemoryHeader,
    region: VhostMemoryRegion,
}

fn guest_to_host(mem: &GuestMemoryMmap, guest_addr: u64) -> u64 {
    mem.get_host_address(GuestAddress(guest_addr))
        .map(|p| p as u64)
        .unwrap_or(0)
}

impl VirtioBackend for VhostVsockBackend {
    fn device_id(&self) -> u32 {
        19
    }

    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    fn queue_count(&self) -> usize {
        3
    }
    fn queue_max_size(&self) -> u16 {
        256
    }

    fn config_read(&self, offset: u64) -> u32 {
        match offset {
            0 => self.guest_cid as u32,
            4 => (self.guest_cid >> 32) as u32,
            _ => 0,
        }
    }

    fn config_write(&mut self, _: u64, _: u32) {}

    fn activate(
        &mut self,
        queues: &[VirtioQueueState],
        mem: &GuestMemoryMmap,
        vm_fd: &Arc<VmFd>,
        irq: u32,
        _interrupt_status: Arc<AtomicU32>,
    ) {
        let vhost_fd = unsafe {
            libc::open(
                b"/dev/vhost-vsock\0".as_ptr() as *const libc::c_char,
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };
        if vhost_fd < 0 {
            eprintln!(
                "shuru: failed to open /dev/vhost-vsock: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        self.vhost_fd = Some(vhost_fd);

        unsafe {
            // Exact order matching Cloud Hypervisor / Firecracker:
            // 1. OWNER → 2. FEATURES → 3. MEM_TABLE → 4. queues (NUM, ADDR, BASE, CALL, KICK) → 5. CID → 6. IRQFD
            if libc::ioctl(vhost_fd, VHOST_SET_OWNER) < 0 {
                eprintln!(
                    "shuru: VHOST_SET_OWNER failed: {}",
                    std::io::Error::last_os_error()
                );
                return;
            }

            // Negotiate features — get supported, intersect, set
            let get_features: u64 = (2u64 << 30) | (8u64 << 16) | (VHOST_VIRTIO << 8) | 0x00;
            let mut avail_features: u64 = 0;
            libc::ioctl(vhost_fd, get_features, &mut avail_features);
            let features = avail_features & VIRTIO_F_VERSION_1;
            if libc::ioctl(vhost_fd, VHOST_SET_FEATURES, &features) < 0 {
                eprintln!(
                    "shuru: VHOST_SET_FEATURES failed: {}",
                    std::io::Error::last_os_error()
                );
                return;
            }

            // Memory table
            let host_addr = guest_to_host(mem, layout::DRAM_BASE);
            let region = mem.find_region(GuestAddress(layout::DRAM_BASE));
            if let Some(region) = region {
                use vm_memory::GuestMemoryRegion;
                let mem_table = VhostMemTable {
                    header: VhostMemoryHeader {
                        nregions: 1,
                        _padding: 0,
                    },
                    region: VhostMemoryRegion {
                        guest_phys_addr: region.start_addr().raw_value(),
                        memory_size: region.len(),
                        userspace_addr: host_addr,
                        _flags_padding: 0,
                    },
                };
                if libc::ioctl(vhost_fd, VHOST_SET_MEM_TABLE, &mem_table) < 0 {
                    eprintln!(
                        "shuru: VHOST_SET_MEM_TABLE failed: {}",
                        std::io::Error::last_os_error()
                    );
                    return;
                }
            }

            // Set up vrings — only RX (0) and TX (1)
            for (i, q) in queues.iter().enumerate().take(2) {
                if !q.ready || q.size == 0 {
                    continue;
                }

                let vring_num = VhostVringState {
                    index: i as u32,
                    num: q.size as u32,
                };
                libc::ioctl(vhost_fd, VHOST_SET_VRING_NUM, &vring_num);

                let vring_addr = VhostVringAddr {
                    index: i as u32,
                    flags: 0,
                    desc_user_addr: guest_to_host(mem, q.desc_addr),
                    used_user_addr: guest_to_host(mem, q.used_addr),
                    avail_user_addr: guest_to_host(mem, q.avail_addr),
                    log_guest_addr: 0,
                };
                libc::ioctl(vhost_fd, VHOST_SET_VRING_ADDR, &vring_addr);

                let vring_base = VhostVringState {
                    index: i as u32,
                    num: 0,
                };
                libc::ioctl(vhost_fd, VHOST_SET_VRING_BASE, &vring_base);

                let kick_evt = EventFd::new(0).expect("eventfd");
                let call_evt = EventFd::new(0).expect("eventfd");

                let call_file = VhostVringFile {
                    index: i as u32,
                    fd: call_evt.as_raw_fd(),
                };
                libc::ioctl(vhost_fd, VHOST_SET_VRING_CALL, &call_file);

                // KICK last — starts the vhost data plane
                let kick_file = VhostVringFile {
                    index: i as u32,
                    fd: kick_evt.as_raw_fd(),
                };
                libc::ioctl(vhost_fd, VHOST_SET_VRING_KICK, &kick_file);

                self.kick_evts.push(kick_evt);
                self.call_evts.push(call_evt);
            }

            // CID AFTER queue setup
            if libc::ioctl(vhost_fd, VHOST_VSOCK_SET_GUEST_CID, &self.guest_cid) < 0 {
                eprintln!(
                    "shuru: VHOST_VSOCK_SET_GUEST_CID failed: {}",
                    std::io::Error::last_os_error()
                );
                return;
            }

            // START the vhost worker — without this, no queues are processed!
            let running: libc::c_int = 1;
            if libc::ioctl(vhost_fd, VHOST_VSOCK_SET_RUNNING, &running) < 0 {
                eprintln!(
                    "shuru: VHOST_VSOCK_SET_RUNNING failed: {}",
                    std::io::Error::last_os_error()
                );
                return;
            }
            // Vhost worker is now running and processing queues.

            // Can't use KVM_IRQFD because we need to set InterruptStatus
            // BEFORE the interrupt reaches the guest.
            self.irq_running.store(true, Ordering::Release);
            let irq_running = self.irq_running.clone();
            let vm_fd_clone = vm_fd.clone();
            let call_fd_0 = self.call_evts.get(0).map(|e| e.as_raw_fd()).unwrap_or(-1);
            let call_fd_1 = self.call_evts.get(1).map(|e| e.as_raw_fd()).unwrap_or(-1);
            self.irq_thread = std::thread::Builder::new()
                .name("vsock-irq".into())
                .spawn(move || {
                    let mut pfds = [
                        libc::pollfd {
                            fd: call_fd_0,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: call_fd_1,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    while irq_running.load(Ordering::Acquire) {
                        let ret = libc::poll(pfds.as_mut_ptr(), 2, 500);
                        if ret > 0 {
                            for pfd in &mut pfds {
                                if pfd.revents & (libc::POLLIN | libc::POLLNVAL) == libc::POLLIN {
                                    let mut val: u64 = 0;
                                    libc::read(pfd.fd, &mut val as *mut _ as *mut libc::c_void, 8);
                                    _interrupt_status.fetch_or(1, Ordering::Release);
                                    let _ = vm_fd_clone.set_irq_line(irq, true);
                                }
                            }
                        }
                    }
                })
                .ok();
        }
    }

    fn process_queue(
        &mut self,
        queue_idx: u16,
        _queues: &mut [VirtioQueueState],
        _mem: &GuestMemoryMmap,
        _vm_fd: &Arc<VmFd>,
        _irq: u32,
    ) {
        // Signal the kick eventfd so the vhost kernel module processes the queue.
        if let Some(evt) = self.kick_evts.get(queue_idx as usize) {
            let _ = evt.write(1);
        }
    }

    fn reset(&mut self) {
        self.irq_running.store(false, Ordering::Release);
        if let Some(h) = self.irq_thread.take() {
            let _ = h.join();
        }
        self.kick_evts.clear();
        self.call_evts.clear();
        if let Some(fd) = self.vhost_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

impl Drop for VhostVsockBackend {
    fn drop(&mut self) {
        self.reset();
    }
}
