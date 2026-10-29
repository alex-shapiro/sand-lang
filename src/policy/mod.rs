//! Sandbox policy types, rule evaluation, and interactive prompts.

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

mod file;
mod format;
mod lint;
mod net;
mod path;
mod rules;

pub use file::{PolicyError, PolicyFile, parse_duration, parse_size, rule_for};
pub use format::format_policy;
pub use lint::lint_policy;
use net::format_url_origin;
pub use net::{check_net_pattern, format_host_port, is_private_address, is_valid_listen_pattern};
#[cfg(feature = "os")]
pub(crate) use net::{is_localhost, is_valid_host, parse_ip, parse_port, split_port};
pub use rules::RulePolicy;

/// A request for an operation outside the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    FsRead(PathBuf),
    FsWrite(PathBuf),
    /// Outgoing TCP connection to a hostname or unbracketed IP address.
    Net {
        host: String,
        port: u16,
        tls: bool,
    },
    /// Outgoing HTTP request. `host` is lowercase without brackets around
    /// IPv6 addresses. `path` excludes the query string and has `.` and `..`
    /// segments removed.
    Http {
        method: String,
        scheme: String,
        host: String,
        port: u16,
        path: String,
    },
    /// Resolved socket address for an allowed `Net` or `Http` request. A
    /// `Prompt` decision from [`CustomPolicy::check`] is treated as `Allow`.
    NetAddr {
        host: String,
        addr: SocketAddr,
        tls: bool,
    },
    /// TCP listener bind request. Port `0` requests an ephemeral port.
    NetListen(SocketAddr),
    /// Environment variable read request, or `"*"` to read all variables.
    Env(String),
    Stdin,
    Stdout,
    Stderr,
    /// Local time zone query.
    TimeZone,
    /// Script module import.
    Import(PathBuf),
    /// Capability defined by a native host module.
    Custom {
        name: String,
        detail: String,
    },
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Capability::FsRead(p) => write!(f, "read {}", p.display()),
            Capability::FsWrite(p) => write!(f, "write {}", p.display()),
            Capability::Net { host, port, tls: true } => write!(f, "connect to {}", format_host_port(host, *port)),
            Capability::Net { host, port, tls: false } => write!(f, "connect to {} (plaintext)", format_host_port(host, *port)),
            Capability::Http { method, scheme, host, port, path } => {
                write!(f, "{method} {}{path}", format_url_origin(scheme, host, *port))
            }
            Capability::NetAddr { host, addr, tls } => {
                if *host == addr.ip().to_string() {
                    write!(f, "connect to {addr}")?
                } else {
                    write!(f, "connect to {host} at {addr}")?
                }
                if *tls { Ok(()) } else { write!(f, " (plaintext)") }
            }
            Capability::NetListen(addr) if addr.port() == 0 => write!(f, "listen on an ephemeral port at {}", addr.ip()),
            Capability::NetListen(addr) => write!(f, "listen on {addr}"),
            Capability::Env(n) if n == "*" => write!(f, "read all environment variables"),
            Capability::Env(n) => write!(f, "read environment variable {n}"),
            Capability::Stdin => write!(f, "read standard input"),
            Capability::Stdout => write!(f, "write standard output"),
            Capability::Stderr => write!(f, "write standard error"),
            Capability::TimeZone => write!(f, "read the local time zone"),
            Capability::Import(p) => write!(f, "import {}", p.display()),
            Capability::Custom { name, detail } => write!(f, "{name} {detail}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Decision {
    Allow,
    Deny,
    Prompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAnswer {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

pub type PromptFuture = Pin<Box<dyn Future<Output = PromptAnswer> + Send>>;

/// Prompts the user to allow or deny a capability. Script execution pauses
/// until the returned future completes.
pub trait Prompter: Send + Sync {
    fn prompt(&self, cap: Capability) -> PromptFuture;
}

impl<F, Fut> Prompter for F
where
    F: Fn(Capability) -> Fut + Send + Sync,
    Fut: Future<Output = PromptAnswer> + Send + 'static,
{
    fn prompt(&self, cap: Capability) -> PromptFuture {
        Box::pin(self(cap))
    }
}

/// Host-defined policy callback; see [`crate::Config::custom_policy`].
pub trait CustomPolicy: Send + Sync {
    /// Evaluates `cap`. File paths in `cap` are normalized with [`normalize_path`].
    fn check(&self, cap: &Capability) -> Decision;

    /// Evaluates an operation on a directory entry such as removal or
    /// rename without following a final symlink.
    fn check_entry(&self, cap: &Capability) -> Decision {
        self.check(cap)
    }

    /// Returns `true` if the policy can deny an entry at or below `path`. If
    /// this returns `true`, the engine scans the directory tree with
    /// [`CustomPolicy::protects`] before moving or removing it.
    fn denies_under(&self, _path: &Path) -> bool {
        true
    }

    /// Returns `true` if the entry at `path` is protected from removal or rename.
    fn protects(&self, path: &Path) -> bool {
        self.check(&Capability::FsRead(path.to_path_buf())) == Decision::Deny
            || self.check_entry(&Capability::FsWrite(path.to_path_buf())) == Decision::Deny
    }
}

impl<F: Fn(&Capability) -> Decision + Send + Sync> CustomPolicy for F {
    fn check(&self, cap: &Capability) -> Decision {
        self(cap)
    }
}

#[derive(Clone)]
pub(crate) enum Policy {
    Rules(Arc<RulePolicy>),
    Custom(Arc<dyn CustomPolicy>),
}

impl fmt::Debug for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Policy::Rules(r) => write!(f, "Rules({r:?})"),
            Policy::Custom(_) => write!(f, "Custom"),
        }
    }
}

