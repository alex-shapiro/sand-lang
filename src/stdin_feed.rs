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
    channel: Arc<StdinChannel>,
}

/// Unique identifier for a single standard input read operation. A read
/// that starts later has a greater identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StdinReadId(NonZeroU64);

/// The state that the host's [`StdinFeed`] handles share with the engine's
/// readers.
#[derive(Default)]
pub(crate) struct StdinChannel {
    state: Mutex<StdinChannelState>,
    changed: Notify,
}

/// The input queue of a [`StdinChannel`] and the state of each reader.
#[derive(Default)]
struct StdinChannelState {
    queue: VecDeque<QueuedInput>,
    readers: Vec<StdinReaderState>,
    readers_created: u64,
    reads_started: u64,
}

/// The state of one [`StdinFeedReader`]. Each engine run or session that reads
/// the feed has its own reader.
struct StdinReaderState {
    reader_key: u64,
    /// The read under way.
    read: Option<StdinReadId>,
    /// Whether the read waits for input.
    waiting: bool,
    /// Whether the engine of this reader has nothing to run.
    engine_idle: bool,
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
    pub fn new() -> StdinFeed {
        StdinFeed::default()
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

    /// Submits `data`, a newline if `data` does not end with one, and an
    /// end-of-input marker as the response to `read`. This completes any type
    /// of read operation, and the engine discards any unread bytes when `read`
    /// finishes. Returns `false` if `read` is not waiting for input.
    pub fn answer(&self, read: StdinReadId, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.channel.answer_read(read, Some(data))
    }

    /// Submits an end-of-input marker to `read` so that `read_line` returns
    /// `None` if `read` has no data yet. Returns `false` if `read` is not
    /// waiting for input.
    pub fn dismiss(&self, read: StdinReadId) -> bool {
        self.channel.answer_read(read, None)
    }

    /// Returns `true` if a read is waiting for input and all other engine
    /// tasks are idle.
    pub fn is_wanted(&self) -> bool {
        self.channel.lock().wanted()
    }

    /// Waits until [`StdinFeed::is_wanted`] is `true`.
    pub async fn wanted(&self) {
        self.wait_until(|state| state.wanted().then_some(())).await;
    }

    /// Returns the [`StdinReadId`] of the active read if it is waiting for input,
    /// even if other tasks are running. If more than one read waits, returns
    /// the oldest.
    pub fn awaited(&self) -> Option<StdinReadId> {
        self.channel.lock().awaited()
    }

    /// Waits until [`StdinFeed::awaited`] differs from `from` and returns the
    /// new value.
    pub async fn awaited_changed(&self, from: Option<StdinReadId>) -> Option<StdinReadId> {
        self.wait_until(|state| {
            let awaited = state.awaited();
            (awaited != from).then_some(awaited)
        })
        .await
    }

    async fn wait_until<T>(&self, predicate: impl Fn(&StdinChannelState) -> Option<T>) -> T {
        loop {
            let changed = self.channel.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = predicate(&self.channel.lock()) {
                return value;
            }
            changed.await;
        }
    }
}

impl StdinChannelState {
    fn wanted(&self) -> bool {
        self.waiting_readers().any(|r| r.engine_idle)
    }

    fn awaited(&self) -> Option<StdinReadId> {
        self.waiting_readers().filter_map(|r| r.read).min()
    }

    fn is_waiting(&self, read: StdinReadId) -> bool {
        self.waiting_readers().any(|r| r.read == Some(read))
    }

    /// The readers whose read waits for input that is not in the queue.
    fn waiting_readers(&self) -> impl Iterator<Item = &StdinReaderState> {
        self.readers.iter().filter(|r| r.waiting && r.read.is_some() && !self.queue.iter().any(|c| c.is_for_read(r.read)))
    }

    fn reader_state(&mut self, reader_key: u64) -> Option<&mut StdinReaderState> {
        self.readers.iter_mut().find(|r| r.reader_key == reader_key)
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
        if !state.is_waiting(read) {
            return false;
        }
        if let Some(data) = data {
            state.queue.push_back(QueuedInput { target: Some(read), data: Some(data) });
        }
        state.queue.push_back(QueuedInput { target: Some(read), data: None });
        self.notify(state);
        true
    }

    fn notify(&self, mut state: MutexGuard<'_, StdinChannelState>) {
        let wakers: Vec<Waker> = state.readers.iter_mut().filter_map(|r| r.waker.take()).collect();
        drop(state);
        wakers.into_iter().for_each(Waker::wake);
        self.changed.notify_waiters();
    }

    fn update(&self, f: impl FnOnce(&mut StdinChannelState)) {
        f(&mut self.lock());
        self.changed.notify_waiters();
    }

