//! Settings for the same retained UI used by the shell.

#[cfg(target_os = "linux")]
mod client;

use std::path::{Path, PathBuf};

use accesskit::Role;
use anyhow::{bail, Result};

use crate::apps::App;
use crate::draw::Font;
use crate::ui::widgets::{Content, Ink, TEXT_SIZE};
use crate::ui::{style, Builder, Node, Tree};

pub const LIGHT: u64 = 101;
pub const DARK: u64 = 102;
pub const ENGLISH: u64 = 103;
pub const CHINESE: u64 = 104;
pub const BROWSER: u64 = 105;
pub const TERMINAL: u64 = 106;
const HTTP: &str = "x-scheme-handler/http";
const TERMINAL_MIME: &str = "x-scheme-handler/terminal";

#[cfg(target_os = "linux")]
pub fn run(display: &str) -> Result<()> {
    client::run(display)
}

pub struct Preferences {
    dir: PathBuf,
    sources: Vec<PathBuf>,
    apps: Vec<App>,
    pub dark: bool,
    language: String,
    os: String,
    pub status: String,
}

impl Preferences {
    pub fn load() -> Self {
        let dir = crate::appearance::dir();
        let config = dir.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut sources = vec![config.join("mimeapps.list")];
        let dirs = std::env::var("XDG_CONFIG_DIRS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/etc/xdg".into());
        sources.extend(dirs.split(':').map(|d| Path::new(d).join("mimeapps.list")));
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
        if let Some(home) = data_home {
            sources.push(home.join("applications/mimeapps.list"));
        }
        let dirs = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        sources.extend(
            dirs.split(':')
                .map(|d| Path::new(d).join("applications/mimeapps.list")),
        );
        Self {
            dir,
            sources,
            apps: crate::apps::catalog(),
            dark: crate::appearance::read(),
            language: crate::locale::messages_locale(),
            os: os_name(&std::fs::read_to_string("/etc/os-release").unwrap_or_default()),
            status: String::new(),
        }
    }

