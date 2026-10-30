//! The filesystem API: reads, writes, listings, deletes, renames, copies,
//! modes, owners and watches, each on the guest file frames. Paths and
//! shapes follow the sandbox API (RFC 0012); a guest error answers with
//! its errno as the code.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::body::{Body as _, Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use shuru_proto::{
    frame, watch_kind, ChmodRequest, ChownRequest, CopyRequest, DirEntry, FsOkResponse, Owner,
    ReadDirRequest, ReadDirResponse, ReadFileRequest, RemoveRequest, RenameRequest, StatRequest,
    StatResponse, WatchDetail, WatchRequest, WriteFileRequest, WriteFileResponse,
};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;

use super::guest::{self, Conn};
use super::http::{
    json_response, parse_bool, query_pairs, read_body, streaming, timestamp, ApiError, Body,
    MAX_JSON_BODY,
};
use super::sandboxes::{Entry, GuestAddr, Node};
use super::ws;

/// The largest file a write takes.
const MAX_WRITE: usize = 16 << 20;
/// Paths one watch socket follows, each on a guest connection of its own.
const MAX_WATCHES: usize = 8;
const MAX_WATCH_MESSAGE: usize = 64 * 1024;
/// Events a watch holds for a client that reads slowly, before it drops
/// them and says how many it dropped.
const WATCH_BACKLOG: usize = 256;

pub async fn route(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
    op: &str,
) -> Result<Response<Body>, ApiError> {
    let method = request.method().clone();
    match (&method, op) {
        (&Method::GET, "read") => read(node, entry, request).await,
        (&Method::PUT, "write") => write(node, entry, request).await,
        (&Method::GET, "list") => list(node, entry, request).await,
        (&Method::DELETE, "delete") => delete(node, entry, request).await,
        (&Method::POST, "rename") => rename(node, entry, request).await,
        (&Method::POST, "copy") => copy(node, entry, request).await,
        (&Method::POST, "chmod") => chmod(node, entry, request).await,
        (&Method::POST, "chown") => chown(node, entry, request).await,
        (&Method::GET, "watch") => watch(node, entry, request).await,
        (
            _,
            "read" | "write" | "list" | "delete" | "rename" | "copy" | "chmod" | "chown" | "watch",
        ) => Err(ApiError::not_allowed(&method, request.uri().path())),
        _ => Err(ApiError::not_found(format!(
            "no route {}",
            request.uri().path()
        ))),
    }
}

/// An absolute guest path: `path` when it is absolute, or else under
/// `working_dir`, which must be absolute and is `/` when left out.
fn resolve(path: &str, working_dir: Option<&str>) -> Result<String, ApiError> {
    if path.is_empty() {
        return Err(ApiError::bad_request("path: required"));
    }
    if path.contains('\0') {
        return Err(ApiError::bad_request("path: contains a NUL"));
    }
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    let dir = working_dir.filter(|dir| !dir.is_empty()).unwrap_or("/");
    if !dir.starts_with('/') {
        return Err(ApiError::bad_request(
            "workingDir: expected an absolute path",
        ));
    }
    Ok(format!("{}/{path}", dir.trim_end_matches('/')))
}

fn parse_mode(key: &str, value: &str) -> Result<u32, ApiError> {
    value
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| u32::from_str_radix(value, 8).ok())
        .flatten()
        .filter(|mode| *mode <= 0o7777)
        .ok_or_else(|| {
            ApiError::bad_request(format!("{key}: expected octal permissions, such as 0644"))
        })
}

