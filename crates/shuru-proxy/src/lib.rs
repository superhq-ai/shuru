pub mod config;
mod device;
mod dns;
mod egress;
mod flows;
mod proxy;
mod secrets;
mod stack;
mod stream;
mod substitute;
mod tls;

pub use config::ProxyConfig;
pub use egress::{Egress, EgressLog};
pub use flows::{FlowAge, Flows};

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::os::unix::io::RawFd;
use std::sync::{Arc, RwLock};

use lru::LruCache;
use proxy::ProxyEngine;
use stack::NetworkStack;
use tls::CertificateAuthority;
use tokio::sync::mpsc;
use tracing::info;

/// Cache of IPs that the proxy is allowed to connect to.
/// Populated by DNS resolution of allowed domains. When the domain allowlist
/// is active, TCP connections to IPs not in this cache are rejected, closing
/// the bypass where a guest connects directly to a hardcoded IP.
///
/// Bounded via LRU so long-lived sandboxes do not accumulate stale IPs
/// indefinitely. Recency is bumped on lookup, so IPs in active use stay warm.
pub type AllowedIps = Arc<RwLock<LruCache<Ipv4Addr, ()>>>;

/// Cap on pinned IPs. A single CDN-fronted domain can resolve to dozens of
/// IPs, and configs can list many domains, so keep this generous.
const ALLOWED_IPS_CAPACITY: usize = 1024;

/// Handle to a running proxy. Shuts down on drop.
pub struct ProxyHandle {
    _stack_thread: std::thread::JoinHandle<()>,
    _runtime_thread: std::thread::JoinHandle<()>,
    /// Placeholder tokens generated for secrets. Key = env var name, Value = placeholder.
    pub placeholders: HashMap<String, String>,
    /// CA certificate in PEM format (for injecting into guest trust store).
    pub ca_cert_pem: Vec<u8>,
    ca_key_pem: String,
    /// The open connections of the guest and when each last carried bytes.
    pub flows: Arc<Flows>,
    /// The bytes the guest sent out.
    pub egress: Arc<Egress>,
}

/// What a guest keeps from the proxy that served it: the CA it trusts and
/// the placeholders in its environment. A restored guest gets a proxy
/// started from the session of the one it was saved with.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub placeholders: HashMap<String, String>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("secrets", &self.placeholders.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ProxyHandle {
    pub fn session(&self) -> Session {
        Session {
            ca_cert_pem: String::from_utf8_lossy(&self.ca_cert_pem).into_owned(),
            ca_key_pem: self.ca_key_pem.clone(),
            placeholders: self.placeholders.clone(),
        }
    }
}

/// Generate a unique placeholder token for a secret.
fn generate_placeholder() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("shuru_tok_{:016x}{:04x}", ts, seq)
}

