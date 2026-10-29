//! Policy file parser and formatter.
//!
//! ```text
//! let owners = alice bob            # a set, referenced as {owners}
//! allow read /usr ~/.gitconfig
//! allow ./                          # read and write the file's directory
//! deny ./**/.env
//! allow http GET api.github.com
//! allow http POST api.github.com/repos/{owners}/**
//! prompt write ./out
//! limit memory 64M
//! ```

use super::net::{HostKind, ListenPattern, NetPattern, QUOTED_STAR_IN_HOST, UrlPattern, format_host_port, url_segments};
use super::path::{Seg, Tok};
use super::rules::{Access, NamePattern, Origin, PathPattern, Rule, RuleKind, Transports};
use super::{Capability, Decision, RulePolicy};
use crate::Limits;
use crate::vm::value::FxHashMap;
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A parsed policy file.
#[derive(Clone, Debug)]
pub struct PolicyFile {
    /// Policy rules.
    pub rules: RulePolicy,
    /// Execution limits configured by `limit` directives.
    pub limits: Limits,
}

/// An error in a policy file, with 1-based line and column numbers.
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

const MAX_EXPANSION: usize = 100_000;

impl PolicyFile {
    /// Parses a policy file. Relative paths are resolved from `base`, and `~`
    /// is resolved from `home` or rejected if `home` is `None`.
    pub fn parse(text: &str, base: &Path, home: Option<&Path>) -> Result<PolicyFile, Vec<PolicyError>> {
        parse(text, base, home).map(|p| p.file)
    }
}

pub(crate) struct Parsed {
    pub file: PolicyFile,
    pub sets: Vec<(String, usize, usize, bool)>,
}

pub(crate) struct Line {
    pub words: Vec<Word>,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Word {
    pub parts: Vec<Part>,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    Raw(String),
    Quoted(String),
    Ref(String),
}

impl Word {
    pub(crate) fn plain(&self) -> Option<Cow<'_, str>> {
        match self.parts.as_slice() {
            [Part::Raw(s) | Part::Quoted(s)] => Some(Cow::Borrowed(s)),
            parts if parts.iter().any(|p| matches!(p, Part::Ref(_))) => None,
            _ => Some(Cow::Owned(self.text())),
        }
    }

    pub(crate) fn text(&self) -> String {
        self.parts
            .iter()
            .map(|p| match p {
                Part::Raw(s) | Part::Quoted(s) => s.clone(),
                Part::Ref(n) => format!("{{{n}}}"),
            })
            .collect()
    }

    /// Returns the word text if all parts are unquoted literals without set references.
    pub(crate) fn bare(&self) -> Option<String> {
        self.parts.iter().map(|p| if let Part::Raw(s) = p { Some(s.as_str()) } else { None }).collect()
    }

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

    /// Formats the word as policy text, preserving quotes around quoted parts.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Raw(s) => out.push_str(s),
                Part::Quoted(s) => out.push_str(&quote(s)),
                Part::Ref(n) => {
                    out.push('{');
                    out.push_str(n);
                    out.push('}');
                }
            }
        }
        out
    }

    fn expand(&self, sets: &FxHashMap<String, (Vec<Word>, usize)>, used: &mut FxHashMap<String, bool>) -> Result<Vec<Word>, String> {
        let mut out = vec![Word { parts: vec![], column: self.column }];
        for p in &self.parts {
            match p {
                Part::Ref(name) => {
                    let Some((values, _)) = sets.get(name) else {
                        return Err(format!("undefined set `{name}`: define it with `let {name} = ...`"));
                    };
                    used.insert(name.clone(), true);
                    if out.len().saturating_mul(values.len()) > MAX_EXPANSION {
                        return Err(format!("`{}` expands to more than {MAX_EXPANSION} patterns", self.render()));
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

fn quote_part(s: &str, first: bool) -> String {
    let special = |c: char| c.is_whitespace() || matches!(c, ',' | '"' | '*' | '{' | '}');
    if !s.is_empty() && !s.contains(special) && !(first && s.starts_with('#')) {
        return s.to_string();
    }
    quote(s)
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn transport(word: &str) -> Option<Transports> {
    if word.eq_ignore_ascii_case("tls") {
        Some(Transports::TLS)
    } else if word.eq_ignore_ascii_case("plaintext") {
        Some(Transports::PLAINTEXT)
    } else {
        None
    }
}

fn invalid_transport_hint(word: &str) -> Option<String> {
    let lower = word.to_ascii_lowercase();
    let advice = match lower.as_str() {
        "http" => "write `plaintext` for unencrypted connections such as `http://`",
        "tcp" => "write `plaintext` for unencrypted connections, or `tls plaintext` for both",
        "https" | "ssl" | "wss" => "write `tls` for encrypted connections",
        "ws" => "write `plaintext` for unencrypted connections such as `ws://`",
        "udp" => "only TCP connections are supported",
        _ => return None,
    };
    Some(format!("`{word}` is not a transport: {advice}; write `{lower}.` for hostname `{lower}`"))
}

const KINDS: [&str; 12] = ["read", "write", "import", "net", "http", "listen", "env", "custom", "stdin", "stdout", "stderr", "timezone"];

fn invalid_capability_hint(word: &str) -> Option<String> {
    let lower = word.to_ascii_lowercase();
    if KINDS.contains(&lower.as_str()) {
        return Some(if lower == word {
            format!("`{word}` is a separate capability: put each rule on a separate line or CLI flag")
        } else {
            format!("capabilities are lowercase: write `{lower}`")
        });
    }
    if ["allow", "deny", "prompt"].contains(&lower.as_str()) {
        return Some(format!("duplicate rule verb: remove `{word}`"));
    }
    if word.contains("://") {
        return Some(format!("`{word}` is a URL, not a path: use an `http` rule"));
    }
    let (head, rest) = word.split_once([':', '='])?;
    let separator = &word[head.len()..=head.len()];
    let head = head.to_ascii_lowercase();
    KINDS.contains(&head.as_str()).then(|| match rest {
        "" => format!("remove `{separator}` after the capability: `{head}`"),
        _ => format!("put a space after the capability: `{head} {rest}`"),
    })
}

fn is_keyword(word: &str) -> bool {
    transport(word).is_some() || invalid_transport_hint(word).is_some() || is_method(word)
}

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
                                _ => return Err((column(i + 1 + j), "invalid escape in quotes: use \\\" or \\\\".into())),
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
                        return Err((column(i), "invalid set reference after `{`: use `{NAME}` or quote `\"{\"`".into()));
                    };
                    parts.push(Part::Ref(name.to_string()));
                    i = end + 1;
                }
                '}' => return Err((column(i), "unmatched `}`: quote `\"}\"` for a literal brace".into())),
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

pub(crate) fn is_method(word: &str) -> bool {
    KNOWN_METHODS.iter().any(|m| m.eq_ignore_ascii_case(word))
        || (word.bytes().any(|b| b.is_ascii_uppercase())
            && word.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_'))
}

const CAPABILITIES: &str = "read, write, import, net, http, listen, env, stdin, stdout, stderr, timezone, or custom";

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
            errors.push(error(bad.column, "set values cannot reference other sets".into()));
        } else if let Some((_, line)) = cx.sets.get(&name_text) {
            errors.push(error(name.column, format!("set `{name_text}` was already defined on line {line}")));
        } else {
            sets.push((name_text.clone(), i + 1, name.column));
            cx.sets.insert(name_text, (values.to_vec(), i + 1));
        }
    }
    let mut file = PolicyFile { rules: RulePolicy::new(), limits: Limits::default() };
    let mut rules = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some((first, args)) = line.words.split_first() else { continue };
        let error = |column, message: String| PolicyError { line: i + 1, column, message };
        let result = match first.plain().as_deref() {
            Some("let") => Ok(()),
            Some(verb @ ("allow" | "deny" | "prompt")) => cx.rule(decision(verb), first, args, i + 1, &mut rules),
            Some("default") => Err((
                first.column,
                "unknown directive `default`: unmatched requests are prompted or denied; write rules for what to allow or deny".into(),
            )),
            Some("limit") => limit(&mut file.limits, args).map_err(|m| (args.first().unwrap_or(first).column, m)),
            _ => {
                Err((first.column, format!("unknown directive `{}`: expected `allow`, `deny`, `prompt`, `limit`, or `let`", first.text())))
            }
        };
        if let Err((column, message)) = result {
            errors.push(error(column, message));
        }
    }
    if !errors.is_empty() {
        errors.sort_by_key(|e| (e.line, e.column));
        return Err(errors);
    }
    file.rules = RulePolicy::new().extend_rules(rules);
    let sets = sets.into_iter().map(|(name, line, column)| (name.clone(), line, column, cx.used.contains_key(&name))).collect();
    Ok(Parsed { file, sets })
}

fn decision(verb: &str) -> Decision {
    match verb {
        "allow" => Decision::Allow,
        "prompt" => Decision::Prompt,
        _ => Decision::Deny,
    }
}

pub(crate) fn verb(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::Prompt => "prompt",
    }
}

