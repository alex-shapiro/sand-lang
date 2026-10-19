//! The policy file linter.

mod search;

use super::file::parse;
use super::net::{Dest, HostIndex, HostKind, NetPattern, ip_spellings, is_private_address};
use super::path::{Seg, path_segments};
use super::rules::{Access, IMPORT, READ, Rule, RuleKind, WRITE};
use super::{Capability, PathSpace, PolicyError, RulePolicy, fold_name};
use crate::lint::LintWarning;
use crate::vm::value::{FxBuild, FxHashMap};
use search::{Budget, Cand, Effect, decides_at, search};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::rc::Rc;

/// Lints a policy file (parsed as by [`super::PolicyFile::parse`]):
///
/// - `redundant-rule`: a rule is the same as a different rule, or other
///   rules make the same decisions as the rule;
/// - `overridden-rule`: rules with higher precedence decide all the requests
///   of the rule, and some of these decisions are different;
/// - `bypassable-http-deny`: a `deny http` rule for a host that `net` rules
///   let scripts connect to (a script can then send it any request);
/// - `unused-set`: no rule uses a `let` set.
///
/// The first two lints examine file rules and `http` rules. They compare
/// paths as they are written (they do not resolve symlinks). A lint reports
/// a rule if no decision changes when you remove the rule, also when this
/// result comes from many rules together. If you remove all the rules that
/// it reports, no decision changes. Some unnecessary rules are not reported:
/// rules whose analysis takes too long, and address rules when `net` rules
/// have address ranges.
pub fn lint_policy(text: &str, base: &Path, home: Option<&Path>) -> Result<Vec<LintWarning>, Vec<PolicyError>> {
    let parsed = parse(text, base, home)?;
    let deny_private = !parsed.file.private_net;
    let policy = if deny_private { parsed.file.rules.deny_private_networks() } else { parsed.file.rules };
    let rules = policy.rules();
    let mut warnings = Vec::new();
    let mut warn = |i: usize, code: &'static str, message: String| {
        if let Some(o) = &rules[i].origin {
            warnings.push(LintWarning { code, message, line: o.line, column: o.column });
        }
    };

    let files = FileRules::new(rules);
    let http = HttpRules::new(rules, deny_private);
    let mut first: FxHashMap<(bool, &RuleKind), usize> = FxHashMap::default();
    for (i, r) in rules.iter().enumerate() {
        first.entry((r.allow, &r.kind)).or_insert(i);
    }
    let allows_imports = |r: &Rule| r.allow && matches!(r.kind, RuleKind::Files(Access::Import, _));
    // With no `allow import` rule, imports no rule decides are allowed.
    let mut import_rules = rules.iter().filter(|r| allows_imports(r)).count();
    let mut removed = vec![false; rules.len()];
    let mut budget = Budget { states: 20_000, steps: 50_000_000 };
    let mut unneeded = Vec::new();
    // From the last rule back, each judged without those already found unneeded.
    for (i, rule) in rules.iter().enumerate().rev() {
        let copy = first[&(rule.allow, &rule.kind)];
        if copy != i {
            warn(i, "redundant-rule", format!("the same as {}", describe(rules, copy)));
        } else {
            let effect = match &rule.kind {
                RuleKind::Files(..) if allows_imports(rule) && import_rules == 1 => None,
                RuleKind::Files(..) => files.effect(i, &removed, &mut budget),
                RuleKind::Http(..) => http.effect(i, &removed, &mut budget),
                _ => None,
            };
            match effect {
                Some(e) if !e.backups.is_empty() || !e.overriders.is_empty() => unneeded.push((i, e)),
                _ => continue,
            }
        }
        removed[i] = true;
        if allows_imports(rule) {
            import_rules -= 1;
        }
    }
    for (i, mut effect) in unneeded {
        // Name the rules that stay: if no rule named stays, those deciding in
        // place of the rule among the rules that stay.
        if effect.backups.iter().chain(&effect.overriders).all(|&j| removed[j]) {
            removed[i] = false;
            let again = match &rules[i].kind {
                RuleKind::Files(..) => files.effect(i, &removed, &mut budget),
                _ => http.effect(i, &removed, &mut budget),
            };
            removed[i] = true;
            if let Some(again) = again.filter(|e| !e.backups.is_empty() || !e.overriders.is_empty()) {
                effect = again;
            }
        }
        for rules in [&mut effect.backups, &mut effect.overriders] {
            if rules.iter().any(|&j| !removed[j]) {
                rules.retain(|&j| !removed[j]);
            }
        }
        if let Some((code, message)) = verdict(rules, i, &effect) {
            warn(i, code, message);
        }
    }

    for (i, rule) in rules.iter().enumerate() {
        if !removed[i]
            && !rule.allow
            && matches!(rule.kind, RuleKind::Http(..))
            && let Some((j, target)) = http.bypass(&policy, i, &removed)
        {
            warn(i, "bypassable-http-deny", format!("{} lets scripts connect to {target} and send any request", describe(rules, j)));
        }
    }
    for (name, line, column, used) in parsed.sets {
        if !used {
            warnings.push(LintWarning { code: "unused-set", message: format!("no rule uses the set `{name}`"), line, column });
        }
    }
    warnings.sort_by_key(|w| (w.line, w.column));
    Ok(warnings)
}

