//! HTTP on the API socket: routing, errors and the pieces the handlers
//! share. Paths and shapes follow the sandbox API (RFC 0012).

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body as _, Bytes, Frame, Incoming};
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::exec;
use super::sandboxes::{Entry, Node};

pub type Body = BoxBody<Bytes, Infallible>;

const MAX_JSON_BODY: usize = 64 * 1024;
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 500;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "already_exists", message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    fn response(&self) -> Response<Body> {
        let body = json!({ "error": self.code, "message": self.message });
        json_response(self.status, &body)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

pub fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    let mut response = Response::new(full(value.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).boxed()
}

/// A body that sends each buffer from the channel as one HTTP chunk, and
/// ends when the sending side is dropped.
pub struct ChannelBody(pub mpsc::Receiver<Bytes>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|c| Ok(Frame::data(c))))
    }
}

pub fn streaming(content_type: &'static str, rx: mpsc::Receiver<Bytes>) -> Response<Body> {
    let mut response = Response::new(ChannelBody(rx).boxed());
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// Reads a request body of at most `max` bytes.
pub async fn read_body(body: Incoming, max: usize) -> Result<Bytes, ApiError> {
    if body.size_hint().lower() > max as u64 {
        return Err(ApiError::too_large(format!("the body is over {max} bytes")));
    }
    match Limited::new(body, max).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            Err(ApiError::too_large(format!("the body is over {max} bytes")))
        }
        Err(e) => Err(ApiError::bad_request(format!("reading the body: {e}"))),
    }
}

/// The query string as decoded pairs, in order, repeated keys kept.
pub fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

pub fn parse_bool(key: &str, value: &str) -> Result<bool, ApiError> {
    match value {
        "" | "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ApiError::bad_request(format!(
            "{key}: expected true or false"
        ))),
    }
}

pub fn parse_number<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, ApiError> {
    value
        .parse()
        .map_err(|_| ApiError::bad_request(format!("{key}: expected a number")))
}

/// Seconds, given as a number or with a unit: `500ms`, `30s`, `5m`, `1h`.
/// Parts of a second round up.
pub fn parse_seconds(key: &str, value: &str) -> Result<u64, ApiError> {
    let bad = || ApiError::bad_request(format!("{key}: expected seconds, such as 30 or 30s"));
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "" | "s" => Some(number),
        "ms" => Some(number.div_ceil(1000)),
        "m" => number.checked_mul(60),
        "h" => number.checked_mul(3600),
        _ => None,
    };
    seconds.ok_or_else(bad)
}

/// Unix seconds as RFC 3339.
pub fn timestamp(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|unix| time::OffsetDateTime::from_unix_timestamp(unix).ok())
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

pub async fn connection(node: Arc<Node>, stream: tokio::net::UnixStream) {
    let service = service_fn(move |request| {
        let node = node.clone();
        async move {
            let response = match route(node, request).await {
                Ok(response) => response,
                Err(e) => e.response(),
            };
            Ok::<_, Infallible>(response)
        }
    });
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

async fn route(node: Arc<Node>, request: Request<Incoming>) -> Result<Response<Body>, ApiError> {
    let path = request.uri().path().to_string();
    let Some(rest) = path.strip_prefix("/v1/sandboxes") else {
        return Err(ApiError::not_found(format!("no route {path}")));
    };
    let segments: Vec<&str> = match rest {
        "" | "/" => Vec::new(),
        rest if rest.starts_with('/') => rest[1..].trim_end_matches('/').split('/').collect(),
        _ => return Err(ApiError::not_found(format!("no route {path}"))),
    };
    let method = request.method().clone();
    match (&method, segments.as_slice()) {
        (&Method::GET, []) => list(&node, &request),
        (&Method::POST, []) => create(&node, request).await,
        (&Method::GET, [name]) => {
            let entry = node.get(name)?;
            Ok(json_response(StatusCode::OK, &sandbox_json(&entry)))
        }
        (&Method::DELETE, [name]) => {
            node.delete(name).await?;
            let mut response = Response::new(full(Bytes::new()));
            *response.status_mut() = StatusCode::NO_CONTENT;
            Ok(response)
        }
        (&Method::GET, [name, "exec"]) => {
            let entry = node.get(name)?;
            if exec::is_websocket(&request) {
                exec::websocket(&node, &entry, request, None).await
            } else {
                exec::list(&entry).await
            }
        }
        (&Method::POST, [name, "exec"]) => {
            let entry = node.get(name)?;
            exec::http(&node, &entry, request).await
        }
        (&Method::GET, [name, "exec", session]) => {
            let entry = node.get(name)?;
            let session = session.to_string();
            exec::websocket(&node, &entry, request, Some(session)).await
        }
        (&Method::POST, [name, "exec", session, "kill"]) => {
            let entry = node.get(name)?;
            exec::kill(&entry, session, request.uri().query()).await
        }
        (_, [] | [_] | [_, "exec"] | [_, "exec", _] | [_, "exec", _, "kill"]) => {
            Err(ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                format!("{method} is not allowed on {path}"),
            ))
        }
        _ => Err(ApiError::not_found(format!("no route {path}"))),
    }
}

