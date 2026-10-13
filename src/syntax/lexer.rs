//! The lexer: source text to tokens, with the automatic statement
//! terminators of §2.3 inserted as the tokens are produced.
//!
//! A string literal without interpolations is one `Str` token. A string
//! with interpolations is a sequence: `StrStart`, then its text (`StrText`)
//! and interpolations (`InterpStart` for the `\(`, the tokens of the
//! expression, `InterpEnd` for the `)`), then `StrEnd`. The lexer keeps a
//! stack of the brackets and interpolations it is in, which decides whether
//! a `)` ends an interpolation and whether a newline ends a statement.

use crate::diag::{Diag, FileId, Span};

/// [`Tok::flags`]: a keyword.
const KEYWORD: u8 = 1;
/// [`Tok::flags`]: a newline after the token ends the statement (§2.3).
const ENDS: u8 = 2;
/// [`Tok::flags`]: a line starting with the token continues the previous line (§2.3).
const CONTINUES: u8 = 4;

/// Declares [`Tok`] with its keywords and punctuation, each with its text
/// and its flags (how it takes part in line termination).
macro_rules! tokens {
    (
        keywords { $($kw:ident $kw_text:literal $kw_flags:expr,)* }
        punctuation { $($p:ident $p_text:literal $p_flags:expr,)* }
    ) => {
        #[derive(Clone, Debug, PartialEq)]
        pub enum Tok {
            Ident,
            Int(i64),
            /// An integer literal that does not fit in 64 bits; holds whether
            /// it is exactly 2^63 (`-9223372036854775808`).
            IntOverflow(bool),
            Float(f64),
            Duration(i64),
            /// A string literal without interpolations (escapes resolved).
            Str(String),
            Bytes(Vec<u8>),
            /// The opening quote of a string with interpolations.
            StrStart,
            /// Text of a string with interpolations (escapes resolved).
            StrText(String),
            /// The `\(` of an interpolation.
            InterpStart,
            /// The `)` of an interpolation; with `spec`, the token starts at
            /// the `:` of the format spec it holds.
            InterpEnd { spec: bool },
            /// The closing quote of a string with interpolations.
            StrEnd,
            $($kw,)*
            $($p,)*
            Eof,
        }

        impl Tok {
            /// The text of a keyword or punctuation token, else "".
            pub fn text(&self) -> &'static str {
                match self {
                    $(Tok::$kw => $kw_text,)*
                    $(Tok::$p => $p_text,)*
                    _ => "",
                }
            }

            fn flags(&self) -> u8 {
                match self {
                    $(Tok::$kw => KEYWORD | $kw_flags,)*
                    $(Tok::$p => $p_flags,)*
                    Tok::Ident
                    | Tok::Int(_)
                    | Tok::IntOverflow(_)
                    | Tok::Float(_)
                    | Tok::Duration(_)
                    | Tok::Str(_)
                    | Tok::Bytes(_)
                    | Tok::StrEnd => ENDS,
                    Tok::StrStart | Tok::StrText(_) | Tok::InterpStart | Tok::InterpEnd { .. } | Tok::Eof => 0,
                }
            }
        }

        /// The keyword spelled `s`, if it is one.
        pub fn keyword(s: &str) -> Option<Tok> {
            Some(match s {
                $($kw_text => Tok::$kw,)*
                _ => return None,
            })
        }
    };
}

