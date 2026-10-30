//! `shuru-node serve` keeps sandboxes on this host and serves the sandbox
//! API for them, HTTP and WebSocket on a unix socket (RFC 0012). Only root
//! can reach the socket; whatever exposes the API further brings its own
//! authentication.

mod exec;
mod guest;
mod http;
mod hub;
mod sandboxes;

use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::signal::unix::{signal, SignalKind};

use crate::vm::Host;
use sandboxes::Node;

#[derive(clap::Args)]
pub struct ServeArgs {
    /// The unix socket the API listens on.
    #[arg(long, default_value = "/run/shuru/serve.sock")]
    socket: PathBuf,
    /// Where sandboxes are kept, each with its settings and working disk.
    #[arg(long, default_value = "/var/lib/shuru")]
    state_dir: PathBuf,
    #[arg(long)]
    kernel: PathBuf,
    #[arg(long)]
    initrd: PathBuf,
    /// The disk each new sandbox starts from, as a copy of its own. Keep it
    /// out of use while sandboxes are created.
    #[arg(long)]
    disk: PathBuf,
    /// CPUs of each new sandbox.
    #[arg(long, default_value_t = 2)]
    cpus: usize,
    /// Guest memory of each new sandbox, in MiB.
    #[arg(long, default_value_t = 2048)]
    memory: u64,
    /// Give each sandbox a network through a jailed shuru-netd. It reaches
    /// public addresses only.
    #[arg(long)]
    allow_net: bool,
    #[arg(long, default_value = "8.8.8.8")]
    dns: Ipv4Addr,
    /// Roots for checking the hosts secrets are sent to.
    #[arg(long, default_value = "/etc/ssl/certs/ca-certificates.crt")]
    ca_bundle: PathBuf,
    /// The first slot sandboxes run in, as `run --slot` picks one.
    #[arg(long, default_value_t = 1000)]
    first_slot: u32,
    /// How many sandboxes may run at once, one per slot.
    #[arg(long, default_value_t = 64)]
    max_running: u32,
    /// The directory holding the statically linked shuru-vmm and
    /// shuru-netd. Defaults to the one holding this binary.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
    /// Show every guest console on stderr, guest tokens included.
    #[arg(long)]
    verbose: bool,
    /// Log calls the seccomp filters forbid instead of killing.
    #[arg(long)]
    seccomp_log: bool,
}

pub fn serve(args: ServeArgs) -> Result<()> {
    let host = Host::open(args.bin_dir.as_deref(), args.verbose, args.seccomp_log)?;
    let node = Arc::new(Node::open(&args, host)?);
    let listener = bind(&args.socket)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let served = runtime.block_on(accept(node, listener));
    let _ = fs::remove_file(&args.socket);
    served
}

/// Binds the socket with no access for anyone but root. A socket file
/// nobody listens on any more is replaced.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(parent)
            .with_context(|| format!("{}", parent.display()))?;
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            bail!("{} exists and is not a socket", path.display());
        }
        if UnixStream::connect(path).is_ok() {
            bail!("something already listens on {}", path.display());
        }
        fs::remove_file(path)?;
    }
    let old = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(path);
    unsafe { libc::umask(old) };
    let listener = listener.with_context(|| format!("{}", path.display()))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Serves until SIGTERM or SIGINT, then shuts every running guest down.
async fn accept(node: Arc<Node>, listener: UnixListener) -> Result<()> {
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(http::connection(node.clone(), stream));
                }
                Err(e) => {
                    eprintln!("shuru-node: accepting a connection: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    drop(listener);
    node.shut_down().await;
    Ok(())
}
