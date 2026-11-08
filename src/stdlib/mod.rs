//! The Sand standard library.
//!
//! Host code can use the Rust types in [`json`], [`log`], and [`time`].

mod base64;
pub(crate) mod core;
mod crypto;
mod fs;
#[cfg(feature = "os")]
pub(crate) mod http;
#[cfg(any(feature = "os", web))]
mod http_common;
#[cfg(feature = "os")]
mod http_server;
#[cfg(all(web, not(feature = "os")))]
mod http_web;
mod io;
pub mod json;
pub mod log;
mod math;
#[cfg(feature = "os")]
pub(crate) mod net;
mod random;
mod regex;
mod statistics;
mod stream;
mod test;
pub mod time;
pub(crate) mod uri;
mod uuid;

use crate::native::NativeModule;

/// Standard modules that need operating-system access: without the `os`
/// feature they do not exist, and importing them is an error that says why.
#[cfg(not(web))]
pub(crate) const OS_MODULES: &[&str] = &["http", "net"];
/// On the web, `http` goes through the browser's `fetch`.
#[cfg(web)]
pub(crate) const OS_MODULES: &[&str] = &["net"];

/// The names of the standard library modules and the prelude, on all
/// platforms. A host module cannot use one of these names, and `import` of
/// one of these names never loads a library script
/// (see [`Config::library`](crate::engine::Config::library)).
pub const STD_MODULES: &[&str] = &[
    "prelude",
    "base64",
    "crypto",
    "env",
    "fs",
    "http",
    "io",
    "json",
    "log",
    "math",
    "net",
    "random",
    "regex",
    "statistics",
    "task",
    "test",
    "time",
    "uri",
    "uuid",
];

/// Returns a prefix of `s` containing at most `max` characters. Use this
/// function to bound script text included in diagnostic or error messages.
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub(crate) fn modules() -> Vec<NativeModule> {
    vec![
        core::prelude(),
        core::task(),
        base64::module(),
        crypto::module(),
        fs::module(),
        #[cfg(feature = "os")]
        http::module(),
        #[cfg(all(web, not(feature = "os")))]
        http_web::module(),
        io::module(),
        io::env_module(),
        json::module(),
        log::module(),
        math::module(),
        #[cfg(feature = "os")]
        net::module(),
        random::module(),
        regex::module(),
        statistics::module(),
        test::module(),
        time::module(),
        uri::module(),
        uuid::module(),
    ]
}

/// The standard error kind for an I/O error.
pub(crate) fn io_kind(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => "not_found",
        PermissionDenied => "permission_denied",
        AlreadyExists => "already_exists",
        TimedOut | WouldBlock => "timeout",
        // Only regular files are opened, so a directory is an invalid argument.
        InvalidData | InvalidInput | IsADirectory => "invalid",
        // The memory limit (`vm::iomem`), or a full `MemFs`.
        OutOfMemory | QuotaExceeded => "limit",
        _ => "io",
    }
}
