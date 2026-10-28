//! Linter for policy files.

use super::file::parse;
use super::net::{HostKind, NetPattern, ProbeTargets, domains_above, enclosing_ranges};
use super::path::{self, Pattern, Seg, name_counts};
use super::rules::{
    Access, Connection, IMPORT, READ, Rank, Rule, RuleKind, WRITE, allow_applies, file_segments, fold_segments, method_matches, precedence,
    scheme_matches,
};
use super::{Capability, Decision, PathSpace, PolicyError};
use crate::lint::LintWarning;
use crate::vm::value::{FxBuild, FxHashMap};
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::path::Path;

/// Lints a policy file and returns any warnings found:
///
/// - `redundant-rule`: a rule duplicates another rule, or broader rules already
///   produce the same decision for all requests that it matches.
/// - `overridden-rule`: higher-precedence rules produce the opposite decision
///   for all requests that the rule matches.
/// - `bypassable-http-deny`: a `deny http` rule matches a destination to which
///   a `net` rule or `default allow` permits raw TCP connections.
/// - `unused-set`: a `let` set is not referenced by any rule.
///
/// The linter compares file, HTTP, and network rules by pattern subsumption,
/// including wildcards. File paths are compared lexically without symlink
/// resolution. If all reported rules are removed at the same time, policy
/// decisions do not change.
pub fn lint_policy(text: &str, base: &Path, home: Option<&Path>) -> Result<Vec<LintWarning>, Vec<PolicyError>> {
    let parsed = parse(text, base, home)?;
    let deny_private = !parsed.file.private_net;
    let policy = parsed.file.rule_policy(Decision::Deny);
    let rules = policy.rules();
    let mut warnings = Vec::new();
    let mut warn = |i: usize, code: &'static str, message: String| {
        if let Some(o) = &rules[i].origin {
            warnings.push(LintWarning { code, message, line: o.line, column: o.column });
        }
    };
    let describe = |i: usize| match &rules[i].origin {
        Some(o) => format!("line {} (`{}`)", o.line, o.text),
        None => "another rule".to_string(),
    };
    let describe_all = |rules: &[usize]| match rules {
        [] => String::new(),
        [single] => describe(*single),
        [init @ .., last] => format!("{} and {}", init.iter().map(|&i| describe(i)).collect::<Vec<_>>().join(", "), describe(*last)),
    };
    let paths = PathRules::new(rules, deny_private);
    let probe_targets = ProbeTargets::new(rules.iter().filter_map(|r| r.kind.http_match().map(|http| http.host)));
    let mut bypasses: FxHashMap<(NetPattern, NetPattern), Option<(String, NetPattern)>> = FxHashMap::default();
    let mut bypass = |reach: NetPattern, denied: &NetPattern| {
        let found = bypasses.entry((reach, denied.clone())).or_insert_with_key(|(reach, denied)| {
            let (host, port) = probe_targets.dest(reach, denied)?;
            let cap = Capability::Net { host, port };
            let by = match policy.verdict(&cap, PathSpace::Memory, &[]) {
                Some((allow, j)) => allow.then(|| describe(j))?,
                None => (policy.check_normalized_in(&cap, PathSpace::Memory) == Decision::Allow)
                    .then(|| format!("`default allow` (line {})", parsed.default_line.unwrap_or(0)))?,
            };
            Some((by, NetPattern { port: Some(port), ..reach.clone() }))
        });
        found.clone()
    };
    let mut same: FxHashMap<(bool, &RuleKind), Vec<usize>> = FxHashMap::default();
    for (i, r) in rules.iter().enumerate() {
        same.entry((r.allow, &r.kind)).or_default().push(i);
    }
    // A pattern token that expands a set produces multiple rules at the same span.
    // Evaluate tokens in reverse order and ignore rules from tokens already marked
    // as redundant so that removing all reported tokens preserves every decision.
    let mut words: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, r) in rules.iter().enumerate() {
        let Some(o) = &r.origin else { continue };
        match words.last_mut() {
            Some(word) if rules[word.start].origin.as_ref().is_some_and(|w| (w.line, w.column) == (o.line, o.column)) => word.end = i + 1,
            _ => words.push(i..i + 1),
        }
    }
    let is_allow_import = |i: usize| rules[i].allow && matches!(rules[i].kind, RuleKind::Files(Access::Import, _));
    let mut allow_imports = (0..rules.len()).filter(|&i| is_allow_import(i)).count();
    let mut removed = vec![false; rules.len()];
    for word in words.iter().rev() {
        // The last remaining `allow import` token also implicitly denies all unmatched imports.
        let imports = word.clone().filter(|&i| is_allow_import(i)).count();
        if imports > 0 && imports == allow_imports {
            continue;
        }
        let is_kept = |j: usize| !word.contains(&j) && !removed[j];
        let find_subsuming = || {
            let (mut same_decision, mut deciders, mut repeats) = (None, Vec::new(), true);
            for i in word.clone() {
                let rule = &rules[i];
                let (decision, by, repeat) = match same[&(rule.allow, &rule.kind)].iter().copied().find(|&c| is_kept(c)) {
                    Some(c) => (true, vec![c], true),
                    None => {
                        let (decision, by) = paths.deciders(paths.get(i)?, &|j| !is_kept(j))?;
                        (decision, by, false)
                    }
                };
                if *same_decision.get_or_insert(decision) != decision {
                    return None;
                }
                deciders.extend(by);
                repeats &= repeat;
            }
            Some((same_decision?, deciders, repeats))
        };
        let Some((same_decision, mut deciders, repeat)) = find_subsuming() else { continue };
        deciders.sort_unstable();
        deciders.dedup();
        let is_single = deciders.len() == 1;
        let message = match (same_decision, rules[word.start].allow) {
            _ if repeat && word.len() == 1 => format!("the same as {}", describe(deciders[0])),
            (true, true) => format!("{} already {} this", describe_all(&deciders), if is_single { "allows" } else { "allow" }),
            (true, false) => format!("{} already {} this", describe_all(&deciders), if is_single { "denies" } else { "deny" }),
            (false, _) => {
                format!("{} {} precedence everywhere this rule applies", describe_all(&deciders), if is_single { "takes" } else { "take" })
            }
        };
        warn(word.start, if same_decision { "redundant-rule" } else { "overridden-rule" }, message);
        removed[word.clone()].fill(true);
        allow_imports -= imports;
    }
    let mut bypass_of = |i: usize| {
        let (RuleKind::Http(_, url), false, Some(r)) = (&rules[i].kind, rules[i].allow, paths.get(i)) else { return None };
        bypass(url.host.clone(), &url.host).or_else(|| {
            let denied = std::iter::once(url.host.clone()).chain(url.host.reaching_ranges()).collect::<Vec<_>>();
            let mut reach = paths.opposed(r, 0).flat_map(|k| match k.scope {
                Scope::Http { host, connections, .. } if connections.contains(&Connection::Named) => {
                    denied.iter().filter_map(|d| host.intersection(d)).collect()
                }
                _ => Vec::new(),
            });
            reach.find_map(|reach| bypass(reach, &url.host))
        })
    };
    for word in &words {
        let mut firsts = word.clone().filter(|&i| same[&(rules[i].allow, &rules[i].kind)][0] == i);
        if let Some((by, reach)) = firsts.find_map(&mut bypass_of) {
            warn(word.start, "bypassable-http-deny", format!("{by} lets scripts connect to {reach} and send any request"));
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

enum Scope<'a> {
    Files(u8),
    Http { host: &'a NetPattern, scheme: Option<&'static str>, methods: Option<&'a [String]>, connections: &'static [Connection] },
}

#[derive(Clone, Copy)]
enum Op<'a> {
    File(u8),
    Method(Option<&'a str>),
    Connect(Connection),
}

struct PathRule<'a> {
    index: usize,
    allow: bool,
    scope: Scope<'a>,
    /// True if this rule can apply to internal network destinations.
    reaches_internal: bool,
    /// True if the host pattern can match an internal network destination.
    internal: bool,
    /// Path segments from the root (case-folded for `deny` rules).
    segs: Vec<Seg>,
    /// Case-folded path segments, or `None` for an `allow` rule with
    /// non-ASCII text adjacent to a wildcard.
    folded: Option<Vec<Seg>>,
    /// Cumulative hashes of literal segments before the first wildcard.
    prefixes: Vec<u64>,
    rank: Rank,
    /// Minimum and maximum segment counts matched by this pattern.
    counts: (usize, Option<usize>),
}

