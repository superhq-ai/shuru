//! One guest connection per exec session that has clients, fanned out to
//! them (RFC 0012). The hub reads the guest at the pace of its fastest
//! client and keeps the newest output for clients that join later. A
//! client more than `MAX_BEHIND` bytes behind is dropped with the offset it
//! reached, to attach again from there. The first client owns the input;
//! when the owner leaves, the next client to send input takes it over. The
//! guest connection closes when the session ends or the last client leaves,
//! which detaches it in the guest.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hyper::body::Bytes;
use shuru_proto::{frame, SessionAttach, SessionInfo, SessionStart};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch, Notify, OwnedMutexGuard, OwnedSemaphorePermit};
use tokio::task::JoinHandle;

use super::guest::{self, Conn};
use super::http::ApiError;
use super::sandboxes::GuestAddr;

/// Live output a client may have queued before it is dropped.
const MAX_BEHIND: usize = 1 << 20;
/// The hub reads on while some client has less than this queued.
const READ_ON_BELOW: usize = 256 << 10;
/// Output a hub keeps for clients that join later.
const KEPT: usize = 1 << 20;
const MAX_CLIENTS: usize = 64;
/// Input frames waiting for the guest.
const INPUT_QUEUE: usize = 16;
/// How long the guest has to connect and answer a start or attach.
const HANDSHAKE: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq)]
pub enum Event {
    /// `frame::STDOUT` or `frame::STDERR`, and the data.
    Output(u8, Bytes),
    /// The exit status as the API sends it.
    Exit(u8),
    Error(String),
    /// Dropped for falling behind; attach again from this offset.
    Behind(u64),
    /// The guest connection ended without an exit.
    Gone,
}

/// The hub of a session, if it has one, locked while one is opened.
type Slot = tokio::sync::Mutex<Option<Arc<Hub>>>;

/// The hubs of one guest, by session id.
#[derive(Default)]
pub struct Hubs {
    sessions: Mutex<HashMap<u64, Arc<Slot>>>,
    /// Held shared by each start until its hub is in place, and alone by an
    /// attach that opens a hub until its hub is in place, so the guest
    /// never has two hub connections for one session.
    starting: tokio::sync::RwLock<()>,
}

/// The slot of a session, locked. It leaves the map when let go without
/// a hub, so a failed or cancelled attach leaves nothing behind.
struct Reserved {
    hubs: Arc<Hubs>,
    id: u64,
    slot: Arc<Slot>,
    hub: OwnedMutexGuard<Option<Arc<Hub>>>,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        if self.hub.is_none() {
            let mut sessions = self.hubs.sessions();
            if sessions
                .get(&self.id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.slot))
            {
                sessions.remove(&self.id);
            }
        }
    }
}

