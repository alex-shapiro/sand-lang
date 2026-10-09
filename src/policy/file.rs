//! Policy files: a policy (rules and limits) as text, for `sand run
//! --policy=FILE` and hosts that let users write policies (see SPEC §14.4).
//!
//! ```text
//! # Each line is a rule; `#` starts a comment.
//! allow read ./data            # paths are relative to the file's directory
//! allow write ./out
//! deny read ./data/secrets     # deny rules win over allow rules
//! allow net api.example.com:443, *.example.org
//! allow env HOME USER
//! default prompt               # ask about anything else (default: deny)
//! limit memory 64M
//! limit time 10s
//! ```

use super::{Capability, Decision, Policy, RulePolicy, is_valid_net_pattern};
use crate::Limits;
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// A parsed policy file: rules, what happens to requests no rule decides,
/// and limits.
#[derive(Clone, Debug)]
pub struct PolicyFile {
    /// The rules (`allow`/`deny` lines). [`PolicyFile::policy`] adds the
    /// `default` decision and the private-network rule.
    pub rules: RulePolicy,
    /// The decision for requests no rule decides (the `default` line), if
    /// given. Hosts choose what applies otherwise ([`PolicyFile::policy_or`]);
    /// [`PolicyFile::policy`] denies.
    pub default: Option<Decision>,
    /// Whether `allow private-net` was given: connections to private and
    /// internal addresses are then not denied (see
    /// [`RulePolicy::deny_private_networks`]).
    pub private_net: bool,
    /// [`Limits::default`] changed by the `limit` lines.
    pub limits: Limits,
}

/// An error in a policy file, at a line (counted from 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for PolicyError {}

