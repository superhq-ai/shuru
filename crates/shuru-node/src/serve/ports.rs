//! Ports: `proxy` carries a TCP connection made inside the guest over a
//! WebSocket, and `ports/watch` follows the ports listening in the guest.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{Sink, SinkExt, StreamExt};
use hyper::body::Incoming;
use hyper::{Request, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use shuru_proto::{frame, ForwardRequest, ForwardResponse, PortEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use super::guest::{self, Conn};
use super::http::{ApiError, Body};
use super::sandboxes::{Entry, GuestAddr, Node};
use super::ws;

const MAX_PROXY_MESSAGE: usize = 1 << 20;
const MAX_WATCH_MESSAGE: usize = 64 * 1024;
const PROXY_CHUNK: usize = 64 * 1024;
/// How long a proxy socket may wait before it says where to connect.
const PROXY_HELLO: Duration = Duration::from_secs(10);
/// How long a side of a proxy may take nothing while data waits for it.
const STALL: Duration = Duration::from_secs(60);
/// How long a watch client may take nothing while an event waits for it.
const WATCH_STALL: Duration = Duration::from_secs(10);
const MAX_HOST: usize = 253;

#[derive(Deserialize)]
struct ProxyTarget {
    host: String,
    port: u16,
}

pub async fn proxy(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let key = ws::key(&request)?;
    let addr = node.guest(entry).await?;
    ws::upgrade(
        &mut request,
        &key,
        MAX_PROXY_MESSAGE,
        entry.busy(),
        move |socket| carry(socket, addr),
    )
}

async fn carry(mut socket: ws::Socket, addr: GuestAddr) {
    let target = match timeout(PROXY_HELLO, socket.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => target(text.as_str()),
        Ok(Some(Ok(Message::Binary(data)))) => target(&String::from_utf8_lossy(&data)),
        Ok(Some(Ok(Message::Close(_)) | Err(_)) | None) => return,
        Ok(Some(Ok(_))) => Err("expected {\"host\":...,\"port\":...}".to_string()),
        Err(_) => Err("no target within 10 s".to_string()),
    };
    let opened = match target {
        Ok(target) => open(&addr, &target)
            .await
            .map(|conn| (conn, format!("{}:{}", target.host, target.port)))
            .map_err(|e| e.message().to_string()),
        Err(message) => Err(message),
    };
    let (conn, target) = match opened {
        Ok(opened) => opened,
        Err(message) => {
            let error = json!({ "status": "error", "message": message });
            if send(&mut socket, Message::text(error.to_string()), STALL).await {
                let close = close_frame(CloseCode::Error, "proxy failed");
                send(&mut socket, close, STALL).await;
            }
            return;
        }
    };
    let connected = json!({ "status": "connected", "target": target });
    if send(&mut socket, Message::text(connected.to_string()), STALL).await {
        relay(socket, conn).await;
    }
}

fn target(text: &str) -> Result<ProxyTarget, String> {
    let target: ProxyTarget = serde_json::from_str(text)
        .map_err(|_| "expected {\"host\":...,\"port\":...}".to_string())?;
    if target.port == 0 {
        return Err("port 0 is not a port to connect to".into());
    }
    if target.host.is_empty() || target.host.len() > MAX_HOST {
        return Err(format!("a host name takes 1 to {MAX_HOST} bytes"));
    }
    Ok(target)
}

async fn open(addr: &GuestAddr, target: &ProxyTarget) -> Result<Conn, ApiError> {
    let mut conn = guest::connect_forward(addr, &addr.streams).await?;
    let request = ForwardRequest {
        port: target.port,
        host: Some(target.host.clone()),
        whole_close: true,
    };
    guest::send_json(&mut conn.stream, frame::FWD_REQ, &request).await?;
    match guest::read_frame(&mut conn.stream).await? {
        Some((frame::FWD_RESP, payload)) => {
            let response: ForwardResponse = serde_json::from_slice(&payload)
                .map_err(|e| ApiError::internal(format!("the guest forward answer: {e}")))?;
            if response.status == "ok" {
                Ok(conn)
            } else {
                Err(ApiError::bad_request(
                    response.message.unwrap_or_else(|| "connect failed".into()),
                ))
            }
        }
        Some((kind, _)) => Err(ApiError::internal(format!(
            "the guest answered with frame {kind:#x}"
        ))),
        None => Err(ApiError::internal("the guest closed the connection")),
    }
}

/// Carries bytes both ways until either side ends, or until a side takes
/// nothing for `STALL` while data waits for it.
async fn relay(socket: ws::Socket, conn: Conn) {
    // The permit goes with the connection, whichever side ends first.
    let Conn { stream, permit } = conn;
    let (mut from_guest, mut to_guest) = stream.into_split();
    let (mut sink, mut source) = socket.split();
    let upstream = async {
        while let Some(Ok(message)) = source.next().await {
            let data = match message {
                Message::Binary(data) => data,
                Message::Text(text) => text.into(),
                Message::Close(_) => break,
                _ => continue,
            };
            let mut left = &data[..];
            while !left.is_empty() {
                match timeout(STALL, to_guest.write(left)).await {
                    Ok(Ok(n)) if n > 0 => left = &left[n..],
                    _ => return,
                }
            }
        }
    };
    let downstream = async {
        let mut buf = vec![0u8; PROXY_CHUNK];
        loop {
            match from_guest.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !send(&mut sink, Message::binary(buf[..n].to_vec()), STALL).await {
                        return;
                    }
                }
            }
        }
        send(&mut sink, close_frame(CloseCode::Normal, ""), STALL).await;
    };
    tokio::select! {
        () = upstream => {}
        () = downstream => {}
    }
    drop((from_guest, to_guest));
    drop(permit);
}

