//! Rule-based capability policies.

use super::net::{Dest, HostIndex, HostKind, ListenPattern, NetPattern, UrlPattern, parse_ip, url_segments};
use super::path::{Glob, Seg, Trie, path_segments};
use super::{Capability, Decision, PathSpace, fold_name, lexical, normalize_path};
use crate::files::Mode;
use crate::vm::value::FxHashMap;
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Access {
    Read,
    Write,
    Import,
    Any,
}

pub(crate) const READ: u8 = 1;
pub(crate) const WRITE: u8 = 2;
pub(crate) const IMPORT: u8 = 4;

impl Access {
    /// The operations a rule for this access decides: an `allow` rule also
    /// allows less (writing implies reading), a `deny` or `prompt` rule also
    /// restricts more (reading guards writing and importing).
    pub(crate) fn mask(self, decision: Decision) -> u8 {
        match (self, decision == Decision::Allow) {
            (Access::Read, true) => READ,
            (Access::Write | Access::Any, true) => READ | WRITE,
            (Access::Read | Access::Any, false) => READ | WRITE | IMPORT,
            (Access::Write, false) => WRITE,
            (Access::Import, _) => IMPORT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PathPattern {
    pub prefix: PathBuf,
    pub rest: Vec<Seg>,
}

impl PathPattern {
    /// The pattern of every path, on every root.
    pub(crate) fn everything() -> PathPattern {
        PathPattern { prefix: PathBuf::new(), rest: vec![] }
    }

    /// The pattern's segments, with its prefix normalized by `normalize`.
    pub(crate) fn segments(&self, normalize: impl FnOnce(&Path) -> PathBuf) -> Vec<Seg> {
        if self.prefix.as_os_str().is_empty() {
            return self.rest.clone();
        }
        file_segments(&normalize(&self.prefix), &self.rest)
    }
}

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

bitflags::bitflags! {
    /// The transports that a `net` or `http` rule matches: TLS (the
    /// default), plaintext, or both.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub(crate) struct Transports: u8 {
        const TLS = 1;
        const PLAINTEXT = 2;
    }
}

impl Transports {
    /// The transport of a connection that uses TLS (`tls`) or not.
    pub(crate) fn of(tls: bool) -> Transports {
        if tls { Transports::TLS } else { Transports::PLAINTEXT }
    }

    /// Whether the rule matches one transport, which makes it more specific.
    pub(crate) fn single(self) -> bool {
        self.bits().count_ones() == 1
    }
}

/// Whether the connection that `cap` (`Net` or `Http`) asks for uses TLS.
pub(crate) fn uses_tls(cap: &Capability) -> bool {
    match cap {
        Capability::Net { tls, .. } => *tls,
        Capability::Http { scheme, .. } => scheme.eq_ignore_ascii_case("https"),
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RuleKind {
    Files(Access, PathPattern),
    Net(NetPattern, Transports),
    Http(Option<Vec<String>>, UrlPattern, Transports),
    Listen(ListenPattern),
    Env(NamePattern),
    Stdin,
    Stdout,
    Stderr,
    TimeZone,
    /// A custom capability's name (`None`: any name) and detail (`None`: any detail).
    Custom(Option<String>, Option<String>),
}

impl RuleKind {
    pub(crate) fn http_match(&self) -> Option<HttpMatch<'_>> {
        match self {
            RuleKind::Http(methods, url, transports) => {
                Some(HttpMatch { methods: methods.as_deref(), transports: *transports, host: &url.host, path: &url.path })
            }
            RuleKind::Net(host, transports) => Some(HttpMatch { methods: None, transports: *transports, host, path: &[] }),
            _ => None,
        }
    }

    pub(crate) fn connections(&self) -> &'static [Connection] {
        match self {
            RuleKind::Net(NetPattern { kind: HostKind::Ip(..), .. }, _) => &[Connection::Named, Connection::Resolved],
            RuleKind::Net(..) => &[Connection::Named],
            _ => &[],
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HttpMatch<'a> {
    pub methods: Option<&'a [String]>,
    pub transports: Transports,
    pub host: &'a NetPattern,
    pub path: &'a [Seg],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Connection {
    Named,
    Resolved,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub decision: Decision,
    pub kind: RuleKind,
    /// Where a policy file gives the rule.
    pub origin: Option<Origin>,
}

impl Rule {
    pub(crate) fn new(decision: Decision, kind: RuleKind) -> Rule {
        Rule { decision, kind, origin: None }
    }

    /// Whether the rule compares names exactly. Allow rules do; deny and
    /// prompt rules match every spelling of their names and every
    /// representation of their addresses.
    pub(crate) fn exact(&self) -> bool {
        self.decision == Decision::Allow
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Origin {
    pub line: usize,
    pub column: usize,
    pub text: String,
}

/// A rule-based policy: the most specific rule that matches a request
/// decides it. Of two rules with equal specificity, `deny` wins over
/// `allow`, and `allow` wins over `prompt`. A request that no rule matches
/// gets `Prompt`: the prompter asks the user, or, without a prompter, the
/// engine denies the request.
///
/// The display of a policy lists its rules as policy file text.
#[derive(Clone, Default)]
pub struct RulePolicy {
    rules: Vec<Rule>,
    host_files: OnceLock<Arc<FileIndex>>,
    memory_files: OnceLock<Arc<FileIndex>>,
    others: OnceLock<Arc<Index>>,
}

impl fmt::Debug for RulePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RulePolicy").field("rules", &self.rules.len()).finish()
    }
}

fn listen_pattern(pattern: &str) -> ListenPattern {
    ListenPattern::parse(pattern).unwrap_or_else(|| panic!("invalid listen pattern {pattern:?}"))
}

fn custom(name: &str, detail: Option<&str>) -> RuleKind {
    RuleKind::Custom((name != "*").then(|| name.to_string()), detail.map(String::from))
}

/// Rule paths are literal prefixes without wildcards; for wildcard patterns,
/// use [`super::PolicyFile`].
impl RulePolicy {
    /// Creates a policy without rules, the same as [`RulePolicy::default`]:
    /// the prompter decides every request.
    pub fn new() -> RulePolicy {
        RulePolicy::default()
    }

    /// Creates a policy that allows writing to standard output and standard
    /// error (`allow stdout`, `allow stderr`).
    pub fn standard() -> RulePolicy {
        RulePolicy::new().allow_stdout().allow_stderr()
    }

    /// Creates a policy with a rule that allows each kind of capability:
    /// all files and imports, network connections with and without TLS,
    /// listeners, environment variables, the standard streams, the time
    /// zone, and every custom capability. Use it only for trusted scripts.
    pub fn allow_all() -> RulePolicy {
        let rules = [
            RuleKind::Files(Access::Any, PathPattern::everything()),
            RuleKind::Files(Access::Import, PathPattern::everything()),
            RuleKind::Net(NetPattern { kind: HostKind::Any, port: None }, Transports::all()),
            RuleKind::Listen(listen_pattern("*")),
            RuleKind::Env(NamePattern::Any),
            RuleKind::Stdin,
            RuleKind::Stdout,
            RuleKind::Stderr,
            RuleKind::TimeZone,
            RuleKind::Custom(None, None),
        ];
        rules.into_iter().fold(RulePolicy::new(), |p, kind| p.push(Decision::Allow, kind))
    }

    /// Adds the rules of `other` after the rules of this policy.
    pub fn extend(self, other: RulePolicy) -> RulePolicy {
        self.extend_rules(other.rules)
    }

    /// Adds each rule of `defaults` for which this policy has no rule with
    /// the same pattern (with any decision). A host uses it to combine the
    /// user's rules with its own defaults: a user rule replaces the default
    /// for the same requests, so `prompt stdin` asks even if the defaults
    /// have `allow stdin`.
    pub fn with_defaults(self, defaults: RulePolicy) -> RulePolicy {
        let kept = defaults.rules.into_iter().filter(|d| !self.rules.iter().any(|r| r.kind == d.kind)).collect();
        self.extend_rules(kept)
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub(crate) fn push_rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self.host_files = OnceLock::new();
        self.memory_files = OnceLock::new();
        self.others = OnceLock::new();
        self
    }

    fn push(self, decision: Decision, kind: RuleKind) -> Self {
        self.push_rule(Rule::new(decision, kind))
    }

    pub(crate) fn extend_rules(self, rules: Vec<Rule>) -> Self {
        rules.into_iter().fold(self, RulePolicy::push_rule)
    }

    fn files_rule(self, decision: Decision, access: Access, p: &Path) -> Self {
        self.push(decision, RuleKind::Files(access, PathPattern { prefix: p.to_path_buf(), rest: vec![] }))
    }

    /// Adds the rules of the policy file line `{verb} {kind} {rest}`.
    ///
    /// # Panics
    ///
    /// Panics if the line is invalid.
    fn parsed(self, verb: &str, kind: &str, rest: &str) -> Self {
        let file = super::PolicyFile::parse(&format!("{verb} {kind} {rest}"), Path::new("/"), None)
            .unwrap_or_else(|e| panic!("invalid {kind} rule {rest:?}: {}", e[0].message));
        let parsed = file.rules.rules.into_iter().map(|r| Rule::new(r.decision, r.kind));
        parsed.fold(self, RulePolicy::push_rule)
    }

    pub fn allow_read(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Allow, Access::Read, p.as_ref())
    }
    /// Denies reading, writing, and importing under `p`.
    pub fn deny_read(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Deny, Access::Read, p.as_ref())
    }
    /// Asks before reading, writing, or importing under `p`.
    pub fn prompt_read(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Prompt, Access::Read, p.as_ref())
    }
    /// Allows reading and writing under `p`.
    pub fn allow_write(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Allow, Access::Write, p.as_ref())
    }
    pub fn deny_write(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Deny, Access::Write, p.as_ref())
    }
    pub fn prompt_write(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Prompt, Access::Write, p.as_ref())
    }
    /// Allows importing the script modules under `p`.
    pub fn allow_import(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Allow, Access::Import, p.as_ref())
    }
    pub fn deny_import(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Deny, Access::Import, p.as_ref())
    }
    pub fn prompt_import(self, p: impl AsRef<Path>) -> Self {
        self.files_rule(Decision::Prompt, Access::Import, p.as_ref())
    }
    /// Allows outgoing connections as the policy file rule `allow net
    /// {rule}` does: optional transports (`tls`, the default, and
    /// `plaintext`), then hosts, such as `"api.example.com:443"` or
    /// `"plaintext localhost:8080"`. A host is a hostname, `*.domain`, `*`,
    /// an IP address, or a CIDR range, optionally with a `:port` suffix.
    ///
    /// # Panics
    ///
    /// Panics if `rule` is invalid; use [`super::check_net_pattern`] to
    /// validate a host pattern.
    pub fn allow_net(self, rule: &str) -> Self {
        self.parsed("allow", "net", rule)
    }
    /// Denies outgoing connections; see [`RulePolicy::allow_net`].
    pub fn deny_net(self, rule: &str) -> Self {
        self.parsed("deny", "net", rule)
    }
    /// Asks before connecting; see [`RulePolicy::allow_net`].
    pub fn prompt_net(self, rule: &str) -> Self {
        self.parsed("prompt", "net", rule)
    }
    /// Allows HTTP requests as the policy file rule `allow http {rule}`
    /// does: optional transports and methods, then URL patterns, such as
    /// `"GET POST api.example.com/v1/**"` or `"plaintext localhost:8080"`.
    ///
    /// # Panics
    ///
    /// Panics if `rule` is invalid.
    pub fn allow_http(self, rule: &str) -> Self {
        self.parsed("allow", "http", rule)
    }
    pub fn deny_http(self, rule: &str) -> Self {
        self.parsed("deny", "http", rule)
    }
    pub fn prompt_http(self, rule: &str) -> Self {
        self.parsed("prompt", "http", rule)
    }
    /// Allows binding a listener matching `pattern`: `ip:port`, `[ipv6]:port`,
    /// `ip`, `localhost`, `*:port`, or `*`. Host `*` matches any address, port
    /// `*` matches any port, and port `0` matches only an ephemeral port
    /// request.
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is invalid.
    pub fn allow_listen(self, pattern: &str) -> Self {
        self.push(Decision::Allow, RuleKind::Listen(listen_pattern(pattern)))
    }
    /// Denies binding a listener matching `pattern`; see [`RulePolicy::allow_listen`].
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is invalid.
    pub fn deny_listen(self, pattern: &str) -> Self {
        self.push(Decision::Deny, RuleKind::Listen(listen_pattern(pattern)))
    }
    /// Asks before binding a listener matching `pattern`; see [`RulePolicy::allow_listen`].
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is invalid.
    pub fn prompt_listen(self, pattern: &str) -> Self {
        self.push(Decision::Prompt, RuleKind::Listen(listen_pattern(pattern)))
    }
    /// Allows reading environment variables matching a name, a `*` pattern
    /// such as `AWS_*`, or `*` for all variables.
    pub fn allow_env(self, name: &str) -> Self {
        self.push(Decision::Allow, RuleKind::Env(NamePattern::parse(name)))
    }
    /// Denies reading matching environment variables. Listing all variables is
    /// denied if any variable is denied.
    pub fn deny_env(self, name: &str) -> Self {
        self.push(Decision::Deny, RuleKind::Env(NamePattern::parse(name)))
    }
    /// Asks before reading matching environment variables. Listing all
    /// variables asks if any variable asks (and none is denied).
    pub fn prompt_env(self, name: &str) -> Self {
        self.push(Decision::Prompt, RuleKind::Env(NamePattern::parse(name)))
    }
    pub fn allow_stdin(self) -> Self {
        self.push(Decision::Allow, RuleKind::Stdin)
    }
    pub fn deny_stdin(self) -> Self {
        self.push(Decision::Deny, RuleKind::Stdin)
    }
    pub fn prompt_stdin(self) -> Self {
        self.push(Decision::Prompt, RuleKind::Stdin)
    }
    pub fn allow_stdout(self) -> Self {
        self.push(Decision::Allow, RuleKind::Stdout)
    }
    pub fn deny_stdout(self) -> Self {
        self.push(Decision::Deny, RuleKind::Stdout)
    }
    pub fn prompt_stdout(self) -> Self {
        self.push(Decision::Prompt, RuleKind::Stdout)
    }
    pub fn allow_stderr(self) -> Self {
        self.push(Decision::Allow, RuleKind::Stderr)
    }
    pub fn deny_stderr(self) -> Self {
        self.push(Decision::Deny, RuleKind::Stderr)
    }
    pub fn prompt_stderr(self) -> Self {
        self.push(Decision::Prompt, RuleKind::Stderr)
    }
    pub fn allow_timezone(self) -> Self {
        self.push(Decision::Allow, RuleKind::TimeZone)
    }
    pub fn deny_timezone(self) -> Self {
        self.push(Decision::Deny, RuleKind::TimeZone)
    }
    pub fn prompt_timezone(self) -> Self {
        self.push(Decision::Prompt, RuleKind::TimeZone)
    }
    /// Allows a custom capability: `name` `"*"` matches every name, and
    /// `detail` `None` matches every detail.
    pub fn allow_custom(self, name: &str, detail: Option<&str>) -> Self {
        self.push(Decision::Allow, custom(name, detail))
    }
    pub fn deny_custom(self, name: &str, detail: Option<&str>) -> Self {
        self.push(Decision::Deny, custom(name, detail))
    }
    pub fn prompt_custom(self, name: &str, detail: Option<&str>) -> Self {
        self.push(Decision::Prompt, custom(name, detail))
    }

    /// Decides `cap`: `Prompt` if no rule matches it. A file capability's
    /// path is first normalized with [`super::normalize_path`].
    pub fn check(&self, cap: &Capability) -> Decision {
        self.check_in(cap, PathSpace::Host)
    }

    /// Like [`RulePolicy::check`] for a capability whose path (`FsRead`,
    /// `FsWrite`, `Import`) the caller already normalized with
    /// [`super::normalize_path`]: skips normalizing it again, which costs a
    /// `realpath`.
    pub fn check_normalized(&self, cap: &Capability) -> Decision {
        self.check_normalized_in(cap, PathSpace::Host)
    }

    /// Like [`RulePolicy::check`] for an operation on a directory entry
    /// itself (removing or renaming it): a final symlink in an `FsRead` or
    /// `FsWrite` path is the entry and is not resolved to its target.
    pub fn check_entry(&self, cap: &Capability) -> Decision {
        self.check_entry_in(cap, PathSpace::Host)
    }

    /// Whether a deny or prompt rule may match `path` or a path below it. If
    /// one may, the engine scans the tree at `path` for protected entries
    /// before it moves or removes the tree.
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

    /// Decides `cap`: `Prompt` if no rule matches it. A `NetAddr` capability
    /// is decided as an address that a `Net` request to its host resolved
    /// to; see [`RulePolicy::check_resolved`].
    pub(crate) fn check_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        if let Capability::NetAddr { host, addr, tls } = cap {
            return self.check_resolved(&Capability::Net { host: host.clone(), port: addr.port(), tls: *tls }, *addr);
        }
        self.verdict(cap, space, &[]).map_or(Decision::Prompt, |(d, _)| d)
    }

    /// Decides connecting to `addr`, an address that the host of `request`
    /// (a `Net` or `Http` request that the policy did not deny) resolved to.
    /// The engine makes this check before each connection, with the
    /// script's request. Returns `Allow` or `Deny`, never `Prompt`.
    ///
    /// The rule that allowed `request`, or prompted for it, competes with the
    /// address rules that match `addr` and the request's transport, by the
    /// specificity of their hosts: a host name ranks as a single address, a
    /// pattern of names under `localhost` above every range, and other
    /// domain patterns and `*` below every address rule. If that rule wins, or if no address rule
    /// matches, the connection is allowed. Otherwise the winning address
    /// rule decides, and a `prompt` rule denies, because the request was
    /// decided already.
    pub fn check_resolved(&self, request: &Capability, addr: std::net::SocketAddr) -> Decision {
        if self.resolved_verdict(request, addr).is_ok() { Decision::Allow } else { Decision::Deny }
    }

    /// Like [`RulePolicy::check_resolved`]. On a denial, returns the index
    /// of the deciding rule, if a rule decides.
    pub(crate) fn resolved_verdict(&self, request: &Capability, addr: std::net::SocketAddr) -> Result<(), Option<usize>> {
        let (Capability::Net { host, .. } | Capability::Http { host, .. }) = request else { return Ok(()) };
        let ip = addr.ip().to_canonical();
        // An IP literal was already checked as `Net` and is resolved only to itself.
        if let Some(literal) = parse_ip(host) {
            return if literal == ip { Ok(()) } else { Err(None) };
        }
        let transport = Transports::of(uses_tls(request));
        let mut best = Best::new(&[]);
        self.others().resolved.find_ip(ip, addr.port(), |is_alt, e| {
            if applies(e.decision, is_alt) && e.transports.contains(transport) {
                best.offer(e.rank.spec(0), e.decision, e.rule);
            }
        });
        let Some((decision, rule)) = best.verdict() else { return Ok(()) };
        if let Some((d, decider)) = self.verdict(request, PathSpace::Host, &[]).filter(|&(d, _)| d != Decision::Deny) {
            let key = |i: usize, d: Decision| resolution_key(&self.rules[i].kind, d);
            if key(decider, d) > key(rule, decision) {
                return Ok(());
            }
        }
        if decision == Decision::Allow { Ok(()) } else { Err(Some(rule)) }
    }

    pub(crate) fn denies_under_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        let index = self.files(space);
        !index.strict.is_empty() && index.strict.any_below(&refs(&folded(&path_segments(path))), READ | WRITE)
    }

    pub(crate) fn protects_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        [READ, WRITE].into_iter().any(|op| {
            let mut best = Best::new(&[]);
            self.file_verdict(path, op, space, &mut best);
            best.verdict().is_some_and(|(d, _)| d != Decision::Allow)
        })
    }

    pub(crate) fn decision_for_any_spelling(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        self.spelling_free_decision(cap, space).filter(|&d| d != Decision::Prompt)
    }

    fn spelling_free_decision(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        let (Capability::FsRead(p) | Capability::FsWrite(p) | Capability::Import(p)) = cap else { return None };
        let index = self.files(space);
        if index.allow.matches_last(&refs(&index.allow_input(&path_segments(p)))) {
            return None;
        }
        Some(self.check_normalized_in(cap, space))
    }

    /// The decision of the most specific rule matching `cap` (except the
    /// rules in `exclude`), and its index.
    pub(crate) fn verdict(&self, cap: &Capability, space: PathSpace, exclude: &[usize]) -> Option<(Decision, usize)> {
        let mut best = Best::new(exclude);
        let index = self.others();
        match cap {
            Capability::FsRead(p) => self.file_verdict(p, READ, space, &mut best),
            Capability::FsWrite(p) => self.file_verdict(p, WRITE, space, &mut best),
            Capability::Import(p) => self.file_verdict(p, IMPORT, space, &mut best),
            Capability::Net { host, port, tls } => self.net_verdict(&Dest::new(host, *port), *tls, &mut best),
            Capability::NetAddr { .. } => {}
            Capability::Http { method, scheme, host, port, path } => {
                let dest = Dest::new(host, *port);
                let (names, alt) = url_segments(path);
                let tls = scheme.eq_ignore_ascii_case("https");
                self.http_verdict(&dest, method, tls, &names, &mut best);
                let Some(alt) = alt else { return best.verdict() };
                // A server may split at decoded separators: both readings
                // must be allowed.
                let mut other = Best::new(exclude);
                self.http_verdict(&dest, method, tls, &alt, &mut other);
                // The stricter of the two decisions holds, which may be the
                // prompt for an unmatched request.
                let strictness = |v: Option<(Decision, usize)>| match v.map_or(Decision::Prompt, |(d, _)| d) {
                    Decision::Allow => 0,
                    Decision::Prompt => 1,
                    Decision::Deny => 2,
                };
                let (main, alt) = (best.verdict(), other.verdict());
                return if strictness(alt) > strictness(main) { alt } else { main };
            }
            Capability::NetListen(addr) => {
                for &(ref pattern, d, rule) in &index.listen {
                    if let Some(score) = pattern.matches(*addr) {
                        best.offer([score, 0, 0, 0], d, rule);
                    }
                }
            }
            Capability::Env(name) if name == "*" => {
                // All variables cannot be had while some are denied, or
                // without asking while some ask.
                let named = index.env_names.values().flatten().copied();
                let guards = named.chain(index.env.iter().map(|&(_, d, rule)| (d, rule)));
                let guard = guards
                    .filter(|&(d, rule)| d != Decision::Allow && !exclude.contains(&(rule as usize)))
                    .max_by_key(|&(d, rule)| (d == Decision::Deny, std::cmp::Reverse(rule)));
                if let Some((d, rule)) = guard {
                    return Some((d, rule as usize));
                }
                for &(ref pattern, d, rule) in &index.env {
                    if *pattern == NamePattern::Any {
                        best.offer([0; 4], d, rule);
                    }
                }
            }
            Capability::Env(name) => {
                for &(d, rule) in index.env_names.get(name.as_str()).into_iter().flatten() {
                    best.offer([2, 0, 0, 0], d, rule);
                }
                for &(ref pattern, d, rule) in &index.env {
                    let spec = match pattern {
                        NamePattern::Glob(g) if g.matches(name) => [1, g.pieces().iter().map(|p| p.len() as u32).sum(), 0, 0],
                        NamePattern::Any => [0; 4],
                        _ => continue,
                    };
                    best.offer(spec, d, rule);
                }
            }
            Capability::Stdin | Capability::Stdout | Capability::Stderr | Capability::TimeZone => {
                for &(ref kind, d, rule) in &index.unit {
                    if std::mem::discriminant(kind) == std::mem::discriminant(cap) {
                        best.offer([0; 4], d, rule);
                    }
                }
            }
            Capability::Custom { name, detail } => {
                let named = index.custom.get(name.as_str()).into_iter().flatten().map(|e| (true, e));
                for (named, &(ref detail_pattern, d, rule)) in named.chain(index.any_custom.iter().map(|e| (false, e))) {
                    if detail_pattern.as_ref().is_none_or(|p| p == detail) {
                        best.offer([2 * named as u32 + detail_pattern.is_some() as u32, 0, 0, 0], d, rule);
                    }
                }
            }
        }
        best.verdict()
    }

    fn file_verdict(&self, p: &Path, op: u8, space: PathSpace, best: &mut Best<'_>) {
        let index = self.files(space);
        let mut offer = |e: &FileEntry, depth: usize| {
            if e.mask & op != 0 {
                best.offer(e.rank.spec(depth), e.decision, e.rule);
            }
        };
        let names = path_segments(p);
        if !index.allow.is_empty() {
            index.allow.matches(&refs(&index.allow_input(&names)), &mut offer);
        }
        if !index.strict.is_empty() {
            index.strict.matches(&refs(&folded(&names)), &mut offer);
        }
    }

    fn net_verdict(&self, dest: &Dest, tls: bool, best: &mut Best<'_>) {
        let transport = Transports::of(tls);
        self.others().named.find(dest, |is_alt, e| {
            if applies(e.decision, is_alt) && e.transports.contains(transport) {
                best.offer(e.rank.spec(0), e.decision, e.rule);
            }
        });
    }

    fn http_verdict(&self, dest: &Dest, method: &str, tls: bool, names: &[String], best: &mut Best<'_>) {
        let index = self.others();
        if index.groups.is_empty() {
            return;
        }
        let method = method.to_ascii_uppercase();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut folded_names = None;
        index.http.find(dest, |is_alt, &group| {
            let group = &index.groups[group as usize];
            let transport = Transports::of(tls);
            let mut offer = |e: &HttpEntry, depth: usize| {
                if method_matches(e.methods.as_deref(), &method) && e.transports.contains(transport) {
                    best.offer(e.rank.spec(depth), e.decision, e.rule);
                }
            };
            if !group.allow.is_empty() && applies(Decision::Allow, is_alt) {
                group.allow.matches(&names, &mut offer);
            }
            if !group.strict.is_empty() {
                let folded = folded_names.get_or_insert_with(|| names.iter().map(|n| fold_name(n)).collect::<Vec<_>>());
                group.strict.matches(&refs(folded), &mut offer);
            }
        });
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

impl RulePolicy {
    /// The rule at index `i`, for messages: "the rule `...` (line N)".
    pub(crate) fn describe_rule(&self, i: usize) -> String {
        let rule = &self.rules[i];
        match &rule.origin {
            Some(o) => format!("the rule `{rule}` (line {})", o.line),
            None => format!("the rule `{rule}`"),
        }
    }
}

/// Lists the rules as policy file text.
impl fmt::Display for RulePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for rule in &self.rules {
            writeln!(f, "{rule}")?;
        }
        Ok(())
    }
}