fn list(node: &Node, request: &Request<Incoming>) -> Result<Response<Body>, ApiError> {
    let mut prefix = String::new();
    let mut after = None;
    let mut max = DEFAULT_PAGE;
    for (key, value) in query_pairs(request.uri().query()) {
        match key.as_str() {
            "prefix" => prefix = value,
            "continuation_token" if !value.is_empty() => after = Some(value),
            "max_results" => {
                max = parse_number(&key, &value)?;
                if !(1..=MAX_PAGE).contains(&max) {
                    return Err(ApiError::bad_request(format!(
                        "max_results: expected 1 to {MAX_PAGE}"
                    )));
                }
            }
            _ => {}
        }
    }
    let (page, more) = node.list(&prefix, after.as_deref(), max);
    let running = page.iter().filter(|entry| entry.is_running()).count();
    let mut body = json!({
        "sandboxes": page.iter().map(|entry| sandbox_json(entry)).collect::<Vec<_>>(),
        "has_more": more,
        "running": running,
        "warm": 0,
        "cold": page.len() - running,
    });
    if more {
        if let Some(last) = page.last() {
            body["next_continuation_token"] = json!(last.record.name);
        }
    }
    Ok(json_response(StatusCode::OK, &body))
}

#[derive(Deserialize)]
struct CreateRequest {
    name: String,
}

async fn create(node: &Arc<Node>, request: Request<Incoming>) -> Result<Response<Body>, ApiError> {
    let body = read_body(request.into_body(), MAX_JSON_BODY).await?;
    let create: CreateRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("the body: {e}")))?;
    let entry = node.create(create.name).await?;
    Ok(json_response(StatusCode::CREATED, &sandbox_json(&entry)))
}

/// A sandbox is `running` while its VM is up and `cold` otherwise; a cold
/// sandbox keeps its disk, and the next request boots it.
fn sandbox_json(entry: &Entry) -> Value {
    let record = &entry.record;
    let created = timestamp(record.created_at);
    json!({
        "id": record.id,
        "name": record.name,
        "status": if entry.is_running() { "running" } else { "cold" },
        "url": null,
        "url_settings": null,
        "created_at": created,
        "updated_at": created,
        "last_running_at": entry.last_running_at().map(timestamp),
        "last_warming_at": null,
        "environment_version": env!("CARGO_PKG_VERSION"),
        "version": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds() {
        assert_eq!(parse_seconds("t", "10").ok(), Some(10));
        assert_eq!(parse_seconds("t", "10s").ok(), Some(10));
        assert_eq!(parse_seconds("t", "1500ms").ok(), Some(2));
        assert_eq!(parse_seconds("t", "2m").ok(), Some(120));
        assert_eq!(parse_seconds("t", "1h").ok(), Some(3600));
        for bad in ["", "s", "-1", "1.5", "10x", "99999999999999999999h"] {
            assert!(parse_seconds("t", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pairs_keep_repeats_and_decode() {
        let pairs = query_pairs(Some(
            "cmd=python3&cmd=-c&cmd=print(1%2B1)&env=A%3Db&dir=%2Froot",
        ));
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("cmd", "python3"),
                ("cmd", "-c"),
                ("cmd", "print(1+1)"),
                ("env", "A=b"),
                ("dir", "/root"),
            ]
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_767_609_000), "2026-01-05T10:30:00Z");
    }
}
