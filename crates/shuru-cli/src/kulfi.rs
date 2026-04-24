use std::collections::HashSet;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;

use anyhow::{bail, Context, Result};
use hyper::body::Incoming;
use hyper::service::service_fn;
use iroh::Endpoint;
use kulfi_id52::SecretKey;
use shuru_proto::PortMapping;
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum KulfiProtocol {
    Http,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KulfiExpose {
    pub host_port: u16,
    pub protocol: KulfiProtocol,
    pub bridge_port: Option<u16>,
}

pub(crate) struct KulfiHandle {
    stop_tx: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for KulfiHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn parse_expose(s: &str) -> Result<KulfiExpose> {
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        bail!("expected HOST_PORT:http|tcp[:BRIDGE_PORT] format");
    }

    let host_port = parts[0]
        .parse()
        .with_context(|| format!("invalid host port: '{}'", parts[0]))?;
    let protocol = match parts[1] {
        "http" => KulfiProtocol::Http,
        "tcp" => KulfiProtocol::Tcp,
        other => bail!("invalid protocol '{}': expected 'http' or 'tcp'", other),
    };
    let bridge_port = parts
        .get(2)
        .map(|part| {
            part.parse()
                .with_context(|| format!("invalid bridge port: '{}'", part))
        })
        .transpose()?;

    Ok(KulfiExpose {
        host_port,
        protocol,
        bridge_port,
    })
}

pub(crate) fn validate_exposes(exposes: &[KulfiExpose], forwards: &[PortMapping]) -> Result<()> {
    let forwarded_ports: HashSet<u16> = forwards.iter().map(|mapping| mapping.host_port).collect();
    let mut seen = HashSet::new();
    let mut explicit_bridge_ports = HashSet::new();

    for expose in exposes {
        if !forwarded_ports.contains(&expose.host_port) {
            bail!(
                "--kulfi {}:{} requires a matching -p/--port forward for host port {}",
                expose.host_port,
                protocol_name(expose.protocol),
                expose.host_port
            );
        }

        if !seen.insert((expose.host_port, expose.protocol)) {
            bail!(
                "duplicate --kulfi mapping for host port {} ({})",
                expose.host_port,
                protocol_name(expose.protocol)
            );
        }

        if let Some(bridge_port) = expose.bridge_port {
            if forwarded_ports.contains(&bridge_port) {
                bail!(
                    "kulfi bridge port {} conflicts with an existing -p/--port host listener",
                    bridge_port
                );
            }

            if !explicit_bridge_ports.insert(bridge_port) {
                bail!("duplicate explicit kulfi bridge port {}", bridge_port);
            }
        }
    }

    Ok(())
}

pub(crate) fn start(exposes: &[KulfiExpose], bridge_domain: &str) -> Result<Option<KulfiHandle>> {
    if exposes.is_empty() {
        return Ok(None);
    }

    let exposes = exposes.to_vec();
    let bridge_domain = bridge_domain.to_string();
    let (stop_tx, stop_rx) = oneshot::channel();

    let thread = thread::Builder::new()
        .name("shuru-kulfi".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("shuru: failed to start Kulfi runtime: {err}");
                    return;
                }
            };

            rt.block_on(async move {
                let secret_key = match load_or_create_secret_key() {
                    Ok(secret_key) => secret_key,
                    Err(err) => {
                        eprintln!("shuru: failed to load Kulfi identity: {err:#}");
                        let _ = stop_rx.await;
                        return;
                    }
                };
                let id52 = secret_key.id52();

                eprintln!("shuru: kulfi identity {}", id52);

                for expose in exposes {
                    let secret_bytes = secret_key.to_bytes();
                    let result = match expose.protocol {
                        KulfiProtocol::Http => {
                            start_http_tunnel(
                                secret_bytes,
                                expose.host_port,
                                expose.bridge_port,
                                &id52,
                                &bridge_domain,
                            )
                            .await
                        }
                        KulfiProtocol::Tcp => {
                            start_tcp_tunnel(secret_bytes, expose.host_port, expose.bridge_port, &id52)
                                .await
                        }
                    };

                    if let Err(err) = result {
                        eprintln!(
                            "shuru: kulfi {} tunnel for host port {} unavailable: {err:#}",
                            protocol_name(expose.protocol),
                            expose.host_port
                        );
                    }
                }

                let _ = stop_rx.await;
            });
        })
        .context("failed to spawn Kulfi integration thread")?;

    Ok(Some(KulfiHandle {
        stop_tx: Some(stop_tx),
        thread: Some(thread),
    }))
}

fn allocate_bridge_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("failed to allocate bridge port")
        .local_addr()
        .expect("allocated bridge listener missing local address")
        .port()
}