impl Policy {
    /// Evaluates `cap` against the host file system, normalizing file paths first.
    pub(crate) fn check(&self, cap: &Capability) -> Decision {
        self.check_in(cap, PathSpace::Host)
    }

    pub(crate) fn check_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::Rules(r) => r.check_in(cap, space),
            Policy::Custom(p) => p.check(cap),
        }
    }

    /// Checks whether a connection to `addr` resolved from an allowed `Net`
    /// or `Http` `request` is permitted. Returns the denial reason on `Err`.
    #[cfg(feature = "os")]
    pub(crate) fn check_resolved(&self, request: &Capability, addr: SocketAddr) -> Result<(), String> {
        match self {
            Policy::Rules(r) => r.resolved_verdict(request, addr).map_err(|rule| match rule {
                Some(rule) => format!("is not allowed by {}", r.describe_rule(rule)),
                None => "is not the requested address".to_string(),
            }),
            Policy::Custom(p) => {
                let (Capability::Net { host, .. } | Capability::Http { host, .. }) = request else { return Ok(()) };
                match p.check(&Capability::NetAddr { host: host.clone(), addr, tls: rules::uses_tls(request) }) {
                    Decision::Deny if is_private_address(addr.ip()) => Err("is private or internal and is not allowed".into()),
                    Decision::Deny => Err("is not allowed".into()),
                    Decision::Allow | Decision::Prompt => Ok(()),
                }
            }
        }
    }

    /// Evaluates `cap` if its file path is already normalized for `space`.
    pub(crate) fn check_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::Rules(r) => r.check_normalized_in(cap, space),
            Policy::Custom(p) => p.check(cap),
        }
    }

    /// Evaluates `cap` for a directory entry operation without following a final symlink.
    pub(crate) fn check_entry_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::Rules(r) => r.check_entry_in(cap, space),
            Policy::Custom(p) => p.check_entry(cap),
        }
    }

    pub(crate) fn check_entry_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::Rules(r) => r.check_normalized_in(cap, space),
            Policy::Custom(p) => p.check_entry(cap),
        }
    }

    /// Returns the decision for `cap` if the result does not depend on the
    /// on-disk case or Unicode normalization of the final path component.
    pub(crate) fn case_insensitive_decision(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        match self {
            Policy::Custom(_) => None,
            Policy::Rules(r) => r.case_insensitive_decision(cap, space),
        }
    }

    /// Returns `true` if a deny or prompt rule can match `path` or a descendant in `space`.
    pub(crate) fn denies_under_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        match self {
            Policy::Rules(r) => r.denies_under_normalized_in(path, space),
            Policy::Custom(p) => p.denies_under(path),
        }
    }

    /// Returns `true` if a deny rule matches reading or writing `path` in `space`.
    pub(crate) fn protects_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        match self {
            Policy::Rules(r) => r.protects_normalized_in(path, space),
            Policy::Custom(p) => p.protects(path),
        }
    }
}

