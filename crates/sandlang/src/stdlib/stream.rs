//! Shared I/O errors, async results, and buffered readers for the `fs`, `io`,
//! `net`, and `http` modules.

use super::io_kind;
use crate::native::convert::{HostError, IntoVmValue};
use crate::native::func::Later;
use crate::vm::iomem::{MemoryLimit, Reservation};
use crate::vm::{Fail, Vm, WaitKind};
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

impl<C> From<IoFailure> for HostError<C> {
    fn from(e: IoFailure) -> HostError<C> {
        HostError::new(e.kind, e.message)
    }
}

pub(crate) type Suspended<T> = Result<Later<T, HostError>, HostError>;

/// Suspends the calling task on `future` with `wait_kind`.
pub(crate) fn suspend<T: IntoVmValue + 'static>(
    wait_kind: WaitKind,
    future: impl Future<Output = Result<T, IoFailure>> + Send + 'static,
) -> Suspended<T> {
    Ok(Later::new(wait_kind, |_| async move { Ok(future.await?) }))
}

/// Suspends the calling task on `line_future` with `wait_kind` to read a
/// UTF-8 line.
///
/// Returns `None` at end of stream. Returns an `"invalid"` [`HostError`] with
/// `invalid_utf8_message` on invalid UTF-8 bytes.
pub(crate) fn suspend_line(
    wait_kind: WaitKind,
    line_future: impl Future<Output = Result<Option<Vec<u8>>, IoFailure>> + Send + 'static,
    invalid_utf8_message: &'static str,
) -> Suspended<Option<String>> {
    Ok(Later::new(wait_kind, |context| async move {
        let line = line_future.await?;
        context.with_vm(|vm| decode_utf8_line(vm, line, invalid_utf8_message))
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

pub(crate) fn utf8(vm: &Vm, data: Vec<u8>) -> Result<Option<String>, Fail> {
    vm.charge_slow_bytes(data.len())?;
    Ok(String::from_utf8(data).ok())
}

/// Decodes an optional byte line as UTF-8.
///
/// Charges fuel for the byte length of `line`. Returns `Ok(None)` if `line` is
/// `None`. Returns an `"invalid"` [`HostError`] with `invalid_utf8_message` on
/// invalid UTF-8 bytes.
pub(crate) fn decode_utf8_line(vm: &Vm, line: Option<Vec<u8>>, invalid_utf8_message: &str) -> Result<Option<String>, HostError<Fail>> {
    let Some(line) = line else { return Ok(None) };
    utf8(vm, line)?.map(Some).ok_or_else(|| HostError::new("invalid", invalid_utf8_message))
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
