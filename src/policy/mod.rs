//! Sandbox policy: capabilities, decisions, rule-based policies and prompting.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

mod file;
pub use file::{PolicyError, PolicyFile, parse_duration, parse_size, rule_for};

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

    /// Decides `cap`. A rule policy normalizes the path of a file capability
    /// first ([`normalize_path`], which resolves symlinks on the host's file
    /// system: a `realpath`); [`Policy::check_normalized`] skips that.
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

    /// The decision on `cap`, a file or import capability whose path is
    /// normalized except that its final name may be spelled differently
    /// than stored (on a case- or normalization-insensitive file system),
    /// if the decision is the same for every such spelling: `None` if the
    /// policy may decide differently (a custom policy, which may compare
    /// paths exactly, or an allow rule below the final name's directory)
    /// or would prompt (prompts show the path, and answers are remembered
    /// for it). Deny rules equate every spelling ([`fold_name`]).
    pub(crate) fn decision_for_any_spelling(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        let decision = match self {
            Policy::AllowAll | Policy::DenyAll => self.check_in(cap, space),
            Policy::Custom(_) => return None,
            Policy::Rules(r) => {
                let (Capability::FsRead(p) | Capability::FsWrite(p) | Capability::Import(p)) = cap else {
                    return None;
                };
                // An allow rule matches `dir/name` for every spelling of
                // `name` unless it is below `dir` (it then names one).
                let dir = p.parent().unwrap_or(p);
                let below = |r: &PathRule| {
                    let rule = match space {
                        PathSpace::Host => &r.host,
                        PathSpace::Memory => &r.memory,
                    };
                    path_starts_with(rule, dir) && rule.components().count() > dir.components().count()
                };
                if r.allow_rules(cap).iter().any(below) {
                    return None;
                }
                r.check_normalized_in(cap, space)
            }
        };
        (decision != Decision::Prompt).then_some(decision)
    }

    /// Whether a deny rule protects `path` or anything below it, so that
    /// moving or removing the whole tree at `path` must be refused even if
    /// `path` itself may be written. Only rule policies have such rules;
    /// other policies decide on the top-level paths alone.
    pub fn denies_under(&self, path: &Path) -> bool {
        self.denies_under_in(path, PathSpace::Host)
    }

    /// Like [`Policy::denies_under`] for a path of the file system `space`.
    pub(crate) fn denies_under_in(&self, path: &Path, space: PathSpace) -> bool {
        match self {
            Policy::Rules(r) => r.denies_under_normalized_in(&r.entry_path(path, space), space),
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
}

/// The file system whose paths a check is about. A rule policy keeps each
/// rule's path in the form of either: canonicalised on the host's file
/// system (resolving symlinks and taking the stored spelling of existing
/// components), and lexically normalized from the root for a memory file
/// system ([`crate::MemFs`]), which has no symlinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathSpace {
    Host,
    Memory,
}

/// A path of a rule, in the form of each [`PathSpace`], and (for deny
/// rules) folded by [`deny_key`] once.
#[derive(Clone, Debug)]
struct PathRule {
    host: PathBuf,
    memory: PathBuf,
    host_key: PathBuf,
    memory_key: PathBuf,
}

impl PathRule {
    fn new(p: &Path) -> PathRule {
        let (host, memory) = (normalize_path(p), crate::files::memory_path(p));
        PathRule { host_key: deny_key(&host), memory_key: deny_key(&memory), host, memory }
    }

    /// The folded path, for deny rules.
    fn key(&self, space: PathSpace) -> &Path {
        match space {
            PathSpace::Host => &self.host_key,
            PathSpace::Memory => &self.memory_key,
        }
    }
}

