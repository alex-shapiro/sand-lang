//! Syntax highlighting and HTML escaping for Sand, policy files, Rust, and JavaScript.

use sandlang::syntax::highlight::{Highlight, highlight};

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn sand(source: &str) -> String {
    sand_linked(source, &|_| None)
}

/// Highlights Sand source code, wrapping type names for which `link` returns
/// `Some(url)` in `<a>` elements.
pub fn sand_linked(source: &str, link: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut at = 0;
    let plain = |out: &mut String, text: &str| {
        let mut rest = text;
        while let Some(start) = rest.find(|c: char| c.is_ascii_alphabetic()) {
            out.push_str(&escape(&rest[..start]));
            let word_len = rest[start..].find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(rest.len() - start);
            let word = &rest[start..start + word_len];
            match link(word) {
                Some(url) => out.push_str(&format!(r#"<a href="{url}">{word}</a>"#)),
                None => out.push_str(word),
            }
            rest = &rest[start + word_len..];
        }
        out.push_str(&escape(rest));
    };
    for (lo, hi, kind) in highlight(source) {
        plain(&mut out, &source[at..lo]);
        let text = &source[lo..hi];
        match (kind, link(text)) {
            (Highlight::Type, Some(url)) => out.push_str(&format!(r#"<a class="hl-type" href="{url}">{}</a>"#, escape(text))),
            _ => out.push_str(&format!(r#"<span class="hl-{}">{}</span>"#, kind.name(), escape(text))),
        }
        at = hi;
    }
    plain(&mut out, &source[at..]);
    out
}

pub fn policy(source: &str) -> String {
    const STATEMENTS: &[&str] = &["allow", "deny", "prompt", "default", "limit", "let"];
    let mut out = String::new();
    for (i, line) in source.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let (code, comment) = match line.find('#') {
            Some(at) if !line[..at].contains('"') => line.split_at(at),
            _ => (line, ""),
        };
        let mut first = true;
        for piece in split_keep_spaces(code) {
            if piece.trim().is_empty() {
                out.push_str(piece);
                continue;
            }
            let class = if first && STATEMENTS.contains(&piece) {
                Some("keyword")
            } else if piece.starts_with('"') {
                Some("string")
            } else if piece.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                Some("number")
            } else if piece.starts_with('{') {
                Some("interpolation")
            } else {
                None
            };
            first = false;
            match class {
                Some(c) => out.push_str(&format!(r#"<span class="hl-{c}">{}</span>"#, escape(piece))),
                None => out.push_str(&escape(piece)),
            }
        }
        if !comment.is_empty() {
            out.push_str(&format!(r#"<span class="hl-comment">{}</span>"#, escape(comment)));
        }
    }
    out
}

pub struct Lang {
    keywords: &'static [&'static str],
    constants: &'static [&'static str],
    types: &'static [&'static str],
    rust: bool,
}

pub const RUST: Lang = Lang {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "fn", "for", "if", "impl", "in",
        "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static", "struct", "super", "trait", "type", "unsafe",
        "use", "where", "while",
    ],
    constants: &["true", "false", "self"],
    types: &["bool", "char", "str", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize"],
    rust: true,
};

pub const JS: Lang = Lang {
    keywords: &[
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "of",
        "return",
        "static",
        "switch",
        "throw",
        "try",
        "typeof",
        "var",
        "void",
        "while",
        "yield",
    ],
    constants: &["true", "false", "null", "undefined", "this"],
    types: &[],
    rust: false,
};

pub fn clike(source: &str, lang: &Lang) -> String {
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &source[i..];
        let c = rest.chars().next().unwrap_or(' ');
        let (class, len) = if rest.starts_with("//") {
            ("comment", rest.find('\n').unwrap_or(rest.len()))
        } else if rest.starts_with("/*") {
            ("comment", rest.find("*/").map_or(rest.len(), |e| e + 2))
        } else if lang.rust && rest.starts_with("#[") {
            ("constant", rest.find(']').map_or(rest.len(), |e| e + 1))
        } else if let Some(n) = raw_string(rest).filter(|_| lang.rust) {
            ("string", n)
        } else if c == '"' || (c == 'b' && rest[1..].starts_with('"')) || (!lang.rust && (c == '\'' || c == '`')) {
            let quote = if c == 'b' { '"' } else { c };
            ("string", quoted(rest, quote))
        } else if lang.rust && c == '\'' {
            let lifetime = rest[1..].chars().nth(1) != Some('\'') && !rest[1..].starts_with('\\');
            if lifetime { ("constant", 1 + word_len(&rest[1..])) } else { ("string", quoted(rest, '\'')) }
        } else if c.is_ascii_digit() {
            let mut n = 0;
            while let Some(d) = rest[n..].chars().next() {
                let fraction = d == '.' && rest[n + 1..].starts_with(|c: char| c.is_ascii_digit());
                if !(d.is_ascii_alphanumeric() || d == '_' || fraction) {
                    break;
                }
                n += 1;
            }
            ("number", n)
        } else if c.is_alphabetic() || c == '_' {
            let n = word_len(rest);
            let word = &rest[..n];
            let after = &rest[n..];
            if lang.keywords.contains(&word) {
                ("keyword", n)
            } else if lang.constants.contains(&word) {
                ("constant", n)
            } else if lang.rust && after.starts_with('!') && !after.starts_with("!=") {
                ("call", n + 1)
            } else if word.starts_with(|c: char| c.is_uppercase()) || lang.types.contains(&word) {
                ("type", n)
            } else if after.starts_with('(') || after.starts_with("::<") {
                ("call", n)
            } else {
                ("", n)
            }
        } else {
            ("", c.len_utf8())
        };
        let text = &rest[..len.max(c.len_utf8())];
        if class.is_empty() {
            out.push_str(&escape(text));
        } else {
            out.push_str(&format!(r#"<span class="hl-{class}">{}</span>"#, escape(text)));
        }
        i += text.len();
    }
    out
}

fn quoted(text: &str, quote: char) -> usize {
    let start = text.find(quote).unwrap_or(0) + 1;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            _ if c == quote => return start + i + 1,
            _ => {}
        }
    }
    text.len()
}

fn raw_string(text: &str) -> Option<usize> {
    let after_r = text.strip_prefix('r').or_else(|| text.strip_prefix("br"))?;
    let body = after_r.trim_start_matches('#');
    let hashes = after_r.len() - body.len();
    let body = body.strip_prefix('"')?;
    let close = format!("\"{}", "#".repeat(hashes));
    let start = text.len() - body.len();
    Some(body.find(&close).map_or(text.len(), |e| start + e + close.len()))
}

fn word_len(text: &str) -> usize {
    text.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(text.len())
}

fn split_keep_spaces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut space = None;
    for (i, c) in text.char_indices() {
        let is_space = c.is_whitespace();
        if space.is_some_and(|s| s != is_space) {
            pieces.push(&text[start..i]);
            start = i;
        }
        space = Some(is_space);
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sand_is_highlighted_and_escaped() {
        assert_eq!(sand("let x = \"<\""), r#"<span class="hl-keyword">let</span> x = <span class="hl-string">&quot;&lt;&quot;</span>"#);
    }

    #[test]
    fn links_apply_to_types_and_plain_words() {
        let link = |w: &str| (w == "Json" || w == "str").then(|| format!("/{w}/"));
        let html = sand_linked("fn parse(text: str) -> Json", &link);
        assert!(html.contains(r#"<a href="/str/">str</a>"#), "{html}");
        assert!(html.contains(r#"<a class="hl-type" href="/Json/">Json</a>"#), "{html}");
    }

    #[test]
    fn rust_is_highlighted() {
        let html = clike("#[tokio::main]\nfn f<'a>(x: &'a str) -> Vec<u8> { println!(\"{x}\"); g(r#\"\"\"#, '\\'', 1.5) } // end", &RUST);
        for part in [
            r#"<span class="hl-constant">#[tokio::main]</span>"#,
            r#"<span class="hl-keyword">fn</span> f&lt;<span class="hl-constant">'a</span>&gt;"#,
            r#"<span class="hl-type">Vec</span>&lt;<span class="hl-type">u8</span>&gt;"#,
            r#"<span class="hl-call">println!</span>(<span class="hl-string">&quot;{x}&quot;</span>)"#,
            r#"<span class="hl-call">g</span>(<span class="hl-string">r#&quot;&quot;&quot;#</span>, <span class="hl-string">'\''</span>, <span class="hl-number">1.5</span>)"#,
            r#"<span class="hl-comment">// end</span>"#,
        ] {
            assert!(html.contains(part), "{part} in {html}");
        }
    }

    #[test]
    fn policy_lines_are_highlighted() {
        let html = policy("allow read /data  # data\nlimit memory 64M");
        assert!(html.starts_with(r#"<span class="hl-keyword">allow</span> read /data  <span class="hl-comment"># data</span>"#), "{html}");
        assert!(html.contains(r#"<span class="hl-number">64M</span>"#), "{html}");
    }
}