    fn update_reader_state(&self, reader_key: u64, f: impl FnOnce(&mut StdinReaderState)) {
        self.update(|state| {
            if let Some(reader) = state.reader_state(reader_key) {
                f(reader);
            }
        });
    }
}

/// The engine's link to its [`StdinFeedReader`], which tells the feed when the
/// engine has nothing to run.
pub(crate) struct StdinIdleSignal {
    channel: Arc<StdinChannel>,
    reader_key: u64,
}

impl StdinIdleSignal {
    /// Marks the engine as idle until the returned guard is dropped.
    pub(crate) fn idle(&self) -> StdinIdleGuard {
        self.channel.update_reader_state(self.reader_key, |r| r.engine_idle = true);
        StdinIdleGuard { channel: self.channel.clone(), reader_key: self.reader_key }
    }
}

/// Guard that marks the engine as idle while held.
pub(crate) struct StdinIdleGuard {
    channel: Arc<StdinChannel>,
    reader_key: u64,
}

impl Drop for StdinIdleGuard {
    fn drop(&mut self) {
        self.channel.update_reader_state(self.reader_key, |r| r.engine_idle = false);
    }
}

/// Guard for an active [`StdinFeedReader`] read. Dropping the guard ends the
/// read and discards the input queued for it.
pub(crate) struct StdinReadGuard {
    channel: Arc<StdinChannel>,
    reader_key: u64,
    read: StdinReadId,
}

impl Drop for StdinReadGuard {
    fn drop(&mut self) {
        let (reader_key, read) = (self.reader_key, self.read);
        self.channel.update(|state| {
            state.queue.retain(|input| input.target != Some(read));
            if let Some(reader) = state.reader_state(reader_key) {
                reader.read = None;
                reader.waiting = false;
                reader.waker = None;
            }
        });
    }
}

const READER_REGISTERED: &str = "a StdinFeedReader is registered until it is dropped";

/// The engine's reader of a [`StdinFeed`].
pub struct StdinFeedReader {
    channel: Arc<StdinChannel>,
    reader_key: u64,
    buf: Vec<u8>,
    buf_target: Option<StdinReadId>,
    at: usize,
    ended: bool,
}

impl StdinFeedReader {
    pub(crate) fn new(feed: &StdinFeed) -> StdinFeedReader {
        let channel = feed.channel.clone();
        let mut state = channel.lock();
        state.readers_created += 1;
        let reader_key = state.readers_created;
        state.readers.push(StdinReaderState { reader_key, read: None, waiting: false, engine_idle: false, waker: None });
        drop(state);
        StdinFeedReader { channel, reader_key, buf: Vec::new(), buf_target: None, at: 0, ended: false }
    }

    pub(crate) fn idle_signal(&self) -> StdinIdleSignal {
        StdinIdleSignal { channel: self.channel.clone(), reader_key: self.reader_key }
    }

    /// Starts a read that can wait for input and assigns it the next
    /// [`StdinReadId`].
    pub(crate) fn begin_read(&mut self) -> StdinReadGuard {
        self.reset_for_read();
        let mut state = self.channel.lock();
        state.reads_started += 1;
        let read_id = StdinReadId(NonZeroU64::new(state.reads_started).expect("read count overflow"));
        if let Some(reader) = state.reader_state(self.reader_key) {
            reader.read = Some(read_id);
        }
        drop(state);
        StdinReadGuard { channel: self.channel.clone(), reader_key: self.reader_key, read: read_id }
    }

    /// Clears EOF state and discards any remaining targeted bytes from a
    /// previous read.
    pub(crate) fn reset_for_read(&mut self) {
        self.ended = false;
        if self.buf_target.is_some() {
            self.at = self.buf.len();
        }
    }

    /// Returns buffered bytes available without waiting and whether the
    /// current read has reached EOF.
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        if self.at == self.buf.len() && !self.ended {
            let mut state = self.channel.lock();
            let read = state.reader_state(self.reader_key).expect(READER_REGISTERED).read;
            let next = state.queue.iter().position(|c| c.is_for_read(read));
            if let Some(input) = next.and_then(|i| state.queue.remove(i)) {
                state.reader_state(self.reader_key).expect(READER_REGISTERED).waiting = false;
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

impl Drop for StdinFeedReader {
    fn drop(&mut self) {
        let reader_key = self.reader_key;
        self.channel.update(|state| state.readers.retain(|r| r.reader_key != reader_key));
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
            let read = state.reader_state(this.reader_key).expect(READER_REGISTERED).read;
            if !state.queue.iter().any(|c| c.is_for_read(read)) {
                let reader = state.reader_state(this.reader_key).expect(READER_REGISTERED);
                reader.waiting = true;
                reader.waker = Some(cx.waker().clone());
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
