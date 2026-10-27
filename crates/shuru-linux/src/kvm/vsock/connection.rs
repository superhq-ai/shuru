use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::Shutdown;
use std::ops::Bound;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

pub const HEADER_LEN: usize = 44;
pub const BUFFER_SIZE: usize = 256 * 1024;
pub const MAX_PACKET: usize = 64 * 1024;
// One connection reads at most this much per turn before the next one goes.
const MAX_TURN: usize = 128 * 1024;
pub const MAX_CONNECTIONS: usize = 1024;
const MAX_RESETS: usize = MAX_CONNECTIONS + 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
pub const HOST_CID: u64 = 2;
pub const GUEST_CID: u64 = 3;
pub const STREAM: u16 = 1;
pub const REQUEST: u16 = 1;
pub const RESPONSE: u16 = 2;
pub const RST: u16 = 3;
pub const SHUTDOWN: u16 = 4;
pub const RW: u16 = 5;
pub const CREDIT_UPDATE: u16 = 6;
pub const CREDIT_REQUEST: u16 = 7;
const RECEIVE: u32 = 1;
const SEND: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub kind: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl Header {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..HEADER_LEN)?;
        Some(Self {
            src_cid: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            dst_cid: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            src_port: u32::from_le_bytes(bytes[16..20].try_into().ok()?),
            dst_port: u32::from_le_bytes(bytes[20..24].try_into().ok()?),
            len: u32::from_le_bytes(bytes[24..28].try_into().ok()?),
            kind: u16::from_le_bytes(bytes[28..30].try_into().ok()?),
            op: u16::from_le_bytes(bytes[30..32].try_into().ok()?),
            flags: u32::from_le_bytes(bytes[32..36].try_into().ok()?),
            buf_alloc: u32::from_le_bytes(bytes[36..40].try_into().ok()?),
            fwd_cnt: u32::from_le_bytes(bytes[40..44].try_into().ok()?),
        })
    }

    pub fn encode(self) -> [u8; HEADER_LEN] {
        let mut bytes = [0; HEADER_LEN];
        bytes[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.len.to_le_bytes());
        bytes[28..30].copy_from_slice(&self.kind.to_le_bytes());
        bytes[30..32].copy_from_slice(&self.op.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.flags.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        bytes
    }
}

#[derive(Default)]
struct Credit {
    sent: u32,
    peer_forwarded: u32,
    peer_alloc: u32,
}

impl Credit {
    fn available(&self) -> u32 {
        self.peer_alloc
            .saturating_sub(self.sent.wrapping_sub(self.peer_forwarded))
    }

    fn update(&mut self, header: Header) -> bool {
        // An acknowledgement cannot advance past the data already sent.
        if header.fwd_cnt.wrapping_sub(self.peer_forwarded)
            > self.sent.wrapping_sub(self.peer_forwarded)
        {
            return false;
        }
        self.peer_forwarded = header.fwd_cnt;
        self.peer_alloc = header.buf_alloc;
        true
    }
}

