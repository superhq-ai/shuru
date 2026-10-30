//! Listening TCP ports (RFC 0012). While a port watch or an attached exec
//! session wants them, a scanner reads /proc/net/tcp and tcp6 every 500 ms
//! and reports ports that open and close: to every port watch, and to the
//! session whose cgroup holds the process that owns the socket.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use shuru_proto::frame;
use shuru_proto::{Port, PortEvent};

const TICK: Duration = Duration::from_millis(500);
const MAX_WATCHES: usize = 64;
const WATCH_BACKLOG: usize = 1024;
const WATCH_STALL: Duration = Duration::from_secs(10);
const LISTEN: &str = "0A";
const SESSION_CGROUP: &str = "/shuru/session-";

struct Watch {
    id: u64,
    tx: SyncSender<PortEvent>,
    listed: bool,
}

#[derive(Default)]
struct Interest {
    watches: Vec<Watch>,
    /// Attachments to exec sessions, as the session loop last counted them.
    attached: usize,
    /// A new watch waits for its list.
    pending: bool,
    next_id: u64,
}

impl Interest {
    fn any(&self) -> bool {
        !self.watches.is_empty() || self.attached > 0
    }
}

static INTEREST: LazyLock<Mutex<Interest>> = LazyLock::new(Mutex::default);
static WAKE: Condvar = Condvar::new();
static SCANNER: Once = Once::new();

fn interest() -> MutexGuard<'static, Interest> {
    INTEREST.lock().unwrap_or_else(|e| e.into_inner())
}

fn start_scanner() {
    SCANNER.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("ports".into())
            .spawn(scan_loop);
        if let Err(e) = spawned {
            eprintln!("shuru-guest: ports: {e}");
        }
    });
}

/// Called by the session loop whenever its attachment count changes.
pub fn set_attached(count: usize) {
    let mut interest = interest();
    if interest.attached == count {
        return;
    }
    interest.attached = count;
    drop(interest);
    if count > 0 {
        start_scanner();
    }
    WAKE.notify_all();
}

/// A port watch, registered with the scanner until it is dropped.
struct Registration {
    id: u64,
    rx: Receiver<PortEvent>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        interest().watches.retain(|watch| watch.id != self.id);
        WAKE.notify_all();
    }
}

fn register() -> Option<Registration> {
    let mut interest = interest();
    if interest.watches.len() >= MAX_WATCHES {
        return None;
    }
    let (tx, rx) = mpsc::sync_channel(WATCH_BACKLOG);
    let id = interest.next_id;
    interest.next_id += 1;
    interest.watches.push(Watch {
        id,
        tx,
        listed: false,
    });
    interest.pending = true;
    drop(interest);
    start_scanner();
    WAKE.notify_all();
    Some(Registration { id, rx })
}

/// Streams PORT_EVENT frames on `writer`: the ports listening now, then
/// each change, until the host goes away.
pub fn watch(mut writer: TcpStream) {
    // A host that stops reading would hold this thread and connection.
    let _ = writer.set_write_timeout(Some(WATCH_STALL));
    let Some(registration) = register() else {
        let msg = format!("too many port watches, the limit is {MAX_WATCHES}");
        let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
        return;
    };
    loop {
        match registration.rx.recv_timeout(TICK) {
            Ok(event) => {
                if frame::send_json(&mut writer, frame::PORT_EVENT, &event).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let msg = format!(
                    "port events were dropped, watch again to catch up (os error {})",
                    libc::ENOBUFS
                );
                let _ = frame::write_frame(&mut writer, frame::ERROR, msg.as_bytes());
                return;
            }
        }
        if crate::files::hung_up(&writer) {
            return;
        }
    }
}

/// A listening socket and the session its owner runs in.
#[derive(Clone)]
struct Listener {
    port: Port,
    session: Option<u64>,
}

