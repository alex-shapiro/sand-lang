//! The policy file linter.

use super::file::parse;
use super::net::{HostKind, NetPattern, Unnamed, domains_above};
use super::path::{self, Pattern, Seg, name_counts};
use super::rules::{
    Access, IMPORT, READ, Rank, Rule, RuleKind, WRITE, file_segments, fold_segments, method_matches, precedence, scheme_matches,
};
use super::{Capability, PathSpace, PolicyError};
use crate::lint::LintWarning;
use crate::vm::value::{FxBuild, FxHashMap};
use std::hash::BuildHasher;
use std::path::Path;

/// Lints a policy file (parsed as by [`super::PolicyFile::parse`]):
///
/// - `redundant-rule`: a rule repeats another, or broader rules already
///   decide everything it covers the same way;
/// - `overridden-rule`: rules that take precedence (more specific, or a
///   deny as specific) decide everything it covers the other way;
/// - `bypassable-http-deny`: a `deny http` rule names a destination that
///   a `net` rule lets scripts connect to (and send anything);
/// - `unused-set`: a `let` set that is never referenced in a rule.
///
/// The linter compares file and http rules by what their patterns match,
/// wildcards included, and compares paths as written, without resolving
/// symlinks. A `net` rule counts as an http rule for any method and path.
/// The linter reports a rule only when, for each file access or http method
/// the rule names, one other rule shows that the rule is not needed. It
/// compares the hosts and ports of `deny http` and `net` rules by what
/// their patterns match.
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
    let describe = |i: usize| match &rules[i].origin {
        Some(o) => format!("line {} (`{}`)", o.line, o.text),
        None => "another rule".to_string(),
    };
    let describe_all = |rules: &[usize]| match rules {
        [] => String::new(),
        [one] => describe(*one),
        [init @ .., last] => format!("{} and {}", init.iter().map(|&i| describe(i)).collect::<Vec<_>>().join(", "), describe(*last)),
    };
    let paths = PathRules::new(rules, deny_private);
    let unnamed = Unnamed::new(rules.iter().filter_map(|r| match &r.kind {
        RuleKind::Net(p) => Some(p),
        RuleKind::Http(_, url) => Some(&url.host),
        _ => None,
    }));
    // For each host pattern: a net rule that lets scripts connect to a
    // destination it matches, and the pattern of such destinations.
    let mut bypasses: FxHashMap<NetPattern, Option<(usize, NetPattern)>> = FxHashMap::default();
    let mut bypass = |reach: NetPattern| {
        let found = bypasses.entry(reach).or_insert_with_key(|reach| {
            let (host, port) = unnamed.dest(reach)?;
            let Some((true, j)) = policy.verdict(&Capability::Net { host, port }, PathSpace::Memory, &[]) else { return None };
            Some((j, NetPattern { port: Some(port), ..reach.clone() }))
        });
        found.clone()
    };
    let allow_imports = rules.iter().filter(|r| r.allow && matches!(r.kind, RuleKind::Files(Access::Import, _))).count();
    let mut same: FxHashMap<(bool, &RuleKind), Vec<usize>> = FxHashMap::default();
    for (i, r) in rules.iter().enumerate() {
        same.entry((r.allow, &r.kind)).or_default().push(i);
    }
    for (i, rule) in rules.iter().enumerate() {
        // Repeats of this rule.
        let copies = &same[&(rule.allow, &rule.kind)];
        if copies[0] != i {
            warn(i, "redundant-rule", format!("the same as {}", describe(copies[0])));
            continue;
        }
        let Some(r) = paths.get(i) else { continue };
        // The only `allow import` rule also denies the imports that no rule matches.
        let restricts_imports = rule.allow && matches!(rule.kind, RuleKind::Files(Access::Import, _)) && allow_imports == 1;
        if !matches!(rule.kind, RuleKind::Net(_)) && !restricts_imports {
            let (mut agree, mut oppose, mut needed) = (Vec::new(), Vec::new(), false);
            let broader = paths.broader(r, copies);
            for op in r.ops() {
                match paths.decider(r, op, &broader, copies) {
                    Some(j) if j.allow == r.allow => agree.push(j.index),
                    Some(j) => oppose.push(j.index),
                    None => needed = true,
                }
            }
            for list in [&mut agree, &mut oppose] {
                list.sort_unstable();
                list.dedup();
            }
            let one = |list: &[usize], singular: &'static str, plural: &'static str| if list.len() == 1 { singular } else { plural };
            let verb = match rule.allow {
                true => one(&agree, "allows", "allow"),
                false => one(&agree, "denies", "deny"),
            };
            if !needed && oppose.is_empty() {
                warn(i, "redundant-rule", format!("{} already {verb} this", describe_all(&agree)));
            } else if !needed && agree.is_empty() {
                let verb = one(&oppose, "takes", "take");
                warn(i, "overridden-rule", format!("{} {verb} precedence everywhere this rule applies", describe_all(&oppose)));
            }
        }
        if let RuleKind::Http(_, url) = &rule.kind
            && !rule.allow
        {
            // First the destinations that only rules for all of the rule's
            // hosts match, then those that net rules that allow name.
            let found = bypass(url.host.clone()).or_else(|| {
                let reach = paths.opposed(r, copies).filter_map(|k| match (&rules[k.index].kind, &k.scope) {
                    (RuleKind::Net(_), Scope::Http { host, .. }) => url.host.intersection(host),
                    _ => None,
                });
                reach.into_iter().find_map(&mut bypass)
            });
            if let Some((j, reach)) = found {
                warn(i, "bypassable-http-deny", format!("{} lets scripts connect to {reach} and send any request", describe(j)));
            }
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

/// The requests other than paths that a rule matches.
enum Scope<'a> {
    /// File requests (`READ`, `WRITE`, `IMPORT`).
    Files(u8),
    /// Http requests. A net rule matches them for any method and scheme.
    Http { host: &'a NetPattern, scheme: Option<&'static str>, methods: Option<&'a [String]> },
}

/// One kind of request: a file access, or an http method (`None`: every
/// method).
#[derive(Clone, Copy)]
enum Op<'a> {
    File(u8),
    Method(Option<&'a str>),
}

/// A file rule (as the policy matches it in a memory file system), an http
/// rule, or a net rule as the policy matches http requests with it.
struct PathRule<'a> {
    index: usize,
    allow: bool,
    scope: Scope<'a>,
    /// Whether it may decide a request to an internal destination, which an
    /// allow rule may do only if it names the destination (see
    /// [`super::RulePolicy::deny_private_networks`]).
    internal: bool,
    /// The pattern, root first (a deny rule's names folded, as the policy
    /// compares them).
    segs: Vec<Seg>,
    /// The pattern with its names folded, if every path it matches folds to
    /// a path the folded pattern matches: `None` for an allow rule with
    /// non-ASCII text around a wildcard.
    folded: Option<Vec<Seg>>,
    /// Hashes of the leading parts of its folded names before the first
    /// wildcard: `prefixes[n]` of the first `n` names.
    prefixes: Vec<u64>,
    rank: Rank,
    /// The fewest and most names of the paths it matches.
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
            RuleKind::Http(methods, url) => {
                let scope = Scope::Http { host: &url.host, scheme: url.scheme, methods: methods.as_deref() };
                (scope, url.path.clone(), Rank::http(methods.as_deref(), url))
            }
            RuleKind::Net(host) => (Scope::Http { host, scheme: None, methods: None }, Vec::new(), Rank::net(host)),
            _ => return None,
        };
        let internal = match scope {
            Scope::Http { host, .. } => !rule.allow || !deny_private || host.may_name_internal(),
            Scope::Files(_) => true,
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
            internal,
            prefixes,
            rank,
            counts: name_counts(&raw),
            folded: (!rule.allow || raw.iter().all(exact_fold)).then(|| folded.clone()),
            segs: if rule.allow { raw } else { folded },
        })
    }

    /// The kinds of request it decides.
    fn ops(&self) -> Vec<Op<'a>> {
        match self.scope {
            Scope::Files(mask) => [READ, WRITE, IMPORT].into_iter().filter(|&op| mask & op != 0).map(Op::File).collect(),
            Scope::Http { methods: Some(methods), .. } => methods.iter().map(|m| Op::Method(Some(m))).collect(),
            Scope::Http { methods: None, .. } => vec![Op::Method(None)],
        }
    }

    /// Whether it decides every request of kind `op` that its patterns match.
    fn decides_all(&self, op: Op) -> bool {
        match (&self.scope, op) {
            (Scope::Files(mask), Op::File(op)) => mask & op != 0,
            (Scope::Http { methods, .. }, Op::Method(Some(m))) => method_matches(*methods, m),
            (Scope::Http { methods, .. }, Op::Method(None)) => methods.is_none(),
            _ => false,
        }
    }

    /// Whether it may decide a request of kind `op`.
    fn may_decide(&self, op: Op) -> bool {
        match (&self.scope, op) {
            (Scope::Files(mask), Op::File(op)) => mask & op != 0,
            (Scope::Http { methods, .. }, Op::Method(m)) => m.is_none_or(|m| method_matches(*methods, m)),
            _ => false,
        }
    }

    /// How it ranks where it matches a path as deep as other rules do
    /// (see [`Rank`]).
    fn rank(&self) -> impl Ord {
        precedence(self.rank.spec(0), self.allow, self.index)
    }

    fn outranks(&self, other: &PathRule) -> bool {
        self.rank() > other.rank()
    }

    /// The patterns of both rules, compared as the policy matches them:
    /// folded unless both rules allow.
    fn comparable<'b>(&'b self, other: &'b PathRule) -> Option<(&'b [Seg], &'b [Seg])> {
        if self.allow && other.allow {
            return Some((&self.segs, &other.segs));
        }
        Some((self.folded.as_deref()?, other.folded.as_deref()?))
    }

    /// Whether this rule matches every request `other` matches (`below`: or
    /// one for an ancestor of its path), apart from the kind of request. An
    /// allow rule never covers a deny rule, which also matches other
    /// spellings of names and addresses.
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
                a.covers(b) && schemes && (a.is_explicit() || self.internal || !other.internal)
            }
            _ => false,
        }
    }

    /// Whether a request this rule matches (`self_below`: or one for a path
    /// below it) may also match `other`, or be for a path below one `other`
    /// matches.
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

    /// The keys of the rules that may cover this one.
    fn broader_keys(&self) -> Vec<Key<'a>> {
        let Scope::Http { host, .. } = self.scope else { return vec![Key::Files] };
        match &host.kind {
            HostKind::Any => vec![Key::Any],
            HostKind::Suffix(s) => [Key::Any, Key::Suffix(s)].into_iter().chain(domains_above(s).map(Key::Suffix)).collect(),
            HostKind::Host(h) => [Key::Any, Key::Name(h)].into_iter().chain(domains_above(h).map(Key::Suffix)).collect(),
            HostKind::Ip(..) => vec![Key::Any, Key::Ips],
        }
    }

    /// The keys of the rules that may match a request this one matches.
    fn related_keys(&self) -> Vec<Key<'a>> {
        let mut keys = self.broader_keys();
        if let Scope::Http { host, .. } = self.scope {
            match &host.kind {
                HostKind::Any => keys = vec![Key::Http],
                HostKind::Suffix(s) => keys.push(Key::Under(s)),
                _ => {}
            }
        }
        keys
    }

    /// The keys to find this rule by.
    fn keys(&self) -> Vec<Key<'a>> {
        let Scope::Http { host, .. } = self.scope else { return vec![Key::Files] };
        let (own, name) = match &host.kind {
            HostKind::Any => (Key::Any, None),
            HostKind::Suffix(s) => (Key::Suffix(s), Some(s)),
            HostKind::Host(h) => (Key::Name(h), Some(h)),
            HostKind::Ip(..) => (Key::Ips, None),
        };
        [Key::Http, own].into_iter().chain(name.into_iter().flat_map(|n| domains_above(n)).map(Key::Under)).collect()
    }
}

