//! A 16550A UART on COM1: transmit, and the transmitter empty interrupt
//! the 8250 driver waits on before it writes again. Nothing is received.

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;

use kvm_ioctls::VmFd;

use super::layout::SERIAL_IRQ;
use crate::kvm::devices::MmioDevice;
use crate::kvm::snapshot::{self, DeviceState};

const DATA: u64 = 0;
const IER: u64 = 1;
const IIR: u64 = 2;
const LCR: u64 = 3;
const MCR: u64 = 4;
const LSR: u64 = 5;
const MSR: u64 = 6;
const SCR: u64 = 7;

const LCR_DLAB: u8 = 0x80;
const IER_THRE: u8 = 0x02;
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_FIFO: u8 = 0xc0;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const MCR_LOOP: u8 = 0x10;
// Carrier, data set ready and clear to send.
const MSR_READY: u8 = 0xb0;

pub struct Serial {
    vm_fd: Arc<VmFd>,
    write_fd: Option<RawFd>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fifo: bool,
    divisor: u16,
    thre_pending: bool,
}

impl Serial {
    pub fn new(vm_fd: Arc<VmFd>, write_fd: Option<RawFd>) -> Self {
        Serial {
            vm_fd,
            write_fd,
            ier: 0,
            lcr: 0x03,
            mcr: 0x08,
            scr: 0,
            fifo: false,
            divisor: 12,
            thre_pending: false,
        }
    }

    /// The transmitter is always empty, so with its interrupt enabled every
    /// write is followed by one.
    fn transmitter_empty(&mut self) {
        if self.ier & IER_THRE != 0 {
            self.thre_pending = true;
            let _ = self.vm_fd.set_irq_line(SERIAL_IRQ, true);
            let _ = self.vm_fd.set_irq_line(SERIAL_IRQ, false);
        }
    }

    fn read_u8(&mut self, offset: u64) -> u8 {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor as u8,
            IER if dlab => (self.divisor >> 8) as u8,
            DATA => 0,
            IER => self.ier,
            IIR => {
                let fifo = if self.fifo { IIR_FIFO } else { 0 };
                if std::mem::take(&mut self.thre_pending) {
                    fifo | IIR_THRE
                } else {
                    fifo | IIR_NONE
                }
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => LSR_THRE | LSR_TEMT,
            MSR if self.mcr & MCR_LOOP != 0 => (self.mcr & 0x0f) << 4,
            MSR => MSR_READY,
            SCR => self.scr,
            _ => 0,
        }
    }

    fn write_u8(&mut self, offset: u64, value: u8) {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor = (self.divisor & 0xff00) | value as u16,
            IER if dlab => self.divisor = (self.divisor & 0x00ff) | (value as u16) << 8,
            DATA => {
                if let (Some(fd), false) = (self.write_fd, self.mcr & MCR_LOOP != 0) {
                    unsafe {
                        libc::write(fd, &value as *const u8 as *const libc::c_void, 1);
                    }
                }
                self.transmitter_empty();
            }
            IER => {
                self.ier = value & 0x0f;
                self.transmitter_empty();
            }
            IIR => self.fifo = value & 1 != 0,
            LCR => self.lcr = value,
            MCR => self.mcr = value & 0x1f,
            SCR => self.scr = value,
            _ => {}
        }
    }
}

impl MmioDevice for Serial {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if let Some(byte) = data.first_mut() {
            *byte = self.read_u8(offset);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if let Some(&byte) = data.first() {
            self.write_u8(offset, byte);
        }
    }

    fn save(&self) -> io::Result<DeviceState> {
        Ok(DeviceState::Serial {
            ier: self.ier,
            lcr: self.lcr,
            mcr: self.mcr,
            scr: self.scr,
            fifo: self.fifo,
            divisor: self.divisor,
            thre_pending: self.thre_pending,
        })
    }

    fn restore(&mut self, state: &DeviceState) -> io::Result<()> {
        let &DeviceState::Serial {
            ier,
            lcr,
            mcr,
            scr,
            fifo,
            divisor,
            thre_pending,
        } = state
        else {
            return Err(snapshot::mismatch());
        };
        self.ier = ier;
        self.lcr = lcr;
        self.mcr = mcr;
        self.scr = scr;
        self.fifo = fifo;
        self.divisor = divisor;
        self.thre_pending = thre_pending;
        Ok(())
    }
}
