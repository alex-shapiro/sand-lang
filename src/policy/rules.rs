//! Rule policies: allow and deny rules, and the indices that decide
//! requests by them.
//!
//! The most specific matching rule decides a request; between equally
//! specific rules deny wins. Rules are indexed by what they match (path
//! names in a trie, hosts in hash tables), so a check costs about the same
//! whatever the number of rules.

use super::net::{
    Dest, HostIndex, HostKind, ListenPattern, NetPattern, UrlPattern, is_internal, is_localhost, is_private_address, parse_ip, url_segments,
};
use super::path::{Glob, Seg, Trie, path_segments};
use super::{Capability, Decision, PathSpace, fold_name, lexical, normalize_path};
use crate::files::Mode;
use crate::vm::value::FxHashMap;
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The file accesses a rule names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Access {
    Read,
    Write,
    Import,
    /// Reading and writing (allow), or all access (deny).
    Any,
}

pub(crate) const READ: u8 = 1;
pub(crate) const WRITE: u8 = 2;
pub(crate) const IMPORT: u8 = 4;

impl Access {
    /// The requests a rule decides: writing implies reading, and what may
    /// not be read may not be written or imported either.
    pub(crate) fn mask(self, allow: bool) -> u8 {
        match (self, allow) {
            (Access::Read, true) => READ,
            (Access::Write | Access::Any, true) => READ | WRITE,
            (Access::Read | Access::Any, false) => READ | WRITE | IMPORT,
            (Access::Write, false) => WRITE,
            (Access::Import, _) => IMPORT,
        }
    }
}

/// A file path pattern: an absolute path, then segments that start with a
/// wildcard (no trailing `**`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PathPattern {
    pub prefix: PathBuf,
    pub rest: Vec<Seg>,
}

/// An environment variable name pattern (`HOME`, `AWS_*`, `*`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NamePattern {
    Exact(String),
    Glob(Glob),
    Any,
}

impl NamePattern {
    pub(crate) fn parse(name: &str) -> NamePattern {
        if name == "*" {
            return NamePattern::Any;
        }
        match Seg::parse(&name.chars().map(|c| (c, c == '*')).collect::<Vec<_>>()) {
            Ok(Seg::Glob(g)) => NamePattern::Glob(g),
            Ok(Seg::Star | Seg::GlobStar) => NamePattern::Any,
            _ => NamePattern::Exact(name.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RuleKind {
    Files(Access, PathPattern),
    Net(NetPattern),
    /// Methods (upper case; `None`: any) and a URL pattern.
    Http(Option<Vec<String>>, UrlPattern),
    Listen(ListenPattern),
    Env(NamePattern),
    Stdin,
    Stdout,
    Stderr,
    TimeZone,
    Custom(String, Option<String>),
}

impl RuleKind {
    /// What the rule matches in http requests. A net rule matches them as
    /// an http rule for its host with any method, scheme and path.
    pub(crate) fn http_match(&self) -> Option<HttpMatch<'_>> {
        match self {
            RuleKind::Http(methods, url) => {
                Some(HttpMatch { methods: methods.as_deref(), scheme: url.scheme, host: &url.host, path: &url.path })
            }
            RuleKind::Net(host) => Some(HttpMatch { methods: None, scheme: None, host, path: &[] }),
            _ => None,
        }
    }

    /// The connections the rule decides: a net rule decides connections to
    /// the hosts it names, and an address pattern also connections to the
    /// addresses that host names resolve to.
    pub(crate) fn connections(&self) -> &'static [Connection] {
        match self {
            RuleKind::Net(NetPattern { kind: HostKind::Ip(..), .. }) => &[Connection::Named, Connection::Resolved],
            RuleKind::Net(_) => &[Connection::Named],
            _ => &[],
        }
    }
}

/// What an http or net rule matches in http requests.
#[derive(Clone, Copy)]
pub(crate) struct HttpMatch<'a> {
    /// Upper case; `None`: any.
    pub methods: Option<&'a [String]>,
    pub scheme: Option<&'static str>,
    pub host: &'a NetPattern,
    pub path: &'a [Seg],
}

/// A connection that net rules decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Connection {
    /// To a host or address as a script names it ([`Capability::Net`]).
    Named,
    /// To an address that a host name resolves to ([`Capability::NetAddr`]).
    Resolved,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub allow: bool,
    pub kind: RuleKind,
    pub origin: Option<Origin>,
}

/// Where a rule was written: the line and column (from 1) of its pattern,
/// and the pattern as written.
#[derive(Clone, Debug)]
pub(crate) struct Origin {
    pub line: usize,
    pub column: usize,
    pub text: String,
}