impl<'a> PathRule<'a> {
    fn new(index: usize, rule: &'a Rule, deny_private: bool) -> Option<PathRule<'a>> {
        let (scope, raw, rank) = match &rule.kind {
            RuleKind::Files(access, pattern) => {
                let raw = file_segments(&crate::files::memory_path(&pattern.prefix), &pattern.rest);
                let rank = Rank::file(&raw);
                (Scope::Files(access.mask(rule.allow)), raw, rank)
            }
            kind => {
                let http = kind.http_match()?;
                let scope = Scope::Http { host: http.host, scheme: http.scheme, methods: http.methods, connections: kind.connections() };
                (scope, http.path.to_vec(), Rank::http(&http))
            }
        };
        let (reaches_internal, internal) = match scope {
            Scope::Http { host, .. } => {
                let reaches = !rule.allow || allow_applies(host.is_explicit(), false, deny_private);
                (reaches, reaches && host.may_match_internal())
            }
            Scope::Files(_) => (true, true),
        };
        let folded = fold_segments(&raw);
        let exact_fold = |s: &Seg| !matches!(s, Seg::Glob(g) if g.pieces().iter().any(|p| !p.is_ascii()));
        let hasher = FxBuild::default();
        let names = folded.iter().map_while(|s| if let Seg::Lit(n) = s { Some(n) } else { None });
        let prefixes = std::iter::once(0)
            .chain(names.scan(0, |h, n| {
                *h = hasher.hash_one((*h, n));
                Some(*h)
            }))
            .collect();
        Some(PathRule {
            index,
            allow: rule.allow,
            scope,
            reaches_internal,
            internal,
            prefixes,
            rank,
            counts: name_counts(&raw),
            folded: (!rule.allow || raw.iter().all(exact_fold)).then(|| folded.clone()),
            segs: if rule.allow { raw } else { folded },
        })
    }

    fn ops(&self) -> Vec<Op<'a>> {
        match self.scope {
            Scope::Files(mask) => [READ, WRITE, IMPORT].into_iter().filter(|&op| mask & op != 0).map(Op::File).collect(),
            Scope::Http { methods, connections, .. } => {
                let methods: Vec<Op> = match methods {
                    Some(methods) => methods.iter().map(|m| Op::Method(Some(m))).collect(),
                    None => vec![Op::Method(None)],
                };
                methods.into_iter().chain(connections.iter().map(|&c| Op::Connect(c))).collect()
            }
        }
    }

    fn decides_all(&self, op: Op) -> bool {
        match (&self.scope, op) {
            (Scope::Files(mask), Op::File(op)) => mask & op != 0,
            (Scope::Http { methods, .. }, Op::Method(Some(m))) => method_matches(*methods, m),
            (Scope::Http { methods, .. }, Op::Method(None)) => methods.is_none(),
            (_, Op::Connect(_)) => self.may_decide(op),
            _ => false,
        }
    }

    fn may_decide(&self, op: Op) -> bool {
        match (&self.scope, op) {
            (Scope::Files(mask), Op::File(op)) => mask & op != 0,
            (Scope::Http { methods, .. }, Op::Method(m)) => m.is_none_or(|m| method_matches(*methods, m)),
            (Scope::Http { connections, .. }, Op::Connect(c)) => connections.contains(&c),
            _ => false,
        }
    }

    fn rank(&self) -> impl Ord {
        precedence(self.rank.spec(0), self.allow, self.index)
    }

    fn outranks(&self, other: &PathRule) -> bool {
        self.rank() > other.rank()
    }

