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

/// A wait of a standard input read for host input. Respond to the wait with
/// [`StdinRequest::answer`] or [`StdinRequest::dismiss`]. A request that
/// closes stays closed: if the read takes part of a line and waits again, the
/// new wait is a new request.
#[derive(Clone)]
pub struct StdinRequest {
    channel: Arc<StdinChannel>,
    wait: u64,
}

/// State shared between the host's [`StdinFeed`] handles and its reader.
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
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
    /// Creates a feed and its only reader.
    pub(crate) fn new(wait_kind: WaitKind) -> (StdinFeed, StdinFeedReader) {
        let channel = Arc::new(StdinChannel { state: Mutex::default(), changed: Notify::new(), wait_kind });
        let reader = StdinFeedReader { channel: channel.clone(), buf: Vec::new(), at: 0, answered: false, ended: false };
        (StdinFeed { channel }, reader)
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

/// The only reader of a [`StdinFeed`]. Call [`StdinFeedReader::begin_read`]
/// to start a read operation.
pub(crate) struct StdinFeedReader {
    pub(crate) channel: Arc<StdinChannel>,
    /// Input that the current read took from the feed, and the offset of its
    /// first unused byte. It is empty between reads.
    buf: Vec<u8>,
    at: usize,
    /// True if `buf` holds an answer, which ends the read when it is used up.
    answered: bool,
    ended: bool,
}

impl StdinFeedReader {
    /// Starts a read operation.
    pub(crate) fn begin_read(&mut self) -> ActiveStdinRead<'_> {
        (self.answered, self.ended) = (false, false);
        ActiveStdinRead { reader: self }
    }
}

/// An active read operation on a [`StdinFeedReader`]. Dropping this guard ends
/// the read, discards an unused answer, and puts other unused input back at
/// the front of the feed.
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
            if r.answered {
                r.ended = true;
            } else if let Some(answer) = state.answer.take() {
                (r.ended, r.answered) = (answer.is_empty(), true);
                (r.buf, r.at) = (answer, 0);
            } else if let Some((data, at)) = state.sent.pop_front() {
                (r.buf, r.at) = (data, at);
            } else {
                r.ended = state.stream_ended;
            }
        }
        (&r.buf[r.at..], r.ended)
    }
}

impl Drop for ActiveStdinRead<'_> {
    fn drop(&mut self) {
        let r = &mut *self.reader;
        let (unused, at) = (std::mem::take(&mut r.buf), std::mem::take(&mut r.at));
        let answered = r.answered;
        r.channel.change(|state| {
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
                let r = &*this.reader;
                return Poll::Ready(Ok(&r.buf[r.at..]));
            }
            let mut state = this.reader.channel.lock();
            if state.answer.is_none() && state.sent.is_empty() && !state.stream_ended {
                if state.open_wait.is_none() {
                    state.waits_started += 1;
                    state.open_wait = Some(state.waits_started);
                }
                state.read_waker = Some(cx.waker().clone());
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
