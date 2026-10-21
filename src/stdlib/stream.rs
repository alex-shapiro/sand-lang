//! Errors and buffered reads shared by the modules that do I/O off the VM
//! thread (`io`'s standard input, `net`, `http`).

use super::io_kind;
use crate::vm::Ctl;
use crate::vm::iomem::{MemoryLimit, Reservation};
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

/// A native returns an `IoFailure` to the script as `Err(Error)`.
impl From<IoFailure> for Ctl {
    fn from(e: IoFailure) -> Ctl {
        Ctl::native_err(e.kind, e.message)
    }
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
