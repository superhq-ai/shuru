//! Docker Engine inside a Shuru VM (RFC 0003). Boots real VMs from the
//! checkpoint built by the docker recipe (`shuru checkpoint create docker
//! --recipe docker`, or `--recipe <checkout>/docker` from a local clone of
//! superhq-ai/recipes), so every test is #[ignore]d. Run with:
//!
//!   SHURU_BIN=target/debug/shuru cargo test -p shuru-cli --test docker -- --ignored
//!
//! Environment:
//!   SHURU_BIN                 codesigned shuru binary (required). cargo test
//!                             relinks target/debug/shuru and drops its
//!                             signature, so sign a copy elsewhere and point
//!                             SHURU_BIN at that.
//!   SHURU_DOCKER_CHECKPOINT   checkpoint name (default "docker")
//!   SHURU_DOCKER_IMAGE        image reference baked into the checkpoint
//!   SHURU_TEST_NET=1          also run the tests that need real egress
//!
//! SHURU_DATA_DIR, SHURU_KERNEL and SHURU_INITRD are inherited by the CLI,
//! so a run can use an isolated kernel and checkpoint directory.
//!
//! dockerd runs as a guest service (RFC 0004): it is up before the command
//! starts and is stopped cleanly before the VM powers off, so the scripts
//! below call `docker` directly.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
const DEFAULT_IMAGE: &str = "nginxinc/nginx-unprivileged:1.31.7-alpine-slim@sha256:a8cb0f33d51fa107c96fd2bd3a71aaf564ec131e9d2bdf8700944f0ed8b170d7";

fn shuru_bin() -> String {
    std::env::var("SHURU_BIN")
        .expect("SHURU_BIN not set: point it at a codesigned shuru binary (e.g. just build)")
}

fn checkpoint() -> String {
    std::env::var("SHURU_DOCKER_CHECKPOINT").unwrap_or_else(|_| "docker".to_string())
}

fn image() -> String {
    std::env::var("SHURU_DOCKER_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn net_tests_enabled() -> bool {
    std::env::var("SHURU_TEST_NET").map(|v| v == "1").unwrap_or(false)
}

/// Build the guest command: the script is base64 encoded so no quoting
/// survives the trip through argv into `sh -c`.
fn guest_cmd(script: &str) -> Vec<String> {
    let script = script.replace("IMAGE", &image());
    let b64 = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    vec![
        "sh".into(),
        "-c".into(),
        format!("echo {} | base64 -d | sh", b64),
    ]
}

/// Runs the script in a fresh VM and echoes the output so a `--nocapture`
/// run doubles as an evidence log.
fn shuru_run(extra: &[&str], script: &str) -> Output {
    let mut cmd = Command::new(shuru_bin());
    cmd.args(["run", "--from", &checkpoint()]);
    cmd.args(extra);
    cmd.arg("--");
    cmd.args(guest_cmd(script));
    cmd.stdin(Stdio::null());
    let out = cmd.output().expect("failed to spawn shuru");
    eprintln!("$ shuru run --from {} {}\n{}", checkpoint(), extra.join(" "), text(&out));
    out
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_get(port: u16, path: &str) -> Option<String> {
    let mut s = TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().unwrap(),
        Duration::from_secs(2),
    )
    .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    )
    .ok()?;
    let mut body = String::new();
    s.read_to_string(&mut body).ok()?;
    Some(body)
}

