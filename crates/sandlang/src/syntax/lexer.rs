//! Converts Sand source text into tokens and comments.
//!
//! The lexer inserts automatic statement terminators (§2.3) as it produces
//! tokens. It tracks open brackets and string interpolations on a stack.
//! The lexer uses this stack to decide whether `)` closes an interpolation
//! and whether a newline ends a statement.
//!
//! The lexer emits a `Str` token for a plain string literal. For an
//! interpolated string, the lexer emits a sequence of tokens:
//!
//! - `StrStart` for the opening quote
//! - `StrText` for each literal text segment
//! - `InterpStart` for `\(`
//! - the tokens of the interpolated expression
//! - `InterpEnd` for `)`
//! - `StrEnd` for the closing quote

use crate::diag::{Diag, FileId, Span};

/// Bit flag for [`TokenKind::flags`]: marks a keyword.
const KEYWORD: u8 = 1;
/// Bit flag for [`TokenKind::flags`]: a newline after this token ends the statement (§2.3).
const ENDS_STATEMENT: u8 = 2;
/// Bit flag for [`TokenKind::flags`]: a line that starts with this token continues the previous line (§2.3).
const CONTINUES_LINE: u8 = 4;

/// Generates [`TokenKind`] along with its keyword lookup and text helpers.
///
/// Each keyword and punctuation entry defines a variant name, source text,
/// and line-termination flags.
macro_rules! tokens {
    (
        keywords { $($keyword:ident $keyword_text:literal $keyword_flags:expr,)* }
        punctuation { $($punct:ident $punct_text:literal $punct_flags:expr,)* }
    ) => {
        #[derive(Clone, Debug, PartialEq)]
        pub enum TokenKind {
            Ident,
            Int(i64),
            /// An integer literal that overflows signed 64-bit integers.
            ///
            /// The boolean is `true` if the value equals `2^63`. The parser
            /// allows that value only if a unary `-` precedes it.
            IntOverflow(bool),
            Float(f64),
            Duration(i64),
            /// A plain string literal with escape sequences decoded.
            Str(String),
            Bytes(Vec<u8>),
            /// The opening quote of an interpolated string.
            StrStart,
            /// Literal text inside an interpolated string with escape sequences decoded.
            StrText(String),
            /// The opening `\(` delimiter of a string interpolation.
            InterpStart,
            /// The closing `)` of a string interpolation.
            ///
            /// If `has_format_spec` is `true`, the token span starts at the `:`
            /// of the format specifier.
            InterpEnd { has_format_spec: bool },
            /// The closing quote of an interpolated string.
            StrEnd,
            $($keyword,)*
            $($punct,)*
            Eof,
        }

        impl TokenKind {
            /// Returns the source text for a keyword or punctuation token, or `""` for other tokens.
            pub fn text(&self) -> &'static str {
                match self {
                    $(TokenKind::$keyword => $keyword_text,)*
                    $(TokenKind::$punct => $punct_text,)*
                    _ => "",
                }
            }

            fn flags(&self) -> u8 {
                match self {
                    $(TokenKind::$keyword => KEYWORD | $keyword_flags,)*
                    $(TokenKind::$punct => $punct_flags,)*
                    TokenKind::Ident
                    | TokenKind::Int(_)
                    | TokenKind::IntOverflow(_)
                    | TokenKind::Float(_)
                    | TokenKind::Duration(_)
                    | TokenKind::Str(_)
                    | TokenKind::Bytes(_)
                    | TokenKind::StrEnd => ENDS_STATEMENT,
                    TokenKind::StrStart
                    | TokenKind::StrText(_)
                    | TokenKind::InterpStart
                    | TokenKind::InterpEnd { .. }
                    | TokenKind::Eof => 0,
                }
            }
        }

        /// Returns the keyword token matching `text`, or `None` if `text` is not a keyword.
        pub fn keyword_token(text: &str) -> Option<TokenKind> {
            Some(match text {
                $($keyword_text => TokenKind::$keyword,)*
                _ => return None,
            })
        }

        /// Returns `true` if `text` is a Sand keyword.
        pub const fn is_keyword(text: &str) -> bool {
            let bytes = text.as_bytes();
            $(if bytes_eq(bytes, $keyword_text.as_bytes()) {
                return true;
            })*
            false
        }
    };
}

