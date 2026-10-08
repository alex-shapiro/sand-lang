//! The lexer: source text to tokens, with automatic statement terminators.

use crate::diag::{Diag, FileId, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    Text(String),
    /// An interpolated expression: byte range of its source, and an optional format spec.
    Expr {
        lo: u32,
        hi: u32,
        spec: Option<(String, u32)>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident,
    Int(i64),
    /// An integer literal that does not fit in i64; holds the value if it is exactly 2^63.
    IntOverflow(bool),
    Float(f64),
    Duration(i64),
    Str(Vec<StrPart>),
    Bytes(Vec<u8>),

    // Keywords.
    And,
    As,
    Break,
    Continue,
    Else,
    Enum,
    Extend,
    False,
    Fn,
    For,
    From,
    If,
    Import,
    In,
    Let,
    Loop,
    Match,
    Native,
    Not,
    Or,
    Pub,
    Return,
    SelfLower,
    SelfUpper,
    Spawn,
    Struct,
    Test,
    True,
    Type,
    Var,
    While,

    // Punctuation.
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    StarStar,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Shl,
    Shr,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    DotDot,
    DotDotEq,
    FatArrow,
    Arrow,
    PipeGt,
    QQ,
    Question,
    Dot,
    Comma,
    Colon,
    Semi,
    At,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Newline,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident => "identifier".into(),
            Tok::Int(_) | Tok::IntOverflow(_) => "integer".into(),
            Tok::Float(_) => "float".into(),
            Tok::Duration(_) => "duration".into(),
            Tok::Str(_) => "string".into(),
            Tok::Bytes(_) => "bytes literal".into(),
            Tok::Semi => "newline or `;`".into(),
            Tok::Newline => "newline".into(),
            Tok::Eof => "end of file".into(),
            other => format!("`{}`", other.text()),
        }
    }

    pub fn text(&self) -> &'static str {
        use Tok::*;
        match self {
            And => "and",
            As => "as",
            Break => "break",
            Continue => "continue",
            Else => "else",
            Enum => "enum",
            Extend => "extend",
            False => "false",
            Fn => "fn",
            For => "for",
            From => "from",
            If => "if",
            Import => "import",
            In => "in",
            Let => "let",
            Loop => "loop",
            Match => "match",
            Native => "native",
            Not => "not",
            Or => "or",
            Pub => "pub",
            Return => "return",
            SelfLower => "self",
            SelfUpper => "Self",
            Spawn => "spawn",
            Struct => "struct",
            Test => "test",
            True => "true",
            Type => "type",
            Var => "var",
            While => "while",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            Percent => "%",
            StarStar => "**",
            Amp => "&",
            Pipe => "|",
            Caret => "^",
            Tilde => "~",
            Shl => "<<",
            Shr => ">>",
            EqEq => "==",
            NotEq => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            Eq => "=",
            PlusEq => "+=",
            MinusEq => "-=",
            StarEq => "*=",
            SlashEq => "/=",
            PercentEq => "%=",
            DotDot => "..",
            DotDotEq => "..=",
            FatArrow => "=>",
            Arrow => "->",
            PipeGt => "|>",
            QQ => "??",
            Question => "?",
            Dot => ".",
            Comma => ",",
            Colon => ":",
            Semi => ";",
            At => "@",
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            _ => "",
        }
    }
}

