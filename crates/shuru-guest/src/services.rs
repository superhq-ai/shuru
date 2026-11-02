//! Guest services (RFC 0004): long running processes declared by the image
//! under /etc/shuru/services/<name>.json. Init starts them at boot, gates
//! exec requests on their readiness, restarts the ones that ask for it,
//! and stops them on SHUTDOWN_REQ.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use shuru_proto::ShutdownResponse;

use crate::children::{Exit, Watch};

const SERVICES_DIR: &str = "/etc/shuru/services";
const LOG_DIR: &str = "/run/shuru/log";
const READY_POLL: Duration = Duration::from_millis(250);
const DEFAULT_READY_TIMEOUT: u64 = 30;
const DEFAULT_STOP_TIMEOUT: u64 = 15;
const LOG_TAIL_LINES: usize = 20;
// /run is a 64 MiB tmpfs, so 16 services with an older file each fit in
// half of it.
const MAX_LOG_BYTES: u64 = 1 << 20;
const LOG_CHECK: Duration = Duration::from_secs(5);
const RESTART_FIRST: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
// A run this long was healthy, so the next restart waits RESTART_FIRST again.
const RESTART_RESET: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
enum State {
    Starting,
    Ready,
    Restarting,
    Failed(String),
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Restart {
    No,
    Always,
}

struct Spec {
    name: String,
    exec: Vec<String>,
    ready: Option<Vec<String>>,
    ready_timeout: Duration,
    stop_timeout: Duration,
    env: HashMap<String, String>,
    cwd: String,
    restart: Restart,
}

struct Service {
    name: String,
    spec: Arc<Spec>,
    child: Option<Watch>,
    exit: Exit,
    state: State,
    stop_timeout: Duration,
    // Commands wait for a restarting service only until it is first ready
    // or this passes; after that it never holds or fails them.
    gate_until: Instant,
    ever_ready: bool,
    started: Instant,
    delay: Duration,
}

struct Registry {
    services: Vec<Service>,
    shutting_down: bool,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    services: Vec::new(),
    shutting_down: false,
});
static CHANGED: Condvar = Condvar::new();

fn log_line(msg: &str) {
    let s = format!("shuru-guest: {}\n", msg);
    unsafe { libc::write(2, s.as_ptr() as *const libc::c_void, s.len()) };
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn argv_field(value: &serde_json::Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(items)) => {
            let mut argv = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) => argv.push(s.to_string()),
                    None => return Err(format!("{} must be an array of strings", key)),
                }
            }
            if argv.is_empty() {
                return Err(format!("{} must not be empty", key));
            }
            Ok(Some(argv))
        }
        Some(_) => Err(format!("{} must be an array of strings", key)),
    }
}

fn secs_field(value: &serde_json::Value, key: &str, default: u64) -> Result<Duration, String> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Duration::from_secs(default)),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Duration::from_secs(n)),
            None => Err(format!("{} must be a non-negative integer", key)),
        },
    }
}

fn parse_spec(name: &str, text: &str) -> Result<Spec, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if !value.is_object() {
        return Err("service file must be a JSON object".into());
    }
    let exec = argv_field(&value, "exec")?.ok_or("exec is required")?;
    let ready = argv_field(&value, "ready")?;
    let ready_timeout = secs_field(&value, "ready_timeout_secs", DEFAULT_READY_TIMEOUT)?;
    let stop_timeout = secs_field(&value, "stop_timeout_secs", DEFAULT_STOP_TIMEOUT)?;
    let mut env = HashMap::new();
    if let Some(obj) = value.get("env") {
        match obj.as_object() {
            Some(map) => {
                for (k, v) in map {
                    match v.as_str() {
                        Some(s) => {
                            env.insert(k.clone(), s.to_string());
                        }
                        None => return Err("env values must be strings".into()),
                    }
                }
            }
            None if obj.is_null() => {}
            None => return Err("env must be an object".into()),
        }
    }
    let cwd = match value.get("cwd") {
        None | Some(serde_json::Value::Null) => "/".to_string(),
        Some(v) => v
            .as_str()
            .map(str::to_string)
            .ok_or("cwd must be a string")?,
    };
    let restart = match value.get("restart") {
        None | Some(serde_json::Value::Null) => Restart::No,
        Some(v) => match v.as_str() {
            Some("no") => Restart::No,
            Some("always") => Restart::Always,
            _ => return Err("restart must be \"no\" or \"always\"".into()),
        },
    };
    Ok(Spec {
        name: name.to_string(),
        exec,
        ready,
        ready_timeout,
        stop_timeout,
        env,
        cwd,
        restart,
    })
}

