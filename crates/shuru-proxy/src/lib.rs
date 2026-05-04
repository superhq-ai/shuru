pub mod config;
mod device;
mod dns;
mod proxy;
mod stack;
mod stream;
mod tls;

pub use config::ProxyConfig;

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

    // Apple recommends SO_RCVBUF >= 2x SO_SNDBUF for VZFileHandleNetworkDeviceAttachment
    // (recommended 4×). With the 65535-byte jumbo IP-MTU set on the attachment
    // (see `shuru-vm::sandbox.rs`), each datagram can be ~64 KiB, so 8 MiB
    // rcvbuf holds ~128 in-flight super-frames — enough headroom for the BDP
    // window before back-pressure trips. macOS `kern.ipc.maxsockbuf` may cap
    // these silently; setsockopt errors are ignored as best-effort tuning.
    unsafe {
        let sndbuf: libc::c_int = 2 * 1024 * 1024;
        let rcvbuf: libc::c_int = 8 * 1024 * 1024;
        libc::setsockopt(
            host_fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as _,
            std::mem::size_of::<libc::c_int>() as _,
        );
        libc::setsockopt(
            host_fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as _,
            std::mem::size_of::<libc::c_int>() as _,
        );
    }

    Ok((fds[0], fds[1]))
}

/// Start the proxy engine. Returns a handle that keeps it running.
///
/// - `host_fd`: the host end of the socketpair (raw L2 Ethernet frames)
/// - `config`: proxy configuration (secrets, network rules)
pub fn start(host_fd: RawFd, config: ProxyConfig) -> anyhow::Result<ProxyHandle> {
    // Install rustls crypto provider (process-wide, idempotent)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let ca = CertificateAuthority::new()?;
    let ca_cert_pem = ca.ca_cert_pem();

    // Generate placeholder tokens for each secret
    let mut placeholders = HashMap::new();
    for name in config.secrets.keys() {
        placeholders.insert(name.clone(), generate_placeholder());
    }

    let allowed_ips: AllowedIps = Arc::new(RwLock::new(LruCache::new(
        NonZeroUsize::new(ALLOWED_IPS_CAPACITY).expect("non-zero capacity"),
    )));

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let stack_thread = std::thread::Builder::new()
        .name("shuru-netstack".into())
        .spawn(move || {
            let mut stack = NetworkStack::new(host_fd, event_tx, cmd_rx);
            stack.run();
        })?;

    let proxy_config = config;
    let proxy_placeholders = placeholders.clone();
    let proxy_allowed_ips = allowed_ips.clone();
    let runtime_thread = std::thread::Builder::new()
        .name("shuru-proxy".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to create tokio runtime for proxy");

            rt.block_on(async move {
                let mut engine = ProxyEngine::new(
                    proxy_config,
                    event_rx,
                    cmd_tx,
                    ca,
                    proxy_placeholders,
                    proxy_allowed_ips,
                );
                engine.run().await;
            });
        })?;

    info!("proxy started");

    Ok(ProxyHandle {
        _stack_thread: stack_thread,
        _runtime_thread: runtime_thread,
        placeholders,
        ca_cert_pem,
    })
}
