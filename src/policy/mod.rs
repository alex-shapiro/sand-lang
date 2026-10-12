//! Sandbox policy: capabilities, decisions, rule-based policies and prompting.

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
pub use net::{is_private_address, is_valid_net_pattern};
pub use rules::RulePolicy;

/// A request to perform an effect outside the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    FsRead(PathBuf),
    FsWrite(PathBuf),
    /// Outgoing connection, by the host name (or IP literal) the script used.
    Net {
        host: String,
        port: u16,
    },
    /// An HTTP request. `host` is lower case, an IPv6 address without
    /// brackets; `path` is the request's path without the query, with no
    /// `.` or `..` segments (as sent).
    Http {
        method: String,
        scheme: String,
        host: String,
        port: u16,
        path: String,
    },
    /// One address `host` resolved to, checked after `Net` was granted and
    /// before connecting. Never prompted: a `Prompt` decision counts as
    /// `Allow` (the `Net` request was already decided).
    NetAddr {
        host: String,
        addr: SocketAddr,
    },
    /// Listening socket address (`host:port`).
    NetListen(String),
    /// Environment variable (`"*"` for all).
    Env(String),
    Stdin,
    Stdout,
    Stderr,
    /// Loading a script module.
    Import(PathBuf),
    /// Defined by host native modules.
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
            Capability::Net { host, port } => write!(f, "connect to {host}:{port}"),
            Capability::Http { method, scheme, host, port, path } => {
                let host = if host.contains(':') { format!("[{host}]") } else { host.clone() };
                let default = if scheme == "http" { 80 } else { 443 };
                if *port == default {
                    write!(f, "{method} {scheme}://{host}{path}")
                } else {
                    write!(f, "{method} {scheme}://{host}:{port}{path}")
                }
            }
            Capability::NetAddr { host, addr } if *host == addr.ip().to_string() => write!(f, "connect to {addr}"),
            Capability::NetAddr { host, addr } => write!(f, "connect to {host} at {addr}"),
            Capability::NetListen(a) => write!(f, "listen on {a}"),
            Capability::Env(n) => write!(f, "read environment variable {n}"),
            Capability::Stdin => write!(f, "read standard input"),
            Capability::Stdout => write!(f, "write standard output"),
            Capability::Stderr => write!(f, "write standard error"),
            Capability::Import(p) => write!(f, "import {}", p.display()),
            Capability::Custom { name, detail } => write!(f, "{name} {detail}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Asks the user about a capability. While a prompt is pending the engine
/// runs no script code.
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

/// A policy deciding capability requests.
#[derive(Clone, Default)]
pub enum Policy {
    AllowAll,
    #[default]
    DenyAll,
    Rules(Arc<RulePolicy>),
    Custom(Arc<dyn Fn(&Capability) -> Decision + Send + Sync>),
}

impl fmt::Debug for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Policy::AllowAll => write!(f, "AllowAll"),
            Policy::DenyAll => write!(f, "DenyAll"),
            Policy::Rules(r) => write!(f, "Rules({r:?})"),
            Policy::Custom(_) => write!(f, "Custom"),
        }
    }
}

impl Policy {
    /// Everything allowed: only for trusted scripts.
    pub fn allow_all() -> Policy {
        Policy::AllowAll
    }
    /// Everything denied except standard output and error.
    pub fn deny_all() -> Policy {
        Policy::DenyAll
    }
    pub fn rules(r: RulePolicy) -> Policy {
        Policy::Rules(Arc::new(r))
    }
    pub fn custom(f: impl Fn(&Capability) -> Decision + Send + Sync + 'static) -> Policy {
        Policy::Custom(Arc::new(f))
    }

    /// Decides `cap`. A rule policy first normalizes a file capability's path
    /// with [`normalize_path`]; [`Policy::check_normalized`] skips that.
    pub fn check(&self, cap: &Capability) -> Decision {
        self.check_in(cap, PathSpace::Host)
    }

