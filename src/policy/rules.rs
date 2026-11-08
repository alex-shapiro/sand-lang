//! Rule-based capability policies.

use super::file;
use super::net::{Dest, HostIndex, HostKind, ListenPattern, NetPattern, UrlPattern, parse_ip, private_ranges, url_segments};
use super::path::{Glob, Seg, Tok, Trie, path_segments, unquoted_tokens};
use super::{Capability, Decision, PathSpace, PatternError, fold_name, lexical, normalize_path};
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
    /// Bitmask of file operations governed by a rule with this access mode and decision.
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

/// A static absolute path for a file rule. Convert it to a literal [`PathPattern`] with
/// [`From`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AbsolutePath(&'static str);

impl AbsolutePath {
    /// Validates that `path` is absolute.
    ///
    /// A path that starts with `/` is absolute on all platforms. On Windows, a path that
    /// starts with `\` or a drive prefix such as `C:\` is also absolute. Panics if `path`
    /// is relative.
    ///
    /// ```compile_fail
    /// use sandlang::policy::rules::AbsolutePath;
    /// const DATA: AbsolutePath = AbsolutePath::new("data");
    /// ```
    pub const fn new(path: &'static str) -> AbsolutePath {
        assert!(super::path::is_absolute(path.as_bytes()), "the path is not absolute");
        AbsolutePath(path)
    }
}

impl From<AbsolutePath> for PathPattern {
    fn from(path: AbsolutePath) -> PathPattern {
        PathPattern::from_absolute(Path::new(path.0))
    }
}

/// A path pattern for a file rule, such as `/data`, `/srv/**/*.log`, or `**/.env`.
///
/// A rule for a path also applies to every path under it. Parse a wildcard pattern with
/// [`str::parse`]. Build a literal pattern with [`PathPattern::literal`] or
/// [`AbsolutePath`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathPattern {
    pub(crate) prefix: PathBuf,
    pub(crate) rest: Vec<Seg>,
}

impl std::str::FromStr for PathPattern {
    type Err = PatternError;

    fn from_str(text: &str) -> Result<PathPattern, PatternError> {
        super::file::parse_path(&unquoted_tokens(text), false, None, None).map_err(PatternError)
    }
}

impl PathPattern {
    /// Creates a pattern that matches `path` and its descendants literally. Returns an
    /// error if `path` is relative.
    pub fn literal(path: impl AsRef<Path>) -> Result<PathPattern, PatternError> {
        let path = path.as_ref();
        if !super::path::is_absolute(path.as_os_str().as_encoded_bytes()) {
            return Err(PatternError(format!("path `{}` is not anchored: write an absolute path", path.display())));
        }
        Ok(PathPattern::from_absolute(path))
    }

    fn from_absolute(path: &Path) -> PathPattern {
        PathPattern { prefix: lexical(path), rest: vec![] }
    }

    /// Returns a pattern that matches every path.
    pub fn everything() -> PathPattern {
        PathPattern { prefix: PathBuf::new(), rest: vec![] }
    }

    pub(crate) fn segments(&self, normalize: impl FnOnce(&Path) -> PathBuf) -> Vec<Seg> {
        if self.prefix.as_os_str().is_empty() {
            return self.rest.clone();
        }
        file_segments(&normalize(&self.prefix), &self.rest)
    }
}

/// An environment variable name pattern for an `env` rule, such as `HOME`, `AWS_*`, or `*`.
///
/// Parse a wildcard pattern with [`str::parse`]. Build a literal pattern with
/// [`NamePattern::exact`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NamePattern(pub(crate) NameKind);

impl NamePattern {
    /// Creates a pattern that matches `name` literally.
    pub fn exact(name: impl Into<String>) -> NamePattern {
        NamePattern(NameKind::Exact(name.into()))
    }
}

impl std::str::FromStr for NamePattern {
    type Err = PatternError;

    fn from_str(name: &str) -> Result<NamePattern, PatternError> {
        NameKind::from_tokens(&unquoted_tokens(name)).map(NamePattern).map_err(PatternError)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NameKind {
    Exact(String),
    Glob(Glob),
    Any,
}

impl NameKind {
    pub(crate) fn from_tokens(text: &[Tok]) -> Result<NameKind, String> {
        if text.windows(2).any(|w| w[0].1 && w[1].1) {
            return Err("cannot use `**` in an environment variable pattern: use `*` to match any characters, e.g. `AWS_*`".into());
        }
        Ok(match Seg::parse(text)? {
            Seg::Lit(s) => NameKind::Exact(s.into()),
            Seg::Glob(g) => NameKind::Glob(g),
            Seg::Star | Seg::GlobStar => NameKind::Any,
        })
    }
}

/// The transports that a `net` or `http` rule controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transports {
    /// Encrypted TLS connections.
    Tls,
    /// Unencrypted connections.
    Plaintext,
    /// Both TLS and plaintext connections.
    Both,
}

impl Transports {
    /// Returns the default transports for a rule without transport keywords.
    pub(crate) fn default_for(decision: Decision) -> Transports {
        if decision == Decision::Allow { Transports::Tls } else { Transports::Both }
    }

