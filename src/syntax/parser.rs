//! Recursive-descent parser with precedence climbing for expressions.

use super::ast::*;
use super::lexer::{Comment, CommentKind, Lexed, Tok, Token, lex};
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
}

pub fn parse_module(src: &str, file: FileId) -> ParseResult {
    let Lexed { tokens, comments, errors, .. } = lex(src, file);
    let mut p = Parser::new(src, file, tokens, &comments);
    p.errors = errors;
    let module = p.module();
    let (tokens, mut errors) = (p.toks, p.errors);
    errors.sort_by_key(|d| d.span.lo);
    errors.truncate(MAX_ERRORS);
    ParseResult { module, comments, tokens, errors }
}

struct Parser<'a> {
    src: &'a str,
    file: FileId,
    toks: Vec<Token>,
    pos: usize,
    comments: &'a [Comment],
    errors: Vec<Diag>,
    no_struct: bool,
    /// In a match guard: `x => ...` ends the guard rather than starting a lambda.
    in_guard: bool,
    depth: usize,
    fatal: bool,
    /// Inside a `trait` body: methods may omit their body.
    in_trait: bool,
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
            no_struct: false,
            in_guard: false,
            depth: 0,
            fatal: false,
            in_trait: false,
        }
    }

    // ------------------------------------------------------------------
    // Token helpers

    fn tok(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_tok(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }
    fn at(&self, t: &Tok) -> bool {
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
        let text = match &mut self.toks[self.pos].tok {
            Tok::Str(s) | Tok::StrText(s) => std::mem::take(s),
            _ => String::new(),
        };
        self.bump();
        text
    }
    fn eat(&mut self, t: &Tok) -> bool {
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
    fn expect(&mut self, t: &Tok, what: &str) -> PResult<()> {
        if self.eat(t) {
            Ok(())
        } else {
            self.expected(what);
            Err(())
        }
    }
    fn skip_semis(&mut self) {
        while self.at(&Tok::Semi) {
            self.bump();
        }
    }
    fn mk(&self, kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }
    fn ident(&mut self, what: &str) -> PResult<Ident> {
        if self.at(&Tok::Ident) {
            let span = self.bump();
            Ok(Ident { name: self.slice(span).to_string(), span })
        } else {
            self.expected(what);
            Err(())
        }
    }
    /// An identifier, also accepting keywords (for function and member names).
    fn name_or_keyword(&mut self, what: &str) -> PResult<Ident> {
        if self.at(&Tok::Ident) || (self.tok().is_keyword() && !self.at(&Tok::SelfLower)) {
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
    /// Doc comment (`///`) immediately preceding byte offset `lo`.
    fn doc_before(&self, lo: u32) -> Option<String> {
        let idx = self.comments.partition_point(|c| c.hi <= lo);
        let mut lines = Vec::new();
        let mut boundary = lo;
        let mut i = idx;
        while i > 0 {
            let c = &self.comments[i - 1];
            if c.kind != CommentKind::Doc {
                break;
            }
            let between = &self.src[c.hi as usize..boundary as usize];
            if !between.trim().is_empty() || between.matches('\n').count() > 1 {
                break;
            }
            let t = c.text(self.src).trim_start_matches("///");
            lines.push(t.strip_prefix(' ').unwrap_or(t).to_string());
            boundary = c.lo;
            i -= 1;
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        Some(lines.join("\n"))
    }

    /// Skips to the next statement boundary after an error.
    fn recover(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.tok() {
                Tok::Eof => return,
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket => depth -= 1,
                Tok::RBrace => {
                    if depth <= 0 {
                        return;
                    }
                    depth -= 1;
                }
                Tok::Semi if depth <= 0 => {
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
        let doc: Vec<String> = self
            .comments
            .iter()
            .take_while(|c| c.kind == CommentKind::ModuleDoc || c.kind == CommentKind::Line)
            .filter(|c| c.kind == CommentKind::ModuleDoc)
            .map(|c| {
                let t = c.text(self.src).trim_start_matches("//!");
                t.strip_prefix(' ').unwrap_or(t).to_string()
            })
            .collect();
        loop {
            self.skip_semis();
            if self.at(&Tok::Eof) || self.fatal {
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
                    if self.at(&Tok::RBrace) {
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
        if self.eat(&Tok::Semi) || self.at(&Tok::Eof) || self.at(&Tok::RBrace) || self.toks[self.pos].newline_before {
            Ok(())
        } else {
            self.expected("newline or `;`");
            Err(())
        }
    }

    fn item(&mut self) -> PResult<Item> {
        let lo = self.lo();
        let doc = self.doc_before(lo);
        let kind = match self.tok() {
            Tok::Import => ItemKind::Import(self.import()?),
            Tok::Ident if self.is_at_selective_import() => ItemKind::Import(self.selective_import()?),
            Tok::Pub => {
                self.bump();
                match self.tok() {
                    Tok::Native if matches!(self.peek_tok(1), Tok::Type) => return self.native_type(),
                    Tok::Fn | Tok::Native => ItemKind::Fn(self.fn_decl(true, doc)?),
                    Tok::Struct => ItemKind::Struct(self.struct_decl(true, doc)?),
                    Tok::Enum => ItemKind::Enum(self.enum_decl(true, doc)?),
                    Tok::Type => ItemKind::TypeAlias(self.type_alias(true, doc)?),
                    Tok::Trait => ItemKind::Trait(self.trait_decl(true, doc)?),
                    Tok::Let | Tok::Var => {
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
            Tok::Fn if matches!(self.peek_tok(1), Tok::Ident) => ItemKind::Fn(self.fn_decl(false, doc)?),
            Tok::Native if matches!(self.peek_tok(1), Tok::Type) => return self.native_type(),
            Tok::Native => ItemKind::Fn(self.fn_decl(false, doc)?),
            Tok::Struct => ItemKind::Struct(self.struct_decl(false, doc)?),
            Tok::Enum => ItemKind::Enum(self.enum_decl(false, doc)?),
            Tok::Extend => ItemKind::Extend(self.extend_decl(doc)?),
            Tok::Trait => ItemKind::Trait(self.trait_decl(false, doc)?),
            Tok::Impl => ItemKind::Impl(self.impl_decl()?),
            Tok::Type if matches!(self.peek_tok(1), Tok::Ident) => ItemKind::TypeAlias(self.type_alias(false, doc)?),
            Tok::Ident if self.cur_text() == "test" && matches!(self.peek_tok(1), Tok::Str(_) | Tok::StrStart) => {
                self.bump();
                let name_span = self.span_here();
                let name = self.plain_string("a test name")?;
                let body = self.block()?;
                ItemKind::Test(TestDecl { name, name_span, body })
            }
            Tok::Ident if self.cur_text() == "test" && matches!(self.peek_tok(1), Tok::LBrace) => {
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
        Ok(Item { kind, span: self.span_from(lo) })
    }

    /// A string literal without interpolations.
    fn plain_string(&mut self, what: &str) -> PResult<String> {
        match self.tok() {
            Tok::Str(_) => Ok(self.take_text()),
            Tok::StrStart => {
                // The whole string, up to its (matching) end.
                let lo = self.lo();
                let mut depth = 0;
                let mut i = self.pos;
                while let Some(t) = self.toks.get(i) {
                    match t.tok {
                        Tok::StrStart => depth += 1,
                        Tok::StrEnd if depth == 1 => break,
                        Tok::StrEnd => depth -= 1,
                        Tok::Eof => break,
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
            Tok::Ident => Ok(ImportSource::Name(self.ident("module name")?)),
            Tok::Str(_) | Tok::StrStart => {
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
        let alias = if self.eat(&Tok::As) { Some(self.ident("alias")?) } else { None };
        self.end_of_item()?;
        Ok(Import { source, alias, names: None })
    }

    fn is_at_selective_import(&self) -> bool {
        self.cur_text() == "from" && matches!(self.peek_tok(1), Tok::Ident | Tok::Str(_)) && matches!(self.peek_tok(2), Tok::Import)
    }

    fn selective_import(&mut self) -> PResult<Import> {
        self.bump();
        let source = self.import_source()?;
        self.expect(&Tok::Import, "`import`")?;
        let mut names = vec![self.ident("name to import")?];
        while self.eat(&Tok::Comma) {
            names.push(self.ident("name to import")?);
        }
        self.end_of_item()?;
        Ok(Import { source, alias: None, names: Some(names) })
    }

    /// Type parameters, which may be `reflect` if `function`.
    fn generics(&mut self, function: bool) -> PResult<Vec<GenericParam>> {
        let mut out = Vec::new();
        if !self.eat(&Tok::Lt) {
            return Ok(out);
        }
        loop {
            let mut reflect = false;
            if self.at(&Tok::Ident) && self.cur_text() == "reflect" && matches!(self.peek_tok(1), Tok::Ident) {
                let span = self.bump();
                if !function {
                    self.errors.push(Diag::error(span, "only the type parameters of a function can be `reflect`"));
                }
                reflect = true;
            }
            let name = self.ident("type parameter")?;
            let bounds = if self.eat(&Tok::Colon) { self.bounds()? } else { Vec::new() };
            out.push(GenericParam { name, bounds, reflect });
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect_gt()?;
            break;
        }
        Ok(out)
    }

    /// `B1 + B2 + ...` where each bound is `Name`, `module.Name` or `Name<Args>`.
    fn bounds(&mut self) -> PResult<Vec<TypeExpr>> {
        let mut out = vec![self.bound()?];
        while self.eat(&Tok::Plus) {
            out.push(self.bound()?);
        }
        Ok(out)
    }

    fn bound(&mut self) -> PResult<TypeExpr> {
        let lo = self.lo();
        let mut path = vec![self.ident("bound")?];
        while self.at(&Tok::Dot) && matches!(self.peek_tok(1), Tok::Ident) {
            self.bump();
            path.push(self.ident("bound")?);
        }
        let mut args = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                args.push(self.ty()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect_gt()?;
        }
        Ok(TypeExpr { kind: TypeExprKind::Named { path, args }, span: self.span_from(lo) })
    }

    /// An optional `where T: B, U: C` clause.
    fn where_clause(&mut self) -> PResult<Vec<WherePredicate>> {
        let mut out = Vec::new();
        if self.at(&Tok::Ident) && self.cur_text() == "where" {
            self.bump();
            loop {
                let name = self.ident("type parameter")?;
                self.expect(&Tok::Colon, "`:`")?;
                let bounds = self.bounds()?;
                out.push(WherePredicate { name, bounds });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Expects `>`, splitting `>>` and `>=` tokens when needed.
    fn expect_gt(&mut self) -> PResult<()> {
        match self.tok() {
            Tok::Gt => {
                self.bump();
                Ok(())
            }
            Tok::Shr => {
                self.toks[self.pos].tok = Tok::Gt;
                self.toks[self.pos].lo += 1;
                Ok(())
            }
            Tok::Ge => {
                self.toks[self.pos].tok = Tok::Eq;
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
        let is_native = self.eat(&Tok::Native);
        self.expect(&Tok::Fn, "`fn`")?;
        let name = self.name_or_keyword("function name")?;
        let generics = self.generics(true)?;
        self.expect(&Tok::LParen, "`(`")?;
        let mut params = Vec::new();
        let mut seen_default = false;
        while !self.at(&Tok::RParen) {
            let plo = self.lo();
            if self.at(&Tok::SelfLower) {
                let span = self.bump();
                params.push(Param { name: Ident { name: "self".into(), span }, ty: None, default: None, span: self.span_from(plo) });
            } else {
                let pname = self.ident("parameter name")?;
                self.expect(&Tok::Colon, "`:`")?;
                let ty = self.ty()?;
                let default = if self.eat(&Tok::Eq) {
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
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.ty()?) } else { None };
        let where_clause = self.where_clause()?;
        let body = if is_native || (bodyless_ok && !self.at(&Tok::LBrace)) {
            None
        } else if self.at(&Tok::LBrace) {
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
        let generics = self.generics(false)?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        loop {
            while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let mlo = self.lo();
            let mdoc = self.doc_before(mlo);
            if self.at(&Tok::Fn) || self.at(&Tok::Native) || (self.at(&Tok::Pub) && matches!(self.peek_tok(1), Tok::Fn | Tok::Native)) {
                let p = self.eat(&Tok::Pub);
                methods.push(self.fn_decl(p, mdoc)?);
                continue;
            }
            let is_pub = self.eat(&Tok::Pub);
            let mutable = self.eat(&Tok::Var);
            let fname = self.ident("field name")?;
            self.expect(&Tok::Colon, "`:`")?;
            let ty = self.ty()?;
            let default = if self.eat(&Tok::Eq) { Some(self.expr()?) } else { None };
            fields.push(FieldDecl { name: fname, ty, default, mutable, is_pub, doc: mdoc, span: self.span_from(mlo) });
            // A type ending in `>` is not followed by an automatic terminator.
            if !(self.at(&Tok::Semi) || self.at(&Tok::Comma) || self.at(&Tok::RBrace) || self.toks[self.pos].newline_before) {
                self.expected("`,`, newline or `}`");
                return Err(());
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(StructDecl { name, generics, fields, methods, is_pub, doc })
    }

    fn enum_decl(&mut self, is_pub: bool, doc: Option<String>) -> PResult<EnumDecl> {
        self.bump();
        let name = self.ident("enum name")?;
        let generics = self.generics(false)?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut variants = Vec::new();
        let mut methods = Vec::new();
        loop {
            while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let vlo = self.lo();
            let vdoc = self.doc_before(vlo);
            if self.at(&Tok::Fn) || self.at(&Tok::Native) || (self.at(&Tok::Pub) && matches!(self.peek_tok(1), Tok::Fn | Tok::Native)) {
                let p = self.eat(&Tok::Pub);
                methods.push(self.fn_decl(p, vdoc)?);
                continue;
            }
            let vname = self.ident("variant name")?;
            let mut fields = Vec::new();
            if self.eat(&Tok::LParen) {
                while !self.at(&Tok::RParen) {
                    let fname = if self.at(&Tok::Ident) && matches!(self.peek_tok(1), Tok::Colon) {
                        let n = self.ident("field name")?;
                        self.bump();
                        Some(n)
                    } else {
                        None
                    };
                    let ty = self.ty()?;
                    fields.push(VariantField { name: fname, ty });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
            }
            variants.push(VariantDecl { name: vname, fields, doc: vdoc, span: self.span_from(vlo) });
            if !(self.at(&Tok::Semi) || self.at(&Tok::Comma) || self.at(&Tok::RBrace) || self.toks[self.pos].newline_before) {
                self.expected("`,`, newline or `}`");
                return Err(());
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(EnumDecl { name, generics, variants, methods, is_pub, doc })
    }

    fn extend_decl(&mut self, doc: Option<String>) -> PResult<ExtendDecl> {
        self.bump();
        let generics = self.generics(false)?;
        let target = self.ty()?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut methods = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let mdoc = self.doc_before(self.lo());
            let p = self.eat(&Tok::Pub);
            if !(self.at(&Tok::Fn) || self.at(&Tok::Native)) {
                self.expected("`fn`");
                return Err(());
            }
            methods.push(self.fn_decl(p, mdoc)?);
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(ExtendDecl { generics, target, methods, doc })
    }

    /// Methods of a `trait` or `impl` body: doc comments and `fn` declarations.
    fn trait_body(&mut self, in_trait: bool) -> PResult<Vec<FnDecl>> {
        self.expect(&Tok::LBrace, "`{`")?;
        let mut methods = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let mdoc = self.doc_before(self.lo());
            if self.at(&Tok::Pub) {
                let what = if in_trait { "trait" } else { "impl" };
                self.error_here(format!("`pub` is not allowed on {what} methods; they are public if the trait is"));
                self.bump();
            }
            // Natives may implement the methods of an impl (not declare a trait's).
            if !(self.at(&Tok::Fn) || (!in_trait && self.at(&Tok::Native))) {
                self.expected("`fn`");
                return Err(());
            }
            self.in_trait = in_trait;
            methods.push(self.fn_decl(false, mdoc)?);
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(methods)
    }

    fn trait_decl(&mut self, is_pub: bool, doc: Option<String>) -> PResult<TraitDecl> {
        self.bump();
        let name = self.ident("trait name")?;
        let generics = self.generics(false)?;
        let supers = if self.eat(&Tok::Colon) { self.bounds()? } else { Vec::new() };
        let methods = self.trait_body(true)?;
        Ok(TraitDecl { name, generics, supers, methods, is_pub, doc })
    }

    fn impl_decl(&mut self) -> PResult<ImplDecl> {
        self.bump();
        let generics = self.generics(false)?;
        let trait_ref = self.bound()?;
        self.expect(&Tok::For, "`for`")?;
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
        let generics = self.generics(false)?;
        self.expect(&Tok::Eq, "`=`")?;
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
            Tok::LBracket => {
                self.bump();
                let a = self.ty()?;
                if self.eat(&Tok::Colon) {
                    let b = self.ty()?;
                    self.expect(&Tok::RBracket, "`]`")?;
                    TypeExpr { kind: TypeExprKind::Map(Box::new(a), Box::new(b)), span: self.span_from(lo) }
                } else {
                    self.expect(&Tok::RBracket, "`]`")?;
                    TypeExpr { kind: TypeExprKind::List(Box::new(a)), span: self.span_from(lo) }
                }
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    TypeExpr { kind: TypeExprKind::Unit, span: self.span_from(lo) }
                } else {
                    let first = self.ty()?;
                    if self.eat(&Tok::RParen) {
                        first
                    } else {
                        let items = self.tuple_rest(first, lo, Self::ty)?;
                        TypeExpr { kind: TypeExprKind::Tuple(items), span: self.span_from(lo) }
                    }
                }
            }
            Tok::Fn => {
                self.bump();
                self.expect(&Tok::LParen, "`(`")?;
                let mut params = Vec::new();
                while !self.at(&Tok::RParen) {
                    params.push(self.ty()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
                let ret = if self.eat(&Tok::Arrow) { Some(Box::new(self.ty()?)) } else { None };
                TypeExpr { kind: TypeExprKind::Fn(params, ret), span: self.span_from(lo) }
            }
            Tok::Ident | Tok::SelfUpper => {
                let mut path = Vec::new();
                if self.at(&Tok::SelfUpper) {
                    let span = self.bump();
                    path.push(Ident { name: "Self".into(), span });
                } else {
                    path.push(self.ident("type")?);
                    while self.at(&Tok::Dot) && matches!(self.peek_tok(1), Tok::Ident) {
                        self.bump();
                        path.push(self.ident("type")?);
                    }
                }
                let mut args = Vec::new();
                if self.eat(&Tok::Lt) {
                    loop {
                        args.push(self.ty()?);
                        if !self.eat(&Tok::Comma) {
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
        while self.at(&Tok::Question) || self.at(&Tok::QQ) {
            let double = self.at(&Tok::QQ);
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
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::RParen) {
                break;
            }
            items.push(elem(self)?);
        }
        self.expect(&Tok::RParen, "`)` or `,`")?;
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
        self.expect(&Tok::LBrace, "`{`")?;
        let saved = std::mem::replace(&mut self.no_struct, false);
        let mut stmts = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) || self.fatal {
                break;
            }
            let start = self.pos;
            match self.stmt() {
                Ok(s) => {
                    stmts.push(s);
                    if !(self.eat(&Tok::Semi) || self.at(&Tok::RBrace)) {
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
        self.no_struct = saved;
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(Block { stmts, span: self.span_from(lo) })
    }

    fn let_stmt(&mut self) -> PResult<Stmt> {
        let lo = self.lo();
        let mut mutable = self.at(&Tok::Var);
        self.bump();
        if !mutable && self.cur_text() == "mut" && matches!(self.peek_tok(1), Tok::Ident | Tok::LParen) {
            self.error_here("Sand has no `let mut`; use `var` for a binding that can change");
            self.bump();
            mutable = true;
        }
        let pattern = self.pattern()?;
        let ty = if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None };
        self.expect(&Tok::Eq, "`=`")?;
        let value = self.expr()?;
        let else_block = if self.at(&Tok::Else) {
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
        if !self.at(&Tok::Ident) {
            self.expected("a name or `_` after `use` (a `use` binding cannot be a pattern)");
            return Err(());
        }
        let name = self.ident("name")?;
        let ty = if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None };
        self.expect(&Tok::Eq, "`=`")?;
        let value = self.expr()?;
        Ok(Stmt { kind: StmtKind::Use { name, ty, value }, span: self.span_from(lo) })
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let lo = self.lo();
        match self.tok() {
            Tok::Import => {
                let msg = if matches!(self.peek_tok(1), Tok::Ident) && self.peek_text(1) == "test" {
                    "`import` is only allowed at the top level; `test` is already in scope in test blocks"
                } else {
                    "`import` is only allowed at the top level"
                };
                self.errors.push(Diag::error(self.span_here(), msg));
                Err(())
            }
            Tok::Ident if self.is_at_selective_import() => {
                self.errors.push(Diag::error(self.span_here(), "`import` is only allowed at the top level"));
                Err(())
            }
            Tok::Let | Tok::Var => self.let_stmt(),
            Tok::Defer => {
                self.bump();
                let e = self.expr()?;
                Ok(Stmt { kind: StmtKind::Defer(e), span: self.span_from(lo) })
            }
            Tok::Use => self.use_stmt(),
            Tok::Fn if matches!(self.peek_tok(1), Tok::Ident) => {
                let doc = self.doc_before(lo);
                let f = self.fn_decl(false, doc)?;
                Ok(Stmt { kind: StmtKind::Fn(f), span: self.span_from(lo) })
            }
            _ => {
                let e = self.expr()?;
                let op = match self.tok() {
                    Tok::Eq => Some(AssignOp::Assign),
                    Tok::PlusEq => Some(AssignOp::Add),
                    Tok::MinusEq => Some(AssignOp::Sub),
                    Tok::StarEq => Some(AssignOp::Mul),
                    Tok::SlashEq => Some(AssignOp::Div),
                    Tok::PercentEq => Some(AssignOp::Rem),
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

    fn expr_no_struct(&mut self) -> PResult<Expr> {
        let saved = std::mem::replace(&mut self.no_struct, true);
        let r = self.expr();
        self.no_struct = saved;
        r
    }

    /// Binding powers: (left, right). Higher binds tighter.
    fn infix_bp(&self) -> Option<(u8, u8)> {
        Some(match self.tok() {
            Tok::QQ => (5, 4),
            Tok::Or => (6, 7),
            Tok::And => (8, 9),
            Tok::EqEq | Tok::NotEq | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge => (12, 13),
            Tok::Pipe => (14, 15),
            Tok::Caret => (16, 17),
            Tok::Amp => (18, 19),
            Tok::Shl | Tok::Shr => (20, 21),
            Tok::DotDot | Tok::DotDotEq => (22, 23),
            Tok::Plus | Tok::Minus => (24, 25),
            Tok::Star | Tok::Slash | Tok::Percent => (26, 27),
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
                Tok::QQ => {
                    let rhs = self.expr_bp(rbp)?;
                    ExprKind::Coalesce(Box::new(lhs), Box::new(rhs))
                }
                Tok::DotDot | Tok::DotDotEq => {
                    let inclusive = op_tok == Tok::DotDotEq;
                    let end = if self.range_end_absent() { None } else { Some(Box::new(self.expr_bp(rbp)?)) };
                    let e = ExprKind::Range { start: Some(Box::new(lhs)), end, inclusive };
                    let ex = self.mk(e, self.span_from(lo));
                    if matches!(self.tok(), Tok::DotDot | Tok::DotDotEq) {
                        self.error_here("range operators cannot be chained");
                        return Err(());
                    }
                    lhs = ex;
                    continue;
                }
                ref t => {
                    let op = match t {
                        Tok::Or => BinOp::Or,
                        Tok::And => BinOp::And,
                        Tok::EqEq => BinOp::Eq,
                        Tok::NotEq => BinOp::Ne,
                        Tok::Lt => BinOp::Lt,
                        Tok::Le => BinOp::Le,
                        Tok::Gt => BinOp::Gt,
                        Tok::Ge => BinOp::Ge,
                        Tok::Pipe => BinOp::BitOr,
                        Tok::Caret => BinOp::BitXor,
                        Tok::Amp => BinOp::BitAnd,
                        Tok::Shl => BinOp::Shl,
                        Tok::Shr => BinOp::Shr,
                        Tok::Plus => BinOp::Add,
                        Tok::Minus => BinOp::Sub,
                        Tok::Star => BinOp::Mul,
                        Tok::Slash => BinOp::Div,
                        Tok::Percent => BinOp::Rem,
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
                        if matches!(self.tok(), Tok::EqEq | Tok::NotEq | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge) {
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
        matches!(self.tok(), Tok::RBracket | Tok::RParen | Tok::Comma | Tok::Semi | Tok::Eof | Tok::RBrace | Tok::LBrace)
    }

    /// Prefix operators, `not`, `spawn` and lambdas; then postfix.
    fn prefix(&mut self, min_bp: u8) -> PResult<Expr> {
        let lo = self.lo();
        match self.tok() {
            Tok::Bang => {
                self.error_here("unexpected `!`; use `not` for negation");
                Err(())
            }
            Tok::Not => {
                self.bump();
                self.enter()?;
                // `not` binds looser than comparisons.
                let e = self.expr_bp(10.max(min_bp));
                self.leave();
                let e = e?;
                Ok(self.mk(ExprKind::Unary(UnOp::Not, Box::new(e)), self.span_from(lo)))
            }
            Tok::Minus | Tok::Tilde => {
                let op = if self.at(&Tok::Minus) { UnOp::Neg } else { UnOp::BitNot };
                self.bump();
                self.enter()?;
                let e = self.prefix(28);
                self.leave();
                let e = e?;
                // Fold `-<literal>` so that i64::MIN is expressible.
                if op == UnOp::Neg
                    && let ExprKind::IntOverflow(true) = e.kind
                    && !self.at(&Tok::StarStar)
                {
                    return Ok(self.mk(ExprKind::Int(i64::MIN), self.span_from(lo)));
                }
                let un = self.mk(ExprKind::Unary(op, Box::new(e)), self.span_from(lo));
                Ok(un)
            }
            Tok::DotDot | Tok::DotDotEq => {
                let inclusive = self.at(&Tok::DotDotEq);
                self.bump();
                let end = if self.range_end_absent() { None } else { Some(Box::new(self.expr_bp(23)?)) };
                Ok(self.mk(ExprKind::Range { start: None, end, inclusive }, self.span_from(lo)))
            }
            Tok::Spawn => {
                self.bump();
                let e = self.expr()?;
                Ok(self.mk(ExprKind::Spawn(Box::new(e)), self.span_from(lo)))
            }
            _ => {
                let base = self.postfix()?;
                if self.at(&Tok::StarStar) {
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
        let saved = std::mem::replace(&mut self.no_struct, false);
        let saved_guard = std::mem::replace(&mut self.in_guard, false);
        let mut args = Vec::new();
        let mut seen_named = false;
        while !self.at(&Tok::RParen) {
            let name = if self.at(&Tok::Ident) && matches!(self.peek_tok(1), Tok::Colon) {
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
            args.push(Arg { name, value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.no_struct = saved;
        self.in_guard = saved_guard;
        self.expect(&Tok::RParen, "`)`")?;
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
                Tok::LParen => {
                    self.bump();
                    let args = self.args()?;
                    e = self.mk(ExprKind::Call { callee: Box::new(e), args }, self.span_from(lo));
                }
                Tok::LBracket => {
                    self.bump();
                    let saved = std::mem::replace(&mut self.no_struct, false);
                    let idx = self.expr();
                    self.no_struct = saved;
                    let idx = idx?;
                    self.expect(&Tok::RBracket, "`]`")?;
                    e = self.mk(ExprKind::Index(Box::new(e), Box::new(idx)), self.span_from(lo));
                }
                Tok::Question => {
                    self.bump();
                    e = self.mk(ExprKind::Try(Box::new(e)), self.span_from(lo));
                }
                Tok::Bang if !self.toks[self.pos].newline_before => {
                    self.error_here("Sand has no postfix `!`; use `?`, `??`, or `unwrap()`");
                    return Err(());
                }
                Tok::Dot => {
                    self.bump();
                    match *self.tok() {
                        Tok::Ident => e = self.member(e, lo)?,
                        _ if self.tok().is_keyword() => e = self.member(e, lo)?,
                        Tok::Int(i) => {
                            let span = self.bump();
                            let Ok(i) = u32::try_from(i) else {
                                self.errors.push(Diag::error(span, "invalid tuple index"));
                                return Err(());
                            };
                            e = self.mk(ExprKind::TupleField(Box::new(e), i, span), self.span_from(lo));
                        }
                        Tok::Float(_) => {
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
                Tok::LBrace if !self.no_struct && self.is_struct_lit_start(&e) => {
                    e = self.struct_lit(e, lo)?;
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// `.name` or `.name(args)` after `e` (the `.` was consumed).
    fn member(&mut self, receiver: Expr, lo: u32) -> PResult<Expr> {
        let name = self.name_or_keyword("field or method name")?;
        let receiver = Box::new(receiver);
        if self.eat(&Tok::LParen) {
            let args = self.args()?;
            Ok(self.mk(ExprKind::MethodCall { receiver, method: name, args }, self.span_from(lo)))
        } else {
            Ok(self.mk(ExprKind::Field(receiver, name), self.span_from(lo)))
        }
    }

    fn path_of(e: &Expr) -> Option<Vec<Ident>> {
        match &e.kind {
            ExprKind::Ident(id) => Some(vec![id.clone()]),
            ExprKind::SelfType => Some(vec![Ident { name: "Self".into(), span: e.span }]),
            ExprKind::Field(base, name) => {
                let mut p = Self::path_of(base)?;
                p.push(name.clone());
                Some(p)
            }
            _ => None,
        }
    }

    fn is_struct_lit_start(&self, e: &Expr) -> bool {
        if Self::path_of(e).is_none() {
            return false;
        }
        match self.peek_tok(1) {
            Tok::RBrace | Tok::DotDot => true,
            Tok::Ident => matches!(self.peek_tok(2), Tok::Colon | Tok::Comma | Tok::RBrace | Tok::Semi),
            Tok::Semi => {
                // `Name {` followed by a newline: look at what follows.
                let mut i = 1;
                while matches!(self.peek_tok(i), Tok::Semi) {
                    i += 1;
                }
                match self.peek_tok(i) {
                    Tok::RBrace | Tok::DotDot => true,
                    Tok::Ident => matches!(self.peek_tok(i + 1), Tok::Colon | Tok::Comma | Tok::RBrace | Tok::Semi),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn struct_lit(&mut self, path_expr: Expr, lo: u32) -> PResult<Expr> {
        let path = Self::path_of(&path_expr).unwrap();
        self.expect(&Tok::LBrace, "`{`")?;
        let saved = std::mem::replace(&mut self.no_struct, false);
        let mut fields = Vec::new();
        let mut base = None;
        loop {
            while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            if self.eat(&Tok::DotDot) {
                base = Some(Box::new(self.expr()?));
                while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
                break;
            }
            let name = self.ident("field name")?;
            let value = if self.eat(&Tok::Colon) { Some(self.expr()?) } else { None };
            fields.push(FieldInit { name, value });
            if !(self.at(&Tok::Comma) || self.at(&Tok::Semi) || self.at(&Tok::RBrace)) {
                self.expected("`,` or `}`");
                self.no_struct = saved;
                return Err(());
            }
        }
        self.no_struct = saved;
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(self.mk(ExprKind::StructLit { path, fields, base }, self.span_from(lo)))
    }

    fn lambda(&mut self, lo: u32) -> PResult<Expr> {
        let mut params = Vec::new();
        let parens = self.at(&Tok::LParen);
        if parens {
            self.bump();
            while !self.at(&Tok::RParen) {
                let name = if self.at(&Tok::Ident) {
                    self.ident("parameter")?
                } else {
                    self.expected("parameter name");
                    return Err(());
                };
                let ty = if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None };
                params.push(LambdaParam { name, ty });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen, "`)`")?;
        } else {
            let name = self.ident("parameter")?;
            params.push(LambdaParam { name, ty: None });
        }
        self.expect(&Tok::FatArrow, "`=>`")?;
        let saved = std::mem::replace(&mut self.no_struct, false);
        let body = self.expr();
        self.no_struct = saved;
        let body = body?;
        Ok(self.mk(ExprKind::Lambda { params, body: Box::new(body), parens }, self.span_from(lo)))
    }

    fn cond(&mut self) -> PResult<Cond> {
        if self.eat(&Tok::Let) {
            let p = self.pattern()?;
            self.expect(&Tok::Eq, "`=`")?;
            let e = self.expr_no_struct()?;
            Ok(Cond::Let(Box::new(p), Box::new(e)))
        } else {
            Ok(Cond::Expr(Box::new(self.expr_no_struct()?)))
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        match *self.tok() {
            Tok::Int(v) => {
                self.bump();
                Ok(self.mk(ExprKind::Int(v), self.span_from(lo)))
            }
            Tok::IntOverflow(exact) => {
                self.bump();
                Ok(self.mk(ExprKind::IntOverflow(exact), self.span_from(lo)))
            }
            Tok::Float(v) => {
                self.bump();
                Ok(self.mk(ExprKind::Float(v), self.span_from(lo)))
            }
            Tok::Inf | Tok::Nan => {
                let v = if self.at(&Tok::Inf) { f64::INFINITY } else { f64::NAN };
                self.bump();
                Ok(self.mk(ExprKind::Float(v), self.span_from(lo)))
            }
            Tok::Duration(v) => {
                self.bump();
                Ok(self.mk(ExprKind::Duration(v), self.span_from(lo)))
            }
            Tok::True | Tok::False => {
                let value = self.at(&Tok::True);
                self.bump();
                Ok(self.mk(ExprKind::Bool(value), self.span_from(lo)))
            }
            Tok::Str(_) => {
                let text = self.take_text();
                Ok(self.mk(ExprKind::Str(vec![StrSeg::Text(text)]), self.span_from(lo)))
            }
            Tok::StrStart => self.interpolated_string(),
            Tok::Bytes(_) => {
                let bytes = match &mut self.toks[self.pos].tok {
                    Tok::Bytes(b) => std::mem::take(b),
                    _ => unreachable!(),
                };
                self.bump();
                Ok(self.mk(ExprKind::Bytes(bytes), self.span_from(lo)))
            }
            Tok::Ident => {
                if matches!(self.peek_tok(1), Tok::FatArrow) && !self.in_guard {
                    return self.lambda(lo);
                }
                if matches!(self.peek_tok(1), Tok::Bang)
                    && (matches!(self.peek_tok(2), Tok::LParen | Tok::LBrace | Tok::LBracket)
                        || MACROS.iter().any(|m| m.name == self.cur_text()))
                {
                    return self.macro_call(lo);
                }
                let id = self.ident("expression")?;
                Ok(self.mk(ExprKind::Ident(id), self.span_from(lo)))
            }
            Tok::SelfLower => {
                self.bump();
                Ok(self.mk(ExprKind::SelfValue, self.span_from(lo)))
            }
            Tok::SelfUpper => {
                self.bump();
                Ok(self.mk(ExprKind::SelfType, self.span_from(lo)))
            }
            Tok::LParen => {
                if !self.in_guard && self.toks[self.pos].opens_lambda {
                    return self.lambda(lo);
                }
                self.bump();
                if self.eat(&Tok::RParen) {
                    return Ok(self.mk(ExprKind::Unit, self.span_from(lo)));
                }
                let saved = std::mem::replace(&mut self.no_struct, false);
                let first = self.expr();
                let first = match first {
                    Ok(f) => f,
                    Err(()) => {
                        self.no_struct = saved;
                        return Err(());
                    }
                };
                if self.eat(&Tok::RParen) {
                    self.no_struct = saved;
                    return Ok(self.mk(ExprKind::Paren(Box::new(first)), self.span_from(lo)));
                }
                let items = self.tuple_rest(first, lo, Self::expr);
                self.no_struct = saved;
                Ok(self.mk(ExprKind::Tuple(items?), self.span_from(lo)))
            }
            Tok::LBracket => self.bracket(lo),
            Tok::LBrace => {
                let b = self.block()?;
                Ok(self.mk(ExprKind::Block(b), self.span_from(lo)))
            }
            Tok::If => self.if_expr(),
            Tok::Match => {
                self.bump();
                let scrutinee = self.expr_no_struct()?;
                self.expect(&Tok::LBrace, "`{`")?;
                let saved = std::mem::replace(&mut self.no_struct, false);
                let mut arms = Vec::new();
                loop {
                    while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
                    if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                        break;
                    }
                    let alo = self.lo();
                    let pattern = self.pattern()?;
                    let guard = if self.eat(&Tok::If) {
                        let saved = std::mem::replace(&mut self.in_guard, true);
                        let g = self.expr();
                        self.in_guard = saved;
                        Some(g?)
                    } else {
                        None
                    };
                    self.expect(&Tok::FatArrow, "`=>`")?;
                    let body = self.expr()?;
                    arms.push(MatchArm { pattern, guard, body, span: self.span_from(alo) });
                    if !(self.at(&Tok::Semi) || self.at(&Tok::Comma) || self.at(&Tok::RBrace)) {
                        self.expected("`,`, newline or `}`");
                        self.no_struct = saved;
                        return Err(());
                    }
                }
                self.no_struct = saved;
                self.expect(&Tok::RBrace, "`}`")?;
                Ok(self.mk(ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, self.span_from(lo)))
            }
            Tok::At => {
                let label = self.label()?;
                if !matches!(self.tok(), Tok::Loop | Tok::While | Tok::For) {
                    self.expected("`loop`, `while` or `for` after a label");
                    return Err(());
                }
                self.loop_expr(lo, Some(label))
            }
            Tok::Loop | Tok::While | Tok::For => self.loop_expr(lo, None),
            Tok::Return => {
                self.bump();
                let v = if self.expr_follows() { Some(Box::new(self.expr()?)) } else { None };
                Ok(self.mk(ExprKind::Return(v), self.span_from(lo)))
            }
            Tok::Break => {
                self.bump();
                let label = if self.at(&Tok::At) { Some(self.label()?) } else { None };
                let value = if self.expr_follows() { Some(Box::new(self.expr()?)) } else { None };
                Ok(self.mk(ExprKind::Break { label, value }, self.span_from(lo)))
            }
            Tok::Continue => {
                self.bump();
                let label = if self.at(&Tok::At) { Some(self.label()?) } else { None };
                Ok(self.mk(ExprKind::Continue(label), self.span_from(lo)))
            }
            Tok::Spawn | Tok::Not | Tok::Minus | Tok::Tilde | Tok::DotDot | Tok::DotDotEq => self.prefix(0),
            _ => {
                self.expected("expression");
                Err(())
            }
        }
    }

    /// `@name` (the label's span covers the name only).
    fn label(&mut self) -> PResult<Ident> {
        self.expect(&Tok::At, "`@`")?;
        if !self.at(&Tok::Ident) || self.toks[self.pos].lo != self.prev_hi() {
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
            Tok::Loop => ExprKind::Loop { label, body: self.block()? },
            Tok::While => {
                let cond = self.cond()?;
                ExprKind::While { label, cond, body: self.block()? }
            }
            _ => {
                let pattern = Box::new(self.pattern()?);
                self.expect(&Tok::In, "`in`")?;
                let iter = self.expr_no_struct()?;
                ExprKind::For { label, pattern, iter: Box::new(iter), body: self.block()? }
            }
        };
        Ok(self.mk(kind, self.span_from(lo)))
    }

    fn expr_follows(&self) -> bool {
        !matches!(self.tok(), Tok::Semi | Tok::RBrace | Tok::RParen | Tok::RBracket | Tok::Comma | Tok::Eof | Tok::Else)
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        self.bump();
        let cond = self.cond()?;
        let then = self.block()?;
        let else_ = if self.eat(&Tok::Else) {
            if self.at(&Tok::If) {
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
            if self.eat(&Tok::For) {
                let p = self.pattern()?;
                self.expect(&Tok::In, "`in`")?;
                let e = self.expr()?;
                clauses.push(CompClause::For(p, e));
            } else if self.eat(&Tok::If) {
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
        if !self.at(&Tok::LParen) {
            self.error_here(format!("a macro call is written `{}!(...)`", name.name));
            return Err(());
        }
        self.bump();
        let saved = std::mem::replace(&mut self.no_struct, false);
        let arg = self.data();
        self.no_struct = saved;
        let arg = arg?;
        self.expect(&Tok::RParen, "`)`")?;
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
        if self.at(&Tok::Ident) && self.cur_text() == "null" {
            self.bump();
            return Ok(Data::Null(self.span_from(lo)));
        }
        if self.eat(&Tok::LBrace) {
            let mut entries = Vec::new();
            loop {
                self.skip_newlines();
                if self.eat(&Tok::RBrace) {
                    break;
                }
                let k = self.expr()?;
                self.expect(&Tok::Colon, "`:`")?;
                entries.push((k, self.data()?));
                self.skip_newlines();
                if !self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    self.expect(&Tok::RBrace, "`,` or `}`")?;
                    break;
                }
            }
            return Ok(Data::Object(entries, self.span_from(lo)));
        }
        if !self.eat(&Tok::LBracket) {
            return Ok(Data::Value(self.expr()?));
        }
        if self.at(&Tok::Colon) && matches!(self.peek_tok(1), Tok::RBracket) {
            self.bump();
            self.bump();
            return Ok(Data::Object(Vec::new(), self.span_from(lo)));
        }
        if self.eat(&Tok::RBracket) {
            return Ok(Data::List(Vec::new(), self.span_from(lo)));
        }
        let first = self.data()?;
        if !self.eat(&Tok::Colon) {
            let mut items = vec![first];
            while self.eat(&Tok::Comma) && !self.at(&Tok::RBracket) {
                items.push(self.data()?);
            }
            self.expect(&Tok::RBracket, "`,` or `]`")?;
            return Ok(Data::List(items, self.span_from(lo)));
        }
        let Data::Value(k) = first else {
            self.errors.push(Diag::error(first.span(), "an object key must be a string"));
            return Err(());
        };
        let mut entries = vec![(k, self.data()?)];
        while self.eat(&Tok::Comma) && !self.at(&Tok::RBracket) {
            let k = self.expr()?;
            self.expect(&Tok::Colon, "`:`")?;
            entries.push((k, self.data()?));
        }
        self.expect(&Tok::RBracket, "`,` or `]`")?;
        Ok(Data::Object(entries, self.span_from(lo)))
    }

    /// Skips the statement ends that newlines insert.
    fn skip_newlines(&mut self) {
        while self.at(&Tok::Semi) && self.toks[self.pos].lo == self.toks[self.pos].hi {
            self.bump();
        }
    }

    fn bracket(&mut self, lo: u32) -> PResult<Expr> {
        self.bump();
        let saved = std::mem::replace(&mut self.no_struct, false);
        let r = self.bracket_inner(lo);
        self.no_struct = saved;
        r
    }

    fn bracket_inner(&mut self, lo: u32) -> PResult<Expr> {
        if self.eat(&Tok::RBracket) {
            return Ok(self.mk(ExprKind::List(vec![]), self.span_from(lo)));
        }
        if self.at(&Tok::Colon) && matches!(self.peek_tok(1), Tok::RBracket) {
            self.bump();
            self.bump();
            return Ok(self.mk(ExprKind::Map(vec![]), self.span_from(lo)));
        }
        let first = self.expr()?;
        if self.eat(&Tok::Colon) {
            let v = self.expr()?;
            if self.at(&Tok::For) {
                let clauses = self.comp_clauses()?;
                self.expect(&Tok::RBracket, "`]`")?;
                return Ok(self.mk(ExprKind::MapComp(Box::new(first), Box::new(v), clauses), self.span_from(lo)));
            }
            let mut pairs = vec![(first, v)];
            while self.eat(&Tok::Comma) {
                if self.at(&Tok::RBracket) {
                    break;
                }
                let k = self.expr()?;
                self.expect(&Tok::Colon, "`:`")?;
                let v = self.expr()?;
                pairs.push((k, v));
            }
            self.expect(&Tok::RBracket, "`]`")?;
            return Ok(self.mk(ExprKind::Map(pairs), self.span_from(lo)));
        }
        if self.eat(&Tok::Semi) {
            let n = self.expr()?;
            self.expect(&Tok::RBracket, "`]`")?;
            return Ok(self.mk(ExprKind::Repeat(Box::new(first), Box::new(n)), self.span_from(lo)));
        }
        if self.at(&Tok::For) {
            let clauses = self.comp_clauses()?;
            self.expect(&Tok::RBracket, "`]`")?;
            return Ok(self.mk(ExprKind::ListComp(Box::new(first), clauses), self.span_from(lo)));
        }
        let mut items = vec![first];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::RBracket) {
                break;
            }
            items.push(self.expr()?);
        }
        self.expect(&Tok::RBracket, "`]`")?;
        Ok(self.mk(ExprKind::List(items), self.span_from(lo)))
    }

    /// A string with interpolations, from its `StrStart` token.
    fn interpolated_string(&mut self) -> PResult<Expr> {
        let lo = self.lo();
        self.bump();
        let mut segs = Vec::new();
        loop {
            match self.tok() {
                Tok::StrText(_) => segs.push(StrSeg::Text(self.take_text())),
                Tok::InterpStart => {
                    let open = self.bump();
                    if let Tok::InterpEnd { .. } = self.tok() {
                        let span = Span::new(self.file, open.lo, self.toks[self.pos].hi);
                        self.errors.push(Diag::error(span, "empty interpolation: `\\(` must hold an expression"));
                        return Err(());
                    }
                    // An interpolation is parsed on its own, like a parenthesized expression.
                    let saved = (std::mem::replace(&mut self.no_struct, false), std::mem::replace(&mut self.in_guard, false));
                    let e = self.expr();
                    (self.no_struct, self.in_guard) = saved;
                    let e = e?;
                    let Tok::InterpEnd { spec } = *self.tok() else {
                        self.expected("`)` to end the interpolation");
                        return Err(());
                    };
                    let end = self.bump();
                    // The token spans `:spec)`.
                    let spec = spec.then(|| {
                        let span = Span::new(self.file, end.lo + 1, end.hi - 1);
                        FormatSpecAst { text: self.slice(span).to_string(), span }
                    });
                    segs.push(StrSeg::Expr(Box::new(e), spec));
                }
                Tok::StrEnd => {
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
        if !self.at(&Tok::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat(&Tok::Pipe) {
            alts.push(self.pattern_atom()?);
        }
        Ok(Pattern { kind: PatternKind::Or(alts), span: self.span_from(lo) })
    }

    fn int_lit_pattern(&mut self) -> PResult<i64> {
        let neg = self.eat(&Tok::Minus);
        match *self.tok() {
            Tok::Int(v) => {
                self.bump();
                Ok(if neg { -v } else { v })
            }
            Tok::IntOverflow(true) if neg => {
                self.bump();
                Ok(i64::MIN)
            }
            _ => {
                self.expected("integer");
                Err(())
            }
        }
    }

    fn pattern_atom(&mut self) -> PResult<Pattern> {
        let lo = self.lo();
        let kind = match self.tok() {
            Tok::Int(_) | Tok::Minus | Tok::IntOverflow(_) => {
                let a = self.int_lit_pattern()?;
                if self.eat(&Tok::DotDotEq) {
                    let b = self.int_lit_pattern()?;
                    PatternKind::Range(a, b)
                } else if self.at(&Tok::DotDot) {
                    self.error_here("exclusive range patterns are not supported; use `..=`");
                    return Err(());
                } else {
                    PatternKind::Literal(PatLit::Int(a))
                }
            }
            Tok::Str(_) | Tok::StrStart => PatternKind::Literal(PatLit::Str(self.plain_string("a string pattern")?)),
            Tok::True | Tok::False => {
                let b = self.at(&Tok::True);
                self.bump();
                PatternKind::Literal(PatLit::Bool(b))
            }
            Tok::LParen => {
                self.bump();
                if self.eat(&Tok::RParen) {
                    PatternKind::Literal(PatLit::Unit)
                } else {
                    let first = self.pattern()?;
                    if self.eat(&Tok::RParen) {
                        return Ok(first);
                    }
                    PatternKind::Tuple(self.tuple_rest(first, lo, Self::pattern)?)
                }
            }
            Tok::LBracket => {
                self.bump();
                let mut before = Vec::new();
                let mut after = Vec::new();
                let mut rest = None;
                while !self.at(&Tok::RBracket) {
                    if self.at(&Tok::DotDot) {
                        if rest.is_some() {
                            self.error_here("only one `..` is allowed in a list pattern");
                            return Err(());
                        }
                        self.bump();
                        let name = if self.at(&Tok::Ident) { Some(self.ident("name")?) } else { None };
                        rest = Some(name);
                    } else if rest.is_some() {
                        after.push(self.pattern()?);
                    } else {
                        before.push(self.pattern()?);
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBracket, "`]`")?;
                PatternKind::List { before, rest, after }
            }
            Tok::Ident | Tok::SelfUpper => {
                let mut path = Vec::new();
                if self.at(&Tok::SelfUpper) {
                    let span = self.bump();
                    path.push(Ident { name: "Self".into(), span });
                } else {
                    path.push(self.ident("pattern")?);
                }
                while self.at(&Tok::Dot) && matches!(self.peek_tok(1), Tok::Ident) {
                    self.bump();
                    path.push(self.ident("name")?);
                }
                if self.at(&Tok::LParen) {
                    self.bump();
                    let mut args = Vec::new();
                    while !self.at(&Tok::RParen) {
                        args.push(self.pattern()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::RParen, "`)`")?;
                    PatternKind::Variant { path, args: Some(args) }
                } else if self.at(&Tok::LBrace) {
                    self.bump();
                    let mut fields = Vec::new();
                    let mut rest = false;
                    loop {
                        while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
                        if self.at(&Tok::RBrace) {
                            break;
                        }
                        if self.eat(&Tok::DotDot) {
                            rest = true;
                            while self.eat(&Tok::Semi) || self.eat(&Tok::Comma) {}
                            break;
                        }
                        let name = self.ident("field name")?;
                        let pattern = if self.eat(&Tok::Colon) { Some(self.pattern()?) } else { None };
                        fields.push(FieldPat { name, pattern });
                        if !(self.at(&Tok::Comma) || self.at(&Tok::Semi) || self.at(&Tok::RBrace)) {
                            self.expected("`,` or `}`");
                            return Err(());
                        }
                    }
                    self.expect(&Tok::RBrace, "`}`")?;
                    PatternKind::Struct { path, fields, rest }
                } else if path.len() == 1 {
                    if path[0].name == "_" { PatternKind::Wildcard } else { PatternKind::Binding(path.pop().unwrap()) }
                } else {
                    PatternKind::Variant { path, args: None }
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
    let call =
        |callee: Expr, value: Expr| Expr { kind: ExprKind::Call { callee: Box::new(callee), args: vec![Arg { name: None, value }] }, span };
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
