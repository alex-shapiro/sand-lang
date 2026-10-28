//! Policy files: a policy (rules and limits) as text, for `sand run
//! --policy=FILE` and hosts that let users write policies (SPEC §14.7).
//!
//! ```text
//! let owners = alice bob            # a set, used as {owners}
//! allow read /usr ~/.gitconfig
//! allow ./                          # read and write the file's directory
//! deny ./**/.env
//! allow http GET api.github.com
//! allow http POST api.github.com/repos/{owners}/**
//! default prompt
//! limit memory 64M
//! ```

use super::net::{ListenPattern, NetPattern, UrlPattern, format_host_port, format_url_origin, parse_ip, url_segments};
use super::path::{Seg, Tok};
use super::rules::{Access, NamePattern, Origin, PathPattern, Rule, RuleKind};
use super::{Capability, Decision, Policy, RulePolicy};
use crate::Limits;
use crate::vm::value::FxHashMap;
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A parsed security policy file.
#[derive(Clone, Debug)]
pub struct PolicyFile {
    /// Policy rules.
    pub rules: RulePolicy,
    /// Fallback decision for unmatched requests.
    pub default: Option<Decision>,
    /// True if private network connections are allowed.
    pub private_net: bool,
    /// Execution limits configured by policy limit directives.
    pub limits: Limits,
}

/// An error in a policy file, at a line and column (counted from 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for PolicyError {}

/// Expansions of a word with set references above this many are errors.
const MAX_EXPANSION: usize = 100_000;

impl PolicyFile {
    /// Parses a policy file. Relative paths (`./data`) are taken from
    /// `base` (usually the file's directory), `~` is `home` (an error if
    /// `None`). Reports every error.
    pub fn parse(text: &str, base: &Path, home: Option<&Path>) -> Result<PolicyFile, Vec<PolicyError>> {
        parse(text, base, home).map(|p| p.file)
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
        Policy::rules(self.rule_policy(fallback))
    }

    pub(crate) fn rule_policy(&self, fallback: Decision) -> RulePolicy {
        let rules = self.rules.clone().fallback(self.default.unwrap_or(fallback));
        if self.private_net { rules } else { rules.deny_private_networks() }
    }
}

/// A parsed policy file and its set metadata.
pub(crate) struct Parsed {
    pub file: PolicyFile,
    /// Declared sets: `(name, line, column, is_used)`.
    pub sets: Vec<(String, usize, usize, bool)>,
    pub default_line: Option<usize>,
}

/// A line of a policy file: words, and the comment (from its `#`).
pub(crate) struct Line {
    pub words: Vec<Word>,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Word {
    pub parts: Vec<Part>,
    /// Column (from 1) of the word's first character.
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    /// Unquoted text, where `*` is a wildcard.
    Raw(String),
    /// Quoted text, taken literally.
    Quoted(String),
    /// `{name}`: each value of the set `name`.
    Ref(String),
}

impl Word {
    /// The word's text if it uses no sets (a keyword, a name: quoting
    /// does not change what a word says, only whether its `*`s are
    /// wildcards).
    pub(crate) fn plain(&self) -> Option<Cow<'_, str>> {
        match self.parts.as_slice() {
            [Part::Raw(s) | Part::Quoted(s)] => Some(Cow::Borrowed(s)),
            parts if parts.iter().any(|p| matches!(p, Part::Ref(_))) => None,
            _ => Some(Cow::Owned(self.text())),
        }
    }

    /// The word's text, quotes and wildcards aside.
    pub(crate) fn text(&self) -> String {
        self.parts
            .iter()
            .map(|p| match p {
                Part::Raw(s) | Part::Quoted(s) => s.clone(),
                Part::Ref(n) => format!("{{{n}}}"),
            })
            .collect()
    }

    /// The word's characters, marking unquoted `*`s as wildcards.
    pub(crate) fn tokens(&self) -> Vec<Tok> {
        let mut out = Vec::new();
        for p in &self.parts {
            match p {
                Part::Raw(s) => out.extend(s.chars().map(|c| (c, c == '*'))),
                Part::Quoted(s) => out.extend(s.chars().map(|c| (c, false))),
                Part::Ref(_) => {}
            }
        }
        out
    }

    /// The word as a policy file writes it: quoting only what needs it.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Raw(s) => out.push_str(s),
                Part::Quoted(s) => out.push_str(&quote_part(s, out.is_empty())),
                Part::Ref(n) => {
                    out.push('{');
                    out.push_str(n);
                    out.push('}');
                }
            }
        }
        out
    }

    /// The word with its set references replaced by each of their values.
    fn expand(&self, sets: &FxHashMap<String, (Vec<Word>, usize)>, used: &mut FxHashMap<String, bool>) -> Result<Vec<Word>, String> {
        let mut out = vec![Word { parts: vec![], column: self.column }];
        for p in &self.parts {
            match p {
                Part::Ref(name) => {
                    let Some((values, _)) = sets.get(name) else { return Err(format!("no set `{name}` (define it: `let {name} = ...`)")) };
                    used.insert(name.clone(), true);
                    if out.len().saturating_mul(values.len()) > MAX_EXPANSION {
                        return Err(format!("`{}` stands for more than {MAX_EXPANSION} patterns", self.render()));
                    }
                    out = out
                        .iter()
                        .flat_map(|w| values.iter().map(move |v| Word { parts: [&w.parts[..], &v.parts[..]].concat(), column: w.column }))
                        .collect();
                }
                p => out.iter_mut().for_each(|w| w.parts.push(p.clone())),
            }
        }
        Ok(out)
    }
}

/// Text as a quoted part of a word, if it needs quoting to be read back
/// literally.
fn quote_part(s: &str, first: bool) -> String {
    let special = |c: char| c.is_whitespace() || matches!(c, ',' | '"' | '*' | '{' | '}');
    if !s.is_empty() && !s.contains(special) && !(first && s.starts_with('#')) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Splits a policy file into lines of words. Words are separated by spaces
/// or commas; `"..."` quotes text (with `\"` and `\\` escapes), and a word
/// may join quoted and unquoted text; `#` at the start of a word starts a
/// comment.
pub(crate) fn lex(text: &str) -> (Vec<Line>, Vec<PolicyError>) {
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        match lex_line(raw) {
            Ok(line) => lines.push(line),
            Err((column, message)) => {
                errors.push(PolicyError { line: i + 1, column, message });
                lines.push(Line { words: vec![], comment: None });
            }
        }
    }
    (lines, errors)
}

