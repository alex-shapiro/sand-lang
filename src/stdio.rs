//! Writes the process's standard output and error for engines. One thread
//! writes both streams, in order, so that a write to a full pipe or a
//! suspended terminal does not prevent an engine from enforcing its
//! limits. Engines wait for the writer through
//! [`OutputSink::wait_backlog`], which lets them stop waiting. On the web,
//! which has no threads, writes happen on the calling thread.

use crate::vm::OutputSink;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// The bytes a sink buffers before it hands them to the writer.
const BUFFER: usize = 8 << 10;

#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Out,
    Err,
}

#[derive(Default)]
struct Writer {
    state: Mutex<Queue>,
    changed: Condvar,
    /// The bytes queued but not yet written.
    backlog: AtomicUsize,
}

#[derive(Default)]
struct Queue {
    chunks: VecDeque<(Stream, Vec<u8>)>,
    /// The number of chunks queued, and written, since the process started.
    queued: u64,
    written: u64,
    /// For each stream, the first error since a flush last reported one.
    errors: [Option<io::Error>; 2],
    running: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer() -> &'static Writer {
    static WRITER: OnceLock<Writer> = OnceLock::new();
    WRITER.get_or_init(Writer::default)
}

fn write_out(stream: Stream, data: &[u8]) -> io::Result<()> {
    match stream {
        Stream::Out => {
            let mut o = io::stdout().lock();
            o.write_all(data).and_then(|()| o.flush())
        }
        Stream::Err => io::stderr().lock().write_all(data),
    }
}

impl Writer {
    /// Queues `data` and returns its sequence number.
    fn push(&'static self, stream: Stream, data: Vec<u8>) -> u64 {
        let mut q = lock(&self.state);
        if !q.running {
            #[cfg(not(web))]
            let started = std::thread::Builder::new().name("sand-stdio".into()).spawn(move || self.run()).is_ok();
            #[cfg(web)]
            let started = false;
            if !started {
                // No writer thread could be started, so write here.
                if let Err(e) = write_out(stream, &data) {
                    q.errors[stream as usize].get_or_insert(e);
                }
                q.queued += 1;
                q.written += 1;
                return q.queued;
            }
            q.running = true;
        }
        self.backlog.fetch_add(data.len(), Ordering::SeqCst);
        q.chunks.push_back((stream, data));
        q.queued += 1;
        self.changed.notify_all();
        q.queued
    }

    #[cfg(not(web))]
    fn run(&self) {
        let mut q = lock(&self.state);
        loop {
            let Some((stream, data)) = q.chunks.pop_front() else {
                q = self.changed.wait(q).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            drop(q);
            let r = write_out(stream, &data);
            q = lock(&self.state);
            if let Err(e) = r {
                q.errors[stream as usize].get_or_insert(e);
            }
            q.written += 1;
            self.backlog.fetch_sub(data.len(), Ordering::SeqCst);
            self.changed.notify_all();
        }
    }

    /// Waits at most `timeout` for `done` to hold, and returns whether it does.
    fn wait(&self, timeout: Duration, done: impl Fn(&Queue) -> bool) -> bool {
        let q = lock(&self.state);
        let (q, _) = self.changed.wait_timeout_while(q, timeout, |q| !done(q)).unwrap_or_else(|e| e.into_inner());
        done(&q)
    }

    /// The first error writing `stream` since the last call, if any.
    fn take_error(&self, stream: Stream) -> io::Result<()> {
        lock(&self.state).errors[stream as usize].take().map_or(Ok(()), Err)
    }

    fn wait_backlog(&self, last: u64, max: usize, timeout: Duration) -> bool {
        if max == 0 {
            self.wait(timeout, |q| q.written >= last)
        } else {
            self.wait(timeout, |_| self.backlog.load(Ordering::SeqCst) <= max)
        }
    }
}

/// Waits until all output that engines wrote to the process's standard
/// output and error has been written. Call it before the process exits.
pub fn flush_stdio() {
    let w = writer();
    let last = lock(&w.state).queued;
    while !w.wait(Duration::from_secs(1), |q| q.written >= last) {}
}

/// An engine's standard output, buffered. Each sink (each configuration)
/// has its own buffer, so engines never flush each other's output. Output
/// to a terminal is line-buffered.
#[derive(Default)]
pub(crate) struct StdoutSink(Mutex<Buffer>);

#[derive(Default)]
struct Buffer {
    data: Vec<u8>,
    tty: Option<bool>,
    /// The sequence number of the last chunk handed to the writer.
    last: u64,
}

impl Buffer {
    fn hand_off(&mut self) {
        if !self.data.is_empty() {
            self.last = writer().push(Stream::Out, std::mem::take(&mut self.data));
        }
    }
}

impl StdoutSink {
    fn buffer(&self) -> MutexGuard<'_, Buffer> {
        lock(&self.0)
    }
}

impl OutputSink for StdoutSink {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        use std::io::IsTerminal;
        let mut b = self.buffer();
        let tty = *b.tty.get_or_insert_with(|| io::stdout().is_terminal());
        b.data.extend_from_slice(data);
        if b.data.len() >= BUFFER || (tty && data.contains(&b'\n')) {
            b.hand_off();
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.buffer().hand_off();
        writer().take_error(Stream::Out)
    }

    fn flush_before_input(&self) -> io::Result<()> {
        let mut b = self.buffer();
        // Like C's stdio, flush only a line-buffered (terminal) stream.
        if b.tty == Some(true) {
            b.hand_off();
        }
        Ok(())
    }

    fn backlog(&self) -> usize {
        writer().backlog.load(Ordering::SeqCst)
    }

    fn wait_backlog(&self, max: usize, timeout: Duration) -> bool {
        let last = self.buffer().last;
        writer().wait_backlog(last, max, timeout)
    }
}

impl Drop for StdoutSink {
    fn drop(&mut self) {
        self.buffer().hand_off();
    }
}

/// An engine's standard error. It is not buffered.
#[derive(Default)]
pub(crate) struct StderrSink {
    last: Mutex<u64>,
}

impl OutputSink for StderrSink {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut last = lock(&self.last);
        *last = writer().push(Stream::Err, data.to_vec());
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        writer().take_error(Stream::Err)
    }

    fn backlog(&self) -> usize {
        writer().backlog.load(Ordering::SeqCst)
    }

    fn wait_backlog(&self, max: usize, timeout: Duration) -> bool {
        let last = *lock(&self.last);
        writer().wait_backlog(last, max, timeout)
    }
}
