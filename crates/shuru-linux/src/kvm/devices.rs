use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kvm_ioctls::{IoEventAddress, VmFd};
use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use super::layout;

// ===========================================================================
// MMIO device traits + bus
// ===========================================================================

/// A device whose registers are guarded as a whole.
pub trait MmioDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]);
    fn write(&mut self, offset: u64, data: &[u8]);
}

/// What the bus dispatches to. Every device does its own locking, so vCPUs
/// touching different devices never wait on each other.
pub trait MmioHandler: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

impl<T: MmioDevice> MmioHandler for Mutex<T> {
    fn read(&self, offset: u64, data: &mut [u8]) {
        self.lock().unwrap().read(offset, data);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        self.lock().unwrap().write(offset, data);
    }
}

pub struct MmioEntry {
    pub base: u64,
    pub size: u64,
    pub device: Box<dyn MmioHandler>,
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

    pub fn add(&mut self, base: u64, size: u64, device: Box<dyn MmioHandler>) {
        self.devices.push(MmioEntry { base, size, device });
    }

    pub fn handle_read(&self, addr: u64, data: &mut [u8]) {
        for entry in &self.devices {
            if addr >= entry.base && addr < entry.base + entry.size {
                entry.device.read(addr - entry.base, data);
                return;
            }
        }
        for b in data.iter_mut() {
            *b = 0;
        }
    }