fn file_rule(decision: Decision, kind: RuleKind, origin: Origin) -> Rule {
    Rule { decision, kind, origin: Some(origin), tier: 0 }
}

impl Context<'_> {
    fn rule(
        &mut self,
        decision: Decision,
        verb_word: &Word,
        args: &[Word],
        line: usize,
        rules: &mut Vec<Rule>,
    ) -> Result<(), (usize, String)> {
        let verb_text = verb(decision);
        let Some((first_arg, rest_args)) = args.split_first() else {
            let action = if decision == Decision::Prompt { "prompt for" } else { verb_text };
            return Err((verb_word.column, format!("expected what to {action}: a path, or {CAPABILITIES}")));
        };
        let keyword = first_arg.plain().filter(|w| w.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
        let (kind, rest_args) = match keyword.as_deref() {
            Some(k @ ("read" | "write" | "import" | "net" | "http" | "listen" | "env" | "custom")) => (k, rest_args),
            Some(k @ ("stdin" | "stdout" | "stderr" | "timezone")) => {
                if let Some(extra) = rest_args.first() {
                    return Err((extra.column, format!("`{verb_text} {k}` does not accept arguments")));
                }
                let kind = match k {
                    "stdin" => RuleKind::Stdin,
                    "timezone" => RuleKind::TimeZone,
                    "stdout" => RuleKind::Stdout,
                    _ => RuleKind::Stderr,
                };
                rules.push(file_rule(decision, kind, Origin { line, column: first_arg.column, text: k.to_string() }));
                return Ok(());
            }
            Some("private-net") => {
                return Err((
                    first_arg.column,
                    "`private-net` is not a capability: specify internal destinations in `net` rules, e.g. `allow net localhost:8080 10.0.0.0/8`"
                        .into(),
                ));
            }
            Some(k) => {
                let message =
                    invalid_capability_hint(k).unwrap_or_else(|| format!("unknown capability `{k}`: expected a path, or {CAPABILITIES}"));
                return Err((first_arg.column, message));
            }
            None => ("files", args),
        };
        if rest_args.is_empty() {
            let hint = match kind {
                "custom" => format!("expected `{verb_text} custom NAME [DETAIL]`"),
                "http" => {
                    format!("expected `{verb_text} http [TRANSPORT...] [METHOD...] URL...`, e.g. `{verb_text} http GET api.example.com/v1`")
                }
                _ => format!("`{verb_text} {kind}` requires arguments"),
            };
            return Err((first_arg.column, hint));
        }
        if kind == "custom" {
            let [name, detail @ ..] = rest_args else { unreachable!() };
            if detail.len() > 1 {
                return Err((detail[1].column, format!("expected `{verb_text} custom NAME [DETAIL]`")));
            }
            let details = match detail.first() {
                Some(d) => {
                    self.expand(d)?.into_iter().map(|w| parse_custom_pattern(&w).map_err(|m| (d.column, m))).collect::<Result<_, _>>()?
                }
                None => vec![None],
            };
            let names = self.expand(name)?;
            if names.len().saturating_mul(details.len()) > MAX_EXPANSION {
                return Err((name.column, format!("`{verb_text} custom` expands to more than {MAX_EXPANSION} rules")));
            }
            for word in names {
                let name_pattern = parse_custom_pattern(&word).map_err(|m| (name.column, m))?;
                for detail in &details {
                    let kind = RuleKind::Custom(name_pattern.clone(), detail.clone());
                    rules.push(file_rule(decision, kind, Origin { line, column: name.column, text: word.render() }));
                }
            }
            return Ok(());
        }
        let mut methods: Option<Vec<String>> = None;
        let mut transports = Transports::empty();
        let mut urls = 0;
        for item in rest_args {
            for word in &self.expand(item)? {
                let bare = word.bare();
                if matches!(kind, "net" | "http")
                    && let Some(bare) = &bare
                {
                    if let Some(t) = transport(bare) {
                        if urls > 0 {
                            let hosts = if kind == "http" { "URL patterns" } else { "hosts" };
                            return Err((item.column, format!("put `tls` and `plaintext` before the {hosts}")));
                        }
                        transports |= t;
                        continue;
                    }
                    if urls == 0
                        && let Some(hint) = invalid_transport_hint(bare)
                    {
                        return Err((item.column, hint));
                    }
                }
                let transports = if transports.is_empty() { Transports::default_for(decision) } else { transports };
                let origin = Origin { line, column: item.column, text: word.render() };
                let kind = match kind {
                    "files" | "read" | "write" | "import" => {
                        let access = match kind {
                            "read" => Access::Read,
                            "write" => Access::Write,
                            "import" => Access::Import,
                            _ => Access::Any,
                        };
                        RuleKind::Files(access, self.path(&word.tokens(), bare.is_some()).map_err(|m| (item.column, m))?)
                    }
                    "net" => {
                        urls += 1;
                        RuleKind::Net(NetPattern::from_tokens(&word.tokens()).map_err(|m| (item.column, m))?, transports)
                    }
                    "http" if bare.as_deref().is_some_and(is_method) => {
                        if urls > 0 {
                            return Err((item.column, "put HTTP methods before the URL patterns".into()));
                        }
                        methods.get_or_insert_with(Vec::new).push(word.text().to_ascii_uppercase());
                        continue;
                    }
                    "http" => {
                        urls += 1;
                        RuleKind::Http(methods.clone(), UrlPattern::parse(&word.tokens()).map_err(|m| (item.column, m))?, transports)
                    }
                    "listen" if word.tokens().iter().any(|&(c, wild)| c == '*' && !wild) => {
                        return Err((item.column, QUOTED_STAR_IN_HOST.into()));
                    }
                    "listen" => RuleKind::Listen(ListenPattern::parse(&word.text()).ok_or_else(|| {
                        (item.column, format!("invalid listen pattern `{}`: expected ip:port, ip, localhost, *:port, or *", word.text()))
                    })?),
                    _ => RuleKind::Env(NamePattern::from_tokens(&word.tokens()).map_err(|m| (item.column, m))?),
                };
                rules.push(file_rule(decision, kind, origin));
            }
        }
        if kind == "http" && urls == 0 {
            let last = rest_args.last().unwrap_or(first_arg);
            let after = if methods.is_some() { "methods" } else { "transports" };
            return Err((last.column, format!("`{verb_text} http` requires a URL pattern after {after}, e.g. `api.example.com/v1`")));
        }
        if kind == "net" && urls == 0 {
            let last = rest_args.last().unwrap_or(first_arg);
            return Err((last.column, format!("`{verb_text} net` requires a host pattern after transports, e.g. `localhost:8080`")));
        }
        Ok(())
    }

    fn expand(&mut self, word: &Word) -> Result<Vec<Word>, (usize, String)> {
        if !word.parts.iter().any(|p| matches!(p, Part::Ref(_))) {
            return Ok(vec![word.clone()]);
        }
        word.expand(&self.sets, &mut self.used).map_err(|m| (word.column, m))
    }

    fn path(&self, text: &[Tok], bare: bool) -> Result<PathPattern, String> {
        let flat: String = text.iter().map(|t| t.0).collect();
        let sep = |c: char| c == '/' || (cfg!(windows) && c == '\\');
        let unanchored = || {
            invalid_capability_hint(&flat).filter(|_| bare).unwrap_or_else(|| {
                format!(
                    "path `{flat}` is not anchored: write `./{flat}` for `{}`, `~/...`, an absolute path, or `**/{flat}` for that name anywhere",
                    self.base.join(&flat).display()
                )
            })
        };
        let starts = |s: &str| flat.starts_with(s) && flat[s.len()..].chars().next().is_none_or(sep);
        let anywhere = text.len() >= 2 && text[..2].iter().all(|t| t.0 == '*' && t.1) && text.get(2).is_none_or(|t| sep(t.0));
        let (mut prefix, rest): (PathBuf, &[Tok]) = if anywhere {
            (PathBuf::new(), text)
        } else if starts("~") {
            let home = self.home.ok_or("cannot resolve `~` without a home directory")?;
            (home.to_path_buf(), &text[1..])
        } else if starts(".") || starts("..") {
            (self.base.to_path_buf(), text)
        } else if flat.starts_with('/') || (cfg!(windows) && Path::new(&flat).is_absolute()) {
            let root = Path::new(&flat)
                .components()
                .take_while(|c| matches!(c, std::path::Component::Prefix(_) | std::path::Component::RootDir))
                .collect::<PathBuf>();
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

fn parse_custom_pattern(word: &Word) -> Result<Option<String>, String> {
    let tokens = word.tokens();
    match tokens.as_slice() {
        [('*', true)] => Ok(None),
        t if t.iter().any(|t| t.1) => Err(format!(
            "custom names and details must be exact or `*`: quote literal asterisks as `\"{}\"`",
            word.text().replace('"', "\\\"")
        )),
        t => Ok(Some(t.iter().map(|t| t.0).collect())),
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
                "unknown limit `{other}`: expected memory, time, cpu, instructions, output, compile-fuel, tasks, call-depth, open-resources, source, or http-body"
            ));
        }
    }
    Ok(())
}

