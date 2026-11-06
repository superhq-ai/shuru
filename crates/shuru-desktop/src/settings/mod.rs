//! Settings: a normal X11 client on the same retained UI as the shell,
//! built from `ui::kit`.

#[cfg(target_os = "linux")]
mod client;

use anyhow::Result;

use crate::apps::App;
use crate::defaults::{desktop_id, is_terminal, Defaults};
use crate::draw::Font;
use crate::locale::{tr, Lang, Msg, LANGS};
use crate::ui::kit::{Choice, Form, Segmented, Select, Switch};
use crate::ui::Tree;
use crate::wallpaper;

#[cfg(target_os = "linux")]
pub fn run(display: &str) -> Result<()> {
    client::run(display)
}

pub const ROOT: u64 = 1;
const TITLE: u64 = 2;
const STATUS: u64 = 3;
const GENERAL_HEADING: u64 = 10;
const GENERAL_CARD: u64 = 11;
pub const APPEARANCE: u64 = 12;
pub const LIGHT: u64 = 13;
pub const DARK: u64 = 14;
pub const WALLPAPER: u64 = 15;
/// The first wallpaper of the shell; the others follow.
pub const WALLPAPERS: u64 = 16;
pub const LANGUAGE: u64 = 22;
/// The first language; the others follow in the order of `LANGS`.
pub const LANGUAGES: u64 = 23;
const APPS_HEADING: u64 = 30;
const APPS_CARD: u64 = 31;
pub const BROWSER: u64 = 32;
const BROWSER_LIST: u64 = 33;
pub const TERMINAL: u64 = 34;
const TERMINAL_LIST: u64 = 35;
const ABOUT_HEADING: u64 = 40;
const ABOUT_CARD: u64 = 41;
const VERSION: u64 = 42;
const SYSTEM: u64 = 43;
const NOTIFICATIONS_HEADING: u64 = 50;
const NOTIFICATIONS_CARD: u64 = 51;
pub const DO_NOT_DISTURB: u64 = 52;
/// The options of the browser list, then of the terminal list.
pub const BROWSERS: u64 = 1000;
pub const TERMINALS: u64 = 2000;

pub struct Preferences {
    defaults: Defaults,
    apps: Vec<App>,
    pub dark: bool,
    pub wallpaper: wallpaper::Choice,
    pub lang: Lang,
    /// Do not disturb.
    pub quiet: bool,
    os: String,
    /// The select whose list is open.
    pub open: Option<u64>,
    /// The last failure, shown until the next action.
    pub error: Option<String>,
}

impl Preferences {
    pub fn load() -> Self {
        Self {
            defaults: Defaults::from_env(),
            apps: crate::apps::catalog(),
            dark: crate::appearance::read(),
            wallpaper: wallpaper::read(),
            lang: crate::locale::lang(),
            quiet: crate::dnd::read(),
            os: os_name(&std::fs::read_to_string("/etc/os-release").unwrap_or_default()),
            open: None,
            error: None,
        }
    }

    fn browsers(&self) -> Vec<&App> {
        self.apps.iter().filter(|a| a.is_browser()).collect()
    }

    fn terminals(&self) -> Vec<&App> {
        self.apps.iter().filter(|a| is_terminal(a)).collect()
    }

    /// Activates a control. Answers the control the keyboard focus goes
    /// to after, when it moves: back to a select once an option is chosen.
    pub fn click(&mut self, id: u64) -> Option<u64> {
        self.error = None;
        match self.apply(id) {
            Ok(focus) => focus,
            Err(e) => {
                self.error = Some(format!("{}: {e:#}", tr(Msg::SaveFailed)));
                None
            }
        }
    }

    fn apply(&mut self, id: u64) -> Result<Option<u64>> {
        let pick = |list: Vec<&App>, base: u64| {
            id.checked_sub(base)
                .and_then(|i| list.get(i as usize).map(|a| desktop_id(a)))
        };
        match id {
            LIGHT | DARK => {
                crate::appearance::write(id == DARK)?;
                self.dark = id == DARK;
            }
            _ if id >= WALLPAPERS && id < WALLPAPERS + wallpaper::CHOICES.len() as u64 => {
                let choice = wallpaper::Choice::of(wallpaper::CHOICES[(id - WALLPAPERS) as usize]);
                wallpaper::write(choice)?;
                self.wallpaper = choice;
            }
            DO_NOT_DISTURB => {
                crate::dnd::write(!self.quiet)?;
                self.quiet = !self.quiet;
            }
            BROWSER | TERMINAL => {
                self.open = if self.open == Some(id) {
                    None
                } else {
                    Some(id)
                };
            }
            _ if (LANGUAGES..LANGUAGES + LANGS.len() as u64).contains(&id) => {
                let lang = LANGS[(id - LANGUAGES) as usize];
                crate::locale::write(lang)?;
                crate::locale::set_lang(lang);
                self.lang = lang;
            }
            _ if id >= TERMINALS => {
                if let Some(app) = pick(self.terminals(), TERMINALS) {
                    self.defaults.set_terminal(&app)?;
                }
                self.open = None;
                return Ok(Some(TERMINAL));
            }
            _ if id >= BROWSERS => {
                if let Some(app) = pick(self.browsers(), BROWSERS) {
                    self.defaults.set_browser(&app)?;
                }
                self.open = None;
                return Ok(Some(BROWSER));
            }
            _ => {}
        }
        Ok(None)
    }

