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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadId(NonZeroU64);

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    chunks: VecDeque<Chunk>,
    readers: Vec<Reader>,
    reader_count: u64,
    read_count: u64,
}

/// The state of one [`FedReader`]. Each engine run or session that reads
/// the feed has its own reader.
struct Reader {
    key: u64,
    /// The read under way.
    read: Option<ReadId>,
    /// Whether the read waits for input.
    waiting: bool,
    /// Whether the engine of this reader has nothing to run.
    blocked: bool,
    waker: Option<Waker>,
}

struct Chunk {
    /// Target read, or `None` for any read.
    target: Option<ReadId>,
    /// Input bytes, or `None` for EOF.
    data: Option<Vec<u8>>,
}

impl Chunk {
    fn is_for(&self, read: Option<ReadId>) -> bool {
        self.target.is_none() || self.target == read
    }
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

    /// Submits `data`, a newline if `data` does not end with one, and an
    /// end-of-input marker as the response to `read`. This completes any type
    /// of read operation, and the engine discards any unread bytes when `read`
    /// finishes. Returns `false` if `read` is not waiting for input.
    pub fn answer(&self, read: ReadId, data: impl Into<Vec<u8>>) -> bool {
        let mut data = data.into();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.shared.submit(read, Some(data))
    }

    /// Submits an end-of-input marker to `read` so that `read_line` returns
    /// `None`. Returns `false` if `read` is not waiting for input.
    pub fn dismiss(&self, read: ReadId) -> bool {
        self.shared.submit(read, None)
    }

    /// Returns `true` if a read is waiting for input and all other engine
    /// tasks are idle.
    pub fn is_wanted(&self) -> bool {
        self.shared.lock().wanted()
    }

    /// Waits until [`StdinFeed::is_wanted`] is `true`.
    pub async fn wanted(&self) {
        self.wait_until(|state| state.wanted().then_some(())).await;
    }

    /// Returns the [`ReadId`] of the active read if it is waiting for input,
    /// even if other tasks are running. If more than one read waits, returns
    /// the oldest.
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
}

impl State {
    fn wanted(&self) -> bool {
        self.waiting_readers().any(|r| r.blocked)
    }

    fn awaited(&self) -> Option<ReadId> {
        self.waiting_readers().filter_map(|r| r.read).min()
    }

    fn is_waiting(&self, read: ReadId) -> bool {
        self.waiting_readers().any(|r| r.read == Some(read))
    }

    /// The readers whose read waits for input that is not in the queue.
    fn waiting_readers(&self) -> impl Iterator<Item = &Reader> {
        self.readers.iter().filter(|r| r.waiting && r.read.is_some() && !self.chunks.iter().any(|c| c.is_for(r.read)))
    }

    fn reader(&mut self, key: u64) -> Option<&mut Reader> {
        self.readers.iter_mut().find(|r| r.key == key)
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
        if !state.is_waiting(read) {
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
        let wakers: Vec<Waker> = state.readers.iter_mut().filter_map(|r| r.waker.take()).collect();
        drop(state);
        wakers.into_iter().for_each(Waker::wake);
        self.changed.notify_waiters();
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_waiters();
    }

    fn update_reader(&self, key: u64, f: impl FnOnce(&mut Reader)) {
        self.update(|state| {
            if let Some(reader) = state.reader(key) {
                f(reader);
            }
        });
    }
}

/// The engine's link to its [`FedReader`], which tells the feed when the
/// engine has nothing to run.
pub(crate) struct FeedLink {
    shared: Arc<Shared>,
    key: u64,
}

impl FeedLink {
    /// Marks the engine as idle until the returned guard is dropped.
    pub(crate) fn blocked(&self) -> Blocked {
        self.shared.update_reader(self.key, |r| r.blocked = true);
        Blocked(self.shared.clone(), self.key)
    }
}

/// Guard that marks the engine as idle while held.
pub(crate) struct Blocked(Arc<Shared>, u64);

impl Drop for Blocked {
    fn drop(&mut self) {
        self.0.update_reader(self.1, |r| r.blocked = false);
    }
}

/// Guard for an active [`FedReader`] read. Dropping the guard ends the read
/// and discards any unread targeted chunks.
pub(crate) struct ReadOp(Arc<Shared>, u64, ReadId);

impl Drop for ReadOp {
    fn drop(&mut self) {
        let (key, read_id) = (self.1, self.2);
        self.0.update(|state| {
            state.chunks.retain(|chunk| chunk.target != Some(read_id));
            if let Some(reader) = state.reader(key) {
                reader.read = None;
                reader.waiting = false;
                reader.waker = None;
            }
        });
    }
}

const REGISTERED: &str = "a FedReader is registered until it is dropped";

/// Reader half of a [`StdinFeed`].
pub struct FedReader {
    shared: Arc<Shared>,
    key: u64,
    buf: Vec<u8>,
    buf_target: Option<ReadId>,
    at: usize,
    ended: bool,
}

impl FedReader {
    pub(crate) fn new(feed: &StdinFeed) -> FedReader {
        let shared = feed.shared.clone();
        let mut state = shared.lock();
        state.reader_count += 1;
        let key = state.reader_count;
        state.readers.push(Reader { key, read: None, waiting: false, blocked: false, waker: None });
        drop(state);
        FedReader { shared, key, buf: Vec::new(), buf_target: None, at: 0, ended: false }
    }

    pub(crate) fn link(&self) -> FeedLink {
        FeedLink { shared: self.shared.clone(), key: self.key }
    }

    /// Starts a read that can wait for input and assigns it the next
    /// [`ReadId`].
    pub(crate) fn begin(&mut self) -> ReadOp {
        self.reset();
        let mut state = self.shared.lock();
        state.read_count += 1;
        let read_id = ReadId(NonZeroU64::new(state.read_count).expect("read count overflow"));
        if let Some(reader) = state.reader(self.key) {
            reader.read = Some(read_id);
        }
        drop(state);
        ReadOp(self.shared.clone(), self.key, read_id)
    }

    /// Clears EOF state and discards any remaining targeted bytes from a
    /// previous read.
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
            let read = state.reader(self.key).expect(REGISTERED).read;
            let next = state.chunks.iter().position(|c| c.is_for(read));
            if let Some(chunk) = next.and_then(|i| state.chunks.remove(i)) {
                state.reader(self.key).expect(REGISTERED).waiting = false;
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

impl Drop for FedReader {
    fn drop(&mut self) {
        let key = self.key;
        self.shared.update(|state| state.readers.retain(|r| r.key != key));
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
            let read = state.reader(this.key).expect(REGISTERED).read;
            if !state.chunks.iter().any(|c| c.is_for(read)) {
                let reader = state.reader(this.key).expect(REGISTERED);
                reader.waiting = true;
                reader.waker = Some(cx.waker().clone());
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
