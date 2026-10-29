//! The thread that runs every session. Process pipes and terminals, pidfds
//! and attached connections are all nonblocking and in one epoll set; other
//! threads reach the loop only through commands, so its state needs no
//! locks.
//!
//! Flow control: output is read from a process only while the ring has
//! room for it without dropping bytes an attachment has not taken, so an
//! attached client gets every byte and a fast writer waits for it. With
//! nothing attached the ring keeps the newest output. Input for a process
//! waits in a bounded queue; a full queue stops reading the owner. A
//! descriptor that must wait is taken out of the epoll set, so a hung up
//! pipe or socket cannot wake the loop until there is room again.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use shuru_proto::frame;
use shuru_proto::{KillEvent, SessionInfo, SessionKill, SessionList, SessionStart};

use super::process::Spawned;
use super::ring::Ring;
use super::{exit_code, lock, parse_signal, release, unix_now, Cmd, Handle};
use crate::cgroup::Cgroup;

const READ_CHUNK: usize = 64 * 1024;
/// An attachment takes more output once less than this waits to be sent.
const SEND_LOW: usize = 64 * 1024;
const STDIN_QUEUE: usize = 256 * 1024;
const KEEP_EXITED: Duration = Duration::from_secs(600);
const MAX_EXITED: usize = 64;
const MAX_ATTACHMENTS: usize = 256;
pub(super) const MAX_PER_SESSION: usize = 16;
const KILL_TIMEOUT: u64 = 10;
const MAX_KILL_TIMEOUT: u64 = 24 * 3600;
const MAX_KILLS: usize = 16;
const REAP_RETRY: Duration = Duration::from_millis(5);
const SIGKILL_WAIT: Duration = Duration::from_secs(5);
const KILL_TICK: Duration = Duration::from_millis(20);
const SWEEP_TICK: Duration = Duration::from_secs(1);
const RATE_WINDOW: usize = 10;
/// Bytes of the command and of the workdir in session info, counted as
/// JSON, so a full session table fits in one frame.
const SHOWN_COMMAND: usize = 2048;
const SHOWN_WORKDIR: usize = 1024;

const WAKE: u64 = 0;
const IN: u32 = libc::EPOLLIN as u32;
const OUT: u32 = libc::EPOLLOUT as u32;
const RDHUP: u32 = libc::EPOLLRDHUP as u32;
const HUP: u32 = (libc::EPOLLHUP | libc::EPOLLERR) as u32;

pub(super) fn start() -> Option<Arc<Handle>> {
    match try_start() {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("shuru-guest: exec sessions unavailable: {e}");
            None
        }
    }
}

