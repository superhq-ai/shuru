//! The language of the shell: a built in string table with English as the
//! fallback, and the locale names .desktop keys are matched against.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    ZhHans,
}

impl Lang {
    /// The locale apps are started with.
    pub fn locale(self) -> &'static str {
        match self {
            Lang::En => "en_US.UTF-8",
            Lang::ZhHans => "zh_CN.UTF-8",
        }
    }

    /// The name of the language in itself.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English (US)",
            Lang::ZhHans => "简体中文",
        }
    }

    /// The BCP 47 tag accessibility tools read.
    pub fn tag(self) -> &'static str {
        match self {
            Lang::En => "en-US",
            Lang::ZhHans => "zh-CN",
        }
    }
}

pub const LANGS: [Lang; 2] = [Lang::En, Lang::ZhHans];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    Applications,
    Agent,
    OpenWindows,
    NoMatches,
    Open,
    NewWindow,
    Minimized,
    NoWindows,
    Search,
    Move,
    Close,
    Minimize,
    Maximize,
    Restore,
    Panel,
    Settings,
    Appearance,
    Light,
    Dark,
    Language,
    LanguageNote,
    DefaultApps,
    Browser,
    Terminal,
    About,
    Version,
    System,
    SaveFailed,
    General,
    Notifications,
    DoNotDisturb,
    DoNotDisturbNote,
    ClearAll,
    NoNotifications,
    Wallpaper,
    WallpaperDefault,
    WallpaperNone,
    WallpaperNote,
}

pub fn file() -> PathBuf {
    crate::appearance::dir().join("language")
}

/// The saved language: the one place the language of the desktop lives.
/// The environment only seeds it, see [`seed`].
pub fn messages_locale() -> String {
    saved_in(&file()).map_or_else(
        || locale_from(|k| std::env::var(k).ok()),
        |l| l.locale().into(),
    )
}

fn saved_in(path: &Path) -> Option<Lang> {
    let text = std::fs::read_to_string(path).ok()?;
    LANGS.into_iter().find(|l| l.locale() == text.trim())
}

/// Saves the language from LC_ALL, LC_MESSAGES or LANG when none is saved,
/// so the runtime picks the first language and the desktop owns it after.
pub fn seed() {
    let path = file();
    if saved_in(&path).is_none() {
        let _ = write_in(&path, lang_of(&locale_from(|k| std::env::var(k).ok())));
    }
}

/// Writes the language by a rename, so a watcher never reads half of it.
pub fn write(lang: Lang) -> std::io::Result<()> {
    write_in(&file(), lang)
}

fn write_in(path: &Path, lang: Lang) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".language.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{}\n", lang.locale()))?;
    std::fs::rename(tmp, path)
}

fn locale_from(get: impl Fn(&str) -> Option<String>) -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| get(k))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

pub fn lang_of(locale: &str) -> Lang {
    let base = locale.split(['.', '@']).next().unwrap_or_default();
    match base.replace('-', "_").as_str() {
        "zh_CN" | "zh_SG" | "zh_Hans" | "zh_Hans_CN" | "zh_Hans_SG" => Lang::ZhHans,
        _ => Lang::En,
    }
}

const UNSET: u8 = u8::MAX;
static LANG: AtomicU8 = AtomicU8::new(UNSET);

/// The language of the shell, read from the saved file on first use and
/// changed by [`set_lang`] when the file changes.
pub fn lang() -> Lang {
    #[cfg(test)]
    if let Some(l) = TEST_LANG.with(|l| l.get()) {
        return l;
    }
    match LANG.load(Ordering::Relaxed) {
        UNSET => {
            let l = lang_of(&messages_locale());
            set_lang(l);
            l
        }
        1 => Lang::ZhHans,
        _ => Lang::En,
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_LANG: std::cell::Cell<Option<Lang>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with the language of this thread set to `l`, so tests in
/// parallel do not see each other's language.
#[cfg(test)]
pub fn with_lang<T>(l: Lang, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Lang>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANG.with(|l| l.set(self.0));
        }
    }
    let _restore = Restore(TEST_LANG.with(|t| t.replace(Some(l))));
    f()
}