/// The wait before the next restart: RESTART_FIRST after a healthy run,
/// otherwise double the last wait, up to RESTART_MAX.
fn next_delay(last: Duration, ran: Duration) -> Duration {
    if ran >= RESTART_RESET || last.is_zero() {
        RESTART_FIRST
    } else {
        (last * 2).min(RESTART_MAX)
    }
}

fn load_specs() -> Vec<Spec> {
    let entries = match std::fs::read_dir(SERVICES_DIR) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut specs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) if valid_name(n) => n.to_string(),
            _ => {
                log_line(&format!(
                    "ignoring service file {}: bad name",
                    path.display()
                ));
                continue;
            }
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log_line(&format!("ignoring service {}: {}", name, e));
                continue;
            }
        };
        match parse_spec(&name, &text) {
            Ok(spec) => specs.push(spec),
            Err(e) => log_line(&format!("ignoring service {}: {}", name, e)),
        }
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

fn log_path(name: &str) -> String {
    format!("{}/{}.log", LOG_DIR, name)
}

fn spawn(spec: &Spec) -> std::io::Result<(Watch, Exit)> {
    std::fs::create_dir_all(LOG_DIR)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(&spec.name))?;
    let log_err = log.try_clone()?;
    let mut cmd = Command::new(&spec.exec[0]);
    cmd.args(&spec.exec[1..]);
    cmd.envs(&spec.env);
    cmd.current_dir(&spec.cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_err));
    // Own process group so a stop can reach anything the service forked.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let exit = Exit::default();
    let (status, name) = (exit.clone(), spec.name.clone());
    let (_, child) = crate::children::spawn_with(
        &mut cmd,
        Box::new(move |s| {
            status.set(s);
            record_exit(&name, s);
        }),
    )?;
    Ok((child, exit))
}

fn describe_exit(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exited with status {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("killed by signal {}", libc::WTERMSIG(status))
    } else {
        "exited".to_string()
    }
}

fn set_state(name: &str, state: State) {
    let mut reg = registry();
    if let Some(svc) = reg.services.iter_mut().find(|s| s.name == name) {
        // A service that died while its probe ran stays failed.
        if svc.state == State::Stopped || (state == State::Ready && svc.state != State::Starting) {
            return;
        }
        if state == State::Ready {
            svc.ever_ready = true;
        }
        svc.state = state;
    }
    drop(reg);
    CHANGED.notify_all();
}

/// Records the exit of a running service, so one that dies at any point
/// is reported even when no thread is watching it. One that asks for it
/// is started again after its delay.
fn record_exit(name: &str, status: i32) {
    let restart = {
        let mut reg = registry();
        if reg.shutting_down {
            return;
        }
        let running = reg
            .services
            .iter_mut()
            .find(|s| s.name == name && matches!(s.state, State::Starting | State::Ready));
        let Some(svc) = running else {
            return;
        };
        if svc.spec.restart == Restart::Always {
            svc.state = State::Restarting;
            svc.delay = next_delay(svc.delay, svc.started.elapsed());
            Some(svc.delay)
        } else {
            svc.state = State::Failed(describe_exit(status));
            None
        }
    };
    CHANGED.notify_all();
    match restart {
        Some(delay) => {
            log_line(&format!(
                "service {} {}, restarting in {}s",
                name,
                describe_exit(status),
                delay.as_secs()
            ));
            restart_after(name.to_string(), delay);
        }
        None => log_line(&format!("service {} {}", name, describe_exit(status))),
    }
}

