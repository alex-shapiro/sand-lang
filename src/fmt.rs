//! The source formatter (`sand fmt`, spec §17.3).
//!
//! Formatting is done in two steps. The syntax tree is first converted into a
//! [`Doc`], a Wadler-style layout tree of text, potential line breaks, groups
//! and indentation; the doc is then rendered at a fixed line width, breaking
//! every group whose content does not fit on the current line.
//!
//! Literals are copied verbatim from the source. Comments are not part of the
//! syntax tree, so they are tracked by byte position: comments between the
//! elements of a list (items, statements, fields, match arms, arguments, ...)
//! become leading or trailing comments of those elements, and any comment found
//! elsewhere is emitted in place (block comments) or after the enclosing element
//! (line comments). No comment is ever dropped.

use crate::diag::{SourceFile, Span};
use crate::syntax::ast::*;
use crate::syntax::lexer::{Comment, CommentKind, Tok, Token, lex};
use crate::syntax::parse_module;

/// Maximum line width.
const WIDTH: usize = 100;
/// Spaces per indentation level.
const INDENT: usize = 4;

/// Formats a source file in canonical style; fails if it does not parse.
pub fn format_source(src: &str) -> Result<String, String> {
    // The formatter recurses over the syntax tree; like the compiler, it runs
    // on a thread with a large stack so maximally nested input cannot overflow.
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(s, || format_on_this_thread(src))
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "internal formatter error".to_string())?
    })
}

fn format_on_this_thread(src: &str) -> Result<String, String> {
    let parsed = parse_module(src, 0);
    if let Some(err) = parsed.errors.first() {
        let (line, col) = SourceFile::new("", src).line_col(err.span.lo);
        return Err(format!("{line}:{col}: {}", err.message));
    }
    let mut f = Formatter::new(src, &parsed.comments);
    let doc = f.module(&parsed.module);
    Ok(render(&doc))
}

// ----------------------------------------------------------------------
// Layout documents

#[derive(Debug)]
enum Doc {
    Text(String),
    /// A trailing line comment: printed like text, but ends the line for the
    /// purpose of measuring (it is always followed by a line break).
    Trailing(String),
    /// A space, or a line break if the enclosing group is broken.
    Line,
    /// Nothing, or a line break if the enclosing group is broken.
    SoftLine,
    /// Always a line break.
    HardLine,
    /// `.0` if the enclosing group is broken, `.1` otherwise.
    IfBreak(&'static str, &'static str),
    /// Indents line breaks inside, if the enclosing group is broken.
    Indent(Box<Doc>),
    /// Printed flat if it fits on the line and is not forced to break.
    ///
    /// A forced break also forces the enclosing groups, except across a `hug`
    /// group: the last argument of a call, which may break on its own while the
    /// call stays on one line (`f(x => {`). When measuring whether its parent
    /// fits, a hug group is assumed to break, as it will be forced to once it has
    /// been printed broken; this keeps formatting idempotent.
    ///
    /// A `strict` group is printed flat only if all of it fits on the line. It is
    /// used for constructs kept on one line because they were on one line in the
    /// source, which must not end up spanning several lines.
    Group {
        doc: Box<Doc>,
        force: bool,
        hug: bool,
        strict: bool,
    },
    Concat(Vec<Doc>),
}

fn text(s: impl Into<String>) -> Doc {
    Doc::Text(s.into())
}

fn cat(docs: Vec<Doc>) -> Doc {
    Doc::Concat(docs)
}

fn indent(doc: Doc) -> Doc {
    Doc::Indent(Box::new(doc))
}

fn group(force: bool, doc: Doc) -> Doc {
    let force = force || needs_break(&doc);
    Doc::Group { doc: Box::new(doc), force, hug: false, strict: false }
}

/// A group that is flat only if it fits entirely (see [`Doc::Group`]).
fn strict_group(force: bool, doc: Doc) -> Doc {
    let force = force || contains_hard(&doc);
    Doc::Group { doc: Box::new(doc), force, hug: false, strict: true }
}

/// True if `doc` contains a forced break outside of nested groups and hug groups.
fn needs_break(doc: &Doc) -> bool {
    let mut stack = vec![doc];
    while let Some(d) = stack.pop() {
        match d {
            Doc::HardLine | Doc::Trailing(_) => return true,
            Doc::Group { force, hug, .. } if *force && !*hug => return true,
            Doc::Indent(d) => stack.push(d),
            Doc::Concat(ds) => stack.extend(ds),
            _ => {}
        }
    }
    false
}

/// Marks the outermost last group of `doc` as a hug group.
fn mark_hug(doc: &mut Doc) -> bool {
    match doc {
        Doc::Group { hug, .. } => {
            *hug = true;
            true
        }
        Doc::Indent(d) => mark_hug(d),
        Doc::Concat(ds) => ds.iter_mut().rev().any(mark_hug),
        _ => false,
    }
}

fn prepend(pre: Option<Doc>, doc: Doc) -> Doc {
    match pre {
        Some(p) => cat(vec![p, doc]),
        None => doc,
    }
}