fn send(socket: &UnixStream, bytes: &[u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn sendv(socket: &UnixStream, iov: &[libc::iovec]) -> io::Result<usize> {
    let message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: iov.as_ptr().cast_mut(),
        msg_iovlen: iov.len() as _,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let n = unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn readv(socket: &UnixStream, iov: &[libc::iovec]) -> io::Result<usize> {
    let n = unsafe { libc::readv(socket.as_raw_fd(), iov.as_ptr(), iov.len() as _) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// The guest rx buffers on offer in one pass, in queue order. Each holds at
/// least a header.
pub trait RxBuffers {
    /// Payload bytes buffer `index` takes after its header, or `None` when
    /// the guest has not posted that many.
    fn room(&mut self, index: usize) -> Option<usize>;
    fn put_header(&mut self, index: usize, header: Header);
    /// Reads at most `max` bytes from `socket` straight into the payloads
    /// of buffers `first..`, filling each to its room before the next.
    fn recv(&mut self, socket: &UnixStream, first: usize, max: usize) -> io::Result<usize>;
}

/// The payload of one guest tx packet.
pub trait Payload {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Sends the bytes from `offset` on to `socket` without blocking.
    fn send(&self, socket: &UnixStream, offset: usize) -> io::Result<usize>;
    /// Appends the bytes from `offset` on to `out`.
    fn copy(&self, offset: usize, out: &mut VecDeque<u8>);
}

impl Payload for [u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn send(&self, socket: &UnixStream, offset: usize) -> io::Result<usize> {
        send(socket, &self[offset..])
    }

    fn copy(&self, offset: usize, out: &mut VecDeque<u8>) {
        out.extend(&self[offset..]);
    }
}

// Host to guest data stays in the host socket until the guest posts an rx
// buffer, so every packet is sized to the buffer that carries it.
pub struct Connection {
    socket: UnixStream,
    guest_port: u32,
    host_port: u32,
    reply: Option<SyncSender<io::Result<()>>>,
    established: bool,
    readable: bool,
    writable: bool,
    credit: Credit,
    credit_requested: bool,
    forwarded: u32,
    advertised: u32,
    received: u32,
    incoming: VecDeque<u8>,
    peer_shutdown: u32,
    local_shutdown: u32,
    send_request: bool,
    send_shutdown: u32,
    send_credit_request: bool,
    send_credit_update: bool,
    deadline: Option<Instant>,
}

impl Connection {
    fn header(&self, op: u16, flags: u32, len: usize) -> Header {
        Header {
            src_cid: HOST_CID,
            dst_cid: GUEST_CID,
            src_port: self.host_port,
            dst_port: self.guest_port,
            len: len as u32,
            kind: STREAM,
            op,
            flags,
            buf_alloc: BUFFER_SIZE as u32,
            fwd_cnt: self.forwarded,
        }
    }

    // Every header tells the guest how much we forwarded, so it doubles as
    // a credit update.
    fn stamp(&mut self, op: u16, flags: u32, len: usize) -> Header {
        self.advertised = self.forwarded;
        self.send_credit_update = false;
        self.header(op, flags, len)
    }

    fn control(&mut self, buffers: &mut dyn RxBuffers, index: usize, op: u16, flags: u32) -> usize {
        buffers.put_header(index, self.stamp(op, flags, 0));
        1
    }

    fn sends_data(&self) -> bool {
        self.established && self.local_shutdown & SEND == 0 && self.peer_shutdown & RECEIVE == 0
    }

    fn receive<P: Payload + ?Sized>(&mut self, header: Header, payload: &P) -> bool {
        if !self.credit.update(header) {
            return false;
        }
        if self.credit.available() > 0 {
            self.credit_requested = false;
        }
        if !self.established {
            if header.op != RESPONSE {
                return false;
            }
            self.established = true;
            self.deadline = None;
            return self
                .reply
                .take()
                .is_some_and(|reply| reply.send(Ok(())).is_ok());
        }
        match header.op {
            RW => {
                let window = BUFFER_SIZE
                    .saturating_sub(self.received.wrapping_sub(self.advertised) as usize);
                if self.peer_shutdown & SEND != 0
                    || self.local_shutdown & RECEIVE != 0
                    || payload.len() > window
                {
                    return false;
                }
                self.received = self.received.wrapping_add(payload.len() as u32);
                let mut offset = 0;
                // Straight from guest memory while nothing is queued ahead.
                while self.writable && self.incoming.is_empty() && offset < payload.len() {
                    match payload.send(&self.socket, offset) {
                        Ok(0) => return false,
                        Ok(n) => {
                            offset += n;
                            self.forwarded = self.forwarded.wrapping_add(n as u32);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.writable = false,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => return false,
                    }
                }
                if offset < payload.len() {
                    payload.copy(offset, &mut self.incoming);
                }
            }
            CREDIT_UPDATE => {}
            CREDIT_REQUEST => self.send_credit_update = true,
            SHUTDOWN if header.flags != 0 && header.flags & !3 == 0 => {
                self.peer_shutdown |= header.flags;
                if header.flags & RECEIVE != 0 {
                    let _ = self.socket.shutdown(Shutdown::Read);
                }
            }
            _ => return false,
        }
        true
    }

    fn pump(&mut self) -> io::Result<bool> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Ok(false);
        }
        if !self.established {
            return Ok(true);
        }
        while self.writable && !self.incoming.is_empty() {
            let (bytes, _) = self.incoming.as_slices();
            match send(&self.socket, bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.incoming.drain(..n);
                    self.forwarded = self.forwarded.wrapping_add(n as u32);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.writable = false,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if self.incoming.is_empty()
            && self.peer_shutdown & SEND != 0
            && self.local_shutdown & RECEIVE == 0
        {
            self.socket.shutdown(Shutdown::Write)?;
            self.local_shutdown |= RECEIVE;
        }
        if self.peer_shutdown == 3 && self.incoming.is_empty() {
            return Ok(false);
        }
        // The guest stalls when its view of our free space runs out, so
        // advertise forwarded bytes before that view drops below one packet.
        let peer_view =
            BUFFER_SIZE.saturating_sub(self.received.wrapping_sub(self.advertised) as usize);
        if self.forwarded != self.advertised && peer_view < MAX_PACKET {
            self.send_credit_update = true;
        }
        if self.local_shutdown == 3 && self.deadline.is_none() {
            self.send_shutdown = 3;
            self.deadline = Some(Instant::now() + HANDSHAKE_TIMEOUT);
        }
        Ok(true)
    }

    // Returns how many buffers from `first` on now hold a packet.
    fn fill(&mut self, buffers: &mut dyn RxBuffers, first: usize) -> io::Result<usize> {
        if !self.established {
            if !std::mem::take(&mut self.send_request) {
                return Ok(0);
            }
            return Ok(self.control(buffers, first, REQUEST, 0));
        }
        if self.send_shutdown != 0 {
            let flags = std::mem::take(&mut self.send_shutdown);
            return Ok(self.control(buffers, first, SHUTDOWN, flags));
        }
        let used = self.fill_data(buffers, first)?;
        if used > 0 {
            return Ok(used);
        }
        if std::mem::take(&mut self.send_credit_request) {
            return Ok(self.control(buffers, first, CREDIT_REQUEST, 0));
        }
        if self.send_credit_update {
            return Ok(self.control(buffers, first, CREDIT_UPDATE, 0));
        }
        Ok(0)
    }

    fn fill_data(&mut self, buffers: &mut dyn RxBuffers, first: usize) -> io::Result<usize> {
        if !self.readable || !self.sends_data() || buffers.room(first).unwrap_or(0) == 0 {
            return Ok(0);
        }
        let credit = self.credit.available() as usize;
        if credit == 0 {
            if !self.credit_requested {
                self.credit_requested = true;
                self.send_credit_request = true;
            }
            return Ok(0);
        }
        match buffers.recv(&self.socket, first, credit.min(MAX_TURN)) {
            Ok(0) => {
                self.readable = false;
                self.local_shutdown |= SEND;
                Ok(self.control(buffers, first, SHUTDOWN, SEND))
            }
            Ok(n) => {
                self.credit.sent = self.credit.sent.wrapping_add(n as u32);
                let mut index = first;
                let mut left = n;
                while left > 0 {
                    let len = buffers.room(index).unwrap_or(0).min(left);
                    if len == 0 {
                        break;
                    }
                    let header = self.stamp(RW, 0, len);
                    buffers.put_header(index, header);
                    left -= len;
                    index += 1;
                }
                Ok(index - first)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.readable = false;
                Ok(0)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "guest connection reset",
            )));
        }
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

pub struct Mux {
    connections: BTreeMap<u32, Connection>,
    resets: VecDeque<Header>,
    next_port: u32,
    last_port: u32,
}

impl Mux {
    pub fn new() -> Self {
        Self {
            connections: BTreeMap::new(),
            resets: VecDeque::new(),
            next_port: 1024,
            last_port: 0,
        }
    }

    pub fn clear(&mut self) {
        self.connections.clear();
        self.resets.clear();
        self.last_port = 0;
    }

    pub fn add(
        &mut self,
        socket: UnixStream,
        guest_port: u32,
        reply: SyncSender<io::Result<()>>,
    ) -> io::Result<u32> {
        if self.connections.len() >= MAX_CONNECTIONS
            || self.connections.len() + self.resets.len() >= MAX_RESETS
        {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "vsock connection limit reached",
            ));
        }
        // Never reuse a tuple during this device lifetime.
        let port = self
            .next_port
            .checked_add(1)
            .ok_or_else(|| io::Error::other("vsock ports exhausted"))?;
        self.next_port = port;
        socket.set_nonblocking(true)?;
        self.connections.insert(
            port,
            Connection {
                socket,
                host_port: port,
                guest_port,
                reply: Some(reply),
                established: false,
                readable: true,
                writable: true,
                credit: Credit::default(),
                credit_requested: false,
                forwarded: 0,
                advertised: 0,
                received: 0,
                incoming: VecDeque::new(),
                peer_shutdown: 0,
                local_shutdown: 0,
                send_request: true,
                send_shutdown: 0,
                send_credit_request: false,
                send_credit_update: false,
                deadline: Some(Instant::now() + HANDSHAKE_TIMEOUT),
            },
        );
        Ok(port)
    }

    pub fn can_receive(&self) -> bool {
        self.resets.len() + self.connections.len() < MAX_RESETS
    }

    fn reset(&mut self, header: Header) {
        if self.resets.len() + self.connections.len() < MAX_RESETS {
            self.resets.push_back(header);
        }
    }

    fn reset_unknown(&mut self, header: Header) {
        self.reset(Header {
            src_cid: HOST_CID,
            dst_cid: GUEST_CID,
            src_port: header.dst_port,
            dst_port: header.src_port,
            kind: header.kind,
            op: RST,
            ..Header::default()
        });
    }

    pub fn cancel(&mut self, port: u32) {
        if let Some(connection) = self.connections.remove(&port) {
            self.reset(connection.header(RST, 0, 0));
        }
    }

    pub fn wake(&mut self, port: u32, readable: bool, writable: bool) {
        if let Some(connection) = self.connections.get_mut(&port) {
            connection.readable |= readable;
            connection.writable |= writable;
        }
    }

    pub fn receive<P: Payload + ?Sized>(&mut self, header: Header, payload: &P) {
        if header.src_cid != GUEST_CID || header.dst_cid != HOST_CID {
            return;
        }
        let valid = header.kind == STREAM
            && header.len as usize == payload.len()
            && (header.op == RW || payload.is_empty())
            && (header.op == SHUTDOWN || header.flags == 0);
        let Some(mut connection) = self.connections.remove(&header.dst_port) else {
            if header.op != RST {
                self.reset_unknown(header);
            }
            return;
        };
        if connection.guest_port != header.src_port {
            self.connections.insert(header.dst_port, connection);
            if header.op != RST {
                self.reset_unknown(header);
            }
        } else if header.op == RST {
            // Dropping the stream wakes both pending connects and active readers.
        } else if valid && connection.receive(header, payload) {
            self.connections.insert(header.dst_port, connection);
        } else {
            self.reset(connection.header(RST, 0, 0));
        }
    }

    pub fn pump(&mut self) {
        let mut closed = Vec::new();
        for (port, connection) in &mut self.connections {
            if !connection.pump().unwrap_or(false) {
                closed.push(*port);
            }
        }
        for port in closed {
            self.cancel(port);
        }
    }

    /// When the earliest handshake or close times out.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.connections.values().filter_map(|c| c.deadline).min()
    }

    // Puts packets into the guest buffers on offer and returns how many it
    // used, all from the front. Connections take turns so a bulk transfer
    // cannot starve the others.
    pub fn fill(&mut self, buffers: &mut dyn RxBuffers) -> usize {
        let mut next = 0;
        while buffers.room(next).is_some() {
            if let Some(header) = self.resets.pop_front() {
                buffers.put_header(next, header);
                next += 1;
                continue;
            }
            let used = self.fill_turn(buffers, next);
            if used == 0 && self.resets.is_empty() {
                break;
            }
            next += used;
        }
        next
    }

    fn fill_turn(&mut self, buffers: &mut dyn RxBuffers, first: usize) -> usize {
        let mut port = self.last_port;
        for _ in 0..self.connections.len() {
            let Some(&next) = self
                .connections
                .range((Bound::Excluded(port), Bound::Unbounded))
                .next()
                .or_else(|| self.connections.first_key_value())
                .map(|(port, _)| port)
            else {
                return 0;
            };
            port = next;
            match self
                .connections
                .get_mut(&port)
                .unwrap()
                .fill(buffers, first)
            {
                Ok(0) => {}
                Ok(used) => {
                    self.last_port = port;
                    return used;
                }
                Err(_) => {
                    self.cancel(port);
                    return 0;
                }
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;

    // Linux 6.12.114 posts rx buffers of one page minus skb overhead.
    const RX_BUFFER: usize = 3776;

    fn pending(mux: &mut Mux) -> (u32, UnixStream, mpsc::Receiver<io::Result<()>>) {
        let (host, device) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let port = mux.add(device, 1024, tx).unwrap();
        (port, host, rx)
    }

    fn guest(port: u32, op: u16) -> Header {
        Header {
            src_cid: GUEST_CID,
            dst_cid: HOST_CID,
            src_port: 1024,
            dst_port: port,
            kind: STREAM,
            op,
            buf_alloc: BUFFER_SIZE as u32,
            ..Header::default()
        }
    }

    struct Buffers {
        data: Vec<Vec<u8>>,
        lens: Vec<usize>,
    }

    impl Buffers {
        fn new(count: usize, size: usize) -> Self {
            Self {
                data: vec![vec![0; size]; count],
                lens: vec![0; count],
            }
        }

        fn packets(&self, used: usize) -> Vec<(Header, Vec<u8>)> {
            (0..used)
                .map(|i| {
                    let header = Header::decode(&self.data[i]).unwrap();
                    assert_eq!(header.len as usize, self.lens[i] - HEADER_LEN);
                    (header, self.data[i][HEADER_LEN..self.lens[i]].to_vec())
                })
                .collect()
        }
    }

    impl RxBuffers for Buffers {
        fn room(&mut self, index: usize) -> Option<usize> {
            let buffer = self.data.get(index)?;
            Some((buffer.len() - HEADER_LEN).min(MAX_PACKET))
        }

        fn put_header(&mut self, index: usize, header: Header) {
            self.data[index][..HEADER_LEN].copy_from_slice(&header.encode());
            self.lens[index] = HEADER_LEN + header.len as usize;
        }

        fn recv(&mut self, socket: &UnixStream, first: usize, max: usize) -> io::Result<usize> {
            let mut iov = Vec::new();
            let mut left = max;
            for index in first.. {
                let room = self.room(index).unwrap_or(0).min(left);
                if room == 0 {
                    break;
                }
                iov.push(libc::iovec {
                    iov_base: self.data[index][HEADER_LEN..].as_mut_ptr().cast(),
                    iov_len: room,
                });
                left -= room;
            }
            readv(socket, &iov)
        }
    }

    fn batch(mux: &mut Mux, count: usize, size: usize) -> Vec<(Header, Vec<u8>)> {
        let mut buffers = Buffers::new(count, size);
        let used = mux.fill(&mut buffers);
        buffers.packets(used)
    }

    fn next(mux: &mut Mux, size: usize) -> Option<(Header, Vec<u8>)> {
        batch(mux, 1, size).pop()
    }

    fn packet(mux: &mut Mux) -> Option<(Header, Vec<u8>)> {
        next(mux, RX_BUFFER)
    }

    fn established(mux: &mut Mux) -> (u32, UnixStream) {
        let (port, host, reply) = pending(mux);
        assert_eq!(packet(mux).unwrap().0.op, REQUEST);
        mux.receive(guest(port, RESPONSE), &[][..]);
        reply.recv().unwrap().unwrap();
        (port, host)
    }

    fn guest_data(mux: &mut Mux, port: u32, data: &[u8]) {
        mux.receive(
            Header {
                len: data.len() as u32,
                ..guest(port, RW)
            },
            data,
        );
    }

    #[test]
    fn header_wire_layout() {
        let header = Header {
            src_cid: 0x0807060504030201,
            dst_cid: 0x100f0e0d0c0b0a09,
            src_port: 0x14131211,
            dst_port: 0x18171615,
            len: 0x1c1b1a19,
            kind: 0x1e1d,
            op: 0x201f,
            flags: 0x24232221,
            buf_alloc: 0x28272625,
            fwd_cnt: 0x2c2b2a29,
        };
        let wire: Vec<u8> = (1..=44).collect();
        assert_eq!(header.encode().as_slice(), wire);
        assert_eq!(Header::decode(&wire), Some(header));
        for len in 0..HEADER_LEN {
            assert!(Header::decode(&wire[..len]).is_none());
        }
    }

    #[test]
    fn connect_waits_for_response_and_reset_refuses() {
        let mut mux = Mux::new();
        let (port, _host, reply) = pending(&mut mux);
        assert!(reply.try_recv().is_err());
        let (request, _) = packet(&mut mux).unwrap();
        assert_eq!(request.src_port, port);
        assert_eq!(request.op, REQUEST);
        assert!(packet(&mut mux).is_none());
        mux.receive(guest(port, RST), &[][..]);
        assert!(reply.recv().unwrap().is_err());
        assert!(mux.connections.is_empty());
        assert!(packet(&mut mux).is_none());
    }

    #[test]
    fn guest_requests_and_unknown_types_are_reset() {
        let mut mux = Mux::new();
        for kind in [STREAM, 99] {
            let mut header = guest(5000, REQUEST);
            header.kind = kind;
            mux.receive(header, &[][..]);
            let (reset, _) = packet(&mut mux).unwrap();
            assert_eq!(reset.op, RST);
            assert_eq!(reset.kind, kind);
            assert_eq!(reset.src_port, 5000);
        }
        mux.receive(guest(5000, RST), &[][..]);
        assert!(packet(&mut mux).is_none());
    }

    #[test]
    fn independent_muxes_with_same_cid_and_ports() {
        let mut first = Mux::new();
        let mut second = Mux::new();
        let (a, mut host_a) = established(&mut first);
        let (b, mut host_b) = established(&mut second);
        assert_eq!(a, b);
        host_a.write_all(b"first").unwrap();
        host_b.write_all(b"second").unwrap();
        assert_eq!(packet(&mut first).unwrap().1, b"first");
        assert_eq!(packet(&mut second).unwrap().1, b"second");
    }

    #[test]
    fn packets_are_sized_to_the_rx_buffer() {
        let mut mux = Mux::new();
        let (port, host) = established(&mut mux);
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        host.set_nonblocking(false).unwrap();
        let writer = {
            let data = data.clone();
            let mut host = host.try_clone().unwrap();
            std::thread::spawn(move || host.write_all(&data).unwrap())
        };
        let mut received = Vec::new();
        while received.len() < data.len() {
            mux.receive(
                Header {
                    fwd_cnt: received.len() as u32,
                    ..guest(port, CREDIT_UPDATE)
                },
                &[][..],
            );
            mux.wake(port, true, false);
            while let Some((header, payload)) = packet(&mut mux) {
                if header.op == RW {
                    assert!(HEADER_LEN + payload.len() <= RX_BUFFER);
                    received.extend(payload);
                }
            }
        }
        writer.join().unwrap();
        assert_eq!(received, data);
        drop(host);
        mux.wake(port, true, false);
        let (shutdown, _) = packet(&mut mux).unwrap();
        assert_eq!((shutdown.op, shutdown.flags), (SHUTDOWN, SEND));
    }

    #[test]
    fn large_buffers_carry_at_most_one_packet_of_data() {
        let mut mux = Mux::new();
        let (port, host) = established(&mut mux);
        host.set_nonblocking(false).unwrap();
        let writer = {
            let mut host = host.try_clone().unwrap();
            std::thread::spawn(move || host.write_all(&vec![7; 3 * MAX_PACKET]).unwrap())
        };
        let mut total = 0;
        while total < 3 * MAX_PACKET {
            mux.wake(port, true, false);
            if let Some((header, payload)) = next(&mut mux, HEADER_LEN + BUFFER_SIZE) {
                assert_eq!(header.op, RW);
                assert!(payload.len() <= MAX_PACKET);
                total += payload.len();
            }
        }
        writer.join().unwrap();
    }

    #[test]
    fn buffers_too_small_for_data_carry_control_only() {
        let mut mux = Mux::new();
        let (_port, _host, _reply) = pending(&mut mux);
        assert_eq!(next(&mut mux, HEADER_LEN).unwrap().0.op, REQUEST);
        let mut mux = Mux::new();
        let (_port, mut host) = established(&mut mux);
        host.write_all(b"data").unwrap();
        assert!(next(&mut mux, HEADER_LEN).is_none());
        assert_eq!(packet(&mut mux).unwrap().1, b"data");
    }

    #[test]
    fn host_data_stops_at_credit_and_resumes_on_update() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        mux.receive(
            Header {
                buf_alloc: 3,
                ..guest(port, CREDIT_UPDATE)
            },
            &[][..],
        );
        host.write_all(b"abcdef").unwrap();
        assert_eq!(packet(&mut mux).unwrap().1, b"abc");
        assert_eq!(packet(&mut mux).unwrap().0.op, CREDIT_REQUEST);
        assert!(packet(&mut mux).is_none());
        mux.receive(
            Header {
                buf_alloc: 3,
                fwd_cnt: 3,
                ..guest(port, CREDIT_UPDATE)
            },
            &[][..],
        );
        assert_eq!(packet(&mut mux).unwrap().1, b"def");
    }

    #[test]
    fn credit_wraparound_and_invalid_ack() {
        let mut credit = Credit {
            sent: 2,
            peer_forwarded: u32::MAX - 1,
            peer_alloc: 8,
        };
        assert_eq!(credit.available(), 4);
        assert!(credit.update(Header {
            fwd_cnt: 1,
            buf_alloc: 8,
            ..Header::default()
        }));
        assert_eq!(credit.available(), 7);
        assert!(!credit.update(Header {
            fwd_cnt: 3,
            buf_alloc: 8,
            ..Header::default()
        }));
        credit.peer_alloc = 0;
        assert_eq!(credit.available(), 0);
    }

    #[test]
    fn credit_update_is_sent_before_the_guest_runs_out() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        let chunk = vec![1; MAX_PACKET];
        for _ in 0..3 {
            guest_data(&mut mux, port, &chunk);
        }
        mux.pump();
        let mut sink = vec![0; 3 * MAX_PACKET];
        host.read_exact(&mut sink).unwrap();
        assert!(packet(&mut mux).is_none());
        guest_data(&mut mux, port, &chunk);
        mux.pump();
        let (update, _) = packet(&mut mux).unwrap();
        assert_eq!(update.op, CREDIT_UPDATE);
        assert_eq!(update.fwd_cnt as usize, 4 * MAX_PACKET);
        assert!(packet(&mut mux).is_none());
    }

    #[test]
    fn receive_window_rejects_overrun() {
        let mut mux = Mux::new();
        let (port, _host) = established(&mut mux);
        mux.connections.get_mut(&port).unwrap().writable = false;
        guest_data(&mut mux, port, &vec![1; BUFFER_SIZE]);
        assert_eq!(mux.connections[&port].incoming.len(), BUFFER_SIZE);
        guest_data(&mut mux, port, &[0]);
        assert!(mux.connections.is_empty());
        assert_eq!(packet(&mut mux).unwrap().0.op, RST);
    }

    #[test]
    fn writes_wait_for_the_host_socket() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        let chunk = vec![2; 4096];
        while mux.connections[&port].writable {
            let connection = &mux.connections[&port];
            let used = connection.received.wrapping_sub(connection.advertised) as usize;
            if BUFFER_SIZE - used >= chunk.len() {
                guest_data(&mut mux, port, &chunk);
            }
            mux.pump();
            while packet(&mut mux).is_some() {}
        }
        let stuck = mux.connections[&port].incoming.len();
        assert!(stuck > 0);
        let mut sink = vec![0; BUFFER_SIZE];
        while host.read(&mut sink).is_ok() {}
        mux.pump();
        assert_eq!(mux.connections[&port].incoming.len(), stuck);
        mux.wake(port, false, true);
        mux.pump();
        assert!(mux.connections[&port].incoming.len() < stuck);
    }

    #[test]
    fn repeated_credit_requests_get_one_update() {
        let mut mux = Mux::new();
        let (port, _host) = established(&mut mux);
        for _ in 0..1000 {
            mux.receive(guest(port, CREDIT_REQUEST), &[][..]);
        }
        assert_eq!(packet(&mut mux).unwrap().0.op, CREDIT_UPDATE);
        assert!(packet(&mut mux).is_none());
    }

    #[test]
    fn resets_are_bounded() {
        let mut mux = Mux::new();
        while mux.can_receive() {
            mux.receive(guest(9999, REQUEST), &[][..]);
        }
        assert_eq!(mux.resets.len(), MAX_RESETS);
        for _ in 0..100 {
            mux.receive(guest(9999, REQUEST), &[][..]);
        }
        assert_eq!(mux.resets.len(), MAX_RESETS);
    }

    #[test]
    fn guest_half_close_drains_data_and_keeps_host_send_open() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        guest_data(&mut mux, port, b"end");
        mux.receive(
            Header {
                flags: SEND,
                ..guest(port, SHUTDOWN)
            },
            &[][..],
        );
        mux.pump();
        let mut data = [0; 3];
        host.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"end");
        assert_eq!(host.read(&mut data).unwrap(), 0);
        host.write_all(b"reply").unwrap();
        mux.pump();
        let packets: Vec<_> = std::iter::from_fn(|| packet(&mut mux)).collect();
        assert!(packets.iter().any(|(_, payload)| payload == b"reply"));
        assert!(mux.connections.contains_key(&port));
    }

    #[test]
    fn host_close_delivers_pending_data_before_shutdown() {
        let mut mux = Mux::new();
        let (_port, mut host) = established(&mut mux);
        host.write_all(b"last words").unwrap();
        drop(host);
        let (data, payload) = packet(&mut mux).unwrap();
        assert_eq!((data.op, payload.as_slice()), (RW, &b"last words"[..]));
        let (shutdown, _) = packet(&mut mux).unwrap();
        assert_eq!((shutdown.op, shutdown.flags), (SHUTDOWN, SEND));
    }

    #[test]
    fn host_half_close_and_guest_full_close() {
        let mut mux = Mux::new();
        let (port, host) = established(&mut mux);
        host.shutdown(Shutdown::Write).unwrap();
        let (shutdown, _) = packet(&mut mux).unwrap();
        assert_eq!((shutdown.op, shutdown.flags), (SHUTDOWN, SEND));
        mux.receive(
            Header {
                flags: 3,
                ..guest(port, SHUTDOWN)
            },
            &[][..],
        );
        mux.pump();
        assert_eq!(packet(&mut mux).unwrap().0.op, RST);
        assert!(mux.connections.is_empty());
    }

    #[test]
    fn matching_half_closes_keep_the_reverse_direction_open() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        host.shutdown(Shutdown::Write).unwrap();
        packet(&mut mux).unwrap();
        mux.receive(
            Header {
                flags: RECEIVE,
                ..guest(port, SHUTDOWN)
            },
            &[][..],
        );
        mux.pump();
        assert!(mux.connections[&port].deadline.is_none());
        assert!(packet(&mut mux).is_none());
        guest_data(&mut mux, port, b"reply");
        mux.pump();
        let mut bytes = [0; 5];
        host.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"reply");
    }

    #[test]
    fn full_close_sends_shutdown_and_waits_for_the_guest() {
        let mut mux = Mux::new();
        let (port, host) = established(&mut mux);
        mux.receive(
            Header {
                flags: SEND,
                ..guest(port, SHUTDOWN)
            },
            &[][..],
        );
        mux.pump();
        host.shutdown(Shutdown::Write).unwrap();
        let (first, _) = packet(&mut mux).unwrap();
        assert_eq!((first.op, first.flags), (SHUTDOWN, SEND));
        mux.pump();
        let (second, _) = packet(&mut mux).unwrap();
        assert_eq!((second.op, second.flags), (SHUTDOWN, 3));
        assert!(mux.connections[&port].deadline.is_some());
        mux.connections.get_mut(&port).unwrap().deadline = Some(Instant::now());
        mux.pump();
        assert_eq!(packet(&mut mux).unwrap().0.op, RST);
    }

    #[test]
    fn malformed_packets_and_spoofed_tuples() {
        let mut mux = Mux::new();
        let (port, _host) = established(&mut mux);
        mux.receive(
            Header {
                src_cid: 99,
                ..guest(port, RST)
            },
            &[][..],
        );
        mux.receive(
            Header {
                src_port: 99,
                ..guest(port, RST)
            },
            &[][..],
        );
        assert!(mux.connections.contains_key(&port));
        mux.receive(
            Header {
                len: 100,
                ..guest(port, RW)
            },
            &[0][..],
        );
        assert!(!mux.connections.contains_key(&port));
        assert_eq!(packet(&mut mux).unwrap().0.op, RST);
    }

    #[test]
    fn timeout_cancellation_and_reset_release_streams() {
        let mut mux = Mux::new();
        let (port, mut host, reply) = pending(&mut mux);
        mux.connections.get_mut(&port).unwrap().deadline = Some(Instant::now());
        mux.pump();
        assert!(reply.recv().unwrap().is_err());
        assert_eq!(host.read(&mut [0]).unwrap(), 0);
        assert_eq!(packet(&mut mux).unwrap().0.op, RST);
        let (port, mut host) = established(&mut mux);
        mux.cancel(port);
        assert_eq!(host.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn guest_reset_drops_unsent_host_data() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        host.write_all(b"never delivered").unwrap();
        mux.receive(guest(port, RST), &[][..]);
        assert!(packet(&mut mux).is_none());
        assert!(host.write_all(b"more").is_err());
    }

    #[test]
    fn device_reset_does_not_reuse_ports_with_pending_connects() {
        let mut mux = Mux::new();
        let (old_port, _old_host, old_reply) = pending(&mut mux);
        mux.clear();
        assert!(old_reply.recv().unwrap().is_err());
        let (new_port, _new_host, _new_reply) = pending(&mut mux);
        assert_ne!(old_port, new_port);
        mux.cancel(old_port);
        assert!(mux.connections.contains_key(&new_port));
    }

    #[test]
    fn one_read_fills_consecutive_buffers() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        let data: Vec<u8> = (0..5 * RX_BUFFER as u32).map(|i| (i % 251) as u8).collect();
        host.write_all(&data).unwrap();
        let packets = batch(&mut mux, 8, RX_BUFFER);
        assert!(packets.len() > 1);
        let mut received: Vec<u8> = Vec::new();
        for (header, payload) in &packets {
            assert_eq!((header.op, header.src_port), (RW, port));
            assert!(HEADER_LEN + payload.len() <= RX_BUFFER);
            received.extend(payload);
        }
        assert_eq!(received.len(), MAX_TURN.min(data.len()));
        assert_eq!(received, data[..received.len()]);
        assert_eq!(packets.last().unwrap().0.fwd_cnt, 0);
    }

    #[test]
    fn a_turn_stops_at_the_guest_credit() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        mux.receive(
            Header {
                buf_alloc: 5000,
                ..guest(port, CREDIT_UPDATE)
            },
            &[][..],
        );
        host.write_all(&vec![3; 3 * RX_BUFFER]).unwrap();
        let packets = batch(&mut mux, 8, RX_BUFFER);
        let data: Vec<usize> = packets
            .iter()
            .filter(|(header, _)| header.op == RW)
            .map(|(_, payload)| payload.len())
            .collect();
        assert_eq!(
            data,
            [RX_BUFFER - HEADER_LEN, 5000 - (RX_BUFFER - HEADER_LEN)]
        );
        assert_eq!(packets.last().unwrap().0.op, CREDIT_REQUEST);
    }

    #[test]
    fn guest_data_goes_straight_to_a_writable_socket() {
        let mut mux = Mux::new();
        let (port, mut host) = established(&mut mux);
        guest_data(&mut mux, port, b"direct");
        assert!(mux.connections[&port].incoming.is_empty());
        assert_eq!(mux.connections[&port].forwarded, 6);
        let mut bytes = [0; 6];
        host.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"direct");
    }

    #[test]
    fn connections_take_turns_within_a_batch() {
        let mut mux = Mux::new();
        let (a, mut host_a) = established(&mut mux);
        let (b, mut host_b) = established(&mut mux);
        host_a.write_all(&vec![1; MAX_TURN + 1000]).unwrap();
        host_b.write_all(&[2; 1000]).unwrap();
        let turn = MAX_TURN.div_ceil(RX_BUFFER - HEADER_LEN);
        let order: Vec<u32> = batch(&mut mux, turn + 2, RX_BUFFER)
            .iter()
            .map(|(header, _)| header.src_port)
            .collect();
        let mut expected = vec![a; turn];
        expected.extend([b, a]);
        assert_eq!(order, expected);
    }

    #[test]
    fn connections_take_turns() {
        let mut mux = Mux::new();
        let (a, mut host_a) = established(&mut mux);
        let (b, mut host_b) = established(&mut mux);
        host_a.write_all(&vec![1; 4 * RX_BUFFER]).unwrap();
        host_b.write_all(&vec![2; 4 * RX_BUFFER]).unwrap();
        let order: Vec<u32> = (0..6)
            .map(|_| packet(&mut mux).unwrap().0.src_port)
            .collect();
        assert_eq!(order, [a, b, a, b, a, b]);
    }
}