    /// Returns the segment slices to compare between two rules (case-folded unless both are `allow` rules).
    fn comparable<'b>(&'b self, other: &'b PathRule) -> Option<(&'b [Seg], &'b [Seg])> {
        if self.allow && other.allow {
            return Some((&self.segs, &other.segs));
        }
        Some((self.folded.as_deref()?, other.folded.as_deref()?))
    }

    /// Returns true if this rule matches every request matched by `other` (or an ancestor path if `below` is true).
    fn covers(&self, other: &PathRule, below: bool) -> bool {
        if (self.allow && !other.allow) || !self.scope_covers(other) {
            return false;
        }
        self.comparable(other).is_some_and(|(a, b)| path::covers(Pattern { segs: a, below }, Pattern { segs: b, below: false }))
    }

    fn scope_covers(&self, other: &PathRule) -> bool {
        match (&self.scope, &other.scope) {
            (Scope::Files(_), Scope::Files(_)) => true,
            (Scope::Http { host: a, scheme: s, .. }, Scope::Http { host: b, scheme: t, .. }) => {
                let schemes = t.map_or(s.is_none(), |t| scheme_matches(*s, t));
                a.covers(b) && schemes && (self.reaches_internal || !other.internal)
            }
            _ => false,
        }
    }

    /// Returns true if a request matched by this rule (or a descendant path if `self_below` is true)
    /// can also match `other` or a descendant of `other`.
    fn may_meet(&self, other: &PathRule, self_below: bool) -> bool {
        let scopes = match (&self.scope, &other.scope) {
            (Scope::Files(_), Scope::Files(_)) => true,
            (Scope::Http { host: a, scheme: s, .. }, Scope::Http { host: b, scheme: t, .. }) => {
                a.may_meet(b) && t.is_none_or(|t| scheme_matches(*s, t))
            }
            _ => false,
        };
        scopes
            && self
                .comparable(other)
                .is_none_or(|(a, b)| path::may_meet(Pattern { segs: a, below: self_below }, Pattern { segs: b, below: true }))
    }

    fn broader_keys(&self) -> Vec<Key<'a>> {
        let Scope::Http { host, .. } = self.scope else { return vec![Key::Files] };
        match &host.kind {
            HostKind::Any => vec![Key::Any],
            HostKind::Suffix(s) => [Key::Any, Key::Suffix(s)].into_iter().chain(domains_above(s).map(Key::Suffix)).collect(),
            HostKind::Host(h) => [Key::Any, Key::Name(h)].into_iter().chain(domains_above(h).map(Key::Suffix)).collect(),
            HostKind::Ip(ip, len) => std::iter::once(Key::Any).chain(enclosing_ranges(*ip, *len).map(Key::Range)).collect(),
        }
    }

    fn related_keys(&self) -> Vec<Key<'a>> {
        let Scope::Http { host, .. } = self.scope else { return vec![Key::Files] };
        match &host.kind {
            HostKind::Any => vec![Key::Http],
            HostKind::Suffix(s) => [Key::Under(s)].into_iter().chain(self.broader_keys()).collect(),
            HostKind::Host(_) => self.broader_keys(),
            // Deny rules also match alternate IP representations.
            HostKind::Ip(..) => vec![Key::Any, Key::Ips],
        }
    }

    fn keys(&self) -> Vec<Key<'a>> {
        let Scope::Http { host, .. } = self.scope else { return vec![Key::Files] };
        let (own, name) = match &host.kind {
            HostKind::Any => (Key::Any, None),
            HostKind::Suffix(s) => (Key::Suffix(s), Some(s)),
            HostKind::Host(h) => (Key::Name(h), Some(h)),
            HostKind::Ip(ip, len) => (Key::Range((*ip, *len)), None),
        };
        let ips = matches!(host.kind, HostKind::Ip(..)).then_some(Key::Ips);
        [Key::Http, own].into_iter().chain(ips).chain(name.into_iter().flat_map(|n| domains_above(n)).map(Key::Under)).collect()
    }
}

