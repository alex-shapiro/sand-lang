//! The linter: style and correctness warnings on top of the type checker.

use crate::check::budget::{CompileBudget, tool_fuel};
use crate::check::{CheckedProgram, Ctor, FnInfo, FnKind};
use crate::diag::SourceFile;
use crate::engine::{Engine, ResolvedFacts};
use crate::hir::{HArg, HKind};
use crate::syntax::ast::*;
use crate::syntax::lexer::CommentKind;
use crate::types::FnId;
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

/// Lints a file (its imports are loaded relative to it). Checking it is
/// bounded by the default [`Limits`](crate::Limits) (`max_compile_fuel`),
/// and the lints by [`tool_fuel`] units (one per syntax tree node and per
/// binding or expression examined again by a lint).
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
    let budget = CompileBudget::new(Some(tool_fuel(src.len())), None, None);
    let out = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn_scoped(s, || run_lints(&src, &facts, &budget))
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "internal linter error".to_string())?
    });
    match budget.stopped() {
        Some(stop) => Err(crate::fmt::stop_message(stop, "lint")),
        None => out,
    }
}

struct Linter<'a> {
    file: SourceFile,
    out: Vec<LintWarning>,
    facts: &'a ResolvedFacts,
    /// Types of bindings by (line, column), from the checker.
    types: HashMap<(usize, usize), String>,
    /// The names of the bindings (with known types) of each scope enclosing
    /// the current point of the current function, innermost last.
    let_scopes: Vec<Vec<String>>,
    /// The types of the bindings in `let_scopes` by name, innermost last.
    in_scope: HashMap<String, Vec<String>>,
    /// Bounds the work (see `lint_file`); once it is exhausted the linter
    /// skips the rest of the program.
    budget: &'a CompileBudget,
}

fn run_lints(src: &str, facts: &ResolvedFacts, budget: &CompileBudget) -> Result<Vec<LintWarning>, String> {
    if !budget.charge((src.len() / 64) as u64) {
        return Ok(vec![]);
    }
    let parsed = crate::syntax::parse_module(src, 0);
    if !parsed.errors.is_empty() {
        return Err("cannot lint a file with syntax errors".into());
    }
    let mut l = Linter {
        file: SourceFile::new("", src),
        out: Vec::new(),
        facts,
        types: facts.locals.iter().map(|(ln, c, f)| ((*ln, *c), f.ty.clone())).collect(),
        let_scopes: vec![vec![]],
        in_scope: HashMap::new(),
        budget,
    };
    l.module(&parsed.module);
    l.fact_lints();

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

fn diverges(b: &CompileBudget, e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue(_) => true,
        ExprKind::Call { callee, .. } => {
            matches!(&callee.kind, ExprKind::Ident(id) if matches!(id.name.as_str(), "panic" | "exit" | "todo"))
        }
        // A `loop` that nothing breaks out of never completes.
        ExprKind::Loop { label, body } => !block_any(body, &mut |x| breaks_to(b, x, label.as_ref(), 0)),
        _ => false,
    }
}

/// Calls `f` on the direct subexpressions of `e`, including the statements of
/// its blocks, but not the bodies of nested function declarations. Stops
/// (returning true) as soon as `f` returns true.
fn any_sub_expr(e: &Expr, f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    fn cond(c: &Cond) -> &Expr {
        match c {
            Cond::Expr(x) | Cond::Let(_, x) => x,
        }
    }
    match &e.kind {
        ExprKind::Block(b) | ExprKind::Loop { body: b, .. } => block_any(b, f),
        ExprKind::While { cond: c, body, .. } => f(cond(c)) || block_any(body, f),
        ExprKind::For { iter, body, .. } => f(iter) || block_any(body, f),
        ExprKind::If { cond: c, then, else_ } => f(cond(c)) || block_any(then, f) || else_.as_deref().is_some_and(&mut *f),
        ExprKind::Match { scrutinee, arms, .. } => f(scrutinee) || arms.iter().any(|a| a.guard.as_ref().is_some_and(&mut *f) || f(&a.body)),
        ExprKind::Str(segs) => segs.iter().any(|s| matches!(s, StrSeg::Expr(x, _) if f(x))),
        ExprKind::List(xs) | ExprKind::Tuple(xs) => xs.iter().any(&mut *f),
        ExprKind::Repeat(a, b) | ExprKind::Pipe(a, b) | ExprKind::Coalesce(a, b) | ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
            f(a) || f(b)
        }
        ExprKind::Map(kvs) => kvs.iter().any(|(k, v)| f(k) || f(v)),
        ExprKind::ListComp(x, cs) => f(x) || clauses_any(cs, f),
        ExprKind::MapComp(k, v, cs) => f(k) || f(v) || clauses_any(cs, f),
        ExprKind::StructLit { fields, base, .. } => {
            fields.iter().any(|x| x.value.as_ref().is_some_and(&mut *f)) || base.as_deref().is_some_and(&mut *f)
        }
        ExprKind::Unary(_, x)
        | ExprKind::Paren(x)
        | ExprKind::Try(x)
        | ExprKind::Spawn(x)
        | ExprKind::Field(x, _)
        | ExprKind::TupleField(x, _, _)
        | ExprKind::Lambda { body: x, .. } => f(x),
        ExprKind::Range { start, end, .. } => start.as_deref().is_some_and(&mut *f) || end.as_deref().is_some_and(&mut *f),
        ExprKind::Call { callee, args } => f(callee) || args.iter().any(|a| f(&a.value)),
        ExprKind::MethodCall { receiver, args, .. } => f(receiver) || args.iter().any(|a| f(&a.value)),
        ExprKind::Return(Some(x)) | ExprKind::Break { value: Some(x), .. } => f(x),
        _ => false,
    }
}

