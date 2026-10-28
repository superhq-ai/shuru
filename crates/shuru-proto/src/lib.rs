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
}

/// Sent by the guest in response to a ForwardRequest.
#[derive(Serialize, Deserialize)]
pub struct ForwardResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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

#[derive(Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    pub len: u64,
}

#[derive(Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// --- Filesystem operations protocol ---

#[derive(Serialize, Deserialize)]
pub struct FsOkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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

#[derive(Serialize, Deserialize)]
pub struct ReadDirRequest {
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize)]
pub struct ReadDirResponse {
    pub entries: Vec<DirEntry>,
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

#[derive(Serialize, Deserialize)]
pub struct CopyRequest {
    pub src: String,
    pub dst: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ChmodRequest {
    pub path: String,
    pub mode: u32,
}

// --- File watching protocol ---

#[derive(Serialize, Deserialize)]
pub struct WatchRequest {
    pub path: String,
    #[serde(default = "default_true")]
    pub recursive: bool,
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

    /// The name the stdio protocol and the SDKs use for a kind.
    pub fn name(kind: u8) -> Option<&'static str> {
        match kind {
            CREATE => Some("create"),
            MODIFY => Some("modify"),
            DELETE => Some("delete"),
            RENAME => Some("rename"),
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
