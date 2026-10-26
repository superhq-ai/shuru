//! Nameless files and reflinks.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};

/// A read-write file with no name on the filesystem holding `dir`, via
/// `O_TMPFILE`. `Ok(None)` when the filesystem does not support it.
pub fn open_anonymous(dir: &str) -> io::Result<Option<File>> {
    let c_dir = CString::new(dir)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL"))?;
    let fd = unsafe {
        libc::open(
            c_dir.as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd >= 0 {
        return Ok(Some(unsafe { File::from_raw_fd(fd) }));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::EISDIR) | Some(libc::EINVAL) => Ok(None),
        _ => Err(err),
    }
}

/// Share the blocks of `src` with `dst` (`FICLONE`). `Ok(false)` when the
/// filesystem cannot, so the caller copies instead.
pub fn reflink(dst: &File, src: &File) -> io::Result<bool> {
    let ret = unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE as _, src.as_raw_fd()) };
    if ret == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EXDEV) | Some(libc::EINVAL) => {
            Ok(false)
        }
        _ => Err(err),
    }
}
