//! The policy file formatter.

use super::PolicyError;
use super::file::{Line, is_method, lex, transport};

/// Formats a policy file: words separated by single spaces and quoted only
/// where needed, transports in lower case, HTTP methods in upper case,
/// trailing comments of
/// consecutive lines aligned, and no runs of blank lines. Fails only on
/// text that does not split into words (an unterminated quote).
pub fn format_policy(text: &str) -> Result<String, Vec<PolicyError>> {
    let (lines, errors) = lex(text);
    if !errors.is_empty() {
        return Err(errors);
    }
    let code: Vec<String> = lines.iter().map(code).collect();
    let mut out = String::new();
    let mut blank = true;
    let mut i = 0;
    while i < lines.len() {
        if code[i].is_empty() && lines[i].comment.is_none() {
            blank = true;
            i += 1;
            continue;
        }
        if blank && !out.is_empty() {
            out.push('\n');
        }
        blank = false;
        // Trailing comments of consecutive lines start in one column.
        let commented = |j: usize| !code[j].is_empty() && lines[j].comment.is_some();
        let end = if commented(i) { (i..lines.len()).find(|&j| !commented(j)).unwrap_or(lines.len()) } else { i + 1 };
        let width = (i..end).map(|j| code[j].chars().count()).max().unwrap_or(0);
        for j in i..end {
            let line = match &lines[j].comment {
                Some(c) if code[j].is_empty() => c.clone(),
                Some(c) => format!("{}{}  {c}", code[j], " ".repeat(width - code[j].chars().count())),
                None => code[j].clone(),
            };
            out.push_str(&line);
            out.push('\n');
        }
        i = end;
    }
    Ok(out)
}