/// A rule-based policy: the most specific rule matching a request decides
/// it (see SPEC §14.5), deny at a tie; requests matching no rule get the
/// fallback decision.
#[derive(Clone)]
pub struct RulePolicy {
    rules: Vec<Rule>,
    deny_private: bool,
    fallback: Decision,
    host_files: OnceLock<Arc<FileIndex>>,
    memory_files: OnceLock<Arc<FileIndex>>,
    others: OnceLock<Arc<Index>>,
}

impl fmt::Debug for RulePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RulePolicy")
            .field("rules", &self.rules.len())
            .field("deny_private", &self.deny_private)
            .field("fallback", &self.fallback)
            .finish()
    }
}

impl Default for RulePolicy {
    fn default() -> Self {
        RulePolicy::new()
    }
}

fn net_pattern(pattern: &str) -> NetPattern {
    NetPattern::parse(pattern).unwrap_or_else(|e| panic!("{e}"))
}

fn listen_pattern(pattern: &str) -> ListenPattern {
    ListenPattern::parse(pattern).unwrap_or_else(|| panic!("invalid listen pattern {pattern:?}"))
}

/// The paths of rules are taken literally (no wildcards) and cover what is
/// below them; policy files ([`super::PolicyFile`]) write patterns.
impl RulePolicy {
    /// No rules: standard output and error and script imports (confined to
    /// the module root) are allowed, everything else falls back to `Deny`.
    pub fn new() -> RulePolicy {
        RulePolicy::from_rules(vec![])
    }