fn scan_loop() {
    let mut known: Option<HashMap<(String, u16), Listener>> = None;
    let mut owners = Owners::default();
    let mut due = Instant::now();
    let mut interest = interest();
    loop {
        if !interest.any() {
            known = None;
            interest = WAKE.wait(interest).unwrap_or_else(|e| e.into_inner());
            continue;
        }
        let now = Instant::now();
        if known.is_some() && !interest.pending && now < due {
            interest = WAKE
                .wait_timeout(interest, due - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            continue;
        }
        interest.pending = false;
        due = now + TICK;
        drop(interest);

        let current = scan(&mut owners);
        // After a quiet spell every port is news to the sessions attached
        // since; every watch is new too, so it gets the list alone.
        let (opened, closed) = match &known {
            Some(known) => changes(known, &current),
            None => (current.values().cloned().collect(), Vec::new()),
        };
        for (session, event) in session_events(&opened, &closed) {
            crate::sessions::port_event(session, event);
        }

        interest = self::interest();
        let mut list = None;
        interest.watches.retain_mut(|watch| {
            if !watch.listed {
                watch.listed = true;
                let ports = list.get_or_insert_with(|| sorted(&current)).clone();
                return deliver(&watch.tx, PortEvent::PortList { ports });
            }
            opened
                .iter()
                .map(|l| PortEvent::PortOpened(l.port.clone()))
                .chain(closed.iter().map(|l| PortEvent::PortClosed(l.port.clone())))
                .all(|event| deliver(&watch.tx, event))
        });
        known = Some(current);
    }
}

/// False once the watch is gone or has fallen behind; either way it is
/// dropped, and a watch that fell behind tells its host.
fn deliver(tx: &SyncSender<PortEvent>, event: PortEvent) -> bool {
    match tx.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
    }
}

fn sorted(listeners: &HashMap<(String, u16), Listener>) -> Vec<Port> {
    let mut ports: Vec<Port> = listeners.values().map(|l| l.port.clone()).collect();
    ports.sort_by(|a, b| (a.port, &a.address).cmp(&(b.port, &b.address)));
    ports
}

fn changes(
    known: &HashMap<(String, u16), Listener>,
    current: &HashMap<(String, u16), Listener>,
) -> (Vec<Listener>, Vec<Listener>) {
    let opened = current
        .iter()
        .filter(|(key, _)| !known.contains_key(*key))
        .map(|(_, l)| l.clone())
        .collect();
    let closed = known
        .iter()
        .filter(|(key, _)| !current.contains_key(*key))
        .map(|(_, l)| l.clone())
        .collect();
    (opened, closed)
}

fn session_events(opened: &[Listener], closed: &[Listener]) -> Vec<(u64, PortEvent)> {
    let opened = opened
        .iter()
        .filter_map(|l| Some((l.session?, PortEvent::PortOpened(l.port.clone()))));
    let closed = closed
        .iter()
        .filter_map(|l| Some((l.session?, PortEvent::PortClosed(l.port.clone()))));
    opened.chain(closed).collect()
}

/// The process behind each socket inode, from the last walk of /proc.
#[derive(Default)]
struct Owners {
    by_inode: HashMap<u64, (u32, Option<u64>)>,
}

impl Owners {
    /// Walks /proc only when a socket is new since the last walk.
    fn refresh(&mut self, inodes: &[u64]) {
        if inodes.iter().all(|inode| self.by_inode.contains_key(inode)) {
            return;
        }
        let mut found = HashMap::new();
        let Ok(procs) = fs::read_dir("/proc") else {
            return;
        };
        for entry in procs.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            let mut session = None;
            for fd in fds.flatten() {
                let Some(inode) = fs::read_link(fd.path())
                    .ok()
                    .and_then(|target| socket_inode(target.to_str()?))
                else {
                    continue;
                };
                if !inodes.contains(&inode) || found.contains_key(&inode) {
                    continue;
                }
                let session = *session.get_or_insert_with(|| session_of(pid));
                found.insert(inode, (pid, session));
            }
        }
        // A socket whose owner is not found is not looked for again.
        for inode in inodes {
            found.entry(*inode).or_insert((0, None));
        }
        self.by_inode = found;
    }

    fn get(&self, inode: u64) -> (u32, Option<u64>) {
        self.by_inode.get(&inode).copied().unwrap_or((0, None))
    }
}

fn scan(owners: &mut Owners) -> HashMap<(String, u16), Listener> {
    let mut sockets = Vec::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        match fs::read_to_string(path) {
            Ok(table) => sockets.extend(listening(&table, v6)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("shuru-guest: ports: {path}: {e}"),
        }
    }
    let inodes: Vec<u64> = sockets.iter().map(|s| s.inode).collect();
    owners.refresh(&inodes);
    sockets
        .into_iter()
        .map(|s| {
            let (pid, session) = owners.get(s.inode);
            let port = Port {
                port: s.port,
                address: s.address,
                pid,
            };
            (
                (port.address.clone(), port.port),
                Listener { port, session },
            )
        })
        .collect()
}