impl Hubs {
    fn sessions(&self) -> MutexGuard<'_, HashMap<u64, Arc<Slot>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The slot of session `id`, locked, and still the one in the map.
    async fn lock_slot(self: &Arc<Self>, id: u64) -> Reserved {
        loop {
            let slot = self.sessions().entry(id).or_default().clone();
            let hub = slot.clone().lock_owned().await;
            if self
                .sessions()
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &slot))
            {
                return Reserved {
                    hubs: self.clone(),
                    id,
                    slot,
                    hub,
                };
            }
        }
    }

    /// Starts a session and joins it as its first client.
    pub async fn start(
        self: &Arc<Self>,
        addr: &GuestAddr,
        start: &SessionStart,
    ) -> Result<Attachment, ApiError> {
        let _starting = self.starting.read().await;
        let (conn, info) = handshake(addr, frame::SESSION_START, start).await?;
        let id = info
            .id
            .parse()
            .map_err(|_| ApiError::internal(format!("the guest named a session {}", info.id)))?;
        let mut slot = self.lock_slot(id).await;
        let (hub, attachment) = Hub::open(self, id, info, conn, None);
        *slot.hub = Some(hub);
        Ok(attachment)
    }

    /// Joins session `id`, from `offset` or from the oldest output kept.
    pub async fn attach(
        self: &Arc<Self>,
        addr: &GuestAddr,
        id: u64,
        offset: Option<u64>,
    ) -> Result<Attachment, ApiError> {
        if let Some(attachment) = self.join(id, offset).await? {
            return Ok(attachment);
        }
        let _no_starts = self.starting.write().await;
        let mut slot = self.lock_slot(id).await;
        if let Some(hub) = slot.hub.clone() {
            if let Some(attachment) = hub.join(offset)? {
                return Ok(attachment);
            }
            // The guest takes the new connection as the owner only once
            // the old one is gone.
            hub.closed().await;
        }
        let attach = SessionAttach {
            id: id.to_string(),
            output_offset: offset,
        };
        let (conn, info) = handshake(addr, frame::SESSION_ATTACH, &attach).await?;
        let (hub, attachment) = Hub::open(self, id, info, conn, offset);
        *slot.hub = Some(hub);
        Ok(attachment)
    }

    /// Joins the hub of session `id` if one is open.
    async fn join(
        self: &Arc<Self>,
        id: u64,
        offset: Option<u64>,
    ) -> Result<Option<Attachment>, ApiError> {
        let slot = self.lock_slot(id).await;
        match slot.hub.as_ref() {
            Some(hub) => hub.join(offset),
            None => Ok(None),
        }
    }

    async fn remove(self: &Arc<Self>, hub: &Arc<Hub>) {
        let mut slot = self.lock_slot(hub.id).await;
        if slot
            .hub
            .as_ref()
            .is_some_and(|other| Arc::ptr_eq(other, hub))
        {
            *slot.hub = None;
        }
    }
}

/// Connects to the guest and sends a start or attach, within `HANDSHAKE`.
async fn handshake(
    addr: &GuestAddr,
    kind: u8,
    request: &impl serde::Serialize,
) -> Result<(Conn, SessionInfo), ApiError> {
    tokio::time::timeout(HANDSHAKE, async {
        let mut conn = guest::connect(addr, &addr.streams).await?;
        guest::send_json(&mut conn.stream, kind, request).await?;
        let info = guest::session_info(&mut conn.stream).await?;
        Ok((conn, info))
    })
    .await
    .map_err(|_| ApiError::unavailable("the guest did not answer in time"))?
}

pub struct Hub {
    id: u64,
    base: SessionInfo,
    state: Mutex<State>,
    /// Whole frames for the writer task, so a client that goes away in the
    /// middle of input never cuts a frame short.
    to_guest: mpsc::Sender<(u8, Vec<u8>)>,
    /// A client drained output, joined or left.
    changed: Notify,
    /// Set once the guest connection is closed.
    closed: watch::Sender<bool>,
}

/// An event and the live bytes it counts toward the lag of its client.
type Queued = (Event, usize);

struct Client {
    tx: mpsc::UnboundedSender<Queued>,
    queued: Arc<AtomicUsize>,
}

#[derive(Default)]
struct State {
    clients: HashMap<u64, Client>,
    owner: Option<u64>,
    next_client: u64,
    kept: VecDeque<(u8, Bytes)>,
    kept_bytes: usize,
    /// The offset of the first byte kept.
    start: u64,
    /// The offset of the next byte.
    end: u64,
    /// Set when the last client leaves or the session ends; no client
    /// joins after it.
    done: bool,
}