tokens! {
    keywords {
        And "and" CONTINUES_LINE,
        As "as" 0,
        Break "break" ENDS_STATEMENT,
        Continue "continue" ENDS_STATEMENT,
        Defer "defer" 0,
        Else "else" CONTINUES_LINE,
        Enum "enum" 0,
        Extend "extend" 0,
        False "false" ENDS_STATEMENT,
        Fn "fn" 0,
        For "for" 0,
        If "if" 0,
        Impl "impl" 0,
        Import "import" 0,
        In "in" 0,
        Inf "inf" ENDS_STATEMENT,
        Let "let" 0,
        Loop "loop" 0,
        Match "match" 0,
        Nan "nan" ENDS_STATEMENT,
        Native "native" 0,
        Not "not" 0,
        Or "or" CONTINUES_LINE,
        Pub "pub" 0,
        Return "return" ENDS_STATEMENT,
        SelfLower "self" ENDS_STATEMENT,
        SelfUpper "Self" ENDS_STATEMENT,
        Spawn "spawn" 0,
        Struct "struct" 0,
        Trait "trait" 0,
        True "true" ENDS_STATEMENT,
        Type "type" 0,
        Use "use" 0,
        Var "var" 0,
        While "while" 0,
    }
    punctuation {
        Plus "+" CONTINUES_LINE,
        // Line continuation for `-` depends on trailing whitespace (§2.3, `Token::negates`).
        Minus "-" 0,
        Star "*" CONTINUES_LINE,
        Slash "/" CONTINUES_LINE,
        Percent "%" CONTINUES_LINE,
        StarStar "**" CONTINUES_LINE,
        Amp "&" CONTINUES_LINE,
        Pipe "|" CONTINUES_LINE,
        Caret "^" CONTINUES_LINE,
        Tilde "~" 0,
        Shl "<<" CONTINUES_LINE,
        Shr ">>" CONTINUES_LINE,
        EqEq "==" CONTINUES_LINE,
        NotEq "!=" CONTINUES_LINE,
        Lt "<" CONTINUES_LINE,
        Le "<=" CONTINUES_LINE,
        Gt ">" CONTINUES_LINE,
        Ge ">=" CONTINUES_LINE,
        Eq "=" 0,
        PlusEq "+=" 0,
        MinusEq "-=" 0,
        StarEq "*=" 0,
        SlashEq "/=" 0,
        PercentEq "%=" 0,
        // Range operators can end an open-ended range such as `1..`.
        DotDot ".." ENDS_STATEMENT,
        DotDotEq "..=" ENDS_STATEMENT,
        FatArrow "=>" CONTINUES_LINE,
        Arrow "->" 0,
        QuestionQuestion "??" CONTINUES_LINE,
        Question "?" ENDS_STATEMENT,
        Dot "." CONTINUES_LINE,
        Comma "," 0,
        Colon ":" 0,
        Semi ";" 0,
        At "@" 0,
        Bang "!" 0,
        LParen "(" 0,
        RParen ")" ENDS_STATEMENT,
        LBracket "[" 0,
        RBracket "]" ENDS_STATEMENT,
        LBrace "{" 0,
        RBrace "}" ENDS_STATEMENT,
    }
}

impl TokenKind {
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident => "identifier".into(),
            TokenKind::Int(_) | TokenKind::IntOverflow(_) => "integer".into(),
            TokenKind::Float(_) => "float".into(),
            TokenKind::Duration(_) => "duration".into(),
            TokenKind::Str(_) | TokenKind::StrStart => "string".into(),
            TokenKind::StrText(_) | TokenKind::StrEnd => "end of string".into(),
            TokenKind::InterpStart => "`\\(`".into(),
            TokenKind::InterpEnd { .. } => "end of interpolation".into(),
            TokenKind::Bytes(_) => "bytes literal".into(),
            TokenKind::Semi => "newline or `;`".into(),
            TokenKind::Eof => "end of file".into(),
            other => format!("`{}`", other.text()),
        }
    }

    /// Returns `true` if this token is a Sand keyword.
    pub fn is_keyword(&self) -> bool {
        self.flags() & KEYWORD != 0
    }

    /// Returns `true` if a newline after this token ends the statement (§2.3).
    pub fn ends_statement(&self) -> bool {
        self.flags() & ENDS_STATEMENT != 0
    }

    /// Returns `true` if a line starting with this token continues the previous line (§2.3).
    pub fn continues_line(&self) -> bool {
        self.flags() & CONTINUES_LINE != 0
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub lo: u32,
    pub hi: u32,
    /// `true` if a line break appears before this token.
    pub newline_before: bool,
    /// `true` if this `(` opens a parameter list followed by `=>`.
    pub opens_lambda: bool,
    /// `true` if this `-` is a unary negation operator (§2.3).
    ///
    /// This applies to `-` preceded by whitespace or start of input and
    /// followed immediately by a non-whitespace byte, as in `-x`.
    pub negates: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    Line,
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
    /// Returns the comment text without its trailing line break.
    pub fn text<'s>(&self, source: &'s str) -> &'s str {
        &source[self.lo as usize..self.hi as usize]
    }
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    pub errors: Vec<Diag>,
    /// `true` if the source ends inside an unclosed string, bracket, or interpolation.
    pub has_unclosed_delimiter: bool,
}

/// Tracks an active string literal while the lexer scans it.
#[derive(Clone, Copy)]
struct StringState {
    /// Byte offset of the opening quote.
    open_pos: usize,
    /// `true` for a `"""` block string.
    is_triple_quoted: bool,
}

/// Indentation state for a `"""` block string (§2.5).
///
/// The lexer strips the shared indentation from all text segments after it
/// finds the closing `"""`.
#[derive(Default)]
struct BlockString {
    /// Indices of emitted `StrText` tokens in `Lexer::tokens`.
    text_token_indices: Vec<usize>,
    /// Leading-whitespace entries as `(segment_index, byte_offset, space_count)`.
    ///
    /// `segment_index` indexes `text_token_indices`, or equals its length for
    /// the text segment currently being scanned.
    indented_lines: Vec<(usize, usize, usize)>,
    /// Smallest indentation width seen on non-blank content lines (§2.5).
    min_indent: Option<usize>,
    /// `true` after the lexer has advanced past the first line of the block.
    is_multiline: bool,
}

impl StringState {
    fn quote_len(&self) -> usize {
        if self.is_triple_quoted { 3 } else { 1 }
    }
}

