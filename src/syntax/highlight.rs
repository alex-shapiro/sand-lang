//! Syntax highlighting: what each part of a source is, from the lexer, for
//! editors (the REPL's line editor, the web playground). Works on incomplete
//! and invalid source.

use super::lexer::{CommentKind, StrPart, Tok, lex, lex_range};

/// What a highlighted range is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Highlight {
    Keyword,
    /// `true`, `false`, `self`, `Self`.
    Constant,
    /// Integer, float and duration literals.
    Number,
    /// String and bytes literals (but not their interpolated expressions).
    String,
    /// The braces around an interpolated expression.
    Interpolation,
    Comment,
    /// `///` and `//!` comments.
    DocComment,
    /// An identifier starting with an upper-case letter (types, traits,
    /// enum variants).
    Type,
    /// An identifier followed by `(`.
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
    let mut out = Vec::new();
    ranges(source, 0, source.len(), 0, &mut out);
    out.sort_by_key(|&(lo, _, _)| lo);
    // Drop anything overlapping an earlier range (it never should).
    let mut end = 0;
    out.retain(|&(lo, hi, _)| {
        let keep = lo >= end && lo < hi;
        if keep {
            end = hi;
        }
        keep
    });
    out
}

/// Nested interpolations are highlighted down to this depth.
const MAX_NESTING: usize = 8;

fn ranges(source: &str, lo: usize, hi: usize, depth: usize, out: &mut Vec<(usize, usize, Highlight)>) {
    let lexed = if lo == 0 && hi == source.len() { lex(source, 0) } else { lex_range(source, 0, lo, hi) };
    for c in &lexed.comments {
        let kind = match c.kind {
            CommentKind::Doc | CommentKind::ModuleDoc => Highlight::DocComment,
            CommentKind::Line | CommentKind::Block => Highlight::Comment,
        };
        out.push((c.lo as usize, c.hi as usize, kind));
    }
    let tokens = &lexed.tokens;
    for (i, t) in tokens.iter().enumerate() {
        let (tlo, thi) = (t.lo as usize, (t.hi as usize).min(source.len()));
        let kind = match &t.tok {
            Tok::Str(parts) => {
                string(source, tlo, thi, parts, depth, out);
                continue;
            }
            Tok::Bytes(_) => Highlight::String,
            Tok::Int(_) | Tok::IntOverflow(_) | Tok::Float(_) | Tok::Duration(_) => Highlight::Number,
            Tok::True | Tok::False | Tok::SelfLower | Tok::SelfUpper => Highlight::Constant,
            Tok::Ident => {
                let text = source.get(tlo..thi).unwrap_or("");
                if text.starts_with(|c: char| c.is_ascii_uppercase()) {
                    Highlight::Type
                } else if matches!(tokens.get(i + 1), Some(n) if n.tok == Tok::LParen && n.lo == t.hi) {
                    Highlight::Call
                } else {
                    continue;
                }
            }
            tok if is_keyword(tok) => Highlight::Keyword,
            _ => continue,
        };
        out.push((tlo, thi, kind));
    }
}

/// A string literal at `lo..hi`: string ranges around its interpolated
/// expressions, which are highlighted as code.
fn string(source: &str, lo: usize, hi: usize, parts: &[StrPart], depth: usize, out: &mut Vec<(usize, usize, Highlight)>) {
    let mut at = lo;
    for part in parts {
        let StrPart::Expr { lo: elo, hi: ehi, spec } = part else { continue };
        let (elo, ehi) = (*elo as usize, *ehi as usize);
        // `{` before the expression, and `}` after it (or its format spec).
        let (Some(open), Some(close)) = (elo.checked_sub(1), source[ehi.min(hi)..hi].find('}').map(|i| ehi + i)) else { continue };
        if open < at || elo > ehi || close >= hi {
            continue;
        }
        if open > at {
            out.push((at, open, Highlight::String));
        }
        out.push((open, elo, Highlight::Interpolation));
        if depth < MAX_NESTING {
            ranges(source, elo, ehi, depth + 1, out);
        }
        if spec.is_some() && close > ehi {
            // `:spec`
            out.push((ehi, close, Highlight::String));
        }
        out.push((close, close + 1, Highlight::Interpolation));
        at = close + 1;
    }
    if hi > at {
        out.push((at, hi, Highlight::String));
    }
}

fn is_keyword(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::And
            | Tok::As
            | Tok::Break
            | Tok::Continue
            | Tok::Defer
            | Tok::Else
            | Tok::Enum
            | Tok::Extend
            | Tok::Fn
            | Tok::For
            | Tok::From
            | Tok::If
            | Tok::Impl
            | Tok::Import
            | Tok::In
            | Tok::Let
            | Tok::Loop
            | Tok::Match
            | Tok::Native
            | Tok::Not
            | Tok::Or
            | Tok::Pub
            | Tok::Return
            | Tok::Spawn
            | Tok::Struct
            | Tok::Test
            | Tok::Trait
            | Tok::Type
            | Tok::Var
            | Tok::While
    )
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
        assert_eq!(
            show("let ok = true and 5s > 1ms"),
            [("let", "keyword"), ("true", "constant"), ("and", "keyword"), ("5s", "number"), ("1ms", "number")]
        );
    }

    #[test]
    fn interpolations_are_code() {
        assert_eq!(
            show(r#"print("n = {len(xs) + 1:>4}!")"#),
            [
                ("print", "call"),
                ("\"n = ", "string"),
                ("{", "interpolation"),
                ("len", "call"),
                ("1", "number"),
                (":>4", "string"),
                ("}", "interpolation"),
                ("!\"", "string"),
            ]
        );
        assert_eq!(show(r#""a{"b{1}"}""#).iter().filter(|(_, h)| *h == "number").count(), 1, "nested");
    }

    #[test]
    fn incomplete_source_is_highlighted() {
        assert_eq!(show("fn f() { \"unterminated"), [("fn", "keyword"), ("f", "call"), ("\"unterminated", "string")]);
        assert_eq!(show("/* open"), [("/* open", "comment")]);
        assert!(highlight("\"{\"").iter().all(|&(lo, hi, _)| lo < hi && hi <= 3));
    }
}
