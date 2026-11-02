//! Values of provided secrets (RFC 0013). The netd of a running sandbox
//! asks on its asks socket; serve checks the name against the config of the
//! sandbox and asks the provider on its unix socket, one request each.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use shuru_node::control::{self, Answer, Ask};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::Semaphore;

use super::sandboxes::Entry;

/// Under the 30 seconds netd waits, so netd hears why.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_ANSWER: usize = 64 * 1024;
const MAX_VALUE: usize = 32 * 1024;
/// Asks of one run answered at once; the rest wait for one to finish.
const IN_FLIGHT: usize = 8;
/// Of an error message kept for the log.
const MAX_MESSAGE: usize = 200;

pub struct Provider {
    socket: PathBuf,
}

/// What the provider answered for one secret.
enum Provided {
    Value {
        value: String,
        expires_in: Option<Duration>,
    },
    /// This sandbox may not have the secret.
    Refused(String),
    /// Anything else, which may pass.
    Failed(String),
}

#[derive(Deserialize)]
struct Value {
    value: String,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct Error {
    message: String,
}

impl Provider {
    pub fn new(socket: PathBuf) -> Provider {
        Provider { socket }
    }

    async fn provide(&self, sandbox: &str, secret: &str) -> Provided {
        match tokio::time::timeout(PROVIDER_TIMEOUT, self.request(sandbox, secret)).await {
            Ok(provided) => provided,
            Err(_) => Provided::Failed(format!(
                "the provider did not answer in {} s",
                PROVIDER_TIMEOUT.as_secs()
            )),
        }
    }

    async fn request(&self, sandbox: &str, secret: &str) -> Provided {
        let stream = match tokio::net::UnixStream::connect(&self.socket).await {
            Ok(stream) => stream,
            Err(e) => {
                return Provided::Failed(format!(
                    "connecting to the provider at {}: {e}",
                    self.socket.display()
                ))
            }
        };
        let (mut sender, connection) = match http1::handshake(TokioIo::new(stream)).await {
            Ok(handshake) => handshake,
            Err(e) => return Provided::Failed(format!("the provider: {e}")),
        };
        tokio::spawn(connection);
        let body = json!({ "sandbox": sandbox, "secret": secret }).to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/secrets")
            .header(HOST, "localhost")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .expect("a valid request");
        let response = match sender.send_request(request).await {
            Ok(response) => response,
            Err(e) => return Provided::Failed(format!("the provider: {e}")),
        };
        let status = response.status();
        let body = match Limited::new(response.into_body(), MAX_ANSWER)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(e) => return Provided::Failed(format!("reading the provider answer: {e}")),
        };
        answer(status, &body, OffsetDateTime::now_utc())
    }
}

/// The provided value of an answer, checked, or why there is none.
fn answer(status: StatusCode, body: &[u8], now: OffsetDateTime) -> Provided {
    if status != StatusCode::OK {
        let message = serde_json::from_slice::<Error>(body)
            .map(|e| printable(&e.message))
            .unwrap_or_default();
        let said = format!("the provider answered {}: {message}", status.as_u16());
        return match status {
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Provided::Refused(said),
            _ => Provided::Failed(said),
        };
    }
    // Only where it failed: the message serde gives can quote the value.
    let Ok(Value { value, expires_at }) = serde_json::from_slice::<Value>(body) else {
        return Provided::Failed("the provider answered 200 without a value".into());
    };
    if value.is_empty() || value.len() > MAX_VALUE || value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Provided::Failed(format!(
            "the provider gave a value that is empty, over {} KiB or holds control characters",
            MAX_VALUE / 1024
        ));
    }
    let expires_in = match expires_at {
        None => None,
        Some(at) => match OffsetDateTime::parse(&at, &Rfc3339) {
            Ok(at) if at > now => Some((at - now).unsigned_abs()),
            Ok(_) => return Provided::Failed("the provider gave a value that has expired".into()),
            Err(_) => {
                return Provided::Failed(format!(
                    "the provider gave expires_at {}, not an RFC 3339 time",
                    printable(&at)
                ))
            }
        },
    };
    Provided::Value { value, expires_in }
}

fn printable(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_MESSAGE)
        .collect()
}