#[derive(Hash)]
enum Key<'a> {
    Files,
    Http,
    Any,
    Name(&'a str),
    Suffix(&'a str),
    Under(&'a str),
    Range((IpAddr, u8)),
    Ips,
}

/// Index of file, HTTP, and network rules keyed by host category and literal path prefixes.
struct PathRules<'a> {
    rules: Vec<PathRule<'a>>,
    at: Vec<usize>,
    exact: FxHashMap<u64, Vec<usize>>,
    within: FxHashMap<u64, Vec<usize>>,
    hasher: FxBuild,
    #[cfg(test)]
    visits: std::cell::Cell<usize>,
}

impl<'a> PathRules<'a> {
    fn new(rules: &'a [Rule], deny_private: bool) -> PathRules<'a> {
        let mut paths = PathRules {
            rules: Vec::new(),
            at: vec![usize::MAX; rules.len()],
            exact: Default::default(),
            within: Default::default(),
            hasher: FxBuild::default(),
            #[cfg(test)]
            visits: Default::default(),
        };
        for (index, rule) in rules.iter().enumerate() {
            let Some(rule) = PathRule::new(index, rule, deny_private) else { continue };
            let at = paths.rules.len();
            for key in rule.keys() {
                let key = paths.hasher.hash_one(key);
                for &h in &rule.prefixes {
                    paths.within.entry(paths.hasher.hash_one((key, h))).or_default().push(at);
                }
                let last = rule.prefixes[rule.prefixes.len() - 1];
                paths.exact.entry(paths.hasher.hash_one((key, last))).or_default().push(at);
            }
            paths.at[index] = at;
            paths.rules.push(rule);
        }
        for bucket in paths.exact.values_mut().chain(paths.within.values_mut()) {
            bucket.sort_by_key(|&at| std::cmp::Reverse(paths.rules[at].rank.tier));
        }
        paths
    }

    fn get(&self, index: usize) -> Option<&PathRule<'a>> {
        self.rules.get(self.at[index])
    }

    fn lookup<'b>(
        &'b self,
        map: &'b FxHashMap<u64, Vec<usize>>,
        key: u64,
        h: u64,
        min_tier: u32,
    ) -> impl Iterator<Item = &'b PathRule<'a>> {
        let found = map.get(&self.hasher.hash_one((key, h))).into_iter().flatten().map(|&at| &self.rules[at]);
        #[cfg(test)]
        let found = found.inspect(|_| self.visits.set(self.visits.get() + 1));
        found.take_while(move |r| r.rank.tier >= min_tier)
    }

    fn hash_keys(&self, keys: Vec<Key>) -> Vec<u64> {
        keys.into_iter().map(|k| self.hasher.hash_one(k)).collect()
    }

    fn broader<'b>(&'b self, i: &'b PathRule<'a>, excluded: &dyn Fn(usize) -> bool) -> Vec<&'b PathRule<'a>> {
        self.maybe_broader(i).filter(|j| !excluded(j.index) && j.covers(i, true)).collect()
    }

    fn maybe_broader<'b>(&'b self, i: &'b PathRule<'a>) -> impl Iterator<Item = &'b PathRule<'a>> {
        let keys = self.hash_keys(i.broader_keys());
        keys.into_iter().flat_map(move |key| i.prefixes.iter().flat_map(move |&h| self.lookup(&self.exact, key, h, 0)))
    }

    fn opposed<'b>(&'b self, i: &'b PathRule<'a>, min_tier: u32) -> impl Iterator<Item = &'b PathRule<'a>> {
        let (&last, above) = i.prefixes.split_last().expect("the empty prefix");
        let found = self.hash_keys(i.related_keys()).into_iter().flat_map(move |key| {
            let above = above.iter().flat_map(move |&h| self.lookup(&self.exact, key, h, min_tier));
            above.chain(self.lookup(&self.within, key, last, min_tier))
        });
        found.filter(|k| k.allow != i.allow && i.may_meet(k, k.rank.tier < i.rank.tier))
    }

    /// Returns the rules outside `excluded` that subsume or override `i` across all of its operations,
    /// together with whether they have the same decision as `i`.
    fn deciders(&self, i: &PathRule<'a>, excluded: &dyn Fn(usize) -> bool) -> Option<(bool, Vec<usize>)> {
        let broader = self.broader(i, excluded);
        let mut same_decision = None;
        let mut deciders = Vec::new();
        for op in i.ops() {
            let j = self.decider(i, op, &broader)?;
            if *same_decision.get_or_insert(j.allow == i.allow) != (j.allow == i.allow) {
                return None;
            }
            deciders.push(j.index);
        }
        deciders.sort_unstable();
        deciders.dedup();
        Some((same_decision?, deciders))
    }

    /// Returns a rule from `broader` that either overrides `i` for operation `op`
    /// or produces the same decision without an intervening opposing rule.
    fn decider<'b>(&'b self, i: &'b PathRule<'a>, op: Op, broader: &[&'b PathRule<'a>]) -> Option<&'b PathRule<'a>> {
        let broader = broader.iter().copied().filter(|j| j.decides_all(op));
        let instead = broader.clone().filter(|j| j.covers(i, false) && j.outranks(i)).max_by_key(|j| j.rank());
        if instead.is_some() {
            return instead;
        }
        let agree = broader.filter(|j| j.allow == i.allow);
        agree
            .filter(|j| self.opposed(i, j.rank.tier).all(|k| !k.may_decide(op) || never_between(i, j, k)))
            .max_by_key(|j| (j.counts.0, j.rank()))
    }
}

