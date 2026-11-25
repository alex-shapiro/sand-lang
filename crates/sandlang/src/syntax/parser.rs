//! Recursive-descent parser with precedence climbing for expressions.

use super::ast::*;
use super::lexer::{Comment, CommentKind, Lexed, Token, TokenKind, lex};
use crate::diag::{Diag, FileId, Span};

/// Maximum syntactic nesting depth (expressions, types, patterns, blocks).
pub const MAX_DEPTH: usize = 256;

/// Max number of syntax errors reported per module.
const MAX_ERRORS: usize = 50;

pub struct ParseResult {
    pub module: Module,
    pub comments: Vec<Comment>,
    /// The tokens, with statement terminators, for the formatter (the
    /// contents of string and bytes literals are moved into the syntax tree).
    pub tokens: Vec<Token>,
    pub errors: Vec<Diag>,
    /// Sorted start byte offsets of `///` comments attached to items.
    pub docs: Vec<u32>,
}

pub fn parse_module(src: &str, file: FileId) -> ParseResult {
    let Lexed { tokens, comments, errors, .. } = lex(src, file);
    let mut p = Parser::new(src, file, tokens, &comments);
    p.errors = errors;
    let module = p.module();
    let (tokens, mut errors, mut docs) = (p.toks, p.errors, p.docs.into_inner());
    errors.sort_by_key(|d| d.span.lo);
    errors.truncate(MAX_ERRORS);
    docs.sort_unstable();
    docs.dedup();
    ParseResult { module, comments, tokens, errors, docs }
}

struct Parser<'a> {
    src: &'a str,
    file: FileId,
    toks: Vec<Token>,
    pos: usize,
    comments: &'a [Comment],
    errors: Vec<Diag>,
    /// In a match guard: `x => ...` ends the guard rather than starting a lambda.
    in_guard: bool,
    depth: usize,
    fatal: bool,
    /// Inside a `trait` body: methods may omit their body.
    in_trait: bool,
    docs: std::cell::RefCell<Vec<u32>>,
}

type PResult<T> = Result<T, ()>;