impl PolicyFile {
    /// Parses a policy file; relative paths in it are taken from `base`
    /// (usually the file's directory). Reports every error.
    pub fn parse(text: &str, base: &Path) -> Result<PolicyFile, Vec<PolicyError>> {
        let mut file = PolicyFile { rules: RulePolicy::new(), default: None, private_net: false, limits: Limits::default() };
        let mut errors = Vec::new();
        let mut default_line = None;
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let words = match words(raw) {
                Ok(w) => w,
                Err(message) => {
                    errors.push(PolicyError { line, message });
                    continue;
                }
            };
            let Some((verb, args)) = words.split_first() else { continue };
            let r = match verb.as_str() {
                "allow" | "deny" => file.rule(verb == "allow", args, base),
                "default" => match args {
                    [d] => match d.as_str() {
                        "deny" | "prompt" | "allow" if default_line.is_some() => {
                            Err(format!("`default` was already given on line {}", default_line.unwrap_or(0)))
                        }
                        "deny" | "prompt" | "allow" => {
                            default_line = Some(line);
                            file.default = Some(match d.as_str() {
                                "allow" => Decision::Allow,
                                "prompt" => Decision::Prompt,
                                _ => Decision::Deny,
                            });
                            Ok(())
                        }
                        other => Err(format!("unknown default `{other}` (expected `deny`, `prompt` or `allow`)")),
                    },
                    _ => Err("expected `default deny`, `default prompt` or `default allow`".into()),
                },
                "limit" => file.limit(args),
                other => Err(format!("unknown rule `{other}` (rules start with `allow`, `deny`, `default` or `limit`)")),
            };
            if let Err(message) = r {
                errors.push(PolicyError { line, message });
            }
        }
        if errors.is_empty() { Ok(file) } else { Err(errors) }
    }

    /// The policy: the rules with the `default` decision (deny if none was
    /// given), denying private networks unless `allow private-net` was given.
    pub fn policy(&self) -> Policy {
        self.policy_or(Decision::Deny)
    }

    /// Like [`PolicyFile::policy`], with `fallback` for requests no rule
    /// decides when the file has no `default` line (e.g. `Prompt` where a
    /// person is at hand to answer).
    pub fn policy_or(&self, fallback: Decision) -> Policy {
        let mut rules = self.rules.clone().fallback(self.default.unwrap_or(fallback));
        if !self.private_net {
            rules = rules.deny_private_networks();
        }
        Policy::rules(rules)
    }

    fn rule(&mut self, allow: bool, args: &[String], base: &Path) -> Result<(), String> {
        let verb = if allow { "allow" } else { "deny" };
        let Some((what, items)) = args.split_first() else {
            return Err(format!("expected what to {verb}: read, write, net, ... "));
        };
        let what = what.as_str();
        // Which rules exist, and how many arguments they take.
        let arity = match (allow, what) {
            (_, "read" | "write" | "net" | "import" | "listen" | "env") => 1..=usize::MAX,
            (_, "stdin") | (true, "private-net") | (false, "stdout" | "stderr") => 0..=0,
            (_, "custom") => 1..=2,
            (true, "stdout" | "stderr") => return Err(format!("standard {} is allowed unless denied (`deny {what}`)", &what[3..])),
            (false, "private-net") => {
                return Err("`deny private-net` is not a rule: private and internal addresses are reached only where allowed".into());
            }
            _ => {
                return Err(format!(
                    "unknown capability `{what}` (expected read, write, import, net, private-net, listen, env, stdin, stdout, stderr or custom)"
                ));
            }
        };
        if !arity.contains(&items.len()) {
            return Err(match (*arity.start(), *arity.end()) {
                (0, 0) => format!("`{verb} {what}` takes no arguments"),
                (1, 2) => format!("expected `{verb} custom NAME [DETAIL]`"),
                _ => format!("`{verb} {what}` needs at least one argument"),
            });
        }
        if matches!(what, "read" | "write" | "import") && items.iter().any(|p| p.is_empty()) {
            return Err(format!("`{verb} {what}` needs paths, not empty words"));
        }
        if what == "net"
            && let Some(bad) = items.iter().find(|p| !is_valid_net_pattern(p))
        {
            return Err(format!(
                "invalid network pattern `{bad}` (expected a host, host:port, *.domain, *, an IP address or a CIDR range)"
            ));
        }
        // The builder takes the rules by value.
        let mut rules = std::mem::take(&mut self.rules);
        match (allow, what) {
            (true, "private-net") => self.private_net = true,
            (true, "stdin") => rules = rules.allow_stdin(),
            (false, "stdin") => rules = rules.deny_stdin(),
            (false, "stdout") => rules = rules.deny_stdout(),
            (false, "stderr") => rules = rules.deny_stderr(),
            (true, "custom") => rules = rules.allow_custom(&items[0], items.get(1).map(String::as_str)),
            (false, "custom") => rules = rules.deny_custom(&items[0], items.get(1).map(String::as_str)),
            _ => {
                for item in items {
                    rules = match (allow, what) {
                        (true, "read") => rules.allow_read(base.join(item)),
                        (false, "read") => rules.deny_read(base.join(item)),
                        (true, "write") => rules.allow_write(base.join(item)),
                        (false, "write") => rules.deny_write(base.join(item)),
                        (true, "import") => rules.allow_import(base.join(item)),
                        (false, "import") => rules.deny_import(base.join(item)),
                        (true, "net") => rules.allow_net(item),
                        (false, "net") => rules.deny_net(item),
                        (true, "listen") => rules.allow_listen(item),
                        (false, "listen") => rules.deny_listen(item),
                        (true, _) => rules.allow_env(item),
                        (false, _) => rules.deny_env(item),
                    };
                }
            }
        }
        self.rules = rules;
        Ok(())
    }

    fn limit(&mut self, args: &[String]) -> Result<(), String> {
        let [name, value] = args else {
            return Err("expected `limit NAME VALUE`, e.g. `limit memory 64M`".into());
        };
        let off = matches!(value.as_str(), "none" | "unlimited");
        let l = &mut self.limits;
        let count = |v: &str| v.replace('_', "").parse::<u64>().map_err(|_| format!("invalid number `{v}`"));
        let small = |v: &str| count(v).and_then(|n| usize::try_from(n).map_err(|_| format!("`{v}` is too large")));
        let optional = |v: &str, parse: &dyn Fn(&str) -> Result<u64, String>| if off { Ok(None) } else { parse(v).map(Some) };
        match name.as_str() {
            "memory" => l.max_memory = if off { None } else { Some(parse_size(value)?) },
            "time" => l.max_wall_time = if off { None } else { Some(parse_duration(value)?) },
            "cpu" => l.max_cpu_time = if off { None } else { Some(parse_duration(value)?) },
            "instructions" => l.max_instructions = optional(value, &count)?,
            "compile-fuel" => l.max_compile_fuel = optional(value, &count)?,
            "output" => l.max_output_bytes = if off { None } else { Some(parse_size_u64(value)?) },
            "tasks" if !off => l.max_tasks = small(value)?,
            "call-depth" if !off => l.max_call_depth = small(value)?,
            "open-resources" if !off => l.max_open_resources = small(value)?,
            "source" if !off => l.max_source_bytes = parse_size(value)?,
            "http-body" if !off => l.max_http_body = parse_size(value)?,
            "tasks" | "call-depth" | "open-resources" | "source" | "http-body" => {
                return Err(format!("the `{name}` limit cannot be removed"));
            }
            other => {
                return Err(format!(
                    "unknown limit `{other}` (expected memory, time, cpu, instructions, output, compile-fuel, tasks, call-depth, open-resources, source or http-body)"
                ));
            }
        }
        Ok(())
    }
}