/// An unclosed bracket or string interpolation on the lexer stack.
enum OpenGroup {
    /// An open `(` paired with its index in `Lexer::tokens`.
    Paren(usize),
    Bracket,
    Brace,
    /// An open `\(` interpolation inside a string literal.
    Interpolation {
        string: StringState,
        open_pos: usize,
    },
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    file_id: FileId,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    errors: Vec<Diag>,
    /// Byte offset of the first newline seen since the last token.
    pending_newline_pos: Option<usize>,
    /// Active brackets and interpolations, with the innermost last.
    open_groups: Vec<OpenGroup>,
    /// Token index of the `(` closed by the previous token, if any.
    closed_paren_index: Option<usize>,
    /// Active `"""` block strings, with the innermost last.
    block_strings: Vec<BlockString>,
    has_unclosed_delimiter: bool,
}

pub fn lex(source: &str, file_id: FileId) -> Lexed {
    let mut lexer = Lexer {
        source,
        bytes: source.as_bytes(),
        pos: 0,
        file_id,
        tokens: Vec::with_capacity(source.len() / 4),
        comments: Vec::new(),
        errors: Vec::new(),
        pending_newline_pos: None,
        open_groups: Vec::new(),
        closed_paren_index: None,
        block_strings: Vec::new(),
        has_unclosed_delimiter: false,
    };
    if source.starts_with('\u{feff}') {
        lexer.pos += 3;
    }
    lexer.lex_all();
    Lexed { tokens: lexer.tokens, comments: lexer.comments, errors: lexer.errors, has_unclosed_delimiter: lexer.has_unclosed_delimiter }
}

/// Returns `true` if `source` forms a complete input for line-by-line readers
/// such as `sand repl`.
///
/// Complete input has three properties:
///
/// - All strings, brackets, and interpolations are closed.
/// - The final non-terminator token can end a statement (§2.3).
/// - The input does not end with a trailing doc comment.
pub fn is_complete(source: &str) -> bool {
    let lexed = lex(source, 0);
    let last = lexed.tokens.iter().rev().find(|t| !matches!(t.kind, TokenKind::Semi | TokenKind::Eof));
    let ends_with_doc = lexed.comments.last().is_some_and(|c| c.kind == CommentKind::Doc && last.is_none_or(|t| c.lo >= t.hi));
    !lexed.has_unclosed_delimiter && last.is_none_or(|t| t.kind.ends_statement()) && !ends_with_doc
}

/// Returns `true` if `text` matches Sand identifier syntax.
///
/// Keywords also return `true`.
pub const fn is_ident(text: &str) -> bool {
    let b = text.as_bytes();
    if b.is_empty() || !is_ident_start(b[0]) {
        return false;
    }
    let mut i = 1;
    while i < b.len() {
        if !is_ident_char(b[i]) {
            return false;
        }
        i += 1;
    }
    true
}

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
const fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Yields the decimal digit values of `text`, skipping `_` separators.
fn digit_values(text: &[u8]) -> impl Iterator<Item = u8> + '_ {
    text.iter().filter(|&&b| b != b'_').map(|&b| b - b'0')
}

impl<'a> Lexer<'a> {
    fn peek_byte(&self, offset: usize) -> u8 {
        self.bytes.get(self.pos + offset).copied().unwrap_or(0)
    }

    fn span(&self, lo: usize, hi: usize) -> Span {
        Span::new(self.file_id, lo as u32, hi as u32)
    }

    fn error(&mut self, lo: usize, hi: usize, msg: impl Into<String>) {
        self.errors.push(Diag::error(self.span(lo, hi), msg));
    }

    /// Emits a token spanning from `lo` to the current byte position.
    fn push_token(&mut self, kind: TokenKind, lo: usize) {
        self.push_token_span(kind, lo, self.pos);
    }

    /// Emits a token for `lo..hi` and updates bracket tracking.
    ///
    /// Inserts an automatic `;` token first if a preceding line break ends the
    /// previous statement (§2.3).
    fn push_token_span(&mut self, kind: TokenKind, lo: usize, hi: usize) {
        let is_space = |b: Option<&u8>| b.is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'));
        let negates = kind == TokenKind::Minus && is_space(lo.checked_sub(1).map(|i| &self.bytes[i])) && !is_space(self.bytes.get(lo + 1));
        let newline_before = match self.pending_newline_pos.take() {
            Some(at) => {
                let in_group =
                    matches!(self.open_groups.last(), Some(OpenGroup::Paren(_) | OpenGroup::Bracket | OpenGroup::Interpolation { .. }));
                // A leading `- ` continues the previous line, whereas `-x` starts a new statement.
                let continues = kind.continues_line() || (kind == TokenKind::Minus && !negates);
                let ends = match self.tokens.as_slice() {
                    // A keyword after `.` is a member name (`x.or`).
                    [.., Token { kind: TokenKind::Dot, .. }, t] if t.kind.is_keyword() => true,
                    [.., t] => t.kind.ends_statement(),
                    [] => false,
                };
                if !in_group && ends && !continues {
                    let at = at as u32;
                    self.tokens.push(Token {
                        kind: TokenKind::Semi,
                        lo: at,
                        hi: at,
                        newline_before: false,
                        opens_lambda: false,
                        negates: false,
                    });
                }
                true
            }
            None => false,
        };
        let index = self.tokens.len();
        if let (TokenKind::FatArrow, Some(open)) = (&kind, self.closed_paren_index) {
            self.tokens[open].opens_lambda = true;
        }
        self.closed_paren_index = None;
        match kind {
            TokenKind::LParen => self.open_groups.push(OpenGroup::Paren(index)),
            TokenKind::LBracket => self.open_groups.push(OpenGroup::Bracket),
            TokenKind::LBrace => self.open_groups.push(OpenGroup::Brace),
            // A stray `]` or `}` does not end an interpolation.
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace
                if !matches!(self.open_groups.last(), Some(OpenGroup::Interpolation { .. })) =>
            {
                if let Some(OpenGroup::Paren(open)) = self.open_groups.pop()
                    && kind == TokenKind::RParen
                {
                    self.closed_paren_index = Some(open);
                }
            }
            _ => {}
        }
        self.tokens.push(Token { kind, lo: lo as u32, hi: hi as u32, newline_before, opens_lambda: false, negates });
    }