/// Create a Unix datagram socketpair for VZFileHandleNetworkDeviceAttachment.
/// Returns (vm_fd, host_fd). The vm_fd goes to VZ, host_fd goes to the proxy.
pub fn create_socketpair() -> anyhow::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(anyhow::anyhow!(
            "socketpair failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let host_fd = fds[1];

    // Apple recommends SO_RCVBUF at least 2x SO_SNDBUF (4x preferred) for
    // VZFileHandleNetworkDeviceAttachment. With the 65535-byte IP-MTU set on
    // the attachment (see `shuru-vm::sandbox.rs`) each datagram can be one
    // 64 KiB frame, so the receive buffer decides how many frames can be in
    // flight before the guest is back-pressured. 6 MiB holds about 96 of
    // them and stays under the macOS kern.ipc.maxsockbuf default of 8 MiB,
    // which the kernel applies per buffer with some headroom removed; a
    // request at the limit fails and leaves the AF_UNIX default of 4 KiB,
    // too small to carry a single frame. The values are read back so a
    // capped socket is reported instead of silently crippling the network.
    let sndbuf = set_sockbuf(host_fd, libc::SO_SNDBUF, 2 * 1024 * 1024);
    let rcvbuf = set_sockbuf(host_fd, libc::SO_RCVBUF, 6 * 1024 * 1024);
    let frame = device::MTU as libc::c_int;
    if rcvbuf < frame || sndbuf < frame {
        return Err(anyhow::anyhow!(
            "host socket buffers too small for a {} byte frame (SO_SNDBUF {}, SO_RCVBUF {}); raise kern.ipc.maxsockbuf",
            frame,
            sndbuf,
            rcvbuf
        ));
    }
    if rcvbuf < 4 * frame {
        tracing::warn!(
            "SO_RCVBUF is {} bytes, under four frames; guest throughput will be limited",
            rcvbuf
        );
    }

    Ok((fds[0], fds[1]))
}

/// Request a socket buffer size and return what the kernel actually set,
/// which may be smaller or unchanged when the request exceeds its limit.
fn set_sockbuf(fd: RawFd, opt: libc::c_int, want: libc::c_int) -> libc::c_int {
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &want as *const _ as _,
            std::mem::size_of::<libc::c_int>() as _,
        );
        let mut got: libc::c_int = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as _;
        if libc::getsockopt(fd, libc::SOL_SOCKET, opt, &mut got as *mut _ as _, &mut len) != 0 {
            return 0;
        }
        got
    }
}

/// Start the proxy engine. Returns a handle that keeps it running.
///
/// - `host_fd`: the host end of the socketpair (raw L2 Ethernet frames)
/// - `config`: proxy configuration (secrets, network rules)
pub fn start(host_fd: RawFd, config: ProxyConfig) -> anyhow::Result<ProxyHandle> {
    start_session(host_fd, config, None)
}

/// `start`, carrying on `session` when given. Its placeholders must cover
/// the configured secrets exactly.
pub fn start_session(
    host_fd: RawFd,
    config: ProxyConfig,
    session: Option<Session>,
) -> anyhow::Result<ProxyHandle> {
    // Install rustls crypto provider (process-wide, idempotent)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (ca, placeholders) = match session {
        Some(session) => {
            let mut names: Vec<_> = session.placeholders.keys().collect();
            let mut secrets: Vec<_> = config.secrets.keys().collect();
            names.sort();
            secrets.sort();
            if names != secrets {
                anyhow::bail!("the secrets differ from the ones the session was started with");
            }
            let ca = CertificateAuthority::from_pem(&session.ca_cert_pem, &session.ca_key_pem)?;
            (ca, session.placeholders)
        }
        None => {
            let placeholders = config
                .secrets
                .keys()
                .map(|name| (name.clone(), generate_placeholder()))
                .collect();
            (CertificateAuthority::new()?, placeholders)
        }
    };
    let ca_cert_pem = ca.ca_cert_pem();
    let ca_key_pem = ca.ca_key_pem();

    let allowed_ips: AllowedIps = Arc::new(RwLock::new(LruCache::new(
        NonZeroUsize::new(ALLOWED_IPS_CAPACITY).expect("non-zero capacity"),
    )));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let flows = Arc::new(Flows::default());
    let stack_flows = flows.clone();
    let gate = egress::Gate::new(&config, std::time::Instant::now());
    let mut engine = ProxyEngine::new(
        config,
        event_rx,
        cmd_tx,
        ca,
        placeholders.clone(),
        allowed_ips,
    )?;
    let egress = engine.egress();

    let stack_thread = std::thread::Builder::new()
        .name("shuru-netstack".into())
        .spawn(move || {
            let mut stack = NetworkStack::new(host_fd, event_tx, cmd_rx, stack_flows, gate);
            stack.run();
        })?;

    let runtime_thread = std::thread::Builder::new()
        .name("shuru-proxy".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to create tokio runtime for proxy");

            rt.block_on(engine.run());
        })?;

    info!("proxy started");

    Ok(ProxyHandle {
        _stack_thread: stack_thread,
        _runtime_thread: runtime_thread,
        placeholders,
        ca_cert_pem,
        ca_key_pem,
        flows,
        egress,
    })
}