/// A rule-based policy. Deny rules take precedence over allow rules; requests
/// matching no rule get the fallback decision.
#[derive(Clone, Debug)]
pub struct RulePolicy {
    read_allow: Vec<PathRule>,
    read_deny: Vec<PathRule>,
    write_allow: Vec<PathRule>,
    write_deny: Vec<PathRule>,
    net_allow: Vec<NetPattern>,
    net_deny: Vec<NetPattern>,
    deny_private: bool,
    listen_allow: Vec<String>,
    listen_deny: Vec<String>,
    env_allow: Vec<String>,
    env_deny: Vec<String>,
    import_allow: Option<Vec<PathRule>>,
    import_deny: Vec<PathRule>,
    /// `Some(true)` allowed, `Some(false)` denied, `None` the fallback decides.
    stdin: Option<bool>,
    stdout: bool,
    stderr: bool,
    custom_allow: Vec<(String, Option<String>)>,
    custom_deny: Vec<(String, Option<String>)>,
    fallback: Decision,
}

impl Default for RulePolicy {
    fn default() -> Self {
        RulePolicy::new()
    }
}

/// Paths (as given by a script, before normalization) longer than this many
/// bytes are refused by the file system functions and imports: no usable
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

/// Normalizes a path lexically (resolving `.` and `..`) and resolves it on
/// the host's file system like `realpath` does for the longest prefix that
/// exists: symlinks are followed, and on macOS existing components are
/// named as stored (case- and normalization-insensitive volumes accept
/// other spellings) and through their firmlinks
/// (`/System/Volumes/Data/Users` is `/Users`). A symlink whose target does
/// not resolve is kept, with the rest of the path.
///
/// Without the `os` feature there is no host file system: paths are only
/// normalized lexically.
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

/// At most this many entries of a directory are read to find the stored
/// name of an entry in it when it cannot be had otherwise (a symlink's;
/// on macOS, that of a file that cannot be opened for reading or of a
/// device); in a larger directory, the entry keeps the name it was given.
/// Deny rules match every spelling of a name anyway; allow rules then need
/// the name as given.
pub const MAX_LINK_SCAN: usize = 4096;

fn under(path: &Path, roots: &[PathRule], space: PathSpace) -> bool {
    match space {
        PathSpace::Host => roots.iter().any(|r| path_starts_with(path, &r.host)),
        // Memory file systems are case sensitive everywhere.
        PathSpace::Memory => roots.iter().any(|r| path.starts_with(&r.memory)),
    }
}

/// Whether `path` is at or below a deny rule. File systems may be case
/// insensitive (Windows, macOS, case-folding directories on Linux, network
/// shares) and normalization insensitive (APFS, HFS+): `data/ſecrets`,
/// `data/SECRETS` and `data/secrets` may name the same entry, as may `café`
/// spelled with a precomposed or a combining accent. Existing paths are
/// canonicalised to their stored spelling, but a path that does not exist
/// yet keeps the spelling it was given in (and could create the denied
/// entry). So on every platform deny rules compare paths by [`fold_name`],
/// which equates every such spelling. Allow rules stay exact, so this only
/// ever denies more.
fn under_deny(path: &Path, roots: &[PathRule], space: PathSpace) -> bool {
    if roots.is_empty() {
        return false;
    }
    let key = deny_key(path);
    roots.iter().any(|r| key.starts_with(r.key(space)))
}

/// `p` with every component folded by [`fold_name`].
fn deny_key(p: &Path) -> PathBuf {
    p.components().map(|c| std::ffi::OsString::from(fold_name(&c.as_os_str().to_string_lossy()))).collect()
}

/// A caseless, normalization-insensitive form of a file name, coarser than
/// the comparisons of case- and normalization-insensitive file systems:
/// canonical decomposition (NFD), case folding (lower case of the upper case,
/// twice, which also folds `ſ`, `ς`, `ß`, `ẞ`, ligatures and the Kelvin
/// sign), NFD again, without default-ignorable characters (which HFS+
/// ignores). Two names a file system may treat as the same entry have the
/// same form.
pub(crate) fn fold_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    if name.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) {
        return name.to_string();
    }
    let mut s: String = name.nfd().collect();
    for _ in 0..2 {
        s = s.to_uppercase().to_lowercase();
    }
    s.nfd().filter(|&c| !is_default_ignorable(c)).collect()
}

