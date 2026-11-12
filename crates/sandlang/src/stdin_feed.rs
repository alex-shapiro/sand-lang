//! Host-supplied standard input stream.

use crate::vm::WaitKind;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// A handle that streams standard input from the host to scripts.
///
/// Create a `StdinFeed` with [`crate::engine::Config::stdin_feed`] or
/// [`crate::engine::Config::stdin_io_feed`]. Await
/// [`StdinFeed::wait_for_request`] to receive a read request. Stream bytes with
/// [`StdinFeed::send`], or answer a single request with
/// [`StdinRequest::answer`]. Clones share the same stream.
#[derive(Clone)]
pub struct StdinFeed {
    channel: Arc<StdinChannel>,
}

/// A handle to a single wait for standard input.
///
/// Respond to the request with [`StdinRequest::answer`] or
/// [`StdinRequest::dismiss`]. If a read consumes a partial line from
/// [`StdinFeed::send`] and waits again, the engine issues a new `StdinRequest`.
#[derive(Clone)]
pub struct StdinRequest {
    channel: Arc<StdinChannel>,
    wait_id: u64,
}

/// Shared standard input channel for an engine.
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
    /// Serializes concurrent standard input reads across tasks and VMs.
    read_lock: tokio::sync::Mutex<()>,
    pub(crate) wait_kind: WaitKind,
}

#[derive(Default)]
struct StdinChannelState {
    /// Buffered chunks from [`StdinFeed::send`], paired with the index of the
    /// first unread byte.
    queued_chunks: VecDeque<(Vec<u8>, usize)>,
    /// True after [`StdinFeed::end`] is called.
    stream_ended: bool,
    /// Response from [`StdinRequest::answer`] or [`StdinRequest::dismiss`] for
    /// the active wait.
    answer: Option<Vec<u8>>,
    /// Identifier of the active wait while a read awaits host input.
    open_wait_id: Option<u64>,
    waits_started: u64,
    /// True while all other tasks and I/O operations in the engine are idle.
    engine_idle: bool,
    read_waker: Option<Waker>,
}

impl StdinChannelState {
    /// Returns `true` if buffered bytes, an answer, or end-of-file is ready.
    fn has_input(&self) -> bool {
        self.answer.is_some() || !self.queued_chunks.is_empty() || self.stream_ended
    }
}

impl StdinFeed {
    /// Creates a feed and its underlying channel.
    pub(crate) fn new(wait_kind: WaitKind) -> (StdinFeed, Arc<StdinChannel>) {
        let channel = StdinChannel::new(wait_kind, StdinChannelState::default());
        (StdinFeed { channel: channel.clone() }, channel)
    }

    /// Appends `data` to the input stream. Buffered bytes remain queued across
    /// reads until a script consumes them. Calls after [`StdinFeed::end`] are
    /// ignored.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        self.channel.change(|state| {
            if !data.is_empty() && !state.stream_ended {
                state.queued_chunks.push_back((data, 0));
            }
        });
    }

    /// Closes the input stream. Subsequent reads consume any buffered bytes
    /// and then return end-of-file immediately.
    pub fn end(&self) {
        self.channel.change(|state| state.stream_ended = true);
    }

    /// Waits until a standard input read needs host input, even if other tasks
    /// are runnable, and returns the [`StdinRequest`] for that wait.
    pub async fn wait_for_request(&self) -> StdinRequest {
        let wait_id = self.channel.wait_until(|state| state.open_wait_id).await;
        StdinRequest { channel: self.channel.clone(), wait_id }
    }
}

impl StdinRequest {
    /// Supplies `data` to the waiting read, appends a newline if `data` does
    /// not end with one, and terminates the read. The engine discards any
    /// bytes of `data` that the read does not consume. Returns `false` if the
    /// request is already closed.
    pub fn answer(&self, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.respond(data)
    }

    /// Terminates the waiting read without supplying bytes. `read_line`
    /// returns `None` if it has not buffered any bytes. Returns `false` if the
    /// request is already closed.
    pub fn dismiss(&self) -> bool {
        self.respond(Vec::new())
    }

    fn respond(&self, answer: Vec<u8>) -> bool {
        let mut answered = false;
        self.channel.change(|state| {
            if state.open_wait_id == Some(self.wait_id) {
                (state.answer, answered) = (Some(answer), true);
            }
        });
        answered
    }

    /// Waits until the request is closed. A request is closed if it is
    /// answered or dismissed, if the stream supplies bytes or is closed, or if
    /// the read is cancelled.
    pub async fn wait_until_closed(&self) {
        self.channel.wait_until(|state| (state.open_wait_id != Some(self.wait_id)).then_some(())).await;
    }

    /// Waits until all other tasks in the script are idle while this request
    /// remains open. A host that shares a terminal with the script awaits this
    /// method before reading from the terminal. Returns `false` if the request
    /// is closed first.
    pub async fn wait_until_nothing_else_runs(&self) -> bool {
        self.channel
            .wait_until(|state| if state.open_wait_id == Some(self.wait_id) { state.engine_idle.then_some(true) } else { Some(false) })
            .await
    }
}

