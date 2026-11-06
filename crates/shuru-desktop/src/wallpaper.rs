//! The wallpaper: the picture shown behind the windows of the desktop, the
//! setting in `~/.config/superhq/wallpaper`, and reading the file.

use std::path::{Path, PathBuf};

use crate::draw::Image;

/// The biggest side a wallpaper is kept at. The screen is at most a few
/// million pixels; a larger file is scaled when it is read.
const MAX_SIDE: usize = 2560;

/// The names of the choices, as the file spells them.
pub const CHOICES: [&str; 5] = ["default", "none", "wall1", "wall2", "wall3"];

/// A choice of the wallpaper: how the desktop background is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The flat color of the appearance; what a desktop without a
    /// wallpaper shows.
    Default,
    /// Flat, with no picture, as `default` paints today.
    None,
    Label(&'static str),
}

impl Choice {
    /// The choice a saved name means. An unknown name is the default, so
    /// a guest with more pictures than a build knows about still has one.
    pub fn of(saved: &str) -> Choice {
        let name = saved.trim().trim_matches('"');
        match name {
            "default" => Choice::Default,
            "none" => Choice::None,
            _ => CHOICES
                .iter()
                .copied()
                .find(|c| *c == name)
                .map(Choice::Label)
                .unwrap_or(Choice::Default),
        }
    }

    /// Whether a name is one this desktop can show: a bare choice of
    /// `CHOICES` or a file that is there. What a name means is `of`, and
    /// an unknown one is the default; this is what tells a typo from a
    /// wallpaper named after a file.
    pub fn known(name: &str) -> bool {
        let name = name.trim().trim_matches('"');
        !name.is_empty() && (matches!(name, "default" | "none") || find(name).is_some())
    }

    /// The name the setting file holds.
    pub fn name(self) -> &'static str {
        match self {
            Choice::Default => "default",
            Choice::None => "none",
            Choice::Label(l) => l,
        }
    }

    /// The picture of this choice, read and scaled, when it has one.
    pub fn image(self) -> Option<Image> {
        let name = match self {
            Choice::Label(l) if l != "default" => l,
            _ => return None,
        };
        let path = find(name)?;
        let (w, h) = Image::size_of(&path)?;
        let side = w.max(h);
        let scale = if side > MAX_SIDE {
            MAX_SIDE as f64 / side as f64
        } else {
            1.0
        };
        let want = (
            ((w as f64 * scale).round() as usize).max(1),
            ((h as f64 * scale).round() as usize).max(1),
        );
        Image::load(&path, want.0, want.1)
    }
}

pub fn dir() -> PathBuf {
    crate::appearance::dir()
}

pub fn file() -> PathBuf {
    dir().join("wallpaper")
}

/// The saved choice.
pub fn read() -> Choice {
    std::fs::read_to_string(file())
        .map(|t| Choice::of(&t))
        .unwrap_or(Choice::Default)
}

/// Writes the setting by a rename, so a watcher never reads half of it.
pub fn write(choice: Choice) -> std::io::Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(".wallpaper.tmp");
    std::fs::write(&tmp, format!("{}\n", choice.name()))?;
    std::fs::rename(tmp, file())
}

/// The directories of the wallpapers of the guest, then the user's own:
/// what the guest ships, and what a person puts in `~/.local/share`.
pub fn dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_DATA_HOME") {
        Some(h) if !h.is_empty() => out.push(PathBuf::from(h).join("backgrounds")),
        _ => out.extend(home.as_ref().map(|h| h.join(".local/share/backgrounds"))),
    }
    let data = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    out.extend(data.split(':').map(|d| Path::new(d).join("backgrounds")));
    out
}

/// The file of a wallpaper named `name`, a bare name of `CHOICES` or a
/// path. A path is taken as it is, so a person can point at any file.
pub fn find(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    for dir in dirs() {
        for ext in ["png", "jpg", "jpeg", "webp"] {
            let path = dir.join(format!("{name}.{ext}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_names_are_a_choice() {
        assert_eq!(Choice::of("default"), Choice::Default);
        assert_eq!(Choice::of(" Default \n"), Choice::Default);
        assert_eq!(Choice::of("none"), Choice::None);
        assert_eq!(Choice::of("wall1"), Choice::Label("wall1"));
        assert_eq!(Choice::of("\"wall3\""), Choice::Label("wall3"));
        assert_eq!(Choice::of("picture-of-mine"), Choice::Default);
        assert_eq!(Choice::of(""), Choice::Default);
    }

    #[test]
    fn the_name_round_trips() {
        for name in CHOICES {
            assert_eq!(Choice::of(name).name(), name);
        }
        assert_eq!(Choice::None.name(), "none");
    }

    #[test]
    fn the_default_and_none_have_no_picture() {
        assert!(Choice::Default.image().is_none());
        assert!(Choice::None.image().is_none());
        assert!(Choice::Label("default").image().is_none());
    }

    #[test]
    fn a_path_names_itself() {
        let path = std::env::temp_dir().join("shuru-wallpaper-not-there.png");
        assert_eq!(find(path.to_str().unwrap()), None);
    }

    #[test]
    fn a_typo_is_not_a_wallpaper_and_a_file_is() {
        assert!(Choice::known("default"));
        assert!(Choice::known("none"));
        assert!(Choice::known("wall1"));
        assert!(!Choice::known("holiday-photo"));
        assert!(!Choice::known(""));
        assert!(!Choice::known("null"));
    }

    #[test]
    fn the_default_names_no_picture() {
        // A name the shell does not know is the default, not a picture
        // looked up in a directory a guest may have put one in.
        assert_eq!(Choice::of("holiday-photo"), Choice::Default);
        assert_eq!(Choice::of("wallpaper"), Choice::Default);
    }
}