/// Target file system for path resolution during policy checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathSpace {
    Host,
    Memory,
}

/// Maximum script-supplied path length in bytes before normalization.
pub const MAX_PATH_BYTES: usize = 4096;

/// Resolves `.` and `..` components lexically.
pub(crate) fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Normalizes `p` lexically and resolves the longest existing prefix on the
/// host file system. Without the `os` feature, only lexical normalization is
/// performed.
pub fn normalize_path(p: &Path) -> PathBuf {
    #[cfg(feature = "os")]
    return crate::files::Fs::Host.resolve(&lexical(p));
    #[cfg(not(feature = "os"))]
    return lexical(p);
}

/// Normalizes parent path components and records the on-disk name of the
/// final component without following a final symlink.
pub fn normalize_entry_path(p: &Path) -> PathBuf {
    #[cfg(feature = "os")]
    {
        let mut t = crate::files::Fs::Host.target(&lexical(p), crate::files::Mode::Entry);
        t.fix_spelling();
        t.path
    }
    #[cfg(not(feature = "os"))]
    return lexical(p);
}

/// Maximum number of directory entries scanned to recover the on-disk name
/// of an entry that cannot be opened directly.
pub const MAX_LINK_SCAN: usize = 4096;

/// Returns a case-folded, NFD-normalized key with default-ignorable code
/// points removed so names that compare equal on case- or
/// normalization-insensitive file systems produce the same key.
pub(crate) fn fold_name(name: &str) -> std::borrow::Cow<'_, str> {
    use unicode_normalization::UnicodeNormalization;
    if name.is_ascii() {
        return if name.bytes().any(|b| b.is_ascii_uppercase()) { name.to_ascii_lowercase().into() } else { name.into() };
    }
    let mut s: String = name.nfd().collect();
    for _ in 0..2 {
        s = s.to_uppercase().to_lowercase();
    }
    s.nfd().filter(|&c| !is_default_ignorable(c)).collect::<String>().into()
}

