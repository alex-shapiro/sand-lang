//! Shared I/O errors, async results, and buffered readers for the `fs`, `io`,
//! `net`, and `http` modules.

use super::io_kind;
use crate::native::{HostError, IntoValue, Later};
use crate::vm::iomem::{MemoryLimit, Reservation};
use crate::vm::{Ctl, Vm};
use std::future::Future;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt};

/// An error produced off the VM thread, turned into a Sand `Error` on it.
#[derive(Debug)]
pub(crate) struct IoFailure {
    pub kind: &'static str,
    pub message: String,
}

impl IoFailure {
    pub fn new(kind: &'static str, message: impl Into<String>) -> IoFailure {
        IoFailure { kind, message: message.into() }
    }
}

impl From<IoFailure> for HostError {
    fn from(e: IoFailure) -> HostError {
        HostError::new(e.kind, e.message)
    }
}

pub(crate) type Suspended<T> = Result<Later<T, HostError>, HostError>;

/// Suspends the calling task on `fut` and runs `ok` when `fut` succeeds.
pub(crate) fn suspend<T: Send + 'static, R: IntoValue + Send + 'static>(
    fut: impl Future<Output = Result<T, IoFailure>> + Send + 'static,
    ok: impl FnOnce(&mut Vm, T) -> Result<R, HostError> + Send + 'static,
) -> Suspended<R> {
    Ok(Later::new(async move {
        let r = fut.await;
        move |vm: &mut Vm| ok(vm, r?)
    }))
}

impl From<MemoryLimit> for IoFailure {
    fn from(e: MemoryLimit) -> IoFailure {
        IoFailure::new("limit", e.to_string())
    }
}

impl From<std::io::Error> for IoFailure {
    fn from(e: std::io::Error) -> IoFailure {
        IoFailure::new(io_kind(&e), e.to_string())
    }
}

pub(crate) fn utf8(vm: &Vm, data: Vec<u8>) -> Result<Option<String>, Ctl> {
    vm.charge_slow_bytes(data.len())?;
    Ok(String::from_utf8(data).ok())
}

pub(crate) fn bytes_read(_: &mut Vm, data: Option<Vec<u8>>) -> Result<Vec<u8>, HostError> {
    Ok(data.unwrap_or_default())
}

pub(crate) fn line_read(vm: &Vm, line: Option<Vec<u8>>, message: &str) -> Result<Option<String>, HostError> {
    let Some(line) = line else { return Ok(None) };
    utf8(vm, line)?.map(Some).ok_or_else(|| HostError::new("invalid", message))
}

#[cfg(any(feature = "os", web))]
pub(crate) fn timeout(d: jiff::SignedDuration) -> std::time::Duration {
    std::time::Duration::try_from(d).unwrap_or_default()
}

/// Reads a line and strips any trailing `\n` or `\r\n`.
pub(crate) async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, res: &mut Reservation) -> Result<Option<Vec<u8>>, IoFailure> {
    let line = read_line_bytes(r, usize::MAX, res).await?;
    Ok(line.map(|mut line| {
        strip_eol(&mut line);
        line
    }))
}

/// Reads up to `max` buffered bytes from `r`, filling the buffer first if empty.
pub(crate) async fn read_upto<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> Result<Vec<u8>, IoFailure> {
    let available = r.fill_buf().await?;
    let data = available[..available.len().min(max)].to_vec();
    r.consume(data.len());
    Ok(data)
}

/// Reads a line including its `\n` (if any); `None` at end of stream. Fails
/// with kind `"limit"` if the line exceeds `max` bytes. The line's buffer is
/// reserved in `res`.
pub(crate) async fn read_line_bytes<R: AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
    res: &mut Reservation,
) -> Result<Option<Vec<u8>>, IoFailure> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        if line.len() + take > max {
            return Err(IoFailure::new("limit", format!("line longer than {max} bytes")));
        }
        res.reserve_buf_upto(&mut line, take, max)?;
        line.extend_from_slice(&buf[..take]);
        r.consume(take);
        if done {
            return Ok(Some(line));
        }
    }
}

/// Appends everything up to the end of `r` to `data`, whose buffer is
/// reserved in `res` as data arrives and grows to at most `cap` bytes (`r`
/// must end by then, e.g. through `take`).
pub(crate) async fn read_to_end_reserved<R: AsyncRead + Unpin>(
    r: &mut R,
    data: &mut Vec<u8>,
    res: &mut Reservation,
    cap: usize,
) -> Result<(), IoFailure> {
    loop {
        res.reserve_buf_upto(data, (16 << 10).min(cap.saturating_sub(data.len())), cap)?;
        if r.read_buf(data).await? == 0 {
            return Ok(());
        }
    }
}

/// Removes a trailing `\n` or `\r\n`.
pub(crate) fn strip_eol(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
}