pub fn keyword(s: &str) -> Option<Tok> {
    use Tok::*;
    Some(match s {
        "and" => And,
        "as" => As,
        "break" => Break,
        "continue" => Continue,
        "else" => Else,
        "enum" => Enum,
        "extend" => Extend,
        "false" => False,
        "fn" => Fn,
        "for" => For,
        "from" => From,
        "if" => If,
        "import" => Import,
        "in" => In,
        "let" => Let,
        "loop" => Loop,
        "match" => Match,
        "native" => Native,
        "not" => Not,
        "or" => Or,
        "pub" => Pub,
        "return" => Return,
        "self" => SelfLower,
        "Self" => SelfUpper,
        "spawn" => Spawn,
        "struct" => Struct,
        "true" => True,
        "type" => Type,
        "var" => Var,
        "while" => While,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub lo: u32,
    pub hi: u32,
    /// True if a newline precedes this token (before semicolon insertion).
    pub newline_before: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Line,
    Block,
    Doc,
    ModuleDoc,
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub kind: CommentKind,
    pub lo: u32,
    pub hi: u32,
    pub text: String,
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    pub errors: Vec<Diag>,
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    end: usize,
    file: FileId,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    errors: Vec<Diag>,
    newline_pending: bool,
}

/// Lexes `src[lo..hi]` (positions are absolute within the file).
pub fn lex_range(src: &str, file: FileId, lo: usize, hi: usize) -> Lexed {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: lo,
        end: hi,
        file,
        tokens: Vec::new(),
        comments: Vec::new(),
        errors: Vec::new(),
        newline_pending: false,
    };
    if lx.src[lo..].starts_with('\u{feff}') {
        lx.pos += 3;
    }
    lx.run();
    let tokens = insert_terminators(lx.tokens);
    Lexed { tokens, comments: lx.comments, errors: lx.errors }
}

pub fn lex(src: &str, file: FileId) -> Lexed {
    lex_range(src, file, 0, src.len())
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

impl<'a> Lexer<'a> {
    fn peek(&self, off: usize) -> u8 {
        let p = self.pos + off;
        if p < self.end { self.bytes[p] } else { 0 }
    }

    fn span(&self, lo: usize, hi: usize) -> Span {
        Span::new(self.file, lo as u32, hi as u32)
    }

    fn error(&mut self, lo: usize, hi: usize, msg: impl Into<String>) {
        self.errors.push(Diag::error(self.span(lo, hi), msg));
    }

    fn push(&mut self, tok: Tok, lo: usize) {
        let nl = std::mem::take(&mut self.newline_pending);
        self.tokens.push(Token { tok, lo: lo as u32, hi: self.pos as u32, newline_before: nl });
    }

    fn run(&mut self) {
        while self.pos < self.end {
            let c = self.bytes[self.pos];
            let lo = self.pos;
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    if !self.newline_pending {
                        self.tokens.push(Token { tok: Tok::Newline, lo: lo as u32, hi: lo as u32 + 1, newline_before: false });
                    }
                    self.newline_pending = true;
                }
                b'/' if self.peek(1) == b'/' => self.line_comment(),
                b'/' if self.peek(1) == b'*' => self.block_comment(),
                b'0'..=b'9' => self.number(),
                b'"' => {
                    if self.peek(1) == b'"' && self.peek(2) == b'"' {
                        self.triple_string();
                    } else {
                        self.pos += 1;
                        let parts = self.string_body(b'"', 0, false);
                        self.push(Tok::Str(parts), lo);
                    }
                }
                b'r' if self.peek(1) == b'"' || (self.peek(1) == b'#' && self.raw_hashes_then_quote(1)) => self.raw_string(),
                b'b' if self.peek(1) == b'"' => {
                    self.pos += 2;
                    let bytes = self.bytes_body();
                    self.push(Tok::Bytes(bytes), lo);
                }
                c if is_ident_start(c) => {
                    while self.pos < self.end && is_ident_char(self.bytes[self.pos]) {
                        self.pos += 1;
                    }
                    let word = &self.src[lo..self.pos];
                    let tok = keyword(word).unwrap_or(Tok::Ident);
                    self.push(tok, lo);
                }
                _ => self.punct(),
            }
        }
        self.push(Tok::Eof, self.end);
        if let Some(t) = self.tokens.last_mut() {
            t.lo = self.end as u32;
            t.hi = self.end as u32;
        }
    }

    fn raw_hashes_then_quote(&self, mut off: usize) -> bool {
        while self.peek(off) == b'#' {
            off += 1;
        }
        self.peek(off) == b'"'
    }

    fn line_comment(&mut self) {
        let lo = self.pos;
        while self.pos < self.end && self.bytes[self.pos] != b'\n' {
            self.pos += 1;
        }
        let text = self.src[lo..self.pos].trim_end_matches('\r').to_string();
        let kind = if text.starts_with("///") && !text.starts_with("////") {
            CommentKind::Doc
        } else if text.starts_with("//!") {
            CommentKind::ModuleDoc
        } else {
            CommentKind::Line
        };
        self.comments.push(Comment { kind, lo: lo as u32, hi: self.pos as u32, text });
    }

    fn block_comment(&mut self) {
        let lo = self.pos;
        self.pos += 2;
        let mut depth = 1;
        while self.pos < self.end {
            if self.bytes[self.pos] == b'/' && self.peek(1) == b'*' {
                depth += 1;
                self.pos += 2;
            } else if self.bytes[self.pos] == b'*' && self.peek(1) == b'/' {
                depth -= 1;
                self.pos += 2;
                if depth == 0 {
                    break;
                }
            } else {
                self.pos += 1;
            }
        }
        if depth > 0 {
            self.error(lo, lo + 2, "unterminated block comment");
        }
        self.comments.push(Comment {
            kind: CommentKind::Block,
            lo: lo as u32,
            hi: self.pos as u32,
            text: self.src[lo..self.pos].to_string(),
        });
    }

    fn number(&mut self) {
        let lo = self.pos;
        let radix = if self.bytes[self.pos] == b'0' {
            match self.peek(1) {
                b'x' | b'X' => 16,
                b'b' | b'B' => 2,
                b'o' | b'O' => 8,
                _ => 10,
            }
        } else {
            10
        };
        if radix != 10 {
            self.pos += 2;
            let start = self.pos;
            while self.pos < self.end && (self.bytes[self.pos].is_ascii_alphanumeric() || self.bytes[self.pos] == b'_') {
                self.pos += 1;
            }
            let digits: String = self.src[start..self.pos].chars().filter(|&c| c != '_').collect();
            if digits.is_empty() {
                self.error(lo, self.pos, "invalid number literal");
                self.push(Tok::Int(0), lo);
                return;
            }
            match u64::from_str_radix(&digits, radix) {
                Ok(v) if v <= i64::MAX as u64 => self.push(Tok::Int(v as i64), lo),
                Ok(v) => self.push(Tok::IntOverflow(v == 1u64 << 63), lo),
                Err(_) => {
                    if digits.chars().all(|c| c.is_digit(radix)) {
                        self.push(Tok::IntOverflow(false), lo);
                    } else {
                        self.error(lo, self.pos, "invalid number literal");
                        self.push(Tok::Int(0), lo);
                    }
                }
            }
            return;
        }
        let mut is_float = false;
        self.digits();
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek(0), b'e' | b'E')
            && (self.peek(1).is_ascii_digit() || (matches!(self.peek(1), b'+' | b'-') && self.peek(2).is_ascii_digit()))
        {
            is_float = true;
            self.pos += 2;
            self.digits();
        }
        let num_end = self.pos;
        let text: String = self.src[lo..num_end].chars().filter(|&c| c != '_').collect();
        // Duration suffix?
        let mut suffix_end = self.pos;
        while suffix_end < self.end && is_ident_char(self.bytes[suffix_end]) {
            suffix_end += 1;
        }
        let suffix = &self.src[self.pos..suffix_end];
        if !suffix.is_empty() {
            let unit: Option<f64> = match suffix {
                "ns" => Some(1.0),
                "us" => Some(1e3),
                "ms" => Some(1e6),
                "s" => Some(1e9),
                "m" => Some(60e9),
                "h" => Some(3600e9),
                _ => None,
            };
            self.pos = suffix_end;
            match unit {
                Some(mult) => {
                    let nanos = if is_float {
                        let v: f64 = text.parse().unwrap_or(0.0);
                        v * mult
                    } else {
                        match text.parse::<i128>() {
                            Ok(v) => (v * mult as i128) as f64,
                            Err(_) => f64::INFINITY,
                        }
                    };
                    if !nanos.is_finite() || nanos.abs() > i64::MAX as f64 {
                        self.error(lo, self.pos, "duration literal out of range");
                        self.push(Tok::Duration(0), lo);
                    } else if is_float {
                        self.push(Tok::Duration(nanos as i64), lo);
                    } else {
                        let v: i128 = text.parse().unwrap_or(0);
                        self.push(Tok::Duration((v * mult as i128) as i64), lo);
                    }
                }
                None => {
                    self.error(lo, self.pos, "invalid number literal");
                    self.push(Tok::Int(0), lo);
                }
            }
            return;
        }
        if is_float {
            let v: f64 = text.parse().unwrap_or(0.0);
            self.push(Tok::Float(v), lo);
        } else {
            match text.parse::<i64>() {
                Ok(v) => self.push(Tok::Int(v), lo),
                Err(_) => {
                    let exact = text.trim_start_matches('0') == "9223372036854775808";
                    self.push(Tok::IntOverflow(exact), lo)
                }
            }
        }
    }

    fn digits(&mut self) {
        while self.pos < self.end
            && (self.bytes[self.pos].is_ascii_digit() || (self.bytes[self.pos] == b'_' && self.peek(1).is_ascii_digit()))
        {
            self.pos += 1;
        }
    }

    /// Parses an escape sequence after the backslash; returns the char.
    fn escape(&mut self, allow_hex: bool) -> Option<Result<char, u8>> {
        let lo = self.pos - 1;
        let c = self.peek(0);
        self.pos += 1;
        let ch = match c {
            b'\\' => '\\',
            b'"' => '"',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'0' => '\0',
            b'{' => '{',
            b'}' => '}',
            b'\'' => '\'',
            b'x' if allow_hex => {
                let h = &self.src[self.pos..(self.pos + 2).min(self.end)];
                if h.len() == 2 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
                    self.pos += 2;
                    return Some(Err(u8::from_str_radix(h, 16).unwrap()));
                }
                self.error(lo, self.pos, "invalid escape: expected two hex digits after \\x");
                return None;
            }
            b'u' => {
                if self.peek(0) != b'{' {
                    self.error(lo, self.pos, "invalid escape: expected \\u{...}");
                    return None;
                }
                let start = self.pos + 1;
                let mut p = start;
                while p < self.end && self.bytes[p].is_ascii_hexdigit() && p - start < 7 {
                    p += 1;
                }
                if p >= self.end || self.bytes[p] != b'}' || p == start || p - start > 6 {
                    self.pos = p.min(self.end);
                    self.error(lo, self.pos, "invalid escape: malformed \\u{...}");
                    return None;
                }
                let v = u32::from_str_radix(&self.src[start..p], 16).unwrap();
                self.pos = p + 1;
                match char::from_u32(v) {
                    Some(c) => c,
                    None => {
                        self.error(lo, self.pos, "invalid escape: not a Unicode scalar value");
                        return None;
                    }
                }
            }
            0 => {
                self.pos -= 1;
                return None;
            }
            _ => {
                // Step over the whole (possibly multi-byte) character.
                let ch_len = self.src[self.pos - 1..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                self.pos = self.pos - 1 + ch_len;
                self.error(lo, self.pos, "invalid escape sequence");
                return None;
            }
        };
        Some(Ok(ch))
    }

    /// Scans a string body after the opening quote, until `quote` (or `"""`
    /// when `triple`). `dedent` whitespace characters are skipped after each newline.
    fn string_body(&mut self, quote: u8, dedent: usize, triple: bool) -> Vec<StrPart> {
        let lo = self.pos - 1;
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            if self.pos >= self.end {
                self.error(lo, (lo + 1).min(self.end), "unterminated string");
                break;
            }
            let c = self.bytes[self.pos];
            if triple {
                if c == b'"' && self.peek(1) == b'"' && self.peek(2) == b'"' {
                    self.pos += 3;
                    break;
                }
            } else if c == quote {
                self.pos += 1;
                break;
            }
            match c {
                b'\\' => {
                    self.pos += 1;
                    if let Some(Ok(ch)) = self.escape(false) {
                        text.push(ch);
                    }
                }
                b'{' => {
                    if !text.is_empty() {
                        parts.push(StrPart::Text(std::mem::take(&mut text)));
                    }
                    self.pos += 1;
                    parts.push(self.interpolation());
                }
                b'}' => {
                    self.error(self.pos, self.pos + 1, "unmatched `}` in string; write `\\}` for a literal brace");
                    self.pos += 1;
                }
                b'\n' => {
                    text.push('\n');
                    self.pos += 1;
                    let mut skipped = 0;
                    while skipped < dedent && self.pos < self.end && matches!(self.bytes[self.pos], b' ' | b'\t') {
                        self.pos += 1;
                        skipped += 1;
                    }
                }
                b'\r' if self.peek(1) == b'\n' => {
                    self.pos += 1;
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    text.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        if !text.is_empty() || parts.is_empty() {
            parts.push(StrPart::Text(text));
        }
        parts
    }

    /// Scans an interpolation after `{`; returns its part.
    fn interpolation(&mut self) -> StrPart {
        let lo = self.pos;
        let mut depth = 0usize;
        let mut spec_at: Option<usize> = None;
        while self.pos < self.end {
            let c = self.bytes[self.pos];
            match c {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    self.pos += 1;
                }
                b')' | b']' => {
                    depth = depth.saturating_sub(1);
                    self.pos += 1;
                }
                b'}' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                b':' if depth == 0 && spec_at.is_none() => {
                    spec_at = Some(self.pos);
                    self.pos += 1;
                }
                b'"' => self.skip_string(),
                b'r' if self.peek(1) == b'"' || (self.peek(1) == b'#' && self.raw_hashes_then_quote(1)) => {
                    let before = self.tokens.len();
                    self.raw_string();
                    self.tokens.truncate(before);
                }
                b'\n' if spec_at.is_none() && depth == 0 => {
                    // Interpolations do not span lines at the top level.
                    break;
                }
                _ => self.pos += 1,
            }
        }
        if self.pos >= self.end || self.bytes[self.pos] != b'}' {
            self.error(lo - 1, lo, "unterminated interpolation in string");
            return StrPart::Expr { lo: lo as u32, hi: self.pos as u32, spec: None };
        }
        let hi = self.pos;
        self.pos += 1; // `}`
        match spec_at {
            Some(c) => StrPart::Expr { lo: lo as u32, hi: c as u32, spec: Some((self.src[c + 1..hi].to_string(), c as u32 + 1)) },
            None => StrPart::Expr { lo: lo as u32, hi: hi as u32, spec: None },
        }
    }

    /// Skips over a nested string literal inside an interpolation.
    fn skip_string(&mut self) {
        let errs = self.errors.len();
        if self.peek(1) == b'"' && self.peek(2) == b'"' {
            let before = self.tokens.len();
            self.triple_string();
            self.tokens.truncate(before);
        } else {
            self.pos += 1;
            let _ = self.string_body(b'"', 0, false);
        }
        // Errors inside nested strings are reported again when the expression is lexed.
        self.errors.truncate(errs);
    }

    fn triple_string(&mut self) {
        let lo = self.pos;
        self.pos += 3;
        let content_start = if self.peek(0) == b'\n' {
            self.pos + 1
        } else if self.peek(0) == b'\r' && self.peek(1) == b'\n' {
            self.pos + 2
        } else {
            self.pos
        };
        // Find the closing delimiter to compute indentation.
        let mut p = content_start;
        let mut close = None;
        while p < self.end {
            if self.bytes[p] == b'\\' {
                p += 2;
                continue;
            }
            if p + 3 <= self.end && &self.bytes[p..p + 3] == b"\"\"\"" {
                close = Some(p);
                break;
            }
            p += 1;
        }
        let Some(close) = close else {
            self.error(lo, lo + 3, "unterminated string");
            self.pos = self.end;
            self.push(Tok::Str(vec![StrPart::Text(String::new())]), lo);
            return;
        };
        let raw = &self.src[content_start..close];
        let mut indent = usize::MAX;
        let lines: Vec<&str> = raw.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            let line = line.trim_end_matches('\r');
            let ws = line.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
            let blank = ws == line.len();
            let is_last = i + 1 == lines.len();
            if !blank || (is_last && lines.len() > 1) {
                indent = indent.min(ws);
            }
        }
        if indent == usize::MAX {
            indent = 0;
        }
        self.pos = content_start;
        // Skip the first line's indentation.
        let mut skipped = 0;
        while skipped < indent && self.pos < close && matches!(self.bytes[self.pos], b' ' | b'\t') {
            self.pos += 1;
            skipped += 1;
        }
        let mut parts = self.string_body(b'"', indent, true);
        // A closing delimiter on its own line leaves only whitespace after the final newline.
        if let Some(StrPart::Text(t)) = parts.last_mut() {
            let trimmed_len = t.trim_end_matches([' ', '\t']).len();
            if t[..trimmed_len].ends_with('\n') {
                t.truncate(trimmed_len);
            }
        }
        self.push(Tok::Str(parts), lo);
    }

    fn raw_string(&mut self) {
        let lo = self.pos;
        self.pos += 1; // r
        let mut hashes = 0;
        while self.peek(0) == b'#' {
            hashes += 1;
            self.pos += 1;
        }
        self.pos += 1; // "
        let start = self.pos;
        loop {
            if self.pos >= self.end {
                self.error(lo, lo + 1, "unterminated raw string");
                self.push(Tok::Str(vec![StrPart::Text(self.src[start..self.end].to_string())]), lo);
                return;
            }
            if self.bytes[self.pos] == b'"' && (0..hashes).all(|i| self.peek(1 + i) == b'#') {
                let text = self.src[start..self.pos].to_string();
                self.pos += 1 + hashes;
                self.push(Tok::Str(vec![StrPart::Text(text)]), lo);
                return;
            }
            self.pos += 1;
        }
    }

    fn bytes_body(&mut self) -> Vec<u8> {
        let lo = self.pos - 2;
        let mut out = Vec::new();
        loop {
            if self.pos >= self.end {
                self.error(lo, lo + 2, "unterminated bytes literal");
                break;
            }
            let c = self.bytes[self.pos];
            if c == b'"' {
                self.pos += 1;
                break;
            }
            if c == b'\\' {
                self.pos += 1;
                match self.escape(true) {
                    Some(Ok(ch)) => {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    Some(Err(b)) => out.push(b),
                    None => {}
                }
            } else {
                out.push(c);
                self.pos += 1;
            }
        }
        out
    }

    fn punct(&mut self) {
        let lo = self.pos;
        let c = self.bytes[self.pos];
        let n = self.peek(1);
        let n2 = self.peek(2);
        let (tok, len) = match (c, n) {
            (b'+', b'=') => (Tok::PlusEq, 2),
            (b'+', _) => (Tok::Plus, 1),
            (b'-', b'=') => (Tok::MinusEq, 2),
            (b'-', b'>') => (Tok::Arrow, 2),
            (b'-', _) => (Tok::Minus, 1),
            (b'*', b'*') => (Tok::StarStar, 2),
            (b'*', b'=') => (Tok::StarEq, 2),
            (b'*', _) => (Tok::Star, 1),
            (b'/', b'=') => (Tok::SlashEq, 2),
            (b'/', _) => (Tok::Slash, 1),
            (b'%', b'=') => (Tok::PercentEq, 2),
            (b'%', _) => (Tok::Percent, 1),
            (b'&', _) => (Tok::Amp, 1),
            (b'|', b'>') => (Tok::PipeGt, 2),
            (b'|', _) => (Tok::Pipe, 1),
            (b'^', _) => (Tok::Caret, 1),
            (b'~', _) => (Tok::Tilde, 1),
            (b'<', b'<') => (Tok::Shl, 2),
            (b'<', b'=') => (Tok::Le, 2),
            (b'<', _) => (Tok::Lt, 1),
            (b'>', b'>') => (Tok::Shr, 2),
            (b'>', b'=') => (Tok::Ge, 2),
            (b'>', _) => (Tok::Gt, 1),
            (b'=', b'=') => (Tok::EqEq, 2),
            (b'=', b'>') => (Tok::FatArrow, 2),
            (b'=', _) => (Tok::Eq, 1),
            (b'!', b'=') => (Tok::NotEq, 2),
            (b'.', b'.') if n2 == b'=' => (Tok::DotDotEq, 3),
            (b'.', b'.') => (Tok::DotDot, 2),
            (b'.', _) => (Tok::Dot, 1),
            (b'?', b'?') => (Tok::QQ, 2),
            (b'?', _) => (Tok::Question, 1),
            (b',', _) => (Tok::Comma, 1),
            (b':', _) => (Tok::Colon, 1),
            (b';', _) => (Tok::Semi, 1),
            (b'@', _) => (Tok::At, 1),
            (b'(', _) => (Tok::LParen, 1),
            (b')', _) => (Tok::RParen, 1),
            (b'[', _) => (Tok::LBracket, 1),
            (b']', _) => (Tok::RBracket, 1),
            (b'{', _) => (Tok::LBrace, 1),
            (b'}', _) => (Tok::RBrace, 1),
            _ => {
                let ch = self.src[self.pos..].chars().next().unwrap();
                self.pos += ch.len_utf8();
                let msg = if ch == '!' {
                    "unexpected character `!`; use `not` for negation".to_string()
                } else {
                    format!("unexpected character `{}`", ch.escape_debug())
                };
                self.error(lo, self.pos, msg);
                return;
            }
        };
        self.pos += len;
        self.push(tok, lo);
    }
}

