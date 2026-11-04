//! HTTP and WebSocket on the unix socket. See README.md for the interface.

use std::convert::Infallible;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{
    HeaderValue, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_VERSION, UPGRADE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::hub::{Client, Gate, Hub, Out};
use crate::input::InputEvent;

type Body = Full<Bytes>;

/// A message from a viewer, and the events of `POST /input`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Msg {
    Move {
        x: i32,
        y: i32,
    },
    Button {
        button: u8,
        down: bool,
    },
    Wheel {
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
    },
    Key {
        keysym: u32,
        down: bool,
    },
    Keyframe,
    Ack {
        frame: u32,
    },
}

impl Msg {
    fn input(self) -> Option<InputEvent> {
        Some(match self {
            Msg::Move { x, y } => InputEvent::Move { x, y },
            Msg::Button { button, down } => InputEvent::Button { button, down },
            Msg::Wheel { dx, dy } => InputEvent::Wheel { dx, dy },
            Msg::Key { keysym, down } => InputEvent::Key { keysym, down },
            Msg::Keyframe | Msg::Ack { .. } => return None,
        })
    }
}

#[derive(Deserialize)]
struct GateBody {
    viewer: Option<String>,
}

pub async fn connection(hub: Arc<Hub>, stream: tokio::net::UnixStream) {
    // Kept small so a slow viewer shows as queue here rather than hiding
    // in the kernel.
    let fd = stream.as_raw_fd();
    let size: libc::c_int = 64 << 10;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    let service = service_fn(move |request| {
        let hub = hub.clone();
        async move { Ok::<_, Infallible>(route(hub, fd, request).await) }
    });
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

fn reply(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    r.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    reply(status, "application/json", value.to_string())
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response<Body> {
    json(status, serde_json::json!({ "error": msg.to_string() }))
}

fn query(request: &Request<Incoming>, key: &str) -> Option<String> {
    form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

async fn body<T: serde::de::DeserializeOwned>(
    request: Request<Incoming>,
    limit: usize,
) -> Result<T, String> {
    let bytes = Limited::new(request.into_body(), limit)
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn gate_json(gate: &Gate) -> serde_json::Value {
    match gate {
        Gate::Shut => serde_json::json!({ "viewer": null }),
        Gate::Agent => serde_json::json!({ "viewer": "agent" }),
        Gate::Viewer(v) => serde_json::json!({ "viewer": v }),
    }
}

async fn route(hub: Arc<Hub>, fd: RawFd, mut request: Request<Incoming>) -> Response<Body> {
    let path = request.uri().path().to_string();
    match (request.method().clone(), path.as_str()) {
        (Method::GET, "/healthz") => reply(StatusCode::OK, "text/plain", "ok\n"),
        (Method::GET, "/still") => {
            let width = match query(&request, "width").map(|w| w.parse::<usize>()) {
                None => None,
                Some(Ok(w)) if w > 0 => Some(w),
                Some(_) => return error(StatusCode::BAD_REQUEST, "width is a positive number"),
            };
            let cursor = match query(&request, "cursor").as_deref() {
                None | Some("0") => false,
                Some("1") => true,
                Some(_) => return error(StatusCode::BAD_REQUEST, "cursor is 0 or 1"),
            };
            still(&hub, width, cursor).await
        }
        (Method::GET, "/stream") => {
            let viewer = query(&request, "viewer");
            if viewer.as_deref().is_some_and(|v| v.is_empty() || v == "agent") {
                return error(StatusCode::BAD_REQUEST, "a viewer id is not empty and not \"agent\"");
            }
            upgrade(&mut request, move |socket| stream(hub, viewer, fd, socket))
        }
        (Method::GET, "/gate") => json(StatusCode::OK, gate_json(&hub.gate.lock().unwrap())),
        (Method::POST, "/gate") => {
            let b: GateBody = match body(request, 4096).await {
                Ok(b) => b,
                Err(e) => return error(StatusCode::BAD_REQUEST, e),
            };
            let gate = match b.viewer {
                None => Gate::Shut,
                Some(v) if v == "agent" => Gate::Agent,
                Some(v) if v.is_empty() => return error(StatusCode::BAD_REQUEST, "empty viewer"),
                Some(v) => Gate::Viewer(v),
            };
            let out = gate_json(&gate);
            hub.set_gate(gate);
            json(StatusCode::OK, out)
        }
        (Method::POST, "/input") => {
            let events: Vec<Msg> = match body(request, 1 << 20).await {
                Ok(e) => e,
                Err(e) => return error(StatusCode::BAD_REQUEST, e),
            };
            let mut input = Vec::with_capacity(events.len());
            for e in events {
                match e.input() {
                    Some(i) => input.push(i),
                    None => return error(StatusCode::BAD_REQUEST, "not an input event"),
                }
            }
            // Checked after the body is read, so the gate cannot shut while
            // half the events are in.
            let gate = hub.gate.lock().unwrap();
            if *gate != Gate::Agent {
                return error(StatusCode::CONFLICT, "the input gate is not open for the agent");
            }
            let n = input.len();
            for i in input {
                let _ = hub.input.send(i);
            }
            drop(gate);
            json(StatusCode::OK, serde_json::json!({ "applied": n }))
        }
        (_, "/healthz" | "/still" | "/stream" | "/gate" | "/input") => {
            error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        }
        _ => error(StatusCode::NOT_FOUND, format!("no route {path}")),
    }
}

async fn still(hub: &Hub, width: Option<usize>, cursor: bool) -> Response<Body> {
    let (tx, rx) = oneshot::channel();
    hub.snapshots.lock().unwrap().push(tx);
    hub.wake();
    let Ok(Ok(shot)) = tokio::time::timeout(Duration::from_secs(5), rx).await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "no frame from capture");
    };
    match tokio::task::spawn_blocking(move || crate::still::png(&shot, width, cursor)).await {
        Ok(Ok(png)) => reply(StatusCode::OK, "image/png", png),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

type Socket = WebSocketStream<TokioIo<Upgraded>>;

fn upgrade<F, Fut>(request: &mut Request<Incoming>, serve: F) -> Response<Body>
where
    F: FnOnce(Socket) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let has = |name, token: &str| {
        request.headers().get_all(name).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case(token))
            })
        })
    };
    if !has(CONNECTION, "upgrade") || !has(UPGRADE, "websocket") {
        return error(StatusCode::BAD_REQUEST, "expected a WebSocket upgrade");
    }
    if request
        .headers()
        .get(SEC_WEBSOCKET_VERSION)
        .is_none_or(|v| v != "13")
    {
        return error(StatusCode::BAD_REQUEST, "expected WebSocket version 13");
    }
    let Some(key) = request.headers().get(SEC_WEBSOCKET_KEY) else {
        return error(StatusCode::BAD_REQUEST, "no Sec-WebSocket-Key");
    };
    let Ok(accept) = HeaderValue::from_str(&derive_accept_key(key.as_bytes())) else {
        return error(StatusCode::BAD_REQUEST, "bad Sec-WebSocket-Key");
    };
    let upgrade = hyper::upgrade::on(request);
    tokio::spawn(async move {
        let Ok(upgraded) = upgrade.await else {
            return;
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(1 << 20))
            .max_frame_size(Some(1 << 20));
        let socket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config))
                .await;
        serve(socket).await;
    });
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(SEC_WEBSOCKET_ACCEPT, accept);
    response
}