/// The names of a path a pattern matches, its wildcards filled in.
fn sample(segs: &[Seg]) -> Vec<Cow<'_, str>> {
    segs.iter()
        .filter_map(|seg| match seg {
            Seg::Lit(n) => Some(Cow::Borrowed(&**n)),
            Seg::Glob(g) => Some(Cow::Owned(g.pieces().concat())),
            Seg::Star => Some(Cow::Borrowed("x")),
            Seg::GlobStar => None,
        })
        .collect()
}

/// The warning for rule `i`, which decides nothing differently from the
/// rules in `effect`; `None` if it applies to no request.
fn verdict(rules: &[Rule], i: usize, effect: &Effect) -> Option<(&'static str, String)> {
    let allow = rules[i].allow;
    let (code, deciders) = if !effect.backups.is_empty() {
        ("redundant-rule", &effect.backups)
    } else if effect.overriders.iter().all(|&j| rules[j].allow == allow) {
        ("redundant-rule", &effect.overriders)
    } else {
        ("overridden-rule", &effect.overriders)
    };
    if deciders.is_empty() {
        return None;
    }
    let (list, plural) = (describe_all(rules, deciders), deciders.len() > 1);
    let message = match (code, allow, plural) {
        ("overridden-rule", _, false) => format!("{list} takes precedence everywhere this rule applies"),
        ("overridden-rule", _, true) => format!("{list} take precedence everywhere this rule applies"),
        (_, true, false) => format!("{list} already allows this"),
        (_, true, true) => format!("{list} already allow this"),
        (_, false, false) => format!("{list} already denies this"),
        (_, false, true) => format!("{list} already deny this"),
    };
    Some((code, message))
}

fn describe(rules: &[Rule], i: usize) -> String {
    match &rules[i].origin {
        Some(o) => format!("line {} (`{}`)", o.line, o.text),
        None => "another rule".to_string(),
    }
}

/// `line 1 (...)`, `line 1 (...) and line 2 (...)`, ..., naming three at most.
fn describe_all(rules: &[Rule], set: &BTreeSet<usize>) -> String {
    let named: Vec<String> = set.iter().take(if set.len() > 3 { 2 } else { 3 }).map(|&i| describe(rules, i)).collect();
    match named.as_slice() {
        [one] => one.clone(),
        [a, b] if set.len() > 3 => format!("{a}, {b} and {} other rules", set.len() - 2),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
        [] => String::new(),
    }
}

/// Hashes of the leading literal names of a pattern (folded):
/// `prefixes[n]` of the first `n`. Two patterns can match the same path, or
/// one a path below the other's, only if the hashes of their names agree
/// as far as both have them.
fn prefixes(segs: &[Seg], hasher: &FxBuild) -> Vec<u64> {
    let names = segs.iter().map_while(|s| if let Seg::Lit(n) = s { Some(fold_name(n)) } else { None });
    std::iter::once(0)
        .chain(names.scan(0, |h, n| {
            *h = hasher.hash_one((*h, n));
            Some(*h)
        }))
        .collect()
}

/// Whether patterns with these [`prefixes`] may match related paths.
fn related(a: &[u64], b: &[u64]) -> bool {
    let n = a.len().min(b.len()) - 1;
    a[n] == b[n]
}

/// A file rule as the policy matches it in a memory file system.
struct FileRule {
    index: usize,
    allow: bool,
    /// The requests it decides (`READ`, `WRITE`, `IMPORT`).
    mask: u8,
    /// The pattern below the root (a deny rule's names folded).
    segs: Vec<Seg>,
    weight: u32,
    prefixes: Vec<u64>,
}

/// The file rules of a policy, indexed by the leading names of their
/// patterns.
struct FileRules {
    rules: Vec<FileRule>,
    /// The position in `rules` of each policy rule.
    at: Vec<usize>,
    /// The rules by all their leading names.
    exact: FxHashMap<u64, Vec<usize>>,
    /// The rules by each run of their leading names.
    within: FxHashMap<u64, Vec<usize>>,
}

impl FileRules {
    fn new(rules: &[Rule]) -> FileRules {
        let mut files =
            FileRules { rules: Vec::new(), at: vec![usize::MAX; rules.len()], exact: Default::default(), within: Default::default() };
        let hasher = FxBuild::default();
        for (index, rule) in rules.iter().enumerate() {
            let RuleKind::Files(access, pattern) = &rule.kind else { continue };
            let prefix = crate::files::memory_path(&pattern.prefix);
            let names = path_segments(&prefix);
            let raw = names.iter().skip(1).map(|s| Seg::Lit((**s).into())).chain(pattern.rest.iter().cloned());
            let segs: Vec<Seg> = if rule.allow { raw.collect() } else { raw.map(|s| s.map(|n| fold_name(n).into_owned())).collect() };
            let prefixes = prefixes(&segs, &hasher);
            let at = files.rules.len();
            for &h in &prefixes {
                files.within.entry(h).or_default().push(at);
            }
            files.exact.entry(prefixes[prefixes.len() - 1]).or_default().push(at);
            files.at[index] = at;
            files.rules.push(FileRule {
                index,
                allow: rule.allow,
                mask: access.mask(rule.allow),
                weight: segs.iter().map(Seg::weight).sum(),
                segs,
                prefixes,
            });
        }
        files
    }