fn restart_after(name: String, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        respawn(&name);
    });
}

/// Starts a restarting service again. It spawns under the registry lock,
/// so a shutdown either sees the new process or stops this from running.
fn respawn(name: &str) {
    let mut reg = registry();
    if reg.shutting_down {
        return;
    }
    let Some(svc) = reg
        .services
        .iter_mut()
        .find(|s| s.name == name && s.state == State::Restarting)
    else {
        return;
    };
    let spec = svc.spec.clone();
    match spawn(&spec) {
        Ok((child, exit)) => {
            svc.child = Some(child);
            svc.exit = exit.clone();
            svc.state = State::Starting;
            svc.started = Instant::now();
            drop(reg);
            CHANGED.notify_all();
            log_line(&format!("service {} restarted", name));
            std::thread::spawn(move || watch_until_ready(spec, exit));
        }
        Err(e) => {
            svc.delay = next_delay(svc.delay, Duration::ZERO);
            let delay = svc.delay;
            drop(reg);
            log_line(&format!(
                "service {} failed to start: {}, retrying in {}s",
                name,
                e,
                delay.as_secs()
            ));
            restart_after(name.to_string(), delay);
        }
    }
}

fn ready_check_passes(spec: &Spec, ready: &[String]) -> bool {
    let mut cmd = Command::new(&ready[0]);
    cmd.args(&ready[1..])
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match crate::children::spawn_waited(&mut cmd) {
        Ok((_, _, exit)) => {
            let status = exit.wait();
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
        }
        Err(_) => false,
    }
}