    pub fn handle_write(&self, addr: u64, data: &[u8]) {
        for entry in &self.devices {
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
const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Upper bound for every device's queue size, so chain walks can track
/// visited descriptors in a fixed array.
const MAX_QUEUE_SIZE: u16 = 256;

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
        assert!(max_size <= MAX_QUEUE_SIZE);
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

    /// The avail index to process up to, or None when the queue is not set
    /// up in a way the device can use: size outside 1..=max_size, rings
    /// outside guest memory, or an avail index more than one ring ahead.
    pub fn pending(&self, mem: &GuestMemoryMmap) -> Option<u16> {
        if !self.ready || self.size == 0 || self.size > self.max_size {
            return None;
        }
        let size = self.size as usize;
        if !mem.check_range(GuestAddress(self.desc_addr), size * 16)
            || !mem.check_range(GuestAddress(self.avail_addr), 6 + size * 2)
            || !mem.check_range(GuestAddress(self.used_addr), 6 + size * 8)
        {
            return None;
        }
        let current = avail_idx(mem, self.avail_addr);
        if current.wrapping_sub(self.last_avail_idx) > self.size {
            return None;
        }
        Some(current)
    }

    /// The descriptors of the chain at `head`, or None if it leaves the
    /// table, loops, or carries a flag outside `flags`. Only valid after
    /// `pending` accepted the queue.
    pub(super) fn chain(
        &self,
        mem: &GuestMemoryMmap,
        head: u16,
        flags: u16,
    ) -> Option<Vec<VringDesc>> {
        let mut chain = Vec::new();
        let mut seen = [false; MAX_QUEUE_SIZE as usize];
        let mut next = head;
        loop {
            if next >= self.size || seen[next as usize] {
                return None;
            }
            seen[next as usize] = true;
            let desc = read_desc(mem, self.desc_addr, next);
            if desc.flags & !flags != 0 {
                return None;
            }
            chain.push(desc);
            if desc.flags & VRING_DESC_F_NEXT == 0 {
                return Some(chain);
            }
            next = desc.next;
        }
    }

    /// Whether the driver wants an interrupt for buffers put on this used ring.
    pub fn wants_interrupt(&self, mem: &GuestMemoryMmap) -> bool {
        // The used index must be visible to the driver before its flag is read.
        fence(Ordering::SeqCst);
        mem.read_obj::<u16>(GuestAddress(self.avail_addr))
            .map_or(true, |flags| {
                u16::from_le(flags) & VRING_AVAIL_F_NO_INTERRUPT == 0
            })
    }
}

pub const INTERRUPT_USED: u32 = 1;
pub const INTERRUPT_CONFIG: u32 = 2;

/// A device interrupt line and the status register that says why it fired.
/// Both change under one lock so an acknowledgement cannot lower the line
/// over a reason raised in the meantime.
pub struct Interrupt {
    vm_fd: Arc<VmFd>,
    irq: u32,
    status: Mutex<u32>,
}

impl Interrupt {
    fn new(vm_fd: Arc<VmFd>, irq: u32) -> Self {
        Interrupt {
            vm_fd,
            irq,
            status: Mutex::new(0),
        }
    }

    pub fn raise(&self, reason: u32) {
        let mut status = self.status.lock().unwrap();
        *status |= reason;
        let _ = self.vm_fd.set_irq_line(self.irq, true);
    }

    fn status(&self) -> u32 {
        *self.status.lock().unwrap()
    }

    fn ack(&self, reasons: u32) {
        let mut status = self.status.lock().unwrap();
        *status &= !reasons;
        if *status == 0 {
            let _ = self.vm_fd.set_irq_line(self.irq, false);
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
    /// The feature set the driver accepted, once it sets FEATURES_OK.
    fn features_ok(&mut self, _features: u64) {}
    fn activate(
        &mut self,
        queues: &[VirtioQueueState],
        mem: &GuestMemoryMmap,
        interrupt: Arc<Interrupt>,
    );
    /// Returns whether any buffer went onto a used ring.
    fn process_queue(
        &mut self,
        queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
    ) -> bool;
    /// A backend with a worker of its own takes the queue kicks, one per
    /// queue in queue order, and the transport leaves its queues alone.
    fn take_kicks(&mut self, _kicks: &[EventFd]) -> io::Result<bool> {
        Ok(false)
    }
    fn reset(&mut self);
}

struct Core {
    backend: Box<dyn VirtioBackend>,
    queues: Vec<VirtioQueueState>,
    mem: GuestMemoryMmap,
    dev_features_sel: u32,
    drv_features: u64,
    drv_features_sel: u32,
    queue_sel: u32,
    status: u32,
    activated: bool,
}

impl Core {
    fn selected_queue(&self) -> Option<&VirtioQueueState> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut VirtioQueueState> {
        self.queues.get_mut(self.queue_sel as usize)
    }
}

/// Guest kicks land on an eventfd per queue inside KVM, so a kick never
/// leaves the kernel and never holds up the vCPU that made it. The queues
/// are served by the backend worker or by a thread per device.
pub struct VirtioMmioDevice {
    core: Arc<Mutex<Core>>,
    interrupt: Arc<Interrupt>,
    vm_fd: Arc<VmFd>,
    notify: IoEventAddress,
    kicks: Vec<EventFd>,
    stop: EventFd,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl VirtioMmioDevice {
    pub fn new(
        backend: Box<dyn VirtioBackend>,
        base: u64,
        spi: u32,
        vm_fd: Arc<VmFd>,
        mem: GuestMemoryMmap,
    ) -> io::Result<Self> {
        let queue_count = backend.queue_count();
        let queue_max = backend.queue_max_size();
        let mut device = VirtioMmioDevice {
            core: Arc::new(Mutex::new(Core {
                backend,
                queues: (0..queue_count)
                    .map(|_| VirtioQueueState::new(queue_max))
                    .collect(),
                mem,
                dev_features_sel: 0,
                drv_features: 0,
                drv_features_sel: 0,
                queue_sel: 0,
                status: 0,
                activated: false,
            })),
            // KVM_IRQ_LINE on ARM: SPI N maps to intid N+32
            interrupt: Arc::new(Interrupt::new(vm_fd.clone(), layout::spi_to_irq(spi))),
            vm_fd,
            notify: IoEventAddress::Mmio(base + VIRTIO_MMIO_QUEUE_NOTIFY),
            kicks: Vec::with_capacity(queue_count),
            stop: EventFd::new(libc::EFD_CLOEXEC)?,
            worker: None,
        };
        for index in 0..queue_count {
            let kick = EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?;
            device
                .vm_fd
                .register_ioevent(&kick, &device.notify, index as u32)
                .map_err(io::Error::other)?;
            device.kicks.push(kick);
        }
        let owned = device
            .core
            .lock()
            .unwrap()
            .backend
            .take_kicks(&device.kicks)?;
        if !owned {
            let kicks = device
                .kicks
                .iter()
                .map(EventFd::try_clone)
                .collect::<io::Result<Vec<_>>>()?;
            let stop = device.stop.try_clone()?;
            let core = device.core.clone();
            let interrupt = device.interrupt.clone();
            device.worker = Some(
                std::thread::Builder::new()
                    .name("shuru-virtio".into())
                    .spawn(move || serve(core, interrupt, kicks, stop))?,
            );
        }
        Ok(device)
    }

    fn kick_all(&self) {
        for kick in &self.kicks {
            let _ = kick.write(1);
        }
    }
}

fn serve(core: Arc<Mutex<Core>>, interrupt: Arc<Interrupt>, kicks: Vec<EventFd>, stop: EventFd) {
    let mut fds: Vec<libc::pollfd> = std::iter::once(&stop)
        .chain(&kicks)
        .map(|event| libc::pollfd {
            fd: event.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    loop {
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[0].revents != 0 {
            return;
        }
        for (index, kick) in kicks.iter().enumerate() {
            if fds[index + 1].revents & libc::POLLIN == 0 {
                continue;
            }
            let _ = kick.read();
            let mut core = core.lock().unwrap();
            let Core {
                backend,
                queues,
                mem,
                activated,
                ..
            } = &mut *core;
            if *activated
                && backend.process_queue(index as u16, queues, mem)
                && queues[index].wants_interrupt(mem)
            {
                interrupt.raise(INTERRUPT_USED);
            }
        }
    }
}

impl Drop for VirtioMmioDevice {
    fn drop(&mut self) {
        let _ = self.stop.write(1);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        for (index, kick) in self.kicks.iter().enumerate() {
            let _ = self
                .vm_fd
                .unregister_ioevent(kick, &self.notify, index as u32);
        }
    }
}

impl MmioHandler for VirtioMmioDevice {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let val: u32 = if offset == VIRTIO_MMIO_INTERRUPT_STATUS {
            self.interrupt.status()
        } else {
            let core = self.core.lock().unwrap();
            match offset {
                VIRTIO_MMIO_MAGIC => 0x7472_6976,
                VIRTIO_MMIO_VERSION => 2,
                VIRTIO_MMIO_DEVICE_ID => core.backend.device_id(),
                VIRTIO_MMIO_VENDOR_ID => 0x554D_4551,
                VIRTIO_MMIO_DEV_FEATURES => {
                    let features = core.backend.device_features();
                    if core.dev_features_sel == 0 {
                        features as u32
                    } else {
                        (features >> 32) as u32
                    }
                }
                VIRTIO_MMIO_QUEUE_NUM_MAX => core.selected_queue().map_or(0, |q| q.max_size as u32),
                VIRTIO_MMIO_QUEUE_READY => core.selected_queue().map_or(0, |q| q.ready as u32),
                VIRTIO_MMIO_STATUS => core.status,
                VIRTIO_MMIO_CONFIG_GENERATION => 0,
                o if o >= VIRTIO_MMIO_CONFIG => core.backend.config_read(o - VIRTIO_MMIO_CONFIG),
                _ => 0,
            }
        };
        let len = data.len().min(4);
        data[..len].copy_from_slice(&val.to_le_bytes()[..len]);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let mut bytes = [0u8; 4];
        let len = data.len().min(4);
        bytes[..len].copy_from_slice(&data[..len]);
        let val = u32::from_le_bytes(bytes);

        match offset {
            VIRTIO_MMIO_INTERRUPT_ACK => return self.interrupt.ack(val),
            // Matching kicks never get here; KVM signals the eventfd itself.
            VIRTIO_MMIO_QUEUE_NOTIFY => return,
            _ => {}
        }

        let mut core = self.core.lock().unwrap();
        match offset {
            VIRTIO_MMIO_DEV_FEATURES_SEL => core.dev_features_sel = val,
            VIRTIO_MMIO_DRV_FEATURES => {
                if core.drv_features_sel == 0 {
                    core.drv_features = (core.drv_features & 0xFFFF_FFFF_0000_0000) | val as u64;
                } else {
                    core.drv_features =
                        (core.drv_features & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_DRV_FEATURES_SEL => core.drv_features_sel = val,
            VIRTIO_MMIO_QUEUE_SEL => core.queue_sel = val,
            VIRTIO_MMIO_QUEUE_NUM => {
                if let Some(q) = core.selected_queue_mut() {
                    q.size = if (1..=q.max_size as u32).contains(&val) {
                        val as u16
                    } else {
                        0
                    };
                }
            }
            VIRTIO_MMIO_QUEUE_READY => {
                if let Some(q) = core.selected_queue_mut() {
                    q.ready = val == 1;
                }
            }
            VIRTIO_MMIO_STATUS => {
                let prev = core.status;
                core.status = val;
                if val == 0 {
                    core.backend.reset();
                    core.activated = false;
                    for q in &mut core.queues {
                        *q = VirtioQueueState::new(q.max_size);
                    }
                    self.interrupt.ack(u32::MAX);
                }
                if (val & VIRTIO_STATUS_FEATURES_OK) != 0 && (prev & VIRTIO_STATUS_FEATURES_OK) == 0
                {
                    let features = core.drv_features & core.backend.device_features();
                    core.backend.features_ok(features);
                }
                if !core.activated
                    && (val & VIRTIO_STATUS_DRIVER_OK) != 0
                    && (val & VIRTIO_STATUS_FEATURES_OK) != 0
                {
                    core.activated = true;
                    let Core {
                        backend,
                        queues,
                        mem,
                        ..
                    } = &mut *core;
                    backend.activate(queues, mem, self.interrupt.clone());
                    drop(core);
                    // Anything queued before the driver finished setting up.
                    self.kick_all();
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                if let Some(q) = core.selected_queue_mut() {
                    q.desc_addr = (q.desc_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                if let Some(q) = core.selected_queue_mut() {
                    q.desc_addr = (q.desc_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_DRIVER_LOW => {
                if let Some(q) = core.selected_queue_mut() {
                    q.avail_addr = (q.avail_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DRIVER_HIGH => {
                if let Some(q) = core.selected_queue_mut() {
                    q.avail_addr = (q.avail_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            VIRTIO_MMIO_QUEUE_DEVICE_LOW => {
                if let Some(q) = core.selected_queue_mut() {
                    q.used_addr = (q.used_addr & !0xFFFF_FFFF) | val as u64;
                }
            }
            VIRTIO_MMIO_QUEUE_DEVICE_HIGH => {
                if let Some(q) = core.selected_queue_mut() {
                    q.used_addr = (q.used_addr & 0xFFFF_FFFF) | ((val as u64) << 32);
                }
            }
            o if o >= VIRTIO_MMIO_CONFIG => {
                core.backend.config_write(o - VIRTIO_MMIO_CONFIG, val);
            }
            _ => {}
        }
    }
}

// ===========================================================================
// Virtio descriptor chain helpers
// ===========================================================================

pub(super) const VRING_DESC_F_NEXT: u16 = 1;
pub(super) const VRING_DESC_F_WRITE: u16 = 2;

#[derive(Clone, Copy)]
pub(super) struct VringDesc {
    pub(super) addr: u64,
    pub(super) len: u32,
    pub(super) flags: u16,
    pub(super) next: u16,
}

pub fn avail_idx(mem: &GuestMemoryMmap, avail_addr: u64) -> u16 {
    let idx = mem
        .read_obj::<u16>(GuestAddress(avail_addr + 2))
        .unwrap_or(0);
    // Ring entries the index covers must not be read before the index.
    fence(Ordering::Acquire);
    idx
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
    fence(Ordering::Release);
    let _ = mem.write_obj(used_idx.wrapping_add(1), GuestAddress(used_addr + 2));
}

// ===========================================================================
// Virtio block backend (device ID 2)
// ===========================================================================

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

#[path = "block_ranges.rs"]
mod block_ranges;
use block_ranges::*;
pub(super) const VIRTIO_F_VERSION_1: u64 = 1 << 32;

pub struct BlockBackend {
    disk: File,
    capacity: u64,
    read_only: bool,
    negotiated: u64,
    buf: Vec<u8>,
}

impl BlockBackend {
    pub fn new(disk: File, read_only: bool) -> std::io::Result<Self> {
        let capacity = disk.metadata()?.len() / 512;
        Ok(BlockBackend {
            disk,
            capacity,
            read_only,
            negotiated: 0,
            buf: vec![0u8; 65536],
        })
    }
    fn request(&mut self, mem: &GuestMemoryMmap, chain: &[VringDesc]) -> Result<u32, u8> {
        let fail = VIRTIO_BLK_S_IOERR;
        if chain.len() < 2 {
            return Err(fail);
        }
        let header = &chain[0];
        if header.len != 16 || header.flags & VRING_DESC_F_WRITE != 0 {
            return Err(fail);
        }
        let mut bytes = [0; 16];
        mem.read_slice(&mut bytes, GuestAddress(header.addr))
            .map_err(|_| fail)?;
        let kind = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let sector = u64::from_le_bytes(bytes[8..].try_into().unwrap());
        let data = &chain[1..chain.len() - 1];
        let mut total = 0usize;
        for desc in data {
            if (desc.flags & VRING_DESC_F_WRITE != 0) != (kind == VIRTIO_BLK_T_IN) {
                return Err(fail);
            }
            if desc.len > 1024 * 1024
                || !mem.check_range(GuestAddress(desc.addr), desc.len as usize)
            {
                return Err(fail);
            }
            total = total.checked_add(desc.len as usize).ok_or(fail)?;
        }
        match kind {
            VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
                if self.read_only
                    || total == 0
                    || total > MAX_RANGES * 16
                    || !total.is_multiple_of(16)
                    || sector != 0
                {
                    return Err(fail);
                }
                let feature = if kind == VIRTIO_BLK_T_DISCARD { 13 } else { 14 };
                if self.negotiated & (1 << feature) == 0 {
                    return Err(VIRTIO_BLK_S_UNSUPP);
                }
                let mut payload = [0u8; MAX_RANGES * 16];
                let mut at = 0;
                for desc in data {
                    let end = at + desc.len as usize;
                    mem.read_slice(&mut payload[at..end], GuestAddress(desc.addr))
                        .map_err(|_| fail)?;
                    at = end;
                }
                let ranges = decode_ranges(&payload[..total], self.capacity, kind)?;
                for (offset, len, unmap) in ranges {
                    if len == 0 {
                        continue;
                    }
                    let mode = if kind == VIRTIO_BLK_T_DISCARD || unmap {
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE
                    } else {
                        libc::FALLOC_FL_ZERO_RANGE | libc::FALLOC_FL_KEEP_SIZE
                    };
                    if unsafe { libc::fallocate(self.disk.as_raw_fd(), mode, offset, len) } < 0 {
                        return Err(fail);
                    }
                }
                Ok(0)
            }
            VIRTIO_BLK_T_IN | VIRTIO_BLK_T_OUT => {
                if kind == VIRTIO_BLK_T_OUT && self.read_only {
                    return Err(fail);
                }
                if !total.is_multiple_of(512)
                    || sector > self.capacity
                    || total as u64 / 512 > self.capacity - sector
                {
                    return Err(fail);
                }
                self.disk
                    .seek(SeekFrom::Start(sector.checked_mul(512).ok_or(fail)?))
                    .map_err(|_| fail)?;
                let mut written = 0;
                for desc in data {
                    let mut at = 0usize;
                    while at < desc.len as usize {
                        let n = self.buf.len().min(desc.len as usize - at);
                        let buf = &mut self.buf[..n];
                        let addr = GuestAddress(desc.addr.checked_add(at as u64).ok_or(fail)?);
                        if kind == VIRTIO_BLK_T_IN {
                            self.disk.read_exact(buf).map_err(|_| fail)?;
                            mem.write_slice(buf, addr).map_err(|_| fail)?;
                            written += n as u32;
                        } else {
                            mem.read_slice(buf, addr).map_err(|_| fail)?;
                            self.disk.write_all(buf).map_err(|_| fail)?;
                        }
                        at += n;
                    }
                }
                Ok(written)
            }
            _ => Err(VIRTIO_BLK_S_UNSUPP),
        }
    }
}

impl VirtioBackend for BlockBackend {
    fn device_id(&self) -> u32 {
        2
    }

    fn device_features(&self) -> u64 {
        let mut f = VIRTIO_F_VERSION_1 | (1 << 1) | (1 << 2);
        if self.read_only {
            f |= 1 << 5;
        } else {
            f |= (1 << 13) | (1 << 14);
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
            8 => 1024 * 1024,
            12 => 254,
            36 | 48 if !self.read_only => MAX_RANGE_SECTORS,
            40 | 52 if !self.read_only => MAX_RANGES as u32,
            44 if !self.read_only => 8,
            56 if !self.read_only => 1,
            _ => 0,
        }
    }

    fn config_write(&mut self, _: u64, _: u32) {}

    fn features_ok(&mut self, features: u64) {
        self.negotiated = features;
    }

    fn activate(&mut self, _: &[VirtioQueueState], _: &GuestMemoryMmap, _: Arc<Interrupt>) {}

    fn process_queue(
        &mut self,
        _queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
    ) -> bool {
        let q = match queues.get_mut(0) {
            Some(q) => q,
            None => return false,
        };
        let current_avail = match q.pending(mem) {
            Some(idx) => idx,
            None => return false,
        };
        let used = q.last_avail_idx != current_avail;
        while q.last_avail_idx != current_avail {
            let head = avail_ring_entry(mem, q.avail_addr, q.size, q.last_avail_idx);
            let chain = q
                .chain(mem, head, VRING_DESC_F_NEXT | VRING_DESC_F_WRITE)
                .unwrap_or_default();
            let status_desc = chain.last().filter(|d| {
                chain.len() >= 2
                    && d.flags == VRING_DESC_F_WRITE
                    && d.len >= 1
                    && mem.check_range(GuestAddress(d.addr), 1)
            });
            let mut used_len = 0;
            if let Some(desc) = status_desc {
                let result = self.request(mem, &chain);
                let status = match result {
                    Ok(n) => {
                        used_len = n;
                        VIRTIO_BLK_S_OK
                    }
                    Err(status) => status,
                };
                let _ = mem.write_obj(status, GuestAddress(desc.addr));
                used_len += 1;
            }
            write_used(
                mem,
                q.used_addr,
                q.size,
                q.last_avail_idx,
                head as u32,
                used_len,
            );
            q.last_avail_idx = q.last_avail_idx.wrapping_add(1);
        }
        used
    }

    fn reset(&mut self) {
        self.negotiated = 0;
    }
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
    tx_buf: Vec<u8>,
}

// The virtio-net header plus one Ethernet frame at the 65535-byte IP MTU.
const MAX_TX_FRAME: usize = VIRTIO_NET_HDR_SIZE + 14 + 65535;

impl NetBackend {
    pub fn new(fd: RawFd, mac: [u8; 6]) -> Self {
        NetBackend {
            fd,
            mac,
            rx_running: Arc::new(AtomicBool::new(false)),
            tx_buf: vec![0; MAX_TX_FRAME],
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
        interrupt: Arc<Interrupt>,
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

        self.rx_thread = Some(
            std::thread::Builder::new()
                .name("shuru-net-rx".into())
                .spawn(move || {
                    net_rx_loop(fd, mem, rx_q, interrupt, running);
                })
                .expect("failed to spawn net-rx thread"),
        );
    }

    fn process_queue(
        &mut self,
        queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
    ) -> bool {
        if queue_idx != 1 {
            return false; // RX handled by thread
        }
        let q = match queues.get_mut(1) {
            Some(q) => q,
            None => return false,
        };
        let current_avail = match q.pending(mem) {
            Some(idx) => idx,
            None => return false,
        };
        let used = q.last_avail_idx != current_avail;
        while q.last_avail_idx != current_avail {
            let head = avail_ring_entry(mem, q.avail_addr, q.size, q.last_avail_idx);

            // A chain that loops, is longer than one frame, or points
            // outside guest memory is dropped.
            let mut len = 0;
            for desc in q.chain(mem, head, VRING_DESC_F_NEXT).unwrap_or_default() {
                let end = len + desc.len as usize;
                if end > MAX_TX_FRAME
                    || mem
                        .read_slice(&mut self.tx_buf[len..end], GuestAddress(desc.addr))
                        .is_err()
                {
                    len = 0;
                    break;
                }
                len = end;
            }

            // Skip virtio-net header, send raw Ethernet frame
            if len > VIRTIO_NET_HDR_SIZE {
                let frame = &self.tx_buf[VIRTIO_NET_HDR_SIZE..len];
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
        used
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
    interrupt: Arc<Interrupt>,
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
        let mut ring = rx_q.clone();
        ring.last_avail_idx = last_avail;
        let current_avail = match ring.pending(&mem) {
            Some(idx) => idx,
            None => continue,
        };
        if last_avail == current_avail {
            continue; // drop frame — no buffers
        }

        let head = avail_ring_entry(&mem, rx_q.avail_addr, rx_q.size, last_avail);
        if head >= rx_q.size {
            last_avail = last_avail.wrapping_add(1);
            continue;
        }
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

            if rx_q.wants_interrupt(&mem) {
                interrupt.raise(INTERRUPT_USED);
            }
        }
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    fn backend() -> BlockBackend {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "shuru-block-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file.set_len(16384).unwrap();
        file.write_all_at(&vec![7; 16384], 0).unwrap();
        let mut disk = BlockBackend::new(file, false).unwrap();
        disk.features_ok(disk.device_features());
        disk
    }

    fn request(mem: &GuestMemoryMmap, kind: u32, ranges: &[(u64, u32, u32)]) -> Vec<VringDesc> {
        mem.write_obj(kind.to_le(), GuestAddress(0x1000)).unwrap();
        mem.write_obj(0u64, GuestAddress(0x1008)).unwrap();
        for (i, (sector, count, flags)) in ranges.iter().enumerate() {
            let at = 0x2000 + i as u64 * 16;
            mem.write_obj(sector.to_le(), GuestAddress(at)).unwrap();
            mem.write_obj(count.to_le(), GuestAddress(at + 8)).unwrap();
            mem.write_obj(flags.to_le(), GuestAddress(at + 12)).unwrap();
        }
        vec![
            VringDesc {
                addr: 0x1000,
                len: 16,
                flags: 1,
                next: 1,
            },
            VringDesc {
                addr: 0x2000,
                len: ranges.len() as u32 * 16,
                flags: 1,
                next: 2,
            },
            VringDesc {
                addr: 0x3000,
                len: 1,
                flags: 2,
                next: 0,
            },
        ]
    }

    #[test]
    fn invalid_later_range_leaves_disk_untouched() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 65536)]).unwrap();
        let mut disk = backend();
        let chain = request(&mem, VIRTIO_BLK_T_DISCARD, &[(0, 8, 0), (31, 2, 0)]);
        assert_eq!(disk.request(&mem, &chain), Err(VIRTIO_BLK_S_IOERR));
        let mut bytes = [0; 16384];
        disk.disk.read_exact_at(&mut bytes, 0).unwrap();
        assert!(bytes.iter().all(|b| *b == 7));
    }

    #[test]
    fn discard_and_both_zero_modes_preserve_size_and_neighbors() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 65536)]).unwrap();
        for (kind, flags) in [
            (VIRTIO_BLK_T_DISCARD, 0),
            (VIRTIO_BLK_T_WRITE_ZEROES, 0),
            (VIRTIO_BLK_T_WRITE_ZEROES, 1),
        ] {
            let mut disk = backend();
            let chain = request(&mem, kind, &[(8, 8, flags)]);
            assert_eq!(disk.request(&mem, &chain), Ok(0));
            let mut bytes = [0; 16384];
            disk.disk.read_exact_at(&mut bytes, 0).unwrap();
            assert!(bytes[..4096].iter().all(|b| *b == 7));
            assert!(bytes[4096..8192].iter().all(|b| *b == 0));
            assert!(bytes[8192..].iter().all(|b| *b == 7));
            assert_eq!(disk.disk.metadata().unwrap().len(), 16384);
        }
    }

    #[test]
    fn descriptor_cycles_and_bad_heads_finish_without_io() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 65536)]).unwrap();
        let mut disk = backend();
        let mut q = VirtioQueueState::new(8);
        q.size = 8;
        q.ready = true;
        q.desc_addr = 0x1000;
        q.avail_addr = 0x2000;
        q.used_addr = 0x3000;
        mem.write_obj(1u16, GuestAddress(q.desc_addr + 12)).unwrap();
        mem.write_obj(0u16, GuestAddress(q.desc_addr + 14)).unwrap();
        mem.write_obj(2u16, GuestAddress(q.avail_addr + 2)).unwrap();
        mem.write_obj(0u16, GuestAddress(q.avail_addr + 4)).unwrap();
        mem.write_obj(100u16, GuestAddress(q.avail_addr + 6))
            .unwrap();
        assert!(disk.process_queue(0, std::slice::from_mut(&mut q), &mem));
        assert_eq!(q.last_avail_idx, 2);
        assert_eq!(
            mem.read_obj::<u16>(GuestAddress(q.used_addr + 2)).unwrap(),
            2
        );
    }

    #[test]
    fn readonly_and_bad_data_direction_are_rejected() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 65536)]).unwrap();
        let mut disk = backend();
        let mut chain = request(&mem, VIRTIO_BLK_T_DISCARD, &[(0, 8, 0)]);
        chain[1].flags |= VRING_DESC_F_WRITE;
        assert_eq!(disk.request(&mem, &chain), Err(VIRTIO_BLK_S_IOERR));
        chain[1].flags &= !VRING_DESC_F_WRITE;
        disk.read_only = true;
        assert_eq!(disk.request(&mem, &chain), Err(VIRTIO_BLK_S_IOERR));
        assert_eq!(disk.device_features() & ((1 << 13) | (1 << 14)), 0);
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;

    fn queue(size: u16) -> VirtioQueueState {
        VirtioQueueState {
            size,
            ready: true,
            desc_addr: DESC,
            avail_addr: AVAIL,
            used_addr: USED,
            ..VirtioQueueState::new(256)
        }
    }

    fn desc(mem: &GuestMemoryMmap, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let at = DESC + i as u64 * 16;
        mem.write_obj(addr, GuestAddress(at)).unwrap();
        mem.write_obj(len, GuestAddress(at + 8)).unwrap();
        mem.write_obj(flags, GuestAddress(at + 12)).unwrap();
        mem.write_obj(next, GuestAddress(at + 14)).unwrap();
    }

    fn offer(mem: &GuestMemoryMmap, heads: &[u16]) {
        for (i, head) in heads.iter().enumerate() {
            mem.write_obj(*head, GuestAddress(AVAIL + 4 + i as u64 * 2))
                .unwrap();
        }
        mem.write_obj(heads.len() as u16, GuestAddress(AVAIL + 2))
            .unwrap();
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap()
    }

    #[test]
    fn pending_rejects_unusable_queues() {
        let mem = memory();
        offer(&mem, &[0]);
        assert_eq!(queue(8).pending(&mem), Some(1));
        assert_eq!(queue(0).pending(&mem), None);
        assert_eq!(queue(257).pending(&mem), None);
        let mut far = queue(8);
        far.desc_addr = 0x1FFC0;
        assert_eq!(far.pending(&mem), None);
        mem.write_obj(9u16, GuestAddress(AVAIL + 2)).unwrap();
        assert_eq!(queue(8).pending(&mem), None);
    }

    #[test]
    fn chain_rejects_loops_out_of_range_and_foreign_flags() {
        let mem = memory();
        let q = queue(8);
        desc(&mem, 0, 0x8000, 4, VRING_DESC_F_NEXT, 1);
        desc(&mem, 1, 0x8000, 4, 0, 0);
        assert_eq!(
            q.chain(&mem, 0, VRING_DESC_F_NEXT).map(|c| c.len()),
            Some(2)
        );
        desc(&mem, 1, 0x8000, 4, VRING_DESC_F_NEXT, 0);
        assert!(q.chain(&mem, 0, VRING_DESC_F_NEXT).is_none());
        desc(&mem, 1, 0x8000, 4, VRING_DESC_F_NEXT, 8);
        assert!(q.chain(&mem, 0, VRING_DESC_F_NEXT).is_none());
        desc(&mem, 1, 0x8000, 4, VRING_DESC_F_WRITE, 0);
        assert!(q.chain(&mem, 0, VRING_DESC_F_NEXT).is_none());
        assert!(q.chain(&mem, 8, VRING_DESC_F_NEXT).is_none());
    }

    #[test]
    fn net_tx_drops_bad_chains_and_sends_good_frames() {
        let mem = memory();
        let (host, guest) = std::os::unix::net::UnixDatagram::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        let mut net = NetBackend::new(guest.as_raw_fd(), [0; 6]);
        let mut queues = [queue(8), queue(8)];

        // A chain that points at itself, then one descriptor of 4 GiB.
        desc(&mem, 0, 0x8000, 64, VRING_DESC_F_NEXT, 0);
        desc(&mem, 1, 0x8000, u32::MAX, 0, 0);
        // A frame split over two descriptors.
        mem.write_slice(&[0xAB; 64], GuestAddress(0x9000)).unwrap();
        desc(
            &mem,
            2,
            0x9000,
            VIRTIO_NET_HDR_SIZE as u32,
            VRING_DESC_F_NEXT,
            3,
        );
        desc(&mem, 3, 0x9000 + VIRTIO_NET_HDR_SIZE as u64, 20, 0, 0);
        offer(&mem, &[0, 1, 2]);

        assert!(net.process_queue(1, &mut queues, &mem));
        assert_eq!(queues[1].last_avail_idx, 3);
        let mut frame = [0u8; 128];
        assert_eq!(host.recv(&mut frame).unwrap(), 20);
        assert!(host.recv(&mut frame).is_err());
        drop(guest);
    }
}
