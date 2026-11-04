//! Apps from `.desktop` files: reading them, finding one by name, and
//! turning an Exec line into a command.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct App {
    /// The file name less `.desktop`, such as `org.gnome.Calculator`.
    pub id: String,
    pub name: String,
    pub exec: String,
    pub wm_class: Option<String>,
    pub terminal: bool,
    pub categories: Vec<String>,
}

impl App {
    /// The dock icon for the app, from its categories.
    pub fn icon(&self) -> crate::chrome::Icon {
        use crate::chrome::Icon;
        let has = |c: &str| self.categories.iter().any(|x| x == c);
        if has("WebBrowser") {
            Icon::Globe
        } else if has("TerminalEmulator") {
            Icon::Terminal
        } else if has("FileManager") {
            Icon::Folder
        } else {
            Icon::Page
        }
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
            || self.program().is_some_and(|p| p.eq_ignore_ascii_case(name))
    }

    /// The argv to start the app with `args` (files or URLs).
    pub fn command(&self, args: &[String]) -> Result<Vec<String>> {
        let mut argv = Vec::new();
        let mut took_args = false;
        for word in split_exec(&self.exec)? {
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
    let mut in_entry = false;
    let (mut name, mut exec, mut wm_class) = (None, None, None);
    let mut terminal = false;
    let mut kind = None;
    let mut categories = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unescape(value.trim());
        match key.trim() {
            "Name" => name = Some(value),
            "Exec" => exec = Some(value),
            "StartupWMClass" => wm_class = Some(value),
            "Terminal" => terminal = value == "true",
            "Type" => kind = Some(value),
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
    }
    if kind.as_deref() != Some("Application") {
        return None;
    }
    Some(App {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        exec: exec?,
        wm_class,
        terminal,
        categories,
    })
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

/// Finds an app by name: the configured ones first, then the XDG dirs.
pub fn find(configured: &[App], name: &str) -> Option<App> {
    if let Some(a) = configured.iter().find(|a| a.is_named(name)) {
        return Some(a.clone());
    }
    if let Ok(a) = load(name) {
        return Some(a);
    }
    for dir in dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "desktop") {
                if let Ok(Some(a)) = read(&p) {
                    if a.is_named(name) {
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
            exec: exec.into(),
            wm_class: None,
            terminal: false,
            categories: vec![],
        }
    }

    #[test]
    fn parses_an_entry() {
        let text = "[Desktop Entry]\nType=Application\nName=Calculator\nName[de]=Rechner\n\
                    Exec=gnome-calculator\nStartupWMClass=gnome-calculator\nCategories=GTK;Utility;\n\
                    [Desktop Action new]\nName=New\nExec=other\n";
        let a = parse("org.gnome.Calculator", text).unwrap();
        assert_eq!(a.name, "Calculator");
        assert_eq!(a.exec, "gnome-calculator");
        assert_eq!(a.wm_class.as_deref(), Some("gnome-calculator"));
        assert!(a.is_named("calculator"));
        assert!(a.is_named("org.gnome.Calculator"));
        assert!(a.is_named("gnome-calculator"));
        assert!(a.owns("gnome-calculator", "Gnome-calculator"));
        assert!(!a.owns("xterm", "XTerm"));
        assert_eq!(a.categories, vec!["GTK", "Utility"]);
        assert_eq!(a.icon(), crate::chrome::Icon::Page);
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
