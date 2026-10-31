use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Binary framing for all vsock communication.
///
/// Frame format: `[u32 BE length][u8 type][payload...]`
/// Length = size of type byte + payload (excludes the 4-byte length prefix).
/// Max frame size: 1 MB.
pub mod frame;
pub mod hello;

// --- Exec protocol ---

#[derive(Serialize, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Names the process for a KillRequest on another connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
}

/// Kill an exec by the id it was started with. Sent on its own connection,
/// so it arrives even when the exec connection is backed up with stdin.
#[derive(Debug, Serialize, Deserialize)]
pub struct KillRequest {
    pub id: u64,
}

// --- Exec sessions ---

#[derive(Serialize, Deserialize)]
pub struct SessionStart {
    #[serde(flatten)]
    pub exec: ExecRequest,
    /// Seconds without an attachment after which the session gets SIGHUP.
    /// Without it the session runs until it exits or is killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_after_disconnect: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionAttach {
    pub id: String,
    /// Replay from this output byte. Without it, everything still buffered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_offset: Option<u64>,
    /// Become the owner now, in place of any owner there is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub take_owner: bool,
}

/// Offsets count the bytes of stdout and stderr together from the start of
/// the session. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub command: Vec<String>,
    pub workdir: String,
    pub tty: bool,
    pub created: u64,
    pub is_active: bool,
    pub last_activity: u64,
    pub bytes_per_second: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// On an attachment: whether its input reaches the process.
    #[serde(default)]
    pub is_owner: bool,
    /// On an attachment: the offset of the first byte that follows. Higher
    /// than the one asked for when those bytes have left the buffer.
    #[serde(default)]
    pub output_offset: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionKill {
    pub id: String,
    /// A name such as `SIGTERM` or `TERM`, or a number. Default SIGTERM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// Seconds to wait before SIGKILL. Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KillEvent {
    Signal {
        message: String,
        signal: String,
        pid: i32,
    },
    Timeout {
        message: String,
    },
    Exited {
        message: String,
    },
    Killed {
        message: String,
    },
    Error {
        message: String,
    },
    Complete {
        exit_code: i32,
    },
}

// --- Shutdown protocol ---

/// Reply to SHUTDOWN_REQ, sent once every guest service has exited and the
/// disk is synced. `killed` lists services that ignored SIGTERM and were
/// killed after their stop timeout.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShutdownResponse {
    #[serde(default)]
    pub stopped: Vec<String>,
    #[serde(default)]
    pub killed: Vec<String>,
}

// --- Port forwarding protocol ---

/// A host:guest port mapping for port forwarding over vsock.
#[derive(Debug, Clone)]
pub struct PortMapping {
    pub host_port: u16,
    pub guest_port: u16,
}

/// Sent by the host over vsock to request forwarding to a guest port.
#[derive(Serialize, Deserialize)]
pub struct ForwardRequest {
    pub port: u16,
    /// A name or address resolved in the guest; 127.0.0.1 when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The host end never half closes, so the end of its input ends the
    /// connection both ways.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole_close: bool,
}

/// Sent by the guest in response to a ForwardRequest.
#[derive(Serialize, Deserialize)]
pub struct ForwardResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A listening TCP socket in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Port {
    pub port: u16,
    pub address: String,
    /// The process that owns the socket, 0 when none was found.
    pub pid: u32,
}

/// The payload of a PORT_EVENT frame: on a ports watch, the list of
/// listening sockets first and then each change; on an exec session, a
/// socket opened or closed by a process of the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PortEvent {
    PortList { ports: Vec<Port> },
    PortOpened(Port),
    PortClosed(Port),
}

// --- Mount protocol ---

/// Sent by the host over vsock to instruct the guest to mount a virtiofs device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountRequest {
    pub tag: String,
    pub guest_path: String,
    /// When true (default), guest mounts via overlay (writes go to tmpfs).
    /// When false, guest mounts VirtioFS directly (writes go to host).
    #[serde(default = "default_true")]
    pub read_only: bool,
}

/// Sent by the guest in response to a MountRequest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountResponse {
    pub tag: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// --- File I/O protocol ---

#[derive(Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    pub len: u64,
    /// Permission bits for the file, a new one or one already there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Create missing parent directories. Without it they are created,
    /// as before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mkdir: Option<bool>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The file as written, on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

// --- Filesystem operations protocol ---

#[derive(Default, Serialize, Deserialize)]
pub struct FsOkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The paths an operation created or changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// The bytes a copy wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// The ids a chown set, names resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    pub path: String,
    /// If true, decompress .tar.gz and extract to `path` as a directory.
    #[serde(default)]
    pub extract: bool,
    /// When extracting, strip N leading path components from each entry,
    /// mirroring `tar --strip-components=N`. Defaults to 0 (keep paths as-is).
    /// Use `1` for releases wrapped in a single top-level directory
    /// (Node.js, Pi, etc.) so `node-v22/bin/node` becomes `bin/node`.
    #[serde(default)]
    pub strip_components: u32,
}