    fn put(&self, name: &str, value: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, value)?;
        std::fs::rename(tmp, self.dir.join(name))?;
        Ok(())
    }

    fn default_id(&self, mime: &str) -> String {
        self.sources
            .iter()
            .find_map(|p| {
                std::fs::read_to_string(p)
                    .ok()
                    .and_then(|s| default_in(&s, mime))
            })
            .unwrap_or_else(|| {
                if mime == HTTP {
                    "chromium.desktop"
                } else {
                    "terminal.desktop"
                }
                .into()
            })
    }

    fn default_label(&self, mime: &str) -> String {
        let id = self.default_id(mime);
        self.apps
            .iter()
            .find(|a| format!("{}.desktop", a.id) == id)
            .map_or_else(
                || id.trim_end_matches(".desktop").into(),
                |a| a.name.clone(),
            )
    }

    fn choices(&self, browser: bool) -> Vec<&App> {
        self.apps
            .iter()
            .filter(|a| {
                if browser {
                    a.is_browser()
                } else {
                    a.categories.iter().any(|c| c == "TerminalEmulator")
                        || a.id == "terminal"
                        || a.program().is_some_and(|p| p == "xterm")
                }
            })
            .collect()
    }

    fn next_default(&self, browser: bool) -> Result<()> {
        let mime = if browser { HTTP } else { TERMINAL_MIME };
        let choices = self.choices(browser);
        if choices.is_empty() {
            bail!("No installed application for this choice");
        }
        let current = self.default_id(mime);
        let next = choices
            .iter()
            .position(|a| format!("{}.desktop", a.id) == current)
            .map_or(0, |i| (i + 1) % choices.len());
        let id = format!("{}.desktop", choices[next].id);
        let path = self
            .sources
            .first()
            .ok_or_else(|| anyhow::anyhow!("No configuration directory"))?;
        let mut text = std::fs::read_to_string(path).unwrap_or_default();
        text = set_default(&text, mime, &id);
        if browser {
            text = set_default(&text, "x-scheme-handler/https", &id);
            text = set_default(&text, "text/html", &id);
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("No configuration directory"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".mimeapps.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn click(&mut self, id: u64) -> Result<()> {
        match id {
            LIGHT | DARK => {
                let dark = id == DARK;
                self.put("appearance", if dark { "dark\n" } else { "light\n" })?;
                self.dark = dark;
                self.status = "Appearance saved".into();
            }
            ENGLISH | CHINESE => {
                let index = usize::from(id == CHINESE);
                let language = crate::locale::SUPPORTED[index].0;
                self.put("language", &format!("{language}\n"))?;
                self.language = language.into();
                self.status = "Language saved. Restart the desktop session to apply".into();
            }
            BROWSER | TERMINAL => {
                self.next_default(id == BROWSER)?;
                self.status = "Default application saved".into();
            }
            _ => bail!("Unsupported setting action"),
        }
        Ok(())
    }

    pub fn tree(&self, font: &mut Font, width: f32) -> Tree {
        let mut b = Builder::new(
            Node::new(
                Content::Box,
                style().column().width(width).pad(24.0, 24.0, 24.0, 24.0),
            )
            .semantic(1, Role::Window, "SuperHQ Settings"),
        );
        heading(&mut b, 200, "Settings", 24.0);
        heading(&mut b, 201, "Appearance", 16.0);
        choice(&mut b, LIGHT, "Light", None, Some(!self.dark), false);
        choice(&mut b, DARK, "Dark", None, Some(self.dark), false);
        heading(&mut b, 202, "Language", 16.0);
        choice(
            &mut b,
            ENGLISH,
            crate::locale::SUPPORTED[0].1,
            None,
            Some(crate::locale::lang_of(&self.language) == crate::locale::Lang::En),
            false,
        );
        choice(
            &mut b,
            CHINESE,
            crate::locale::SUPPORTED[1].1,
            None,
            Some(crate::locale::lang_of(&self.language) == crate::locale::Lang::ZhHans),
            false,
        );
        label(
            &mut b,
            203,
            "Language changes apply after a desktop session restart",
        );
        heading(&mut b, 204, "Default apps", 16.0);
        choice(
            &mut b,
            BROWSER,
            "Browser",
            Some(&self.default_label(HTTP)),
            None,
            self.choices(true).is_empty(),
        );
        choice(
            &mut b,
            TERMINAL,
            "Terminal",
            Some(&self.default_label(TERMINAL_MIME)),
            None,
            self.choices(false).is_empty(),
        );
        label(
            &mut b,
            205,
            "Activate a default app row to choose the next installed app",
        );
        heading(&mut b, 206, "Notifications", 16.0);
        label(&mut b, 207, "Up to 3 cards appear at the top right");
        label(
            &mut b,
            208,
            "Cards expire after 5 seconds unless an app asks otherwise",
        );
        label(
            &mut b,
            209,
            "The desktop keeps the 50 most recent notifications",
        );
        label(&mut b, 210, "Apps can provide notification actions");
        heading(&mut b, 211, "About", 16.0);
        label(&mut b, 212, "SuperHQ");
        label(
            &mut b,
            213,
            &format!("shuru-desktop {}", env!("CARGO_PKG_VERSION")),
        );
        label(&mut b, 214, &self.os);
        if !self.status.is_empty() {
            label(&mut b, 215, &self.status);
        }
        b.finish(font)
    }
}

fn text(value: &str, size: f32, ink: Ink) -> Content {
    Content::Text {
        text: value.into(),
        size,
        ink,
    }
}
fn heading(b: &mut Builder, id: u64, name: &str, size: f32) {
    b.row(
        Node::new(
            text(name, size, Ink::Text),
            style().height(size + 12.0).margin(12.0, 0.0, 4.0, 0.0),
        )
        .semantic(id, Role::Heading, name),
    );
}
fn label(b: &mut Builder, id: u64, name: &str) {
    b.row(
        Node::new(text(name, 12.0, Ink::Dim), style().height(22.0)).semantic(id, Role::Label, name),
    );
}
fn choice(
    b: &mut Builder,
    id: u64,
    name: &str,
    value: Option<&str>,
    selected: Option<bool>,
    disabled: bool,
) {
    let mut node = Node::new(
        Content::RaisedChoice,
        style()
            .row()
            .center()
            .height(36.0)
            .pad(0.0, 12.0, 0.0, 12.0)
            .margin(0.0, 0.0, 6.0, 0.0),
    )
    .semantic(
        id,
        if selected.is_some() {
            Role::RadioButton
        } else {
            Role::Button
        },
        name,
    );
    let semantic = node.semantic.as_mut().unwrap();
    semantic.add_action(accesskit::Action::Focus);
    if disabled {
        semantic.set_disabled();
    }
    if let Some(value) = value {
        semantic.set_value(value);
    }
    if let Some(selected) = selected {
        semantic.set_selected(selected);
        semantic.set_toggled(if selected {
            accesskit::Toggled::True
        } else {
            accesskit::Toggled::False
        });
    }
    let row = b.row(node);
    b.add(
        row,
        Node::new(text(name, TEXT_SIZE, Ink::Text), style().grow()),
    );
    if let Some(value) = value {
        b.add(
            row,
            Node::new(text(value, TEXT_SIZE, Ink::Dim), style().max_width(240.0)),
        );
    }
    if let Some(on) = selected {
        b.add(row, Node::new(Content::Switch { on }, style()));
    }
}

fn os_name(text: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|s| s.trim_matches('"').into())
        .unwrap_or_else(|| "Debian GNU/Linux".into())
}

