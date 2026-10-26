//! Recipes (RFC 0005). Local recipe directories are written on the fly and
//! built into throwaway checkpoints in real VMs, so every test but the
//! first is #[ignore]d. Run with:
//!
//!   SHURU_BIN=target/debug/shuru cargo test -p shuru-cli --test recipes -- --ignored
//!
//! SHURU_BIN must be a codesigned binary (see tests/docker.rs). SHURU_DATA_DIR,
//! SHURU_KERNEL and SHURU_INITRD are inherited by the CLI.
//!
//!   SHURU_DOCKER_RECIPE   path to the docker recipe directory from a checkout
//!                         of superhq-ai/recipes; with SHURU_TEST_NET=1 it is
//!                         built for real and runs a container

use std::path::{Path, PathBuf};
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

/// A local recipe directory. Steps are plain files so a test can edit one
/// and observe the rebuild.
struct LocalRecipe {
    dir: tempfile::TempDir,
}

impl LocalRecipe {
    fn new(name: &str, spec_json: &str, steps: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_json.replace("NAME", name);
        std::fs::write(dir.path().join("recipe.json"), spec).unwrap();
        for (file, body) in steps {
            std::fs::write(dir.path().join(file), body).unwrap();
        }
        LocalRecipe { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn path_str(&self) -> String {
        self.dir.path().to_str().unwrap().to_string()
    }
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

const TWO_STEP_SPEC: &str = r#"{
  "name": "NAME",
  "description": "two steps and a service",
  "disk_size": 2048,
  "services": {
    "hello": {
      "exec": ["sh", "-c", "touch /run/hello-ran; exec sleep infinity"],
      "ready": ["test", "-f", "/run/hello-ran"]
    }
  },
  "steps": ["first.sh", "second.sh"]
}"#;

const FIRST_STEP: &str = "mkdir -p /opt/recipe\necho step1 >> /opt/recipe/log\ntest -f /etc/shuru/services/hello.json && echo service-file-present >> /opt/recipe/log\n";
const SECOND_STEP: &str = "echo step2 >> /opt/recipe/log\ntest -f /run/hello-ran && echo step2-saw-service >> /opt/recipe/log\n";

#[test]
fn show_prints_metadata_and_steps_without_booting() {
    let recipe = LocalRecipe::new(
        "show-test",
        TWO_STEP_SPEC,
        &[("first.sh", FIRST_STEP), ("second.sh", SECOND_STEP)],
    );
    let out = shuru(&["recipe", "show", &recipe.path_str()]);
    assert!(out.status.success(), "{}", text(&out));
    let s = stdout(&out);
    assert!(s.contains("name:        show-test"), "{}", s);
    assert!(s.contains("description: two steps and a service"), "{}", s);
    assert!(s.contains("network:     offline during build"), "{}", s);
    assert!(s.contains("services:    hello"), "{}", s);
    assert!(s.contains("--- /etc/shuru/services/hello.json ---"), "{}", s);
    assert!(s.contains("--- first.sh ---"), "{}", s);
    assert!(s.contains("echo step2 >> /opt/recipe/log"), "{}", s);
}

#[test]
fn show_rejects_bad_recipes() {
    let bad_name = LocalRecipe::new("Bad Name", TWO_STEP_SPEC, &[("first.sh", ""), ("second.sh", "")]);
    let out = shuru(&["recipe", "show", &bad_name.path_str()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("must match [a-z0-9]"), "{}", text(&out));

    let missing_step = LocalRecipe::new("ok", TWO_STEP_SPEC, &[("first.sh", "")]);
    let out = shuru(&["recipe", "show", &missing_step.path_str()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("reading step second.sh"), "{}", text(&out));

    let future = LocalRecipe::new(
        "future",
        r#"{"name": "NAME", "min_shuru_version": "99.0.0"}"#,
        &[("build.sh", "true\n")],
    );
    let out = shuru(&["recipe", "show", &future.path_str()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("needs shuru 99.0.0 or newer"), "{}", text(&out));
}

#[test]
#[ignore]
fn run_builds_once_reuses_and_rebuilds_when_the_recipe_changes() {
    let name = unique("shuru-test-recipe");
    let _cleanup = Cleanup(vec![name.clone()]);
    let recipe = LocalRecipe::new(
        &name,
        TWO_STEP_SPEC,
        &[("first.sh", FIRST_STEP), ("second.sh", SECOND_STEP)],
    );
    let check = "cat /opt/recipe/log; test -f /run/hello-ran && echo RUN_SAW_SERVICE";

    let out = shuru(&["run", "--recipe", &recipe.path_str(), "--", "sh", "-c", check]);
    assert!(out.status.success(), "{}", text(&out));
    let s = stdout(&out);
    let e = stderr(&out);
    assert!(e.contains(&format!("building recipe {} from", name)), "{}", e);
    assert!(e.contains("step 1/2: first.sh"), "{}", e);
    assert!(e.contains("step 2/2: second.sh"), "{}", e);
    assert!(s.contains("step1\nservice-file-present\nstep2\nstep2-saw-service\n"), "{}", s);
    assert!(s.contains("RUN_SAW_SERVICE"), "{}", s);

    let out = shuru(&["run", "--recipe", &recipe.path_str(), "--", "sh", "-c", check]);
    assert!(out.status.success(), "{}", text(&out));
    let e = stderr(&out);
    assert!(e.contains(&format!("using checkpoint {}", name)), "{}", e);
    assert!(!e.contains("building recipe"), "{}", e);

    let list = shuru(&["checkpoint", "list"]);
    let l = stdout(&list);
    let row = l.lines().find(|line| line.starts_with(&name)).unwrap_or("");
    assert!(row.contains(&recipe.path_str()), "{}", l);

    std::fs::write(
        recipe.path().join("second.sh"),
        format!("{}echo step2-v2 >> /opt/recipe/log\n", SECOND_STEP),
    )
    .unwrap();
    let out = shuru(&["run", "--recipe", &recipe.path_str(), "--", "sh", "-c", check]);
    assert!(out.status.success(), "{}", text(&out));
    let e = stderr(&out);
    assert!(e.contains("recipe changed"), "{}", e);
    assert!(e.contains("building recipe"), "{}", e);
    assert!(stdout(&out).contains("step2-v2"), "{}", text(&out));

    let leftovers = stdout(&shuru(&["checkpoint", "list"]));
    assert!(!leftovers.contains(&format!("{}-step1", name)), "{}", leftovers);
}

#[test]
#[ignore]
fn create_builds_under_a_chosen_name_and_refuses_a_command() {
    let name = unique("shuru-test-recipe-named");
    let _cleanup = Cleanup(vec![name.clone()]);
    let recipe = LocalRecipe::new(
        "irrelevant",
        r#"{"name": "NAME", "steps": ["build.sh"]}"#,
        &[("build.sh", "echo built-by-recipe > /opt/marker\n")],
    );

    let out = shuru(&["checkpoint", "create", &name, "--recipe", &recipe.path_str(), "--", "echo", "no"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("drop the command"), "{}", text(&out));

    let out = shuru(&["checkpoint", "create", &name, "--recipe", &recipe.path_str()]);
    assert!(out.status.success(), "{}", text(&out));

    let out = shuru(&["run", "--from", &name, "--", "cat", "/opt/marker"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(stdout(&out).contains("built-by-recipe"), "{}", text(&out));

    let out = shuru(&["checkpoint", "create", &name, "--recipe", &recipe.path_str()]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("already exists"), "{}", text(&out));
}

#[test]
#[ignore]
fn update_rebuilds_a_recipe_checkpoint_at_the_same_revision() {
    let name = unique("shuru-test-recipe-update");
    let _cleanup = Cleanup(vec![name.clone(), format!("{}.update", name)]);
    let recipe = LocalRecipe::new(
        "irrelevant",
        r#"{"name": "NAME", "steps": ["build.sh"]}"#,
        &[("build.sh", "date +%s%N > /opt/built\n")],
    );

    let out = shuru(&["checkpoint", "create", &name, "--recipe", &recipe.path_str()]);
    assert!(out.status.success(), "{}", text(&out));
    let first = stdout(&shuru(&["run", "--from", &name, "--", "cat", "/opt/built"]));
    assert!(!first.trim().is_empty(), "{}", first);

    let out = shuru(&["checkpoint", "update", &name, "--recipe", &recipe.path_str()]);
    assert!(out.status.success(), "{}", text(&out));
    let e = stderr(&out);
    assert!(e.contains("building recipe"), "{}", e);
    assert!(e.contains(&format!("checkpoint '{}' updated", name)), "{}", e);
    let second = stdout(&shuru(&["run", "--from", &name, "--", "cat", "/opt/built"]));
    assert_ne!(first.trim(), second.trim(), "{}", second);

    let list = stdout(&shuru(&["checkpoint", "list"]));
    let row = list.lines().find(|l| l.starts_with(&name)).unwrap_or("");
    assert!(row.contains(&recipe.path_str()), "{}", list);
    assert!(!list.contains(&format!("{}.update", name)), "{}", list);
}

#[test]
#[ignore]
fn failing_step_keeps_no_final_checkpoint() {
    let name = unique("shuru-test-recipe-fail");
    let _cleanup = Cleanup(vec![name.clone(), format!("{}-step1", name)]);
    let recipe = LocalRecipe::new(
        &name,
        r#"{"name": "NAME", "steps": ["ok.sh", "bad.sh"]}"#,
        &[("ok.sh", "echo fine\n"), ("bad.sh", "echo boom >&2\nexit 7\n")],
    );
    let out = shuru(&["run", "--recipe", &recipe.path_str(), "--", "true"]);
    assert!(!out.status.success(), "{}", text(&out));
    let e = stderr(&out);
    assert!(e.contains("step bad.sh exited with status 7"), "{}", e);
    assert!(e.contains(&format!("intermediate checkpoint {}-step1 kept", name)), "{}", e);
    let list = stdout(&shuru(&["checkpoint", "list"]));
    assert!(!list.lines().any(|l| l.starts_with(&format!("{} ", name))), "{}", list);
}

#[test]
#[ignore]
fn docker_recipe_builds_and_runs_a_container() {
    let Ok(dir) = std::env::var("SHURU_DOCKER_RECIPE") else {
        eprintln!("skipped: set SHURU_DOCKER_RECIPE to the docker recipe directory");
        return;
    };
    if !std::env::var("SHURU_TEST_NET").map(|v| v == "1").unwrap_or(false) {
        eprintln!("skipped: set SHURU_TEST_NET=1");
        return;
    }
    let dir = PathBuf::from(dir);
    let name = unique("shuru-test-recipe-docker");
    let _cleanup = Cleanup(vec![name.clone()]);

    let out = shuru(&["checkpoint", "create", &name, "--recipe", dir.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(stderr(&out).contains("step 2/2: pull.sh"), "{}", text(&out));

    let out = shuru(&[
        "run",
        "--from",
        &name,
        "--",
        "sh",
        "-c",
        "docker info --format '{{.ServerVersion}}' && docker run --rm nginxinc/nginx-unprivileged:1.31.7-alpine-slim id -u",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let s = stdout(&out);
    assert!(s.contains("29.9.2"), "{}", s);
    assert!(s.contains("\n101\n"), "{}", s);
}
