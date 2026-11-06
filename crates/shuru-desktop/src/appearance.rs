//! Light or dark: the setting in ~/.config/superhq/appearance, a watch on
//! it, and passing it on to GTK, GNOME settings and Chromium.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const LIGHT_THEME: &str = "SuperHQ";
pub const DARK_THEME: &str = "SuperHQ-dark";
pub fn theme(dark: bool) -> &'static str {
    if dark {
        DARK_THEME
    } else {
        LIGHT_THEME
    }
}

pub fn dir() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()))
                .join(".config")
        });
    config.join("superhq")
}

pub fn file() -> PathBuf {
    dir().join("appearance")
}

/// Whether the file says dark. Anything but "dark" is light.
pub fn parse(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("dark")
}

pub fn read() -> bool {
    std::fs::read_to_string(file()).is_ok_and(|t| parse(&t))
}

pub fn name(dark: bool) -> &'static str {
    if dark {
        "dark"
    } else {
        "light"
    }
}

/// Writes the setting by a rename, so a watcher never reads half of it.
pub fn write(dark: bool) -> std::io::Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(".appearance.tmp");
    std::fs::write(&tmp, format!("{}\n", name(dark)))?;
    std::fs::rename(tmp, file())
}

/// Which settings a batch of watch events touched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub appearance: bool,
    pub language: bool,
    pub do_not_disturb: bool,
}

impl Changes {
    pub fn any(self) -> bool {
        self.appearance || self.language || self.do_not_disturb
    }
}

/// An inotify watch on the directory of the settings.
pub struct Watch {
    fd: OwnedFd,
}

impl Watch {
    pub fn new() -> Option<Watch> {
        Self::in_dir(&dir())
    }

    fn in_dir(dir: &Path) -> Option<Watch> {
        std::fs::create_dir_all(dir).ok()?;
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_CREATE;
        if unsafe { libc::inotify_add_watch(raw, path.as_ptr(), mask) } < 0 {
            return None;
        }
        Some(Watch { fd })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Reads the pending events; answers which settings they touched.
    pub fn drain(&self) -> Changes {
        let mut buf = [0u8; 4096];
        let mut hit = Changes::default();
        loop {
            let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return hit;
            }
            let mut at = 0;
            let head = std::mem::size_of::<libc::inotify_event>();
            while at + head <= n as usize {
                let ev = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(at).cast::<libc::inotify_event>())
                };
                let name = &buf[at + head..at + head + ev.len as usize];
                let name = name.split(|&b| b == 0).next().unwrap_or_default();
                match name {
                    b"appearance" => hit.appearance = true,
                    b"language" => hit.language = true,
                    b"do-not-disturb" => hit.do_not_disturb = true,
                    _ => {}
                }
                at += head + ev.len as usize;
            }
        }
    }
}

/// Tells GNOME settings and GTK settings.ini about the appearance, for
/// apps that start later and for libadwaita and Chromium. XSETTINGS, which
/// running GTK apps follow, is ours to own and is set by the WM itself.
pub fn propagate(dark: bool) {
    let scheme = if dark { "prefer-dark" } else { "prefer-light" };
    let theme = if dark { DARK_THEME } else { LIGHT_THEME };
    for (key, value) in [("color-scheme", scheme), ("gtk-theme", theme)] {
        let _ = Command::new("gsettings")
            .args(["set", "org.gnome.desktop.interface", key, value])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(reap);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = Path::new(&home).join(".config/gtk-3.0/settings.ini");
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        let new = settings_ini(&old, dark);
        if new != old {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
            let _ = std::fs::write(&path, new);
        }
    }
}

fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || child.wait());
}