fn default_in(text: &str, mime: &str) -> Option<String> {
    let mut defaults = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            defaults = line == "[Default Applications]";
            continue;
        }
        if defaults {
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
    }
    None
}

fn set_default(text: &str, mime: &str, app: &str) -> String {
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
        if defaults
            && line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == mime)
        {
            if !wrote {
                out.push(format!("{mime}={app};"));
                wrote = true;
            }
        } else {
            out.push(line.into());
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
    use crate::draw::Rect;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "superhq-settings-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app(id: &str, name: &str, category: &str) -> App {
        App {
            id: id.into(),
            name: name.into(),
            plain_name: name.into(),
            exec: id.into(),
            wm_class: None,
            terminal: false,
            categories: vec![category.into()],
            icon: None,
            generic: None,
            actions: Vec::new(),
        }
    }
    fn preferences(dir: &Path) -> Preferences {
        Preferences {
            dir: dir.join("superhq"),
            sources: vec![dir.join("mimeapps.list")],
            apps: vec![
                app("chromium", "Chromium", "WebBrowser"),
                app("browser", "Browser", "WebBrowser"),
                app("terminal", "Terminal", "TerminalEmulator"),
            ],
            dark: false,
            language: "en_US.UTF-8".into(),
            os: os_name("ID=debian\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n"),
            status: String::new(),
        }
    }

    #[test]
    fn appearance_actions_write_the_watched_file() {
        let dir = Directory::new();
        let mut preferences = preferences(&dir.0);
        preferences.click(DARK).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("superhq/appearance")).unwrap(),
            "dark\n"
        );
        assert!(preferences.dark);
        preferences.click(LIGHT).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("superhq/appearance")).unwrap(),
            "light\n"
        );
        preferences.click(CHINESE).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("superhq/language")).unwrap(),
            "zh_CN.UTF-8\n"
        );
        assert!(preferences.click(999).is_err());
    }

    #[test]
    fn defaults_preserve_other_sections_and_follow_source_precedence() {
        let dir = Directory::new();
        let mut preferences = preferences(&dir.0);
        let lower = dir.0.join("system.list");
        std::fs::write(
            &lower,
            "[Default Applications]\nx-scheme-handler/http=browser.desktop;\n",
        )
        .unwrap();
        preferences.sources.push(lower);
        assert_eq!(preferences.default_label(HTTP), "Browser");
        std::fs::write(&preferences.sources[0], "# Keep this\n[Default Applications]\nx-scheme-handler/http=chromium.desktop;\nimage/png=viewer.desktop;\n[Added Associations]\ntext/plain=editor.desktop;\n").unwrap();
        preferences.click(BROWSER).unwrap();
        assert_eq!(preferences.default_label(HTTP), "Browser");
        let text = std::fs::read_to_string(&preferences.sources[0]).unwrap();
        assert!(text.contains("# Keep this\n"));
        assert!(text.contains("image/png=viewer.desktop;\n"));
        assert!(text.contains("[Added Associations]\ntext/plain=editor.desktop;"));
        assert_eq!(
            default_in(&text, "x-scheme-handler/https").as_deref(),
            Some("browser.desktop")
        );
        preferences.click(TERMINAL).unwrap();
        let text = std::fs::read_to_string(&preferences.sources[0]).unwrap();
        assert_eq!(
            default_in(&text, TERMINAL_MIME).as_deref(),
            Some("terminal.desktop")
        );
    }

    #[test]
    fn settings_layout_semantics_navigation_and_about_are_stable() {
        let dir = Directory::new();
        let preferences = preferences(&dir.0);
        let mut font = Font::for_test();
        let mut tree = preferences.tree(&mut font, 680.0);
        assert_eq!(tree.size().0, 680.0);
        let view = Rect::new(0.0, 0.0, 680.0, 700.0);
        let json = tree.json(view, (0, 0));
        let nodes = json.as_array().unwrap();
        for id in [LIGHT, DARK, ENGLISH, CHINESE, BROWSER, TERMINAL] {
            let node = nodes.iter().find(|n| n["id"] == id).unwrap();
            assert_eq!(node["bounds"]["height"], 36.0);
            assert_eq!(node["bounds"]["width"], 632.0);
            assert!(node["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "click"));
        }
        for name in [
            "Appearance",
            "Language",
            "Default apps",
            "Notifications",
            "About",
            "SuperHQ",
            "Debian GNU/Linux 13 (trixie)",
        ] {
            assert!(nodes.iter().any(|n| n["name"] == name));
        }
        tree.step(true, 700.0);
        assert_eq!(tree.selected_id(), Some(LIGHT));
        tree.step(true, 700.0);
        assert_eq!(tree.selected_id(), Some(DARK));
        let json = tree.json(view, (0, 0));
        assert_eq!(
            json.as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == LIGHT)
                .unwrap()["states"]["selected"],
            true
        );
        let (nodes, _) = tree.accesskit(view, (20, 30));
        let ids: std::collections::HashSet<_> = nodes.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), nodes.len());
        assert_eq!(os_name(""), "Debian GNU/Linux");
    }

    #[test]
    fn desktop_entry_launches_the_client_mode() {
        let app = crate::apps::parse(
            "settings",
            include_str!("../../guest/applications/settings.desktop"),
        )
        .unwrap();
        assert_eq!(app.command(&[]).unwrap(), ["shuru-desktop", "--settings"]);
        assert!(app.owns("superhq-settings", "SuperHQSettings"));
    }
}