async fn stream(hub: Arc<Hub>, viewer: Option<String>, fd: RawFd, socket: Socket) {
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let client = hub.add_client(viewer.clone(), tx, fd);
    let c2 = client.clone();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let n = m.len();
            let msg = match m {
                Out::Bin(b) => Message::Binary(b),
                Out::Text(t) => Message::Text(t.into()),
            };
            let sent = sink.send(msg).await;
            c2.wrote(n);
            if sent.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = source.next().await {
        match msg {
            Message::Text(t) => on_text(&hub, &client, t.as_str()),
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
    // Keys and buttons held through this stream are let go; the gate stays
    // with the viewer, who may reconnect.
    if hub.may_input(viewer.as_deref()) {
        let _ = hub.input.send(InputEvent::ReleaseAll);
    }
    hub.remove_client(client.id);
}

fn on_text(hub: &Hub, client: &Client, text: &str) {
    // Messages this version does not know are skipped.
    let Ok(msg) = serde_json::from_str::<Msg>(text) else {
        return;
    };
    match msg {
        Msg::Ack { frame } => {
            client.on_ack(frame);
            hub.wake();
        }
        Msg::Keyframe => {
            client.resync();
            hub.wake();
        }
        input => {
            let gate = hub.gate.lock().unwrap();
            let open = matches!((&*gate, client.viewer.as_deref()), (Gate::Viewer(g), Some(v)) if g == v);
            if let (true, Some(ev)) = (open, input.input()) {
                let _ = hub.input.send(ev);
            }
        }
    }
}