tokens! {
    keywords {
        And "and" CONTINUES,
        As "as" 0,
        Break "break" ENDS,
        Continue "continue" ENDS,
        Defer "defer" 0,
        Else "else" CONTINUES,
        Enum "enum" 0,
        Extend "extend" 0,
        False "false" ENDS,
        Fn "fn" 0,
        For "for" 0,
        From "from" 0,
        If "if" 0,
        Impl "impl" 0,
        Import "import" 0,
        In "in" 0,
        Inf "inf" ENDS,
        Let "let" 0,
        Loop "loop" 0,
        Match "match" 0,
        Nan "nan" ENDS,
        Native "native" 0,
        Not "not" 0,
        Or "or" CONTINUES,
        Pub "pub" 0,
        Return "return" ENDS,
        SelfLower "self" ENDS,
        SelfUpper "Self" ENDS,
        Spawn "spawn" 0,
        Struct "struct" 0,
        Trait "trait" 0,
        True "true" ENDS,
        Type "type" 0,
        Use "use" 0,
        Var "var" 0,
        While "while" 0,
    }
    punctuation {
        Plus "+" CONTINUES,
        // Whether a line starting with `-` continues the previous one
        // depends on the space after it (§2.3, `Token::negates`).
        Minus "-" 0,
        Star "*" CONTINUES,
        Slash "/" CONTINUES,
        Percent "%" CONTINUES,
        StarStar "**" CONTINUES,
        Amp "&" CONTINUES,
        Pipe "|" CONTINUES,
        Caret "^" CONTINUES,
        Tilde "~" 0,
        Shl "<<" CONTINUES,
        Shr ">>" CONTINUES,
        EqEq "==" CONTINUES,
        NotEq "!=" CONTINUES,
        Lt "<" CONTINUES,
        Le "<=" CONTINUES,
        Gt ">" CONTINUES,
        Ge ">=" CONTINUES,
        Eq "=" 0,
        PlusEq "+=" 0,
        MinusEq "-=" 0,
        StarEq "*=" 0,
        SlashEq "/=" 0,
        PercentEq "%=" 0,
        // They may end an open range (`1..`).
        DotDot ".." ENDS,
        DotDotEq "..=" ENDS,
        FatArrow "=>" CONTINUES,
        Arrow "->" 0,
        PipeGt "|>" CONTINUES,
        QQ "??" CONTINUES,
        Question "?" ENDS,
        Dot "." CONTINUES,
        Comma "," 0,
        Colon ":" 0,
        Semi ";" 0,
        At "@" 0,
        Bang "!" 0,
        LParen "(" 0,
        RParen ")" ENDS,
        LBracket "[" 0,
        RBracket "]" ENDS,
        LBrace "{" 0,
        RBrace "}" ENDS,
    }
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident => "identifier".into(),
            Tok::Int(_) | Tok::IntOverflow(_) => "integer".into(),
            Tok::Float(_) => "float".into(),
            Tok::Duration(_) => "duration".into(),
            Tok::Str(_) | Tok::StrStart => "string".into(),
            Tok::StrText(_) | Tok::StrEnd => "end of string".into(),
            Tok::InterpStart => "`\\(`".into(),
            Tok::InterpEnd { .. } => "end of interpolation".into(),
            Tok::Bytes(_) => "bytes literal".into(),
            Tok::Semi => "newline or `;`".into(),
            Tok::Eof => "end of file".into(),
            other => format!("`{}`", other.text()),
        }
    }

    /// A keyword (including `true`, `self`, ...).
    pub fn is_keyword(&self) -> bool {
        self.flags() & KEYWORD != 0
    }

    /// Whether a newline after the token ends the statement (§2.3).
    pub fn ends_statement(&self) -> bool {
        self.flags() & ENDS != 0
    }

    /// Whether a line starting with the token continues the previous one (§2.3).
    pub fn continues_line(&self) -> bool {
        self.flags() & CONTINUES != 0
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub lo: u32,
    pub hi: u32,
    /// A line break precedes the token (whether or not it ended a statement).
    pub newline_before: bool,
    /// For `(`: the group it opens is followed by `=>`, so it holds the
    /// parameters of a lambda.
    pub opens_lambda: bool,
    /// For `-`: whitespace (or the start of the source) before it and none
    /// after (`-x`), so it is a negation, never a subtraction (§2.3).
    pub negates: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Line,
    Block,
    Doc,
    ModuleDoc,
}

#[derive(Clone, Copy, Debug)]
pub struct Comment {
    pub kind: CommentKind,
    pub lo: u32,
    pub hi: u32,
}

impl Comment {
    /// The comment's text (without a line comment's line break).
    pub fn text<'s>(&self, src: &'s str) -> &'s str {
        &src[self.lo as usize..self.hi as usize]
    }
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    pub errors: Vec<Diag>,
    /// The source ends inside a string, a comment or brackets.
    pub open: bool,
}

/// A string being lexed.
#[derive(Clone, Copy)]
struct StrState {
    /// Where its opening quote is.
    lo: usize,
    /// A `"""` string (whose lines are in [`Lexer::blocks`]).
    triple: bool,
}

/// The lines of a `"""` string being lexed, whose common indentation is
/// removed from its text once its end is known (§2.5).
#[derive(Default)]
struct Block {
    /// Its `StrText` tokens so far (indices into the tokens).
    texts: Vec<usize>,
    /// Where its lines that start with whitespace start: the text (an index
    /// into `texts`, or `texts.len()` for the text being lexed), the offset in
    /// it and the number of whitespace characters.
    lines: Vec<(usize, usize, usize)>,
    /// The least indentation of the lines that count (§2.5), so far.
    indent: Option<usize>,
    /// Whether a line has started (after the first).
    multiline: bool,
}

impl StrState {
    fn quote_len(&self) -> usize {
        if self.triple { 3 } else { 1 }
    }
}

/// An open bracket or interpolation.
enum Group {
    /// `(`, with the index of its token.
    Paren(usize),
    Bracket,
    Brace,
    /// An interpolation (whose `\(` is at `open`) of the string `str`.
    Interp {
        str: StrState,
        open: usize,
    },
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    file: FileId,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    errors: Vec<Diag>,
    /// The first line break since the last token, if any.
    newline: Option<usize>,
    /// The brackets and interpolations open at `pos`, innermost last.
    groups: Vec<Group>,
    /// The `(` whose group the last token closed, if it did.
    closed_paren: Option<usize>,
    /// The `"""` strings being lexed, innermost last.
    blocks: Vec<Block>,
    open: bool,
}