/// True if printing `doc` flat would still produce a line break.
fn contains_hard(doc: &Doc) -> bool {
    let mut stack = vec![doc];
    while let Some(d) = stack.pop() {
        match d {
            Doc::HardLine | Doc::Trailing(_) | Doc::Group { force: true, .. } => return true,
            Doc::Text(s) if s.contains('\n') => return true,
            Doc::Indent(d) | Doc::Group { doc: d, .. } => stack.push(d),
            Doc::Concat(ds) => stack.extend(ds),
            _ => {}
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

struct Printer {
    out: String,
    col: usize,
    pending_indent: Option<usize>,
}

impl Printer {
    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(n) = self.pending_indent.take() {
            self.out.extend(std::iter::repeat_n(' ', n));
        }
        self.out.push_str(s);
        match s.rfind('\n') {
            Some(i) => self.col = s[i + 1..].chars().count(),
            None => self.col += s.chars().count(),
        }
    }

    fn newline(&mut self, indent: usize) {
        let trimmed = self.out.trim_end_matches(' ').len();
        self.out.truncate(trimmed);
        self.out.push('\n');
        self.pending_indent = Some(indent);
        self.col = indent;
    }
}

fn render(doc: &Doc) -> String {
    let mut p = Printer { out: String::new(), col: 0, pending_indent: None };
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((ind, mode, doc)) = stack.pop() {
        match doc {
            Doc::Text(s) | Doc::Trailing(s) => p.text(s),
            Doc::Line if mode == Mode::Flat => p.text(" "),
            Doc::SoftLine if mode == Mode::Flat => {}
            Doc::Line | Doc::SoftLine | Doc::HardLine => p.newline(ind),
            Doc::IfBreak(broken, flat) => p.text(if mode == Mode::Break { broken } else { flat }),
            Doc::Indent(d) => stack.push((if mode == Mode::Break { ind + INDENT } else { ind }, mode, d)),
            Doc::Group { doc: d, force, hug, strict } => {
                let m = if *force {
                    Mode::Break
                } else if (mode == Mode::Flat && !hug) || fits(d, &stack, WIDTH as isize - p.col as isize, *strict) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((ind, m, d));
            }
            Doc::Concat(ds) => stack.extend(ds.iter().rev().map(|d| (ind, mode, d))),
        }
    }
    let mut out = p.out;
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// How a doc is measured by [`fits`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Measure {
    /// Part of the group being measured: nested groups are flat, except forced
    /// and hug groups and the groups inside them.
    Next,
    /// Inside a strict group: hug groups are flat too.
    Strict,
    /// After the group being measured: nested groups may break.
    Rest,
}

/// True if `next` printed flat, followed by `rest`, fits in `width` columns up
/// to the next line break.
fn fits(next: &Doc, rest: &[(usize, Mode, &Doc)], mut width: isize, strict: bool) -> bool {
    let first = if strict { Measure::Strict } else { Measure::Next };
    let mut stack: Vec<(Mode, &Doc, Measure)> = vec![(Mode::Flat, next, first)];
    let mut rest_idx = rest.len();
    while width >= 0 {
        let (mode, doc, how) = match stack.pop() {
            Some(x) => x,
            None if rest_idx == 0 => return true,
            None => {
                rest_idx -= 1;
                (rest[rest_idx].1, rest[rest_idx].2, Measure::Rest)
            }
        };
        match doc {
            Doc::Text(s) => match s.find('\n') {
                Some(i) => return width >= s[..i].chars().count() as isize,
                None => width -= s.chars().count() as isize,
            },
            Doc::Trailing(_) | Doc::HardLine => return true,
            Doc::Line | Doc::SoftLine if mode == Mode::Break => return true,
            Doc::Line => width -= 1,
            Doc::SoftLine => {}
            Doc::IfBreak(broken, flat) => width -= if mode == Mode::Break { broken.len() } else { flat.len() } as isize,
            Doc::Indent(d) => stack.push((mode, d, how)),
            Doc::Group { doc: d, force, hug, strict } => {
                let (m, how) = match how {
                    _ if *force => (Mode::Break, how),
                    Measure::Rest if *hug => (Mode::Break, how),
                    Measure::Rest => (mode, how),
                    // Within a group that breaks, nested groups may break too.
                    Measure::Next if *hug || mode == Mode::Break => (Mode::Break, how),
                    Measure::Next if *strict => (Mode::Flat, Measure::Strict),
                    Measure::Next | Measure::Strict => (Mode::Flat, how),
                };
                stack.push((m, d, how));
            }
            Doc::Concat(ds) => stack.extend(ds.iter().rev().map(|d| (mode, d, how))),
        }
    }
    false
}

// ----------------------------------------------------------------------
// Syntax tree to doc

/// An element of a vertical list: one per line (or comma-separated when the
/// enclosing construct is printed inline).
#[derive(Clone, Copy)]
enum Member<'a> {
    Item(&'a Item),
    Stmt(&'a Stmt),
    Field(&'a FieldDecl),
    /// A method, with the start of its declaration (including `pub`).
    Method(&'a FnDecl, u32),
    Variant(&'a VariantDecl),
    Arm(&'a MatchArm),
}

impl Member<'_> {
    fn lo(&self) -> u32 {
        match self {
            Member::Item(i) => i.span.lo,
            Member::Stmt(s) => s.span.lo,
            Member::Field(f) => f.span.lo,
            Member::Method(_, lo) => *lo,
            Member::Variant(v) => v.span.lo,
            Member::Arm(a) => a.span.lo,
        }
    }

    /// End of the element (for items, possibly including a trailing `;`).
    fn hi(&self) -> u32 {
        match self {
            Member::Item(Item { kind: ItemKind::Stmt(s), .. }) => s.span.hi,
            Member::Item(i) => i.span.hi,
            Member::Stmt(s) => s.span.hi,
            Member::Field(f) => f.span.hi,
            Member::Method(f, _) => f.span.hi,
            Member::Variant(v) => v.span.hi,
            Member::Arm(a) => a.span.hi,
        }
    }

    /// True for declarations that are separated from their neighbours by a blank line.
    fn is_decl(&self) -> bool {
        match self {
            Member::Item(i) => match &i.kind {
                ItemKind::Fn(f) => f.body.is_some(),
                ItemKind::Struct(_)
                | ItemKind::Enum(_)
                | ItemKind::Extend(_)
                | ItemKind::Trait(_)
                | ItemKind::Impl(_)
                | ItemKind::Test(_) => true,
                _ => false,
            },
            Member::Method(f, _) => f.body.is_some(),
            _ => false,
        }
    }

    /// True if the element's last token does not end a statement (§2.3), so a
    /// newline after it is not a terminator and an explicit separator is needed.
    fn ends_open(&self) -> bool {
        match *self {
            Member::Item(Item { kind: ItemKind::Stmt(s), .. }) | Member::Stmt(s) => stmt_ends_open(s),
            Member::Field(f) => f.default.as_ref().is_some_and(ends_open),
            Member::Arm(a) => ends_open(&a.body),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Module,
    Block,
    /// Struct, enum and extend bodies.
    Body,
    Arms,
}

/// An element of a comma-separated list.
#[derive(Clone, Copy)]
enum CElem<'a> {
    Expr(&'a Expr),
    Arg(&'a Arg),
    Entry(&'a Expr, &'a Expr),
    Field(&'a FieldInit),
    /// `..base` in a struct literal, with the position of `..`.
    Base(&'a Expr, u32),
    Param(&'a Param),
}

impl CElem<'_> {
    fn lo(&self) -> u32 {
        match self {
            CElem::Expr(e) => e.span.lo,
            CElem::Arg(a) => a.name.as_ref().map_or(a.value.span.lo, |n| n.span.lo),
            CElem::Entry(k, _) => k.span.lo,
            CElem::Field(f) => f.name.span.lo,
            CElem::Base(_, lo) => *lo,
            CElem::Param(p) => p.span.lo,
        }
    }

    fn hi(&self) -> u32 {
        match self {
            CElem::Expr(e) | CElem::Base(e, _) => e.span.hi,
            CElem::Arg(a) => a.value.span.hi,
            CElem::Entry(_, v) => v.span.hi,
            CElem::Field(f) => f.value.as_ref().map_or(f.name.span.hi, |v| v.span.hi),
            CElem::Param(p) => p.span.hi,
        }
    }
}

#[derive(Clone, Copy)]
struct ListOpts {
    /// Break even if the list fits on one line.
    force: bool,
    /// Spaces inside the brackets when printed flat (`Point { x: 1 }`).
    pad: bool,
    /// Add a trailing comma when broken.
    trailing_comma: bool,
    /// The last element may break on its own (see [`Doc::Group`]).
    hug_last: bool,
}

const LIST: ListOpts = ListOpts { force: false, pad: false, trailing_comma: true, hug_last: false };

/// True if a newline after `e` would not terminate it (it ends with an open range).
fn ends_open(e: &Expr) -> bool {
    let mut e = e;
    loop {
        e = match &e.kind {
            ExprKind::Range { end: None, .. } => return true,
            ExprKind::Range { end: Some(x), .. }
            | ExprKind::Binary(_, _, x)
            | ExprKind::Pipe(_, x)
            | ExprKind::Coalesce(_, x)
            | ExprKind::Unary(_, x)
            | ExprKind::Return(Some(x))
            | ExprKind::Break { value: Some(x), .. }
            | ExprKind::Spawn(x) => x,
            ExprKind::Lambda { body, .. } => body,
            _ => return false,
        };
    }
}

/// `@name ` before a labeled loop.
fn label_prefix(label: &Option<Ident>) -> String {
    label.as_ref().map(|l| format!("@{} ", l.name)).unwrap_or_default()
}

fn stmt_ends_open(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Let { value, else_block: None, .. } => ends_open(value),
        StmtKind::Assign { value, .. } => ends_open(value),
        StmtKind::Expr(e) | StmtKind::Defer(e) => ends_open(e),
        _ => false,
    }
}

/// Expressions whose body is laid out on several lines anyway.
fn is_block_like(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Block(_)
            | ExprKind::Match { .. }
            | ExprKind::If { .. }
            | ExprKind::Loop { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
    )
}

/// Arguments that are never moved to a line of their own when they are the only one.
fn is_atom(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Str(_)
            | ExprKind::Bytes(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Duration(_)
            | ExprKind::Bool(_)
            | ExprKind::Ident(_)
            | ExprKind::SelfValue
            | ExprKind::Unit
    )
}

/// Last arguments that may break while the call stays on one line.
fn is_huggable(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::List(xs) => !xs.is_empty(),
        ExprKind::Map(xs) => !xs.is_empty(),
        ExprKind::Match { arms, .. } => !arms.is_empty(),
        ExprKind::StructLit { .. } | ExprKind::If { .. } | ExprKind::Block(_) => true,
        ExprKind::Lambda { body, .. } => is_block_like(body) || is_huggable(body),
        ExprKind::Paren(x) => is_huggable(x),
        _ => false,
    }
}

fn unop_text(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "not ",
        UnOp::BitNot => "~",
    }
}

fn assign_op_text(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Assign => " = ",
        AssignOp::Add => " += ",
        AssignOp::Sub => " -= ",
        AssignOp::Mul => " *= ",
        AssignOp::Div => " /= ",
        AssignOp::Rem => " %= ",
    }
}

fn is_line_comment(c: &Comment) -> bool {
    c.kind != CommentKind::Block
}

struct Formatter<'a> {
    src: &'a str,
    comments: &'a [Comment],
    /// Source tokens, without inserted statement terminators.
    tokens: Vec<Token>,
    /// Index of the first comment not yet emitted.
    next: usize,
    /// Line comments found inside an element, to be emitted after it.
    deferred: Vec<usize>,
}

impl<'a> Formatter<'a> {
    fn new(src: &'a str, comments: &'a [Comment]) -> Formatter<'a> {
        let tokens = lex(src, 0).tokens.into_iter().filter(|t| !(t.tok == Tok::Semi && t.lo == t.hi) && t.tok != Tok::Eof).collect();
        Formatter { src, comments, tokens, next: 0, deferred: Vec::new() }
    }

    // ------------------------------------------------------------------
    // Source helpers

    fn slice(&self, span: Span) -> &'a str {
        &self.src[span.lo as usize..span.hi as usize]
    }

    /// The source text of `span` without whitespace and comments.
    fn clean(&self, span: Span) -> String {
        let mut out = String::new();
        let mut pos = span.lo as usize;
        for c in self.comments.iter().filter(|c| c.lo >= span.lo && c.hi <= span.hi) {
            out.push_str(&self.src[pos..c.lo as usize]);
            pos = c.hi as usize;
        }
        out.push_str(&self.src[pos..span.hi as usize]);
        out.retain(|c| !c.is_whitespace());
        out
    }

    /// Position of the first token at or after `pos` of the same kind as `tok`.
    fn find_tok(&self, pos: u32, tok: &Tok) -> u32 {
        let start = self.tokens.partition_point(|t| t.lo < pos);
        self.tokens[start..].iter().find(|t| std::mem::discriminant(&t.tok) == std::mem::discriminant(tok)).map_or(pos, |t| t.lo)
    }

    /// Position of the token just before `pos`.
    fn prev_tok(&self, pos: u32) -> u32 {
        let idx = self.tokens.partition_point(|t| t.lo < pos);
        if idx == 0 { pos } else { self.tokens[idx - 1].lo }
    }

    /// End of the last token before `pos`, ignoring `;`.
    fn token_end(&self, pos: u32) -> u32 {
        let mut idx = self.tokens.partition_point(|t| t.lo < pos);
        while idx > 0 && self.tokens[idx - 1].tok == Tok::Semi {
            idx -= 1;
        }
        if idx == 0 { pos } else { self.tokens[idx - 1].hi.min(pos) }
    }

    /// Start of a function declaration, including a leading `pub`.
    fn fn_lo(&self, f: &FnDecl) -> u32 {
        let lo = f.span.lo;
        if f.is_pub {
            let idx = self.tokens.partition_point(|t| t.lo < lo);
            if idx > 0 && self.tokens[idx - 1].tok == Tok::Pub {
                return self.tokens[idx - 1].lo;
            }
        }
        lo
    }

    /// True if the opening bracket at `pos` is directly followed by a newline.
    fn newline_after(&self, pos: u32) -> bool {
        self.src[pos as usize + 1..].chars().take_while(|c| c.is_whitespace()).any(|c| c == '\n')
    }

    /// True if the source has an empty line between `a` and `b`.
    fn blank_between(&self, a: u32, b: u32) -> bool {
        a < b && self.src[a as usize..b as usize].matches('\n').count() >= 2
    }

    fn newline_between(&self, a: u32, b: u32) -> bool {
        a < b && self.src[a as usize..b as usize].contains('\n')
    }

    // ------------------------------------------------------------------
    // Comments

    fn comment_text(&self, i: usize) -> &'a str {
        self.comments[i].text.trim_end()
    }

    fn has_comment_before(&self, pos: u32) -> bool {
        self.comments.get(self.next).is_some_and(|c| c.lo < pos)
    }

    /// Takes all remaining comments that start before `pos`.
    fn take_before(&mut self, pos: u32) -> Vec<usize> {
        let start = self.next;
        while self.has_comment_before(pos) {
            self.next += 1;
        }
        (start..self.next).collect()
    }

    /// Emits block comments before `pos` in place; line comments are deferred
    /// to the end of the enclosing list element.
    fn inline_comments(&mut self, pos: u32) -> Option<Doc> {
        if !self.has_comment_before(pos) {
            return None;
        }
        let mut docs = Vec::new();
        for i in self.take_before(pos) {
            if is_line_comment(&self.comments[i]) {
                self.deferred.push(i);
            } else {
                docs.push(text(self.comment_text(i)));
                docs.push(text(" "));
            }
        }
        Some(cat(docs))
    }

    /// Emits the comments before a list element, each on its own line unless it
    /// is a block comment on the same line as the element. Returns true if a
    /// line break was emitted.
    fn leading_comments(&mut self, lo: u32, parts: &mut Vec<Doc>) -> bool {
        let leading = self.take_before(lo);
        let mut hard = false;
        for (k, &ci) in leading.iter().enumerate() {
            let c = &self.comments[ci];
            if k > 0 && self.blank_between(self.comments[leading[k - 1]].hi, c.lo) {
                parts.push(Doc::HardLine);
            }
            parts.push(text(self.comment_text(ci)));
            let next_lo = leading.get(k + 1).map_or(lo, |&n| self.comments[n].lo);
            if is_line_comment(c) || self.newline_between(c.hi, next_lo) {
                parts.push(Doc::HardLine);
                hard = true;
                if k + 1 == leading.len() && self.blank_between(c.hi, lo) {
                    parts.push(Doc::HardLine);
                }
            } else {
                parts.push(text(" "));
            }
        }
        hard
    }

    /// Emits the comments of a list element ending at `hi` whose leading
    /// comments start at `parts[at]`. Block comments left inside the element
    /// and comments on the same line before `limit` follow it; line comments
    /// from inside it (those deferred since `mark`) go on their own lines before
    /// it (and before its doc comments, which must stay attached). Returns the end
    /// of the last comment taken from after the element, and whether a line
    /// break is needed.
    fn element_comments(&mut self, at: usize, hi: u32, limit: u32, mark: usize, parts: &mut Vec<Doc>) -> (u32, bool) {
        let mut inner = self.deferred.split_off(mark);
        let mut after = Vec::new();
        for i in self.take_before(hi) {
            if is_line_comment(&self.comments[i]) { inner.push(i) } else { after.push(i) }
        }
        inner.sort_unstable();
        let mut end = hi;
        while let Some(c) = self.comments.get(self.next) {
            if c.lo >= limit || self.newline_between(end, c.lo) {
                break;
            }
            end = c.hi;
            after.push(self.next);
            self.next += 1;
        }
        let mut hard = !inner.is_empty();
        let before = inner.iter().flat_map(|&i| [text(self.comment_text(i)), Doc::HardLine]);
        parts.splice(at..at, before.collect::<Vec<_>>());
        for i in after {
            let t = self.comment_text(i);
            if is_line_comment(&self.comments[i]) {
                parts.push(Doc::Trailing(format!(" {t}")));
                hard = true;
            } else {
                parts.push(text(format!(" {t}")));
            }
        }
        (end, hard)
    }

    // ------------------------------------------------------------------
    // Lists

    /// Formats the elements of a vertical list; `close` is the position of the
    /// closing brace (or the end of the file). Returns the parts and whether
    /// they contain comments that need line breaks.
    fn vertical_list(&mut self, kind: ListKind, members: &[Member<'a>], close: u32) -> (Vec<Doc>, bool) {
        let mut parts = Vec::new();
        let mut hard = false;
        let mut prev_end = 0;
        for (i, &m) in members.iter().enumerate() {
            let lo = m.lo();
            if i > 0 {
                let prev = members[i - 1];
                let first = self.comments.get(self.next).filter(|c| c.lo < lo).map_or(lo, |c| c.lo);
                let joined = kind == ListKind::Body && self.joined_variants(prev, m);
                parts.push(if joined { text(" ") } else { Doc::Line });
                let forced = match kind {
                    ListKind::Module | ListKind::Body => prev.is_decl() || m.is_decl(),
                    _ => false,
                };
                if forced || self.blank_between(prev_end, first) {
                    parts.push(Doc::HardLine);
                }
            }
            let at = parts.len();
            hard |= self.leading_comments(lo, &mut parts);
            let mark = self.deferred.len();
            parts.push(self.member(m));
            let next = members.get(i + 1);
            if let Some(&next) = next
                && let Some(sep) = self.separator(kind, m, next)
            {
                parts.push(sep);
            }
            let limit = next.map_or(close, |n| n.lo());
            let (end, h) = self.element_comments(at, self.token_end(m.hi()), limit, mark, &mut parts);
            hard |= h;
            prev_end = end;
        }
        let dangling = self.take_before(close);
        for (k, &ci) in dangling.iter().enumerate() {
            let c = &self.comments[ci];
            if !members.is_empty() || k > 0 {
                parts.push(Doc::HardLine);
                if self.blank_between(prev_end, c.lo) {
                    parts.push(Doc::HardLine);
                }
            }
            parts.push(text(self.comment_text(ci)));
            prev_end = c.hi;
            hard = true;
        }
        (parts, hard)
    }

    /// Payload-less variants written on one line stay on one line.
    fn joined_variants(&self, prev: Member, next: Member) -> bool {
        match (prev, next) {
            (Member::Variant(a), Member::Variant(b)) => {
                a.fields.is_empty() && b.fields.is_empty() && !self.newline_between(a.span.hi, b.span.lo)
            }
            _ => false,
        }
    }

    /// The separator printed after `m` when it is followed by `next`.
    fn separator(&self, kind: ListKind, m: Member, next: Member) -> Option<Doc> {
        let open = m.ends_open();
        match kind {
            ListKind::Module | ListKind::Block => open.then(|| text(";")),
            ListKind::Arms => Some(if open { text(",") } else { Doc::IfBreak("", ",") }),
            ListKind::Body => match m {
                Member::Method(..) => None,
                _ if open || self.joined_variants(m, next) => Some(text(",")),
                _ => Some(Doc::IfBreak("", ",")),
            },
        }
    }

    fn member(&mut self, m: Member<'a>) -> Doc {
        match m {
            Member::Item(i) => self.item(i),
            Member::Stmt(s) => self.stmt(s),
            Member::Field(f) => self.field_decl(f),
            Member::Method(f, _) => self.fn_decl(f),
            Member::Variant(v) => self.variant(v),
            Member::Arm(a) => self.arm(a),
        }
    }

    /// A `{ ... }` body around a vertical list, printed inline if `inline` and
    /// the content allows it. The result is not grouped; returns whether it may
    /// be printed flat.
    fn braced(&mut self, kind: ListKind, members: &[Member<'a>], close: u32, inline: bool) -> (Doc, bool) {
        if members.is_empty() && !self.has_comment_before(close) {
            return (text("{}"), true);
        }
        let (parts, hard) = self.vertical_list(kind, members, close);
        let body = cat(parts);
        let flat = inline && !hard && !contains_hard(&body);
        (cat(vec![text("{"), indent(cat(vec![Doc::Line, body])), Doc::Line, text("}")]), flat)
    }

    /// Formats a comma-separated list between `open` and `close`, which are at
    /// `open_pos` and `close_pos` in the source.
    fn comma_list(
        &mut self,
        brackets: (&'static str, &'static str),
        open_pos: u32,
        close_pos: u32,
        elems: &[CElem<'a>],
        opts: ListOpts,
    ) -> Doc {
        let (open, close) = brackets;
        let pre = self.inline_comments(open_pos);
        if elems.is_empty() && !self.has_comment_before(close_pos) {
            return prepend(pre, text(format!("{open}{close}")));
        }
        let mut parts = Vec::new();
        let mut hard = opts.force;
        for (i, &el) in elems.iter().enumerate() {
            parts.push(if i == 0 && !opts.pad { Doc::SoftLine } else { Doc::Line });
            let at = parts.len();
            hard |= self.leading_comments(el.lo(), &mut parts);
            let mark = self.deferred.len();
            let mut doc = self.celem(el);
            let next = elems.get(i + 1);
            if next.is_none() && opts.hug_last {
                mark_hug(&mut doc);
            }
            parts.push(doc);
            if next.is_some() {
                parts.push(text(","));
            } else if opts.trailing_comma && !matches!(el, CElem::Base(..)) {
                parts.push(Doc::IfBreak(",", ""));
            }
            let limit = next.map_or(close_pos, |n| n.lo());
            hard |= self.element_comments(at, el.hi(), limit, mark, &mut parts).1;
        }
        for ci in self.take_before(close_pos) {
            parts.push(Doc::Line);
            parts.push(text(self.comment_text(ci)));
            hard = true;
        }
        let end = if opts.pad { Doc::Line } else { Doc::SoftLine };
        prepend(pre, group(hard, cat(vec![text(open), indent(cat(parts)), end, text(close)])))
    }

    fn celem(&mut self, el: CElem<'a>) -> Doc {
        match el {
            CElem::Expr(e) => self.expr(e),
            CElem::Arg(a) => match &a.name {
                Some(n) => cat(vec![text(format!("{}: ", n.name)), self.expr(&a.value)]),
                None => self.expr(&a.value),
            },
            CElem::Entry(k, v) => cat(vec![self.expr(k), text(": "), self.expr(v)]),
            CElem::Field(f) => match &f.value {
                Some(v) => cat(vec![text(format!("{}: ", f.name.name)), self.expr(v)]),
                None => text(&f.name.name),
            },
            CElem::Base(e, _) => cat(vec![text(".."), self.expr(e)]),
            CElem::Param(p) => self.param(p),
        }
    }

    /// Joins docs with `, ` (for lists that are never broken).
    fn join<T>(&mut self, xs: &'a [T], mut f: impl FnMut(&mut Self, &'a T) -> Doc) -> Doc {
        let mut parts = Vec::new();
        for (i, x) in xs.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(f(self, x));
        }
        cat(parts)
    }

    // ------------------------------------------------------------------
    // Items

    fn module(&mut self, m: &'a Module) -> Doc {
        let members: Vec<Member> = m.items.iter().map(Member::Item).collect();
        let (mut parts, _) = self.vertical_list(ListKind::Module, &members, self.src.len() as u32);
        // Nothing should be left, but never drop a comment.
        let mut rest = std::mem::take(&mut self.deferred);
        rest.extend(self.take_before(u32::MAX));
        for i in rest {
            parts.push(Doc::HardLine);
            parts.push(text(self.comment_text(i)));
        }
        cat(parts)
    }

    fn item(&mut self, item: &'a Item) -> Doc {
        match &item.kind {
            ItemKind::Import(i) => self.import(i),
            ItemKind::Fn(f) => self.fn_decl(f),
            ItemKind::Struct(s) => self.struct_decl(s, item.span),
            ItemKind::Enum(e) => self.enum_decl(e, item.span),
            ItemKind::Extend(x) => self.extend_decl(x, item.span),
            ItemKind::Trait(t) => self.trait_decl(t, item.span),
            ItemKind::Impl(i) => self.impl_decl(i, item.span),
            ItemKind::TypeAlias(t) => {
                let vis = if t.is_pub { "pub " } else { "" };
                let generics = self.generics(&t.generics);
                cat(vec![text(format!("{vis}type {}", t.name.name)), generics, text(" = "), self.ty(&t.ty)])
            }
            ItemKind::NativeType(n) => {
                let vis = if n.is_pub { "pub " } else { "" };
                cat(vec![text(format!("{vis}native type {}", n.name.name)), self.generics(&n.generics)])
            }
            ItemKind::Test(t) => cat(vec![text(format!("test {} ", self.slice(t.name_span))), self.block(&t.body)]),
            ItemKind::Stmt(s) => self.stmt(s),
        }
    }

    fn import(&mut self, i: &'a Import) -> Doc {
        let source = match &i.source {
            ImportSource::Name(id) => id.name.as_str(),
            ImportSource::Path(_, span) => self.slice(*span),
        };
        let mut s = match &i.names {
            Some(names) => {
                let names: Vec<&str> = names.iter().map(|n| n.name.as_str()).collect();
                format!("from {source} import {}", names.join(", "))
            }
            None => format!("import {source}"),
        };
        if let Some(alias) = &i.alias {
            s.push_str(" as ");
            s.push_str(&alias.name);
        }
        text(s)
    }

    fn generics(&mut self, gs: &'a [GenericParam]) -> Doc {
        if gs.is_empty() {
            return text("");
        }
        let mut parts = vec![text("<")];
        for (i, g) in gs.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(text(if g.reflect { format!("reflect {}", g.name.name) } else { g.name.name.clone() }));
            if !g.bounds.is_empty() {
                parts.push(text(": "));
                parts.push(self.bounds(&g.bounds));
            }
        }
        parts.push(text(">"));
        cat(parts)
    }

    /// `A + B<T> + m.C`
    fn bounds(&mut self, bs: &'a [TypeExpr]) -> Doc {
        let mut parts = Vec::new();
        for (i, b) in bs.iter().enumerate() {
            if i > 0 {
                parts.push(text(" + "));
            }
            parts.push(self.ty(b));
        }
        cat(parts)
    }

    fn where_clause(&mut self, preds: &'a [WherePredicate]) -> Doc {
        let mut parts = vec![text(" where ")];
        for (i, w) in preds.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(text(format!("{}: ", w.name.name)));
            parts.push(self.bounds(&w.bounds));
        }
        cat(parts)
    }

    fn fn_decl(&mut self, f: &'a FnDecl) -> Doc {
        let mut head = String::new();
        if f.is_pub {
            head.push_str("pub ");
        }
        if f.is_native {
            head.push_str("native ");
        }
        head.push_str("fn ");
        head.push_str(&f.name.name);
        let mut parts = vec![text(head), self.generics(&f.generics)];
        let open = self.find_tok(f.name.span.hi, &Tok::LParen);
        let close = self.find_tok(f.params.last().map_or(open + 1, |p| p.span.hi), &Tok::RParen);
        let params: Vec<CElem> = f.params.iter().map(CElem::Param).collect();
        parts.push(self.comma_list(("(", ")"), open, close, &params, LIST));
        if let Some(ret) = &f.ret {
            parts.push(text(" -> "));
            parts.push(self.ty(ret));
        }
        if !f.where_clause.is_empty() {
            parts.push(self.where_clause(&f.where_clause));
        }
        if let Some(body) = &f.body {
            parts.push(text(" "));
            parts.push(self.block(body));
        }
        cat(parts)
    }

    fn param(&mut self, p: &'a Param) -> Doc {
        let Some(ty) = &p.ty else {
            return text(&p.name.name);
        };
        let mut parts = vec![text(format!("{}: ", p.name.name)), self.ty(ty)];
        if let Some(d) = &p.default {
            parts.push(text(" = "));
            parts.push(self.expr(d));
        }
        cat(parts)
    }

    /// Position of the closing brace of a declaration spanning `span`. The span
    /// may extend over a trailing comment, whose braces do not count.
    fn closing_brace(&self, span: Span) -> u32 {
        let mut hi = span.hi as usize;
        while let Some(i) = self.src[..hi].rfind('}') {
            match self.comments.iter().find(|c| c.lo as usize <= i && i < c.hi as usize) {
                Some(c) => hi = c.lo as usize,
                None => return i as u32,
            }
        }
        span.hi
    }

    fn struct_decl(&mut self, s: &'a StructDecl, span: Span) -> Doc {
        let vis = if s.is_pub { "pub " } else { "" };
        let head = cat(vec![text(format!("{vis}struct {}", s.name.name)), self.generics(&s.generics), text(" ")]);
        let mut members: Vec<Member> = s.fields.iter().map(Member::Field).collect();
        members.extend(s.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))));
        members.sort_by_key(|m| m.lo());
        let (body, flat) = self.braced(ListKind::Body, &members, self.closing_brace(span), s.inline && s.methods.is_empty());
        cat(vec![head, strict_group(!flat, body)])
    }

    fn field_decl(&mut self, f: &'a FieldDecl) -> Doc {
        let var = if f.mutable { "var " } else { "" };
        let mut parts = vec![text(format!("{var}{}: ", f.name.name)), self.ty(&f.ty)];
        if let Some(d) = &f.default {
            parts.push(text(" = "));
            parts.push(self.expr(d));
        }
        cat(parts)
    }

    fn enum_decl(&mut self, e: &'a EnumDecl, span: Span) -> Doc {
        let vis = if e.is_pub { "pub " } else { "" };
        let head = cat(vec![text(format!("{vis}enum {}", e.name.name)), self.generics(&e.generics), text(" ")]);
        let mut members: Vec<Member> = e.variants.iter().map(Member::Variant).collect();
        members.extend(e.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))));
        members.sort_by_key(|m| m.lo());
        let inline = e.inline && e.methods.is_empty() && e.variants.iter().all(|v| v.fields.is_empty());
        let (body, flat) = self.braced(ListKind::Body, &members, self.closing_brace(span), inline);
        cat(vec![head, strict_group(!flat, body)])
    }

    fn variant(&mut self, v: &'a VariantDecl) -> Doc {
        if v.fields.is_empty() {
            return text(&v.name.name);
        }
        let fields = self.join(&v.fields, |f, field| {
            let ty = f.ty(&field.ty);
            match &field.name {
                Some(n) => cat(vec![text(format!("{}: ", n.name)), ty]),
                None => ty,
            }
        });
        cat(vec![text(format!("{}(", v.name.name)), fields, text(")")])
    }

    fn extend_decl(&mut self, x: &'a ExtendDecl, span: Span) -> Doc {
        let head = cat(vec![text("extend"), self.generics(&x.generics), text(" "), self.ty(&x.target), text(" ")]);
        let members: Vec<Member> = x.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))).collect();
        let (body, flat) = self.braced(ListKind::Body, &members, self.closing_brace(span), false);
        cat(vec![head, strict_group(!flat, body)])
    }

    fn trait_decl(&mut self, t: &'a TraitDecl, span: Span) -> Doc {
        let vis = if t.is_pub { "pub " } else { "" };
        let mut head = vec![text(format!("{vis}trait {}", t.name.name)), self.generics(&t.generics)];
        if !t.supers.is_empty() {
            head.push(text(": "));
            head.push(self.bounds(&t.supers));
        }
        head.push(text(" "));
        let members: Vec<Member> = t.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))).collect();
        let (body, flat) = self.braced(ListKind::Body, &members, self.closing_brace(span), false);
        cat(vec![cat(head), strict_group(!flat, body)])
    }

    fn impl_decl(&mut self, i: &'a ImplDecl, span: Span) -> Doc {
        let mut head = vec![text("impl"), self.generics(&i.generics), text(" "), self.ty(&i.trait_ref), text(" for "), self.ty(&i.target)];
        if !i.where_clause.is_empty() {
            head.push(self.where_clause(&i.where_clause));
        }
        head.push(text(" "));
        let members: Vec<Member> = i.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))).collect();
        let (body, flat) = self.braced(ListKind::Body, &members, self.closing_brace(span), false);
        cat(vec![cat(head), strict_group(!flat, body)])
    }

    // ------------------------------------------------------------------
    // Statements and blocks

    fn stmt(&mut self, s: &'a Stmt) -> Doc {
        match &s.kind {
            StmtKind::Let { pattern, ty, value, else_block, mutable, is_pub, .. } => {
                let vis = if *is_pub { "pub " } else { "" };
                let kw = if *mutable { "var" } else { "let" };
                let mut parts = vec![text(format!("{vis}{kw} ")), self.pat(pattern)];
                if let Some(ty) = ty {
                    parts.push(text(": "));
                    parts.push(self.ty(ty));
                }
                parts.push(text(" = "));
                parts.push(self.expr(value));
                if let Some(b) = else_block {
                    parts.push(text(" else "));
                    parts.push(self.block(b));
                }
                cat(parts)
            }
            StmtKind::Assign { target, op, value } => cat(vec![self.expr(target), text(assign_op_text(*op)), self.expr(value)]),
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Fn(f) => self.fn_decl(f),
            StmtKind::Defer(e) => cat(vec![text("defer "), self.expr(e)]),
        }
    }

    /// A block without its group; returns whether it may be printed flat.
    fn block_parts(&mut self, b: &'a Block) -> (Doc, bool) {
        let members: Vec<Member> = b.stmts.iter().map(Member::Stmt).collect();
        self.braced(ListKind::Block, &members, b.span.hi - 1, b.inline && b.stmts.len() <= 1)
    }

    fn block(&mut self, b: &'a Block) -> Doc {
        let (doc, flat) = self.block_parts(b);
        strict_group(!flat, doc)
    }

    // ------------------------------------------------------------------
    // Expressions

    fn expr(&mut self, e: &'a Expr) -> Doc {
        let pre = self.inline_comments(e.span.lo);
        let doc = self.expr_kind(e);
        prepend(pre, doc)
    }

    fn expr_kind(&mut self, e: &'a Expr) -> Doc {
        match &e.kind {
            ExprKind::Unit => text("()"),
            ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
            ExprKind::Int(_) | ExprKind::IntOverflow(_) | ExprKind::Float(_) | ExprKind::Duration(_) => text(self.clean(e.span)),
            ExprKind::Str(_) | ExprKind::Bytes(_) | ExprKind::Error => text(self.slice(e.span)),
            ExprKind::Ident(id) => text(&id.name),
            ExprKind::SelfValue => text("self"),
            ExprKind::SelfType => text("Self"),
            ExprKind::List(items) => {
                let elems: Vec<CElem> = items.iter().map(CElem::Expr).collect();
                self.bracket_list(e, &elems)
            }
            ExprKind::Map(pairs) if pairs.is_empty() => text("[:]"),
            ExprKind::Map(pairs) => {
                let elems: Vec<CElem> = pairs.iter().map(|(k, v)| CElem::Entry(k, v)).collect();
                self.bracket_list(e, &elems)
            }
            ExprKind::Repeat(x, n) => cat(vec![text("["), self.expr(x), text("; "), self.expr(n), text("]")]),
            ExprKind::ListComp(x, clauses) => {
                let mut parts = vec![text("["), self.expr(x)];
                self.comp_clauses(clauses, &mut parts);
                parts.push(text("]"));
                cat(parts)
            }
            ExprKind::MapComp(k, v, clauses) => {
                let mut parts = vec![text("["), self.expr(k), text(": "), self.expr(v)];
                self.comp_clauses(clauses, &mut parts);
                parts.push(text("]"));
                cat(parts)
            }
            ExprKind::Tuple(items) => {
                let elems: Vec<CElem> = items.iter().map(CElem::Expr).collect();
                self.comma_list(("(", ")"), e.span.lo, e.span.hi - 1, &elems, LIST)
            }
            ExprKind::Paren(x) => cat(vec![text("("), self.expr(x), text(")")]),
            ExprKind::StructLit { path, fields, base } => {
                let name: Vec<&str> = path.iter().map(|p| p.name.as_str()).collect();
                let open = self.find_tok(path.last().map_or(e.span.lo, |p| p.span.hi), &Tok::LBrace);
                let mut elems: Vec<CElem> = fields.iter().map(CElem::Field).collect();
                if let Some(b) = base {
                    elems.push(CElem::Base(b, self.prev_tok(b.span.lo)));
                }
                let opts = ListOpts { force: self.newline_after(open), pad: true, ..LIST };
                cat(vec![text(format!("{} ", name.join("."))), self.comma_list(("{", "}"), open, e.span.hi - 1, &elems, opts)])
            }
            ExprKind::Unary(op, x) => cat(vec![text(unop_text(*op)), self.expr(x)]),
            ExprKind::Binary(..) | ExprKind::Pipe(..) | ExprKind::Coalesce(..) => self.binary(e),
            ExprKind::Range { start, end, inclusive } => {
                let mut parts = Vec::new();
                if let Some(s) = start {
                    parts.push(self.expr(s));
                }
                parts.push(text(if *inclusive { "..=" } else { ".." }));
                if let Some(x) = end {
                    parts.push(self.expr(x));
                }
                cat(parts)
            }
            ExprKind::Call { .. }
            | ExprKind::MethodCall { .. }
            | ExprKind::Field(..)
            | ExprKind::TupleField(..)
            | ExprKind::Index(..)
            | ExprKind::Try(_) => self.chain(e),
            ExprKind::If { .. } => self.if_expr(e),
            ExprKind::Match { scrutinee, arms, inline } => {
                let head = cat(vec![text("match "), self.expr(scrutinee), text(" ")]);
                let members: Vec<Member> = arms.iter().map(Member::Arm).collect();
                let (body, flat) = self.braced(ListKind::Arms, &members, e.span.hi - 1, *inline);
                cat(vec![head, strict_group(!flat, body)])
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { label, body } => cat(vec![text(format!("{}loop ", label_prefix(label))), self.block(body)]),
            ExprKind::While { label, cond, body } => {
                cat(vec![text(format!("{}while ", label_prefix(label))), self.cond(cond), text(" "), self.block(body)])
            }
            ExprKind::For { label, pattern, iter, body } => cat(vec![
                text(format!("{}for ", label_prefix(label))),
                self.pat(pattern),
                text(" in "),
                self.expr(iter),
                text(" "),
                self.block(body),
            ]),
            ExprKind::Lambda { params, body, parens } => {
                let params = if *parens {
                    let ps = self.join(params, |f, p| match &p.ty {
                        Some(ty) => cat(vec![text(format!("{}: ", p.name.name)), f.ty(ty)]),
                        None => text(&p.name.name),
                    });
                    cat(vec![text("("), ps, text(")")])
                } else {
                    text(params.first().map_or("", |p| p.name.name.as_str()))
                };
                cat(vec![params, text(" => "), self.expr(body)])
            }
            ExprKind::Return(v) => self.keyword_value("return", v.as_deref()),
            ExprKind::Break { label, value } => match label {
                Some(l) => self.keyword_value(&format!("break @{}", l.name), value.as_deref()),
                None => self.keyword_value("break", value.as_deref()),
            },
            ExprKind::Continue(label) => text(match label {
                Some(l) => format!("continue @{}", l.name),
                None => "continue".into(),
            }),
            ExprKind::Spawn(x) => cat(vec![text("spawn "), self.expr(x)]),
        }
    }

    fn keyword_value(&mut self, kw: &str, value: Option<&'a Expr>) -> Doc {
        match value {
            Some(v) => cat(vec![text(format!("{kw} ")), self.expr(v)]),
            None => text(kw),
        }
    }

    /// A list or map literal; it stays broken if the source broke the line after `[`.
    fn bracket_list(&mut self, e: &'a Expr, elems: &[CElem<'a>]) -> Doc {
        let opts = ListOpts { force: self.newline_after(e.span.lo), ..LIST };
        self.comma_list(("[", "]"), e.span.lo, e.span.hi - 1, elems, opts)
    }

    fn comp_clauses(&mut self, clauses: &'a [CompClause], parts: &mut Vec<Doc>) {
        for c in clauses {
            match c {
                CompClause::For(p, x) => {
                    parts.push(text(" for "));
                    parts.push(self.pat(p));
                    parts.push(text(" in "));
                    parts.push(self.expr(x));
                }
                CompClause::If(x) => {
                    parts.push(text(" if "));
                    parts.push(self.expr(x));
                }
            }
        }
    }

    fn cond(&mut self, c: &'a Cond) -> Doc {
        match c {
            Cond::Expr(e) => self.expr(e),
            Cond::Let(p, e) => cat(vec![text("let "), self.pat(p), text(" = "), self.expr(e)]),
        }
    }

    /// A chain of binary operators, flattened along its left spine.
    fn binary(&mut self, e: &'a Expr) -> Doc {
        let mut rhs = Vec::new();
        let mut cur = e;
        loop {
            cur = match &cur.kind {
                ExprKind::Binary(op, l, r) => {
                    rhs.push((op.text(), &**r));
                    l
                }
                ExprKind::Pipe(l, r) => {
                    rhs.push(("|>", &**r));
                    l
                }
                ExprKind::Coalesce(l, r) => {
                    rhs.push(("??", &**r));
                    l
                }
                _ => break,
            };
        }
        let mut parts = vec![self.expr(cur)];
        for (op, r) in rhs.into_iter().rev() {
            parts.push(text(format!(" {op} ")));
            parts.push(self.expr(r));
        }
        cat(parts)
    }

    /// An `if`/`else if`/`else` chain; its blocks are printed inline together or not at all.
    fn if_expr(&mut self, e: &'a Expr) -> Doc {
        let mut parts = Vec::new();
        let mut flat = true;
        let mut cur = e;
        while let ExprKind::If { cond, then, else_ } = &cur.kind {
            parts.push(text("if "));
            parts.push(self.cond(cond));
            parts.push(text(" "));
            let (doc, f) = self.block_parts(then);
            parts.push(doc);
            flat &= f;
            let Some(el) = else_ else { break };
            parts.push(text(" else "));
            if let Some(pre) = self.inline_comments(el.span.lo) {
                parts.push(pre);
            }
            match &el.kind {
                ExprKind::Block(b) => {
                    let (doc, f) = self.block_parts(b);
                    parts.push(doc);
                    flat &= f;
                    break;
                }
                _ => cur = el,
            }
        }
        strict_group(!flat, cat(parts))
    }

    /// Postfix chains (calls, method calls, fields, indexing, `?`). Chains of
    /// method calls that do not fit are broken before each `.method(...)`.
    fn chain(&mut self, e: &'a Expr) -> Doc {
        let mut ops = Vec::new();
        let mut base = e;
        loop {
            base = match &base.kind {
                ExprKind::Call { callee, .. } => {
                    ops.push(base);
                    callee
                }
                ExprKind::MethodCall { receiver: x, .. }
                | ExprKind::Field(x, _)
                | ExprKind::TupleField(x, ..)
                | ExprKind::Index(x, _)
                | ExprKind::Try(x) => {
                    ops.push(base);
                    x
                }
                _ => break,
            };
        }
        ops.reverse();
        let is_call = |op: &Expr| matches!(op.kind, ExprKind::MethodCall { .. });
        let calls = ops.iter().filter(|op| is_call(op)).count();

        // The head stays on the first line: the base with its fields and
        // indexing, and the first method call if it has no arguments or the
        // base is a short name.
        let mut head_len = ops.iter().position(|op| is_call(op)).unwrap_or(ops.len());
        if let Some(ExprKind::MethodCall { args, .. }) = ops.get(head_len).map(|op| &op.kind) {
            let short_base = head_len == 0
                && match &base.kind {
                    ExprKind::Ident(id) => id.name.len() <= INDENT || id.name.starts_with(|c: char| c.is_ascii_uppercase()),
                    ExprKind::SelfValue | ExprKind::SelfType => true,
                    _ => false,
                };
            if args.is_empty() || short_base {
                head_len += 1;
                while head_len < ops.len() && !is_call(ops[head_len]) {
                    head_len += 1;
                }
            }
        }

        let mut head = vec![self.expr(base)];
        for op in &ops[..head_len] {
            head.push(self.postfix(op));
        }
        let links = &ops[head_len..];
        if calls < 2 || links.is_empty() {
            for op in links {
                head.push(self.postfix(op));
            }
            return cat(head);
        }
        // Long chains of calls taking lambdas are always broken.
        let has_lambda = |op: &Expr| match &op.kind {
            ExprKind::MethodCall { args, .. } => {
                args.iter().any(|a| matches!(&a.value.kind, ExprKind::Lambda { body, .. } if !is_block_like(body)))
            }
            _ => false,
        };
        let force = links.iter().filter(|op| is_call(op)).count() >= 3 && links.iter().any(|op| has_lambda(op));
        let mut tail = Vec::new();
        for op in links {
            if is_call(op) {
                tail.push(Doc::SoftLine);
            }
            tail.push(self.postfix(op));
        }
        group(force, cat(vec![cat(head), indent(cat(tail))]))
    }

    fn postfix(&mut self, op: &'a Expr) -> Doc {
        match &op.kind {
            ExprKind::Call { callee, args } => self.call_args(args, callee.span.hi, op.span.hi - 1),
            ExprKind::MethodCall { method, args, .. } => {
                cat(vec![text(format!(".{}", method.name)), self.call_args(args, method.span.hi, op.span.hi - 1)])
            }
            ExprKind::Field(_, name) => text(format!(".{}", name.name)),
            ExprKind::TupleField(_, i, _) => text(format!(".{i}")),
            ExprKind::Index(_, idx) => cat(vec![text("["), self.expr(idx), text("]")]),
            // `x??` would lex as the `??` operator.
            ExprKind::Try(x) => text(if matches!(x.kind, ExprKind::Try(_)) { " ?" } else { "?" }),
            _ => self.expr(op),
        }
    }

    /// Call arguments; the `(` is the first one after `after`.
    fn call_args(&mut self, args: &'a [Arg], after: u32, close: u32) -> Doc {
        let open = self.find_tok(after, &Tok::LParen);
        let pre = self.inline_comments(open);
        if let [arg] = args {
            // A lone literal or name gains nothing from a line of its own.
            if arg.name.is_none() && is_atom(&arg.value) && !self.has_comment_before(close) {
                return prepend(pre, cat(vec![text("("), self.expr(&arg.value), text(")")]));
            }
        }
        let elems: Vec<CElem> = args.iter().map(CElem::Arg).collect();
        let hug = args.last().is_some_and(|a| is_huggable(&a.value));
        prepend(pre, self.comma_list(("(", ")"), open, close, &elems, ListOpts { hug_last: hug, ..LIST }))
    }

    fn arm(&mut self, a: &'a MatchArm) -> Doc {
        let mut parts = vec![self.pat(&a.pattern)];
        if let Some(g) = &a.guard {
            parts.push(text(" if "));
            parts.push(self.expr(g));
        }
        parts.push(text(" => "));
        parts.push(self.expr(&a.body));
        cat(parts)
    }

    // ------------------------------------------------------------------
    // Types and patterns

    fn ty(&mut self, t: &'a TypeExpr) -> Doc {
        let pre = self.inline_comments(t.span.lo);
        let doc = match &t.kind {
            TypeExprKind::Named { path, args } => {
                let name: Vec<&str> = path.iter().map(|p| p.name.as_str()).collect();
                let mut parts = vec![text(name.join("."))];
                if !args.is_empty() {
                    parts.push(text("<"));
                    parts.push(self.join(args, Self::ty));
                    parts.push(text(">"));
                }
                cat(parts)
            }
            TypeExprKind::List(x) => cat(vec![text("["), self.ty(x), text("]")]),
            TypeExprKind::Map(k, v) => cat(vec![text("["), self.ty(k), text(": "), self.ty(v), text("]")]),
            TypeExprKind::Tuple(xs) => {
                let comma = if xs.len() == 1 { "," } else { "" };
                cat(vec![text("("), self.join(xs, Self::ty), text(format!("{comma})"))])
            }
            TypeExprKind::Option(x) => {
                // `(fn() -> T)?` needs its parentheses: `fn() -> T?` returns an option.
                if matches!(x.kind, TypeExprKind::Fn(_, Some(_))) {
                    cat(vec![text("("), self.ty(x), text(")?")])
                } else {
                    cat(vec![self.ty(x), text("?")])
                }
            }
            TypeExprKind::Fn(params, ret) => {
                let mut parts = vec![text("fn("), self.join(params, Self::ty), text(")")];
                if let Some(r) = ret {
                    parts.push(text(" -> "));
                    parts.push(self.ty(r));
                }
                cat(parts)
            }
            TypeExprKind::Unit => text("()"),
        };
        prepend(pre, doc)
    }

    fn pat(&mut self, p: &'a Pattern) -> Doc {
        let pre = self.inline_comments(p.span.lo);
        let doc = match &p.kind {
            PatternKind::Wildcard => text("_"),
            PatternKind::Binding(id) => text(&id.name),
            PatternKind::Literal(PatLit::Str(_)) => text(self.slice(p.span)),
            PatternKind::Literal(PatLit::Bool(b)) => text(if *b { "true" } else { "false" }),
            PatternKind::Literal(PatLit::Unit) => text("()"),
            PatternKind::Literal(PatLit::Int(_)) | PatternKind::Range(..) => text(self.clean(p.span)),
            PatternKind::Tuple(ps) => {
                let comma = if ps.len() == 1 { "," } else { "" };
                cat(vec![text("("), self.join(ps, Self::pat), text(format!("{comma})"))])
            }
            PatternKind::Variant { path, args } => {
                let name: Vec<&str> = path.iter().map(|p| p.name.as_str()).collect();
                match args {
                    Some(args) => cat(vec![text(format!("{}(", name.join("."))), self.join(args, Self::pat), text(")")]),
                    None => text(name.join(".")),
                }
            }
            PatternKind::Struct { path, fields, rest } => {
                let name: Vec<&str> = path.iter().map(|p| p.name.as_str()).collect();
                if fields.is_empty() && !rest {
                    text(format!("{} {{}}", name.join(".")))
                } else {
                    let mut parts = vec![text(format!("{} {{ ", name.join(".")))];
                    parts.push(self.join(fields, |f, field| match &field.pattern {
                        Some(p) => cat(vec![text(format!("{}: ", field.name.name)), f.pat(p)]),
                        None => text(&field.name.name),
                    }));
                    if *rest {
                        parts.push(text(if fields.is_empty() { ".." } else { ", .." }));
                    }
                    parts.push(text(" }"));
                    cat(parts)
                }
            }
            PatternKind::List { before, rest, after } => {
                let mut parts = vec![text("[")];
                let mut first = true;
                let mut sep = |parts: &mut Vec<Doc>| {
                    if !std::mem::take(&mut first) {
                        parts.push(text(", "));
                    }
                };
                for x in before {
                    sep(&mut parts);
                    parts.push(self.pat(x));
                }
                if let Some(name) = rest {
                    sep(&mut parts);
                    parts.push(text(format!("..{}", name.as_ref().map_or("", |n| n.name.as_str()))));
                }
                for x in after {
                    sep(&mut parts);
                    parts.push(self.pat(x));
                }
                parts.push(text("]"));
                cat(parts)
            }
            PatternKind::Or(ps) => {
                let mut parts = Vec::new();
                for (i, x) in ps.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(" | "));
                    }
                    parts.push(self.pat(x));
                }
                cat(parts)
            }
        };
        prepend(pre, doc)
    }
}
