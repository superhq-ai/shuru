mod connection;
mod queue;

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap, VolatileSlice};
use vmm_sys_util::eventfd::EventFd;

use super::devices::{
    Interrupt, VirtioBackend, VirtioQueueState, INTERRUPT_USED, VIRTIO_F_VERSION_1,
};
use connection::{
    readv, sendv, Header, Mux, Payload, RxBuffers, GUEST_CID, HEADER_LEN, MAX_PACKET,
};
use queue::{invalid_queue, Queue};

// A finite batch keeps reset and host connects responsive under guest load.
const BATCH: usize = 256;
const IOV_MAX: usize = 1024;
// Epoll tokens: 0 is the wake eventfd, host ports fit in 32 bits, kicks
// sit above them.
const KICK: u64 = 1 << 40;

/// Splits a descriptor chain into its header bytes and the payload after.
fn split<'a>(
    mem: &'a GuestMemoryMmap,
    descriptors: &[(u64, usize)],
) -> io::Result<(Vec<VolatileSlice<'a>>, Vec<VolatileSlice<'a>>, usize)> {
    let mut header = Vec::new();
    let mut payload = Vec::new();
    let mut missing = HEADER_LEN;
    let mut len = 0;
    for &(address, size) in descriptors {
        if size == 0 {
            continue;
        }
        let slice = mem
            .get_slice(GuestAddress(address), size)
            .map_err(|_| invalid_queue())?;
        let take = missing.min(size);
        if take > 0 {
            header.push(slice.subslice(0, take).map_err(|_| invalid_queue())?);
            missing -= take;
        }
        if size > take {
            payload.push(slice.offset(take).map_err(|_| invalid_queue())?);
            len += size - take;
        }
    }
    if missing > 0 {
        return Err(invalid_queue());
    }
    Ok((header, payload, len))
}

struct TxPacket<'a> {
    slices: Vec<VolatileSlice<'a>>,
    len: usize,
}

impl<'a> TxPacket<'a> {
    fn parse(mem: &'a GuestMemoryMmap, descriptors: &[(u64, usize)]) -> io::Result<(Header, Self)> {
        let (header, slices, len) = split(mem, descriptors)?;
        let mut bytes = [0; HEADER_LEN];
        let mut offset = 0;
        for slice in &header {
            offset += slice.copy_to(&mut bytes[offset..]);
        }
        let header = Header::decode(&bytes).ok_or_else(invalid_queue)?;
        Ok((header, Self { slices, len }))
    }

    /// The slices holding the bytes from `offset` on.
    fn from(&self, mut offset: usize) -> impl Iterator<Item = VolatileSlice<'a>> + '_ {
        self.slices.iter().filter_map(move |slice| {
            if offset >= slice.len() {
                offset -= slice.len();
                return None;
            }
            let rest = slice.offset(offset).ok();
            offset = 0;
            rest
        })
    }
}

impl Payload for TxPacket<'_> {
    fn len(&self) -> usize {
        self.len
    }

    fn send(&self, socket: &UnixStream, offset: usize) -> io::Result<usize> {
        let slices: Vec<_> = self.from(offset).take(IOV_MAX).collect();
        let guards: Vec<_> = slices.iter().map(|slice| slice.ptr_guard()).collect();
        let iov: Vec<libc::iovec> = guards
            .iter()
            .zip(&slices)
            .map(|(guard, slice)| libc::iovec {
                iov_base: guard.as_ptr().cast_mut().cast(),
                iov_len: slice.len(),
            })
            .collect();
        sendv(socket, &iov)
    }

    fn copy(&self, offset: usize, out: &mut VecDeque<u8>) {
        let start = out.len();
        out.resize(start + self.len - offset, 0);
        let (front, back) = out.as_mut_slices();
        let mut parts = if start >= front.len() {
            vec![&mut back[start - front.len()..]]
        } else {
            vec![&mut front[start..], back]
        }
        .into_iter();
        let mut dst: &mut [u8] = parts.next().unwrap_or_default();
        for slice in self.from(offset) {
            let mut done = 0;
            while done < slice.len() {
                if dst.is_empty() {
                    match parts.next() {
                        Some(next) => dst = next,
                        None => return,
                    }
                }
                let n = slice.offset(done).map_or(0, |rest| rest.copy_to(dst));
                if n == 0 {
                    return;
                }
                dst = &mut std::mem::take(&mut dst)[n..];
                done += n;
            }
        }
    }
}