fn protocol_name(protocol: KulfiProtocol) -> &'static str {
    match protocol {
        KulfiProtocol::Http => "http",
        KulfiProtocol::Tcp => "tcp",
    }
}

async fn start_http_tunnel(
    secret_bytes: [u8; 32],
    host_port: u16,
    bridge_port: Option<u16>,
    id52: &str,
    bridge_domain: &str,
) -> Result<()> {
    let expose_endpoint = kulfi_utils::get_endpoint(SecretKey::from_bytes(&secret_bytes))
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind Kulfi HTTP endpoint: {err:?}"))?;
    let bridge_endpoint = kulfi_utils::get_endpoint(SecretKey::from_bytes(&secret_bytes))
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind Kulfi HTTP bridge endpoint: {err:?}"))?;

    let listener = TokioTcpListener::bind(format!("127.0.0.1:{}", bridge_port.unwrap_or(0)))
        .await
        .with_context(|| {
            format!(
                "failed to bind Kulfi HTTP bridge port {}",
                bridge_port.unwrap_or(0)
            )
        })?;
    let bound_port = listener.local_addr()?.port();

    eprintln!(
        "shuru: kulfi http bridge for 127.0.0.1:{} at http://127.0.0.1:{}",
        host_port, bound_port
    );
    eprintln!(
        "shuru: kulfi public URL (bridge-dependent) https://{}.{}",
        id52, bridge_domain
    );

    let expose_host = "127.0.0.1".to_string();
    tokio::spawn(async move {
        if let Err(err) = run_http_expose(expose_endpoint, expose_host, host_port).await {
            eprintln!("shuru: kulfi http expose stopped: {err:#}");
        }
    });

    let proxy_target = id52.to_string();
    tokio::spawn(async move {
        if let Err(err) = run_http_bridge(listener, bridge_endpoint, Some(proxy_target.clone())).await
        {
            eprintln!("shuru: kulfi http bridge stopped for {}: {err:#}", proxy_target);
        }
    });

    Ok(())
}

async fn start_tcp_tunnel(
    secret_bytes: [u8; 32],
    host_port: u16,
    bridge_port: Option<u16>,
    id52: &str,
) -> Result<()> {
    let expose_endpoint = kulfi_utils::get_endpoint(SecretKey::from_bytes(&secret_bytes))
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind Kulfi TCP endpoint: {err:?}"))?;
    let bridge_endpoint = kulfi_utils::get_endpoint(SecretKey::from_bytes(&secret_bytes))
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind Kulfi TCP bridge endpoint: {err:?}"))?;

    let listener = TokioTcpListener::bind(format!(
        "127.0.0.1:{}",
        bridge_port.unwrap_or_else(allocate_bridge_port)
    ))
    .await
    .with_context(|| {
        format!(
            "failed to bind Kulfi TCP bridge port {}",
            bridge_port.unwrap_or(0)
        )
    })?;
    let bound_port = listener.local_addr()?.port();

    eprintln!(
        "shuru: kulfi tcp bridge for 127.0.0.1:{} at 127.0.0.1:{}",
        host_port, bound_port
    );
    eprintln!("shuru: connect locally with: nc 127.0.0.1 {}", bound_port);

    let expose_host = "127.0.0.1".to_string();
    tokio::spawn(async move {
        if let Err(err) = run_tcp_expose(expose_endpoint, expose_host, host_port).await {
            eprintln!("shuru: kulfi tcp expose stopped: {err:#}");
        }
    });

    let proxy_target = id52.to_string();
    tokio::spawn(async move {
        if let Err(err) = run_tcp_bridge(listener, bridge_endpoint, proxy_target.clone()).await {
            eprintln!("shuru: kulfi tcp bridge stopped for {}: {err:#}", proxy_target);
        }
    });

    Ok(())
}

async fn run_http_expose(endpoint: Endpoint, host: String, port: u16) -> eyre::Result<()> {
    let client_pools = kulfi_utils::HttpConnectionPools::default();

    loop {
        let Some(conn) = endpoint.accept().await else {
            break;
        };

        let client_pools = client_pools.clone();
        let host = host.clone();
        tokio::spawn(async move {
            let conn = match conn.await {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::error!("failed to accept Kulfi HTTP connection: {err:?}");
                    return;
                }
            };

            if let Err(err) = handle_http_expose_connection(conn, client_pools, host, port).await {
                tracing::error!("Kulfi HTTP expose connection failed: {err:?}");
            }
        });
    }

    endpoint.close().await;
    Ok(())
}

