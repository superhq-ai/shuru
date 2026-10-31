//! Exec sessions (RFC 0012): processes that outlive the connection that
//! started them. Output is kept in a ring buffer, and an attachment replays
//! it from a byte offset counted over stdout and stderr together.
//!
//! One thread runs every session (see `event_loop`). A connection thread
//! starts the process, so a slow spawn never holds the others up, and then
//! hands it and the connection to the loop.

mod event_loop;
mod process;
mod ring;

use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use shuru_proto::frame;
use shuru_proto::{KillEvent, PortEvent, SessionAttach, SessionKill, SessionList, SessionStart};

const MAX_RUNNING: usize = 128;

static RUNNING: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static HANDLE: LazyLock<Option<Arc<Handle>>> = LazyLock::new(event_loop::start);

enum Cmd {
    Start {
        id: u64,
        req: SessionStart,
        spawned: process::Spawned,
        cgroup: crate::cgroup::Cgroup,
        conn: TcpStream,
    },
    Attach {
        req: SessionAttach,
        conn: TcpStream,
    },
    List(mpsc::Sender<SessionList>),
    Kill {
        req: SessionKill,
        events: mpsc::Sender<KillEvent>,
    },
    Exited {
        id: u64,
        status: libc::c_int,
    },
    Port {
        id: u64,
        event: PortEvent,
    },
    #[cfg(test)]
    Inspect {
        id: u64,
        reply: mpsc::Sender<Option<Inspected>>,
    },
}

#[cfg(test)]
#[derive(Debug)]
struct Inspected {
    has_owner: bool,
    attachments: usize,
}

/// How other threads reach the loop: commands, and an eventfd to wake it.
struct Handle {
    queue: Mutex<Vec<Cmd>>,
    wake: OwnedFd,
}

impl Handle {
    fn post(&self, cmd: Cmd) {
        lock(&self.queue).push(cmd);
        let one = 1u64;
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            )
        };
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Exit status as the API reports it: the code, or 128 plus the signal.
fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

/// Counts a session as running unless MAX_RUNNING already are.
fn reserve(running: &AtomicUsize) -> bool {
    running
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_RUNNING).then_some(n + 1)
        })
        .is_ok()
}

fn release() {
    RUNNING.fetch_sub(1, Ordering::AcqRel);
}

fn error(conn: &mut TcpStream, msg: &str) {
    let _ = frame::write_frame(conn, frame::ERROR, msg.as_bytes());
}

fn handle(conn: &mut TcpStream) -> Option<&'static Arc<Handle>> {
    let handle = HANDLE.as_ref();
    if handle.is_none() {
        error(conn, "exec sessions are unavailable");
    }
    handle
}

/// Tells the attachments of session `id` that one of its processes opened
/// or closed a port.
pub fn port_event(id: u64, event: PortEvent) {
    if let Some(handle) = HANDLE.as_ref() {
        handle.post(Cmd::Port { id, event });
    }
}

