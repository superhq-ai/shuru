//! Exec over HTTP and WebSocket, the session list and kill, each mapped
//! onto the guest session frames (RFC 0012). Every exec is a session; a
//! client that goes away only detaches from it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use serde_json::{json, Value};
use shuru_proto::{
    frame, ExecRequest, KillEvent, SessionInfo, SessionKill, SessionList, SessionStart,
};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use super::http::{
    json_response, parse_bool, parse_number, parse_seconds, query_pairs, read_body, streaming,
    timestamp, ApiError, Body,
};
use super::hub::{Attachment, Event, Input};
use super::sandboxes::{Entry, Node};
use super::{guest, ws};

const MAX_STDIN: usize = 10 << 20;
const MAX_MESSAGE: usize = 1 << 20;
/// How long a client has to answer the close after the exit.
const CLOSE_WAIT: Duration = Duration::from_secs(5);
const DEFAULT_COMMAND: &str = "bash";

const STREAM_STDIN: u8 = 0;
const STREAM_STDOUT: u8 = 1;
const STREAM_STDERR: u8 = 2;
const STREAM_EXIT: u8 = 3;
const STREAM_STDIN_EOF: u8 = 4;

/// The session a query asks for, and whether the HTTP form sends its body
/// as stdin. Only the WebSocket form takes a terminal.
fn start_request(query: Option<&str>, websocket: bool) -> Result<(SessionStart, bool), ApiError> {
    let mut argv = Vec::new();
    let mut path = None;
    let mut env = HashMap::new();
    let mut cwd = None;
    let mut stdin = false;
    let mut tty = false;
    let mut rows = None;
    let mut cols = None;
    let mut max_run_after_disconnect = None;
    for (key, value) in query_pairs(query) {
        match key.as_str() {
            "cmd" => argv.push(value),
            "path" if !value.is_empty() => path = Some(value),
            "env" => {
                let (name, value) = value
                    .split_once('=')
                    .filter(|(name, _)| !name.is_empty())
                    .ok_or_else(|| ApiError::bad_request("env: expected KEY=VALUE"))?;
                env.insert(name.to_string(), value.to_string());
            }
            "dir" if !value.is_empty() => cwd = Some(value),
            "stdin" => stdin = parse_bool(&key, &value)?,
            "tty" if websocket => tty = parse_bool(&key, &value)?,
            "rows" if websocket => rows = Some(parse_number(&key, &value)?),
            "cols" if websocket => cols = Some(parse_number(&key, &value)?),
            "max_run_after_disconnect" if websocket => {
                max_run_after_disconnect = Some(parse_seconds(&key, &value)?)
            }
            _ => {}
        }
    }
    match (path, argv.first_mut()) {
        (Some(path), Some(first)) => *first = path,
        (Some(path), None) => argv.push(path),
        (None, Some(_)) => {}
        (None, None) => argv.push(DEFAULT_COMMAND.into()),
    }
    let start = SessionStart {
        exec: ExecRequest {
            argv,
            env,
            tty: tty.then_some(true),
            rows,
            cols,
            cwd,
            id: None,
        },
        max_run_after_disconnect,
    };
    Ok((start, stdin))
}

fn tagged(stream: u8, data: &[u8]) -> Bytes {
    let mut tagged = Vec::with_capacity(1 + data.len());
    tagged.push(stream);
    tagged.extend_from_slice(data);
    tagged.into()
}

fn stream_byte(kind: u8) -> u8 {
    if kind == frame::STDERR {
        STREAM_STDERR
    } else {
        STREAM_STDOUT
    }
}