/// Unicode's Default_Ignorable_Code_Point characters (zero-width and
/// formatting characters, variation selectors, ...).
fn is_default_ignorable(c: char) -> bool {
    matches!(c as u32,
        0xAD | 0x34F | 0x61C | 0x115F..=0x1160 | 0x17B4..=0x17B5 | 0x180B..=0x180F
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
        | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFF8 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0000..=0xE0FFF)
}

/// `Path::starts_with`, ignoring case on Windows, whose file systems are case
/// insensitive (paths that do not exist yet keep the case they were given in).
fn path_starts_with(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        fold(path).starts_with(fold(root))
    } else {
        path.starts_with(root)
    }
}

/// A network pattern of [`RulePolicy::allow_net`] / [`RulePolicy::deny_net`].
#[derive(Clone, Debug)]
struct NetPattern {
    kind: NetPatternKind,
    /// `None` matches any port.
    port: Option<u16>,
}

#[derive(Clone, Debug)]
enum NetPatternKind {
    /// `*`: any host.
    Any,
    /// `*.example.com`: strict subdomains.
    Suffix(String),
    /// An exact host name (lower case).
    Host(String),
    /// An IP address or CIDR range (`10.0.0.0/8`, `fc00::/7`).
    Ip(IpAddr, u8),
    /// A malformed pattern, which matches nothing.
    Invalid,
}

/// Whether `pattern` is a valid pattern for [`RulePolicy::allow_net`] /
/// [`RulePolicy::deny_net`] (invalid patterns are accepted there but match
/// nothing).
pub fn is_valid_net_pattern(pattern: &str) -> bool {
    NetPattern::parse(pattern).is_some()
}

/// A host name in canonical form (lower case, without a trailing dot), if
/// `name` is one.
fn host_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let valid = !name.is_empty() && !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    valid.then(|| name.to_ascii_lowercase())
}

impl NetPattern {
    /// Parses a pattern; an invalid one matches nothing.
    fn parse_lossy(pattern: &str) -> NetPattern {
        NetPattern::parse(pattern).unwrap_or(NetPattern { kind: NetPatternKind::Invalid, port: None })
    }