/// The rule that decides `cap` from now on: allows it (or, unless
/// `allow`, denies it). What a prompt's "always" (or "never") answer says
/// as a policy file line, e.g. `allow read /data/a.txt`. `None` for
/// requests no rule names (`NetAddr`, which is never prompted; standard
/// output and error, which are allowed unless denied).
pub fn rule_for(cap: &Capability, allow: bool) -> Option<String> {
    let verb = if allow { "allow" } else { "deny" };
    let path = |p: &Path| p.to_str().map(str::to_string);
    let (what, args): (&str, Vec<String>) = match cap {
        Capability::FsRead(p) => ("read", vec![path(p)?]),
        Capability::FsWrite(p) => ("write", vec![path(p)?]),
        Capability::Import(p) => ("import", vec![path(p)?]),
        Capability::Net { host, port } => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let pattern = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
            if !is_valid_net_pattern(&pattern) {
                return None;
            }
            ("net", vec![pattern])
        }
        Capability::NetListen(addr) => ("listen", vec![addr.clone()]),
        Capability::Env(name) => ("env", vec![name.clone()]),
        Capability::Stdin => ("stdin", vec![]),
        Capability::Stdout | Capability::Stderr if allow => return None,
        Capability::Stdout => ("stdout", vec![]),
        Capability::Stderr => ("stderr", vec![]),
        Capability::Custom { name, detail } => ("custom", vec![name.clone(), detail.clone()]),
        Capability::NetAddr { .. } => return None,
    };
    let mut line = format!("{verb} {what}");
    for arg in &args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    Some(line)
}

/// `word`, quoted if a policy file would not read it back as one word.
fn quote(word: &str) -> String {
    let plain = !word.is_empty() && !word.starts_with('#') && !word.chars().any(|c| c.is_whitespace() || c == ',' || c == '"' || c == '\\');
    if plain { word.to_string() } else { format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\"")) }
}

/// The words of a line: separated by spaces or commas, `"double quoted"`
/// (with `\"` and `\\` escapes) to contain them, up to a `#` that starts a
/// word (a comment). A quote within a word is an error, so that no word is
/// silently split or cut short.
fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    let separator = |c: char| c.is_whitespace() || c == ',';
    while let Some(&c) = chars.peek() {
        match c {
            '#' => break,
            c if separator(c) => {
                chars.next();
            }
            '"' => {
                chars.next();
                let mut word = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\')) => word.push(e),
                            _ => return Err("invalid escape in a quoted word (use \\\" or \\\\)".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated quoted word".into()),
                    }
                }
                if chars.peek().is_some_and(|&c| !separator(c)) {
                    return Err(format!("unexpected text after the quoted word \"{word}\" (separate words with spaces)"));
                }
                out.push(word);
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if separator(c) {
                        break;
                    }
                    if c == '"' {
                        return Err(format!("unexpected `\"` in `{word}` (quote whole words)"));
                    }
                    word.push(c);
                    chars.next();
                }
                out.push(word);
            }
        }
    }
    Ok(out)
}

/// Parses a size: a number of bytes with an optional `K`, `M` or `G` suffix
/// (powers of 1024), e.g. `512`, `64K`, `256M`, `1G`. Sizes too large for
/// `usize` (on 32-bit targets) saturate, like those too large for `u64`
/// with a suffix.
pub fn parse_size(s: &str) -> Result<usize, String> {
    parse_size_u64(s).map(crate::vm::value::u64_to_usize_saturating)
}

/// [`parse_size`] for byte counts kept as `u64` on every target.
fn parse_size_u64(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().map(|n| n.saturating_mul(mult)).map_err(|_| format!("invalid size `{s}` (use e.g. 4096, 64K, 256M, 1G)"))
}