fn block_any(b: &Block, f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    b.stmts.iter().any(|s| match &s.kind {
        StmtKind::Let { value, else_block, .. } => f(value) || else_block.as_ref().is_some_and(|b| block_any(b, f)),
        StmtKind::Assign { target, value, .. } => f(target) || f(value),
        StmtKind::Expr(x) | StmtKind::Defer(x) => f(x),
        StmtKind::Fn(_) => false,
    })
}

fn clauses_any(cs: &[CompClause], f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    cs.iter().any(|c| match c {
        CompClause::For(_, x) | CompClause::If(x) => f(x),
    })
}

/// Whether `e` contains a `break` leaving the loop labeled `label` that is
/// `depth` loops out (labels and loops do not reach into lambdas). Charges
/// `b` per expression (nested loops are searched again for each enclosing
/// one), answering `true` once it is exhausted.
fn breaks_to(b: &CompileBudget, e: &Expr, label: Option<&Ident>, depth: usize) -> bool {
    if !b.charge(1) {
        return true;
    }
    match &e.kind {
        ExprKind::Break { label: Some(l), .. } if label.is_some_and(|x| x.name == l.name) => true,
        ExprKind::Break { label: None, .. } if depth == 0 => true,
        ExprKind::Lambda { .. } => false,
        ExprKind::Loop { .. } | ExprKind::While { .. } | ExprKind::For { .. } => {
            any_sub_expr(e, &mut |x| breaks_to(b, x, label, depth + 1))
        }
        _ => any_sub_expr(e, &mut |x| breaks_to(b, x, label, depth)),
    }
}

/// Whether `e` contains a `break @name` or `continue @name` (outside
/// lambdas). Charges `b` like `breaks_to`.
fn uses_label(b: &CompileBudget, e: &Expr, name: &str) -> bool {
    if !b.charge(1) {
        return true;
    }
    match &e.kind {
        ExprKind::Break { label: Some(l), .. } | ExprKind::Continue(Some(l)) if l.name == name => true,
        ExprKind::Lambda { .. } => false,
        _ => any_sub_expr(e, &mut |x| uses_label(b, x, name)),
    }
}

impl<'a> Linter<'a> {
    fn line_of(&self, pos: usize) -> usize {
        self.file.line_col(pos as u32).0
    }