    /// Like [`Policy::check`] for paths of the file system `space`.
    pub(crate) fn check_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::AllowAll => Decision::Allow,
            Policy::DenyAll => match cap {
                Capability::Stdout | Capability::Stderr | Capability::Import(_) => Decision::Allow,
                _ => Decision::Deny,
            },
            Policy::Rules(r) => r.check_in(cap, space),
            Policy::Custom(f) => f(cap),
        }
    }

    /// Like [`Policy::check`] for a capability whose path (`FsRead`,
    /// `FsWrite`) the caller already normalized with [`normalize_path`]
    /// (and will operate on exactly): a rule policy skips normalizing it
    /// again, which costs a `realpath`.
    pub fn check_normalized(&self, cap: &Capability) -> Decision {
        self.check_normalized_in(cap, PathSpace::Host)
    }

    /// Like [`Policy::check_normalized`] for paths of the file system `space`
    /// (normalized by it).
    pub(crate) fn check_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::Rules(r) => r.check_normalized_in(cap, space),
            _ => self.check_in(cap, space),
        }
    }

    /// Like [`Policy::check`] for an operation on a directory entry itself
    /// (removing or renaming it): a final symlink in an `FsRead`/`FsWrite`
    /// path is the entry and is not resolved to its target.
    pub fn check_entry(&self, cap: &Capability) -> Decision {
        self.check_entry_in(cap, PathSpace::Host)
    }

    /// Like [`Policy::check_entry`] for paths of the file system `space`.
    pub(crate) fn check_entry_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match self {
            Policy::Rules(r) => r.check_entry_in(cap, space),
            _ => self.check_in(cap, space),
        }
    }

    /// The decision on `cap` (a file or import capability normalized except
    /// for the final name's spelling) if it holds for every spelling. `None`
    /// for a custom policy (which may compare paths exactly), an allow rule
    /// naming the final name, or a prompt (which shows the path).
    pub(crate) fn decision_for_any_spelling(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        match self {
            Policy::AllowAll | Policy::DenyAll => Some(self.check_in(cap, space)),
            Policy::Custom(_) => None,
            Policy::Rules(r) => r.decision_for_any_spelling(cap, space),
        }
    }

    /// Whether a deny rule may match `path` or a path below it. If one may,
    /// the tree at `path` must be scanned for protected entries
    /// ([`Policy::protects_normalized_in`]) before it is moved or removed.
    pub fn denies_under(&self, path: &Path) -> bool {
        self.denies_under_in(path, PathSpace::Host)
    }

    /// Like [`Policy::denies_under`] for a path of the file system `space`.
    pub(crate) fn denies_under_in(&self, path: &Path, space: PathSpace) -> bool {
        match self {
            Policy::Rules(r) => r.denies_under_normalized_in(&rules::entry_path(path, space), space),
            _ => false,
        }
    }

    /// Like [`Policy::denies_under_in`] for a path already normalized as
    /// the path of an entry itself.
    pub(crate) fn denies_under_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        match self {
            Policy::Rules(r) => r.denies_under_normalized_in(path, space),
            _ => false,
        }
    }

    /// Whether a deny rule decides reading or writing the entry at `path`
    /// (normalized in `space`). Such an entry protects every tree it is in:
    /// none of them may be moved or removed as a whole, even if the policy
    /// allows writing the tree's root.
    pub(crate) fn protects_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        match self {
            Policy::Rules(r) => r.protects_normalized_in(path, space),
            _ => false,
        }
    }
}

/// The file system whose paths a check is about. A rule policy resolves its
/// rules' paths for each: canonicalised on the host, lexically normalized
/// for a memory file system ([`crate::MemFs`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathSpace {
    Host,
    Memory,
}

/// Paths (as given by a script, before normalization) longer than this many
/// bytes are denied by the file system functions and imports: no usable
/// path is that long (`PATH_MAX` is 4096 bytes on Linux and 1024 on macOS),
/// and normalizing one costs time in proportion to its length.
pub const MAX_PATH_BYTES: usize = 4096;

