//! The host's file system (the `os` feature): paths resolved by the
//! platform's walk (`unix.rs`, `windows.rs`) and operations through it.

use crate::vm::iomem::Reservation;
use std::fs::File;
use std::io::{self, Read, Write};

#[cfg(unix)]
pub(crate) use super::unix::{Resolved, resolve};
#[cfg(windows)]
pub(crate) use super::windows::{Resolved, resolve};

/// Reads the regular file `file` into a buffer reserved in `res`. The
/// file's length is reserved first, so a file too large for the memory
/// limit fails before it is read. The buffer starts at no more than 64 MiB
/// (the length may be that of a sparse file) and grows as data arrives.
pub(crate) fn read_reserved(mut file: File, mut res: Reservation) -> io::Result<(Vec<u8>, Reservation)> {
    let len = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    res.ensure(len)?;
    let mut data = Vec::new();
    res.reserve_buf(&mut data, len.min(64 << 20))?;
    let mut chunk = [0u8; 8192];
    loop {
        let start = data.len();
        let n = if data.capacity() > start {
            data.resize(data.capacity(), 0);
            let n = read_some(&mut file, &mut data[start..]);
            data.truncate(start + n.as_ref().map_or(0, |&n| n));
            n?
        } else {
            let n = read_some(&mut file, &mut chunk)?;
            res.reserve_buf(&mut data, n)?;
            data.extend_from_slice(&chunk[..n]);
            n
        };
        if n == 0 {
            return Ok((data, res));
        }
    }
}

fn read_some(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match r.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            r => return r,
        }
    }
}

/// Writes `data` to the opened file.
pub(crate) fn write_all(mut file: File, data: &[u8]) -> io::Result<()> {
    file.write_all(data)?;
    file.flush()
}

/// Copies the contents of `from` to `to`. Permissions are not copied: a new
/// file gets the default mode, so a copy never becomes executable (or
/// setuid) because its source was.
pub(crate) fn copy(mut from: File, mut to: File) -> io::Result<()> {
    io::copy(&mut from, &mut to)?;
    Ok(())
}