fn mode_string(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

/// The errno a guest error message ends in.
fn errno(message: &str) -> Option<i32> {
    message
        .rsplit_once("(os error ")
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .and_then(|n| n.parse().ok())
}

/// Names of the errnos a file operation in the guest can end in.
const ERRNOS: &[(i32, &str)] = &[
    (libc::EPERM, "EPERM"),
    (libc::ENOENT, "ENOENT"),
    (libc::EIO, "EIO"),
    (libc::E2BIG, "E2BIG"),
    (libc::EBADF, "EBADF"),
    (libc::EAGAIN, "EAGAIN"),
    (libc::ENOMEM, "ENOMEM"),
    (libc::EACCES, "EACCES"),
    (libc::EBUSY, "EBUSY"),
    (libc::EEXIST, "EEXIST"),
    (libc::EXDEV, "EXDEV"),
    (libc::ENODEV, "ENODEV"),
    (libc::ENOTDIR, "ENOTDIR"),
    (libc::EISDIR, "EISDIR"),
    (libc::EINVAL, "EINVAL"),
    (libc::ENFILE, "ENFILE"),
    (libc::EMFILE, "EMFILE"),
    (libc::ETXTBSY, "ETXTBSY"),
    (libc::EFBIG, "EFBIG"),
    (libc::ENOSPC, "ENOSPC"),
    (libc::ESPIPE, "ESPIPE"),
    (libc::EROFS, "EROFS"),
    (libc::EMLINK, "EMLINK"),
    (libc::ENAMETOOLONG, "ENAMETOOLONG"),
    (libc::ENOTEMPTY, "ENOTEMPTY"),
    (libc::ELOOP, "ELOOP"),
    (libc::ENOBUFS, "ENOBUFS"),
    (libc::EDQUOT, "EDQUOT"),
];

/// A guest error, named by its errno, with a status that says whose
/// mistake it was.
fn fs_error(message: &str, path: &str) -> ApiError {
    let errno = errno(message).unwrap_or(libc::EIO);
    let code = ERRNOS
        .iter()
        .find(|(n, _)| *n == errno)
        .map_or("EIO", |(_, name)| name);
    let status = match errno {
        libc::ENOENT => StatusCode::NOT_FOUND,
        libc::EEXIST => StatusCode::CONFLICT,
        libc::EACCES | libc::EPERM | libc::EROFS => StatusCode::FORBIDDEN,
        libc::ENOTDIR
        | libc::EISDIR
        | libc::ENOTEMPTY
        | libc::EINVAL
        | libc::EXDEV
        | libc::ENAMETOOLONG
        | libc::ELOOP
        | libc::E2BIG => StatusCode::BAD_REQUEST,
        libc::EFBIG => StatusCode::PAYLOAD_TOO_LARGE,
        libc::ENOSPC | libc::EDQUOT => StatusCode::INSUFFICIENT_STORAGE,
        libc::EBUSY | libc::ETXTBSY => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError::fs(status, code, message, path)
}

/// What the query string of a file request carries.
#[derive(Default)]
struct Params {
    path: String,
    working_dir: Option<String>,
    recursive: bool,
    pattern: Option<String>,
    mode: Option<u32>,
    mkdir: Option<bool>,
}

impl Params {
    fn parse(query: Option<&str>) -> Result<Params, ApiError> {
        let mut params = Params::default();
        for (key, value) in query_pairs(query) {
            match key.as_str() {
                "path" => params.path = value,
                "workingDir" => params.working_dir = Some(value),
                "recursive" => params.recursive = parse_bool(&key, &value)?,
                "pattern" if !value.is_empty() => params.pattern = Some(value),
                "mode" => params.mode = Some(parse_mode(&key, &value)?),
                "mkdir" | "mkdirParents" => params.mkdir = Some(parse_bool(&key, &value)?),
                _ => {}
            }
        }
        Ok(params)
    }

    fn target(&self) -> Result<String, ApiError> {
        resolve(&self.path, self.working_dir.as_deref())
    }
}

fn json_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("the body: {e}")))
}

/// Sends a request on a new guest connection.
async fn call(
    addr: &GuestAddr,
    kind: u8,
    request: &impl serde::Serialize,
) -> Result<Conn, ApiError> {
    let mut conn = guest::connect(addr, &addr.calls).await?;
    guest::send_json(&mut conn.stream, kind, request).await?;
    Ok(conn)
}

/// The next frame, of kind `want`, or the guest error about `path`.
async fn answer(conn: &mut Conn, want: u8, path: &str) -> Result<Vec<u8>, ApiError> {
    match guest::read_frame(&mut conn.stream).await? {
        Some((kind, payload)) if kind == want => Ok(payload),
        Some((frame::ERROR, payload)) => Err(fs_error(&String::from_utf8_lossy(&payload), path)),
        Some((kind, _)) => Err(ApiError::internal(format!(
            "the guest answered with frame {kind:#x}"
        ))),
        None => Err(ApiError::internal("the guest closed the connection")),
    }
}

