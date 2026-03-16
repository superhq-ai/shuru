use std::collections::VecDeque;
use std::os::unix::io::RawFd;

use smoltcp::phy::{self, Checksum, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

const MTU: usize = 1514; // 14-byte Ethernet header + 1500-byte IP payload
const MAX_PENDING_FRAMES: usize = 256;

/// smoltcp Device backed by a Unix datagram socketpair fd.
///
/// One end of the socketpair is given to VZFileHandleNetworkDeviceAttachment
/// (the VM side). This Device reads/writes the other end (the host side),
/// giving us raw L2 Ethernet frames from/to the guest.
pub struct VZDevice {
    fd: RawFd,
    recv_buf: Vec<u8>,
    /// Frames pre-read by `drain_recv()`, waiting to be consumed by smoltcp.
    pending_rx: VecDeque<Vec<u8>>,
}

impl VZDevice {
    pub fn new(fd: RawFd) -> Self {
        VZDevice {
            fd,
            recv_buf: vec![0u8; MTU + 64], // slack for oversized frames
            pending_rx: VecDeque::new(),
        }
    }

    /// Non-blocking read of a single frame from the socketpair.
    fn recv_one_frame(&mut self) -> Option<Vec<u8>> {
        let n = unsafe {
            libc::recv(
                self.fd,
                self.recv_buf.as_mut_ptr() as *mut libc::c_void,
                self.recv_buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            return None;
        }
        Some(self.recv_buf[..n as usize].to_vec())
    }

    /// Drain all available frames from the socketpair (non-blocking).
    /// Call this before `Interface::poll()` so we can inspect frames
    /// (e.g. to detect TCP SYN and dynamically add listening sockets).
    pub fn drain_recv(&mut self) {
        while self.pending_rx.len() < MAX_PENDING_FRAMES {
            match self.recv_one_frame() {
                Some(frame) => self.pending_rx.push_back(frame),
                None => break,
            }
        }
    }

    /// Iterate over all pending frames for inspection (e.g. SYN detection).
    pub fn pending_frames(&self) -> impl Iterator<Item = &[u8]> {
        self.pending_rx.iter().map(|v| v.as_slice())
    }
}

pub struct VZRxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for VZRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

pub struct VZTxToken {
    fd: RawFd,
}

impl phy::TxToken for VZTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        let sent = unsafe {
            libc::send(
                self.fd,
                buffer.as_ptr() as *const libc::c_void,
                buffer.len(),
                0,
            )
        };
        if sent < 0 {
            tracing::debug!("TX {len} bytes failed: sent={sent}");
        }
        result
    }
}

impl Device for VZDevice {
    type RxToken<'a> = VZRxToken;
    type TxToken<'a> = VZTxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // First drain pre-read frames, then try reading directly from the
        // socketpair for frames that arrived during poll.
        let buffer = self
            .pending_rx
            .pop_front()
            .or_else(|| self.recv_one_frame())?;
        Some((VZRxToken { buffer }, VZTxToken { fd: self.fd }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VZTxToken { fd: self.fd })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        // The guest VirtIO NIC offloads checksum calculation, so incoming
        // frames may have partial/invalid checksums. Tell smoltcp to only
        // generate checksums on TX (for the guest to verify), not verify on RX.
        caps.checksum.ipv4 = Checksum::Tx;
        caps.checksum.tcp = Checksum::Tx;
        caps.checksum.udp = Checksum::Tx;
        caps.checksum.icmpv4 = Checksum::Tx;
        caps
    }
}
