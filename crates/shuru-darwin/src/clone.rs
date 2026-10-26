//! APFS copy-on-write clones.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
    fn fclonefileat(
        srcfd: libc::c_int,
        dst_dirfd: libc::c_int,
        dst: *const libc::c_char,
        flags: u32,
    ) -> libc::c_int;
}

fn c_path(path: &str) -> io::Result<CString> {
    CString::new(path).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL"))
}

/// Clone the file at `src` to a new file at `dst`, which must not exist.
pub fn clone_path(src: &str, dst: &str) -> io::Result<()> {
    let c_src = c_path(src)?;
    let c_dst = c_path(dst)?;
    let ret = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Clone the open file `fd` to a new file at `dst`, which must not exist.
/// Works on a file that has no name any more.
pub fn clone_fd(fd: RawFd, dst: &str) -> io::Result<()> {
    let c_dst = c_path(dst)?;
    let ret = unsafe { fclonefileat(fd, libc::AT_FDCWD, c_dst.as_ptr(), 0) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