fn lex_line(line: &str) -> Result<Line, (usize, String)> {
    let ascii = line.is_ascii();
    let column = |i: usize| if ascii { i + 1 } else { line[..i].chars().count() + 1 };
    let separator = |c: char| c.is_whitespace() || c == ',';
    let mut words = Vec::new();
    let mut i = 0;
    while let Some(c) = line[i..].chars().next() {
        if separator(c) {
            i += c.len_utf8();
            continue;
        }
        if c == '#' {
            return Ok(Line { words, comment: Some(line[i..].trim_end().to_string()) });
        }
        let start = i;
        let mut parts: Vec<Part> = Vec::new();
        while let Some(c) = line[i..].chars().next().filter(|&c| !separator(c)) {
            match c {
                '"' => {
                    let mut text = String::new();
                    let mut chars = line[i + 1..].char_indices();
                    loop {
                        match chars.next() {
                            Some((j, '"')) => {
                                i += j + 2;
                                break;
                            }
                            Some((j, '\\')) => match chars.next() {
                                Some((_, e @ ('"' | '\\'))) => text.push(e),
                                _ => return Err((column(i + 1 + j), "invalid escape in quotes (use \\\" or \\\\)".into())),
                            },
                            Some((_, c)) => text.push(c),
                            None => return Err((column(i), "unterminated quotes".into())),
                        }
                    }
                    parts.push(Part::Quoted(text));
                }
                '{' => {
                    let end = line[i..].find('}').map(|n| i + n);
                    let name = &line[i + 1..end.unwrap_or(i + 1)];
                    let Some(end) = end.filter(|_| is_name(name)) else {
                        return Err((column(i), "`{` starts a set reference such as `{owners}` (quote it for a literal `{`)".into()));
                    };
                    parts.push(Part::Ref(name.to_string()));
                    i = end + 1;
                }
                '}' => return Err((column(i), "unmatched `}` (quote it for a literal `}`)".into())),
                _ => {
                    let len = line[i..].find(|c: char| separator(c) || matches!(c, '"' | '{' | '}')).unwrap_or(line.len() - i);
                    parts.push(Part::Raw(line[i..i + len].to_string()));
                    i += len;
                }
            }
        }
        words.push(Word { parts, column: column(start) });
    }
    Ok(Line { words, comment: None })
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

const KNOWN_METHODS: [&str; 9] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT"];

/// Whether a word of an `http` rule is a method: a standard method in any
/// case, or any upper-case token (`PROPFIND`).
pub(crate) fn is_method(word: &str) -> bool {
    KNOWN_METHODS.iter().any(|m| m.eq_ignore_ascii_case(word))
        || (word.bytes().any(|b| b.is_ascii_uppercase())
            && word.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_'))
}

const CAPABILITIES: &str = "read, write, import, net, http, private-net, listen, env, stdin, stdout, stderr, timezone or custom";

/// The context rules are parsed in.
struct Context<'a> {
    base: &'a Path,
    home: Option<&'a Path>,
    sets: FxHashMap<String, (Vec<Word>, usize)>,
    used: FxHashMap<String, bool>,
}

pub(crate) fn parse(text: &str, base: &Path, home: Option<&Path>) -> Result<Parsed, Vec<PolicyError>> {
    let (lines, mut errors) = lex(text);
    let mut cx = Context { base, home, sets: FxHashMap::default(), used: FxHashMap::default() };
    let mut sets = Vec::new();
    // Sets first: rules may use them wherever they are defined.
    for (i, line) in lines.iter().enumerate() {
        let Some(first) = line.words.first() else { continue };
        if first.plain().as_deref() != Some("let") {
            continue;
        }
        let error = |column, message: String| PolicyError { line: i + 1, column, message };
        let (name, values) = match &line.words[1..] {
            [name, eq, values @ ..]
                if name.plain().is_some_and(|n| is_name(&n)) && eq.plain().as_deref() == Some("=") && !values.is_empty() =>
            {
                (name, values)
            }
            _ => {
                errors.push(error(first.column, "expected `let NAME = VALUE...`, e.g. `let owners = alice bob`".into()));
                continue;
            }
        };
        let name_text = name.text();
        if let Some(bad) = values.iter().find(|v| v.parts.iter().any(|p| matches!(p, Part::Ref(_)))) {
            errors.push(error(bad.column, "the values of a set cannot use sets".into()));
        } else if let Some((_, line)) = cx.sets.get(&name_text) {
            errors.push(error(name.column, format!("set `{name_text}` was already defined on line {line}")));
        } else {
            sets.push((name_text.clone(), i + 1, name.column));
            cx.sets.insert(name_text, (values.to_vec(), i + 1));
        }
    }
    let mut file = PolicyFile { rules: RulePolicy::new(), default: None, private_net: false, limits: Limits::default() };
    let mut rules = Vec::new();
    let mut default_line = None;
    for (i, line) in lines.iter().enumerate() {
        let Some((first, args)) = line.words.split_first() else { continue };
        let error = |column, message: String| PolicyError { line: i + 1, column, message };
        let result = match first.plain().as_deref() {
            Some("let") => Ok(()),
            Some(verb @ ("allow" | "deny")) => cx.rule(verb == "allow", first, args, i + 1, &mut rules, &mut file.private_net),
            Some("default") => match args {
                [d] => match d.plain().as_deref() {
                    Some("deny" | "prompt" | "allow") if default_line.is_some() => {
                        Err((d.column, format!("`default` was already given on line {}", default_line.unwrap_or(0))))
                    }
                    Some(d @ ("deny" | "prompt" | "allow")) => {
                        default_line = Some(i + 1);
                        file.default = Some(match d {
                            "allow" => Decision::Allow,
                            "prompt" => Decision::Prompt,
                            _ => Decision::Deny,
                        });
                        Ok(())
                    }
                    _ => Err((d.column, format!("unknown default `{}` (expected `deny`, `prompt` or `allow`)", d.text()))),
                },
                _ => Err((first.column, "expected `default deny`, `default prompt` or `default allow`".into())),
            },
            Some("limit") => limit(&mut file.limits, args).map_err(|m| (args.first().unwrap_or(first).column, m)),
            _ => Err((
                first.column,
                format!("unknown rule `{}` (rules start with `allow`, `deny`, `default`, `limit` or `let`)", first.text()),
            )),
        };
        if let Err((column, message)) = result {
            errors.push(error(column, message));
        }
    }
    if !errors.is_empty() {
        errors.sort_by_key(|e| (e.line, e.column));
        return Err(errors);
    }
    file.rules = RulePolicy::new().extend(rules);
    let sets = sets.into_iter().map(|(name, line, column)| (name.clone(), line, column, cx.used.contains_key(&name))).collect();
    Ok(Parsed { file, sets, default_line })
}

impl Context<'_> {
    /// Parses an `allow` or `deny` rule into `rules`.
    fn rule(
        &mut self,
        allow: bool,
        verb: &Word,
        args: &[Word],
        line: usize,
        rules: &mut Vec<Rule>,
        private_net: &mut bool,
    ) -> Result<(), (usize, String)> {
        let verb_text = if allow { "allow" } else { "deny" };
        let Some((what, items)) = args.split_first() else {
            return Err((verb.column, format!("expected what to {verb_text}: a path, or {CAPABILITIES}")));
        };
        let keyword = what.plain().filter(|w| w.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
        let (kind, items) = match keyword.as_deref() {
            Some(k @ ("read" | "write" | "import" | "net" | "http" | "listen" | "env" | "custom")) => (k, items),
            Some(k @ ("stdin" | "stdout" | "stderr" | "timezone" | "private-net")) => {
                if let Some(extra) = items.first() {
                    return Err((extra.column, format!("`{verb_text} {k}` takes no arguments")));
                }
                let kind = match (allow, k) {
                    (true, "private-net") => {
                        *private_net = true;
                        return Ok(());
                    }
                    (false, "private-net") => {
                        return Err((
                            what.column,
                            "`deny private-net` is not a rule: private and internal addresses are reached only where allowed".into(),
                        ));
                    }
                    (true, "stdout" | "stderr") => {
                        return Err((what.column, format!("standard {} is allowed unless denied (`deny {k}`)", &k[3..])));
                    }
                    (_, "stdin") => RuleKind::Stdin,
                    (_, "timezone") => RuleKind::TimeZone,
                    (_, "stdout") => RuleKind::Stdout,
                    _ => RuleKind::Stderr,
                };
                rules.push(Rule { allow, kind, origin: Some(Origin { line, column: what.column, text: k.to_string() }) });
                return Ok(());
            }
            Some(k) => return Err((what.column, format!("unknown capability `{k}` (expected a path, or {CAPABILITIES})"))),
            None => ("files", args),
        };
        if items.is_empty() {
            let hint = match kind {
                "custom" => format!("expected `{verb_text} custom NAME [DETAIL]`"),
                "http" => format!("expected `{verb_text} http [METHOD...] URL...`, e.g. `{verb_text} http GET api.example.com/v1`"),
                _ => format!("`{verb_text} {kind}` needs at least one argument"),
            };
            return Err((what.column, hint));
        }
        if kind == "custom" {
            let [name, detail @ ..] = items else { unreachable!() };
            if detail.len() > 1 {
                return Err((detail[1].column, format!("expected `{verb_text} custom NAME [DETAIL]`")));
            }
            let kind = RuleKind::Custom(name.text(), detail.first().map(Word::text));
            rules.push(Rule { allow, kind, origin: Some(Origin { line, column: name.column, text: name.render() }) });
            return Ok(());
        }
        let mut methods: Option<Vec<String>> = None;
        let mut urls = 0;
        for item in items {
            let expanded;
            let words = if item.parts.iter().any(|p| matches!(p, Part::Ref(_))) {
                expanded = item.expand(&self.sets, &mut self.used).map_err(|m| (item.column, m))?;
                &expanded[..]
            } else {
                std::slice::from_ref(item)
            };
            for word in words {
                let origin = Some(Origin { line, column: item.column, text: word.render() });
                let kind = match kind {
                    "files" | "read" | "write" | "import" => {
                        let access = match kind {
                            "read" => Access::Read,
                            "write" => Access::Write,
                            "import" => Access::Import,
                            _ => Access::Any,
                        };
                        RuleKind::Files(access, self.path(&word.tokens()).map_err(|m| (item.column, m))?)
                    }
                    "net" => RuleKind::Net(NetPattern::parse(&word.text()).map_err(|m| (item.column, m))?),
                    "http" if is_method(&word.text()) => {
                        if urls > 0 {
                            return Err((item.column, "methods come before the URL patterns".into()));
                        }
                        methods.get_or_insert_with(Vec::new).push(word.text().to_ascii_uppercase());
                        continue;
                    }
                    "http" => {
                        urls += 1;
                        RuleKind::Http(methods.clone(), UrlPattern::parse(&word.tokens()).map_err(|m| (item.column, m))?)
                    }
                    "listen" => RuleKind::Listen(ListenPattern::parse(&word.text()).ok_or_else(|| {
                        (item.column, format!("invalid listen pattern `{}` (expected ip:port, ip, localhost, *:port or *)", word.text()))
                    })?),
                    _ => RuleKind::Env(NamePattern::parse(&word.text())),
                };
                rules.push(Rule { allow, kind, origin });
            }
        }
        if kind == "http" && urls == 0 {
            let last = items.last().unwrap_or(what);
            return Err((last.column, format!("`{verb_text} http` needs a URL pattern after its methods, e.g. `api.example.com/v1`")));
        }
        Ok(())
    }

    /// Parses a path pattern: `/...`, `./...`, `../...` (from the base) or
    /// `~/...` (from home), names separated by `/` (also `\` on Windows).
    fn path(&self, text: &[Tok]) -> Result<PathPattern, String> {
        let flat: String = text.iter().map(|t| t.0).collect();
        let sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
        let unanchored = || {
            format!("path `{flat}` is not anchored: write `./{flat}` for a path relative to the policy file, `~/...` or an absolute path")
        };
        let starts = |s: &str| flat.starts_with(s) && flat[s.len()..].chars().next().is_none_or(sep);
        let (mut prefix, rest): (PathBuf, &[Tok]) = if starts("~") {
            let home = self.home.ok_or("`~` cannot be used here: there is no home directory")?;
            (home.to_path_buf(), &text[1..])
        } else if starts(".") || starts("..") {
            (self.base.to_path_buf(), text)
        } else if flat.starts_with('/') || (cfg!(windows) && Path::new(&flat).is_absolute()) {
            let root = Path::new(&flat).components().take_while(|c| !matches!(c, std::path::Component::Normal(_))).collect::<PathBuf>();
            let skip = root.as_os_str().to_string_lossy().chars().count().min(text.len());
            (root, &text[skip..])
        } else {
            return Err(unanchored());
        };
        let mut segs: Vec<Seg> = Vec::new();
        for seg in rest.split(|t| sep(t.0) && !t.1).filter(|s| !s.is_empty()) {
            let parsed = Seg::parse(seg)?;
            match (&parsed, segs.is_empty()) {
                (Seg::Lit(s), _) if &**s == "." => {}
                (Seg::Lit(s), true) if &**s == ".." => {
                    prefix.pop();
                }
                (Seg::Lit(s), false) if &**s == ".." => return Err("`..` cannot follow a wildcard".into()),
                (Seg::Lit(s), true) => prefix.push(&**s),
                _ => segs.push(parsed),
            }
        }
        while segs.last() == Some(&Seg::GlobStar) {
            segs.pop();
        }
        Ok(PathPattern { prefix, rest: segs })
    }
}

fn limit(l: &mut Limits, args: &[Word]) -> Result<(), String> {
    let (Some(name), Some(value), None) = (args.first().and_then(Word::plain), args.get(1).map(Word::text), args.get(2)) else {
        return Err("expected `limit NAME VALUE`, e.g. `limit memory 64M`".into());
    };
    let value = value.as_str();
    let off = matches!(value, "none" | "unlimited");
    let count = |v: &str| v.replace('_', "").parse::<u64>().map_err(|_| format!("invalid number `{v}`"));
    let small = |v: &str| count(v).and_then(|n| usize::try_from(n).map_err(|_| format!("`{v}` is too large")));
    let optional = |v: &str, parse: &dyn Fn(&str) -> Result<u64, String>| if off { Ok(None) } else { parse(v).map(Some) };
    match &*name {
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

/// Generates a policy rule string for a capability prompt response.
pub fn rule_for(cap: &Capability, allow: bool) -> Option<String> {
    let verb = if allow { "allow" } else { "deny" };
    let path = |p: &Path| p.to_str().map(|s| quote_part(s, true));
    let rule = match cap {
        Capability::FsRead(p) => format!("read {}", path(p)?),
        Capability::FsWrite(p) => format!("write {}", path(p)?),
        Capability::Import(p) => format!("import {}", path(p)?),
        Capability::Net { host, port } => {
            let pattern = format_host_port(host, *port);
            NetPattern::parse(&pattern).ok()?;
            format!("net {pattern}")
        }
        Capability::Http { method, scheme, host, port, path } => {
            let (names, alt) = url_segments(path);
            if alt.is_some() || !is_method(&method.to_ascii_uppercase()) || !matches!(scheme.as_str(), "http" | "https") {
                return None;
            }
            // Include the port for IP addresses so the parser does not treat a
            // numeric first path segment as a CIDR prefix length.
            let mut url = match parse_ip(host) {
                Some(_) => format!("{scheme}://{}", format_host_port(host, *port)),
                None => format_url_origin(scheme, host, *port),
            };
            for name in &names {
                url.push('/');
                url.push_str(&crate::stdlib::uri_encode(name, b"!$&'()+;=:@-._~"));
            }
            UrlPattern::parse(&url.chars().map(|c| (c, false)).collect::<Vec<_>>()).ok()?;
            format!("http {} {url}", method.to_ascii_uppercase())
        }
        Capability::NetListen(addr) => format!("listen {}", quote_part(&addr.to_string(), true)),
        Capability::Env(name) => format!("env {}", quote_part(name, true)),
        Capability::Stdin => "stdin".into(),
        Capability::TimeZone => "timezone".into(),
        Capability::Stdout | Capability::Stderr if allow => return None,
        Capability::Stdout => "stdout".into(),
        Capability::Stderr => "stderr".into(),
        Capability::Custom { name, detail } => format!("custom {} {}", quote_part(name, true), quote_part(detail, true)),
        Capability::NetAddr { .. } => return None,
    };
    Some(format!("{verb} {rule}"))
}

/// Parses a size: a number of bytes with an optional `K`, `M` or `G` suffix
/// (powers of 1024), e.g. `512`, `64K`, `256M`, `1G`. Sizes too large for
/// `usize` saturate, as do suffixed ones too large for `u64`.
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

/// Parses a duration such as `500ms`, `10s`, `1.5m`, or `1d`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let units: [(&str, f64); 8] =
        [("ms", 1e-3), ("us", 1e-6), ("ns", 1e-9), ("s", 1.0), ("m", 60.0), ("h", 3600.0), ("d", 86400.0), ("w", 604800.0)];
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
    use crate::policy::PathSpace;

    fn parse(text: &str) -> PolicyFile {
        PolicyFile::parse(text, Path::new("/base"), Some(Path::new("/home/me"))).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn errors(text: &str) -> Vec<String> {
        PolicyFile::parse(text, Path::new("/base"), Some(Path::new("/home/me")))
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.to_string())
            .collect()
    }

    fn decide(p: &PolicyFile, cap: Capability) -> Decision {
        p.policy().check_in(&cap, PathSpace::Memory)
    }

    fn read(p: &PolicyFile, path: &str) -> Decision {
        decide(p, Capability::FsRead(PathBuf::from(path)))
    }

    fn write(p: &PolicyFile, path: &str) -> Decision {
        decide(p, Capability::FsWrite(PathBuf::from(path)))
    }

    fn http(p: &PolicyFile, method: &str, url: &str) -> Decision {
        let (scheme, rest) = url.split_once("://").unwrap();
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.ends_with(']') => (h, p.parse().unwrap()),
            _ => (authority, if scheme == "http" { 80 } else { 443 }),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let path = if path.is_empty() { "/".into() } else { path.to_string() };
        decide(p, Capability::Http { method: method.into(), scheme: scheme.into(), host, port, path })
    }

    use Decision::{Allow, Deny, Prompt};

    #[test]
    fn rules_limits_and_comments() {
        let p = parse(
            "# A policy\n\
             allow read ./data, \"/with space\"   # trailing comment\n\
             allow write /out\n\
             deny read ./data/secrets\n\
             allow env HOME,USER\n\
             allow net api.example.com:443 *.example.org\n\
             default prompt\n\
             limit memory 64M\n\
             limit time 1.5s\n\
             limit instructions 1_000_000\n\
             limit cpu none\n",
        );
        assert_eq!(read(&p, "/base/data/x"), Allow);
        assert_eq!(read(&p, "/with space/x"), Allow);
        assert_eq!(read(&p, "/base/data/secrets/x"), Deny);
        assert_eq!(read(&p, "/out/x"), Allow, "write implies read");
        assert_eq!(write(&p, "/base/data/x"), Prompt, "the default decides the rest");
        assert_eq!(decide(&p, Capability::Env("USER".into())), Allow);
        assert_eq!(decide(&p, Capability::Net { host: "a.example.org".into(), port: 80 }), Allow);
        assert_eq!(decide(&p, Capability::Net { host: "localhost".into(), port: 80 }), Prompt);
        assert_eq!(decide(&p, Capability::Stdout), Allow);
        assert_eq!(p.limits.max_memory, Some(64 << 20));
        assert_eq!(p.limits.max_wall_time, Some(Duration::from_millis(1500)));
        assert_eq!(p.limits.max_instructions, Some(1_000_000));
        assert_eq!(p.limits.max_cpu_time, None);
        assert!(!p.private_net);
    }

    #[test]
    fn paths_are_anchored_and_take_wildcards() {
        let p = parse(
            "allow read /System/** ~/notes/*.md\n\
             allow ./\n\
             deny ~/Documents/*\n\
             allow ~/Documents/project\n\
             deny ./**/.env\n\
             deny write ./*/generated\n\
             allow read \"/lit/*\"",
        );
        assert_eq!(read(&p, "/System"), Allow);
        assert_eq!(read(&p, "/System/Library/x"), Allow);
        assert_eq!(read(&p, "/Systemd"), Deny);
        assert_eq!(read(&p, "/home/me/notes/a.md"), Allow);
        assert_eq!(read(&p, "/home/me/notes/a.txt"), Deny);
        assert_eq!(read(&p, "/home/me/notes/sub/a.md"), Deny, "`*` stays within a directory");
        assert_eq!(write(&p, "/base/src/main.sand"), Allow);
        assert_eq!(read(&p, "/base/.env"), Deny);
        assert_eq!(write(&p, "/base/a/b/.env/x"), Deny);
        assert_eq!(read(&p, "/base/a/b/.envrc"), Allow);
        assert_eq!(read(&p, "/home/me/Documents"), Deny, "no rule");
        assert_eq!(read(&p, "/home/me/Documents/taxes/2024.pdf"), Deny);
        assert_eq!(write(&p, "/home/me/Documents/project/x"), Allow, "more specific than `~/Documents/*`");
        assert_eq!(read(&p, "/base/lib/generated/x"), Allow);
        assert_eq!(write(&p, "/base/lib/generated/x"), Deny);
        assert_eq!(read(&p, "/lit/*"), Allow, "quoted wildcards are literal");
        assert_eq!(read(&p, "/lit/x"), Deny);
    }

    #[test]
    fn the_most_specific_rule_wins_and_deny_wins_ties() {
        let p = parse(
            "allow read /\n\
             deny /home\n\
             allow /home/me/project\n\
             deny read /home/*/.ssh\n\
             allow read /home/me/.ssh/config\n\
             deny read /a/*\n\
             allow read /a/b\n\
             allow read /c/*\n\
             deny read /c/*",
        );
        assert_eq!(read(&p, "/etc/hosts"), Allow);
        assert_eq!(read(&p, "/home/other/x"), Deny);
        assert_eq!(write(&p, "/home/me/project/src/x"), Allow);
        assert_eq!(read(&p, "/home/me/.ssh/id_ed25519"), Deny, "matches deeper than `/home/me`");
        assert_eq!(read(&p, "/home/me/.ssh/config"), Allow);
        assert_eq!(read(&p, "/a/b/c"), Allow, "a name is more specific than `*`");
        assert_eq!(read(&p, "/a/x"), Deny);
        assert_eq!(read(&p, "/c/x"), Deny, "a tie");
    }

    #[test]
    fn deny_rules_match_other_spellings_and_allow_rules_do_not() {
        let p = parse("allow ./data\ndeny ./data/secrets\nallow read ./data/Secrets/public");
        assert_eq!(read(&p, "/base/data/SECRETS/x"), Deny);
        assert_eq!(read(&p, "/base/data/ſecrets/x"), Deny);
        assert_eq!(read(&p, "/base/data/Secrets/public/x"), Allow);
        assert_eq!(read(&p, "/base/data/SECRETS/public/x"), Deny);
    }

    #[test]
    fn sets_expand_where_used() {
        let p = parse(
            "allow http GET POST api.github.com/repos/{owners}/{repos}\n\
             let owners = alice, bob\n\
             let repos = \"one\" two\n\
             allow read {dirs}/*.txt\n\
             let dirs = ./a /b",
        );
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/bob/two/issues"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/carol/two/issues"), Deny);
        assert_eq!(read(&p, "/base/a/x.txt"), Allow);
        assert_eq!(read(&p, "/b/x.txt"), Allow);
        assert_eq!(errors("allow read {nope}"), ["1:12: no set `nope` (define it: `let nope = ...`)"]);
        assert_eq!(errors("let a = x\nlet a = y"), ["2:5: set `a` was already defined on line 1"]);
        assert_eq!(errors("let a = {b}\nlet b = x"), ["1:9: the values of a set cannot use sets"]);
        assert_eq!(errors("let a ="), ["1:1: expected `let NAME = VALUE...`, e.g. `let owners = alice bob`"]);
        let many = "let n = 0 1 2 3 4 5 6 7 8 9\nallow env {n}{n}{n}{n}{n}{n}";
        assert_eq!(errors(many), ["2:11: `{n}{n}{n}{n}{n}{n}` stands for more than 100000 patterns"]);
    }

    #[test]
    fn http_rules_match_methods_hosts_and_paths() {
        let p = parse(
            "allow http GET HEAD github.com api.github.com\n\
             allow http POST api.github.com/repos/me/**\n\
             allow http api.github.com/repos/me/scratch\n\
             deny http DELETE api.github.com\n\
             allow http https://example.com/public\n\
             allow http *.example.org:8080/api/*/items\n\
             deny http *.example.org:8080/api/admin/*\n\
             allow http localhost:3000",
        );
        assert_eq!(http(&p, "GET", "https://github.com/me/repo"), Allow);
        assert_eq!(http(&p, "get", "https://github.com/"), Allow, "methods ignore case");
        assert_eq!(http(&p, "POST", "https://github.com/x"), Deny);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/me/x/issues"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/you/x"), Deny);
        assert_eq!(http(&p, "DELETE", "https://api.github.com/repos/me/x"), Deny);
        assert_eq!(http(&p, "DELETE", "https://api.github.com/repos/me/scratch/x"), Allow, "the deeper match wins");
        assert_eq!(http(&p, "DELETE", "https://api.github.com/user"), Deny);
        assert_eq!(http(&p, "PUT", "https://example.com/public/a"), Allow);
        assert_eq!(http(&p, "PUT", "http://example.com/public/a"), Deny, "the scheme was given");
        assert_eq!(http(&p, "PUT", "https://example.com:8443/public/a"), Deny, "its default port too");
        assert_eq!(http(&p, "GET", "http://a.example.org:8080/api/v1/items/3"), Allow);
        assert_eq!(http(&p, "GET", "http://a.example.org:8080/api/admin/items"), Deny, "a tie");
        assert_eq!(http(&p, "GET", "http://localhost:3000/"), Allow, "named explicitly");
        // Paths are compared as servers see them.
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/me/../you/x"), Deny);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/%6De/x"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/me%2F..%2F..%2Fyou/x"), Deny, "%2F may be a separator");
        assert_eq!(http(&p, "GET", "http://a.example.org:8080/api/ADMIN/items"), Deny, "deny rules ignore case");
    }

    #[test]
    fn net_rules_decide_http_requests_and_not_the_other_way_round() {
        let p = parse("allow net api.example.com\ndeny http DELETE api.example.com/admin\nallow http GET other.com");
        assert_eq!(http(&p, "PATCH", "https://api.example.com/x"), Allow);
        assert_eq!(http(&p, "DELETE", "https://api.example.com/admin/x"), Deny);
        assert_eq!(http(&p, "GET", "https://other.com/x"), Allow);
        assert_eq!(decide(&p, Capability::Net { host: "other.com".into(), port: 443 }), Deny);
        let p = parse("allow http *\nallow private-net\ndeny net 10.0.0.0/8");
        assert_eq!(http(&p, "GET", "http://10.1.2.3/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[::ffff:10.1.2.3]/x"), Deny);
        assert_eq!(http(&p, "GET", "http://8.8.8.8/x"), Allow);
        let p = parse("allow net localhost:8080");
        assert_eq!(http(&p, "GET", "https://localhost:8080/"), Allow);
        let p = parse("allow http *");
        assert_eq!(http(&p, "GET", "http://127.0.0.1/"), Deny, "private addresses need an explicit rule");
        assert_eq!(http(&p, "GET", "http://localhost/"), Deny);
    }

    #[test]
    fn http_rules_take_single_addresses() {
        let p =
            parse("allow http *\nallow private-net\ndeny http 10.0.0.1/x\ndeny http GET [fd00::1]/admin\ndeny http [2001:db8::5]:8080/x");
        assert_eq!(http(&p, "GET", "http://10.0.0.1/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[::ffff:10.0.0.1]/x"), Deny);
        assert_eq!(http(&p, "GET", "http://10.0.0.2/x"), Allow);
        assert_eq!(http(&p, "GET", "http://[fd00::1]/admin/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[fd00::1]/x"), Allow);
        assert_eq!(http(&p, "GET", "http://[2001:db8::5]:8080/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[2001:db8::5]/x"), Allow);
        let p = parse("allow http *\nallow private-net\ndeny http [::1]/admin\ndeny http 10.0.0.1:80/8");
        assert_eq!(http(&p, "GET", "http://[::1]/admin"), Deny);
        assert_eq!(http(&p, "GET", "http://[::1]/x"), Allow);
        assert_eq!(http(&p, "GET", "http://10.0.0.1/8"), Deny);
        assert_eq!(http(&p, "GET", "http://10.0.0.1/x"), Allow);
        let p = parse("allow http *\nallow private-net\ndeny http 10.0.0.1/32/8\ndeny http [::]/128/8");
        assert_eq!(http(&p, "GET", "http://10.0.0.1/8"), Deny);
        assert_eq!(http(&p, "GET", "http://10.0.0.2/8"), Allow);
        assert_eq!(http(&p, "GET", "http://[::]/8"), Deny);
        assert_eq!(http(&p, "GET", "http://[::1]/8"), Allow);
        assert_eq!(
            errors(
                "deny http 10.0.0.0/8\ndeny http [fc00::/7]:443/x\ndeny http 192.168.1.1/24/x\ndeny http [::1]/64/x\nallow net 10.0.0.1/8\nallow net 10.0.0.0/+8"
            ),
            [
                "1:11: `10.0.0.0/8` is a range, and http rules match single addresses: write a net rule for the range (`net 10.0.0.0/8`), or `10.0.0.0/32/8` for the path `/8` on `10.0.0.0`",
                "2:11: `[fc00::/7]:443` is a range, and http rules match single addresses: write a net rule for the range (`net [fc00::/7]:443`)",
                "3:11: `192.168.1.1/24` is a range, and http rules match single addresses: write a net rule for the range (`net 192.168.1.0/24`), or `192.168.1.1/32/24` for the path `/24` on `192.168.1.1`",
                "4:11: `[::1]/64` is a range, and http rules match single addresses: write a net rule for the range (`net ::/64`), or `[::1]/128/64` for the path `/64` on `::1`",
                "5:11: `10.0.0.1/8` has bits set after its prefix length: write `10.0.0.0/8`",
                "6:11: invalid host pattern `10.0.0.0/+8` (expected a host, host:port, *.domain, *, an IP address or a CIDR range)",
            ]
        );
    }

    #[test]
    fn hashes_start_comments_only_at_the_start_of_a_word() {
        let p = parse("deny read ./data/x#1   # the comment\nallow read ./data");
        assert_eq!(read(&p, "/base/data/x#1"), Deny);
        assert_eq!(read(&p, "/base/data/x"), Allow);
    }

    #[test]
    fn every_capability_can_be_denied() {
        let p = parse("default allow\ndeny env SECRET AWS_*\ndeny stdin\ndeny listen *:22\ndeny import /base/vendor\ndeny custom db write");
        assert_eq!(decide(&p, Capability::Env("SECRET".into())), Deny);
        assert_eq!(decide(&p, Capability::Env("AWS_KEY".into())), Deny);
        assert_eq!(decide(&p, Capability::Env("OTHER".into())), Allow);
        assert_eq!(decide(&p, Capability::Env("*".into())), Deny, "all variables would include the denied one");
        assert_eq!(decide(&p, Capability::Stdin), Deny);
        assert_eq!(decide(&p, Capability::NetListen("0.0.0.0:22".parse().unwrap())), Deny);
        assert_eq!(decide(&p, Capability::NetListen("0.0.0.0:80".parse().unwrap())), Allow);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/vendor/x.sand"))), Deny);
        assert_eq!(decide(&p, Capability::Custom { name: "db".into(), detail: "write".into() }), Deny);
        assert_eq!(decide(&p, Capability::Custom { name: "db".into(), detail: "read".into() }), Allow);
        let p = parse("allow env AWS_* HOME\ndeny env AWS_SECRET_*\nallow env AWS_SECRET_PUBLIC");
        assert_eq!(decide(&p, Capability::Env("AWS_REGION".into())), Allow);
        assert_eq!(decide(&p, Capability::Env("AWS_SECRET_KEY".into())), Deny);
        assert_eq!(decide(&p, Capability::Env("AWS_SECRET_PUBLIC".into())), Allow);
    }

    #[test]
    fn imports_are_restricted_by_import_rules_and_read_denies() {
        let p = parse("deny read ./private");
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/x.sand"))), Allow);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/private/x.sand"))), Deny);
        let p = parse("allow import ./lib\ndeny read ./lib/private\nallow import ./lib/private/ok.sand");
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/x.sand"))), Allow);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/other.sand"))), Deny);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/private/x.sand"))), Deny);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/private/ok.sand"))), Allow);
    }

    #[test]
    fn prompt_answers_become_rules_that_decide_them() {
        let caps = [
            Capability::FsRead(PathBuf::from("/data/a file, \"quoted\".txt")),
            Capability::FsWrite(PathBuf::from("/out/#hash")),
            Capability::FsRead(PathBuf::from("/data/*/{x}")),
            Capability::Import(PathBuf::from("/lib/util.sand")),
            Capability::Net { host: "api.example.com".into(), port: 443 },
            Capability::Net { host: "::1".into(), port: 8080 },
            Capability::Net { host: "10.0.0.5".into(), port: 80 },
            Capability::Http {
                method: "GET".into(),
                scheme: "https".into(),
                host: "api.example.com".into(),
                port: 443,
                path: "/a b/*/%25/x?y".into(),
            },
            Capability::Http { method: "post".into(), scheme: "http".into(), host: "::1".into(), port: 8080, path: "/".into() },
            Capability::NetListen("127.0.0.1:8080".parse().unwrap()),
            Capability::Env("HOME".into()),
            Capability::Env("*".into()),
            Capability::Stdin,
            Capability::TimeZone,
            Capability::Custom { name: "db".into(), detail: "drop table".into() },
            Capability::NetListen("[::1]:0".parse().unwrap()),
        ];
        for cap in &caps {
            for allow in [true, false] {
                let rule = rule_for(cap, allow).unwrap_or_else(|| panic!("no rule for {cap:?}"));
                // With the rule, a policy that would otherwise ask decides.
                let p = parse(&format!("default prompt\nallow private-net\n{rule}"));
                let want = if allow { Allow } else { Deny };
                assert_eq!(decide(&p, cap.clone()), want, "{rule}");
            }
        }
        assert_eq!(rule_for(&caps[0], true).unwrap(), "allow read \"/data/a file, \\\"quoted\\\".txt\"");
        assert_eq!(rule_for(&caps[2], true).unwrap(), "allow read \"/data/*/{x}\"");
        assert_eq!(rule_for(&caps[7], true).unwrap(), "allow http GET https://api.example.com/a%20b/%2A/%25/x%3Fy");
        assert_eq!(rule_for(&caps[8], false).unwrap(), "deny http POST http://[::1]:8080");
        assert_eq!(rule_for(&Capability::Stdout, true), None);
        assert_eq!(rule_for(&Capability::Stdout, false).as_deref(), Some("deny stdout"));
        let p = parse(&format!("default prompt\n{}", rule_for(caps.last().unwrap(), true).unwrap()));
        assert_eq!(decide(&p, Capability::NetListen("[::1]:22".parse().unwrap())), Prompt);
    }

    proptest::proptest! {
        #[test]
        fn rules_for_http_requests_to_addresses_match_single_address(
            bits in proptest::prelude::any::<u128>(),
            zeros in 0..128u32,
            v6 in proptest::prelude::any::<bool>(),
            https in proptest::prelude::any::<bool>(),
            port in proptest::sample::select(vec![80u16, 443, 8080]),
            names in proptest::collection::vec(0..=130u32, 1..3),
        ) {
            let ip: std::net::IpAddr = if v6 {
                std::net::Ipv6Addr::from(bits.checked_shr(zeros).unwrap_or(0).checked_shl(zeros).unwrap_or(0)).into()
            } else {
                let zeros = zeros % 32;
                std::net::Ipv4Addr::from((bits as u32) >> zeros << zeros).into()
            };
            let other = match ip {
                std::net::IpAddr::V4(v4) => std::net::IpAddr::V4((u32::from(v4) ^ 1).into()),
                std::net::IpAddr::V6(v6) => std::net::IpAddr::V6((u128::from(v6) ^ 1).into()),
            };
            let path: String = names.iter().map(|n| format!("/{n}")).collect();
            let scheme = if https { "https" } else { "http" };
            let cap = |ip: std::net::IpAddr| Capability::Http {
                method: "GET".into(), scheme: scheme.into(), host: ip.to_string(), port, path: path.clone(),
            };
            for allow in [true, false] {
                let rule = rule_for(&cap(ip), allow);
                proptest::prop_assert!(rule.is_some(), "no rule for {}", cap(ip));
                let p = parse(&format!("default prompt\nallow private-net\n{}", rule.unwrap()));
                proptest::prop_assert_eq!(decide(&p, cap(ip)), if allow { Allow } else { Deny }, "{}", cap(ip));
                proptest::prop_assert_eq!(decide(&p, cap(other)), Prompt, "{}: {}", rule_for(&cap(ip), allow).unwrap(), cap(other));
            }
        }
    }

    #[test]
    fn private_networks_are_denied_unless_allowed() {
        let net = Capability::Net { host: "10.1.2.3".into(), port: 80 };
        let p = parse("allow net *");
        assert_eq!(decide(&p, net.clone()), Deny);
        let p = parse("allow net *\nallow private-net");
        assert_eq!(decide(&p, net.clone()), Allow);
        let p = parse("default allow");
        for host in ["10.1.2.3", "169.254.169.254", "localhost", "127.0.0.1", "::1"] {
            assert_eq!(decide(&p, Capability::Net { host: host.into(), port: 80 }), Deny, "{host}");
            assert_eq!(http(&p, "GET", &format!("http://{}/", super::format_host_port(host, 80))), Deny, "{host}");
        }
        assert_eq!(decide(&p, Capability::Net { host: "example.com".into(), port: 80 }), Allow);
        assert_eq!(decide(&parse("default allow\nallow private-net"), net.clone()), Allow);
        assert_eq!(decide(&parse("default prompt"), net), Prompt);
    }

    #[test]
    fn every_error_is_reported_with_its_place() {
        assert_eq!(
            errors(
                "alow read /x\nallow\nallow net not a pattern!\nlimit memory lots\nlimit\ndefault maybe\nallow stdout\ndeny custom\nallow read \"open\n\
                 allow raed /x\nallow read data\nallow read ~user\nallow http GET\nallow http a.com GET\nallow http ftp://x\nallow read /a/**/../b\n\
                 allow read /a/b**\nallow http a.com/x?y"
            ),
            [
                "1:1: unknown rule `alow` (rules start with `allow`, `deny`, `default`, `limit` or `let`)",
                "2:1: expected what to allow: a path, or read, write, import, net, http, private-net, listen, env, stdin, stdout, stderr, timezone or custom",
                "3:17: invalid host pattern `pattern!` (expected a host, host:port, *.domain, *, an IP address or a CIDR range)",
                "4:7: invalid size `lots` (use e.g. 4096, 64K, 256M, 1G)",
                "5:1: expected `limit NAME VALUE`, e.g. `limit memory 64M`",
                "6:9: unknown default `maybe` (expected `deny`, `prompt` or `allow`)",
                "7:7: standard out is allowed unless denied (`deny stdout`)",
                "8:6: expected `deny custom NAME [DETAIL]`",
                "9:12: unterminated quotes",
                "10:7: unknown capability `raed` (expected a path, or read, write, import, net, http, private-net, listen, env, stdin, stdout, stderr, timezone or custom)",
                "11:12: path `data` is not anchored: write `./data` for a path relative to the policy file, `~/...` or an absolute path",
                "12:12: path `~user` is not anchored: write `./~user` for a path relative to the policy file, `~/...` or an absolute path",
                "13:12: `allow http` needs a URL pattern after its methods, e.g. `api.example.com/v1`",
                "14:18: methods come before the URL patterns",
                "15:12: unsupported scheme `ftp` (expected http or https)",
                "16:12: `..` cannot follow a wildcard",
                "17:12: `**` must be a whole path segment",
                "18:12: URL patterns match paths, not queries (remove the `?...`)",
            ]
        );
        assert_eq!(errors("default deny\ndefault allow"), ["2:9: `default` was already given on line 1"]);
        assert_eq!(errors("allow stdin now"), ["1:13: `allow stdin` takes no arguments"]);
        assert_eq!(errors("allow read ~/x"), Vec::<String>::new());
        assert_eq!(
            PolicyFile::parse("allow read ~/x", Path::new("/"), None).unwrap_err()[0].message,
            "`~` cannot be used here: there is no home directory"
        );
        assert_eq!(errors("allow read a}"), ["1:13: unmatched `}` (quote it for a literal `}`)"]);
        assert_eq!(errors("allow read {a b}"), ["1:12: `{` starts a set reference such as `{owners}` (quote it for a literal `{`)"]);
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