/// Returns true if opposing rule `k` cannot rank between rule `i` and broader rule `j`
/// on any request decided by `i`.
fn never_between(i: &PathRule, j: &PathRule, k: &PathRule) -> bool {
    let k_deepest = match (k.counts.1, i.counts.1.filter(|_| k.rank.tier >= i.rank.tier)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let below_j = match k.rank.tier.cmp(&j.rank.tier) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => k_deepest.is_some_and(|d| j.counts.0 > d || (j.counts.0 == d && j.outranks(k))),
    };
    let above_i = match k.rank.tier.cmp(&i.rank.tier) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => i.counts.1.is_some_and(|d| d <= k.counts.0) && k.outranks(i),
    };
    below_j || above_i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyFile;
    use proptest::prelude::*;

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
        assert_eq!(lint("deny ~/\nallow ./project\ndeny ./project/.env\nallow read ./project/.env/public"), Vec::<String>::new());
        assert_eq!(lint("allow read ./a\nallow write ./a"), ["1:12: warning[redundant-rule]: line 2 (`./a`) already allows this"]);
        assert_eq!(lint("allow read ./a\nallow write ./a/b"), Vec::<String>::new());
        assert_eq!(lint("allow write ./a\nallow read ./a"), ["2:12: warning[redundant-rule]: line 1 (`./a`) already allows this"]);
        assert_eq!(lint("allow import ./lib/a\ndeny ./lib/a"), Vec::<String>::new());
        assert_eq!(
            lint("allow import ./lib/a\ndeny ./lib/a\nallow import ./src"),
            ["1:14: warning[overridden-rule]: line 2 (`./lib/a`) takes precedence everywhere this rule applies"]
        );
    }

    #[test]
    fn words_with_sets() {
        let none = Vec::<String>::new();
        assert_eq!(lint("let d = a b\nallow /r\ndeny /r/a\ndeny /r/{d}/secret"), none);
        assert_eq!(lint("let h = a.com x.org\nallow net *.com\nallow net {h}"), none);
        assert_eq!(
            lint("allow net a.com\nlet h = a.com x.org\nallow net {h}"),
            ["1:11: warning[redundant-rule]: the same as line 3 (`a.com`)"]
        );
        assert_eq!(lint("let h = *.com a.com x.org\nallow net {h}"), none);
        assert_eq!(lint("let h = a.com a.com\nallow net {h}"), none);
        assert_eq!(
            lint("deny /r/*/secret\nlet d = a b\ndeny /r/{d}/secret"),
            ["3:6: warning[redundant-rule]: line 1 (`/r/*/secret`) already denies this"]
        );
        assert_eq!(
            lint("allow net a.com b.com\nlet h = a.com b.com\nallow net {h}"),
            ["3:11: warning[redundant-rule]: line 1 (`a.com`) and line 1 (`b.com`) already allow this"]
        );
        assert_eq!(
            lint("let h = a.com b.com\nallow net *\ndeny http {h}/admin"),
            ["3:11: warning[bypassable-http-deny]: line 2 (`*`) lets scripts connect to a.com:443 and send any request"]
        );
    }

    #[test]
    fn reported_rules_do_not_justify_others() {
        assert_eq!(lint("allow /r\ndeny /r/a\ndeny read /r/a"), ["3:11: warning[redundant-rule]: line 2 (`/r/a`) already denies this"]);
        assert_eq!(
            lint("allow http a.com\ndeny http a.com/admin\ndeny http a.com/ADMIN"),
            ["3:11: warning[redundant-rule]: line 2 (`a.com/admin`) already denies this"]
        );
        assert_eq!(
            lint("allow http GET POST a.com\nallow http POST GET a.com"),
            ["2:21: warning[redundant-rule]: line 1 (`a.com`) already allows this"]
        );
        assert_eq!(
            lint("allow import /r/a\nallow import /r/b\ndeny /r/a\ndeny /r/b"),
            ["2:14: warning[overridden-rule]: line 4 (`/r/b`) takes precedence everywhere this rule applies"]
        );
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
        assert_eq!(lint("allow read ./src\nallow read ./src/**/*.rs\ndeny ./**/.env"), none);
        assert_eq!(lint("allow ./a/*.txt\ndeny ./a/*"), none);
        assert_eq!(lint("deny ./a/*.pem\nallow ./a/key*"), none);
        assert_eq!(lint("allow ./data\ndeny ./data/secret\nallow read ./data/**/x"), none);
        assert_eq!(lint("deny ~/\nallow ./project\ndeny ./project/.env\nallow read ./project/.env/*"), none);
        assert_eq!(
            lint("allow ./data\ndeny ./data/secret\nallow read ./data/*"),
            ["3:12: warning[redundant-rule]: line 1 (`./data`) already allows this"]
        );
        assert_eq!(
            lint("deny ~/\nallow ~/proj\nallow read ~/proj/src/*.rs"),
            ["3:12: warning[redundant-rule]: line 2 (`~/proj`) already allows this"]
        );
        assert_eq!(
            lint("allow ./A/*\ndeny ./a/*"),
            ["1:7: warning[overridden-rule]: line 2 (`./a/*`) takes precedence everywhere this rule applies"]
        );
        assert_eq!(lint("deny ./a/*\nallow ./a/*.TXT\nallow ./A/*.txt"), none);
        assert_eq!(
            lint("let ext = pem key\ndeny ./**/*.{ext}\ndeny ./keys/*.pem"),
            ["3:6: warning[redundant-rule]: line 2 (`./**/*.pem`) already denies this"]
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
        assert_eq!(lint("allow net *\ndeny http localhost/x"), Vec::<String>::new());
        assert_eq!(
            lint("allow private-net\nallow net *\ndeny http localhost/x"),
            ["3:11: warning[bypassable-http-deny]: line 2 (`*`) lets scripts connect to localhost:443 and send any request"]
        );
        assert_eq!(
            lint("allow net *.example.com\ndeny http *.example.com/admin\nallow net *\ndeny http POST *"),
            [
                "2:11: warning[bypassable-http-deny]: line 1 (`*.example.com`) lets scripts connect to *.example.com:443 and send any request",
                "4:16: warning[bypassable-http-deny]: line 3 (`*`) lets scripts connect to *:443 and send any request",
            ]
        );
        assert_eq!(
            lint("deny net *\nallow net api.example.com\ndeny http *.example.com/admin"),
            [
                "3:11: warning[redundant-rule]: line 1 (`*`) already denies this",
                "3:11: warning[bypassable-http-deny]: line 2 (`api.example.com`) lets scripts connect to api.example.com:443 and send any request",
            ]
        );
        assert_eq!(
            lint("allow net api.example.com:8080\ndeny http api.example.com/x"),
            [
                "2:11: warning[bypassable-http-deny]: line 1 (`api.example.com:8080`) lets scripts connect to api.example.com:8080 and send any request"
            ]
        );
        assert_eq!(
            lint("allow net *\ndeny net *:443\ndeny http api.example.com/x"),
            ["3:11: warning[bypassable-http-deny]: line 1 (`*`) lets scripts connect to api.example.com:80 and send any request"]
        );
        assert_eq!(
            lint("allow net 10.0.0.0/8\ndeny net 10.0.0.1\ndeny net 10.128.0.0/9\ndeny http *"),
            ["4:11: warning[bypassable-http-deny]: line 1 (`10.0.0.0/8`) lets scripts connect to 10.0.0.0/8:443 and send any request"]
        );
        assert_eq!(lint("allow net 10.0.0.0/8\ndeny net 10.0.0.0/9\ndeny net 10.128.0.0/9\ndeny http *"), Vec::<String>::new());
        assert_eq!(
            lint("allow net [::]/0\ndeny http 8.8.8.8/admin"),
            ["2:11: warning[bypassable-http-deny]: line 1 (`[::]/0`) lets scripts connect to [64:ff9b::808:808]:443 and send any request"]
        );
        assert_eq!(
            lint("allow net [2002::]/16\ndeny http 10.0.0.1"),
            [
                "2:11: warning[bypassable-http-deny]: line 1 (`[2002::]/16`) lets scripts connect to [2002:a00:1::/48]:443 and send any request"
            ]
        );
        assert_eq!(lint("allow net [fc00::]/7\ndeny http 10.0.0.1"), Vec::<String>::new());
        assert_eq!(
            lint("allow net [64:ff9b::]/96\ndeny http 0.0.0.0/x"),
            ["2:11: warning[bypassable-http-deny]: line 1 (`[64:ff9b::]/96`) lets scripts connect to [64:ff9b::]:443 and send any request"]
        );
        assert_eq!(
            lint("default allow\ndeny http api.example.com/admin\ndeny http localhost/admin"),
            [
                "2:11: warning[bypassable-http-deny]: `default allow` (line 1) lets scripts connect to api.example.com:443 and send any request"
            ]
        );
        assert_eq!(
            lint("default allow\ndeny net api.example.com\ndeny http api.example.com/admin"),
            ["3:11: warning[redundant-rule]: line 2 (`api.example.com`) already denies this"]
        );
        assert_eq!(lint("default prompt\ndeny http api.example.com/admin"), Vec::<String>::new());
        assert_eq!(
            lint("allow net *\ndeny net *.example.com\ndeny http *.example.com/a"),
            ["3:11: warning[redundant-rule]: line 2 (`*.example.com`) already denies this"]
        );
    }

    #[test]
    fn http_rules_with_wildcards() {
        let none = Vec::<String>::new();
        assert_eq!(
            lint("deny http *.example.com/admin\ndeny http DELETE api.example.com/ADMIN/**"),
            ["2:18: warning[redundant-rule]: line 1 (`*.example.com/admin`) already denies this"]
        );
        assert_eq!(
            lint("allow http *.example.com:443/v1\nallow http GET https://api.example.com/v1/*.json"),
            ["2:16: warning[redundant-rule]: line 1 (`*.example.com:443/v1`) already allows this"]
        );
        assert_eq!(lint("allow http https://api.example.com\nallow http api.example.com:443/x"), none);
        assert_eq!(lint("allow http api.example.com:8080\nallow http https://api.example.com/x"), none);
        assert_eq!(lint("allow http a.com/Admin\nallow http a.com/admin/x"), none);
        assert_eq!(
            lint("allow net *.example.com\nallow http GET api.example.com/v1\ndeny net x.com\ndeny http POST x.com/a"),
            [
                "2:16: warning[redundant-rule]: line 1 (`*.example.com`) already allows this",
                "4:16: warning[redundant-rule]: line 3 (`x.com`) already denies this",
            ]
        );
        assert_eq!(
            lint("allow http api.example.com\ndeny net api.example.com"),
            ["1:12: warning[overridden-rule]: line 2 (`api.example.com`) takes precedence everywhere this rule applies"]
        );
        assert_eq!(
            lint("allow http GET a.com\nallow http POST a.com\nallow http GET POST a.com/v1"),
            ["3:21: warning[redundant-rule]: line 1 (`a.com`) and line 2 (`a.com`) already allow this"]
        );
        assert_eq!(lint("allow http GET a.com\nallow http GET POST a.com/v1"), none);
        assert_eq!(lint("allow http GET a.com/x\nallow http a.com/x/y"), none);
        assert_eq!(
            lint("allow http GET a.com/x\ndeny http GET POST a.com/x"),
            ["1:16: warning[overridden-rule]: line 2 (`a.com/x`) takes precedence everywhere this rule applies"]
        );
    }

    #[test]
    fn net_rules() {
        let none = Vec::<String>::new();
        assert_eq!(
            lint(
                "allow net *.example.com\nallow net api.example.com\ndeny net *\ndeny net x.com:443\nallow net 10.0.0.0/8\nallow net 10.1.2.3"
            ),
            [
                "2:11: warning[redundant-rule]: line 1 (`*.example.com`) already allows this",
                "4:10: warning[redundant-rule]: line 3 (`*`) already denies this",
                "6:11: warning[redundant-rule]: line 5 (`10.0.0.0/8`) already allows this",
            ]
        );
        assert_eq!(
            lint("allow net api.example.com\ndeny net api.example.com"),
            ["1:11: warning[overridden-rule]: line 2 (`api.example.com`) takes precedence everywhere this rule applies"]
        );
        assert_eq!(lint("allow http *.example.com\nallow net api.example.com"), none);
        assert_eq!(lint("allow net *\ndeny net *.example.com\nallow net api.example.com"), none);
        assert_eq!(lint("deny net *\ndeny net 10.0.0.0/8"), none);
        assert_eq!(lint("allow net *\nallow net 8.8.8.8"), none);
        assert_eq!(lint("allow net *\nallow net localhost"), none);
        assert_eq!(
            lint("allow private-net\nallow net *\nallow net localhost"),
            ["3:11: warning[redundant-rule]: line 2 (`*`) already allows this"]
        );
    }

    #[test]
    fn address_rules_are_indexed_by_range() {
        let text: String =
            (0..1000).map(|i| format!("deny net 10.0.{}.{}\n", i / 256, i % 256)).collect::<String>() + "allow net 10.0.0.0/8";
        let rules = PolicyFile::parse(&text, Path::new("/base"), None).unwrap().rules;
        let paths = PathRules::new(rules.rules(), true);
        let candidates = paths.maybe_broader(paths.get(0).unwrap()).count();
        assert!(candidates <= 3, "{candidates} candidates");
    }

    #[test]
    fn http_rules_between_others() {
        let none = Vec::<String>::new();
        assert_eq!(lint("allow http *.example.com\ndeny http api.example.com/admin\nallow http GET api.example.com/admin/x"), none);
        assert_eq!(
            lint("allow http api.example.com\ndeny http *.example.com/a/b\nallow http api.example.com/a"),
            ["3:12: warning[redundant-rule]: line 1 (`api.example.com`) already allows this"]
        );
        assert_eq!(lint("allow http *\ndeny http *.example.com/a/b\nallow http api.example.com/a"), none);
    }

    #[test]
    fn http_rules_for_internal_destinations() {
        let none = Vec::<String>::new();
        assert_eq!(lint("allow http *\nallow http localhost/x\nallow http 10.0.0.1/x"), none);
        assert_eq!(
            lint("allow http *\nallow http 8.8.8.8/x\nallow http *.localhost/x"),
            [
                "2:12: warning[redundant-rule]: line 1 (`*`) already allows this",
                "3:12: warning[redundant-rule]: line 1 (`*`) already allows this",
            ]
        );
        assert_eq!(
            lint("allow private-net\nallow http *\nallow http localhost/x"),
            ["3:12: warning[redundant-rule]: line 2 (`*`) already allows this"]
        );
        assert_eq!(
            lint("allow net 10.0.0.0/8\nallow http 10.0.0.1/x"),
            ["2:12: warning[redundant-rule]: line 1 (`10.0.0.0/8`) already allows this"]
        );
    }

    fn visits(text: &str) -> usize {
        let rules = PolicyFile::parse(text, Path::new("/base"), None).unwrap().rules;
        let paths = PathRules::new(rules.rules(), true);
        for r in &paths.rules {
            paths.deciders(r, &|j| j == r.index);
        }
        paths.visits.get()
    }

    #[test]
    fn net_rule_comparison_scales_linearly() {
        let net = |n: usize, broad: &str, deny: &str| {
            let allow: String = (0..n).map(|i| format!("allow net h{i}.example.com\n")).collect();
            let deny: String = (0..n).map(|i| format!("deny http {deny}/a{i}\n")).collect();
            visits(&format!("allow net {broad}\n{allow}{deny}"))
        };
        for (broad, deny) in [("*", "*.example.com"), ("*.example.com", "*.com")] {
            let (small, large) = (net(100, broad, deny), net(400, broad, deny));
            assert!(large < 6 * small, "{broad} and {deny}: {small} visits for 100 rules, {large} for 400");
        }
    }

    fn rule_line() -> impl Strategy<Value = String> {
        let names = || prop::sample::select(vec!["a", "A", "b", "é", "*", "**", "x.*", "k*", "*.txt", "{n}"]);
        let path = prop::collection::vec(names(), 0..4).prop_map(|n| n.iter().map(|n| format!("/{n}")).collect::<String>());
        let hosts = vec![
            "*",
            "*.com",
            "*.example.com",
            "*.api.example.com",
            "example.com",
            "api.example.com",
            "b.api.example.com",
            "localhost",
            "*.localhost",
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "8.8.8.8",
            "[::1]",
            "[64:ff9b::808:808]",
            "x0.example.com",
            "[::ffff:0:a00:1]",
            "[::]",
            "{h}",
        ];
        let ranges = ["10.0.0.0/8", "8.0.0.0/8", "0.0.0.0/0", "[::]/0", "127.0.0.0/8", "[64:ff9b::]/96", "[2002::]/16"];
        let net_hosts = prop::sample::select(hosts.iter().chain(&ranges).copied().collect::<Vec<_>>());
        let hosts = prop::sample::select(hosts);
        let port = || prop::sample::select(vec!["", "", ":443", ":80", ":8080"]);
        let scheme = prop::sample::select(vec!["", "", "https://", "http://"]);
        let methods = prop::sample::select(vec!["", "", "GET ", "POST ", "GET POST ", "PUT GET "]);
        let access = prop::sample::select(vec!["", "read ", "write ", "import "]);
        let verb = prop::sample::select(vec!["allow", "deny"]);
        prop_oneof![
            (verb.clone(), access, path.clone()).prop_map(|(v, a, p)| format!("{v} {a}/r{p}")),
            (verb.clone(), methods, scheme, hosts, port(), path).prop_map(|(v, m, s, h, port, p)| format!("{v} http {m}{s}{h}{port}{p}")),
            (verb, net_hosts, port()).prop_map(|(v, h, port)| format!("{v} net {h}{port}")),
        ]
    }

    const SETS: &str = "\nlet n = a b\nlet h = api.example.com 10.0.0.1 *.com";

    fn probes() -> &'static [Capability] {
        static PROBES: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(make_probes);
        &PROBES
    }

    fn make_probes() -> Vec<Capability> {
        let mut probes = Vec::new();
        let names = ["a", "A", "b", "é", "É", "x.txt", "k1"];
        let mut paths = vec![String::new()];
        for _ in 0..4 {
            let next: Vec<String> = paths.iter().flat_map(|p| names.iter().map(move |n| format!("{p}/{n}"))).collect();
            paths.extend(next.into_iter().filter(|p| p.matches('/').count() <= 4));
            paths.sort();
            paths.dedup();
        }
        for p in &paths {
            let p = std::path::PathBuf::from(format!("/r{p}"));
            probes.extend([Capability::FsRead(p.clone()), Capability::FsWrite(p.clone()), Capability::Import(p)]);
        }
        let hosts = [
            "example.com",
            "api.example.com",
            "b.api.example.com",
            "c.b.api.example.com",
            "other.com",
            "localhost",
            "a.localhost",
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "10.2.3.4",
            "8.8.8.8",
            "8.1.1.1",
            "::",
            "::1",
            "::ffff:10.0.0.1",
            "64:ff9b::808:808",
            "x0.example.com",
            "x0",
            "::ffff:0:a00:1",
            "2002:a00:1::",
            "LOCALHOST",
            "localhost.",
            "API.example.com",
            "127.0.0.2",
        ];
        for host in hosts {
            for (scheme, port) in [("https", 443), ("http", 80), ("http", 443), ("https", 80), ("https", 8080), ("https", 22)] {
                probes.push(Capability::Net { host: host.into(), port });
                for method in ["GET", "POST", "PUT"] {
                    for path in [
                        "/",
                        "/a",
                        "/A/a",
                        "/x.txt/k1",
                        "/k1/A/a",
                        "/a%2Fk1",
                        "/a/b/x.txt",
                        "/b/a/a",
                        "/%C3%A9",
                        "/%C3%89/a/b",
                        "/a/%C3%A9/k1",
                    ] {
                        let (method, scheme, host, path) = (method.into(), scheme.into(), host.into(), path.into());
                        probes.push(Capability::Http { method, scheme, host, port, path });
                    }
                }
            }
        }
        for host in ["example.com", "api.example.com", "localhost", "other.com"] {
            for ip in ["0.0.0.0", "10.0.0.1", "10.2.3.4", "8.8.8.8", "127.0.0.1", "::1", "64:ff9b::808:808"] {
                for port in [443, 80] {
                    let addr = std::net::SocketAddr::new(ip.parse().unwrap(), port);
                    probes.push(Capability::NetAddr { host: host.into(), addr });
                }
            }
        }
        probes
    }

    fn destinations(rules: &[Rule]) -> Vec<(String, u16)> {
        let probed = probes().iter().filter_map(|cap| if let Capability::Net { host, .. } = cap { Some(host.clone()) } else { None });
        let patterns: Vec<&NetPattern> = rules
            .iter()
            .filter_map(|r| match &r.kind {
                RuleKind::Net(p) => Some(p),
                RuleKind::Http(_, url) => Some(&url.host),
                _ => None,
            })
            .collect();
        let mut hosts: Vec<String> = probed.collect();
        let mut ports = vec![443, 80, 8080, 8443];
        for p in &patterns {
            ports.extend(p.port);
            match &p.kind {
                HostKind::Any => hosts.push("x9".into()),
                HostKind::Suffix(s) => hosts.push(format!("x9.{s}")),
                HostKind::Host(h) => hosts.push(h.clone()),
                HostKind::Ip(ip, len) => {
                    let bits = (if ip.is_ipv4() { 32 } else { 128 }) - *len as u32;
                    let last = u128::MAX.checked_shr(128 - bits).unwrap_or(0);
                    for offset in [0, 1, 2, last / 4 + 1, last / 2, last / 2 + 1, last.saturating_sub(1), last] {
                        let offset = offset.min(last);
                        let at = match ip {
                            IpAddr::V4(v4) => IpAddr::V4((u32::from(*v4) + offset as u32).into()),
                            IpAddr::V6(v6) => IpAddr::V6((u128::from(*v6) + offset).into()),
                        };
                        hosts.push(at.to_string());
                        if let IpAddr::V4(v4) = at {
                            let [a, b, c, d] = v4.octets();
                            let (hi, lo) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
                            hosts.extend([
                                format!("::{v4}"),
                                format!("::ffff:0:{v4}"),
                                format!("64:ff9b::{v4}"),
                                format!("2002:{hi:x}:{lo:x}::"),
                            ]);
                        }
                    }
                }
            }
        }
        hosts.sort();
        hosts.dedup();
        ports.sort();
        ports.dedup();
        hosts.iter().flat_map(|h| ports.iter().map(move |&p| (h.clone(), p))).collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn reported_rules_are_not_needed(
            lines in prop::collection::vec(rule_line(), 1..9),
            private in any::<bool>(),
            default in prop::sample::select(vec!["", "default allow", "default prompt"]),
        ) {
            let (base, home) = (Path::new("/base"), Some(Path::new("/home/me")));
            let text = |lines: &[String]| lines.join("\n") + if private { "\nallow private-net\n" } else { "\n" } + default + SETS;
            let policy = |lines: &[String]| PolicyFile::parse(&text(lines), base, home).unwrap().rule_policy(crate::policy::Decision::Deny);
            let with = policy(&lines);
            let reported: Vec<usize> = lint_policy(&text(&lines), base, home)
                .unwrap()
                .iter()
                .filter(|w| w.code == "redundant-rule" || w.code == "overridden-rule")
                .map(|w| w.line - 1)
                .collect();
            let removals = reported.iter().map(|&line| vec![line]).chain(std::iter::once(reported.clone()));
            for removed in removals {
                let rest: Vec<String> = lines.iter().enumerate().filter(|(i, _)| !removed.contains(i)).map(|(_, l)| l.clone()).collect();
                let without = policy(&rest);
                for cap in probes() {
                    let (a, b) = (with.check_normalized_in(cap, PathSpace::Memory), without.check_normalized_in(cap, PathSpace::Memory));
                    prop_assert_eq!(a, b, "{}\nwithout lines {:?}: {}", text(&lines), removed.iter().map(|l| l + 1).collect::<Vec<_>>(), cap);
                }
            }
        }

        #[test]
        fn bypassable_http_denies_are_reported(
            lines in prop::collection::vec(rule_line(), 1..9),
            private in any::<bool>(),
            default in prop::sample::select(vec!["", "default allow", "default prompt"]),
        ) {
            let text = lines.join("\n") + if private { "\nallow private-net\n" } else { "\n" } + default + SETS;
            let warnings = lint_policy(&text, Path::new("/base"), None).unwrap();
            let policy = PolicyFile::parse(&text, Path::new("/base"), None).unwrap().rule_policy(crate::policy::Decision::Deny);
            let rules = policy.rules();
            let dests = destinations(rules);
            let mut reached: FxHashMap<usize, Option<(String, u16)>> = FxHashMap::default();
            for (i, rule) in rules.iter().enumerate() {
                let (false, RuleKind::Http(_, url), Some(o)) = (rule.allow, &rule.kind, &rule.origin) else { continue };
                if rules[..i].iter().any(|r| !r.allow && r.kind == rule.kind) {
                    continue;
                }
                let deny = crate::policy::RulePolicy::new().deny_net(&url.host.to_string());
                let found = dests.iter().find(|(host, port)| {
                    let cap = Capability::Net { host: host.clone(), port: *port };
                    deny.verdict(&cap, PathSpace::Memory, &[]).is_some()
                        && policy.check_normalized_in(&cap, PathSpace::Memory) == crate::policy::Decision::Allow
                });
                let line = reached.entry(o.line).or_default();
                *line = line.take().or(found.cloned());
            }
            for (line, reached) in reached {
                let reported = warnings.iter().any(|w| w.code == "bypassable-http-deny" && w.line == line);
                prop_assert_eq!(reported, reached.is_some(), "{}\nline {}: {:?}", text, line, reached);
            }
        }

        #[test]
        fn narrower_http_rules_are_reported(
            allow in any::<bool>(),
            broad_host in 0..3usize,
            narrow_host in 0..3usize,
            broad_port in any::<bool>(),
            scheme in any::<bool>(),
            method in any::<bool>(),
            path in prop::collection::vec(prop::sample::select(vec!["a", "*", "**", "x.*"]), 0..3),
            extra in prop::collection::vec(prop::sample::select(vec!["a", "*", "x.json"]), 0..2),
            narrow_first in any::<bool>(),
        ) {
            let hosts = ["*", "*.example.com", "api.example.com"];
            let verb = if allow { "allow" } else { "deny" };
            let path: String = path.iter().map(|n| format!("/{n}")).collect();
            let broad = format!("{verb} http {}{}{path}", hosts[broad_host], if broad_port { ":443" } else { "" });
            let narrow = format!(
                "{verb} http {}{}{}{}{path}{}",
                if method { "GET " } else { "" },
                if scheme { "https://" } else { "" },
                hosts[broad_host.max(narrow_host)],
                if broad_port && !scheme { ":443" } else { "" },
                extra.iter().map(|n| format!("/{n}")).collect::<String>(),
            );
            let (text, line) = if narrow_first { (format!("{narrow}\n{broad}"), 1) } else { (format!("{broad}\n{narrow}"), 2) };
            let warnings = lint_policy(&text, Path::new("/base"), None).unwrap();
            let line = if narrow == broad { 2 } else { line };
            prop_assert!(warnings.iter().any(|w| w.line == line && w.code == "redundant-rule"), "{}: {:?}", text, warnings);
        }
    }
}