async fn parsed<T: DeserializeOwned>(conn: &mut Conn, want: u8, path: &str) -> Result<T, ApiError> {
    let payload = answer(conn, want, path).await?;
    serde_json::from_slice(&payload)
        .map_err(|e| ApiError::internal(format!("the guest answer: {e}")))
}

async fn fs_ok(conn: &mut Conn, path: &str) -> Result<FsOkResponse, ApiError> {
    parsed(conn, frame::FS_OK_RESP, path).await
}

async fn read(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let path = Params::parse(request.uri().query())?.target()?;
    let addr = node.guest(entry).await?;
    let read = ReadFileRequest { path: path.clone() };
    let mut conn = call(&addr, frame::READ_FILE_REQ, &read).await?;
    let first = answer(&mut conn, frame::READ_FILE_RESP, &path).await?;
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        // The whole connection moves in, its permit with it.
        let mut conn = conn;
        let mut chunk = first;
        while !chunk.is_empty() {
            if tx.send(Ok(chunk.into())).await.is_err() {
                return;
            }
            let next = tokio::select! {
                next = guest::read_frame(&mut conn.stream) => next,
                () = tx.closed() => return,
            };
            chunk = match next {
                Ok(Some((frame::READ_FILE_RESP, chunk))) => chunk,
                Ok(Some((frame::ERROR, message))) => {
                    let message = String::from_utf8_lossy(&message).into_owned();
                    let _ = tx.send(Err(io::Error::other(message))).await;
                    return;
                }
                _ => {
                    let _ = tx
                        .send(Err(io::Error::other("the guest cut the read")))
                        .await;
                    return;
                }
            };
        }
    });
    Ok(streaming("application/octet-stream", rx))
}

/// A write body, streamed when its length is known up front.
enum Upload {
    Streamed(Incoming, u64),
    Buffered(Bytes),
}

async fn write(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let params = Params::parse(request.uri().query())?;
    let path = params.target()?;
    let body = request.into_body();
    let upload = match body.size_hint().exact() {
        Some(len) if len > MAX_WRITE as u64 => {
            return Err(ApiError::too_large(format!(
                "the body is over {MAX_WRITE} bytes"
            )))
        }
        Some(len) => Upload::Streamed(body, len),
        None => Upload::Buffered(read_body(body, MAX_WRITE).await?),
    };
    let len = match &upload {
        Upload::Streamed(_, len) => *len,
        Upload::Buffered(bytes) => bytes.len() as u64,
    };
    let addr = node.guest(entry).await?;
    let write = WriteFileRequest {
        path: path.clone(),
        len,
        mode: params.mode,
        mkdir: params.mkdir,
    };
    let mut conn = call(&addr, frame::WRITE_FILE_REQ, &write).await?;
    match upload {
        Upload::Buffered(bytes) => send_data(&mut conn, &bytes).await?,
        Upload::Streamed(mut body, _) => {
            while let Some(part) = body.frame().await {
                let part =
                    part.map_err(|e| ApiError::bad_request(format!("reading the body: {e}")))?;
                if let Ok(data) = part.into_data() {
                    send_data(&mut conn, &data).await?;
                }
            }
        }
    }
    let written: WriteFileResponse = parsed(&mut conn, frame::WRITE_FILE_RESP, &path).await?;
    if !written.ok {
        let message = written.error.unwrap_or_else(|| "the write failed".into());
        return Err(fs_error(&message, &path));
    }
    let body = json!({
        "path": path,
        "size": written.size,
        "mode": written.mode.map(mode_string),
    });
    Ok(json_response(StatusCode::OK, &body))
}

async fn send_data(conn: &mut Conn, data: &[u8]) -> Result<(), ApiError> {
    for chunk in data.chunks(frame::FILE_CHUNK) {
        guest::write_frame(&mut conn.stream, frame::WRITE_FILE_DATA, chunk).await?;
    }
    Ok(())
}

fn entry_json(entry: &DirEntry, path: &str) -> Value {
    let kind = match entry.entry_type.as_str() {
        "dir" => "directory",
        "symlink" => "symlink",
        _ => "file",
    };
    json!({
        "name": entry.name,
        "path": path,
        "type": kind,
        "size": entry.size,
        "mode": mode_string(entry.mode.unwrap_or(0)),
        "modTime": timestamp(entry.mtime.unwrap_or(0)),
        "isDir": kind == "directory",
    })
}

