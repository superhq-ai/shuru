//! The working disk of one VM run: a copy-on-write clone of a base image
//! that exists only as long as the process holding it.
//!
//! The clone is anonymous. On Linux it is created with `O_TMPFILE` and
//! never has a name; on macOS the framework needs a path, so the clone gets
//! a name for the two syscalls between creation and open, then is unlinked
//! and attached through `/dev/fd`. Either way the kernel frees the blocks
//! when the last descriptor closes, however the process ends, so nothing
//! has to be cleaned up afterwards. Same idiom as `tmpfile(3)` and QEMU
//! `-snapshot`.
//!
//! Cloning is a reflink where the filesystem has one (APFS, XFS with
//! reflink=1, btrfs), so a boot costs no disk and no copy time. Filesystems
//! without reflinks fall back to a full copy, with a note the first time so
//! an operator knows why boots are slow and disks fill up.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, RawFd};

use anyhow::{bail, Context, Result};

pub struct WorkDisk {
    file: File,
}

impl WorkDisk {
    /// Clone `base` into an anonymous file on the filesystem holding `dir`,
    /// grown to `size_mb` if that is larger than the base.
    pub fn clone_from(base: &str, dir: &str, size_mb: u64) -> Result<Self> {
        let disk = Self::clone_anonymous(base, dir)?;
        let target = size_mb * 1024 * 1024;
        let current = disk.file.metadata()?.len();
        if target < current {
            bail!(
                "disk size {}MB is smaller than the base image ({}MB)",
                size_mb,
                current / 1024 / 1024
            );
        }
        if target > current {
            disk.file.set_len(target)?;
        }
        Ok(disk)
    }

    /// An empty anonymous file, for a VM whose disk is served some other
    /// way and only needs a placeholder to attach.
    pub fn empty(dir: &str) -> Result<Self> {
        Ok(Self {
            file: anonymous_file(dir)?,
        })
    }

    /// The path the VM opens. Valid only in this process and only while
    /// this value lives.
    pub fn attach_path(&self) -> String {
        #[cfg(target_os = "macos")]
        {
            format!("/dev/fd/{}", self.file.as_raw_fd())
        }
        #[cfg(not(target_os = "macos"))]
        {
            format!("/proc/self/fd/{}", self.file.as_raw_fd())
        }
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Clone the current contents to `dest`, which must not exist. Instant
    /// on a reflink filesystem, a copy elsewhere.
    pub fn save_to(&self, dest: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            shuru_darwin::clone_fd(self.file.as_raw_fd(), dest)
                .with_context(|| format!("clone to {} failed", dest))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)
                .with_context(|| format!("creating {}", dest))?;
            if shuru_linux::reflink(&out, &self.file)? {
                return Ok(());
            }
            copy_contents(&self.file, &out).with_context(|| format!("copying to {}", dest))
        }
    }

    #[cfg(target_os = "macos")]
    fn clone_anonymous(base: &str, dir: &str) -> Result<Self> {
        let name = format!("{}/.work-{}", dir, random_hex());
        shuru_darwin::clone_path(base, &name)
            .with_context(|| format!("clonefile({} -> {}) failed", base, name))?;
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name);
        let _ = std::fs::remove_file(&name);
        Ok(Self { file: opened? })
    }

    #[cfg(not(target_os = "macos"))]
    fn clone_anonymous(base: &str, dir: &str) -> Result<Self> {
        use std::sync::Once;
        static COPY_NOTE: Once = Once::new();

        let file = anonymous_file(dir)?;
        let src = File::open(base).with_context(|| format!("opening {}", base))?;
        if !shuru_linux::reflink(&file, &src)? {
            COPY_NOTE.call_once(|| {
                eprintln!(
                    "shuru: note: {} does not support reflinks, copying disk images instead; put the data directory on XFS or btrfs for instant clones",
                    dir
                );
            });
            copy_contents(&src, &file).with_context(|| format!("copying {}", base))?;
        }
        Ok(Self { file })
    }
}