pub fn start(req: SessionStart, mut conn: TcpStream) {
    let Some(handle) = handle(&mut conn) else {
        return;
    };
    if !reserve(&RUNNING) {
        let msg = format!("too many running sessions, the limit is {MAX_RUNNING}");
        return error(&mut conn, &msg);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let cgroup = match crate::cgroup::Cgroup::create(&format!("session-{id}")) {
        Ok(cgroup) => cgroup,
        Err(e) => {
            release();
            return error(
                &mut conn,
                &format!("failed to create the session cgroup: {e}"),
            );
        }
    };
    let exits = handle.clone();
    let on_exit = Box::new(move |status| exits.post(Cmd::Exited { id, status }));
    match process::spawn(&req.exec, &cgroup, on_exit) {
        Ok(spawned) => handle.post(Cmd::Start {
            id,
            req,
            spawned,
            cgroup,
            conn,
        }),
        Err(e) => {
            cgroup.signal(libc::SIGKILL);
            drop(cgroup);
            release();
            error(&mut conn, &format!("failed to spawn: {e}"))
        }
    }
}

pub fn attach(req: SessionAttach, mut conn: TcpStream) {
    if let Some(handle) = handle(&mut conn) {
        handle.post(Cmd::Attach { req, conn });
    }
}

pub fn list() -> SessionList {
    let (reply, list) = mpsc::channel();
    if let Some(handle) = HANDLE.as_ref() {
        handle.post(Cmd::List(reply));
    }
    list.recv().unwrap_or(SessionList {
        sessions: Vec::new(),
    })
}

const SIGNALS: &[(&str, libc::c_int)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("USR2", libc::SIGUSR2),
    ("TERM", libc::SIGTERM),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
];

/// `SIGTERM`, `term` or `15`, with the name to report.
fn parse_signal(name: &str) -> Option<(libc::c_int, String)> {
    if let Ok(n) = name.parse::<libc::c_int>() {
        if !(1..=64).contains(&n) {
            return None;
        }
        let known = SIGNALS.iter().find(|(_, s)| *s == n);
        return Some((n, known.map_or(n.to_string(), |(k, _)| format!("SIG{k}"))));
    }
    let upper = name.to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS
        .iter()
        .find(|(k, _)| *k == bare)
        .map(|(k, s)| (*s, format!("SIG{k}")))
}

fn kill_events(req: SessionKill) -> mpsc::Receiver<KillEvent> {
    let (events, received) = mpsc::channel();
    match HANDLE.as_ref() {
        Some(handle) => handle.post(Cmd::Kill { req, events }),
        None => {
            let _ = events.send(KillEvent::Error {
                message: "exec sessions are unavailable".into(),
            });
        }
    }
    received
}

/// Streams the events of a kill until it completes or fails.
pub fn kill(req: SessionKill, conn: &mut TcpStream) {
    for event in kill_events(req) {
        if frame::send_json(conn, frame::SESSION_KILL_EVENT, &event).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuru_proto::{ExecRequest, SessionInfo};
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    #[test]
    fn running_sessions_are_capped() {
        let running = AtomicUsize::new(MAX_RUNNING - 1);
        assert!(reserve(&running));
        assert!(!reserve(&running));
        assert_eq!(running.load(Ordering::Acquire), MAX_RUNNING);
        running.fetch_sub(1, Ordering::AcqRel);
        assert!(reserve(&running));
    }

    #[test]
    fn signals_by_name_and_number() {
        assert_eq!(
            parse_signal("SIGTERM"),
            Some((libc::SIGTERM, "SIGTERM".into()))
        );
        assert_eq!(parse_signal("int"), Some((libc::SIGINT, "SIGINT".into())));
        assert_eq!(parse_signal("9"), Some((libc::SIGKILL, "SIGKILL".into())));
        assert_eq!(parse_signal("40"), Some((40, "40".into())));
        assert_eq!(parse_signal("SIGNOPE"), None);
        assert_eq!(parse_signal("0"), None);
    }

    // The tests below run real processes, so they need Linux: run them in a
    // container, as the guest does not build its tests on macOS.

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        (client, server)
    }

    /// A pair whose buffers fill well before the output the tests write,
    /// and stay above the loopback MSS.
    fn small_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let size: libc::c_int = 256 * 1024;
        let set = |fd: i32, opt| unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &size as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        set(listener.as_raw_fd(), libc::SO_SNDBUF);
        let client = TcpStream::connect(addr).unwrap();
        set(client.as_raw_fd(), libc::SO_RCVBUF);
        let (server, _) = listener.accept().unwrap();
        set(server.as_raw_fd(), libc::SO_SNDBUF);
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        (client, server)
    }

    fn start_session(argv: &[&str], tty: bool, hangup: Option<u64>) -> TcpStream {
        start_on(pair(), argv, tty, hangup)
    }

    fn start_on(
        (client, server): (TcpStream, TcpStream),
        argv: &[&str],
        tty: bool,
        hangup: Option<u64>,
    ) -> TcpStream {
        let req = SessionStart {
            exec: ExecRequest {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                env: HashMap::new(),
                tty: Some(tty),
                rows: None,
                cols: None,
                cwd: None,
                id: None,
            },
            max_run_after_disconnect: hangup,
        };
        start(req, server);
        client
    }

    fn attach_session(id: &str, offset: Option<u64>) -> TcpStream {
        attach_as(id, offset, false)
    }

    fn attach_as(id: &str, offset: Option<u64>, take_owner: bool) -> TcpStream {
        let (client, server) = pair();
        let req = SessionAttach {
            id: id.into(),
            output_offset: offset,
            take_owner,
        };
        attach(req, server);
        client
    }

    fn info(conn: &mut TcpStream) -> SessionInfo {
        match frame::read_frame(conn).unwrap() {
            Some((frame::SESSION_INFO, payload)) => serde_json::from_slice(&payload).unwrap(),
            Some((t, payload)) => panic!("frame {t:#x}: {}", String::from_utf8_lossy(&payload)),
            None => panic!("closed"),
        }
    }

    /// Output until EXIT, as (stdout, stderr, exit code).
    fn drain(conn: &mut TcpStream) -> (Vec<u8>, Vec<u8>, i32) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            match frame::read_frame(conn).unwrap() {
                Some((frame::STDOUT, data)) => out.extend(data),
                Some((frame::STDERR, data)) => err.extend(data),
                Some((frame::EXIT, payload)) => {
                    return (out, err, frame::parse_exit_code(&payload).unwrap())
                }
                Some((t, payload)) => panic!("frame {t:#x}: {}", String::from_utf8_lossy(&payload)),
                None => panic!("closed before EXIT"),
            }
        }
    }

    fn read_until(conn: &mut TcpStream, want: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        while !got.windows(want.len()).any(|w| w == want) {
            match frame::read_frame(conn).unwrap() {
                Some((frame::STDOUT | frame::STDERR, data)) => got.extend(data),
                other => panic!("unexpected {:?}", other.map(|(t, _)| t)),
            }
        }
        got
    }

    fn session_info(id: &str) -> SessionInfo {
        list().sessions.into_iter().find(|s| s.id == id).unwrap()
    }

    fn inspect(id: &str) -> Inspected {
        let (reply, got) = mpsc::channel();
        let id = id.parse().unwrap();
        HANDLE.as_ref().unwrap().post(Cmd::Inspect { id, reply });
        got.recv().unwrap().unwrap()
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_all(id: &str) -> Vec<KillEvent> {
        kill_events(SessionKill {
            id: id.into(),
            signal: None,
            timeout: Some(1),
        })
        .into_iter()
        .collect()
    }

    fn kinds(events: &[KillEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                KillEvent::Signal { .. } => "signal",
                KillEvent::Timeout { .. } => "timeout",
                KillEvent::Killed { .. } => "killed",
                KillEvent::Complete { .. } => "complete",
                KillEvent::Exited { .. } => "exited",
                KillEvent::Error { .. } => "error",
            })
            .collect()
    }

    #[test]
    fn output_both_streams_and_exit_code() {
        let mut conn = start_session(&["sh", "-c", "echo out; echo err >&2; exit 3"], false, None);
        let info = info(&mut conn);
        assert!(info.is_owner);
        assert_eq!(info.output_offset, 0);
        let (out, err, code) = drain(&mut conn);
        assert_eq!(
            (out.as_slice(), err.as_slice(), code),
            (&b"out\n"[..], &b"err\n"[..], 3)
        );
    }

    #[test]
    fn exit_closes_the_attachment() {
        let mut conn = start_session(&["true"], false, None);
        info(&mut conn);
        drain(&mut conn);
        assert!(frame::read_frame(&mut conn).unwrap().is_none());
    }

    #[test]
    fn stdin_and_stdin_eof() {
        let mut conn = start_session(&["sh", "-c", "cat; echo done"], false, None);
        info(&mut conn);
        frame::write_frame(&mut conn, frame::STDIN, b"ping\n").unwrap();
        frame::write_frame(&mut conn, frame::STDIN_EOF, &[]).unwrap();
        let (out, _, code) = drain(&mut conn);
        assert_eq!((out.as_slice(), code), (&b"ping\ndone\n"[..], 0));
    }

    #[test]
    fn survives_a_dropped_connection_and_replays_from_an_offset() {
        let mut conn = start_session(
            &[
                "sh",
                "-c",
                "echo first; read line; echo \"got $line\"; echo last",
            ],
            false,
            None,
        );
        let id = info(&mut conn).id;
        read_until(&mut conn, b"first\n");
        drop(conn);
        wait_for("detach", || inspect(&id).attachments == 0);
        assert!(session_info(&id).is_active);

        let mut conn = attach_session(&id, Some(2));
        let attached = info(&mut conn);
        assert!(attached.is_owner);
        assert_eq!(attached.output_offset, 2);
        assert_eq!(read_until(&mut conn, b"rst\n"), b"rst\n");
        frame::write_frame(&mut conn, frame::STDIN, b"hello\n").unwrap();
        let (out, _, code) = drain(&mut conn);
        assert_eq!((out.as_slice(), code), (&b"got hello\nlast\n"[..], 0));
    }

    #[test]
    fn attach_after_exit_replays_everything() {
        let mut conn = start_session(&["sh", "-c", "echo gone; exit 7"], false, None);
        let id = info(&mut conn).id;
        drop(conn);
        wait_for("exit", || session_info(&id).exit_code.is_some());
        let mut conn = attach_session(&id, None);
        let attached = info(&mut conn);
        assert!(!attached.is_active);
        assert_eq!(attached.output_offset, 0);
        let (out, _, code) = drain(&mut conn);
        assert_eq!((out.as_slice(), code), (&b"gone\n"[..], 7));
    }

    #[test]
    fn only_the_owner_sends_input() {
        let mut owner = start_session(
            &["sh", "-c", "read a; echo \"a=$a\"; read b; echo \"b=$b\""],
            false,
            None,
        );
        let id = info(&mut owner).id;
        let mut other = attach_session(&id, None);
        assert!(!info(&mut other).is_owner);
        frame::write_frame(&mut other, frame::STDIN, b"ignored\n").unwrap();
        frame::write_frame(&mut owner, frame::STDIN, b"one\n").unwrap();
        assert_eq!(read_until(&mut other, b"a=one\n"), b"a=one\n");
        drop(owner);
        wait_for("owner detach", || !inspect(&id).has_owner);
        frame::write_frame(&mut other, frame::STDIN, b"two\n").unwrap();
        let (out, _, _) = drain(&mut other);
        assert_eq!(out, b"b=two\n");
    }

    #[test]
    fn an_attachment_can_take_over_as_owner() {
        let mut first = start_session(&["sh", "-c", "read a; echo \"a=$a\""], false, None);
        let id = info(&mut first).id;
        let mut second = attach_as(&id, None, true);
        assert!(info(&mut second).is_owner);
        frame::write_frame(&mut first, frame::STDIN, b"ignored\n").unwrap();
        frame::write_frame(&mut second, frame::STDIN, b"x\n").unwrap();
        assert_eq!(drain(&mut second).0, b"a=x\n");
    }

    #[test]
    fn an_attachment_with_no_owner_becomes_the_owner() {
        let mut conn = start_session(&["sh", "-c", "read a; echo \"a=$a\""], false, None);
        let id = info(&mut conn).id;
        drop(conn);
        wait_for("detach", || inspect(&id).attachments == 0);
        let mut conn = attach_session(&id, None);
        assert!(info(&mut conn).is_owner);
        frame::write_frame(&mut conn, frame::STDIN, b"x\n").unwrap();
        assert_eq!(drain(&mut conn).0, b"a=x\n");
    }

    #[test]
    fn tty_session_with_resize() {
        let mut conn = start_session(&["sh", "-c", "read x; stty size"], true, None);
        let info = info(&mut conn);
        assert!(info.tty);
        assert_eq!((info.rows, info.cols), (Some(24), Some(80)));
        frame::write_frame(&mut conn, frame::RESIZE, &frame::resize_payload(40, 120)).unwrap();
        frame::write_frame(&mut conn, frame::STDIN, b"\n").unwrap();
        let (out, _, code) = drain(&mut conn);
        assert!(String::from_utf8_lossy(&out).contains("40 120"), "{out:?}");
        assert_eq!(code, 0);
    }

    #[test]
    fn hangup_after_disconnect() {
        let mut conn = start_session(
            &[
                "sh",
                "-c",
                "trap 'exit 42' HUP; echo ready; while :; do sleep 0.1; done",
            ],
            false,
            Some(1),
        );
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        drop(conn);
        wait_for("hangup", || session_info(&id).exit_code == Some(42));
    }

    #[test]
    fn hangup_again_after_each_disconnect() {
        let script = "n=0; trap 'n=$((n+1)); echo hup$n; [ $n -ge 2 ] && exit 42' HUP; \
                      echo ready; while :; do sleep 0.1; done";
        let mut conn = start_session(&["sh", "-c", script], false, Some(1));
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        drop(conn);
        wait_for("detach", || inspect(&id).attachments == 0);
        std::thread::sleep(Duration::from_millis(1500));
        let mut conn = attach_session(&id, Some(0));
        info(&mut conn);
        read_until(&mut conn, b"hup1\n");
        assert!(session_info(&id).is_active);
        drop(conn);
        wait_for("second hangup", || session_info(&id).exit_code == Some(42));
    }

    #[test]
    fn no_hangup_without_a_limit() {
        let mut conn = start_session(&["sleep", "30"], false, None);
        let id = info(&mut conn).id;
        drop(conn);
        std::thread::sleep(Duration::from_secs(3));
        assert!(session_info(&id).is_active);
        kill_all(&id);
    }

    #[test]
    fn kill_reaches_the_whole_group() {
        let script = "sleep 300 & sleep 300 & echo ready; wait";
        let mut conn = start_session(&["sh", "-c", script], false, None);
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        let events = kill_all(&id);
        assert!(
            matches!(events[0], KillEvent::Signal { ref signal, pid, .. } if signal == "SIGTERM" && pid > 1)
        );
        assert_eq!(
            events.last(),
            Some(&KillEvent::Complete {
                exit_code: 128 + libc::SIGTERM
            })
        );
        // Both sleeps hold stdout, so EXIT comes only once they are gone too.
        let (_, _, code) = drain(&mut conn);
        assert_eq!(code, 128 + libc::SIGTERM);
    }

    #[test]
    fn kill_waits_for_the_whole_group() {
        // The leader dies on TERM; a member that ignores it lives on.
        let script = "(trap '' TERM; echo ready; exec sleep 300) & sleep 300";
        let mut conn = start_session(&["sh", "-c", script], false, None);
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        let events = kill_all(&id);
        assert_eq!(kinds(&events), ["signal", "timeout", "killed", "complete"]);
        assert_eq!(
            events[3],
            KillEvent::Complete {
                exit_code: 128 + libc::SIGTERM
            }
        );
        drain(&mut conn);
    }

    #[test]
    fn kill_reaches_a_process_that_left_the_group() {
        let script = "setsid sleep 300 & echo ready; wait";
        let mut conn = start_session(&["sh", "-c", script], false, None);
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        let events = kill_all(&id);
        assert_eq!(kinds(&events), ["signal", "exited", "complete"]);
        drain(&mut conn);
    }

    #[test]
    fn kill_reaches_a_cgroup_below_the_session() {
        let script = r#"d=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir $d/below
sh -c 'echo 0 > $1/below/cgroup.procs; trap "echo below-term; exit 0" TERM; echo ready; while :; do sleep 0.1; done' sh $d &
wait"#;
        let mut conn = start_session(&["sh", "-c", script], false, None);
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        let events = kill_all(&id);
        assert_eq!(kinds(&events), ["signal", "exited", "complete"]);
        let (out, _, _) = drain(&mut conn);
        assert!(String::from_utf8_lossy(&out).contains("below-term"));
    }

    #[test]
    fn a_session_runs_in_its_own_cgroup() {
        let mut conn = start_session(&["cat", "/proc/self/cgroup"], false, None);
        let id = info(&mut conn).id;
        let (out, _, code) = drain(&mut conn);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0);
        assert!(
            out.trim_end().ends_with(&format!("/shuru/session-{id}")),
            "{out}"
        );
    }

    #[test]
    fn kill_escalates_to_sigkill() {
        let mut conn = start_session(
            &[
                "sh",
                "-c",
                "trap '' TERM; echo ready; while :; do sleep 0.1; done",
            ],
            false,
            None,
        );
        let id = info(&mut conn).id;
        read_until(&mut conn, b"ready\n");
        let events = kill_all(&id);
        assert_eq!(kinds(&events), ["signal", "timeout", "killed", "complete"]);
        assert_eq!(
            events[3],
            KillEvent::Complete {
                exit_code: 128 + libc::SIGKILL
            }
        );
    }

    #[test]
    fn kill_after_exit_reports_the_exit() {
        let mut conn = start_session(&["true"], false, None);
        let id = info(&mut conn).id;
        drain(&mut conn);
        let events = kill_all(&id);
        assert_eq!(kinds(&events), ["exited", "complete"]);
        assert_eq!(events[1], KillEvent::Complete { exit_code: 0 });
    }

    #[test]
    fn unknown_session() {
        let mut conn = attach_session("999999", None);
        match frame::read_frame(&mut conn).unwrap() {
            Some((frame::ERROR, msg)) => assert_eq!(msg, b"no session 999999"),
            other => panic!("{:?}", other.map(|(t, _)| t)),
        }
        assert_eq!(kinds(&kill_all("999999")), ["error"]);
    }

    #[test]
    fn spawn_failure_is_an_error() {
        let mut conn = start_session(&["/no/such/binary"], false, None);
        match frame::read_frame(&mut conn).unwrap() {
            Some((frame::ERROR, msg)) => {
                assert!(String::from_utf8_lossy(&msg).starts_with("failed to spawn"))
            }
            other => panic!("{:?}", other.map(|(t, _)| t)),
        }
    }

    #[test]
    fn a_slow_attachment_holds_the_process_back_and_loses_nothing() {
        let mut conn = start_on(
            small_pair(),
            &["head", "-c", "3000000", "/dev/zero"],
            false,
            None,
        );
        let id = info(&mut conn).id;
        std::thread::sleep(Duration::from_millis(500));
        assert!(session_info(&id).exit_code.is_none());
        let (out, _, code) = drain(&mut conn);
        assert_eq!(code, 0);
        assert_eq!(out.len(), 3_000_000);
    }

    #[test]
    fn detached_output_keeps_the_newest_buffer() {
        let mut conn = start_on(
            small_pair(),
            &["head", "-c", "3000000", "/dev/zero"],
            false,
            None,
        );
        let id = info(&mut conn).id;
        drop(conn);
        wait_for("exit", || session_info(&id).exit_code.is_some());
        let mut again = attach_session(&id, Some(0));
        let attached = info(&mut again);
        let (out, _, code) = drain(&mut again);
        assert_eq!(code, 0);
        assert_eq!(attached.output_offset + out.len() as u64, 3_000_000);
        assert_eq!(out.len() as u64, ring::BUFFER);
    }

    #[test]
    fn a_full_stdin_holds_only_its_session() {
        let mut conn = start_session(&["sleep", "300"], false, None);
        let id = info(&mut conn).id;
        let mut writer = conn.try_clone().unwrap();
        std::thread::spawn(move || {
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..64 {
                if frame::write_frame(&mut writer, frame::STDIN, &chunk).is_err() {
                    break;
                }
            }
        });
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        assert!(session_info(&id).is_active);
        let mut other = start_session(&["echo", "free"], false, None);
        info(&mut other);
        assert_eq!(drain(&mut other).0, b"free\n");
        assert!(started.elapsed() < Duration::from_secs(2));
        let events = kill_all(&id);
        assert_eq!(kinds(&events).last(), Some(&"complete"));
        drain(&mut conn);
    }

    #[test]
    fn huge_timeouts_are_safe() {
        let mut conn = start_session(&["sleep", "300"], false, Some(u64::MAX));
        let id = info(&mut conn).id;
        drop(conn);
        wait_for("detach", || inspect(&id).attachments == 0);
        let events: Vec<KillEvent> = kill_events(SessionKill {
            id: id.clone(),
            signal: None,
            timeout: Some(u64::MAX),
        })
        .into_iter()
        .collect();
        assert_eq!(kinds(&events), ["signal", "exited", "complete"]);
    }

    /// A pair that, like vsock and unlike TCP, shows a close at once even
    /// with sent bytes still unread.
    fn unix_pair() -> (TcpStream, TcpStream) {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = TcpStream::from(OwnedFd::from(client));
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        (client, TcpStream::from(OwnedFd::from(server)))
    }

    #[test]
    fn a_blocked_owner_that_leaves_is_detached() {
        let mut conn = start_on(unix_pair(), &["sleep", "300"], false, Some(1));
        let id = info(&mut conn).id;
        let mut writer = conn.try_clone().unwrap();
        let fill = std::thread::spawn(move || {
            let chunk = vec![b'x'; 64 * 1024];
            while frame::write_frame(&mut writer, frame::STDIN, &chunk).is_ok() {}
        });
        std::thread::sleep(Duration::from_millis(500));
        conn.shutdown(std::net::Shutdown::Both).unwrap();
        fill.join().unwrap();
        wait_for("hangup", || {
            session_info(&id).exit_code == Some(128 + libc::SIGHUP)
        });
    }

    #[test]
    fn errors_echo_a_bounded_id() {
        let long = "9".repeat(100_000);
        let mut conn = attach_session(&long, None);
        match frame::read_frame(&mut conn).unwrap() {
            Some((frame::ERROR, msg)) => assert!(msg.len() < 100, "{}", msg.len()),
            other => panic!("{:?}", other.map(|(t, _)| t)),
        }
    }

    #[test]
    fn attachments_per_session_are_capped() {
        let mut conn = start_session(&["sleep", "300"], false, None);
        let id = info(&mut conn).id;
        let mut held = Vec::new();
        for _ in 1..event_loop::MAX_PER_SESSION {
            let mut c = attach_session(&id, None);
            info(&mut c);
            held.push(c);
        }
        let mut refused = attach_session(&id, None);
        match frame::read_frame(&mut refused).unwrap() {
            Some((frame::ERROR, msg)) => assert_eq!(msg, b"too many attachments"),
            other => panic!("{:?}", other.map(|(t, _)| t)),
        }
        kill_all(&id);
    }
}
