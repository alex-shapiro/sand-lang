//! The standard library: Sand declarations plus Rust natives.
//!
//! Each module `name` consists of `name.sand` (public declarations, docs and
//! pure-Sand helpers) and `name.rs` (the natives, registered by name).

mod base64;
mod core;
mod crypto;
mod fs;
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
        _ => "io",
    }
}

/// Resolves a script-supplied path against the engine's working directory.
pub(crate) fn resolve_path(vm: &crate::vm::Vm, p: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(p);
    let joined = if path.is_absolute() { path.to_path_buf() } else { vm.host.cwd.join(path) };
    crate::policy::normalize_path(&joined)
}
