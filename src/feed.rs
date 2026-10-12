//! Standard input that the host passes to scripts ([`StdinFeed`]).

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::Notify;

/// Standard input that the host sends to scripts
/// ([`crate::Config::stdin_feed`]). A front end that reads the process's
/// standard input (such as a REPL) waits for [`StdinFeed::wanted`], reads
/// what the user types, and passes it on with [`StdinFeed::send`]. Clones
/// share the same input; data is kept until scripts read it.
#[derive(Clone, Default)]
pub struct StdinFeed {
    shared: Arc<Shared>,
}

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    /// Woken whenever [`StdinFeed::is_wanted`] may have changed.
    changed: Notify,
}

#[derive(Default)]
struct State {
    /// Data sent and not yet read; `None` marks an end of input.
    chunks: VecDeque<Option<Vec<u8>>>,
    /// Whether a read waits for data.
    waiting: bool,
    /// Whether the engine has nothing to run until some operation completes.
    blocked: bool,
    reader: Option<Waker>,
}

impl StdinFeed {
    pub fn new() -> StdinFeed {
        StdinFeed::default()
    }

    /// Passes `data` to the scripts' reads of standard input.
    pub fn send(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if !data.is_empty() {
            self.shared.push(Some(data));
        }
    }

    /// Ends the input at this point: the read that reaches it sees the end
    /// of input (`read_line` returns `None`); later reads wait for more data.
    pub fn end(&self) {
        self.shared.push(None);
    }

    /// Whether a script waits for input that has not been sent, while its
    /// engine has nothing else to run.
    pub fn is_wanted(&self) -> bool {
        self.shared.lock().wanted()
    }

    /// Waits until [`StdinFeed::is_wanted`] is true.
    pub async fn wanted(&self) {
        loop {
            let changed = self.shared.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if self.is_wanted() {
                return;
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
        self.waiting && self.blocked && self.chunks.is_empty()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, chunk: Option<Vec<u8>>) {
        let reader = {
            let mut st = self.lock();
            st.chunks.push_back(chunk);
            st.reader.take()
        };
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

/// One read of a [`FedReader`] under way; it no longer waits once dropped.
pub(crate) struct ReadOp(Arc<Shared>);

impl Drop for ReadOp {
    fn drop(&mut self) {
        self.0.update(|st| {
            st.waiting = false;
            st.reader = None;
        });
    }
}

/// The reading end of a [`StdinFeed`].
pub struct FedReader {
    shared: Arc<Shared>,
    buf: Vec<u8>,
    at: usize,
    /// Set once the current read has reached an end of input.
    ended: bool,
}

impl FedReader {
    pub(crate) fn new(feed: &StdinFeed) -> FedReader {
        FedReader { shared: feed.shared(), buf: Vec::new(), at: 0, ended: false }
    }

    /// Starts a read: an end of input reached by an earlier read is over.
    pub(crate) fn begin(&mut self) -> ReadOp {
        self.ended = false;
        ReadOp(self.shared.clone())
    }

    /// The data available without waiting, and whether the input ends
    /// after it (an end of input is consumed if nothing precedes it).
    pub(crate) fn available(&mut self) -> (&[u8], bool) {
        if self.at == self.buf.len() && !self.ended {
            match self.shared.lock().chunks.pop_front() {
                Some(Some(data)) => (self.buf, self.at) = (data, 0),
                Some(None) => self.ended = true,
                None => {}
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
