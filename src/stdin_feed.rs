//! Host-supplied standard input stream ([`StdinFeed`]).

use crate::vm::WaitKind;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// A channel that supplies standard input from the host to scripts
/// ([`crate::Config::stdin_feed`]). Hosts wait for
/// [`StdinFeed::wait_for_request`], and stream input with [`StdinFeed::send`]
/// or answer one read with [`StdinRequest::answer`]. Clones share the same
/// underlying stream.
#[derive(Clone)]
pub struct StdinFeed {
    channel: Arc<StdinChannel>,
}

/// A standard input read that waits for host input. Respond to it with
/// [`StdinRequest::answer`] or [`StdinRequest::dismiss`]. Each wait gets its
/// own request: if the read takes part of a line and waits again, the engine
/// makes a new request.
#[derive(Clone)]
pub struct StdinRequest {
    channel: Arc<StdinChannel>,
    wait: u64,
}

/// The standard input of an engine: a stream that the host sends through a
/// [`StdinFeed`], or fixed input.
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
    /// Held by the read in progress, so that reads of the engine's VMs take
    /// turns.
    read_lock: tokio::sync::Mutex<()>,
    pub(crate) wait_kind: WaitKind,
}

#[derive(Default)]
struct StdinChannelState {
    /// Chunks from [`StdinFeed::send`] that no read has used, each with the
    /// offset of its first unused byte.
    sent: VecDeque<(Vec<u8>, usize)>,
    /// True after [`StdinFeed::end`].
    stream_ended: bool,
    /// The answer to the open wait, until the read takes it.
    answer: Option<Vec<u8>>,
    /// The number of the wait that is open, if a read waits for input.
    open_wait: Option<u64>,
    waits_started: u64,
    /// True if the engine has no runnable tasks or ready I/O.
    engine_idle: bool,
    read_waker: Option<Waker>,
}

impl StdinFeed {
    /// Creates a feed and the channel that the engine reads it through.
    pub(crate) fn new(wait_kind: WaitKind) -> (StdinFeed, Arc<StdinChannel>) {
        let channel = StdinChannel::new(wait_kind, StdinChannelState::default());
        (StdinFeed { channel: channel.clone() }, channel)
    }

    /// Appends `data` to the input stream. Queued data persists across reads
    /// until a script consumes it. Has no effect after the stream ends.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        self.channel.change(|state| {
            if !data.is_empty() && !state.stream_ended {
                state.sent.push_back((data, 0));
                state.open_wait = None;
            }
        });
    }

    /// Ends the input stream. Reads take the input that is already queued,
    /// and then every read gets an end of input without a wait.
    pub fn end(&self) {
        self.channel.change(|state| {
            state.stream_ended = true;
            state.open_wait = None;
        });
    }

    /// Waits until a read needs input, even if other tasks are running, and
    /// returns its [`StdinRequest`].
    pub async fn wait_for_request(&self) -> StdinRequest {
        let wait = self.channel.wait_until(|state| state.open_wait).await;
        StdinRequest { channel: self.channel.clone(), wait }
    }
}

impl StdinRequest {
    /// Gives the read `data`, a trailing newline if `data` lacks one, and an
    /// end of input, so that a read of any kind completes. When the read
    /// finishes, the engine discards the part of `data` that it did not use.
    /// Returns `false` if the request is already closed.
    pub fn answer(&self, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.respond(data)
    }

    /// Gives the read only an end of input, so that `read_line` returns `None`
    /// if the read has no data yet. Returns `false` if the request is already
    /// closed.
    pub fn dismiss(&self) -> bool {
        self.respond(Vec::new())
    }

    fn respond(&self, answer: Vec<u8>) -> bool {
        let mut answered = false;
        self.channel.change(|state| {
            if state.open_wait == Some(self.wait) {
                (state.answer, state.open_wait, answered) = (Some(answer), None, true);
            }
        });
        answered
    }

    /// Waits until the read receives input or ends.
    pub async fn wait_until_closed(&self) {
        self.channel.wait_until(|state| (state.open_wait != Some(self.wait)).then_some(())).await;
    }

    /// Waits until no other task of the script can run while the read waits.
    /// A host that shares a terminal with the script waits for this before it
    /// reads the terminal. Returns `false` if the request closes first.
    pub async fn wait_until_nothing_else_runs(&self) -> bool {
        self.channel
            .wait_until(|state| if state.open_wait == Some(self.wait) { state.engine_idle.then_some(true) } else { Some(false) })
            .await
    }
}

impl StdinChannel {
    fn new(wait_kind: WaitKind, state: StdinChannelState) -> Arc<StdinChannel> {
        Arc::new(StdinChannel { state: Mutex::new(state), changed: Notify::new(), read_lock: Default::default(), wait_kind })
    }

    /// Creates a channel whose stream holds `input` and has ended.
    pub(crate) fn with_input(input: Vec<u8>) -> Arc<StdinChannel> {
        StdinChannel::new(WaitKind::Io, StdinChannelState { sent: VecDeque::from([(input, 0)]), stream_ended: true, ..Default::default() })
    }

    fn lock(&self) -> MutexGuard<'_, StdinChannelState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Applies `f` to the state and wakes the read and every host waiter.
    fn change(&self, f: impl FnOnce(&mut StdinChannelState)) {
        let mut state = self.lock();
        f(&mut state);
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
            if let Some(value) = predicate(&self.lock()) {
                return value;
            }
            changed.await;
        }
    }
}

/// Marks the engine as idle for a [`StdinChannel`] until it is dropped.
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
    /// Starts a read operation when the read in progress ends.
    pub(crate) async fn begin_read(&self) -> ActiveStdinRead<'_> {
        ActiveStdinRead::new(self, self.read_lock.lock().await)
    }

    /// Starts a read operation, or returns `None` if another is in progress.
    pub(crate) fn try_begin_read(&self) -> Option<ActiveStdinRead<'_>> {
        Some(ActiveStdinRead::new(self, self.read_lock.try_lock().ok()?))
    }
}

/// An active read operation on a [`StdinChannel`]. Dropping this guard ends
/// the read, discards an unused answer, and puts other unused input back at
/// the front of the feed.
pub(crate) struct ActiveStdinRead<'a> {
    channel: &'a StdinChannel,
    _turn: tokio::sync::MutexGuard<'a, ()>,
    /// Input that the read took from the feed, and the offset of its first
    /// unused byte.
    buf: Vec<u8>,
    at: usize,
    /// True if `buf` holds an answer, which ends the read when it is used up.
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
            let mut state = self.channel.lock();
            if self.answered {
                self.ended = true;
            } else if let Some(answer) = state.answer.take() {
                (self.ended, self.answered) = (answer.is_empty(), true);
                (self.buf, self.at) = (answer, 0);
            } else if let Some((data, at)) = state.sent.pop_front() {
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
                state.sent.push_front((unused, at));
            }
            (state.answer, state.open_wait) = (None, None);
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
            let mut state = this.channel.lock();
            if state.answer.is_none() && state.sent.is_empty() && !state.stream_ended {
                if state.open_wait.is_none() {
                    state.waits_started += 1;
                    state.open_wait = Some(state.waits_started);
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
