use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn check_format(dest: &str) -> Result<()> {
    let path = Path::new(dest);
    let other = match path.extension().and_then(|s| s.to_str()) {
        Some("ext4") => path.with_extension("idx"),
        Some("idx") => path.with_extension("ext4"),
        _ => return Ok(()),
    };
    anyhow::ensure!(
        !other.exists(),
        "checkpoint uses another storage format; choose a new name"
    );
    Ok(())
}

pub(crate) fn publish(
    dest: &str,
    copy: impl FnOnce(&str) -> Result<()>,
    finish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let staging = PathBuf::from(format!("{dest}.partial"));
    match std::fs::create_dir(&staging) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e).context("reserve checkpoint staging directory");
        }
        _ => {}
    }
    // The lock tells a save in progress from one that died and left its
    // staging directory behind.
    let lock = std::fs::File::open(&staging)?;
    if lock.try_lock().is_err() {
        anyhow::bail!("another save of this checkpoint is in progress");
    }
    let guard = Partial(staging);
    let image = guard.0.join("disk");
    match std::fs::remove_file(&image) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    copy(image.to_str().context("checkpoint path is not UTF-8")?)?;
    std::fs::File::open(&image)?.sync_all()?;
    finish()?;
    std::fs::rename(&image, dest).context("publish sealed checkpoint")?;
    let parent = Path::new(dest)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn failed_copy_or_expired_lease_preserves_destination() {
        let dir = std::env::temp_dir().join(format!("shuru-publish-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("checkpoint");
        let dest = dest.to_str().unwrap();
        std::fs::write(dest, b"previous").unwrap();
        assert!(publish(
            dest,
            |p| {
                std::fs::write(p, b"partial")?;
                bail!("copy failed")
            },
            || Ok(())
        )
        .is_err());
        assert_eq!(std::fs::read(dest).unwrap(), b"previous");
        assert!(publish(
            dest,
            |p| {
                std::fs::write(p, b"new")?;
                Ok(())
            },
            || { bail!("Expired") }
        )
        .is_err());
        assert_eq!(std::fs::read(dest).unwrap(), b"previous");
        publish(
            dest,
            |p| {
                std::fs::write(p, b"new")?;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"new");

        let staging = format!("{dest}.partial");
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(format!("{staging}/disk"), b"left by a crash").unwrap();
        let held = std::fs::File::open(&staging).unwrap();
        held.lock().unwrap();
        assert!(publish(dest, |_| panic!("must not copy"), || Ok(())).is_err());
        assert!(Path::new(&staging).is_dir());
        drop(held);
        publish(
            dest,
            |p| {
                std::fs::write(p, b"after crash")?;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"after crash");
        assert!(!Path::new(&staging).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