    /// Returns `true` if `self` includes the transport indicated by `tls`.
    pub(crate) fn controls(self, tls: bool) -> bool {
        match self {
            Transports::Tls => tls,
            Transports::Plaintext => !tls,
            Transports::Both => true,
        }
    }
}

/// An HTTP request method for an `http` rule, such as `GET` or `M-SEARCH`.
///
/// [`str::parse`] accepts standard methods in any case. It also accepts custom uppercase
/// tokens made of ASCII letters, digits, `-`, and `_`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpMethod(Cow<'static, str>);

impl HttpMethod {
    pub const GET: HttpMethod = HttpMethod(Cow::Borrowed("GET"));
    pub const HEAD: HttpMethod = HttpMethod(Cow::Borrowed("HEAD"));
    pub const POST: HttpMethod = HttpMethod(Cow::Borrowed("POST"));
    pub const PUT: HttpMethod = HttpMethod(Cow::Borrowed("PUT"));
    pub const PATCH: HttpMethod = HttpMethod(Cow::Borrowed("PATCH"));
    pub const DELETE: HttpMethod = HttpMethod(Cow::Borrowed("DELETE"));
    pub const OPTIONS: HttpMethod = HttpMethod(Cow::Borrowed("OPTIONS"));
    pub const TRACE: HttpMethod = HttpMethod(Cow::Borrowed("TRACE"));
    pub const CONNECT: HttpMethod = HttpMethod(Cow::Borrowed("CONNECT"));

    /// Standard HTTP methods that [`str::parse`] accepts in any case.
    const KNOWN: [HttpMethod; 9] =
        [Self::GET, Self::HEAD, Self::POST, Self::PUT, Self::PATCH, Self::DELETE, Self::OPTIONS, Self::TRACE, Self::CONNECT];

    /// Returns the method name in uppercase, such as `"GET"`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for HttpMethod {
    type Err = PatternError;

    fn from_str(word: &str) -> Result<HttpMethod, PatternError> {
        let known = HttpMethod::KNOWN.into_iter().find(|m| m.as_str().eq_ignore_ascii_case(word));
        let token = word.bytes().any(|b| b.is_ascii_uppercase())
            && word.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if file::transport(word).is_some() || file::invalid_transport_hint(word).is_some() || (known.is_none() && !token) {
            return Err(PatternError(format!(
                "`{word}` is not an HTTP method: expected a method such as `GET`, or an uppercase token such as `M-SEARCH`"
            )));
        }
        Ok(known.unwrap_or_else(|| HttpMethod(Cow::Owned(word.into()))))
    }
}

/// The HTTP methods that an `http` rule matches.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Methods(Option<Vec<HttpMethod>>);

impl Methods {
    /// Matches every HTTP method.
    pub fn all() -> Methods {
        Methods(None)
    }

    /// Matches only `first` and the methods in `rest`.
    pub fn only(first: HttpMethod, rest: impl IntoIterator<Item = HttpMethod>) -> Methods {
        let mut list = vec![first];
        for method in rest {
            if !list.contains(&method) {
                list.push(method);
            }
        }
        Methods(Some(list))
    }

    /// Returns the specific methods to match, or `None` if `self` matches every method.
    pub(crate) fn listed(&self) -> Option<&[HttpMethod]> {
        self.0.as_deref()
    }

    /// Returns `true` if `self` matches the uppercase method name `method`.
    pub(crate) fn matches(&self, method: &str) -> bool {
        self.0.as_ref().is_none_or(|list| list.iter().any(|m| m.as_str() == method))
    }
}

/// Matches all HTTP methods for a `net` rule.
static ALL_METHODS: Methods = Methods(None);

/// A name or detail pattern for a custom capability rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CustomMatch {
    /// Matches any name or detail.
    Any,
    /// Matches this exact string literally.
    Exact(String),
}

impl CustomMatch {
    pub(crate) fn matches(&self, text: &str) -> bool {
        match self {
            CustomMatch::Any => true,
            CustomMatch::Exact(exact) => exact == text,
        }
    }
}

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
    Http(Methods, UrlPattern, Transports),
    Listen(ListenPattern),
    Env(NamePattern),
    Stdin,
    Stdout,
    Stderr,
    TimeZone,
    Custom(CustomMatch, CustomMatch),
}

