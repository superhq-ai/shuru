//! One sandbox VM, run by `shuru-node` inside the jail. Fd 3 is a listening
//! control socket. The first connection brings the configuration and every
//! descriptor the VM needs; after that the VMM opens nothing.

use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};

use anyhow::{bail, Context, Result};
use shuru_linux::{VirtualMachine, VmDescriptors, VmImage, VmState};
use shuru_node::control::{self, Boot, Request, Response, VmmConfig};
use shuru_node::seccomp;

const CONTROL_FD: i32 = 3;

fn main() {
    if let Err(e) = run() {
        eprintln!("shuru-vmm: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    let log = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--seccomp-log") => true,
        Some(other) => bail!("unknown argument {other}"),
    };
    let listener = unsafe { UnixListener::from_raw_fd(CONTROL_FD) };
    let (stream, _) = listener.accept().context("accepting the node")?;
    let (request, fds) =
        control::receive::<Request>(&stream, control::MAX_FDS)?.context("node left")?;
    let Request::Configure(config) = request else {
        bail!("the first request must be the configuration");
    };
    let vm = match start(config, fds, log) {
        Ok(vm) => vm,
        Err(e) => {
            let message = format!("{e:#}");
            let _ = control::send(&stream, &Response::Error { message }, &[]);
            return Err(e);
        }
    };
    control::send(&stream, &Response::Ok, &[])?;

    // A node that restarts connects again.
    let mut stream = stream;
    loop {
        serve(&vm, &stream);
        stream = listener.accept().context("accepting the node")?.0;
    }
}

fn start(config: VmmConfig, fds: Vec<OwnedFd>, log: bool) -> Result<VirtualMachine> {
    if fds.len() != config.fd_count() {
        bail!(
            "expected {} descriptors, got {}",
            config.fd_count(),
            fds.len()
        );
    }
    let mut fds = fds.into_iter();
    let mut next = || fds.next().unwrap();
    let kvm = next();
    let kernel = match config.boot {
        Boot::Kernel {
            command_line,
            initrd,
        } => Some((
            File::from(next()),
            initrd.then(|| File::from(next())),
            command_line,
        )),
        Boot::Snapshot => None,
    };
    let disk = config.disk.as_ref().map(|_| File::from(next()));
    let serial_write = config.serial.then(&mut next);
    let network = config.network_mac.map(|_| next());
    let vsock = UnixListener::from(next());
    let image = match kernel {
        Some((kernel, initrd, command_line)) => VmImage::Kernel {
            kernel,
            initrd,
            command_line,
        },
        None => VmImage::Snapshot {
            memory: File::from(next()),
            state: File::from(next()),
        },
    };

    let vm = VirtualMachine::from_descriptors(VmDescriptors {
        kvm,
        image,
        disk,
        disk_read_only: config.disk.as_ref().is_some_and(|disk| disk.read_only),
        serial_write,
        serial_read: None,
        network,
        network_mac: config.network_mac,
        cpu_count: config.cpus,
        memory_bytes: config.memory_bytes,
        vsock: Some(config.vsock_connections),
        balloon: config.balloon,
    })?;
    vm.serve_vsock(vsock)?;
    seccomp::install_vmm(log)?;

    // A guest power off ends the process whatever the node is doing.
    let states = vm.state_channel();
    std::thread::Builder::new()
        .name("shuru-vmm-state".into())
        .spawn(move || {
            while let Ok(state) = states.recv() {
                if matches!(state, VmState::Stopped | VmState::Error) {
                    std::process::exit(0);
                }
            }
        })?;
    vm.start()?;
    Ok(vm)
}

fn serve(vm: &VirtualMachine, stream: &UnixStream) {
    while let Ok(Some((request, fds))) = control::receive::<Request>(stream, control::SNAPSHOT_FDS)
    {
        let response = match request {
            Request::Configure(_) | Request::ConfigureNetwork(_) => Response::Error {
                message: "already configured".into(),
            },
            Request::Flows | Request::Egress => Response::Error {
                message: "not a request for the VMM".into(),
            },
            // The node shuts the guest down first when it wants a clean
            // stop, so this is the power cord.
            Request::Stop => {
                let _ = control::send(stream, &Response::Ok, &[]);
                std::process::exit(0);
            }
            Request::SetMemoryTarget { bytes } => match vm.set_memory_target(bytes) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
            Request::Stats => Response::Stats {
                running: vm.state() == VmState::Running,
                ballooned_bytes: vm.ballooned_bytes(),
            },
            Request::Snapshot { resume } => {
                let result = match <[_; control::SNAPSHOT_FDS]>::try_from(fds) {
                    Ok(fds) => {
                        let [memory, state, pagemap] = fds.map(File::from);
                        vm.snapshot(&memory, &state, &pagemap, resume)
                            .map_err(|e| e.to_string())
                    }
                    Err(_) => {
                        Err("a snapshot needs a memory file, a state file and a pagemap".into())
                    }
                };
                match result {
                    Ok(()) => Response::Ok,
                    Err(message) => Response::Error { message },
                }
            }
        };
        if control::send(stream, &response, &[]).is_err() {
            return;
        }
    }
}