/// A file with no name on the filesystem holding `dir`.
fn anonymous_file(dir: &str) -> Result<File> {
    #[cfg(not(target_os = "macos"))]
    if let Some(file) = shuru_linux::open_anonymous(dir)? {
        return Ok(file);
    }
    let name = format!("{}/.work-{}", dir, random_hex());
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&name);
    let _ = std::fs::remove_file(&name);
    Ok(opened?)
}

/// Copy by offset so neither descriptor's position is touched.
#[cfg(not(target_os = "macos"))]
fn copy_contents(src: &File, dst: &File) -> Result<()> {
    let len = src.metadata()?.len();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < len {
        let n = src.read_at(&mut buf, off)?;
        if n == 0 {
            break;
        }
        dst.write_all_at(&buf[..n], off)?;
        off += n as u64;
    }
    dst.set_len(len)?;
    Ok(())
}

fn random_hex() -> String {
    let mut bytes = [0u8; 8];
    if let Ok(f) = File::open("/dev/urandom") {
        let _ = f.read_exact_at(&mut bytes, 0);
    }
    let pid = std::process::id() as u64;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{:016x}",
        u64::from_le_bytes(bytes) ^ pid.rotate_left(32) ^ nanos
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> String {
        let dir = std::env::temp_dir().join(format!("shuru-workdisk-{}", random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn names_in(dir: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_clone_has_no_name_and_the_base_content() {
        let dir = temp_dir();
        let base = format!("{}/base.img", dir);
        std::fs::File::create(&base)
            .unwrap()
            .write_all(b"base image bytes")
            .unwrap();

        let disk = WorkDisk::clone_from(&base, &dir, 1).unwrap();
        assert_eq!(names_in(&dir), vec!["base.img".to_string()]);

        let mut head = [0u8; 16];
        disk.file.read_exact_at(&mut head, 0).unwrap();
        assert_eq!(&head, b"base image bytes");
        assert_eq!(disk.file.metadata().unwrap().len(), 1024 * 1024);
        assert!(disk.attach_path().ends_with(&disk.as_raw_fd().to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_go_to_the_clone_not_the_base() {
        let dir = temp_dir();
        let base = format!("{}/base.img", dir);
        std::fs::File::create(&base)
            .unwrap()
            .write_all(b"original")
            .unwrap();

        let disk = WorkDisk::clone_from(&base, &dir, 1).unwrap();
        disk.file.write_all_at(b"changed!", 0).unwrap();
        assert_eq!(std::fs::read(&base).unwrap(), b"original");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_to_captures_the_current_contents_and_refuses_to_overwrite() {
        let dir = temp_dir();
        let base = format!("{}/base.img", dir);
        std::fs::File::create(&base)
            .unwrap()
            .write_all(&vec![7u8; 4096])
            .unwrap();

        let disk = WorkDisk::clone_from(&base, &dir, 1).unwrap();
        disk.file.write_all_at(b"saved state", 100).unwrap();
        let dest = format!("{}/snap.img", dir);
        disk.save_to(&dest).unwrap();

        let saved = std::fs::read(&dest).unwrap();
        assert_eq!(saved.len(), 1024 * 1024);
        assert_eq!(&saved[100..111], b"saved state");
        assert_eq!(saved[0], 7);
        assert!(disk.save_to(&dest).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_disk_smaller_than_the_base_is_refused() {
        let dir = temp_dir();
        let base = format!("{}/base.img", dir);
        std::fs::File::create(&base)
            .unwrap()
            .set_len(3 * 1024 * 1024)
            .unwrap();
        assert!(WorkDisk::clone_from(&base, &dir, 2).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_disk_has_no_name_either() {
        let dir = temp_dir();
        let disk = WorkDisk::empty(&dir).unwrap();
        assert!(names_in(&dir).is_empty());
        assert_eq!(disk.file.metadata().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