    pub(crate) fn from_rules(rules: Vec<Rule>) -> RulePolicy {
        RulePolicy {
            rules,
            deny_private: false,
            fallback: Decision::Deny,
            host_files: OnceLock::new(),
            memory_files: OnceLock::new(),
            others: OnceLock::new(),
        }
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub(crate) fn push(mut self, allow: bool, kind: RuleKind) -> Self {
        self.rules.push(Rule { allow, kind, origin: None });
        self.host_files = OnceLock::new();
        self.memory_files = OnceLock::new();
        self.others = OnceLock::new();
        self
    }

    pub(crate) fn extend(mut self, rules: Vec<Rule>) -> Self {
        for r in rules {
            self = self.push(r.allow, r.kind);
            if let Some(last) = self.rules.last_mut() {
                last.origin = r.origin;
            }
        }
        self
    }

    fn files_rule(self, allow: bool, access: Access, p: &Path) -> Self {
        self.push(allow, RuleKind::Files(access, PathPattern { prefix: p.to_path_buf(), rest: vec![] }))
    }

    pub fn allow_read(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(true, Access::Read, p.as_ref())
    }
    /// Denies reading, writing and importing under `p`.
    pub fn deny_read(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(false, Access::Read, p.as_ref())
    }
    /// Allows writing (and reading) under `p`.
    pub fn allow_write(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(true, Access::Write, p.as_ref())
    }
    pub fn deny_write(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(false, Access::Write, p.as_ref())
    }
    /// Restricts script imports to the directories of `allow_import` rules.
    pub fn allow_import(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(true, Access::Import, p.as_ref())
    }
    pub fn deny_import(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(false, Access::Import, p.as_ref())
    }
    /// Allows outgoing connections. Patterns: `host`, `*.example.com`
    /// (strict subdomains), `*`, an IP address (`127.0.0.1`, `::1`) or a
    /// CIDR range (`10.0.0.0/8`, `fc00::/7`), each optionally with a port
    /// (`host:443`, `[::1]:8080`, `10.0.0.0/8:443`, `[fc00::/7]:443`).
    ///
    /// # Panics
    ///
    /// If the pattern is invalid (see [`super::check_net_pattern`]).
    pub fn allow_net(self, pattern: &str) -> Self {
        self.push(true, RuleKind::Net(net_pattern(pattern)))
    }
    /// Denies outgoing connections ([`RulePolicy::allow_net`]'s patterns).
    pub fn deny_net(self, pattern: &str) -> Self {
        self.push(false, RuleKind::Net(net_pattern(pattern)))
    }
    /// Allows HTTP requests: optional methods, then a URL pattern, as in a
    /// policy file's `allow http` rule (`"GET POST api.example.com/v1/**"`).
    ///
    /// # Panics
    ///
    /// If the rule is invalid.
    pub fn allow_http(self, rule: &str) -> Self {
        self.http(true, rule)
    }
    pub fn deny_http(self, rule: &str) -> Self {
        self.http(false, rule)
    }
    fn http(mut self, allow: bool, rule: &str) -> Self {
        let verb = if allow { "allow" } else { "deny" };
        let file = super::PolicyFile::parse(&format!("{verb} http {rule}"), Path::new("/"), None)
            .unwrap_or_else(|e| panic!("invalid http rule {rule:?}: {}", e[0].message));
        for r in file.rules.rules {
            self = self.push(r.allow, r.kind);
        }
        self
    }
    /// Denies connections to private and internal addresses (see
    /// [`super::is_private_address`]), whether named directly or reached by
    /// resolving a host name. Exempt are IP literals and `localhost` allowed
    /// by an explicit (non-wildcard) pattern or by a prompt answer, and
    /// resolved addresses matching an IP or range `allow_net` pattern.
    pub fn deny_private_networks(mut self) -> Self {
        self.deny_private = true;
        self
    }
    /// Allows binding a listener matching `pattern` (`ip:port`, `[ipv6]:port`,
    /// `ip`, `localhost`, `*:port`, or `*`). Host `*` matches any address, port
    /// `*` matches any port, and port `0` matches only an ephemeral port
    /// request.
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is invalid.
    pub fn allow_listen(self, pattern: &str) -> Self {
        self.push(true, RuleKind::Listen(listen_pattern(pattern)))
    }
    /// Denies binding a listener matching `pattern` (see [`RulePolicy::allow_listen`]).
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is invalid.
    pub fn deny_listen(self, pattern: &str) -> Self {
        self.push(false, RuleKind::Listen(listen_pattern(pattern)))
    }
    /// Allows reading environment variables: a name, a pattern with `*`
    /// (`AWS_*`), or `*` for all of them.
    pub fn allow_env(self, name: &str) -> Self {
        self.push(true, RuleKind::Env(NamePattern::parse(name)))
    }
    /// Denies reading matching variables. Listing all variables
    /// (`Env("*")`) is denied while any variable is.
    pub fn deny_env(self, name: &str) -> Self {
        self.push(false, RuleKind::Env(NamePattern::parse(name)))
    }
    pub fn allow_stdin(self) -> Self {
        self.push(true, RuleKind::Stdin)
    }
    pub fn deny_stdin(self) -> Self {
        self.push(false, RuleKind::Stdin)
    }
    pub fn allow_timezone(self) -> Self {
        self.push(true, RuleKind::TimeZone)
    }
    pub fn deny_timezone(self) -> Self {
        self.push(false, RuleKind::TimeZone)
    }
    pub fn deny_stdout(self) -> Self {
        self.push(false, RuleKind::Stdout)
    }
    pub fn deny_stderr(self) -> Self {
        self.push(false, RuleKind::Stderr)
    }
    /// Allows a custom capability (`detail = None` allows any detail).
    pub fn allow_custom(self, name: &str, detail: Option<&str>) -> Self {
        self.push(true, RuleKind::Custom(name.to_string(), detail.map(String::from)))
    }
    pub fn deny_custom(self, name: &str, detail: Option<&str>) -> Self {
        self.push(false, RuleKind::Custom(name.to_string(), detail.map(String::from)))
    }
    pub fn fallback(mut self, d: Decision) -> Self {
        self.fallback = d;
        self
    }

    pub fn check(&self, cap: &Capability) -> Decision {
        self.check_in(cap, PathSpace::Host)
    }

    /// See [`super::Policy::check_entry`].
    pub fn check_entry(&self, cap: &Capability) -> Decision {
        self.check_entry_in(cap, PathSpace::Host)
    }

    /// See [`super::Policy::denies_under`].
    pub fn denies_under(&self, path: &Path) -> bool {
        self.denies_under_normalized_in(&entry_path(path, PathSpace::Host), PathSpace::Host)
    }

    pub(crate) fn check_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match cap {
            Capability::FsRead(p) | Capability::FsWrite(p) | Capability::Import(p) => self.check_path(cap, p, Mode::Follow, space),
            _ => self.check_normalized_in(cap, space),
        }
    }

