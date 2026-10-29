use std::io;
use std::sync::atomic::{fence, Ordering};

use vm_memory::{Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

use super::connection::{BUFFER_SIZE, HEADER_LEN};

const VRING_USED_F_NO_NOTIFY: u16 = 1;
const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;

pub(super) fn invalid_queue() -> io::Error {
    io::Error::other("invalid vsock virtqueue")
}

pub(super) struct Queue {
    size: u16,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    next_avail: u16,
    used: u16,
}

impl Queue {
    pub(super) fn new(
        size: u16,
        desc_addr: u64,
        avail_addr: u64,
        used_addr: u64,
        mem: &GuestMemoryMmap,
    ) -> io::Result<Self> {
        let count = size as usize;
        if count == 0
            || count > 256
            || !count.is_power_of_two()
            || !desc_addr.is_multiple_of(16)
            || !avail_addr.is_multiple_of(2)
            || !used_addr.is_multiple_of(4)
        {
            return Err(invalid_queue());
        }
        for (address, len) in [
            (desc_addr, count * 16),
            (avail_addr, 6 + count * 2),
            (used_addr, 6 + count * 8),
        ] {
            if address.checked_add(len as u64).is_none()
                || !mem.check_range(GuestAddress(address), len)
            {
                return Err(invalid_queue());
            }
        }
        Ok(Self {
            size,
            desc_addr,
            avail_addr,
            used_addr,
            next_avail: 0,
            used: 0,
        })
    }

    /// The ring index of the next buffer the device takes.
    pub(super) fn index(&self) -> u16 {
        self.next_avail
    }

    /// The queue as a device that has used every buffer before `index`.
    pub(super) fn starting_at(mut self, index: u16) -> Self {
        self.next_avail = index;
        self.used = index;
        self
    }

    /// The avail index: how many buffers the driver has posted, wrapping.
    pub(super) fn posted(&self, mem: &GuestMemoryMmap) -> io::Result<u16> {
        mem.read_obj::<u16>(GuestAddress(self.avail_addr + 2))
            .map(u16::from_le)
            .map_err(|_| invalid_queue())
    }

    pub(super) fn head(&self, mem: &GuestMemoryMmap) -> io::Result<Option<u16>> {
        self.head_at(mem, 0)
    }

    /// The head `ahead` places after the next one, if the driver posted it.
    pub(super) fn head_at(&self, mem: &GuestMemoryMmap, ahead: usize) -> io::Result<Option<u16>> {
        let q = self;
        let available = u16::from_le(
            mem.read_obj::<u16>(GuestAddress(q.avail_addr + 2))
                .map_err(|_| invalid_queue())?,
        );
        fence(Ordering::Acquire);
        let count = available.wrapping_sub(q.next_avail);
        if count > q.size {
            return Err(invalid_queue());
        }
        if ahead >= count as usize {
            return Ok(None);
        }
        let slot = q.next_avail.wrapping_add(ahead as u16) % q.size;
        mem.read_obj::<u16>(GuestAddress(q.avail_addr + 4 + slot as u64 * 2))
            .map(u16::from_le)
            .map(Some)
            .map_err(|_| invalid_queue())
    }

    /// Asks the driver to kick, or not, when it posts buffers.
    pub(super) fn set_notify(&self, mem: &GuestMemoryMmap, on: bool) -> io::Result<()> {
        let flags: u16 = if on { 0 } else { VRING_USED_F_NO_NOTIFY };
        mem.write_obj(flags.to_le(), GuestAddress(self.used_addr))
            .map_err(|_| invalid_queue())?;
        // The flag has to be visible before the ring is checked again.
        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Whether the driver wants an interrupt for buffers already used.
    pub(super) fn wants_interrupt(&self, mem: &GuestMemoryMmap) -> bool {
        fence(Ordering::SeqCst);
        mem.read_obj::<u16>(GuestAddress(self.avail_addr))
            .map_or(true, |flags| {
                u16::from_le(flags) & VRING_AVAIL_F_NO_INTERRUPT == 0
            })
    }

    pub(super) fn descriptors(
        &self,
        mem: &GuestMemoryMmap,
        head: u16,
        writable: bool,
    ) -> io::Result<Vec<(u64, usize)>> {
        let mut result = Vec::new();
        let mut index = head;
        let mut total = 0usize;
        let mut visited = [false; 256];
        loop {
            if index >= self.size || visited[index as usize] {
                return Err(invalid_queue());
            }
            visited[index as usize] = true;
            let mut bytes = [0; 16];
            mem.read_slice(&mut bytes, GuestAddress(self.desc_addr + index as u64 * 16))
                .map_err(|_| invalid_queue())?;
            let address = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(bytes[12..14].try_into().unwrap());
            if flags & !3 != 0
                || (flags & 2 != 0) != writable
                || len > BUFFER_SIZE + HEADER_LEN
                || address.checked_add(len as u64).is_none()
                || !mem.check_range(GuestAddress(address), len)
            {
                return Err(invalid_queue());
            }
            total = total.checked_add(len).ok_or_else(invalid_queue)?;
            if total > BUFFER_SIZE + HEADER_LEN {
                return Err(invalid_queue());
            }
            result.push((address, len));
            if flags & 1 == 0 {
                return Ok(result);
            }
            index = u16::from_le_bytes(bytes[14..16].try_into().unwrap());
        }
    }

    pub(super) fn complete(
        &mut self,
        mem: &GuestMemoryMmap,
        head: u16,
        len: u32,
    ) -> io::Result<()> {
        let address = self.used_addr + 4 + (self.used % self.size) as u64 * 8;
        mem.write_obj((head as u32).to_le(), GuestAddress(address))
            .map_err(|_| invalid_queue())?;
        mem.write_obj(len.to_le(), GuestAddress(address + 4))
            .map_err(|_| invalid_queue())?;
        fence(Ordering::Release);
        self.used = self.used.wrapping_add(1);
        mem.write_obj(self.used.to_le(), GuestAddress(self.used_addr + 2))
            .map_err(|_| invalid_queue())?;
        self.next_avail = self.next_avail.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap()
    }

    fn queue(mem: &GuestMemoryMmap) -> Queue {
        Queue::new(8, 0x1000, 0x2000, 0x3000, mem).unwrap()
    }

    fn descriptor(
        mem: &GuestMemoryMmap,
        index: u16,
        address: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&address.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&next.to_le_bytes());
        mem.write_slice(&bytes, GuestAddress(0x1000 + index as u64 * 16))
            .unwrap();
    }

    #[test]
    fn queue_configuration_is_bounded_and_aligned() {
        let mem = memory();
        for size in [0, 3, 257, u16::MAX] {
            assert!(Queue::new(size, 0x1000, 0x2000, 0x3000, &mem).is_err());
        }
        for (desc, avail, used) in [
            (1, 0x2000, 0x3000),
            (0x1000, 1, 0x3000),
            (0x1000, 0x2000, 1),
            (u64::MAX - 15, 0x2000, 0x3000),
            (0x1000, 0x2000, 1024 * 1024),
        ] {
            assert!(Queue::new(8, desc, avail, used, &mem).is_err());
        }
    }

    #[test]
    fn split_header_and_payload_chain() {
        let mem = memory();
        let q = queue(&mem);
        descriptor(&mem, 0, 0x4000, HEADER_LEN as u32, 3, 1);
        descriptor(&mem, 1, 0x5000, 4096, 2, 0);
        assert_eq!(
            q.descriptors(&mem, 0, true).unwrap(),
            vec![(0x4000, HEADER_LEN), (0x5000, 4096)]
        );
        assert!(q.descriptors(&mem, 0, false).is_err());
    }

    #[test]
    fn descriptor_cycles_flags_indices_and_ranges_are_rejected() {
        let mem = memory();
        let q = queue(&mem);
        for (address, len, flags, next) in [
            (0x4000, 44, 1, 0),
            (0x4000, 44, 1, 8),
            (0x4000, 44, 4, 0),
            (0x4000, 44, 2, 0),
            (u64::MAX, 44, 0, 0),
            (1024 * 1024 - 10, 44, 0, 0),
            (0x4000, u32::MAX, 0, 0),
        ] {
            descriptor(&mem, 0, address, len, flags, next);
            assert!(q.descriptors(&mem, 0, false).is_err());
        }
        assert!(q.descriptors(&mem, 8, false).is_err());
        descriptor(&mem, 0, 0x4000, BUFFER_SIZE as u32, 1, 1);
        descriptor(&mem, 1, 0x4000, BUFFER_SIZE as u32, 0, 0);
        assert!(q.descriptors(&mem, 0, false).is_err());
    }

    #[test]
    fn available_and_used_indices_wrap_and_publish_lengths() {
        let mem = memory();
        let mut q = queue(&mem);
        assert!(q.head(&mem).unwrap().is_none());
        q.next_avail = u16::MAX;
        q.used = u16::MAX;
        mem.write_obj(0u16, GuestAddress(0x2002)).unwrap();
        mem.write_obj(5u16.to_le(), GuestAddress(0x2004 + 7 * 2))
            .unwrap();
        assert_eq!(q.head(&mem).unwrap(), Some(5));
        q.complete(&mem, 5, 123).unwrap();
        assert_eq!(q.used, 0);
        assert_eq!(q.next_avail, 0);
        assert_eq!(mem.read_obj::<u16>(GuestAddress(0x3002)).unwrap(), 0);
        assert_eq!(
            u32::from_le(mem.read_obj::<u32>(GuestAddress(0x3004 + 7 * 8)).unwrap()),
            5
        );
        assert_eq!(
            u32::from_le(mem.read_obj::<u32>(GuestAddress(0x3008 + 7 * 8)).unwrap()),
            123
        );
        assert!(q.head(&mem).unwrap().is_none());
        mem.write_obj(9u16.to_le(), GuestAddress(0x2002)).unwrap();
        assert!(q.head(&mem).is_err());
    }

    #[test]
    fn heads_ahead_and_notification_flags() {
        let mem = memory();
        let q = queue(&mem);
        mem.write_obj(3u16.to_le(), GuestAddress(0x2002)).unwrap();
        for (slot, head) in [4u16, 6, 1].iter().enumerate() {
            mem.write_obj(head.to_le(), GuestAddress(0x2004 + slot as u64 * 2))
                .unwrap();
        }
        assert_eq!(q.head_at(&mem, 0).unwrap(), Some(4));
        assert_eq!(q.head_at(&mem, 2).unwrap(), Some(1));
        assert!(q.head_at(&mem, 3).unwrap().is_none());
        q.set_notify(&mem, false).unwrap();
        assert_eq!(mem.read_obj::<u16>(GuestAddress(0x3000)).unwrap(), 1);
        q.set_notify(&mem, true).unwrap();
        assert_eq!(mem.read_obj::<u16>(GuestAddress(0x3000)).unwrap(), 0);
        assert!(q.wants_interrupt(&mem));
        mem.write_obj(1u16.to_le(), GuestAddress(0x2000)).unwrap();
        assert!(!q.wants_interrupt(&mem));
    }

    #[test]
    fn arbitrary_descriptor_bytes_do_not_panic() {
        let mem = memory();
        let q = queue(&mem);
        let mut seed = 123456789u64;
        for _ in 0..4096 {
            let mut bytes = [0; 128];
            for byte in &mut bytes {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = seed as u8;
            }
            mem.write_slice(&bytes, GuestAddress(0x1000)).unwrap();
            let _ = q.descriptors(&mem, (seed % 16) as u16, seed & 16 != 0);
        }
    }
}