fn watch_until_ready(spec: Arc<Spec>, exit: Exit) {
    let deadline = Instant::now() + spec.ready_timeout;
    loop {
        if exit.get().is_some() {
            return;
        }
        match &spec.ready {
            None => {
                set_state(&spec.name, State::Ready);
                return;
            }
            Some(ready) => {
                let passed = ready_check_passes(&spec, ready);
                // The run this watches may have ended during the probe, and
                // a restart begun since must not be judged by it.
                if exit.get().is_some() {
                    return;
                }
                if passed {
                    set_state(&spec.name, State::Ready);
                    log_line(&format!("service {} ready", spec.name));
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            let msg = format!("not ready after {}s", spec.ready_timeout.as_secs());
            log_line(&format!("service {} {}", spec.name, msg));
            // A restarting service keeps running; only its exit restarts it.
            match spec.restart {
                Restart::Always => set_state(&spec.name, State::Ready),
                Restart::No => set_state(&spec.name, State::Failed(msg)),
            }
            return;
        }
        std::thread::sleep(READY_POLL);
    }
}

/// Spawn every declared service. Returns the number started.
pub fn start_all() -> usize {
    let specs = load_specs();
    let mut started = 0;
    for spec in specs {
        let spec = Arc::new(spec);
        // Registered before its spawn, so an exit at once is recorded.
        let now = Instant::now();
        registry().services.push(Service {
            name: spec.name.clone(),
            spec: spec.clone(),
            child: None,
            exit: Exit::default(),
            state: State::Starting,
            stop_timeout: spec.stop_timeout,
            gate_until: now + spec.ready_timeout,
            ever_ready: false,
            started: now,
            delay: Duration::ZERO,
        });
        let (child, exit) = match spawn(&spec) {
            Ok(started) => started,
            Err(e) if spec.restart == Restart::Always => {
                log_line(&format!(
                    "service {} failed to start: {}, retrying in {}s",
                    spec.name,
                    e,
                    RESTART_FIRST.as_secs()
                ));
                if let Some(svc) = registry().services.iter_mut().find(|s| s.name == spec.name) {
                    svc.state = State::Restarting;
                    svc.delay = RESTART_FIRST;
                }
                restart_after(spec.name.clone(), RESTART_FIRST);
                continue;
            }
            Err(e) => {
                log_line(&format!("service {} failed to start: {}", spec.name, e));
                set_state(&spec.name, State::Failed(format!("failed to start: {}", e)));
                continue;
            }
        };
        if let Some(svc) = registry().services.iter_mut().find(|s| s.name == spec.name) {
            svc.child = Some(child);
            svc.exit = exit.clone();
        }
        started += 1;
        std::thread::spawn(move || watch_until_ready(spec, exit));
    }
    if !registry().services.is_empty() {
        std::thread::spawn(cap_logs);
    }
    started
}

fn cap_logs() {
    loop {
        std::thread::sleep(LOG_CHECK);
        let names: Vec<String> = {
            let reg = registry();
            if reg.shutting_down {
                return;
            }
            reg.services.iter().map(|s| s.name.clone()).collect()
        };
        for name in names {
            if let Err(e) = cap_log(Path::new(&log_path(&name)), MAX_LOG_BYTES) {
                log_line(&format!("service {} log not capped: {}", name, e));
            }
        }
    }
}

/// Moves a log past `max` bytes to `<log>.1` and empties it. The service
/// writes with O_APPEND, so it carries on at the start of the emptied file;
/// lines written between the copy and the truncate are lost.
fn cap_log(path: &Path, max: u64) -> std::io::Result<bool> {
    let len = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len <= max {
        return Ok(false);
    }
    let mut older = path.as_os_str().to_owned();
    older.push(".1");
    std::fs::copy(path, &older)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(0)?;
    Ok(true)
}

fn log_tail(name: &str) -> String {
    let text = std::fs::read_to_string(log_path(name)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

/// Whether commands still wait for this service: one that does not restart
/// until it is ready or failed, one that does until it is first ready or
/// its ready timeout has passed since boot.
fn holds(s: &Service, now: Instant) -> bool {
    match s.spec.restart {
        Restart::No => s.state == State::Starting,
        Restart::Always => {
            !s.ever_ready
                && now < s.gate_until
                && matches!(s.state, State::Starting | State::Restarting)
        }
    }
}

/// Block until no service holds commands. Ok when every service that does
/// not restart is ready (or none exist), Err with a message naming the
/// first one that failed. A restarting service never fails a command.
pub fn wait_ready() -> Result<(), String> {
    if registry().shutting_down {
        return Err("guest is shutting down".into());
    }
    let mut reg = registry();
    loop {
        if reg.shutting_down {
            return Err("guest is shutting down".into());
        }
        let now = Instant::now();
        if reg.services.iter().any(|s| holds(s, now)) {
            reg = CHANGED
                .wait_timeout(reg, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner())
                .0;
            continue;
        }
        let failed = reg.services.iter().find_map(|s| match &s.state {
            State::Failed(reason) if s.spec.restart == Restart::No => {
                Some((s.name.clone(), reason.clone()))
            }
            _ => None,
        });
        drop(reg);
        return match failed {
            None => Ok(()),
            Some((name, reason)) => {
                let tail = log_tail(&name);
                let mut msg = format!("service {} {}", name, reason);
                if !tail.is_empty() {
                    msg.push_str(&format!("\n--- {} ---\n{}\n", log_path(&name), tail));
                }
                Err(msg)
            }
        };
    }
}

/// Stop every running service (SIGTERM, then SIGKILL to its process group
/// after its stop timeout). The disk control path handles synchronization.
pub fn shutdown() -> ShutdownResponse {
    let targets: Vec<(String, Watch, Exit, Duration)> = {
        let mut reg = registry();
        reg.shutting_down = true;
        reg.services
            .iter()
            .filter(|s| matches!(s.state, State::Starting | State::Ready))
            .filter_map(|s| Some((s.name.clone(), s.child?, s.exit.clone(), s.stop_timeout)))
            .collect()
    };
    CHANGED.notify_all();

    let mut resp = ShutdownResponse::default();
    if !targets.is_empty() {
        let start = Instant::now();
        for (_, child, _, _) in &targets {
            crate::children::signal(*child, false, libc::SIGTERM);
        }
        let mut pending = targets;
        loop {
            pending.retain(|(name, _, exit, _)| match exit.get() {
                Some(_) => {
                    resp.stopped.push(name.clone());
                    false
                }
                None => true,
            });
            if pending.is_empty() {
                break;
            }
            let elapsed = start.elapsed();
            let mut expired = Vec::new();
            pending.retain(|(name, child, exit, timeout)| {
                if elapsed >= *timeout {
                    crate::children::signal(*child, true, libc::SIGKILL);
                    crate::children::signal(*child, false, libc::SIGKILL);
                    expired.push((name.clone(), exit.clone()));
                    false
                } else {
                    true
                }
            });
            for (name, exit) in expired {
                log_line(&format!("service {} ignored SIGTERM, killed", name));
                for _ in 0..100 {
                    if exit.get().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                resp.killed.push(name);
            }
            if pending.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        resp.stopped.sort();
        resp.killed.sort();
        for name in resp.stopped.iter().chain(resp.killed.iter()) {
            set_state(name, State::Stopped);
        }
        log_line(&format!(
            "services stopped in {:.1}s (stopped: {:?}, killed: {:?})",
            start.elapsed().as_secs_f64(),
            resp.stopped,
            resp.killed
        ));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("shuru-services-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn restart_defaults_to_no() {
        let spec = parse_spec("web", r#"{"exec": ["/bin/true"]}"#).unwrap();
        assert_eq!(spec.restart, Restart::No);
        let spec = parse_spec("web", r#"{"exec": ["/bin/true"], "restart": null}"#).unwrap();
        assert_eq!(spec.restart, Restart::No);
        let spec = parse_spec("web", r#"{"exec": ["/bin/true"], "restart": "no"}"#).unwrap();
        assert_eq!(spec.restart, Restart::No);
    }

    #[test]
    fn restart_always_parses() {
        let spec = parse_spec("web", r#"{"exec": ["/bin/true"], "restart": "always"}"#).unwrap();
        assert_eq!(spec.restart, Restart::Always);
    }

    #[test]
    fn restart_refuses_other_values() {
        for value in [r#""on-failure""#, "true", "1", r#""Always""#] {
            let text = format!(r#"{{"exec": ["/bin/true"], "restart": {value}}}"#);
            assert!(parse_spec("web", &text).is_err(), "{value}");
        }
    }

    #[test]
    fn restart_waits_double_up_to_a_minute() {
        let mut delay = Duration::ZERO;
        let mut seen = Vec::new();
        for _ in 0..9 {
            delay = next_delay(delay, Duration::from_secs(2));
            seen.push(delay.as_secs());
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn restart_wait_resets_after_a_healthy_run() {
        assert_eq!(
            next_delay(Duration::from_secs(60), RESTART_RESET),
            RESTART_FIRST
        );
        assert_eq!(
            next_delay(
                Duration::from_secs(60),
                RESTART_RESET - Duration::from_millis(1)
            ),
            RESTART_MAX
        );
    }

    #[test]
    fn log_under_the_cap_is_left() {
        let dir = Scratch::new("under");
        let log = dir.0.join("web.log");
        std::fs::write(&log, b"0123456789").unwrap();
        assert!(!cap_log(&log, 10).unwrap());
        assert_eq!(std::fs::read(&log).unwrap(), b"0123456789");
        assert!(!dir.0.join("web.log.1").exists());
    }

    #[test]
    fn log_over_the_cap_moves_to_the_older_file() {
        let dir = Scratch::new("over");
        let log = dir.0.join("web.log");
        std::fs::write(dir.0.join("web.log.1"), b"oldest").unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        std::io::Write::write_all(&mut writer, b"0123456789a").unwrap();
        assert!(cap_log(&log, 10).unwrap());
        assert_eq!(
            std::fs::read(dir.0.join("web.log.1")).unwrap(),
            b"0123456789a"
        );
        std::io::Write::write_all(&mut writer, b"next").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"next");
    }

    #[test]
    fn missing_log_is_not_an_error() {
        let dir = Scratch::new("missing");
        assert!(!cap_log(&dir.0.join("web.log"), 10).unwrap());
    }
}