    pub(crate) fn check_entry_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        match cap {
            Capability::FsRead(p) | Capability::FsWrite(p) => self.check_path(cap, p, Mode::Entry, space),
            _ => self.check_in(cap, space),
        }
    }

    fn check_path(&self, cap: &Capability, p: &Path, mode: Mode, space: PathSpace) -> Decision {
        let with = |p: PathBuf| match cap {
            Capability::FsRead(_) => Capability::FsRead(p),
            Capability::FsWrite(_) => Capability::FsWrite(p),
            _ => Capability::Import(p),
        };
        #[cfg(feature = "os")]
        if space == PathSpace::Host {
            let mut t = crate::files::Fs::Host.target(&lexical(p), mode);
            if !t.exact() {
                if let Some(d) = self.spelling_free_decision(&with(t.path.clone()), space) {
                    return d;
                }
                t.fix_spelling();
            }
            return self.check_normalized_in(&with(t.path), space);
        }
        let _ = mode;
        let normal = match space {
            PathSpace::Host => lexical(p),
            PathSpace::Memory => crate::files::memory_path(p),
        };
        self.check_normalized_in(&with(normal), space)
    }

    /// Decides `cap`, whose path (if any) is normalized in `space`.
    pub(crate) fn check_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        if let Capability::NetAddr { host, addr } = cap {
            return self.check_addr(host, *addr);
        }
        match self.verdict(cap, space, &[]) {
            Some((true, _)) => Decision::Allow,
            Some((false, _)) => Decision::Deny,
            None => match cap {
                Capability::Stdout | Capability::Stderr => Decision::Allow,
                Capability::Import(_) if !self.others().imports_restricted => Decision::Allow,
                // Only an explicit rule or a prompt answer allows an internal destination.
                Capability::Net { host, port } | Capability::Http { host, port, .. }
                    if self.fallback == Decision::Allow && self.deny_private && is_internal(&Dest::new(host, *port)) =>
                {
                    Decision::Deny
                }
                _ => self.fallback,
            },
        }
    }

    pub(crate) fn denies_under_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        let index = self.files(space);
        !index.deny.is_empty() && index.deny.any_below(&refs(&folded(&path_segments(path))), READ | WRITE)
    }

    /// See [`super::Policy::protects_normalized_in`].
    pub(crate) fn protects_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        [READ, WRITE].into_iter().any(|op| {
            let mut best = Best::new(&[]);
            self.file_verdict(path, op, space, &mut best);
            matches!(best.verdict(), Some((false, _)))
        })
    }

    /// The decision on a file capability normalized but for the spelling of
    /// its final name, if it is the same for every spelling (see
    /// [`super::Policy::decision_for_any_spelling`]).
    pub(crate) fn decision_for_any_spelling(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        self.spelling_free_decision(cap, space).filter(|&d| d != Decision::Prompt)
    }

    fn spelling_free_decision(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        let (Capability::FsRead(p) | Capability::FsWrite(p) | Capability::Import(p)) = cap else { return None };
        let index = self.files(space);
        // Deny rules compare case-folded names.
        if index.allow.matches_last(&refs(&index.allow_input(&path_segments(p)))) {
            return None;
        }
        Some(self.check_normalized_in(cap, space))
    }

    /// The rule deciding `cap` (normalized in `space`), leaving out the
    /// rules `exclude`: whether it allows, and its index. `None` if no rule
    /// matches.
    pub(crate) fn verdict(&self, cap: &Capability, space: PathSpace, exclude: &[usize]) -> Option<(bool, usize)> {
        let mut best = Best::new(exclude);
        let index = self.others();
        match cap {
            Capability::FsRead(p) => self.file_verdict(p, READ, space, &mut best),
            Capability::FsWrite(p) => self.file_verdict(p, WRITE, space, &mut best),
            Capability::Import(p) => self.file_verdict(p, IMPORT, space, &mut best),
            Capability::Net { host, port } => self.net_verdict(&Dest::new(host, *port), &mut best),
            Capability::NetAddr { .. } => {}
            Capability::Http { method, scheme, host, port, path } => {
                let dest = Dest::new(host, *port);
                let (names, alt) = url_segments(path);
                self.http_verdict(&dest, method, scheme, &names, &mut best);
                let Some(alt) = alt else { return best.verdict() };
                // A server may split at decoded separators: both readings
                // must be allowed.
                let mut other = Best::new(exclude);
                self.http_verdict(&dest, method, scheme, &alt, &mut other);
                return match (best.verdict(), other.verdict()) {
                    (Some((false, r)), _) | (_, Some((false, r))) => Some((false, r)),
                    (Some((true, r)), Some((true, _))) => Some((true, r)),
                    _ => None,
                };
            }
            Capability::NetListen(addr) => {
                for &(ref pattern, allow, rule) in &index.listen {
                    if let Some(score) = pattern.matches(*addr) {
                        best.offer([score, 0, 0, 0], allow, rule);
                    }
                }
            }
            Capability::Env(name) if name == "*" => {
                // All variables cannot be had while some are denied.
                let named = index.env_names.values().flatten().map(|&(allow, rule)| (allow, rule));
                let denies = named.chain(index.env.iter().map(|&(_, allow, rule)| (allow, rule)));
                if let Some((_, rule)) = denies.filter(|&(allow, rule)| !allow && !exclude.contains(&(rule as usize))).min_by_key(|e| e.1) {
                    return Some((false, rule as usize));
                }
                for &(ref pattern, allow, rule) in &index.env {
                    if *pattern == NamePattern::Any {
                        best.offer([0; 4], allow, rule);
                    }
                }
            }
            Capability::Env(name) => {
                for &(allow, rule) in index.env_names.get(name.as_str()).into_iter().flatten() {
                    best.offer([2, 0, 0, 0], allow, rule);
                }
                for (pattern, allow, rule) in &index.env {
                    let spec = match pattern {
                        NamePattern::Glob(g) if g.matches(name) => [1, g.pieces().iter().map(|p| p.len() as u32).sum(), 0, 0],
                        NamePattern::Any => [0; 4],
                        _ => continue,
                    };
                    best.offer(spec, *allow, *rule);
                }
            }
            Capability::Stdin | Capability::Stdout | Capability::Stderr | Capability::TimeZone => {
                for &(ref kind, allow, rule) in &index.unit {
                    if std::mem::discriminant(kind) == std::mem::discriminant(cap) {
                        best.offer([0; 4], allow, rule);
                    }
                }
            }
            Capability::Custom { name, detail } => {
                for &(ref d, allow, rule) in index.custom.get(name.as_str()).into_iter().flatten() {
                    if d.as_ref().is_none_or(|d| d == detail) {
                        best.offer([d.is_some() as u32, 0, 0, 0], allow, rule);
                    }
                }
            }
        }
        best.verdict()
    }

    fn file_verdict(&self, p: &Path, op: u8, space: PathSpace, best: &mut Best<'_>) {
        let index = self.files(space);
        let mut offer = |e: &FileEntry, depth: usize, allow: bool| {
            if e.mask & op != 0 {
                best.offer(e.rank.spec(depth), allow, e.rule);
            }
        };
        let names = path_segments(p);
        if !index.allow.is_empty() {
            index.allow.matches(&refs(&index.allow_input(&names)), |e, depth| offer(e, depth, true));
        }
        if !index.deny.is_empty() {
            index.deny.matches(&refs(&folded(&names)), |e, depth| offer(e, depth, false));
        }
    }

    /// Offers the net rules matching `dest`. Unless `allow private-net`,
    /// internal destinations named directly need an explicit allow rule.
    fn net_verdict(&self, dest: &Dest, best: &mut Best<'_>) {
        let internal = self.deny_private && is_internal(dest);
        self.others().named.find(dest, |explicit, spelled, e| {
            if !e.allow || allow_applies(explicit, spelled, internal) {
                best.offer(e.rank.spec(0), e.allow, e.rule);
            }
        });
    }

    /// Offers the http and net rules matching a request (see
    /// [`RuleKind::http_match`]).
    fn http_verdict(&self, dest: &Dest, method: &str, scheme: &str, names: &[String], best: &mut Best<'_>) {
        let index = self.others();
        if index.groups.is_empty() {
            return;
        }
        let internal = self.deny_private && is_internal(dest);
        let method = method.to_ascii_uppercase();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut folded_names = None;
        index.http.find(dest, |explicit, spelled, &group| {
            let group = &index.groups[group as usize];
            let mut offer = |e: &HttpEntry, depth: usize, allow: bool| {
                if method_matches(e.methods.as_deref(), &method) && scheme_matches(e.scheme, scheme) {
                    best.offer(e.rank.spec(depth), allow, e.rule);
                }
            };
            if !group.allow.is_empty() && allow_applies(explicit, spelled, internal) {
                group.allow.matches(&names, |e, depth| offer(e, depth, true));
            }
            if !group.deny.is_empty() {
                let folded = folded_names.get_or_insert_with(|| names.iter().map(|n| fold_name(n)).collect::<Vec<_>>());
                group.deny.matches(&refs(folded), |e, depth| offer(e, depth, false));
            }
        });
    }

    /// Decides connecting to `addr`, an address `host` resolved to.
    fn check_addr(&self, host: &str, addr: std::net::SocketAddr) -> Decision {
        let ip = addr.ip().to_canonical();
        // An IP literal was fully decided as `Net`, and resolves only to itself.
        if let Some(literal) = parse_ip(host) {
            return if literal == ip { Decision::Allow } else { Decision::Deny };
        }
        let mut best = Best::new(&[]);
        self.others().resolved.find_ip(ip, addr.port(), |spelled, e| {
            if !e.allow || allow_applies(true, spelled, false) {
                best.offer(e.rank.spec(0), e.allow, e.rule);
            }
        });
        let allowed = match best.verdict() {
            Some((allow, _)) => allow,
            None => !self.deny_private || !is_private_address(ip) || (is_localhost(host) && ip.is_loopback()),
        };
        if allowed { Decision::Allow } else { Decision::Deny }
    }

    fn files(&self, space: PathSpace) -> &FileIndex {
        let cell = match space {
            PathSpace::Host => &self.host_files,
            PathSpace::Memory => &self.memory_files,
        };
        cell.get_or_init(|| Arc::new(FileIndex::build(&self.rules, space)))
    }

    fn others(&self) -> &Index {
        self.others.get_or_init(|| Arc::new(Index::build(&self.rules)))
    }
}

