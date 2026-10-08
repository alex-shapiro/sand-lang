//! The linter: style and correctness warnings on top of the type checker.

use crate::engine::{Engine, ResolvedFacts};
use crate::syntax::ast::*;
use crate::syntax::lexer::CommentKind;
use crate::{Config, Policy};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintWarning {
    pub code: &'static str,
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for LintWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: warning[{}]: {}", self.line, self.column, self.code, self.message)
    }
}

/// Lints a file (its imports are loaded relative to it).
pub fn lint_file(path: &Path) -> Result<Vec<LintWarning>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let engine = Engine::new(Config::new().policy(Policy::allow_all()));
    let rt = tokio::runtime::Builder::new_current_thread().build().map_err(|e| e.to_string())?;
    let facts = rt.block_on(engine.lint_facts(path)).map_err(|e| e.to_string())?;
    lint_with(&src, facts)
}

/// Lints source text (imports are resolved against the current directory).
pub fn lint_source(name: &str, src: &str) -> Result<Vec<LintWarning>, String> {
    let engine = Engine::new(Config::new().policy(Policy::allow_all()));
    let rt = tokio::runtime::Builder::new_current_thread().build().map_err(|e| e.to_string())?;
    let facts = rt.block_on(engine.lint_facts_source(name, src)).map_err(|e| e.to_string())?;
    lint_with(src, facts)
}

fn lint_with(src: &str, facts: ResolvedFacts) -> Result<Vec<LintWarning>, String> {
    let src = src.to_string();
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(s, || run_lints(&src, &facts))
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "internal linter error".to_string())?
    })
}

struct Linter<'a> {
    src: &'a str,
    line_starts: Vec<usize>,
    out: Vec<LintWarning>,
    facts: &'a ResolvedFacts,
    /// Types of bindings by (line, column), from the checker.
    types: HashMap<(usize, usize), String>,
    /// `let` bindings in enclosing scopes of the current function: name -> type.
    let_scopes: Vec<Vec<(String, String)>>,
}

fn run_lints(src: &str, facts: &ResolvedFacts) -> Result<Vec<LintWarning>, String> {
    let parsed = crate::syntax::parse_module(src, 0);
    if !parsed.errors.is_empty() {
        return Err("cannot lint a file with syntax errors".into());
    }
    let mut line_starts = vec![0];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let mut l = Linter {
        src,
        line_starts,
        out: Vec::new(),
        facts,
        types: facts.locals.iter().map(|(ln, c, f)| ((*ln, *c), f.ty.clone())).collect(),
        let_scopes: vec![vec![]],
    };
    l.module(&parsed.module);
    l.fact_lints(&parsed.module);

    // Suppressions.
    let mut file_allow: HashSet<String> = HashSet::new();
    let mut line_allow: HashMap<usize, HashSet<String>> = HashMap::new();
    for c in &parsed.comments {
        let text = match c.kind {
            CommentKind::ModuleDoc => c.text.trim_start_matches("//!").trim(),
            CommentKind::Line => c.text.trim_start_matches("//").trim(),
            _ => continue,
        };
        let Some(rest) = text.strip_prefix("lint: allow(") else { continue };
        let names: HashSet<String> = rest.trim_end_matches(')').split(',').map(|n| n.trim().to_string()).collect();
        if c.kind == CommentKind::ModuleDoc {
            file_allow.extend(names);
        } else {
            let line = l.line_of(c.lo as usize);
            line_allow.entry(line + 1).or_default().extend(names);
        }
    }
    let mut out: Vec<LintWarning> = l
        .out
        .into_iter()
        .filter(|w| !file_allow.contains(w.code) && !line_allow.get(&w.line).is_some_and(|s| s.contains(w.code)))
        .collect();
    out.sort_by(|a, b| (a.line, a.column, a.code).cmp(&(b.line, b.column, b.code)));
    out.dedup();
    Ok(out)
}

fn is_upper_camel(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !s.contains('_')
}

fn is_snake(s: &str) -> bool {
    let t = s.trim_start_matches('_');
    t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_screaming(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && s.chars().any(|c| c.is_ascii_uppercase())
}

fn diverges(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Return(_) | ExprKind::Break(_) | ExprKind::Continue => true,
        ExprKind::Call { callee, .. } => {
            matches!(&callee.kind, ExprKind::Ident(id) if matches!(id.name.as_str(), "panic" | "exit" | "todo"))
        }
        _ => false,
    }
}

