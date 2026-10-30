//! Guest connections for the API: the hello on a blocking thread, then
//! frames over the async socket.

use std::io;
use std::sync::Arc;

use shuru_proto::{frame, SessionInfo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::http::ApiError;
use super::sandboxes::GuestAddr;
use crate::vm;

/// The most a frame of input carries, well under the frame limit.
pub const INPUT_CHUNK: usize = 64 * 1024;

/// A guest connection and its place in the budget it came from.
pub struct Conn {
    pub stream: UnixStream,
    pub permit: OwnedSemaphorePermit,
}

/// Connects to the guest within `budget`, a share of the connections its
/// VM takes, so one kind of use cannot starve the others.
pub async fn connect(addr: &GuestAddr, budget: &Arc<Semaphore>) -> Result<Conn, ApiError> {
    connect_port(addr, budget, shuru_proto::VSOCK_PORT).await
}

/// Connects to the guest port forwarder, which `FWD_REQ` points at a TCP
/// address in the guest.
pub async fn connect_forward(addr: &GuestAddr, budget: &Arc<Semaphore>) -> Result<Conn, ApiError> {
    connect_port(addr, budget, shuru_proto::VSOCK_PORT_FORWARD).await
}

async fn connect_port(
    addr: &GuestAddr,
    budget: &Arc<Semaphore>,
    port: u32,
) -> Result<Conn, ApiError> {
    let permit = budget
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::unavailable("the sandbox has too many connections open"))?;
    let addr = addr.clone();
    // The permit goes with the connect, which runs on even if the caller
    // goes away.
    let (stream, permit) = tokio::task::spawn_blocking(move || {
        (
            vm::try_connect_guest_port(&addr.vsock, &addr.identity, port),
            permit,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("{e}")))?;
    let stream = stream
        .map_err(|e| ApiError::internal(format!("connecting to the guest: {e:#}")))?
        .ok_or_else(|| ApiError::unavailable("the guest took no more connections"))?;
    stream.set_nonblocking(true)?;
    Ok(Conn {
        stream: UnixStream::from_std(stream)?,
        permit,
    })
}

/// The answer to a session start or attach.
pub async fn session_info(guest: &mut UnixStream) -> Result<SessionInfo, ApiError> {
    match read_frame(guest).await? {
        Some((frame::SESSION_INFO, payload)) => serde_json::from_slice(&payload)
            .map_err(|e| ApiError::internal(format!("the guest session info: {e}"))),
        Some((frame::ERROR, payload)) => Err(guest_error(&payload)),
        Some((kind, _)) => Err(ApiError::internal(format!(
            "the guest answered with frame {kind:#x}"
        ))),
        None => Err(ApiError::internal("the guest closed the connection")),
    }
}

pub fn guest_error(payload: &[u8]) -> ApiError {
    let message = String::from_utf8_lossy(payload).into_owned();
    if message.starts_with("no session ") {
        ApiError::not_found(message)
    } else {
        ApiError::bad_request(message)
    }
}

/// The exit status as the API sends it, one byte: the status, or 128 plus
/// the signal, as the guest reports it.
pub fn exit_byte(payload: &[u8]) -> u8 {
    frame::parse_exit_code(payload).unwrap_or(1) as u8
}

/// A frame, or `None` at a clean end of the stream.
pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut header[1..]).await?;
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if len == 0 || len > frame::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length out of range: {len}"),
        ));
    }
    let mut payload = vec![0u8; len as usize - 1];
    r.read_exact(&mut payload).await?;
    Ok(Some((header[4], payload)))
}

pub async fn write_frame(
    w: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&(1 + payload.len() as u32).to_be_bytes());
    buf.push(kind);
    buf.extend_from_slice(payload);
    w.write_all(&buf).await
}

pub async fn send_json(
    w: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    value: &impl serde::Serialize,
) -> io::Result<()> {
    let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
    write_frame(w, kind, &payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_bytes() {
        assert_eq!(exit_byte(&frame::exit_payload(0)), 0);
        assert_eq!(exit_byte(&frame::exit_payload(143)), 143);
        assert_eq!(exit_byte(&[]), 1);
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        write_frame(&mut a, frame::STDOUT, b"hello").await.unwrap();
        write_frame(&mut a, frame::STDIN_EOF, &[]).await.unwrap();
        drop(a);
        assert_eq!(
            read_frame(&mut b).await.unwrap(),
            Some((frame::STDOUT, b"hello".to_vec()))
        );
        assert_eq!(
            read_frame(&mut b).await.unwrap(),
            Some((frame::STDIN_EOF, Vec::new()))
        );
        assert_eq!(read_frame(&mut b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn refuses_an_oversized_frame() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(frame::MAX_FRAME + 1).to_be_bytes())
            .await
            .unwrap();
        a.write_all(&[frame::STDOUT]).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn a_cut_frame_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0]).await.unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }
}
