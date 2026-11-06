//! Do not disturb: the setting in ~/.config/superhq/do-not-disturb. While
//! it is on, notifications go to the notification center without a banner.

use std::path::{Path, PathBuf};

pub fn file() -> PathBuf {
    crate::appearance::dir().join("do-not-disturb")
}

/// Whether the file says on. Anything else, or no file, is off.
pub fn parse(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("on")
}

pub fn read() -> bool {
    read_in(&file())
}

fn read_in(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| parse(&t))
}

/// Writes the setting by a rename, so a watcher never reads half of it.
pub fn write(on: bool) -> std::io::Result<()> {
    write_in(&file(), on)
}

fn write_in(path: &Path, on: bool) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".do-not-disturb.{}.tmp", std::process::id()));
    std::fs::write(&tmp, if on { "on\n" } else { "off\n" })?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_on_is_on() {
        assert!(parse("on\n"));
        assert!(parse(" ON "));
        assert!(!parse("off"));
        assert!(!parse(""));
        assert!(!parse("yes"));
    }

    #[test]
    fn the_setting_round_trips() {
        let dir = std::env::temp_dir().join(format!("shuru-dnd-{}", std::process::id()));
        let path = dir.join("do-not-disturb");
        assert!(!read_in(&path));
        write_in(&path, true).unwrap();
        assert!(read_in(&path));
        write_in(&path, false).unwrap();
        assert!(!read_in(&path));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