/// HTTP exec: stdin from the body, then the response streams one chunk per
/// frame, a stream byte and the data, the exit status last.
pub async fn http(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let (start, stdin) = start_request(request.uri().query(), false)?;
    let input = if stdin {
        read_body(request.into_body(), MAX_STDIN).await?
    } else {
        Bytes::new()
    };
    let addr = node.guest(entry).await?;
    let mut attachment = addr.hubs.start(&addr, &start).await?;
    let (tx, rx) = mpsc::channel(16);
    let feed = feed(attachment.input(), input);
    tokio::spawn(async move {
        let output = async move {
            loop {
                let event = tokio::select! {
                    event = attachment.recv() => event,
                    () = tx.closed() => return,
                };
                let chunk = match event {
                    Some(Event::Output(kind, data)) => tagged(stream_byte(kind), &data),
                    Some(Event::Exit(code)) => {
                        let _ = tx.send(Ok(tagged(STREAM_EXIT, &[code]))).await;
                        return;
                    }
                    Some(Event::Port(_)) => continue,
                    Some(Event::Error(_) | Event::Behind(_) | Event::Gone) | None => return,
                };
                if tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
        };
        tokio::select! {
            () = output => {}
            () = feed => {}
        }
    });
    Ok(streaming("application/octet-stream", rx))
}

/// Sends the input and its end, then waits for the output to finish.
async fn feed(input: Input, data: Bytes) {
    if input.write(&data).await.is_ok() {
        let _ = input.send(frame::STDIN_EOF, &[]).await;
    }
    std::future::pending::<()>().await;
}

/// WebSocket exec, a new session or an attachment to `session`.
pub async fn websocket(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    mut request: Request<Incoming>,
    session: Option<String>,
) -> Result<Response<Body>, ApiError> {
    let key = ws::key(&request)?;
    let query = request.uri().query();
    let attachment = match session {
        None => {
            let (start, _) = start_request(query, true)?;
            let addr = node.guest(entry).await?;
            addr.hubs.start(&addr, &start).await?
        }
        Some(id) => {
            let mut output_offset = None;
            for (key, value) in query_pairs(query) {
                if key == "output_offset" {
                    output_offset = Some(parse_number::<u64>(&key, &value)?);
                }
            }
            let missing = || ApiError::not_found(format!("no session {id}"));
            // The guest reads ids as numbers; one written another way
            // would name the same session under a second key.
            let number = id
                .parse::<u64>()
                .ok()
                .filter(|n| n.to_string() == id)
                .ok_or_else(missing)?;
            let addr = entry.guest_if_running().await.ok_or_else(missing)?;
            addr.hubs.attach(&addr, number, output_offset).await?
        }
    };

    ws::upgrade(&mut request, &key, MAX_MESSAGE, move |socket| {
        bridge(socket, attachment)
    })
}

/// Carries a session between a WebSocket and its hub. Binary messages
/// carry a stream byte, or raw terminal bytes in a terminal session; text
/// messages carry JSON.
async fn bridge(ws: ws::Socket, mut attachment: Attachment) {
    let info = attachment.info.clone();
    let tty = info.tty;
    let (mut sink, mut stream) = ws.split();
    let mut hello = vec![info_message(&info)];
    if let Some(bytes) = attachment.gap {
        hello.push(json!({
            "type": "gap",
            "bytes": bytes,
            "output_offset": info.output_offset,
        }));
    }
    for message in hello {
        if sink.send(Message::text(message.to_string())).await.is_err() {
            return;
        }
    }
    let to_guest = attachment.input();

    let output = async move {
        let close = loop {
            let message = match attachment.recv().await {
                Some(Event::Output(_, data)) if tty => Message::binary(data),
                Some(Event::Output(kind, data)) => {
                    Message::binary(tagged(stream_byte(kind), &data))
                }
                Some(Event::Port(event)) => match String::from_utf8(event.into()) {
                    Ok(text) => Message::text(text),
                    Err(_) => continue,
                },
                Some(Event::Exit(code)) => {
                    let exit = if tty {
                        Message::text(json!({ "type": "exit", "exit_code": code }).to_string())
                    } else {
                        Message::binary(tagged(STREAM_EXIT, &[code]))
                    };
                    if sink.send(exit).await.is_err() {
                        return;
                    }
                    break (CloseCode::Normal, "");
                }
                Some(Event::Error(message)) => {
                    let error = json!({ "type": "error", "message": message });
                    let _ = sink.send(Message::text(error.to_string())).await;
                    break (CloseCode::Error, "guest error");
                }
                Some(Event::Behind(offset)) => {
                    let behind = json!({ "type": "behind", "output_offset": offset });
                    let _ = sink.send(Message::text(behind.to_string())).await;
                    break (CloseCode::Again, "too far behind");
                }
                Some(Event::Gone) | None => {
                    break (CloseCode::Error, "the guest closed the connection")
                }
            };
            if sink.send(message).await.is_err() {
                return;
            }
        };
        drop(attachment);
        let (code, reason) = close;
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        let _ = sink.send(Message::Close(Some(frame))).await;
    };

    // Input the hub no longer takes, because the session ended or this
    // client fell behind, is dropped while the output side says why.
    let input = async move {
        let mut forwarding = true;
        while let Some(Ok(message)) = stream.next().await {
            if !forwarding && !matches!(message, Message::Close(_)) {
                continue;
            }
            let sent = match message {
                Message::Binary(data) if tty => to_guest.write(&data).await,
                Message::Binary(data) => match data.split_first() {
                    Some((&STREAM_STDIN, data)) => to_guest.write(data).await,
                    Some((&STREAM_STDIN_EOF, _)) => to_guest.send(frame::STDIN_EOF, &[]).await,
                    _ => Ok(()),
                },
                Message::Text(text) => control(&to_guest, text.as_str()).await,
                Message::Close(_) => break,
                _ => Ok(()),
            };
            forwarding = sent.is_ok();
        }
    };

    tokio::pin!(output, input);
    tokio::select! {
        () = &mut input => {}
        () = &mut output => {
            let _ = tokio::time::timeout(CLOSE_WAIT, &mut input).await;
        }
    }
}

/// A JSON message from the client: `resize` with `cols` and `rows`, or
/// `signal`, of which the guest connection carries SIGTERM.
async fn control(to_guest: &Input, text: &str) -> std::io::Result<()> {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return Ok(());
    };
    match message["type"].as_str() {
        Some("resize") => {
            let size = |key: &str| message[key].as_u64().and_then(|n| u16::try_from(n).ok());
            if let (Some(rows), Some(cols)) = (size("rows"), size("cols")) {
                let payload = frame::resize_payload(rows, cols);
                to_guest.send(frame::RESIZE, &payload).await?;
            }
        }
        Some("signal") => {
            if matches!(message["signal"].as_str(), Some("SIGTERM" | "TERM")) {
                to_guest.send(frame::KILL, &[]).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn info_message(info: &SessionInfo) -> Value {
    json!({
        "type": "session_info",
        "session_id": info.id,
        "command": info.command.join(" "),
        "created": timestamp(info.created),
        "cols": info.cols,
        "rows": info.rows,
        "is_owner": info.is_owner,
        "tty": info.tty,
        "output_offset": info.output_offset,
    })
}

fn session_json(info: &SessionInfo) -> Value {
    json!({
        "id": info.id,
        "command": info.command.join(" "),
        "workdir": info.workdir,
        "tty": info.tty,
        "created": timestamp(info.created),
        "is_active": info.is_active,
        "last_activity": timestamp(info.last_activity),
        "bytes_per_second": info.bytes_per_second,
    })
}

/// The sessions of a running sandbox. One that is not running has none.
pub async fn list(entry: &Entry) -> Result<Response<Body>, ApiError> {
    let sessions = match entry.guest_if_running().await {
        None => Vec::new(),
        Some(addr) => {
            let mut guest = guest::connect(&addr, &addr.calls).await?;
            guest::write_frame(&mut guest.stream, frame::SESSION_LIST, &[]).await?;
            match guest::read_frame(&mut guest.stream).await? {
                Some((frame::SESSION_LIST_RESP, payload)) => {
                    serde_json::from_slice::<SessionList>(&payload)
                        .map_err(|e| ApiError::internal(format!("the guest session list: {e}")))?
                        .sessions
                }
                Some((frame::ERROR, payload)) => return Err(guest::guest_error(&payload)),
                _ => return Err(ApiError::internal("the guest did not list its sessions")),
            }
        }
    };
    let body = json!({
        "sessions": sessions.iter().map(session_json).collect::<Vec<_>>(),
        "count": sessions.len(),
    });
    Ok(json_response(StatusCode::OK, &body))
}

/// Kill streams the guest events as NDJSON, the last `complete` or
/// `error`.
pub async fn kill(
    entry: &Entry,
    id: &str,
    query: Option<&str>,
) -> Result<Response<Body>, ApiError> {
    let mut signal = None;
    let mut timeout = None;
    for (key, value) in query_pairs(query) {
        match key.as_str() {
            "signal" if !value.is_empty() => signal = Some(value),
            "timeout" => timeout = Some(parse_seconds(&key, &value)?),
            _ => {}
        }
    }
    let addr = entry
        .guest_if_running()
        .await
        .ok_or_else(|| ApiError::not_found(format!("no session {id}")))?;
    let mut guest = guest::connect(&addr, &addr.calls).await?;
    let request = SessionKill {
        id: id.to_string(),
        signal,
        timeout,
    };
    guest::send_json(&mut guest.stream, frame::SESSION_KILL, &request).await?;
    let first = kill_event(&mut guest.stream).await?;
    if let Some(KillEvent::Error { message }) = &first.1 {
        if message.starts_with("no session ") {
            return Err(ApiError::not_found(message.clone()));
        }
    }
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        // The whole connection moves in, its permit with it.
        let mut guest = guest;
        let mut event = Ok(first);
        while let Ok((line, parsed)) = event {
            if tx.send(Ok(line)).await.is_err() {
                return;
            }
            if matches!(
                parsed,
                None | Some(KillEvent::Complete { .. } | KillEvent::Error { .. })
            ) {
                return;
            }
            event = tokio::select! {
                event = kill_event(&mut guest.stream) => event,
                () = tx.closed() => return,
            };
        }
    });
    Ok(streaming("application/x-ndjson", rx))
}

/// The next event as an NDJSON line, and parsed when it is one the guest
/// should send.
async fn kill_event(guest: &mut UnixStream) -> Result<(Bytes, Option<KillEvent>), ApiError> {
    match guest::read_frame(guest).await? {
        Some((frame::SESSION_KILL_EVENT, mut payload)) => {
            let parsed = serde_json::from_slice(&payload).ok();
            payload.push(b'\n');
            Ok((payload.into(), parsed))
        }
        Some((frame::ERROR, payload)) => Err(guest::guest_error(&payload)),
        Some((kind, _)) => Err(ApiError::internal(format!(
            "the guest answered the kill with frame {kind:#x}"
        ))),
        None => Err(ApiError::internal("the guest closed the connection")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(query: &str, websocket: bool) -> (SessionStart, bool) {
        start_request(Some(query), websocket).unwrap_or_else(|e| panic!("{query}: {e:?}"))
    }

    #[test]
    fn a_command_with_arguments() {
        let (start, stdin) = parse(
            "cmd=python3&cmd=-c&cmd=print(1)&dir=%2Froot&env=A%3D1&stdin=true",
            false,
        );
        assert_eq!(start.exec.argv, ["python3", "-c", "print(1)"]);
        assert_eq!(start.exec.cwd.as_deref(), Some("/root"));
        assert_eq!(start.exec.env.get("A").map(String::as_str), Some("1"));
        assert!(stdin);
        assert_eq!(start.exec.tty, None);
    }

    #[test]
    fn path_names_the_executable() {
        let (start, _) = parse("cmd=ls&cmd=-l&path=%2Fbin%2Fls", false);
        assert_eq!(start.exec.argv, ["/bin/ls", "-l"]);
        let (start, _) = parse("path=%2Fbin%2Fls", false);
        assert_eq!(start.exec.argv, ["/bin/ls"]);
        let (start, _) = parse("", true);
        assert_eq!(start.exec.argv, [DEFAULT_COMMAND]);
    }

    #[test]
    fn only_the_websocket_takes_a_terminal() {
        let (start, _) = parse(
            "cmd=sh&tty=true&rows=40&cols=120&max_run_after_disconnect=30s",
            false,
        );
        assert_eq!(start.exec.tty, None);
        assert_eq!(start.exec.rows, None);
        assert_eq!(start.max_run_after_disconnect, None);
        let (start, _) = parse(
            "cmd=sh&tty=true&rows=40&cols=120&max_run_after_disconnect=30s",
            true,
        );
        assert_eq!(start.exec.tty, Some(true));
        assert_eq!((start.exec.rows, start.exec.cols), (Some(40), Some(120)));
        assert_eq!(start.max_run_after_disconnect, Some(30));
    }

    #[test]
    fn refuses_bad_values() {
        for query in [
            "env=NOVALUE",
            "env=%3Dx",
            "stdin=maybe",
            "rows=-1",
            "rows=70000",
            "max_run_after_disconnect=soon",
        ] {
            assert!(start_request(Some(query), true).is_err(), "{query}");
        }
    }
}
