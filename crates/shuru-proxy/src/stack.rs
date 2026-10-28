use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::io::RawFd;
use std::sync::Arc;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{self, Socket as TcpSocket};
use smoltcp::socket::udp::{self, Socket as UdpSocket};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr, IpEndpoint,
    IpListenEndpoint, Ipv4Address, Ipv4Packet, TcpPacket,
};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use crate::device::{DrainStatus, VZDevice};

/// Gateway IP inside the virtual network (host-side smoltcp).
pub const GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
const PREFIX_LEN: u8 = 24;
/// Gateway MAC address (locally administered).
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

// Per-connection smoltcp ring buffers, allocated on every SYN the guest
// sends. They only span the hop between guest and host, under a millisecond;
// the remote round trip is the host kernel socket. 512 KiB and 256 KiB
// measured the same single-stream throughput as 1 MiB and 512 KiB (about 2
// Gbit/s down, 9 up on an M-series Mac), 256 KiB and 128 KiB lost a fifth.
const TCP_RX_BUF_SIZE: usize = 512 * 1024;
const TCP_TX_BUF_SIZE: usize = 256 * 1024;
// Hard ceiling on live TCP sockets, listening or connected. At the buffer
// sizes above this bounds the netstack at 384 MiB no matter what the guest
// does; SYNs beyond it are dropped and the guest retransmits.
const MAX_TCP_SOCKETS: usize = 512;

// Bytes in flight between the stack and the engine on one connection, in
// each direction. Toward the host, the stack leaves data in smoltcp once
// this many bytes wait on the engine, so a stalled remote closes the
// window of the guest; each message also counts MESSAGE_COST, so trickled
// one-byte segments cannot queue unbounded numbers of messages. Toward the
// guest, a producer waits for permits that come back as the stack moves
// bytes into smoltcp, so a guest that stops reading stops the upstream read.
pub(crate) const TO_HOST_BUDGET: usize = 128 * 1024;
pub(crate) const MESSAGE_COST: usize = 256;
pub(crate) const TO_GUEST_BUDGET: usize = 128 * 1024;
const RECV_CHUNK: usize = 64 * 1024;
// DNS queries being resolved at once; more are dropped and the guest
// resolver retries.
const MAX_DNS_INFLIGHT: usize = 64;
// A listener whose SYN never completes a handshake is reaped after this.
const LISTEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
// A socket the host closed is dropped once the guest is silent this long.
const CLOSE_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(60);

/// A new TCP connection from the guest, ready to be proxied.
pub struct TcpConnection {
    pub id: ConnectionId,
    pub dst: SocketAddr,
    /// Permits for bytes toward the guest, TO_GUEST_BUDGET in total.
    pub to_guest: Arc<Semaphore>,
}

/// Opaque handle to a guest-side TCP connection inside smoltcp. smoltcp
/// reuses a handle once its socket is removed, so the serial tells a later
/// connection apart from the one a late command was meant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(SocketHandle, u64);

/// Events from the network stack to the proxy engine.
pub enum StackEvent {
    /// A new TCP connection was established from the guest.
    NewConnection(TcpConnection),
    /// Data received from the guest on an established connection.
    Data { id: ConnectionId, payload: Vec<u8> },
    /// Guest closed the connection.
    Closed { id: ConnectionId },
    /// A DNS query arrived on UDP port 53. The permit is held until it is
    /// answered.
    DnsQuery {
        src: IpEndpoint,
        payload: Vec<u8>,
        permit: OwnedSemaphorePermit,
    },
}

/// Commands from the proxy engine back to the network stack.
pub enum StackCommand {
    /// Send data to the guest on an established connection. The sender
    /// holds a permit for each byte.
    Send { id: ConnectionId, payload: Vec<u8> },
    /// The engine took `n` bytes of guest data off the connection.
    Consumed { id: ConnectionId, n: usize },
    /// Close a connection from the host side.
    Close { id: ConnectionId },
    /// Send a DNS response back to the guest.
    DnsResponse { dst: IpEndpoint, payload: Vec<u8> },
}