struct RxChain<'a> {
    head: u16,
    header: Vec<VolatileSlice<'a>>,
    payload: Vec<VolatileSlice<'a>>,
    room: usize,
    len: u32,
}

// Buffers are read from the ring only as packets need them, so a small
// message does not pay for every buffer the guest posted.
struct RxBatch<'a> {
    mem: &'a GuestMemoryMmap,
    queue: &'a Queue,
    chains: Vec<RxChain<'a>>,
    done: bool,
    // A malformed chain at the front, to hand back empty.
    invalid: Option<u16>,
    error: Option<io::Error>,
}

impl<'a> RxBatch<'a> {
    fn new(mem: &'a GuestMemoryMmap, queue: &'a Queue) -> Self {
        Self {
            mem,
            queue,
            chains: Vec::new(),
            done: false,
            invalid: None,
            error: None,
        }
    }

    fn load(&mut self, index: usize) -> bool {
        while self.chains.len() <= index && !self.done {
            if self.chains.len() == BATCH {
                self.done = true;
                break;
            }
            match self.queue.head_at(self.mem, self.chains.len()) {
                Ok(Some(head)) => match self
                    .queue
                    .descriptors(self.mem, head, true)
                    .and_then(|descriptors| split(self.mem, &descriptors))
                {
                    Ok((header, payload, room)) => self.chains.push(RxChain {
                        head,
                        header,
                        payload,
                        room,
                        len: 0,
                    }),
                    Err(_) => {
                        if self.chains.is_empty() {
                            self.invalid = Some(head);
                        }
                        self.done = true;
                    }
                },
                Ok(None) => self.done = true,
                Err(error) => {
                    self.error = Some(error);
                    self.done = true;
                }
            }
        }
        index < self.chains.len()
    }
}

impl RxBuffers for RxBatch<'_> {
    fn room(&mut self, index: usize) -> Option<usize> {
        self.load(index)
            .then(|| self.chains[index].room.min(MAX_PACKET))
    }

    fn put_header(&mut self, index: usize, header: Header) {
        let chain = &mut self.chains[index];
        let bytes = header.encode();
        let mut offset = 0;
        for slice in &chain.header {
            slice.copy_from(&bytes[offset..offset + slice.len()]);
            offset += slice.len();
        }
        chain.len = (HEADER_LEN + header.len as usize) as u32;
    }

    fn recv(&mut self, socket: &UnixStream, first: usize, max: usize) -> io::Result<usize> {
        let mut parts = Vec::new();
        let mut left = max;
        'chains: for index in first.. {
            let mut room = self.room(index).unwrap_or(0).min(left);
            if room == 0 {
                break;
            }
            left -= room;
            for slice in &self.chains[index].payload {
                if room == 0 {
                    break;
                }
                if parts.len() == IOV_MAX {
                    break 'chains;
                }
                let len = slice.len().min(room);
                parts.push((slice.ptr_guard_mut(), len));
                room -= len;
            }
        }
        let iov: Vec<libc::iovec> = parts
            .iter()
            .map(|(guard, len)| libc::iovec {
                iov_base: guard.as_ptr().cast(),
                iov_len: *len,
            })
            .collect();
        readv(socket, &iov)
    }
}

struct Transport {
    rx: Queue,
    tx: Queue,
    mem: GuestMemoryMmap,
    interrupt: Arc<Interrupt>,
}

impl Transport {
    // One pass over both queues with guest kicks off. Returns whether there
    // is more to do right away.
    fn process(&mut self, mux: &mut Mux) -> io::Result<bool> {
        let Transport {
            rx,
            tx,
            mem,
            interrupt,
        } = self;
        tx.set_notify(mem, false)?;
        rx.set_notify(mem, false)?;

        let mut tx_used = false;
        for _ in 0..BATCH {
            if !mux.can_receive() {
                break;
            }
            let Some(head) = tx.head(mem)? else {
                break;
            };
            if let Ok(descriptors) = tx.descriptors(mem, head, false) {
                if let Ok((header, payload)) = TxPacket::parse(mem, &descriptors) {
                    mux.receive(header, &payload);
                }
            }
            tx.complete(mem, head, 0)?;
            tx_used = true;
        }
        mux.pump();

        let mut batch = RxBatch::new(mem, rx);
        let used = mux.fill(&mut batch);
        let filled: Vec<(u16, u32)> = batch.chains[..used]
            .iter()
            .map(|chain| (chain.head, chain.len))
            .collect();
        let invalid = batch.invalid;
        if let Some(error) = batch.error.take() {
            return Err(error);
        }
        drop(batch);
        for (head, len) in filled {
            rx.complete(mem, head, len)?;
        }
        if let Some(head) = invalid {
            rx.complete(mem, head, 0)?;
        }
        let rx_used = used > 0 || invalid.is_some();
        let rx_drained = rx.head(mem)?.is_none();

        tx.set_notify(mem, true)?;
        rx.set_notify(mem, true)?;
        if (tx_used && tx.wants_interrupt(mem)) || (rx_used && rx.wants_interrupt(mem)) {
            interrupt.raise(INTERRUPT_USED);
        }
        // Buffers posted while kicks were off came without one.
        Ok(tx_used
            || rx_used
            || (mux.can_receive() && tx.head(mem)?.is_some())
            || (rx_drained && rx.head(mem)?.is_some()))
    }
}