    /// The rules that may match a path at, above or below one `rule` matches,
    /// but for those `removed`.
    fn related(&self, rule: &FileRule, removed: &[bool]) -> Vec<&FileRule> {
        let (&last, above) = rule.prefixes.split_last().expect("the empty prefix");
        let above = above.iter().flat_map(|h| self.exact.get(h).into_iter().flatten());
        let mut related: Vec<&FileRule> = above
            .chain(self.within.get(&last).into_iter().flatten())
            .map(|&a| &self.rules[a])
            .filter(|r| !removed[r.index] && related(&r.prefixes, &rule.prefixes))
            .collect();
        related.sort_unstable_by_key(|r| r.index);
        related.dedup_by_key(|r| r.index);
        related
    }

    /// What decides in place of file rule `i` (with the rules `removed`
    /// gone), or `None` if it is needed.
    fn effect(&self, i: usize, removed: &[bool], budget: &mut Budget) -> Option<Effect> {
        let me = &self.rules[self.at[i]];
        let related = self.related(me, removed);
        budget.spend(related.len())?;
        let names = sample(&me.segs);
        let mut effect = Effect::default();
        for op in [READ, WRITE, IMPORT].into_iter().filter(|&op| me.mask & op != 0) {
            let cands: Vec<Cand> = related
                .iter()
                .filter(|r| r.mask & op != 0)
                .map(|r| Cand { rule: r.index, allow: r.allow, head: [0, 0], tail: [r.weight, 0], segs: &r.segs })
                .collect();
            let at = cands.iter().position(|c| c.rule == i)?;
            budget.spend(cands.len() * (names.len() + 1))?;
            if decides_at(&cands, at, &names) || !search(&cands, at, &names, &mut effect, budget) {
                return None;
            }
        }
        Some(effect)
    }
}

/// An `http` or `net` rule as the policy matches http requests.
struct HttpRule<'r> {
    index: usize,
    allow: bool,
    host: &'r NetPattern,
    scheme: Option<&'static str>,
    methods: Option<&'r [String]>,
    /// The path pattern (a deny rule's names folded); none for `net` rules.
    segs: Vec<Seg>,
    weight: u32,
    prefixes: Vec<u64>,
}

/// A host pattern matching a destination: the rule's position, how
/// narrowly the pattern names it ([`HostIndex::find`]'s score, ports
/// aside), whether it names it without wildcards, and whether it matched
/// another spelling of its address.
#[derive(Clone, Copy)]
struct Hit {
    at: u32,
    score: u32,
    explicit: bool,
    spelled: bool,
}

/// The `http` and `net` rules of a policy, with destinations that tell
/// their host patterns apart.
struct HttpRules<'r> {
    rules: Vec<HttpRule<'r>>,
    at: Vec<usize>,
    /// The host patterns without their ports.
    hosts: HostIndex<u32>,
    deny_private: bool,
    /// A label no host pattern begins with: `{fresh}.{suffix}` stands for
    /// the names that `*.{suffix}` matches and no other pattern names.
    fresh: String,
    /// The destinations standing for the names below each domain.
    under: FxHashMap<String, Vec<String>>,
    /// The destinations standing for every host name: those of the name
    /// patterns, one no pattern matches but `*`, and `localhost`.
    names: Vec<String>,
    /// The addresses the patterns name, if none names a range.
    addresses: Option<Vec<IpAddr>>,
    /// A port no pattern names, and a method no rule names.
    port: u16,
    method: String,
    /// The host patterns matching each destination ([`HttpRules::hits`]).
    hits: Hits<String>,
    /// Those matching a destination but not a broader one ([`HttpRules::extra`]).
    extra: Hits<(String, String)>,
}

/// Host patterns matching destinations, as they are found.
type Hits<K> = RefCell<FxHashMap<K, Rc<[Hit]>>>;