/// What [`PathRules`] indexes rules by, besides their names: their kind
/// and host pattern.
#[derive(Hash)]
enum Key<'a> {
    Files,
    /// Every http or net rule.
    Http,
    /// Rules for any host.
    Any,
    /// Rules for a host name.
    Name(&'a str),
    /// Rules for the strict subdomains of a domain.
    Suffix(&'a str),
    /// Rules for a host name or domain strictly below a domain.
    Under(&'a str),
    /// Rules for addresses.
    Ips,
}

/// The file, http and net rules of a policy, indexed by their keys and the
/// folded names before their first wildcard: two rules can match the same
/// request only if the names of one begin with those of the other. The
/// indices hash their keys, so they may also give unrelated rules.
struct PathRules<'a> {
    rules: Vec<PathRule<'a>>,
    /// The position in `rules` of each policy rule (`usize::MAX` if not a
    /// file, http or net rule).
    at: Vec<usize>,
    /// The rules by key and names before the first wildcard.
    exact: FxHashMap<u64, Vec<usize>>,
    /// The rules by key and each leading part of their names before the
    /// first wildcard.
    within: FxHashMap<u64, Vec<usize>>,
    hasher: FxBuild,
}

impl<'a> PathRules<'a> {
    fn new(rules: &'a [Rule], deny_private: bool) -> PathRules<'a> {
        let mut paths = PathRules {
            rules: Vec::new(),
            at: vec![usize::MAX; rules.len()],
            exact: Default::default(),
            within: Default::default(),
            hasher: FxBuild::default(),
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
        paths
    }

    fn get(&self, index: usize) -> Option<&PathRule<'a>> {
        self.rules.get(self.at[index])
    }

    /// The rules indexed in `map` by `key` and the names hashed as `h`.
    fn lookup<'b>(&'b self, map: &'b FxHashMap<u64, Vec<usize>>, key: u64, h: u64) -> impl Iterator<Item = &'b PathRule<'a>> {
        map.get(&self.hasher.hash_one((key, h))).into_iter().flatten().map(|&at| &self.rules[at])
    }

    fn hash_keys(&self, keys: Vec<Key>) -> Vec<u64> {
        keys.into_iter().map(|k| self.hasher.hash_one(k)).collect()
    }

    /// The rules other than `copies` that match every request `i` matches,
    /// or one for an ancestor of its path. Only the rules whose names before
    /// the first wildcard begin those of `i` may do so.
    fn broader<'b>(&'b self, i: &'b PathRule<'a>, copies: &[usize]) -> Vec<&'b PathRule<'a>> {
        let keys = self.hash_keys(i.broader_keys());
        let found = keys.iter().flat_map(|&key| i.prefixes.iter().flat_map(move |&h| self.lookup(&self.exact, key, h)));
        found.filter(|j| !copies.contains(&j.index) && j.covers(i, true)).collect()
    }

    /// The rules other than `copies` that decide the other way and may
    /// match a request `i` matches (or one for a path below it, if they rank
    /// below `i`). Only the rules whose names before the first wildcard
    /// begin those of `i`, or are the start of them, may do so.
    fn opposed<'b>(&'b self, i: &'b PathRule<'a>, copies: &'b [usize]) -> impl Iterator<Item = &'b PathRule<'a>> {
        let (&last, above) = i.prefixes.split_last().expect("the empty prefix");
        let found = self.hash_keys(i.related_keys()).into_iter().flat_map(move |key| {
            let above = above.iter().flat_map(move |&h| self.lookup(&self.exact, key, h));
            above.chain(self.lookup(&self.within, key, last))
        });
        found.filter(|k| k.allow != i.allow && !copies.contains(&k.index) && i.may_meet(k, k.rank.tier < i.rank.tier))
    }

    /// A rule that shows that `i` is not needed for requests of kind `op`:
    /// one that decides every request `i` matches instead of `i`, or one
    /// that would decide them the same way without `i`. `None` if no
    /// single rule shows this. `broader` are the rules [`PathRules::broader`]
    /// gives.
    ///
    /// Where `i` decides a request for a path below a path `y` that `i`
    /// matches, `i` also decides the request for `y`, and every rule of the
    /// same tier as `i` ranks the same for both paths. So for such rules
    /// only the paths `i` matches need to be considered.
    fn decider<'b>(&'b self, i: &'b PathRule<'a>, op: Op, broader: &[&'b PathRule<'a>], copies: &'b [usize]) -> Option<&'b PathRule<'a>> {
        let broader = broader.iter().copied().filter(|j| j.decides_all(op));
        let instead = broader.clone().filter(|j| j.covers(i, false) && j.outranks(i)).max_by_key(|j| j.rank());
        if instead.is_some() {
            return instead;
        }
        let agree = broader.filter(|j| j.allow == i.allow);
        agree
            .filter(|j| self.opposed(i, copies).all(|k| !k.may_decide(op) || never_between(i, j, k)))
            .max_by_key(|j| (j.counts.0, j.rank()))
    }
}