struct State {
    transport: Option<Transport>,
    mux: Mux,
    failed: bool,
}

pub struct VsockControl {
    state: Mutex<State>,
    wake: EventFd,
    kicks: Mutex<Vec<EventFd>>,
    epoll: OwnedFd,
    stopping: AtomicBool,
}

impl VsockControl {
    pub fn deactivate(&self) {
        let mut state = self.state.lock().unwrap();
        state.transport = None;
        state.mux.clear();
        state.failed = false;
        self.notify();
    }

    fn notify(&self) {
        let _ = self.wake.write(1);
    }

    fn register(&self, fd: i32, token: u64, events: i32) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: events as u32,
            u64: token,
        };
        let ret =
            unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn connect(&self, port: u32) -> io::Result<TcpStream> {
        let (client, device) = UnixStream::pair()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let host_port;
        {
            let mut state = self.state.lock().unwrap();
            if state.failed || state.transport.is_none() || self.stopping.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "vsock device is not ready",
                ));
            }
            let fd = device.as_raw_fd();
            host_port = state.mux.add(device, port, sender)?;
            let events = libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET;
            if let Err(error) = self.register(fd, host_port as u64, events) {
                state.mux.cancel(host_port);
                return Err(error);
            }
        }
        self.notify();
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {
                // The public backend API uses TcpStream for any connected stream fd.
                Ok(unsafe { TcpStream::from_raw_fd(client.into_raw_fd()) })
            }
            result => {
                self.state.lock().unwrap().mux.cancel(host_port);
                self.notify();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "vsock connect timed out",
                    )),
                }
            }
        }
    }
}

pub struct VsockBackend {
    control: Arc<VsockControl>,
    worker: Option<std::thread::JoinHandle<()>>,
    negotiated: bool,
}

impl VsockBackend {
    pub fn new() -> io::Result<Self> {
        let wake = EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)?;
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let control = Arc::new(VsockControl {
            state: Mutex::new(State {
                transport: None,
                mux: Mux::new(),
                failed: false,
            }),
            wake,
            kicks: Mutex::new(Vec::new()),
            epoll: unsafe { OwnedFd::from_raw_fd(fd) },
            stopping: AtomicBool::new(false),
        });
        control.register(control.wake.as_raw_fd(), 0, libc::EPOLLIN)?;
        let worker_control = control.clone();
        let worker = std::thread::Builder::new()
            .name("shuru-vsock".into())
            .spawn(move || run(worker_control))?;
        Ok(Self {
            control,
            worker: Some(worker),
            negotiated: false,
        })
    }

    pub fn control(&self) -> Arc<VsockControl> {
        self.control.clone()
    }
}

/// Milliseconds until `deadline`, rounded up so it has passed on waking.
fn wait_ms(deadline: Option<Instant>) -> i32 {
    deadline.map_or(-1, |deadline| {
        let left = deadline.saturating_duration_since(Instant::now());
        left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32
    })
}

fn run(control: Arc<VsockControl>) {
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 128];
    let mut busy = false;
    while !control.stopping.load(Ordering::Acquire) {
        let timeout = if busy {
            0
        } else {
            wait_ms(control.state.lock().unwrap().mux.next_deadline())
        };
        let n = unsafe {
            libc::epoll_wait(
                control.epoll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                timeout,
            )
        };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        let mut state = control.state.lock().unwrap();
        if n < 0 {
            state.failed = true;
            state.mux.clear();
            state.transport = None;
            break;
        }
        for event in &events[..n as usize] {
            let token = event.u64;
            if token == 0 {
                let _ = control.wake.read();
                continue;
            }
            if token & KICK != 0 {
                if let Some(kick) = control.kicks.lock().unwrap().get((token & !KICK) as usize) {
                    let _ = kick.read();
                }
                continue;
            }
            let ready = event.events as i32;
            let closed = ready & (libc::EPOLLHUP | libc::EPOLLERR) != 0;
            state.mux.wake(
                token as u32,
                closed || ready & (libc::EPOLLIN | libc::EPOLLRDHUP) != 0,
                closed || ready & libc::EPOLLOUT != 0,
            );
        }
        busy = false;
        if let Some(mut transport) = state.transport.take() {
            match transport.process(&mut state.mux) {
                Ok(more) => {
                    state.transport = Some(transport);
                    busy = more;
                }
                Err(_) => {
                    state.failed = true;
                    state.mux.clear();
                }
            }
        }
    }
}