fn try_start() -> io::Result<Arc<Handle>> {
    crate::cgroup::init()?;
    let epoll = owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
    let wake = owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
    ctl(&epoll, libc::EPOLL_CTL_ADD, wake.as_raw_fd(), IN, WAKE)?;
    let handle = Arc::new(Handle {
        queue: Mutex::new(Vec::new()),
        wake,
    });
    let mut event_loop = Loop {
        handle: handle.clone(),
        poll: Poll {
            epoll,
            sources: HashMap::new(),
            next: WAKE + 1,
        },
        sessions: BTreeMap::new(),
        attachments: HashMap::new(),
        next_attachment: 1,
        early_exits: HashMap::new(),
        dirty: HashSet::new(),
        next_sweep: Instant::now(),
        reap_pending: false,
        scratch: vec![0; READ_CHUNK],
    };
    std::thread::Builder::new()
        .name("sessions".into())
        .spawn(move || event_loop.run())?;
    Ok(handle)
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ctl(epoll: &OwnedFd, op: libc::c_int, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut event = libc::epoll_event { events, u64: token };
    if unsafe { libc::epoll_ctl(epoll.as_raw_fd(), op, fd, &mut event) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[derive(Clone, Copy)]
enum Source {
    Output(u64, u8),
    Stdin(u64),
    Pidfd(u64),
    Attachment(u64),
}

/// A descriptor and the events it is registered for; none means it is
/// out of the epoll set.
struct Watched {
    fd: OwnedFd,
    token: u64,
    events: u32,
}

struct Poll {
    epoll: OwnedFd,
    sources: HashMap<u64, Source>,
    next: u64,
}

impl Poll {
    fn watch(&mut self, fd: OwnedFd, source: Source) -> Watched {
        let token = self.next;
        self.next += 1;
        self.sources.insert(token, source);
        Watched {
            fd,
            token,
            events: 0,
        }
    }

    fn set(&self, w: &mut Watched, events: u32) -> io::Result<()> {
        if events == w.events {
            return Ok(());
        }
        let op = match (w.events, events) {
            (0, _) => libc::EPOLL_CTL_ADD,
            (_, 0) => libc::EPOLL_CTL_DEL,
            _ => libc::EPOLL_CTL_MOD,
        };
        ctl(&self.epoll, op, w.fd.as_raw_fd(), events, w.token)?;
        w.events = events;
        Ok(())
    }

    fn unwatch(&mut self, mut w: Watched) {
        let _ = self.set(&mut w, 0);
        self.sources.remove(&w.token);
    }
}

struct Session {
    id: u64,
    command: Vec<String>,
    workdir: String,
    tty: bool,
    created: u64,
    pid: i32,
    cgroup: Cgroup,
    pidfd: Option<Watched>,
    stdin: Option<Watched>,
    stdin_queue: VecDeque<u8>,
    stdin_eof: bool,
    outputs: Vec<(u8, Watched)>,
    ring: Ring,
    exit: Option<i32>,
    finished: Option<Instant>,
    attachments: Vec<u64>,
    owner: Option<u64>,
    hangup_after: Option<Duration>,
    hangup_at: Option<Instant>,
    size: Option<(u16, u16)>,
    last_activity: u64,
    rate: [(u64, u64); RATE_WINDOW],
    kills: Vec<KillWait>,
}

struct KillWait {
    events: mpsc::Sender<KillEvent>,
    timeout: u64,
    deadline: Instant,
    escalated: bool,
}

struct Attachment {
    session: u64,
    sock: Watched,
    /// Output up to here is in `out`.
    cursor: u64,
    out: Vec<u8>,
    sent: usize,
    inp: Vec<u8>,
    taken: usize,
    /// Its input waits for room in the stdin queue.
    blocked: bool,
    /// EXIT is in `out`; the connection closes once it is sent.
    closing: bool,
}

impl Session {
    fn info(&self) -> SessionInfo {
        let now = unix_now();
        SessionInfo {
            id: self.id.to_string(),
            command: shown_command(&self.command),
            workdir: clip(&self.workdir, &mut { SHOWN_WORKDIR }),
            tty: self.tty,
            created: self.created,
            is_active: self.exit.is_none(),
            last_activity: self.last_activity,
            bytes_per_second: self
                .rate
                .iter()
                .filter(|(sec, _)| sec + RATE_WINDOW as u64 > now)
                .map(|(_, n)| n)
                .sum::<u64>() as f64
                / RATE_WINDOW as f64,
            rows: self.size.map(|(rows, _)| rows),
            cols: self.size.map(|(_, cols)| cols),
            exit_code: self.exit,
            is_owner: false,
            output_offset: 0,
        }
    }

    fn output(&mut self, stream: u8, data: &[u8]) {
        self.ring.push(stream, data);
        let now = unix_now();
        self.last_activity = now;
        let slot = &mut self.rate[now as usize % RATE_WINDOW];
        if slot.0 != now {
            *slot = (now, 0);
        }
        slot.1 += data.len() as u64;
    }

    /// Input from this attachment reaches the process: it is the owner, or
    /// there is none and it becomes the owner.
    fn claim(&mut self, attachment: u64) -> bool {
        match self.owner {
            Some(owner) => owner == attachment,
            None => {
                self.owner = Some(attachment);
                true
            }
        }
    }

    fn takes_input(&self, attachment: u64) -> bool {
        self.owner.is_none_or(|owner| owner == attachment)
    }
}

/// The longest prefix of `s` whose JSON form fits in `left` bytes.
fn clip(s: &str, left: &mut usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let n = match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        };
        if n > *left {
            break;
        }
        *left -= n;
        out.push(c);
    }
    out
}

fn shown_command(command: &[String]) -> Vec<String> {
    let mut left = SHOWN_COMMAND;
    let mut out = Vec::new();
    for arg in command {
        // Its quotes and comma.
        if left < 3 {
            break;
        }
        left -= 3;
        out.push(clip(arg, &mut left));
    }
    out
}

struct Loop {
    handle: Arc<Handle>,
    poll: Poll,
    sessions: BTreeMap<u64, Session>,
    attachments: HashMap<u64, Attachment>,
    next_attachment: u64,
    /// Exits reaped before their session reached the loop.
    early_exits: HashMap<u64, i32>,
    dirty: HashSet<u64>,
    next_sweep: Instant,
    /// A reap waits for a spawn to let go of the children lock.
    reap_pending: bool,
    scratch: Vec<u8>,
}

impl Loop {
    fn run(&mut self) {
        let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 128];
        loop {
            let timeout = self.timeout();
            let n = unsafe {
                libc::epoll_wait(
                    self.poll.epoll.as_raw_fd(),
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    timeout,
                )
            };
            if n < 0 && errno() != libc::EINTR {
                eprintln!(
                    "shuru-guest: sessions: epoll_wait: {}",
                    io::Error::last_os_error()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            for event in &events[..n.max(0) as usize] {
                let (token, flags) = ({ event.u64 }, { event.events });
                self.event(token, flags);
            }
            self.commands();
            self.timers();
            self.settle();
        }
    }

    fn timeout(&self) -> libc::c_int {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut sooner = |at: Instant| next = Some(next.map_or(at, |n| n.min(at)));
        for s in self.sessions.values() {
            if let Some(at) = s.hangup_at {
                sooner(at);
            }
            if !s.kills.is_empty() {
                sooner(now + KILL_TICK);
            }
            if s.finished.is_some() {
                sooner(self.next_sweep);
            }
        }
        if self.reap_pending {
            sooner(now + REAP_RETRY);
        }
        match next {
            None => -1,
            Some(at) => at
                .saturating_duration_since(now)
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as libc::c_int,
        }
    }

    fn event(&mut self, token: u64, flags: u32) {
        if token == WAKE {
            let mut count = 0u64;
            let wake = self.handle.wake.as_raw_fd();
            unsafe { libc::read(wake, &mut count as *mut u64 as *mut libc::c_void, 8) };
            return;
        }
        let Some(source) = self.poll.sources.get(&token).copied() else {
            return;
        };
        match source {
            Source::Output(sid, stream) => self.read_output(sid, stream),
            Source::Stdin(sid) => self.write_stdin(sid),
            Source::Pidfd(sid) => {
                self.reap();
                if let Some(w) = self.sessions.get_mut(&sid).and_then(|s| s.pidfd.take()) {
                    self.poll.unwatch(w);
                }
            }
            Source::Attachment(aid) => {
                let Some(sid) = self.attachments.get(&aid).map(|a| a.session) else {
                    return;
                };
                if flags & (IN | RDHUP) != 0 {
                    self.read_input(aid);
                }
                if flags & OUT != 0 {
                    self.flush(aid);
                }
                // A blocked attachment is not read, so its hangup ends it.
                let blocked = self.attachments.get(&aid).is_some_and(|a| a.blocked);
                if flags & HUP != 0 || (blocked && flags & RDHUP != 0) {
                    self.detach(aid);
                }
                self.dirty.insert(sid);
            }
        }
    }

    /// How much of `stream` can be read without dropping output an
    /// attachment has not taken.
    fn room(&self, sid: u64, stream: u8) -> usize {
        let Some(s) = self.sessions.get(&sid) else {
            return 0;
        };
        let keep = s
            .attachments
            .iter()
            .filter_map(|a| self.attachments.get(a))
            .map(|a| a.cursor)
            .min();
        match keep {
            None => READ_CHUNK,
            Some(keep) => s.ring.room(stream, keep).min(READ_CHUNK),
        }
    }

    fn read_output(&mut self, sid: u64, stream: u8) {
        // A few reads, so one busy process does not starve the rest.
        for _ in 0..4 {
            let room = self.room(sid, stream);
            let Some(s) = self.sessions.get_mut(&sid) else {
                return;
            };
            let Some(index) = s.outputs.iter().position(|(st, _)| *st == stream) else {
                return;
            };
            if room == 0 {
                break;
            }
            let fd = s.outputs[index].1.fd.as_raw_fd();
            let n = unsafe { libc::read(fd, self.scratch.as_mut_ptr() as *mut libc::c_void, room) };
            if n > 0 {
                s.output(stream, &self.scratch[..n as usize]);
                self.dirty.insert(sid);
                if (n as usize) < room {
                    break;
                }
                continue;
            }
            if n < 0 && errno() == libc::EAGAIN {
                break;
            }
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            // End of output; a terminal reads EIO once nothing holds it open.
            let (_, w) = s.outputs.remove(index);
            self.poll.unwatch(w);
            self.dirty.insert(sid);
            break;
        }
    }

    fn write_stdin(&mut self, sid: u64) {
        let Some(s) = self.sessions.get_mut(&sid) else {
            return;
        };
        let Some(fd) = s.stdin.as_ref().map(|w| w.fd.as_raw_fd()) else {
            return;
        };
        while !s.stdin_queue.is_empty() {
            let (data, _) = s.stdin_queue.as_slices();
            let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
            if n > 0 {
                s.stdin_queue.drain(..n as usize);
                continue;
            }
            match errno() {
                libc::EAGAIN => break,
                libc::EINTR => continue,
                _ => {
                    // The process closed its stdin.
                    s.stdin_queue.clear();
                    if let Some(w) = s.stdin.take() {
                        self.poll.unwatch(w);
                    }
                    break;
                }
            }
        }
        self.dirty.insert(sid);
    }

    fn read_input(&mut self, aid: u64) {
        let mut ended = false;
        if let Some(a) = self.attachments.get_mut(&aid) {
            let fd = a.sock.fd.as_raw_fd();
            while !a.blocked && a.inp.len() - a.taken <= frame::MAX_FRAME as usize + 4 {
                if a.taken > 0 {
                    a.inp.drain(..a.taken);
                    a.taken = 0;
                }
                let len = a.inp.len();
                a.inp.resize(len + READ_CHUNK, 0);
                let n = unsafe {
                    libc::read(
                        fd,
                        a.inp[len..].as_mut_ptr() as *mut libc::c_void,
                        READ_CHUNK,
                    )
                };
                a.inp.truncate(len + n.max(0) as usize);
                if n > 0 {
                    continue;
                }
                if n < 0 && errno() == libc::EAGAIN {
                    break;
                }
                if n < 0 && errno() == libc::EINTR {
                    continue;
                }
                ended = true;
                break;
            }
        }
        self.take_input(aid);
        if ended {
            self.detach(aid);
        }
    }

    /// Handles the whole frames an attachment has sent, until its input
    /// has to wait for room in the stdin queue.
    fn take_input(&mut self, aid: u64) {
        loop {
            let Some(a) = self.attachments.get_mut(&aid) else {
                return;
            };
            let sid = a.session;
            let pending = &a.inp[a.taken..];
            if pending.len() < 5 {
                return;
            }
            let len = u32::from_be_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
            if len == 0 || len > frame::MAX_FRAME as usize {
                self.detach(aid);
                return;
            }
            if pending.len() < 4 + len {
                return;
            }
            let kind = pending[4];
            let Some(s) = self.sessions.get_mut(&sid) else {
                return;
            };
            if kind == frame::STDIN && s.takes_input(aid) && s.stdin_queue.len() >= STDIN_QUEUE {
                a.blocked = true;
                return;
            }
            a.blocked = false;
            let payload = &pending[5..4 + len];
            match kind {
                frame::STDIN => {
                    if s.claim(aid) && s.stdin.is_some() {
                        s.stdin_queue.extend(payload);
                        s.last_activity = unix_now();
                    }
                }
                frame::STDIN_EOF => {
                    if s.claim(aid) {
                        s.stdin_eof = true;
                    }
                }
                frame::RESIZE => {
                    if let Some((rows, cols)) = frame::parse_resize(payload) {
                        if s.tty && s.claim(aid) {
                            resize(s, rows, cols);
                        }
                    }
                }
                frame::KILL => s.cgroup.signal(libc::SIGTERM),
                _ => {}
            }
            a.taken += 4 + len;
            self.dirty.insert(sid);
        }
    }

    /// Queues output for an attachment while little of it waits, and EXIT
    /// once all of it is queued and the session has finished.
    fn fill(&mut self, aid: u64) {
        let Some(a) = self.attachments.get_mut(&aid) else {
            return;
        };
        let Some(s) = self.sessions.get(&a.session) else {
            return;
        };
        if a.sent > 0 {
            a.out.drain(..a.sent);
            a.sent = 0;
        }
        while a.out.len() < SEND_LOW && a.cursor < s.ring.end() {
            a.cursor = s.ring.frames(a.cursor, SEND_LOW, &mut a.out);
        }
        if let (Some(_), false) = (s.finished, a.closing) {
            if a.cursor == s.ring.end() {
                let code = s.exit.unwrap_or(1);
                let _ = frame::write_frame(&mut a.out, frame::EXIT, &frame::exit_payload(code));
                a.closing = true;
            }
        }
    }

    fn flush(&mut self, aid: u64) {
        let Some(a) = self.attachments.get_mut(&aid) else {
            return;
        };
        let fd = a.sock.fd.as_raw_fd();
        let mut failed = false;
        while a.sent < a.out.len() {
            let data = &a.out[a.sent..];
            let n = unsafe {
                libc::send(
                    fd,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if n > 0 {
                a.sent += n as usize;
                continue;
            }
            match errno() {
                libc::EAGAIN => break,
                libc::EINTR => continue,
                _ => {
                    failed = true;
                    break;
                }
            }
        }
        if failed || (a.closing && a.sent == a.out.len()) {
            self.detach(aid);
        }
    }

    fn detach(&mut self, aid: u64) {
        let Some(a) = self.attachments.remove(&aid) else {
            return;
        };
        self.poll.unwatch(a.sock);
        if let Some(s) = self.sessions.get_mut(&a.session) {
            s.attachments.retain(|other| *other != aid);
            if s.owner == Some(aid) {
                s.owner = None;
            }
            if s.attachments.is_empty() && s.finished.is_none() {
                s.hangup_at = s.hangup_after.and_then(after);
            }
            self.dirty.insert(a.session);
        }
    }

    fn commands(&mut self) {
        loop {
            let commands = std::mem::take(&mut *lock(&self.handle.queue));
            if commands.is_empty() {
                return;
            }
            for command in commands {
                match command {
                    Cmd::Start {
                        id,
                        req,
                        spawned,
                        cgroup,
                        conn,
                    } => self.start(id, req, spawned, cgroup, conn),
                    Cmd::Attach { req, conn } => match req.id.parse::<u64>() {
                        Ok(sid) if self.sessions.contains_key(&sid) => {
                            self.attach(sid, conn, req.output_offset)
                        }
                        _ => refuse(conn, &format!("no session {}", shown_id(&req.id))),
                    },
                    Cmd::List(reply) => {
                        let sessions = self.sessions.values().map(Session::info).collect();
                        let _ = reply.send(SessionList { sessions });
                    }
                    Cmd::Kill { req, events } => self.kill(req, events),
                    Cmd::Exited { id, status } => self.exited(id, status),
                    #[cfg(test)]
                    Cmd::Inspect { id, reply } => {
                        let found = self.sessions.get(&id).map(|s| super::Inspected {
                            has_owner: s.owner.is_some(),
                            attachments: s.attachments.len(),
                        });
                        let _ = reply.send(found);
                    }
                }
            }
        }
    }

    fn start(
        &mut self,
        id: u64,
        req: SessionStart,
        spawned: Spawned,
        cgroup: Cgroup,
        conn: TcpStream,
    ) {
        let exec = req.exec;
        let tty = exec.tty.unwrap_or(false);
        let mut session = Session {
            id,
            workdir: exec.cwd.clone().unwrap_or_else(|| "/".into()),
            command: exec.argv,
            tty,
            created: unix_now(),
            pid: spawned.pid,
            cgroup,
            pidfd: Some(self.poll.watch(spawned.pidfd, Source::Pidfd(id))),
            stdin: Some(self.poll.watch(spawned.stdin, Source::Stdin(id))),
            stdin_queue: VecDeque::new(),
            stdin_eof: false,
            outputs: spawned
                .outputs
                .into_iter()
                .map(|(stream, fd)| (stream, self.poll.watch(fd, Source::Output(id, stream))))
                .collect(),
            ring: Ring::new(),
            exit: None,
            finished: None,
            attachments: Vec::new(),
            owner: None,
            hangup_after: req.max_run_after_disconnect.map(Duration::from_secs),
            hangup_at: None,
            size: tty.then(|| (exec.rows.unwrap_or(24), exec.cols.unwrap_or(80))),
            last_activity: unix_now(),
            rate: [(0, 0); RATE_WINDOW],
            kills: Vec::new(),
        };
        let pidfd = match self.early_exits.remove(&id) {
            Some(status) => {
                session.exit = Some(exit_code(status));
                session.pidfd.take()
            }
            None => None,
        };
        if let Some(w) = pidfd {
            self.poll.unwatch(w);
        }
        self.sessions.insert(id, session);
        // The starting connection is attached before any output is read, so
        // none of it can be dropped before it is sent.
        self.attach(id, conn, Some(0));
        if let Some(s) = self.sessions.get_mut(&id) {
            if s.attachments.is_empty() {
                s.hangup_at = s.hangup_after.and_then(after);
            }
        }
        self.dirty.insert(id);
    }

    fn attach(&mut self, sid: u64, conn: TcpStream, offset: Option<u64>) {
        let Some(s) = self.sessions.get_mut(&sid) else {
            return;
        };
        if s.attachments.len() >= MAX_PER_SESSION || self.attachments.len() >= MAX_ATTACHMENTS {
            return refuse(conn, "too many attachments");
        }
        let cursor = offset.unwrap_or(0).clamp(s.ring.start(), s.ring.end());
        let aid = self.next_attachment;
        let is_owner = s.owner.is_none();
        let info = SessionInfo {
            is_owner,
            output_offset: cursor,
            ..s.info()
        };
        let mut out = Vec::new();
        if frame::send_json(&mut out, frame::SESSION_INFO, &info).is_err()
            || conn.set_nonblocking(true).is_err()
        {
            return;
        }
        self.next_attachment += 1;
        if is_owner {
            s.owner = Some(aid);
        }
        s.attachments.push(aid);
        s.hangup_at = None;
        let sock = self
            .poll
            .watch(OwnedFd::from(conn), Source::Attachment(aid));
        self.attachments.insert(
            aid,
            Attachment {
                session: sid,
                sock,
                cursor,
                out,
                sent: 0,
                inp: Vec::new(),
                taken: 0,
                blocked: false,
                closing: false,
            },
        );
        self.dirty.insert(sid);
    }

    fn exited(&mut self, id: u64, status: libc::c_int) {
        let Some(s) = self.sessions.get_mut(&id) else {
            self.early_exits.insert(id, status);
            return;
        };
        s.exit = Some(exit_code(status));
        if let Some(w) = s.pidfd.take() {
            self.poll.unwatch(w);
        }
        self.dirty.insert(id);
    }

    fn kill(&mut self, req: SessionKill, events: mpsc::Sender<KillEvent>) {
        let session = req
            .id
            .parse::<u64>()
            .ok()
            .and_then(|id| self.sessions.get_mut(&id));
        let Some(s) = session else {
            let message = format!("no session {}", shown_id(&req.id));
            let _ = events.send(KillEvent::Error { message });
            return;
        };
        let Some((signal, name)) = parse_signal(req.signal.as_deref().unwrap_or("SIGTERM")) else {
            let message = format!(
                "unknown signal {}",
                shown_id(req.signal.as_deref().unwrap_or(""))
            );
            let _ = events.send(KillEvent::Error { message });
            return;
        };
        if let (Some(code), false) = (s.exit, s.cgroup.populated()) {
            let _ = events.send(KillEvent::Exited {
                message: "Process exited".into(),
            });
            let _ = events.send(KillEvent::Complete { exit_code: code });
            return;
        }
        if s.kills.len() >= MAX_KILLS {
            let message = format!("too many kills of session {}", s.id);
            let _ = events.send(KillEvent::Error { message });
            return;
        }
        s.cgroup.signal(signal);
        let _ = events.send(KillEvent::Signal {
            message: format!("Signaling {name} to the processes of session {}", s.id),
            signal: name,
            pid: s.pid,
        });
        let timeout = req.timeout.unwrap_or(KILL_TIMEOUT).min(MAX_KILL_TIMEOUT);
        s.kills.push(KillWait {
            events,
            timeout,
            deadline: Instant::now() + Duration::from_secs(timeout),
            escalated: false,
        });
    }

    /// Reaps exited sessions without waiting on a spawn in progress.
    fn reap(&mut self) {
        self.reap_pending = !crate::children::reap_watched();
    }

    fn timers(&mut self) {
        let now = Instant::now();
        for s in self.sessions.values_mut() {
            if s.hangup_at.is_some_and(|at| at <= now) {
                s.hangup_at = None;
                s.cgroup.signal(libc::SIGHUP);
            }
        }
        if self.reap_pending || self.sessions.values().any(|s| !s.kills.is_empty()) {
            self.reap();
            self.commands();
            for s in self.sessions.values_mut().filter(|s| !s.kills.is_empty()) {
                let done = s.exit.filter(|_| !s.cgroup.populated());
                for mut wait in std::mem::take(&mut s.kills) {
                    let send = |event| {
                        let _ = wait.events.send(event);
                    };
                    match done {
                        Some(code) => {
                            send(if wait.escalated {
                                KillEvent::Killed {
                                    message: "Process killed".into(),
                                }
                            } else {
                                KillEvent::Exited {
                                    message: "Process exited".into(),
                                }
                            });
                            send(KillEvent::Complete { exit_code: code });
                        }
                        None if now < wait.deadline => s.kills.push(wait),
                        None if !wait.escalated => {
                            send(KillEvent::Timeout {
                                message: format!(
                                    "Timeout after {}s, sending SIGKILL",
                                    wait.timeout
                                ),
                            });
                            s.cgroup.signal(libc::SIGKILL);
                            wait.escalated = true;
                            wait.deadline = now + SIGKILL_WAIT;
                            s.kills.push(wait);
                        }
                        None => send(KillEvent::Error {
                            message: "the session did not exit after SIGKILL".into(),
                        }),
                    }
                }
            }
        }
        if now >= self.next_sweep {
            self.next_sweep = now + SWEEP_TICK;
            self.sweep(now);
        }
    }

    /// Drops sessions that finished more than KEEP_EXITED ago, and the
    /// oldest finished ones beyond MAX_EXITED.
    fn sweep(&mut self, now: Instant) {
        // A session still sending to an attachment or being killed stays.
        let mut ended: Vec<(Instant, u64)> = self
            .sessions
            .values()
            .filter(|s| s.attachments.is_empty() && s.kills.is_empty())
            .filter_map(|s| s.finished.map(|at| (at, s.id)))
            .collect();
        ended.sort();
        let excess = ended.len().saturating_sub(MAX_EXITED);
        for (i, (at, id)) in ended.into_iter().enumerate() {
            if i < excess || now.duration_since(at) >= KEEP_EXITED {
                let attachments = self.sessions[&id].attachments.clone();
                for aid in attachments {
                    self.detach(aid);
                }
                self.sessions.remove(&id);
                self.dirty.remove(&id);
            }
        }
    }

    /// Brings changed sessions up to date: finishes them, moves output to
    /// their attachments, and registers each descriptor for what it can do
    /// now.
    fn settle(&mut self) {
        while let Some(&sid) = self.dirty.iter().next() {
            self.dirty.remove(&sid);
            let Some(s) = self.sessions.get_mut(&sid) else {
                continue;
            };
            if s.finished.is_none() && s.exit.is_some() && s.outputs.is_empty() {
                s.finished = Some(Instant::now());
                s.hangup_at = None;
                release();
            }
            let stdin_done = s.finished.is_some() || (s.stdin_eof && s.stdin_queue.is_empty());
            if stdin_done && !(s.tty && s.finished.is_none()) {
                s.stdin_queue.clear();
                if let Some(w) = s.stdin.take() {
                    self.poll.unwatch(w);
                }
            }
            for aid in s.attachments.clone() {
                self.fill(aid);
                self.flush(aid);
                let unblock = self.attachments.get(&aid).is_some_and(|a| a.blocked)
                    && self.sessions[&sid].stdin_queue.len() < STDIN_QUEUE;
                if unblock {
                    self.attachments.get_mut(&aid).unwrap().blocked = false;
                    self.take_input(aid);
                }
            }
            self.register(sid);
        }
    }

    fn register(&mut self, sid: u64) {
        let rooms: Vec<(u8, usize)> = match self.sessions.get(&sid) {
            Some(s) => s
                .outputs
                .iter()
                .map(|(stream, _)| (*stream, self.room(sid, *stream)))
                .collect(),
            None => return,
        };
        let Some(s) = self.sessions.get_mut(&sid) else {
            return;
        };
        let mut failed = false;
        for ((_, w), (_, room)) in s.outputs.iter_mut().zip(&rooms) {
            failed |= self.poll.set(w, if *room > 0 { IN } else { 0 }).is_err();
        }
        if let Some(w) = s.stdin.as_mut() {
            let events = if s.stdin_queue.is_empty() { 0 } else { OUT };
            failed |= self.poll.set(w, events).is_err();
        }
        if let Some(w) = s.pidfd.as_mut() {
            failed |= self.poll.set(w, IN).is_err();
        }
        if failed {
            eprintln!("shuru-guest: session {sid}: {}", io::Error::last_os_error());
        }
        let end = s.ring.end();
        for aid in s.attachments.clone() {
            let Some(a) = self.attachments.get_mut(&aid) else {
                continue;
            };
            // A blocked attachment is not read, but its hangup still ends it.
            let mut events = RDHUP;
            if !a.blocked {
                events |= IN;
            }
            // Output it has not taken goes out when the socket has room.
            if a.sent < a.out.len() || a.cursor < end {
                events |= OUT;
            }
            if self.poll.set(&mut a.sock, events).is_err() {
                self.detach(aid);
            }
        }
    }
}

fn resize(s: &mut Session, rows: u16, cols: u16) {
    let Some(w) = &s.stdin else {
        return;
    };
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(w.fd.as_raw_fd(), libc::TIOCSWINSZ, &size) } == 0 {
        s.size = Some((rows, cols));
    }
}

/// Answers a connection that cannot attach. The answer is small and the
/// connection new, so it fits the socket buffer; if not, it is dropped.
fn refuse(mut conn: TcpStream, message: &str) {
    if conn.set_nonblocking(true).is_ok() {
        let _ = frame::write_frame(&mut conn, frame::ERROR, message.as_bytes());
    }
}

/// A session id or signal name from a request, bounded for an answer.
fn shown_id(id: &str) -> String {
    clip(id, &mut 64)
}

/// `now + after`, or None when that is past what an Instant holds.
fn after(after: Duration) -> Option<Instant> {
    Instant::now().checked_add(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_commands_fit_their_budget() {
        let fits = |command: &[String]| {
            let json = serde_json::to_string(&shown_command(command)).unwrap();
            assert!(json.len() <= SHOWN_COMMAND + 2, "{}", json.len());
        };
        fits(&["\u{1}".repeat(5000), "x".into()]);
        fits(&["\u{e9}".repeat(3000)]);
        fits(&vec![String::new(); 5000]);
        let short = ["sh".to_string(), "-c".into(), "echo \"hi\"".into()];
        assert_eq!(shown_command(&short), short);
    }
}