/// Whether rule `k`, which decides the other way, never ranks below `i`
/// and above `j` for a request `i` decides (where `j` matches the path or
/// an ancestor). Rank depends first on the tier, then on the depth of the
/// deepest path a rule matches (the path or an ancestor). There, `j`
/// matches at least `j.counts.0` names deep, and `k` at most `k.counts.1`
/// names deep (and, of the same tier as `i`, at most `i.counts.1`). If no
/// path `i` matches is deeper than the shallowest path `k` matches, then
/// `k` matches only paths that `i` matches exactly, at the same depth.
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
        // A rule overriding a broader one of the other kind is needed.
        assert_eq!(lint("deny ~/\nallow ./project\ndeny ./project/.env\nallow read ./project/.env/public"), Vec::<String>::new());
        assert_eq!(lint("allow read ./a\nallow write ./a"), ["1:12: warning[redundant-rule]: line 2 (`./a`) already allows this"]);
        assert_eq!(lint("allow read ./a\nallow write ./a/b"), Vec::<String>::new());
        assert_eq!(lint("allow write ./a\nallow read ./a"), ["2:12: warning[redundant-rule]: line 1 (`./a`) already allows this"]);
        // The only `allow import` rule restricts imports to its paths.
        assert_eq!(lint("allow import ./lib/a\ndeny ./lib/a"), Vec::<String>::new());
        assert_eq!(
            lint("allow import ./lib/a\ndeny ./lib/a\nallow import ./src"),
            ["1:14: warning[overridden-rule]: line 2 (`./lib/a`) takes precedence everywhere this rule applies"]
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
        // Only an explicit rule lets scripts connect to an internal destination.
        assert_eq!(lint("allow net *\ndeny http localhost/x"), Vec::<String>::new());
        assert_eq!(
            lint("allow private-net\nallow net *\ndeny http localhost/x"),
            ["3:11: warning[bypassable-http-deny]: line 2 (`*`) lets scripts connect to localhost:443 and send any request"]
        );
        // Wildcard denies, and net rules for some of the hosts or ports they name.
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
        // Narrower ranges deny some or all of a range that a net rule allows.
        assert_eq!(
            lint("allow net 10.0.0.0/8\ndeny net 10.0.0.1\ndeny net 10.0.0.0/9\ndeny http *"),
            ["4:11: warning[bypassable-http-deny]: line 1 (`10.0.0.0/8`) lets scripts connect to 10.0.0.0/8:443 and send any request"]
        );
        assert_eq!(lint("allow net 10.0.0.0/8\ndeny net 10.0.0.0/9\ndeny net 10.128.0.0/9\ndeny http *"), Vec::<String>::new());
        // A more specific net rule denies the hosts the wildcard names.
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
        // A rule with a scheme does not match the other scheme on its port.
        assert_eq!(lint("allow http https://api.example.com\nallow http api.example.com:443/x"), none);
        assert_eq!(lint("allow http api.example.com:8080\nallow http https://api.example.com/x"), none);
        // Allow rules compare names exactly; deny rules fold them.
        assert_eq!(lint("allow http a.com/Admin\nallow http a.com/admin/x"), none);
        // A net rule decides http requests for any method and path.
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
        // Each method may be decided by another rule.
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
    fn http_rules_between_others() {
        let none = Vec::<String>::new();
        // A deny for the same host ranks between the rules.
        assert_eq!(lint("allow http *.example.com\ndeny http api.example.com/admin\nallow http GET api.example.com/admin/x"), none);
        // A deny for a broader host ranks below both.
        assert_eq!(
            lint("allow http api.example.com\ndeny http *.example.com/a/b\nallow http api.example.com/a"),
            ["3:12: warning[redundant-rule]: line 1 (`api.example.com`) already allows this"]
        );
        // The deny ranks between the rules below `/a`, where the host decides first.
        assert_eq!(lint("allow http *\ndeny http *.example.com/a/b\nallow http api.example.com/a"), none);
    }

    #[test]
    fn http_rules_for_internal_destinations() {
        let none = Vec::<String>::new();
        // A wildcard allow rule does not allow internal destinations.
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

    /// A policy line for paths under `/r` or for hosts that [`probes`] name.
    fn rule_line() -> impl Strategy<Value = String> {
        let names = || prop::sample::select(vec!["a", "A", "b", "é", "*", "**", "x.*", "k*", "*.txt"]);
        let path = prop::collection::vec(names(), 0..4).prop_map(|n| n.iter().map(|n| format!("/{n}")).collect::<String>());
        let hosts = || {
            prop::sample::select(vec![
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
                "10.0.0.0/8",
                "8.0.0.0/8",
                "0.0.0.0/0",
                "[::]/0",
            ])
        };
        let port = || prop::sample::select(vec!["", "", ":443", ":80", ":8080"]);
        let scheme = prop::sample::select(vec!["", "", "https://", "http://"]);
        let methods = prop::sample::select(vec!["", "", "GET ", "POST ", "GET POST ", "PUT GET "]);
        let access = prop::sample::select(vec!["", "read ", "write ", "import "]);
        let verb = prop::sample::select(vec!["allow", "deny"]);
        // In an http rule, `/len` after an address begins the path.
        prop_oneof![
            (verb.clone(), access, path.clone()).prop_map(|(v, a, p)| format!("{v} {a}/r{p}")),
            (verb.clone(), methods, scheme, hosts(), port(), path).prop_map(|(v, m, s, h, port, p)| format!("{v} http {m}{s}{h}{port}{p}")),
            (verb, hosts(), port()).prop_map(|(v, h, port)| format!("{v} net {h}{port}")),
        ]
    }

    /// Requests that rules from [`rule_line`] decide in different ways.
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
        ];
        for host in hosts {
            for (scheme, port) in [("https", 443), ("http", 80), ("http", 443), ("https", 80), ("https", 8080)] {
                probes.push(Capability::Net { host: host.into(), port });
                for method in ["GET", "POST", "PUT"] {
                    for path in ["/", "/a", "/A/a", "/x.txt/k1", "/k1/A/a", "/a%2Fk1", "/a/b/x.txt", "/b/a/a"] {
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

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Removing a rule the linter reports does not change any decision.
        #[test]
        fn reported_rules_are_not_needed(lines in prop::collection::vec(rule_line(), 1..9), private in any::<bool>()) {
            let (base, home) = (Path::new("/base"), Some(Path::new("/home/me")));
            let text = |lines: &[String]| lines.join("\n") + if private { "\nallow private-net" } else { "" };
            let policy = |lines: &[String]| {
                let rules = PolicyFile::parse(&text(lines), base, home).unwrap().rules;
                if private { rules } else { rules.deny_private_networks() }
            };
            let with = policy(&lines);
            for w in lint_policy(&text(&lines), base, home).unwrap() {
                if w.code == "unused-set" || w.code == "bypassable-http-deny" {
                    continue;
                }
                let mut rest = lines.clone();
                rest.remove(w.line - 1);
                let without = policy(&rest);
                for cap in probes() {
                    let (a, b) = (with.check_normalized_in(cap, PathSpace::Memory), without.check_normalized_in(cap, PathSpace::Memory));
                    prop_assert_eq!(a, b, "{}\n{}: {}", text(&lines), w, cap);
                }
            }
        }

        /// A `deny http` rule is reported if net rules let scripts connect
        /// to a destination it names.
        #[test]
        fn bypassable_http_denies_are_reported(lines in prop::collection::vec(rule_line(), 1..9), private in any::<bool>()) {
            let text = lines.join("\n") + if private { "\nallow private-net" } else { "" };
            let warnings = lint_policy(&text, Path::new("/base"), None).unwrap();
            let file = PolicyFile::parse(&text, Path::new("/base"), None).unwrap();
            let policy = if private { file.rules.clone() } else { file.rules.clone().deny_private_networks() };
            let dests: Vec<(&String, u16)> =
                probes().iter().filter_map(|cap| if let Capability::Net { host, port } = cap { Some((host, *port)) } else { None }).collect();
            for rule in policy.rules() {
                let (false, RuleKind::Http(_, url), Some(o)) = (rule.allow, &rule.kind, &rule.origin) else { continue };
                // The linter reports only that a repeat is a repeat.
                if warnings.iter().any(|w| w.line == o.line && w.message.starts_with("the same as")) {
                    continue;
                }
                let reached = dests.iter().find(|(host, port)| {
                    let named = NetPattern::parse(&crate::policy::net::format_host_port(host, *port)).unwrap();
                    let cap = Capability::Net { host: (*host).clone(), port: *port };
                    url.host.covers(&named) && matches!(policy.verdict(&cap, PathSpace::Memory, &[]), Some((true, _)))
                });
                let reported = warnings.iter().any(|w| w.code == "bypassable-http-deny" && w.line == o.line);
                prop_assert!(reported || reached.is_none(), "{}\n{:?}: {:?}", text, o.text, reached);
            }
        }

        /// A rule for less than a broader rule that decides the same way is
        /// reported.
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
