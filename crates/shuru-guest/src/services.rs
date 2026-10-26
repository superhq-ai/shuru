//! Guest services (RFC 0004): long running processes declared by the image
//! under /etc/shuru/services/<name>.json. Init starts them at boot, gates
//! exec requests on their readiness, and stops them on SHUTDOWN_REQ.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use shuru_proto::ShutdownResponse;

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
    pid: i32,
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
                log_line(&format!("ignoring service file {}: bad name", path.display()));
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

fn spawn(spec: &Spec) -> std::io::Result<i32> {
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
    let child = cmd.spawn()?;
    Ok(child.id() as i32)
}

/// Non-blocking exit check. `None` while running, `Some(status)` once the
/// process is gone. A status of -1 means the main loop reaped it first.
fn poll_exit(pid: i32) -> Option<i32> {
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r == pid {
        Some(status)
    } else if r < 0 {
        Some(-1)
    } else {
        None
    }
}

fn describe_exit(status: i32) -> String {
    if status < 0 {
        "exited".to_string()
    } else if libc::WIFEXITED(status) {
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
        if matches!(svc.state, State::Stopped) {
            return;
        }
        svc.state = state;
    }
    drop(reg);
    CHANGED.notify_all();
}

/// Called by the PID 1 reaper for every child it collects, so a service
/// that dies at any point is recorded even when no thread is polling it.
pub fn reaped(pid: i32, status: i32) {
    let name = {
        let reg = registry();
        if reg.shutting_down {
            return;
        }
        reg.services
            .iter()
            .find(|s| s.pid == pid && s.state != State::Stopped)
            .map(|s| s.name.clone())
    };
    if let Some(name) = name {
        log_line(&format!("service {} {}", name, describe_exit(status)));
        set_state(&name, State::Failed(describe_exit(status)));
    }
}

fn ready_check_passes(spec: &Spec, ready: &[String]) -> bool {
    Command::new(&ready[0])
        .args(&ready[1..])
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn watch_until_ready(spec: Spec, pid: i32) {
    let deadline = Instant::now() + spec.ready_timeout;
    loop {
        if let Some(status) = poll_exit(pid) {
            set_state(&spec.name, State::Failed(describe_exit(status)));
            log_line(&format!("service {} {}", spec.name, describe_exit(status)));
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
            let msg = format!(
                "not ready after {}s",
                spec.ready_timeout.as_secs()
            );
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
        let pid = match spawn(&spec) {
            Ok(pid) => pid,
            Err(e) => {
                log_line(&format!("service {} failed to start: {}", spec.name, e));
                registry().services.push(Service {
                    name: spec.name.clone(),
                    pid: 0,
                    state: State::Failed(format!("failed to start: {}", e)),
                    stop_timeout: spec.stop_timeout,
                });
                continue;
            }
        };
        registry().services.push(Service {
            name: spec.name.clone(),
            pid,
            state: State::Starting,
            stop_timeout: spec.stop_timeout,
        });
        started += 1;
        std::thread::spawn(move || watch_until_ready(spec, pid));
    }
    started
}

fn log_tail(name: &str) -> String {
    let text = std::fs::read_to_string(log_path(name)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    lines[start..].join("\n")
}

/// Pick up exits the reaper has not seen yet, so a service that died
/// between two connections is reported on the next exec.
fn poll_all() {
    let pids: Vec<(String, i32)> = registry()
        .services
        .iter()
        .filter(|s| s.pid > 0 && matches!(s.state, State::Starting | State::Ready))
        .map(|s| (s.name.clone(), s.pid))
        .collect();
    for (name, pid) in pids {
        if let Some(status) = poll_exit(pid) {
            if status >= 0 {
                log_line(&format!("service {} {}", name, describe_exit(status)));
                set_state(&name, State::Failed(describe_exit(status)));
            }
        }
    }
}

/// Block until no service is still starting. Ok when every service is ready
/// (or none exist), Err with a message naming the first failed service.
pub fn wait_ready() -> Result<(), String> {
    if registry().shutting_down {
        return Err("guest is shutting down".into());
    }
    poll_all();
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
        let failed = reg
            .services
            .iter()
            .find_map(|s| match &s.state {
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
/// after its stop timeout), then sync the disk.
pub fn shutdown() -> ShutdownResponse {
    let targets: Vec<(String, i32, Duration)> = {
        let mut reg = registry();
        reg.shutting_down = true;
        reg.services
            .iter()
            .filter(|s| s.pid > 0 && matches!(s.state, State::Starting | State::Ready))
            .map(|s| (s.name.clone(), s.pid, s.stop_timeout))
            .collect()
    };
    CHANGED.notify_all();

    let mut resp = ShutdownResponse::default();
    if !targets.is_empty() {
        let start = Instant::now();
        for (_, pid, _) in &targets {
            unsafe { libc::kill(*pid, libc::SIGTERM) };
        }
        let mut pending: Vec<(String, i32, Duration)> = targets;
        loop {
            pending.retain(|(name, pid, _)| match poll_exit(*pid) {
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
            pending.retain(|(name, pid, timeout)| {
                if elapsed >= *timeout {
                    unsafe {
                        libc::kill(-*pid, libc::SIGKILL);
                        libc::kill(*pid, libc::SIGKILL);
                    }
                    expired.push((name.clone(), *pid));
                    false
                } else {
                    true
                }
            });
            for (name, pid) in expired {
                log_line(&format!("service {} ignored SIGTERM, killed", name));
                // Reap the killed process so it does not linger as a zombie.
                for _ in 0..100 {
                    if poll_exit(pid).is_some() {
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
    unsafe { libc::sync() };
    resp
}