#[test]
#[ignore]
fn dockerd_runs_pinned_image_as_non_root_with_default_isolation() {
    let out = shuru_run(
        &[],
        r#"
set -e
docker info --format "{{.CgroupDriver}} cgroup-v{{.CgroupVersion}} storage={{.Driver}} kernel={{.KernelVersion}} security={{.SecurityOptions}}"
docker version --format "docker={{.Server.Version}} containerd={{(index .Server.Components 1).Version}} runc={{(index .Server.Components 2).Version}}"
docker image inspect --format "digest={{index .RepoDigests 0}} user={{.Config.User}}" IMAGE
echo "uid=$(docker run --rm IMAGE id -u)"
docker run -d --name web IMAGE > /dev/null
sleep 1
PID=$(docker inspect --format "{{.State.Pid}}" web)
echo "nginx-master-$(grep ^Uid: /proc/$PID/status | tr -s "\t" " ")"
echo "privileged=$(docker inspect --format "{{.HostConfig.Privileged}}" web)"
echo "seccomp=$(docker inspect --format "{{.HostConfig.SecurityOpt}}" web)"
echo "no-new-privs=$(docker exec web grep NoNewPrivs /proc/1/status)"
echo "seccomp-mode=$(docker exec web grep Seccomp: /proc/1/status)"
docker rm -f web > /dev/null
"#,
    );
    assert!(out.status.success(), "{}", text(&out));
    let s = stdout(&out);
    assert!(s.contains("cgroup-v2"), "{}", s);
    assert!(s.contains("storage=overlayfs"), "{}", s);
    assert!(s.contains("name=seccomp,profile=builtin"), "{}", s);
    assert!(s.contains("name=no-new-privileges"), "{}", s);
    assert!(s.contains("digest=nginxinc/nginx-unprivileged@sha256:a8cb0f33"), "{}", s);
    assert!(s.contains("user=101"), "{}", s);
    assert!(s.contains("\nuid=101\n"), "{}", s);
    assert!(s.contains("nginx-master-Uid: 101 101 101 101"), "{}", s);
    assert!(s.contains("privileged=false"), "{}", s);
    assert!(s.contains("seccomp=[]"), "{}", s);
    assert!(s.contains("NoNewPrivs:\t1"), "{}", s);
    assert!(s.contains("Seccomp:\t2"), "{}", s);
}

