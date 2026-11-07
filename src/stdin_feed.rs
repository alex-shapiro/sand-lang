//! Host-supplied standard input stream ([`StdinFeed`]).

use crate::vm::WaitKind;
use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU64;
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
/// [`StdinRequest::answer`] or [`StdinRequest::dismiss`]. Two requests are
/// equal if they refer to the same wait. A request that closes stays closed:
/// if the read takes part of a line and waits again, the new wait is a new
/// request.
#[derive(Clone)]
pub struct StdinRequest {
    channel: Arc<StdinChannel>,
    wait: StdinWaitId,
}

/// Identifier of one wait of a standard input read for input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StdinWaitId(NonZeroU64);

/// State shared between the host's [`StdinFeed`] handles and the session's
/// reader.
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
    wait_kind: WaitKind,
}

/// Input queue and reader state for a [`StdinChannel`].
#[derive(Default)]
struct StdinChannelState {
    queue: VecDeque<QueuedInput>,
    waits_started: u64,
    /// True if the engine has no runnable tasks or ready I/O.
    engine_idle: bool,
    /// The active read operation, once it has waited for input.
    current_read: Option<CurrentRead>,
}

/// A read operation that has waited for input.
struct CurrentRead {
    /// The read's latest wait.
    wait: StdinWaitId,
    /// True while the latest wait is open. Set to `false` when the read
    /// consumes queued input.
    needs_input: bool,
    waker: Option<Waker>,
}

struct QueuedInput {
    /// Target wait, or `None` for any read.
    target: Option<StdinWaitId>,
    /// Input bytes, or `None` for EOF.
    data: Option<Vec<u8>>,
    /// The offset in `data` of the first byte that no read has used.
    start: usize,
}

impl QueuedInput {
    fn is_for_wait(&self, wait: Option<StdinWaitId>) -> bool {
        self.target.is_none() || self.target == wait
    }
}

impl StdinFeed {
    pub(crate) fn new(wait_kind: WaitKind) -> StdinFeed {
        StdinFeed { channel: Arc::new(StdinChannel { state: Mutex::default(), changed: Notify::new(), wait_kind }) }
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
            self.channel.push(QueuedInput { target: None, data: Some(data), start: 0 });
        }
    }

    /// Enqueues an end-of-input marker for the next read that reaches it.
    /// Later reads wait for new input.
    pub fn end(&self) {
        self.channel.push(QueuedInput { target: None, data: None, start: 0 });
    }

    /// Waits until a read needs input, even if other tasks are running, and
    /// returns its [`StdinRequest`].
    pub async fn wait_for_request(&self) -> StdinRequest {
        let wait = self.channel.wait_until(StdinChannelState::open_wait).await;
        StdinRequest { channel: self.channel.clone(), wait }
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
        self.channel.answer_wait(self.wait, Some(data))
    }

    /// Submits an end-of-input marker to the read so that `read_line` returns
    /// `None` if the read has no buffered data. Returns `false` if the read is
    /// already closed.
    pub fn dismiss(&self) -> bool {
        self.channel.answer_wait(self.wait, None)
    }

    /// Returns `true` if the read is still waiting for a response.
    pub fn is_open(&self) -> bool {
        self.channel.lock().open_wait() == Some(self.wait)
    }

    /// Waits until the read receives input or ends.
    pub async fn wait_until_closed(&self) {
        let wait = self.wait;
        self.channel.wait_until(|state| (state.open_wait() != Some(wait)).then_some(())).await;
    }

    /// Returns `true` if the read is still waiting and no other task of the
    /// script can run.
    pub fn nothing_else_runs(&self) -> bool {
        let state = self.channel.lock();
        state.engine_idle && state.open_wait() == Some(self.wait)
    }

    /// Waits until no other task of the script can run while the read waits,
    /// as a host needs that shares a terminal with the script. Returns `false`
    /// if the read closes first.
    pub async fn wait_until_nothing_else_runs(&self) -> bool {
        let wait = self.wait;
        self.channel
            .wait_until(|state| match state.open_wait() {
                Some(open) if open == wait => state.engine_idle.then_some(true),
                _ => Some(false),
            })
            .await
    }
}