    fn lex_all(&mut self) {
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            let lo = self.pos;
            let in_interp = matches!(self.open_groups.last(), Some(OpenGroup::Interpolation { .. }));
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                // Interpolations cannot contain unbracketed newlines.
                b'\n' if in_interp => self.finish_unterminated_interpolation(),
                b'\n' => {
                    self.pos += 1;
                    self.pending_newline_pos.get_or_insert(lo);
                }
                b')' if in_interp => {
                    self.pos += 1;
                    self.end_interpolation(lo, false);
                }
                // Scan a format specifier up to the closing `)`.
                b':' if in_interp => match self.bytes[lo..].iter().position(|&b| b == b')' || b == b'\n') {
                    Some(n) if self.bytes[lo + n] == b')' => {
                        self.pos = lo + n + 1;
                        self.end_interpolation(lo, true);
                    }
                    n => {
                        self.pos = n.map_or(self.bytes.len(), |n| lo + n);
                        self.finish_unterminated_interpolation();
                    }
                },
                b'/' if self.peek_byte(1) == b'/' => self.lex_line_comment(),
                b'/' if self.peek_byte(1) == b'*' => self.reject_block_comment(),
                b'0'..=b'9' => self.lex_number(),
                b'"' if self.peek_byte(1) == b'"' && self.peek_byte(2) == b'"' => self.lex_triple_string(),
                b'"' => {
                    self.pos += 1;
                    self.lex_string_body(StringState { open_pos: lo, is_triple_quoted: false }, false);
                }
                b'r' if self.peek_byte(1) == b'"' || (self.peek_byte(1) == b'#' && self.has_raw_string_opener(1)) => self.lex_raw_string(),
                b'b' if self.peek_byte(1) == b'"' => {
                    self.pos += 2;
                    let bytes = self.lex_bytes_body();
                    self.push_token(TokenKind::Bytes(bytes), lo);
                }
                c if is_ident_start(c) => {
                    while self.pos < self.bytes.len() && is_ident_char(self.bytes[self.pos]) {
                        self.pos += 1;
                    }
                    let kind = keyword_token(&self.source[lo..self.pos]).unwrap_or(TokenKind::Ident);
                    self.push_token(kind, lo);
                }
                _ => self.lex_punctuation(),
            }
        }
        // Close any open interpolations at end of file so their outer strings are also reported as unterminated.
        while matches!(self.open_groups.last(), Some(OpenGroup::Interpolation { .. })) {
            self.finish_unterminated_interpolation();
        }
        self.has_unclosed_delimiter |= !self.open_groups.is_empty();
        self.push_token(TokenKind::Eof, self.pos);
    }

    /// Emits `InterpEnd` starting at `lo` and resumes scanning the enclosing string.
    fn end_interpolation(&mut self, lo: usize, has_format_spec: bool) {
        self.push_token(TokenKind::InterpEnd { has_format_spec }, lo);
        if let Some(OpenGroup::Interpolation { string, .. }) = self.open_groups.pop() {
            self.lex_string_body(string, true);
        }
    }

    /// Reports the innermost interpolation as unterminated and resumes scanning
    /// its enclosing string.
    fn finish_unterminated_interpolation(&mut self) {
        let Some(&OpenGroup::Interpolation { open_pos, .. }) = self.open_groups.last() else { return };
        self.error(open_pos, open_pos + 2, "unterminated interpolation: no `)` closes this `\\(` on its line");
        self.end_interpolation(self.pos, false);
    }

    fn has_raw_string_opener(&self, mut offset: usize) -> bool {
        while self.peek_byte(offset) == b'#' {
            offset += 1;
        }
        self.peek_byte(offset) == b'"'
    }

    fn lex_line_comment(&mut self) {
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

    /// Reports an error for `/*` and skips through the matching `*/` to suppress
    /// cascading errors.
    fn reject_block_comment(&mut self) {
        let lo = self.pos;
        self.error(lo, lo + 2, "Sand has no block comments: start each comment line with `//`");
        self.pos = self.source[lo + 2..].find("*/").map_or(self.bytes.len(), |n| lo + n + 4);
    }

    fn lex_number(&mut self) {
        let lo = self.pos;
        match (self.bytes[lo], self.peek_byte(1)) {
            (b'0', b'x' | b'X') => return self.lex_radix_int(16),
            (b'0', b'b' | b'B') => return self.lex_radix_int(2),
            (b'0', b'o' | b'O') => return self.lex_radix_int(8),
            _ => {}
        }
        self.skip_digits();
        let int = lo..self.pos;
        let mut frac = None;
        if self.peek_byte(0) == b'.' && self.peek_byte(1).is_ascii_digit() {
            self.pos += 1;
            let start = self.pos;
            self.skip_digits();
            frac = Some(start..self.pos);
        }
        let mut exp = None;
        if matches!(self.peek_byte(0), b'e' | b'E')
            && (self.peek_byte(1).is_ascii_digit() || (matches!(self.peek_byte(1), b'+' | b'-') && self.peek_byte(2).is_ascii_digit()))
        {
            let start = self.pos + 1;
            self.pos += 2;
            self.skip_digits();
            exp = Some(start..self.pos);
        }
        let num_end = self.pos;
        let mut suffix_end = self.pos;
        while suffix_end < self.bytes.len() && is_ident_char(self.bytes[suffix_end]) {
            suffix_end += 1;
        }
        if suffix_end > self.pos {
            let unit: Option<i128> = match &self.source[self.pos..suffix_end] {
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
            match unit.map(|unit| self.duration_nanos(int, frac, exp, unit)) {
                Some(Some(n)) => self.push_token(TokenKind::Duration(n), lo),
                Some(None) => {
                    self.error(lo, self.pos, "duration literal out of range");
                    self.push_token(TokenKind::Duration(0), lo);
                }
                None => {
                    self.error(lo, self.pos, "invalid number literal");
                    self.push_token(TokenKind::Int(0), lo);
                }
            }
            return;
        }
        let text = &self.source[lo..num_end];
        if frac.is_some() || exp.is_some() {
            let v = if text.contains('_') { text.replace('_', "").parse() } else { text.parse() };
            self.push_token(TokenKind::Float(v.unwrap_or(0.0)), lo);
            return;
        }
        // Saturate at `u64::MAX` because overflow handling only needs to distinguish `2^63`.
        let v = digit_values(text.as_bytes()).try_fold(0u64, |v, d| v.checked_mul(10)?.checked_add(d as u64)).unwrap_or(u64::MAX);
        match i64::try_from(v) {
            Ok(v) => self.push_token(TokenKind::Int(v), lo),
            Err(_) => self.push_token(TokenKind::IntOverflow(v == 1 << 63), lo),
        }
    }

    /// Lexes a hexadecimal, binary, or octal integer literal (§2.5).
    ///
    /// Any 64-bit bit pattern is accepted, so values at or above `2^63` wrap
    /// to negative `i64` values.
    fn lex_radix_int(&mut self, radix: u32) {
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
                self.push_token(TokenKind::Int(0), lo);
            }
            Some(v) => self.push_token(TokenKind::Int(v as i64), lo),
            None => self.push_token(TokenKind::IntOverflow(false), lo),
        }
    }

    /// Computes the exact duration in nanoseconds, or `None` if it overflows `i64`.
    ///
    /// `exp` includes its optional sign. Sub-nanosecond fractions are truncated.
    fn duration_nanos(
        &self,
        int: std::ops::Range<usize>,
        frac: Option<std::ops::Range<usize>>,
        exp: Option<std::ops::Range<usize>>,
        unit: i128,
    ) -> Option<i64> {
        // Represent the value as `mantissa * 10^scale` while keeping `mantissa * unit`
        // within `i128`. Any extra trailing digits are far below 1 ns for values
        // that fit in `i64`.
        let limit = i128::MAX / unit;
        let (mut mantissa, mut scale) = (0i128, 0i64);
        let frac = frac.unwrap_or(int.end..int.end);
        for (d, fraction) in digit_values(&self.bytes[int]).map(|d| (d, false)).chain(digit_values(&self.bytes[frac]).map(|d| (d, true))) {
            match mantissa.checked_mul(10).and_then(|x| x.checked_add(d as i128)).filter(|&x| x <= limit) {
                Some(next) => {
                    mantissa = next;
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
        let n = mantissa * unit;
        let n = match u32::try_from(scale.unsigned_abs()) {
            _ if n == 0 => 0,
            Ok(k) if scale >= 0 => n.checked_mul(10i128.checked_pow(k)?)?,
            Ok(k) => 10i128.checked_pow(k).map_or(0, |p| n / p),
            Err(_) if scale >= 0 => return None,
            Err(_) => 0,
        };
        i64::try_from(n).ok()
    }

    fn skip_digits(&mut self) {
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || (self.bytes[self.pos] == b'_' && self.peek_byte(1).is_ascii_digit()))
        {
            self.pos += 1;
        }
    }

    /// Lexes the escape sequence after `\`, returning a `char`, a raw `\xHH`
    /// byte, or `None` on error.
    fn lex_escape(&mut self, allow_hex: bool) -> Option<Result<char, u8>> {
        let lo = self.pos - 1;
        let c = self.peek_byte(0);
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
            // Regular strings handle `\(` before calling `lex_escape`.
            b'(' => {
                self.error(lo, self.pos, "invalid escape `\\(`: bytes literals have no interpolation");
                return None;
            }
            b'x' if allow_hex => {
                // Inspect bytes directly because the next two bytes can split a UTF-8 character.
                let (hi, lo_digit) = (self.peek_byte(0), self.peek_byte(1));
                if hi.is_ascii_hexdigit() && lo_digit.is_ascii_hexdigit() {
                    let h = &self.source[self.pos..self.pos + 2];
                    self.pos += 2;
                    return Some(Err(u8::from_str_radix(h, 16).unwrap()));
                }
                self.error(lo, self.pos, "invalid escape: expected two hex digits after \\x");
                return None;
            }
            b'u' => {
                if self.peek_byte(0) != b'{' {
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
                let v = u32::from_str_radix(&self.source[start..p], 16).unwrap();
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
                // Advance past the full UTF-8 character.
                let ch_len = self.source[self.pos - 1..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                self.pos = self.pos - 1 + ch_len;
                self.error(lo, self.pos, "invalid escape sequence");
                return None;
            }
        };
        Some(Ok(ch))
    }

    /// Lexes the body of `state` from the current position until its closing
    /// quote or the next `\(` interpolation.
    ///
    /// `has_interpolations` is `true` after the first interpolation in the string.
    fn lex_string_body(&mut self, state: StringState, has_interpolations: bool) {
        let text_lo = self.pos;
        let mut text = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.error(state.open_pos, state.open_pos + state.quote_len(), "unterminated string");
                self.has_unclosed_delimiter = true;
                return self.finish_string(state, has_interpolations, text, text_lo);
            }
            let c = self.bytes[self.pos];
            if c == b'"' && (!state.is_triple_quoted || (self.peek_byte(1) == b'"' && self.peek_byte(2) == b'"')) {
                return self.finish_string(state, has_interpolations, text, text_lo);
            }
            match c {
                b'\\' if self.peek_byte(1) == b'(' => {
                    if !has_interpolations {
                        self.push_token_span(TokenKind::StrStart, state.open_pos, state.open_pos + state.quote_len());
                    }
                    if !text.is_empty() {
                        self.push_token_span(TokenKind::StrText(text), text_lo, self.pos);
                        if state.is_triple_quoted {
                            let index = self.tokens.len() - 1;
                            self.block_strings.last_mut().unwrap().text_token_indices.push(index);
                        }
                    }
                    let open_pos = self.pos;
                    self.pos += 2;
                    self.push_token(TokenKind::InterpStart, open_pos);
                    self.open_groups.push(OpenGroup::Interpolation { string: state, open_pos });
                    return;
                }
                b'\\' => {
                    self.pos += 1;
                    if let Some(Ok(ch)) = self.lex_escape(false) {
                        text.push(ch);
                    }
                }
                b'\n' => {
                    text.push('\n');
                    self.pos += 1;
                    if state.is_triple_quoted {
                        self.record_block_line(text.len());
                    }
                }
                b'\r' if self.peek_byte(1) == b'\n' => {
                    self.pos += 1;
                }
                _ => {
                    let ch = self.source[self.pos..].chars().next().unwrap();
                    text.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Records the leading whitespace of a line in the innermost `"""` string.
    ///
    /// `text_offset` is the byte offset of this line inside the current text
    /// segment. Non-blank lines and a closing `"""` line after the first line
    /// contribute to the shared indentation width.
    fn record_block_line(&mut self, text_offset: usize) {
        let ws = self.bytes[self.pos..].iter().take_while(|&&b| b == b' ' || b == b'\t').count();
        let rest = &self.bytes[self.pos + ws..];
        let block = self.block_strings.last_mut().unwrap();
        let blank = rest.is_empty() || rest.starts_with(b"\n") || rest.starts_with(b"\r\n");
        if (!blank && !rest.starts_with(b"\"\"\"")) || (rest.starts_with(b"\"\"\"") && block.is_multiline) {
            block.min_indent = Some(block.min_indent.map_or(ws, |i| i.min(ws)));
        }
        if ws > 0 {
            block.indented_lines.push((block.text_token_indices.len(), text_offset, ws));
        }
        block.is_multiline = true;
    }

    /// Emits the final tokens for `state` at the closing quote or end of file.
    fn finish_string(&mut self, state: StringState, has_interpolations: bool, mut text: String, text_lo: usize) {
        let close = self.pos;
        self.pos = (self.pos + state.quote_len()).min(self.bytes.len());
        if state.is_triple_quoted {
            // Strip the shared indentation in a single pass per segment, then
            // remove trailing spaces before a closing `"""` on its own line.
            let block = self.block_strings.pop().unwrap();
            let indent = block.min_indent.unwrap_or(0);
            for lines in block.indented_lines.chunk_by(|a, b| a.0 == b.0).filter(|_| indent > 0) {
                let text = match block.text_token_indices.get(lines[0].0) {
                    Some(&i) => match &mut self.tokens[i].kind {
                        TokenKind::StrText(t) => t,
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
        if !has_interpolations {
            self.push_token(TokenKind::Str(text), state.open_pos);
            return;
        }
        if !text.is_empty() {
            self.push_token_span(TokenKind::StrText(text), text_lo, close);
        }
        self.push_token(TokenKind::StrEnd, close);
    }

    fn lex_triple_string(&mut self) {
        let lo = self.pos;
        self.pos += 3;
        if self.peek_byte(0) == b'\n' {
            self.pos += 1;
        } else if self.peek_byte(0) == b'\r' && self.peek_byte(1) == b'\n' {
            self.pos += 2;
        }
        self.block_strings.push(BlockString::default());
        self.record_block_line(0);
        self.block_strings.last_mut().unwrap().is_multiline = false;
        self.lex_string_body(StringState { open_pos: lo, is_triple_quoted: true }, false);
    }

    fn lex_raw_string(&mut self) {
        let lo = self.pos;
        self.pos += 1;
        let mut hashes = 0;
        while self.peek_byte(0) == b'#' {
            hashes += 1;
            self.pos += 1;
        }
        self.pos += 1;
        let start = self.pos;
        loop {
            if self.pos >= self.bytes.len() {
                self.error(lo, lo + 1, "unterminated raw string");
                self.has_unclosed_delimiter = true;
                self.push_token(TokenKind::Str(self.source[start..].to_string()), lo);
                return;
            }
            if self.bytes[self.pos] == b'"' && (0..hashes).all(|i| self.peek_byte(1 + i) == b'#') {
                let text = self.source[start..self.pos].to_string();
                self.pos += 1 + hashes;
                self.push_token(TokenKind::Str(text), lo);
                return;
            }
            self.pos += 1;
        }
    }

    fn lex_bytes_body(&mut self) -> Vec<u8> {
        let lo = self.pos - 2;
        let mut out = Vec::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.error(lo, lo + 2, "unterminated bytes literal");
                self.has_unclosed_delimiter = true;
                break;
            }
            let c = self.bytes[self.pos];
            if c == b'"' {
                self.pos += 1;
                break;
            }
            if c == b'\\' {
                self.pos += 1;
                match self.lex_escape(true) {
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

    fn lex_punctuation(&mut self) {
        let lo = self.pos;
        let c = self.bytes[self.pos];
        let n = self.peek_byte(1);
        let n2 = self.peek_byte(2);
        let (kind, len) = match (c, n) {
            (b'+', b'=') => (TokenKind::PlusEq, 2),
            (b'+', _) => (TokenKind::Plus, 1),
            (b'-', b'=') => (TokenKind::MinusEq, 2),
            (b'-', b'>') => (TokenKind::Arrow, 2),
            (b'-', _) => (TokenKind::Minus, 1),
            (b'*', b'*') => (TokenKind::StarStar, 2),
            (b'*', b'=') => (TokenKind::StarEq, 2),
            (b'*', _) => (TokenKind::Star, 1),
            (b'/', b'=') => (TokenKind::SlashEq, 2),
            (b'/', _) => (TokenKind::Slash, 1),
            (b'%', b'=') => (TokenKind::PercentEq, 2),
            (b'%', _) => (TokenKind::Percent, 1),
            (b'&', _) => (TokenKind::Amp, 1),
            (b'|', _) => (TokenKind::Pipe, 1),
            (b'^', _) => (TokenKind::Caret, 1),
            (b'~', _) => (TokenKind::Tilde, 1),
            (b'<', b'<') => (TokenKind::Shl, 2),
            (b'<', b'=') => (TokenKind::Le, 2),
            (b'<', _) => (TokenKind::Lt, 1),
            (b'>', b'>') => (TokenKind::Shr, 2),
            (b'>', b'=') => (TokenKind::Ge, 2),
            (b'>', _) => (TokenKind::Gt, 1),
            (b'=', b'=') => (TokenKind::EqEq, 2),
            (b'=', b'>') => (TokenKind::FatArrow, 2),
            (b'=', _) => (TokenKind::Eq, 1),
            (b'!', b'=') => (TokenKind::NotEq, 2),
            (b'.', b'.') if n2 == b'=' => (TokenKind::DotDotEq, 3),
            (b'.', b'.') => (TokenKind::DotDot, 2),
            (b'.', _) => (TokenKind::Dot, 1),
            (b'?', b'?') => (TokenKind::QuestionQuestion, 2),
            (b'?', _) => (TokenKind::Question, 1),
            (b',', _) => (TokenKind::Comma, 1),
            (b':', _) => (TokenKind::Colon, 1),
            (b';', _) => (TokenKind::Semi, 1),
            (b'@', _) => (TokenKind::At, 1),
            (b'!', _) => (TokenKind::Bang, 1),
            (b'(', _) => (TokenKind::LParen, 1),
            (b')', _) => (TokenKind::RParen, 1),
            (b'[', _) => (TokenKind::LBracket, 1),
            (b']', _) => (TokenKind::RBracket, 1),
            (b'{', _) => (TokenKind::LBrace, 1),
            (b'}', _) => (TokenKind::RBrace, 1),
            _ => {
                let ch = self.source[self.pos..].chars().next().unwrap();
                self.pos += ch.len_utf8();
                self.error(lo, self.pos, format!("unexpected character `{}`", ch.escape_debug()));
                return;
            }
        };
        self.pos += len;
        self.push_token(kind, lo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_kinds(source: &str) -> Vec<TokenKind> {
        lex(source, 0).tokens.into_iter().map(|t| t.kind).collect()
    }

    fn duration(source: &str) -> Option<i64> {
        let lexed = lex(source, 0);
        match (&lexed.tokens[0].kind, lexed.errors.is_empty()) {
            (TokenKind::Duration(n), true) => Some(*n),
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
        assert_eq!(token_kinds("0xFFFF_FFFF_FFFF_FFFF")[0], TokenKind::Int(-1));
        assert_eq!(token_kinds("0x8000000000000000")[0], TokenKind::Int(i64::MIN));
        assert_eq!(token_kinds("0x7fffffffffffffff")[0], TokenKind::Int(i64::MAX));
        assert_eq!(token_kinds(&format!("0b{}", "1".repeat(64)))[0], TokenKind::Int(-1));
        assert_eq!(token_kinds("0o1777777777777777777777")[0], TokenKind::Int(-1));
        assert_eq!(token_kinds("0x1_0000_0000_0000_0000")[0], TokenKind::IntOverflow(false));
        assert_eq!(token_kinds("9223372036854775808")[0], TokenKind::IntOverflow(true));
        assert_eq!(token_kinds("18446744073709551616")[0], TokenKind::IntOverflow(false));
        assert!(!lex("0xfg", 0).errors.is_empty());
        assert!(!lex("0x", 0).errors.is_empty());
    }

    #[test]
    fn terminators() {
        use TokenKind::*;
        // `-x` starts a new statement, `- x` and `*` continue the line, and `..` ends a statement.
        assert_eq!(token_kinds("a\n-b"), [Ident, Semi, Minus, Ident, Eof]);
        assert_eq!(token_kinds("a\n- b"), [Ident, Minus, Ident, Eof]);
        assert_eq!(token_kinds("a\n-\nb"), [Ident, Minus, Ident, Eof]);
        assert_eq!(token_kinds("a\n* b"), [Ident, Star, Ident, Eof]);
        assert_eq!(token_kinds("r = 1..\nf()"), [Ident, Eq, Int(1), DotDot, Semi, Ident, LParen, RParen, Eof]);
        assert_eq!(token_kinds("(a\nb)\n[c\n]"), [LParen, Ident, Ident, RParen, Semi, LBracket, Ident, RBracket, Eof]);
        assert_eq!(token_kinds("f(x => {\na\n})"), [Ident, LParen, Ident, FatArrow, LBrace, Ident, Semi, RBrace, RParen, Eof]);
    }

    #[test]
    fn interpolations() {
        use TokenKind::*;
        assert_eq!(
            token_kinds(r#""a\(x)b""#),
            [StrStart, StrText("a".into()), InterpStart, Ident, InterpEnd { has_format_spec: false }, StrText("b".into()), StrEnd, Eof]
        );
        assert_eq!(token_kinds(r#""\(x:>4)""#), [StrStart, InterpStart, Ident, InterpEnd { has_format_spec: true }, StrEnd, Eof]);
        assert_eq!(token_kinds(r#""\("\(1)")""#)[2..6], [StrStart, InterpStart, Int(1), InterpEnd { has_format_spec: false }]);
        // A `:` inside nested parentheses is ordinary syntax, not a format specifier.
        assert_eq!(token_kinds(r#""\(f((a): 1))""#)[2..10], [Ident, LParen, LParen, Ident, RParen, Colon, Int(1), RParen]);
        // Braces are ordinary characters.
        assert_eq!(token_kinds(r#""[0-9]{4} {\"a\": 1}""#), [Str("[0-9]{4} {\"a\": 1}".into()), Eof]);
        // Newlines inside an interpolation's brackets are not terminators.
        assert_eq!(token_kinds("\"\\(f(\n1))\"")[3..6], [LParen, Int(1), RParen]);
        let errors = |source: &str| lex(source, 0).errors.into_iter().map(|e| e.message).collect::<Vec<_>>();
        assert!(errors("\"\\(x\n)\"")[0].contains("unterminated interpolation"));
        assert!(errors("\"\\(x:>4\n\"")[0].contains("unterminated interpolation"));
        assert!(errors(r#""a\{b\}""#)[0].contains("braces need no escape"));
        assert!(errors(r#"b"\(x)""#)[0].contains("no interpolation"));
    }

    #[test]
    fn block_strings_remove_the_indentation_of_their_text() {
        use TokenKind::*;
        assert_eq!(token_kinds("\"\"\"\n    a\n      b\n    \"\"\""), [Str("a\n  b\n".into()), Eof]);
        assert_eq!(token_kinds("\"\"\"\n  a\n\n  b\"\"\""), [Str("a\n\nb".into()), Eof]);
        assert_eq!(token_kinds("\"\"\"  a\"\"\""), [Str("a".into()), Eof]);
        // Lines inside an interpolation are code, not text.
        assert_eq!(
            token_kinds("\"\"\"\n    \\(f(\n1))\n    x\n    \"\"\""),
            [
                StrStart,
                StrText("".into()),
                InterpStart,
                Ident,
                LParen,
                Int(1),
                RParen,
                InterpEnd { has_format_spec: false },
                StrText("\nx\n".into()),
                StrEnd,
                Eof
            ]
        );
        // Nested block string inside an interpolation.
        assert_eq!(token_kinds("\"\"\"\n  \\(\"\"\"\n    a\n    \"\"\")\n  \"\"\"")[3], Str("a\n".into()));
    }

    #[test]
    fn minus_with_space_before_and_none_after_negates() {
        let negates =
            |source: &str| lex(source, 0).tokens.iter().filter(|t| t.kind == TokenKind::Minus).map(|t| t.negates).collect::<Vec<_>>();
        assert_eq!(negates("-a - b -c (-d) a-b a- b\n-e\n- f"), [true, false, true, false, false, false, true, false]);
    }

    #[test]
    fn lambda_parameters_are_marked() {
        let lexed = lex("f((a, b) => a, (c))", 0);
        let marks: Vec<bool> = lexed.tokens.iter().filter(|t| t.kind == TokenKind::LParen).map(|t| t.opens_lambda).collect();
        assert_eq!(marks, [false, true, false]);
    }

    #[test]
    fn slash_star_is_one_error() {
        let lexed = lex("let x = 1 /* a\n * b */ + 2\n/* c", 0);
        let errors: Vec<_> = lexed.errors.iter().map(|e| (e.span.lo, e.message.as_str())).collect();
        let msg = "Sand has no block comments: start each comment line with `//`";
        assert_eq!(errors, [(10, msg), (27, msg)]);
        assert!(lexed.comments.is_empty());
        assert_eq!(lexed.tokens.iter().filter(|t| t.kind == TokenKind::Plus).count(), 1);
    }

    #[test]
    fn completeness() {
        for complete in ["", "x", "let x = 1", "f(1)\n", "x;", "\"a\\(1)\"", "\"{\"", "1..", "// c\nx"] {
            assert!(is_complete(complete), "{complete:?}");
        }
        for incomplete in ["f(", "\"abc", "\"a\\(1", "\"a\\(f(", "\"\"\"\n\\(", "x +", "x.", "if x {} else", "/// doc", "r#\"x", "let x ="]
        {
            assert!(!is_complete(incomplete), "{incomplete:?}");
        }
    }
}
