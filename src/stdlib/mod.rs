//! The standard library: Sand declarations plus Rust natives.
//!
//! Each module `name` consists of `name.sand` (public declarations, docs and
//! pure-Sand helpers) and `name.rs` (the natives, registered by name).

mod base64;
mod core;
mod crypto;
mod fs;
mod fs_safe;
mod help;
mod http;
mod io;
mod json;
mod log;
mod math;
mod net;
mod random;
mod regex;
mod statistics;
mod test;
mod time;
mod uri;
mod uuid;

use crate::engine::Registry;
pub(crate) use net::TlsSettings;

pub(crate) fn register(r: &mut Registry) {
    core::register(r);
    base64::register(r);
    crypto::register(r);
    fs::register(r);
    help::register(r);
    http::register(r);
    io::register(r);
    json::register(r);
    log::register(r);
    math::register(r);
    net::register(r);
    random::register(r);
    regex::register(r);
    statistics::register(r);
    test::register(r);
    time::register(r);
    uri::register(r);
    uuid::register(r);
}

/// The standard error kind for an I/O error.
pub(crate) fn io_kind(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => "not_found",
        PermissionDenied => "permission_denied",
        AlreadyExists => "already_exists",
        TimedOut | WouldBlock => "timeout",
        InvalidData | InvalidInput => "invalid",
        // I/O buffers that do not fit in the memory limit (see `vm::iomem`).
        OutOfMemory => "limit",
        _ => "io",
    }
}

/// Reads a script module (at most `max + 1` bytes, so that oversized sources
/// are detected without reading them whole) without following symlinks.
pub(crate) fn read_source(path: &std::path::Path, max: usize) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read};
    let mut data = Vec::new();
    fs_safe::open_read(path)?.take(max as u64 + 1).read_to_end(&mut data)?;
    if data.len() > max {
        return Err(Error::new(ErrorKind::InvalidData, "source too large"));
    }
    String::from_utf8(data).map_err(|_| Error::new(ErrorKind::InvalidData, "source is not valid UTF-8"))
}

/// Resolves a script-supplied path against the engine's working directory.
pub(crate) fn resolve_path(vm: &crate::vm::Vm, p: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(p);
    let joined = if path.is_absolute() { path.to_path_buf() } else { vm.host.cwd.join(path) };
    crate::policy::normalize_path(&joined)
}
