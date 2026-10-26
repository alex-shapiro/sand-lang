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

use crate::check::budget::{CompileBudget, Stop, tool_fuel};
use crate::diag::{SourceFile, Span};
use crate::syntax::ast::*;
use crate::syntax::lexer::{Comment, CommentKind, Tok, Token};
use crate::syntax::parse_module;
use std::sync::Arc;

/// Maximum line width.
const WIDTH: usize = 100;
/// Spaces per indentation level.
const INDENT: usize = 4;

/// Formats a source file in canonical style.
pub fn format_source(src: &str) -> Result<String, String> {
    format_source_with(src, CompileBudget::new(Some(tool_fuel(src.len())), None, None))
}

/// Like [`format_source`], with the work bounded by a budget.
pub fn format_source_with(src: &str, budget: CompileBudget) -> Result<String, String> {
    if !budget.check() {
        return Err(stop_message(budget.stopped().unwrap_or(Stop::Interrupted), "format"));
    }
    // Like the compiler, the formatter recurses, so it needs a deep stack.
    let (src, budget) = (src.to_owned(), Arc::new(budget));
    let theirs = budget.clone();
    let out = crate::platform::on_deep_stack(move || format_on_this_thread(&src, &theirs))
        .unwrap_or_else(|p| Err(format!("internal formatter error: {}", crate::unwind::panic_text(&*p))));
    match budget.stopped() {
        Some(stop) => Err(stop_message(stop, "format")),
        None => out,
    }
}

pub(crate) fn stop_message(stop: Stop, what: &str) -> String {
    match stop {
        Stop::Fuel => format!("source too complex to {what}: exceeded the {what} budget"),
        Stop::Time => format!("time limit exceeded (while trying to {what} the source)"),
        Stop::Interrupted => "interrupted".into(),
    }
}

fn format_on_this_thread(src: &str, budget: &CompileBudget) -> Result<String, String> {
    if !budget.charge((src.len() / 64) as u64) {
        return Ok(String::new());
    }
    let parsed = parse_module(src, 0);
    if let Some(err) = parsed.errors.first() {
        let (line, col) = SourceFile::new("", src).line_col(err.span.lo);
        return Err(format!("{line}:{col}: {}", err.message));
    }
    let mut f = Formatter::new(src, &parsed.comments, &parsed.docs, parsed.tokens, budget);
    let doc = f.module(&parsed.module);
    Ok(render(&doc, budget))
}

/// What formatting `src` must preserve, as text to compare (for tests and
/// fuzzing): its syntax tree without positions, and each block comment with
/// the tokens around it (commas and statement terminators aside, which
/// formatting adds and removes). Fails if `src` does not parse.
#[doc(hidden)]
pub fn meaning(src: &str) -> Result<String, String> {
    let parsed = parse_module(src, 0);
    if let Some(err) = parsed.errors.first() {
        return Err(err.message.clone());
    }
    let mut out = without_spans(&format!("{:#?}", parsed.module));
    let args = macro_args(&parsed.tokens);
    let in_arg = |t: &Token| args.iter().any(|&(lo, hi)| t.lo > lo && t.hi <= hi);
    let all = &parsed.tokens;
    // The formatter prints `[:]` in a macro argument as `{}`.
    let empty_object = |i: usize| {
        all[i].tok == Tok::Colon
            && in_arg(&all[i])
            && all[i - 1].tok == Tok::LBracket
            && all.get(i + 1).is_some_and(|t| t.tok == Tok::RBracket)
    };
    let tokens: Vec<&Token> =
        all.iter().enumerate().filter(|&(i, t)| !matches!(t.tok, Tok::Comma | Tok::Semi) && !empty_object(i)).map(|(_, t)| t).collect();
    // In a macro argument, `[` and `{` both open an object.
    let text = |t: Option<&&Token>| match t {
        Some(t) if in_arg(t) && matches!(t.tok, Tok::LBracket | Tok::LBrace) => "{",
        Some(t) if in_arg(t) && matches!(t.tok, Tok::RBracket | Tok::RBrace) => "}",
        Some(t) => &src[t.lo as usize..t.hi as usize],
        None => "",
    };
    for c in parsed.comments.iter().filter(|c| c.kind == CommentKind::Block) {
        let next = tokens.partition_point(|t| t.lo < c.lo);
        let before = next.checked_sub(1).and_then(|i| tokens.get(i));
        out.push_str(&format!("\n{} {} {}", text(before), c.text(src), text(tokens.get(next))));
    }
    Ok(out)
}

/// The source ranges between the parentheses of the macro calls (`name!(...)`).
fn macro_args(tokens: &[Token]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if t.tok != Tok::Bang || i == 0 || tokens[i - 1].tok != Tok::Ident || tokens.get(i + 1).is_none_or(|t| t.tok != Tok::LParen) {
            continue;
        }
        let mut depth = 0;
        for c in &tokens[i + 1..] {
            match c.tok {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                out.push((tokens[i + 1].lo, c.lo));
                break;
            }
        }
    }
    out
}