pub(crate) fn entry_path(p: &Path, space: PathSpace) -> PathBuf {
    match space {
        #[cfg(feature = "os")]
        PathSpace::Host => crate::files::Fs::Host.target(&lexical(p), Mode::Entry).path,
        #[cfg(not(feature = "os"))]
        PathSpace::Host => lexical(p),
        PathSpace::Memory => crate::files::memory_path(p),
    }
}

fn folded<'a>(names: &'a [Cow<'a, str>]) -> Vec<Cow<'a, str>> {
    names.iter().map(|s| fold_name(s)).collect()
}

fn refs<S: AsRef<str>>(names: &[S]) -> Vec<&str> {
    names.iter().map(AsRef::as_ref).collect()
}

/// The order in which a rule competes with address rules for a resolved
/// address ([`RulePolicy::check_resolved`]): by the specificity of its host
/// alone, then by its decision. A host name ranks as a single address and a
/// pattern of the names under `localhost` as a narrower pattern, since both
/// name one host. Other domain patterns rank below every address rule, as
/// `*` does: they name hosts they do not know, so they do not vouch for the
/// addresses those hosts resolve to.
pub(crate) fn resolution_key(kind: &RuleKind, decision: Decision) -> Precedence {
    let tier = match kind.http_match() {
        Some(HttpMatch { host: NetPattern { kind: HostKind::Suffix(s), .. }, .. }) if !super::net::is_localhost(s) => 0,
        Some(http) => http.host.host_rank(),
        None => 0,
    };
    precedence([tier, 0, 0, 0], decision, usize::MAX)
}

/// Whether a rule with `decision` applies to a request through another
/// representation of its address: deny and prompt rules do, allow rules do
/// not.
pub(crate) fn applies(decision: Decision, is_alternate_ip: bool) -> bool {
    !is_alternate_ip || decision != Decision::Allow
}

pub(crate) fn method_matches(methods: Option<&[String]>, method: &str) -> bool {
    methods.is_none_or(|m| m.iter().any(|m| m == method))
}

pub(crate) fn file_segments(prefix: &Path, rest: &[Seg]) -> Vec<Seg> {
    path_segments(prefix).iter().map(|s| Seg::Lit((**s).into())).chain(rest.iter().cloned()).collect()
}

pub(crate) fn fold_segments(segs: &[Seg]) -> Vec<Seg> {
    segs.iter().map(|s| s.map(|n| fold_name(n).into_owned())).collect()
}

pub(crate) type Spec = [u32; 4];

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rank {
    pub tier: u32,
    pub weight: u32,
    pub methods: bool,
}

impl Rank {
    pub(crate) fn file(segs: &[Seg]) -> Rank {
        Rank { tier: 0, weight: segs.iter().map(Seg::weight).sum(), methods: false }
    }

    pub(crate) fn http(http: &HttpMatch) -> Rank {
        let tier = http.host.rank() * 2 + http.transports.single() as u32;
        Rank { tier, weight: http.path.iter().map(Seg::weight).sum(), methods: http.methods.is_some() }
    }

    pub(crate) fn spec(self, depth: usize) -> Spec {
        [self.tier, depth as u32, self.weight, self.methods as u32]
    }
}

/// How strongly a decision holds against an opposite decision of a rule with
/// equal specificity: `deny` wins over `allow`, which wins over `prompt` (so
/// a rule saved from a prompt answer decides that prompt).
pub(crate) fn strength(decision: Decision) -> u8 {
    match decision {
        Decision::Deny => 2,
        Decision::Allow => 1,
        Decision::Prompt => 0,
    }
}

/// The order of matching rules: the greatest decides.
pub(crate) type Precedence = (Spec, u8, std::cmp::Reverse<usize>);

pub(crate) fn precedence(spec: Spec, decision: Decision, rule: usize) -> Precedence {
    (spec, strength(decision), std::cmp::Reverse(rule))
}

struct Best<'a> {
    exclude: &'a [usize],
    best: Option<(Precedence, Decision)>,
}

