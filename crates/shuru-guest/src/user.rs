//! The user a command or a service runs as (RFC 0015): looked up in the
//! passwd and group files of the guest before the fork, taken on between
//! the fork and the exec.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

use shuru_proto::{Owner, RunAs};

const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";

struct Ids {
    uid: u32,
    gid: u32,
    groups: Vec<libc::gid_t>,
}

impl Ids {
    /// The groups, the group and the user, in that order, as the group
    /// calls need root.
    fn enter(&self) -> io::Result<()> {
        unsafe {
            if libc::setgroups(self.groups.len(), self.groups.as_ptr()) != 0
                || libc::setgid(self.gid) != 0
                || libc::setuid(self.uid) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// What a child takes on between its fork and its exec, made before the
/// fork: the account, then the directory, entered as the user so it is
/// one the user can enter. A home that is missing, as `/nonexistent` of
/// nobody, starts it in the root, as login does; a directory asked for
/// that cannot be entered is an error.
pub struct Switch {
    ids: Ids,
    dir: CString,
    asked: bool,
}

impl Switch {
    /// Syscalls only, so it is safe after a fork.
    pub fn enter(&self) -> io::Result<()> {
        self.ids.enter()?;
        unsafe {
            if libc::chdir(self.dir.as_ptr()) == 0 {
                return Ok(());
            }
            if !self.asked && libc::chdir(c"/".as_ptr()) == 0 {
                return Ok(());
            }
        }
        Err(io::Error::last_os_error())
    }
}

fn invalid(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e)
}

pub struct Account {
    ids: Ids,
    name: String,
    home: String,
    shell: String,
}

/// The account a request asks for, or None when it names neither a user
/// nor a group and the process stays root.
pub fn of(run_as: &RunAs) -> io::Result<Option<Account>> {
    let (user, group) = (run_as.user.as_ref(), run_as.group.as_ref());
    if user.is_none() && group.is_none() {
        return Ok(None);
    }
    let passwd = fs::read_to_string(PASSWD).unwrap_or_default();
    let groups = fs::read_to_string(GROUP).unwrap_or_default();
    Account::find(&passwd, &groups, user.unwrap_or(&Owner::Id(0)), group).map(Some)
}

struct Entry<'a> {
    name: &'a str,
    id: u32,
    rest: &'a str,
}

/// Each `name:password:id:rest` line of a passwd or group file.
fn entries(text: &str) -> impl Iterator<Item = Entry<'_>> {
    text.lines().filter_map(|line| {
        let mut fields = line.splitn(4, ':');
        let name = fields.next()?;
        fields.next()?;
        let id = fields.next()?.parse().ok()?;
        Some(Entry {
            name,
            id,
            rest: fields.next().unwrap_or(""),
        })
    })
}

/// A name, or a number given as a name, matches by name first, as chown
/// does; only then does a number match an id.
fn find<'a>(text: &'a str, owner: &Owner) -> Option<Entry<'a>> {
    match owner {
        Owner::Id(id) => entries(text).find(|e| e.id == *id),
        Owner::Name(name) => entries(text).find(|e| e.name == name).or_else(|| {
            let id: u32 = name.parse().ok()?;
            entries(text).find(|e| e.id == id)
        }),
    }
}

fn describe(owner: &Owner) -> String {
    match owner {
        Owner::Id(id) => id.to_string(),
        Owner::Name(name) => name.clone(),
    }
}