pub fn set_lang(l: Lang) {
    LANG.store(l as u8, Ordering::Relaxed);
}

/// Rereads the saved language; answers whether it changed.
pub fn reload() -> bool {
    let before = lang();
    let now = lang_of(&messages_locale());
    set_lang(now);
    now != before
}

/// The locale keys to try for `Key[...]` in a .desktop file, best first,
/// as the Desktop Entry spec orders them: lang_COUNTRY@MODIFIER,
/// lang_COUNTRY, lang@MODIFIER, lang.
pub fn desktop_keys(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((r, m)) => (r, Some(m)),
        None => (locale, None),
    };
    let rest = rest.split('.').next().unwrap_or_default();
    if rest.is_empty() || rest == "C" || rest == "POSIX" {
        return Vec::new();
    }
    let (lang, country) = match rest.split_once('_') {
        Some((l, c)) => (l, Some(c)),
        None => (rest, None),
    };
    let mut out = Vec::new();
    if let (Some(c), Some(m)) = (country, modifier) {
        out.push(format!("{lang}_{c}@{m}"));
    }
    if let Some(c) = country {
        out.push(format!("{lang}_{c}"));
    }
    if let Some(m) = modifier {
        out.push(format!("{lang}@{m}"));
    }
    out.push(lang.to_string());
    out
}

pub fn tr(m: Msg) -> &'static str {
    text(lang(), m)
}

pub fn text(lang: Lang, m: Msg) -> &'static str {
    use Msg::*;
    match lang {
        Lang::En => match m {
            Applications => "Applications",
            Agent => "Agent",
            OpenWindows => "Open windows",
            NoMatches => "No matches",
            Open => "Open",
            NewWindow => "New Window",
            Minimized => "Minimized",
            NoWindows => "No windows open",
            Search => "Search apps and windows, or type a URL",
            Move => "Move",
            Close => "Close",
            Minimize => "Minimize",
            Maximize => "Maximize",
            Restore => "Restore",
            Panel => "Panel",
            Settings => "Settings",
            Appearance => "Appearance",
            Light => "Light",
            Dark => "Dark",
            Language => "Language",
            LanguageNote => "Open apps switch language the next time they start",
            DefaultApps => "Default apps",
            Browser => "Browser",
            Terminal => "Terminal",
            About => "About",
            Version => "Version",
            System => "System",
            SaveFailed => "Could not save the setting",
            General => "General",
            Notifications => "Notifications",
            DoNotDisturb => "Do not disturb",
            DoNotDisturbNote => "New notifications go to the notification center without a banner",
            ClearAll => "Clear all",
            NoNotifications => "No notifications",
            Wallpaper => "Wallpaper",
            WallpaperDefault => "Default",
            WallpaperNone => "None",
            WallpaperNote => {
                "A picture from /usr/share/backgrounds, or from \
                ~/.local/share/backgrounds"
            }
        },
        Lang::ZhHans => match m {
            Applications => "应用程序",
            Agent => "智能体",
            OpenWindows => "已打开的窗口",
            NoMatches => "无匹配项",
            Open => "打开",
            NewWindow => "新建窗口",
            Minimized => "已最小化",
            NoWindows => "没有打开的窗口",
            Search => "搜索应用和窗口，或输入网址",
            Move => "移动",
            Close => "关闭",
            Minimize => "最小化",
            Maximize => "最大化",
            Restore => "还原",
            Panel => "面板",
            Settings => "设置",
            Appearance => "外观",
            Light => "浅色",
            Dark => "深色",
            Language => "语言",
            LanguageNote => "已打开的应用将在下次启动时切换语言",
            DefaultApps => "默认应用",
            Browser => "浏览器",
            Terminal => "终端",
            About => "关于",
            Version => "版本",
            System => "系统",
            SaveFailed => "无法保存设置",
            General => "通用",
            Notifications => "通知",
            DoNotDisturb => "勿扰模式",
            DoNotDisturbNote => "新通知将直接进入通知中心，不弹出横幅",
            ClearAll => "全部清除",
            NoNotifications => "没有通知",
            Wallpaper => "壁纸",
            WallpaperDefault => "默认",
            WallpaperNone => "无",
            WallpaperNote => {
                "图片取自 /usr/share/backgrounds 或 \
                ~/.local/share/backgrounds"
            }
        },
    }
}