impl PartialEq for StdinRequest {
    fn eq(&self, other: &StdinRequest) -> bool {
        Arc::ptr_eq(&self.channel, &other.channel) && self.wait == other.wait
    }
}

impl Eq for StdinRequest {}

impl std::fmt::Debug for StdinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StdinRequest").field(&self.wait.0).finish()
    }
}

impl StdinChannelState {
    /// Returns the open wait that no queued input can end, if any.
    fn open_wait(&self) -> Option<StdinWaitId> {
        let read = self.current_read.as_ref().filter(|r| r.needs_input)?;
        (!self.queue.iter().any(|input| input.is_for_wait(Some(read.wait)))).then_some(read.wait)
    }

    fn current_wait(&self) -> Option<StdinWaitId> {
        self.current_read.as_ref().map(|r| r.wait)
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

    fn answer_wait(&self, wait: StdinWaitId, data: Option<Vec<u8>>) -> bool {
        let mut state = self.lock();
        if state.open_wait() != Some(wait) {
            return false;
        }
        if let Some(data) = data {
            state.queue.push_back(QueuedInput { target: Some(wait), data: Some(data), start: 0 });
        }
        state.queue.push_back(QueuedInput { target: Some(wait), data: None, start: 0 });
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
        let waker = state.current_read.as_mut().and_then(|r| r.waker.take());
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
    /// Input taken from the feed during a read. It is empty between reads.
    buf: Vec<u8>,
    buf_target: Option<StdinWaitId>,
    at: usize,
    ended: bool,
}

impl StdinFeedReader {
    pub(crate) fn idle_signal(&self) -> StdinIdleSignal {
        StdinIdleSignal { channel: self.channel.clone() }
    }

    pub(crate) fn wait_kind(&self) -> WaitKind {
        self.channel.wait_kind
    }

    /// Starts a read operation. Each time the read waits for input, the wait
    /// receives a new [`StdinWaitId`]. The read ignores any EOF or targeted
    /// response from an earlier read.
    pub(crate) fn begin_read(&mut self) -> ActiveStdinRead<'_> {
        self.ended = false;
        ActiveStdinRead { reader: self }
    }
}

/// An active read operation on a [`StdinFeedReader`]. Dropping this guard ends
/// the read, discards any unread input targeted at it, and puts other unread
/// input back at the front of the feed.
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
            let wait = state.current_wait();
            let next = state.queue.iter().position(|input| input.is_for_wait(wait));
            if let Some(input) = next.and_then(|i| state.queue.remove(i)) {
                if let Some(read) = &mut state.current_read {
                    read.needs_input = false;
                }
                r.buf_target = input.target;
                match input.data {
                    Some(data) => (r.buf, r.at) = (data, input.start),
                    None => r.ended = true,
                }
            }
        }
        (&r.buf[r.at..], r.ended)
    }
}

impl Drop for ActiveStdinRead<'_> {
    fn drop(&mut self) {
        let r = &mut *self.reader;
        let (unread, start, target) = (std::mem::take(&mut r.buf), std::mem::take(&mut r.at), r.buf_target.take());
        r.channel.update(|state| {
            if target.is_none() && start < unread.len() {
                state.queue.push_front(QueuedInput { target: None, data: Some(unread), start });
            }
            let Some(read) = state.current_read.take() else { return };
            state.queue.retain(|input| input.target != Some(read.wait));
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
            let wait = state.current_wait();
            if !state.queue.iter().any(|input| input.is_for_wait(wait)) {
                let waker = Some(cx.waker().clone());
                match &mut state.current_read {
                    Some(read) if read.needs_input => read.waker = waker,
                    _ => {
                        state.waits_started += 1;
                        let wait = StdinWaitId(NonZeroU64::new(state.waits_started).expect("wait count overflow"));
                        state.current_read = Some(CurrentRead { wait, needs_input: true, waker });
                    }
                }
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