#[cfg(test)]
mod accessibility_tests {
    use super::*;
    use accesskit::{ActionHandler, ActionRequest, NodeId, TreeId, TreeInfo, TreeUpdate};
    use accesskit_atspi_common::{
        Adapter, AdapterCallback, AppContext, FullNodeId, InterfaceSet, State, WindowBounds,
    };

    struct Callbacks;
    impl AdapterCallback for Callbacks {
        fn register_interfaces(&self, _: &Adapter, _: FullNodeId, _: InterfaceSet) {}
        fn unregister_interfaces(&self, _: &Adapter, _: FullNodeId, _: InterfaceSet) {}
        fn emit_event(&self, _: &Adapter, _: accesskit_atspi_common::Event) {}
    }
    impl ActionHandler for Callbacks {
        fn do_action(&mut self, _: ActionRequest) {}
    }

    #[test]
    fn settings_exposes_radio_state_and_labels_to_atspi() {
        let preferences = Preferences {
            dir: PathBuf::new(),
            sources: Vec::new(),
            apps: Vec::new(),
            dark: false,
            language: "en_US.UTF-8".into(),
            os: "Debian GNU/Linux".into(),
            status: String::new(),
        };
        let mut font = Font::for_test();
        let tree = preferences.tree(&mut font, 680.0);
        let (nodes, focus) = tree.accesskit(crate::draw::Rect::new(0.0, 0.0, 680.0, 700.0), (0, 0));
        let adapter = Adapter::new(
            &AppContext::new(Some("SuperHQ Settings".into())),
            Callbacks,
            TreeUpdate {
                nodes,
                tree: Some(TreeInfo::new(NodeId(1))),
                tree_id: TreeId::ROOT,
                focus,
            },
            true,
            WindowBounds::default(),
            Callbacks,
        );
        let root = adapter.platform_node(adapter.root_id());
        assert_eq!(root.name().unwrap(), "SuperHQ Settings");
        let children: Vec<_> = root.map_children(|id| adapter.platform_node(id)).unwrap();
        let light = children
            .iter()
            .find(|n| n.name().unwrap() == "Light")
            .unwrap();
        assert!(light.state().contains(State::Checked));
        assert_eq!(light.n_actions().unwrap(), 1);
        assert!(children
            .iter()
            .any(|n| n.name().unwrap() == "Notifications"));
        let browser = tree.row_of(BROWSER).unwrap();
        assert!(!tree.supports(browser, accesskit::Action::Click));
    }
}