impl RuleKind {
    pub(crate) fn http_match(&self) -> Option<HttpMatch<'_>> {
        match self {
            RuleKind::Http(methods, url, transports) => {
                Some(HttpMatch { methods, transports: *transports, host: &url.host, path: &url.path })
            }
            RuleKind::Net(host, transports) => Some(HttpMatch { methods: &ALL_METHODS, transports: *transports, host, path: &[] }),
            _ => None,
        }
    }

    /// Returns `true` if the rule controls raw TCP connections.
    pub(crate) fn connects(&self) -> bool {
        matches!(self, RuleKind::Net(..))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HttpMatch<'a> {
    pub methods: &'a Methods,
    pub transports: Transports,
    pub host: &'a NetPattern,
    pub path: &'a [Seg],
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub decision: Decision,
    pub kind: RuleKind,
    pub origin: Option<Origin>,
    /// Rule tier, where `0` is for primary rules and higher values are for fallback rules added via [`RulePolicy::with_defaults`].
    pub tier: u32,
}

impl Rule {
    pub(crate) fn new(decision: Decision, kind: RuleKind) -> Rule {
        Rule { decision, kind, origin: None, tier: 0 }
    }

    /// Returns `true` if the rule matches only exact, case-sensitive names and canonical IP representations.
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

/// A rule-based capability policy.
///
/// Evaluates each request using the most specific matching rule. On a
/// specificity tie, `deny` overrides `allow`, and `allow` overrides `prompt`.
/// If a request does not match any rule, [`RulePolicy::check`] returns
/// [`Decision::Prompt`].
#[derive(Clone, Default)]
pub struct RulePolicy {
    rules: Vec<Rule>,
    host_files: OnceLock<Arc<FileIndex>>,
    memory_files: OnceLock<Arc<FileIndex>>,
    non_file_index: OnceLock<Arc<Index>>,
}

impl fmt::Debug for RulePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RulePolicy").field("rules", &self.rules.len()).finish()
    }
}

impl RulePolicy {
    /// Creates an empty policy that returns [`Decision::Prompt`] for every request.
    pub fn new() -> RulePolicy {
        RulePolicy::default()
    }

    /// Creates a policy that allows `stdout` and `stderr`.
    pub fn standard() -> RulePolicy {
        RulePolicy::new().allow_stdout().allow_stderr()
    }

    /// Creates a policy that allows all capabilities. Use this only for trusted scripts.
    pub fn allow_all() -> RulePolicy {
        let rules = [
            RuleKind::Files(Access::Any, PathPattern::everything()),
            RuleKind::Files(Access::Import, PathPattern::everything()),
            RuleKind::Net(NetPattern { kind: HostKind::Any, port: None }, Transports::Both),
            RuleKind::Listen(ListenPattern::ANY),
            RuleKind::Env(NamePattern(NameKind::Any)),
            RuleKind::Stdin,
            RuleKind::Stdout,
            RuleKind::Stderr,
            RuleKind::TimeZone,
            RuleKind::Custom(CustomMatch::Any, CustomMatch::Any),
        ];
        rules.into_iter().fold(RulePolicy::new(), |p, kind| p.push(Decision::Allow, kind))
    }

    /// Appends the rules of `other` to this policy.
    pub fn extend(mut self, other: RulePolicy) -> RulePolicy {
        self.rules.extend(other.rules);
        self.changed()
    }

    /// Appends `defaults` as fallback rules that apply only if the rules in `self` do not match a request.
    pub fn with_defaults(mut self, defaults: RulePolicy) -> RulePolicy {
        let below = self.rules.iter().map(|r| r.tier).max().unwrap_or(0) + 1;
        self.rules.extend(defaults.rules.into_iter().map(|r| Rule { tier: r.tier + below, ..r }));
        self.changed()
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub(crate) fn push_rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self.changed()
    }

    fn changed(mut self) -> Self {
        self.host_files = OnceLock::new();
        self.memory_files = OnceLock::new();
        self.non_file_index = OnceLock::new();
        self
    }

    fn push(self, decision: Decision, kind: RuleKind) -> Self {
        self.push_rule(Rule::new(decision, kind))
    }

    pub(crate) fn extend_rules(self, rules: Vec<Rule>) -> Self {
        rules.into_iter().fold(self, RulePolicy::push_rule)
    }