impl Hub {
    /// A hub over `conn` with its first client, which joins before the hub
    /// reads so the hub never starts empty.
    fn open(
        hubs: &Arc<Hubs>,
        id: u64,
        base: SessionInfo,
        conn: Conn,
        offset: Option<u64>,
    ) -> (Arc<Hub>, Attachment) {
        let Conn { stream, permit } = conn;
        let (from_guest, to_guest) = stream.into_split();
        let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
        let hub = Arc::new(Hub {
            id,
            state: Mutex::new(State {
                start: base.output_offset,
                end: base.output_offset,
                ..State::default()
            }),
            base,
            to_guest: input_tx,
            changed: Notify::new(),
            closed: watch::Sender::new(false),
        });
        let attachment = hub.add(&mut hub.state(), offset);
        let ended = Arc::new(Notify::new());
        let writer = tokio::spawn(write(to_guest, input_rx, ended.clone()));
        tokio::spawn(read(
            hub.clone(),
            hubs.clone(),
            from_guest,
            Teardown {
                writer,
                ended,
                permit,
            },
        ));
        (hub, attachment)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns once the guest connection is closed. Only a hub that is done
    /// gets there, and soon.
    async fn closed(&self) {
        let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
    }

    /// A new client, or `None` once the hub is done.
    fn join(self: &Arc<Self>, offset: Option<u64>) -> Result<Option<Attachment>, ApiError> {
        let mut state = self.state();
        if state.done {
            return Ok(None);
        }
        if state.clients.len() >= MAX_CLIENTS {
            return Err(ApiError::unavailable(format!(
                "session {} has {MAX_CLIENTS} clients",
                self.id
            )));
        }
        let attachment = self.add(&mut state, offset);
        drop(state);
        // The reader may be waiting for a client with room.
        self.changed.notify_waiters();
        Ok(Some(attachment))
    }

    /// Adds a client with the output kept from `offset` on queued. Replayed
    /// output does not count toward its lag.
    fn add(self: &Arc<Self>, state: &mut State, offset: Option<u64>) -> Attachment {
        let from = offset.unwrap_or(state.start).clamp(state.start, state.end);
        let gap = offset
            .filter(|asked| *asked < from)
            .map(|asked| from - asked);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut at = state.start;
        for (stream, data) in &state.kept {
            let len = data.len() as u64;
            if at + len > from {
                let skip = from.saturating_sub(at) as usize;
                let _ = tx.send((Event::Output(*stream, data.slice(skip..)), 0));
            }
            at += len;
        }
        let id = state.next_client;
        state.next_client += 1;
        let is_owner = *state.owner.get_or_insert(id) == id;
        let queued = Arc::new(AtomicUsize::new(0));
        state.clients.insert(
            id,
            Client {
                tx,
                queued: queued.clone(),
            },
        );
        let mut info = self.base.clone();
        info.is_owner = is_owner;
        info.output_offset = from;
        Attachment {
            input: Input {
                hub: self.clone(),
                id,
            },
            rx,
            queued,
            info,
            gap,
        }
    }

    fn leave(&self, id: u64) {
        let mut state = self.state();
        state.clients.remove(&id);
        if state.owner == Some(id) {
            state.owner = None;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Hands output to every client, dropping those too far behind.
    fn publish(&self, stream: u8, data: Bytes) {
        let mut state = self.state();
        let before = state.end;
        let len = data.len();
        state.end += len as u64;
        state.kept.push_back((stream, data.clone()));
        state.kept_bytes += len;
        while state.kept_bytes > KEPT && state.kept.len() > 1 {
            if let Some((_, old)) = state.kept.pop_front() {
                state.kept_bytes -= old.len();
                state.start += old.len() as u64;
            }
        }
        let mut behind = Vec::new();
        for (id, client) in &state.clients {
            if client.queued.load(Ordering::Acquire) + len > MAX_BEHIND {
                let _ = client.tx.send((Event::Behind(before), 0));
                behind.push(*id);
            } else {
                client.queued.fetch_add(len, Ordering::AcqRel);
                let _ = client.tx.send((Event::Output(stream, data.clone()), len));
            }
        }
        for id in behind {
            state.clients.remove(&id);
            if state.owner == Some(id) {
                state.owner = None;
            }
        }
    }

    /// Sends the last event to every client and takes no more.
    fn finish(&self, event: impl Fn() -> Event) {
        let mut state = self.state();
        state.done = true;
        for client in state.clients.values() {
            let _ = client.tx.send((event(), 0));
        }
        state.clients.clear();
    }

    /// Whether some client has room for more output. Once no client is
    /// left the hub is done and this is `None`.
    fn room(&self) -> Option<bool> {
        let mut state = self.state();
        if state.clients.is_empty() {
            state.done = true;
            return None;
        }
        Some(
            state
                .clients
                .values()
                .any(|c| c.queued.load(Ordering::Acquire) < READ_ON_BELOW),
        )
    }

    /// Waits until some client has room, or returns false once none is left.
    async fn wait_for_room(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.room() {
                None => return false,
                Some(true) => return true,
                Some(false) => changed.await,
            }
        }
    }

    /// Returns once no client is left.
    async fn emptied(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.room().is_none() {
                return;
            }
            changed.await;
        }
    }

    /// Input from client `id`, which reaches the guest only from the owner,
    /// except a kill. A client with no owner in place becomes it.
    /// A client the hub no longer has, because it ended or dropped it,
    /// gets an error.
    async fn input(&self, id: u64, kind: u8, payload: &[u8]) -> io::Result<()> {
        {
            let mut state = self.state();
            if state.done || !state.clients.contains_key(&id) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if kind != frame::KILL && *state.owner.get_or_insert(id) != id {
                return Ok(());
            }
        }
        self.to_guest
            .send((kind, payload.to_vec()))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

/// Writes input frames to the guest until the hub ends.
async fn write(
    mut to_guest: OwnedWriteHalf,
    mut frames: mpsc::Receiver<(u8, Vec<u8>)>,
    ended: Arc<Notify>,
) {
    let writing = async {
        while let Some((kind, payload)) = frames.recv().await {
            if guest::write_frame(&mut to_guest, kind, &payload)
                .await
                .is_err()
            {
                return;
            }
        }
    };
    tokio::select! {
        () = writing => {}
        () = ended.notified() => {}
    }
}

/// What the reader lets go of when the hub ends: it stops the writer and
/// waits for it, so the guest connection is closed, whoever still holds
/// the hub, before its permit returns.
struct Teardown {
    writer: JoinHandle<()>,
    ended: Arc<Notify>,
    permit: OwnedSemaphorePermit,
}

/// Reads the guest connection of `hub` until the session exits or no
/// client is left, then closes it.
async fn read(hub: Arc<Hub>, hubs: Arc<Hubs>, mut from_guest: OwnedReadHalf, teardown: Teardown) {
    while hub.wait_for_room().await {
        let frame = tokio::select! {
            frame = guest::read_frame(&mut from_guest) => frame,
            () = hub.emptied() => break,
        };
        match frame {
            Ok(Some((frame::STDOUT | frame::STDERR, data))) if data.is_empty() => {}
            Ok(Some((kind @ (frame::STDOUT | frame::STDERR), data))) => {
                hub.publish(kind, data.into())
            }
            Ok(Some((frame::EXIT, payload))) => {
                let code = guest::exit_byte(&payload);
                hub.finish(|| Event::Exit(code));
                break;
            }
            Ok(Some((frame::ERROR, payload))) => {
                let message = String::from_utf8_lossy(&payload).into_owned();
                hub.finish(|| Event::Error(message.clone()));
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                hub.finish(|| Event::Gone);
                break;
            }
        }
    }
    hub.state().done = true;
    let Teardown {
        writer,
        ended,
        permit,
    } = teardown;
    ended.notify_one();
    let _ = writer.await;
    drop(from_guest);
    drop(permit);
    hub.closed.send_replace(true);
    hubs.remove(&hub).await;
}

/// A client of a session.
pub struct Attachment {
    input: Input,
    rx: mpsc::UnboundedReceiver<Queued>,
    queued: Arc<AtomicUsize>,
    /// The session as this client sees it: whether it owns the input, and
    /// the offset its output starts at.
    pub info: SessionInfo,
    /// Bytes between the offset asked for and the oldest one kept.
    pub gap: Option<u64>,
}

impl Attachment {
    /// The next event, `None` after the last.
    pub async fn recv(&mut self) -> Option<Event> {
        let (event, counted) = self.rx.recv().await?;
        if counted > 0 {
            self.queued.fetch_sub(counted, Ordering::AcqRel);
            self.input.hub.changed.notify_waiters();
        }
        Some(event)
    }

    /// Sends input as this client, for use beside `recv`.
    pub fn input(&self) -> Input {
        self.input.clone()
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.input.hub.leave(self.input.id);
    }
}

#[derive(Clone)]
pub struct Input {
    hub: Arc<Hub>,
    id: u64,
}

impl Input {
    pub async fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        self.hub.input(self.id, kind, payload).await
    }

    /// Sends `data` as STDIN frames.
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(guest::INPUT_CHUNK) {
            self.send(frame::STDIN, chunk).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;
    use tokio::sync::Semaphore;

    fn info(output_offset: u64) -> SessionInfo {
        SessionInfo {
            id: "1".into(),
            command: vec!["sh".into()],
            workdir: "/".into(),
            tty: false,
            created: 0,
            is_active: true,
            last_activity: 0,
            bytes_per_second: 0.0,
            rows: None,
            cols: None,
            exit_code: None,
            is_owner: true,
            output_offset,
        }
    }

    /// A hub at `output_offset` with its first client, and the guest end.
    async fn hub(output_offset: u64) -> (Arc<Hubs>, Arc<Hub>, Attachment, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let hubs = Arc::new(Hubs::default());
        let conn = Conn {
            stream: ours,
            permit,
        };
        let (hub, first) = Hub::open(&hubs, 1, info(output_offset), conn, None);
        let mut slot = hubs.lock_slot(1).await;
        *slot.hub = Some(hub.clone());
        drop(slot);
        (hubs, hub, first, theirs)
    }

    #[tokio::test]
    async fn a_slot_left_empty_is_forgotten() {
        let hubs = Arc::new(Hubs::default());
        drop(hubs.lock_slot(5).await);
        assert!(hubs.sessions().is_empty());
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let waiting = async {
            while !done() {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap_or_else(|_| panic!("{what}"));
    }

    fn output(data: &'static [u8]) -> Event {
        Event::Output(frame::STDOUT, Bytes::from_static(data))
    }

    #[tokio::test]
    async fn a_later_client_gets_what_is_kept() {
        let (_hubs, hub, mut first, _guest) = hub(100).await;
        hub.publish(frame::STDOUT, Bytes::from_static(b"hello "));
        hub.publish(frame::STDOUT, Bytes::from_static(b"world"));
        assert_eq!(first.recv().await, Some(output(b"hello ")));

        let mut second = hub.join(Some(103)).unwrap().unwrap();
        assert_eq!((second.info.output_offset, second.gap), (103, None));
        assert!(!second.info.is_owner);
        assert_eq!(second.recv().await, Some(output(b"lo ")));
        assert_eq!(second.recv().await, Some(output(b"world")));

        let third = hub.join(Some(40)).unwrap().unwrap();
        assert_eq!((third.info.output_offset, third.gap), (100, Some(60)));
        let last = hub.join(Some(500)).unwrap().unwrap();
        assert_eq!((last.info.output_offset, last.gap), (111, None));
    }

    #[tokio::test]
    async fn keeps_the_newest_output() {
        let (_hubs, hub, _first, _guest) = hub(0).await;
        let chunk = Bytes::from(vec![b'x'; 300 << 10]);
        for _ in 0..4 {
            hub.publish(frame::STDOUT, chunk.clone());
        }
        let state = hub.state();
        assert_eq!(state.kept.len(), 3);
        assert_eq!((state.start, state.end), (300 << 10, 1200 << 10));
    }

    #[tokio::test]
    async fn drops_a_client_too_far_behind() {
        let (_hubs, hub, mut slow, _guest) = hub(0).await;
        let mut fast = hub.join(None).unwrap().unwrap();
        let chunk = Bytes::from(vec![b'x'; 256 << 10]);
        for _ in 0..5 {
            hub.publish(frame::STDOUT, chunk.clone());
            assert!(matches!(fast.recv().await, Some(Event::Output(..))));
        }
        for _ in 0..4 {
            assert!(matches!(slow.recv().await, Some(Event::Output(..))));
        }
        assert_eq!(slow.recv().await, Some(Event::Behind(1 << 20)));
        assert_eq!(slow.recv().await, None);
        assert_eq!(hub.state().owner, None);
    }

    #[tokio::test]
    async fn a_client_that_joins_wakes_a_paused_hub() {
        let (_hubs, hub, _stuck, mut guest) = hub(0).await;
        // Into its first read, after which the hub pauses.
        tokio::task::yield_now().await;
        hub.publish(frame::STDOUT, Bytes::from(vec![b'x'; READ_ON_BELOW]));
        guest::write_frame(&mut guest, frame::STDOUT, b"early")
            .await
            .unwrap();
        until("the hub reads", || hub.state().end > READ_ON_BELOW as u64).await;
        let mut joined = hub.join(None).unwrap().unwrap();
        assert!(matches!(joined.recv().await, Some(Event::Output(..))));
        assert_eq!(joined.recv().await, Some(output(b"early")));
        guest::write_frame(&mut guest, frame::STDOUT, b"late")
            .await
            .unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), joined.recv()).await;
        assert_eq!(next.unwrap(), Some(output(b"late")));
    }

    #[tokio::test]
    async fn input_comes_from_the_owner() {
        let (_hubs, hub, first, mut guest) = hub(0).await;
        let second = hub.join(None).unwrap().unwrap();
        second.input().write(b"ignored").await.unwrap();
        second.input().send(frame::KILL, &[]).await.unwrap();
        first.input().write(b"typed").await.unwrap();
        drop(first);
        second.input().write(b"taken").await.unwrap();
        for (kind, payload) in [
            (frame::KILL, &b""[..]),
            (frame::STDIN, b"typed"),
            (frame::STDIN, b"taken"),
        ] {
            assert_eq!(
                guest::read_frame(&mut guest).await.unwrap(),
                Some((kind, payload.to_vec()))
            );
        }
    }

    #[tokio::test]
    async fn ends_every_client_and_closes() {
        let (hubs, hub, mut first, mut guest) = hub(0).await;
        let mut second = hub.join(None).unwrap().unwrap();
        guest::write_frame(&mut guest, frame::STDOUT, b"out")
            .await
            .unwrap();
        guest::write_frame(&mut guest, frame::EXIT, &frame::exit_payload(7))
            .await
            .unwrap();
        for client in [&mut first, &mut second] {
            assert_eq!(client.recv().await, Some(output(b"out")));
            assert_eq!(client.recv().await, Some(Event::Exit(7)));
            assert_eq!(client.recv().await, None);
        }
        assert!(hub.join(None).unwrap().is_none());
        // Closed although the clients still hold the hub.
        assert_eq!(guest::read_frame(&mut guest).await.unwrap(), None);
        until("the hub is removed", || hubs.sessions().is_empty()).await;
        assert!(first.input().send(frame::STDIN, b"x").await.is_err());
    }

    #[tokio::test]
    async fn closes_when_the_last_client_leaves() {
        let (_hubs, hub, first, mut guest) = hub(0).await;
        drop(first);
        assert_eq!(guest::read_frame(&mut guest).await.unwrap(), None);
        assert!(hub.join(None).unwrap().is_none());
        tokio::time::timeout(Duration::from_secs(5), hub.closed())
            .await
            .unwrap();
    }
}
