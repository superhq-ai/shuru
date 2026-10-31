use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, AcquireError, OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;

use crate::egress::{Egress, Tally};
use crate::stack::{ConnectionId, StackCommand};

/// Largest single send toward the guest. Must not exceed TO_GUEST_BUDGET.
const SEND_CHUNK: usize = 64 * 1024;

/// Guest data for one connection. Taking a payload returns its bytes to
/// the budget of the stack, so the stack reads more from the guest only as fast
/// as the connection consumes it. A payload over the egress pace is held
/// back first, which slows the guest the same way.
pub struct GuestRx {
    id: ConnectionId,
    data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    tally: Arc<Tally>,
    egress: Arc<Egress>,
    held: Option<(Vec<u8>, Pin<Box<Sleep>>)>,
}

impl GuestRx {
    pub(crate) fn new(
        id: ConnectionId,
        data_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        cmd_tx: mpsc::UnboundedSender<StackCommand>,
        tally: Arc<Tally>,
        egress: Arc<Egress>,
    ) -> Self {
        GuestRx {
            id,
            data_rx,
            cmd_tx,
            tally,
            egress,
            held: None,
        }
    }

    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        let (data, mut wait) = match self.held.take() {
            Some(held) => held,
            None => {
                let data = match self.data_rx.poll_recv(cx) {
                    Poll::Ready(Some(data)) => data,
                    polled => return polled,
                };
                match self.egress.spend(data.len(), Instant::now()) {
                    Some(delay) => (data, Box::pin(tokio::time::sleep(delay))),
                    None => return Poll::Ready(Some(self.release(data))),
                }
            }
        };
        if wait.as_mut().poll(cx).is_pending() {
            self.held = Some((data, wait));
            return Poll::Pending;
        }
        Poll::Ready(Some(self.release(data)))
    }

    fn release(&self, data: Vec<u8>) -> Vec<u8> {
        self.tally
            .up
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.egress.sent(data.len());
        let _ = self.cmd_tx.send(StackCommand::Consumed {
            id: self.id,
            n: data.len(),
        });
        data
    }
}

/// Sends toward the guest. Every byte holds a permit until the stack moves
/// it into the smoltcp socket, so a guest that stops reading makes `send`
/// wait instead of queueing.
#[derive(Clone)]
pub struct GuestTx {
    id: ConnectionId,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    budget: Arc<Semaphore>,
    tally: Arc<Tally>,
}

impl GuestTx {
    pub(crate) fn new(
        id: ConnectionId,
        cmd_tx: mpsc::UnboundedSender<StackCommand>,
        budget: Arc<Semaphore>,
        tally: Arc<Tally>,
    ) -> Self {
        GuestTx {
            id,
            cmd_tx,
            budget,
            tally,
        }
    }

    pub async fn send(&self, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(SEND_CHUNK) {
            self.budget
                .acquire_many(chunk.len() as u32)
                .await
                .map_err(|_| closed())?
                .forget();
            self.enqueue(chunk)?;
        }
        Ok(())
    }

    fn enqueue(&self, chunk: &[u8]) -> io::Result<()> {
        self.cmd_tx
            .send(StackCommand::Send {
                id: self.id,
                payload: chunk.to_vec(),
            })
            .map_err(|_| closed())?;
        self.tally
            .down
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connection closed")
}

type Acquire = Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

/// AsyncRead + AsyncWrite adapter over smoltcp channel-based data flow.
///
/// Allows tokio-rustls (and any other async I/O) to work with data
/// arriving from the smoltcp NetworkStack via channels.
pub struct ChannelStream {
    rx: GuestRx,
    tx: GuestTx,
    read_buf: Vec<u8>,
    read_pos: usize,
    /// Permits being acquired for a write, and how many.
    acquire: Option<(Acquire, usize)>,
}

impl ChannelStream {
    pub fn new(rx: GuestRx, tx: GuestTx) -> Self {
        ChannelStream {
            rx,
            tx,
            read_buf: Vec::new(),
            read_pos: 0,
            acquire: None,
        }
    }

    /// Prepend data to the read buffer (e.g., a ClientHello already consumed).
    pub fn prepend(&mut self, data: Vec<u8>) {
        if self.read_pos < self.read_buf.len() {
            // There's still unread data — prepend before it
            let remaining = self.read_buf[self.read_pos..].to_vec();
            self.read_buf = data;
            self.read_buf.extend_from_slice(&remaining);
        } else {
            self.read_buf = data;
        }
        self.read_pos = 0;
    }
}

impl AsyncRead for ChannelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Drain buffered data first
        if self.read_pos < self.read_buf.len() {
            let available = &self.read_buf[self.read_pos..];
            let n = available.len().min(buf.remaining());
            buf.put_slice(&available[..n]);
            self.read_pos += n;
            if self.read_pos >= self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }

        // Receive from channel
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.read_buf = data;
                    self.read_pos = n;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())), // EOF
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for ChannelStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = &mut *self;
        let (acquire, want) = this.acquire.get_or_insert_with(|| {
            let want = buf.len().min(SEND_CHUNK);
            let budget = this.tx.budget.clone();
            (Box::pin(budget.acquire_many_owned(want as u32)), want)
        });
        let permit = match acquire.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        let want = *want;
        this.acquire = None;
        permit.map_err(|_| closed())?.forget();
        // The caller may retry with a shorter buffer than the permits cover.
        let n = want.min(buf.len());
        if want > n {
            this.tx.budget.add_permits(want - n);
        }
        this.tx.enqueue(&buf[..n])?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = self.tx.cmd_tx.send(StackCommand::Close { id: self.tx.id });
        Poll::Ready(Ok(()))
    }
}
