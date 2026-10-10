//! The linter: style and correctness warnings on top of the type checker.

use crate::check::budget::{CompileBudget, tool_fuel};
use crate::check::{CheckedProgram, Ctor, FnInfo, FnKind};
use crate::diag::SourceFile;
use crate::engine::Engine;
use crate::hir::{HArg, HKind};
use crate::modules::ResolvedFacts;
use crate::syntax::ast::*;
use crate::syntax::lexer::CommentKind;
use crate::types::FnId;
use crate::{Config, Error, Policy};
use std::collections::{HashMap, HashSet};
#[cfg(feature = "os")]
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

/// Lints a file of the host's file system, loading its imports as `sand
/// check` does.
#[cfg(feature = "os")]
pub fn lint_file(path: &Path) -> Result<Vec<LintWarning>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let engine = Engine::new(Config::new().policy(Policy::deny_all()));
    let facts = engine.lint_facts(path).map_err(|e| e.to_string())?;
    lint_with(&src, facts)
}

/// Lints source text, resolving imports against the current directory.
pub fn lint_source(name: &str, src: &str) -> Result<Vec<LintWarning>, String> {
    lint_source_with(&Engine::new(Config::new().policy(Policy::deny_all())), name, src)
}

/// Lints source text as `engine` would run it: imports come from its file
/// system and module root, as its policy allows (without prompting).
pub fn lint_source_with(engine: &Engine, name: &str, src: &str) -> Result<Vec<LintWarning>, String> {
    check_and_lint(engine, name, src).map_err(|e| e.to_string())
}

/// Like [`lint_source_with`], but returns compile errors as [`Error::Compile`].
/// A lint that cannot finish is an [`Error::Io`].
pub fn check_and_lint(engine: &Engine, name: &str, src: &str) -> Result<Vec<LintWarning>, Error> {
    let facts = engine.lint_facts_source(name, src)?;
    lint_with(src, facts).map_err(Error::Io)
}

fn lint_with(src: &str, facts: ResolvedFacts) -> Result<Vec<LintWarning>, String> {
    let src = src.to_string();
    let budget = CompileBudget::new(Some(tool_fuel(src.len())), None, None);
    let out = crate::platform::with_stack(64 << 20, || run_lints(&src, &facts, &budget))
        .map_err(|e| format!("internal linter error: {e}"))
        .and_then(|r| r);
    match budget.stopped() {
        Some(stop) => Err(crate::fmt::stop_message(stop, "lint")),
        None => out,
    }
}

