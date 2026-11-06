use std::path::{Path, PathBuf};

use accesskit::{Role, Toggled};

use super::*;
use crate::draw::Rect;

struct Directory(PathBuf);

impl Directory {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("superhq-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn app(id: &str, name: &str, category: &str) -> App {
    crate::apps::parse(
        id,
        &format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec={id}\nCategories={category};\n"
        ),
    )
    .unwrap()
}

fn preferences(dir: &Path) -> Preferences {
    Preferences {
        defaults: Defaults {
            mimeapps: vec![dir.join("mimeapps.list")],
            terminals: vec![dir.join("xdg-terminals.list")],
        },
        apps: vec![
            app("chromium", "Chromium", "WebBrowser"),
            app("firefox", "Firefox", "WebBrowser"),
            app("terminal", "Terminal", "TerminalEmulator"),
            app("files", "Files", "Utility"),
        ],
        dark: false,
        lang: Lang::En,
        quiet: false,
        os: os_name("ID=debian\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n"),
        open: None,
        error: None,
    }
}

fn tree(p: &Preferences) -> Tree {
    crate::locale::with_lang(p.lang, || p.tree(&mut Font::for_test(), 640.0))
}

fn click(p: &mut Preferences, id: u64) -> Option<u64> {
    crate::locale::with_lang(Lang::En, || p.click(id))
}

#[test]
fn choosing_a_browser_opens_the_list_saves_and_returns_the_focus() {
    let dir = Directory::new("browser");
    let mut p = preferences(&dir.0);
    assert_eq!(click(&mut p, BROWSER), None);
    assert_eq!(p.open, Some(BROWSER));
    let t = tree(&p);
    assert_eq!(t.semantic(BROWSERS + 1).unwrap().label(), Some("Firefox"));
    assert!(
        t.semantic(BROWSERS + 2).is_none(),
        "only browsers are listed"
    );
    assert_eq!(click(&mut p, BROWSERS + 1), Some(BROWSER));
    assert_eq!(p.open, None);
    assert_eq!(p.defaults.browser().as_deref(), Some("firefox.desktop"));
    assert_eq!(tree(&p).semantic(BROWSER).unwrap().value(), Some("Firefox"));
}

#[test]
fn choosing_a_terminal_writes_the_xdg_list() {
    let dir = Directory::new("terminal");
    let mut p = preferences(&dir.0);
    click(&mut p, TERMINAL);
    assert_eq!(click(&mut p, TERMINALS), Some(TERMINAL));
    assert_eq!(p.defaults.terminal().as_deref(), Some("terminal.desktop"));
}

#[test]
fn a_failed_save_shows_as_an_assertive_error() {
    let dir = Directory::new("error");
    let mut p = preferences(&dir.0);
    std::fs::write(dir.0.join("blocked"), "").unwrap();
    p.defaults.mimeapps = vec![dir.0.join("blocked/mimeapps.list")];
    click(&mut p, BROWSER);
    click(&mut p, BROWSERS);
    let error = p.error.clone().unwrap();
    assert!(error.starts_with("Could not save the setting"), "{error}");
    let t = tree(&p);
    let status = t.semantic(STATUS).unwrap();
    assert_eq!(status.role(), Role::Status);
    assert_eq!(status.live(), Some(accesskit::Live::Assertive));
    click(&mut p, BROWSER);
    assert!(p.error.is_none(), "the next action clears it");
}

#[test]
fn the_form_is_in_the_language_of_the_desktop() {
    let dir = Directory::new("language");
    let mut p = preferences(&dir.0);
    p.lang = Lang::ZhHans;
    let t = tree(&p);
    let json = t.json(Rect::new(0.0, 0.0, 640.0, 2000.0), (0, 0));
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    for name in [
        "设置",
        "通用",
        "外观",
        "浅色",
        "深色",
        "语言",
        "默认应用",
        "浏览器",
        "终端",
        "通知",
        "勿扰模式",
        "关于",
    ] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    assert_eq!(t.semantic(ROOT).unwrap().language(), Some("zh-CN"));
    assert_eq!(t.semantic(LANGUAGES).unwrap().language(), Some("en-US"));
    assert_eq!(
        t.semantic(LANGUAGES + 1).unwrap().toggled(),
        Some(Toggled::True)
    );
}

#[test]
fn every_control_is_reachable_in_reading_order() {
    let dir = Directory::new("order");
    let p = preferences(&dir.0);
    let mut t = tree(&p);
    let mut seen = Vec::new();
    for _ in 0..5 {
        t.step(true, 400.0);
        seen.push(t.selected_id().unwrap());
    }
    assert_eq!(
        seen,
        [APPEARANCE, LANGUAGE, BROWSER, TERMINAL, DO_NOT_DISTURB]
    );
}

#[test]
fn do_not_disturb_is_a_switch_with_its_help() {
    let dir = Directory::new("quiet");
    let mut p = preferences(&dir.0);
    let t = tree(&p);
    let s = t.semantic(DO_NOT_DISTURB).unwrap();
    assert_eq!(s.role(), Role::Switch);
    assert_eq!(s.label(), Some("Do not disturb"));
    assert_eq!(s.toggled(), Some(Toggled::False));
    assert!(s.description().is_some_and(|d| d.contains("banner")));
    assert!(s.supports_action(accesskit::Action::Click));
    p.quiet = true;
    let t = tree(&p);
    assert_eq!(
        t.semantic(DO_NOT_DISTURB).unwrap().toggled(),
        Some(Toggled::True)
    );
}

#[test]
fn appearance_and_language_share_the_general_card() {
    let dir = Directory::new("general");
    let t = tree(&preferences(&dir.0));
    let card = t.semantic(GENERAL_CARD).unwrap();
    assert_eq!(card.label(), Some("General"));
    let json = t.json(Rect::new(0.0, 0.0, 640.0, 2000.0), (0, 0));
    let card = json
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == GENERAL_CARD)
        .unwrap();
    let kids: Vec<u64> = card["children"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_u64())
        .collect();
    assert!(
        kids.contains(&APPEARANCE) && kids.contains(&LANGUAGE),
        "{kids:?}"
    );
}

#[test]
fn about_names_the_version_and_system() {
    let dir = Directory::new("about");
    let t = tree(&preferences(&dir.0));
    use crate::ui::kit::info::value_id;
    let version = t.semantic(VERSION).unwrap();
    assert_eq!(version.role(), Role::Group);
    assert_eq!(version.label(), Some("Version"));
    assert_eq!(
        t.semantic(value_id(VERSION)).unwrap().value(),
        Some(format!("shuru-desktop {}", env!("CARGO_PKG_VERSION")).as_str())
    );
    assert_eq!(
        t.semantic(value_id(SYSTEM)).unwrap().value(),
        Some("Debian GNU/Linux 13 (trixie)")
    );
    assert_eq!(os_name(""), "Debian GNU/Linux");
}

#[test]
fn the_desktop_entry_launches_the_client_mode() {
    let app = crate::apps::parse(
        "settings",
        include_str!("../../guest/applications/settings.desktop"),
    )
    .unwrap();
    assert_eq!(app.command(&[]).unwrap(), ["shuru-desktop", "--settings"]);
    assert!(app.owns("superhq-settings", "SuperHQSettings"));
}
