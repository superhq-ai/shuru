//! `shuru-screen` streams an X11 display as H.264 over a WebSocket and
//! takes input for it by XTEST, behind a gate. It serves HTTP on a unix
//! socket only; whatever exposes it further brings its own authentication.

#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod convert;
#[cfg(target_os = "linux")]
mod encoder;
#[cfg(target_os = "linux")]
mod hub;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod still;
#[cfg(target_os = "linux")]
mod video;

#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = linux::run() {
        eprintln!("shuru-screen: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("shuru-screen: runs on Linux only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use anyhow::{bail, Context, Result};
    use clap::Parser;
    use tokio::signal::unix::{signal, SignalKind};

    use crate::input::InputEvent;
    use crate::video::VideoConfig;

    #[derive(Parser)]
    #[command(version, about = "Streams an X11 display over a unix socket")]
    struct Args {
        /// The X display to stream, such as :1.
        #[arg(long, env = "DISPLAY")]
        display: String,
        /// The unix socket to serve on, made with mode 0600.
        #[arg(long)]
        socket: PathBuf,
        /// Frames a second at most.
        #[arg(long, default_value_t = 30)]
        framerate: u32,
        /// Highest bitrate, in kbit/s.
        #[arg(long, default_value_t = 6000)]
        max_kbps: u32,
        /// Lowest bitrate the stream backs off to, in kbit/s.
        #[arg(long, default_value_t = 500)]
        min_kbps: u32,
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        if !(1..=120).contains(&args.framerate) {
            bail!("--framerate is from 1 to 120");
        }
        if args.min_kbps == 0 || args.min_kbps > args.max_kbps {
            bail!("--min-kbps is above 0 and at most --max-kbps");
        }
        let ncpu = std::thread::available_parallelism().map_or(4, |n| n.get());
        let cfg = VideoConfig {
            fps: args.framerate,
            max_kbps: args.max_kbps,
            min_kbps: args.min_kbps,
            // x264 slices cost more CPU per frame the more there are; one
            // thread up to 7 CPUs.
            threads: (ncpu as u32 / 4).clamp(1, 4),
            convert_threads: ncpu.clamp(1, 4),
            crf: 24.0,
            paintover_crf: 16.0,
            paintover_burst: 3,
            paintover_delay: Duration::from_millis(150),
        };
        let input = crate::input::spawn(&args.display)
            .with_context(|| format!("input on {}", args.display))?;
        let hub = crate::video::start(&args.display, cfg, input.clone())?;
        let listener = bind(&args.socket)?;
        eprintln!("shuru-screen: serving on {}", args.socket.display());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let served = runtime.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener)?;
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            loop {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            tokio::spawn(crate::server::connection(hub.clone(), stream));
                        }
                        Err(e) => {
                            eprintln!("shuru-screen: accepting a connection: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    },
                    _ = terminate.recv() => break,
                    _ = interrupt.recv() => break,
                }
            }
            anyhow::Ok(())
        });
        let _ = fs::remove_file(&args.socket);
        // Let go of held keys and undo keymap changes before the X
        // connection closes.
        let _ = input.send(InputEvent::Shutdown);
        std::thread::sleep(Duration::from_millis(100));
        served
    }

    /// Binds the socket with no access for anyone but its owner. A socket
    /// file nobody listens on any more is replaced.
    fn bind(path: &Path) -> Result<UnixListener> {
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
}