/// The path of the entry `p` itself, normalized in `space` without resolving
/// the final name's spelling.
pub(crate) fn entry_path(p: &Path, space: PathSpace) -> PathBuf {
    match space {
        #[cfg(feature = "os")]
        PathSpace::Host => crate::files::Fs::Host.target(&lexical(p), Mode::Entry).path,
        #[cfg(not(feature = "os"))]
        PathSpace::Host => lexical(p),
        PathSpace::Memory => crate::files::memory_path(p),
    }
}

/// Names, folded as deny rules compare them.
fn folded<'a>(names: &'a [Cow<'a, str>]) -> Vec<Cow<'a, str>> {
    names.iter().map(|s| fold_name(s)).collect()
}

fn refs<S: AsRef<str>>(names: &[S]) -> Vec<&str> {
    names.iter().map(AsRef::as_ref).collect()
}

/// Whether an allow rule whose host pattern matches a destination decides
/// requests to it: not where it matches another spelling of the address,
/// and, for an `internal` destination, only if the pattern is `explicit`.
pub(crate) fn allow_applies(explicit: bool, spelled: bool, internal: bool) -> bool {
    !spelled && (explicit || !internal)
}

/// Whether an http rule's methods (upper case; `None`: any) include
/// `method` (upper case).
pub(crate) fn method_matches(methods: Option<&[String]>, method: &str) -> bool {
    methods.is_none_or(|m| m.iter().any(|m| m == method))
}

/// Whether an http rule's scheme (`None`: any) matches `scheme`.
pub(crate) fn scheme_matches(rule: Option<&str>, scheme: &str) -> bool {
    rule.is_none_or(|s| s.eq_ignore_ascii_case(scheme))
}

/// The pattern of a file rule whose prefix normalizes to `prefix`, root
/// first.
pub(crate) fn file_segments(prefix: &Path, rest: &[Seg]) -> Vec<Seg> {
    path_segments(prefix).iter().map(|s| Seg::Lit((**s).into())).chain(rest.iter().cloned()).collect()
}

/// A pattern with its names folded as deny rules compare them (see
/// [`fold_name`]).
pub(crate) fn fold_segments(segs: &[Seg]) -> Vec<Seg> {
    segs.iter().map(|s| s.map(|n| fold_name(n).into_owned())).collect()
}

/// How specific a rule's match is, compared in order.
pub(crate) type Spec = [u32; 4];

/// How a file, http or net rule ranks where it matches, apart from the
/// depth of the deepest path it matches there: by its tier, then by that
/// depth, then by its weight, then by whether it names methods.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rank {
    /// For http and net rules, the rank of the host pattern, then whether
    /// the rule names a scheme. 0 for file rules.
    pub tier: u32,
    /// How much the path pattern narrows the paths it matches.
    pub weight: u32,
    pub methods: bool,
}

