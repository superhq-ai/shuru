mod connection;
mod queue;

use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use kvm_ioctls::VmFd;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use super::devices::{VirtioBackend, VirtioQueueState, VIRTIO_F_VERSION_1};
use connection::{Header, Mux, GUEST_CID, HEADER_LEN, MAX_PACKET};
use queue::{invalid_queue, Queue};

// A finite batch keeps reset and host connects responsive under guest load.
const BATCH: usize = 256;

struct Transport {
    rx: Queue,
    tx: Queue,
    mem: GuestMemoryMmap,
    vm: Arc<VmFd>,
    irq: u32,
    status: Arc<AtomicU32>,
    packet: Vec<u8>,
}

impl Transport {
    fn process(&mut self, mux: &mut Mux) -> io::Result<bool> {
        let mut used = false;
        for _ in 0..BATCH {
            if !mux.can_receive() {
                break;
            }
            let Some(head) = self.tx.head(&self.mem)? else {
                break;
            };
            if let Ok(descriptors) = self.tx.descriptors(&self.mem, head, false) {
                let mut bytes = vec![0; descriptors.iter().map(|(_, len)| len).sum()];
                let mut offset = 0;
                for (address, len) in descriptors {
                    self.mem
                        .read_slice(&mut bytes[offset..offset + len], GuestAddress(address))
                        .map_err(|_| invalid_queue())?;
                    offset += len;
                }
                if let Some(header) = Header::decode(&bytes) {
                    mux.receive(header, &bytes[HEADER_LEN..]);
                }
            }
            self.tx.complete(&self.mem, head, 0)?;
            used = true;
        }
        mux.pump();
        for _ in 0..BATCH {
            let Some(head) = self.rx.head(&self.mem)? else {
                break;
            };
            let Ok(descriptors) = self.rx.descriptors(&self.mem, head, true) else {
                self.rx.complete(&self.mem, head, 0)?;
                used = true;
                continue;
            };
            let capacity = descriptors
                .iter()
                .map(|(_, len)| len)
                .sum::<usize>()
                .min(self.packet.len());
            let Some(len) = mux.fill(&mut self.packet[..capacity]) else {
                break;
            };
            let mut written = 0;
            for (address, size) in descriptors {
                if written == len {
                    break;
                }
                let n = size.min(len - written);
                self.mem
                    .write_slice(&self.packet[written..written + n], GuestAddress(address))
                    .map_err(|_| invalid_queue())?;
                written += n;
            }
            self.rx.complete(&self.mem, head, len as u32)?;
            used = true;
        }
        if used {
            self.status.fetch_or(1, Ordering::Release);
            self.vm
                .set_irq_line(self.irq, true)
                .map_err(io::Error::other)?;
        }
        Ok(used)
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

fn run(control: Arc<VsockControl>) {
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 128];
    while !control.stopping.load(Ordering::Acquire) {
        let n = unsafe {
            libc::epoll_wait(
                control.epoll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                100,
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
            if event.u64 == 0 {
                let _ = control.wake.read();
                continue;
            }
            let ready = event.events as i32;
            let closed = ready & (libc::EPOLLHUP | libc::EPOLLERR) != 0;
            state.mux.wake(
                event.u64 as u32,
                closed || ready & (libc::EPOLLIN | libc::EPOLLRDHUP) != 0,
                closed || ready & libc::EPOLLOUT != 0,
            );
        }
        if let Some(mut transport) = state.transport.take() {
            match transport.process(&mut state.mux) {
                Ok(used) => {
                    state.transport = Some(transport);
                    // Completing guest buffers can unblock more work, so look again.
                    if used {
                        control.notify();
                    }
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
        vm: &Arc<VmFd>,
        irq: u32,
        status: Arc<AtomicU32>,
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
                    vm: vm.clone(),
                    irq,
                    status,
                    packet: vec![0; HEADER_LEN + MAX_PACKET],
                });
            }
            _ => {
                state.failed = true;
            }
        }
        self.control.notify();
    }
    fn process_queue(
        &mut self,
        _: u16,
        _: &mut [VirtioQueueState],
        _: &GuestMemoryMmap,
        _: &Arc<VmFd>,
        _: u32,
    ) {
        self.control.notify();
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