impl<'a> Parser<'a> {
    fn new(src: &'a str, file: FileId, toks: Vec<Token>, comments: &'a [Comment]) -> Parser<'a> {
        Parser {
            src,
            file,
            toks,
            pos: 0,
            comments,
            errors: Vec::new(),
            in_guard: false,
            depth: 0,
            fatal: false,
            in_trait: false,
            docs: Default::default(),
        }
    }

    // ------------------------------------------------------------------
    // Token helpers

    fn tok(&self) -> &TokenKind {
        &self.toks[self.pos].kind
    }
    fn peek_tok(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].kind
    }
    fn at(&self, t: &TokenKind) -> bool {
        std::mem::discriminant(self.tok()) == std::mem::discriminant(t)
    }
    fn span_here(&self) -> Span {
        let t = &self.toks[self.pos];
        Span::new(self.file, t.lo, t.hi)
    }
    fn prev_hi(&self) -> u32 {
        if self.pos == 0 { 0 } else { self.toks[self.pos - 1].hi }
    }
    fn lo(&self) -> u32 {
        self.toks[self.pos].lo
    }
    fn span_from(&self, lo: u32) -> Span {
        Span::new(self.file, lo, self.prev_hi().max(lo))
    }
    /// Moves to the next token; returns the span of the current one.
    fn bump(&mut self) -> Span {
        let span = self.span_here();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        span
    }
    /// The text of a string token (`Str` or `StrText`), taken from it, and
    /// moves past it.
    fn take_text(&mut self) -> String {
        let text = match &mut self.toks[self.pos].kind {
            TokenKind::Str(s) | TokenKind::StrText(s) => std::mem::take(s),
            _ => String::new(),
        };
        self.bump();
        text
    }
    fn eat(&mut self, t: &TokenKind) -> bool {
        if self.at(t) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn slice(&self, span: Span) -> &'a str {
        &self.src[span.lo as usize..span.hi as usize]
    }
    fn cur_text(&self) -> &'a str {
        self.slice(self.span_here())
    }
    fn peek_text(&self, n: usize) -> &'a str {
        let t = &self.toks[(self.pos + n).min(self.toks.len() - 1)];
        &self.src[t.lo as usize..t.hi as usize]
    }
    fn error_here(&mut self, msg: impl Into<String>) {
        let span = self.span_here();
        self.errors.push(Diag::error(span, msg));
    }
    fn expected(&mut self, what: &str) {
        let found = self.tok().describe();
        self.error_here(format!("expected {what}, found {found}"));
    }
    fn expect(&mut self, t: &TokenKind, what: &str) -> PResult<()> {
        if self.eat(t) {
            Ok(())
        } else {
            self.expected(what);
            Err(())
        }
    }
    fn skip_semis(&mut self) {
        while self.at(&TokenKind::Semi) {
            self.bump();
        }
    }
    fn mk(&self, kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }
    fn ident(&mut self, what: &str) -> PResult<Ident> {
        if self.at(&TokenKind::Ident) {
            let span = self.bump();
            Ok(Ident { name: self.slice(span).to_string(), span })
        } else {
            self.expected(what);
            Err(())
        }
    }
    /// An identifier, also accepting keywords (for function and member names).
    fn name_or_keyword(&mut self, what: &str) -> PResult<Ident> {
        if self.at(&TokenKind::Ident) || (self.tok().is_keyword() && !self.at(&TokenKind::SelfLower)) {
            let span = self.bump();
            Ok(Ident { name: self.slice(span).to_string(), span })
        } else {
            self.expected(what);
            Err(())
        }
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            if !self.fatal {
                self.error_here("nesting too deep");
            }
            self.fatal = true;
        }
        if self.fatal { Err(()) } else { Ok(()) }
    }
    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn doc_before(&self, lo: u32) -> Option<String> {
        let (doc, starts) = self.doc_comments_before(lo);
        self.docs.borrow_mut().extend(starts);
        doc
    }

    fn doc_comments_before(&self, lo: u32) -> (Option<String>, Vec<u32>) {
        let idx = self.comments.partition_point(|c| c.hi <= lo);
        let mut lines = Vec::new();
        let mut starts = Vec::new();
        let mut boundary = lo;
        let mut i = idx;
        while i > 0 {
            let c = &self.comments[i - 1];
            if c.kind != CommentKind::Doc {
                break;
            }
            let between = &self.src[c.hi as usize..boundary as usize];
            let line_start = self.src[..c.lo as usize].rfind('\n').map_or(0, |i| i + 1);
            let after_code = !self.src[line_start..c.lo as usize].trim().is_empty();
            if !between.trim().is_empty() || between.matches('\n').count() > 1 || after_code {
                break;
            }
            let t = c.text(self.src).trim_start_matches("///");
            lines.push(t.strip_prefix(' ').unwrap_or(t).trim_end().to_string());
            starts.push(c.lo);
            boundary = c.lo;
            i -= 1;
        }
        if lines.is_empty() {
            return (None, starts);
        }
        lines.reverse();
        (Some(lines.join("\n")), starts)
    }

    /// Skips to the next statement boundary after an error.
    fn recover(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.tok() {
                TokenKind::Eof => return,
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket => depth -= 1,
                TokenKind::RBrace => {
                    if depth <= 0 {
                        return;
                    }
                    depth -= 1;
                }
                TokenKind::Semi if depth <= 0 => {
                    self.bump();
                    return;
                }
                _ => {}
            }
            self.bump();
        }
    }

    // ------------------------------------------------------------------
    // Module and items

    fn module(&mut self) -> Module {
        let mut items = Vec::new();
        let code = self.toks.iter().find(|t| t.kind != TokenKind::Semi).map_or(u32::MAX, |t| t.lo);
        for c in self.comments.iter().filter(|c| c.kind == CommentKind::ModuleDoc && c.lo > code) {
            let msg = "a `//!` module documentation comment must come before the first item; use `//` here";
            self.errors.push(Diag::error(Span::new(self.file, c.lo, c.hi), msg));
        }
        let doc: Vec<String> = self
            .comments
            .iter()
            .take_while(|c| c.lo < code)
            .filter(|c| c.kind == CommentKind::ModuleDoc)
            .map(|c| {
                let t = c.text(self.src).trim_start_matches("//!");
                t.strip_prefix(' ').unwrap_or(t).trim_end().to_string()
            })
            .collect();
        loop {
            self.skip_semis();
            if self.at(&TokenKind::Eof) || self.fatal {
                break;
            }
            let start = self.pos;
            match self.item() {
                Ok(item) => items.push(item),
                Err(()) => {
                    if self.fatal {
                        break;
                    }
                    if self.pos == start {
                        self.bump();
                    }
                    self.recover();
                    if self.at(&TokenKind::RBrace) {
                        self.error_here("unexpected `}`");
                        self.bump();
                    }
                }
            }
        }
        let span = Span::new(self.file, 0, self.src.len() as u32);
        Module { items, doc: if doc.is_empty() { None } else { Some(doc.join("\n")) }, span }
    }

    fn end_of_item(&mut self) -> PResult<()> {
        if self.eat(&TokenKind::Semi) || self.at(&TokenKind::Eof) || self.at(&TokenKind::RBrace) || self.toks[self.pos].newline_before {
            Ok(())
        } else {
            self.expected("newline or `;`");
            Err(())
        }
    }

    fn item(&mut self) -> PResult<Item> {
        let lo = self.lo();
        let (doc, starts) = self.doc_comments_before(lo);
        let kind = match self.tok() {
            TokenKind::Import => ItemKind::Import(self.import()?),
            TokenKind::Ident if self.is_at_selective_import() => ItemKind::Import(self.selective_import()?),
            TokenKind::Pub => {
                self.bump();
                match self.tok() {
                    TokenKind::Native if matches!(self.peek_tok(1), TokenKind::Type) => return self.native_type(),
                    TokenKind::Fn | TokenKind::Native => ItemKind::Fn(self.fn_decl(true, doc)?),
                    TokenKind::Struct => ItemKind::Struct(self.struct_decl(true, doc)?),
                    TokenKind::Enum => ItemKind::Enum(self.enum_decl(true, doc)?),
                    TokenKind::Type => ItemKind::TypeAlias(self.type_alias(true, doc)?),
                    TokenKind::Trait => ItemKind::Trait(self.trait_decl(true, doc)?),
                    TokenKind::Let | TokenKind::Var => {
                        let mut s = self.let_stmt()?;
                        if let StmtKind::Let { is_pub, doc: d, .. } = &mut s.kind {
                            *is_pub = true;
                            *d = doc;
                        }
                        self.end_of_item()?;
                        ItemKind::Stmt(s)
                    }
                    _ => {
                        self.expected("`fn`, `struct`, `enum`, `trait`, `type`, `let` or `var` after `pub`");
                        return Err(());
                    }
                }
            }
            TokenKind::Fn if matches!(self.peek_tok(1), TokenKind::Ident) => ItemKind::Fn(self.fn_decl(false, doc)?),
            TokenKind::Native if matches!(self.peek_tok(1), TokenKind::Type) => return self.native_type(),
            TokenKind::Native => ItemKind::Fn(self.fn_decl(false, doc)?),
            TokenKind::Struct => ItemKind::Struct(self.struct_decl(false, doc)?),
            TokenKind::Enum => ItemKind::Enum(self.enum_decl(false, doc)?),
            TokenKind::Extend => ItemKind::Extend(self.extend_decl(doc)?),
            TokenKind::Trait => ItemKind::Trait(self.trait_decl(false, doc)?),
            TokenKind::Impl => ItemKind::Impl(self.impl_decl()?),
            TokenKind::Type if matches!(self.peek_tok(1), TokenKind::Ident) => ItemKind::TypeAlias(self.type_alias(false, doc)?),
            TokenKind::Ident if self.cur_text() == "test" && matches!(self.peek_tok(1), TokenKind::Str(_) | TokenKind::StrStart) => {
                self.bump();
                let name_span = self.span_here();
                let name = self.plain_string("a test name")?;
                let body = self.block()?;
                ItemKind::Test(TestDecl { name, name_span, body })
            }
            TokenKind::Ident if self.cur_text() == "test" && matches!(self.peek_tok(1), TokenKind::LBrace) => {
                let name_span = self.span_here();
                self.errors.push(Diag::error(name_span, "expected a test name: `test \"...\" { ... }`"));
                self.bump();
                let body = self.block()?;
                ItemKind::Test(TestDecl { name: String::new(), name_span, body })
            }
            _ => {
                let mut s = self.stmt()?;
                if let StmtKind::Let { doc: d, .. } = &mut s.kind {
                    *d = doc;
                }
                self.end_of_item()?;
                ItemKind::Stmt(s)
            }
        };
        if !matches!(kind, ItemKind::Stmt(_) | ItemKind::Import(_)) {
            self.end_of_item()?;
        }
        let documented = match &kind {
            ItemKind::Import(_) | ItemKind::Impl(_) | ItemKind::Test(_) => false,
            ItemKind::Stmt(s) => matches!(s.kind, StmtKind::Let { .. } | StmtKind::Fn(_)),
            _ => true,
        };
        if documented {
            self.docs.borrow_mut().extend(starts);
        }
        Ok(Item { kind, span: self.span_from(lo) })
    }

    /// A string literal without interpolations.
    fn plain_string(&mut self, what: &str) -> PResult<String> {
        match self.tok() {
            TokenKind::Str(_) => Ok(self.take_text()),
            TokenKind::StrStart => {
                // The whole string, up to its (matching) end.
                let lo = self.lo();
                let mut depth = 0;
                let mut i = self.pos;
                while let Some(t) = self.toks.get(i) {
                    match t.kind {
                        TokenKind::StrStart => depth += 1,
                        TokenKind::StrEnd if depth == 1 => break,
                        TokenKind::StrEnd => depth -= 1,
                        TokenKind::Eof => break,
                        _ => {}
                    }
                    i += 1;
                }
                let hi = self.toks.get(i).map_or(lo, |t| t.hi);
                self.errors
                    .push(Diag::error(Span::new(self.file, lo, hi), format!("{what} must be a plain string, without interpolation")));
                Err(())
            }
            _ => {
                self.expected("string");
                Err(())
            }
        }
    }

    fn import_source(&mut self) -> PResult<ImportSource> {
        match self.tok() {
            TokenKind::Ident => Ok(ImportSource::Name(self.ident("module name")?)),
            TokenKind::Str(_) | TokenKind::StrStart => {
                let lo = self.lo();
                let path = self.plain_string("an import path")?;
                Ok(ImportSource::Path(path, self.span_from(lo)))
            }
            _ => {
                self.expected("module name or path string");
                Err(())
            }
        }
    }

    fn import(&mut self) -> PResult<Import> {
        self.bump();
        let source = self.import_source()?;
        let alias = if self.eat(&TokenKind::As) { Some(self.ident("alias")?) } else { None };
        self.end_of_item()?;
        Ok(Import { source, alias, names: None })
    }

    fn is_at_selective_import(&self) -> bool {
        self.cur_text() == "from"
            && matches!(self.peek_tok(1), TokenKind::Ident | TokenKind::Str(_))
            && matches!(self.peek_tok(2), TokenKind::Import)
    }

    fn selective_import(&mut self) -> PResult<Import> {
        self.bump();
        let source = self.import_source()?;
        self.expect(&TokenKind::Import, "`import`")?;
        let mut names = vec![self.ident("name to import")?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.ident("name to import")?);
        }
        self.end_of_item()?;
        Ok(Import { source, alias: None, names: Some(names) })
    }

    fn generics(&mut self) -> PResult<Vec<GenericParam>> {
        let mut out = Vec::new();
        if !self.eat(&TokenKind::Lt) {
            return Ok(out);
        }
        loop {
            let name = self.ident("type parameter")?;
            let traits = if self.eat(&TokenKind::Colon) { self.traits()? } else { Vec::new() };
            out.push(GenericParam { name, traits });
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            self.expect_gt()?;
            break;
        }
        Ok(out)
    }

    /// `A + B + ...` where each trait is `Name`, `module.Name` or `Name<Args>`.
    fn traits(&mut self) -> PResult<Vec<TypeExpr>> {
        let mut out = vec![self.trait_name()?];
        while self.eat(&TokenKind::Plus) {
            out.push(self.trait_name()?);
        }
        Ok(out)
    }

    fn trait_name(&mut self) -> PResult<TypeExpr> {
        let lo = self.lo();
        let mut path = vec![self.ident("trait")?];
        while self.at(&TokenKind::Dot) && matches!(self.peek_tok(1), TokenKind::Ident) {
            self.bump();
            path.push(self.ident("trait")?);
        }
        let mut args = Vec::new();
        if self.eat(&TokenKind::Lt) {
            loop {
                args.push(self.ty()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_gt()?;
        }
        Ok(TypeExpr { kind: TypeExprKind::Named { path, args }, span: self.span_from(lo) })
    }

    /// An optional `where T: A, U: B + C` clause.
    fn where_clause(&mut self) -> PResult<Vec<WherePredicate>> {
        let mut out = Vec::new();
        if self.at(&TokenKind::Ident) && self.cur_text() == "where" {
            self.bump();
            loop {
                let name = self.ident("type parameter")?;
                self.expect(&TokenKind::Colon, "`:`")?;
                let traits = self.traits()?;
                out.push(WherePredicate { name, traits });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Expects `>`, splitting `>>` and `>=` tokens when needed.
    fn expect_gt(&mut self) -> PResult<()> {
        match self.tok() {
            TokenKind::Gt => {
                self.bump();
                Ok(())
            }
            TokenKind::Shr => {
                self.toks[self.pos].kind = TokenKind::Gt;
                self.toks[self.pos].lo += 1;
                Ok(())
            }
            TokenKind::Ge => {
                self.toks[self.pos].kind = TokenKind::Eq;
                self.toks[self.pos].lo += 1;
                Ok(())
            }
            _ => {
                self.expected("`>`");
                Err(())
            }
        }
    }

    fn fn_decl(&mut self, is_pub: bool, doc: Option<String>) -> PResult<FnDecl> {
        // Only the trait method itself may omit its body, not local functions inside it.
        let bodyless_ok = std::mem::replace(&mut self.in_trait, false);
        let lo = self.lo();
        let is_native = self.eat(&TokenKind::Native);
        self.expect(&TokenKind::Fn, "`fn`")?;
        let name = self.name_or_keyword("function name")?;
        let generics = self.generics()?;
        self.expect(&TokenKind::LParen, "`(`")?;
        let mut params = Vec::new();
        let mut seen_default = false;
        while !self.at(&TokenKind::RParen) {
            let plo = self.lo();
            if self.at(&TokenKind::SelfLower) {
                let span = self.bump();
                params.push(Param { name: Ident { name: "self".into(), span }, ty: None, default: None, span: self.span_from(plo) });
            } else {
                let pname = self.ident("parameter name")?;
                self.expect(&TokenKind::Colon, "`:`")?;
                let ty = self.ty()?;
                let default = if self.eat(&TokenKind::Eq) {
                    seen_default = true;
                    Some(self.expr()?)
                } else {
                    if seen_default {
                        self.errors.push(Diag::error(pname.span, "parameter without default after parameter with default"));
                    }
                    None
                };
                params.push(Param { name: pname, ty: Some(ty), default, span: self.span_from(plo) });
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`")?;
        let ret = if self.eat(&TokenKind::Arrow) { Some(self.ty()?) } else { None };
        let where_clause = self.where_clause()?;
        let body = if is_native || (bodyless_ok && !self.at(&TokenKind::LBrace)) {
            None
        } else if self.at(&TokenKind::LBrace) {
            Some(self.block()?)
        } else {
            self.expected("`{`");
            return Err(());
        };
        Ok(FnDecl { name, generics, params, ret, where_clause, body, is_pub, is_native, doc, span: self.span_from(lo) })
    }

    fn struct_decl(&mut self, is_pub: bool, doc: Option<String>) -> PResult<StructDecl> {
        self.bump();
        let name = self.ident("struct name")?;
        let generics = self.generics()?;
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        loop {
            while self.eat(&TokenKind::Semi) || self.eat(&TokenKind::Comma) {}
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            let mlo = self.lo();
            let mdoc = self.doc_before(mlo);
            if self.at(&TokenKind::Fn)
                || self.at(&TokenKind::Native)
                || (self.at(&TokenKind::Pub) && matches!(self.peek_tok(1), TokenKind::Fn | TokenKind::Native))
            {
                let p = self.eat(&TokenKind::Pub);
                methods.push(self.fn_decl(p, mdoc)?);
                continue;
            }
            let is_pub = self.eat(&TokenKind::Pub);
            let mutable = self.eat(&TokenKind::Var);
            let fname = self.ident("field name")?;
            self.expect(&TokenKind::Colon, "`:`")?;
            let ty = self.ty()?;
            let default = if self.eat(&TokenKind::Eq) { Some(self.expr()?) } else { None };
            fields.push(FieldDecl { name: fname, ty, default, mutable, is_pub, doc: mdoc, span: self.span_from(mlo) });
            // A type ending in `>` is not followed by an automatic terminator.
            if !(self.at(&TokenKind::Semi)
                || self.at(&TokenKind::Comma)
                || self.at(&TokenKind::RBrace)
                || self.toks[self.pos].newline_before)
            {
                self.expected("`,`, newline or `}`");
                return Err(());
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Ok(StructDecl { name, generics, fields, methods, is_pub, doc })
    }

    fn enum_decl(&mut self, is_pub: bool, doc: Option<String>) -> PResult<EnumDecl> {
        self.bump();
        let name = self.ident("enum name")?;
        let generics = self.generics()?;
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut variants = Vec::new();
        let mut methods = Vec::new();
        loop {
            while self.eat(&TokenKind::Semi) || self.eat(&TokenKind::Comma) {}
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            let vlo = self.lo();
            let vdoc = self.doc_before(vlo);
            if self.at(&TokenKind::Fn)
                || self.at(&TokenKind::Native)
                || (self.at(&TokenKind::Pub) && matches!(self.peek_tok(1), TokenKind::Fn | TokenKind::Native))
            {
                let p = self.eat(&TokenKind::Pub);
                methods.push(self.fn_decl(p, vdoc)?);
                continue;
            }
            let vname = self.ident("variant name")?;
            let mut fields = Vec::new();
            if self.eat(&TokenKind::LParen) {
                while !self.at(&TokenKind::RParen) {
                    let fname = if self.at(&TokenKind::Ident) && matches!(self.peek_tok(1), TokenKind::Colon) {
                        let n = self.ident("field name")?;
                        self.bump();
                        Some(n)
                    } else {
                        None
                    };
                    let ty = self.ty()?;
                    fields.push(VariantField { name: fname, ty });
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RParen, "`)`")?;
            }
            variants.push(VariantDecl { name: vname, fields, doc: vdoc, span: self.span_from(vlo) });
            if !(self.at(&TokenKind::Semi)
                || self.at(&TokenKind::Comma)
                || self.at(&TokenKind::RBrace)
                || self.toks[self.pos].newline_before)
            {
                self.expected("`,`, newline or `}`");
                return Err(());
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Ok(EnumDecl { name, generics, variants, methods, is_pub, doc })
    }

    fn extend_decl(&mut self, doc: Option<String>) -> PResult<ExtendDecl> {
        self.bump();
        let generics = self.generics()?;
        let target = self.ty()?;
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut methods = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            let mdoc = self.doc_before(self.lo());
            let p = self.eat(&TokenKind::Pub);
            if !(self.at(&TokenKind::Fn) || self.at(&TokenKind::Native)) {
                self.expected("`fn`");
                return Err(());
            }
            methods.push(self.fn_decl(p, mdoc)?);
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Ok(ExtendDecl { generics, target, methods, doc })
    }

    /// Methods of a `trait` or `impl` body: doc comments and `fn` declarations.
    fn trait_body(&mut self, in_trait: bool) -> PResult<Vec<FnDecl>> {
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut methods = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            let mdoc = self.doc_before(self.lo());
            if self.at(&TokenKind::Pub) {
                let what = if in_trait { "trait" } else { "impl" };
                self.error_here(format!("`pub` is not allowed on {what} methods; they are public if the trait is"));
                self.bump();
            }
            if !(self.at(&TokenKind::Fn) || self.at(&TokenKind::Native)) {
                self.expected("`fn`");
                return Err(());
            }
            self.in_trait = in_trait;
            methods.push(self.fn_decl(false, mdoc)?);
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Ok(methods)
    }

    fn trait_decl(&mut self, is_pub: bool, doc: Option<String>) -> PResult<TraitDecl> {
        self.bump();
        let name = self.ident("trait name")?;
        let generics = self.generics()?;
        let supers = if self.eat(&TokenKind::Colon) { self.traits()? } else { Vec::new() };
        let methods = self.trait_body(true)?;
        Ok(TraitDecl { name, generics, supers, methods, is_pub, doc })
    }

    fn impl_decl(&mut self) -> PResult<ImplDecl> {
        self.bump();
        let generics = self.generics()?;
        let trait_ref = self.trait_name()?;
        self.expect(&TokenKind::For, "`for`")?;
        let target = self.ty()?;
        let where_clause = self.where_clause()?;
        let methods = self.trait_body(false)?;
        Ok(ImplDecl { generics, trait_ref, target, where_clause, methods })
    }

    fn native_type<T>(&mut self) -> PResult<T> {
        self.error_here("native types are declared by registering Rust types with `NativeModule`, not in Sand source");
        Err(())
    }

    fn type_alias(&mut self, is_pub: bool, doc: Option<String>) -> PResult<TypeAlias> {
        self.bump();
        let name = self.ident("type name")?;
        let generics = self.generics()?;
        self.expect(&TokenKind::Eq, "`=`")?;
        let ty = self.ty()?;
        Ok(TypeAlias { name, generics, ty, is_pub, doc })
    }

    // ------------------------------------------------------------------
    // Types

    fn ty(&mut self) -> PResult<TypeExpr> {
        self.enter()?;
        let r = self.ty_inner();
        self.leave();
        r
    }

    fn ty_inner(&mut self) -> PResult<TypeExpr> {
        let lo = self.lo();
        let mut t = match self.tok() {
            TokenKind::LBracket => {
                self.bump();
                let a = self.ty()?;
                if self.eat(&TokenKind::Colon) {
                    let b = self.ty()?;
                    self.expect(&TokenKind::RBracket, "`]`")?;
                    TypeExpr { kind: TypeExprKind::Map(Box::new(a), Box::new(b)), span: self.span_from(lo) }
                } else {
                    self.expect(&TokenKind::RBracket, "`]`")?;
                    TypeExpr { kind: TypeExprKind::List(Box::new(a)), span: self.span_from(lo) }
                }
            }
            TokenKind::LParen => {
                self.bump();
                if self.eat(&TokenKind::RParen) {
                    TypeExpr { kind: TypeExprKind::Unit, span: self.span_from(lo) }
                } else {
                    let first = self.ty()?;
                    if self.eat(&TokenKind::RParen) {
                        first
                    } else {
                        let items = self.tuple_rest(first, lo, Self::ty)?;
                        TypeExpr { kind: TypeExprKind::Tuple(items), span: self.span_from(lo) }
                    }
                }
            }
            TokenKind::Fn => {
                self.bump();
                self.expect(&TokenKind::LParen, "`(`")?;
                let mut params = Vec::new();
                while !self.at(&TokenKind::RParen) {
                    params.push(self.ty()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RParen, "`)`")?;
                let ret = if self.eat(&TokenKind::Arrow) { Some(Box::new(self.ty()?)) } else { None };
                TypeExpr { kind: TypeExprKind::Fn(params, ret), span: self.span_from(lo) }
            }
            TokenKind::Ident | TokenKind::SelfUpper => {
                let mut path = Vec::new();
                if self.at(&TokenKind::SelfUpper) {
                    let span = self.bump();
                    path.push(Ident { name: "Self".into(), span });
                } else {
                    path.push(self.ident("type")?);
                    while self.at(&TokenKind::Dot) && matches!(self.peek_tok(1), TokenKind::Ident) {
                        self.bump();
                        path.push(self.ident("type")?);
                    }
                }
                let mut args = Vec::new();
                if self.eat(&TokenKind::Lt) {
                    loop {
                        args.push(self.ty()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect_gt()?;
                }
                TypeExpr { kind: TypeExprKind::Named { path, args }, span: self.span_from(lo) }
            }
            _ => {
                self.expected("type");
                return Err(());
            }
        };
        while self.at(&TokenKind::Question) || self.at(&TokenKind::QuestionQuestion) {
            let double = self.at(&TokenKind::QuestionQuestion);
            self.bump();
            t = TypeExpr { kind: TypeExprKind::Option(Box::new(t)), span: self.span_from(lo) };
            if double {
                t = TypeExpr { kind: TypeExprKind::Option(Box::new(t)), span: self.span_from(lo) };
            }
        }
        Ok(t)
    }

    /// The rest of a parenthesized tuple (type, pattern or expression)
    /// after its first element: `, b, c)`, with an optional trailing comma.
    /// A tuple has two or more elements (§5.2), so `(a,)` is an error.
    fn tuple_rest<T>(&mut self, first: T, lo: u32, mut elem: impl FnMut(&mut Self) -> PResult<T>) -> PResult<Vec<T>> {
        let mut items = vec![first];
        while self.eat(&TokenKind::Comma) {
            if self.at(&TokenKind::RParen) {
                break;
            }
            items.push(elem(self)?);
        }
        self.expect(&TokenKind::RParen, "`)` or `,`")?;
        if items.len() == 1 {
            self.errors.push(Diag::error(self.span_from(lo), "a tuple has two or more elements; without the comma, `(x)` is just `x`"));
        }
        Ok(items)
    }

    // ------------------------------------------------------------------
    // Statements and blocks

    fn block(&mut self) -> PResult<Block> {
        self.enter()?;
        let r = self.block_inner();
        self.leave();
        r
    }

    fn block_inner(&mut self) -> PResult<Block> {
        let lo = self.lo();
        self.expect(&TokenKind::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) || self.fatal {
                break;
            }
            let start = self.pos;
            match self.stmt() {
                Ok(s) => {
                    stmts.push(s);
                    if !(self.eat(&TokenKind::Semi) || self.at(&TokenKind::RBrace)) {
                        self.expected("newline or `;`");
                        self.recover();
                    }
                }
                Err(()) => {
                    if self.fatal {
                        break;
                    }
                    if self.pos == start {
                        self.bump();
                    }
                    self.recover();
                }
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;
        Ok(Block { stmts, span: self.span_from(lo) })
    }

    fn let_stmt(&mut self) -> PResult<Stmt> {
        let lo = self.lo();
        let mut mutable = self.at(&TokenKind::Var);
        self.bump();
        if !mutable && self.cur_text() == "mut" && matches!(self.peek_tok(1), TokenKind::Ident | TokenKind::LParen) {
            self.error_here("Sand has no `let mut`; use `var` for a binding that can change");
            self.bump();
            mutable = true;
        }
        let pattern = self.pattern()?;
        let ty = if self.eat(&TokenKind::Colon) { Some(self.ty()?) } else { None };
        self.expect(&TokenKind::Eq, "`=`")?;
        let value = self.expr()?;
        let else_block = if self.at(&TokenKind::Else) {
            self.bump();
            Some(self.block()?)
        } else {
            None
        };
        Ok(Stmt { kind: StmtKind::Let { pattern, ty, value, else_block, mutable, is_pub: false, doc: None }, span: self.span_from(lo) })
    }

    /// `use name [: type] = value` or `use _ = value`.
    fn use_stmt(&mut self) -> PResult<Stmt> {
        let lo = self.lo();
        self.bump();
        if !self.at(&TokenKind::Ident) {
            self.expected("a name or `_` after `use` (a `use` binding cannot be a pattern)");
            return Err(());
        }
        let name = self.ident("name")?;
        let ty = if self.eat(&TokenKind::Colon) { Some(self.ty()?) } else { None };
        self.expect(&TokenKind::Eq, "`=`")?;
        let value = self.expr()?;
        Ok(Stmt { kind: StmtKind::Use { name, ty, value }, span: self.span_from(lo) })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let lo = self.lo();
        match self.tok() {
            TokenKind::Import => {
                let msg = if matches!(self.peek_tok(1), TokenKind::Ident) && self.peek_text(1) == "test" {
                    "`import` is only allowed at the top level; `test` is already in scope in test blocks"
                } else {
                    "`import` is only allowed at the top level"
                };
                self.errors.push(Diag::error(self.span_here(), msg));
                Err(())
            }
            TokenKind::Ident if self.is_at_selective_import() => {
                self.errors.push(Diag::error(self.span_here(), "`import` is only allowed at the top level"));
                Err(())
            }
            TokenKind::Let | TokenKind::Var => self.let_stmt(),
            TokenKind::Defer => {
                self.bump();
                let e = self.expr()?;
                Ok(Stmt { kind: StmtKind::Defer(e), span: self.span_from(lo) })
            }
            TokenKind::Use => self.use_stmt(),
            TokenKind::Fn if matches!(self.peek_tok(1), TokenKind::Ident) => {
                let doc = self.doc_before(lo);
                let f = self.fn_decl(false, doc)?;
                Ok(Stmt { kind: StmtKind::Fn(f), span: self.span_from(lo) })
            }
            _ => {
                let e = self.expr()?;
                let op = match self.tok() {
                    TokenKind::Eq => Some(AssignOp::Assign),
                    TokenKind::PlusEq => Some(AssignOp::Add),
                    TokenKind::MinusEq => Some(AssignOp::Sub),
                    TokenKind::StarEq => Some(AssignOp::Mul),
                    TokenKind::SlashEq => Some(AssignOp::Div),
                    TokenKind::PercentEq => Some(AssignOp::Rem),
                    _ => None,
                };
                if let Some(op) = op {
                    self.bump();
                    let value = self.expr()?;
                    return Ok(Stmt { kind: StmtKind::Assign { target: e, op, value }, span: self.span_from(lo) });
                }
                Ok(Stmt { kind: StmtKind::Expr(e), span: self.span_from(lo) })
            }
        }
    }

    // ------------------------------------------------------------------
    // Expressions

    pub fn expr(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.expr_bp(0);
        self.leave();
        r
    }

    /// Binding powers: (left, right). Higher binds tighter.
    fn infix_bp(&self) -> Option<(u8, u8)> {
        Some(match self.tok() {
            TokenKind::QuestionQuestion => (5, 4),
            TokenKind::Or => (6, 7),
            TokenKind::And => (8, 9),
            TokenKind::EqEq | TokenKind::NotEq | TokenKind::Lt | TokenKind::Le | TokenKind::Gt | TokenKind::Ge => (12, 13),
            TokenKind::Pipe => (14, 15),
            TokenKind::Caret => (16, 17),
            TokenKind::Amp => (18, 19),
            TokenKind::Shl | TokenKind::Shr => (20, 21),
            TokenKind::DotDot | TokenKind::DotDotEq => (22, 23),
            TokenKind::Plus | TokenKind::Minus => (24, 25),
            TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (26, 27),
            _ => return None,
        })
    }

    fn expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        let lo = self.lo();
        let mut lhs = self.prefix(min_bp)?;
        let mut chain = 0usize;
        while let Some((lbp, rbp)) = self.infix_bp() {
            if self.toks[self.pos].negates {
                self.error_here(
                    "ambiguous `-`: with a space before it and none after, `-` negates the operand that follows; \
                     to subtract, put spaces on both sides (`a - b`)",
                );
                return Err(());
            }
            if lbp < min_bp {
                break;
            }
            chain += 1;
            if self.depth + chain > MAX_DEPTH * 4 {
                if !self.fatal {
                    self.error_here("expression too long or nesting too deep");
                }
                self.fatal = true;
                return Err(());
            }
            let op_tok = self.tok().clone();
            self.bump();
            let kind = match op_tok {
                TokenKind::QuestionQuestion => {
                    let rhs = self.expr_bp(rbp)?;
                    ExprKind::Coalesce(Box::new(lhs), Box::new(rhs))
                }
                TokenKind::DotDot | TokenKind::DotDotEq => {
                    let inclusive = op_tok == TokenKind::DotDotEq;
                    let end = if self.range_end_absent() { None } else { Some(Box::new(self.expr_bp(rbp)?)) };
                    let e = ExprKind::Range { start: Some(Box::new(lhs)), end, inclusive };
                    let ex = self.mk(e, self.span_from(lo));
                    if matches!(self.tok(), TokenKind::DotDot | TokenKind::DotDotEq) {
                        self.error_here("range operators cannot be chained");
                        return Err(());
                    }
                    lhs = ex;
                    continue;
                }
                ref t => {
                    let op = match t {
                        TokenKind::Or => BinOp::Or,
                        TokenKind::And => BinOp::And,
                        TokenKind::EqEq => BinOp::Eq,
                        TokenKind::NotEq => BinOp::Ne,
                        TokenKind::Lt => BinOp::Lt,
                        TokenKind::Le => BinOp::Le,
                        TokenKind::Gt => BinOp::Gt,
                        TokenKind::Ge => BinOp::Ge,
                        TokenKind::Pipe => BinOp::BitOr,
                        TokenKind::Caret => BinOp::BitXor,
                        TokenKind::Amp => BinOp::BitAnd,
                        TokenKind::Shl => BinOp::Shl,
                        TokenKind::Shr => BinOp::Shr,
                        TokenKind::Plus => BinOp::Add,
                        TokenKind::Minus => BinOp::Sub,
                        TokenKind::Star => BinOp::Mul,
                        TokenKind::Slash => BinOp::Div,
                        TokenKind::Percent => BinOp::Rem,
                        _ => unreachable!(),
                    };
                    let rhs = self.expr_bp(rbp)?;
                    if op.is_comparison() {
                        if let ExprKind::Binary(prev, ..) = &lhs.kind
                            && prev.is_comparison()
                            && !matches!(lhs.kind, ExprKind::Paren(_))
                        {
                            self.errors.push(Diag::error(self.span_from(lo), "comparison operators cannot be chained"));
                        }
                        if matches!(
                            self.tok(),
                            TokenKind::EqEq | TokenKind::NotEq | TokenKind::Lt | TokenKind::Le | TokenKind::Gt | TokenKind::Ge
                        ) {
                            self.error_here("comparison operators cannot be chained");
                            return Err(());
                        }
                    }
                    ExprKind::Binary(op, Box::new(lhs), Box::new(rhs))
                }
            };
            lhs = self.mk(kind, self.span_from(lo));
        }
        Ok(lhs)
    }

    fn range_end_absent(&self) -> bool {
        matches!(
            self.tok(),
            TokenKind::RBracket
                | TokenKind::RParen
                | TokenKind::Comma
                | TokenKind::Semi
                | TokenKind::Eof
                | TokenKind::RBrace
                | TokenKind::LBrace
        )
    }

    /// Prefix operators, `not`, `spawn` and lambdas; then postfix.
    fn prefix(&mut self, min_bp: u8) -> PResult<Expr> {
        let lo = self.lo();
        match self.tok() {
            TokenKind::Bang => {
                self.error_here("unexpected `!`; use `not` for negation");
                Err(())
            }
            TokenKind::Not => {
                self.bump();
                self.enter()?;
                // `not` binds looser than comparisons.
                let e = self.expr_bp(10.max(min_bp));
                self.leave();
                let e = e?;
                Ok(self.mk(ExprKind::Unary(UnOp::Not, Box::new(e)), self.span_from(lo)))
            }
            TokenKind::Minus | TokenKind::Tilde => {
                let op = if self.at(&TokenKind::Minus) { UnOp::Neg } else { UnOp::BitNot };
                self.bump();
                self.enter()?;
                let e = self.prefix(28);
                self.leave();
                let e = e?;
                // Fold `-<literal>` so that i64::MIN is expressible.
                if op == UnOp::Neg
                    && let ExprKind::IntOverflow(true) = e.kind
                    && !self.at(&TokenKind::StarStar)
                {
                    return Ok(self.mk(ExprKind::Int(i64::MIN), self.span_from(lo)));
                }
                let un = self.mk(ExprKind::Unary(op, Box::new(e)), self.span_from(lo));
                Ok(un)
            }
            TokenKind::DotDot | TokenKind::DotDotEq => {
                let inclusive = self.at(&TokenKind::DotDotEq);
                self.bump();
                let end = if self.range_end_absent() { None } else { Some(Box::new(self.expr_bp(23)?)) };
                Ok(self.mk(ExprKind::Range { start: None, end, inclusive }, self.span_from(lo)))
            }
            TokenKind::Spawn => {
                self.bump();
                let e = self.expr()?;
                Ok(self.mk(ExprKind::Spawn(Box::new(e)), self.span_from(lo)))
            }
            _ => {
                let base = self.postfix()?;
                if self.at(&TokenKind::StarStar) {
                    self.bump();
                    self.enter()?;
                    let rhs = self.prefix(28);
                    self.leave();
                    let rhs = rhs?;
                    return Ok(self.mk(ExprKind::Binary(BinOp::Pow, Box::new(base), Box::new(rhs)), self.span_from(lo)));
                }
                Ok(base)
            }
        }
    }

    fn args(&mut self) -> PResult<Vec<Arg>> {
        // Assumes `(` was consumed.
        let saved_guard = std::mem::replace(&mut self.in_guard, false);
        let r = self.args_inner();
        self.in_guard = saved_guard;
        r
    }

    fn args_inner(&mut self) -> PResult<Vec<Arg>> {
        let mut args = Vec::new();
        let mut seen_named = false;
        while !self.at(&TokenKind::RParen) {
            if args.last().is_some_and(|a: &Arg| a.spread) {
                self.error_here("`..base` must be the last argument");
            }
            if self.eat(&TokenKind::DotDot) {
                let value = self.expr()?;
                args.push(Arg { name: None, value, spread: true });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            let name = if self.at(&TokenKind::Ident) && matches!(self.peek_tok(1), TokenKind::Colon) {
                let n = self.ident("argument name")?;
                self.bump();
                seen_named = true;
                Some(n)
            } else {
                if seen_named {
                    self.error_here("positional argument after named argument");
                }
                None
            };
            let value = self.expr()?;
            args.push(Arg { name, value, spread: false });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`")?;
        Ok(args)
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        let mut e = self.primary()?;
        let mut chain = 0usize;
        loop {
            chain += 1;
            if self.depth + chain > MAX_DEPTH * 4 {
                if !self.fatal {
                    self.error_here("expression too long or nesting too deep");
                }
                self.fatal = true;
                return Err(());
            }
            match *self.tok() {
                TokenKind::LParen => {
                    self.bump();
                    let args = self.args()?;
                    e = self.mk(ExprKind::Call { callee: Box::new(e), args }, self.span_from(lo));
                }
                TokenKind::LBracket => {
                    self.bump();
                    let idx = self.expr()?;
                    self.expect(&TokenKind::RBracket, "`]`")?;
                    e = self.mk(ExprKind::Index(Box::new(e), Box::new(idx)), self.span_from(lo));
                }
                TokenKind::Question => {
                    self.bump();
                    e = self.mk(ExprKind::Try(Box::new(e)), self.span_from(lo));
                }
                TokenKind::Bang if !self.toks[self.pos].newline_before => {
                    self.error_here("Sand has no postfix `!`; use `?`, `??`, or `unwrap()`");
                    return Err(());
                }
                TokenKind::Dot => {
                    self.bump();
                    match *self.tok() {
                        TokenKind::Ident => e = self.member(e, lo)?,
                        _ if self.tok().is_keyword() => e = self.member(e, lo)?,
                        TokenKind::Int(i) => {
                            let span = self.bump();
                            let Ok(i) = u32::try_from(i) else {
                                self.errors.push(Diag::error(span, "invalid tuple index"));
                                return Err(());
                            };
                            e = self.mk(ExprKind::TupleField(Box::new(e), i, span), self.span_from(lo));
                        }
                        TokenKind::Float(_) => {
                            // `t.0.1` lexes as `t` `.` `0.1`.
                            let span = self.bump();
                            for part in self.slice(span).split('.') {
                                // Only plain digits (`t.0.1`, not `t.1e3`).
                                let index = part.bytes().all(|b| b.is_ascii_digit()).then(|| part.parse::<u32>().ok()).flatten();
                                let Some(i) = index else {
                                    self.errors.push(Diag::error(span, "invalid tuple index"));
                                    return Err(());
                                };
                                e = self.mk(ExprKind::TupleField(Box::new(e), i, span), self.span_from(lo));
                            }
                        }
                        _ => {
                            self.expected("field or method name");
                            return Err(());
                        }
                    }
                }
                TokenKind::LBrace if self.is_struct_literal(&e) => {
                    self.error_here("a struct is constructed with `Name(field: value)`");
                    return Err(());
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// Returns `true` if `e {` has the form of a brace-style struct literal
    /// (`Name { x: 1 }`) so the parser can report a targeted diagnostic.
    fn is_struct_literal(&self, e: &Expr) -> bool {
        let name = match &e.kind {
            ExprKind::Ident(id) | ExprKind::Field(_, id) => &id.name,
            ExprKind::SelfType => "Self",
            _ => return false,
        };
        name.starts_with(|c: char| c.is_ascii_uppercase())
            && match self.peek_tok(1) {
                TokenKind::DotDot => true,
                TokenKind::Ident => matches!(self.peek_tok(2), TokenKind::Colon | TokenKind::Comma),
                _ => false,
            }
    }

    /// `.name` or `.name(args)` after `e` (the `.` was consumed).
    fn member(&mut self, receiver: Expr, lo: u32) -> PResult<Expr> {
        let name = self.name_or_keyword("field or method name")?;
        let receiver = Box::new(receiver);
        if self.eat(&TokenKind::LParen) {
            let args = self.args()?;
            Ok(self.mk(ExprKind::MethodCall { receiver, method: name, args }, self.span_from(lo)))
        } else {
            Ok(self.mk(ExprKind::Field(receiver, name), self.span_from(lo)))
        }
    }

    fn lambda(&mut self, lo: u32) -> PResult<Expr> {
        let mut params = Vec::new();
        let parens = self.at(&TokenKind::LParen);
        if parens {
            self.bump();
            while !self.at(&TokenKind::RParen) {
                let name = if self.at(&TokenKind::Ident) {
                    self.ident("parameter")?
                } else {
                    self.expected("parameter name");
                    return Err(());
                };
                let ty = if self.eat(&TokenKind::Colon) { Some(self.ty()?) } else { None };
                params.push(LambdaParam { name, ty });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "`)`")?;
        } else {
            let name = self.ident("parameter")?;
            params.push(LambdaParam { name, ty: None });
        }
        self.expect(&TokenKind::FatArrow, "`=>`")?;
        let body = self.expr()?;
        Ok(self.mk(ExprKind::Lambda { params, body: Box::new(body), parens }, self.span_from(lo)))
    }

    fn cond(&mut self) -> PResult<Cond> {
        if self.eat(&TokenKind::Let) {
            let p = self.pattern()?;
            self.expect(&TokenKind::Eq, "`=`")?;
            let e = self.expr()?;
            Ok(Cond::Let(Box::new(p), Box::new(e)))
        } else {
            Ok(Cond::Expr(Box::new(self.expr()?)))
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        match *self.tok() {
            TokenKind::Int(v) => {
                self.bump();
                Ok(self.mk(ExprKind::Int(v), self.span_from(lo)))
            }
            TokenKind::IntOverflow(exact) => {
                self.bump();
                Ok(self.mk(ExprKind::IntOverflow(exact), self.span_from(lo)))
            }
            TokenKind::Float(v) => {
                self.bump();
                Ok(self.mk(ExprKind::Float(v), self.span_from(lo)))
            }
            TokenKind::Inf | TokenKind::Nan => {
                let v = if self.at(&TokenKind::Inf) { f64::INFINITY } else { f64::NAN };
                self.bump();
                Ok(self.mk(ExprKind::Float(v), self.span_from(lo)))
            }
            TokenKind::Duration(v) => {
                self.bump();
                Ok(self.mk(ExprKind::Duration(v), self.span_from(lo)))
            }
            TokenKind::True | TokenKind::False => {
                let value = self.at(&TokenKind::True);
                self.bump();
                Ok(self.mk(ExprKind::Bool(value), self.span_from(lo)))
            }
            TokenKind::Str(_) => {
                let text = self.take_text();
                Ok(self.mk(ExprKind::Str(vec![StrSeg::Text(text)]), self.span_from(lo)))
            }
            TokenKind::StrStart => self.interpolated_string(),
            TokenKind::Bytes(_) => {
                let bytes = match &mut self.toks[self.pos].kind {
                    TokenKind::Bytes(b) => std::mem::take(b),
                    _ => unreachable!(),
                };
                self.bump();
                Ok(self.mk(ExprKind::Bytes(bytes), self.span_from(lo)))
            }
            TokenKind::Ident => {
                if matches!(self.peek_tok(1), TokenKind::FatArrow) && !self.in_guard {
                    return self.lambda(lo);
                }
                if matches!(self.peek_tok(1), TokenKind::Bang)
                    && (matches!(self.peek_tok(2), TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket)
                        || MACROS.iter().any(|m| m.name == self.cur_text()))
                {
                    return self.macro_call(lo);
                }
                let id = self.ident("expression")?;
                Ok(self.mk(ExprKind::Ident(id), self.span_from(lo)))
            }
            TokenKind::SelfLower => {
                self.bump();
                Ok(self.mk(ExprKind::SelfValue, self.span_from(lo)))
            }
            TokenKind::SelfUpper => {
                self.bump();
                Ok(self.mk(ExprKind::SelfType, self.span_from(lo)))
            }
            TokenKind::LParen => {
                if !self.in_guard && self.toks[self.pos].opens_lambda {
                    return self.lambda(lo);
                }
                self.bump();
                if self.eat(&TokenKind::RParen) {
                    return Ok(self.mk(ExprKind::Unit, self.span_from(lo)));
                }
                let first = self.expr()?;
                if self.eat(&TokenKind::RParen) {
                    return Ok(self.mk(ExprKind::Paren(Box::new(first)), self.span_from(lo)));
                }
                let items = self.tuple_rest(first, lo, Self::expr)?;
                Ok(self.mk(ExprKind::Tuple(items), self.span_from(lo)))
            }
            TokenKind::LBracket => self.bracket(lo),
            TokenKind::LBrace => {
                let b = self.block()?;
                Ok(self.mk(ExprKind::Block(b), self.span_from(lo)))
            }
            TokenKind::If => self.if_expr(),
            TokenKind::Match => {
                self.bump();
                let scrutinee = self.expr()?;
                self.expect(&TokenKind::LBrace, "`{`")?;
                let mut arms = Vec::new();
                loop {
                    while self.eat(&TokenKind::Semi) || self.eat(&TokenKind::Comma) {}
                    if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                        break;
                    }
                    let alo = self.lo();
                    let pattern = self.pattern()?;
                    let guard = if self.eat(&TokenKind::If) {
                        let saved = std::mem::replace(&mut self.in_guard, true);
                        let g = self.expr();
                        self.in_guard = saved;
                        Some(g?)
                    } else {
                        None
                    };
                    self.expect(&TokenKind::FatArrow, "`=>`")?;
                    let body = self.expr()?;
                    arms.push(MatchArm { pattern, guard, body, span: self.span_from(alo) });
                    if !(self.at(&TokenKind::Semi) || self.at(&TokenKind::Comma) || self.at(&TokenKind::RBrace)) {
                        self.expected("`,`, newline or `}`");
                        return Err(());
                    }
                }
                self.expect(&TokenKind::RBrace, "`}`")?;
                Ok(self.mk(ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, self.span_from(lo)))
            }
            TokenKind::At => {
                let label = self.label()?;
                if !matches!(self.tok(), TokenKind::Loop | TokenKind::While | TokenKind::For) {
                    self.expected("`loop`, `while` or `for` after a label");
                    return Err(());
                }
                self.loop_expr(lo, Some(label))
            }
            TokenKind::Loop | TokenKind::While | TokenKind::For => self.loop_expr(lo, None),
            TokenKind::Return => {
                self.bump();
                let v = if self.expr_follows() { Some(Box::new(self.expr()?)) } else { None };
                Ok(self.mk(ExprKind::Return(v), self.span_from(lo)))
            }
            TokenKind::Break => {
                self.bump();
                let label = if self.at(&TokenKind::At) { Some(self.label()?) } else { None };
                let value = if self.expr_follows() { Some(Box::new(self.expr()?)) } else { None };
                Ok(self.mk(ExprKind::Break { label, value }, self.span_from(lo)))
            }
            TokenKind::Continue => {
                self.bump();
                let label = if self.at(&TokenKind::At) { Some(self.label()?) } else { None };
                Ok(self.mk(ExprKind::Continue(label), self.span_from(lo)))
            }
            TokenKind::Spawn | TokenKind::Not | TokenKind::Minus | TokenKind::Tilde | TokenKind::DotDot | TokenKind::DotDotEq => {
                self.prefix(0)
            }
            _ => {
                self.expected("expression");
                Err(())
            }
        }
    }

    /// `@name` (the label's span covers the name only).
    fn label(&mut self) -> PResult<Ident> {
        self.expect(&TokenKind::At, "`@`")?;
        if !self.at(&TokenKind::Ident) || self.toks[self.pos].lo != self.prev_hi() {
            self.expected("a label name directly after `@`");
            return Err(());
        }
        self.ident("label")
    }

    /// `loop`, `while` or `for`, with an optional label already parsed.
    fn loop_expr(&mut self, lo: u32, label: Option<Ident>) -> PResult<Expr> {
        let tok = self.tok().clone();
        self.bump();
        let kind = match tok {
            TokenKind::Loop => ExprKind::Loop { label, body: self.block()? },
            TokenKind::While => {
                let cond = self.cond()?;
                ExprKind::While { label, cond, body: self.block()? }
            }
            _ => {
                let pattern = Box::new(self.pattern()?);
                self.expect(&TokenKind::In, "`in`")?;
                let iter = self.expr()?;
                ExprKind::For { label, pattern, iter: Box::new(iter), body: self.block()? }
            }
        };
        Ok(self.mk(kind, self.span_from(lo)))
    }

    fn expr_follows(&self) -> bool {
        !matches!(
            self.tok(),
            TokenKind::Semi
                | TokenKind::RBrace
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::Comma
                | TokenKind::Eof
                | TokenKind::Else
        )
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        self.bump();
        let cond = self.cond()?;
        let then = self.block()?;
        let else_ = if self.eat(&TokenKind::Else) {
            if self.at(&TokenKind::If) {
                self.enter()?;
                let e = self.if_expr();
                self.leave();
                Some(Box::new(e?))
            } else {
                let blo = self.lo();
                let b = self.block()?;
                Some(Box::new(self.mk(ExprKind::Block(b), self.span_from(blo))))
            }
        } else {
            None
        };
        Ok(self.mk(ExprKind::If { cond, then, else_ }, self.span_from(lo)))
    }

    fn comp_clauses(&mut self) -> PResult<Vec<CompClause>> {
        let mut clauses = Vec::new();
        loop {
            if self.eat(&TokenKind::For) {
                let p = self.pattern()?;
                self.expect(&TokenKind::In, "`in`")?;
                let e = self.expr()?;
                clauses.push(CompClause::For(p, e));
            } else if self.eat(&TokenKind::If) {
                clauses.push(CompClause::If(self.expr()?));
            } else {
                break;
            }
        }
        Ok(clauses)
    }

    /// `name!(data)`: a call of a data macro.
    fn macro_call(&mut self, lo: u32) -> PResult<Expr> {
        let name = self.ident("macro name")?;
        self.bump();
        let Some(mac) = MACROS.iter().find(|m| m.name == name.name) else {
            let names: Vec<String> = MACROS.iter().map(|m| format!("`{}!`", m.name)).collect();
            let known =
                if names.len() == 1 { format!("the only macro is {}", names[0]) } else { format!("the macros are {}", names.join(", ")) };
            self.errors.push(Diag::error(name.span, format!("unknown macro `{}!`; {known}", name.name)));
            return Err(());
        };
        if !self.at(&TokenKind::LParen) {
            self.error_here(format!("a macro call is written `{}!(...)`", name.name));
            return Err(());
        }
        self.bump();
        let arg = self.data()?;
        self.expect(&TokenKind::RParen, "`)`")?;
        let expansion = expand_data(mac, &arg);
        Ok(self.mk(ExprKind::Macro(Box::new(MacroCall { name, arg, expansion })), self.span_from(lo)))
    }

    /// A data macro's argument: an object, a list, `null`, or a Sand expression.
    fn data(&mut self) -> PResult<Data> {
        self.enter()?;
        let r = self.data_inner();
        self.leave();
        r
    }

    fn data_inner(&mut self) -> PResult<Data> {
        let lo = self.lo();
        if self.at(&TokenKind::Ident) && self.cur_text() == "null" {
            self.bump();
            return Ok(Data::Null(self.span_from(lo)));
        }
        if self.eat(&TokenKind::LBrace) {
            let mut entries = Vec::new();
            loop {
                self.skip_newlines();
                if self.eat(&TokenKind::RBrace) {
                    break;
                }
                let k = self.expr()?;
                self.expect(&TokenKind::Colon, "`:`")?;
                entries.push((k, self.data()?));
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) {
                    self.skip_newlines();
                    self.expect(&TokenKind::RBrace, "`,` or `}`")?;
                    break;
                }
            }
            return Ok(Data::Object(entries, self.span_from(lo)));
        }
        if !self.eat(&TokenKind::LBracket) {
            return Ok(Data::Value(self.expr()?));
        }
        if self.at(&TokenKind::Colon) && matches!(self.peek_tok(1), TokenKind::RBracket) {
            self.bump();
            self.bump();
            return Ok(Data::Object(Vec::new(), self.span_from(lo)));
        }
        if self.eat(&TokenKind::RBracket) {
            return Ok(Data::List(Vec::new(), self.span_from(lo)));
        }
        let first = self.data()?;
        if !self.eat(&TokenKind::Colon) {
            let mut items = vec![first];
            while self.eat(&TokenKind::Comma) && !self.at(&TokenKind::RBracket) {
                items.push(self.data()?);
            }
            self.expect(&TokenKind::RBracket, "`,` or `]`")?;
            return Ok(Data::List(items, self.span_from(lo)));
        }
        let Data::Value(k) = first else {
            self.errors.push(Diag::error(first.span(), "an object key must be a string"));
            return Err(());
        };
        let mut entries = vec![(k, self.data()?)];
        while self.eat(&TokenKind::Comma) && !self.at(&TokenKind::RBracket) {
            let k = self.expr()?;
            self.expect(&TokenKind::Colon, "`:`")?;
            entries.push((k, self.data()?));
        }
        self.expect(&TokenKind::RBracket, "`,` or `]`")?;
        Ok(Data::Object(entries, self.span_from(lo)))
    }

    /// Skips the statement ends that newlines insert.
    fn skip_newlines(&mut self) {
        while self.at(&TokenKind::Semi) && self.toks[self.pos].lo == self.toks[self.pos].hi {
            self.bump();
        }
    }

    fn bracket(&mut self, lo: u32) -> PResult<Expr> {
        self.bump();
        self.bracket_inner(lo)
    }

    fn bracket_inner(&mut self, lo: u32) -> PResult<Expr> {
        if self.eat(&TokenKind::RBracket) {
            return Ok(self.mk(ExprKind::List(vec![]), self.span_from(lo)));
        }
        if self.at(&TokenKind::Colon) && matches!(self.peek_tok(1), TokenKind::RBracket) {
            self.bump();
            self.bump();
            return Ok(self.mk(ExprKind::Map(vec![]), self.span_from(lo)));
        }
        let first = self.expr()?;
        if self.eat(&TokenKind::Colon) {
            let v = self.expr()?;
            if self.at(&TokenKind::For) {
                let clauses = self.comp_clauses()?;
                self.expect(&TokenKind::RBracket, "`]`")?;
                return Ok(self.mk(ExprKind::MapComp(Box::new(first), Box::new(v), clauses), self.span_from(lo)));
            }
            let mut pairs = vec![(first, v)];
            while self.eat(&TokenKind::Comma) {
                if self.at(&TokenKind::RBracket) {
                    break;
                }
                let k = self.expr()?;
                self.expect(&TokenKind::Colon, "`:`")?;
                let v = self.expr()?;
                pairs.push((k, v));
            }
            self.expect(&TokenKind::RBracket, "`]`")?;
            return Ok(self.mk(ExprKind::Map(pairs), self.span_from(lo)));
        }
        if self.eat(&TokenKind::Semi) {
            let n = self.expr()?;
            self.expect(&TokenKind::RBracket, "`]`")?;
            return Ok(self.mk(ExprKind::Repeat(Box::new(first), Box::new(n)), self.span_from(lo)));
        }
        if self.at(&TokenKind::For) {
            let clauses = self.comp_clauses()?;
            self.expect(&TokenKind::RBracket, "`]`")?;
            return Ok(self.mk(ExprKind::ListComp(Box::new(first), clauses), self.span_from(lo)));
        }
        let mut items = vec![first];
        while self.eat(&TokenKind::Comma) {
            if self.at(&TokenKind::RBracket) {
                break;
            }
            items.push(self.expr()?);
        }
        self.expect(&TokenKind::RBracket, "`]`")?;
        Ok(self.mk(ExprKind::List(items), self.span_from(lo)))
    }

    /// A string with interpolations, from its `StrStart` token.
    fn interpolated_string(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        self.bump();
        let mut segs = Vec::new();
        loop {
            match self.tok() {
                TokenKind::StrText(_) => segs.push(StrSeg::Text(self.take_text())),
                TokenKind::InterpStart => {
                    let open = self.bump();
                    if let TokenKind::InterpEnd { .. } = self.tok() {
                        let span = Span::new(self.file, open.lo, self.toks[self.pos].hi);
                        self.errors.push(Diag::error(span, "empty interpolation: `\\(` must hold an expression"));
                        return Err(());
                    }
                    // An interpolation is parsed on its own, like a parenthesized expression.
                    let saved = std::mem::replace(&mut self.in_guard, false);
                    let e = self.expr();
                    self.in_guard = saved;
                    let e = e?;
                    let TokenKind::InterpEnd { has_format_spec } = *self.tok() else {
                        self.expected("`)` to end the interpolation");
                        return Err(());
                    };
                    let end = self.bump();
                    // The token spans `:spec)`.
                    let spec = has_format_spec.then(|| {
                        let span = Span::new(self.file, end.lo + 1, end.hi - 1);
                        FormatSpecAst { text: self.slice(span).to_string(), span }
                    });
                    segs.push(StrSeg::Expr(Box::new(e), spec));
                }
                TokenKind::StrEnd => {
                    self.bump();
                    return Ok(self.mk(ExprKind::Str(segs), self.span_from(lo)));
                }
                _ => {
                    self.expected("end of string");
                    return Err(());
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Patterns

    fn pattern(&mut self) -> PResult<Pattern> {
        self.enter()?;
        let r = self.pattern_or();
        self.leave();
        r
    }

    fn pattern_or(&mut self) -> PResult<Pattern> {
        let lo = self.lo();
        let first = self.pattern_atom()?;
        if !self.at(&TokenKind::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat(&TokenKind::Pipe) {
            alts.push(self.pattern_atom()?);
        }
        Ok(Pattern { kind: PatternKind::Or(alts), span: self.span_from(lo) })
    }

    fn int_lit_pattern(&mut self) -> PResult<i64> {
        let neg = self.eat(&TokenKind::Minus);
        match *self.tok() {
            TokenKind::Int(v) => {
                self.bump();
                Ok(if neg { -v } else { v })
            }
            TokenKind::IntOverflow(true) if neg => {
                self.bump();
                Ok(i64::MIN)
            }
            _ => {
                self.expected("integer");
                Err(())
            }
        }
    }

    fn pattern_args(&mut self) -> PResult<PatArgs> {
        let mut args = PatArgs::default();
        while !self.at(&TokenKind::RParen) {
            if args.rest {
                self.error_here("`..` must be last in a pattern");
            }
            if self.eat(&TokenKind::DotDot) {
                args.rest = true;
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }
            if self.at(&TokenKind::Ident) && matches!(self.peek_tok(1), TokenKind::Colon) {
                let name = self.ident("field name")?;
                self.bump();
                args.named.push((name, self.pattern()?));
            } else {
                if !args.named.is_empty() {
                    self.error_here("positional pattern after named pattern");
                }
                args.positional.push(self.pattern()?);
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`")?;
        Ok(args)
    }

    fn pattern_atom(&mut self) -> PResult<Pattern> {
        let lo = self.lo();
        let kind = match self.tok() {
            TokenKind::Int(_) | TokenKind::Minus | TokenKind::IntOverflow(_) => {
                let a = self.int_lit_pattern()?;
                if self.eat(&TokenKind::DotDotEq) {
                    let b = self.int_lit_pattern()?;
                    PatternKind::Range(a, b)
                } else if self.at(&TokenKind::DotDot) {
                    self.error_here("exclusive range patterns are not supported; use `..=`");
                    return Err(());
                } else {
                    PatternKind::Literal(PatLit::Int(a))
                }
            }
            TokenKind::Str(_) | TokenKind::StrStart => PatternKind::Literal(PatLit::Str(self.plain_string("a string pattern")?)),
            TokenKind::True | TokenKind::False => {
                let b = self.at(&TokenKind::True);
                self.bump();
                PatternKind::Literal(PatLit::Bool(b))
            }
            TokenKind::LParen => {
                self.bump();
                if self.eat(&TokenKind::RParen) {
                    PatternKind::Literal(PatLit::Unit)
                } else {
                    let first = self.pattern()?;
                    if self.eat(&TokenKind::RParen) {
                        return Ok(first);
                    }
                    PatternKind::Tuple(self.tuple_rest(first, lo, Self::pattern)?)
                }
            }
            TokenKind::LBracket => {
                self.bump();
                let mut before = Vec::new();
                let mut after = Vec::new();
                let mut rest = None;
                while !self.at(&TokenKind::RBracket) {
                    if self.at(&TokenKind::DotDot) {
                        if rest.is_some() {
                            self.error_here("only one `..` is allowed in a list pattern");
                            return Err(());
                        }
                        self.bump();
                        let name = if self.at(&TokenKind::Ident) { Some(self.ident("name")?) } else { None };
                        rest = Some(name);
                    } else if rest.is_some() {
                        after.push(self.pattern()?);
                    } else {
                        before.push(self.pattern()?);
                    }
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RBracket, "`]`")?;
                PatternKind::List { before, rest, after }
            }
            TokenKind::Ident | TokenKind::SelfUpper => {
                let mut path = Vec::new();
                if self.at(&TokenKind::SelfUpper) {
                    let span = self.bump();
                    path.push(Ident { name: "Self".into(), span });
                } else {
                    path.push(self.ident("pattern")?);
                }
                while self.at(&TokenKind::Dot) && matches!(self.peek_tok(1), TokenKind::Ident) {
                    self.bump();
                    path.push(self.ident("name")?);
                }
                if self.eat(&TokenKind::LParen) {
                    PatternKind::Ctor { path, args: Some(self.pattern_args()?) }
                } else if self.at(&TokenKind::LBrace) {
                    self.error_here("a struct pattern is written `Name(field, ..)` or `Name(field: pattern)`");
                    return Err(());
                } else if path.len() == 1 {
                    if path[0].name == "_" { PatternKind::Wildcard } else { PatternKind::Binding(path.pop().unwrap()) }
                } else {
                    PatternKind::Ctor { path, args: None }
                }
            }
            _ => {
                self.expected("pattern");
                return Err(());
            }
        };
        Ok(Pattern { kind, span: self.span_from(lo) })
    }
}

/// The expansion of a data macro's argument into calls of the macro's module.
fn expand_data(mac: &DataMacro, d: &Data) -> Expr {
    let span = d.span();
    let path = |names: &[&str]| {
        names.iter().fold(Expr { kind: ExprKind::ModuleRef(mac.module.to_string()), span }, |e, n| Expr {
            kind: ExprKind::Field(Box::new(e), Ident { name: n.to_string(), span }),
            span,
        })
    };
    let call = |callee: Expr, value: Expr| Expr {
        kind: ExprKind::Call { callee: Box::new(callee), args: vec![Arg { name: None, value, spread: false }] },
        span,
    };
    match d {
        Data::Null(_) => path(mac.null),
        Data::Value(e) => call(path(mac.value), e.clone()),
        Data::List(xs, _) => call(path(mac.list), Expr { kind: ExprKind::List(xs.iter().map(|x| expand_data(mac, x)).collect()), span }),
        Data::Object(kvs, _) => {
            let pairs = kvs.iter().map(|(k, v)| (k.clone(), expand_data(mac, v))).collect();
            call(path(mac.object), Expr { kind: ExprKind::Map(pairs), span })
        }
    }
}
