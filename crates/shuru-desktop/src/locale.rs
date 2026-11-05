//! The language of the shell: a built in string table with English as the
//! fallback, and the locale names .desktop keys are matched against.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    ZhHans,
}

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
}

/// The messages locale: LC_ALL, then LC_MESSAGES, then LANG.
pub fn messages_locale() -> String {
    locale_from(|k| std::env::var(k).ok())
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

pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(|| lang_of(&messages_locale()))
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
        ] {
            assert_ne!(text(Lang::ZhHans, m), text(Lang::En, m));
        }
    }
}