/// Parses a duration: a number with a unit, `ns`, `us`, `ms`, `s`, `m` or
/// `h`, e.g. `500ms`, `10s`, `1.5m`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let units: [(&str, f64); 6] = [("ms", 1e-3), ("us", 1e-6), ("ns", 1e-9), ("s", 1.0), ("m", 60.0), ("h", 3600.0)];
    for (u, mult) in units {
        if let Some(n) = s.strip_suffix(u)
            && let Ok(v) = n.parse::<f64>()
            && v >= 0.0
            && v.is_finite()
        {
            return Duration::try_from_secs_f64(v * mult).map_err(|_| format!("duration `{s}` is too long"));
        }
    }
    Err(format!("invalid duration `{s}` (use e.g. 500ms, 10s, 2m)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Capability;
    use std::path::PathBuf;

    fn parse(text: &str) -> PolicyFile {
        PolicyFile::parse(text, Path::new("/base")).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn errors(text: &str) -> Vec<String> {
        PolicyFile::parse(text, Path::new("/base")).err().unwrap_or_default().iter().map(|e| e.to_string()).collect()
    }

    fn decide(p: &PolicyFile, cap: Capability) -> Decision {
        p.policy().check_in(&cap, crate::policy::PathSpace::Memory)
    }

    #[test]
    fn rules_limits_and_comments() {
        let p = parse(
            "# A policy\n\
             allow read data, \"/with space\"   # trailing comment\n\
             allow write /out\n\
             deny read data/secrets\n\
             allow env HOME,USER\n\
             allow net api.example.com:443 *.example.org\n\
             default prompt\n\
             limit memory 64M\n\
             limit time 1.5s\n\
             limit instructions 1_000_000\n\
             limit cpu none\n",
        );
        let read = |s: &str| Capability::FsRead(PathBuf::from(s));
        assert_eq!(decide(&p, read("/base/data/x")), Decision::Allow);
        assert_eq!(decide(&p, read("/with space/x")), Decision::Allow);
        assert_eq!(decide(&p, read("/base/data/secrets/x")), Decision::Deny);
        assert_eq!(decide(&p, read("/out/x")), Decision::Allow, "write implies read");
        assert_eq!(decide(&p, Capability::FsWrite(PathBuf::from("/base/data/x"))), Decision::Prompt, "the default decides the rest");
        assert_eq!(decide(&p, Capability::Env("USER".into())), Decision::Allow);
        assert_eq!(decide(&p, Capability::Net { host: "a.example.org".into(), port: 80 }), Decision::Allow);
        assert_eq!(decide(&p, Capability::Net { host: "localhost".into(), port: 80 }), Decision::Prompt);
        assert_eq!(decide(&p, Capability::Stdout), Decision::Allow);
        assert_eq!(p.limits.max_memory, Some(64 << 20));
        assert_eq!(p.limits.max_wall_time, Some(Duration::from_millis(1500)));
        assert_eq!(p.limits.max_instructions, Some(1_000_000));
        assert_eq!(p.limits.max_cpu_time, None);
        assert!(!p.private_net);
    }

    #[test]
    fn hashes_start_comments_only_at_the_start_of_a_word() {
        let p = parse("deny read data/x#1   # the comment\nallow read data");
        let read = |s: &str| decide(&p, Capability::FsRead(PathBuf::from(s)));
        assert_eq!(read("/base/data/x#1"), Decision::Deny);
        assert_eq!(read("/base/data/x"), Decision::Allow);
    }

    #[test]
    fn every_capability_can_be_denied() {
        let p = parse("default allow\ndeny env SECRET\ndeny stdin\ndeny listen *:22\ndeny import /base/vendor\ndeny custom db write");
        assert_eq!(decide(&p, Capability::Env("SECRET".into())), Decision::Deny);
        assert_eq!(decide(&p, Capability::Env("OTHER".into())), Decision::Allow);
        assert_eq!(decide(&p, Capability::Env("*".into())), Decision::Deny, "all variables would include the denied one");
        assert_eq!(decide(&p, Capability::Stdin), Decision::Deny);
        assert_eq!(decide(&p, Capability::NetListen("0.0.0.0:22".into())), Decision::Deny);
        assert_eq!(decide(&p, Capability::NetListen("0.0.0.0:80".into())), Decision::Allow);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/vendor/x.sand"))), Decision::Deny);
        assert_eq!(decide(&p, Capability::Custom { name: "db".into(), detail: "write".into() }), Decision::Deny);
        assert_eq!(decide(&p, Capability::Custom { name: "db".into(), detail: "read".into() }), Decision::Allow);
    }

    #[test]
    fn prompt_answers_become_rules_that_decide_them() {
        let caps = [
            Capability::FsRead(PathBuf::from("/data/a file, \"quoted\".txt")),
            Capability::FsWrite(PathBuf::from("/out/#hash")),
            Capability::Import(PathBuf::from("/lib/util.sand")),
            Capability::Net { host: "api.example.com".into(), port: 443 },
            Capability::Net { host: "[::1]".into(), port: 8080 },
            Capability::Net { host: "10.0.0.5".into(), port: 80 },
            Capability::NetListen("127.0.0.1:8080".into()),
            Capability::Env("HOME".into()),
            Capability::Env("*".into()),
            Capability::Stdin,
            Capability::Custom { name: "db".into(), detail: "drop table".into() },
        ];
        for cap in &caps {
            for allow in [true, false] {
                let rule = rule_for(cap, allow).unwrap_or_else(|| panic!("no rule for {cap:?}"));
                // With the rule, a policy that would otherwise ask decides.
                let p = parse(&format!("default prompt\n{rule}"));
                let want = if allow { Decision::Allow } else { Decision::Deny };
                assert_eq!(decide(&p, cap.clone()), want, "{rule}");
            }
        }
        assert_eq!(rule_for(&caps[0], true).unwrap(), "allow read \"/data/a file, \\\"quoted\\\".txt\"");
        assert_eq!(rule_for(&Capability::Stdout, true), None);
        assert_eq!(rule_for(&Capability::Stdout, false).as_deref(), Some("deny stdout"));
    }

    #[test]
    fn private_networks_are_denied_unless_allowed() {
        let net = Capability::Net { host: "10.1.2.3".into(), port: 80 };
        let p = parse("allow net *");
        assert_eq!(decide(&p, net.clone()), Decision::Deny);
        let p = parse("allow net *\nallow private-net");
        assert_eq!(decide(&p, net), Decision::Allow);
    }

    #[test]
    fn every_error_is_reported_with_its_line() {
        assert_eq!(
            errors(
                "alow read /x\nallow\nallow net not a pattern!\nlimit memory lots\nlimit\ndefault maybe\nallow stdout\ndeny custom\nallow read \"open"
            ),
            [
                "line 1: unknown rule `alow` (rules start with `allow`, `deny`, `default` or `limit`)",
                "line 2: expected what to allow: read, write, net, ... ",
                "line 3: invalid network pattern `pattern!` (expected a host, host:port, *.domain, *, an IP address or a CIDR range)",
                "line 4: invalid size `lots` (use e.g. 4096, 64K, 256M, 1G)",
                "line 5: expected `limit NAME VALUE`, e.g. `limit memory 64M`",
                "line 6: unknown default `maybe` (expected `deny`, `prompt` or `allow`)",
                "line 7: standard out is allowed unless denied (`deny stdout`)",
                "line 8: expected `deny custom NAME [DETAIL]`",
                "line 9: unterminated quoted word",
            ]
        );
        assert_eq!(errors("default deny\ndefault allow"), ["line 2: `default` was already given on line 1"]);
        assert_eq!(errors("allow stdin now"), ["line 1: `allow stdin` takes no arguments"]);
        assert_eq!(errors("allow read \"a\"b"), ["line 1: unexpected text after the quoted word \"a\" (separate words with spaces)"]);
        assert_eq!(errors("allow read a\"b\""), ["line 1: unexpected `\"` in `a` (quote whole words)"]);
        assert_eq!(errors("allow read \"\""), ["line 1: `allow read` needs paths, not empty words"]);
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("2k"), Ok(2048));
        assert_eq!(parse_size("1G"), Ok(1 << 30));
        assert!(parse_size("-1").is_err());
        // Byte counts past 4 GiB keep their value where they are `u64`
        // (the output limit) and saturate where they are `usize`.
        assert_eq!(parse_size_u64("8G"), Ok(8 << 30));
        assert_eq!(parse_size_u64("99999999999G"), Ok(u64::MAX));
        assert_eq!(parse_size("8589934592").map(|n| n as u64), Ok((8u64 << 30).min(usize::MAX as u64)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("1e300h").is_err());
    }
}