impl Rank {
    pub(crate) fn file(segs: &[Seg]) -> Rank {
        Rank { tier: 0, weight: segs.iter().map(Seg::weight).sum(), methods: false }
    }

    pub(crate) fn http(http: &HttpMatch) -> Rank {
        let tier = http.host.rank() * 2 + http.scheme.is_some() as u32;
        Rank { tier, weight: http.path.iter().map(Seg::weight).sum(), methods: http.methods.is_some() }
    }

    pub(crate) fn spec(self, depth: usize) -> Spec {
        [self.tier, depth as u32, self.weight, self.methods as u32]
    }
}

/// How a rule ranks where it matches with `spec`: the more specific, then
/// a deny, then the first.
pub(crate) fn precedence(spec: Spec, allow: bool, rule: usize) -> (Spec, bool, std::cmp::Reverse<usize>) {
    (spec, !allow, std::cmp::Reverse(rule))
}

/// The deciding rule among those offered: the most specific, deny at a
/// tie, then the first.
struct Best<'a> {
    exclude: &'a [usize],
    best: Option<(Spec, bool, std::cmp::Reverse<usize>)>,
}

impl<'a> Best<'a> {
    fn new(exclude: &'a [usize]) -> Best<'a> {
        Best { exclude, best: None }
    }

    fn offer(&mut self, spec: Spec, allow: bool, rule: u32) {
        let rule = rule as usize;
        let candidate = precedence(spec, allow, rule);
        if !self.exclude.contains(&rule) && self.best.is_none_or(|b| candidate > b) {
            self.best = Some(candidate);
        }
    }

    fn verdict(&self) -> Option<(bool, usize)> {
        self.best.map(|(_, deny, rule)| (!deny, rule.0))
    }
}

/// The file rules of a policy, with their paths normalized in one space.
#[derive(Debug)]
struct FileIndex {
    allow: Trie<FileEntry>,
    /// Deny rules, by names folded with [`fold_name`]: they also match other
    /// spellings a file system may equate.
    deny: Trie<FileEntry>,
    /// Whether allow rules ignore case (Windows paths).
    caseless: bool,
}

#[derive(Debug)]
struct FileEntry {
    rule: u32,
    mask: u8,
    rank: Rank,
}

impl FileIndex {
    fn build(rules: &[Rule], space: PathSpace) -> FileIndex {
        let caseless = cfg!(windows) && space == PathSpace::Host;
        let mut index = FileIndex { allow: Trie::default(), deny: Trie::default(), caseless };
        let mut resolver = Resolver::default();
        for (i, rule) in rules.iter().enumerate() {
            let RuleKind::Files(access, pattern) = &rule.kind else { continue };
            let prefix = match space {
                PathSpace::Host => resolver.normalize(&pattern.prefix),
                PathSpace::Memory => crate::files::memory_path(&pattern.prefix),
            };
            let segs = file_segments(&prefix, &pattern.rest);
            let mask = access.mask(rule.allow);
            let entry = FileEntry { rule: i as u32, mask, rank: Rank::file(&segs) };
            if rule.allow {
                let segs: Vec<Seg> = if caseless { segs.iter().map(|s| s.map(str::to_lowercase)).collect() } else { segs };
                index.allow.insert(&segs, entry, mask);
            } else {
                index.deny.insert(&fold_segments(&segs), entry, mask);
            }
        }
        index
    }