#[test]
#[ignore]
fn http_through_port_forward_reaches_container() {
    let port = free_port();
    let token = format!("shuru-docker-{}", std::process::id());
    let script = format!(
        r#"
set -e
mkdir -p /srv/www
echo "{token}" > /srv/www/index.html
docker run -d --name web -p 8080:8080 -v /srv/www:/usr/share/nginx/html:ro IMAGE > /dev/null
echo READY
i=0
until docker logs web 2>&1 | grep -q "GET /done"; do
    sleep 0.2
    i=$((i + 1))
    if [ "$i" -ge 300 ]; then echo TIMEOUT; break; fi
done
docker rm -f web > /dev/null
echo STOPPED
"#
    );

    let child = Command::new(shuru_bin())
        .args(["run", "--from", &checkpoint(), "-p", &format!("{}:8080", port), "--"])
        .args(guest_cmd(&script))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn shuru");

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut body = None;
    while Instant::now() < deadline {
        if let Some(b) = http_get(port, "/") {
            if b.contains(&token) {
                body = Some(b);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = http_get(port, "/done");
    let out = child.wait_with_output().unwrap();
    eprintln!("$ shuru run --from {} -p {}:8080\n{}", checkpoint(), port, text(&out));
    if let Some(b) = &body {
        eprintln!("$ GET http://127.0.0.1:{}/\n{}", port, b);
    }

    let body = body.unwrap_or_else(|| panic!("no HTTP response with token on host port {}\n{}", port, text(&out)));
    assert!(body.starts_with("HTTP/1.1 200"), "{}", body);
    assert!(body.contains("Server: nginx"), "{}", body);
    assert!(stdout(&out).contains("STOPPED"), "{}", text(&out));
    assert!(!stdout(&out).contains("TIMEOUT"), "{}", text(&out));
}

#[test]
#[ignore]
fn container_has_no_egress_without_allow_net() {
    let out = shuru_run(
        &[],
        r#"
echo "guest-eth0=$(ip -o link show eth0 2>&1 | head -c 40)"
docker run --rm IMAGE sh -c "wget -T 5 -qO- http://example.com && echo EGRESS_OK || echo EGRESS_BLOCKED"
docker run --rm IMAGE sh -c "nslookup example.com 2>&1 | tail -n 1; wget -T 5 -qO- http://93.184.215.14 && echo IP_OK || echo IP_BLOCKED"
"#,
    );
    let s = stdout(&out);
    assert!(s.contains("guest-eth0=Device \"eth0\" does not exist"), "{}", text(&out));
    assert!(s.contains("EGRESS_BLOCKED"), "{}", text(&out));
    assert!(s.contains("IP_BLOCKED"), "{}", text(&out));
    assert!(!s.contains("EGRESS_OK"), "{}", text(&out));
}

#[test]
#[ignore]
fn container_egress_follows_host_allowlist() {
    if !net_tests_enabled() {
        eprintln!("skipped: set SHURU_TEST_NET=1");
        return;
    }
    let out = shuru_run(
        &["--allow-net", "--allow-host", "example.com"],
        r#"
docker run --rm IMAGE sh -c "wget -T 15 -qO- http://example.com | grep -o \"<title>[^<]*</title>\" && echo ALLOWED_OK || echo ALLOWED_FAIL"
docker run --rm IMAGE sh -c "wget -T 5 -qO- http://www.iana.org/ > /dev/null 2>&1 && echo DOMAIN_LEAK || echo DOMAIN_BLOCKED"
docker run --rm IMAGE sh -c "wget -T 5 -qO- http://1.1.1.1/ > /dev/null 2>&1 && echo IP_LEAK || echo IP_BLOCKED"
docker run --rm --dns 8.8.8.8 IMAGE sh -c "wget -T 5 -qO- http://example.com > /dev/null 2>&1 && echo DNS_LEAK || echo DNS_BLOCKED"
"#,
    );
    let s = stdout(&out);
    assert!(s.contains("Example Domain"), "{}", text(&out));
    assert!(s.contains("ALLOWED_OK"), "{}", text(&out));
    assert!(s.contains("DOMAIN_BLOCKED"), "{}", text(&out));
    assert!(s.contains("IP_BLOCKED"), "{}", text(&out));
    assert!(s.contains("DNS_BLOCKED"), "{}", text(&out));
    assert!(!s.contains("LEAK"), "{}", text(&out));
}

/// A listener on the host loopback must be unreachable from a container,
/// with and without networking. The gateway address and host.shuru.internal
/// are the only names a guest could try.
#[test]
#[ignore]
fn container_cannot_reach_host_loopback_services() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    let script = format!(
        r#"
docker run --rm IMAGE sh -c "wget -T 5 -qO- http://10.0.0.1:{port}/ > /dev/null 2>&1 && echo GATEWAY_REACHED || echo GATEWAY_BLOCKED"
docker run --rm IMAGE sh -c "wget -T 5 -qO- http://host.shuru.internal:{port}/ > /dev/null 2>&1 && echo HOSTNAME_REACHED || echo HOSTNAME_BLOCKED"
"#
    );

    for extra in [&[][..], &["--allow-net"][..]] {
        let out = shuru_run(extra, &script);
        let s = stdout(&out);
        assert!(s.contains("GATEWAY_BLOCKED"), "{:?}\n{}", extra, text(&out));
        assert!(s.contains("HOSTNAME_BLOCKED"), "{:?}\n{}", extra, text(&out));
        assert!(!s.contains("REACHED"), "{:?}\n{}", extra, text(&out));
    }
    assert!(
        listener.accept().is_err(),
        "host listener on {} received a connection from the sandbox",
        port
    );
}

#[test]
#[ignore]
fn container_cannot_write_to_host_through_default_mount() {
    // Mounts must live under the CWD, which cargo sets to the crate dir.
    let tmp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    std::fs::write(tmp.path().join("host.txt"), "from-host\n").unwrap();
    let spec = format!("{}:/workspace", tmp.path().to_str().unwrap());

    let out = shuru_run(
        &["--mount", &spec],
        r#"
docker run --rm --user 0 -v /workspace:/w IMAGE sh -c "cat /w/host.txt; echo escaped > /w/escaped.txt && echo WROTE_IN_GUEST"
docker run --rm -v /:/vmroot IMAGE sh -c "ls /vmroot/Users > /dev/null 2>&1 && echo HOST_FS_VISIBLE || echo HOST_FS_ABSENT"
"#,
    );
    let s = stdout(&out);
    assert!(s.contains("from-host"), "{}", text(&out));
    assert!(s.contains("WROTE_IN_GUEST"), "{}", text(&out));
    assert!(s.contains("HOST_FS_ABSENT"), "{}", text(&out));
    assert!(
        !tmp.path().join("escaped.txt").exists(),
        "container write reached the host through a read-only mount"
    );
}

#[test]
#[ignore]
fn resource_limits_apply_inside_containers() {
    let out = shuru_run(
        &[],
        r#"
docker run --rm --memory 64m --cpus 0.5 --pids-limit 20 IMAGE sh -c "echo memory.max=\$(cat /sys/fs/cgroup/memory.max); echo cpu.max=\$(cat /sys/fs/cgroup/cpu.max); echo pids.max=\$(cat /sys/fs/cgroup/pids.max)"
docker run --rm --memory 32m IMAGE sh -c "a=\$(head -c 200m /dev/zero | tr \"\\0\" a); echo survived"
echo "oom-exit=$?"
docker run --rm --pids-limit 5 IMAGE sh -c "for i in 1 2 3 4 5 6 7 8 9 10; do sleep 3 & done; wait; echo forked-all" 2>&1 | tail -n 1
"#,
    );
    let s = stdout(&out);
    assert!(s.contains("memory.max=67108864"), "{}", text(&out));
    assert!(s.contains("cpu.max=50000 100000"), "{}", text(&out));
    assert!(s.contains("pids.max=20"), "{}", text(&out));
    assert!(s.contains("oom-exit=137"), "{}", text(&out));
    assert!(!s.contains("survived"), "{}", text(&out));
    assert!(!s.contains("forked-all"), "{}", text(&out));
}

#[test]
#[ignore]
fn named_volume_persists_across_container_restart() {
    let token = format!("persisted-{}", std::process::id());
    let script = format!(
        r#"
set -e
docker volume create vdata > /dev/null
docker create --name keeper -v vdata:/tmp IMAGE sleep 120 > /dev/null
docker start keeper > /dev/null
docker exec keeper sh -c "echo {token} > /tmp/marker"
docker stop -t 1 keeper > /dev/null
docker start keeper > /dev/null
echo "after-container-restart=$(docker exec keeper cat /tmp/marker)"
docker rm -f keeper > /dev/null
echo "after-new-container=$(docker run --rm -v vdata:/tmp IMAGE cat /tmp/marker)"
"#
    );
    let out = shuru_run(&[], &script);
    assert!(out.status.success(), "{}", text(&out));
    let s = stdout(&out);
    assert!(s.contains(&format!("after-container-restart={}", token)), "{}", s);
    assert!(s.contains(&format!("after-new-container={}", token)), "{}", s);
}

/// Shuru resets the disk on every run, so persistence across a VM stop and
/// start goes through a checkpoint. The guest stops dockerd and syncs the
/// disk during the shutdown handshake, before the checkpoint is cloned, so
/// the volume store is consistent and the daemon restarts cleanly on the
/// next boot.
#[test]
#[ignore]
fn named_volume_persists_across_vm_stop_start_via_checkpoint() {
    let name = format!("shuru-test-docker-data-{}", std::process::id());
    let token = format!("vm-persisted-{}", std::process::id());

    let write = format!(
        r#"
set -e
docker volume create vdata > /dev/null
docker run --rm -v vdata:/tmp IMAGE sh -c "echo {token} > /tmp/marker"
"#
    );
    let create = Command::new(shuru_bin())
        .args(["checkpoint", "create", &name, "--verbose", "--from", &checkpoint(), "--"])
        .args(guest_cmd(&write))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(create.status.success(), "{}", text(&create));

    let read = Command::new(shuru_bin())
        .args(["run", "--from", &name, "--"])
        .args(guest_cmd(
            r#"echo "after-vm-restart=$(docker run --rm -v vdata:/tmp IMAGE cat /tmp/marker)""#,
        ))
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let _ = Command::new(shuru_bin())
        .args(["checkpoint", "delete", &name])
        .output();
    eprintln!("$ shuru checkpoint create {} --verbose --from {}\n{}", name, checkpoint(), text(&create));
    eprintln!("$ shuru run --from {}\n{}", name, text(&read));

    assert!(
        String::from_utf8_lossy(&create.stderr).contains("stopped services: docker"),
        "{}",
        text(&create)
    );
    assert!(read.status.success(), "{}", text(&read));
    assert!(
        stdout(&read).contains(&format!("after-vm-restart={}", token)),
        "{}",
        text(&read)
    );
}