/// `Debug` output without the `Span { .. }`s in it.
fn without_spans(debug: &str) -> String {
    let mut out = String::with_capacity(debug.len());
    let mut rest = debug;
    while let Some(at) = rest.find("Span {") {
        out.push_str(&rest[..at]);
        let end = rest[at..].find('}').map_or(rest.len(), |e| at + e + 1);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
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
    /// A `strict` group is printed flat only if the entire group fits on the
    /// current line. When printed flat, nested non-forced groups (including
    /// `hug` groups) are also printed flat.
    ///
    /// A `break_if_rest_fits` group must end with a one-character bracket. It
    /// breaks only if the text after the group then fits after the bracket.
    Group {
        doc: Box<Doc>,
        force: bool,
        hug: bool,
        strict: bool,
        break_if_rest_fits: bool,
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

fn group(b: &CompileBudget, force: bool, doc: Doc) -> Doc {
    let force = force || needs_break(b, &doc);
    Doc::Group { doc: Box::new(doc), force, hug: false, strict: false, break_if_rest_fits: false }
}

/// A group that is flat only if it fits entirely (see [`Doc::Group`]).
fn strict_group(b: &CompileBudget, force: bool, doc: Doc) -> Doc {
    let force = force || contains_hard(b, &doc);
    Doc::Group { doc: Box::new(doc), force, hug: false, strict: true, break_if_rest_fits: false }
}

/// True if `doc` contains a forced break outside of nested groups and hug groups.
fn needs_break(b: &CompileBudget, doc: &Doc) -> bool {
    let mut stack = vec![doc];
    while let Some(d) = stack.pop() {
        if !b.charge(1) {
            return false;
        }
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
fn mark_hug(b: &CompileBudget, doc: &mut Doc) -> bool {
    if !b.charge(1) {
        return false;
    }
    match doc {
        Doc::Group { hug, .. } => {
            *hug = true;
            true
        }
        Doc::Indent(d) => mark_hug(b, d),
        Doc::Concat(ds) => ds.iter_mut().rev().any(|d| mark_hug(b, d)),
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
fn contains_hard(b: &CompileBudget, doc: &Doc) -> bool {
    let mut stack = vec![doc];
    while let Some(d) = stack.pop() {
        let scanned = if let Doc::Text(s) = d { s.len() } else { 0 };
        if !b.charge(1 + (scanned / 64) as u64) {
            return false;
        }
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
    /// Flat mode for a `strict` group: keeps all non-forced nested groups
    /// flat, including `hug` groups.
    Strict,
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

fn render(doc: &Doc, b: &CompileBudget) -> String {
    let mut p = Printer { out: String::new(), col: 0, pending_indent: None };
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((ind, mode, doc)) = stack.pop() {
        let printed = match doc {
            Doc::Text(s) | Doc::Trailing(s) => s.len(),
            Doc::Line | Doc::SoftLine | Doc::HardLine => ind,
            _ => 0,
        };
        if !b.charge(1 + printed as u64) {
            break;
        }
        match doc {
            Doc::Text(s) | Doc::Trailing(s) => p.text(s),
            Doc::Line if mode != Mode::Break => p.text(" "),
            Doc::SoftLine if mode != Mode::Break => {}
            Doc::Line | Doc::SoftLine | Doc::HardLine => p.newline(ind),
            Doc::IfBreak(broken, flat) => p.text(if mode == Mode::Break { broken } else { flat }),
            Doc::Indent(d) => stack.push((if mode == Mode::Break { ind + INDENT } else { ind }, mode, d)),
            Doc::Group { doc: d, force, hug, strict, break_if_rest_fits } => {
                let flat = |d: &Doc, col: usize| fits(d, &stack, WIDTH as isize - col as isize, *strict, b);
                let m = if *force {
                    Mode::Break
                } else if mode == Mode::Strict {
                    Mode::Strict
                } else if (mode == Mode::Flat && !hug)
                    || flat(d, p.col)
                    || (*break_if_rest_fits && !flat(&Doc::Concat(Vec::new()), ind + 1))
                {
                    if *strict { Mode::Strict } else { Mode::Flat }
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
fn fits(next: &Doc, rest: &[(usize, Mode, &Doc)], mut width: isize, strict: bool, b: &CompileBudget) -> bool {
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
        let measured = if let Doc::Text(s) = doc { s.len() } else { 0 };
        if !b.charge(1 + (measured / 64) as u64) {
            return false;
        }
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
            Doc::Group { doc: d, force, hug, strict, .. } => {
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
    /// `..base` in a construction, with the position of `..`.
    Base(&'a Expr, u32),
    Param(&'a Param),
    Data(&'a Data),
    DataEntry(&'a Expr, &'a Data),
    Pat(&'a Pattern),
    /// `name: pattern` in a constructor pattern.
    NamedPat(&'a Ident, &'a Pattern),
    /// `..` in a constructor pattern, or `..name` in a list pattern, with the
    /// position of `..`.
    Rest(u32, Option<&'a Ident>),
}

impl CElem<'_> {
    fn lo(&self) -> u32 {
        match self {
            CElem::Expr(e) => e.span.lo,
            CElem::Arg(a) => a.name.as_ref().map_or(a.value.span.lo, |n| n.span.lo),
            CElem::Entry(k, _) => k.span.lo,
            CElem::Base(_, lo) => *lo,
            CElem::Param(p) => p.span.lo,
            CElem::Data(d) => d.span().lo,
            CElem::DataEntry(k, _) => k.span.lo,
            CElem::Pat(p) => p.span.lo,
            CElem::NamedPat(n, _) => n.span.lo,
            CElem::Rest(lo, _) => *lo,
        }
    }

    fn hi(&self) -> u32 {
        match self {
            CElem::Expr(e) | CElem::Base(e, _) => e.span.hi,
            CElem::Arg(a) => a.value.span.hi,
            CElem::Entry(_, v) => v.span.hi,
            CElem::Param(p) => p.span.hi,
            CElem::Data(d) | CElem::DataEntry(_, d) => d.span().hi,
            CElem::Pat(p) | CElem::NamedPat(_, p) => p.span.hi,
            CElem::Rest(lo, name) => name.map_or(lo + 2, |n| n.span.hi),
        }
    }
}

#[derive(Clone, Copy)]
struct ListOpts {
    /// Break even if the list fits on one line.
    force: bool,
    /// Add a trailing comma when broken.
    trailing_comma: bool,
    /// The last element may break on its own (see [`Doc::Group`]).
    hug_last: bool,
    /// Break only if the text after the list then fits (see [`Doc::Group`]).
    break_if_rest_fits: bool,
}

const LIST: ListOpts = ListOpts { force: false, trailing_comma: true, hug_last: false, break_if_rest_fits: false };
/// Options for the sub-patterns of a pattern.
const PATTERN: ListOpts = ListOpts { break_if_rest_fits: true, ..LIST };

/// The operator and operands of a binary operation.
fn binary_parts(e: &Expr) -> Option<(&'static str, &Expr, &Expr)> {
    match &e.kind {
        ExprKind::Binary(op, l, r) => Some((op.text(), l, r)),
        ExprKind::Coalesce(l, r) => Some(("??", l, r)),
        _ => None,
    }
}

fn binary_level(e: &Expr) -> u8 {
    match &e.kind {
        ExprKind::Coalesce(..) => 1,
        ExprKind::Binary(op, ..) => match op {
            BinOp::Or => 2,
            BinOp::And => 3,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 5,
            BinOp::BitOr => 6,
            BinOp::BitXor => 7,
            BinOp::BitAnd => 8,
            BinOp::Shl | BinOp::Shr => 9,
            BinOp::Add | BinOp::Sub => 11,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 12,
            BinOp::Pow => 14,
        },
        _ => 0,
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
        ExprKind::If { .. } | ExprKind::Block(_) => true,
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
    /// See [`crate::syntax::parser::ParseResult::docs`].
    docs: &'a [u32],
    /// Source tokens, without inserted statement terminators.
    tokens: Vec<Token>,
    /// Positions of the line breaks of the source.
    newlines: Vec<u32>,
    /// Index of the first comment not yet emitted.
    next: usize,
    /// Line comments found inside an element, to be emitted after it.
    deferred: Vec<usize>,
    budget: &'a CompileBudget,
}

impl<'a> Formatter<'a> {
    fn new(src: &'a str, comments: &'a [Comment], docs: &'a [u32], mut tokens: Vec<Token>, budget: &'a CompileBudget) -> Formatter<'a> {
        tokens.retain(|t| !(t.tok == Tok::Semi && t.lo == t.hi) && t.tok != Tok::Eof);
        let newlines = src.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i as u32).collect();
        Formatter { src, comments, docs, tokens, newlines, next: 0, deferred: Vec::new(), budget }
    }

    // ------------------------------------------------------------------
    // Source helpers

    fn slice(&self, span: Span) -> &'a str {
        &self.src[span.lo as usize..span.hi as usize]
    }

    /// A number literal (or integer pattern) without whitespace, or as it
    /// is if it has comments inside (`- /* one */ 1`).
    fn number(&mut self, span: Span) -> Doc {
        if self.comments.get(self.next).is_some_and(|c| c.lo >= span.lo && c.lo < span.hi) {
            self.verbatim(span)
        } else {
            text(self.clean(span))
        }
    }

    /// The source text of `span` without whitespace and comments.
    fn clean(&self, span: Span) -> String {
        let mut out = String::new();
        let mut pos = span.lo as usize;
        let first = self.comments.partition_point(|c| c.lo < span.lo);
        for c in self.comments[first..].iter().take_while(|c| c.lo < span.hi).filter(|c| c.hi <= span.hi) {
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

    /// The number of line breaks before `pos`.
    fn line_of(&self, pos: u32) -> usize {
        self.newlines.partition_point(|&nl| nl < pos)
    }

    /// True if the source has an empty line between `a` and `b`.
    fn blank_between(&self, a: u32, b: u32) -> bool {
        a < b && self.line_of(b) - self.line_of(a) >= 2
    }

    fn newline_between(&self, a: u32, b: u32) -> bool {
        a < b && self.line_of(b) > self.line_of(a)
    }

    /// Returns true if `open`, `close`, and the gaps between `members` do not
    /// contain a newline. Newlines inside a member's own span are ignored.
    fn on_one_line(&self, open: u32, members: &[Member], close: u32) -> bool {
        let mut prev = open;
        for m in members {
            if self.newline_between(prev, m.lo()) {
                return false;
            }
            prev = m.hi();
        }
        !self.newline_between(prev, close)
    }

    /// Position of the first token at or after `pos`.
    fn next_tok(&self, pos: u32) -> u32 {
        self.tokens.get(self.tokens.partition_point(|t| t.lo < pos)).map_or(pos, |t| t.lo)
    }

    // ------------------------------------------------------------------
    // Comments

    /// The text of comment `i`. A `///` comment that does not document an
    /// item becomes `//`, so that the formatter cannot make it document an
    /// item if it moves the comment.
    fn comment_text(&self, i: usize) -> String {
        let c = &self.comments[i];
        let text = c.text(self.src).trim_end();
        match c.kind {
            CommentKind::Doc if self.docs.binary_search(&c.lo).is_err() => {
                // `///!` must not become the module doc comment `//!`.
                let rest = &text[3..];
                if rest.starts_with(['!', '/']) { format!("// {rest}") } else { format!("//{rest}") }
            }
            _ => text.to_string(),
        }
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

    /// `s`, the text of the first `tok` token at or after `pos`, after the
    /// block comments before that token (so they stay in place).
    fn punct(&mut self, pos: u32, tok: &Tok, s: &'static str) -> Doc {
        let at = self.find_tok(pos, tok);
        self.text_at(at, s)
    }

    /// `s`, after the block comments before `at` (spaced as `s` is).
    fn text_at(&mut self, at: u32, s: &'static str) -> Doc {
        match self.inline_comments(at) {
            None => text(s),
            Some(pre) => match s.strip_prefix(' ') {
                Some(rest) => cat(vec![text(" "), pre, text(rest)]),
                None => cat(vec![pre, text(s)]),
            },
        }
    }

    /// The source text of `span` as it is (a literal), with the comments
    /// inside it (in an interpolation), which are not emitted again.
    fn verbatim(&mut self, span: Span) -> Doc {
        while self.comments.get(self.next).is_some_and(|c| c.lo < span.hi) {
            if self.comments[self.next].lo < span.lo {
                break;
            }
            self.next += 1;
        }
        text(self.slice(span))
    }

    /// `.name` after the expression ending at `after`, keeping the block
    /// comments around the `.` in place.
    fn dot(&mut self, after: u32, name: impl FnOnce(&mut Self) -> Doc) -> Doc {
        let dot = self.punct(after, &Tok::Dot, ".");
        cat(vec![dot, name(self)])
    }

    /// The keywords `kw` of a declaration starting at `lo`, after `pub ` if
    /// `is_pub` (keeping a block comment after `pub` in place).
    fn declared(&mut self, is_pub: bool, lo: u32, kw: String) -> Doc {
        if !is_pub {
            return text(kw);
        }
        let after = self.next_tok(lo + 1);
        cat(vec![text("pub "), prepend(self.inline_comments(after), text(kw))])
    }

    /// `name`, after the block comments before it.
    fn name(&mut self, id: &Ident) -> Doc {
        prepend(self.inline_comments(id.span.lo), text(&id.name))
    }

    /// `a.b.C`, with the block comments in it.
    fn path(&mut self, path: &[Ident]) -> Doc {
        let mut parts = vec![self.name(&path[0])];
        for w in path.windows(2) {
            parts.push(self.dot(w[0].span.hi, |f| f.name(&w[1])));
        }
        cat(parts)
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
        match kind {
            ListKind::Module | ListKind::Block => None,
            ListKind::Arms => Some(Doc::IfBreak("", ",")),
            ListKind::Body => match m {
                Member::Method(..) => None,
                _ if self.joined_variants(m, next) => Some(text(",")),
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
    fn braced(&mut self, kind: ListKind, members: &[Member<'a>], open: u32, close: u32, inline: bool) -> (Doc, bool) {
        let pre = self.inline_comments(open);
        if members.is_empty() && !self.has_comment_before(close) {
            return (prepend(pre, text("{}")), true);
        }
        let (parts, hard) = self.vertical_list(kind, members, close);
        let body = cat(parts);
        let flat = inline && !hard && !contains_hard(self.budget, &body);
        (prepend(pre, cat(vec![text("{"), indent(cat(vec![Doc::Line, body])), Doc::Line, text("}")])), flat)
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
            parts.push(if i == 0 { Doc::SoftLine } else { Doc::Line });
            let at = parts.len();
            hard |= self.leading_comments(el.lo(), &mut parts);
            let mark = self.deferred.len();
            let mut doc = self.celem(el);
            let next = elems.get(i + 1);
            if next.is_none() && opts.hug_last {
                mark_hug(self.budget, &mut doc);
            }
            parts.push(doc);
            if next.is_some() {
                parts.push(text(","));
            } else if opts.trailing_comma && !matches!(el, CElem::Base(..) | CElem::Rest(..)) {
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
        let mut doc = group(self.budget, hard, cat(vec![text(open), indent(cat(parts)), Doc::SoftLine, text(close)]));
        if let Doc::Group { break_if_rest_fits, .. } = &mut doc {
            *break_if_rest_fits = opts.break_if_rest_fits;
        }
        prepend(pre, doc)
    }

    fn celem(&mut self, el: CElem<'a>) -> Doc {
        match el {
            CElem::Expr(e) => self.expr(e),
            CElem::Arg(a) => match &a.name {
                Some(n) => cat(vec![self.name(n), self.punct(n.span.hi, &Tok::Colon, ": "), self.expr(&a.value)]),
                None => self.expr(&a.value),
            },
            CElem::Entry(k, v) => cat(vec![self.expr(k), self.punct(k.span.hi, &Tok::Colon, ": "), self.expr(v)]),
            CElem::Base(e, _) => cat(vec![text(".."), self.expr(e)]),
            CElem::Param(p) => self.param(p),
            CElem::Data(d) => self.data(d),
            CElem::DataEntry(k, v) => cat(vec![self.expr(k), self.punct(k.span.hi, &Tok::Colon, ": "), self.data(v)]),
            CElem::Pat(p) => self.pat(p),
            CElem::NamedPat(n, p) => cat(vec![self.name(n), self.punct(n.span.hi, &Tok::Colon, ": "), self.pat(p)]),
            CElem::Rest(_, name) => match name {
                Some(n) => cat(vec![text(".."), self.name(n)]),
                None => text(".."),
            },
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
        if !self.budget.charge(1) {
            return text("");
        }
        match &item.kind {
            ItemKind::Import(i) => self.import(i),
            ItemKind::Fn(f) => self.fn_decl(f),
            ItemKind::Struct(s) => self.struct_decl(s, item.span),
            ItemKind::Enum(e) => self.enum_decl(e, item.span),
            ItemKind::Extend(x) => self.extend_decl(x, item.span),
            ItemKind::Trait(t) => self.trait_decl(t, item.span),
            ItemKind::Impl(i) => self.impl_decl(i, item.span),
            ItemKind::TypeAlias(t) => {
                let head = self.declared(t.is_pub, item.span.lo, "type ".into());
                let name = self.name(&t.name);
                let generics = self.generics(&t.generics);
                let eq = self.punct(t.generics.last().map_or(t.name.span.hi, |g| g.name.span.hi), &Tok::Eq, " = ");
                cat(vec![head, name, generics, eq, self.ty(&t.ty)])
            }
            ItemKind::Test(t) => {
                let name = prepend(self.inline_comments(t.name_span.lo), text(self.slice(t.name_span)));
                cat(vec![text("test "), name, text(" "), self.block(&t.body)])
            }
            ItemKind::Stmt(s) => self.stmt(s),
        }
    }

    fn import(&mut self, i: &'a Import) -> Doc {
        let (source, source_end) = match &i.source {
            ImportSource::Name(id) => (self.name(id), id.span.hi),
            ImportSource::Path(_, span) => (prepend(self.inline_comments(span.lo), text(self.slice(*span))), span.hi),
        };
        let mut parts = match &i.names {
            Some(names) => {
                let keyword = self.punct(source_end, &Tok::Import, " import ");
                vec![text("from "), source, keyword, self.join(names, Self::name)]
            }
            None => vec![text("import "), source],
        };
        if let Some(alias) = &i.alias {
            parts.push(self.punct(source_end, &Tok::As, " as "));
            parts.push(self.name(alias));
        }
        cat(parts)
    }

    fn generics(&mut self, gs: &'a [GenericParam]) -> Doc {
        if gs.is_empty() {
            return text("");
        }
        // `<` is the token before the first parameter (or its `reflect`).
        let mut lt = self.prev_tok(gs[0].name.span.lo);
        if gs[0].reflect {
            lt = self.prev_tok(lt);
        }
        let mut parts = vec![self.text_at(lt, "<")];
        let mut end = 0;
        for (i, g) in gs.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            if g.reflect {
                parts.push(text("reflect "));
            }
            parts.push(self.name(&g.name));
            end = g.name.span.hi;
            if let Some(last) = g.traits.last() {
                parts.push(self.punct(g.name.span.hi, &Tok::Colon, ": "));
                parts.push(self.traits(&g.traits));
                end = last.span.hi;
            }
        }
        parts.push(self.text_at(self.next_tok(end), ">"));
        cat(parts)
    }

    /// `A + B<T> + m.C`
    fn traits(&mut self, ts: &'a [TypeExpr]) -> Doc {
        let mut parts = Vec::new();
        for (i, t) in ts.iter().enumerate() {
            if i > 0 {
                parts.push(self.punct(ts[i - 1].span.hi, &Tok::Plus, " + "));
            }
            parts.push(self.ty(t));
        }
        cat(parts)
    }

    /// ` where T: A, ...` after what ends at `after`.
    fn where_clause(&mut self, after: u32, preds: &'a [WherePredicate]) -> Doc {
        // `where` is an identifier.
        let mut parts = vec![self.punct(after, &Tok::Ident, " where ")];
        for (i, w) in preds.iter().enumerate() {
            if i > 0 {
                parts.push(text(", "));
            }
            parts.push(self.name(&w.name));
            parts.push(self.punct(w.name.span.hi, &Tok::Colon, ": "));
            parts.push(self.traits(&w.traits));
        }
        cat(parts)
    }

    fn fn_decl(&mut self, f: &'a FnDecl) -> Doc {
        let keywords = if f.is_native { "native fn " } else { "fn " };
        let mut parts = vec![self.declared(f.is_pub, self.fn_lo(f), keywords.into()), self.name(&f.name), self.generics(&f.generics)];
        let open = self.find_tok(f.name.span.hi, &Tok::LParen);
        let close = self.find_tok(f.params.last().map_or(open + 1, |p| p.span.hi), &Tok::RParen);
        let params: Vec<CElem> = f.params.iter().map(CElem::Param).collect();
        parts.push(self.comma_list(("(", ")"), open, close, &params, LIST));
        if let Some(ret) = &f.ret {
            parts.push(self.punct(close, &Tok::Arrow, " -> "));
            parts.push(self.ty(ret));
        }
        if !f.where_clause.is_empty() {
            let after = f.ret.as_ref().map_or(close + 1, |r| r.span.hi);
            parts.push(self.where_clause(after, &f.where_clause));
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
        let mut parts = vec![text(&p.name.name), self.punct(p.name.span.hi, &Tok::Colon, ": "), self.ty(ty)];
        if let Some(d) = &p.default {
            parts.push(self.punct(ty.span.hi, &Tok::Eq, " = "));
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
        let head = cat(vec![self.declared(s.is_pub, span.lo, "struct ".into()), self.name(&s.name), self.generics(&s.generics), text(" ")]);
        let mut members: Vec<Member> = s.fields.iter().map(Member::Field).collect();
        members.extend(s.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))));
        members.sort_by_key(|m| m.lo());
        let close = self.closing_brace(span);
        let inline = s.methods.is_empty() && !self.newline_between(span.lo, close);
        let (body, flat) = self.braced(ListKind::Body, &members, self.find_tok(span.lo, &Tok::LBrace), close, inline);
        cat(vec![head, strict_group(self.budget, !flat, body)])
    }

    fn field_decl(&mut self, f: &'a FieldDecl) -> Doc {
        let head = match (f.is_pub, f.mutable) {
            (_, true) => self.declared(f.is_pub, f.span.lo, "var ".into()),
            (true, false) => text("pub "),
            (false, false) => text(""),
        };
        let mut parts = vec![head, self.name(&f.name), self.punct(f.name.span.hi, &Tok::Colon, ": "), self.ty(&f.ty)];
        if let Some(d) = &f.default {
            parts.push(self.punct(f.ty.span.hi, &Tok::Eq, " = "));
            parts.push(self.expr(d));
        }
        cat(parts)
    }

    fn enum_decl(&mut self, e: &'a EnumDecl, span: Span) -> Doc {
        let head = cat(vec![self.declared(e.is_pub, span.lo, "enum ".into()), self.name(&e.name), self.generics(&e.generics), text(" ")]);
        let mut members: Vec<Member> = e.variants.iter().map(Member::Variant).collect();
        members.extend(e.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))));
        members.sort_by_key(|m| m.lo());
        let close = self.closing_brace(span);
        let inline = e.methods.is_empty() && e.variants.iter().all(|v| v.fields.is_empty()) && !self.newline_between(span.lo, close);
        let (body, flat) = self.braced(ListKind::Body, &members, self.find_tok(span.lo, &Tok::LBrace), close, inline);
        cat(vec![head, strict_group(self.budget, !flat, body)])
    }

    fn variant(&mut self, v: &'a VariantDecl) -> Doc {
        let name = self.name(&v.name);
        let Some(last) = v.fields.last() else { return name };
        let open = self.punct(v.name.span.hi, &Tok::LParen, "(");
        let fields = self.join(&v.fields, |f, field| match &field.name {
            Some(n) => cat(vec![f.name(n), f.punct(n.span.hi, &Tok::Colon, ": "), f.ty(&field.ty)]),
            None => f.ty(&field.ty),
        });
        let close = self.punct(last.ty.span.hi, &Tok::RParen, ")");
        cat(vec![name, open, fields, close])
    }

    fn extend_decl(&mut self, x: &'a ExtendDecl, span: Span) -> Doc {
        let head = cat(vec![text("extend"), self.generics(&x.generics), text(" "), self.ty(&x.target), text(" ")]);
        let members: Vec<Member> = x.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))).collect();
        let (body, flat) = self.braced(ListKind::Body, &members, self.find_tok(span.lo, &Tok::LBrace), self.closing_brace(span), false);
        cat(vec![head, strict_group(self.budget, !flat, body)])
    }

    fn trait_decl(&mut self, t: &'a TraitDecl, span: Span) -> Doc {
        let mut head = vec![self.declared(t.is_pub, span.lo, "trait ".into()), self.name(&t.name), self.generics(&t.generics)];
        if !t.supers.is_empty() {
            head.push(self.punct(t.generics.last().map_or(t.name.span.hi, |g| g.name.span.hi), &Tok::Colon, ": "));
            head.push(self.traits(&t.supers));
        }
        head.push(text(" "));
        let members: Vec<Member> = t.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))).collect();
        let (body, flat) = self.braced(ListKind::Body, &members, self.find_tok(span.lo, &Tok::LBrace), self.closing_brace(span), false);
        cat(vec![cat(head), strict_group(self.budget, !flat, body)])
    }

    fn impl_decl(&mut self, i: &'a ImplDecl, span: Span) -> Doc {
        let mut head = vec![text("impl"), self.generics(&i.generics), text(" "), self.ty(&i.trait_ref)];
        head.push(self.punct(i.trait_ref.span.hi, &Tok::For, " for "));
        head.push(self.ty(&i.target));
        if !i.where_clause.is_empty() {
            head.push(self.where_clause(i.target.span.hi, &i.where_clause));
        }
        head.push(text(" "));
        let members: Vec<Member> = i.methods.iter().map(|f| Member::Method(f, self.fn_lo(f))).collect();
        let (body, flat) = self.braced(ListKind::Body, &members, self.find_tok(span.lo, &Tok::LBrace), self.closing_brace(span), false);
        cat(vec![cat(head), strict_group(self.budget, !flat, body)])
    }

    // ------------------------------------------------------------------
    // Statements and blocks

    fn stmt(&mut self, s: &'a Stmt) -> Doc {
        if !self.budget.charge(1) {
            return text("");
        }
        match &s.kind {
            StmtKind::Let { pattern, ty, value, else_block, mutable, is_pub, .. } => {
                let vis = if *is_pub { "pub " } else { "" };
                let kw = if *mutable { "var" } else { "let" };
                let mut parts = vec![text(format!("{vis}{kw} ")), self.pat(pattern)];
                if let Some(ty) = ty {
                    parts.push(self.punct(pattern.span.hi, &Tok::Colon, ": "));
                    parts.push(self.ty(ty));
                }
                parts.push(self.punct(ty.as_ref().map_or(pattern.span.hi, |t| t.span.hi), &Tok::Eq, " = "));
                parts.push(self.expr(value));
                if let Some(b) = else_block {
                    parts.push(self.punct(value.span.hi, &Tok::Else, " else "));
                    parts.push(self.block(b));
                }
                cat(parts)
            }
            StmtKind::Assign { target, op, value } => {
                let target_doc = self.expr(target);
                let op_doc = match self.inline_comments(self.next_tok(target.span.hi)) {
                    Some(pre) => cat(vec![text(" "), pre, text(assign_op_text(*op).trim_start())]),
                    None => text(assign_op_text(*op)),
                };
                cat(vec![target_doc, op_doc, self.expr(value)])
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Fn(f) => self.fn_decl(f),
            StmtKind::Defer(e) => cat(vec![text("defer "), self.expr(e)]),
            StmtKind::Use { name, ty, value } => {
                let mut parts = vec![text("use "), self.name(name)];
                if let Some(ty) = ty {
                    parts.push(self.punct(name.span.hi, &Tok::Colon, ": "));
                    parts.push(self.ty(ty));
                }
                parts.push(self.punct(ty.as_ref().map_or(name.span.hi, |t| t.span.hi), &Tok::Eq, " = "));
                parts.push(self.expr(value));
                cat(parts)
            }
        }
    }

    /// A block without its group; returns whether it may be printed flat.
    fn block_parts(&mut self, b: &'a Block) -> (Doc, bool) {
        let members: Vec<Member> = b.stmts.iter().map(Member::Stmt).collect();
        let close = b.span.hi - 1;
        let inline = b.stmts.len() <= 1 && self.on_one_line(b.span.lo, &members, close);
        self.braced(ListKind::Block, &members, b.span.lo, close, inline)
    }

    fn block(&mut self, b: &'a Block) -> Doc {
        let (doc, flat) = self.block_parts(b);
        strict_group(self.budget, !flat, doc)
    }

    // ------------------------------------------------------------------
    // Expressions

    fn expr(&mut self, e: &'a Expr) -> Doc {
        if !self.budget.charge(1) {
            return text("");
        }
        let pre = self.inline_comments(e.span.lo);
        let doc = self.expr_kind(e);
        prepend(pre, doc)
    }

    fn expr_kind(&mut self, e: &'a Expr) -> Doc {
        match &e.kind {
            ExprKind::Unit => cat(vec![text("("), self.text_at(e.span.hi - 1, ")")]),
            ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
            ExprKind::Int(_) | ExprKind::IntOverflow(_) | ExprKind::Float(_) | ExprKind::Duration(_) => self.number(e.span),
            ExprKind::Str(_) | ExprKind::Bytes(_) | ExprKind::Error => self.verbatim(e.span),
            ExprKind::Ident(id) => text(&id.name),
            ExprKind::SelfValue => text("self"),
            ExprKind::SelfType => text("Self"),
            ExprKind::ModuleRef(name) => text(name),
            ExprKind::Macro(m) => {
                let bang = self.find_tok(m.name.span.hi, &Tok::Bang);
                let parts = vec![
                    text(&m.name.name),
                    self.text_at(bang, "!"),
                    self.punct(bang + 1, &Tok::LParen, "("),
                    self.data(&m.arg),
                    self.text_at(e.span.hi - 1, ")"),
                ];
                cat(parts)
            }
            ExprKind::List(items) => {
                let elems: Vec<CElem> = items.iter().map(CElem::Expr).collect();
                self.bracket_list(e, &elems)
            }
            ExprKind::Map(pairs) if pairs.is_empty() => {
                let colon = self.text_at(self.next_tok(e.span.lo + 1), ":");
                cat(vec![text("["), colon, self.text_at(e.span.hi - 1, "]")])
            }
            ExprKind::Map(pairs) => {
                let elems: Vec<CElem> = pairs.iter().map(|(k, v)| CElem::Entry(k, v)).collect();
                self.bracket_list(e, &elems)
            }
            ExprKind::Repeat(x, n) => {
                cat(vec![text("["), self.expr(x), self.punct(x.span.hi, &Tok::Semi, "; "), self.expr(n), self.text_at(e.span.hi - 1, "]")])
            }
            ExprKind::ListComp(x, clauses) => {
                let mut parts = vec![text("["), self.expr(x)];
                self.comp_clauses(x.span.hi, clauses, &mut parts);
                parts.push(self.text_at(e.span.hi - 1, "]"));
                cat(parts)
            }
            ExprKind::MapComp(k, v, clauses) => {
                let mut parts = vec![text("["), self.expr(k), self.punct(k.span.hi, &Tok::Colon, ": "), self.expr(v)];
                self.comp_clauses(v.span.hi, clauses, &mut parts);
                parts.push(self.text_at(e.span.hi - 1, "]"));
                cat(parts)
            }
            ExprKind::Tuple(items) => {
                let elems: Vec<CElem> = items.iter().map(CElem::Expr).collect();
                self.comma_list(("(", ")"), e.span.lo, e.span.hi - 1, &elems, LIST)
            }
            ExprKind::Paren(x) => cat(vec![text("("), self.expr(x), self.text_at(e.span.hi - 1, ")")]),
            ExprKind::Unary(op, x) => cat(vec![text(unop_text(*op)), self.expr(x)]),
            ExprKind::Binary(..) | ExprKind::Coalesce(..) => self.binary(e),
            ExprKind::Range { start, end, inclusive } => {
                let mut parts = Vec::new();
                if let Some(s) = start {
                    parts.push(self.expr(s));
                }
                let (tok, op) = if *inclusive { (Tok::DotDotEq, "..=") } else { (Tok::DotDot, "..") };
                parts.push(self.punct(start.as_ref().map_or(e.span.lo, |s| s.span.hi), &tok, op));
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
            ExprKind::Match { scrutinee, arms } => {
                let head = cat(vec![text("match "), self.expr(scrutinee), text(" ")]);
                let members: Vec<Member> = arms.iter().map(Member::Arm).collect();
                let close = e.span.hi - 1;
                let open = self.find_tok(scrutinee.span.hi, &Tok::LBrace);
                let (body, flat) = self.braced(ListKind::Arms, &members, open, close, self.on_one_line(open, &members, close));
                cat(vec![head, strict_group(self.budget, !flat, body)])
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop { label, body } => cat(vec![self.loop_head(label, "loop "), self.block(body)]),
            ExprKind::While { label, cond, body } => {
                cat(vec![self.loop_head(label, "while "), self.cond(cond), text(" "), self.block(body)])
            }
            ExprKind::For { label, pattern, iter, body } => cat(vec![
                self.loop_head(label, "for "),
                self.pat(pattern),
                self.punct(pattern.span.hi, &Tok::In, " in "),
                self.expr(iter),
                text(" "),
                self.block(body),
            ]),
            ExprKind::Lambda { params, body, parens } => {
                let params_end = params.last().map_or(e.span.lo, |p| p.ty.as_ref().map_or(p.name.span.hi, |t| t.span.hi));
                let params = if *parens {
                    let ps = self.join(params, |f, p| match &p.ty {
                        Some(ty) => cat(vec![f.name(&p.name), f.punct(p.name.span.hi, &Tok::Colon, ": "), f.ty(ty)]),
                        None => f.name(&p.name),
                    });
                    let close = self.punct(params_end, &Tok::RParen, ")");
                    cat(vec![text("("), ps, close])
                } else {
                    params.first().map_or(text(""), |p| self.name(&p.name))
                };
                let arrow = self.punct(params_end, &Tok::FatArrow, " => ");
                cat(vec![params, arrow, self.expr(body)])
            }
            ExprKind::Return(v) => self.keyword_value("return", v.as_deref()),
            ExprKind::Break { label, value } => match label {
                Some(l) => {
                    let label = self.label(l);
                    cat(vec![text("break "), label, self.keyword_value("", value.as_deref())])
                }
                None => self.keyword_value("break", value.as_deref()),
            },
            ExprKind::Continue(label) => match label {
                Some(l) => cat(vec![text("continue "), self.label(l)]),
                None => text("continue"),
            },
            ExprKind::Spawn(x) => cat(vec![text("spawn "), self.expr(x)]),
        }
    }

    fn keyword_value(&mut self, kw: &str, value: Option<&'a Expr>) -> Doc {
        match value {
            Some(v) => cat(vec![text(format!("{kw} ")), self.expr(v)]),
            None => text(kw),
        }
    }

    /// `@name`, with the block comments before its `@` and its name.
    fn label(&mut self, l: &Ident) -> Doc {
        let at = self.text_at(l.span.lo - 1, "@");
        cat(vec![at, self.name(l)])
    }

    /// `@name loop ` (`kw` being `"loop "`, `"while "` or `"for "`), or `kw`.
    fn loop_head(&mut self, label: &Option<Ident>, kw: &'static str) -> Doc {
        let Some(l) = label else { return text(kw) };
        let label = self.label(l);
        cat(vec![label, text(" "), self.text_at(self.next_tok(l.span.hi), kw)])
    }

    /// A list or map literal; it stays broken if the source broke the line after `[`.
    /// A data macro's argument; an object is always printed as `{...}`.
    fn data(&mut self, d: &'a Data) -> Doc {
        match d {
            Data::Null(span) => self.text_at(span.lo, "null"),
            Data::Value(e) => self.expr(e),
            Data::List(xs, span) => {
                let elems: Vec<CElem> = xs.iter().map(CElem::Data).collect();
                let opts = ListOpts { force: self.newline_after(span.lo), ..LIST };
                self.comma_list(("[", "]"), span.lo, span.hi - 1, &elems, opts)
            }
            Data::Object(kvs, span) => {
                let elems: Vec<CElem> = kvs.iter().map(|(k, v)| CElem::DataEntry(k, v)).collect();
                let opts = ListOpts { force: self.newline_after(span.lo), ..LIST };
                self.comma_list(("{", "}"), span.lo, span.hi - 1, &elems, opts)
            }
        }
    }

    fn bracket_list(&mut self, e: &'a Expr, elems: &[CElem<'a>]) -> Doc {
        let opts = ListOpts { force: self.newline_after(e.span.lo), ..LIST };
        self.comma_list(("[", "]"), e.span.lo, e.span.hi - 1, elems, opts)
    }

    /// The clauses of a comprehension whose element ends at `end`.
    fn comp_clauses(&mut self, mut end: u32, clauses: &'a [CompClause], parts: &mut Vec<Doc>) {
        for c in clauses {
            match c {
                CompClause::For(p, x) => {
                    parts.push(self.punct(end, &Tok::For, " for "));
                    parts.push(self.pat(p));
                    parts.push(self.punct(p.span.hi, &Tok::In, " in "));
                    parts.push(self.expr(x));
                    end = x.span.hi;
                }
                CompClause::If(x) => {
                    parts.push(self.punct(end, &Tok::If, " if "));
                    parts.push(self.expr(x));
                    end = x.span.hi;
                }
            }
        }
    }

    fn cond(&mut self, c: &'a Cond) -> Doc {
        match c {
            Cond::Expr(e) => self.expr(e),
            Cond::Let(p, e) => {
                let keyword = self.text_at(self.prev_tok(p.span.lo), "let ");
                let pattern = self.pat(p);
                cat(vec![keyword, pattern, self.punct(p.span.hi, &Tok::Eq, " = "), self.expr(e)])
            }
        }
    }

    /// A chain of binary operators of one precedence level, flattened
    /// along its left spine. If it does not fit on the line, it is broken
    /// before each operator (a line starting with `- ` continues the
    /// previous one, §2.3).
    fn binary(&mut self, e: &'a Expr) -> Doc {
        let level = binary_level(e);
        let mut rhs = Vec::new();
        let mut cur = e;
        while let Some((op, l, r)) = binary_parts(cur)
            && binary_level(cur) == level
        {
            rhs.push((op, r));
            cur = l;
        }
        let first = self.expr(cur);
        let mut end = cur.span.hi;
        let mut tail = Vec::new();
        let last = rhs.len().saturating_sub(1);
        for (i, (op, r)) in rhs.into_iter().rev().enumerate() {
            let pre = self.inline_comments(self.next_tok(end));
            let mut operand = self.expr(r);
            // `x ?? match y {`: the last operand may break on its own.
            if i == last && (is_block_like(r) || is_huggable(r)) {
                mark_hug(self.budget, &mut operand);
            }
            tail.extend([Doc::Line, prepend(pre, text(format!("{op} "))), operand]);
            end = r.span.hi;
        }
        group(self.budget, false, cat(vec![first, indent(cat(tail))]))
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
            parts.push(self.punct(then.span.hi, &Tok::Else, " else "));
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
        strict_group(self.budget, !flat, cat(parts))
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
        let mut tail = Vec::new();
        for op in links {
            if is_call(op) {
                tail.push(Doc::SoftLine);
            }
            tail.push(self.postfix(op));
        }
        group(self.budget, false, cat(vec![cat(head), indent(cat(tail))]))
    }

    fn postfix(&mut self, op: &'a Expr) -> Doc {
        match &op.kind {
            ExprKind::Call { callee, args } => self.call_args(args, callee.span.hi, op.span.hi - 1),
            ExprKind::MethodCall { receiver, method, args } => {
                let name = self.dot(receiver.span.hi, |f| f.name(method));
                cat(vec![name, self.call_args(args, method.span.hi, op.span.hi - 1)])
            }
            ExprKind::Field(x, name) => self.dot(x.span.hi, |f| f.name(name)),
            ExprKind::TupleField(x, i, span) => {
                let field = self.dot(x.span.hi, |f| prepend(f.inline_comments(span.lo), text(i.to_string())));
                // Without the space, `1.0` is a float literal.
                let number = matches!(x.kind, ExprKind::Int(_) | ExprKind::IntOverflow(_) | ExprKind::Float(_));
                if number { cat(vec![text(" "), field]) } else { field }
            }
            ExprKind::Index(x, idx) => {
                let open = self.punct(x.span.hi, &Tok::LBracket, "[");
                cat(vec![open, self.expr(idx), self.text_at(op.span.hi - 1, "]")])
            }
            // `x??` would lex as the `??` operator.
            ExprKind::Try(x) => self.text_at(op.span.hi - 1, if matches!(x.kind, ExprKind::Try(_)) { " ?" } else { "?" }),
            _ => self.expr(op),
        }
    }

    /// Lays out the sub-patterns of a pattern between `brackets`, as
    /// [`Self::call_args`] lays out arguments.
    fn pattern_list(&mut self, brackets: (&'static str, &'static str), open: u32, close: u32, elems: &[CElem<'a>]) -> Doc {
        let atom = |el: &CElem| match el {
            CElem::Pat(p) => {
                matches!(p.kind, PatternKind::Wildcard | PatternKind::Binding(_) | PatternKind::Literal(_) | PatternKind::Range(..))
            }
            CElem::Rest(..) => true,
            _ => false,
        };
        if let [el] = elems
            && atom(el)
            && !self.has_comment_before(close)
        {
            let pre = self.inline_comments(open);
            return prepend(pre, cat(vec![text(brackets.0), self.celem(*el), text(brackets.1)]));
        }
        self.comma_list(brackets, open, close, elems, PATTERN)
    }

    /// Call arguments; the `(` is the first one after `after`.
    fn call_args(&mut self, args: &'a [Arg], after: u32, close: u32) -> Doc {
        let open = self.find_tok(after, &Tok::LParen);
        let pre = self.inline_comments(open);
        if let [arg] = args {
            // A lone literal or name gains nothing from a line of its own.
            if arg.name.is_none() && !arg.spread && is_atom(&arg.value) && !self.has_comment_before(close) {
                return prepend(pre, cat(vec![text("("), self.expr(&arg.value), text(")")]));
            }
        }
        let elems: Vec<CElem> =
            args.iter().map(|a| if a.spread { CElem::Base(&a.value, self.prev_tok(a.value.span.lo)) } else { CElem::Arg(a) }).collect();
        let hug = args.last().is_some_and(|a| is_huggable(&a.value));
        prepend(pre, self.comma_list(("(", ")"), open, close, &elems, ListOpts { hug_last: hug, ..LIST }))
    }

    fn arm(&mut self, a: &'a MatchArm) -> Doc {
        let mut parts = vec![self.pat(&a.pattern)];
        if let Some(g) = &a.guard {
            parts.push(self.punct(a.pattern.span.hi, &Tok::If, " if "));
            parts.push(self.expr(g));
        }
        let end = a.guard.as_ref().map_or(a.pattern.span.hi, |g| g.span.hi);
        parts.push(self.punct(end, &Tok::FatArrow, " => "));
        parts.push(self.expr(&a.body));
        cat(parts)
    }

    // ------------------------------------------------------------------
    // Types and patterns

    fn ty(&mut self, t: &'a TypeExpr) -> Doc {
        if !self.budget.charge(1) {
            return text("");
        }
        let pre = self.inline_comments(t.span.lo);
        let doc = match &t.kind {
            TypeExprKind::Named { path, args } => {
                let mut parts = vec![self.path(path)];
                if let Some(last) = args.last() {
                    parts.push(self.punct(path[path.len() - 1].span.hi, &Tok::Lt, "<"));
                    parts.push(self.join(args, Self::ty));
                    parts.push(self.text_at(self.next_tok(last.span.hi), ">"));
                }
                cat(parts)
            }
            TypeExprKind::List(x) => cat(vec![text("["), self.ty(x), self.punct(x.span.hi, &Tok::RBracket, "]")]),
            TypeExprKind::Map(k, v) => cat(vec![
                text("["),
                self.ty(k),
                self.punct(k.span.hi, &Tok::Colon, ": "),
                self.ty(v),
                self.punct(v.span.hi, &Tok::RBracket, "]"),
            ]),
            TypeExprKind::Tuple(xs) => {
                let items = self.join(xs, Self::ty);
                let close = self.punct(xs.last().map_or(t.span.lo, |x| x.span.hi), &Tok::RParen, ")");
                cat(vec![text("("), items, close])
            }
            TypeExprKind::Option(x) => {
                // `(fn() -> T)?` needs its parentheses: `fn() -> T?` returns an option.
                if matches!(x.kind, TypeExprKind::Fn(_, Some(_))) {
                    cat(vec![text("("), self.ty(x), text(")?")])
                } else {
                    let inner = self.ty(x);
                    // `T??` is one token.
                    let question = if x.span.hi == t.span.hi { text("?") } else { self.text_at(self.next_tok(x.span.hi), "?") };
                    cat(vec![inner, question])
                }
            }
            TypeExprKind::Fn(params, ret) => {
                let open = self.punct(t.span.lo, &Tok::LParen, "(");
                let items = self.join(params, Self::ty);
                let close = self.punct(params.last().map_or(t.span.lo, |p| p.span.hi), &Tok::RParen, ")");
                let mut parts = vec![text("fn"), open, items, close];
                if let Some(r) = ret {
                    parts.push(self.punct(params.last().map_or(t.span.lo, |p| p.span.hi), &Tok::Arrow, " -> "));
                    parts.push(self.ty(r));
                }
                cat(parts)
            }
            TypeExprKind::Unit => cat(vec![text("("), self.text_at(t.span.hi - 1, ")")]),
        };
        prepend(pre, doc)
    }

    fn pat(&mut self, p: &'a Pattern) -> Doc {
        if !self.budget.charge(1) {
            return text("");
        }
        let pre = self.inline_comments(p.span.lo);
        let doc = match &p.kind {
            PatternKind::Wildcard => text("_"),
            PatternKind::Binding(id) => text(&id.name),
            PatternKind::Literal(PatLit::Str(_)) => self.verbatim(p.span),
            PatternKind::Literal(PatLit::Bool(b)) => text(if *b { "true" } else { "false" }),
            PatternKind::Literal(PatLit::Unit) => cat(vec![text("("), self.text_at(p.span.hi - 1, ")")]),
            PatternKind::Literal(PatLit::Int(_)) | PatternKind::Range(..) => self.number(p.span),
            PatternKind::Tuple(ps) => {
                let elems: Vec<CElem> = ps.iter().map(CElem::Pat).collect();
                self.pattern_list(("(", ")"), p.span.lo, p.span.hi - 1, &elems)
            }
            PatternKind::Ctor { path, args } => {
                let name = self.path(path);
                match args {
                    Some(args) => {
                        let open = self.find_tok(path[path.len() - 1].span.hi, &Tok::LParen);
                        let mut elems: Vec<CElem> = args.positional.iter().map(CElem::Pat).collect();
                        elems.extend(args.named.iter().map(|(n, x)| CElem::NamedPat(n, x)));
                        if args.rest {
                            let after = args.patterns().map(|x| x.span.hi).max().unwrap_or(open);
                            elems.push(CElem::Rest(self.find_tok(after, &Tok::DotDot), None));
                        }
                        cat(vec![name, self.pattern_list(("(", ")"), open, p.span.hi - 1, &elems)])
                    }
                    None => name,
                }
            }
            PatternKind::List { before, rest, after } => {
                let mut elems: Vec<CElem> = before.iter().map(CElem::Pat).collect();
                if let Some(name) = rest {
                    let at = self.find_tok(before.last().map_or(p.span.lo, |x| x.span.hi), &Tok::DotDot);
                    elems.push(CElem::Rest(at, name.as_ref()));
                }
                elems.extend(after.iter().map(CElem::Pat));
                self.pattern_list(("[", "]"), p.span.lo, p.span.hi - 1, &elems)
            }
            PatternKind::Or(ps) => {
                let mut parts = Vec::new();
                for (i, x) in ps.iter().enumerate() {
                    if i > 0 {
                        parts.push(self.punct(ps[i - 1].span.hi, &Tok::Pipe, " | "));
                    }
                    parts.push(self.pat(x));
                }
                cat(parts)
            }
        };
        prepend(pre, doc)
    }
}
