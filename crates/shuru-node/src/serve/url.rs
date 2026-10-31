//! `url`: HTTP to port 8080 in the guest, which a sandbox URL serves. Each
//! request takes its own connection to the guest. An upgrade, such as the
//! WebSocket of a dev server, is relayed both ways once the server in the
//! guest takes it.

use std::io;
use std::sync::Arc;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::header::{HeaderValue, HOST, UPGRADE};
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;

use super::guest::Conn;
use super::http::{full, ApiError, Body};
use super::ports;
use super::sandboxes::{Entry, Node};

pub const PORT: u16 = 8080;
/// The host the URL was asked for, which the server in the guest sees as
/// `Host`.
const FORWARDED_HOST: &str = "x-forwarded-host";

/// `/v1/sandboxes/<name>/url<path>`: the sandbox and the path and query to
/// ask its server for, with every slash kept.
pub fn target<'a>(path: &'a str, query: Option<&str>) -> Option<(&'a str, String)> {
    let (name, rest) = path.strip_prefix("/v1/sandboxes/")?.split_once('/')?;
    let rest = rest.strip_prefix("url")?;
    if name.is_empty() || !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let path = if rest.is_empty() { "/" } else { rest };
    Some((
        name,
        match query {
            Some(query) => format!("{path}?{query}"),
            None => path.to_string(),
        },
    ))
}

pub async fn forward(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    mut request: Request<Incoming>,
    target: &str,
) -> Result<Response<Body>, ApiError> {
    let uri: Uri = target
        .parse()
        .map_err(|_| ApiError::bad_request(format!("{target} is not a path")))?;
    let addr = node.guest(entry).await?;
    let Conn { stream, permit } = ports::open(&addr, "localhost", PORT)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("port {PORT}: {}", e.message())))?;
    let permit = Arc::new(permit);
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| ApiError::bad_gateway(format!("port {PORT}: {e}")))?;
    let held = permit.clone();
    tokio::spawn(async move {
        let _permit = held;
        let _ = connection.with_upgrades().await;
    });

    let upgrade = request
        .headers()
        .contains_key(UPGRADE)
        .then(|| hyper::upgrade::on(&mut request));
    let (mut parts, body) = request.into_parts();
    parts.uri = uri;
    let host = parts
        .headers
        .remove(FORWARDED_HOST)
        .unwrap_or_else(|| HeaderValue::from_static("localhost"));
    parts.headers.insert(HOST, host);
    let mut response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(|e| ApiError::bad_gateway(format!("port {PORT}: {e}")))?;

    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Ok(response.map(|body| body.map_err(io::Error::other).boxed()));
    }
    let Some(client) = upgrade else {
        return Err(ApiError::bad_gateway(format!(
            "port {PORT} switched protocols unasked"
        )));
    };
    let server = hyper::upgrade::on(&mut response);
    let busy = entry.busy();
    tokio::spawn(async move {
        let _busy = busy;
        let _permit = permit;
        if let (Ok(client), Ok(server)) = tokio::join!(client, server) {
            let _ =
                tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(server))
                    .await;
        }
    });
    let (parts, _) = response.into_parts();
    Ok(Response::from_parts(parts, full("")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_keeps_the_path_and_query() {
        assert_eq!(
            target("/v1/sandboxes/web/url", None),
            Some(("web", "/".to_string()))
        );
        assert_eq!(
            target("/v1/sandboxes/web/url/", None),
            Some(("web", "/".to_string()))
        );
        assert_eq!(
            target("/v1/sandboxes/web/url/a//b/", Some("x=1")),
            Some(("web", "/a//b/?x=1".to_string()))
        );
    }

    #[test]
    fn other_paths_are_not_targets() {
        assert_eq!(target("/v1/sandboxes/web/urls", None), None);
        assert_eq!(target("/v1/sandboxes/web/exec", None), None);
        assert_eq!(target("/v1/sandboxes//url", None), None);
        assert_eq!(target("/v1/sandboxes/web", None), None);
    }
}