pub fn lex(src: &str, file: FileId) -> Lexed {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        file,
        tokens: Vec::with_capacity(src.len() / 4),
        comments: Vec::new(),
        errors: Vec::new(),
        newline: None,
        groups: Vec::new(),
        closed_paren: None,
        blocks: Vec::new(),
        open: false,
    };
    if src.starts_with('\u{feff}') {
        lx.pos += 3;
    }
    lx.run();
    Lexed { tokens: lx.tokens, comments: lx.comments, errors: lx.errors, open: lx.open }
}

/// Whether `source` is a complete input for a front end that reads it line
/// by line (`sand repl`): it does not end inside a string, a comment or
/// brackets, its last token can end a statement (§2.3), and it does not end
/// with a doc comment (which belongs to the item that follows).
pub fn is_complete(source: &str) -> bool {
    let lexed = lex(source, 0);
    let last = lexed.tokens.iter().rev().find(|t| !matches!(t.tok, Tok::Semi | Tok::Eof));
    let ends_with_doc = lexed.comments.last().is_some_and(|c| c.kind == CommentKind::Doc && last.is_none_or(|t| c.lo >= t.hi));
    !lexed.open && last.is_none_or(|t| t.tok.ends_statement()) && !ends_with_doc
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The digits of `text` (a number literal's), without `_` separators.
fn digit_values(text: &[u8]) -> impl Iterator<Item = u8> + '_ {
    text.iter().filter(|&&b| b != b'_').map(|&b| b - b'0')
}

impl<'a> Lexer<'a> {
    fn peek(&self, off: usize) -> u8 {
        self.bytes.get(self.pos + off).copied().unwrap_or(0)
    }

    fn span(&self, lo: usize, hi: usize) -> Span {
        Span::new(self.file, lo as u32, hi as u32)
    }

    fn error(&mut self, lo: usize, hi: usize, msg: impl Into<String>) {
        self.errors.push(Diag::error(self.span(lo, hi), msg));
    }

    /// Adds a token ending at the current position.
    fn push(&mut self, tok: Tok, lo: usize) {
        self.push_span(tok, lo, self.pos);
    }

    /// Adds a token, after a statement terminator if the line break before
    /// it ends a statement (§2.3), and tracks the brackets it opens or closes.
    fn push_span(&mut self, tok: Tok, lo: usize, hi: usize) {
        let space = |b: Option<&u8>| b.is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'));
        let negates = tok == Tok::Minus && space(lo.checked_sub(1).map(|i| &self.bytes[i])) && !space(self.bytes.get(lo + 1));
        let newline_before = match self.newline.take() {
            Some(at) => {
                let in_group = matches!(self.groups.last(), Some(Group::Paren(_) | Group::Bracket | Group::Interp { .. }));
                // A line starting with `- ` continues the previous one; with `-x`, it is a new statement.
                let continues = tok.continues_line() || (tok == Tok::Minus && !negates);
                if !in_group && self.tokens.last().is_some_and(|t| t.tok.ends_statement()) && !continues {
                    let at = at as u32;
                    self.tokens.push(Token { tok: Tok::Semi, lo: at, hi: at, newline_before: false, opens_lambda: false, negates: false });
                }
                true
            }
            None => false,
        };
        let index = self.tokens.len();
        if let (Tok::FatArrow, Some(open)) = (&tok, self.closed_paren) {
            self.tokens[open].opens_lambda = true;
        }
        self.closed_paren = None;
        match tok {
            Tok::LParen => self.groups.push(Group::Paren(index)),
            Tok::LBracket => self.groups.push(Group::Bracket),
            Tok::LBrace => self.groups.push(Group::Brace),
            // A stray `]` or `}` does not end an interpolation.
            Tok::RParen | Tok::RBracket | Tok::RBrace if !matches!(self.groups.last(), Some(Group::Interp { .. })) => {
                if let Some(Group::Paren(open)) = self.groups.pop()
                    && tok == Tok::RParen
                {
                    self.closed_paren = Some(open);
                }
            }
            _ => {}
        }
        self.tokens.push(Token { tok, lo: lo as u32, hi: hi as u32, newline_before, opens_lambda: false, negates });
    }

    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            let lo = self.pos;
            let in_interp = matches!(self.groups.last(), Some(Group::Interp { .. }));
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                // An interpolation does not span lines (outside brackets).
                b'\n' if in_interp => self.unterminated_interpolation(),
                b'\n' => {
                    self.pos += 1;
                    self.newline.get_or_insert(lo);
                }
                b')' if in_interp => {
                    self.pos += 1;
                    self.end_interpolation(lo, false);
                }
                // A format spec: up to the `)`.
                b':' if in_interp => match self.bytes[lo..].iter().position(|&b| b == b')' || b == b'\n') {
                    Some(n) if self.bytes[lo + n] == b')' => {
                        self.pos = lo + n + 1;
                        self.end_interpolation(lo, true);
                    }
                    n => {
                        self.pos = n.map_or(self.bytes.len(), |n| lo + n);
                        self.unterminated_interpolation();
                    }
                },
                b'/' if self.peek(1) == b'/' => self.line_comment(),
                b'/' if self.peek(1) == b'*' => self.block_comment(),
                b'0'..=b'9' => self.number(),
                b'"' if self.peek(1) == b'"' && self.peek(2) == b'"' => self.triple_string(),
                b'"' => {
                    self.pos += 1;
                    self.string_body(StrState { lo, triple: false }, false);
                }
                b'r' if self.peek(1) == b'"' || (self.peek(1) == b'#' && self.raw_hashes_then_quote(1)) => self.raw_string(),
                b'b' if self.peek(1) == b'"' => {
                    self.pos += 2;
                    let bytes = self.bytes_body();
                    self.push(Tok::Bytes(bytes), lo);
                }
                c if is_ident_start(c) => {
                    while self.pos < self.bytes.len() && is_ident_char(self.bytes[self.pos]) {
                        self.pos += 1;
                    }
                    let tok = keyword(&self.src[lo..self.pos]).unwrap_or(Tok::Ident);
                    self.push(tok, lo);
                }
                _ => self.punct(),
            }
        }
        // The end inside interpolations: each one's string is unterminated too.
        while matches!(self.groups.last(), Some(Group::Interp { .. })) {
            self.unterminated_interpolation();
        }
        self.open |= !self.groups.is_empty();
        self.push(Tok::Eof, self.pos);
    }

    /// Ends the interpolation whose `)` (or format spec) starts at `lo` and
    /// continues its string.
    fn end_interpolation(&mut self, lo: usize, spec: bool) {
        self.push(Tok::InterpEnd { spec }, lo);
        if let Some(Group::Interp { str, .. }) = self.groups.pop() {
            self.string_body(str, true);
        }
    }

    /// Reports the innermost interpolation as unterminated, ends it here and
    /// continues its string.
    fn unterminated_interpolation(&mut self) {
        let Some(&Group::Interp { open, .. }) = self.groups.last() else { return };
        self.error(open, open + 2, "unterminated interpolation: no `)` closes this `\\(` on its line");
        self.end_interpolation(self.pos, false);
    }

    fn raw_hashes_then_quote(&self, mut off: usize) -> bool {
        while self.peek(off) == b'#' {
            off += 1;
        }
        self.peek(off) == b'"'
    }

    fn line_comment(&mut self) {
        let lo = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.pos += 1;
        }
        let hi = if self.pos > lo && self.bytes[self.pos - 1] == b'\r' { self.pos - 1 } else { self.pos };
        let text = &self.bytes[lo..hi];
        let kind = if text.starts_with(b"///") && !text.starts_with(b"////") {
            CommentKind::Doc
        } else if text.starts_with(b"//!") {
            CommentKind::ModuleDoc
        } else {
            CommentKind::Line
        };
        self.comments.push(Comment { kind, lo: lo as u32, hi: hi as u32 });
    }

    fn block_comment(&mut self) {
        let lo = self.pos;
        self.pos += 2;
        let mut depth = 1;
        while self.pos < self.bytes.len() {
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
            self.open = true;
        }
        self.comments.push(Comment { kind: CommentKind::Block, lo: lo as u32, hi: self.pos as u32 });
    }

    fn number(&mut self) {
        let lo = self.pos;
        match (self.bytes[lo], self.peek(1)) {
            (b'0', b'x' | b'X') => return self.radix_int(16),
            (b'0', b'b' | b'B') => return self.radix_int(2),
            (b'0', b'o' | b'O') => return self.radix_int(8),
            _ => {}
        }
        self.digits();
        let int = lo..self.pos;
        let mut frac = None;
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            self.pos += 1;
            let start = self.pos;
            self.digits();
            frac = Some(start..self.pos);
        }
        let mut exp = None;
        if matches!(self.peek(0), b'e' | b'E')
            && (self.peek(1).is_ascii_digit() || (matches!(self.peek(1), b'+' | b'-') && self.peek(2).is_ascii_digit()))
        {
            let start = self.pos + 1;
            self.pos += 2;
            self.digits();
            exp = Some(start..self.pos);
        }
        let num_end = self.pos;
        let mut suffix_end = self.pos;
        while suffix_end < self.bytes.len() && is_ident_char(self.bytes[suffix_end]) {
            suffix_end += 1;
        }
        if suffix_end > self.pos {
            let unit: Option<i128> = match &self.src[self.pos..suffix_end] {
                "ns" => Some(1),
                "us" => Some(1_000),
                "ms" => Some(1_000_000),
                "s" => Some(1_000_000_000),
                "m" => Some(60_000_000_000),
                "h" => Some(3_600_000_000_000),
                "d" => Some(86_400_000_000_000),
                "w" => Some(604_800_000_000_000),
                _ => None,
            };
            self.pos = suffix_end;
            match unit.map(|unit| self.nanos(int, frac, exp, unit)) {
                Some(Some(n)) => self.push(Tok::Duration(n), lo),
                Some(None) => {
                    self.error(lo, self.pos, "duration literal out of range");
                    self.push(Tok::Duration(0), lo);
                }
                None => {
                    self.error(lo, self.pos, "invalid number literal");
                    self.push(Tok::Int(0), lo);
                }
            }
            return;
        }
        let text = &self.src[lo..num_end];
        if frac.is_some() || exp.is_some() {
            let v = if text.contains('_') { text.replace('_', "").parse() } else { text.parse() };
            self.push(Tok::Float(v.unwrap_or(0.0)), lo);
            return;
        }
        // Up to 2^64 - 1, then saturated (only whether it is 2^63 matters).
        let v = digit_values(text.as_bytes()).try_fold(0u64, |v, d| v.checked_mul(10)?.checked_add(d as u64)).unwrap_or(u64::MAX);
        match i64::try_from(v) {
            Ok(v) => self.push(Tok::Int(v), lo),
            Err(_) => self.push(Tok::IntOverflow(v == 1 << 63), lo),
        }
    }

    /// A hexadecimal, binary or octal integer (after its first `0`): any
    /// 64-bit pattern, so values from 2^63 are negative (§2.5).
    fn radix_int(&mut self, radix: u32) {
        let lo = self.pos;
        self.pos += 2;
        let start = self.pos;
        while self.pos < self.bytes.len() && is_ident_char(self.bytes[self.pos]) {
            self.pos += 1;
        }
        let digits = self.bytes[start..self.pos].iter().filter(|&&b| b != b'_');
        let mut value = Some(0u64);
        let mut any = false;
        for &b in digits {
            let Some(d) = (b as char).to_digit(radix) else {
                any = false;
                break;
            };
            any = true;
            value = value.and_then(|v| v.checked_mul(radix as u64)?.checked_add(d as u64));
        }
        match value {
            _ if !any => {
                self.error(lo, self.pos, "invalid number literal");
                self.push(Tok::Int(0), lo);
            }
            Some(v) => self.push(Tok::Int(v as i64), lo),
            None => self.push(Tok::IntOverflow(false), lo),
        }
    }

    /// The exact value in nanoseconds of a duration literal with the given
    /// integer, fraction and exponent digits (the exponent with its sign)
    /// and unit; `None` if it does not fit in 64 bits. Fractions of a
    /// nanosecond are truncated.
    fn nanos(
        &self,
        int: std::ops::Range<usize>,
        frac: Option<std::ops::Range<usize>>,
        exp: Option<std::ops::Range<usize>>,
        unit: i128,
    ) -> Option<i64> {
        // The value is `m * 10^scale`, with `m * unit` within i128: digits
        // beyond that are dropped (far below a nanosecond, for a value
        // that fits in 64 bits).
        let limit = i128::MAX / unit;
        let (mut m, mut scale) = (0i128, 0i64);
        let frac = frac.unwrap_or(int.end..int.end);
        for (d, fraction) in digit_values(&self.bytes[int]).map(|d| (d, false)).chain(digit_values(&self.bytes[frac]).map(|d| (d, true))) {
            match m.checked_mul(10).and_then(|x| x.checked_add(d as i128)).filter(|&x| x <= limit) {
                Some(next) => {
                    m = next;
                    scale -= fraction as i64;
                }
                None => scale += !fraction as i64,
            }
        }
        if let Some(exp) = exp {
            let (negative, digits) = match self.bytes[exp.start] {
                b'-' => (true, exp.start + 1..exp.end),
                b'+' => (false, exp.start + 1..exp.end),
                _ => (false, exp),
            };
            let e = digit_values(&self.bytes[digits]).fold(0i64, |e, d| (e * 10 + d as i64).min(1 << 20));
            scale += if negative { -e } else { e };
        }
        let n = m * unit;
        let n = match u32::try_from(scale.unsigned_abs()) {
            _ if n == 0 => 0,
            Ok(k) if scale >= 0 => n.checked_mul(10i128.checked_pow(k)?)?,
            Ok(k) => 10i128.checked_pow(k).map_or(0, |p| n / p),
            Err(_) if scale >= 0 => return None,
            Err(_) => 0,
        };
        i64::try_from(n).ok()
    }

    fn digits(&mut self) {
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || (self.bytes[self.pos] == b'_' && self.peek(1).is_ascii_digit()))
        {
            self.pos += 1;
        }
    }

    /// Lexes an escape after its backslash: a char, a `\xHH` byte, or `None`.
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
            b'\'' => '\'',
            b'{' | b'}' => {
                let msg = format!("invalid escape `\\{}`: braces need no escape (interpolation is `\\(...)`)", c as char);
                self.error(lo, self.pos, msg);
                return None;
            }
            // Strings take `\(` before escapes.
            b'(' => {
                self.error(lo, self.pos, "invalid escape `\\(`: bytes literals have no interpolation");
                return None;
            }
            b'x' if allow_hex => {
                // Checked byte-wise: the next two bytes may split a character.
                let (hi, lo_digit) = (self.peek(0), self.peek(1));
                if hi.is_ascii_hexdigit() && lo_digit.is_ascii_hexdigit() {
                    let h = &self.src[self.pos..self.pos + 2];
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
                while p < self.bytes.len() && self.bytes[p].is_ascii_hexdigit() && p - start < 7 {
                    p += 1;
                }
                if p >= self.bytes.len() || self.bytes[p] != b'}' || p == start || p - start > 6 {
                    self.pos = p.min(self.bytes.len());
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
            0 if self.pos > self.bytes.len() => {
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

    /// Lexes the body of the string `st` from the current position: after
    /// its opening quote, or (`started`) after an interpolation. Stops at
    /// its end, or at the `\(` of an interpolation, whose expression is
    /// lexed next.
    fn string_body(&mut self, st: StrState, started: bool) {
        let text_lo = self.pos;
        let mut text = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.error(st.lo, st.lo + st.quote_len(), "unterminated string");
                self.open = true;
                return self.end_string(st, started, text, text_lo);
            }
            let c = self.bytes[self.pos];
            if c == b'"' && (!st.triple || (self.peek(1) == b'"' && self.peek(2) == b'"')) {
                return self.end_string(st, started, text, text_lo);
            }
            match c {
                b'\\' if self.peek(1) == b'(' => {
                    if !started {
                        self.push_span(Tok::StrStart, st.lo, st.lo + st.quote_len());
                    }
                    if !text.is_empty() {
                        self.push_span(Tok::StrText(text), text_lo, self.pos);
                        if st.triple {
                            let index = self.tokens.len() - 1;
                            self.blocks.last_mut().unwrap().texts.push(index);
                        }
                    }
                    let open = self.pos;
                    self.pos += 2;
                    self.push(Tok::InterpStart, open);
                    self.groups.push(Group::Interp { str: st, open });
                    return;
                }
                b'\\' => {
                    self.pos += 1;
                    if let Some(Ok(ch)) = self.escape(false) {
                        text.push(ch);
                    }
                }
                b'\n' => {
                    text.push('\n');
                    self.pos += 1;
                    if st.triple {
                        self.block_line(text.len());
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
    }

    /// Notes a line of the innermost `"""` string starting at the current
    /// position, `at` bytes into the text being lexed. A line counts toward
    /// the indentation unless it is blank; the line of the closing `"""`
    /// counts unless it is the first.
    fn block_line(&mut self, at: usize) {
        let ws = self.bytes[self.pos..].iter().take_while(|&&b| b == b' ' || b == b'\t').count();
        let rest = &self.bytes[self.pos + ws..];
        let block = self.blocks.last_mut().unwrap();
        let blank = rest.is_empty() || rest.starts_with(b"\n") || rest.starts_with(b"\r\n");
        if (!blank && !rest.starts_with(b"\"\"\"")) || (rest.starts_with(b"\"\"\"") && block.multiline) {
            block.indent = Some(block.indent.map_or(ws, |i| i.min(ws)));
        }
        if ws > 0 {
            block.lines.push((block.texts.len(), at, ws));
        }
        block.multiline = true;
    }

    /// Ends the string `st` at its closing quote (at the current position,
    /// or the end of the source), after its last `text`.
    fn end_string(&mut self, st: StrState, started: bool, mut text: String, text_lo: usize) {
        let close = self.pos;
        self.pos = (self.pos + st.quote_len()).min(self.bytes.len());
        if st.triple {
            // Remove the common indentation, then the whitespace before a
            // closing delimiter on its own line. Each text is rebuilt in one
            // pass: removing indentation in place would move the rest of the
            // text once per line.
            let block = self.blocks.pop().unwrap();
            let indent = block.indent.unwrap_or(0);
            for lines in block.lines.chunk_by(|a, b| a.0 == b.0).filter(|_| indent > 0) {
                let text = match block.texts.get(lines[0].0) {
                    Some(&i) => match &mut self.tokens[i].tok {
                        Tok::StrText(t) => t,
                        _ => unreachable!(),
                    },
                    None => &mut text,
                };
                let mut out = String::with_capacity(text.len());
                let mut kept = 0;
                for &(_, at, ws) in lines {
                    out.push_str(&text[kept..at]);
                    kept = at + ws.min(indent);
                }
                out.push_str(&text[kept..]);
                *text = out;
            }
            let trimmed_len = text.trim_end_matches([' ', '\t']).len();
            if text[..trimmed_len].ends_with('\n') {
                text.truncate(trimmed_len);
            }
        }
        if !started {
            self.push(Tok::Str(text), st.lo);
            return;
        }
        if !text.is_empty() {
            self.push_span(Tok::StrText(text), text_lo, close);
        }
        self.push(Tok::StrEnd, close);
    }

    fn triple_string(&mut self) {
        let lo = self.pos;
        self.pos += 3;
        if self.peek(0) == b'\n' {
            self.pos += 1;
        } else if self.peek(0) == b'\r' && self.peek(1) == b'\n' {
            self.pos += 2;
        }
        self.blocks.push(Block::default());
        self.block_line(0);
        self.blocks.last_mut().unwrap().multiline = false;
        self.string_body(StrState { lo, triple: true }, false);
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
            if self.pos >= self.bytes.len() {
                self.error(lo, lo + 1, "unterminated raw string");
                self.open = true;
                self.push(Tok::Str(self.src[start..].to_string()), lo);
                return;
            }
            if self.bytes[self.pos] == b'"' && (0..hashes).all(|i| self.peek(1 + i) == b'#') {
                let text = self.src[start..self.pos].to_string();
                self.pos += 1 + hashes;
                self.push(Tok::Str(text), lo);
                return;
            }
            self.pos += 1;
        }
    }

    fn bytes_body(&mut self) -> Vec<u8> {
        let lo = self.pos - 2;
        let mut out = Vec::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.error(lo, lo + 2, "unterminated bytes literal");
                self.open = true;
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
            (b'!', _) => (Tok::Bang, 1),
            (b'(', _) => (Tok::LParen, 1),
            (b')', _) => (Tok::RParen, 1),
            (b'[', _) => (Tok::LBracket, 1),
            (b']', _) => (Tok::RBracket, 1),
            (b'{', _) => (Tok::LBrace, 1),
            (b'}', _) => (Tok::RBrace, 1),
            _ => {
                let ch = self.src[self.pos..].chars().next().unwrap();
                self.pos += ch.len_utf8();
                self.error(lo, self.pos, format!("unexpected character `{}`", ch.escape_debug()));
                return;
            }
        };
        self.pos += len;
        self.push(tok, lo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src, 0).tokens.into_iter().map(|t| t.tok).collect()
    }

    fn duration(src: &str) -> Option<i64> {
        let lexed = lex(src, 0);
        match (&lexed.tokens[0].tok, lexed.errors.is_empty()) {
            (Tok::Duration(n), true) => Some(*n),
            _ => None,
        }
    }

    #[test]
    fn durations_are_exact() {
        assert_eq!(duration("1.005s"), Some(1_005_000_000));
        assert_eq!(duration("0.1s"), Some(100_000_000));
        assert_eq!(duration("2.5e-3s"), Some(2_500_000));
        assert_eq!(duration("1e3ms"), Some(1_000_000_000));
        assert_eq!(duration("1.5h"), Some(5_400_000_000_000));
        assert_eq!(duration("0.333333333333333333333333333333333333333333333s"), Some(333_333_333));
        assert_eq!(duration("1.00000000000000000000000000000000000000000000s"), Some(1_000_000_000));
        assert_eq!(duration("1.9ns"), Some(1));
        assert_eq!(duration("1_000.000_5us"), Some(1_000_000));
        assert_eq!(duration("1e-400s"), Some(0));
        assert_eq!(duration("9223372036854775807ns"), Some(i64::MAX));
        assert_eq!(duration("9223372036854775808ns"), None);
        assert_eq!(duration("1e400s"), None);
        assert_eq!(duration("0e400s"), Some(0));
    }

    #[test]
    fn hex_binary_and_octal_literals_hold_any_64_bit_pattern() {
        assert_eq!(toks("0xFFFF_FFFF_FFFF_FFFF")[0], Tok::Int(-1));
        assert_eq!(toks("0x8000000000000000")[0], Tok::Int(i64::MIN));
        assert_eq!(toks("0x7fffffffffffffff")[0], Tok::Int(i64::MAX));
        assert_eq!(toks(&format!("0b{}", "1".repeat(64)))[0], Tok::Int(-1));
        assert_eq!(toks("0o1777777777777777777777")[0], Tok::Int(-1));
        assert_eq!(toks("0x1_0000_0000_0000_0000")[0], Tok::IntOverflow(false));
        assert_eq!(toks("9223372036854775808")[0], Tok::IntOverflow(true));
        assert_eq!(toks("18446744073709551616")[0], Tok::IntOverflow(false));
        assert!(!lex("0xfg", 0).errors.is_empty());
        assert!(!lex("0x", 0).errors.is_empty());
    }

    #[test]
    fn terminators() {
        use Tok::*;
        // `-x` starts a new statement, `- x` continues the line; `*` continues it; `..` ends one.
        assert_eq!(toks("a\n-b"), [Ident, Semi, Minus, Ident, Eof]);
        assert_eq!(toks("a\n- b"), [Ident, Minus, Ident, Eof]);
        assert_eq!(toks("a\n-\nb"), [Ident, Minus, Ident, Eof]);
        assert_eq!(toks("a\n* b"), [Ident, Star, Ident, Eof]);
        assert_eq!(toks("r = 1..\nf()"), [Ident, Eq, Int(1), DotDot, Semi, Ident, LParen, RParen, Eof]);
        assert_eq!(toks("(a\nb)\n[c\n]"), [LParen, Ident, Ident, RParen, Semi, LBracket, Ident, RBracket, Eof]);
        assert_eq!(toks("f(x => {\na\n})"), [Ident, LParen, Ident, FatArrow, LBrace, Ident, Semi, RBrace, RParen, Eof]);
    }

    #[test]
    fn interpolations() {
        use Tok::*;
        assert_eq!(
            toks(r#""a\(x)b""#),
            [StrStart, StrText("a".into()), InterpStart, Ident, InterpEnd { spec: false }, StrText("b".into()), StrEnd, Eof]
        );
        assert_eq!(toks(r#""\(x:>4)""#), [StrStart, InterpStart, Ident, InterpEnd { spec: true }, StrEnd, Eof]);
        assert_eq!(toks(r#""\("\(1)")""#)[2..6], [StrStart, InterpStart, Int(1), InterpEnd { spec: false }]);
        // Parentheses inside an interpolation; a `:` inside them is not a spec.
        assert_eq!(toks(r#""\(f((a): 1))""#)[2..10], [Ident, LParen, LParen, Ident, RParen, Colon, Int(1), RParen]);
        // Braces are ordinary characters.
        assert_eq!(toks(r#""[0-9]{4} {\"a\": 1}""#), [Str("[0-9]{4} {\"a\": 1}".into()), Eof]);
        // Newlines inside an interpolation's brackets are not terminators.
        assert_eq!(toks("\"\\(f(\n1))\"")[3..6], [LParen, Int(1), RParen]);
        let errors = |src: &str| lex(src, 0).errors.into_iter().map(|e| e.message).collect::<Vec<_>>();
        assert!(errors("\"\\(x\n)\"")[0].contains("unterminated interpolation"));
        assert!(errors("\"\\(x:>4\n\"")[0].contains("unterminated interpolation"));
        assert!(errors(r#""a\{b\}""#)[0].contains("braces need no escape"));
        assert!(errors(r#"b"\(x)""#)[0].contains("no interpolation"));
    }

    #[test]
    fn block_strings_remove_the_indentation_of_their_text() {
        use Tok::*;
        assert_eq!(toks("\"\"\"\n    a\n      b\n    \"\"\""), [Str("a\n  b\n".into()), Eof]);
        assert_eq!(toks("\"\"\"\n  a\n\n  b\"\"\""), [Str("a\n\nb".into()), Eof]);
        assert_eq!(toks("\"\"\"  a\"\"\""), [Str("a".into()), Eof]);
        // Lines inside an interpolation are code, not text.
        assert_eq!(
            toks("\"\"\"\n    \\(f(\n1))\n    x\n    \"\"\""),
            [
                StrStart,
                StrText("".into()),
                InterpStart,
                Ident,
                LParen,
                Int(1),
                RParen,
                InterpEnd { spec: false },
                StrText("\nx\n".into()),
                StrEnd,
                Eof
            ]
        );
        // A block string inside an interpolation of another.
        assert_eq!(toks("\"\"\"\n  \\(\"\"\"\n    a\n    \"\"\")\n  \"\"\"")[3], Str("a\n".into()));
    }

    #[test]
    fn minus_with_space_before_and_none_after_negates() {
        let negates = |src: &str| lex(src, 0).tokens.iter().filter(|t| t.tok == Tok::Minus).map(|t| t.negates).collect::<Vec<_>>();
        assert_eq!(negates("-a - b -c (-d) a-b a- b\n-e\n- f"), [true, false, true, false, false, false, true, false]);
    }

    #[test]
    fn lambda_parameters_are_marked() {
        let lexed = lex("f((a, b) => a, (c))", 0);
        let marks: Vec<bool> = lexed.tokens.iter().filter(|t| t.tok == Tok::LParen).map(|t| t.opens_lambda).collect();
        assert_eq!(marks, [false, true, false]);
    }

    #[test]
    fn completeness() {
        for complete in ["", "x", "let x = 1", "f(1)\n", "x;", "\"a\\(1)\"", "\"{\"", "/* c */", "1..", "// c\nx"] {
            assert!(is_complete(complete), "{complete:?}");
        }
        for incomplete in
            ["f(", "\"abc", "\"a\\(1", "\"a\\(f(", "\"\"\"\n\\(", "/* c", "x +", "x.", "if x {} else", "/// doc", "r#\"x", "let x ="]
        {
            assert!(!is_complete(incomplete), "{incomplete:?}");
        }
    }
}
