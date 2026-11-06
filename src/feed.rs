//! Host-supplied standard input stream ([`StdinFeed`]).

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// A channel that supplies standard input from the host to scripts
/// ([`crate::Config::stdin_feed`]). A terminal host can await
/// [`StdinFeed::wanted`] and forward input with [`StdinFeed::send`]. A GUI
/// host can monitor [`StdinFeed::awaited_changed`] and submit an answer to
/// each read with [`StdinFeed::answer`] or [`StdinFeed::dismiss`]. Clones
/// share the same underlying stream.
#[derive(Clone, Default)]
pub struct StdinFeed {
    shared: Arc<Shared>,
}

/// Unique identifier for a single standard input read operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadId(NonZeroU64);

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    chunks: VecDeque<Chunk>,
    active_read: Option<ReadId>,
    read_count: u64,
    waiting: bool,
    blocked: bool,
    reader: Option<Waker>,
}

struct Chunk {
    /// Target read, or `None` for any read.
    target: Option<ReadId>,
    /// Input bytes, or `None` for EOF.
    data: Option<Vec<u8>>,
}

impl ReadId {
    /// Returns the 1-based number of this read among the reads that can wait.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl StdinFeed {
    pub fn new() -> StdinFeed {
        StdinFeed::default()
    }

    /// Appends `data` to the input stream. Sent data stays in the queue across
    /// reads until a script reads it.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if !data.is_empty() {
            self.shared.push(Chunk { target: None, data: Some(data) });
        }
    }

    /// Enqueues an end-of-input marker for the next read that reaches it.
    /// Subsequent reads wait for new input.
    pub fn end(&self) {
        self.shared.push(Chunk { target: None, data: None });
    }

    /// Submits `data` as the answer to `read`, as a user who types `data` and
    /// presses Return. The read gets `data`, a line end if `data` does not end
    /// with one, and then an end of input, so a read of any kind completes.
    /// The engine discards the part of the answer that the read does not use.
    /// Returns `false` if `read` does not wait for input.
    pub fn answer(&self, read: ReadId, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.shared.submit(read, Some(data))
    }

    /// Gives `read` an end of input, so `read_line` returns `None`. Returns
    /// `false` if `read` does not wait for input.
    pub fn dismiss(&self, read: ReadId) -> bool {
        self.shared.submit(read, None)
    }

    /// Returns `true` if a read is waiting for input and all other engine
    /// tasks are idle.
    pub fn is_wanted(&self) -> bool {
        self.shared.lock().wanted()
    }

    /// Waits until [`StdinFeed::is_wanted`] becomes `true`.
    pub async fn wanted(&self) {
        self.wait_until(|state| state.wanted().then_some(())).await;
    }

    /// Returns the [`ReadId`] of the active read if it is waiting for input,
    /// even if other tasks are running.
    pub fn awaited(&self) -> Option<ReadId> {
        self.shared.lock().awaited()
    }

    /// Waits until [`StdinFeed::awaited`] differs from `from` and returns the
    /// new value.
    pub async fn awaited_changed(&self, from: Option<ReadId>) -> Option<ReadId> {
        self.wait_until(|state| {
            let awaited = state.awaited();
            (awaited != from).then_some(awaited)
        })
        .await
    }

    async fn wait_until<T>(&self, predicate: impl Fn(&State) -> Option<T>) -> T {
        loop {
            let changed = self.shared.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = predicate(&self.shared.lock()) {
                return value;
            }
            changed.await;
        }
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
}

impl State {
    fn wanted(&self) -> bool {
        self.blocked && self.awaited().is_some()
    }

    fn awaited(&self) -> Option<ReadId> {
        self.active_read.filter(|_| self.waiting && self.chunks.is_empty())
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, chunk: Chunk) {
        let mut state = self.lock();
        state.chunks.push_back(chunk);
        self.notify(state);
    }