/// The smoltcp-based network stack.
///
/// Runs on a dedicated thread, polling the VZDevice and smoltcp interface.
/// Communicates with the async proxy engine via channels.
pub struct NetworkStack {
    device: VZDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dns_handle: SocketHandle,
    connections: HashMap<SocketHandle, SocketAddr>,
    /// Listening sockets per destination. Multiple sockets support concurrent
    /// connections to the same IP:port (e.g. parallel npm downloads).
    listening: HashMap<(Ipv4Address, u16), Vec<SocketHandle>>,
    pending_send: HashMap<SocketHandle, VecDeque<u8>>,
    /// Sockets waiting for pending_send to drain before closing.
    closing: HashSet<SocketHandle>,
    /// Sockets the host closed, removed once smoltcp finishes with them.
    draining: HashSet<SocketHandle>,
    /// When each listener was created.
    listen_since: HashMap<SocketHandle, std::time::Instant>,
    /// Guest bytes, plus MESSAGE_COST per message, the engine has not taken.
    to_host: HashMap<SocketHandle, usize>,
    to_guest: HashMap<SocketHandle, Arc<Semaphore>>,
    serials: HashMap<SocketHandle, u64>,
    next_serial: u64,
    dns_inflight: Arc<Semaphore>,
    /// Reusable buffer for poll_tcp_sockets recv_slice.
    recv_buf: Vec<u8>,
    /// Reusable scratch map for inspect_pending_frames.
    syn_scratch: HashMap<(Ipv4Address, u16), usize>,
    event_tx: mpsc::UnboundedSender<StackEvent>,
    cmd_rx: mpsc::UnboundedReceiver<StackCommand>,
}