    /// Allows reading paths that match `path`.
    pub fn allow_read(self, path: PathPattern) -> Self {
        self.push(Decision::Allow, RuleKind::Files(Access::Read, path))
    }
    /// Denies reading, writing, and importing paths that match `path`.
    pub fn deny_read(self, path: PathPattern) -> Self {
        self.push(Decision::Deny, RuleKind::Files(Access::Read, path))
    }
    /// Prompts before reading, writing, or importing paths that match `path`.
    pub fn prompt_read(self, path: PathPattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Files(Access::Read, path))
    }
    /// Allows reading and writing paths that match `path`.
    pub fn allow_write(self, path: PathPattern) -> Self {
        self.push(Decision::Allow, RuleKind::Files(Access::Write, path))
    }
    /// Denies writing paths that match `path`.
    pub fn deny_write(self, path: PathPattern) -> Self {
        self.push(Decision::Deny, RuleKind::Files(Access::Write, path))
    }
    /// Prompts before writing paths that match `path`.
    pub fn prompt_write(self, path: PathPattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Files(Access::Write, path))
    }
    /// Allows importing script modules that match `path`.
    pub fn allow_import(self, path: PathPattern) -> Self {
        self.push(Decision::Allow, RuleKind::Files(Access::Import, path))
    }
    /// Denies importing script modules that match `path`.
    pub fn deny_import(self, path: PathPattern) -> Self {
        self.push(Decision::Deny, RuleKind::Files(Access::Import, path))
    }
    /// Prompts before importing script modules that match `path`.
    pub fn prompt_import(self, path: PathPattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Files(Access::Import, path))
    }
    /// Allows connections over `transports` to hosts that match `host`.
    pub fn allow_net(self, transports: Transports, host: NetPattern) -> Self {
        self.push(Decision::Allow, RuleKind::Net(host, transports))
    }
    /// Denies connections over `transports` to hosts that match `host`.
    pub fn deny_net(self, transports: Transports, host: NetPattern) -> Self {
        self.push(Decision::Deny, RuleKind::Net(host, transports))
    }
    /// Prompts before connections over `transports` to hosts that match `host`.
    pub fn prompt_net(self, transports: Transports, host: NetPattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Net(host, transports))
    }
    /// Allows HTTP requests that match `methods` and `url` over `transports`.
    pub fn allow_http(self, transports: Transports, methods: Methods, url: UrlPattern) -> Self {
        self.push(Decision::Allow, RuleKind::Http(methods, url, transports))
    }
    /// Denies HTTP requests that match `methods` and `url` over `transports`.
    pub fn deny_http(self, transports: Transports, methods: Methods, url: UrlPattern) -> Self {
        self.push(Decision::Deny, RuleKind::Http(methods, url, transports))
    }
    /// Prompts before HTTP requests that match `methods` and `url` over `transports`.
    pub fn prompt_http(self, transports: Transports, methods: Methods, url: UrlPattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Http(methods, url, transports))
    }
    /// Denies connections to `localhost` and private network addresses.
    ///
    /// Adds a `deny net` rule for `localhost` and for each address range in
    /// [`super::net::is_private_address`]. A more specific address rule, such as
    /// `allow net 10.0.0.5`, overrides these rules for all allowed hostnames.
    /// Rules of equal specificity, such as `allow net localhost`, and hostname
    /// rules, such as `allow net api.example.com`, do not override them.
    /// Hostname rules do not match IP literals. To allow a local server, list
    /// each host form: `allow net localhost:8080 127.0.0.1:8080 [::1]:8080`.
    ///
    /// Add these rules to the primary rules. In a [`RulePolicy::with_defaults`]
    /// tier, they do not apply to requests that match a primary rule.
    ///
    /// In the WebAssembly build, the browser resolves hostnames, so these rules
    /// block only IP literals and `localhost` names.
    pub fn deny_private_net(self) -> Self {
        let localhost = NetPattern { kind: HostKind::Host("localhost".into()), port: None };
        let hosts = std::iter::once(localhost).chain(private_ranges().iter().cloned());
        hosts.fold(self, |policy, host| policy.deny_net(Transports::Both, host))
    }
    /// Allows binding a listener to an address that matches `pattern`.
    pub fn allow_listen(self, pattern: ListenPattern) -> Self {
        self.push(Decision::Allow, RuleKind::Listen(pattern))
    }
    /// Denies binding a listener to an address that matches `pattern`.
    pub fn deny_listen(self, pattern: ListenPattern) -> Self {
        self.push(Decision::Deny, RuleKind::Listen(pattern))
    }
    /// Prompts before binding a listener to an address that matches `pattern`.
    pub fn prompt_listen(self, pattern: ListenPattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Listen(pattern))
    }
    /// Allows reading environment variables whose names match `name`.
    pub fn allow_env(self, name: NamePattern) -> Self {
        self.push(Decision::Allow, RuleKind::Env(name))
    }
    /// Denies reading environment variables whose names match `name`.
    pub fn deny_env(self, name: NamePattern) -> Self {
        self.push(Decision::Deny, RuleKind::Env(name))
    }
    /// Prompts before reading environment variables whose names match `name`.
    pub fn prompt_env(self, name: NamePattern) -> Self {
        self.push(Decision::Prompt, RuleKind::Env(name))
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
    /// Allows custom capabilities that match `name` and `detail`.
    pub fn allow_custom(self, name: CustomMatch, detail: CustomMatch) -> Self {
        self.push(Decision::Allow, RuleKind::Custom(name, detail))
    }
    /// Denies custom capabilities that match `name` and `detail`.
    pub fn deny_custom(self, name: CustomMatch, detail: CustomMatch) -> Self {
        self.push(Decision::Deny, RuleKind::Custom(name, detail))
    }
    /// Prompts before custom capabilities that match `name` and `detail`.
    pub fn prompt_custom(self, name: CustomMatch, detail: CustomMatch) -> Self {
        self.push(Decision::Prompt, RuleKind::Custom(name, detail))
    }

    /// Evaluates `cap`, normalizing file paths with [`super::normalize_path`].
    pub fn check(&self, cap: &Capability) -> Decision {
        self.check_in(cap, PathSpace::Host)
    }

    /// Evaluates `cap` if its file path is already normalized with [`super::normalize_path`].
    pub fn check_normalized(&self, cap: &Capability) -> Decision {
        self.check_normalized_in(cap, PathSpace::Host)
    }

    /// Evaluates `cap` for a directory entry operation such as removal or
    /// rename without following a final symlink.
    pub fn check_entry(&self, cap: &Capability) -> Decision {
        self.check_entry_in(cap, PathSpace::Host)
    }

    /// Returns `true` if a deny or prompt rule can match `path` or a descendant.
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
                if let Some(d) = self.unnormalized_name_decision(&with(t.path.clone()), space) {
                    return d;
                }
                t.fix_spelling();
            }
            return self.check_normalized_in(&with(t.path), space);
        }
        let _ = mode;
        let normal = match space {
            PathSpace::Host => lexical(p),
            PathSpace::Memory => crate::files::mem::normalize(p),
        };
        self.check_normalized_in(&with(normal), space)
    }

    pub(crate) fn check_normalized_in(&self, cap: &Capability, space: PathSpace) -> Decision {
        if let Capability::NetAddr { host, addr, tls } = cap {
            return self.check_resolved(&Capability::Net { host: host.clone(), port: addr.port(), tls: *tls }, *addr);
        }
        self.verdict(cap, space, &[]).map_or(Decision::Prompt, |(d, _)| d)
    }

    /// Checks whether an allowed connection from a `Net` or `Http` `request` is permitted.
    ///
    /// Uses IP and CIDR rules to match `addr`, plus `localhost` rules if applicable.
    /// The policy ranks address rules using the same order as for an IP literal,
    /// with the rule that allowed `request` in place of `*`. A matching address rule
    /// overrides the allowing rule unless the address rule is in a lower [`RulePolicy::with_defaults`] tier.
    /// The allowing rule acts as `allow`, even if it is an approved `prompt` rule.
    /// Any other `prompt` address rule denies the connection.
    pub fn check_resolved(&self, request: &Capability, addr: std::net::SocketAddr) -> Decision {
        if self.resolved_verdict(request, addr).is_ok() { Decision::Allow } else { Decision::Deny }
    }

    pub(crate) fn resolved_verdict(&self, request: &Capability, addr: std::net::SocketAddr) -> Result<(), Option<usize>> {
        let (Capability::Net { host, .. } | Capability::Http { host, .. }) = request else { return Ok(()) };
        let ip = addr.ip().to_canonical();
        if let Some(literal) = parse_ip(host) {
            return if literal == ip { Ok(()) } else { Err(None) };
        }
        let index = self.non_file_index();
        let dest = Dest::new(host, addr.port());
        // The rule that allowed `request` acts as `allow`, even if it is an approved `prompt` rule.
        let address_verdict = |allowing: Option<(Decision, usize)>, offer_address_rules: &dyn Fn(&mut Best)| {
            let allowing = allowing.filter(|&(d, _)| d != Decision::Deny).map(|(_, rule)| rule);
            let mut best = Best { approved: allowing, ..Best::new(self, &[]) };
            offer_address_rules(&mut best);
            if let Some(rule) = allowing {
                best.offer([0; 4], Decision::Allow, rule as u32);
            }
            match best.verdict() {
                Some((Decision::Allow, _)) | None => Ok(()),
                Some((_, rule)) => Err(Some(rule)),
            }
        };
        let tls = uses_tls(request);
        if let Capability::Http { method, path, .. } = request {
            let (names, alt) = url_segments(path);
            return std::iter::once(names).chain(alt).try_for_each(|names| {
                let allowing = self.check_http_segments(request, &names, &[], &|f| index.http.find(&dest, f));
                address_verdict(allowing, &|best| self.http_verdict(&|f| index.http.find_address(&dest, ip, f), method, tls, &names, best))
            });
        }
        let allowing = self.verdict(request, PathSpace::Host, &[]);
        address_verdict(allowing, &|best| index.net.find_address(&dest, ip, |is_alt, e| e.offer_to(best, tls, is_alt)))
    }

    pub(crate) fn denies_under_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        let index = self.file_index(space);
        !index.strict.is_empty() && index.strict.any_below(&refs(&folded(&path_segments(path))), READ | WRITE)
    }

    pub(crate) fn protects_normalized_in(&self, path: &Path, space: PathSpace) -> bool {
        [READ, WRITE].into_iter().any(|op| {
            let mut best = Best::new(self, &[]);
            self.file_verdict(path, op, space, &mut best);
            best.verdict().is_some_and(|(d, _)| d != Decision::Allow)
        })
    }

    pub(crate) fn case_insensitive_decision(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        self.unnormalized_name_decision(cap, space).filter(|&d| d != Decision::Prompt)
    }

    fn unnormalized_name_decision(&self, cap: &Capability, space: PathSpace) -> Option<Decision> {
        let (Capability::FsRead(p) | Capability::FsWrite(p) | Capability::Import(p)) = cap else { return None };
        let index = self.file_index(space);
        if index.allow.matches_last(&refs(&index.allow_input(&path_segments(p)))) {
            return None;
        }
        Some(self.check_normalized_in(cap, space))
    }

    pub(crate) fn verdict(&self, cap: &Capability, space: PathSpace, exclude: &[usize]) -> Option<(Decision, usize)> {
        let mut best = Best::new(self, exclude);
        let index = self.non_file_index();
        match cap {
            Capability::FsRead(p) => self.file_verdict(p, READ, space, &mut best),
            Capability::FsWrite(p) => self.file_verdict(p, WRITE, space, &mut best),
            Capability::Import(p) => self.file_verdict(p, IMPORT, space, &mut best),
            Capability::Net { host, port, tls } => self.net_verdict(&Dest::new(host, *port), *tls, &mut best),
            Capability::NetAddr { .. } => {}
            Capability::Http { host, port, .. } => {
                let dest = Dest::new(host, *port);
                return self.check_http(cap, exclude, &|f| index.http.find(&dest, f));
            }
            Capability::NetListen(addr) => {
                for &(ref pattern, d, rule) in &index.listen {
                    if let Some(score) = pattern.matches(*addr) {
                        best.offer([score, 0, 0, 0], d, rule);
                    }
                }
            }
            Capability::Env(name) if name == "*" => {
                // Evaluate `Env("*")` against a representative sample for each rule and an unmatched character so the result equals the strictest decision for any variable.
                let pieces = || index.env.iter().flat_map(|(p, ..)| if let NameKind::Glob(g) = p { g.pieces() } else { &[] });
                let used: std::collections::HashSet<char> =
                    index.env_names.keys().map(|n| &**n).chain(pieces().map(|p| &**p)).flat_map(str::chars).collect();
                let unused = (0..=u32::from(char::MAX)).filter_map(char::from_u32).find(|c| !used.contains(c)).unwrap_or('\0');
                let samples = index.env_names.keys().cloned().chain(index.env.iter().map(|(p, ..)| match p {
                    NameKind::Glob(g) => g.pieces().join(&*unused.to_string()),
                    _ => unused.to_string(),
                }));
                let verdicts = samples.chain([unused.to_string()]).map(|n| self.env_verdict(&n, exclude));
                return verdicts.max_by_key(|&v| strictness(v)).flatten();
            }
            Capability::Env(name) => return self.env_verdict(name, exclude),
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
                    if detail_pattern.matches(detail) {
                        let exact_detail = matches!(detail_pattern, CustomMatch::Exact(_));
                        best.offer([2 * named as u32 + exact_detail as u32, 0, 0, 0], d, rule);
                    }
                }
            }
        }
        best.verdict()
    }

    fn env_verdict(&self, name: &str, exclude: &[usize]) -> Option<(Decision, usize)> {
        let index = self.non_file_index();
        let mut best = Best::new(self, exclude);
        for &(d, rule) in index.env_names.get(name).into_iter().flatten() {
            best.offer([2, 0, 0, 0], d, rule);
        }
        for &(ref pattern, d, rule) in &index.env {
            let spec = match pattern {
                NameKind::Glob(g) if g.matches(name) => [1, g.pieces().iter().map(|p| p.len() as u32).sum(), 0, 0],
                NameKind::Any => [0; 4],
                _ => continue,
            };
            best.offer(spec, d, rule);
        }
        best.verdict()
    }

    fn file_verdict(&self, p: &Path, op: u8, space: PathSpace, best: &mut Best<'_>) {
        let index = self.file_index(space);
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
        self.non_file_index().net.find(dest, |is_alt, e| e.offer_to(best, tls, is_alt));
    }

    fn check_http(&self, cap: &Capability, exclude: &[usize], find: &FindGroups) -> Option<(Decision, usize)> {
        let Capability::Http { path, .. } = cap else { return None };
        let (names, alt) = url_segments(path);
        let verdicts = std::iter::once(names).chain(alt).map(|names| self.check_http_segments(cap, &names, exclude, find));
        verdicts.reduce(|main, alt| if strictness(alt) > strictness(main) { alt } else { main }).flatten()
    }

    fn check_http_segments(&self, cap: &Capability, names: &[String], exclude: &[usize], find: &FindGroups) -> Option<(Decision, usize)> {
        let Capability::Http { method, .. } = cap else { return None };
        let mut best = Best::new(self, exclude);
        self.http_verdict(find, method, uses_tls(cap), names, &mut best);
        best.verdict()
    }

    fn http_verdict(&self, find: &FindGroups, method: &str, tls: bool, names: &[String], best: &mut Best<'_>) {
        let index = self.non_file_index();
        if index.groups.is_empty() {
            return;
        }
        let method = method.to_ascii_uppercase();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut folded_names = None;
        find(&mut |is_alt, &group| {
            let group = &index.groups[group as usize];
            let mut offer = |e: &HttpEntry, depth: usize| {
                if e.methods.matches(&method) && e.transports.controls(tls) {
                    best.offer(e.rank.spec(depth), e.decision, e.rule);
                }
            };
            if !group.allow.is_empty() && matches_address_form(Decision::Allow, is_alt) {
                group.allow.matches(&names, &mut offer);
            }
            if !group.strict.is_empty() {
                let folded = folded_names.get_or_insert_with(|| names.iter().map(|n| fold_name(n)).collect::<Vec<_>>());
                group.strict.matches(&refs(folded), &mut offer);
            }
        });
    }

    fn file_index(&self, space: PathSpace) -> &FileIndex {
        let cell = match space {
            PathSpace::Host => &self.host_files,
            PathSpace::Memory => &self.memory_files,
        };
        cell.get_or_init(|| Arc::new(FileIndex::build(&self.rules, space)))
    }

    fn non_file_index(&self) -> &Index {
        self.non_file_index.get_or_init(|| Arc::new(Index::build(&self.rules)))
    }
}