    fn submit(&self, read: ReadId, data: Option<Vec<u8>>) -> bool {
        let mut state = self.lock();
        if state.awaited() != Some(read) {
            return false;
        }
        if let Some(data) = data {
            state.chunks.push_back(Chunk { target: Some(read), data: Some(data) });
        }
        state.chunks.push_back(Chunk { target: Some(read), data: None });
        self.notify(state);
        true
    }

    fn notify(&self, mut state: MutexGuard<'_, State>) {
        let reader = state.reader.take();
        drop(state);
        if let Some(waker) = reader {
            waker.wake();
        }
        self.changed.notify_waiters();
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_waiters();
    }

    /// Marks the engine as idle until the returned guard is dropped.
    pub(crate) fn blocked(self: &Arc<Self>) -> Blocked {
        self.update(|state| state.blocked = true);
        Blocked(self.clone())
    }
}

/// Guard that marks the engine as idle while held.
pub(crate) struct Blocked(Arc<Shared>);

impl Drop for Blocked {
    fn drop(&mut self) {
        self.0.update(|state| state.blocked = false);
    }
}

/// Guard for an active [`FedReader`] read. Dropping the guard ends the read
/// and discards any unread targeted chunks.
pub(crate) struct ReadOp(Arc<Shared>, ReadId);

impl Drop for ReadOp {
    fn drop(&mut self) {
        let read_id = self.1;
        self.0.update(|state| {
            state.chunks.retain(|chunk| chunk.target != Some(read_id));
            state.active_read = None;
            state.waiting = false;
            state.reader = None;
        });
    }
}

/// Reader half of a [`StdinFeed`].
pub struct FedReader {
    shared: Arc<Shared>,
    buf: Vec<u8>,
    buf_target: Option<ReadId>,
    at: usize,
    ended: bool,
}

impl FedReader {
    pub(crate) fn new(feed: &StdinFeed) -> FedReader {
        FedReader { shared: feed.shared(), buf: Vec::new(), buf_target: None, at: 0, ended: false }
    }

    /// Starts a read that can wait for input, and gives it the next
    /// [`ReadId`].
    pub(crate) fn begin(&mut self) -> ReadOp {
        self.reset();
        let mut state = self.shared.lock();
        state.read_count += 1;
        let read_id = ReadId(NonZeroU64::new(state.read_count).expect("read count overflow"));
        state.active_read = Some(read_id);
        drop(state);
        ReadOp(self.shared.clone(), read_id)
    }

    /// Prepares a read that does not wait: an end of input or an answer from
    /// an earlier read is over.
    pub(crate) fn reset(&mut self) {
        self.ended = false;
        if self.buf_target.is_some() {
            self.at = self.buf.len();
        }
    }

    /// Returns buffered bytes available without waiting and whether the
    /// current read has reached EOF.
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        if self.at == self.buf.len() && !self.ended {
            let mut state = self.shared.lock();
            if let Some(chunk) = state.chunks.pop_front() {
                state.waiting = false;
                self.buf_target = chunk.target;
                match chunk.data {
                    Some(data) => (self.buf, self.at) = (data, 0),
                    None => self.ended = true,
                }
            }
        }
        (&self.buf[self.at..], self.ended)
    }
}

impl AsyncRead for FedReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let data = std::task::ready!(self.as_mut().poll_fill_buf(cx))?;
        let n = data.len().min(out.remaining());
        out.put_slice(&data[..n]);
        self.consume(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for FedReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        loop {
            let (data, ended) = this.available();
            if !data.is_empty() || ended {
                return Poll::Ready(Ok(&this.buf[this.at..]));
            }
            let mut state = this.shared.lock();
            if state.chunks.is_empty() {
                state.waiting = true;
                state.reader = Some(cx.waker().clone());
                drop(state);
                this.shared.changed.notify_waiters();
                return Poll::Pending;
            }
        }
    }

    fn consume(mut self: Pin<&mut Self>, n: usize) {
        self.at = (self.at + n).min(self.buf.len());
    }
}
