//! HTTP and WebSocket on the unix socket. See README.md for the interface.
//!
//! This is shuru's thin layer over voxl's streaming hub: the unix-socket
//! transport, the JSON control protocol, the per-task gate (gate.rs) and the
//! two integration hooks (a keyframe on a desktop-lease change and on a
//! decoder-aware reconnect; input-quiet sharpening), each behind a capability
//! a plain voxl viewer never negotiates.

use std::convert::Infallible;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
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

use crate::gate::{Gate, GateState};
use crate::hub::{Client, Hub, Out};
use crate::input::{InputEvent, TEXT_MAX};

type Body = Full<Bytes>;

/// What the server routes over, carried to every handler.
#[derive(Clone)]
pub struct State {
    pub hub: Arc<Hub>,
    pub gate: Arc<GateState>,
}

/// The integration hooks a stream negotiated in `/stream?caps=`.
#[derive(Clone, Copy, Default)]
struct Caps {
    sharpen_on_input_quiet: bool,
    keyframe_on_lease: bool,
}

impl Caps {
    fn parse(csv: Option<&str>) -> Self {
        let mut c = Caps::default();
        for cap in csv.unwrap_or("").split(',') {
            match cap.trim() {
                "sharpenOnInputQuiet" => c.sharpen_on_input_quiet = true,
                "keyframeOnLease" => c.keyframe_on_lease = true,
                _ => {}
            }
        }
        c
    }
}

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
    Text {
        #[serde(deserialize_with = "capped_text")]
        text: String,
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
            Msg::Text { text } => InputEvent::Text(text),
            Msg::Keyframe | Msg::Ack { .. } => return None,
        })
    }
}

fn capped_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let text = String::deserialize(d)?;
    if text.chars().count() > TEXT_MAX {
        return Err(serde::de::Error::custom(format!(
            "text is longer than {TEXT_MAX} chars"
        )));
    }
    Ok(text)
}

#[derive(Deserialize)]
struct GateBody {
    viewer: Option<String>,
}

pub async fn connection(state: State, stream: tokio::net::UnixStream) {
    // Kept small so a slow viewer shows as queue here rather than hiding
    // in the kernel; voxl's flow control reads the backlog from acks.
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
        let state = state.clone();
        async move { Ok::<_, Infallible>(route(state, request).await) }
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

/// Moves the gate to `next` and applies the side effects when it actually changed:
/// release the keys the old holder held, tell each viewer whether its input lands now,
/// and (for the keyframe-on-lease hook) force a keyframe so a lease change starts the
/// new holder on a clean picture.
fn set_gate(state: &State, next: Gate) {
    let Some(_previous) = state.gate.set(next) else {
        return;
    };
    if let Some(tx) = state.hub.input.lock().as_ref() {
        let _ = tx.send(InputEvent::ReleaseAll);
    }
    let gate = state.gate.get();
    let keyframe = state.hub.keyframe_on_lease();
    for c in state.hub.clients() {
        c.send(Out::Text(gate.message(c.viewer.as_deref())));
        if keyframe {
            c.resync();
        }
    }
    state.hub.wake();
}

async fn route(state: State, mut request: Request<Incoming>) -> Response<Body> {
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
            still(&state.hub, width, cursor).await
        }
        (Method::GET, "/stream") => {
            let viewer = query(&request, "viewer");
            if viewer
                .as_deref()
                .is_some_and(|v| v.is_empty() || v == "agent")
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "a viewer id is not empty and not \"agent\"",
                );
            }
            let caps = Caps::parse(query(&request, "caps").as_deref());
            upgrade(&mut request, move |socket| {
                stream(state, viewer, caps, socket)
            })
        }
        (Method::GET, "/gate") => json(StatusCode::OK, state.gate.get().holder()),
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
            let out = gate.holder();
            set_gate(&state, gate);
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
            if !state.gate.may_agent() {
                return error(
                    StatusCode::CONFLICT,
                    "the input gate is not open for the agent",
                );
            }
            let n = input.len();
            if let Some(tx) = state.hub.input.lock().as_ref() {
                for i in input {
                    let _ = tx.send(i);
                }
            }
            state.hub.note_input();
            state.hub.wake();
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
    hub.snapshots.lock().push(tx);
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