async fn handle_http_expose_connection(
    conn: iroh::endpoint::Connection,
    client_pools: kulfi_utils::HttpConnectionPools,
    host: String,
    port: u16,
) -> eyre::Result<()> {
    loop {
        let (mut send, recv) = kulfi_utils::accept_bi(&conn, kulfi_utils::Protocol::Http).await?;
        if let Err(err) =
            kulfi_utils::peer_to_http(&format!("{host}:{port}"), client_pools.clone(), &mut send, recv).await
        {
            tracing::error!("failed to proxy Kulfi HTTP stream: {err:?}");
        }
        send.finish()?;
    }
}

async fn run_tcp_expose(endpoint: Endpoint, host: String, port: u16) -> eyre::Result<()> {
    loop {
        let Some(conn) = endpoint.accept().await else {
            break;
        };

        let host = host.clone();
        tokio::spawn(async move {
            let conn = match conn.await {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::error!("failed to accept Kulfi TCP connection: {err:?}");
                    return;
                }
            };

            if let Err(err) = handle_tcp_expose_connection(conn, host, port).await {
                tracing::error!("Kulfi TCP expose connection failed: {err:?}");
            }
        });
    }

    endpoint.close().await;
    Ok(())
}

async fn handle_tcp_expose_connection(
    conn: iroh::endpoint::Connection,
    host: String,
    port: u16,
) -> eyre::Result<()> {
    loop {
        let (send, recv) = kulfi_utils::accept_bi(&conn, kulfi_utils::Protocol::Tcp).await?;
        let addr = format!("{host}:{port}");
        tokio::spawn(async move {
            if let Err(err) = kulfi_utils::peer_to_tcp(&addr, send, recv).await {
                tracing::error!("failed to proxy Kulfi TCP stream: {err:?}");
            }
        });
    }
}

async fn run_http_bridge(
    listener: TokioTcpListener,
    endpoint: Endpoint,
    proxy_target: Option<String>,
) -> eyre::Result<()> {
    println!("Listening on http://127.0.0.1:{}", listener.local_addr()?.port());
    let peer_connections = kulfi_utils::PeerStreamSenders::default();

    loop {
        let (stream, _) = listener.accept().await?;
        let endpoint = endpoint.clone();
        let peer_connections = peer_connections.clone();
        let proxy_target = proxy_target.clone();
        tokio::spawn(async move {
            handle_http_bridge_connection(endpoint, stream, peer_connections, proxy_target).await;
        });
    }
}

async fn handle_http_bridge_connection(
    endpoint: Endpoint,
    stream: TokioTcpStream,
    peer_connections: kulfi_utils::PeerStreamSenders,
    proxy_target: Option<String>,
) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let graceful = kulfi_utils::Graceful::default();
    let builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    tokio::pin! {
        let conn = builder.serve_connection(
            io,
            service_fn(|request| {
                handle_http_bridge_request(
                    request,
                    endpoint.clone(),
                    peer_connections.clone(),
                    proxy_target.clone(),
                    graceful.clone(),
                )
            }),
        );
    }

    if let Err(err) = conn.await {
        tracing::error!("Kulfi HTTP bridge connection failed: {err:?}");
    }
}

async fn handle_http_bridge_request(
    request: hyper::Request<Incoming>,
    endpoint: Endpoint,
    peer_connections: kulfi_utils::PeerStreamSenders,
    proxy_target: Option<String>,
    graceful: kulfi_utils::Graceful,
) -> kulfi_utils::http::ProxyResult<eyre::Error> {
    let peer_id = match get_peer_id52_from_host(
        request.headers().get("Host").and_then(|header| header.to_str().ok()),
        proxy_target,
    ) {
        Ok(peer_id) => peer_id,
        Err(err) => {
            tracing::error!("failed to determine Kulfi peer id from Host header: {err:?}");
            return Ok(kulfi_utils::bad_request!(
                "failed to get peer id from request"
            ));
        }
    };

    kulfi_utils::http_to_peer(
        kulfi_utils::Protocol::Http.into(),
        request,
        endpoint,
        &peer_id,
        peer_connections,
        graceful,
    )
    .await
}

async fn run_tcp_bridge(
    listener: TokioTcpListener,
    endpoint: Endpoint,
    proxy_target: String,
) -> eyre::Result<()> {
    println!("Listening on 127.0.0.1:{}", listener.local_addr()?.port());
    let peer_connections = kulfi_utils::PeerStreamSenders::default();

    loop {
        let (stream, _) = listener.accept().await?;
        let endpoint = endpoint.clone();
        let peer_connections = peer_connections.clone();
        let proxy_target = proxy_target.clone();
        tokio::spawn(async move {
            if let Err(err) = kulfi_utils::tcp_to_peer(
                kulfi_utils::Protocol::Tcp.into(),
                endpoint,
                stream,
                &proxy_target,
                peer_connections,
                kulfi_utils::Graceful::default(),
            )
            .await
            {
                tracing::error!("Kulfi TCP bridge connection failed: {err:?}");
            }
        });
    }
}