/// Answers the asks of one run of `entry` until its netd closes its end.
/// Must be called on the runtime, which the answers are fetched on.
pub fn answer_asks(
    provider: Arc<Provider>,
    asks: UnixStream,
    entry: Arc<Entry>,
) -> std::io::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    let writer = Arc::new(Mutex::new(asks.try_clone()?));
    let name = entry.record.name.clone();
    std::thread::Builder::new()
        .name(format!("asks-{name}"))
        .spawn(move || {
            let in_flight = Arc::new(Semaphore::new(IN_FLIGHT));
            while let Ok(Some((ask, _))) = control::receive::<Ask>(&asks, 0) {
                let Ok(permit) = runtime.block_on(in_flight.clone().acquire_owned()) else {
                    break;
                };
                let provider = provider.clone();
                let entry = entry.clone();
                let writer = writer.clone();
                runtime.spawn(async move {
                    let _permit = permit;
                    let answer = ask_for(&provider, &entry, ask).await;
                    let _ = tokio::task::spawn_blocking(move || {
                        let writer = writer.lock().unwrap_or_else(|e| e.into_inner());
                        control::send(&writer, &answer, &[])
                    })
                    .await;
                });
            }
        })?;
    Ok(())
}

async fn ask_for(provider: &Provider, entry: &Entry, ask: Ask) -> Answer {
    let name = &entry.record.name;
    let provided = entry
        .config()
        .secrets
        .get(&ask.secret)
        .is_some_and(|secret| secret.provider);
    let provided = match provided {
        true => provider.provide(name, &ask.secret).await,
        false => Provided::Refused(format!(
            "{} is not a provided secret of the sandbox",
            printable(&ask.secret)
        )),
    };
    let (error, refused) = match provided {
        Provided::Value { value, expires_in } => {
            return Answer {
                ask: ask.ask,
                value: Some(value),
                expires_in_ms: expires_in.map(|d| d.as_millis() as u64),
                error: None,
                refused: false,
            }
        }
        Provided::Refused(error) => (error, true),
        Provided::Failed(error) => (error, false),
    };
    eprintln!(
        "shuru-node: secret {} of sandbox {name}: {error}",
        printable(&ask.secret)
    );
    Answer {
        ask: ask.ask,
        value: None,
        expires_in_ms: None,
        error: Some(error),
        refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> OffsetDateTime {
        OffsetDateTime::parse(rfc3339, &Rfc3339).unwrap()
    }

    #[test]
    fn a_value_and_when_it_expires() {
        let now = at("2026-11-02T18:00:00Z");
        let body = br#"{"value":"ghs_x","expires_at":"2026-11-02T19:00:00Z"}"#;
        let Provided::Value { value, expires_in } = answer(StatusCode::OK, body, now) else {
            panic!("no value");
        };
        assert_eq!(value, "ghs_x");
        assert_eq!(expires_in, Some(Duration::from_secs(3600)));

        let Provided::Value { expires_in, .. } = answer(StatusCode::OK, br#"{"value":"v"}"#, now)
        else {
            panic!("no value");
        };
        assert_eq!(expires_in, None);
    }

    #[test]
    fn refusals_and_failures() {
        let now = at("2026-11-02T18:00:00Z");
        for status in [StatusCode::FORBIDDEN, StatusCode::NOT_FOUND] {
            let Provided::Refused(said) = answer(status, br#"{"message":"not connected"}"#, now)
            else {
                panic!("{status} is not a refusal");
            };
            assert_eq!(
                said,
                format!("the provider answered {}: not connected", status.as_u16())
            );
        }
        let failures: [(StatusCode, &[u8]); 7] = [
            (StatusCode::SERVICE_UNAVAILABLE, br#"{"message":"busy"}"#),
            (StatusCode::UNAUTHORIZED, b"no"),
            (StatusCode::OK, b"s3cret"),
            (StatusCode::OK, br#"{"value":""}"#),
            (StatusCode::OK, br#"{"value":"s3cret\r\nX: 1"}"#),
            (
                StatusCode::OK,
                br#"{"value":"s3cret","expires_at":"2026-11-02T17:00:00Z"}"#,
            ),
            (
                StatusCode::OK,
                br#"{"value":"s3cret","expires_at":"tomorrow"}"#,
            ),
        ];
        for (status, body) in failures {
            let Provided::Failed(said) = answer(status, body, now) else {
                panic!(
                    "{status} {} is not a failure",
                    String::from_utf8_lossy(body)
                );
            };
            assert!(!said.contains("s3cret"), "{said}");
        }
    }

    #[test]
    fn a_message_is_kept_short_and_on_one_line() {
        let long = format!("{}\nX: 1{}", "a".repeat(10), "b".repeat(500));
        let message = printable(&long);
        assert!(!message.contains('\n'));
        assert_eq!(message.chars().count(), MAX_MESSAGE);
    }
}