impl<'a> Best<'a> {
    fn new(exclude: &'a [usize]) -> Best<'a> {
        Best { exclude, best: None }
    }

    fn offer(&mut self, spec: Spec, decision: Decision, rule: u32) {
        let rule = rule as usize;
        let candidate = precedence(spec, decision, rule);
        if !self.exclude.contains(&rule) && self.best.is_none_or(|(b, _)| candidate > b) {
            self.best = Some((candidate, decision));
        }
    }

    fn verdict(&self) -> Option<(Decision, usize)> {
        self.best.map(|((.., rule), decision)| (decision, rule.0))
    }
}

#[derive(Debug)]
struct FileIndex {
    /// Allow rules, which compare names exactly.
    allow: Trie<FileEntry>,
    /// Deny and prompt rules, which compare folded names.
    strict: Trie<FileEntry>,
    caseless: bool,
}

#[derive(Debug)]
struct FileEntry {
    rule: u32,
    decision: Decision,
    mask: u8,
    rank: Rank,
}

impl FileIndex {
    fn build(rules: &[Rule], space: PathSpace) -> FileIndex {
        let caseless = cfg!(windows) && space == PathSpace::Host;
        let mut index = FileIndex { allow: Trie::default(), strict: Trie::default(), caseless };
        let mut resolver = Resolver::default();
        for (i, rule) in rules.iter().enumerate() {
            let RuleKind::Files(access, pattern) = &rule.kind else { continue };
            let segs = pattern.segments(|prefix| match space {
                PathSpace::Host => resolver.normalize(prefix),
                PathSpace::Memory => crate::files::memory_path(prefix),
            });
            let mask = access.mask(rule.decision);
            let entry = FileEntry { rule: i as u32, decision: rule.decision, mask, rank: Rank::file(&segs) };
            if rule.exact() {
                let segs: Vec<Seg> = if caseless { segs.iter().map(|s| s.map(str::to_lowercase)).collect() } else { segs };
                index.allow.insert(&segs, entry, mask);
            } else {
                index.strict.insert(&fold_segments(&segs), entry, mask);
            }
        }
        index
    }