    /// Names as allow rules compare them.
    fn allow_input<'a>(&self, names: &'a [Cow<'a, str>]) -> Vec<Cow<'a, str>> {
        names.iter().map(|n| if self.caseless { Cow::Owned(n.to_lowercase()) } else { Cow::Borrowed(&**n) }).collect()
    }
}

/// Normalizes the paths of many rules as [`normalize_path`] does, taking
/// one walk and one listing per directory rather than a walk per path.
#[derive(Default)]
struct Resolver {
    /// Directories: normalized, and what they hold (`None`: not listed).
    dirs: FxHashMap<PathBuf, (PathBuf, Option<Listing>)>,
}

enum Listing {
    /// The directory does not exist, so nothing in it does.
    Missing,
    Names {
        /// The names of its regular files.
        files: hashbrown::HashSet<std::ffi::OsString, crate::vm::value::FxBuild>,
        /// The folded names of all entries (see [`fold_name`]); `None` if
        /// the listing was incomplete.
        folded: Option<hashbrown::HashSet<String, crate::vm::value::FxBuild>>,
    },
}

/// The most entries listed of a directory.
const MAX_LISTING: usize = 1 << 20;

impl Resolver {
    fn normalize(&mut self, p: &Path) -> PathBuf {
        if !cfg!(feature = "os") {
            return normalize_path(p);
        }
        if let Some((resolved, _)) = self.dirs.get(p) {
            return resolved.clone();
        }
        let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return normalize_path(p) };
        let (resolved, listing) = self.dirs.entry(dir.to_path_buf()).or_insert_with(|| {
            let resolved = normalize_path(dir);
            let listing = match std::fs::read_dir(&resolved) {
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Some(Listing::Missing),
                Err(_) => None,
                Ok(entries) => {
                    let mut files = hashbrown::HashSet::default();
                    let mut folded = Some(hashbrown::HashSet::default());
                    for (i, e) in entries.enumerate() {
                        if i == MAX_LISTING {
                            folded = None;
                            break;
                        }
                        let Ok(e) = e else {
                            folded = None;
                            continue;
                        };
                        let name = e.file_name();
                        if let Some(f) = &mut folded {
                            f.insert(fold_name(&name.to_string_lossy()).into_owned());
                        }
                        if e.file_type().is_ok_and(|t| t.is_file()) {
                            files.insert(name);
                        }
                    }
                    Some(Listing::Names { files, folded })
                }
            };
            (resolved, listing)
        });
        match listing {
            // Regular files and nonexistent entries resolve directly;
            // directories may be firmlinks or junctions requiring a full walk.
            Some(Listing::Missing) => resolved.join(name),
            Some(Listing::Names { files, .. }) if files.contains(name) => resolved.join(name),
            Some(Listing::Names { folded: Some(folded), .. }) if !folded.contains(&*fold_name(&name.to_string_lossy())) => {
                resolved.join(name)
            }
            _ => normalize_path(p),
        }
    }
}

/// The rules of a policy other than file rules.
#[derive(Debug, Default)]
struct Index {
    /// The rules for each kind of connection (see [`RuleKind::connections`]).
    named: HostIndex<NetEntry>,
    resolved: HostIndex<NetEntry>,
    /// Groups of http and net rules by host pattern (see
    /// [`RuleKind::http_match`]).
    http: HostIndex<u32>,
    groups: Vec<HttpGroup>,
    listen: Vec<(ListenPattern, bool, u32)>,
    /// Rules for variables by name, and those with patterns.
    env_names: FxHashMap<String, Vec<(bool, u32)>>,
    env: Vec<(NamePattern, bool, u32)>,
    unit: Vec<(Capability, bool, u32)>,
    /// Custom rules by name: their detail, if any.
    custom: FxHashMap<String, Vec<(Option<String>, bool, u32)>>,
    imports_restricted: bool,
}

#[derive(Debug)]
struct NetEntry {
    rule: u32,
    allow: bool,
    rank: Rank,
}

#[derive(Debug, Default)]
struct HttpGroup {
    allow: Trie<HttpEntry>,
    /// Deny rules, by folded names (see [`FileIndex::deny`]).
    deny: Trie<HttpEntry>,
}

#[derive(Debug)]
struct HttpEntry {
    rule: u32,
    methods: Option<Vec<String>>,
    scheme: Option<&'static str>,
    rank: Rank,
}

impl Index {
    fn build(rules: &[Rule]) -> Index {
        let mut index = Index::default();
        let mut groups: FxHashMap<String, u32> = FxHashMap::default();
        for (i, rule) in rules.iter().enumerate() {
            let (rule_id, allow) = (i as u32, rule.allow);
            if let Some(http) = rule.kind.http_match() {
                let rank = Rank::http(&http);
                for &connection in rule.kind.connections() {
                    let entry = NetEntry { rule: rule_id, allow, rank };
                    match connection {
                        Connection::Named => index.named.insert(http.host, entry),
                        Connection::Resolved => index.resolved.insert(http.host, entry),
                    }
                }
                let group = *groups.entry(http.host.to_string()).or_insert_with(|| {
                    index.http.insert(http.host, index.groups.len() as u32);
                    index.groups.push(HttpGroup::default());
                    index.groups.len() as u32 - 1
                });
                let entry = HttpEntry { rule: rule_id, methods: http.methods.map(<[String]>::to_vec), scheme: http.scheme, rank };
                let group = &mut index.groups[group as usize];
                if allow {
                    group.allow.insert(http.path, entry, 1);
                } else {
                    group.deny.insert(&fold_segments(http.path), entry, 1);
                }
            }
            match &rule.kind {
                RuleKind::Files(Access::Import, _) => index.imports_restricted |= allow,
                RuleKind::Files(..) | RuleKind::Net(_) | RuleKind::Http(..) => {}
                RuleKind::Listen(p) => index.listen.push((p.clone(), allow, rule_id)),
                RuleKind::Env(NamePattern::Exact(n)) => index.env_names.entry(n.clone()).or_default().push((allow, rule_id)),
                RuleKind::Env(p) => index.env.push((p.clone(), allow, rule_id)),
                RuleKind::Stdin => index.unit.push((Capability::Stdin, allow, rule_id)),
                RuleKind::Stdout => index.unit.push((Capability::Stdout, allow, rule_id)),
                RuleKind::Stderr => index.unit.push((Capability::Stderr, allow, rule_id)),
                RuleKind::TimeZone => index.unit.push((Capability::TimeZone, allow, rule_id)),
                RuleKind::Custom(n, d) => index.custom.entry(n.clone()).or_default().push((d.clone(), allow, rule_id)),
            }
        }
        index
    }
}

#[cfg(all(test, feature = "os", unix))]
mod tests {
    use super::*;