/// settings.ini with the theme and the dark preference set, the rest kept.
pub fn settings_ini(old: &str, dark: bool) -> String {
    let theme = if dark { DARK_THEME } else { LIGHT_THEME };
    let set = [
        ("gtk-theme-name", theme.to_string()),
        (
            "gtk-application-prefer-dark-theme",
            if dark { "1" } else { "0" }.to_string(),
        ),
    ];
    let mut lines: Vec<String> = Vec::new();
    let mut in_settings = false;
    let mut seen = [false; 2];
    for line in old.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_settings {
                for (i, (k, v)) in set.iter().enumerate() {
                    if !seen[i] {
                        lines.push(format!("{k}={v}"));
                        seen[i] = true;
                    }
                }
            }
            in_settings = t == "[Settings]";
        }
        if in_settings {
            if let Some(i) = set
                .iter()
                .position(|(k, _)| t.split('=').next().map(str::trim) == Some(k))
            {
                lines.push(format!("{}={}", set[i].0, set[i].1));
                seen[i] = true;
                continue;
            }
        }
        lines.push(line.to_string());
    }
    if !lines.iter().any(|l| l.trim() == "[Settings]") {
        lines.push("[Settings]".to_string());
        in_settings = true;
    }
    if in_settings {
        for (i, (k, v)) in set.iter().enumerate() {
            if !seen[i] {
                lines.push(format!("{k}={v}"));
            }
        }
    }
    lines.join("\n") + "\n"
}

/// The _XSETTINGS_SETTINGS value: Net/ThemeName, little endian.
pub fn xsettings(serial: u32, theme: &str) -> Vec<u8> {
    let pad = |v: &mut Vec<u8>| {
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
    };
    let mut v = vec![0u8, 0, 0, 0];
    v.extend(serial.to_le_bytes());
    v.extend(1u32.to_le_bytes());
    let name = b"Net/ThemeName";
    v.push(1);
    v.push(0);
    v.extend((name.len() as u16).to_le_bytes());
    v.extend(name);
    pad(&mut v);
    v.extend(serial.to_le_bytes());
    v.extend((theme.len() as u32).to_le_bytes());
    v.extend(theme.as_bytes());
    pad(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_reads_appearance_events_after_atomic_replacement() {
        let path =
            std::env::temp_dir().join(format!("shuru-appearance-watch-{}", std::process::id()));
        let watch = Watch::in_dir(&path).unwrap();
        std::fs::write(path.join("other"), "ignored").unwrap();
        assert!(!watch.drain().any());
        std::fs::write(path.join(".appearance.tmp"), "dark\n").unwrap();
        std::fs::rename(path.join(".appearance.tmp"), path.join("appearance")).unwrap();
        assert_eq!(
            watch.drain(),
            Changes {
                appearance: true,
                ..Changes::default()
            }
        );
        assert!(!watch.drain().any());
        std::fs::write(path.join(".language.tmp"), "zh_CN.UTF-8\n").unwrap();
        std::fs::rename(path.join(".language.tmp"), path.join("language")).unwrap();
        assert!(watch.drain().language);
        std::fs::write(path.join("do-not-disturb"), "on\n").unwrap();
        assert!(watch.drain().do_not_disturb);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn only_dark_is_dark() {
        assert!(parse("dark\n"));
        assert!(parse(" Dark "));
        assert!(!parse("light"));
        assert!(!parse(""));
        assert!(!parse("purple"));
    }

    #[test]
    fn settings_ini_keeps_the_rest() {
        let old = "[Settings]\ngtk-theme-name=SuperHQ\ngtk-font-name=Inter 10\n";
        let new = settings_ini(old, true);
        assert_eq!(
            new,
            "[Settings]\ngtk-theme-name=SuperHQ-dark\ngtk-font-name=Inter 10\n\
             gtk-application-prefer-dark-theme=1\n"
        );
        assert_eq!(
            settings_ini("", false),
            "[Settings]\ngtk-theme-name=SuperHQ\ngtk-application-prefer-dark-theme=0\n"
        );
    }

    #[test]
    fn xsettings_is_aligned() {
        let v = xsettings(3, DARK_THEME);
        assert_eq!(v.len() % 4, 0);
        assert_eq!(&v[4..8], &3u32.to_le_bytes());
        assert_eq!(&v[8..12], &1u32.to_le_bytes());
        assert_eq!(&v[16..29], b"Net/ThemeName");
        assert_eq!(&v[v.len() - 12..], b"SuperHQ-dark");
    }
}