impl RulePolicy {
    pub(crate) fn describe_rule(&self, i: usize) -> String {
        let rule = &self.rules[i];
        match &rule.origin {
            Some(o) => format!("the rule `{rule}` on line {}", o.line),
            None => format!("the rule `{rule}`"),
        }
    }
}

impl fmt::Display for RulePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rules: Vec<&Rule> = self.rules.iter().collect();
        rules.sort_by_key(|r| r.tier);
        for rule in rules {
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
        PathSpace::Memory => crate::files::mem::normalize(p),
    }
}

fn folded<'a>(names: &'a [Cow<'a, str>]) -> Vec<Cow<'a, str>> {
    names.iter().map(|s| fold_name(s)).collect()
}

fn refs<S: AsRef<str>>(names: &[S]) -> Vec<&str> {
    names.iter().map(AsRef::as_ref).collect()
}

pub(crate) fn matches_address_form(decision: Decision, is_alternate_ip: bool) -> bool {
    !is_alternate_ip || decision != Decision::Allow
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
    pub host: u32,
    pub weight: u32,
    pub methods: bool,
}

impl Rank {
    pub(crate) fn file(segs: &[Seg]) -> Rank {
        Rank { host: 0, weight: segs.iter().map(Seg::weight).sum(), methods: false }
    }

    pub(crate) fn http(http: &HttpMatch) -> Rank {
        Rank { host: http.host.rank(), weight: http.path.iter().map(Seg::weight).sum(), methods: http.methods.listed().is_some() }
    }

    pub(crate) fn spec(self, depth: usize) -> Spec {
        [self.host, depth as u32, self.weight, self.methods as u32]
    }
}

pub(crate) fn decision_priority(decision: Decision) -> u8 {
    match decision {
        Decision::Deny => 2,
        Decision::Allow => 1,
        Decision::Prompt => 0,
    }
}

type FindGroups<'a> = dyn Fn(&mut dyn FnMut(bool, &u32)) + 'a;

fn strictness(v: Option<(Decision, usize)>) -> (u8, bool) {
    match v {
        None => (1, false),
        Some((Decision::Allow, _)) => (0, true),
        Some((Decision::Prompt, _)) => (1, true),
        Some((Decision::Deny, _)) => (2, true),
    }
}

pub(crate) type Precedence = (Spec, u8, std::cmp::Reverse<usize>);

pub(crate) fn precedence(spec: Spec, decision: Decision, rule: usize) -> Precedence {
    (spec, decision_priority(decision), std::cmp::Reverse(rule))
}

struct Best<'a> {
    exclude: &'a [usize],
    rules: &'a [Rule],
    /// Rule index that acts as `allow`, such as an approved `prompt` rule.
    approved: Option<usize>,
    best: Option<((std::cmp::Reverse<u32>, Precedence), Decision)>,
}

