//! The arm64 platform devices: the PL011 UART and the PL031 RTC.

use std::io;
use std::os::fd::RawFd;

use crate::kvm::devices::MmioDevice;
use crate::kvm::snapshot::{self, DeviceState};

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

    fn save(&self) -> io::Result<DeviceState> {
        Ok(DeviceState::Uart {
            lcr: self.lcr,
            cr: self.cr,
            imsc: self.imsc,
            ibrd: self.ibrd,
            fbrd: self.fbrd,
        })
    }

    fn restore(&mut self, state: &DeviceState) -> io::Result<()> {
        let &DeviceState::Uart {
            lcr,
            cr,
            imsc,
            ibrd,
            fbrd,
        } = state
        else {
            return Err(snapshot::mismatch());
        };
        (self.lcr, self.cr, self.imsc, self.ibrd, self.fbrd) = (lcr, cr, imsc, ibrd, fbrd);
        Ok(())
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

    fn save(&self) -> io::Result<DeviceState> {
        Ok(DeviceState::Rtc {
            mr: self.mr,
            cr: self.cr,
            imsc: self.imsc,
        })
    }

    fn restore(&mut self, state: &DeviceState) -> io::Result<()> {
        let &DeviceState::Rtc { mr, cr, imsc } = state else {
            return Err(snapshot::mismatch());
        };
        (self.mr, self.cr, self.imsc) = (mr, cr, imsc);
        Ok(())
    }
}