    fn parse(pattern: &str) -> Option<NetPattern> {
        // `[v6]`, `[v6/len]` or `[v6]/len`, each optionally with `:port`;
        // otherwise a port may follow a single colon.
        let (body, port): (String, Option<&str>) = if let Some(rest) = pattern.strip_prefix('[') {
            let (inner, after) = rest.split_once(']')?;
            let (len, port) = match after.strip_prefix('/') {
                Some(len_port) => match len_port.split_once(':') {
                    Some((len, port)) => (Some(len), Some(port)),
                    None => (Some(len_port), None),
                },
                None if after.is_empty() => (None, None),
                None => (None, Some(after.strip_prefix(':')?)),
            };
            if !inner.contains(':') {
                return None;
            }
            (len.map_or_else(|| inner.to_string(), |len| format!("{inner}/{len}")), port)
        } else {
            match pattern.split_once(':') {
                Some((body, port)) if !port.contains(':') => (body.to_string(), Some(port)),
                _ => (pattern.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None => None,
        };
        let kind = if body == "*" {
            NetPatternKind::Any
        } else if let Some(suffix) = body.strip_prefix("*.") {
            NetPatternKind::Suffix(host_name(suffix)?)
        } else if let Some((ip, len)) = body.split_once('/') {
            let ip: IpAddr = ip.parse().ok()?;
            let len: u8 = len.parse().ok()?;
            match ip {
                // Ranges of IPv4-mapped addresses are IPv4 ranges.
                IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() && len >= 96 => NetPatternKind::Ip(ip.to_canonical(), len - 96),
                IpAddr::V4(_) if len <= 32 => NetPatternKind::Ip(ip, len),
                IpAddr::V6(_) if len <= 128 => NetPatternKind::Ip(ip, len),
                _ => return None,
            }
        } else if let Some(ip) = body_ip(&body) {
            NetPatternKind::Ip(ip, if ip.is_ipv4() { 32 } else { 128 })
        } else {
            NetPatternKind::Host(host_name(&body)?)
        };
        Some(NetPattern { kind, port })
    }

    /// Whether the pattern names hosts or addresses explicitly (no wildcard).
    fn is_explicit(&self) -> bool {
        matches!(self.kind, NetPatternKind::Host(_) | NetPatternKind::Ip(..))
    }

    /// Matches the host name (or IP literal) of a connection request. A
    /// trailing dot (`example.com.`) does not change the name.
    fn matches_host(&self, host: &str, ip: Option<IpAddr>, port: u16) -> bool {
        if self.port.is_some_and(|p| p != port) {
            return false;
        }
        let name = host.strip_suffix('.').unwrap_or(host).as_bytes();
        match &self.kind {
            NetPatternKind::Any => true,
            NetPatternKind::Suffix(suffix) => {
                let suffix = suffix.as_bytes();
                name.len() > suffix.len() + 1
                    && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
                    && name[name.len() - suffix.len() - 1] == b'.'
            }
            NetPatternKind::Host(h) => h.as_bytes().eq_ignore_ascii_case(name),
            NetPatternKind::Ip(..) => ip.is_some_and(|ip| self.matches_ip(ip, port)),
            NetPatternKind::Invalid => false,
        }
    }

    /// Like [`NetPattern::matches_host`] for a deny rule: an IP or range
    /// pattern also matches the addresses an IP literal may reach (see
    /// [`NetPattern::denies_ip`]).
    fn denies_host(&self, host: &str, ip: Option<IpAddr>, port: u16) -> bool {
        match (&self.kind, ip) {
            (NetPatternKind::Ip(..), Some(ip)) => self.denies_ip(ip, port),
            _ => self.matches_host(host, ip, port),
        }
    }

    /// Like [`NetPattern::matches_ip`] for a deny rule, which must also
    /// catch other spellings of the addresses it covers: the unspecified
    /// address (`0.0.0.0`, `::`), which connects to the local host, matches
    /// as `127.0.0.1` and `::1`, and IPv6 addresses embedding an IPv4 address
    /// (IPv4-compatible, IPv4-translated, NAT64 `64:ff9b::/96`, 6to4
    /// `2002::/16`) also match as that address.
    fn denies_ip(&self, ip: IpAddr, port: u16) -> bool {
        let ip = ip.to_canonical();
        if self.matches_ip(ip, port) {
            return true;
        }
        if ip.is_unspecified() {
            return [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)].into_iter().any(|l| self.matches_ip(l, port));
        }
        match ip {
            IpAddr::V6(v6) => embedded_ipv4(v6).is_some_and(|v4| self.denies_ip(IpAddr::V4(v4), port)),
            IpAddr::V4(_) => false,
        }
    }

    /// Matches a resolved address; only IP and range patterns do.
    fn matches_ip(&self, ip: IpAddr, port: u16) -> bool {
        let NetPatternKind::Ip(net, len) = self.kind else { return false };
        if self.port.is_some_and(|p| p != port) {
            return false;
        }
        match (net, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => prefix_eq(&net.octets(), &ip.octets(), len),
            (IpAddr::V6(net), IpAddr::V6(ip)) => prefix_eq(&net.octets(), &ip.octets(), len),
            _ => false,
        }
    }
}

/// The IPv4 address embedded in an IPv6 address of a form that reaches it:
/// IPv4-compatible (`::a.b.c.d`, not `::` or `::1`), IPv4-translated
/// (`::ffff:0:a.b.c.d`), NAT64 (`64:ff9b::a.b.c.d`) or 6to4 (`2002:aabb:ccdd::`).
/// (IPv4-mapped addresses are converted by `to_canonical`.)
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let v4 = |hi: u16, lo: u16| Some(Ipv4Addr::from(((hi as u32) << 16) | lo as u32));
    match ip.segments() {
        [0, 0, 0, 0, 0, 0, 0, 0 | 1] => None,
        [0, 0, 0, 0, 0, 0, hi, lo] | [0, 0, 0, 0, 0xffff, 0, hi, lo] | [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => v4(hi, lo),
        [0x2002, hi, lo, ..] => v4(hi, lo),
        _ => None,
    }
}

/// Whether the first `len` bits of `a` and `b` agree.
fn prefix_eq(a: &[u8], b: &[u8], len: u8) -> bool {
    let (bytes, bits) = ((len / 8) as usize, len % 8);
    a[..bytes] == b[..bytes] && (bits == 0 || (a[bytes] ^ b[bytes]) >> (8 - bits) == 0)
}

/// Parses an IP literal (IPv6 optionally in brackets); IPv4-mapped IPv6
/// addresses become IPv4.
fn parse_ip(s: &str) -> Option<IpAddr> {
    body_ip(s.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(s))
}

fn body_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

fn is_localhost(host: &str) -> bool {
    host.strip_suffix('.').unwrap_or(host).eq_ignore_ascii_case("localhost")
}

/// Whether `ip` is not a public unicast internet address: loopback, private
/// (RFC 1918), shared/CGNAT (100.64/10), link-local, unique-local
/// (`fc00::/7`), site-local, unspecified, multicast, broadcast, documentation,
/// benchmarking and other reserved ranges. IPv6 forms that embed an IPv4
/// address (IPv4-mapped, IPv4-compatible, NAT64 `64:ff9b::/96`, 6to4
/// `2002::/16`) are classified by the embedded address.
pub fn is_private_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0                                      // "this network", unspecified
        || a == 10                              // RFC 1918
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || a == 127                             // loopback
        || (a == 169 && b == 254)               // link-local (incl. cloud metadata)
        || (a == 172 && (16..32).contains(&b))  // RFC 1918
        || (a == 192 && b == 0 && c == 0)       // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)       // documentation
        || (a == 192 && b == 88 && c == 99)     // 6to4 relay anycast
        || (a == 192 && b == 168)               // RFC 1918
        || (a == 198 && (18..20).contains(&b))  // benchmarking
        || (a == 198 && b == 51 && c == 100)    // documentation
        || (a == 203 && b == 0 && c == 113)     // documentation
        || a >= 224 // multicast, reserved, broadcast
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| is_private_v4(Ipv4Addr::from(((hi as u32) << 16) | lo as u32));
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_private_v4(v4);
    }
    match s {
        // Unspecified, loopback and IPv4-compatible addresses.
        [0, 0, 0, 0, 0, 0, _, _] => true,
        // IPv4-translated (RFC 6145): classified by the embedded IPv4 address.
        [0, 0, 0, 0, 0xffff, 0, hi, lo] => embedded(hi, lo),
        // NAT64 well-known prefix: classified by the embedded IPv4 address.
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => embedded(hi, lo),
        // Local-use NAT64, discard-only.
        [0x64, 0xff9b, 1, ..] | [0x100, 0, 0, 0, ..] => true,
        // 6to4: classified by the embedded IPv4 address.
        [0x2002, hi, lo, ..] => embedded(hi, lo),
        // IETF protocol assignments (incl. Teredo) and documentation.
        [0x2001, b, ..] if b < 0x200 => true,
        [0x2001, 0xdb8, ..] => true,
        [0x3fff, b, ..] if b < 0x1000 => true,
        // Unique local, link-local, site-local, multicast.
        [a, ..] => a & 0xfe00 == 0xfc00 || a & 0xffc0 == 0xfe80 || a & 0xffc0 == 0xfec0 || a & 0xff00 == 0xff00,
    }
}