fn child_path(root: &str, name: &str) -> String {
    format!("{}/{name}", root.trim_end_matches('/'))
}

fn basename(path: &str) -> &str {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((_, "")) | None => path,
        Some((_, name)) => name,
    }
}

/// A listing of a directory, or of a file as its one entry. A long one
/// streams as the guest sends it.
async fn list(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let params = Params::parse(request.uri().query())?;
    let path = params.target()?;
    let addr = node.guest(entry).await?;
    let stat = StatRequest { path: path.clone() };
    let mut conn = call(&addr, frame::STAT_REQ, &stat).await?;
    let stat: StatResponse = parsed(&mut conn, frame::STAT_RESP, &path).await?;
    let itself = DirEntry {
        name: basename(&path).to_string(),
        entry_type: if stat.is_symlink {
            "symlink"
        } else if stat.is_dir {
            "dir"
        } else {
            "file"
        }
        .to_string(),
        size: stat.size,
        mode: Some(stat.mode),
        mtime: Some(stat.mtime),
    };
    let single = || {
        let body = json!({ "path": path, "entries": [entry_json(&itself, &path)], "count": 1 });
        Ok(json_response(StatusCode::OK, &body))
    };
    if !stat.is_dir && !stat.is_symlink {
        return single();
    }
    let read_dir = ReadDirRequest {
        path: path.clone(),
        recursive: params.recursive,
        pattern: params.pattern,
        stream: true,
    };
    guest::send_json(&mut conn.stream, frame::READ_DIR_REQ, &read_dir).await?;
    // A link lists as its directory, or as itself when it points at none.
    let first: ReadDirResponse = match parsed(&mut conn, frame::READ_DIR_RESP, &path).await {
        Err(e)
            if stat.is_symlink
                && matches!(errno(e.message()), Some(libc::ENOTDIR | libc::ENOENT)) =>
        {
            return single()
        }
        first => first?,
    };

    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut conn = conn;
        let mut out = format!("{{\"path\":{},\"entries\":[", json!(path));
        let mut count = 0u64;
        let mut page = first;
        loop {
            for entry in &page.entries {
                if count > 0 {
                    out.push(',');
                }
                out.push_str(&entry_json(entry, &child_path(&path, &entry.name)).to_string());
                count += 1;
            }
            if !page.more {
                break;
            }
            if tx.send(Ok(std::mem::take(&mut out).into())).await.is_err() {
                return;
            }
            let next = tokio::select! {
                next = guest::read_frame(&mut conn.stream) => next,
                () = tx.closed() => return,
            };
            page = match next {
                Ok(Some((frame::READ_DIR_RESP, payload))) => match serde_json::from_slice(&payload)
                {
                    Ok(page) => page,
                    Err(e) => {
                        let _ = tx.send(Err(io::Error::other(e))).await;
                        return;
                    }
                },
                _ => {
                    let _ = tx
                        .send(Err(io::Error::other("the guest cut the listing")))
                        .await;
                    return;
                }
            };
        }
        out.push_str(&format!("],\"count\":{count}}}"));
        let _ = tx.send(Ok(out.into())).await;
    });
    Ok(streaming("application/json", rx))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteBody {
    path: String,
    working_dir: Option<String>,
    recursive: Option<bool>,
}