impl<'a> Best<'a> {
    fn new(policy: &'a RulePolicy, exclude: &'a [usize]) -> Best<'a> {
        Best { exclude, rules: &policy.rules, approved: None, best: None }
    }

    fn offer(&mut self, spec: Spec, decision: Decision, rule: u32) {
        let rule = rule as usize;
        let decision = if self.approved == Some(rule) { Decision::Allow } else { decision };
        let candidate = (std::cmp::Reverse(self.rules[rule].tier), precedence(spec, decision, rule));
        if !self.exclude.contains(&rule) && self.best.is_none_or(|(b, _)| candidate > b) {
            self.best = Some((candidate, decision));
        }
    }

    fn verdict(&self) -> Option<(Decision, usize)> {
        self.best.map(|((_, (.., rule)), decision)| (decision, rule.0))
    }
}

#[derive(Debug)]
struct FileIndex {
    allow: Trie<FileEntry>,
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
                PathSpace::Memory => crate::files::mem::normalize(prefix),
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
            // Regular files and nonexistent entries are resolved directly;
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
    net: HostIndex<NetEntry>,
    http: HostIndex<u32>,
    groups: Vec<HttpGroup>,
    listen: Vec<(ListenPattern, Decision, u32)>,
    env_names: FxHashMap<String, Vec<(Decision, u32)>>,
    env: Vec<(NameKind, Decision, u32)>,
    unit: Vec<(Capability, Decision, u32)>,
    custom: FxHashMap<String, Vec<(CustomMatch, Decision, u32)>>,
    any_custom: Vec<(CustomMatch, Decision, u32)>,
}

