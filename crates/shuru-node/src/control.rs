//! The node talks to each jailed process over one unix stream. A message is
//! a u32 little endian length and a JSON body. The configuration carries
//! the descriptors as SCM_RIGHTS on its first byte.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

// Room for a system CA bundle.
pub const MAX_MESSAGE: usize = 1 << 20;
pub const MAX_FDS: usize = 8;
pub const SNAPSHOT_FDS: usize = 3;

/// The VM to build. Descriptors follow in this order, each optional one
/// present only when its field says so: `/dev/kvm`, kernel, initrd, disk,
/// serial output, network socket, vsock listener, saved memory, saved
/// state.
#[derive(Debug, Serialize, Deserialize)]
pub struct VmmConfig {
    pub cpus: usize,
    pub memory_bytes: u64,
    pub boot: Boot,
    pub disk: Option<Disk>,
    pub serial: bool,
    pub network_mac: Option<[u8; 6]>,
    pub vsock_connections: usize,
    pub balloon: bool,
}

/// What the guest starts from.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boot {
    /// The kernel, and the initrd when `initrd` says so.
    Kernel { command_line: String, initrd: bool },
    /// The memory and state a `Snapshot` request saved. The devices must
    /// be the ones it was taken with.
    Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Disk {
    pub read_only: bool,
}

impl VmmConfig {
    pub fn fd_count(&self) -> usize {
        let image = match self.boot {
            Boot::Kernel { initrd, .. } => 1 + initrd as usize,
            Boot::Snapshot => 2,
        };
        2 + image
            + self.disk.is_some() as usize
            + self.serial as usize
            + self.network_mac.is_some() as usize
    }
}

/// The network process for one sandbox. One descriptor follows: the host
/// end of the frame socket the VM sends Ethernet on.
#[derive(Debug, Serialize, Deserialize)]
pub struct NetdConfig {
    pub dns_resolver: Ipv4Addr,
    /// Domain patterns the guest may reach. Empty allows every public
    /// address.
    pub allow: Vec<String>,
    pub secrets: HashMap<String, Secret>,
    /// PEM roots for checking upstream servers when relaying secrets.
    pub upstream_roots: String,
    /// The proxy session a restored guest was saved with.
    #[serde(default)]
    pub session: Option<shuru_proxy::Session>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Secret {
    pub value: String,
    pub hosts: Vec<String>,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Configure(VmmConfig),
    ConfigureNetwork(NetdConfig),
    Stop,
    SetMemoryTarget {
        bytes: u64,
    },
    Stats,
    /// Pause the VM and save it. Three descriptors follow: the memory file,
    /// the state file and the /proc pagemap of the VMM. The memory file is
    /// empty and as long as guest memory, or for a restored VM a copy of
    /// the one it was restored from. Without `resume` the VM stays paused.
    Snapshot {
        resume: bool,
    },
    /// The open connections of the guest, for netd.
    Flows,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Stats {
        running: bool,
        ballooned_bytes: u64,
    },
    /// The guest sees the session placeholders in place of secret values,
    /// and trusts its CA for the hosts they are relayed to.
    Network {
        session: shuru_proxy::Session,
    },
    Flows {
        flows: Vec<shuru_proxy::FlowAge>,
    },
    Error {
        message: String,
    },
}

pub fn send(stream: &UnixStream, message: &impl Serialize, fds: &[RawFd]) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_MESSAGE || fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control message too large",
        ));
    }
    let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(&body);

    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if !fds.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(header).cast::<RawFd>(),
                fds.len(),
            );
        }
    }
    let sent = loop {
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            break n as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    (&*stream).write_all(&bytes[sent..])
}

/// Reads one message, keeping at most `max_fds` descriptors sent with it.
/// Returns `None` when the peer closed between messages.
pub fn receive<T: DeserializeOwned>(
    stream: &UnixStream,
    max_fds: usize,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let mut length = [0u8; 4];
    let mut fds = Vec::new();
    let mut got = 0;
    while got < length.len() {
        let n = receive_with_fds(stream, &mut length[got..], max_fds, &mut fds)?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += n;
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control message too large",
        ));
    }
    let mut body = vec![0; length];
    (&*stream).read_exact(&mut body)?;
    let message = serde_json::from_slice(&body)?;
    Ok(Some((message, fds)))
}

fn receive_with_fds(
    stream: &UnixStream,
    buf: &mut [u8],
    max_fds: usize,
    fds: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let space = unsafe { libc::CMSG_SPACE((max_fds * std::mem::size_of::<RawFd>()) as u32) };
    let mut control = vec![0u8; space as usize];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if max_fds > 0 {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
    }
    let n = loop {
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected descriptors on the control socket",
        ));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Seek;

    #[test]
    fn a_configuration_carries_its_descriptors() {
        let (node, vmm) = UnixStream::pair().unwrap();
        let mut file = File::open("/proc/self/exe").unwrap();
        let config = VmmConfig {
            cpus: 2,
            memory_bytes: 1 << 30,
            boot: Boot::Kernel {
                command_line: "quiet".into(),
                initrd: false,
            },
            disk: None,
            serial: false,
            network_mac: None,
            vsock_connections: 128,
            balloon: true,
        };
        let fds = [file.as_raw_fd(); 3];
        assert_eq!(config.fd_count(), fds.len());
        send(&node, &Request::Configure(config), &fds).unwrap();
        send(&node, &Request::Stop, &[]).unwrap();

        let (request, received) = receive::<Request>(&vmm, MAX_FDS).unwrap().unwrap();
        assert!(matches!(request, Request::Configure(c) if c.vsock_connections == 128));
        assert_eq!(received.len(), 3);
        file.seek(io::SeekFrom::Start(4)).unwrap();
        let mut other = File::from(received.into_iter().next().unwrap());
        assert_eq!(other.stream_position().unwrap(), 4);

        let (request, received) = receive::<Request>(&vmm, MAX_FDS).unwrap().unwrap();
        assert!(matches!(request, Request::Stop));
        assert!(received.is_empty());
        drop(node);
        assert!(receive::<Request>(&vmm, MAX_FDS).unwrap().is_none());
    }

    #[test]
    fn descriptors_from_a_peer_that_may_not_send_them_are_refused() {
        let (vmm, node) = UnixStream::pair().unwrap();
        let file = File::open("/proc/self/exe").unwrap();
        send(&vmm, &Response::Ok, &[file.as_raw_fd()]).unwrap();
        assert!(receive::<Response>(&node, 0).is_err());
    }

    #[test]
    fn oversized_messages_are_refused() {
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(&(MAX_MESSAGE as u32 + 1).to_le_bytes())
            .unwrap();
        assert!(receive::<Request>(&a, 0).is_err());
    }
}
