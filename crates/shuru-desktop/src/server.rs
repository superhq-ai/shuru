//! HTTP on the unix socket. See README.md for the routes.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::wm::{Cmd, Handle, Settle, Who};

type Body = Full<Bytes>;

#[derive(Clone)]
pub struct State {
    pub wm: Handle,
    pub settle: Arc<Settle>,
}

pub async fn connection(state: State, stream: tokio::net::UnixStream) {
    let service = service_fn(move |request| {
        let state = state.clone();
        async move { Ok::<_, Infallible>(route(state, request).await) }
    });
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

fn json_reply(status: StatusCode, value: Value) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(value.to_string())));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response<Body> {
    json_reply(status, json!({ "error": msg.to_string() }))
}

fn answer(out: Result<Value, (u16, String)>) -> Response<Body> {
    match out {
        Ok(v) => json_reply(StatusCode::OK, v),
        Err((code, msg)) => error(
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            msg,
        ),
    }
}

fn query(request: &Request<Incoming>, key: &str) -> Option<String> {
    form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn query_ms(request: &Request<Incoming>, key: &str, default: u64) -> Result<u64, String> {
    match query(request, key) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("{key} is a number of milliseconds")),
    }
}

async fn body<T: serde::de::DeserializeOwned>(request: Request<Incoming>) -> Result<T, String> {
    let bytes = Limited::new(request.into_body(), 64 * 1024)
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let bytes: &[u8] = if bytes.is_empty() { b"{}" } else { &bytes };
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct NoteAction {
    key: Option<String>,
}

#[derive(Deserialize)]
struct Appearance {
    appearance: String,
}

#[derive(Deserialize)]
struct Id {
    id: u32,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Focus {
    id: u32,
    #[serde(default = "yes")]
    maximize: bool,
}

#[derive(Deserialize)]
struct Launch {
    app: String,
    #[serde(default)]
    args: Vec<String>,
    timeout_ms: Option<u64>,
    #[serde(default = "yes")]
    maximize: bool,
}

#[derive(Deserialize)]
struct Point {
    x: i16,
    y: i16,
}

async fn route(state: State, request: Request<Incoming>) -> Response<Body> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    if !matches!(path.as_str(), "/healthz" | "/settle" | "/windows") {
        eprintln!("shuru-desktop: {method} {path}");
    }
    match (method, path.as_str()) {
        (Method::GET, "/healthz") => json_reply(StatusCode::OK, json!({ "ok": true })),
        (Method::GET, "/windows") => answer(state.wm.call(Cmd::Windows).await),
        (Method::POST, "/focus") => match body::<Focus>(request).await {
            Ok(b) => answer(
                state
                    .wm
                    .call(|reply| Cmd::Focus {
                        id: b.id,
                        maximize: b.maximize,
                        reply,
                    })
                    .await,
            ),
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        (Method::POST, "/close") => match body::<Id>(request).await {
            Ok(b) => answer(state.wm.call(|r| Cmd::Close(b.id, r)).await),
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        (Method::POST, "/launch") => match body::<Launch>(request).await {
            Ok(b) => {
                let timeout =
                    Duration::from_millis(b.timeout_ms.unwrap_or(15_000).clamp(100, 120_000));
                answer(
                    state
                        .wm
                        .call(|reply| Cmd::Launch {
                            app: b.app,
                            args: b.args,
                            timeout,
                            maximize: b.maximize,
                            reply,
                        })
                        .await,
                )
            }
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        (Method::GET, "/appearance") => answer(state.wm.call(|r| Cmd::Appearance(None, r)).await),
        (Method::PUT, "/appearance") => match body::<Appearance>(request).await {
            Ok(b) => {
                let dark = match b.appearance.as_str() {
                    "dark" => true,
                    "light" => false,
                    _ => {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "appearance is light or dark".to_string(),
                        )
                    }
                };
                answer(state.wm.call(|r| Cmd::Appearance(Some(dark), r)).await)
            }
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        (Method::GET, "/notifications") => answer(state.wm.call(Cmd::Notifications).await),
        (Method::POST, p) if p.starts_with("/notifications/") && p.ends_with("/action") => {
            let id = p
                .trim_start_matches("/notifications/")
                .trim_end_matches("/action")
                .parse::<u32>();
            let Ok(id) = id else {
                return error(StatusCode::NOT_FOUND, "no such notification");
            };
            match body::<NoteAction>(request).await {
                Ok(b) => answer(
                    state
                        .wm
                        .call(|reply| Cmd::NoteAction {
                            id,
                            key: b.key.unwrap_or_else(|| "default".into()),
                            reply,
                        })
                        .await,
                ),
                Err(e) => error(StatusCode::BAD_REQUEST, e),
            }
        }
        (Method::GET, "/settle") => {
            let quiet = match query_ms(&request, "quiet_ms", 300) {
                Ok(v) => Duration::from_millis(v.min(60_000)),
                Err(e) => return error(StatusCode::BAD_REQUEST, e),
            };
            let max = match query_ms(&request, "max_ms", 5000) {
                Ok(v) => Duration::from_millis(v.min(120_000)),
                Err(e) => return error(StatusCode::BAD_REQUEST, e),
            };
            settle(&state.settle, quiet, max).await
        }
        (Method::GET, "/overlay") => answer(
            state
                .wm
                .call(|reply| Cmd::Overlay {
                    who: None,
                    ripple: None,
                    reply,
                })
                .await,
        ),
        (Method::POST, "/overlay") => {
            let v: Value = match body(request).await {
                Ok(v) => v,
                Err(e) => return error(StatusCode::BAD_REQUEST, e),
            };
            // `who` absent leaves it; null hands control to nobody.
            let who = match v.get("who") {
                None => None,
                Some(w) => match serde_json::from_value::<Option<Who>>(w.clone()) {
                    Ok(w) => Some(w),
                    Err(_) => {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "who is \"agent\", \"person\" or null",
                        )
                    }
                },
            };
            let ripple = match v.get("ripple").filter(|r| !r.is_null()) {
                None => None,
                Some(p) => match serde_json::from_value::<Point>(p.clone()) {
                    Ok(p) => Some((p.x, p.y)),
                    Err(_) => return error(StatusCode::BAD_REQUEST, "ripple is {x, y}"),
                },
            };
            answer(
                state
                    .wm
                    .call(|reply| Cmd::Overlay { who, ripple, reply })
                    .await,
            )
        }
        (
            _,
            "/healthz" | "/windows" | "/focus" | "/close" | "/launch" | "/settle" | "/overlay"
            | "/appearance" | "/notifications",
        ) => error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
        _ => error(StatusCode::NOT_FOUND, format!("no route {path}")),
    }
}

/// Answers once the screen has been still for `quiet`, or at `max`.
async fn settle(s: &Settle, quiet: Duration, max: Duration) -> Response<Body> {
    let start = Instant::now();
    loop {
        let still = s.quiet();
        let waited = start.elapsed();
        if still >= quiet || waited >= max {
            return json_reply(
                StatusCode::OK,
                json!({
                    "settled": still >= quiet,
                    "waited_ms": waited.as_millis() as u64,
                    "quiet_ms": still.as_millis() as u64,
                }),
            );
        }
        tokio::time::sleep((quiet - still).min(max - waited)).await;
    }
}