fn listen_matches(pattern: &str, addr: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let (ph, pp) = pattern.rsplit_once(':').unwrap_or((pattern, "0"));
    let (ah, ap) = addr.rsplit_once(':').unwrap_or((addr, "0"));
    let host_ok = ph == "*"
        || ph.trim_matches(['[', ']']) == ah.trim_matches(['[', ']'])
        || (ph == "localhost" && (ah == "127.0.0.1" || ah == "::1"));
    host_ok && (pp == "0" || pp == "*" || pp == ap)
}

impl RulePolicy {
    /// Standard output and error are allowed; script imports are allowed
    /// (they are always confined to the module root); everything else falls
    /// back to `Deny`.
    pub fn new() -> RulePolicy {
        RulePolicy {
            read_allow: vec![],
            read_deny: vec![],
            write_allow: vec![],
            write_deny: vec![],
            net_allow: vec![],
            net_deny: vec![],
            deny_private: false,
            listen_allow: vec![],
            listen_deny: vec![],
            env_allow: vec![],
            env_deny: vec![],
            import_allow: None,
            import_deny: vec![],
            stdin: None,
            stdout: true,
            stderr: true,
            custom_allow: vec![],
            custom_deny: vec![],
            fallback: Decision::Deny,
        }
    }
    pub fn allow_read(mut self, p: impl AsRef<Path>) -> Self {
        self.read_allow.push(PathRule::new(p.as_ref()));
        self
    }
    pub fn deny_read(mut self, p: impl AsRef<Path>) -> Self {
        self.read_deny.push(PathRule::new(p.as_ref()));
        self
    }
    /// Allows writing (and reading) under `p`.
    pub fn allow_write(mut self, p: impl AsRef<Path>) -> Self {
        let n = PathRule::new(p.as_ref());
        self.write_allow.push(n.clone());
        self.read_allow.push(n);
        self
    }
    pub fn deny_write(mut self, p: impl AsRef<Path>) -> Self {
        self.write_deny.push(PathRule::new(p.as_ref()));
        self
    }
    /// Allows outgoing connections. Patterns: `host`, `*.example.com`, `*`,
    /// an IP address (`127.0.0.1`, `::1`) or a CIDR range (`10.0.0.0/8`,
    /// `fc00::/7`), each optionally with a port (`host:443`, `[::1]:8080`,
    /// `10.0.0.0/8:443`, `[fc00::/7]:443`). IP and range patterns match IP
    /// literals used by the script and, after name resolution, exempt
    /// matching addresses from [`RulePolicy::deny_private_networks`]. Host
    /// names are compared case-insensitively, ignoring a trailing dot. An
    /// invalid pattern (see [`is_valid_net_pattern`]) matches nothing.
    pub fn allow_net(mut self, pattern: &str) -> Self {
        self.net_allow.push(NetPattern::parse_lossy(pattern));
        self
    }
    /// Denies outgoing connections (same patterns as [`RulePolicy::allow_net`]).
    /// IP and range patterns also apply to the addresses host names resolve to.
    pub fn deny_net(mut self, pattern: &str) -> Self {
        self.net_deny.push(NetPattern::parse_lossy(pattern));
        self
    }
    /// Denies connections to private and internal addresses (see
    /// [`is_private_address`]), whether named directly or reached by
    /// resolving a host name. Exempt are IP literals and `localhost` allowed
    /// by an explicit (non-wildcard) pattern or by a prompt answer, and
    /// resolved addresses matching an IP or range `allow_net` pattern.
    pub fn deny_private_networks(mut self) -> Self {
        self.deny_private = true;
        self
    }
    /// `host:port`; port `0` allows any port.
    pub fn allow_listen(mut self, addr: &str) -> Self {
        self.listen_allow.push(addr.to_string());
        self
    }
    /// Denies listening on matching addresses (patterns as for
    /// [`RulePolicy::allow_listen`]).
    pub fn deny_listen(mut self, addr: &str) -> Self {
        self.listen_deny.push(addr.to_string());
        self
    }
    pub fn allow_env(mut self, name: &str) -> Self {
        self.env_allow.push(name.to_string());
        self
    }
    /// Denies reading the variable `name` (`"*"`: every variable). Listing
    /// all variables (`Env("*")`) is denied while any variable is.
    pub fn deny_env(mut self, name: &str) -> Self {
        self.env_deny.push(name.to_string());
        self
    }
    pub fn allow_stdin(mut self) -> Self {
        self.stdin = Some(true);
        self
    }
    pub fn deny_stdin(mut self) -> Self {
        self.stdin = Some(false);
        self
    }
    pub fn deny_stdout(mut self) -> Self {
        self.stdout = false;
        self
    }
    pub fn deny_stderr(mut self) -> Self {
        self.stderr = false;
        self
    }
    /// Restricts imports to the given directories.
    pub fn allow_import(mut self, p: impl AsRef<Path>) -> Self {
        self.import_allow.get_or_insert_with(Vec::new).push(PathRule::new(p.as_ref()));
        self
    }
    /// Denies importing scripts under `p`.
    pub fn deny_import(mut self, p: impl AsRef<Path>) -> Self {
        self.import_deny.push(PathRule::new(p.as_ref()));
        self
    }
    /// Allows a custom capability (`detail = None` allows any detail).
    pub fn allow_custom(mut self, name: &str, detail: Option<&str>) -> Self {
        self.custom_allow.push((name.to_string(), detail.map(String::from)));
        self
    }
    /// Denies a custom capability (`detail = None` denies any detail).
    pub fn deny_custom(mut self, name: &str, detail: Option<&str>) -> Self {
        self.custom_deny.push((name.to_string(), detail.map(String::from)));
        self
    }
    pub fn fallback(mut self, d: Decision) -> Self {
        self.fallback = d;
        self
    }

