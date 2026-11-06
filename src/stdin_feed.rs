//! Host-supplied standard input stream ([`StdinFeed`]).

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// A channel that supplies standard input from the host to the scripts of
/// one session ([`crate::Session::stdin_feed`]). A terminal host waits for
/// [`StdinFeed::wait_until_input_needed`] and forwards input with
/// [`StdinFeed::send`]. An app with its own input field waits for
/// [`StdinFeed::wait_for_request`] and answers each [`StdinRequest`]. Clones
/// share the same underlying stream.
#[derive(Clone)]
pub struct StdinFeed {
    channel: Arc<StdinChannel>,
}

/// A read of standard input that waits for the host to answer it. Answer it
/// with [`StdinRequest::answer`] or [`StdinRequest::dismiss`]. Two requests
/// are equal if they are for the same read.
#[derive(Clone)]
pub struct StdinRequest {
    channel: Arc<StdinChannel>,
    read: StdinReadId,
}

/// Identifier of one read of standard input, given the first time the read
/// waits for input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StdinReadId(NonZeroU64);

/// The state that the host's [`StdinFeed`] handles share with the session's
/// reader.
#[derive(Default)]
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
}

/// The input queue of a [`StdinChannel`] and the state of its reader.
#[derive(Default)]
struct StdinChannelState {
    queue: VecDeque<QueuedInput>,
    reads_started: u64,
    /// Whether the engine of the reader has nothing to run.
    engine_idle: bool,
    /// The read under way, after it first waits for input.
    waiting_read: Option<WaitingRead>,
}

/// A read that has waited for input.
struct WaitingRead {
    id: StdinReadId,
    /// Whether the read waits for input now. It is false from the time a
    /// read takes queued input until it waits again.
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

    /// The session's reader of this feed.
    pub(crate) fn reader(&self) -> StdinFeedReader {
        StdinFeedReader { channel: self.channel.clone(), buf: Vec::new(), buf_target: None, at: 0, ended: false }
    }

    /// Appends `data` to the input stream. Sent data stays in the queue across
    /// reads until a script reads it.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if !data.is_empty() {
            self.channel.push(QueuedInput { target: None, data: Some(data) });
        }
    }

    /// Enqueues an end-of-input marker for the next read that reaches it.
    /// Subsequent reads wait for new input.
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

    /// Waits until a read waits for input, also while other tasks run, and
    /// returns the request for it.
    pub async fn wait_for_request(&self) -> StdinRequest {
        let read = self.channel.wait_until(StdinChannelState::waiting_read_id).await;
        StdinRequest { channel: self.channel.clone(), read }
    }
}

impl StdinRequest {
    /// Submits `data`, a newline if `data` does not end with one, and an
    /// end-of-input marker as the response to the read. This completes any
    /// type of read operation, and the engine discards any unread bytes when
    /// the read finishes. Returns `false` if the read is closed.
    pub fn answer(&self, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.channel.answer_read(self.read, Some(data))
    }

    /// Submits an end-of-input marker to the read so that `read_line` returns
    /// `None` if the read has no data yet. Returns `false` if the read is
    /// closed.
    pub fn dismiss(&self) -> bool {
        self.channel.answer_read(self.read, None)
    }

    /// Returns `true` if the read still waits for an answer.
    pub fn is_open(&self) -> bool {
        self.channel.lock().waiting_read_id() == Some(self.read)
    }

    /// Waits until the read no longer waits for an answer: it got input, or
    /// it ended.
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

    /// The read that waits for input that is not in the queue.
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

/// The engine's link to its [`StdinFeedReader`], which tells the feed when the
/// engine has nothing to run.
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

/// Guard for an active [`StdinFeedReader`] read. Dropping the guard ends the
/// read and discards the input queued for it.
pub(crate) struct StdinReadGuard {
    channel: Arc<StdinChannel>,
}

impl Drop for StdinReadGuard {
    fn drop(&mut self) {
        self.channel.update(|state| {
            let Some(read) = state.waiting_read.take() else { return };
            state.queue.retain(|input| input.target != Some(read.id));
        });
    }
}

/// The session's reader of its [`StdinFeed`].
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

    /// Starts a read. The read gets a [`StdinReadId`] the first time it waits
    /// for input. An end of input or an answer from an earlier read does not
    /// reach it.
    pub(crate) fn begin_read(&mut self) -> StdinReadGuard {
        self.ended = false;
        if self.buf_target.is_some() {
            self.at = self.buf.len();
        }
        StdinReadGuard { channel: self.channel.clone() }
    }

    /// Returns buffered bytes available without waiting and whether the
    /// current read has reached EOF.
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        if self.at == self.buf.len() && !self.ended {
            let mut state = self.channel.lock();
            let read = state.read_id();
            let next = state.queue.iter().position(|input| input.is_for_read(read));
            if let Some(input) = next.and_then(|i| state.queue.remove(i)) {
                if let Some(read) = &mut state.waiting_read {
                    read.waiting = false;
                }
                self.buf_target = input.target;
                match input.data {
                    Some(data) => (self.buf, self.at) = (data, 0),
                    None => self.ended = true,
                }
            }
        }
        (&self.buf[self.at..], self.ended)
    }
}

impl AsyncRead for StdinFeedReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let data = std::task::ready!(self.as_mut().poll_fill_buf(cx))?;
        let n = data.len().min(out.remaining());
        out.put_slice(&data[..n]);
        self.consume(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for StdinFeedReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        loop {
            let (data, ended) = this.available();
            if !data.is_empty() || ended {
                return Poll::Ready(Ok(&this.buf[this.at..]));
            }
            let mut state = this.channel.lock();
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
                this.channel.changed.notify_waiters();
                return Poll::Pending;
            }
        }
    }

    fn consume(mut self: Pin<&mut Self>, n: usize) {
        self.at = (self.at + n).min(self.buf.len());
    }
}