/// Resolves `.` and `..` lexically.
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

/// Normalizes a path lexically (`.`, `..`), then resolves its longest
/// existing prefix on the host's file system as `realpath` does: following
/// symlinks and, on macOS, spelling components as stored and resolving
/// firmlinks (`/System/Volumes/Data/Users` is `/Users`). A symlink whose
/// target does not resolve is kept, with the rest of the path.
///
/// Without the `os` feature, paths are only normalized lexically.
pub fn normalize_path(p: &Path) -> PathBuf {
    #[cfg(feature = "os")]
    return crate::files::Fs::Host.resolve(&lexical(p));
    #[cfg(not(feature = "os"))]
    return lexical(p);
}

/// Like [`normalize_path`], but a final symlink is not resolved: the path of
/// a directory entry itself (its parent directory normalized). An existing
/// entry is named as stored, so that on a case- or
/// normalization-insensitive file system `data/ſecrets` becomes
/// `data/secrets`; for a symlink this may take a scan of its directory (see
/// [`MAX_LINK_SCAN`]).
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

/// The most directory entries read to find an entry's stored name when
/// there is no other way (a symlink's; on macOS, a device's or an unreadable
/// file's). In a larger directory the entry keeps the given name, which deny
/// rules still match but allow rules must name exactly.
pub const MAX_LINK_SCAN: usize = 4096;

/// A caseless, normalization-insensitive form of a file name, coarser than
/// the comparisons of case- and normalization-insensitive file systems:
/// canonical decomposition (NFD), case folding (lower case of the upper case,
/// twice, which also folds `ſ`, `ς`, `ß`, `ẞ`, ligatures and the Kelvin
/// sign), NFD again, without default-ignorable characters (which HFS+
/// ignores). Two names a file system may treat as the same entry have the
/// same form.
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

/// Unicode's Default_Ignorable_Code_Point characters (zero-width and
/// formatting characters, variation selectors, ...).
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
    fn folded_names_equate_what_file_systems_may_equate() {
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

    /// The resolver agrees with `realpath` (the standard library's
    /// `canonicalize`) on the longest prefix that resolves.
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
                // The obvious algorithm: drop components from the end until it resolves.
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
        // Very long paths take few calls.
        let long = base.join("a/".repeat(100_000) + "b");
        let start = std::time::Instant::now();
        assert!(normalize_path(&long).starts_with(&base));
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// On a case-insensitive file system (macOS's default), entries are
    /// named as stored, symlinks too.
    #[cfg(all(feature = "os", target_os = "macos"))]
    #[test]
    fn entries_are_named_as_stored() {
        let base = normalize_path(&std::env::temp_dir()).join(format!("sand-policy-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Dir")).unwrap();
        std::fs::write(base.join("Dir/File.txt"), "").unwrap();
        std::os::unix::fs::symlink("/nowhere", base.join("Link")).unwrap();
        if !base.join("dir").exists() {
            // A case-sensitive volume.
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
    fn checks_decide_by_stored_names() {
        let base = normalize_path(&std::env::temp_dir()).join(format!("sand-policy-spelling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Dir/Sub")).unwrap();
        for f in ["Dir/File.txt", "Dir/Other.txt", "Dir/Sub/Secret.txt", "Dir/Sub/x.txt"] {
            std::fs::write(base.join(f), "").unwrap();
        }
        if !base.join("dir").exists() {
            std::fs::remove_dir_all(&base).unwrap();
            return;
        }
        let p = Policy::rules(
            RulePolicy::new()
                .allow_read(base.join("Dir/File.txt"))
                .allow_read(base.join("Dir/Sub"))
                .deny_read(base.join("Dir/Sub/Secret.txt")),
        );
        for (given, want) in [
            ("dir/file.txt", Decision::Allow),
            ("DIR/FILE.TXT", Decision::Allow),
            ("dir/other.txt", Decision::Deny),
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
