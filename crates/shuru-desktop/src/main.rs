//! `shuru-desktop` is the window manager of a desktop an agent uses: it
//! frames and stacks windows, draws a panel, a rail, menus and overlays for
//! the person watching, and answers an API on a unix socket. See RFC 0014.

#[cfg(target_os = "linux")]
mod appearance;
#[cfg(target_os = "linux")]
mod apps;
#[cfg(target_os = "linux")]
mod atspi;
#[cfg(target_os = "linux")]
mod bus;
#[cfg(target_os = "linux")]
mod chrome;
#[cfg(target_os = "linux")]
mod defaults;
#[cfg(target_os = "linux")]
mod dnd;
#[cfg(target_os = "linux")]
mod draw;
#[cfg(target_os = "linux")]
mod glyphs;
#[cfg(target_os = "linux")]
mod icon;
#[cfg(target_os = "linux")]
mod locale;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod settings;
#[cfg(target_os = "linux")]
mod ui;
#[cfg(target_os = "linux")]
mod wm;

#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = linux::run() {
        eprintln!("shuru-desktop: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("shuru-desktop: runs on Linux only");
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

    use crate::server::State;

    #[derive(Parser)]
    #[command(version, about = "A window manager for a desktop an agent uses")]
    struct Args {
        /// The X display to manage, such as :1.
        #[arg(long, env = "DISPLAY")]
        display: String,
        /// The unix socket to serve on, made with mode 0600.
        #[arg(long, required_unless_present = "settings")]
        socket: Option<PathBuf>,
        /// Opens the Settings client instead of managing the display.
        #[arg(long)]
        settings: bool,
        /// Apps for the rail: paths to .desktop files or their ids, such
        /// as org.gnome.Calculator.
        #[arg(long, value_delimiter = ',')]
        apps: Vec<String>,
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        if args.settings {
            return crate::settings::run(&args.display);
        }
        let socket = args.socket.context("--socket is required")?;
        crate::locale::seed();
        let mut apps = Vec::new();
        for spec in &args.apps {
            match crate::apps::load(spec) {
                Ok(a) => apps.push(a),
                Err(e) => eprintln!("shuru-desktop: app {spec}: {e:#}"),
            }
        }
        let (bus_tx, bus_rx) = tokio::sync::mpsc::unbounded_channel();
        let (wm, settle) = crate::wm::start(&args.display, apps, bus_tx)
            .with_context(|| format!("managing {}", args.display))?;
        let listener = bind(&socket)?;
        eprintln!("shuru-desktop: serving on {}", socket.display());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.spawn(crate::bus::run(wm.clone(), bus_rx));
        let state = State { wm, settle };
        let served = runtime.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(listener)?;
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            loop {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            tokio::spawn(crate::server::connection(state.clone(), stream));
                        }
                        Err(e) => {
                            eprintln!("shuru-desktop: accepting a connection: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    },
                    _ = terminate.recv() => break,
                    _ = interrupt.recv() => break,
                }
            }
            anyhow::Ok(())
        });
        let _ = fs::remove_file(&socket);
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