/// Formats a policy rule that allows or denies `cap`, or returns `None` if
/// `cap` cannot be represented as a single rule.
pub fn rule_for(cap: &Capability, allow: bool) -> Option<String> {
    let decision = if allow { Decision::Allow } else { Decision::Deny };
    let path = |p: &Path| p.to_str().map(|s| quote_part(s, true));
    let rule = match cap {
        Capability::FsRead(p) => format!("read {}", path(p)?),
        Capability::FsWrite(p) => format!("write {}", path(p)?),
        Capability::Import(p) => format!("import {}", path(p)?),
        Capability::Net { host, port, tls } => {
            let pattern = format_host_port(host, *port);
            NetPattern::parse(&pattern).ok()?;
            format!("net {}{pattern}", transport_words(Transports::of(*tls), decision))
        }
        Capability::Http { method, scheme, host, port, path } => {
            let (names, alt) = url_segments(path);
            let transport_word = match scheme.as_str() {
                "https" => transport_words(Transports::TLS, decision),
                "http" => transport_words(Transports::PLAINTEXT, decision),
                _ => return None,
            };
            let method = method.to_ascii_uppercase();
            if alt.is_some() || !is_method(&method) || transport(&method).is_some() || invalid_transport_hint(&method).is_some() {
                return None;
            }
            // Always include the port so a numeric first path segment after an IP address is not parsed as a CIDR prefix length.
            let mut url = format_host_port(host, *port);
            for name in &names {
                url.push('/');
                url.push_str(&crate::stdlib::uri_encode(name, b"!$&'()+;=:@-._~"));
            }
            UrlPattern::parse(&url.chars().map(|c| (c, false)).collect::<Vec<_>>()).ok()?;
            format!("http {transport_word}{method} {url}")
        }
        Capability::NetListen(addr) => format!("listen {}", quote_part(&addr.to_string(), true)),
        Capability::Env(name) if name == "*" => "env *".into(),
        Capability::Env(name) => format!("env {}", quote_part(name, true)),
        Capability::Stdin => "stdin".into(),
        Capability::TimeZone => "timezone".into(),
        Capability::Stdout => "stdout".into(),
        Capability::Stderr => "stderr".into(),
        Capability::Custom { name, detail } => format!("custom {} {}", quote_part(name, true), quote_part(detail, true)),
        Capability::NetAddr { .. } => return None,
    };
    (!rule.contains(['\n', '\r'])).then(|| format!("{} {rule}", verb(decision)))
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", verb(self.decision))?;
        match &self.kind {
            RuleKind::Files(access, pattern) => {
                let access = match access {
                    Access::Read => "read ",
                    Access::Write => "write ",
                    Access::Import => "import ",
                    Access::Any => "",
                };
                let mut path = match pattern.prefix.to_str() {
                    Some("") if pattern.rest.is_empty() => "**".to_string(),
                    Some("") => String::new(),
                    Some(prefix) => quote_part(prefix, true),
                    None => quote_part(&pattern.prefix.to_string_lossy(), true),
                };
                for seg in &pattern.rest {
                    if !path.is_empty() && !path.ends_with(['/', '\\']) {
                        path.push('/');
                    }
                    path += &render_seg(seg, |s, _| quote_part(s, false));
                }
                write!(f, "{access}{path}")
            }
            RuleKind::Net(pattern, transports) => write!(f, "net {}{}", transport_words(*transports, self.decision), host_text(pattern)),
            RuleKind::Http(methods, url, transports) => {
                write!(f, "http {}", transport_words(*transports, self.decision))?;
                for m in methods.iter().flatten() {
                    write!(f, "{m} ")?;
                }
                match (&url.host.kind, url.host.port) {
                    (HostKind::Ip(ip @ std::net::IpAddr::V6(_), _), None) => write!(f, "[{ip}]")?,
                    _ => write!(f, "{}", host_text(&url.host))?,
                }
                if let HostKind::Ip(ip, _) = url.host.kind
                    && url.host.port.is_none()
                    && url.path.first().is_some_and(is_numeric_prefix)
                {
                    write!(f, "/{}", if ip.is_ipv6() { 128 } else { 32 })?;
                }
                for seg in &url.path {
                    write!(f, "/{}", render_seg(seg, |s, _| crate::stdlib::uri_encode(s, b"!$&'()+;=:@-._~")))?;
                }
                Ok(())
            }
            RuleKind::Listen(pattern) => write!(f, "listen {pattern}"),
            RuleKind::Env(NamePattern::Exact(name)) => write!(f, "env {}", quote_part(name, true)),
            RuleKind::Env(NamePattern::Glob(g)) => write!(f, "env {}", render_seg(&Seg::Glob(g.clone()), quote_part)),
            RuleKind::Env(NamePattern::Any) => write!(f, "env *"),
            RuleKind::Stdin => write!(f, "stdin"),
            RuleKind::Stdout => write!(f, "stdout"),
            RuleKind::Stderr => write!(f, "stderr"),
            RuleKind::TimeZone => write!(f, "timezone"),
            RuleKind::Custom(name, detail) => {
                write!(f, "custom {}", name.as_deref().map_or_else(|| "*".to_string(), |n| quote_part(n, true)))?;
                match detail {
                    Some(detail) => write!(f, " {}", quote_part(detail, true)),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Formats a host pattern as policy text, appending a trailing dot if the hostname matches a keyword.
fn host_text(pattern: &NetPattern) -> String {
    match &pattern.kind {
        HostKind::Host(h) if is_keyword(h) => {
            let port = pattern.port.map_or_else(String::new, |p| format!(":{p}"));
            format!("{h}.{port}")
        }
        _ => pattern.to_string(),
    }
}

fn is_numeric_prefix(seg: &Seg) -> bool {
    let Seg::Lit(name) = seg else { return false };
    let digits = name.split(':').next().unwrap_or_default();
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn transport_words(transports: Transports, decision: Decision) -> &'static str {
    if transports == Transports::default_for(decision) {
        ""
    } else if transports == Transports::TLS {
        "tls "
    } else if transports == Transports::PLAINTEXT {
        "plaintext "
    } else {
        "tls plaintext "
    }
}

fn render_seg(seg: &Seg, name: impl Fn(&str, bool) -> String) -> String {
    match seg {
        Seg::Lit(s) => name(s, true),
        Seg::Glob(g) => {
            let pieces = g.pieces().iter().enumerate();
            pieces.map(|(i, p)| if p.is_empty() { String::new() } else { name(p, i == 0) }).collect::<Vec<_>>().join("*")
        }
        Seg::Star => "*".into(),
        Seg::GlobStar => "**".into(),
    }
}

/// Parses a byte count with an optional `K`, `M`, or `G` binary suffix, saturating on overflow.
pub fn parse_size(s: &str) -> Result<usize, String> {
    parse_size_u64(s).map(crate::vm::value::u64_to_usize_saturating)
}

fn parse_size_u64(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().map(|n| n.saturating_mul(mult)).map_err(|_| format!("invalid size `{s}`: use e.g. 4096, 64K, 256M, or 1G"))
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
    Err(format!("invalid duration `{s}`: use e.g. 500ms, 10s, or 2m"))
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
        p.rules.check_in(&cap, PathSpace::Memory)
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
             limit memory 64M\n\
             limit time 1.5s\n\
             limit instructions 1_000_000\n\
             limit cpu none\n",
        );
        assert_eq!(read(&p, "/base/data/x"), Allow);
        assert_eq!(read(&p, "/with space/x"), Allow);
        assert_eq!(read(&p, "/base/data/secrets/x"), Deny);
        assert_eq!(read(&p, "/out/x"), Allow);
        assert_eq!(write(&p, "/base/data/x"), Prompt);
        assert_eq!(decide(&p, Capability::Env("USER".into())), Allow);
        assert_eq!(decide(&p, Capability::Net { host: "a.example.org".into(), port: 80, tls: true }), Allow);
        assert_eq!(decide(&p, Capability::Net { host: "a.example.org".into(), port: 80, tls: false }), Prompt);
        assert_eq!(decide(&p, Capability::Net { host: "localhost".into(), port: 80, tls: true }), Prompt);
        assert_eq!(decide(&p, Capability::Stdout), Prompt);
        assert_eq!(p.limits.max_memory, Some(64 << 20));
        assert_eq!(p.limits.max_wall_time, Some(Duration::from_millis(1500)));
        assert_eq!(p.limits.max_instructions, Some(1_000_000));
        assert_eq!(p.limits.max_cpu_time, None);
    }

    #[test]
    fn paths_are_anchored_and_support_wildcards() {
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
        assert_eq!(read(&p, "/Systemd"), Prompt);
        assert_eq!(read(&p, "/home/me/notes/a.md"), Allow);
        assert_eq!(read(&p, "/home/me/notes/a.txt"), Prompt);
        assert_eq!(read(&p, "/home/me/notes/sub/a.md"), Prompt);
        assert_eq!(write(&p, "/base/src/main.sand"), Allow);
        assert_eq!(read(&p, "/base/.env"), Deny);
        assert_eq!(write(&p, "/base/a/b/.env/x"), Deny);
        assert_eq!(read(&p, "/base/a/b/.envrc"), Allow);
        assert_eq!(read(&p, "/home/me/Documents"), Prompt);
        assert_eq!(read(&p, "/home/me/Documents/taxes/2024.pdf"), Deny);
        assert_eq!(write(&p, "/home/me/Documents/project/x"), Allow);
        assert_eq!(read(&p, "/base/lib/generated/x"), Allow);
        assert_eq!(write(&p, "/base/lib/generated/x"), Deny);
        assert_eq!(read(&p, "/lit/*"), Allow);
        assert_eq!(read(&p, "/lit/x"), Prompt);
    }

    #[test]
    fn specificity_and_tie_breaking() {
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
        assert_eq!(read(&p, "/home/me/.ssh/id_ed25519"), Deny);
        assert_eq!(read(&p, "/home/me/.ssh/config"), Allow);
        assert_eq!(read(&p, "/a/b/c"), Allow);
        assert_eq!(read(&p, "/a/x"), Deny);
        assert_eq!(read(&p, "/c/x"), Deny);
    }

    #[test]
    fn deny_rules_are_case_insensitive_and_allow_rules_are_case_sensitive() {
        let p = parse("allow ./data\ndeny ./data/secrets\nallow read ./data/Secrets/public");
        assert_eq!(read(&p, "/base/data/SECRETS/x"), Deny);
        assert_eq!(read(&p, "/base/data/ſecrets/x"), Deny);
        assert_eq!(read(&p, "/base/data/Secrets/public/x"), Allow);
        assert_eq!(read(&p, "/base/data/SECRETS/public/x"), Deny);
    }

    #[test]
    fn set_expansion() {
        let p = parse(
            "allow http GET POST api.github.com/repos/{owners}/{repos}\n\
             let owners = alice, bob\n\
             let repos = \"one\" two\n\
             allow read {dirs}/*.txt\n\
             let dirs = ./a /b",
        );
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/bob/two/issues"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/carol/two/issues"), Prompt);
        assert_eq!(read(&p, "/base/a/x.txt"), Allow);
        assert_eq!(read(&p, "/b/x.txt"), Allow);
        assert_eq!(errors("allow read {nope}"), ["1:12: undefined set `nope`: define it with `let nope = ...`"]);
        assert_eq!(errors("let a = x\nlet a = y"), ["2:5: set `a` was already defined on line 1"]);
        assert_eq!(errors("let a = {b}\nlet b = x"), ["1:9: set values cannot reference other sets"]);
        assert_eq!(errors("let a ="), ["1:1: expected `let NAME = VALUE...`, e.g. `let owners = alice bob`"]);
        let many = "let n = 0 1 2 3 4 5 6 7 8 9\nallow env {n}{n}{n}{n}{n}{n}";
        assert_eq!(errors(many), ["2:11: `{n}{n}{n}{n}{n}{n}` expands to more than 100000 patterns"]);
    }

    #[test]
    fn http_rule_matching() {
        let p = parse(
            "allow http GET HEAD github.com api.github.com\n\
             allow http POST api.github.com/repos/me/**\n\
             allow http api.github.com/repos/me/scratch\n\
             deny http DELETE api.github.com\n\
             allow http example.com:443/public\n\
             allow http plaintext *.example.org:8080/api/*/items\n\
             deny http plaintext *.example.org:8080/api/admin/*\n\
             allow http plaintext localhost:3000",
        );
        assert_eq!(http(&p, "GET", "https://github.com/me/repo"), Allow);
        assert_eq!(http(&p, "get", "https://github.com/"), Allow);
        assert_eq!(http(&p, "POST", "https://github.com/x"), Prompt);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/me/x/issues"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/you/x"), Prompt);
        assert_eq!(http(&p, "DELETE", "https://api.github.com/repos/me/x"), Deny);
        assert_eq!(http(&p, "DELETE", "https://api.github.com/repos/me/scratch/x"), Allow);
        assert_eq!(http(&p, "DELETE", "https://api.github.com/user"), Deny);
        assert_eq!(http(&p, "PUT", "https://example.com/public/a"), Allow);
        assert_eq!(http(&p, "PUT", "http://example.com:443/public/a"), Prompt);
        assert_eq!(http(&p, "PUT", "https://example.com:8443/public/a"), Prompt);
        assert_eq!(http(&p, "GET", "http://a.example.org:8080/api/v1/items/3"), Allow);
        assert_eq!(http(&p, "GET", "https://a.example.org:8080/api/v1/items/3"), Prompt);
        assert_eq!(http(&p, "GET", "http://a.example.org:8080/api/admin/items"), Deny);
        assert_eq!(http(&p, "GET", "http://localhost:3000/"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/me/../you/x"), Prompt);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/%6De/x"), Allow);
        assert_eq!(http(&p, "POST", "https://api.github.com/repos/me%2F..%2F..%2Fyou/x"), Prompt);
        assert_eq!(http(&p, "GET", "http://a.example.org:8080/api/ADMIN/items"), Deny);
    }

    #[test]
    fn transport_selection() {
        let p = parse(
            "allow net db.example.com:5432\n\
             allow net plaintext localhost:5432\n\
             allow net tls plaintext cache.example.com\n\
             deny net evil.com\n\
             allow net tls plaintext *.corp.example\n\
             deny net plaintext *.corp.example\n\
             deny net tls legacy.example\n\
             allow net plaintext legacy.example\n\
             allow net plaintext old.example\n\
             deny net old.example\n\
             allow net tls plaintext tls-first.example\n\
             deny net plaintext tls-first.example\n\
             allow http plaintext GET localhost:8080",
        );
        let net = |host: &str, port: u16, tls: bool| decide(&p, Capability::Net { host: host.into(), port, tls });
        assert_eq!(net("db.example.com", 5432, true), Allow);
        assert_eq!(net("db.example.com", 5432, false), Prompt);
        assert_eq!(net("localhost", 5432, false), Allow);
        assert_eq!(net("localhost", 5432, true), Prompt);
        assert_eq!((net("cache.example.com", 6379, true), net("cache.example.com", 6379, false)), (Allow, Allow));
        assert_eq!((net("evil.com", 443, true), net("evil.com", 443, false)), (Deny, Deny));
        assert_eq!((net("a.corp.example", 80, true), net("a.corp.example", 80, false)), (Allow, Deny));
        assert_eq!((net("legacy.example", 80, true), net("legacy.example", 80, false)), (Deny, Allow));
        assert_eq!((net("old.example", 80, true), net("old.example", 80, false)), (Deny, Deny));
        assert_eq!((net("tls-first.example", 80, true), net("tls-first.example", 80, false)), (Allow, Deny));
        assert_eq!(
            parse("deny net a\ndeny net tls a\nprompt http plaintext a\nprompt http tls plaintext a\nallow net tls a\nallow net tls plaintext a")
                .rules
                .to_string(),
            "deny net a\ndeny net tls a\nprompt http plaintext a\nprompt http a\nallow net a\nallow net tls plaintext a\n",
        );
        assert_eq!(http(&p, "GET", "http://localhost:8080/x"), Allow);
        assert_eq!(http(&p, "GET", "https://localhost:8080/x"), Prompt);
        assert_eq!(http(&p, "POST", "http://localhost:8080/x"), Prompt);
        assert_eq!(http(&p, "PUT", "https://db.example.com:5432/"), Allow);
        assert_eq!(http(&p, "PUT", "http://db.example.com:5432/"), Prompt);
        assert_eq!(http(&p, "PUT", "http://evil.com/"), Deny);
        assert_eq!(parse("allow net tls. plaintext.").rules.to_string(), "allow net tls.\nallow net plaintext.\n");
        assert_eq!(
            errors("allow net localhost tls\nallow net tls\nallow http https://a.com\nallow http http://a.com/x"),
            [
                "1:21: put `tls` and `plaintext` before the hosts",
                "2:11: `allow net` requires a host pattern after transports, e.g. `localhost:8080`",
                "3:12: URL patterns must not include a scheme: remove `https://` and write `tls` before the URL patterns",
                "4:12: URL patterns must not include a scheme: remove `http://` and write `plaintext` before the URL patterns",
            ]
        );
    }

    #[test]
    fn ip_address_with_trailing_dot() {
        let p = parse("allow net tls plaintext *\ndeny net 169.254.169.254.\nallow http 10.0.0.1./x");
        let net = |host: &str| decide(&p, Capability::Net { host: host.into(), port: 80, tls: true });
        assert_eq!((net("169.254.169.254"), net("169.254.169.254.")), (Deny, Deny));
        assert_eq!(p.rules.to_string(), "allow net tls plaintext *\ndeny net 169.254.169.254\nallow http 10.0.0.1/x\n");
        assert_eq!(http(&p, "GET", "https://10.0.0.1/x"), Allow);
    }

    #[test]
    fn capability_and_scheme_hints() {
        assert_eq!(
            errors(
                "allow NET a.com\nallow Read ./x\nallow read:./x\nallow env=HOME\nallow read ./x write ./y\nallow allow read ./x\n\
                 allow net ws://a.com\nallow net tcp://db:5432\nallow http ws://a.com/x\nallow http wss://a.com/x\nallow net gopher://a.com"
            ),
            [
                "1:7: capabilities are lowercase: write `net`",
                "2:7: capabilities are lowercase: write `read`",
                "3:7: put a space after the capability: `read ./x`",
                "4:7: put a space after the capability: `env HOME`",
                "5:16: `write` is a separate capability: put each rule on a separate line or CLI flag",
                "6:7: duplicate rule verb: remove `allow`",
                "7:11: net rules require host patterns, not URLs: remove `ws://` and write `plaintext` before the hosts",
                "8:11: net rules require host patterns, not URLs: remove `tcp://` and write `plaintext` before the hosts",
                "9:12: URL patterns must not include a scheme: remove `ws://` and write `plaintext` before the URL patterns",
                "10:12: URL patterns must not include a scheme: remove `wss://` and write `tls` before the URL patterns",
                "11:11: net rules require host patterns, not URLs: remove `gopher://`",
            ]
        );
    }

    #[test]
    fn url_patterns_reject_encoded_and_backslash_separators() {
        assert_eq!(
            errors("deny http a.com/x%2Fy\ndeny http a.com/x%5cy/z\ndeny http \"a.com/x\\\\y\""),
            [
                "1:11: `x%2Fy` contains an encoded or backslash path separator: write `x/y`",
                "2:11: `x%5cy` contains an encoded or backslash path separator: write `x/y`",
                "3:11: `x\\y` contains an encoded or backslash path separator: write `x/y`",
            ]
        );
    }

    #[test]
    fn invalid_percent_escape_with_encoded_separator() {
        let p = parse("allow http plaintext *\ndeny http a.com/admin/**");
        for path in ["/admin%2Fx", "/admin%2F%FF", "/admin%2F%zz", "/admin%2Fa%", "/%FF/..%2Fadmin/x"] {
            assert_eq!(http(&p, "GET", &format!("http://a.com{path}")), Deny, "{path}");
        }
        assert_eq!(http(&p, "GET", "http://a.com/admin%FF"), Allow);
        assert_eq!(errors("deny http a.com/x%2F%FF"), ["1:11: `x%2F%FF` contains an encoded or backslash path separator: write `x/%FF`"]);
    }

    #[test]
    fn scheme_hint_in_multi_word_rules() {
        assert_eq!(
            errors("allow http GET https://a.com/x\ndeny net tls plaintext http://a.com\ndeny net a.com https://b.com"),
            [
                "1:16: URL patterns must not include a scheme: remove `https://` and write `tls` before the URL patterns",
                "2:24: net rules require host patterns, not URLs: remove `http://` and write `plaintext` before the hosts",
                "3:16: net rules require host patterns, not URLs: remove `https://` and write `tls` before the hosts",
            ]
        );
    }

    #[test]
    fn capability_separator_before_space() {
        assert_eq!(
            errors("allow read: ./data\nallow stdin:"),
            ["1:7: remove `:` after the capability: `read`", "2:7: remove `:` after the capability: `stdin`"]
        );
    }

    #[test]
    fn capability_hint_skips_urls_and_quoted_words() {
        assert_eq!(
            errors("allow read http://a.com/x\nallow \"Read\""),
            [
                "1:12: `http://a.com/x` is a URL, not a path: use an `http` rule",
                "2:7: path `Read` is not anchored: write `./Read` for `/base/Read`, `~/...`, an absolute path, or `**/Read` for that name anywhere",
            ]
        );
    }

    #[test]
    fn custom_capability_rules() {
        let p = parse("allow custom *\ndeny custom db \"drop table\"\nprompt custom db\nallow custom \"*\" literal");
        let custom = |name: &str, detail: &str| decide(&p, Capability::Custom { name: name.into(), detail: detail.into() });
        assert_eq!(custom("door", "front"), Allow);
        assert_eq!(custom("db", "select"), Prompt);
        assert_eq!(custom("db", "drop table"), Deny);
        assert_eq!(custom("*", "literal"), Allow);
        assert_eq!(p.rules.to_string(), "allow custom *\ndeny custom db \"drop table\"\nprompt custom db\nallow custom \"*\" literal\n");
    }

    #[test]
    fn dot_dot_at_root() {
        let p = parse("allow read /\ndeny read /../etc\ndeny /./a/../b");
        assert_eq!(p.rules.to_string(), "allow read /\ndeny read /etc\ndeny /b\n");
        assert_eq!(read(&p, "/etc/passwd"), Deny);
    }

    #[test]
    fn quoted_asterisks_are_literal() {
        let p = parse("allow env \"AWS_*\" A\"*\"B*");
        let env = |name: &str| decide(&p, Capability::Env(name.into()));
        assert_eq!((env("AWS_*"), env("AWS_KEY")), (Allow, Prompt));
        assert_eq!((env("A*BC"), env("AXBC")), (Allow, Prompt));
        let message = "a quoted `*` is not a wildcard, and an address or port cannot contain `*`: remove the quotes";
        assert_eq!(
            errors("allow net \"*\"\nallow net \"*\".a.com\nallow http GET \"*\"/x\nallow listen \"*\":80"),
            [1, 2, 3, 4].map(|line| format!("{line}:{}: {message}", if line == 3 { 16 } else { 11 + (line == 4) as usize * 3 }))
        );
        for name in ["AWS_*", "**", "A*B"] {
            let p = parse(&rule_for(&Capability::Env(name.into()), true).unwrap());
            let env = |name: &str| decide(&p, Capability::Env(name.into()));
            assert_eq!((env(name), env("AWS_SECRET_ACCESS_KEY"), env("*")), (Allow, Prompt, Prompt), "{name}");
        }
    }

    #[test]
    fn double_asterisks_rejected_in_env_names() {
        let message = "cannot use `**` in an environment variable pattern: use `*` to match any characters, e.g. `AWS_*`";
        assert_eq!(errors("deny env AWS_**\nallow env **"), [format!("1:10: {message}"), format!("2:11: {message}")]);
        assert_eq!(decide(&parse("deny env \"AWS_**\""), Capability::Env("AWS_**".into())), Deny);
    }

    #[test]
    fn custom_rule_expansion_limit() {
        let values: Vec<String> = (0..400).map(|i| format!("v{i}")).collect();
        let text = format!("let a = {}\ndeny custom {{a}} {{a}}", values.join(" "));
        assert_eq!(errors(&text), ["2:13: `deny custom` expands to more than 100000 rules"]);
        let text = format!("let a = {}\ndeny custom {{a}} x", values.join(" "));
        assert_eq!(parse(&text).rules.to_string().lines().count(), 400);
    }

    #[test]
    fn custom_rule_set_expansion_and_wildcards() {
        let p = parse("allow custom *\nlet danger = drop truncate\ndeny custom db {danger}\ndeny custom fs *\nallow custom kv \"*\"");
        let custom = |name: &str, detail: &str| decide(&p, Capability::Custom { name: name.into(), detail: detail.into() });
        assert_eq!((custom("db", "drop"), custom("db", "truncate"), custom("db", "select")), (Deny, Deny, Allow));
        assert_eq!((custom("fs", "delete"), custom("fs", "*")), (Deny, Deny));
        assert_eq!((custom("kv", "*"), custom("kv", "get")), (Allow, Allow));
        assert_eq!(
            p.rules.to_string(),
            "allow custom *\ndeny custom db drop\ndeny custom db truncate\ndeny custom fs\nallow custom kv \"*\"\n"
        );
        assert_eq!(
            errors("deny custom db drop*\ndeny custom a*b"),
            [
                "1:16: custom names and details must be exact or `*`: quote literal asterisks as `\"drop*\"`",
                "2:13: custom names and details must be exact or `*`: quote literal asterisks as `\"a*b\"`",
            ]
        );
    }

    #[test]
    fn transport_case_insensitivity_and_invalid_transport_hints() {
        let p = parse("allow http tls plaintext *\ndeny http PLAINTEXT api.example.com\nlet t = plaintext\ndeny net {t} Tls evil.com");
        assert_eq!(http(&p, "GET", "http://api.example.com/"), Deny);
        assert_eq!(decide(&p, Capability::Net { host: "evil.com".into(), port: 80, tls: false }), Deny);
        assert_eq!(p.rules.to_string(), "allow http tls plaintext *\ndeny http plaintext api.example.com\ndeny net evil.com\n");
        assert_eq!(
            errors("deny http http evil.com\ndeny net TCP evil.com\nallow http GET https a.com\nallow net ssl a.com"),
            [
                "1:11: `http` is not a transport: write `plaintext` for unencrypted connections such as `http://`; write `http.` for hostname `http`",
                "2:10: `TCP` is not a transport: write `plaintext` for unencrypted connections, or `tls plaintext` for both; write `tcp.` for hostname `tcp`",
                "3:16: `https` is not a transport: write `tls` for encrypted connections; write `https.` for hostname `https`",
                "4:11: `ssl` is not a transport: write `tls` for encrypted connections; write `ssl.` for hostname `ssl`",
            ]
        );
        assert_eq!(
            errors(
                "allow net udp localhost:53
allow http ws a.com
allow http GET a.com tls"
            ),
            [
                "1:11: `udp` is not a transport: only TCP connections are supported; write `udp.` for hostname `udp`",
                "2:12: `ws` is not a transport: write `plaintext` for unencrypted connections such as `ws://`; write `ws.` for hostname `ws`",
                "3:22: put `tls` and `plaintext` before the URL patterns",
            ]
        );
        assert_eq!(parse("allow net udp. wss.").rules.to_string(), "allow net udp.\nallow net wss.\n");
        let p = parse("allow net \"tls\" http. a.com https\nallow http \"plaintext\"/x");
        assert_eq!(p.rules.to_string(), "allow net tls.\nallow net http.\nallow net a.com\nallow net https.\nallow http plaintext./x\n");
        assert_eq!(parse(&p.rules.to_string()).rules.to_string(), p.rules.to_string());
        assert_eq!(http(&parse("allow http \"GET\""), "POST", "https://get/x"), Allow);
    }

    #[test]
    fn displayed_rules_round_trip() {
        for text in [
            "allow custom slack \"#general\"",
            "deny env \"#X\" \"#a\"*",
            "deny http plaintext 10.0.0.1/32/8 10.0.0.1/32/8:1 [::]/128/8",
            "allow http post.:8080 delete. \"get\"/x",
        ] {
            let shown = parse(text).rules.to_string();
            let again = PolicyFile::parse(&shown, Path::new("/"), None).map(|f| f.rules.to_string());
            assert_eq!(again.as_deref(), Ok(shown.as_str()), "{text}");
        }
        let p = parse(&parse("allow custom slack \"#general\"").rules.to_string());
        assert_eq!(decide(&p, Capability::Custom { name: "slack".into(), detail: "#random".into() }), Prompt);
    }

    #[test]
    fn globstar_matches_all_paths() {
        let p = parse("allow import **\ndeny **/.env\nallow read /data");
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/anywhere/x.sand"))), Allow);
        assert_eq!(read(&p, "/data/.env"), Deny);
        assert_eq!(read(&p, "/other/sub/.env"), Deny);
        assert_eq!(read(&p, "/data/x"), Allow);
        assert_eq!(read(&p, "/other/x"), Prompt);
        assert_eq!(p.rules.to_string(), "allow import **\ndeny **/.env\nallow read /data\n");
    }

    #[test]
    fn net_rules_apply_to_http_requests() {
        let p = parse("allow net api.example.com\ndeny http DELETE api.example.com/admin\nallow http GET other.com");
        assert_eq!(http(&p, "PATCH", "https://api.example.com/x"), Allow);
        assert_eq!(http(&p, "DELETE", "https://api.example.com/admin/x"), Deny);
        assert_eq!(http(&p, "GET", "https://other.com/x"), Allow);
        assert_eq!(decide(&p, Capability::Net { host: "other.com".into(), port: 443, tls: true }), Prompt);
        let p = parse("allow http tls plaintext *\ndeny net 10.0.0.0/8");
        assert_eq!(http(&p, "GET", "http://10.1.2.3/x"), Deny);
        assert_eq!(http(&p, "GET", "https://[::ffff:10.1.2.3]/x"), Deny);
        assert_eq!(http(&p, "GET", "http://8.8.8.8/x"), Allow);
        let p = parse("allow net localhost:8080");
        assert_eq!(http(&p, "GET", "https://localhost:8080/"), Allow);
        let p = parse("allow http *");
        assert_eq!(http(&p, "GET", "https://127.0.0.1/"), Allow);
        assert_eq!(http(&p, "GET", "http://127.0.0.1/"), Prompt);
    }

    #[test]
    fn http_rules_accept_single_ip_addresses() {
        let p = parse(
            "allow http plaintext *\ndeny http plaintext 10.0.0.1/x\ndeny http plaintext GET [fd00::1]/admin\ndeny http plaintext [2001:db8::5]:8080/x",
        );
        assert_eq!(http(&p, "GET", "http://10.0.0.1/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[::ffff:10.0.0.1]/x"), Deny);
        assert_eq!(http(&p, "GET", "http://10.0.0.2/x"), Allow);
        assert_eq!(http(&p, "GET", "http://[fd00::1]/admin/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[fd00::1]/x"), Allow);
        assert_eq!(http(&p, "GET", "http://[2001:db8::5]:8080/x"), Deny);
        assert_eq!(http(&p, "GET", "http://[2001:db8::5]/x"), Allow);
        let p = parse("allow http plaintext *\ndeny http plaintext [::1]/admin\ndeny http plaintext 10.0.0.1:80/8");
        assert_eq!(http(&p, "GET", "http://[::1]/admin"), Deny);
        assert_eq!(http(&p, "GET", "http://[::1]/x"), Allow);
        assert_eq!(http(&p, "GET", "http://10.0.0.1/8"), Deny);
        assert_eq!(http(&p, "GET", "http://10.0.0.1/x"), Allow);
        let p = parse("allow http plaintext *\ndeny http plaintext 10.0.0.1/32/8\ndeny http plaintext [::]/128/8");
        assert_eq!(http(&p, "GET", "http://10.0.0.1/8"), Deny);
        assert_eq!(http(&p, "GET", "http://10.0.0.2/8"), Allow);
        assert_eq!(http(&p, "GET", "http://[::]/8"), Deny);
        assert_eq!(http(&p, "GET", "http://[::1]/8"), Allow);
        assert_eq!(
            errors(
                "deny http 10.0.0.0/8\ndeny http [fc00::/7]:443/x\ndeny http 192.168.1.1/24/x\ndeny http [::1]/64/x\nallow net 10.0.0.1/8\nallow net 10.0.0.0/+8"
            ),
            [
                "1:11: `10.0.0.0/8` is a CIDR range, and `http` rules accept only single IP addresses: use `net 10.0.0.0/8` for the range, or `10.0.0.0/32/8` for the path `/8` on `10.0.0.0`",
                "2:11: `[fc00::/7]:443` is a CIDR range, and `http` rules accept only single IP addresses: use `net [fc00::/7]:443` for the range",
                "3:11: `192.168.1.1/24` is a CIDR range, and `http` rules accept only single IP addresses: use `net 192.168.1.0/24` for the range, or `192.168.1.1/32/24` for the path `/24` on `192.168.1.1`",
                "4:11: `[::1]/64` is a CIDR range, and `http` rules accept only single IP addresses: use `net ::/64` for the range, or `[::1]/128/64` for the path `/64` on `::1`",
                "5:11: `10.0.0.1/8` has host bits set after the prefix length: write `10.0.0.0/8`",
                "6:11: invalid host pattern `10.0.0.0/+8`: expected a host, host:port, *.domain, *, an IP address, or a CIDR range",
            ]
        );
    }

    #[test]
    fn hash_in_middle_of_word_is_not_a_comment() {
        let p = parse("deny read ./data/x#1   # the comment\nallow read ./data");
        assert_eq!(read(&p, "/base/data/x#1"), Deny);
        assert_eq!(read(&p, "/base/data/x"), Allow);
    }

    #[test]
    fn deny_rules_for_each_capability() {
        let p = parse(
            "allow env *\nallow stdin\nallow listen *\nallow import /\nallow custom *\n\
             deny env SECRET AWS_*\ndeny stdin\ndeny listen *:22\ndeny import /base/vendor\ndeny custom db write",
        );
        assert_eq!(decide(&p, Capability::Env("SECRET".into())), Deny);
        assert_eq!(decide(&p, Capability::Env("AWS_KEY".into())), Deny);
        assert_eq!(decide(&p, Capability::Env("OTHER".into())), Allow);
        assert_eq!(decide(&p, Capability::Env("*".into())), Deny);
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
    fn imports_respect_import_rules_and_read_denies() {
        let p = parse("allow import **\ndeny read ./private");
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/x.sand"))), Allow);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/private/x.sand"))), Deny);
        let p = parse("allow import ./lib\ndeny read ./lib/private\nallow import ./lib/private/ok.sand");
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/x.sand"))), Allow);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/other.sand"))), Prompt);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/private/x.sand"))), Deny);
        assert_eq!(decide(&p, Capability::Import(PathBuf::from("/base/lib/private/ok.sand"))), Allow);
    }

    #[test]
    fn rule_for_round_trip() {
        let caps = [
            Capability::FsRead(PathBuf::from("/data/a file, \"quoted\".txt")),
            Capability::FsWrite(PathBuf::from("/out/#hash")),
            Capability::FsRead(PathBuf::from("/data/*/{x}")),
            Capability::Import(PathBuf::from("/lib/util.sand")),
            Capability::Net { host: "api.example.com".into(), port: 443, tls: true },
            Capability::Net { host: "::1".into(), port: 8080, tls: true },
            Capability::Net { host: "10.0.0.5".into(), port: 80, tls: true },
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
            Capability::Stdout,
            Capability::Stderr,
            Capability::Env("#X".into()),
            Capability::Custom { name: "#chan".into(), detail: "#general".into() },
            Capability::Net { host: "delete".into(), port: 443, tls: false },
        ];
        for cap in &caps {
            for allow in [true, false] {
                let rule = rule_for(cap, allow).unwrap_or_else(|| panic!("expected rule for {cap:?}"));
                let p = parse(&rule);
                let want = if allow { Allow } else { Deny };
                assert_eq!(decide(&p, cap.clone()), want, "{rule}");
            }
        }
        assert_eq!(rule_for(&caps[0], true).unwrap(), "allow read \"/data/a file, \\\"quoted\\\".txt\"");
        assert_eq!(rule_for(&caps[2], true).unwrap(), "allow read \"/data/*/{x}\"");
        assert_eq!(rule_for(&caps[7], true).unwrap(), "allow http GET api.example.com:443/a%20b/%2A/%25/x%3Fy");
        assert_eq!(rule_for(&caps[8], false).unwrap(), "deny http plaintext POST [::1]:8080");
        let plain = Capability::Net { host: "localhost".into(), port: 5432, tls: false };
        assert_eq!(rule_for(&plain, true).unwrap(), "allow net plaintext localhost:5432");
        assert_eq!(rule_for(&Capability::Stdout, true).as_deref(), Some("allow stdout"));
        assert_eq!(rule_for(&Capability::Stderr, false).as_deref(), Some("deny stderr"));
        assert_eq!(rule_for(&Capability::FsRead(PathBuf::from("/tmp/a\nb")), true), None);
        assert_eq!(rule_for(&Capability::Env("A\rB".into()), true), None);
        let p = parse(&rule_for(caps.last().unwrap(), true).unwrap());
        assert_eq!(decide(&p, Capability::NetListen("[::1]:22".parse().unwrap())), Prompt);
        let request = |method: &str| Capability::Http {
            method: method.into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            port: 443,
            path: "/x".into(),
        };
        for method in ["TLS", "tls", "PLAINTEXT", "Plaintext", "HTTP", "HTTPS", "TCP", "SSL"] {
            assert_eq!(rule_for(&request(method), true), None, "{method}");
        }
        let p = parse(&rule_for(&request("PURGE"), true).unwrap());
        assert_eq!((decide(&p, request("PURGE")), decide(&p, request("DELETE"))), (Allow, Prompt));
    }

    fn word() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::strategy::Strategy;
        let words = [
            "a",
            "B",
            "\"a b\"",
            "\"*\"",
            "*",
            "x*",
            "\"x\"*",
            "\"{x}\"",
            "\"tls\"",
            "tls.",
            "\"plaintext\"",
            "\"a,b\"",
            "\"\\\"q\"",
            "é",
            "\"\"",
            "\"%2F\"",
            "%41",
            "\"..\"",
            "\"#\"",
            "\"#a\"",
            "\"#\"*",
            "a#",
            "\"http\"",
            "\"GET\"",
            "8",
            "32",
        ];
        proptest::sample::select(words.to_vec()).prop_map(str::to_string)
    }

    fn line() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;
        let verb = prop::sample::select(vec!["allow", "deny", "prompt"]);
        let path = || prop::collection::vec(word(), 0..4).prop_map(|w| w.iter().map(|n| format!("/{n}")).collect::<String>());
        let root = prop::sample::select(vec!["/r", "./", "~", "**", "\"/my dir\""]);
        let host = prop::sample::select(vec![
            "a.com",
            "*.a.com",
            "*",
            "10.0.0.1",
            "[::1]",
            "[::]",
            "10.0.0.1:80",
            "[::1]:8080",
            "tls.",
            "plaintext.:80",
            "10.0.0.0/8",
            "delete.",
            "http.",
            "\"get\"",
            "10.0.0.1/32",
            "[::1]/128",
        ]);
        let transport = prop::sample::select(vec!["", "tls ", "plaintext ", "tls plaintext ", "PLAINTEXT "]);
        let methods = prop::sample::select(vec!["", "GET ", "post ", "M-SEARCH "]);
        let access = prop::sample::select(vec!["", "read ", "write ", "import "]);
        prop_oneof![
            (verb.clone(), access, root, path()).prop_map(|(v, a, r, p)| format!("{v} {a}{r}{p}")),
            (verb.clone(), transport.clone(), methods, host.clone(), path()).prop_map(|(v, t, m, h, p)| format!("{v} http {t}{m}{h}{p}")),
            (verb.clone(), transport, host).prop_map(|(v, t, h)| format!("{v} net {t}{h}")),
            (verb.clone(), word()).prop_map(|(v, w)| format!("{v} env {w}")),
            (verb.clone(), word(), prop::option::of(word())).prop_map(|(v, n, d)| format!("{v} custom {n} {}", d.unwrap_or_default())),
            (verb, prop::sample::select(vec!["*", "*:80", "localhost", "[::1]:0", "::1", "127.0.0.1:*"]))
                .prop_map(|(v, l)| format!("{v} listen {l}")),
        ]
    }

    proptest::proptest! {
        #[test]
        fn displayed_policies_round_trip(line in line()) {
            let Ok(file) = PolicyFile::parse(&line, Path::new("/base"), Some(Path::new("/home/me"))) else { return Ok(()) };
            let shown = file.rules.to_string();
            let again = PolicyFile::parse(&shown, Path::new("/elsewhere"), Some(Path::new("/other")));
            proptest::prop_assert!(again.is_ok(), "{:?} -> {:?} -> {:?}", line, shown, again.err());
            proptest::prop_assert_eq!(&again.unwrap().rules.to_string(), &shown, "{:?}", line);
        }

        #[test]
        fn rule_for_http_ip_address_matches_single_ip(
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
                proptest::prop_assert!(rule.is_some(), "expected rule for {}", cap(ip));
                let p = parse(&rule.unwrap());
                proptest::prop_assert_eq!(decide(&p, cap(ip)), if allow { Allow } else { Deny }, "{}", cap(ip));
                proptest::prop_assert_eq!(decide(&p, cap(other)), Prompt, "{}: {}", rule_for(&cap(ip), allow).unwrap(), cap(other));
            }
        }
    }

    #[test]
    fn wildcard_net_rule_and_private_net_error() {
        let p = parse("allow net *");
        for host in ["10.1.2.3", "169.254.169.254", "localhost", "app.localhost", "127.0.0.1", "::1", "example.com"] {
            assert_eq!(decide(&p, Capability::Net { host: host.into(), port: 80, tls: true }), Allow, "{host}");
        }
        assert_eq!(decide(&p, Capability::Net { host: "example.com".into(), port: 80, tls: false }), Prompt);
        assert_eq!(
            errors("allow private-net\ndeny private-net"),
            [
                "1:7: `private-net` is not a capability: specify internal destinations in `net` rules, e.g. `allow net localhost:8080 10.0.0.0/8`",
                "2:6: `private-net` is not a capability: specify internal destinations in `net` rules, e.g. `allow net localhost:8080 10.0.0.0/8`",
            ]
        );
    }

    #[test]
    fn prompt_rule_precedence() {
        let p = parse(
            "allow ./\nprompt write ./out\nprompt http POST api.example.com\nallow http api.example.com\nprompt env AWS_*\nallow env HOME",
        );
        assert_eq!(read(&p, "/base/out/x"), Allow);
        assert_eq!(write(&p, "/base/out/x"), Prompt);
        assert_eq!(write(&p, "/base/OUT/x"), Prompt);
        assert_eq!(write(&p, "/base/src/x"), Allow);
        assert_eq!(http(&p, "POST", "https://api.example.com/x"), Prompt);
        assert_eq!(http(&p, "GET", "https://api.example.com/x"), Allow);
        assert_eq!(decide(&p, Capability::Env("AWS_KEY".into())), Prompt);
        assert_eq!(decide(&p, Capability::Env("*".into())), Prompt);
        let p = parse("allow http a.com b.com\nprompt http a.com/x/y");
        assert_eq!(http(&p, "GET", "https://a.com/x%2Fy"), Prompt);
        assert_eq!(http(&p, "GET", "https://a.com/x%2Fz"), Allow);
        assert_eq!(http(&p, "GET", "https://b.com/x%2Fy"), Allow);
        let p = parse("allow http a.com/x\nprompt http a.com/x/y");
        assert_eq!(http(&p, "GET", "https://a.com/x/z"), Allow);
        assert_eq!(http(&p, "GET", "https://a.com/x%2Fz"), Prompt);
        let p = parse("prompt http a.com\ndeny http a.com/x/y");
        assert_eq!(http(&p, "GET", "https://a.com/x%2Fy"), Deny);
        let p = parse("prompt read ./data\nallow read ./data\ndeny env X\nprompt env Y");
        assert_eq!(read(&p, "/base/data/x"), Allow);
        assert_eq!(decide(&p, Capability::Env("*".into())), Deny);
    }

    #[test]
    fn rule_display_formatting() {
        let text = "allow read /data/*.txt \"/my data\"/x\ndeny ./out/**/secret\nprompt import /lib\nallow net *.example.com:443 [::1]:80 10.0.0.0/8\n\
                    allow net plaintext localhost:5432\nallow net tls plaintext db.internal\n\
                    deny http GET POST api.example.com/v1/*\ndeny http plaintext [::1]:8080/a%20b\nallow listen *:8080 [::1]:0 localhost\n\
                    prompt env AWS_* HOME\nallow stdin\ndeny timezone\nallow custom db \"drop table\"";
        let p = parse(text);
        let shown = p.rules.to_string();
        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(
            lines,
            [
                "allow read /data/*.txt",
                "allow read \"/my data/x\"",
                "deny /base/out/**/secret",
                "prompt import /lib",
                "allow net *.example.com:443",
                "allow net [::1]:80",
                "allow net 10.0.0.0/8",
                "allow net plaintext localhost:5432",
                "allow net tls plaintext db.internal",
                "deny http GET POST api.example.com/v1/*",
                "deny http plaintext [::1]:8080/a%20b",
                "allow listen *:8080",
                "allow listen [::1]:0",
                "allow listen localhost",
                "prompt env AWS_*",
                "prompt env HOME",
                "allow stdin",
                "deny timezone",
                "allow custom db \"drop table\"",
            ]
        );
        let again = parse(&lines.join("\n"));
        assert_eq!(again.rules.to_string(), shown);
    }

    #[test]
    fn error_locations_and_messages() {
        assert_eq!(
            errors(
                "alow read /x\nallow\nallow net not a pattern!\nlimit memory lots\nlimit\ndefault deny\nallow stdout\ndeny custom\nallow read \"open\n\
                 allow raed /x\nallow read data\nallow read ~user\nallow http GET\nallow http a.com GET\nallow http ftp://x\nallow read /a/**/../b\n\
                 allow read /a/b**\nallow http a.com/x?y"
            ),
            [
                "1:1: unknown directive `alow`: expected `allow`, `deny`, `prompt`, `limit`, or `let`",
                "2:1: expected what to allow: a path, or read, write, import, net, http, listen, env, stdin, stdout, stderr, timezone, or custom",
                "3:17: invalid host pattern `pattern!`: expected a host, host:port, *.domain, *, an IP address, or a CIDR range",
                "4:7: invalid size `lots`: use e.g. 4096, 64K, 256M, or 1G",
                "5:1: expected `limit NAME VALUE`, e.g. `limit memory 64M`",
                "6:1: unknown directive `default`: unmatched requests are prompted or denied; write rules for what to allow or deny",
                "8:6: expected `deny custom NAME [DETAIL]`",
                "9:12: unterminated quotes",
                "10:7: unknown capability `raed`: expected a path, or read, write, import, net, http, listen, env, stdin, stdout, stderr, timezone, or custom",
                "11:12: path `data` is not anchored: write `./data` for `/base/data`, `~/...`, an absolute path, or `**/data` for that name anywhere",
                "12:12: path `~user` is not anchored: write `./~user` for `/base/~user`, `~/...`, an absolute path, or `**/~user` for that name anywhere",
                "13:12: `allow http` requires a URL pattern after methods, e.g. `api.example.com/v1`",
                "14:18: put HTTP methods before the URL patterns",
                "15:12: URL patterns must not include a scheme: remove `ftp://`",
                "16:12: `..` cannot follow a wildcard",
                "17:12: `**` must be a whole path segment",
                "18:12: URL patterns must not include a query string: remove `?...`",
            ]
        );
        assert_eq!(
            errors("prompt"),
            [
                "1:1: expected what to prompt for: a path, or read, write, import, net, http, listen, env, stdin, stdout, stderr, timezone, or custom"
            ]
        );
        assert_eq!(errors("allow stdin now"), ["1:13: `allow stdin` does not accept arguments"]);
        assert_eq!(errors("deny http plaintext"), ["1:11: `deny http` requires a URL pattern after transports, e.g. `api.example.com/v1`"]);
        assert_eq!(
            errors("allow net https://api.example.com/v1\nallow net http://localhost:8080"),
            [
                "1:11: net rules require host patterns, not URLs: remove `https://` and write `tls` before the hosts",
                "2:11: net rules require host patterns, not URLs: remove `http://` and write `plaintext` before the hosts",
            ]
        );
        assert_eq!(errors("allow read ~/x"), Vec::<String>::new());
        assert_eq!(
            PolicyFile::parse("allow read ~/x", Path::new("/"), None).unwrap_err()[0].message,
            "cannot resolve `~` without a home directory"
        );
        assert_eq!(errors("allow read a}"), ["1:13: unmatched `}`: quote `\"}\"` for a literal brace"]);
        assert_eq!(errors("allow read {a b}"), ["1:12: invalid set reference after `{`: use `{NAME}` or quote `\"{\"`"]);
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("2k"), Ok(2048));
        assert_eq!(parse_size("1G"), Ok(1 << 30));
        assert!(parse_size("-1").is_err());
        assert_eq!(parse_size_u64("8G"), Ok(8 << 30));
        assert_eq!(parse_size_u64("99999999999G"), Ok(u64::MAX));
        assert_eq!(parse_size("8589934592").map(|n| n as u64), Ok((8u64 << 30).min(usize::MAX as u64)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("1e300h").is_err());
    }
}
