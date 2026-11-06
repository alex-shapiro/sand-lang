//! Host-supplied standard input stream ([`StdinFeed`]).

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// A channel that supplies standard input from the host to a session's scripts
/// ([`crate::Session::stdin_feed`]). Terminal hosts wait for
/// [`StdinFeed::wait_until_input_needed`] and forward input with
/// [`StdinFeed::send`]. Interactive applications wait for
/// [`StdinFeed::wait_for_request`] and respond to each [`StdinRequest`]. Clones
/// share the same underlying stream.
#[derive(Clone)]
pub struct StdinFeed {
    channel: Arc<StdinChannel>,
}

/// A pending standard input read waiting for a host response. Respond to the
/// read with [`StdinRequest::answer`] or [`StdinRequest::dismiss`]. Two
/// requests are equal if they refer to the same read operation.
#[derive(Clone)]
pub struct StdinRequest {
    channel: Arc<StdinChannel>,
    read: StdinReadId,
}

/// Identifier of a standard input read, assigned the first time the read waits
/// for input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StdinReadId(NonZeroU64);

/// State shared between the host's [`StdinFeed`] handles and the session's
/// reader.
#[derive(Default)]
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
}

/// Input queue and reader state for a [`StdinChannel`].
#[derive(Default)]
struct StdinChannelState {
    queue: VecDeque<QueuedInput>,
    reads_started: u64,
    /// True if the engine has no runnable tasks or ready I/O.
    engine_idle: bool,
    /// The active read operation, once it has waited for input.
    waiting_read: Option<WaitingRead>,
}

/// A read operation that has waited for input.
struct WaitingRead {
    id: StdinReadId,
    /// True if the read is currently waiting for input. Set to `false` when the
    /// read consumes queued input, and back to `true` if it waits again.
    waiting: bool,
    waker: Option<Waker>,
}

struct QueuedInput {
    /// Target read, or `None` for any read.
    target: Option<StdinReadId>,
    /// Input bytes, or `None` for EOF.
    data: Option<Vec<u8>>,
}

impl QueuedInput {
    fn is_for_read(&self, read: Option<StdinReadId>) -> bool {
        self.target.is_none() || self.target == read
    }
}

impl StdinFeed {
    pub(crate) fn new() -> StdinFeed {
        StdinFeed { channel: Arc::default() }
    }

    /// Creates the session's reader for this feed.
    pub(crate) fn reader(&self) -> StdinFeedReader {
        StdinFeedReader { channel: self.channel.clone(), buf: Vec::new(), buf_target: None, at: 0, ended: false }
    }

    /// Appends `data` to the input stream. Queued data persists across reads
    /// until a script consumes it.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if !data.is_empty() {
            self.channel.push(QueuedInput { target: None, data: Some(data) });
        }
    }

    /// Enqueues an end-of-input marker for the next read that reaches it.
    /// Later reads wait for new input.
    pub fn end(&self) {
        self.channel.push(QueuedInput { target: None, data: None });
    }

    /// Returns `true` if a read is waiting for input and all other engine
    /// tasks are idle.
    pub fn needs_input(&self) -> bool {
        self.channel.lock().needs_input()
    }

    /// Waits until [`StdinFeed::needs_input`] is `true`.
    pub async fn wait_until_input_needed(&self) {
        self.channel.wait_until(|state| state.needs_input().then_some(())).await;
    }

    /// Waits until a read needs input, even if other tasks are running, and
    /// returns its [`StdinRequest`].
    pub async fn wait_for_request(&self) -> StdinRequest {
        let read = self.channel.wait_until(StdinChannelState::waiting_read_id).await;
        StdinRequest { channel: self.channel.clone(), read }
    }
}

impl StdinRequest {
    /// Submits `data`, a trailing newline if `data` lacks one, and an
    /// end-of-input marker as the response to the read. This completes any
    /// read operation. When the read finishes, the engine discards any unread
    /// bytes from the response. Returns `false` if the read is already closed.
    pub fn answer(&self, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.channel.answer_read(self.read, Some(data))
    }

    /// Submits an end-of-input marker to the read so that `read_line` returns
    /// `None` if the read has no buffered data. Returns `false` if the read is
    /// already closed.
    pub fn dismiss(&self) -> bool {
        self.channel.answer_read(self.read, None)
    }

    /// Returns `true` if the read is still waiting for a response.
    pub fn is_open(&self) -> bool {
        self.channel.lock().waiting_read_id() == Some(self.read)
    }

    /// Waits until the read receives input or ends.
    pub async fn wait_until_closed(&self) {
        let read = self.read;
        self.channel.wait_until(|state| (state.waiting_read_id() != Some(read)).then_some(())).await;
    }
}

impl PartialEq for StdinRequest {
    fn eq(&self, other: &StdinRequest) -> bool {
        Arc::ptr_eq(&self.channel, &other.channel) && self.read == other.read
    }
}

impl Eq for StdinRequest {}

impl std::fmt::Debug for StdinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StdinRequest").field(&self.read.0).finish()
    }
}

impl StdinChannelState {
    fn needs_input(&self) -> bool {
        self.engine_idle && self.waiting_read_id().is_some()
    }

    /// Returns the ID of the read waiting for unqueued input, if any.
    fn waiting_read_id(&self) -> Option<StdinReadId> {
        let read = self.waiting_read.as_ref().filter(|r| r.waiting)?;
        (!self.queue.iter().any(|input| input.is_for_read(Some(read.id)))).then_some(read.id)
    }

