//! Syntax highlighting from the lexer, for the REPL's line editor and the
//! web playground. Works on incomplete and invalid source.

use super::lexer::{Comment, CommentKind, Tok, lex};

/// What a highlighted range is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Highlight {
    Keyword,
    /// `true`, `false`, `self`, `Self`.
    Constant,
    /// Integer, float and duration literals.
    Number,
    /// String and bytes literals, except interpolated expressions.
    String,
    /// `\(` and `)` around an interpolated expression.
    Interpolation,
    Comment,
    /// `///` and `//!` comments.
    DocComment,
    /// An identifier starting with an upper-case letter (types, traits,
    /// enum variants).
    Type,
    /// An identifier followed by `(`, or the name in a `fn` declaration.
    Call,
}

impl Highlight {
    /// A short name, e.g. for CSS classes.
    pub fn name(self) -> &'static str {
        match self {
            Highlight::Keyword => "keyword",
            Highlight::Constant => "constant",
            Highlight::Number => "number",
            Highlight::String => "string",
            Highlight::Interpolation => "interpolation",
            Highlight::Comment => "comment",
            Highlight::DocComment => "doc",
            Highlight::Type => "type",
            Highlight::Call => "call",
        }
    }
}

/// The highlighted ranges of `source` (byte offsets, in order, not
/// overlapping). Text in no range is plain.
pub fn highlight(source: &str) -> Vec<(usize, usize, Highlight)> {
    let lexed = lex(source, 0);
    let mut out: Vec<(usize, usize, Highlight)> = Vec::new();
    let mut add = |lo: usize, hi: usize, kind: Highlight| {
        let hi = hi.min(source.len());
        match out.last_mut() {
            _ if lo >= hi => {}
            // The parts of a string (quotes, text, format specs) are one range.
            Some(last) if last.1 == lo && last.2 == kind && kind == Highlight::String => last.1 = hi,
            Some(last) if last.1 > lo => {}
            _ => out.push((lo, hi, kind)),
        }
    };
    let mut comments = lexed.comments.iter().peekable();
    let tokens = &lexed.tokens;
    for (i, t) in tokens.iter().enumerate() {
        let (lo, hi) = (t.lo as usize, t.hi as usize);
        while let Some(c) = comments.next_if(|c| (c.lo as usize) < lo) {
            add(c.lo as usize, c.hi as usize, comment(c));
        }
        let after_fn = i > 0 && tokens[i - 1].tok == Tok::Fn;
        let after_dot = i > 0 && tokens[i - 1].tok == Tok::Dot;
        let kind = match &t.tok {
            Tok::Str(_) | Tok::Bytes(_) | Tok::StrStart | Tok::StrText(_) | Tok::StrEnd => Highlight::String,
            Tok::InterpStart => Highlight::Interpolation,
            Tok::InterpEnd { spec } => {
                if *spec {
                    add(lo, hi - 1, Highlight::String);
                }
                add(hi - 1, hi, Highlight::Interpolation);
                continue;
            }
            Tok::Int(_) | Tok::IntOverflow(_) | Tok::Float(_) | Tok::Inf | Tok::Nan | Tok::Duration(_) => Highlight::Number,
            Tok::True | Tok::False | Tok::SelfLower | Tok::SelfUpper => Highlight::Constant,
            // Treat keywords after `fn` or `.` as method names (`fn extend`, `xs.extend(ys)`).
            tok if tok.is_keyword() && !(after_fn || after_dot) => Highlight::Keyword,
            tok if *tok == Tok::Ident || tok.is_keyword() => {
                if &source[lo..hi] == "from" && tokens.get(i + 2).is_some_and(|n| n.tok == Tok::Import) {
                    Highlight::Keyword
                } else if source[lo..hi].starts_with(|c: char| c.is_ascii_uppercase()) {
                    Highlight::Type
                } else if matches!(tokens.get(i + 1), Some(n) if n.tok == Tok::LParen && n.lo == t.hi) || after_fn {
                    Highlight::Call
                } else if let Some(n) = tokens.get(i + 1).filter(|n| n.tok == Tok::Bang && n.lo == t.hi) {
                    add(lo, n.hi as usize, Highlight::Call);
                    continue;
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        add(lo, hi, kind);
    }
    for c in comments {
        add(c.lo as usize, c.hi as usize, comment(c));
    }
    out
}

fn comment(c: &Comment) -> Highlight {
    match c.kind {
        CommentKind::Doc | CommentKind::ModuleDoc => Highlight::DocComment,
        CommentKind::Line => Highlight::Comment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(source: &str) -> Vec<(&str, &'static str)> {
        highlight(source).into_iter().map(|(lo, hi, h)| (&source[lo..hi], h.name())).collect()
    }

    #[test]
    fn tokens_comments_and_strings() {
        assert_eq!(
            show("/// Doc\nfn area(s: Shape) -> float { 3.5 * s.r // radius\n}"),
            [("/// Doc", "doc"), ("fn", "keyword"), ("area", "call"), ("Shape", "type"), ("3.5", "number"), ("// radius", "comment"),]
        );
        assert_eq!(show("fn zip<U>(other: [U])"), [("fn", "keyword"), ("zip", "call"), ("U", "type"), ("U", "type")]);
        assert_eq!(
            show("fn extend(self) { xs.extend(ys) }"),
            [("fn", "keyword"), ("extend", "call"), ("self", "constant"), ("extend", "call")]
        );
        assert_eq!(show("let f: fn(int) = g"), [("let", "keyword"), ("fn", "keyword")]);
        assert_eq!(
            show("let ok = true and 5s > 1ms"),
            [("let", "keyword"), ("true", "constant"), ("and", "keyword"), ("5s", "number"), ("1ms", "number")]
        );
    }

    #[test]
    fn interpolations_are_code() {
        assert_eq!(
            show(r#"print("n = \(len(xs) + 1:>4)!")"#),
            [
                ("print", "call"),
                ("\"n = ", "string"),
                ("\\(", "interpolation"),
                ("len", "call"),
                ("1", "number"),
                (":>4", "string"),
                (")", "interpolation"),
                ("!\"", "string"),
            ]
        );
        assert_eq!(show(r#""a\("b\(1)")""#).iter().filter(|(_, h)| *h == "number").count(), 1, "nested");
    }

    #[test]
    fn incomplete_source_is_highlighted() {
        assert_eq!(show("fn f() { \"unterminated"), [("fn", "keyword"), ("f", "call"), ("\"unterminated", "string")]);
        assert_eq!(show("json!({\"a\": null})"), [("json!", "call"), ("\"a\"", "string")]);
        assert!(highlight("\"\\(\"").iter().all(|&(lo, hi, _)| lo < hi && hi <= 4));
    }
}
