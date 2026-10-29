//! Guest services (RFC 0004): long running processes declared by the image
//! under /etc/shuru/services/<name>.json. Init starts them at boot, gates
//! exec requests on their readiness, and stops them on SHUTDOWN_REQ.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use shuru_proto::ShutdownResponse;

use crate::children::{Exit, Watch};

const SERVICES_DIR: &str = "/etc/shuru/services";
const LOG_DIR: &str = "/run/shuru/log";
const READY_POLL: Duration = Duration::from_millis(250);
const DEFAULT_READY_TIMEOUT: u64 = 30;
const DEFAULT_STOP_TIMEOUT: u64 = 15;
const LOG_TAIL_LINES: usize = 20;

#[derive(Clone, Debug, PartialEq)]
enum State {
    Starting,
    Ready,
    Failed(String),
    Stopped,
}

struct Spec {
    name: String,
    exec: Vec<String>,
    ready: Option<Vec<String>>,
    ready_timeout: Duration,
    stop_timeout: Duration,
    env: HashMap<String, String>,
    cwd: String,
}

struct Service {
    name: String,
    child: Option<Watch>,
    exit: Exit,
    state: State,
    stop_timeout: Duration,
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
    Ok(Spec {
        name: name.to_string(),
        exec,
        ready,
        ready_timeout,
        stop_timeout,
        env,
        cwd,
    })
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
        svc.state = state;
    }
    drop(reg);
    CHANGED.notify_all();
}

/// Records the exit of a running service, so one that dies at any point
/// is reported even when no thread is watching it.
fn record_exit(name: &str, status: i32) {
    {
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
        svc.state = State::Failed(describe_exit(status));
    }
    CHANGED.notify_all();
    log_line(&format!("service {} {}", name, describe_exit(status)));
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

fn watch_until_ready(spec: Spec, exit: Exit) {
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
                if ready_check_passes(&spec, ready) {
                    set_state(&spec.name, State::Ready);
                    log_line(&format!("service {} ready", spec.name));
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            let msg = format!("not ready after {}s", spec.ready_timeout.as_secs());
            log_line(&format!("service {} {}", spec.name, msg));
            set_state(&spec.name, State::Failed(msg));
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
        // Registered before its spawn, so an exit at once is recorded.
        registry().services.push(Service {
            name: spec.name.clone(),
            child: None,
            exit: Exit::default(),
            state: State::Starting,
            stop_timeout: spec.stop_timeout,
        });
        let (child, exit) = match spawn(&spec) {
            Ok(started) => started,
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
    started
}

fn log_tail(name: &str) -> String {
    let text = std::fs::read_to_string(log_path(name)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

/// Block until no service is still starting. Ok when every service is ready
/// (or none exist), Err with a message naming the first failed service.
pub fn wait_ready() -> Result<(), String> {
    if registry().shutting_down {
        return Err("guest is shutting down".into());
    }
    let mut reg = registry();
    loop {
        if reg.shutting_down {
            return Err("guest is shutting down".into());
        }
        if reg.services.iter().any(|s| s.state == State::Starting) {
            reg = CHANGED
                .wait_timeout(reg, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner())
                .0;
            continue;
        }
        let failed = reg.services.iter().find_map(|s| match &s.state {
            State::Failed(reason) => Some((s.name.clone(), reason.clone())),
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