struct Linter<'a> {
    file: SourceFile,
    out: Vec<LintWarning>,
    facts: &'a ResolvedFacts,
    /// Types of bindings by (line, column), from the checker (empty for
    /// top-level bindings).
    types: HashMap<(usize, usize), String>,
    /// The payload-less variants of the enum type of capitalized bindings
    /// by (line, column), from the checker (`variant-like-binding`).
    variants: HashMap<(usize, usize), Vec<String>>,
    /// The names of the bindings (with known types) of each scope enclosing
    /// the current point of the current function, innermost last.
    let_scopes: Vec<Vec<String>>,
    /// The types of the bindings in `let_scopes` by name, innermost last.
    in_scope: HashMap<String, Vec<String>>,
    /// Names that may shadow the prelude's (see [`declarations`]).
    declared: HashSet<String>,
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
    let declared = declarations(&parsed.module);
    let mut l = Linter {
        file: SourceFile::new("", src),
        out: Vec::new(),
        facts,
        types: facts.locals.iter().map(|(ln, c, f)| ((*ln, *c), f.ty.clone())).collect(),
        variants: facts.locals.iter().filter(|(.., f)| !f.variants.is_empty()).map(|(ln, c, f)| ((*ln, *c), f.variants.clone())).collect(),
        let_scopes: vec![vec![]],
        in_scope: HashMap::new(),
        declared,
        budget,
    };
    l.module(&parsed.module);
    l.fact_lints();

    let mut file_allow: HashSet<String> = HashSet::new();
    let mut line_allow: HashMap<usize, HashSet<String>> = HashMap::new();
    for c in &parsed.comments {
        let text = match c.kind {
            CommentKind::ModuleDoc => c.text(src).trim_start_matches("//!").trim(),
            CommentKind::Line => c.text(src).trim_start_matches("//").trim(),
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

/// The edit distance between `a` and `b` (insertions, deletions,
/// substitutions and transpositions of adjacent characters).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    // Rows i - 2, i - 1 and i of the distances between prefixes.
    let mut rows = [vec![0; b.len() + 1], vec![0; b.len() + 1], (0..=b.len()).collect::<Vec<_>>()];
    for i in 1..=a.len() {
        rows.rotate_left(1);
        rows[2][0] = i;
        for j in 1..=b.len() {
            let cost = (a[i - 1] != b[j - 1]) as usize;
            let mut d = (rows[1][j] + 1).min(rows[2][j - 1] + 1).min(rows[1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(rows[0][j - 2] + 1);
            }
            rows[2][j] = d;
        }
    }
    rows[2][b.len()]
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

/// The names of the functions (at any depth), imports and top-level
/// bindings of a module, which may shadow the prelude's.
fn declarations(m: &Module) -> HashSet<String> {
    fn function(f: &FnDecl, out: &mut HashSet<String>) {
        out.insert(f.name.name.clone());
        for s in f.body.iter().flat_map(|b| &b.stmts) {
            if let StmtKind::Fn(f) = &s.kind {
                function(f, out);
            }
        }
    }
    let mut out = HashSet::new();
    for item in &m.items {
        match &item.kind {
            ItemKind::Fn(f) => function(f, &mut out),
            ItemKind::Import(i) => out.extend(i.alias.iter().chain(i.names.iter().flatten()).map(|n| n.name.clone())),
            ItemKind::Stmt(Stmt { kind: StmtKind::Let { pattern, .. }, .. }) => {
                let mut ids = Vec::new();
                pattern.bindings(&mut ids);
                out.extend(ids.into_iter().map(|id| id.name));
            }
            _ => {}
        }
    }
    out
}

/// An expression statement that computes a value and does nothing else.
fn has_no_effect(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Unit
        | ExprKind::Bool(_)
        | ExprKind::Int(_)
        | ExprKind::IntOverflow(_)
        | ExprKind::Float(_)
        | ExprKind::Duration(_)
        | ExprKind::Bytes(_)
        | ExprKind::Ident(_)
        | ExprKind::SelfValue
        | ExprKind::Lambda { .. } => true,
        ExprKind::Str(segs) => segs.iter().all(|s| matches!(s, StrSeg::Text(_))),
        ExprKind::Paren(x) | ExprKind::Unary(_, x) | ExprKind::Field(x, _) | ExprKind::TupleField(x, ..) => has_no_effect(x),
        ExprKind::Binary(_, a, b) => has_no_effect(a) && has_no_effect(b),
        ExprKind::Range { start, end, .. } => start.iter().chain(end).all(|x| has_no_effect(x)),
        ExprKind::List(xs) | ExprKind::Tuple(xs) => xs.iter().all(has_no_effect),
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
        StmtKind::Expr(x) | StmtKind::Defer(x) | StmtKind::Use { value: x, .. } => f(x),
        StmtKind::Fn(_) => false,
    })
}

fn clauses_any(cs: &[CompClause], f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    cs.iter().any(|c| match c {
        CompClause::For(_, x) | CompClause::If(x) => f(x),
    })
}

/// Whether `e` contains a `break` leaving the loop labeled `label` that is
/// `depth` loops out (labels and loops do not reach into lambdas).
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

/// Whether `e` contains a `break @name` or `continue @name` (outside lambdas).
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

    fn warn(&mut self, pos: u32, code: &'static str, message: String) {
        let (line, column) = self.file.line_col(pos);
        self.out.push(LintWarning { code, message, line, column });
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
        let stmts: Vec<&Stmt> = m.items.iter().filter_map(|i| if let ItemKind::Stmt(s) = &i.kind { Some(s) } else { None }).collect();
        self.unreachable(&stmts);
        self.no_effect(&stmts, None);
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
        self.types.get(&self.file.line_col(id.span.lo)).filter(|t| !t.is_empty()).cloned()
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

    /// Whether `name` is the prelude's function of that name (nothing in
    /// the module or in scope shadows it).
    fn is_prelude(&self, name: &str) -> bool {
        !self.declared.contains(name) && !self.in_scope.contains_key(name)
    }

    /// Whether evaluating `e` never completes.
    fn diverges(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue(_) => true,
            ExprKind::Call { callee, .. } => {
                matches!(&callee.kind, ExprKind::Ident(id) if matches!(id.name.as_str(), "panic" | "exit" | "todo") && self.is_prelude(&id.name))
            }
            // A `loop` that nothing breaks out of never completes.
            ExprKind::Loop { label, body } => !block_any(body, &mut |x| breaks_to(self.budget, x, label.as_ref(), 0)),
            _ => false,
        }
    }

    fn unreachable(&mut self, stmts: &[&Stmt]) {
        for w in stmts.windows(2) {
            if let StmtKind::Expr(e) = &w[0].kind
                && self.diverges(e)
            {
                self.warn(w[1].span.lo, "unreachable-code", "unreachable code".into());
                break;
            }
        }
    }

    /// `no-effect`: expression statements whose value is discarded and that
    /// do nothing else (`value` is the statement whose value is used, if any).
    fn no_effect(&mut self, stmts: &[&Stmt], value: Option<&Stmt>) {
        for s in stmts {
            if let StmtKind::Expr(e) = &s.kind
                && value.is_none_or(|v| !std::ptr::eq(v, *s))
                && has_no_effect(e)
            {
                let message = if matches!(e.kind, ExprKind::Unary(UnOp::Neg, _)) {
                    "expression statement has no effect (a line starting with `-x` starts a new statement; to subtract \
                     on a continued line, write `- x`)"
                } else {
                    "expression statement has no effect"
                };
                self.warn(s.span.lo, "no-effect", message.into());
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
        // The last statement is the block's value.
        self.no_effect(&refs, b.stmts.last());
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

    /// The naming lints of the bindings of a pattern. A capitalized name
    /// that the checker found to bind (rather than to be a variant) is a
    /// `variant-like-binding`, unless it is a top-level constant
    /// (`SCREAMING_SNAKE_CASE`). `is_let` marks a `let`, `for` or
    /// comprehension pattern, where facts may be missing for a binding.
    fn pattern_names_in(&mut self, p: &Pattern, top_level_let: bool, is_let: bool) {
        let mut ids = Vec::new();
        p.bindings(&mut ids);
        for id in ids {
            let constant = top_level_let && is_screaming(&id.name);
            if id.name.starts_with(|c: char| c.is_ascii_uppercase()) && !constant {
                if self.types.contains_key(&self.file.line_col(id.span.lo)) {
                    self.variant_like_binding(&id);
                    continue;
                }
                if !is_let {
                    // A variant.
                    continue;
                }
            }
            self.name(&id, is_snake(&id.name) || constant, "variable", "snake_case");
        }
    }

    /// `variant-like-binding`: a capitalized name in a pattern that binds a
    /// new variable, most likely a misspelled variant without a payload.
    fn variant_like_binding(&mut self, id: &Ident) {
        let name = id.name.to_ascii_lowercase();
        let close = self.variants.get(&self.file.line_col(id.span.lo)).and_then(|variants| {
            variants
                .iter()
                .map(|v| (edit_distance(&name, &v.rsplit('.').next().unwrap_or(v).to_ascii_lowercase()), v))
                .filter(|&(d, _)| d <= (name.chars().count() / 3).max(1))
                .min_by_key(|&(d, _)| d)
                .map(|(_, v)| v.clone())
        });
        let message = match close {
            Some(v) => format!("`{}` binds a new variable; did you mean the variant `{v}`?", id.name),
            None => format!("`{}` binds a new variable rather than matching a variant; use a lowercase name for a binding", id.name),
        };
        self.warn(id.span.lo, "variant-like-binding", message);
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
            StmtKind::Use { name, value, .. } => {
                self.expr(value);
                if name.name != "_" {
                    let p = Pattern { kind: PatternKind::Binding(name.clone()), span: name.span };
                    self.pattern_names(&p, false);
                    self.check_shadowing(&p);
                    self.bind_pattern(&p);
                }
            }
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
                    && self.is_prelude("todo")
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
        for (line, col, code, message) in &facts.resources {
            self.out.push(LintWarning { code, message: message.clone(), line: *line, column: *col });
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
        let Some(body) = checked.bodies.get(id) else { continue };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("emtpy", "empty"), 1);
        assert_eq!(edit_distance("fulll", "full"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn variant_like_bindings_suggest_the_closest_variant() {
        let src = "enum State {\n    Empty\n    Closed\n    Full(int)\n}\nfn f(s: State) -> int {\n    match s {\n        Full(n) => n\n        Closed => 1\n        Emtpy => 0\n    }\n}\nfn g(s: State) -> int {\n    match s {\n        Full(n) => n\n        Rest => 1\n    }\n}\nprint(f(State.Empty) + g(State.Empty))\n";
        let messages: Vec<String> =
            lint_source("main.sand", src).unwrap().into_iter().filter(|w| w.code == "variant-like-binding").map(|w| w.message).collect();
        assert_eq!(
            messages,
            [
                "`Emtpy` binds a new variable; did you mean the variant `State.Empty`?",
                "`Rest` binds a new variable rather than matching a variant; use a lowercase name for a binding",
            ]
        );
    }
}
