//! The network for one sandbox, run by `shuru-node` inside the jail with
//! the host network. Fd 3 is a listening control socket. The first
//! connection brings the egress policy, the secrets and the host end of
//! the VM frame socket.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};

use anyhow::{bail, Context, Result};
use shuru_node::control::{self, NetdConfig, Request, Response};
use shuru_node::seccomp;
use shuru_proxy::config::{ConnectionRate, NetworkConfig, Rate, SecretConfig};
use shuru_proxy::{EgressLog, ProxyConfig};

const CONTROL_FD: i32 = 3;

/// Mail submission and relay. A guest on a shared host sends mail through
/// an HTTP API, not from the address of the host.
const SMTP: [u16; 4] = [25, 465, 587, 2525];

/// Room for an install or a page load with many hosts, too little for a
/// scan or a flood to get far from the address of the host.
const CONNECTION_RATE: ConnectionRate = ConnectionRate {
    connections: Rate {
        per_second: 50,
        burst: 200,
    },
    destinations: Rate {
        per_second: 5,
        burst: 100,
    },
};

fn main() {
    if let Err(e) = run() {
        eprintln!("shuru-netd: {e:#}");
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
    let (request, fds) = control::receive::<Request>(&stream, 1)?.context("node left")?;
    let Request::ConfigureNetwork(config) = request else {
        bail!("the first request must be the network configuration");
    };
    let (proxy, egress, response) = match start(config, fds, log) {
        Ok(started) => started,
        Err(e) => {
            let message = format!("{e:#}");
            let _ = control::send(&stream, &Response::Error { message }, &[]);
            return Err(e);
        }
    };
    control::send(&stream, &response, &[])?;

    let mut stream = stream;
    loop {
        serve(&stream, &proxy, &egress);
        stream = listener.accept().context("accepting the node")?.0;
    }
}

fn start(
    config: NetdConfig,
    fds: Vec<OwnedFd>,
    log: bool,
) -> Result<(shuru_proxy::ProxyHandle, EgressLog, Response)> {
    let [frames]: [OwnedFd; 1] = fds
        .try_into()
        .map_err(|fds: Vec<_>| anyhow::anyhow!("expected 1 descriptor, got {}", fds.len()))?;
    let secrets: HashMap<String, SecretConfig> = config
        .secrets
        .into_iter()
        .map(|(name, secret)| {
            let secret = SecretConfig {
                value: Some(secret.value),
                hosts: secret.hosts,
                ..Default::default()
            };
            (name, secret)
        })
        .collect();
    let egress = EgressLog::stderr(&config.sandbox);
    let proxy = ProxyConfig {
        dns_resolver: config.dns_resolver,
        secrets,
        network: NetworkConfig {
            allow: config.allow,
        },
        public_only: true,
        deny_ports: SMTP.to_vec(),
        connection_rate: Some(CONNECTION_RATE),
        egress_log: Some(egress.clone()),
        upstream_roots: Some(config.upstream_roots.into_bytes()),
        ..Default::default()
    };
    seccomp::install_netd(log)?;
    let handle = shuru_proxy::start_session(frames.into_raw_fd(), proxy, config.session)?;
    let response = Response::Network {
        session: handle.session(),
    };
    Ok((handle, egress, response))
}

fn serve(stream: &UnixStream, proxy: &shuru_proxy::ProxyHandle, egress: &EgressLog) {
    while let Ok(Some((request, _))) = control::receive::<Request>(stream, 0) {
        let response = match request {
            Request::Stop => {
                egress.close();
                let _ = control::send(stream, &Response::Ok, &[]);
                std::process::exit(0);
            }
            Request::Flows => Response::Flows {
                flows: proxy.flows.ages(),
            },
            _ => Response::Error {
                message: "not a request for the network process".into(),
            },
        };
        if control::send(stream, &response, &[]).is_err() {
            return;
        }
    }
}
