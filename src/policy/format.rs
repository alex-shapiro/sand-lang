//! The policy file formatter.

use super::PolicyError;
use super::file::{Line, is_method, lex};

/// Formats a policy file: words separated by single spaces and quoted only
/// where needed, HTTP methods in upper case, trailing comments of
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
    let http = line.words.len() > 2
        && matches!(line.words[0].plain().as_deref(), Some("allow" | "deny" | "prompt"))
        && line.words[1].plain().as_deref() == Some("http");
    let mut methods = http;
    let mut out = Vec::new();
    for (i, w) in line.words.iter().enumerate() {
        let mut text = w.render();
        if i >= 2 && methods {
            match w.plain().filter(|p| is_method(p)) {
                Some(m) => text = m.to_ascii_uppercase(),
                None => methods = false,
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
    ];

    /// What a policy decides on some requests, or its errors (formatting
    /// may move them to other lines).
    fn meaning(text: &str) -> Result<Vec<crate::policy::Decision>, Vec<String>> {
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
        Ok(caps.iter().map(|c| policy.check_in(c, PathSpace::Memory)).collect())
    }

    proptest! {
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
        let want = "# Policy\nallow read /data \"/my data\"/**  # mine\ndeny ./x                        # x\n\n\
                    allow http GET HEAD api.example.com\nlet hosts = a.com b.com\nallow net {hosts}\nlimit memory 64M\n\
                    prompt http POST api.example.com\n";
        assert_eq!(format_policy(src).unwrap(), want);
        assert_eq!(format_policy(want).unwrap(), want);
        assert_eq!(format_policy("").unwrap(), "");
        assert_eq!(format_policy("allow read \"*\" \"#x\" \"{a}\" \"a\\\"b\"").unwrap(), "allow read \"*\" \"#x\" \"{a}\" \"a\\\"b\"\n");
        assert!(format_policy("allow read \"open").is_err());
    }
}
