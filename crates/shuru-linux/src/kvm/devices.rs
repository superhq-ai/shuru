use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kvm_ioctls::{IoEventAddress, VmFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
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
                    q.size = val as u16;
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
const VRING_DESC_F_WRITE: u16 = 2;

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

    fn activate(&mut self, _: &[VirtioQueueState], _: &GuestMemoryMmap, _: Arc<Interrupt>) {}

    fn process_queue(
        &mut self,
        _queue_idx: u16,
        queues: &mut [VirtioQueueState],
        mem: &GuestMemoryMmap,
    ) -> bool {
        let q = match queues.get_mut(0) {
            Some(q) if q.ready => q,
            _ => return false,
        };

        let current_avail = avail_idx(mem, q.avail_addr);
        let used = q.last_avail_idx != current_avail;

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
        used
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
            Some(q) if q.ready => q,
            _ => return false,
        };

        let current_avail = avail_idx(mem, q.avail_addr);
        let used = q.last_avail_idx != current_avail;
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

            if rx_q.wants_interrupt(&mem) {
                interrupt.raise(INTERRUPT_USED);
            }
        }
    }
}