fn is_default_ignorable(c: char) -> bool {
    matches!(c as u32,
        0xAD | 0x34F | 0x61C | 0x115F..=0x1160 | 0x17B4..=0x17B5 | 0x180B..=0x180F
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
        | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFF8 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0000..=0xE0FFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_name_equates_case_and_normalization_variants() {
        let same = [
            ("secrets", "ſecrets"),
            ("secrets", "SECRETS"),
            ("config.json", "config.jſon"),
            ("kelvin", "\u{212a}elvin"),
            ("σοφία", "ΣΟΦΊΑ"),
            ("λόγος", "λόγοσ"),
            ("straße", "STRASSE"),
            ("straße", "stra\u{1e9e}e"),
            ("café", "cafe\u{301}"),
            ("Ångström", "\u{212b}ngstro\u{308}m"),
            ("a;b", "a\u{37e}b"),
            ("ﬁle", "FILE"),
            ("se\u{200b}crets", "secrets"),
            ("secrets\u{feff}", "secrets"),
        ];
        for (a, b) in same {
            assert_eq!(fold_name(a), fold_name(b), "{a:?} vs {b:?}");
        }
        for (a, b) in [("secrets", "secret"), ("café", "cafe"), ("a", "b")] {
            assert_ne!(fold_name(a), fold_name(b), "{a:?} vs {b:?}");
        }
        assert_eq!(fold_name("plain.txt"), "plain.txt");
    }

    #[test]
    #[cfg(all(feature = "os", unix))]
    fn normalization_finds_the_longest_existing_prefix() {
        let base = normalize_path(&std::env::temp_dir()).join(format!("sand-policy-norm-{}", std::process::id()));
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/file"), "").unwrap();
        std::os::unix::fs::symlink(base.join("a/b"), base.join("link")).unwrap();
        std::os::unix::fs::symlink("../a/file", base.join("a/b/file_link")).unwrap();
        std::os::unix::fs::symlink("missing/x", base.join("a/dangling")).unwrap();
        for rest in [
            "",
            "a",
            "a/b",
            "a/b/c",
            "a/b/c/d/e",
            "a/x/../b/./c",
            "missing/a/b",
            "a/b/../../..",
            "link",
            "link/c/d",
            "link/file_link",
            "link/file_link/x",
            "a/dangling",
            "a/dangling/x/y",
            "a/file/x",
        ] {
            let p = base.join(rest);
            let expected = {
                let lexical = lexical(&p);
                let mut existing = lexical.clone();
                let mut tail = Vec::new();
                while existing.canonicalize().is_err() {
                    tail.push(existing.file_name().unwrap().to_os_string());
                    existing.pop();
                }
                let mut r = existing.canonicalize().unwrap();
                r.extend(tail.iter().rev());
                r
            };
            assert_eq!(normalize_path(&p), expected, "{rest}");
        }
        let long = base.join("a/".repeat(100_000) + "b");
        let start = std::time::Instant::now();
        assert!(normalize_path(&long).starts_with(&base));
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(all(feature = "os", target_os = "macos"))]
    #[test]
    fn entries_are_resolved_to_on_disk_names() {
        let base = normalize_path(&std::env::temp_dir()).join(format!("sand-policy-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Dir")).unwrap();
        std::fs::write(base.join("Dir/File.txt"), "").unwrap();
        std::os::unix::fs::symlink("/nowhere", base.join("Link")).unwrap();
        if !base.join("dir").exists() {
            std::fs::remove_dir_all(&base).unwrap();
            return;
        }
        assert_eq!(normalize_entry_path(&base.join("LINK")), base.join("Link"));
        assert_eq!(normalize_entry_path(&base.join("Link")), base.join("Link"));
        assert_eq!(normalize_path(&base.join("DIR/FILE.TXT")), base.join("Dir/File.txt"));
        assert_eq!(normalize_entry_path(&base.join("dir/file.txt")), base.join("Dir/File.txt"));
        assert_eq!(normalize_path(&base.join("dir/new.TXT")), base.join("Dir/new.TXT"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(all(feature = "os", target_os = "macos"))]
    #[test]
    fn checks_use_stored_file_names() {
        let base = normalize_path(&std::env::temp_dir()).join(format!("sand-policy-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Dir/Sub")).unwrap();
        for f in ["Dir/File.txt", "Dir/Other.txt", "Dir/Sub/Secret.txt", "Dir/Sub/x.txt"] {
            std::fs::write(base.join(f), "").unwrap();
        }
        if !base.join("dir").exists() {
            std::fs::remove_dir_all(&base).unwrap();
            return;
        }
        let p = RulePolicy::new()
            .allow_read(base.join("Dir/File.txt"))
            .allow_read(base.join("Dir/Sub"))
            .deny_read(base.join("Dir/Sub/Secret.txt"));
        for (given, want) in [
            ("dir/file.txt", Decision::Allow),
            ("DIR/FILE.TXT", Decision::Allow),
            ("dir/other.txt", Decision::Prompt),
            ("dir/sub/X.TXT", Decision::Allow),
            ("dir/sub/SECRET.txt", Decision::Deny),
            ("dir/sub/new.txt", Decision::Allow),
        ] {
            assert_eq!(p.check(&Capability::FsRead(base.join(given))), want, "{given}");
            assert_eq!(p.check_entry(&Capability::FsRead(base.join(given))), want, "{given}");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