    fn read_id(&self) -> Option<StdinReadId> {
        self.waiting_read.as_ref().map(|r| r.id)
    }
}

impl StdinChannel {
    fn lock(&self) -> MutexGuard<'_, StdinChannelState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, input: QueuedInput) {
        let mut state = self.lock();
        state.queue.push_back(input);
        self.notify(state);
    }

    fn answer_read(&self, read: StdinReadId, data: Option<Vec<u8>>) -> bool {
        let mut state = self.lock();
        if state.waiting_read_id() != Some(read) {
            return false;
        }
        if let Some(data) = data {
            state.queue.push_back(QueuedInput { target: Some(read), data: Some(data) });
        }
        state.queue.push_back(QueuedInput { target: Some(read), data: None });
        self.notify(state);
        true
    }

    async fn wait_until<T>(&self, predicate: impl Fn(&StdinChannelState) -> Option<T>) -> T {
        loop {
            let changed = self.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = predicate(&self.lock()) {
                return value;
            }
            changed.await;
        }
    }

    fn notify(&self, mut state: MutexGuard<'_, StdinChannelState>) {
        let waker = state.waiting_read.as_mut().and_then(|r| r.waker.take());
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
        self.changed.notify_waiters();
    }

    fn update(&self, f: impl FnOnce(&mut StdinChannelState)) {
        f(&mut self.lock());
        self.changed.notify_waiters();
    }
}

/// Notifies the [`StdinChannel`] when the engine is idle.
pub(crate) struct StdinIdleSignal {
    channel: Arc<StdinChannel>,
}

impl StdinIdleSignal {
    /// Marks the engine as idle until the returned guard is dropped.
    pub(crate) fn idle(&self) -> StdinIdleGuard {
        self.channel.update(|state| state.engine_idle = true);
        StdinIdleGuard { channel: self.channel.clone() }
    }
}

/// Guard that marks the engine as idle while held.
pub(crate) struct StdinIdleGuard {
    channel: Arc<StdinChannel>,
}

impl Drop for StdinIdleGuard {
    fn drop(&mut self) {
        self.channel.update(|state| state.engine_idle = false);
    }
}

/// The session's reader for a [`StdinFeed`]. Call [`StdinFeedReader::begin_read`]
/// to start a read operation.
pub(crate) struct StdinFeedReader {
    channel: Arc<StdinChannel>,
    buf: Vec<u8>,
    buf_target: Option<StdinReadId>,
    at: usize,
    ended: bool,
}

impl StdinFeedReader {
    pub(crate) fn idle_signal(&self) -> StdinIdleSignal {
        StdinIdleSignal { channel: self.channel.clone() }
    }

    /// Starts a read operation. The read receives a [`StdinReadId`] the first
    /// time it waits for input. It ignores any EOF or targeted response from
    /// an earlier read.
    pub(crate) fn begin_read(&mut self) -> ActiveStdinRead<'_> {
        self.ended = false;
        if self.buf_target.is_some() {
            self.at = self.buf.len();
        }
        ActiveStdinRead { reader: self }
    }
}

/// An active read operation on a [`StdinFeedReader`]. Dropping this guard ends
/// the read and discards any unread input targeted at it.
pub(crate) struct ActiveStdinRead<'a> {
    reader: &'a mut StdinFeedReader,
}

impl ActiveStdinRead<'_> {
    /// Returns buffered bytes available without waiting and whether the
    /// current read has reached EOF.
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        let r = &mut *self.reader;
        if r.at == r.buf.len() && !r.ended {
            let mut state = r.channel.lock();
            let read = state.read_id();
            let next = state.queue.iter().position(|input| input.is_for_read(read));
            if let Some(input) = next.and_then(|i| state.queue.remove(i)) {
                if let Some(read) = &mut state.waiting_read {
                    read.waiting = false;
                }
                r.buf_target = input.target;
                match input.data {
                    Some(data) => (r.buf, r.at) = (data, 0),
                    None => r.ended = true,
                }
            }
        }
        (&r.buf[r.at..], r.ended)
    }
}

impl Drop for ActiveStdinRead<'_> {
    fn drop(&mut self) {
        self.reader.channel.update(|state| {
            let Some(read) = state.waiting_read.take() else { return };
            state.queue.retain(|input| input.target != Some(read.id));
        });
    }
}

impl AsyncRead for ActiveStdinRead<'_> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let data = std::task::ready!(self.as_mut().poll_fill_buf(cx))?;
        let n = data.len().min(out.remaining());
        out.put_slice(&data[..n]);
        self.consume(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for ActiveStdinRead<'_> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        loop {
            let (data, ended) = this.available();
            if !data.is_empty() || ended {
                let r = &*this.reader;
                return Poll::Ready(Ok(&r.buf[r.at..]));
            }
            let mut state = this.reader.channel.lock();
            let read = state.read_id();
            if !state.queue.iter().any(|input| input.is_for_read(read)) {
                let id = read.unwrap_or_else(|| {
                    state.reads_started += 1;
                    StdinReadId(NonZeroU64::new(state.reads_started).expect("read count overflow"))
                });
                let read = state.waiting_read.get_or_insert(WaitingRead { id, waiting: true, waker: None });
                read.waiting = true;
                read.waker = Some(cx.waker().clone());
                drop(state);
                this.reader.channel.changed.notify_waiters();
                return Poll::Pending;
            }
        }
    }

    fn consume(self: Pin<&mut Self>, n: usize) {
        let r = &mut *self.get_mut().reader;
        r.at = (r.at + n).min(r.buf.len());
    }
}