    fn allow_input<'a>(&self, names: &'a [Cow<'a, str>]) -> Vec<Cow<'a, str>> {
        names.iter().map(|n| if self.caseless { Cow::Owned(n.to_lowercase()) } else { Cow::Borrowed(&**n) }).collect()
    }
}

#[derive(Default)]
struct Resolver {
    dirs: FxHashMap<PathBuf, (PathBuf, Option<Listing>)>,
}

enum Listing {
    Missing,
    Names {
        files: hashbrown::HashSet<std::ffi::OsString, crate::vm::value::FxBuild>,
        folded: Option<hashbrown::HashSet<String, crate::vm::value::FxBuild>>,
    },
}

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

#[derive(Debug, Default)]
struct Index {
    named: HostIndex<NetEntry>,
    resolved: HostIndex<NetEntry>,
    http: HostIndex<u32>,
    groups: Vec<HttpGroup>,
    listen: Vec<(ListenPattern, Decision, u32)>,
    env_names: FxHashMap<String, Vec<(Decision, u32)>>,
    env: Vec<(NamePattern, Decision, u32)>,
    unit: Vec<(Capability, Decision, u32)>,
    custom: FxHashMap<String, Vec<(Option<String>, Decision, u32)>>,
    any_custom: Vec<(Option<String>, Decision, u32)>,
}

#[derive(Debug)]
struct NetEntry {
    rule: u32,
    decision: Decision,
    transports: Transports,
    rank: Rank,
}

#[derive(Debug, Default)]
struct HttpGroup {
    /// Allow rules, which compare path names exactly.
    allow: Trie<HttpEntry>,
    /// Deny and prompt rules, which compare folded path names.
    strict: Trie<HttpEntry>,
}

#[derive(Debug)]
struct HttpEntry {
    rule: u32,
    decision: Decision,
    methods: Option<Vec<String>>,
    transports: Transports,
    rank: Rank,
}

impl Index {
    fn build(rules: &[Rule]) -> Index {
        let mut index = Index::default();
        let mut groups: FxHashMap<&NetPattern, u32> = FxHashMap::default();
        for (i, rule) in rules.iter().enumerate() {
            let (rule_id, decision) = (i as u32, rule.decision);
            if let Some(http) = rule.kind.http_match() {
                let rank = Rank::http(&http);
                for &connection in rule.kind.connections() {
                    let entry = NetEntry { rule: rule_id, decision, transports: http.transports, rank };
                    match connection {
                        Connection::Named => index.named.insert(http.host, entry),
                        Connection::Resolved => index.resolved.insert(http.host, entry),
                    }
                }
                let group = *groups.entry(http.host).or_insert_with(|| {
                    index.http.insert(http.host, index.groups.len() as u32);
                    index.groups.push(HttpGroup::default());
                    index.groups.len() as u32 - 1
                });
                let methods = http.methods.map(<[String]>::to_vec);
                let entry = HttpEntry { rule: rule_id, decision, methods, transports: http.transports, rank };
                let group = &mut index.groups[group as usize];
                if rule.exact() {
                    group.allow.insert(http.path, entry, 1);
                } else {
                    group.strict.insert(&fold_segments(http.path), entry, 1);
                }
            }
            match &rule.kind {
                RuleKind::Files(..) | RuleKind::Net(..) | RuleKind::Http(..) => {}
                RuleKind::Listen(p) => index.listen.push((p.clone(), decision, rule_id)),
                RuleKind::Env(NamePattern::Exact(n)) => index.env_names.entry(n.clone()).or_default().push((decision, rule_id)),
                RuleKind::Env(p) => index.env.push((p.clone(), decision, rule_id)),
                RuleKind::Stdin => index.unit.push((Capability::Stdin, decision, rule_id)),
                RuleKind::Stdout => index.unit.push((Capability::Stdout, decision, rule_id)),
                RuleKind::Stderr => index.unit.push((Capability::Stderr, decision, rule_id)),
                RuleKind::TimeZone => index.unit.push((Capability::TimeZone, decision, rule_id)),
                RuleKind::Custom(Some(n), d) => index.custom.entry(n.clone()).or_default().push((d.clone(), decision, rule_id)),
                RuleKind::Custom(None, d) => index.any_custom.push((d.clone(), decision, rule_id)),
            }
        }
        index
    }
}

#[cfg(all(test, feature = "os", unix))]
mod tests {
    use super::*;

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
