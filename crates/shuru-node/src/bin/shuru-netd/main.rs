//! The network for one sandbox, run by `shuru-node` inside the jail with
//! the host network. Fd 3 is a listening control socket. The first
//! connection brings the egress policy, the secrets, the host end of the
//! VM frame socket and, for provided secrets, the socket to ask the node
//! for their values on.

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use shuru_node::control::{self, Answer, Ask, NetdConfig, Request, Response, Secret};
use shuru_node::seccomp;
use shuru_proxy::config::{ConnectionRate, NetworkConfig, Rate, SecretConfig};
use shuru_proxy::{EgressLog, Provided, ProxyConfig, Refused, SecretProvider};
use tokio::sync::oneshot;

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

/// 100 Mbit/s for all the connections of a sandbox, with a second of it
/// at once.
const EGRESS_RATE: Rate = Rate {
    per_second: 12_500_000,
    burst: 12_500_000,
};

/// How long the node has to answer an ask, provider and control plane
/// included.
const ASK_TIMEOUT: Duration = Duration::from_secs(30);

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
    let (request, fds) = control::receive::<Request>(&stream, 2)?.context("node left")?;
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
    if fds.len() != config.fd_count() {
        bail!(
            "expected {} descriptors, got {}",
            config.fd_count(),
            fds.len()
        );
    }
    let mut fds = fds.into_iter();
    let frames = fds.next().expect("counted");
    let provider = fds
        .next()
        .map(|asks| NodeAsks::start(UnixStream::from(asks)))
        .transpose()?;
    let secrets = proxy_secrets(config.secrets);
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
        egress_rate: Some(EGRESS_RATE),
        egress_log: Some(egress.clone()),
        upstream_roots: Some(config.upstream_roots.into_bytes()),
        provider: provider.map(|p| p as Arc<dyn SecretProvider>),
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
            Request::Egress => Response::Egress {
                bytes: proxy.egress.bytes(),
            },
            Request::UpdateSecrets { secrets } => Response::Updated {
                pending: proxy.update_secrets(proxy_secrets(secrets)),
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

fn proxy_secrets(secrets: HashMap<String, Secret>) -> HashMap<String, SecretConfig> {
    secrets
        .into_iter()
        .map(|(name, secret)| {
            let secret = SecretConfig {
                value: secret.value,
                hosts: secret.hosts,
                provided: secret.provided,
                scope: secret.scope,
                ..Default::default()
            };
            (name, secret)
        })
        .collect()
}

/// Asks the node for provided values on the asks socket. A thread reads
/// the answers, which come in any order, and hands each to its asker.
#[derive(Debug)]
struct NodeAsks {
    writer: Mutex<UnixStream>,
    waiting: Waiting,
    next: AtomicU64,
}

/// The askers waiting for an answer, by ask; None once the node is gone.
type Waiting = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Answer>>>>>;

impl NodeAsks {
    fn start(stream: UnixStream) -> Result<Arc<NodeAsks>> {
        let reader = stream.try_clone()?;
        let waiting: Waiting = Arc::new(Mutex::new(Some(HashMap::new())));
        let answers = waiting.clone();
        std::thread::Builder::new()
            .name("shuru-netd-asks".into())
            .spawn(move || {
                while let Ok(Some((answer, _))) = control::receive::<Answer>(&reader, 0) {
                    let waiter = answers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                        .and_then(|waiting| waiting.remove(&answer.ask));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(answer);
                    }
                }
                // Every ask still waiting, and every later one, fails.
                answers.lock().unwrap_or_else(|e| e.into_inner()).take();
            })?;
        Ok(Arc::new(NodeAsks {
            writer: Mutex::new(stream),
            waiting,
            next: AtomicU64::new(1),
        }))
    }

    async fn ask(&self, name: &str) -> Result<Provided> {
        let ask = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .ok_or_else(|| anyhow!("the node no longer answers"))?
            .insert(ask, tx);
        let sent = control::send(
            &self.writer.lock().unwrap_or_else(|e| e.into_inner()),
            &Ask {
                ask,
                secret: name.to_string(),
            },
            &[],
        );
        if let Err(e) = sent {
            self.forget(ask);
            bail!("asking the node: {e}");
        }
        let answer = match tokio::time::timeout(ASK_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                self.forget(ask);
                bail!("the node no longer answers");
            }
            Err(_) => {
                self.forget(ask);
                bail!("the node did not answer in {} s", ASK_TIMEOUT.as_secs());
            }
        };
        match (answer.value, answer.error) {
            (Some(value), None) => Ok(Provided {
                value,
                expires_in: answer.expires_in_ms.map(Duration::from_millis),
            }),
            (_, Some(error)) if answer.refused => Err(Refused(error).into()),
            (_, Some(error)) => Err(anyhow!("{error}")),
            (None, None) => bail!("the node answered neither a value nor an error"),
        }
    }

    fn forget(&self, ask: u64) {
        if let Some(waiting) = self
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            waiting.remove(&ask);
        }
    }
}

impl SecretProvider for NodeAsks {
    fn provide<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Provided>> + Send + 'a>> {
        Box::pin(self.ask(name))
    }
}
