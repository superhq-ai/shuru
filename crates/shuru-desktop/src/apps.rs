//! Apps from `.desktop` files: reading them, finding one by name, and
//! turning an Exec line into a command.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct App {
    /// The file name less `.desktop`, such as `org.gnome.Calculator`.
    pub id: String,
    /// The Name for the locale of the shell.
    pub name: String,
    /// The Name without a locale.
    pub plain_name: String,
    pub exec: String,
    pub wm_class: Option<String>,
    pub terminal: bool,
    pub categories: Vec<String>,
    /// The Icon key: a name in the icon themes or a path.
    pub icon: Option<String>,
    pub generic: Option<String>,
    /// The Desktop Actions, in the order of the Actions key.
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub name: String,
    pub exec: String,
}

impl App {
    pub fn is_browser(&self) -> bool {
        self.categories.iter().any(|c| c == "WebBrowser")
    }

    /// Whether a window with this WM_CLASS belongs to the app.
    pub fn owns(&self, instance: &str, class: &str) -> bool {
        let same = |a: &str| {
            !a.is_empty() && (a.eq_ignore_ascii_case(instance) || a.eq_ignore_ascii_case(class))
        };
        if let Some(c) = &self.wm_class {
            return same(c);
        }
        same(&self.id)
            || same(self.id.rsplit('.').next().unwrap_or_default())
            || self.program().is_some_and(|p| same(&p))
    }

    /// The base name of the program the Exec line runs.
    pub fn program(&self) -> Option<String> {
        let first = split_exec(&self.exec).ok()?.into_iter().next()?;
        let base = first.rsplit('/').next().unwrap_or(&first).to_string();
        Some(base)
    }

    /// Whether `name` names this app: its id, its Name or its program.
    pub fn is_named(&self, name: &str) -> bool {
        self.id.eq_ignore_ascii_case(name)
            || self.name.eq_ignore_ascii_case(name)
            || self.plain_name.eq_ignore_ascii_case(name)
            || self.program().is_some_and(|p| p.eq_ignore_ascii_case(name))
    }

    /// The argv to start the app with `args` (files or URLs).
    pub fn command(&self, args: &[String]) -> Result<Vec<String>> {
        self.expand(&self.exec, args)
    }

    /// The argv of one of its Desktop Actions.
    pub fn action_command(&self, i: usize) -> Result<Vec<String>> {
        let action = self.actions.get(i).context("no such action")?;
        self.expand(&action.exec, &[])
    }

    fn expand(&self, exec: &str, args: &[String]) -> Result<Vec<String>> {
        let mut argv = Vec::new();
        let mut took_args = false;
        for word in split_exec(exec)? {
            match word.as_str() {
                "%F" | "%U" => {
                    argv.extend(args.iter().cloned());
                    took_args = true;
                    continue;
                }
                "%i" | "%k" => continue,
                _ => {}
            }
            let mut out = String::new();
            let mut chars = word.chars();
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('%') => out.push('%'),
                    Some('f' | 'u') => {
                        if let Some(a) = args.first() {
                            out.push_str(a);
                        }
                        took_args = true;
                    }
                    Some('c') => out.push_str(&self.name),
                    Some(_) | None => {}
                }
            }
            if !out.is_empty() {
                argv.push(out);
            }
        }
        // An Exec line with no field code takes the args at its end.
        if !took_args {
            argv.extend(args.iter().cloned());
        }
        if argv.is_empty() {
            bail!("{} has an empty Exec line", self.id);
        }
        if self.terminal {
            argv.splice(0..0, ["xterm".to_string(), "-e".to_string()]);
        }
        Ok(argv)
    }
}

/// Reads a `.desktop` file. None for files that are not applications.
pub fn read(path: &Path) -> Result<Option<App>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
    let id = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .trim_end_matches(".desktop")
        .to_string();
    Ok(parse(&id, &text))
}

pub fn parse(id: &str, text: &str) -> Option<App> {
    parse_in(
        id,
        text,
        &crate::locale::desktop_keys(&crate::locale::messages_locale()),
    )
}

/// A localized value: the best rank seen so far, lower is better, and the
/// value.
type Local = Option<(usize, String)>;

fn keep(slot: &mut Local, rank: usize, value: &str) {
    if slot.as_ref().is_none_or(|(r, _)| rank < *r) {
        *slot = Some((rank, value.to_string()));
    }
}

/// `Key` or `Key[locale]` with the rank of its locale in `locales`; a key
/// for another locale has none.
fn split_key<'a>(key: &'a str, locales: &[String]) -> Option<(&'a str, usize)> {
    match key.split_once('[') {
        None => Some((key, locales.len())),
        Some((base, rest)) => {
            let loc = rest.strip_suffix(']')?;
            locales.iter().position(|l| l == loc).map(|i| (base, i))
        }
    }
}