    /// See [`Policy::check_entry`].
    pub fn check_entry(&self, cap: &Capability) -> Decision {
        self.check_entry_in(cap, PathSpace::Host)
    }

    fn check_entry_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match cap {
            Capability::FsRead(p) => self.check_path(&self.entry_path(p, space), false, space),
            Capability::FsWrite(p) => self.check_path(&self.entry_path(p, space), true, space),
            _ => self.check_in(cap, space),
        }
    }

    /// See [`Policy::denies_under`]: whether a read or write deny rule lies
    /// at or below `path` (a final symlink is not resolved).
    pub fn denies_under(&self, path: &Path) -> bool {
        self.denies_under_normalized_in(&self.entry_path(path, PathSpace::Host), PathSpace::Host)
    }

    fn denies_under_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        let key = deny_key(path);
        self.read_deny.iter().chain(&self.write_deny).any(|rule| rule.key(space).starts_with(&key))
    }

    /// The path of the entry `p` itself, normalized in `space`.
    fn entry_path(&self, p: &Path, space: PathSpace) -> PathBuf {
        match space {
            PathSpace::Host => normalize_entry_path(p),
            PathSpace::Memory => crate::files::memory_path(p),
        }
    }

    /// Decides access to a normalized path.
    fn check_path(&self, p: &Path, write: bool, space: PathSpace) -> Decision {
        let allow_or = |b: bool| if b { Decision::Allow } else { self.fallback };
        if write {
            if under_deny(p, &self.write_deny, space) || under_deny(p, &self.read_deny, space) {
                Decision::Deny
            } else {
                allow_or(under(p, &self.write_allow, space))
            }
        } else if under_deny(p, &self.read_deny, space) {
            Decision::Deny
        } else {
            allow_or(under(p, &self.read_allow, space))
        }
    }

    /// [`Policy::check_normalized_in`].
    fn check_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match cap {
            Capability::FsRead(p) => self.check_path(p, false, space),
            Capability::FsWrite(p) => self.check_path(p, true, space),
            Capability::Import(p) => self.check_import(p, space),
            _ => self.check_in(cap, space),
        }
    }

    /// Decides importing the module at the normalized path `p`.
    fn check_import(&self, p: &Path, space: PathSpace) -> Decision {
        // Importing reads the module: read deny rules apply.
        if under_deny(p, &self.read_deny, space) || under_deny(p, &self.import_deny, space) {
            return Decision::Deny;
        }
        match &self.import_allow {
            None => Decision::Allow,
            Some(roots) if under(p, roots, space) => Decision::Allow,
            Some(_) => self.fallback,
        }
    }

    /// The allow rules that decide `cap` (none for other capabilities).
    fn allow_rules(&self, cap: &Capability) -> &[PathRule] {
        match cap {
            Capability::FsRead(_) => &self.read_allow,
            Capability::FsWrite(_) => &self.write_allow,
            Capability::Import(_) => self.import_allow.as_deref().unwrap_or_default(),
            _ => &[],
        }
    }

    pub fn check(&self, cap: &Capability) -> Decision {
        self.check_in(cap, PathSpace::Host)
    }

    fn check_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        // Paths as the policy compares them in `space`.
        let normal = |p: &Path| match space {
            PathSpace::Host => normalize_path(p),
            PathSpace::Memory => crate::files::memory_path(p),
        };
        let allow_or = |b: bool| if b { Decision::Allow } else { self.fallback };
        match cap {
            Capability::Stdout => {
                if self.stdout {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Capability::Stderr => {
                if self.stderr {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Capability::Stdin => match self.stdin {
                Some(true) => Decision::Allow,
                Some(false) => Decision::Deny,
                None => self.fallback,
            },
            Capability::FsRead(p) => self.check_path(&normal(p), false, space),
            Capability::FsWrite(p) => self.check_path(&normal(p), true, space),
            Capability::Net { host, port } => {
                let ip = parse_ip(host);
                let host = host.trim_start_matches('[').trim_end_matches(']');
                if self.net_deny.iter().any(|pat| pat.denies_host(host, ip, *port)) {
                    return Decision::Deny;
                }
                // Internal destinations named directly need an explicit rule (or a prompt).
                let internal = self.deny_private && (ip.is_some_and(is_private_address) || is_localhost(host));
                allow_or(self.net_allow.iter().any(|pat| (!internal || pat.is_explicit()) && pat.matches_host(host, ip, *port)))
            }
            Capability::NetAddr { host, addr } => {
                let ip = addr.ip().to_canonical();
                // An IP literal was fully decided as `Net`, and resolves only to itself.
                if let Some(literal) = parse_ip(host) {
                    return if literal == ip { Decision::Allow } else { Decision::Deny };
                }
                if self.net_deny.iter().any(|pat| pat.denies_ip(ip, addr.port())) {
                    return Decision::Deny;
                }
                let allowed = !self.deny_private
                    || !is_private_address(ip)
                    || (is_localhost(host) && ip.is_loopback())
                    || self.net_allow.iter().any(|pat| pat.matches_ip(ip, addr.port()));
                if allowed { Decision::Allow } else { Decision::Deny }
            }
            Capability::NetListen(addr) => {
                if self.listen_deny.iter().any(|pat| listen_matches(pat, addr)) {
                    return Decision::Deny;
                }
                allow_or(self.listen_allow.iter().any(|pat| listen_matches(pat, addr)))
            }
            Capability::Env(name) => {
                // All variables cannot be had while some are denied.
                if self.env_deny.iter().any(|n| n == name || n == "*") || (name == "*" && !self.env_deny.is_empty()) {
                    return Decision::Deny;
                }
                allow_or(self.env_allow.iter().any(|n| n == name || n == "*"))
            }
            Capability::Import(p) => self.check_import(&normal(p), space),
            Capability::Custom { name, detail } => {
                let matches = |(n, d): &(String, Option<String>)| n == name && d.as_ref().is_none_or(|d| d == detail);
                if self.custom_deny.iter().any(matches) {
                    return Decision::Deny;
                }
                allow_or(self.custom_allow.iter().any(matches))
            }
        }
    }
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
        // Very long paths take few calls (this used to take seconds).
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
}