impl VirtioBackend for VsockBackend {
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
            0 => GUEST_CID as u32,
            4 => (GUEST_CID >> 32) as u32,
            _ => 0,
        }
    }
    fn config_write(&mut self, _: u64, _: u32) {}
    fn features_ok(&mut self, features: u64) {
        self.negotiated = features & VIRTIO_F_VERSION_1 != 0;
    }
    fn activate(
        &mut self,
        queues: &[VirtioQueueState],
        mem: &GuestMemoryMmap,
        interrupt: Arc<Interrupt>,
    ) {
        let mut state = self.control.state.lock().unwrap();
        let queues = queues
            .iter()
            .map(|queue| {
                if !queue.ready {
                    return Err(invalid_queue());
                }
                Queue::new(
                    queue.size,
                    queue.desc_addr,
                    queue.avail_addr,
                    queue.used_addr,
                    mem,
                )
            })
            .collect::<io::Result<Vec<_>>>();
        match queues {
            Ok(mut queues) if queues.len() == 3 && !state.failed && self.negotiated => {
                queues.pop();
                let tx = queues.pop().unwrap();
                let rx = queues.pop().unwrap();
                state.transport = Some(Transport {
                    rx,
                    tx,
                    mem: mem.clone(),
                    interrupt,
                });
            }
            _ => {
                state.failed = true;
            }
        }
        self.control.notify();
    }
    fn process_queue(&mut self, _: u16, _: &mut [VirtioQueueState], _: &GuestMemoryMmap) -> bool {
        false
    }
    fn take_kicks(&mut self, kicks: &[EventFd]) -> io::Result<bool> {
        let mut owned = self.control.kicks.lock().unwrap();
        for (index, kick) in kicks.iter().enumerate() {
            let kick = kick.try_clone()?;
            self.control
                .register(kick.as_raw_fd(), KICK | index as u64, libc::EPOLLIN)?;
            owned.push(kick);
        }
        Ok(true)
    }
    fn reset(&mut self) {
        self.negotiated = false;
        self.control.deactivate();
    }
}