#[derive(Debug)]
struct NetEntry {
    rule: u32,
    decision: Decision,
    transports: Transports,
    rank: Rank,
}

impl NetEntry {
    /// Offers the rule to `best` if it matches the transport and the address form.
    fn offer_to(&self, best: &mut Best<'_>, tls: bool, is_alternate: bool) {
        if matches_address_form(self.decision, is_alternate) && self.transports.controls(tls) {
            best.offer(self.rank.spec(0), self.decision, self.rule);
        }
    }
}

#[derive(Debug, Default)]
struct HttpGroup {
    allow: Trie<HttpEntry>,
    strict: Trie<HttpEntry>,
}

#[derive(Debug)]
struct HttpEntry {
    rule: u32,
    decision: Decision,
    methods: Methods,
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
                if rule.kind.connects() {
                    index.net.insert(http.host, NetEntry { rule: rule_id, decision, transports: http.transports, rank });
                }
                let group = *groups.entry(http.host).or_insert_with(|| {
                    index.http.insert(http.host, index.groups.len() as u32);
                    index.groups.push(HttpGroup::default());
                    index.groups.len() as u32 - 1
                });
                let entry = HttpEntry { rule: rule_id, decision, methods: http.methods.clone(), transports: http.transports, rank };
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
                RuleKind::Env(NamePattern(NameKind::Exact(n))) => index.env_names.entry(n.clone()).or_default().push((decision, rule_id)),
                RuleKind::Env(NamePattern(p)) => index.env.push((p.clone(), decision, rule_id)),
                RuleKind::Stdin => index.unit.push((Capability::Stdin, decision, rule_id)),
                RuleKind::Stdout => index.unit.push((Capability::Stdout, decision, rule_id)),
                RuleKind::Stderr => index.unit.push((Capability::Stderr, decision, rule_id)),
                RuleKind::TimeZone => index.unit.push((Capability::TimeZone, decision, rule_id)),
                RuleKind::Custom(CustomMatch::Exact(n), d) => {
                    index.custom.entry(n.clone()).or_default().push((d.clone(), decision, rule_id))
                }
                RuleKind::Custom(CustomMatch::Any, d) => index.any_custom.push((d.clone(), decision, rule_id)),
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

#[cfg(test)]
mod props {
    use super::*;
    use proptest::prelude::*;

