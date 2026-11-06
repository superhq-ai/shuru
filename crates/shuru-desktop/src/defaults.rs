//! Default apps. The browser is the handler of `x-scheme-handler/http`
//! in `mimeapps.list`, as `xdg-open` reads it. The terminal is the first
//! entry of `xdg-terminals.list`, as `xdg-terminal-exec` reads it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::apps::App;

pub const HTTP: &str = "x-scheme-handler/http";
/// What choosing a browser sets, so links and pages open in it.
const BROWSER_TYPES: [&str; 4] = [
    HTTP,
    "x-scheme-handler/https",
    "text/html",
    "application/xhtml+xml",
];

/// Where the defaults are read from, most important first; the first of
/// each list is the one written.
pub struct Defaults {
    pub mimeapps: Vec<PathBuf>,
    pub terminals: Vec<PathBuf>,
}

fn env_dirs(key: &str, fallback: &str) -> Vec<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.into())
        .split(':')
        .map(PathBuf::from)
        .collect()
}

impl Defaults {
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home.join(".config"), PathBuf::from);
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home.join(".local/share"), PathBuf::from);
        let config_dirs = env_dirs("XDG_CONFIG_DIRS", "/etc/xdg");
        let data_dirs = env_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
        let mut mimeapps = vec![config.join("mimeapps.list")];
        mimeapps.extend(config_dirs.iter().map(|d| d.join("mimeapps.list")));
        mimeapps.push(data.join("applications/mimeapps.list"));
        mimeapps.extend(
            data_dirs
                .iter()
                .map(|d| d.join("applications/mimeapps.list")),
        );
        let mut terminals = vec![config.join("xdg-terminals.list")];
        terminals.extend(config_dirs.iter().map(|d| d.join("xdg-terminals.list")));
        Self {
            mimeapps,
            terminals,
        }
    }

    /// The desktop id of the default browser, such as `chromium.desktop`.
    pub fn browser(&self) -> Option<String> {
        self.mimeapps
            .iter()
            .find_map(|p| default_in(&std::fs::read_to_string(p).ok()?, HTTP))
    }

    pub fn set_browser(&self, id: &str) -> Result<()> {
        let path = self
            .mimeapps
            .first()
            .context("no configuration directory")?;
        let mut text = std::fs::read_to_string(path).unwrap_or_default();
        for mime in BROWSER_TYPES {
            text = set_default(&text, mime, id);
        }
        replace(path, &text)
    }

    /// The desktop id of the default terminal.
    pub fn terminal(&self) -> Option<String> {
        self.terminals
            .iter()
            .find_map(|p| first_terminal(&std::fs::read_to_string(p).ok()?))
    }

    /// Puts `id` first in the user list, keeping the rest after it.
    pub fn set_terminal(&self, id: &str) -> Result<()> {
        let path = self
            .terminals
            .first()
            .context("no configuration directory")?;
        let old = std::fs::read_to_string(path).unwrap_or_default();
        let mut text = format!("{id}\n");
        for line in old.lines().filter(|l| l.trim() != id) {
            text.push_str(line);
            text.push('\n');
        }
        replace(path, &text)
    }
}

/// The desktop id of an app.
pub fn desktop_id(app: &App) -> String {
    format!("{}.desktop", app.id)
}

pub fn is_terminal(app: &App) -> bool {
    app.categories.iter().any(|c| c == "TerminalEmulator")
}

/// The command a terminal app runs another command with: its Exec line
/// less field codes, then its X-TerminalArgExec, `-e` when it has none.
pub fn terminal_prefix(terminal: Option<&App>) -> Vec<String> {
    let Some(app) = terminal else {
        return vec!["xterm".into(), "-e".into()];
    };
    let mut argv: Vec<String> = crate::apps::split_exec(&app.exec)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| !(w.len() == 2 && w.starts_with('%')))
        .collect();
    if argv.is_empty() {
        return vec!["xterm".into(), "-e".into()];
    }
    match app.terminal_arg_exec.as_deref() {
        Some("") => {}
        Some(arg) => argv.push(arg.into()),
        None => argv.push("-e".into()),
    }
    argv
}

/// The terminal apps that need one run in.
pub fn terminal_app() -> Option<App> {
    let id = Defaults::from_env().terminal()?;
    crate::apps::load(id.trim_end_matches(".desktop")).ok()
}