fn ends_statement(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Ident
            | Tok::SelfLower
            | Tok::SelfUpper
            | Tok::True
            | Tok::False
            | Tok::Break
            | Tok::Continue
            | Tok::Return
            | Tok::Int(_)
            | Tok::IntOverflow(_)
            | Tok::Float(_)
            | Tok::Duration(_)
            | Tok::Str(_)
            | Tok::Bytes(_)
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Question
    )
}

fn continues_line(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Dot
            | Tok::PipeGt
            | Tok::Else
            | Tok::And
            | Tok::Or
            | Tok::FatArrow
            | Tok::Plus
            | Tok::Slash
            | Tok::Percent
            | Tok::StarStar
            | Tok::Amp
            | Tok::Pipe
            | Tok::Caret
            | Tok::Shl
            | Tok::Shr
            | Tok::EqEq
            | Tok::NotEq
            | Tok::Lt
            | Tok::Le
            | Tok::Gt
            | Tok::Ge
            | Tok::QQ
    )
}

/// Converts newlines into statement terminators according to §2.3.
fn insert_terminators(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut stack: Vec<Tok> = Vec::new();
    let n = tokens.len();
    let mut i = 0;
    let mut tokens = tokens;
    while i < n {
        let t = std::mem::replace(&mut tokens[i], Token { tok: Tok::Eof, lo: 0, hi: 0, newline_before: false });
        match t.tok {
            Tok::Newline => {
                let in_group = matches!(stack.last(), Some(Tok::LParen) | Some(Tok::LBracket));
                let prev_ends = out.last().map(|p| ends_statement(&p.tok)).unwrap_or(false);
                let next = tokens[i + 1..].iter().find(|t| t.tok != Tok::Newline);
                let next_continues = next.map(|t| continues_line(&t.tok)).unwrap_or(false);
                if !in_group && prev_ends && !next_continues {
                    out.push(Token { tok: Tok::Semi, lo: t.lo, hi: t.lo, newline_before: false });
                }
            }
            _ => {
                match t.tok {
                    Tok::LParen | Tok::LBracket | Tok::LBrace => stack.push(t.tok.clone()),
                    Tok::RParen | Tok::RBracket | Tok::RBrace => {
                        stack.pop();
                    }
                    _ => {}
                }
                out.push(t);
            }
        }
        i += 1;
    }
    out
}