    pub fn tree(&self, font: &mut Font, width: f32) -> Tree {
        let mut form = Form::new(ROOT, tr(Msg::Settings), self.lang.tag(), width);
        form.title(TITLE, tr(Msg::Settings));
        if let Some(e) = &self.error {
            form.status(STATUS, e, true);
        }

        let mut s = form.section(GENERAL_HEADING, GENERAL_CARD, tr(Msg::General));
        form.segmented(
            &mut s,
            Segmented {
                id: APPEARANCE,
                label: tr(Msg::Appearance),
                choices: &[
                    Choice::new(LIGHT, tr(Msg::Light)),
                    Choice::new(DARK, tr(Msg::Dark)),
                ],
                chosen: if self.dark { DARK } else { LIGHT },
                description: None,
            },
        );
        let languages: Vec<Choice> = LANGS
            .iter()
            .enumerate()
            .map(|(i, l)| Choice::new(LANGUAGES + i as u64, l.endonym()).in_lang(l.tag()))
            .collect();
        let papers: Vec<Choice> = wallpaper::CHOICES
            .iter()
            .enumerate()
            .map(|(i, name)| {
                Choice::new(
                    WALLPAPERS + i as u64,
                    match *name {
                        "default" => tr(Msg::WallpaperDefault),
                        "none" => tr(Msg::WallpaperNone),
                        other => other,
                    },
                )
            })
            .collect();
        form.segmented(
            &mut s,
            Segmented {
                id: WALLPAPER,
                label: tr(Msg::Wallpaper),
                choices: &papers,
                chosen: WALLPAPERS
                    + wallpaper::CHOICES
                        .iter()
                        .position(|n| *n == self.wallpaper.name())
                        .unwrap_or(0) as u64,
                description: Some(tr(Msg::WallpaperNote)),
            },
        );
        form.segmented(
            &mut s,
            Segmented {
                id: LANGUAGE,
                label: tr(Msg::Language),
                choices: &languages,
                chosen: LANGUAGES + LANGS.iter().position(|l| *l == self.lang).unwrap_or(0) as u64,
                description: Some(tr(Msg::LanguageNote)),
            },
        );

        let mut s = form.section(APPS_HEADING, APPS_CARD, tr(Msg::DefaultApps));
        let lists = [
            (
                BROWSER,
                BROWSER_LIST,
                Msg::Browser,
                self.browsers(),
                BROWSERS,
                self.defaults.browser(),
            ),
            (
                TERMINAL,
                TERMINAL_LIST,
                Msg::Terminal,
                self.terminals(),
                TERMINALS,
                self.defaults.terminal(),
            ),
        ];
        for (id, list, name, apps, base, current) in lists {
            let choices: Vec<Choice> = apps
                .iter()
                .enumerate()
                .map(|(i, a)| Choice::new(base + i as u64, &a.name))
                .collect();
            let chosen = apps
                .iter()
                .position(|a| Some(desktop_id(a)) == current)
                .or((!apps.is_empty()).then_some(0))
                .map(|i| base + i as u64);
            form.select(
                &mut s,
                Select {
                    id,
                    list,
                    label: tr(name),
                    choices: &choices,
                    chosen,
                    open: self.open == Some(id),
                    description: None,
                },
            );
        }

        let mut s = form.section(
            NOTIFICATIONS_HEADING,
            NOTIFICATIONS_CARD,
            tr(Msg::Notifications),
        );
        form.switch(
            &mut s,
            Switch {
                id: DO_NOT_DISTURB,
                label: tr(Msg::DoNotDisturb),
                on: self.quiet,
                description: Some(tr(Msg::DoNotDisturbNote)),
            },
        );

        let mut s = form.section(ABOUT_HEADING, ABOUT_CARD, tr(Msg::About));
        let version = format!("shuru-desktop {}", env!("CARGO_PKG_VERSION"));
        form.info(&mut s, VERSION, tr(Msg::Version), &version);
        form.info(&mut s, SYSTEM, tr(Msg::System), &self.os);
        form.finish(font)
    }
}

fn os_name(text: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|s| s.trim_matches('"').into())
        .unwrap_or_else(|| "Debian GNU/Linux".into())
}

#[cfg(test)]
mod tests;