/// Sends `message`, false if it failed or the client took nothing for
/// `stall`.
async fn send<S>(sink: &mut S, message: Message, stall: Duration) -> bool
where
    S: Sink<Message> + Unpin,
{
    matches!(timeout(stall, sink.send(message)).await, Ok(Ok(())))
}

fn close_frame(code: CloseCode, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

pub async fn watch(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let key = ws::key(&request)?;
    let addr = node.guest(entry).await?;
    let mut conn = guest::connect(&addr, &addr.streams).await?;
    guest::write_frame(&mut conn.stream, frame::PORTS_WATCH_REQ, &[]).await?;
    // The list comes first, or the reason there is none.
    let first = match guest::read_frame(&mut conn.stream).await? {
        Some((frame::PORT_EVENT, payload)) => event_json(&payload)?,
        Some((frame::ERROR, payload)) => return Err(guest::guest_error(&payload)),
        Some((kind, _)) => {
            return Err(ApiError::internal(format!(
                "the guest answered with frame {kind:#x}"
            )))
        }
        None => return Err(ApiError::internal("the guest closed the connection")),
    };
    ws::upgrade(
        &mut request,
        &key,
        MAX_WATCH_MESSAGE,
        entry.busy(),
        move |socket| follow(socket, conn, first),
    )
}

async fn follow(socket: ws::Socket, conn: Conn, first: Value) {
    let (mut sink, mut source) = socket.split();
    if !send(&mut sink, Message::text(first.to_string()), WATCH_STALL).await {
        return;
    }
    // The stream keeps a frame read in progress when a client message
    // comes first, so no frame is cut.
    let frames = futures_util::stream::unfold(conn, |mut conn| async move {
        let frame = guest::read_frame(&mut conn.stream).await;
        Some((frame, conn))
    });
    let mut frames = Box::pin(frames);
    loop {
        let message = tokio::select! {
            Some(frame) = frames.next() => match frame {
                Ok(Some((frame::PORT_EVENT, payload))) => match event_json(&payload) {
                    Ok(event) => event,
                    Err(_) => continue,
                },
                Ok(Some((frame::ERROR, payload))) => {
                    let message = String::from_utf8_lossy(&payload);
                    json!({ "type": "error", "message": message })
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            },
            message = source.next() => match message {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
        };
        let last = message["type"] == "error";
        if !send(&mut sink, Message::text(message.to_string()), WATCH_STALL).await {
            return;
        }
        if last {
            break;
        }
    }
    drop(frames);
    let close = close_frame(CloseCode::Error, "the port watch ended");
    send(&mut sink, close, WATCH_STALL).await;
}

/// A guest port event as the API sends it: list entries carry the type of
/// an opened port.
fn event_json(payload: &[u8]) -> Result<Value, ApiError> {
    let event: PortEvent = serde_json::from_slice(payload)
        .map_err(|e| ApiError::internal(format!("the guest port event: {e}")))?;
    Ok(match event {
        PortEvent::PortList { ports } => {
            let ports: Vec<Value> = ports
                .into_iter()
                .map(|port| json!(PortEvent::PortOpened(port)))
                .collect();
            json!({ "type": "port_list", "ports": ports })
        }
        event => json!(event),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuru_proto::Port;

    #[test]
    fn list_entries_carry_the_opened_type() {
        let port = Port {
            port: 3000,
            address: "::".into(),
            pid: 9,
        };
        let list = serde_json::to_vec(&PortEvent::PortList {
            ports: vec![port.clone()],
        })
        .unwrap();
        assert_eq!(
            event_json(&list).unwrap(),
            json!({
                "type": "port_list",
                "ports": [{ "type": "port_opened", "port": 3000, "address": "::", "pid": 9 }],
            })
        );
        let closed = serde_json::to_vec(&PortEvent::PortClosed(port)).unwrap();
        assert_eq!(event_json(&closed).unwrap()["type"], "port_closed");
    }

    #[test]
    fn proxy_targets_are_checked() {
        let ok = target(r#"{"host":"localhost","port":8080}"#).unwrap();
        assert_eq!((ok.host.as_str(), ok.port), ("localhost", 8080));
        assert!(target(r#"{"host":"localhost","port":0}"#).is_err());
        assert!(target(r#"{"host":"","port":80}"#).is_err());
        assert!(target(r#"{"port":80}"#).is_err());
        assert!(target("hello").is_err());
    }
}
