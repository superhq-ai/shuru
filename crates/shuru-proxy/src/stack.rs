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
use crate::egress::{Gate, Verdict};
use crate::flows::Flows;

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
// does; SYNs beyond it are held back and the guest retransmits.
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
    /// SYNs to let through per destination, where the rest are held back.
    held: HashMap<(Ipv4Address, u16), usize>,
    gate: Gate,
    event_tx: mpsc::UnboundedSender<StackEvent>,
    cmd_rx: mpsc::UnboundedReceiver<StackCommand>,
    flows: Arc<Flows>,
}

impl NetworkStack {
    pub fn new(
        host_fd: RawFd,
        event_tx: mpsc::UnboundedSender<StackEvent>,
        cmd_rx: mpsc::UnboundedReceiver<StackCommand>,
        flows: Arc<Flows>,
        gate: Gate,
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
            held: HashMap::new(),
            gate,
            event_tx,
            cmd_rx,
            flows,
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
            self.gate.report(std::time::Instant::now());
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
                self.flows.sent_to_guest(id.1);
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
        let now = std::time::Instant::now();
        for ((dst_ip, dst_port), syn_count) in syn_entries {
            let existing = self
                .listening
                .get(&(dst_ip, dst_port))
                .map_or(0, |v| v.len());
            let needed = syn_count.saturating_sub(existing);

            for added in 0..needed {
                // A SYN without a listener gets a reset from smoltcp, the
                // answer of a closed port. One held back never reaches it.
                let keep = existing + added;
                if live_sockets >= MAX_TCP_SOCKETS {
                    warn!(
                        "SYN → {}:{} held: {} TCP sockets live, limit {}",
                        dst_ip, dst_port, live_sockets, MAX_TCP_SOCKETS
                    );
                    self.held.insert((dst_ip, dst_port), keep);
                    break;
                }
                match self.gate.admit((dst_ip, dst_port), now) {
                    Verdict::Open => {}
                    Verdict::Refuse => {
                        debug!("SYN → {dst_ip}:{dst_port} refused: port denied");
                        break;
                    }
                    Verdict::Wait => {
                        debug!("SYN → {dst_ip}:{dst_port} held: over the connection rate");
                        self.held.insert((dst_ip, dst_port), keep);
                        break;
                    }
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

        if !self.held.is_empty() {
            let held = &mut self.held;
            self.device.retain_pending(|frame| {
                let Some(dst) = Self::parse_syn_dst(frame) else {
                    return true;
                };
                match held.get_mut(&dst) {
                    Some(0) => false,
                    Some(keep) => {
                        *keep -= 1;
                        true
                    }
                    None => true,
                }
            });
            held.clear();
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

            // Handed to the proxy once the handshake completes. A socket
            // closed in SYN-RECEIVED never sends its FIN.
            let connected = matches!(
                socket.state(),
                tcp::State::Established | tcp::State::CloseWait
            );
            if connected && !self.connections.contains_key(&handle) {
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
                    self.flows.opened(self.next_serial);
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
                        self.flows.sent_by_guest(self.serials[&handle]);
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
        if let Some(serial) = self.serials.remove(&handle) {
            self.flows.closed(serial);
        }
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
                let state = sockets.get::<TcpSocket>(*handle).state();
                let stale = matches!(state, tcp::State::Listen | tcp::State::SynReceived)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{
        ArpOperation, ArpPacket, ArpRepr, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Repr,
        TcpControl, TcpRepr, TcpSeqNumber,
    };

    use super::*;
    use crate::config::{ConnectionRate, ProxyConfig, Rate};

    const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0, 0, 0, 0, 0x02]);
    const GUEST_IP: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);
    const REMOTE: Ipv4Address = Ipv4Address::new(93, 184, 216, 34);

    #[derive(Debug, PartialEq)]
    enum Answer {
        SynAck,
        Reset,
    }

    /// A stack on one end of a socketpair, the guest on the other.
    struct Rig {
        guest: RawFd,
        cmd_tx: Option<mpsc::UnboundedSender<StackCommand>>,
        thread: Option<std::thread::JoinHandle<()>>,
        _events: mpsc::UnboundedReceiver<StackEvent>,
    }

    impl Rig {
        fn new(config: &ProxyConfig) -> Self {
            let (guest, host) = crate::create_socketpair().unwrap();
            let (event_tx, events) = mpsc::unbounded_channel();
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let gate = Gate::new(config, std::time::Instant::now());
            let thread = std::thread::spawn(move || {
                let mut stack =
                    NetworkStack::new(host, event_tx, cmd_rx, Arc::new(Flows::default()), gate);
                stack.run();
                unsafe { libc::close(host) };
            });
            let rig = Rig {
                guest,
                cmd_tx: Some(cmd_tx),
                thread: Some(thread),
                _events: events,
            };
            rig.resolve_gateway();
            rig
        }

        /// What a guest does before its first connection; it also tells
        /// the stack where the guest is, so its first answer is not lost
        /// to a lookup of its own.
        fn resolve_gateway(&self) {
            let request = ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Request,
                source_hardware_addr: GUEST_MAC,
                source_protocol_addr: GUEST_IP,
                target_hardware_addr: EthernetAddress([0; 6]),
                target_protocol_addr: GATEWAY_IP,
            };
            self.send_arp(request, EthernetAddress::BROADCAST);
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut buf = vec![0u8; 65536];
            while std::time::Instant::now() < deadline {
                let n = unsafe {
                    libc::recv(
                        self.guest,
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if n > 0 {
                    let frame = EthernetFrame::new_checked(&buf[..n as usize]).unwrap();
                    if frame.ethertype() == EthernetProtocol::Arp {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the gateway never answered ARP");
        }

        fn syn(&self, dst_port: u16, src_port: u16) {
            let tcp = TcpRepr {
                src_port,
                dst_port,
                control: TcpControl::Syn,
                seq_number: TcpSeqNumber(1000),
                ack_number: None,
                window_len: 64240,
                window_scale: None,
                max_seg_size: Some(1460),
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload: &[],
            };
            let ip = Ipv4Repr {
                src_addr: GUEST_IP,
                dst_addr: REMOTE,
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            };
            let eth = EthernetRepr {
                src_addr: GUEST_MAC,
                dst_addr: GATEWAY_MAC,
                ethertype: EthernetProtocol::Ipv4,
            };
            let mut buf = vec![0u8; eth.buffer_len() + ip.buffer_len() + tcp.buffer_len()];
            let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
            eth.emit(&mut frame);
            let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
            ip.emit(&mut packet, &ChecksumCapabilities::default());
            let mut segment = TcpPacket::new_unchecked(packet.payload_mut());
            tcp.emit(
                &mut segment,
                &IpAddress::Ipv4(GUEST_IP),
                &IpAddress::Ipv4(REMOTE),
                &ChecksumCapabilities::default(),
            );
            self.send(&buf);
        }

        fn send(&self, frame: &[u8]) {
            let n = unsafe { libc::send(self.guest, frame.as_ptr().cast(), frame.len(), 0) };
            assert!(n > 0, "send failed: {}", std::io::Error::last_os_error());
        }

        /// What the stack answers the SYN from `src_port` within `wait`,
        /// answering its ARP requests for the guest on the way.
        fn answer(&self, src_port: u16, wait: Duration) -> Option<Answer> {
            let deadline = std::time::Instant::now() + wait;
            let mut buf = vec![0u8; 65536];
            while std::time::Instant::now() < deadline {
                let n = unsafe {
                    libc::recv(
                        self.guest,
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if n <= 0 {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let frame = EthernetFrame::new_checked(&buf[..n as usize]).unwrap();
                match frame.ethertype() {
                    EthernetProtocol::Arp => self.arp_reply(&frame),
                    EthernetProtocol::Ipv4 => {
                        let packet = Ipv4Packet::new_checked(frame.payload()).unwrap();
                        let segment = TcpPacket::new_checked(packet.payload()).unwrap();
                        if segment.dst_port() != src_port {
                            continue;
                        }
                        if segment.rst() {
                            return Some(Answer::Reset);
                        }
                        if segment.syn() && segment.ack() {
                            return Some(Answer::SynAck);
                        }
                    }
                    _ => {}
                }
            }
            None
        }

        fn arp_reply(&self, frame: &EthernetFrame<&[u8]>) {
            let request =
                ArpRepr::parse(&ArpPacket::new_checked(frame.payload()).unwrap()).unwrap();
            let ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Request,
                source_hardware_addr,
                source_protocol_addr,
                ..
            } = request
            else {
                return;
            };
            let reply = ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr: GUEST_MAC,
                source_protocol_addr: GUEST_IP,
                target_hardware_addr: source_hardware_addr,
                target_protocol_addr: source_protocol_addr,
            };
            self.send_arp(reply, source_hardware_addr);
        }

        fn send_arp(&self, arp: ArpRepr, to: EthernetAddress) {
            let eth = EthernetRepr {
                src_addr: GUEST_MAC,
                dst_addr: to,
                ethertype: EthernetProtocol::Arp,
            };
            let mut buf = vec![0u8; eth.buffer_len() + arp.buffer_len()];
            let mut out = EthernetFrame::new_unchecked(&mut buf[..]);
            eth.emit(&mut out);
            arp.emit(&mut ArpPacket::new_unchecked(out.payload_mut()));
            self.send(&buf);
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.cmd_tx.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            unsafe { libc::close(self.guest) };
        }
    }

    fn paced(destinations: Rate) -> ProxyConfig {
        ProxyConfig {
            deny_ports: vec![25],
            connection_rate: Some(ConnectionRate {
                connections: Rate {
                    per_second: 1000,
                    burst: 1000,
                },
                destinations,
            }),
            ..Default::default()
        }
    }

    const SHORT: Duration = Duration::from_millis(300);

    #[test]
    fn a_denied_port_is_answered_as_closed() {
        let rig = Rig::new(&paced(Rate {
            per_second: 10,
            burst: 10,
        }));
        rig.syn(25, 40001);
        assert_eq!(rig.answer(40001, SHORT), Some(Answer::Reset));
        rig.syn(443, 40002);
        assert_eq!(rig.answer(40002, SHORT), Some(Answer::SynAck));
    }

    #[test]
    fn a_syn_over_the_rate_gets_no_answer_until_it_comes_again() {
        let rig = Rig::new(&paced(Rate {
            per_second: 2,
            burst: 2,
        }));
        rig.syn(443, 40001);
        assert_eq!(rig.answer(40001, SHORT), Some(Answer::SynAck));
        rig.syn(80, 40002);
        assert_eq!(rig.answer(40002, SHORT), Some(Answer::SynAck));
        // Neither a reset nor a handshake: the guest is left to retransmit.
        rig.syn(8080, 40003);
        assert_eq!(rig.answer(40003, SHORT), None);
        std::thread::sleep(Duration::from_millis(300));
        rig.syn(8080, 40003);
        assert_eq!(rig.answer(40003, SHORT), Some(Answer::SynAck));
    }
}
