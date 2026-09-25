use std::cell::Cell;
use std::os::unix::io::RawFd;

pub struct FileHandleNetworkAttachment {
    pub(crate) fd: RawFd,
}

impl FileHandleNetworkAttachment {
    /// Creates a network attachment from a connected datagram socket fd.
    /// The fd should be one end of `socketpair(AF_UNIX, SOCK_DGRAM)`.
    /// `mtu` is accepted for API parity with the Darwin implementation
    /// and ignored on this platform (Linux build is a stub).
    pub fn new(fd: RawFd, _mtu: i64) -> Self {
        FileHandleNetworkAttachment { fd }
    }
}

pub struct MACAddress {
    pub(crate) bytes: [u8; 6],
}

impl MACAddress {
    pub fn new() -> Self {
        MACAddress { bytes: [0; 6] }
    }

    /// Generate a random locally-administered MAC address.
    pub fn random_local() -> Self {
        let mut bytes = [0u8; 6];
        let fd = unsafe { libc::open(b"/dev/urandom\0".as_ptr() as *const _, libc::O_RDONLY) };
        if fd >= 0 {
            unsafe {
                libc::read(fd, bytes.as_mut_ptr() as *mut libc::c_void, 6);
                libc::close(fd);
            }
        }
        // Set locally administered + unicast bits
        bytes[0] = (bytes[0] & 0xFC) | 0x02;
        MACAddress { bytes }
    }
}

impl Default for MACAddress {
    fn default() -> Self {
        Self::new()
    }
}

pub struct VirtioNetworkDevice {
    pub(crate) fd: Option<RawFd>,
    pub(crate) mac: Cell<[u8; 6]>,
}

impl VirtioNetworkDevice {
    pub fn new() -> Self {
        VirtioNetworkDevice {
            fd: None,
            mac: Cell::new([0; 6]),
        }
    }

    pub fn new_with_attachment(attachment: &FileHandleNetworkAttachment) -> Self {
        VirtioNetworkDevice {
            fd: Some(attachment.fd),
            mac: Cell::new([0; 6]),
        }
    }

    pub fn set_attachment(&mut self, attachment: &FileHandleNetworkAttachment) {
        self.fd = Some(attachment.fd);
    }

    pub fn set_mac_address(&self, address: &MACAddress) {
        self.mac.set(address.bytes);
    }

    pub(crate) fn mac_bytes(&self) -> [u8; 6] {
        self.mac.get()
    }
}

impl Default for VirtioNetworkDevice {
    fn default() -> Self {
        Self::new()
    }
}