impl Drop for VsockBackend {
    fn drop(&mut self) {
        self.control.stopping.store(true, Ordering::Release);
        self.control.notify();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use vm_memory::Bytes;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1024 * 1024)]).unwrap()
    }

    fn header(len: usize) -> Header {
        Header {
            len: len as u32,
            op: connection::RW,
            ..Header::default()
        }
    }

    #[test]
    fn tx_payload_after_a_header_that_shares_its_descriptor() {
        let mem = memory();
        let payload: Vec<u8> = (0..110).collect();
        mem.write_slice(&header(110).encode(), GuestAddress(0x1000))
            .unwrap();
        mem.write_slice(&payload[..10], GuestAddress(0x1000 + HEADER_LEN as u64))
            .unwrap();
        mem.write_slice(&payload[10..], GuestAddress(0x2000))
            .unwrap();
        let descriptors = [(0x1000, HEADER_LEN + 10), (0x2000, 100)];
        let (decoded, packet) = TxPacket::parse(&mem, &descriptors).unwrap();
        assert_eq!(decoded, header(110));
        assert_eq!(Payload::len(&packet), 110);

        let (mut host, device) = UnixStream::pair().unwrap();
        assert_eq!(packet.send(&device, 5).unwrap(), 105);
        let mut sent = vec![0; 105];
        host.read_exact(&mut sent).unwrap();
        assert_eq!(sent, payload[5..]);

        // Start the copy at the wrap point of the deque.
        let mut out = VecDeque::with_capacity(128);
        out.extend([0xee; 100]);
        out.drain(..90);
        packet.copy(3, &mut out);
        assert_eq!(out.len(), 10 + 107);
        assert!(out.iter().take(10).all(|&b| b == 0xee));
        assert!(out
            .iter()
            .skip(10)
            .copied()
            .eq(payload[3..].iter().copied()));
    }

    #[test]
    fn rx_header_split_across_descriptors_and_one_read_for_several_buffers() {
        let mem = memory();
        let first = [(0x4000, 20), (0x5000, HEADER_LEN - 20 + 100)];
        let second = [(0x6000, HEADER_LEN + 50)];
        let queue = Queue::new(8, 0x1000, 0x2000, 0x3000, &mem).unwrap();
        let mut batch = RxBatch::new(&mem, &queue);
        batch.done = true;
        for (head, descriptors) in [(0, &first[..]), (1, &second[..])] {
            let (header, payload, room) = split(&mem, descriptors).unwrap();
            batch.chains.push(RxChain {
                head,
                header,
                payload,
                room,
                len: 0,
            });
        }
        assert_eq!(
            (batch.room(0), batch.room(1), batch.room(2)),
            (Some(100), Some(50), None)
        );

        let (mut host, device) = UnixStream::pair().unwrap();
        let data: Vec<u8> = (0..130).collect();
        host.write_all(&data).unwrap();
        assert_eq!(batch.recv(&device, 0, 1000).unwrap(), 130);
        batch.put_header(0, header(100));
        batch.put_header(1, header(30));
        assert_eq!((batch.chains[0].len, batch.chains[1].len), (144, 74));

        let mut wire = [0; HEADER_LEN];
        mem.read_slice(&mut wire[..20], GuestAddress(0x4000))
            .unwrap();
        mem.read_slice(&mut wire[20..], GuestAddress(0x5000))
            .unwrap();
        assert_eq!(Header::decode(&wire), Some(header(100)));
        let mut got = vec![0; 100];
        mem.read_slice(&mut got, GuestAddress(0x5000 + 24)).unwrap();
        assert_eq!(got, data[..100]);
        let mut got = vec![0; 30];
        mem.read_slice(&mut got, GuestAddress(0x6000 + HEADER_LEN as u64))
            .unwrap();
        assert_eq!(got, data[100..]);
    }

    #[test]
    fn chains_shorter_than_a_header_are_rejected() {
        let mem = memory();
        assert!(split(&mem, &[(0x1000, HEADER_LEN - 1)]).is_err());
        assert!(split(&mem, &[(0x1000, 10), (0x2000, 0), (0x3000, 33)]).is_err());
        assert_eq!(split(&mem, &[(0x1000, HEADER_LEN)]).unwrap().2, 0);
    }

    // Posts one writable descriptor per head, then makes them available.
    fn post(mem: &GuestMemoryMmap, buffers: &[(u64, u32)]) {
        for (index, &(address, len)) in buffers.iter().enumerate() {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&address.to_le_bytes());
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(&2u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            mem.write_slice(&bytes, GuestAddress(0x1000 + index as u64 * 16))
                .unwrap();
            mem.write_obj(
                (index as u16).to_le(),
                GuestAddress(0x2004 + index as u64 * 2),
            )
            .unwrap();
        }
        mem.write_obj((buffers.len() as u16).to_le(), GuestAddress(0x2002))
            .unwrap();
    }

    #[test]
    fn buffers_are_read_from_the_ring_only_when_needed() {
        let mem = memory();
        let queue = Queue::new(8, 0x1000, 0x2000, 0x3000, &mem).unwrap();
        post(&mem, &[(0x4000, 3776), (0x5000, 3776), (0x6000, 3776)]);
        let mut batch = RxBatch::new(&mem, &queue);
        assert_eq!(batch.room(0), Some(3776 - HEADER_LEN));
        assert_eq!(batch.chains.len(), 1);
        assert_eq!(batch.room(2), Some(3776 - HEADER_LEN));
        assert_eq!(batch.room(3), None);
        assert_eq!(batch.chains.len(), 3);
        assert!(batch.invalid.is_none());
    }

    #[test]
    fn a_malformed_buffer_at_the_front_is_set_aside() {
        let mem = memory();
        let queue = Queue::new(8, 0x1000, 0x2000, 0x3000, &mem).unwrap();
        post(&mem, &[(0x4000, 10), (0x5000, 3776)]);
        let mut batch = RxBatch::new(&mem, &queue);
        assert_eq!(batch.room(0), None);
        assert_eq!(batch.invalid, Some(0));

        post(&mem, &[(0x4000, 3776), (0x5000, 10)]);
        let mut batch = RxBatch::new(&mem, &queue);
        assert!(batch.room(0).is_some());
        assert_eq!(batch.room(1), None);
        assert!(batch.invalid.is_none());
    }
}