impl<'a> Linter<'a> {
    fn line_of(&self, pos: usize) -> usize {
        match self.line_starts.binary_search(&pos) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    fn col_of(&self, pos: usize) -> usize {
        let line = self.line_of(pos);
        let start = self.line_starts[line - 1];
        self.src.get(start..pos).map(|s| s.chars().count()).unwrap_or(0) + 1
    }

    fn warn(&mut self, pos: u32, code: &'static str, message: String) {
        let p = pos as usize;
        self.out.push(LintWarning { code, message, line: self.line_of(p), column: self.col_of(p) });
    }

    fn name(&mut self, id: &Ident, ok: bool, what: &str, style: &str) {
        if !ok {
            self.warn(id.span.lo, "naming", format!("{what} `{}` should be {style}", id.name));
        }
    }

    // ------------------------------------------------------------------
    // AST lints

    fn module(&mut self, m: &Module) {
        for item in &m.items {
            match &item.kind {
                ItemKind::Fn(f) => self.fn_decl(f),
                ItemKind::Struct(s) => {
                    self.name(&s.name, is_upper_camel(&s.name.name), "type", "UpperCamelCase");
                    self.generics(&s.generics);
                    for f in &s.fields {
                        self.name(&f.name, is_snake(&f.name.name), "field", "snake_case");
                        if let Some(d) = &f.default {
                            self.expr(d);
                        }
                    }
                    for m in &s.methods {
                        self.fn_decl(m);
                    }
                }
                ItemKind::Enum(e) => {
                    self.name(&e.name, is_upper_camel(&e.name.name), "type", "UpperCamelCase");
                    self.generics(&e.generics);
                    for v in &e.variants {
                        self.name(&v.name, is_upper_camel(&v.name.name), "variant", "UpperCamelCase");
                    }
                    for m in &e.methods {
                        self.fn_decl(m);
                    }
                }
                ItemKind::Extend(x) => {
                    for m in &x.methods {
                        self.fn_decl(m);
                    }
                }
                ItemKind::TypeAlias(a) => self.name(&a.name, is_upper_camel(&a.name.name), "type", "UpperCamelCase"),
                ItemKind::NativeType(n) => self.name(&n.name, is_upper_camel(&n.name.name), "type", "UpperCamelCase"),
                ItemKind::Import(imp) => {
                    if let Some(a) = &imp.alias {
                        self.name(a, is_snake(&a.name), "module alias", "snake_case");
                    }
                }
                ItemKind::Test(t) => {
                    self.let_scopes.push(vec![]);
                    self.block(&t.body);
                    self.let_scopes.pop();
                }
                ItemKind::Stmt(s) => self.stmt(s, true),
            }
        }
        // Unreachable top-level statements.
        let stmts: Vec<&Stmt> = m.items.iter().filter_map(|i| if let ItemKind::Stmt(s) = &i.kind { Some(s) } else { None }).collect();
        self.unreachable(&stmts);
    }

    fn generics(&mut self, gs: &[GenericParam]) {
        for g in gs {
            self.name(&g.name, is_upper_camel(&g.name.name), "type parameter", "UpperCamelCase");
        }
    }

    fn fn_decl(&mut self, f: &FnDecl) {
        self.name(&f.name, is_snake(&f.name.name), "function", "snake_case");
        self.generics(&f.generics);
        for p in &f.params {
            if p.name.name != "self" {
                self.name(&p.name, is_snake(&p.name.name), "parameter", "snake_case");
            }
            if let Some(d) = &p.default {
                self.expr(d);
            }
        }
        if let Some(b) = &f.body {
            let saved = std::mem::replace(&mut self.let_scopes, vec![vec![]]);
            self.block(b);
            self.let_scopes = saved;
        }
    }

    fn unreachable(&mut self, stmts: &[&Stmt]) {
        for w in stmts.windows(2) {
            if let StmtKind::Expr(e) = &w[0].kind
                && diverges(e)
            {
                self.warn(w[1].span.lo, "unreachable-code", "unreachable code".into());
                break;
            }
        }
    }

    fn block(&mut self, b: &Block) {
        self.let_scopes.push(vec![]);
        for s in &b.stmts {
            self.stmt(s, false);
        }
        let refs: Vec<&Stmt> = b.stmts.iter().collect();
        self.unreachable(&refs);
        self.let_scopes.pop();
    }

    fn empty_block(&mut self, b: &Block) {
        if b.stmts.is_empty() {
            self.warn(b.span.lo, "empty-block", "empty block".into());
        }
    }

    fn pattern_names(&mut self, p: &Pattern, top_level_let: bool) {
        self.pattern_names_in(p, top_level_let, true);
    }

    /// `is_let`: in `match`/`if let` patterns a capitalised name is a variant.
    fn pattern_names_in(&mut self, p: &Pattern, top_level_let: bool, is_let: bool) {
        let mut ids = Vec::new();
        p.bindings(&mut ids);
        for id in ids {
            if !is_let && id.name.starts_with(|c: char| c.is_ascii_uppercase()) {
                continue;
            }
            let ok = is_snake(&id.name) || (top_level_let && is_screaming(&id.name));
            self.name(&id, ok, "variable", "snake_case");
        }
    }

    fn stmt(&mut self, s: &Stmt, top: bool) {
        match &s.kind {
            StmtKind::Let { pattern, value, else_block, mutable, .. } => {
                self.expr(value);
                if let Some(b) = else_block {
                    self.block(b);
                }
                self.pattern_names(pattern, top && !mutable);
                if let PatternKind::Binding(id) = &pattern.kind {
                    let pos = (self.line_of(id.span.lo as usize), self.col_of(id.span.lo as usize));
                    let ty = self.types.get(&pos).cloned().unwrap_or_default();
                    let prior =
                        self.let_scopes.iter().rev().flat_map(|sc| sc.iter().rev()).find(|(n, _)| *n == id.name).map(|(_, t)| t.clone());
                    if let Some(prev_ty) = prior
                        && !prev_ty.is_empty()
                        && !ty.is_empty()
                        && prev_ty != ty
                    {
                        self.warn(
                            id.span.lo,
                            "shadowed-variable",
                            format!("`{}` shadows a binding of a different type ({prev_ty} vs {ty})", id.name),
                        );
                    }
                    if let Some(scope) = self.let_scopes.last_mut() {
                        scope.push((id.name.clone(), ty));
                    }
                }
            }
            StmtKind::Assign { target, op, value } => {
                if *op == AssignOp::Assign
                    && let (ExprKind::Ident(a), ExprKind::Ident(b)) = (&target.kind, &value.kind)
                    && a.name == b.name
                {
                    self.warn(s.span.lo, "self-assignment", format!("`{}` is assigned to itself", a.name));
                }
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Fn(f) => self.fn_decl(f),
        }
    }

    fn cond(&mut self, c: &Cond) {
        match c {
            Cond::Expr(e) => self.expr(e),
            Cond::Let(p, e) => {
                self.pattern_names_in(p, false, false);
                self.expr(e);
            }
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Binary(op, a, b) => {
                if matches!(op, BinOp::Eq | BinOp::Ne) {
                    if matches!(a.kind, ExprKind::Bool(_)) || matches!(b.kind, ExprKind::Bool(_)) {
                        self.warn(
                            e.span.lo,
                            "bool-comparison",
                            "comparison with a boolean literal; use the value directly or `not`".into(),
                        );
                    }
                    let is_len =
                        |x: &Expr| matches!(&x.kind, ExprKind::MethodCall { method, args, .. } if method.name == "len" && args.is_empty());
                    let is_zero = |x: &Expr| matches!(x.kind, ExprKind::Int(0));
                    if (is_len(a) && is_zero(b)) || (is_zero(a) && is_len(b)) {
                        self.warn(e.span.lo, "len-zero", "use `is_empty()` instead of comparing `len()` with 0".into());
                    }
                }
                self.expr(a);
                self.expr(b);
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Ident(id) = &callee.kind
                    && id.name == "todo"
                {
                    self.warn(e.span.lo, "todo", "`todo()` left in code".into());
                }
                self.expr(callee);
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::If { cond, then, else_ } => {
                self.cond(cond);
                self.empty_block(then);
                self.block(then);
                if let Some(x) = else_ {
                    if let ExprKind::Block(b) = &x.kind {
                        self.empty_block(b);
                    }
                    self.expr(x);
                }
            }
            ExprKind::While { cond, body } => {
                self.cond(cond);
                self.empty_block(body);
                self.block(body);
            }
            ExprKind::Loop(body) => {
                self.empty_block(body);
                self.block(body);
            }
            ExprKind::For { pattern, iter, body } => {
                self.pattern_names(pattern, false);
                self.expr(iter);
                self.empty_block(body);
                self.block(body);
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Match { scrutinee, arms, .. } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pattern_names_in(&a.pattern, false, false);
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    self.expr(&a.body);
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                for p in params {
                    self.name(&p.name, is_snake(&p.name.name), "parameter", "snake_case");
                }
                self.let_scopes.push(vec![]);
                self.expr(body);
                self.let_scopes.pop();
            }
            ExprKind::Str(segs) => {
                for s in segs {
                    if let StrSeg::Expr(x, _) = s {
                        self.expr(x);
                    }
                }
            }
            ExprKind::List(xs) | ExprKind::Tuple(xs) => xs.iter().for_each(|x| self.expr(x)),
            ExprKind::Repeat(a, b) | ExprKind::Pipe(a, b) | ExprKind::Coalesce(a, b) | ExprKind::Index(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            ExprKind::Map(kvs) => {
                for (k, v) in kvs {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::ListComp(x, clauses) => {
                self.expr(x);
                self.clauses(clauses);
            }
            ExprKind::MapComp(k, v, clauses) => {
                self.expr(k);
                self.expr(v);
                self.clauses(clauses);
            }
            ExprKind::StructLit { fields, base, .. } => {
                for f in fields {
                    if let Some(v) = &f.value {
                        self.expr(v);
                    }
                }
                if let Some(b) = base {
                    self.expr(b);
                }
            }
            ExprKind::Unary(_, x)
            | ExprKind::Paren(x)
            | ExprKind::Try(x)
            | ExprKind::Spawn(x)
            | ExprKind::Field(x, _)
            | ExprKind::TupleField(x, _, _) => self.expr(x),
            ExprKind::Range { start, end, .. } => {
                if let Some(x) = start {
                    self.expr(x);
                }
                if let Some(x) = end {
                    self.expr(x);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.expr(receiver);
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::Return(Some(x)) | ExprKind::Break(Some(x)) => self.expr(x),
            _ => {}
        }
    }

    fn clauses(&mut self, cs: &[CompClause]) {
        for c in cs {
            match c {
                CompClause::For(p, e) => {
                    self.pattern_names(p, false);
                    self.expr(e);
                }
                CompClause::If(e) => self.expr(e),
            }
        }
    }

    // ------------------------------------------------------------------
    // Lints from checker facts

    fn fact_lints(&mut self, m: &Module) {
        let facts = self.facts;
        for (line, col, f) in &facts.locals {
            if f.name.starts_with('_') || f.name == "self" || f.name.starts_with('<') {
                continue;
            }
            if !f.used {
                self.out.push(LintWarning {
                    code: "unused-variable",
                    message: format!("unused variable `{}`", f.name),
                    line: *line,
                    column: *col,
                });
            }
            if f.mutable && !f.assigned {
                self.out.push(LintWarning {
                    code: "unnecessary-var",
                    message: format!("`{}` is never reassigned; use `let`", f.name),
                    line: *line,
                    column: *col,
                });
            }
        }
        for (line, col) in &facts.unused_results {
            self.out.push(LintWarning {
                code: "unused-result",
                message: "this `Result` is discarded; handle it with `?`, `match` or `let _ =`".into(),
                line: *line,
                column: *col,
            });
        }
        for (line, col, name) in &facts.imports {
            if !facts.used_names.contains(name) {
                self.out.push(LintWarning { code: "unused-import", message: format!("unused import `{name}`"), line: *line, column: *col });
            }
        }
        for item in &m.items {
            if let ItemKind::Fn(f) = &item.kind
                && !f.is_pub
                && !facts.used_names.contains(&f.name.name)
            {
                self.warn(f.name.span.lo, "unused-function", format!("function `{}` is never used", f.name.name));
            }
        }
    }
}