    fn strictness(d: Decision) -> u8 {
        match d {
            Decision::Allow => 0,
            Decision::Prompt => 1,
            Decision::Deny => 2,
        }
    }

    proptest! {
        #[test]
        fn the_env_listing_is_as_strict_as_any_variable(
            rules in prop::collection::vec((0..3usize, prop::sample::select(vec!["*", "A*", "AB*", "*_KEY", "A*B", "\"*\"", "HOME", "AWS_KEY", "AB", "*B*"])), 1..6),
            defaults in prop::collection::vec((0..3usize, prop::sample::select(vec!["*", "A*", "HOME", "*_KEY"])), 0..3),
        ) {
            let verbs = ["allow", "deny", "prompt"];
            let text = |rules: &[(usize, &str)]| rules.iter().map(|(v, n)| format!("{} env {n}\n", verbs[*v])).collect::<String>();
            let parse = |text: &str| super::super::file::PolicyFile::parse(text, Path::new("/"), None).unwrap().rules;
            let policy = parse(&text(&rules)).with_defaults(parse(&text(&defaults)));
            let listing = policy.check(&Capability::Env("*".into()));
            for name in ["HOME", "AWS_KEY", "AB", "A", "ABX_KEY", "AXB", "*", "X", "B", "XBX"] {
                let one = policy.check(&Capability::Env(name.into()));
                prop_assert!(strictness(listing) >= strictness(one), "{}{}listing: {:?}, {}: {:?}", text(&rules), text(&defaults), listing, name, one);
            }
        }
    }
}
