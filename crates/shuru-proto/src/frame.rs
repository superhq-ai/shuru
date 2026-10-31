use std::io::{self, Read, Write};

pub const HELLO: u8 = 0x08;
pub const HELLO_OK: u8 = 0x09;

// I/O streams
pub const STDIN: u8 = 0x01;
pub const STDOUT: u8 = 0x02;
pub const STDERR: u8 = 0x03;

// Control
pub const RESIZE: u8 = 0x04;
pub const EXIT: u8 = 0x05;
pub const ERROR: u8 = 0x06;
pub const KILL: u8 = 0x07;
/// Closes the stdin of a session. Empty payload.
pub const STDIN_EOF: u8 = 0x0A;

// Exec handshake
pub const EXEC_REQ: u8 = 0x10;

// Mount handshake
pub const MOUNT_REQ: u8 = 0x11;
pub const MOUNT_RESP: u8 = 0x12;

// File I/O
// File contents travel in frames of at most FILE_CHUNK bytes. A read ends
// with an empty READ_FILE_RESP; a write sends WriteFileRequest::len bytes.
pub const READ_FILE_REQ: u8 = 0x13;
pub const READ_FILE_RESP: u8 = 0x14;
pub const WRITE_FILE_REQ: u8 = 0x15;
pub const WRITE_FILE_DATA: u8 = 0x16;
pub const WRITE_FILE_RESP: u8 = 0x17;

// Port forwarding
pub const FWD_REQ: u8 = 0x20;
pub const FWD_RESP: u8 = 0x21;
/// Answered by PORT_EVENT frames, a PortList first, until the host hangs up.
pub const PORTS_WATCH_REQ: u8 = 0x22;
pub const PORT_EVENT: u8 = 0x23;

// File watching
pub const WATCH_REQ: u8 = 0x30;
pub const WATCH_EVENT: u8 = 0x31;
pub const WATCH_DETAIL: u8 = 0x32;

// Filesystem operations
pub const MKDIR_REQ: u8 = 0x40;
pub const FS_OK_RESP: u8 = 0x41;
pub const READ_DIR_REQ: u8 = 0x42;
pub const READ_DIR_RESP: u8 = 0x43;
pub const STAT_REQ: u8 = 0x44;
pub const STAT_RESP: u8 = 0x45;
pub const REMOVE_REQ: u8 = 0x46;
pub const RENAME_REQ: u8 = 0x48;
pub const COPY_REQ: u8 = 0x4A;
pub const CHMOD_REQ: u8 = 0x4C;
pub const CHOWN_REQ: u8 = 0x4D;

// Overlay operations
pub const DISCARD_REQ: u8 = 0x4E;
pub const DISCARD_RESP: u8 = 0x4F;

// Download
pub const DOWNLOAD_REQ: u8 = 0x50;
pub const DOWNLOAD_PROGRESS: u8 = 0x51;

// Shutdown: host asks the guest to stop its services and sync the disk
pub const SHUTDOWN_REQ: u8 = 0x60;
pub const SHUTDOWN_RESP: u8 = 0x61;

/// The largest frame, its type byte included.
pub const MAX_FRAME: u32 = 1 << 20;

pub const FILE_CHUNK: usize = 256 * 1024;

/// Write a binary frame: `[u32 BE length][u8 type][payload]`.
///
/// Assembles the header + payload into a single buffer so that the entire
/// frame is sent in one `write_all` call. This avoids multiple small TCP
/// segments when `TCP_NODELAY` is enabled.
pub fn write_frame(w: &mut impl Write, msg_type: u8, payload: &[u8]) -> io::Result<()> {
    let len = 1u32 + payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + 1 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(msg_type);
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Read a binary frame. Returns `None` on clean EOF, `Err` on protocol
/// violations or I/O errors.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length out of range: {}", len),
        ));
    }
    let mut type_buf = [0u8; 1];
    r.read_exact(&mut type_buf)?;
    let payload_len = (len - 1) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok(Some((type_buf[0], payload)))
}

/// Serialize `msg` as JSON and send it as a typed frame.
pub fn send_json(w: &mut impl Write, msg_type: u8, msg: &impl serde::Serialize) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    write_frame(w, msg_type, &payload)
}

/// Try to parse a complete frame from the front of `buf`.
/// Returns `Some((msg_type, payload_start, total_len))` if a full
/// frame is available, `None` if more data is needed.
pub fn try_parse(buf: &[u8]) -> Option<(u8, usize, usize)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len == 0 || len > MAX_FRAME {
        return None;
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return None;
    }
    let msg_type = buf[4];
    Some((msg_type, 5, total))
}

/// Build a RESIZE payload: `[u16 BE rows][u16 BE cols]`.
pub fn resize_payload(rows: u16, cols: u16) -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0..2].copy_from_slice(&rows.to_be_bytes());
    buf[2..4].copy_from_slice(&cols.to_be_bytes());
    buf
}

/// Parse a RESIZE payload into (rows, cols).
pub fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    let rows = u16::from_be_bytes([payload[0], payload[1]]);
    let cols = u16::from_be_bytes([payload[2], payload[3]]);
    Some((rows, cols))
}

/// Build an EXIT payload: `[i32 BE code]`.
pub fn exit_payload(code: i32) -> [u8; 4] {
    code.to_be_bytes()
}

/// Parse an EXIT payload into an i32 exit code.
pub fn parse_exit_code(payload: &[u8]) -> Option<i32> {
    if payload.len() < 4 {
        return None;
    }
    Some(i32::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

// Disk sealing uses a lease id so a stale thaw cannot release a later seal.
pub const SEAL_REQ: u8 = 0x62;
pub const SEAL_RESP: u8 = 0x63;
pub const THAW_REQ: u8 = 0x64;
pub const THAW_RESP: u8 = 0x65;
pub const TRIM_REQ: u8 = 0x66;
pub const TRIM_RESP: u8 = 0x67;

pub const KILL_REQ: u8 = 0x18;
pub const KILL_RESP: u8 = 0x19;

// After a restore the host gives the guest a fresh identity, the wall clock
// and RNG seed material. See hello::Restore.
pub const RESTORE_REQ: u8 = 0x68;
pub const RESTORE_RESP: u8 = 0x69;

// Exec sessions outlive the connection that started them (RFC 0012).
// START and ATTACH answer with SESSION_INFO, then STDOUT and STDERR from
// its output_offset, then EXIT; the connection takes STDIN, STDIN_EOF,
// RESIZE and KILL like an EXEC_REQ connection.
pub const SESSION_START: u8 = 0x70;
pub const SESSION_ATTACH: u8 = 0x71;
pub const SESSION_INFO: u8 = 0x72;
pub const SESSION_LIST: u8 = 0x73;
pub const SESSION_LIST_RESP: u8 = 0x74;
/// Answered by SESSION_KILL_EVENT frames, the last one `complete` or `error`.
pub const SESSION_KILL: u8 = 0x75;
pub const SESSION_KILL_EVENT: u8 = 0x76;