fn get_peer_id52_from_host(host: Option<&str>, proxy_target: Option<String>) -> eyre::Result<String> {
    let first = match host.and_then(|host| host.split_once('.')) {
        Some((first, _)) => first,
        None => return Err(eyre::anyhow!("got http request without Host header")),
    };

    if first == "127" {
        if let Some(target) = proxy_target {
            return Ok(target);
        }
    }

    if first.len() != 52 && proxy_target.is_none() {
        return Err(eyre::anyhow!("got http request with invalid peer id"));
    }

    if let Some(target) = proxy_target {
        if first != target {
            return Err(eyre::anyhow!("got http request with invalid peer id"));
        }
    }

    Ok(first.to_string())
}

fn load_or_create_secret_key() -> Result<SecretKey> {
    let path = kulfi_secret_key_path()?;

    match fs::read_to_string(&path) {
        Ok(secret) => SecretKey::from_str(secret.trim())
            .with_context(|| format!("failed to parse Kulfi secret key at {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let secret_key = SecretKey::generate();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create Kulfi state dir {}", parent.display()))?;
            }
            fs::write(&path, format!("{}\n", secret_key))
                .with_context(|| format!("failed to write Kulfi secret key to {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o600);
                fs::set_permissions(&path, perms).with_context(|| {
                    format!("failed to tighten permissions on {}", path.display())
                })?;
            }
            Ok(secret_key)
        }
        Err(err) => Err(err).with_context(|| format!("failed to read Kulfi secret key from {}", path.display())),
    }
}

fn kulfi_secret_key_path() -> Result<PathBuf> {
    let data_dir = shuru_vm::default_data_dir();
    Ok(PathBuf::from(data_dir).join("kulfi").join("secret-key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_expose() {
        let expose = parse_expose("3000:http").unwrap();
        assert_eq!(expose.host_port, 3000);
        assert_eq!(expose.protocol, KulfiProtocol::Http);
        assert_eq!(expose.bridge_port, None);
    }

    #[test]
    fn parse_tcp_expose_with_bridge_port() {
        let expose = parse_expose("2222:tcp:9000").unwrap();
        assert_eq!(expose.host_port, 2222);
        assert_eq!(expose.protocol, KulfiProtocol::Tcp);
        assert_eq!(expose.bridge_port, Some(9000));
    }

    #[test]
    fn reject_unknown_protocol() {
        assert!(parse_expose("3000:udp").is_err());
    }

    #[test]
    fn reject_missing_forward() {
        let exposes = vec![KulfiExpose {
            host_port: 3000,
            protocol: KulfiProtocol::Http,
            bridge_port: None,
        }];
        let err = validate_exposes(&exposes, &[]).unwrap_err();
        assert!(err.to_string().contains("matching -p/--port"));
    }

    #[test]
    fn reject_duplicate_protocol_mapping() {
        let exposes = vec![
            KulfiExpose {
                host_port: 3000,
                protocol: KulfiProtocol::Http,
                bridge_port: None,
            },
            KulfiExpose {
                host_port: 3000,
                protocol: KulfiProtocol::Http,
                bridge_port: Some(8081),
            },
        ];
        let forwards = vec![PortMapping {
            host_port: 3000,
            guest_port: 3000,
        }];
        let err = validate_exposes(&exposes, &forwards).unwrap_err();
        assert!(err.to_string().contains("duplicate --kulfi"));
    }

    #[test]
    fn reject_bridge_port_conflicting_with_forward() {
        let exposes = vec![KulfiExpose {
            host_port: 3000,
            protocol: KulfiProtocol::Http,
            bridge_port: Some(3000),
        }];
        let forwards = vec![PortMapping {
            host_port: 3000,
            guest_port: 3000,
        }];
        let err = validate_exposes(&exposes, &forwards).unwrap_err();
        assert!(err.to_string().contains("conflicts with an existing -p/--port"));
    }

    #[test]
    fn reject_duplicate_explicit_bridge_ports() {
        let exposes = vec![
            KulfiExpose {
                host_port: 3000,
                protocol: KulfiProtocol::Http,
                bridge_port: Some(9000),
            },
            KulfiExpose {
                host_port: 4000,
                protocol: KulfiProtocol::Tcp,
                bridge_port: Some(9000),
            },
        ];
        let forwards = vec![
            PortMapping {
                host_port: 3000,
                guest_port: 3000,
            },
            PortMapping {
                host_port: 4000,
                guest_port: 4000,
            },
        ];
        let err = validate_exposes(&exposes, &forwards).unwrap_err();
        assert!(err.to_string().contains("duplicate explicit kulfi bridge port"));
    }
}
