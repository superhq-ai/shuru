use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::io::RawFd;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{self, Socket as TcpSocket};
use smoltcp::socket::udp::{self, Socket as UdpSocket};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr, IpEndpoint,
    IpListenEndpoint, Ipv4Address, Ipv4Packet, TcpPacket,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::device::VZDevice;

/// Gateway IP inside the virtual network (host-side smoltcp).
pub const GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
const PREFIX_LEN: u8 = 24;
/// Gateway MAC address (locally administered).
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

const TCP_RX_BUF_SIZE: usize = 65536;
const TCP_TX_BUF_SIZE: usize = 65536;

/// A new TCP connection from the guest, ready to be proxied.
pub struct TcpConnection {
    pub id: ConnectionId,
    pub dst: SocketAddr,
}

/// Opaque handle to a guest-side TCP connection inside smoltcp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(SocketHandle);

/// Events from the network stack to the proxy engine.
pub enum StackEvent {
    /// A new TCP connection was established from the guest.
    NewConnection(TcpConnection),
    /// Data received from the guest on an established connection.
    Data { id: ConnectionId, payload: Vec<u8> },
    /// Guest closed the connection.
    Closed { id: ConnectionId },
    /// A DNS query arrived on UDP port 53.
    DnsQuery { src: IpEndpoint, payload: Vec<u8> },
}

/// Commands from the proxy engine back to the network stack.
pub enum StackCommand {
    /// Send data to the guest on an established connection.
    Send { id: ConnectionId, payload: Vec<u8> },
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
            recv_buf: vec![0u8; TCP_RX_BUF_SIZE],
            syn_scratch: HashMap::new(),
            event_tx,
            cmd_rx,
        }
    }

    /// Run the poll loop. Blocks the current thread.
    pub fn run(&mut self) {
        loop {
            self.process_commands();

            // Drain all available frames and inspect for TCP SYN
            self.device.drain_recv();
            self.inspect_pending_frames();

            self.iface
                .poll(Self::now(), &mut self.device, &mut self.sockets);

            self.poll_tcp_sockets();
            self.drain_pending_sends();
            self.poll_dns_socket();

            let delay = self
                .iface
                .poll_delay(Self::now(), &self.sockets)
                .map(|d| {
                    let micros = d.total_micros();
                    if micros == 0 {
                        std::time::Duration::from_millis(1)
                    } else {
                        std::time::Duration::from_micros(micros.min(50_000))
                    }
                })
                .unwrap_or(std::time::Duration::from_millis(1));

            std::thread::sleep(delay);
        }
    }

    fn process_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                StackCommand::Send { id, payload } => {
                    self.pending_send
                        .entry(id.0)
                        .or_default()
                        .extend(&payload);
                }
                StackCommand::Close { id } => {
                    // Defer close until pending_send is drained so we don't
                    // drop data that hasn't been pushed to smoltcp yet.
                    if self.pending_send.get(&id.0).map_or(true, |p| p.is_empty()) {
                        // Nothing pending — close immediately
                        let socket = self.sockets.get_mut::<TcpSocket>(id.0);
                        socket.close();
                        self.connections.remove(&id.0);
                        self.pending_send.remove(&id.0);
                    } else {
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
        for ((dst_ip, dst_port), syn_count) in syn_entries {
            let existing = self
                .listening
                .get(&(dst_ip, dst_port))
                .map_or(0, |v| v.len());
            let needed = syn_count.saturating_sub(existing);

            for _ in 0..needed {
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
            if drained_empty && self.closing.remove(&handle) {
                let socket = self.sockets.get_mut::<TcpSocket>(handle);
                socket.close();
                self.connections.remove(&handle);
                self.pending_send.remove(&handle);
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
                    let ipv4 = match local.addr {
                        IpAddress::Ipv4(ip) => ip,
                    };
                    // Remove this specific handle from the listener vec
                    if let Some(handles) = self.listening.get_mut(&(ipv4, local.port)) {
                        handles.retain(|h| *h != handle);
                        if handles.is_empty() {
                            self.listening.remove(&(ipv4, local.port));
                        }
                    }

                    let actual_dst = SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::from(ipv4.octets())),
                        local.port,
                    );

                    self.connections.insert(handle, actual_dst);
                    let _ = self.event_tx.send(StackEvent::NewConnection(TcpConnection {
                        id: ConnectionId(handle),
                        dst: actual_dst,
                    }));
                }
            }

            // Read data from established connections
            if self.connections.contains_key(&handle) && socket.can_recv() {
                match socket.recv_slice(&mut self.recv_buf) {
                    Ok(n) if n > 0 => {
                        let _ = self.event_tx.send(StackEvent::Data {
                            id: ConnectionId(handle),
                            payload: self.recv_buf[..n].to_vec(),
                        });
                    }
                    _ => {}
                }
            }

            // Detect closed connections
            if self.connections.contains_key(&handle)
                && !socket.is_open()
                && !socket.may_recv()
                && !socket.may_send()
            {
                self.connections.remove(&handle);
                self.pending_send.remove(&handle);
                let _ = self.event_tx.send(StackEvent::Closed {
                    id: ConnectionId(handle),
                });
                self.sockets.remove(handle);
            }
        }
    }

    fn poll_dns_socket(&mut self) {
        let socket = self.sockets.get_mut::<UdpSocket>(self.dns_handle);
        let mut buf = [0u8; 4096];
        while socket.can_recv() {
            match socket.recv_slice(&mut buf) {
                Ok((n, meta)) => {
                    let _ = self.event_tx.send(StackEvent::DnsQuery {
                        src: meta.endpoint,
                        payload: buf[..n].to_vec(),
                    });
                }
                Err(_) => break,
            }
        }
    }

    fn now() -> Instant {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        Instant::from_micros(ts.as_micros() as i64)
    }
}