/// A delete names its path in the query, or in a JSON body.
async fn delete(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let params = Params::parse(request.uri().query())?;
    let (path, recursive) = if params.path.is_empty() {
        let body: DeleteBody = json_body(&read_body(request.into_body(), MAX_JSON_BODY).await?)?;
        let path = resolve(&body.path, body.working_dir.as_deref())?;
        (path, body.recursive.unwrap_or(false))
    } else {
        (params.target()?, params.recursive)
    };
    let addr = node.guest(entry).await?;
    let remove = RemoveRequest {
        path: path.clone(),
        recursive,
    };
    fs_ok(&mut call(&addr, frame::REMOVE_REQ, &remove).await?, &path).await?;
    Ok(json_response(
        StatusCode::OK,
        &json!({ "deleted": [path], "count": 1 }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveBody {
    source: String,
    dest: String,
    working_dir: Option<String>,
    recursive: Option<bool>,
    preserve_attrs: Option<bool>,
}

impl MoveBody {
    async fn read(request: Request<Incoming>) -> Result<(MoveBody, String, String), ApiError> {
        let body: MoveBody = json_body(&read_body(request.into_body(), MAX_JSON_BODY).await?)?;
        let source = resolve(&body.source, body.working_dir.as_deref())?;
        let dest = resolve(&body.dest, body.working_dir.as_deref())?;
        Ok((body, source, dest))
    }
}

async fn rename(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let (_, source, dest) = MoveBody::read(request).await?;
    let addr = node.guest(entry).await?;
    let rename = RenameRequest {
        old_path: source.clone(),
        new_path: dest.clone(),
    };
    fs_ok(&mut call(&addr, frame::RENAME_REQ, &rename).await?, &source).await?;
    Ok(json_response(
        StatusCode::OK,
        &json!({ "source": source, "dest": dest }),
    ))
}

async fn copy(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let (body, source, dest) = MoveBody::read(request).await?;
    let addr = node.guest(entry).await?;
    let copy = CopyRequest {
        src: source.clone(),
        dst: dest.clone(),
        recursive: body.recursive.unwrap_or(false),
        preserve_attrs: body.preserve_attrs.unwrap_or(false),
    };
    let done = fs_ok(&mut call(&addr, frame::COPY_REQ, &copy).await?, &source).await?;
    let body = json!({
        "source": source,
        "dest": dest,
        "copied": [{ "source": source, "dest": dest }],
        "count": done.count.unwrap_or(0),
        "totalBytes": done.total_bytes.unwrap_or(0),
    });
    Ok(json_response(StatusCode::OK, &body))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChmodBody {
    path: String,
    working_dir: Option<String>,
    mode: String,
    recursive: Option<bool>,
}

async fn chmod(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let body: ChmodBody = json_body(&read_body(request.into_body(), MAX_JSON_BODY).await?)?;
    let path = resolve(&body.path, body.working_dir.as_deref())?;
    let mode = parse_mode("mode", &body.mode)?;
    let addr = node.guest(entry).await?;
    let chmod = ChmodRequest {
        path: path.clone(),
        mode,
        recursive: body.recursive.unwrap_or(false),
    };
    let done = fs_ok(&mut call(&addr, frame::CHMOD_REQ, &chmod).await?, &path).await?;
    let body = json!({
        "path": path,
        "mode": mode_string(mode),
        "count": done.count.unwrap_or(0),
    });
    Ok(json_response(StatusCode::OK, &body))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChownBody {
    path: String,
    working_dir: Option<String>,
    uid: Option<Owner>,
    gid: Option<Owner>,
    recursive: Option<bool>,
}

async fn chown(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let body: ChownBody = json_body(&read_body(request.into_body(), MAX_JSON_BODY).await?)?;
    let path = resolve(&body.path, body.working_dir.as_deref())?;
    if body.uid.is_none() && body.gid.is_none() {
        return Err(ApiError::bad_request("uid or gid: required"));
    }
    let addr = node.guest(entry).await?;
    let chown = ChownRequest {
        path: path.clone(),
        uid: body.uid,
        gid: body.gid,
        recursive: body.recursive.unwrap_or(false),
    };
    let done = fs_ok(&mut call(&addr, frame::CHOWN_REQ, &chown).await?, &path).await?;
    let body = json!({
        "path": path,
        "affected": [{ "path": path, "uid": done.uid, "gid": done.gid }],
        "count": done.count.unwrap_or(0),
    });
    Ok(json_response(StatusCode::OK, &body))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscribe {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    paths: Vec<String>,
    recursive: Option<bool>,
    working_dir: Option<String>,
}

/// A WebSocket that follows the paths a client subscribes to, each with a
/// watch of its own in the guest.
async fn watch(
    node: &Arc<Node>,
    entry: &Arc<Entry>,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, ApiError> {
    let key = ws::key(&request)?;
    let addr = node.guest(entry).await?;
    ws::upgrade(&mut request, &key, MAX_WATCH_MESSAGE, move |socket| {
        follow(socket, addr)
    })
}

async fn follow(socket: ws::Socket, addr: GuestAddr) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut events) = mpsc::channel(WATCH_BACKLOG);
    let feed = Feed {
        tx,
        dropped: Arc::new(AtomicU64::new(0)),
        lost: Arc::new(Notify::new()),
    };
    // Dropping the set ends the watches and their guest connections.
    let mut watches = JoinSet::new();
    loop {
        let replies = tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    subscribe(&addr, text.as_str(), &mut watches, &feed).await
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
            Some(event) = events.recv() => vec![event],
            () = feed.lost.notified() => match feed.dropped.swap(0, Ordering::Relaxed) {
                0 => continue,
                n => {
                    let message = format!("{n} events dropped while the client was behind");
                    vec![json!({ "type": "error", "message": message })]
                }
            },
        };
        for reply in replies {
            if sink.send(Message::text(reply.to_string())).await.is_err() {
                return;
            }
        }
    }
}

fn watch_error(message: &str, path: &str) -> Value {
    json!({ "type": "error", "message": message, "path": path })
}

/// Where the watches of one socket send their events, and the count of
/// those dropped because the client was behind, with a wakeup for it.
/// Dropping keeps the guest from ever waiting on a slow client.
#[derive(Clone)]
struct Feed {
    tx: mpsc::Sender<Value>,
    dropped: Arc<AtomicU64>,
    lost: Arc<Notify>,
}

async fn subscribe(
    addr: &GuestAddr,
    text: &str,
    watches: &mut JoinSet<()>,
    feed: &Feed,
) -> Vec<Value> {
    let request = match serde_json::from_str::<Subscribe>(text) {
        Ok(request) if request.kind == "subscribe" => request,
        _ => {
            let message = "expected {\"type\":\"subscribe\",\"paths\":[...]}";
            return vec![json!({ "type": "error", "message": message })];
        }
    };
    let recursive = request.recursive.unwrap_or(false);
    let mut replies = Vec::new();
    let mut subscribed = Vec::new();
    for path in &request.paths {
        let path = match resolve(path, request.working_dir.as_deref()) {
            Ok(path) => path,
            Err(e) => {
                replies.push(watch_error(e.message(), path));
                continue;
            }
        };
        while watches.try_join_next().is_some() {}
        if watches.len() >= MAX_WATCHES {
            let message = format!("a socket follows at most {MAX_WATCHES} paths");
            replies.push(watch_error(&message, &path));
            continue;
        }
        match open_watch(addr, &path, recursive).await {
            Ok(conn) => {
                watches.spawn(forward(conn, path.clone(), feed.clone()));
                subscribed.push(path);
            }
            Err(e) => replies.push(watch_error(e.message(), &path)),
        }
    }
    if !subscribed.is_empty() {
        replies.insert(0, json!({ "type": "subscribed", "paths": subscribed }));
    }
    replies
}

async fn open_watch(addr: &GuestAddr, path: &str, recursive: bool) -> Result<Conn, ApiError> {
    let mut conn = guest::connect(addr, &addr.streams).await?;
    let request = WatchRequest {
        path: path.to_string(),
        recursive,
        detail: true,
    };
    guest::send_json(&mut conn.stream, frame::WATCH_REQ, &request).await?;
    fs_ok(&mut conn, path).await?;
    Ok(conn)
}

async fn forward(mut conn: Conn, path: String, feed: Feed) {
    loop {
        let event = match guest::read_frame(&mut conn.stream).await {
            Ok(Some((frame::WATCH_DETAIL, payload))) => {
                match serde_json::from_slice::<WatchDetail>(&payload) {
                    Ok(detail) => event_json(&detail),
                    Err(_) => continue,
                }
            }
            Ok(Some((frame::ERROR, payload))) => {
                watch_error(&String::from_utf8_lossy(&payload), &path)
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => {
                let _ = feed.tx.send(watch_error("the watch ended", &path)).await;
                return;
            }
        };
        match feed.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                feed.dropped.fetch_add(1, Ordering::Relaxed);
                feed.lost.notify_one();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

fn event_json(detail: &WatchDetail) -> Value {
    let event = match detail.kind {
        watch_kind::CREATE => "create",
        watch_kind::DELETE => "remove",
        watch_kind::RENAME => "rename",
        watch_kind::CHMOD => "chmod",
        _ => "write",
    };
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let mut value = json!({
        "type": "event",
        "path": detail.path,
        "event": event,
        "timestamp": now,
        "isDir": detail.is_dir,
    });
    if let Some(size) = detail.size {
        value["size"] = json!(size);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_against_the_working_dir() {
        assert_eq!(resolve("/etc/hosts", Some("/root")).unwrap(), "/etc/hosts");
        assert_eq!(resolve("a/b", None).unwrap(), "/a/b");
        assert_eq!(resolve("a", Some("/root/")).unwrap(), "/root/a");
        assert_eq!(resolve("a", Some("")).unwrap(), "/a");
        assert!(resolve("", None).is_err());
        assert!(resolve("a", Some("root")).is_err());
        assert!(resolve("a\0b", None).is_err());
    }

    #[test]
    fn modes() {
        assert_eq!(parse_mode("m", "0644").ok(), Some(0o644));
        assert_eq!(parse_mode("m", "755").ok(), Some(0o755));
        assert_eq!(parse_mode("m", "4755").ok(), Some(0o4755));
        for bad in ["", "0x1", "+644", "0888", "17777", "-1"] {
            assert!(parse_mode("m", bad).is_err(), "{bad}");
        }
        assert_eq!(mode_string(0o100644), "0644");
        assert_eq!(mode_string(0o4755), "4755");
    }

    #[test]
    fn errors_by_errno() {
        let e = fs_error("read_file /x: No such file or directory (os error 2)", "/x");
        assert_eq!(e.response().status(), StatusCode::NOT_FOUND);
        let e = fs_error("remove /d: Directory not empty (os error 39)", "/d");
        assert_eq!(e.response().status(), StatusCode::BAD_REQUEST);
        let e = fs_error("copy: Permission denied (os error 13)", "/p");
        assert_eq!(e.response().status(), StatusCode::FORBIDDEN);
        let e = fs_error("odd", "/p");
        assert_eq!(e.response().status(), StatusCode::INTERNAL_SERVER_ERROR);
        let e = fs_error("write_file /f: No space left on device (os error 28)", "/f");
        assert_eq!(e.response().status(), StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(e.code(), "ENOSPC");
        let e = fs_error("rename: Invalid cross-device link (os error 18)", "/f");
        assert_eq!(e.code(), "EXDEV");
        assert_eq!(fs_error("odd (os error 4095)", "/f").code(), "EIO");
    }

    #[tokio::test]
    async fn an_fs_error_names_its_code_and_path() {
        let e = fs_error("stat /x: No such file or directory (os error 2)", "/x");
        let body = e.response().into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            json!({
                "error": "stat /x: No such file or directory (os error 2)",
                "code": "ENOENT",
                "path": "/x",
            })
        );
    }

    #[test]
    fn names() {
        assert_eq!(basename("/a/b.txt"), "b.txt");
        assert_eq!(basename("/a/dir/"), "dir");
        assert_eq!(basename("/"), "/");
        assert_eq!(child_path("/", "x"), "/x");
        assert_eq!(child_path("/a/", "b/c"), "/a/b/c");
    }

    #[test]
    fn entries() {
        let entry = DirEntry {
            name: "src/main.rs".into(),
            entry_type: "file".into(),
            size: 12,
            mode: Some(0o100644),
            mtime: Some(0),
        };
        assert_eq!(
            entry_json(&entry, "/w/src/main.rs"),
            json!({
                "name": "src/main.rs",
                "path": "/w/src/main.rs",
                "type": "file",
                "size": 12,
                "mode": "0644",
                "modTime": "1970-01-01T00:00:00Z",
                "isDir": false,
            })
        );
    }

    #[test]
    fn events() {
        let detail = WatchDetail {
            kind: watch_kind::DELETE,
            path: "/w/gone".into(),
            size: None,
            is_dir: true,
        };
        let event = event_json(&detail);
        assert_eq!(event["event"], "remove");
        assert_eq!(event["isDir"], true);
        assert!(event.get("size").is_none());
        let detail = WatchDetail {
            kind: watch_kind::MODIFY,
            size: Some(3),
            is_dir: false,
            ..detail
        };
        assert_eq!(event_json(&detail)["event"], "write");
        assert_eq!(event_json(&detail)["size"], 3);
    }
}
