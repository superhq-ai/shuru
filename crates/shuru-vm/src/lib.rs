#![forbid(unsafe_code)]

mod checkpoint;
pub use checkpoint::check_format as check_checkpoint_format;
mod sandbox;
mod serial;
mod workdisk;

pub use sandbox::{AfterSave, MountConfig, PortForwardHandle, Sandbox, VmConfigBuilder};
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

pub const MIN_DISK_MB: u64 = 4096;

pub fn validate_disk_size(size_mb: u64) -> anyhow::Result<u64> {
    if size_mb < MIN_DISK_MB {
        anyhow::bail!("disk size must be at least {} MB", MIN_DISK_MB);
    }
    size_mb
        .checked_mul(1024 * 1024)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| anyhow::anyhow!("disk size exceeds the supported byte range"))
}

#[cfg(test)]
mod disk_size_tests {
    use super::*;

    #[test]
    fn disk_sizes_have_a_minimum_and_do_not_overflow() {
        assert!(validate_disk_size(4095).is_err());
        assert!(validate_disk_size(u64::MAX).is_err());
        assert_eq!(validate_disk_size(4096).unwrap(), 4 * 1024 * 1024 * 1024);
    }
}