async fn stream(state: State, viewer: Option<String>, caps: Caps, socket: Socket) {
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    state
        .hub
        .add_caps(caps.sharpen_on_input_quiet, caps.keyframe_on_lease);
    let client = state.hub.add_client(viewer.clone(), tx);
    // The gate state the hub does not carry, sent after the hub's hello/cursor/pointer.
    client.send(Out::Text(state.gate.get().message(viewer.as_deref())));
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let msg = match m {
                Out::Bin(b) => Message::Binary(b),
                Out::Text(t) => Message::Text(t.into()),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = source.next().await {
        match msg {
            Message::Text(t) => on_text(&state, &client, t.as_str()),
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
    // Keys and buttons held through this stream are let go; the gate stays
    // with the viewer, who may reconnect.
    if state.gate.may_viewer(viewer.as_deref()) {
        if let Some(tx) = state.hub.input.lock().as_ref() {
            let _ = tx.send(InputEvent::ReleaseAll);
        }
    }
    state
        .hub
        .remove_caps(caps.sharpen_on_input_quiet, caps.keyframe_on_lease);
    state.hub.remove_client(client.id);
}

fn on_text(state: &State, client: &Client, text: &str) {
    // Messages this version does not know are skipped.
    let Ok(msg) = serde_json::from_str::<Msg>(text) else {
        return;
    };
    match msg {
        Msg::Ack { frame } => {
            client.on_ack(frame);
            state.hub.wake();
        }
        Msg::Keyframe => {
            // Decoder-aware reconnect hook: the viewer asks for a fresh keyframe.
            client.needs_keyframe.store(true, Ordering::Relaxed);
            client.started.store(false, Ordering::Relaxed);
            state.hub.wake();
        }
        input => {
            if state.gate.may_viewer(client.viewer.as_deref()) {
                if let Some(ev) = input.input() {
                    if let Some(tx) = state.hub.input.lock().as_ref() {
                        let _ = tx.send(ev);
                    }
                    state.hub.note_input();
                    state.hub.wake();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_message() {
        let msg: Msg = serde_json::from_str(r#"{"type":"text","text":"你好，世界\n"}"#).unwrap();
        match msg.input() {
            Some(InputEvent::Text(t)) => assert_eq!(t, "你好，世界\n"),
            _ => panic!("not a text event"),
        }
        let list: Vec<Msg> = serde_json::from_str(
            r#"[{"type":"text","text":"a"},{"type":"key","keysym":65293,"down":true}]"#,
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert!(serde_json::from_str::<Msg>(r#"{"type":"text"}"#).is_err());
        assert!(serde_json::from_str::<Msg>(r#"{"type":"text","text":5}"#).is_err());
    }

    #[test]
    fn text_is_capped_in_chars() {
        let at_cap = format!(r#"{{"type":"text","text":"{}"}}"#, "界".repeat(TEXT_MAX));
        assert!(serde_json::from_str::<Msg>(&at_cap).is_ok());
        let over = format!(r#"{{"type":"text","text":"{}"}}"#, "a".repeat(TEXT_MAX + 1));
        let err = serde_json::from_str::<Msg>(&over)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("text is longer than 4096 chars"), "{err}");
    }

    #[test]
    fn caps_parse_only_known() {
        let c = Caps::parse(Some("sharpenOnInputQuiet,keyframeOnLease,bogus"));
        assert!(c.sharpen_on_input_quiet && c.keyframe_on_lease);
        let none = Caps::parse(None);
        assert!(!none.sharpen_on_input_quiet && !none.keyframe_on_lease);
    }
}