#[derive(Serialize, Deserialize)]
pub struct DownloadProgress {
    pub bytes_downloaded: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct MkdirRequest {
    pub path: String,
    #[serde(default = "default_true")]
    pub recursive: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ReadDirRequest {
    pub path: String,
    /// List the whole tree, each entry named by its path below `path`.
    #[serde(default)]
    pub recursive: bool,
    /// Keep only entries that match this gitignore style pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Answer with as many READ_DIR_RESP frames as it takes, each with
    /// `more` set but the last, instead of one frame.
    #[serde(default)]
    pub stream: bool,
}

#[derive(Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: u64,
    /// The full `st_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ReadDirResponse {
    pub entries: Vec<DirEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

#[derive(Serialize, Deserialize)]
pub struct StatRequest {
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct StatResponse {
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
}

#[derive(Serialize, Deserialize)]
pub struct RemoveRequest {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
}

/// Discard overlay changes for a file: removes it from the overlay upper dir,
/// revealing the original host version from the lower layer.
#[derive(Serialize, Deserialize)]
pub struct DiscardRequest {
    /// Path relative to the overlay mount (e.g., "/workspace/src/main.rs")
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct RenameRequest {
    pub old_path: String,
    pub new_path: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct CopyRequest {
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub recursive: bool,
    /// Keep owners and times as well as modes.
    #[serde(default)]
    pub preserve_attrs: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ChmodRequest {
    pub path: String,
    pub mode: u32,
    #[serde(default)]
    pub recursive: bool,
}

/// A user or group by number or by name, a name looked up in the guest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Owner {
    Id(u32),
    Name(String),
}

/// Answered by FS_OK_RESP with `count`, `uid` and `gid`. An owner left
/// out stays as it is.
#[derive(Default, Serialize, Deserialize)]
pub struct ChownRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Owner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<Owner>,
    #[serde(default)]
    pub recursive: bool,
}

// --- File watching protocol ---

#[derive(Serialize, Deserialize)]
pub struct WatchRequest {
    pub path: String,
    #[serde(default = "default_true")]
    pub recursive: bool,
    /// Every change, directories included and nothing ignored: an
    /// FS_OK_RESP once the watch is in place, then WATCH_DETAIL frames,
    /// with an ERROR frame for a failure.
    #[serde(default)]
    pub detail: bool,
}

/// The payload of a WATCH_DETAIL frame.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchDetail {
    /// A `watch_kind`.
    pub kind: u8,
    pub path: String,
    /// The size after the change, when the path is still there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub is_dir: bool,
}

fn default_true() -> bool {
    true
}

/// Binary watch event kinds.
pub mod watch_kind {
    pub const CREATE: u8 = 0x01;
    pub const MODIFY: u8 = 0x02;
    pub const DELETE: u8 = 0x03;
    pub const RENAME: u8 = 0x04;
    /// Owner, mode or times; only in WATCH_DETAIL.
    pub const CHMOD: u8 = 0x05;

    /// The name the stdio protocol and the SDKs use for a kind.
    pub fn name(kind: u8) -> Option<&'static str> {
        match kind {
            CREATE => Some("create"),
            MODIFY => Some("modify"),
            DELETE => Some("delete"),
            RENAME => Some("rename"),
            CHMOD => Some("chmod"),
            _ => None,
        }
    }
}

/// A filesystem watch event. Binary format: `[u8 kind][path bytes...]`
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub kind: u8,
    pub path: String,
}

impl WatchEvent {
    /// Encode to binary payload for a WATCH_EVENT frame.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + self.path.len());
        buf.push(self.kind);
        buf.extend_from_slice(self.path.as_bytes());
        buf
    }

    /// Decode from binary payload of a WATCH_EVENT frame.
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.is_empty() {
            return None;
        }
        let kind = payload[0];
        let path = std::str::from_utf8(&payload[1..]).ok()?.to_string();
        Some(Self { kind, path })
    }
}

pub const VSOCK_PORT: u32 = 1024;
pub const VSOCK_PORT_FORWARD: u32 = 1025;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_request_read_only_true_by_default() {
        let json = r#"{"tag":"mount0","guest_path":"/workspace"}"#;
        let req: MountRequest = serde_json::from_str(json).unwrap();
        assert!(req.read_only);
    }

    #[test]
    fn mount_request_read_only_false_roundtrips() {
        let req = MountRequest {
            tag: "mount0".into(),
            guest_path: "/workspace".into(),
            read_only: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let req2: MountRequest = serde_json::from_str(&json).unwrap();
        assert!(!req2.read_only);
    }

    #[test]
    fn port_events_are_tagged_by_type() {
        let port = Port {
            port: 8080,
            address: "0.0.0.0".into(),
            pid: 42,
        };
        let opened = serde_json::to_value(PortEvent::PortOpened(port.clone())).unwrap();
        assert_eq!(
            opened,
            serde_json::json!({"type": "port_opened", "port": 8080, "address": "0.0.0.0", "pid": 42})
        );
        let list = PortEvent::PortList { ports: vec![port] };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["type"], "port_list");
        assert_eq!(json["ports"][0]["port"], 8080);
        assert_eq!(serde_json::from_value::<PortEvent>(json).unwrap(), list);
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SealRequest {
    pub id: u64,
    pub stop_services: bool,
    pub lease_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThawRequest {
    pub id: u64,
}

/// `Sealed` carries what the guest stopped when the seal asked it to stop
/// its services.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum SealResponse {
    Sealed(ShutdownResponse),
    Thawed,
    Expired,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TrimResponse {
    pub supported: bool,
    pub bytes: u64,
}
