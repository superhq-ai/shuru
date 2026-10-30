//! WebSocket upgrades on the API socket.

use std::future::Future;

use hyper::body::{Bytes, Incoming};
use hyper::header::{
    HeaderName, HeaderValue, CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_VERSION, UPGRADE,
};
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use super::http::{full, ApiError, Body};

pub type Socket = WebSocketStream<TokioIo<Upgraded>>;

pub fn is_websocket(request: &Request<Incoming>) -> bool {
    let has = |name: HeaderName, token: &str| {
        request.headers().get_all(name).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case(token))
            })
        })
    };
    has(CONNECTION, "upgrade") && has(UPGRADE, "websocket")
}

/// The key of a WebSocket upgrade request this server takes.
pub fn key(request: &Request<Incoming>) -> Result<HeaderValue, ApiError> {
    if !is_websocket(request) {
        return Err(ApiError::bad_request("expected a WebSocket upgrade"));
    }
    if request
        .headers()
        .get(SEC_WEBSOCKET_VERSION)
        .is_none_or(|version| version != "13")
    {
        return Err(ApiError::bad_request("expected WebSocket version 13"));
    }
    request
        .headers()
        .get(SEC_WEBSOCKET_KEY)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("no Sec-WebSocket-Key"))
}

/// Answers the upgrade, then runs `serve` on the socket once the client
/// has it.
pub fn upgrade<F, Fut>(
    request: &mut Request<Incoming>,
    key: &HeaderValue,
    max_message: usize,
    serve: F,
) -> Result<Response<Body>, ApiError>
where
    F: FnOnce(Socket) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let accept = HeaderValue::from_str(&derive_accept_key(key.as_bytes()))
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let upgrade = hyper::upgrade::on(request);
    tokio::spawn(async move {
        let Ok(upgraded) = upgrade.await else {
            return;
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message))
            .max_frame_size(Some(max_message));
        let socket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config))
                .await;
        serve(socket).await;
    });

    let mut response = Response::new(full(Bytes::new()));
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(SEC_WEBSOCKET_ACCEPT, accept);
    Ok(response)
}
