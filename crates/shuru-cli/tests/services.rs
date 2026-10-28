//! Guest services (RFC 0004). Each test builds a throwaway checkpoint from
//! the base rootfs with a small shell service and boots real VMs from it,
//! so every test is #[ignore]d. Run with:
//!
//!   SHURU_BIN=target/debug/shuru cargo test -p shuru-cli --test services -- --ignored
//!
//! SHURU_BIN must be a codesigned binary (see tests/docker.rs). SHURU_DATA_DIR,
//! SHURU_KERNEL and SHURU_INITRD are inherited by the CLI.

use std::process::{Command, Output, Stdio};

use base64::Engine;

fn shuru_bin() -> String {
    std::env::var("SHURU_BIN")
        .expect("SHURU_BIN not set: point it at a codesigned shuru binary (e.g. just build)")
}

fn guest_cmd(script: &str) -> Vec<String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    vec![
        "sh".into(),
        "-c".into(),
        format!("echo {} | base64 -d | sh", b64),
    ]
}

fn text(out: &Output) -> String {
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn shuru(args: &[&str], script: &str) -> Output {
    let out = Command::new(shuru_bin())
        .args(args)
        .arg("--")
        .args(guest_cmd(script))
        .stdin(Stdio::null())
        .output()
        .expect("failed to spawn shuru");
    eprintln!("$ shuru {}\n{}", args.join(" "), text(&out));
    out
}

/// A checkpoint that is deleted when dropped, so a failed assertion does
/// not leave it behind.
struct Checkpoint(String);

impl Checkpoint {
    fn create(suffix: &str, from: Option<&str>, setup: &str) -> Self {
        let name = format!("shuru-test-svc-{}-{}", suffix, std::process::id());
        let mut args = vec!["checkpoint", "create", name.as_str(), "--verbose"];
        if let Some(from) = from {
            args.extend(["--from", from]);
        }
        let out = shuru(&args, setup);
        assert!(
            out.status.success(),
            "checkpoint create failed\n{}",
            text(&out)
        );
        Checkpoint(name)
    }
}

impl Drop for Checkpoint {
    fn drop(&mut self) {
        let _ = Command::new(shuru_bin())
            .args(["checkpoint", "delete", &self.0])
            .output();
    }
}

/// Writes a service file. The JSON is passed through base64 so the shell
/// never sees its quotes.
fn declare(name: &str, json: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    format!(
        "mkdir -p /etc/shuru/services && echo {} | base64 -d > /etc/shuru/services/{}.json\n",
        b64, name
    )
}

#[test]
#[ignore]
fn service_with_delayed_ready_check_gates_the_command() {
    let setup = declare(
        "slow",
        r#"{"exec": ["sh", "-c", "sleep 2; touch /run/slow-ready; exec sleep infinity"],
            "ready": ["test", "-f", "/run/slow-ready"],
            "ready_timeout_secs": 20, "stop_timeout_secs": 5}"#,
    ) + &declare(
        "fast",
        r#"{"exec": ["sh", "-c", "echo fast-started; exec sleep infinity"]}"#,
    );
    let ckpt = Checkpoint::create("gate", None, &setup);

    let out = shuru(
        &["run", "--from", &ckpt.0, "--verbose"],
        r#"
if [ -f /run/slow-ready ]; then echo GATED_OK; else echo GATED_FAIL; fi
echo "fast-log=$(cat /run/shuru/log/fast.log)"
echo "slow-alive=$(pgrep -c -f "sleep infinity")"
"#,
    );
    assert!(out.status.success(), "{}", text(&out));
    let s = stdout(&out);
    assert!(s.contains("GATED_OK"), "{}", text(&out));
    assert!(s.contains("fast-log=fast-started"), "{}", text(&out));
    assert!(s.contains("slow-alive=2"), "{}", text(&out));
    assert!(
        stderr(&out).contains("stopped services: fast, slow"),
        "{}",
        text(&out)
    );
}

#[test]
#[ignore]
fn service_that_fails_blocks_the_command_with_its_log() {
    let setup = declare(
        "bad",
        r#"{"exec": ["sh", "-c", "echo booting bad service; sleep 1; echo giving up; exit 3"],
            "ready": ["false"], "ready_timeout_secs": 10}"#,
    );
    let ckpt = Checkpoint::create("fail", None, &setup);

    let out = shuru(&["run", "--from", &ckpt.0], "echo SHOULD_NOT_RUN");
    assert!(!out.status.success(), "{}", text(&out));
    assert!(!stdout(&out).contains("SHOULD_NOT_RUN"), "{}", text(&out));
    let e = stderr(&out);
    assert!(
        e.contains("service bad exited with status 3"),
        "{}",
        text(&out)
    );
    assert!(e.contains("giving up"), "{}", text(&out));
}

#[test]
#[ignore]
fn shutdown_delivers_sigterm_and_syncs_before_the_checkpoint_is_cloned() {
    let setup = declare(
        "trap",
        r#"{"exec": ["sh", "-c", "trap \"echo got-term > /var/lib/trap-marker; exit 0\" TERM; while true; do sleep 1; done"],
            "stop_timeout_secs": 10}"#,
    );
    let base = Checkpoint::create("term", None, &setup);
    let derived = Checkpoint::create("term-derived", Some(&base.0), "echo RAN_IN_BASE");

    let out = shuru(
        &["run", "--from", &derived.0],
        "cat /var/lib/trap-marker 2>/dev/null || echo NO_MARKER",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(stdout(&out).contains("got-term"), "{}", text(&out));
}

#[test]
#[ignore]
fn service_ignoring_sigterm_is_killed_after_its_stop_timeout() {
    let setup = declare(
        "stubborn",
        r#"{"exec": ["sh", "-c", "trap \"\" TERM; while true; do sleep 1; done"],
            "stop_timeout_secs": 2}"#,
    );
    let ckpt = Checkpoint::create("kill", None, &setup);

    let start = std::time::Instant::now();
    let out = shuru(&["run", "--from", &ckpt.0], "echo RAN");
    let elapsed = start.elapsed();
    assert!(out.status.success(), "{}", text(&out));
    assert!(stdout(&out).contains("RAN"), "{}", text(&out));
    assert!(
        stderr(&out).contains("services ignored SIGTERM and were killed: stubborn"),
        "{}",
        text(&out)
    );
    assert!(elapsed.as_secs() < 30, "stop took {:?}", elapsed);
}