/// "Hide App" and "Quit App" in the language of the shell.
pub fn hide(app: &str) -> String {
    match lang() {
        Lang::En => format!("Hide {app}"),
        Lang::ZhHans => format!("隐藏 {app}"),
    }
}

pub fn quit(app: &str) -> String {
    match lang() {
        Lang::En => format!("Quit {app}"),
        Lang::ZhHans => format!("退出 {app}"),
    }
}

/// The name of the button that dismisses a notification.
pub fn dismiss(title: &str) -> String {
    match lang() {
        Lang::En => format!("Dismiss {title}"),
        Lang::ZhHans => format!("关闭通知：{title}"),
    }
}

pub fn open_in(app: &str) -> String {
    match lang() {
        Lang::En => format!("Open in {app}"),
        Lang::ZhHans => format!("在 {app} 中打开"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_locale_follows_the_posix_order() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(locale_from(env(&[("LANG", "en_US.UTF-8")])), "en_US.UTF-8");
        assert_eq!(
            locale_from(env(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_MESSAGES", "zh_CN.UTF-8")
            ])),
            "zh_CN.UTF-8"
        );
        assert_eq!(
            locale_from(env(&[("LC_ALL", "C"), ("LC_MESSAGES", "zh_CN.UTF-8")])),
            "C"
        );
    }

    #[test]
    fn the_saved_language_is_seeded_once_and_then_wins() {
        let dir = std::env::temp_dir().join(format!("shuru-language-{}", std::process::id()));
        let path = dir.join("language");
        assert_eq!(saved_in(&path), None);
        write_in(&path, Lang::ZhHans).unwrap();
        assert_eq!(saved_in(&path), Some(Lang::ZhHans));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zh_CN.UTF-8\n");
        std::fs::write(&path, "fr_FR.UTF-8\n").unwrap();
        assert_eq!(saved_in(&path), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn languages_round_trip_through_their_locale() {
        for l in LANGS {
            assert_eq!(lang_of(l.locale()), l);
        }
    }

    #[test]
    fn simplified_chinese_locales_pick_the_table() {
        for l in ["zh_CN.UTF-8", "zh_SG", "zh-Hans", "zh_CN.utf8@pinyin"] {
            assert_eq!(lang_of(l), Lang::ZhHans, "{l}");
        }
        for l in ["", "C", "en_US.UTF-8", "zh_TW.UTF-8", "ja_JP.UTF-8"] {
            assert_eq!(lang_of(l), Lang::En, "{l}");
        }
    }

    #[test]
    fn desktop_keys_go_from_specific_to_general() {
        assert_eq!(desktop_keys("zh_CN.UTF-8"), ["zh_CN", "zh"]);
        assert_eq!(
            desktop_keys("sr_RS.UTF-8@latin"),
            ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert!(desktop_keys("C.UTF-8").is_empty());
        assert!(desktop_keys("").is_empty());
    }

    #[test]
    fn every_message_has_chinese() {
        use Msg::*;
        for m in [
            Applications,
            Agent,
            OpenWindows,
            NoMatches,
            Open,
            NewWindow,
            Minimized,
            NoWindows,
            Search,
            Move,
            Close,
            Minimize,
            Maximize,
            Restore,
            Panel,
            Settings,
            Appearance,
            Light,
            Dark,
            Language,
            LanguageNote,
            DefaultApps,
            Browser,
            Terminal,
            About,
            Version,
            System,
            SaveFailed,
            General,
            Notifications,
            DoNotDisturb,
            DoNotDisturbNote,
            ClearAll,
            NoNotifications,
        ] {
            assert_ne!(text(Lang::ZhHans, m), text(Lang::En, m));
        }
    }
}