impl StdinChannel {
    fn new(wait_kind: WaitKind, state: StdinChannelState) -> Arc<StdinChannel> {
        Arc::new(StdinChannel { state: Mutex::new(state), changed: Notify::new(), read_lock: Default::default(), wait_kind })
    }

    /// Creates a closed channel preloaded with `input`.
    pub(crate) fn with_input(input: Vec<u8>) -> Arc<StdinChannel> {
        StdinChannel::new(
            WaitKind::Io,
            StdinChannelState { queued_chunks: VecDeque::from([(input, 0)]), stream_ended: true, ..Default::default() },
        )
    }

    /// Updates the channel state with `f`, clears the active wait if input is
    /// ready, and wakes waiting tasks.
    fn change(&self, f: impl FnOnce(&mut StdinChannelState)) {
        let mut state = self.state.lock();
        f(&mut state);
        if state.has_input() {
            state.open_wait_id = None;
        }
        let waker = state.read_waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
        self.changed.notify_waiters();
    }

    async fn wait_until<T>(&self, predicate: impl Fn(&StdinChannelState) -> Option<T>) -> T {
        loop {
            let changed = self.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = predicate(&self.state.lock()) {
                return value;
            }
            changed.await;
        }
    }
}

/// Marks the engine as idle on a [`StdinChannel`] until dropped.
pub(crate) struct StdinIdleGuard {
    channel: Arc<StdinChannel>,
}

impl StdinIdleGuard {
    pub(crate) fn new(channel: Arc<StdinChannel>) -> StdinIdleGuard {
        channel.change(|state| state.engine_idle = true);
        StdinIdleGuard { channel }
    }
}

impl Drop for StdinIdleGuard {
    fn drop(&mut self) {
        self.channel.change(|state| state.engine_idle = false);
    }
}

impl StdinChannel {
    /// Acquires the read lock and begins a standard input read.
    pub(crate) async fn begin_read(&self) -> ActiveStdinRead<'_> {
        ActiveStdinRead::new(self, self.read_lock.lock().await)
    }

    /// Begins a standard input read if the read lock is free.
    pub(crate) fn try_begin_read(&self) -> Option<ActiveStdinRead<'_>> {
        Some(ActiveStdinRead::new(self, self.read_lock.try_lock().ok()?))
    }
}

/// Guard for an in-progress read on a [`StdinChannel`].
///
/// Dropping this guard finishes the read, discards any unconsumed answer
/// bytes, and returns unconsumed stream bytes to the front of the queue.
pub(crate) struct ActiveStdinRead<'a> {
    channel: &'a StdinChannel,
    _turn: tokio::sync::MutexGuard<'a, ()>,
    /// Current chunk taken from the channel, with the index of its first
    /// unread byte in `at`.
    buf: Vec<u8>,
    at: usize,
    /// True if `buf` came from [`StdinRequest::answer`] or
    /// [`StdinRequest::dismiss`].
    answered: bool,
    ended: bool,
}

impl<'a> ActiveStdinRead<'a> {
    fn new(channel: &'a StdinChannel, turn: tokio::sync::MutexGuard<'a, ()>) -> ActiveStdinRead<'a> {
        ActiveStdinRead { channel, _turn: turn, buf: Vec::new(), at: 0, answered: false, ended: false }
    }

    /// Returns buffered bytes available without waiting and whether the
    /// current read has reached EOF.
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        if self.at == self.buf.len() && !self.ended {
            let mut state = self.channel.state.lock();
            if self.answered {
                self.ended = true;
            } else if let Some(answer) = state.answer.take() {
                (self.ended, self.answered) = (answer.is_empty(), true);
                (self.buf, self.at) = (answer, 0);
            } else if let Some((data, at)) = state.queued_chunks.pop_front() {
                (self.buf, self.at) = (data, at);
            } else {
                self.ended = state.stream_ended;
            }
        }
        (&self.buf[self.at..], self.ended)
    }
}

impl Drop for ActiveStdinRead<'_> {
    fn drop(&mut self) {
        let (unused, at, answered) = (std::mem::take(&mut self.buf), self.at, self.answered);
        self.channel.change(|state| {
            if !answered && at < unused.len() {
                state.queued_chunks.push_front((unused, at));
            }
            (state.answer, state.open_wait_id) = (None, None);
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
                return Poll::Ready(Ok(&this.buf[this.at..]));
            }
            let mut state = this.channel.state.lock();
            if !state.has_input() {
                if state.open_wait_id.is_none() {
                    state.waits_started += 1;
                    state.open_wait_id = Some(state.waits_started);
                }
                state.read_waker = Some(cx.waker().clone());
                drop(state);
                this.channel.changed.notify_waiters();
                return Poll::Pending;
            }
        }
    }

    fn consume(self: Pin<&mut Self>, n: usize) {
        let this = self.get_mut();
        this.at = (this.at + n).min(this.buf.len());
    }
}