    fn col_of(&self, pos: usize) -> usize {
        self.file.line_col(pos as u32).1
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
                ItemKind::Trait(t) => {
                    self.name(&t.name, is_upper_camel(&t.name.name), "trait", "UpperCamelCase");
                    self.generics(&t.generics);
                    for m in &t.methods {
                        self.fn_decl(m);
                    }
                }
                ItemKind::Impl(i) => {
                    self.generics(&i.generics);
                    // Method names are dictated by the trait (and linted there).
                    for m in &i.methods {
                        self.fn_parts(m);
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
                    // A test is a separate function: top-level bindings are not shadowed by it.
                    let saved = (std::mem::take(&mut self.let_scopes), std::mem::take(&mut self.in_scope));
                    self.block(&t.body);
                    (self.let_scopes, self.in_scope) = saved;
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
        self.fn_parts(f);
    }

    fn fn_parts(&mut self, f: &FnDecl) {
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
            let saved = (std::mem::replace(&mut self.let_scopes, vec![vec![]]), std::mem::take(&mut self.in_scope));
            for p in &f.params {
                if p.name.name != "self" {
                    self.bind(&p.name);
                }
            }
            self.block(b);
            (self.let_scopes, self.in_scope) = saved;
        }
    }

    /// The checker's type of the binding introduced at `id`, if known.
    fn binding_type(&self, id: &Ident) -> Option<String> {
        let pos = (self.line_of(id.span.lo as usize), self.col_of(id.span.lo as usize));
        self.types.get(&pos).filter(|t| !t.is_empty()).cloned()
    }

    /// Brings a binding into the innermost scope. Names without a checker type
    /// (e.g. variant names in patterns) are not bindings and are skipped.
    fn bind(&mut self, id: &Ident) {
        if let Some(ty) = self.binding_type(id)
            && let Some(scope) = self.let_scopes.last_mut()
        {
            scope.push(id.name.clone());
            self.in_scope.entry(id.name.clone()).or_default().push(ty);
        }
    }

    fn push_scope(&mut self) {
        self.let_scopes.push(vec![]);
    }

    fn pop_scope(&mut self) {
        for name in self.let_scopes.pop().unwrap_or_default() {
            if let Some(types) = self.in_scope.get_mut(&name) {
                types.pop();
                if types.is_empty() {
                    self.in_scope.remove(&name);
                }
            }
        }
    }

    fn bind_pattern(&mut self, p: &Pattern) {
        let mut ids = Vec::new();
        p.bindings(&mut ids);
        for id in &ids {
            self.bind(id);
        }
    }

    /// `shadowed-variable`: a `let`/`var` binding whose name is bound, with a
    /// different type, by a binding of the same function that is still in scope.
    fn check_shadowing(&mut self, p: &Pattern) {
        let mut ids = Vec::new();
        p.bindings(&mut ids);
        for id in &ids {
            let Some(ty) = self.binding_type(id) else { continue };
            let prior = self.in_scope.get(&id.name).and_then(|types| types.last()).cloned();
            if let Some(prev_ty) = prior
                && prev_ty != ty
            {
                self.warn(
                    id.span.lo,
                    "shadowed-variable",
                    format!("`{}` shadows a binding of a different type ({prev_ty} vs {ty})", id.name),
                );
            }
        }
    }

    fn unreachable(&mut self, stmts: &[&Stmt]) {
        for w in stmts.windows(2) {
            if let StmtKind::Expr(e) = &w[0].kind
                && diverges(self.budget, e)
            {
                self.warn(w[1].span.lo, "unreachable-code", "unreachable code".into());
                break;
            }
        }
    }

    fn block(&mut self, b: &Block) {
        self.push_scope();
        for s in &b.stmts {
            self.stmt(s, false);
        }
        let refs: Vec<&Stmt> = b.stmts.iter().collect();
        self.unreachable(&refs);
        self.pop_scope();
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
        if !self.budget.charge(1) {
            return;
        }
        match &s.kind {
            StmtKind::Let { pattern, value, else_block, mutable, .. } => {
                self.expr(value);
                if let Some(b) = else_block {
                    self.block(b);
                }
                self.pattern_names(pattern, top && !mutable);
                self.check_shadowing(pattern);
                self.bind_pattern(pattern);
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
            StmtKind::Expr(e) | StmtKind::Defer(e) => self.expr(e),
            StmtKind::Fn(f) => self.fn_decl(f),
        }
    }

    fn cond(&mut self, c: &Cond) {
        match c {
            Cond::Expr(e) => self.expr(e),
            Cond::Let(p, e) => {
                self.pattern_names_in(p, false, false);
                self.expr(e);
                self.bind_pattern(p);
            }
        }
    }

    fn expr(&mut self, e: &Expr) {
        if !self.budget.charge(1) {
            return;
        }
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
                // `if let` bindings are in scope in the `then` block only.
                self.push_scope();
                self.cond(cond);
                self.empty_block(then);
                self.block(then);
                self.pop_scope();
                if let Some(x) = else_ {
                    if let ExprKind::Block(b) = &x.kind {
                        self.empty_block(b);
                    }
                    self.expr(x);
                }
            }
            ExprKind::While { label, cond, body } => {
                self.label(label.as_ref(), body);
                self.push_scope();
                self.cond(cond);
                self.empty_block(body);
                self.block(body);
                self.pop_scope();
            }
            ExprKind::Loop { label, body } => {
                self.label(label.as_ref(), body);
                self.empty_block(body);
                self.block(body);
            }
            ExprKind::For { label, pattern, iter, body } => {
                self.label(label.as_ref(), body);
                self.pattern_names(pattern, false);
                self.expr(iter);
                self.push_scope();
                self.bind_pattern(pattern);
                self.empty_block(body);
                self.block(body);
                self.pop_scope();
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Match { scrutinee, arms, .. } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pattern_names_in(&a.pattern, false, false);
                    self.push_scope();
                    self.bind_pattern(&a.pattern);
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    self.expr(&a.body);
                    self.pop_scope();
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                for p in params {
                    self.name(&p.name, is_snake(&p.name.name), "parameter", "snake_case");
                }
                self.push_scope();
                for p in params {
                    self.bind(&p.name);
                }
                self.expr(body);
                self.pop_scope();
            }
            ExprKind::Str(segs) => {
                for s in segs {
                    if let StrSeg::Expr(x, spec) = s {
                        // `"[0-9]{4}"` interpolates `4`: almost always meant as literal braces.
                        if spec.is_none() && matches!(x.kind, ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Duration(_)) {
                            self.warn(
                                x.span.lo,
                                "interpolated-literal",
                                "interpolating a literal; for literal braces use a raw string (`r\"...\"`) or `\\{`".into(),
                            );
                        }
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
                self.push_scope();
                self.clauses(clauses);
                self.expr(x);
                self.pop_scope();
            }
            ExprKind::MapComp(k, v, clauses) => {
                self.push_scope();
                self.clauses(clauses);
                self.expr(k);
                self.expr(v);
                self.pop_scope();
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
            ExprKind::Return(Some(x)) | ExprKind::Break { value: Some(x), .. } => self.expr(x),
            _ => {}
        }
    }

    /// Lints a loop label: its name, and whether anything refers to it.
    fn label(&mut self, label: Option<&Ident>, body: &Block) {
        let Some(l) = label else { return };
        self.name(l, is_snake(&l.name), "label", "snake_case");
        if !block_any(body, &mut |x| uses_label(self.budget, x, &l.name)) {
            self.warn(l.span.lo, "unused-label", format!("label `@{}` is never used", l.name));
        }
    }

    fn clauses(&mut self, cs: &[CompClause]) {
        for c in cs {
            match c {
                CompClause::For(p, e) => {
                    self.pattern_names(p, false);
                    self.expr(e);
                    self.bind_pattern(p);
                }
                CompClause::If(e) => self.expr(e),
            }
        }
    }

    // ------------------------------------------------------------------
    // Lints from checker facts

    fn fact_lints(&mut self) {
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
        for (line, col, name, is_method) in &facts.unused_fns {
            let what = if *is_method { "method" } else { "function" };
            self.out.push(LintWarning {
                code: "unused-function",
                message: format!("{what} `{name}` is never used"),
                line: *line,
                column: *col,
            });
        }
    }
}

/// Private functions and methods of the main module that are unreachable from
/// its public API, top-level code and tests (`unused-function`).
///
/// References are the resolved function ids in the checked HIR: direct calls
/// (including pipes and method calls), function values and default-argument
/// helpers. Reachability (rather than "referenced anywhere") means a function
/// used only by itself or by other unused functions is unused too.
pub(crate) fn unused_functions(checked: &CheckedProgram) -> Vec<FnId> {
    let main = checked.main_module;
    let n = checked.fns.len();
    let is_root = |f: &FnInfo| -> bool {
        // Trait methods are reached through dictionaries, not static references;
        // their names are dictated by the trait.
        if f.module != main || f.impl_of.is_some() || f.trait_of.is_some() {
            return true;
        }
        match f.kind {
            FnKind::Init | FnKind::Test(_) | FnKind::Native(_) | FnKind::TraitMethod(..) => true,
            FnKind::DefaultHelper => false,
            FnKind::Script => match f.owner {
                // Methods are public exactly when their type is.
                Some(Ctor::Def(d)) => checked.types.get(d as usize).is_none_or(|t| t.is_pub),
                Some(_) => true,
                None => f.is_pub,
            },
        }
    };
    let mut reached = vec![false; n];
    let mut stack: Vec<FnId> = Vec::new();
    for (i, f) in checked.fns.iter().enumerate() {
        if is_root(f) {
            reached[i] = true;
            stack.push(i as FnId);
        }
    }
    while let Some(id) = stack.pop() {
        let Some(Some(body)) = checked.bodies.get(id as usize) else { continue };
        let mut refs: Vec<FnId> = Vec::new();
        crate::compile::walk(&body.body, &mut |e| match &e.kind {
            HKind::FnValue(f) => refs.push(*f),
            HKind::Call { func, args } => {
                refs.push(*func);
                refs.extend(args.iter().filter_map(|a| if let HArg::Default { helper, .. } = a { Some(*helper) } else { None }));
            }
            HKind::CallNative { args, .. } => {
                refs.extend(args.iter().filter_map(|a| if let HArg::Default { helper, .. } = a { Some(*helper) } else { None }));
            }
            _ => {}
        });
        for r in refs {
            if let Some(seen) = reached.get_mut(r as usize)
                && !*seen
            {
                *seen = true;
                stack.push(r);
            }
        }
    }
    checked
        .fns
        .iter()
        .enumerate()
        .filter(|(i, f)| !reached[*i] && f.module == main && matches!(f.kind, FnKind::Script))
        .map(|(i, _)| i as FnId)
        .collect()
}
