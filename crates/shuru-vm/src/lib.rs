#![forbid(unsafe_code)]

mod sandbox;
mod serial;
mod workdisk;

pub use sandbox::{MountConfig, PortForwardHandle, Sandbox, VmConfigBuilder};
pub use shuru_proto::{
    frame, ExecRequest, ForwardRequest, ForwardResponse, MountRequest, MountResponse, PortMapping,
    ReadFileRequest, WriteFileRequest, WriteFileResponse, VSOCK_PORT, VSOCK_PORT_FORWARD,
};
pub use workdisk::WorkDisk;

// Re-exports from platform-specific backend for advanced/escape-hatch use
#[cfg(target_os = "macos")]
pub use shuru_darwin::VirtualMachine;
#[cfg(target_os = "macos")]
pub use shuru_darwin::VmState;
#[cfg(target_os = "macos")]
pub use shuru_darwin::VzError;

#[cfg(target_os = "linux")]
pub use shuru_linux::VirtualMachine;
#[cfg(target_os = "linux")]
pub use shuru_linux::VmState;
#[cfg(target_os = "linux")]
pub use shuru_linux::VzError;

/// Reject checkpoint names that could escape the checkpoints directory.
pub fn validate_checkpoint_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("checkpoint name cannot be empty".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') || name.contains("..") {
        return Err(format!("invalid checkpoint name: '{}'", name));
    }
    Ok(())
}

/// Where assets, checkpoints and instances live. `SHURU_DATA_DIR` overrides
/// the default so a build or a test run can use its own kernel, rootfs and
/// checkpoints without touching the installed ones.
pub fn default_data_dir() -> String {
    if let Ok(dir) = std::env::var("SHURU_DATA_DIR") {
        if !dir.is_empty() {
            return dir;
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/.local/share/shuru", home)
}