fn parse_in(id: &str, text: &str, locales: &[String]) -> Option<App> {
    // The section a line is in: the entry, an action by its id, or another.
    let mut section: Option<Option<String>> = None;
    let (mut exec, mut wm_class, mut icon) = (None, None, None);
    let (mut name, mut plain_name, mut generic): (Local, Option<String>, Local) =
        (None, None, None);
    let mut terminal = false;
    let mut kind = None;
    let mut categories = Vec::new();
    let mut order: Vec<String> = Vec::new();
    let mut actions: Vec<(String, Local, Option<String>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = if line == "[Desktop Entry]" {
                Some(None)
            } else {
                line.strip_prefix("[Desktop Action ")
                    .and_then(|r| r.strip_suffix(']'))
                    .map(|a| Some(a.to_string()))
            };
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unescape(value.trim());
        let Some((key, rank)) = split_key(key.trim(), locales) else {
            continue;
        };
        let plain = rank == locales.len();
        match &section {
            Some(None) => match key {
                "Name" => {
                    keep(&mut name, rank, &value);
                    if plain {
                        plain_name = Some(value);
                    }
                }
                "GenericName" => keep(&mut generic, rank, &value),
                _ if !plain => {}
                "Exec" => exec = Some(value),
                "StartupWMClass" => wm_class = Some(value),
                "Terminal" => terminal = value == "true",
                "Type" => kind = Some(value),
                "Icon" if !value.is_empty() => icon = Some(value),
                "Categories" => categories = list(&value),
                "Actions" => order = list(&value),
                _ => {}
            },
            Some(Some(a)) => {
                let i = match actions.iter().position(|(x, _, _)| x == a) {
                    Some(i) => i,
                    None => {
                        actions.push((a.clone(), None, None));
                        actions.len() - 1
                    }
                };
                match key {
                    "Name" => keep(&mut actions[i].1, rank, &value),
                    "Exec" if plain => actions[i].2 = Some(value),
                    _ => {}
                }
            }
            None => {}
        }
    }
    if kind.as_deref() != Some("Application") {
        return None;
    }
    let actions = order
        .iter()
        .filter_map(|id| actions.iter().find(|(x, _, _)| x == id))
        .filter_map(|(_, n, e)| {
            Some(Action {
                name: n.clone()?.1,
                exec: e.clone()?,
            })
        })
        .collect();
    Some(App {
        id: id.to_string(),
        name: name.map_or_else(|| id.to_string(), |(_, n)| n),
        plain_name: plain_name.unwrap_or_else(|| id.to_string()),
        exec: exec?,
        wm_class,
        terminal,
        categories,
        icon,
        generic: generic.map(|(_, g)| g),
        actions,
    })
}