    /// Resolving many paths at once agrees with resolving each.
    #[test]
    fn the_resolver_agrees_with_normalize_path() {
        let base = normalize_path(&std::env::temp_dir()).join(format!("sand-policy-resolver-{}", std::process::id()));
        std::fs::create_dir_all(base.join("dir/sub")).unwrap();
        std::fs::write(base.join("dir/File.txt"), "").unwrap();
        std::os::unix::fs::symlink(base.join("dir/File.txt"), base.join("dir/file-link")).unwrap();
        std::os::unix::fs::symlink(base.join("dir/sub"), base.join("dir/dir-link")).unwrap();
        std::os::unix::fs::symlink("missing", base.join("dir/dangling")).unwrap();
        let mut resolver = Resolver::default();
        for rest in [
            "dir/File.txt",
            "dir/file.TXT",
            "dir/sub",
            "dir/SUB",
            "dir/file-link",
            "dir/dir-link",
            "dir/dangling",
            "dir/missing",
            "dir/dir-link/x",
            "dir/dangling/x",
            "missing/x",
            "dir/File.txt/x",
            "dir/new.TXT",
            "dir/FILE-LINK",
            "dir/sub/x",
            "dir/sub",
            "dir/SUB/x",
        ] {
            let p = base.join(rest);
            assert_eq!(resolver.normalize(&p), normalize_path(&p), "{rest}");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
