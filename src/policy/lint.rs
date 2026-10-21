//! The policy file linter.

use super::file::parse;
use super::net::{HostKind, format_host_port};
use super::path::{self, Pattern, Seg, name_counts, path_segments};
use super::rules::{IMPORT, READ, Rule, RuleKind, WRITE};
use super::{Capability, PathSpace, PolicyError, fold_name};
use crate::lint::LintWarning;
use crate::vm::value::{FxBuild, FxHashMap};
use std::hash::BuildHasher;
use std::path::Path;

/// Lints a policy file (parsed as by [`super::PolicyFile::parse`]):
///
/// - `redundant-rule`: a rule repeats another, or a broader rule already
///   decides everything it covers the same way;
/// - `overridden-rule`: a rule that takes precedence (more specific, or a
///   deny as specific) decides everything it covers the other way;
/// - `bypassable-http-deny`: a `deny http` rule for a host that `net` rules
///   let scripts connect to (and send it anything);
/// - `unused-set`: a `let` set that is never referenced in a rule.
///
/// Rules about paths are compared as written, without resolving symlinks.
/// A rule is reported only when a single other rule shows it is not needed.
pub fn lint_policy(text: &str, base: &Path, home: Option<&Path>) -> Result<Vec<LintWarning>, Vec<PolicyError>> {
    let parsed = parse(text, base, home)?;
    let policy = &parsed.file.rules;
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
    let files = FileRules::new(rules);
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
        match &rule.kind {
            RuleKind::Files(..) => {
                let i = files.get(i);
                let (mut same, mut other, mut needed) = (None, None, false);
                for op in [READ, WRITE, IMPORT].into_iter().filter(|&op| i.mask & op != 0) {
                    match files.decider(i, op, copies) {
                        Some(j) if j.allow == i.allow => same = Some(j.index),
                        Some(j) => other = Some(j.index),
                        None => needed = true,
                    }
                }
                let verb = if rule.allow { "allows" } else { "denies" };
                match (needed, same, other) {
                    (false, Some(j), None) => warn(i.index, "redundant-rule", format!("{} already {verb} this", describe(j))),
                    (false, None, Some(j)) => {
                        warn(i.index, "overridden-rule", format!("{} takes precedence everywhere this rule applies", describe(j)))
                    }
                    _ => {}
                }
            }
            RuleKind::Http(_, url) if !rule.allow => {
                let host = match &url.host.kind {
                    HostKind::Host(h) => h.clone(),
                    HostKind::Ip(ip, _) if url.host.is_explicit() => ip.to_string(),
                    _ => continue,
                };
                let port = url.host.port.unwrap_or(443);
                if let Some((true, j)) = policy.verdict(&Capability::Net { host: host.clone(), port }, PathSpace::Memory, &[]) {
                    warn(
                        i,
                        "bypassable-http-deny",
                        format!("{} lets scripts connect to {} and send any request", describe(j), format_host_port(&host, port)),
                    );
                }
            }
            _ => {}
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

/// A file rule as the policy matches it, in a memory file system.
struct FileRule {
    index: usize,
    allow: bool,
    /// The requests it decides (`READ`, `WRITE`, `IMPORT`).
    mask: u8,
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
    weight: u32,
    /// The fewest and most names of the paths it matches.
    counts: (usize, Option<usize>),
}

impl FileRule {
    /// How rules matching a path to the same depth rank: the heavier, then
    /// a deny, then the first.
    fn rank(&self) -> (u32, bool, std::cmp::Reverse<usize>) {
        (self.weight, !self.allow, std::cmp::Reverse(self.index))
    }

    fn outranks(&self, other: &FileRule) -> bool {
        self.rank() > other.rank()
    }

    /// The patterns of both rules, compared as the policy matches them:
    /// folded unless both rules allow.
    fn comparable<'a>(&'a self, other: &'a FileRule) -> Option<(&'a [Seg], &'a [Seg])> {
        if self.allow && other.allow {
            return Some((&self.segs, &other.segs));
        }
        Some((self.folded.as_deref()?, other.folded.as_deref()?))
    }

    /// Whether this rule matches every path `other` matches (`below`: or an
    /// ancestor of it). An allow rule matches names as written, so it never
    /// covers a deny rule, which also matches other spellings.
    fn covers(&self, other: &FileRule, below: bool) -> bool {
        if self.allow && !other.allow {
            return false;
        }
        self.comparable(other).is_some_and(|(a, b)| path::covers(Pattern { segs: a, below }, Pattern { segs: b, below: false }))
    }

    /// Whether a path this rule matches may be at or below a path `other`
    /// matches.
    fn may_be_below(&self, other: &FileRule) -> bool {
        self.comparable(other).is_none_or(|(a, b)| path::may_meet(Pattern { segs: a, below: false }, Pattern { segs: b, below: true }))
    }
}

/// The file rules of a policy, indexed by the folded names before their
/// first wildcard: two rules can match the same path only if those of one
/// begin with those of the other. The indices hash the names, so they may
/// also give unrelated rules.
struct FileRules {
    rules: Vec<FileRule>,
    /// The position in `rules` of each policy rule (`usize::MAX` if not a
    /// file rule).
    at: Vec<usize>,
    /// The rules by their names before the first wildcard.
    exact: FxHashMap<u64, Vec<usize>>,
    /// The rules by each leading part of their names before the first
    /// wildcard.
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
            let raw: Vec<Seg> = path_segments(&prefix).iter().map(|s| Seg::Lit((**s).into())).chain(pattern.rest.iter().cloned()).collect();
            let folded: Vec<Seg> = raw.iter().map(|s| s.map(|n| fold_name(n).into_owned())).collect();
            let exact_fold = |s: &Seg| !matches!(s, Seg::Glob(g) if g.pieces().iter().any(|p| !p.is_ascii()));
            let names = folded.iter().map_while(|s| if let Seg::Lit(n) = s { Some(n) } else { None });
            let prefixes: Vec<u64> = std::iter::once(0)
                .chain(names.scan(0, |h, n| {
                    *h = hasher.hash_one((*h, n));
                    Some(*h)
                }))
                .collect();
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
                prefixes,
                weight: raw.iter().map(Seg::weight).sum(),
                counts: name_counts(&raw),
                folded: (!rule.allow || raw.iter().all(exact_fold)).then(|| folded.clone()),
                segs: if rule.allow { raw } else { folded },
            });
        }
        files
    }

    fn get(&self, index: usize) -> &FileRule {
        &self.rules[self.at[index]]
    }

    /// The rules that may match every path `rule` matches: those whose
    /// names before the first wildcard begin those of `rule`.
    fn broader<'a>(&'a self, rule: &'a FileRule) -> impl Iterator<Item = &'a FileRule> {
        rule.prefixes.iter().flat_map(|h| self.exact.get(h).into_iter().flatten()).map(|&at| &self.rules[at])
    }

    /// The rules that may match a path at, above or below one `rule` matches.
    fn related<'a>(&'a self, rule: &'a FileRule) -> impl Iterator<Item = &'a FileRule> {
        let (&last, above) = rule.prefixes.split_last().expect("the empty prefix");
        let above = above.iter().flat_map(|h| self.exact.get(h).into_iter().flatten());
        above.chain(self.within.get(&last).into_iter().flatten()).map(|&at| &self.rules[at])
    }

    /// A rule that shows that `i` is not needed for request `op`: one that
    /// decides every path `i` matches instead of `i`, or one that would
    /// decide them the same way without `i`. `None` if no single rule
    /// shows this. Copies of `i` (`copies`) do not count.
    ///
    /// If `i` decides a path below a path `y` that `i` matches, then `i`
    /// also decides `y`, and every other rule ranks the same at both paths.
    /// So only the paths `i` matches need to be considered.
    fn decider<'a>(&'a self, i: &'a FileRule, op: u8, copies: &[usize]) -> Option<&'a FileRule> {
        let applies = |r: &&FileRule| r.mask & op != 0 && !copies.contains(&r.index);
        let instead = self.broader(i).filter(applies).filter(|j| j.covers(i, false) && j.outranks(i)).max_by_key(|j| j.rank());
        if instead.is_some() {
            return instead;
        }
        let broader: Vec<&FileRule> = self.broader(i).filter(applies).filter(|j| j.allow == i.allow && j.covers(i, true)).collect();
        if broader.is_empty() {
            return None;
        }
        let opposed: Vec<&FileRule> = self.related(i).filter(applies).filter(|k| k.allow != i.allow && i.may_be_below(k)).collect();
        broader.into_iter().filter(|j| opposed.iter().all(|k| never_between(i, j, k))).max_by_key(|j| (j.counts.0, j.rank()))
    }
}

/// Whether rule `k`, which decides the other way, never ranks below `i`
/// and above `j` at a path `i` matches (where `j` matches the path or an
/// ancestor). A rule's rank at a path depends first on the depth of the
/// deepest path it matches there (the path or an ancestor). There, `j`
/// matches at least `j.counts.0` names deep, and `k` at most `k.counts.1`
/// and `i.counts.1` names deep. If no path `i` matches is deeper than the
/// shallowest path `k` matches, then `k` matches only paths that `i`
/// matches exactly, at the same depth.
fn never_between(i: &FileRule, j: &FileRule, k: &FileRule) -> bool {
    let k_deepest = match (k.counts.1, i.counts.1) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let below_j = k_deepest.is_some_and(|d| j.counts.0 > d || (j.counts.0 == d && j.outranks(k)));
    let above_i = i.counts.1.is_some_and(|d| d <= k.counts.0) && k.outranks(i);
    below_j || above_i
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
    }
}
