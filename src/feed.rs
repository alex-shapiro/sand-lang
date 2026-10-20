//! Standard input that the host passes to scripts ([`StdinFeed`]).

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// Standard input that the host sends to scripts
/// ([`crate::Config::stdin_feed`]). A front end that reads the process's
/// standard input (such as a REPL) waits for [`StdinFeed::wanted`], reads
/// what the user types, and passes it on with [`StdinFeed::send`]. A front
/// end that shows its own input field watches [`StdinFeed::awaited_changed`]
/// and answers each read with [`StdinFeed::answer`]. Clones share the same
/// input; data sent with `send` is kept until scripts read it.
#[derive(Clone, Default)]
pub struct StdinFeed {
    shared: Arc<Shared>,
}

/// One read of standard input by a script. Each read has a new identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadId(NonZeroU64);

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    /// Woken whenever [`State`] may have changed.
    changed: Notify,
}

#[derive(Default)]
struct State {
    /// Data sent and not yet read.
    chunks: VecDeque<Chunk>,
    /// The read under way, if any.
    read: Option<ReadId>,
    /// The number of reads started.
    reads: u64,
    /// Whether the read under way waits for data.
    waiting: bool,
    /// Whether the engine has nothing to run until some operation completes.
    blocked: bool,
    reader: Option<Waker>,
}

/// Data sent to standard input.
struct Chunk {
    /// The read that the data answers, or `None` for any read.
    to: Option<ReadId>,
    /// The data, or `None` for an end of input.
    data: Option<Vec<u8>>,
}

impl ReadId {
    /// The number of the read, counted from 1 for each feed.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl StdinFeed {
    pub fn new() -> StdinFeed {
        StdinFeed::default()
    }

    /// Passes `data` to the scripts' reads of standard input.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if !data.is_empty() {
            self.shared.push(Chunk { to: None, data: Some(data) });
        }
    }

    /// Ends the input at this point: the read that reaches it sees the end
    /// of input (`read_line` returns `None`); later reads wait for more data.
    pub fn end(&self) {
        self.shared.push(Chunk { to: None, data: None });
    }

    /// Passes `data` to the read `read` only. Returns false, and passes
    /// nothing, if that read is over. Data that the read leaves unread is
    /// discarded when the read ends.
    pub fn answer(&self, read: ReadId, data: impl Into<Vec<u8>>) -> bool {
        let data = data.into();
        let mut st = self.shared.lock();
        if st.read != Some(read) {
            return false;
        }
        if !data.is_empty() {
            st.chunks.push_back(Chunk { to: Some(read), data: Some(data) });
            self.shared.notify(st);
        }
        true
    }

    /// Whether a script waits for input that has not been sent, while its
    /// engine has nothing else to run.
    pub fn is_wanted(&self) -> bool {
        self.shared.lock().wanted()
    }

    /// Waits until [`StdinFeed::is_wanted`] is true.
    pub async fn wanted(&self) {
        self.until(|st| st.wanted().then_some(())).await;
    }

    /// The read that waits for input that has not been sent, whether or not
    /// its engine has other tasks to run.
    pub fn awaited(&self) -> Option<ReadId> {
        self.shared.lock().awaited()
    }

    /// Waits until [`StdinFeed::awaited`] is not `from`, and returns it. A
    /// read that starts and ends between two calls is not seen, but each
    /// read that waits when a call returns has a new identifier.
    pub async fn awaited_changed(&self, from: Option<ReadId>) -> Option<ReadId> {
        self.until(|st| {
            let awaited = st.awaited();
            (awaited != from).then_some(awaited)
        })
        .await
    }

    async fn until<T>(&self, done: impl Fn(&State) -> Option<T>) -> T {
        loop {
            let changed = self.shared.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = done(&self.shared.lock()) {
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
        self.read.filter(|_| self.waiting && self.chunks.is_empty())
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, chunk: Chunk) {
        let mut st = self.lock();
        st.chunks.push_back(chunk);
        self.notify(st);
    }

    /// Wakes the reader and the watchers after a change to `st`.
    fn notify(&self, mut st: MutexGuard<'_, State>) {
        let reader = st.reader.take();
        drop(st);
        if let Some(w) = reader {
            w.wake();
        }
        self.changed.notify_waiters();
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_waiters();
    }

    /// Marks the engine as having nothing to run until the returned guard is
    /// dropped.
    pub(crate) fn blocked(self: &Arc<Self>) -> Blocked {
        self.update(|st| st.blocked = true);
        Blocked(self.clone())
    }
}

/// See [`Shared::blocked`].
pub(crate) struct Blocked(Arc<Shared>);

impl Drop for Blocked {
    fn drop(&mut self) {
        self.0.update(|st| st.blocked = false);
    }
}

/// One read of a [`FedReader`] under way. When it is dropped, the read is
/// over and the data sent to it alone is discarded.
pub(crate) struct ReadOp(Arc<Shared>, ReadId);

impl Drop for ReadOp {
    fn drop(&mut self) {
        let read = self.1;
        self.0.update(|st| {
            st.chunks.retain(|c| c.to != Some(read));
            st.read = None;
            st.waiting = false;
            st.reader = None;
        });
    }
}

/// The reading end of a [`StdinFeed`].
pub struct FedReader {
    shared: Arc<Shared>,
    buf: Vec<u8>,
    /// The read that `buf` answers, or `None` for any read.
    buf_to: Option<ReadId>,
    at: usize,
    /// Set once the current read has reached an end of input.
    ended: bool,
}

impl FedReader {
    pub(crate) fn new(feed: &StdinFeed) -> FedReader {
        FedReader { shared: feed.shared(), buf: Vec::new(), buf_to: None, at: 0, ended: false }
    }

    /// Starts a read: an end of input reached by an earlier read is over.
    pub(crate) fn begin(&mut self) -> ReadOp {
        self.ended = false;
        let mut st = self.shared.lock();
        st.reads += 1;
        let read = ReadId(NonZeroU64::new(st.reads).expect("read count overflow"));
        st.read = Some(read);
        drop(st);
        if self.buf_to.is_some() {
            self.at = self.buf.len();
        }
        ReadOp(self.shared.clone(), read)
    }

    /// The data available without waiting, and whether the input ends
    /// after it (an end of input is consumed if nothing precedes it).
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        if self.at == self.buf.len() && !self.ended {
            let mut st = self.shared.lock();
            if let Some(chunk) = st.chunks.pop_front() {
                // The read has data now; it waits again only if the data is not enough.
                st.waiting = false;
                self.buf_to = chunk.to;
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
            let mut st = this.shared.lock();
            // Data may have arrived since `available` looked.
            if st.chunks.is_empty() {
                st.waiting = true;
                st.reader = Some(cx.waker().clone());
                drop(st);
                this.shared.changed.notify_waiters();
                return Poll::Pending;
            }
        }
    }

    fn consume(mut self: Pin<&mut Self>, n: usize) {
        self.at = (self.at + n).min(self.buf.len());
    }
}