impl NetworkStack {
    pub fn new(
        host_fd: RawFd,
        event_tx: mpsc::UnboundedSender<StackEvent>,
        cmd_rx: mpsc::UnboundedReceiver<StackCommand>,
    ) -> Self {
        let mut device = VZDevice::new(host_fd);

        let config = Config::new(HardwareAddress::Ethernet(GATEWAY_MAC));
        let mut iface = Interface::new(config, &mut device, Self::now());
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GATEWAY_IP), PREFIX_LEN))
                .unwrap();
        });
        iface.set_any_ip(true);
        iface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY_IP)
            .unwrap();

        let mut sockets = SocketSet::new(vec![]);

        // DNS socket: listen on gateway IP, port 53
        let udp_rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
        let udp_tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
        let mut dns_socket = UdpSocket::new(udp_rx, udp_tx);
        dns_socket
            .bind(IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(GATEWAY_IP)),
                port: 53,
            })
            .expect("bind DNS socket");
        let dns_handle = sockets.add(dns_socket);

        NetworkStack {
            device,
            iface,
            sockets,
            dns_handle,
            connections: HashMap::new(),
            listening: HashMap::new(),
            pending_send: HashMap::new(),
            closing: HashSet::new(),
            draining: HashSet::new(),
            listen_since: HashMap::new(),
            to_host: HashMap::new(),
            to_guest: HashMap::new(),
            serials: HashMap::new(),
            next_serial: 0,
            dns_inflight: Arc::new(Semaphore::new(MAX_DNS_INFLIGHT)),
            recv_buf: vec![0u8; RECV_CHUNK],
            syn_scratch: HashMap::new(),
            event_tx,
            cmd_rx,
        }
    }

    /// Run the poll loop. Blocks the current thread until the proxy engine
    /// drops `cmd_tx` or the host FD closes.
    ///
    /// Internally this hosts a Tokio current-thread runtime so the loop can
    /// `select!` over (a) host FD readiness via [`AsyncFd`], (b) commands on
    /// `cmd_rx`, and (c) smoltcp's own timer (`poll_delay`). This replaces a
    /// previous `thread::sleep(1ms)` busy-wait that bounded the smoltcp ↔
    /// guest internal RTT and capped throughput at ~1 MB/s regardless of
    /// buffer sizes.
    pub fn run(&mut self) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("failed to build current-thread runtime for netstack");
        rt.block_on(self.run_async());
    }

    async fn run_async(&mut self) {
        let async_fd = match AsyncFd::with_interest(self.device.fd(), Interest::READABLE) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("AsyncFd registration failed for host_fd: {e}");
                return;
            }
        };

        loop {
            // Synchronous work phase: drain everything available right now.
            // (Commands are drained inside the select! arm so they share a
            //  single waker; this top-of-loop drain is just for frames.)
            let drain_status = self.device.drain_recv();
            self.reap_sockets();
            self.inspect_pending_frames();
            self.iface
                .poll(Self::now(), &mut self.device, &mut self.sockets);
            self.poll_tcp_sockets();
            self.drain_pending_sends();
            self.poll_dns_socket();

            let poll_delay = self.iface.poll_delay(Self::now(), &self.sockets);

            // If there is still local work, do not await — loop immediately.
            // This covers: drain stopped at MAX_PENDING_FRAMES with the
            // kernel buffer still non-empty, smoltcp wants an immediate
            // re-poll, or pre-read frames remain queued (e.g. produced by
            // smoltcp itself during the previous poll).
            let immediate = matches!(drain_status, DrainStatus::HitCap)
                || !self.device.pending_rx_empty()
                || matches!(poll_delay, Some(d) if d.total_micros() == 0);
            if immediate {
                tokio::task::yield_now().await;
                continue;
            }

            // Convert smoltcp's poll_delay into a tokio sleep deadline.
            let sleep_micros = poll_delay.map(|d| d.total_micros());

            tokio::select! {
                biased;
                // (1) Frame arrived from guest.
                ready = async_fd.readable() => {
                    match ready {
                        Ok(mut guard) => {
                            // Acknowledge readiness; the next loop iteration
                            // will drain via MSG_DONTWAIT. If we are wrong and
                            // the FD isn't really readable yet, drain_recv()
                            // will return WouldBlock and we re-await.
                            guard.clear_ready();
                        }
                        Err(e) => {
                            warn!("AsyncFd readable error: {e}");
                            return;
                        }
                    }
                }
                // (2) Command arrived from proxy engine. Drain the rest with
                //     try_recv() so a burst of commands costs one wake.
                maybe_cmd = self.cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(cmd) => {
                            self.handle_command(cmd);
                            while let Ok(cmd) = self.cmd_rx.try_recv() {
                                self.handle_command(cmd);
                            }
                        }
                        None => {
                            // Proxy engine dropped its cmd_tx — shut down.
                            return;
                        }
                    }
                }
                // (3) smoltcp's own timer (retransmit / delayed ACK).
                _ = sleep_or_pending(sleep_micros) => {}
            }
        }
    }

    fn handle_command(&mut self, cmd: StackCommand) {
        let id = match &cmd {
            StackCommand::Send { id, .. }
            | StackCommand::Consumed { id, .. }
            | StackCommand::Close { id } => Some(*id),
            StackCommand::DnsResponse { .. } => None,
        };
        if id.is_some_and(|id| self.serials.get(&id.0) != Some(&id.1)) {
            return;
        }
        match cmd {
            StackCommand::Send { id, payload } => {
                // Ignore sends for connections already removed by poll_tcp_sockets
                if !self.connections.contains_key(&id.0) {
                    return;
                }
                self.pending_send.entry(id.0).or_default().extend(&payload);
            }
            StackCommand::Consumed { id, n } => {
                if let Some(in_flight) = self.to_host.get_mut(&id.0) {
                    *in_flight = in_flight.saturating_sub(n + MESSAGE_COST);
                }
            }
            StackCommand::Close { id } => {
                // Connection may have already been cleaned up by poll_tcp_sockets
                if !self.connections.contains_key(&id.0) {
                    return;
                }
                // Defer close until pending_send is drained so we don't
                // drop data that has not been pushed to smoltcp yet. A guest
                // that stops answering is timed out by smoltcp.
                if self.pending_send.get(&id.0).is_none_or(|p| p.is_empty()) {
                    self.close_socket(id.0);
                } else {
                    let socket = self.sockets.get_mut::<TcpSocket>(id.0);
                    socket.set_timeout(Some(CLOSE_TIMEOUT));
                    self.closing.insert(id.0);
                }
            }
            StackCommand::DnsResponse { dst, payload } => {
                let socket = self.sockets.get_mut::<UdpSocket>(self.dns_handle);
                if let Err(e) = socket.send_slice(&payload, dst) {
                    warn!("failed to send DNS response: {e}");
                }
            }
        }
    }

    fn inspect_pending_frames(&mut self) {
        // Count SYNs per destination. Multiple SYNs to the same IP:port
        // need separate listening sockets (one per connection).
        self.syn_scratch.clear();
        for frame in self.device.pending_frames() {
            if let Some((dst_ip, dst_port)) = Self::parse_syn_dst(frame) {
                *self.syn_scratch.entry((dst_ip, dst_port)).or_default() += 1;
            }
        }

        // Drain into a local vec to release &mut self borrow on syn_scratch
        let syn_entries: Vec<_> = self.syn_scratch.drain().collect();
        let mut live_sockets = self.sockets.iter().count();
        for ((dst_ip, dst_port), syn_count) in syn_entries {
            let existing = self
                .listening
                .get(&(dst_ip, dst_port))
                .map_or(0, |v| v.len());
            let needed = syn_count.saturating_sub(existing);

            for _ in 0..needed {
                if live_sockets >= MAX_TCP_SOCKETS {
                    warn!(
                        "SYN → {}:{} dropped: {} TCP sockets live, limit {}",
                        dst_ip, dst_port, live_sockets, MAX_TCP_SOCKETS
                    );
                    break;
                }
                debug!("SYN → {}:{}, adding listener", dst_ip, dst_port);

                let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_RX_BUF_SIZE]);
                let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_TX_BUF_SIZE]);
                let mut socket = TcpSocket::new(rx_buf, tx_buf);
                if socket
                    .listen(IpListenEndpoint {
                        addr: Some(IpAddress::Ipv4(dst_ip)),
                        port: dst_port,
                    })
                    .is_err()
                {
                    warn!("failed to listen on {}:{}", dst_ip, dst_port);
                    continue;
                }
                let handle = self.sockets.add(socket);
                self.listen_since.insert(handle, std::time::Instant::now());
                live_sockets += 1;
                self.listening
                    .entry((dst_ip, dst_port))
                    .or_default()
                    .push(handle);
            }
        }
    }

    /// Parse a TCP SYN's destination from a raw Ethernet frame.
    /// Returns None if the frame is not a TCP SYN (without ACK).
    fn parse_syn_dst(frame: &[u8]) -> Option<(Ipv4Address, u16)> {
        let eth = EthernetFrame::new_checked(frame).ok()?;
        if eth.ethertype() != smoltcp::wire::EthernetProtocol::Ipv4 {
            return None;
        }
        let ipv4 = Ipv4Packet::new_checked(eth.payload()).ok()?;
        if ipv4.next_header() != smoltcp::wire::IpProtocol::Tcp {
            return None;
        }
        let tcp = TcpPacket::new_checked(ipv4.payload()).ok()?;
        if !tcp.syn() || tcp.ack() {
            return None;
        }
        Some((ipv4.dst_addr(), tcp.dst_port()))
    }

    fn drain_pending_sends(&mut self) {
        let handles: Vec<SocketHandle> = self.pending_send.keys().copied().collect();
        for handle in handles {
            // Socket may have been removed by poll_tcp_sockets — discard stale entry
            if !self.connections.contains_key(&handle) && !self.closing.contains(&handle) {
                self.pending_send.remove(&handle);
                continue;
            }
            let socket = self.sockets.get_mut::<TcpSocket>(handle);
            let mut drained_empty = false;
            if let Some(pending) = self.pending_send.get_mut(&handle) {
                // Loop to drain both slices of the VecDeque ring buffer
                while !pending.is_empty() && socket.can_send() {
                    let (a, _) = pending.as_slices();
                    if a.is_empty() {
                        break;
                    }
                    match socket.send_slice(a) {
                        Ok(0) => break,
                        Ok(n) => {
                            pending.drain(..n);
                            if let Some(budget) = self.to_guest.get(&handle) {
                                budget.add_permits(n);
                            }
                        }
                        Err(e) => {
                            warn!("failed to send to guest: {e}");
                            pending.clear();
                            break;
                        }
                    }
                }
                drained_empty = pending.is_empty();
            }
            // If this socket was waiting to close and all data is now flushed,
            // initiate the TCP close (FIN).
            if drained_empty && self.closing.contains(&handle) {
                self.close_socket(handle);
            }
        }
    }

    fn poll_tcp_sockets(&mut self) {
        let handles: Vec<SocketHandle> = self
            .listening
            .values()
            .flatten()
            .copied()
            .chain(self.connections.keys().copied())
            .collect();

        for handle in handles {
            let socket = self.sockets.get_mut::<TcpSocket>(handle);

            // LISTEN → ESTABLISHED transition
            if socket.is_active() && !self.connections.contains_key(&handle) {
                // local_endpoint() is the destination the guest was trying to reach
                // (because any_ip=true, smoltcp accepted it as local)
                if let Some(local) = socket.local_endpoint() {
                    let IpAddress::Ipv4(ipv4) = local.addr;
                    // Remove this specific handle from the listener vec
                    if let Some(handles) = self.listening.get_mut(&(ipv4, local.port)) {
                        handles.retain(|h| *h != handle);
                        if handles.is_empty() {
                            self.listening.remove(&(ipv4, local.port));
                        }
                    }

                    let actual_dst =
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ipv4.octets())), local.port);

                    self.listen_since.remove(&handle);
                    self.next_serial += 1;
                    self.serials.insert(handle, self.next_serial);
                    self.connections.insert(handle, actual_dst);
                    self.to_host.insert(handle, 0);
                    let to_guest = Arc::new(Semaphore::new(TO_GUEST_BUDGET));
                    self.to_guest.insert(handle, to_guest.clone());
                    let _ = self.event_tx.send(StackEvent::NewConnection(TcpConnection {
                        id: ConnectionId(handle, self.next_serial),
                        dst: actual_dst,
                        to_guest,
                    }));
                }
            }

            // Read data from established connections, up to the budget.
            let room = self
                .to_host
                .get(&handle)
                .map_or(0, |in_flight| TO_HOST_BUDGET.saturating_sub(*in_flight));
            if room > MESSAGE_COST && socket.can_recv() {
                let max = (room - MESSAGE_COST).min(self.recv_buf.len());
                if let Ok(n) = socket.recv_slice(&mut self.recv_buf[..max]) {
                    if n > 0 {
                        if let Some(in_flight) = self.to_host.get_mut(&handle) {
                            *in_flight += n + MESSAGE_COST;
                        }
                        let _ = self.event_tx.send(StackEvent::Data {
                            id: ConnectionId(handle, self.serials[&handle]),
                            payload: self.recv_buf[..n].to_vec(),
                        });
                    }
                }
            }

            // Detect closed connections
            if self.connections.contains_key(&handle)
                && !socket.is_open()
                && !socket.may_recv()
                && !socket.may_send()
            {
                let id = ConnectionId(handle, self.serials[&handle]);
                self.forget(handle);
                let _ = self.event_tx.send(StackEvent::Closed { id });
                self.sockets.remove(handle);
            }
        }
    }

    fn poll_dns_socket(&mut self) {
        let socket = self.sockets.get_mut::<UdpSocket>(self.dns_handle);
        let mut buf = [0u8; 4096];
        while socket.can_recv() {
            match socket.recv_slice(&mut buf) {
                Ok((n, meta)) => match self.dns_inflight.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let _ = self.event_tx.send(StackEvent::DnsQuery {
                            src: meta.endpoint,
                            payload: buf[..n].to_vec(),
                            permit,
                        });
                    }
                    Err(_) => debug!("DNS query dropped: {MAX_DNS_INFLIGHT} in flight"),
                },
                Err(_) => break,
            }
        }
    }

    /// Stop tracking a connection. Producers waiting to send to it fail.
    fn forget(&mut self, handle: SocketHandle) {
        self.connections.remove(&handle);
        self.pending_send.remove(&handle);
        self.closing.remove(&handle);
        self.to_host.remove(&handle);
        self.serials.remove(&handle);
        if let Some(budget) = self.to_guest.remove(&handle) {
            budget.close();
        }
    }

    /// Start a host-side close. The socket stays in the set until smoltcp
    /// finishes the close or the guest goes silent for CLOSE_TIMEOUT.
    fn close_socket(&mut self, handle: SocketHandle) {
        let socket = self.sockets.get_mut::<TcpSocket>(handle);
        socket.set_timeout(Some(CLOSE_TIMEOUT));
        socket.close();
        self.forget(handle);
        self.draining.insert(handle);
    }

    /// Remove closed sockets and listeners whose handshake never came.
    fn reap_sockets(&mut self) {
        let sockets = &mut self.sockets;
        self.draining.retain(|handle| {
            let state = sockets.get::<TcpSocket>(*handle).state();
            let done = matches!(state, tcp::State::Closed | tcp::State::TimeWait);
            if done {
                sockets.remove(*handle);
            }
            !done
        });
        let since = &mut self.listen_since;
        self.listening.retain(|_, handles| {
            handles.retain(|handle| {
                let stale = sockets.get::<TcpSocket>(*handle).state() == tcp::State::Listen
                    && since
                        .get(handle)
                        .is_some_and(|t| t.elapsed() >= LISTEN_TIMEOUT);
                if stale {
                    since.remove(handle);
                    sockets.remove(*handle);
                }
                !stale
            });
            !handles.is_empty()
        });
    }

    fn now() -> Instant {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        Instant::from_micros(ts.as_micros() as i64)
    }
}

/// Sleep for `micros` microseconds, or never resolve if `None`.
/// Used as the third arm of `select!` so the loop wakes when smoltcp's own
/// timer fires (delayed ACK / retransmit) but stays asleep otherwise.
async fn sleep_or_pending(micros: Option<u64>) {
    match micros {
        Some(0) => {} // immediate; loop will continue without awaiting wakers
        Some(us) => tokio::time::sleep(std::time::Duration::from_micros(us)).await,
        None => std::future::pending::<()>().await,
    }
}