fn list(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// The escapes of a string value: `\s`, `\n`, `\t`, `\r`, `\\`.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// Splits an Exec line into words: spaces part them, double quotes group,
/// and in quotes a backslash escapes `"`, `` ` ``, `$` and `\`.
pub fn split_exec(exec: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '`' | '$' | '\\')) => word.push(e),
                            Some(e) => {
                                word.push('\\');
                                word.push(e);
                            }
                            None => bail!("an Exec line ends in a quote"),
                        },
                        Some(o) => word.push(o),
                        None => bail!("an Exec line has an open quote"),
                    }
                }
            }
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            o => {
                word.push(o);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The application directories of the XDG data dirs.
fn dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(h) if !h.is_empty() => out.push(PathBuf::from(h)),
        _ => {
            if let Some(home) = std::env::var_os("HOME") {
                out.push(PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let data = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    out.extend(data.split(':').map(PathBuf::from));
    out.into_iter().map(|d| d.join("applications")).collect()
}

/// An app from a path to a `.desktop` file or an id in the XDG dirs.
pub fn load(spec: &str) -> Result<App> {
    let path = Path::new(spec);
    if spec.contains('/') {
        return read(path)?.with_context(|| format!("{spec} is not an application"));
    }
    let file = format!("{}.desktop", spec.trim_end_matches(".desktop"));
    for dir in dirs() {
        let p = dir.join(&file);
        if p.exists() {
            return read(&p)?.with_context(|| format!("{} is not an application", p.display()));
        }
    }
    bail!("no {file} in the application dirs")
}

/// The installed application choices, with user entries first.
pub fn catalog() -> Vec<App> {
    let mut seen = std::collections::HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            if !seen.insert(entry.file_name()) {
                continue;
            }
            if let Ok(Some(app)) = read(&path) {
                apps.push(app);
            }
        }
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    apps
}

/// Finds an app by name: the configured ones first, then the XDG dirs.
pub fn find(configured: &[App], name: &str) -> Option<App> {
    if let Some(a) = configured.iter().find(|a| a.is_named(name)) {
        return Some(a.clone());
    }
    if let Ok(a) = load(name) {
        return Some(a);
    }
    scan(|a| a.is_named(name))
}

/// The app in the XDG dirs that a window with this WM_CLASS belongs to.
pub fn owner(instance: &str, class: &str) -> Option<App> {
    scan(|a| a.owns(instance, class))
}

fn scan(want: impl Fn(&App) -> bool) -> Option<App> {
    for dir in dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "desktop") {
                if let Ok(Some(a)) = read(&p) {
                    if want(&a) {
                        return Some(a);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(exec: &str) -> App {
        App {
            id: "org.example.Viewer".into(),
            name: "Viewer".into(),
            plain_name: "Viewer".into(),
            exec: exec.into(),
            wm_class: None,
            terminal: false,
            categories: vec![],
            icon: None,
            generic: None,
            actions: vec![],
        }
    }

    #[test]
    fn parses_an_entry() {
        let text = "[Desktop Entry]\nType=Application\nName=Calculator\nName[de]=Rechner\n\
                    Exec=gnome-calculator\nStartupWMClass=gnome-calculator\nCategories=GTK;Utility;\n\
                    Icon=org.gnome.Calculator\nGenericName=Calculator\nActions=new;gone;\n\
                    [Desktop Action new]\nName=New\nExec=other --new\n";
        let a = parse_in("org.gnome.Calculator", text, &[]).unwrap();
        assert_eq!(a.name, "Calculator");
        assert_eq!(a.exec, "gnome-calculator");
        assert_eq!(a.wm_class.as_deref(), Some("gnome-calculator"));
        assert!(a.is_named("calculator"));
        assert!(a.is_named("org.gnome.Calculator"));
        assert!(a.is_named("gnome-calculator"));
        assert!(a.owns("gnome-calculator", "Gnome-calculator"));
        assert!(!a.owns("xterm", "XTerm"));
        assert_eq!(a.categories, vec!["GTK", "Utility"]);
        assert_eq!(a.icon.as_deref(), Some("org.gnome.Calculator"));
        assert_eq!(
            a.actions,
            vec![Action {
                name: "New".into(),
                exec: "other --new".into()
            }]
        );
        assert_eq!(a.action_command(0).unwrap(), vec!["other", "--new"]);
        assert!(!a.is_browser());
    }

    #[test]
    fn localized_keys_follow_the_locale() {
        let text = "[Desktop Entry]\nType=Application\nName=Files\nName[zh]=文件\n\
                    Name[zh_CN]=文件管理\nName[de]=Dateien\nGenericName=File manager\n\
                    GenericName[zh]=文件管理器\nExec=files\nExec[zh_CN]=nope\nActions=w;\n\
                    [Desktop Action w]\nName=New Window\nName[zh_CN]=新建窗口\nExec=files -w\n";
        let zh = parse_in("files", text, &crate::locale::desktop_keys("zh_CN.UTF-8")).unwrap();
        assert_eq!(zh.name, "文件管理");
        assert_eq!(zh.generic.as_deref(), Some("文件管理器"));
        assert_eq!(zh.exec, "files");
        assert_eq!(zh.actions[0].name, "新建窗口");
        assert!(zh.is_named("Files"));
        let sg = parse_in("files", text, &crate::locale::desktop_keys("zh_SG")).unwrap();
        assert_eq!(sg.name, "文件");
        let en = parse_in("files", text, &crate::locale::desktop_keys("en_US.UTF-8")).unwrap();
        assert_eq!(en.name, "Files");
        assert_eq!(en.actions[0].name, "New Window");
    }

    #[test]
    fn skips_links_and_entries_without_exec() {
        assert!(parse("x", "[Desktop Entry]\nType=Link\nName=X\nURL=http://x\n").is_none());
        assert!(parse("x", "[Desktop Entry]\nType=Application\nName=X\n").is_none());
    }

    #[test]
    fn splits_quoted_words() {
        let w = split_exec(r#"sh -c "echo \"hi\" \$HOME" 'x'"#).unwrap();
        assert_eq!(w, vec!["sh", "-c", "echo \"hi\" $HOME", "'x'"]);
        assert!(split_exec(r#"a "b"#).is_err());
    }

    #[test]
    fn expands_field_codes() {
        let args = vec!["a.txt".to_string(), "b.txt".to_string()];
        assert_eq!(
            app("viewer %F").command(&args).unwrap(),
            vec!["viewer", "a.txt", "b.txt"]
        );
        assert_eq!(
            app("viewer %u").command(&args).unwrap(),
            vec!["viewer", "a.txt"]
        );
        assert_eq!(app("viewer %U").command(&[]).unwrap(), vec!["viewer"]);
        assert_eq!(
            app("viewer --title=%c %i 100%%").command(&[]).unwrap(),
            vec!["viewer", "--title=Viewer", "100%"]
        );
        assert_eq!(
            app("/usr/bin/viewer").command(&args).unwrap(),
            vec!["/usr/bin/viewer", "a.txt", "b.txt"]
        );
        assert_eq!(app("/usr/bin/viewer").program().as_deref(), Some("viewer"));
    }

    #[test]
    fn owns_by_program_without_a_class() {
        let a = app("/usr/bin/viewer %f");
        assert!(a.owns("viewer", "Viewer"));
        assert!(a.owns("x", "org.example.viewer"));
        assert!(!a.owns("", ""));
    }

    #[test]
    fn terminal_apps_run_in_xterm() {
        let mut a = app("htop");
        a.terminal = true;
        assert_eq!(a.command(&[]).unwrap(), vec!["xterm", "-e", "htop"]);
    }
}