/// A line's words, formatted.
fn code(line: &Line) -> String {
    let connection = line.words.len() > 2
        && matches!(line.words[0].plain().as_deref(), Some("allow" | "deny" | "prompt"))
        && matches!(line.words[1].plain().as_deref(), Some("net" | "http"));
    let http = connection && line.words[1].plain().as_deref() == Some("http");
    // Transports, then (in http rules) methods, as the parser reads them.
    let mut keywords = connection;
    let mut out = Vec::new();
    for (i, w) in line.words.iter().enumerate() {
        let mut text = w.render();
        if i >= 2 && keywords {
            match w.bare() {
                Some(t) if transport(&t).is_some() => text = t.to_ascii_lowercase(),
                Some(m) if http && is_method(&m) => text = m.to_ascii_uppercase(),
                _ => keywords = false,
            }
        }
        out.push(text);
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Capability, PathSpace, PolicyFile};
    use proptest::prelude::*;
    use std::path::{Path, PathBuf};

    const WORDS: &[&str] = &[
        "allow",
        "deny",
        "read",
        "write",
        "import",
        "http",
        "net",
        "env",
        "let",
        "x",
        "=",
        "GET",
        "post",
        "./a",
        "/b/*",
        "~/c/**",
        "\"q w\"",
        "{x}",
        "*.example.com",
        "api.example.com/v1/*",
        "https://api.example.com",
        "#c",
        "\"",
        "{",
        "}",
        "default",
        "prompt",
        "limit",
        "memory",
        "64M",
        "*",
        "**",
        "a\"b\"",
        "/d/\"e f\"/*",
        "\"#\"",
        "\"*\"",
        "/b/x",
        "stdin",
        "tls",
        "plaintext",
        "PLAINTEXT",
        "custom",
        "listen",
        "\"get\"",
        "\"tls\"",
        "http.",
        "localhost:8080",
    ];

    /// A policy's rules as text and what it decides on some requests, or
    /// its errors (formatting may move them to other lines).
    fn meaning(text: &str) -> Result<(String, Vec<crate::policy::Decision>), Vec<String>> {
        let file = PolicyFile::parse(text, Path::new("/base"), Some(Path::new("/home/me")))
            .map_err(|e| e.into_iter().map(|e| e.message).collect::<Vec<_>>())?;
        let policy = file.rules;
        let path = |p: &str| PathBuf::from(p);
        let http = |m: &str, path: &str| Capability::Http {
            method: m.into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            port: 443,
            path: path.into(),
        };
        let caps = [
            Capability::FsRead(path("/base/a/1")),
            Capability::FsWrite(path("/b/x")),
            Capability::FsRead(path("/home/me/c/d/e")),
            Capability::FsRead(path("/d/e f/g")),
            Capability::FsRead(path("/b/*")),
            Capability::Import(path("/base/a/m.sand")),
            Capability::Env("x".into()),
            Capability::Stdin,
            Capability::Net { host: "w.example.com".into(), port: 443, tls: true },
            http("GET", "/v1/z"),
            http("POST", "/"),
        ];
        Ok((policy.to_string(), caps.iter().map(|c| policy.check_in(c, PathSpace::Memory)).collect()))
    }

    /// A word of raw and quoted pieces, some of them parts of keywords.
    fn split_word() -> impl Strategy<Value = String> {
        let pieces = [
            "pl", "aintext", "\"pl\"", "\"aintext\"", "t", "ls", "\"t\"", "\"ls\"", "GE", "T", "\"GE\"", "\"T\"", "\"\"", "{x}", "{y}", "*",
            "\"*\"", "a.com", "\".com\"", "#", "\"#\"", ":80", "/8", "10.0.0.1", "\"10.0.0.1\"", "/p",
        ];
        let word = prop::collection::vec(prop::sample::select(pieces.to_vec()), 1..4).prop_map(|p| p.concat());
        word.prop_filter("not a comment", |w| !w.starts_with('#'))
    }

    fn set_value() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec!["pl", "\"pl\"", "aintext", "\"aintext\"", "T", "\"T\"", "tls", "\"tls\"", "GET", "\"GET\"", "a.com", "*", "\"*\""])
    }

    proptest! {
        /// Formatting keeps the rules of policies whose words join quoted
        /// text, raw text and sets.
        #[test]
        fn formatting_keeps_the_rules_of_split_quotes_and_sets(
            x in set_value(),
            y in set_value(),
            verb in prop::sample::select(vec!["allow", "deny"]),
            kind in prop::sample::select(vec!["net", "http", "env", "custom"]),
            words in prop::collection::vec(split_word(), 1..4),
        ) {
            let text = format!("let x = {x}\nlet y = {y}\n{verb} {kind} {}\n", words.join(" "));
            let Ok(before) = PolicyFile::parse(&text, Path::new("/base"), None) else { return Ok(()) };
            let formatted = format_policy(&text).unwrap();
            let after = PolicyFile::parse(&formatted, Path::new("/base"), None);
            prop_assert!(after.is_ok(), "{:?} -> {:?}", text, formatted);
            prop_assert_eq!(before.rules.to_string(), after.unwrap().rules.to_string(), "{:?} -> {:?}", text, formatted);
        }

        #[test]
        fn formatting_is_idempotent_and_keeps_the_meaning(
            lines in proptest::collection::vec(proptest::collection::vec((0..WORDS.len(), 0..3usize), 0..7), 0..6)
        ) {
            let text: String = lines
                .iter()
                .map(|line| line.iter().map(|&(w, sep)| format!("{}{}", WORDS[w], [" ", ",", "  "][sep])).collect::<String>() + "\n")
                .collect();
            if let Ok(formatted) = format_policy(&text) {
                prop_assert_eq!(&format_policy(&formatted).unwrap(), &formatted);
                prop_assert_eq!(meaning(&text), meaning(&formatted), "{:?} -> {:?}", text, formatted);
            }
        }
    }

    #[test]
    fn formats_words_quotes_methods_comments_and_blank_lines() {
        let src = "\n\n# Policy\nallow  read  \"/data\",\"/my data\"/**   # mine\n\
                   deny ./x # x\n\n\n\nallow http get, head  api.example.com\nlet  hosts =  a.com,b.com\n\
                   allow net {hosts}\n  limit memory 64M  \nprompt  http post   api.example.com\n\n";
        let want = "# Policy\nallow read \"/data\" \"/my data\"/**  # mine\ndeny ./x                          # x\n\n\
                    allow http GET HEAD api.example.com\nlet hosts = a.com b.com\nallow net {hosts}\nlimit memory 64M\n\
                    prompt http POST api.example.com\n";
        assert_eq!(format_policy(src).unwrap(), want);
        assert_eq!(format_policy(want).unwrap(), want);
        assert_eq!(format_policy("").unwrap(), "");
        assert_eq!(format_policy("allow read \"*\" \"#x\" \"{a}\" \"a\\\"b\"").unwrap(), "allow read \"*\" \"#x\" \"{a}\" \"a\\\"b\"\n");
        assert!(format_policy("allow read \"open").is_err());
        let src = "allow http PLAINTEXT get a.com\nallow http \"get\" \"tls\"/x\nallow net TLS Plaintext a.com\n";
        let want = "allow http plaintext GET a.com\nallow http \"get\" \"tls\"/x\nallow net tls plaintext a.com\n";
        // Quotes stay where a set reference joins them with other text, and on set values.
        for src in ["let x = aintext\nallow net \"pl\"{x} a.com\n", "let t = T\ndeny http \"GE\"{t} a.com\n", "let x = \"pl\"\nallow net {x}aintext a.com\n"] {
            assert_eq!(format_policy(src).unwrap(), src);
        }
        assert_eq!(format_policy(src).unwrap(), want);
    }
}