impl Account {
    fn find(passwd: &str, groups: &str, user: &Owner, group: Option<&Owner>) -> io::Result<Self> {
        let unknown = |what: &str, owner: &Owner| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no {what} {} in the guest", describe(owner)),
            )
        };
        let entry = find(passwd, user).ok_or_else(|| unknown("user", user))?;
        // gid:gecos:home:shell
        let mut rest = entry.rest.splitn(4, ':');
        let primary: u32 = rest
            .next()
            .and_then(|gid| gid.parse().ok())
            .ok_or_else(|| unknown("user", user))?;
        let _gecos = rest.next();
        let home = rest.next().filter(|h| !h.is_empty()).unwrap_or("/");
        let shell = rest.next().filter(|s| !s.is_empty()).unwrap_or("/bin/sh");
        let (gid, list) = match group {
            Some(group) => {
                let gid = find(groups, group)
                    .ok_or_else(|| unknown("group", group))?
                    .id;
                (gid, vec![gid])
            }
            None => {
                let mut list = vec![primary];
                for g in entries(groups) {
                    if g.rest.split(',').any(|member| member == entry.name) && !list.contains(&g.id)
                    {
                        list.push(g.id);
                    }
                }
                (primary, list)
            }
        };
        Ok(Account {
            ids: Ids {
                uid: entry.id,
                gid,
                groups: list,
            },
            name: entry.name.to_string(),
            home: home.to_string(),
            shell: shell.to_string(),
        })
    }

    /// HOME, USER, LOGNAME and SHELL of the account, but those `env` sets.
    pub fn env(&self, env: &HashMap<String, String>) -> Vec<(&'static str, String)> {
        [
            ("HOME", &self.home),
            ("USER", &self.name),
            ("LOGNAME", &self.name),
            ("SHELL", &self.shell),
        ]
        .into_iter()
        .filter(|(key, _)| !env.contains_key(*key))
        .map(|(key, value)| (key, value.clone()))
        .collect()
    }

    /// The `KEY=VALUE` of `env`, for a child that sets its env itself.
    pub fn env_vars(&self, env: &HashMap<String, String>) -> io::Result<Vec<CString>> {
        self.env(env)
            .into_iter()
            .map(|(key, value)| CString::new(format!("{key}={value}")).map_err(invalid))
            .collect()
    }

    /// The switch to the account, in `cwd`, else the home.
    pub fn switch(&self, cwd: Option<&str>) -> io::Result<Switch> {
        Ok(Switch {
            ids: Ids {
                uid: self.ids.uid,
                gid: self.ids.gid,
                groups: self.ids.groups.clone(),
            },
            dir: CString::new(cwd.unwrap_or(&self.home)).map_err(invalid)?,
            asked: cwd.is_some(),
        })
    }

    /// Runs `cmd` as the account, in `cwd` or the home, after the closures
    /// it already has.
    pub fn apply(
        &self,
        cmd: &mut Command,
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> io::Result<()> {
        cmd.envs(self.env(env));
        let switch = self.switch(cwd)?;
        unsafe { cmd.pre_exec(move || switch.enter()) };
        Ok(())
    }

    /// Gives a terminal to the account, as login does: its owner, the tty
    /// group when there is one, and mode 0620.
    pub fn own_terminal(&self, terminal: &impl AsRawFd) -> io::Result<()> {
        let groups = fs::read_to_string(GROUP).unwrap_or_default();
        let tty = find(&groups, &Owner::Name("tty".into())).map_or(self.ids.gid, |g| g.id);
        let fd = terminal.as_raw_fd();
        unsafe {
            if libc::fchown(fd, self.ids.uid, tty) != 0 || libc::fchmod(fd, 0o620) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        desktop:x:1000:1000:Desktop:/home/desktop:/bin/bash\n\
        1234:x:1500:1500::/home/odd:\n\
        svc:x:999:999:::\n";
    const GROUPS: &str = "root:x:0:\ntty:x:5:\naudio:x:29:desktop\n\
        desktop:x:1000:\nvideo:x:44:svc,desktop\n";

    fn account(user: Owner, group: Option<Owner>) -> io::Result<Account> {
        Account::find(PASSWD, GROUPS, &user, group.as_ref())
    }

    #[test]
    fn a_name_takes_its_ids_home_and_groups() {
        let a = account(Owner::Name("desktop".into()), None).unwrap();
        assert_eq!((a.ids.uid, a.ids.gid), (1000, 1000));
        assert_eq!(a.ids.groups, vec![1000, 29, 44]);
        assert_eq!(
            (a.home.as_str(), a.shell.as_str()),
            ("/home/desktop", "/bin/bash")
        );
    }

    #[test]
    fn a_uid_takes_its_entry() {
        let a = account(Owner::Id(1000), None).unwrap();
        assert_eq!(a.name, "desktop");
    }

    #[test]
    fn a_number_as_a_name_is_a_name_first() {
        assert_eq!(
            account(Owner::Name("1234".into()), None).unwrap().ids.uid,
            1500
        );
        assert_eq!(
            account(Owner::Name("1000".into()), None).unwrap().name,
            "desktop"
        );
    }

    #[test]
    fn a_group_replaces_the_groups() {
        let a = account(
            Owner::Name("desktop".into()),
            Some(Owner::Name("video".into())),
        )
        .unwrap();
        assert_eq!((a.ids.gid, a.ids.groups.clone()), (44, vec![44]));
    }

    #[test]
    fn empty_fields_have_defaults() {
        let a = account(Owner::Name("svc".into()), None).unwrap();
        assert_eq!((a.home.as_str(), a.shell.as_str()), ("/", "/bin/sh"));
        assert_eq!(a.ids.groups, vec![999, 44]);
    }

    #[test]
    fn unknown_users_and_groups_are_errors() {
        let e = account(Owner::Name("nobody-at-all".into()), None)
            .err()
            .unwrap();
        assert_eq!(e.to_string(), "no user nobody-at-all in the guest");
        assert!(account(Owner::Id(4242), None).is_err());
        let e = account(Owner::Id(0), Some(Owner::Name("wheel".into())))
            .err()
            .unwrap();
        assert_eq!(e.to_string(), "no group wheel in the guest");
    }

    #[test]
    fn the_env_keeps_what_the_request_sets() {
        let a = account(Owner::Name("desktop".into()), None).unwrap();
        let env = HashMap::from([("HOME".to_string(), "/work".to_string())]);
        let keys: Vec<_> = a.env(&env).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["USER", "LOGNAME", "SHELL"]);
    }
}