fn replace(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("no configuration directory")?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("list");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn first_terminal(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-'))
        .map(|l| l.split(':').next().unwrap_or(l).to_string())
}

pub fn default_in(text: &str, mime: &str) -> Option<String> {
    let mut defaults = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            defaults = line == "[Default Applications]";
            continue;
        }
        if !defaults {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == mime {
                return value
                    .split(';')
                    .map(str::trim)
                    .find(|v| !v.is_empty())
                    .map(String::from);
            }
        }
    }
    None
}

/// Sets `mime` to `app` in the Default Applications section, keeping every
/// other line, comment and section as it was.
pub fn set_default(text: &str, mime: &str, app: &str) -> String {
    let mut out = Vec::new();
    let mut defaults = false;
    let mut found_section = false;
    let mut wrote = false;
    for line in text.lines() {
        if line.trim().starts_with('[') {
            if defaults && !wrote {
                out.push(format!("{mime}={app};"));
                wrote = true;
            }
            defaults = line.trim() == "[Default Applications]";
            found_section |= defaults;
        }
        let ours = defaults
            && line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == mime);
        if !ours {
            out.push(line.into());
        } else if !wrote {
            out.push(format!("{mime}={app};"));
            wrote = true;
        }
    }
    if !found_section {
        out.push("[Default Applications]".into());
    }
    if !wrote {
        out.push(format!("{mime}={app};"));
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shuru-defaults-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_browser_choice_keeps_other_lines_and_wins_over_the_system() {
        let dir = temp("browser");
        let d = Defaults {
            mimeapps: vec![dir.join("user.list"), dir.join("system.list")],
            terminals: Vec::new(),
        };
        std::fs::write(
            &d.mimeapps[1],
            "[Default Applications]\nx-scheme-handler/http=chromium.desktop;\n",
        )
        .unwrap();
        assert_eq!(d.browser().as_deref(), Some("chromium.desktop"));
        std::fs::write(
            &d.mimeapps[0],
            "# Keep this\n[Default Applications]\nimage/png=viewer.desktop;\n[Added Associations]\ntext/plain=editor.desktop;\n",
        )
        .unwrap();
        d.set_browser("firefox.desktop").unwrap();
        assert_eq!(d.browser().as_deref(), Some("firefox.desktop"));
        let text = std::fs::read_to_string(&d.mimeapps[0]).unwrap();
        assert!(text.starts_with("# Keep this\n"));
        assert!(text.contains("image/png=viewer.desktop;\n"));
        assert!(text.contains("[Added Associations]\ntext/plain=editor.desktop;"));
        for mime in BROWSER_TYPES {
            assert_eq!(default_in(&text, mime).as_deref(), Some("firefox.desktop"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_terminal_choice_goes_first_in_the_xdg_list() {
        let dir = temp("terminal");
        let d = Defaults {
            mimeapps: Vec::new(),
            terminals: vec![dir.join("xdg-terminals.list"), dir.join("system.list")],
        };
        std::fs::write(&d.terminals[1], "# system\nterminal.desktop\n").unwrap();
        assert_eq!(d.terminal().as_deref(), Some("terminal.desktop"));
        std::fs::write(&d.terminals[0], "foot.desktop\nkitty.desktop:new-window\n").unwrap();
        d.set_terminal("kitty.desktop").unwrap();
        assert_eq!(d.terminal().as_deref(), Some("kitty.desktop"));
        let text = std::fs::read_to_string(&d.terminals[0]).unwrap();
        assert_eq!(
            text,
            "kitty.desktop\nfoot.desktop\nkitty.desktop:new-window\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_commands_follow_the_terminal_entry() {
        assert_eq!(terminal_prefix(None), ["xterm", "-e"]);
        let mut app = crate::apps::parse(
            "foot",
            "[Desktop Entry]\nType=Application\nName=Foot\nExec=foot %U\nCategories=TerminalEmulator;\n",
        )
        .unwrap();
        assert_eq!(terminal_prefix(Some(&app)), ["foot", "-e"]);
        app.terminal_arg_exec = Some("--".into());
        assert_eq!(terminal_prefix(Some(&app)), ["foot", "--"]);
        app.terminal_arg_exec = Some(String::new());
        assert_eq!(terminal_prefix(Some(&app)), ["foot"]);
        assert!(is_terminal(&app));
    }
}
