//! The policy file linter.

use super::file::parse;
use super::net::HostKind;
use super::rules::{Access, RuleKind};
use super::{Capability, PathSpace, PolicyError};
use crate::lint::LintWarning;
use crate::vm::value::FxHashMap;
use std::path::Path;

/// Lints a policy file (parsed as by [`super::PolicyFile::parse`]):
///
/// - `redundant-rule`: a rule repeats another, or a broader rule already
///   decides everything it covers the same way;
/// - `overridden-rule`: a rule that takes precedence (more specific, or a
///   deny as specific) decides everything it covers the other way;
/// - `bypassable-http-deny`: a `deny http` rule for a host that `net` rules
///   let scripts connect to (and send it anything);
/// - `unused-set`: a `let` set no rule uses.
///
/// Rules about paths are compared as written, without resolving symlinks.
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
        let verb = if rule.allow { "allows" } else { "denies" };
        match &rule.kind {
            // A rule naming one path decides it and what is below it the same
            // way, unless more specific rules do: whether it is needed shows
            // at the path itself.
            RuleKind::Files(access, pattern) if pattern.rest.is_empty() => {
                let p = crate::files::memory_path(&pattern.prefix);
                let ops = match (access, rule.allow) {
                    (Access::Read, true) => vec![Capability::FsRead(p)],
                    (Access::Write, false) => vec![Capability::FsWrite(p)],
                    (Access::Import, _) => vec![Capability::Import(p)],
                    (_, false) => vec![Capability::FsRead(p.clone()), Capability::FsWrite(p.clone()), Capability::Import(p)],
                    (_, true) => vec![Capability::FsRead(p.clone()), Capability::FsWrite(p)],
                };
                let (mut same, mut other) = (None, None);
                let mut needed = false;
                for cap in &ops {
                    match policy.verdict(cap, PathSpace::Memory, &[]) {
                        Some((allow, j)) if j != i => *(if allow == rule.allow { &mut same } else { &mut other }) = Some(j),
                        _ => match policy.verdict(cap, PathSpace::Memory, copies) {
                            Some((allow, j)) if allow == rule.allow => same = Some(j),
                            _ => needed = true,
                        },
                    }
                }
                match (needed, same, other) {
                    (false, Some(j), None) => warn(i, "redundant-rule", format!("{} already {verb} this", describe(j))),
                    (false, None, Some(j)) => {
                        warn(i, "overridden-rule", format!("{} takes precedence everywhere this rule applies", describe(j)))
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
                    warn(i, "bypassable-http-deny", format!("{} lets scripts connect to {host}:{port} and send any request", describe(j)));
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