#[derive(Debug, PartialEq)]
struct Socket {
    address: String,
    port: u16,
    inode: u64,
}

/// Listening sockets in a /proc/net/tcp or tcp6 table.
fn listening(table: &str, v6: bool) -> Vec<Socket> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(3) != Some(&LISTEN) {
                return None;
            }
            let (address, port) = fields.get(1)?.split_once(':')?;
            Some(Socket {
                address: parse_address(address, v6)?,
                port: u16::from_str_radix(port, 16).ok()?,
                inode: fields.get(9)?.parse().ok()?,
            })
        })
        .collect()
}

/// The kernel prints each 32 bit word of the address as a number in the
/// host byte order.
fn parse_address(hex: &str, v6: bool) -> Option<String> {
    let words = if v6 { 4 } else { 1 };
    if hex.len() != words * 8 || !hex.is_ascii() {
        return None;
    }
    let mut bytes = Vec::with_capacity(words * 4);
    for i in 0..words {
        let word = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    Some(if v6 {
        let octets: [u8; 16] = bytes.try_into().ok()?;
        Ipv6Addr::from(octets).to_string()
    } else {
        let octets: [u8; 4] = bytes.try_into().ok()?;
        Ipv4Addr::from(octets).to_string()
    })
}

fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn session_of(pid: u32) -> Option<u64> {
    session_in(&fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

/// The exec session of a cgroup v2 membership line, nested cgroups included.
fn session_in(cgroup: &str) -> Option<u64> {
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    let rest = path.strip_prefix(SESSION_CGROUP)?;
    rest.split('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 4243 1 0000000000000000 20 4 30 10 -1
   2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 17 1 0000000000000000 100 0 0 10 0
";

    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 100 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn listening_sockets_are_read_from_proc_tables() {
        assert_eq!(
            listening(TCP, false),
            [
                Socket {
                    address: "127.0.0.1".into(),
                    port: 8080,
                    inode: 4242
                },
                Socket {
                    address: "0.0.0.0".into(),
                    port: 22,
                    inode: 17
                },
            ]
        );
        assert_eq!(
            listening(TCP6, true),
            [
                Socket {
                    address: "::".into(),
                    port: 3000,
                    inode: 99
                },
                Socket {
                    address: "::1".into(),
                    port: 80,
                    inode: 100
                },
            ]
        );
    }

    #[test]
    fn sessions_are_read_from_cgroup_paths() {
        assert_eq!(session_in("0::/shuru/session-12\n"), Some(12));
        assert_eq!(session_in("0::/shuru/session-7/inner\n"), Some(7));
        assert_eq!(session_in("0::/shuru/exec-3\n"), None);
        assert_eq!(session_in("0::/\n"), None);
        assert_eq!(socket_inode("socket:[4242]"), Some(4242));
        assert_eq!(socket_inode("pipe:[4242]"), None);
    }

    #[test]
    fn changes_are_keyed_by_address_and_port() {
        let listener = |address: &str, port, pid, session| {
            let port = Port {
                port,
                address: address.into(),
                pid,
            };
            (
                (port.address.clone(), port.port),
                Listener { port, session },
            )
        };
        let known = HashMap::from([
            listener("0.0.0.0", 22, 1, None),
            listener("::", 80, 5, Some(3)),
        ]);
        let current = HashMap::from([
            listener("0.0.0.0", 22, 1, None),
            listener("::", 8080, 6, Some(3)),
        ]);
        let (opened, closed) = changes(&known, &current);
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].port.port, 8080);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].port.port, 80);
        let events = session_events(&opened, &closed);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], (3, PortEvent::PortOpened(p)) if p.pid == 6));
        assert!(matches!(&events[1], (3, PortEvent::PortClosed(p)) if p.pid == 5));
    }

    #[test]
    fn a_watch_sees_a_port_open_in_the_guest() {
        let rx = &register().unwrap().rx;
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(first, PortEvent::PortList { .. }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if let PortEvent::PortOpened(p) = event {
                if p.port == port {
                    assert_eq!(p.address, "127.0.0.1");
                    assert_eq!(p.pid, std::process::id());
                    break;
                }
            }
        }
    }
}