impl<'r> HttpRules<'r> {
    fn new(rules: &'r [Rule], deny_private: bool) -> HttpRules<'r> {
        let hasher = FxBuild::default();
        let mut http = HttpRules {
            rules: Vec::new(),
            at: vec![usize::MAX; rules.len()],
            hosts: HostIndex::default(),
            deny_private,
            fresh: String::new(),
            under: FxHashMap::default(),
            names: Vec::new(),
            addresses: Some(Vec::new()),
            port: 0,
            method: String::new(),
            hits: RefCell::default(),
            extra: RefCell::default(),
        };
        for (index, rule) in rules.iter().enumerate() {
            let (host, scheme, methods, segs) = match &rule.kind {
                RuleKind::Net(host) => (host, None, None, Vec::new()),
                RuleKind::Http(methods, url) => {
                    let segs =
                        if rule.allow { url.path.clone() } else { url.path.iter().map(|s| s.map(|n| fold_name(n).into_owned())).collect() };
                    (&url.host, url.scheme, methods.as_deref(), segs)
                }
                _ => continue,
            };
            http.at[index] = http.rules.len();
            http.hosts.insert(&NetPattern { kind: host.kind.clone(), port: None }, http.rules.len() as u32);
            http.rules.push(HttpRule {
                index,
                allow: rule.allow,
                host,
                scheme,
                methods,
                weight: segs.iter().map(Seg::weight).sum(),
                prefixes: prefixes(&segs, &hasher),
                segs,
            });
        }

        let mut labels = HashSet::new();
        let mut ports = HashSet::new();
        let mut methods = HashSet::new();
        for r in &http.rules {
            match &r.host.kind {
                HostKind::Host(name) | HostKind::Suffix(name) => {
                    labels.insert(name.split('.').next().unwrap_or_default());
                }
                HostKind::Ip(ip, len) if *len < if ip.is_ipv4() { 32 } else { 128 } => http.addresses = None,
                HostKind::Ip(ip, _) => {
                    if let Some(a) = &mut http.addresses {
                        a.push(*ip);
                    }
                }
                HostKind::Any => {}
            }
            ports.extend(r.host.port);
            methods.extend(r.methods.into_iter().flatten().map(String::as_str));
        }
        http.fresh = (0..).map(|n| format!("x{n}")).find(|l| !labels.contains(l.as_str())).unwrap_or_default();
        http.port = (1..=u16::MAX).rev().find(|p| !ports.contains(p)).unwrap_or(0);
        http.method = (0..).map(|n| format!("OTHER{n}")).find(|m| !methods.contains(m.as_str())).unwrap_or_default();

        for r in &http.rules {
            let (name, domains) = match &r.host.kind {
                HostKind::Host(h) => (h.clone(), &h[h.find('.').map_or(h.len(), |i| i + 1)..]),
                HostKind::Suffix(s) => (format!("{}.{s}", http.fresh), s.as_str()),
                _ => continue,
            };
            let mut domain = domains;
            while !domain.is_empty() {
                http.under.entry(domain.to_string()).or_default().push(name.clone());
                domain = domain.find('.').map_or("", |i| &domain[i + 1..]);
            }
            http.names.push(name);
        }
        for names in http.under.values_mut().chain([&mut http.names]) {
            names.sort_unstable();
            names.dedup();
        }
        http.names.extend([http.fresh.clone(), "localhost".to_string()]);
        http
    }

    /// Destinations (host names and addresses) that stand for every
    /// destination `rule`'s host pattern matches: two destinations not told
    /// apart by them match the same patterns the same way. `None` where
    /// that would take address ranges apart.
    fn destinations(&self, rule: &HttpRule) -> Option<Vec<String>> {
        Some(match &rule.host.kind {
            HostKind::Host(h) => vec![h.clone()],
            HostKind::Suffix(s) => {
                std::iter::once(format!("{}.{s}", self.fresh)).chain(self.under.get(s).into_iter().flatten().cloned()).collect()
            }
            HostKind::Ip(ip, _) => self.addresses(Some(*ip))?.iter().map(IpAddr::to_string).collect(),
            HostKind::Any => {
                let names = self.names.iter().filter(|&n| *n != self.fresh).cloned();
                std::iter::once(self.fresh.clone()).chain(names).chain(self.addresses(None)?.iter().map(IpAddr::to_string)).collect()
            }
        })
    }

    /// The first of [`HttpRules::destinations`] for a wildcard host pattern:
    /// one that only the patterns matching all the others match.
    fn broadest(&self, rule: &HttpRule) -> Option<String> {
        match &rule.host.kind {
            HostKind::Suffix(s) => Some(format!("{}.{s}", self.fresh)),
            HostKind::Any => Some(self.fresh.clone()),
            _ => None,
        }
    }

    /// The addresses that stand for those that reach `target` (any address
    /// if `None`), or `None` if a pattern names a range of addresses.
    fn addresses(&self, target: Option<IpAddr>) -> Option<Vec<IpAddr>> {
        let named = self.addresses.as_ref()?;
        let mut all = vec![];
        let mut add = |ip: IpAddr| {
            all.push(ip);
            if let IpAddr::V4(v4) = ip {
                let [a, b, c, d] = v4.octets();
                let (hi, lo) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
                all.extend(
                    [
                        [0, 0, 0, 0, 0, 0, hi, lo],
                        [0, 0, 0, 0, 0xffff, 0, hi, lo],
                        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo],
                        [0x2002, hi, lo, 0, 0, 0, 0, 0],
                    ]
                    .map(|s| IpAddr::V6(Ipv6Addr::from(s))),
                );
            }
        };
        let unspecified = [IpAddr::V4(Ipv4Addr::UNSPECIFIED), IpAddr::V6(Ipv6Addr::UNSPECIFIED)];
        match target {
            Some(ip) => {
                add(ip);
                if ip.is_loopback() {
                    unspecified.into_iter().for_each(&mut add);
                }
                all.extend(named.iter().filter(|&&n| ip_spellings(n).contains(&ip)));
            }
            None => {
                let fresh = |spell: fn(u8) -> IpAddr| (1..=255).map(spell).find(|ip| !named.contains(ip));
                let public4 = fresh(|n| IpAddr::V4(Ipv4Addr::new(8, 8, n, 8)));
                let private4 = fresh(|n| IpAddr::V4(Ipv4Addr::new(10, 0, n, 1)));
                let public6 = fresh(|n| IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, n.into(), 0, 0, 0, 0, 1)));
                let private6 = fresh(|n| IpAddr::V6(Ipv6Addr::new(0xfd00, n.into(), 0, 0, 0, 0, 0, 1)));
                let loopback = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];
                for ip in named.iter().copied().chain(unspecified).chain(loopback).chain([public4?, private4?, public6?, private6?]) {
                    add(ip);
                }
            }
        }
        all.sort_unstable();
        all.dedup();
        Some(all)
    }

    /// The host patterns matching destination `name`, with any port.
    fn hits(&self, name: &str) -> Rc<[Hit]> {
        if let Some(hits) = self.hits.borrow().get(name) {
            return hits.clone();
        }
        let mut hits = Vec::new();
        self.hosts.find(&Dest::new(name, 0), |score, explicit, spelled, &at| hits.push(Hit { at, score, explicit, spelled }));
        let hits: Rc<[Hit]> = hits.into();
        self.hits.borrow_mut().insert(name.to_string(), hits.clone());
        hits
    }

    /// The host patterns matching destination `name` but not `broader`.
    fn extra(&self, name: &str, broader: &str) -> Rc<[Hit]> {
        let key = (name.to_string(), broader.to_string());
        if let Some(hits) = self.extra.borrow().get(&key) {
            return hits.clone();
        }
        let common: HashSet<u32> = self.hits(broader).iter().map(|h| h.at).collect();
        let extra: Rc<[Hit]> = self.hits(name).iter().filter(|h| !common.contains(&h.at)).copied().collect();
        self.extra.borrow_mut().insert(key, extra.clone());
        extra
    }

    /// Whether `dest` is a private address or the local host, which only
    /// explicit allow rules allow.
    fn internal(&self, dest: &Dest) -> bool {
        self.deny_private && (dest.ip.is_some_and(is_private_address) || super::net::is_localhost(dest.name))
    }

    /// What decides in place of http rule `i` (with the rules `removed`
    /// gone), or `None` if it is needed.
    fn effect(&self, i: usize, removed: &[bool], budget: &mut Budget) -> Option<Effect> {
        let me = &self.rules[self.at[i]];
        let names = sample(&me.segs);
        let mut effect = Effect::default();
        let mut tried = HashSet::new();
        let broadest = self.broadest(me);
        // Whether a rule may decide a request alongside `me`.
        let alongside = |r: &HttpRule| {
            !removed[r.index]
                && related(&r.prefixes, &me.prefixes)
                && (me.methods.is_none() || r.methods.is_none_or(|m| m.iter().any(|m| me.methods.into_iter().flatten().any(|n| n == m))))
                && (me.scheme.is_none() || r.scheme.is_none_or(|s| Some(s) == me.scheme))
                && (me.host.port.is_none() || r.host.port.is_none_or(|p| Some(p) == me.host.port))
        };
        for name in self.destinations(me)? {
            let dest = Dest::new(&name, 0);
            let internal = self.internal(&dest);
            // A name that only rules that never apply with `me` tell apart from
            // the broadest is like it.
            if let Some(broadest) = broadest.as_deref().filter(|b| *b != name && dest.ip.is_none())
                && internal == self.internal(&Dest::new(broadest, 0))
            {
                let extra = self.extra(&name, broadest);
                budget.spend(extra.len())?;
                if !extra.iter().any(|h| alongside(&self.rules[h.at as usize])) {
                    continue;
                }
            }
            let hits = self.hits(&name);
            budget.spend(hits.len())?;
            let hits: Vec<Hit> = hits
                .iter()
                .copied()
                .filter(|h| {
                    let r = &self.rules[h.at as usize];
                    !removed[r.index] && related(&r.prefixes, &me.prefixes) && (!r.allow || !h.spelled && (h.explicit || !internal))
                })
                .collect();
            let mut ports: Vec<u16> = hits.iter().filter_map(|h| self.rules[h.at as usize].host.port).chain([self.port]).collect();
            ports.sort_unstable();
            ports.dedup();
            let mut methods: Vec<&str> = match me.methods {
                Some(m) => m.iter().map(String::as_str).collect(),
                None => hits.iter().flat_map(|h| self.rules[h.at as usize].methods.into_iter().flatten()).map(String::as_str).collect(),
            };
            methods.push(&self.method);
            methods.sort_unstable();
            methods.dedup();
            for port in ports {
                for scheme in ["http", "https"] {
                    for &method in &methods {
                        let applies = |r: &HttpRule| {
                            r.host.port.is_none_or(|p| p == port)
                                && r.scheme.is_none_or(|s| s == scheme)
                                && r.methods.is_none_or(|m| m.iter().any(|m| m == method))
                        };
                        if !applies(me) {
                            continue;
                        }
                        let cands: Vec<Cand> = hits
                            .iter()
                            .map(|h| (h, &self.rules[h.at as usize]))
                            .filter(|(_, r)| applies(r))
                            .map(|(h, r)| Cand {
                                rule: r.index,
                                allow: r.allow,
                                head: [h.score + r.host.port.is_some() as u32, r.scheme.is_some() as u32],
                                tail: [r.weight, r.methods.is_some() as u32],
                                segs: &r.segs,
                            })
                            .collect();
                        budget.spend(hits.len() + cands.len() * (names.len() + 1))?;
                        let Some(at) = cands.iter().position(|c| c.rule == i) else { continue };
                        if decides_at(&cands, at, &names)
                            || tried.insert(cands.iter().map(|c| (c.rule, c.head)).collect::<Vec<_>>())
                                && !search(&cands, at, &names, &mut effect, budget)
                        {
                            return None;
                        }
                    }
                }
            }
        }
        Some(effect)
    }

    /// A `net` rule that lets scripts connect to a destination that deny
    /// http rule `i` matches, and that destination (`*.domain` for names
    /// standing for a domain's), leaving out the rules `removed`.
    fn bypass(&self, policy: &RulePolicy, i: usize, removed: &[bool]) -> Option<(usize, String)> {
        let me = &self.rules[self.at[i]];
        for name in self.destinations(me)? {
            let ports: Vec<u16> = match me.host.port {
                Some(p) => vec![p],
                None => self.hits(&name).iter().filter_map(|h| self.rules[h.at as usize].host.port).chain([443]).collect(),
            };
            for port in ports {
                let cap = Capability::Net { host: name.clone(), port };
                if let Some((true, j)) = policy.verdict(&cap, PathSpace::Memory, &|r| removed[r]) {
                    let shown = match name.strip_prefix(&self.fresh).and_then(|n| n.strip_prefix('.')) {
                        Some(domain) => format!("*.{domain}"),
                        None if name.contains(':') => format!("[{name}]"),
                        None => name,
                    };
                    return Some((j, format!("{shown}:{port}")));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lint(text: &str) -> Vec<String> {
        lint_policy(text, Path::new("/base"), Some(Path::new("/home/me"))).unwrap().iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn redundant_and_overridden_rules() {
        assert_eq!(
            lint("allow ./data\nallow read ./data/x\nallow read ./data/x\ndeny ~/.ssh\ndeny ~/.ssh/id\nallow write ./out\ndeny ./out"),
            [
                "2:12: warning[redundant-rule]: line 1 (`./data`) already allows this",
                "3:12: warning[redundant-rule]: the same as line 2 (`./data/x`)",
                "5:6: warning[redundant-rule]: line 4 (`~/.ssh`) already denies this",
                "6:13: warning[overridden-rule]: line 7 (`./out`) takes precedence everywhere this rule applies",
            ]
        );
        // A rule overriding a broader one of the other kind is needed.
        assert_eq!(lint("deny ~/\nallow ./project\ndeny ./project/.env\nallow read ./project/.env/public"), Vec::<String>::new());
        assert_eq!(lint("allow read ./a\nallow write ./a"), ["1:12: warning[redundant-rule]: line 2 (`./a`) already allows this"]);
        assert_eq!(lint("allow read ./a\nallow write ./a/b"), Vec::<String>::new());
        assert_eq!(lint("allow write ./a\nallow read ./a"), ["2:12: warning[redundant-rule]: line 1 (`./a`) already allows this"]);
        // Of two rules that each make the other unneeded, one is reported.
        assert_eq!(lint("deny read ./x\ndeny ./x"), ["2:6: warning[redundant-rule]: line 1 (`./x`) already denies this"]);
    }

    #[test]
    fn rules_with_wildcards() {
        let none = Vec::<String>::new();
        assert_eq!(
            lint("allow read ./src\nallow read ./src/**/*.rs\ndeny ./a/**/.env\ndeny ./a/b/.env\nallow ./out/*\ndeny ./out/*"),
            [
                "2:12: warning[redundant-rule]: line 1 (`./src`) already allows this",
                "4:6: warning[redundant-rule]: line 3 (`./a/**/.env`) already denies this",
                "5:7: warning[overridden-rule]: line 6 (`./out/*`) takes precedence everywhere this rule applies",
            ]
        );
        // Without line 2, `./src/.env/x.rs` would be denied.
        assert_eq!(lint("allow read ./src\nallow read ./src/**/*.rs\ndeny ./**/.env"), none);
        // The more specific of two overlapping patterns decides where both match.
        assert_eq!(lint("allow ./a/*.txt\ndeny ./a/*"), none);
        assert_eq!(lint("deny ./a/*.pem\nallow ./a/key*"), none);
        // A deny between the broader rule and a rule decides some paths.
        assert_eq!(lint("allow ./data\ndeny ./data/secret\nallow read ./data/**/x"), none);
        assert_eq!(lint("deny ~/\nallow ./project\ndeny ./project/.env\nallow read ./project/.env/*"), none);
        // A deny that only ranks above the rule, or below the broader rule.
        assert_eq!(
            lint("allow ./data\ndeny ./data/secret\nallow read ./data/*"),
            ["3:12: warning[redundant-rule]: line 1 (`./data`) already allows this"]
        );
        assert_eq!(
            lint("deny ~/\nallow ~/proj\nallow read ~/proj/src/*.rs"),
            ["3:12: warning[redundant-rule]: line 2 (`~/proj`) already allows this"]
        );
        // Deny rules match other spellings of a name; allow rules do not.
        assert_eq!(
            lint("allow ./A/*\ndeny ./a/*"),
            ["1:7: warning[overridden-rule]: line 2 (`./a/*`) takes precedence everywhere this rule applies"]
        );
        assert_eq!(lint("deny ./a/*\nallow ./a/*.TXT\nallow ./A/*.txt"), none);
        assert_eq!(
            lint("let ext = pem key\ndeny ./**/*.{ext}\ndeny ./keys/*.pem"),
            ["3:6: warning[redundant-rule]: line 2 (`./**/*.pem`) already denies this"]
        );
        // A wildcard name that a spelling of a denied name may match.
        assert_eq!(lint("deny ./a/secret\nallow ./a/S*\nallow read ./a/*"), none);
        assert_eq!(
            lint("allow ./a/*.rs\ndeny ./a/*.rs"),
            ["1:7: warning[overridden-rule]: line 2 (`./a/*.rs`) takes precedence everywhere this rule applies"]
        );
    }

    #[test]
    fn rules_made_unneeded_by_several_others() {
        // Line 1 decides reading `./a/b/c` in place of line 3, and line 2
        // writing it; without line 3, line 1 is not needed either.
        assert_eq!(
            lint("allow read ./a/b\nallow write ./a\nallow ./a/b/c"),
            [
                "1:12: warning[redundant-rule]: line 2 (`./a`) already allows this",
                "3:7: warning[redundant-rule]: line 2 (`./a`) already allows this",
            ]
        );
        // Line 1 decides `./a/b` and line 2 `./a/x/b`.
        assert_eq!(
            lint("allow read ./a/b\nallow read ./a/*/**/b\nallow read ./a/**/b"),
            ["3:12: warning[redundant-rule]: line 1 (`./a/b`) and line 2 (`./a/*/**/b`) already allow this"]
        );
        // Without line 4, `./data/SECRET/x` would be denied.
        assert_eq!(
            lint("allow ./data\ndeny ./data/secret\nallow read ./data/secret/**/x\nallow read ./data/**/x"),
            ["3:12: warning[redundant-rule]: line 4 (`./data/**/x`) already allows this"]
        );
        // Removing every rule reported changes nothing: of the two broad
        // rules, one goes.
        assert_eq!(
            lint("allow read ./a/x*\nallow read ./a/*y\nallow read ./a/*"),
            [
                "1:12: warning[redundant-rule]: line 3 (`./a/*`) already allows this",
                "2:12: warning[redundant-rule]: line 3 (`./a/*`) already allows this",
            ]
        );
    }

    #[test]
    fn http_rules() {
        let none = Vec::<String>::new();
        // Line 3 ranks above line 1 (it names a scheme), and line 5 above line
        // 4, but they decide the same.
        assert_eq!(
            lint(
                "allow http api.example.com/v1\nallow http GET api.example.com/v1/users\nallow http https://api.example.com/v1/x\n\
                 deny http DELETE *.example.com\ndeny http DELETE *.example.com/v1"
            ),
            [
                "2:16: warning[redundant-rule]: line 1 (`api.example.com/v1`) already allows this",
                "3:12: warning[redundant-rule]: line 1 (`api.example.com/v1`) already allows this",
                "5:18: warning[redundant-rule]: line 4 (`*.example.com`) already denies this",
            ]
        );
        // A deny naming the scheme ranks between line 1 and line 3.
        assert_eq!(
            lint("allow http api.example.com/v1\ndeny http DELETE https://api.example.com\nallow http https://api.example.com/v1/x"),
            none
        );
        // Methods split among rules.
        assert_eq!(
            lint("allow http GET api.example.com\nallow http POST api.example.com\nallow http GET POST api.example.com/v1"),
            ["3:21: warning[redundant-rule]: line 1 (`api.example.com`) and line 2 (`api.example.com`) already allow this"]
        );
        // A net rule decides every request to its host, and ranks below http rules.
        assert_eq!(
            lint("allow net api.example.com\nallow http api.example.com/v1"),
            ["2:12: warning[redundant-rule]: line 1 (`api.example.com`) already allows this"]
        );
        assert_eq!(
            lint("allow net api.example.com\ndeny http POST api.example.com\nallow http api.example.com/v1"),
            [
                "2:16: warning[bypassable-http-deny]: line 1 (`api.example.com`) lets scripts connect to api.example.com:443 and send any request"
            ]
        );
        // A wildcard host covers the hosts it matches, but not internal ones.
        assert_eq!(
            lint("allow http *.example.com/v1\nallow http api.example.com/v1/x"),
            ["2:12: warning[redundant-rule]: line 1 (`*.example.com/v1`) already allows this"]
        );
        assert_eq!(lint("allow http *\nallow http localhost/x"), none);
        assert_eq!(
            lint("allow private-net\nallow http *\nallow http localhost/x"),
            ["3:12: warning[redundant-rule]: line 2 (`*`) already allows this"]
        );
        assert_eq!(lint("allow http 1.2.3.4/x\nallow http *"), ["1:12: warning[redundant-rule]: line 2 (`*`) already allows this"]);
        // Address ranges are not taken apart.
        assert_eq!(lint("allow net 10.0.0.0/8\nallow http 10.1.2.3/x"), none);
        // Deny rules match other spellings of paths and addresses; allow rules do not.
        assert_eq!(
            lint("allow http api.example.com/Admin\ndeny http api.example.com/admin"),
            ["1:12: warning[overridden-rule]: line 2 (`api.example.com/admin`) takes precedence everywhere this rule applies"]
        );
        assert_eq!(lint("deny http api.example.com/admin\nallow http api.example.com/Admin/x"), none);
        assert_eq!(
            lint("allow private-net\ndeny http 1.2.3.4/x\nallow http 64:ff9b::102:304/x"),
            ["3:12: warning[overridden-rule]: line 2 (`1.2.3.4/x`) takes precedence everywhere this rule applies"]
        );
    }

    #[test]
    fn http_denies_that_net_rules_bypass_and_unused_sets() {
        assert_eq!(
            lint("let unused = a\nlet hosts = api.example.com\nallow net {hosts}\ndeny http DELETE {hosts}/x\ndeny http other.com"),
            [
                "1:5: warning[unused-set]: no rule uses the set `unused`",
                "4:18: warning[bypassable-http-deny]: line 3 (`api.example.com`) lets scripts connect to api.example.com:443 and send any request",
            ]
        );
        assert_eq!(
            lint("allow net *.example.com:8443\ndeny http *.example.com/admin\ndeny http localhost/admin\nallow net *"),
            [
                "2:11: warning[bypassable-http-deny]: line 1 (`*.example.com:8443`) lets scripts connect to *.example.com:8443 and send any request"
            ]
        );
    }

    mod removing_what_is_reported {
        use super::super::*;
        use proptest::prelude::*;

        const NAMES: &[&str] = &["a", "A", "b", "x.rs", "X.RS", "*.rs", "x*", "*", "**"];
        const REQUEST_NAMES: &[&str] = &["a", "A", "a\u{200b}", "b", "x.rs", "X.RS", "xb", "y", "\u{17f}"];
        const HOSTS: &[&str] =
            &["api.example.com", "*.example.com", "*", "1.2.3.4", "64:ff9b::102:304", "127.0.0.1", "localhost", "api.example.com:8443"];
        const REQUEST_HOSTS: &[&str] =
            &["api.example.com", "x.example.com", "other.org", "localhost", "1.2.3.4", "64:ff9b::102:304", "0.0.0.0", "10.0.0.1"];

        fn path(names: &[usize]) -> String {
            names.iter().map(|&n| NAMES[n]).collect::<Vec<_>>().join("/")
        }

        /// A file, http or net rule from numbers.
        fn rule(kind: usize, allow: bool, which: usize, host: usize, names: &[usize]) -> String {
            let verb = if allow { "allow" } else { "deny" };
            match kind {
                0 => format!("{verb} {} /r/{}", ["read", "write", "import", ""][which % 4], path(names)),
                1 => {
                    let methods = ["", "GET", "POST", "GET POST"][which % 4];
                    let scheme = ["", "https://"][which / 4 % 2];
                    format!("{verb} http {methods} {scheme}{}/{}", HOSTS[host], path(names))
                }
                _ => format!("{verb} net {}", HOSTS[host]),
            }
        }

        /// What `policy` decides for each request: allow, deny, or nothing.
        fn decisions(policy: &RulePolicy, requests: &[Capability]) -> Vec<Option<bool>> {
            requests.iter().map(|cap| policy.verdict(cap, PathSpace::Memory, &|_| false).map(|v| v.0)).collect()
        }

        fn policy(text: &str) -> RulePolicy {
            let file = super::super::super::PolicyFile::parse(text, Path::new("/"), None).unwrap();
            if file.private_net { file.rules } else { file.rules.deny_private_networks() }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2000))]

            #[test]
            fn changes_no_decision(
                rules in proptest::collection::vec(
                    (0..3usize, any::<bool>(), 0..8usize, 0..HOSTS.len(), proptest::collection::vec(0..NAMES.len(), 0..3)),
                    1..7,
                ),
                private_net in any::<bool>(),
                paths in proptest::collection::vec(proptest::collection::vec(0..REQUEST_NAMES.len(), 0..4), 12),
            ) {
                let mut lines: Vec<String> = rules.iter().map(|(k, a, w, h, n)| rule(*k, *a, *w, *h, n)).collect();
                if private_net {
                    lines.push("allow private-net".into());
                }
                let text = lines.join("\n");
                let Ok(warnings) = lint_policy(&text, Path::new("/"), None) else { return Ok(()) };
                let gone: Vec<usize> =
                    warnings.iter().filter(|w| w.code != "bypassable-http-deny").map(|w| w.line - 1).collect();
                let kept: Vec<&str> = lines.iter().enumerate().filter(|(i, _)| !gone.contains(i)).map(|(_, l)| l.as_str()).collect();
                let mut requests = Vec::new();
                for (k, names) in paths.iter().enumerate() {
                    let p = names.iter().map(|&n| REQUEST_NAMES[n]).collect::<Vec<_>>().join("/");
                    requests.push([Capability::FsRead, Capability::FsWrite, Capability::Import][k % 3](format!("/r/{p}").into()));
                    for host in REQUEST_HOSTS {
                        for (method, scheme, port) in [("GET", "https", 443), ("POST", "http", 8443), ("DELETE", "https", 80)] {
                            let (method, scheme, host, path) = (method.into(), scheme.into(), host.to_string(), format!("/{p}"));
                            requests.push(Capability::Http { method, scheme, host, port, path });
                        }
                    }
                }
                prop_assert_eq!(
                    decisions(&policy(&text), &requests),
                    decisions(&policy(&kept.join("\n")), &requests),
                    "{} -> {:?}", text, warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>()
                );
            }
        }
    }
}
