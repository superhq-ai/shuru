//! `shuru checkpoint update` (issue #44). Every test boots real VMs, so all
//! are #[ignore]d. Run with:
//!
//!   SHURU_BIN=target/debug/shuru cargo test -p shuru-cli --test checkpoint_update -- --ignored
//!
//! SHURU_BIN must be a codesigned binary (see tests/docker.rs). SHURU_DATA_DIR,
//! SHURU_KERNEL and SHURU_INITRD are inherited by the CLI.

use std::process::{Command, Output, Stdio};

fn shuru_bin() -> String {
    std::env::var("SHURU_BIN")
        .expect("SHURU_BIN not set: point it at a codesigned shuru binary (e.g. just build)")
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

fn shuru(args: &[&str]) -> Output {
    let out = Command::new(shuru_bin())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to spawn shuru");
    eprintln!("$ shuru {}\n{}", args.join(" "), text(&out));
    out
}

fn unique(prefix: &str) -> String {
    format!("{}-{}", prefix, std::process::id())
}

struct Cleanup(Vec<String>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for name in &self.0 {
            let _ = Command::new(shuru_bin())
                .args(["checkpoint", "delete", name])
                .output();
        }
    }
}

fn files_in(name: &str) -> String {
    let out = shuru(&["run", "--from", name, "--", "ls", "/opt/m"]);
    assert!(out.status.success(), "{}", text(&out));
    stdout(&out).trim().to_string()
}

#[test]
#[ignore]
fn update_creates_replaces_layers_and_keeps_the_old_one_on_failure() {
    let name = unique("shuru-test-update");
    let _cleanup = Cleanup(vec![name.clone(), format!("{}.update", name)]);

    // Missing checkpoint: update behaves like create.
    let out = shuru(&[
        "checkpoint",
        "update",
        &name,
        "--",
        "sh",
        "-c",
        "mkdir -p /opt/m && touch /opt/m/one",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stderr(&out).contains(&format!("checkpoint '{}' saved", name)),
        "{}",
        text(&out)
    );
    assert_eq!(files_in(&name), "one");

    // Plain update starts from the base image again.
    let out = shuru(&[
        "checkpoint",
        "update",
        &name,
        "--",
        "sh",
        "-c",
        "mkdir -p /opt/m && touch /opt/m/two",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        stderr(&out).contains(&format!("checkpoint '{}' updated", name)),
        "{}",
        text(&out)
    );
    assert_eq!(files_in(&name), "two");

    // --from NAME layers the command on top of the existing checkpoint.
    let out = shuru(&[
        "checkpoint",
        "update",
        &name,
        "--from",
        &name,
        "--",
        "touch",
        "/opt/m/three",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(files_in(&name), "three\ntwo");

    // A failing command leaves the old checkpoint in place and no staging copy.
    let out = shuru(&[
        "checkpoint",
        "update",
        &name,
        "--from",
        &name,
        "--",
        "sh",
        "-c",
        "touch /opt/m/four; exit 3",
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    let e = stderr(&out);
    assert!(e.contains("exited with status 3"), "{}", e);
    assert!(
        e.contains(&format!("checkpoint '{}' unchanged", name)),
        "{}",
        e
    );
    assert_eq!(files_in(&name), "three\ntwo");
    let list = stdout(&shuru(&["checkpoint", "list"]));
    assert!(!list.contains(&format!("{}.update", name)), "{}", list);

    // create still refuses to overwrite and points at update.
    let out = shuru(&["checkpoint", "create", &name, "--", "true"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        stderr(&out).contains("shuru checkpoint update"),
        "{}",
        text(&out)
    );
}
