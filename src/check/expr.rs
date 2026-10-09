//! Type checking of statements and expressions.

use super::*;
use crate::syntax::ast::{Arg, AssignOp, BinOp, Block, CompClause, Cond, Expr, ExprKind, Stmt, StmtKind, StrSeg, UnOp};

/// What a name or path in expression position refers to as a namespace.
#[derive(Clone, Debug)]
pub(crate) enum Ns {
    Module(ModId),
    Type(Type),
    Trait(TraitId),
}

struct CallArgs<'e> {
    positional: Vec<&'e Expr>,
    named: Vec<(&'e ast::Ident, &'e Expr)>,
}

impl<'e> CallArgs<'e> {
    fn from(args: &'e [Arg]) -> CallArgs<'e> {
        let mut positional = Vec::new();
        let mut named = Vec::new();
        for a in args {
            match &a.name {
                Some(n) => named.push((n, &a.value)),
                None => positional.push(&a.value),
            }
        }
        CallArgs { positional, named }
    }
}

/// How a `for` loop or comprehension clause iterates.
pub(crate) enum IterPlan {
    /// Ranges, lists, maps, sets and channels, with dedicated instructions.
    Builtin(IterKind),
    /// An `Iterator`: the value is kept in a hidden local, and `next` (a call
    /// of type `T?` on it) runs before each iteration.
    Lazy { local: LocalId, next: Box<HExpr> },
}

/// An instantiated function signature.
pub(crate) struct Instance {
    pub fresh: Vec<Type>,
    pub params: Vec<Type>,
    pub ret: Type,
    /// Evidence needed for the hidden dictionary parameters.
    pub dicts: Vec<(Type, TraitRef)>,
}

pub(crate) fn mk(kind: HKind, ty: Type, span: Span) -> HExpr {
    HExpr { kind, ty, span }
}

fn unit(span: Span) -> HExpr {
    mk(HKind::Unit, Type::Unit, span)
}

fn err(span: Span) -> HExpr {
    mk(HKind::Error, Type::Error, span)
}

impl Checker<'_> {
    // ------------------------------------------------------------------
    // Entry points

    /// Checks `e` against `expected`, reporting a mismatch.
    pub(crate) fn check_expr(&mut self, e: &Expr, expected: &Type) -> HExpr {
        let h = self.infer_inner(e, Some(expected), true);
        if !self.unify(expected, &h.ty) {
            let (want, got) = self.show_pair(expected, &h.ty);
            let msg = format!("type mismatch: expected {want}, found {got}");
            self.error(e.span, msg);
        }
        h
    }

    pub(crate) fn infer(&mut self, e: &Expr, hint: Option<&Type>) -> HExpr {
        self.infer_inner(e, hint, true)
    }

    pub(crate) fn check_block(&mut self, b: &Block, hint: Option<&Type>, want: bool) -> HExpr {
        // Bindings inside nested blocks are locals, even at module level.
        let saved_top = std::mem::replace(&mut self.fs.top_level, false);
        let h = self.check_block_inner(b, hint, want);
        self.fs.top_level = saved_top;
        h
    }

    fn check_block_inner(&mut self, b: &Block, hint: Option<&Type>, want: bool) -> HExpr {
        self.push_scope();
        let mut out = Vec::new();
        let n = b.stmts.len();
        let mut ty = Type::Unit;
        let mut diverged = false;
        let mut i = 0;
        while i < n {
            let s = &b.stmts[i];
            // Groups of consecutive local functions may call each other.
            if let StmtKind::Fn(_) = s.kind {
                let mut group = Vec::new();
                while i < n {
                    if let StmtKind::Fn(f) = &b.stmts[i].kind {
                        group.push(f);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(self.check_local_fns(&group));
                ty = Type::Unit;
                continue;
            }
            let last = i + 1 == n;
            if last
                && want
                && let StmtKind::Expr(e) = &s.kind
            {
                let h = self.infer_inner(e, hint, true);
                // A unit-typed statement after a diverging one is not a value.
                ty = if diverged && self.resolve(&h.ty) == Type::Unit { Type::Never } else { h.ty.clone() };
                out.push(h);
                i += 1;
                continue;
            }
            let hs = self.check_stmt(s, false);
            if let Some(h) = hs.last() {
                if self.resolve(&h.ty).is_never() {
                    diverged = true;
                }
                ty = if h.ty.is_never() || diverged { Type::Never } else { Type::Unit };
            }
            out.extend(hs);
            i += 1;
        }
        self.pop_scope();
        if !want {
            // The value of the block is discarded: make it unit.
            if !matches!(out.last().map(|h| self.resolve(&h.ty)), Some(Type::Unit) | Some(Type::Never) | None) {
                out.push(unit(b.span));
            }
            if !ty.is_never() {
                ty = Type::Unit;
            }
        }
        mk(HKind::Block(out), ty, b.span)
    }

    /// Checks a statement; returns HIR statements.
    pub(crate) fn check_stmt(&mut self, s: &Stmt, _want: bool) -> Vec<HExpr> {
        match &s.kind {
            StmtKind::Expr(e) => {
                let h = self.infer_inner(e, None, false);
                if !matches!(
                    e.kind,
                    ExprKind::If { .. }
                        | ExprKind::Match { .. }
                        | ExprKind::Block(_)
                        | ExprKind::Loop { .. }
                        | ExprKind::While { .. }
                        | ExprKind::For { .. }
                ) {
                    self.fs_result_stmts_push(h.ty.clone(), e.span);
                }
                vec![h]
            }
            StmtKind::Let { pattern, ty, value, else_block, mutable, is_pub, .. } => {
                self.check_let(s, pattern, ty.as_ref(), value, else_block.as_ref(), *mutable, *is_pub)
            }
            StmtKind::Assign { target, op, value } => {
                vec![self.check_assign(target, *op, value, s.span)]
            }
            StmtKind::Fn(f) => vec![self.check_local_fns(&[f])],
            StmtKind::Defer(e) => vec![self.check_defer(e, s.span)],
        }
    }

    /// `defer e`: `e` must have type `()` and cannot leave the deferred
    /// expression (with `return`, `?`, or `break`/`continue` to an outer loop).
    fn check_defer(&mut self, e: &Expr, span: Span) -> HExpr {
        let ctx = self.fs.ctx.last_mut().unwrap();
        let saved = ctx.in_defer.replace(ctx.loops.len());
        let saved_top = std::mem::replace(&mut self.fs.top_level, false);
        let h = self.infer(e, Some(&Type::Unit));
        self.fs.top_level = saved_top;
        self.fs.ctx.last_mut().unwrap().in_defer = saved;
        let t = self.resolve(&h.ty);
        if !t.is_never() && !self.unify(&Type::Unit, &t) {
            let msg = format!("a deferred expression must have type (), found {}; discard the value with `let _ = ...`", self.show(&t));
            self.error(e.span, msg);
        }
        mk(HKind::Defer(Box::new(h)), Type::Unit, span)
    }

    /// Reports `return` or `?` inside a deferred expression.
    fn check_not_in_defer(&mut self, what: &str, span: Span) {
        if self.fs.ctx.last().is_some_and(|c| c.in_defer.is_some()) {
            self.error(span, format!("{what} cannot be used in a deferred expression"));
        }
    }

    fn fs_result_stmts_push(&mut self, t: Type, span: Span) {
        self.fs_unused.push((t, span));
    }

    fn resolve_annotation(&mut self, t: &ast::TypeExpr) -> Type {
        let generics = self.fs.generics.clone();
        let self_ty = self.fs.self_ty.clone();
        self.resolve_type_in(t, &generics, self_ty.as_ref())
    }

    #[allow(clippy::too_many_arguments)]
    fn check_let(
        &mut self,
        s: &Stmt,
        pattern: &ast::Pattern,
        ty: Option<&ast::TypeExpr>,
        value: &Expr,
        else_block: Option<&Block>,
        mutable: bool,
        is_pub: bool,
    ) -> Vec<HExpr> {
        let annotated = ty.map(|t| self.resolve_annotation(t));
        let init = match &annotated {
            Some(t) => self.check_expr(value, t),
            None => self.infer(value, None),
        };
        let vty = annotated.clone().unwrap_or_else(|| init.ty.clone());
        // Top-level bindings only need global slots if functions (or other
        // modules, or later session inputs) can see them; otherwise they are
        // initialiser locals.
        let mut bound = Vec::new();
        pattern.bindings(&mut bound);
        let needs_global = is_pub || self.in_session() || bound.iter().any(|b| self.fn_names.contains(&b.name));
        let top = self.fs.top_level && needs_global;
        if is_pub && !self.fs.top_level {
            self.error(s.span, "`pub` is only allowed at the top level");
        }
        // Module-level simple bindings become globals.
        if top
            && let ast::PatternKind::Binding(id) = &pattern.kind
            && else_block.is_none()
        {
            let g = self.globals.len() as GlobalId;
            self.globals.push(GlobalInfo { name: id.name.clone(), module: self.cur_mod, ty: vty.clone(), mutable, is_pub, span: id.span });
            self.define(&id.name, Def::Global(g), is_pub, id.span);
            self.lint.locals.push(LocalFact {
                span: id.span,
                name: id.name.clone(),
                used: is_pub,
                mutable,
                assigned: false,
                ty: String::new(),
                is_param: false,
                global: true,
                func: u32::MAX,
            });
            self.global_facts.push((g, self.lint.locals.len() - 1));
            return vec![mk(
                HKind::Assign { place: Place::Global(g), op: None, op_ty: Type::Unit, value: Box::new(init) },
                Type::Unit,
                s.span,
            )];
        }
        let hp = self.check_pattern(pattern, &vty, mutable);
        let refutable = !self.pattern_irrefutable(&hp);
        let else_h = match else_block {
            Some(b) => {
                // Bindings are not visible in the else block.
                let names = self.pattern_names(&hp);
                let saved: Vec<(String, LocalId)> = self.fs.scopes.last().unwrap().clone();
                self.fs.scopes.last_mut().unwrap().retain(|(n, _)| !names.contains(n));
                let h = self.check_block(b, None, false);
                *self.fs.scopes.last_mut().unwrap() = saved;
                if !self.resolve(&h.ty).is_never() {
                    self.error(b.span, "the `else` block of `let ... else` must diverge (return, break, continue or panic)");
                }
                Some(Box::new(h))
            }
            None => {
                if refutable {
                    self.error(pattern.span, "refutable pattern in `let`; use `let ... else` or `match`");
                }
                None
            }
        };
        let mut out = vec![mk(HKind::Let { pat: hp.clone(), init: Box::new(init), else_: else_h }, Type::Unit, s.span)];
        if top {
            // Bindings of a top-level destructuring pattern become globals.
            let bound = self.pattern_bindings(&hp);
            let names: Vec<String> = bound.iter().map(|(n, _)| n.clone()).collect();
            self.fs.scopes.last_mut().unwrap().retain(|(n, _)| !names.contains(n));
            for (name, l) in bound {
                let lty = self.fs.data[l as usize].ty.clone();
                let g = self.globals.len() as GlobalId;
                let span = self.fs.locals[l as usize].span;
                self.globals.push(GlobalInfo { name: name.clone(), module: self.cur_mod, ty: lty.clone(), mutable, is_pub, span });
                self.define(&name, Def::Global(g), is_pub, span);
                self.fs.data[l as usize].used = true;
                self.lint.locals.push(LocalFact {
                    span,
                    name: name.clone(),
                    used: is_pub,
                    mutable,
                    assigned: false,
                    ty: String::new(),
                    is_param: false,
                    global: true,
                    func: u32::MAX,
                });
                self.global_facts.push((g, self.lint.locals.len() - 1));
                out.push(mk(
                    HKind::Assign { place: Place::Global(g), op: None, op_ty: Type::Unit, value: Box::new(mk(HKind::Local(l), lty, span)) },
                    Type::Unit,
                    span,
                ));
            }
        } else {
            for (_, l) in self.pattern_bindings(&hp) {
                let t = self.fs.data[l as usize].ty.clone();
                let span = self.fs.locals[l as usize].span;
                self.fs.infer_checks.push((t, span));
            }
        }
        out
    }

    fn check_assign(&mut self, target: &Expr, op: AssignOp, value: &Expr, span: Span) -> HExpr {
        let (place, tty) = match &target.kind {
            ExprKind::Ident(id) => {
                if let Some(l) = self.find_local(&id.name) {
                    self.use_local_for_assign(l);
                    if !self.fs.locals[l as usize].mutable {
                        self.error(target.span, format!("cannot assign to immutable binding `{}`", id.name));
                    }
                    (Place::Local(l), self.fs.data[l as usize].ty.clone())
                } else {
                    match self.lookup_global(&id.name) {
                        Some(Def::Global(g)) => {
                            let gi = self.globals[g as usize].clone();
                            if !gi.mutable {
                                self.error(target.span, format!("cannot assign to immutable binding `{}`", id.name));
                            } else if gi.module != self.cur_mod {
                                self.error(target.span, format!("cannot assign to `{}` from another module", id.name));
                            }
                            self.mark_global_assigned(g);
                            (Place::Global(g), gi.ty.clone())
                        }
                        Some(_) => {
                            self.error(target.span, format!("cannot assign to `{}`", id.name));
                            (Place::Local(0), Type::Error)
                        }
                        None => {
                            self.error(target.span, format!("undefined name `{}`", id.name));
                            (Place::Local(0), Type::Error)
                        }
                    }
                }
            }
            ExprKind::Field(obj, name) => {
                if let Some(Ns::Module(m)) = self.resolve_ns(obj) {
                    if let Some(Def::Global(g)) = self.module_member(m, &name.name, name.span) {
                        let ty = self.globals[g as usize].ty.clone();
                        self.error(target.span, format!("cannot assign to `{}` from another module", name.name));
                        (Place::Global(g), ty)
                    } else {
                        self.error(target.span, "cannot assign to this expression");
                        (Place::Local(0), Type::Error)
                    }
                } else {
                    let o = self.infer(obj, None);
                    let ot = self.resolve(&o.ty);
                    match self.struct_field(&ot, &name.name) {
                        Some((idx, fty, mutable)) => {
                            if !mutable {
                                self.error(name.span, format!("cannot assign to immutable field `{}`", name.name));
                            }
                            (Place::Field { obj: Box::new(o), index: idx }, fty)
                        }
                        None => {
                            if !matches!(ot, Type::Error) {
                                let msg = if matches!(ot, Type::Var(_)) {
                                    "type annotation needed before field access".to_string()
                                } else {
                                    format!("no field `{}` on type {}", name.name, self.show(&ot))
                                };
                                self.error(name.span, msg);
                            }
                            (Place::Local(0), Type::Error)
                        }
                    }
                }
            }
            ExprKind::Index(obj, idx) => {
                let o = self.infer(obj, None);
                match self.resolve(&o.ty) {
                    Type::List(t) => {
                        let i = self.check_expr(idx, &Type::Int);
                        (Place::Index { obj: Box::new(o), index: Box::new(i), map: false }, *t)
                    }
                    Type::Map(k, v) => {
                        let i = self.check_expr(idx, &k);
                        (Place::Index { obj: Box::new(o), index: Box::new(i), map: true }, *v)
                    }
                    Type::Error => (Place::Local(0), Type::Error),
                    Type::Var(_) => {
                        self.error(obj.span, "type annotation needed before indexing");
                        (Place::Local(0), Type::Error)
                    }
                    other => {
                        let msg = format!("cannot assign to an element of {}", self.show(&other));
                        self.error(target.span, msg);
                        (Place::Local(0), Type::Error)
                    }
                }
            }
            ExprKind::TupleField(..) => {
                self.error(target.span, "cannot assign to a tuple element; tuples are immutable");
                (Place::Local(0), Type::Error)
            }
            _ => {
                self.error(target.span, "cannot assign to this expression");
                (Place::Local(0), Type::Error)
            }
        };
        let arith = match op {
            AssignOp::Assign => None,
            AssignOp::Add => Some(ArithOp::Add),
            AssignOp::Sub => Some(ArithOp::Sub),
            AssignOp::Mul => Some(ArithOp::Mul),
            AssignOp::Div => Some(ArithOp::Div),
            AssignOp::Rem => Some(ArithOp::Rem),
        };
        let v = match arith {
            None => self.check_expr(value, &tty),
            Some(aop) => {
                let r = self.infer(value, Some(&tty));
                let res = self.arith_type(aop, &tty, &r.ty, span);
                self.expect_type(&tty, &res, span);
                r
            }
        };
        mk(HKind::Assign { place, op: arith, op_ty: tty, value: Box::new(v) }, Type::Unit, span)
    }

    fn use_local_for_assign(&mut self, l: LocalId) {
        self.fs.data[l as usize].assigned = true;
        let cur = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
        let info = &mut self.fs.locals[l as usize];
        if info.owner != cur && info.mutable {
            info.cell = true;
        }
    }

    fn mark_global_assigned(&mut self, g: GlobalId) {
        if let Some((_, idx)) = self.global_facts.iter().find(|(x, _)| *x == g) {
            let idx = *idx;
            self.lint.locals[idx].assigned = true;
        }
    }

    fn mark_global_used(&mut self, g: GlobalId) {
        if let Some((_, idx)) = self.global_facts.iter().find(|(x, _)| *x == g) {
            let idx = *idx;
            self.lint.locals[idx].used = true;
        }
    }

    // ------------------------------------------------------------------
    // Local functions

    fn check_local_fns(&mut self, group: &[&ast::FnDecl]) -> HExpr {
        let mut locals = Vec::new();
        let mut sigs = Vec::new();
        for f in group {
            let mut scope = self.fs.generics.clone();
            let self_ty = self.fs.self_ty.clone();
            let own = self.declare_params(&f.generics, &scope, self_ty.as_ref());
            for (g, &p) in f.generics.iter().zip(&own) {
                scope.push((g.name.name.clone(), p));
                if !self.params[p as usize].traits.is_empty() {
                    self.error(g.name.span, "local functions cannot have trait bounds; declare the function at the top level");
                    self.params[p as usize].traits.clear();
                }
            }
            let mut ptys = Vec::new();
            for p in &f.params {
                let t = match &p.ty {
                    Some(t) => self.resolve_type_in(t, &scope, self_ty.as_ref()),
                    None => {
                        self.error(p.name.span, "`self` is only allowed in methods");
                        Type::Error
                    }
                };
                if p.default.is_some() {
                    self.error(p.name.span, "local functions cannot have default parameters");
                }
                ptys.push(t);
            }
            let ret = f.ret.as_ref().map(|t| self.resolve_type_in(t, &scope, self_ty.as_ref())).unwrap_or(Type::Unit);
            let fty = Type::func(ptys.clone(), ret.clone());
            let l = self.declare_local(&f.name.name, fty.clone(), false, f.name.span, false);
            self.fs.locals[l as usize].cell = true;
            self.fs.data[l as usize].scheme = own.clone();
            self.fs.data[l as usize].used = true;
            locals.push(l);
            sigs.push((ptys, ret, scope, fty));
        }
        let mut lambdas = Vec::new();
        for (f, (ptys, ret, scope, fty)) in group.iter().zip(sigs) {
            let saved_generics = std::mem::replace(&mut self.fs.generics, scope);
            self.fs.next_lambda += 1;
            let id = self.fs.next_lambda;
            self.fs.ctx.push(FnCtx { ret: ret.clone(), kind: CtxKind::Lambda, loops: vec![], lambda: id, in_defer: None });
            self.push_scope();
            let mut params = Vec::new();
            for (p, t) in f.params.iter().zip(&ptys) {
                params.push(self.declare_local(&p.name.name, t.clone(), false, p.name.span, true));
            }
            let body_ast = f.body.as_ref().unwrap();
            let want = ret != Type::Unit;
            let body = self.check_block(body_ast, if want { Some(&ret) } else { None }, want);
            if want {
                self.expect_type(&ret, &body.ty, self.block_tail_span(body_ast));
            }
            self.pop_scope();
            self.fs.ctx.pop();
            self.fs.generics = saved_generics;
            let signature =
                self.lambda_signature(&f.name.name, &f.params.iter().map(|p| p.name.name.clone()).collect::<Vec<_>>(), &ptys, &ret);
            lambdas.push(mk(
                HKind::Lambda(Box::new(HLambda {
                    id,
                    name: f.name.name.clone(),
                    params,
                    body,
                    doc: f.doc.clone(),
                    signature,
                    self_local: None,
                })),
                fty,
                f.span,
            ));
        }
        mk(HKind::LocalFns { locals, lambdas }, Type::Unit, group[0].span)
    }

    fn lambda_signature(&self, name: &str, names: &[String], ptys: &[Type], ret: &Type) -> String {
        let ps: Vec<(String, Type, bool)> = names.iter().zip(ptys).map(|(n, t)| (n.clone(), self.zonk(t), false)).collect();
        self.printer().signature(name, &ps, &self.zonk(ret))
    }

    // ------------------------------------------------------------------
    // Expressions

    /// Checks an expression within the compile budget: each expression costs
    /// fuel, and its type may not grow beyond `MAX_TYPE_SIZE` nodes.
    fn infer_inner(&mut self, e: &Expr, hint: Option<&Type>, want: bool) -> HExpr {
        if !self.budget.charge(1) {
            return err(e.span);
        }
        let mut h = self.infer_kind(e, hint, want);
        self.limit_type_size(&mut h.ty, e.span);
        h
    }

    fn infer_kind(&mut self, e: &Expr, hint: Option<&Type>, want: bool) -> HExpr {
        let span = e.span;
        match &e.kind {
            ExprKind::Error => err(span),
            ExprKind::Unit => unit(span),
            ExprKind::Bool(b) => mk(HKind::Bool(*b), Type::Bool, span),
            ExprKind::Int(i) => mk(HKind::Int(*i), Type::Int, span),
            ExprKind::IntOverflow(_) => {
                self.error(span, "integer literal out of range");
                mk(HKind::Int(0), Type::Int, span)
            }
            ExprKind::Float(f) => mk(HKind::Float(*f), Type::Float, span),
            ExprKind::Duration(d) => mk(HKind::Dur(*d), Type::Duration, span),
            ExprKind::Bytes(b) => mk(HKind::Bytes(b.clone()), Type::Bytes, span),
            ExprKind::Str(segs) => self.check_string(segs, span),
            ExprKind::Ident(id) => self.check_ident(id, hint),
            ExprKind::SelfValue => match self.find_local("self") {
                Some(l) => {
                    self.use_local(l);
                    mk(HKind::Local(l), self.fs.data[l as usize].ty.clone(), span)
                }
                None => {
                    self.error(span, "`self` is only available in methods");
                    err(span)
                }
            },
            ExprKind::SelfType => {
                self.error(span, "`Self` is a type, not a value");
                err(span)
            }
            ExprKind::Paren(inner) => self.infer_inner(inner, hint, want),
            ExprKind::List(items) => {
                let elem_hint = match hint.map(|h| self.resolve(h)) {
                    Some(Type::List(t)) => Some(*t),
                    _ => None,
                };
                let elem = self.fresh();
                let mut hs = Vec::new();
                for it in items {
                    let h = if is_lambda(it) { elem_hint.as_ref() } else { None };
                    let x = self.infer(it, h.or(Some(&elem)));
                    self.expect_type(&elem, &x.ty, it.span);
                    hs.push(x);
                }
                mk(HKind::List(hs), Type::list(elem), span)
            }
            ExprKind::Repeat(elem, count) => {
                let elem_hint = match hint.map(|h| self.resolve(h)) {
                    Some(Type::List(t)) => Some(*t),
                    _ => None,
                };
                let eh = self.infer(elem, elem_hint.as_ref());
                let ch = self.check_expr(count, &Type::Int);
                let t = Type::list(eh.ty.clone());
                mk(HKind::Repeat { elem: Box::new(eh), count: Box::new(ch) }, t, span)
            }
            ExprKind::Map(pairs) => {
                let vhint = match hint.map(|h| self.resolve(h)) {
                    Some(Type::Map(_, v)) => Some(*v),
                    _ => None,
                };
                let (kh, vh) = (self.fresh(), self.fresh());
                let mut hs = Vec::new();
                for (k, v) in pairs {
                    let kk = self.check_expr(k, &kh);
                    let h = if is_lambda(v) { vhint.as_ref() } else { None };
                    let vv = self.infer(v, h.or(Some(&vh)));
                    self.expect_type(&vh, &vv.ty, v.span);
                    hs.push((kk, vv));
                }
                if let Some((k, _)) = pairs.first() {
                    self.require_bound(&kh, Bound::Hash, k.span);
                } else {
                    self.fs.obligations.push((kh.clone(), Bound::Hash, span));
                }
                mk(HKind::Map(hs), Type::map(kh, vh), span)
            }
            ExprKind::ListComp(elem, clauses) => self.check_comprehension(Some(elem), None, clauses, hint, span),
            ExprKind::MapComp(k, v, clauses) => self.check_comprehension(Some(k), Some(v), clauses, hint, span),
            ExprKind::Tuple(items) => {
                let hints: Option<Vec<Type>> = match hint.map(|h| self.resolve(h)) {
                    Some(Type::Tuple(ts)) if ts.len() == items.len() => Some(ts),
                    _ => None,
                };
                let mut hs = Vec::new();
                for (i, it) in items.iter().enumerate() {
                    let h = hints.as_ref().map(|ts| &ts[i]);
                    hs.push(self.infer(it, h));
                }
                let t = Type::Tuple(hs.iter().map(|h| h.ty.clone()).collect());
                mk(HKind::Tuple(hs), t, span)
            }
            ExprKind::StructLit { path, fields, base } => self.check_struct_lit(path, fields, base.as_deref(), hint, span),
            ExprKind::Unary(op, inner) => self.check_unary(*op, inner, span),
            ExprKind::Binary(op, l, r) => self.check_binary(*op, l, r, span),
            ExprKind::Pipe(l, r) => self.check_pipe(l, r, hint, span),
            ExprKind::Coalesce(l, r) => {
                let lh = self.infer(l, None);
                let lt = self.resolve(&lh.ty);
                match lt {
                    Type::Named(d, args) if d == self.wk.option => {
                        let rh = self.check_expr(r, &args[0]);
                        mk(HKind::Coalesce { l: Box::new(lh), r: Box::new(rh), result: false }, args[0].clone(), span)
                    }
                    Type::Named(d, args) if d == self.wk.result => {
                        let rh = self.check_expr(r, &args[0]);
                        mk(HKind::Coalesce { l: Box::new(lh), r: Box::new(rh), result: true }, args[0].clone(), span)
                    }
                    Type::Error => err(span),
                    Type::Var(_) => {
                        self.error(l.span, "type annotation needed before `??`");
                        err(span)
                    }
                    other => {
                        let msg = format!("`??` requires an Option or Result, found {}", self.show(&other));
                        self.error(l.span, msg);
                        err(span)
                    }
                }
            }
            ExprKind::Range { start, end, inclusive } => {
                let (Some(s), Some(en)) = (start, end) else {
                    self.error(span, "open ranges are only allowed in slices");
                    return err(span);
                };
                let sh = self.check_expr(s, &Type::Int);
                let eh = self.check_expr(en, &Type::Int);
                mk(HKind::Range { start: Box::new(sh), end: Box::new(eh), inclusive: *inclusive }, Type::Range, span)
            }
            ExprKind::Call { callee, args } => self.check_call(callee, args, hint, span),
            ExprKind::MethodCall { receiver, method, args } => self.check_method_call(receiver, method, args, hint, span),
            ExprKind::Field(obj, name) => self.check_field(obj, name, hint, span),
            ExprKind::TupleField(obj, idx, ispan) => {
                let o = self.infer(obj, None);
                match self.resolve(&o.ty) {
                    Type::Tuple(ts) => match ts.get(*idx as usize) {
                        Some(t) => mk(HKind::Field { obj: Box::new(o), index: *idx }, t.clone(), span),
                        None => {
                            let msg = format!("no field `{idx}` on type {}", self.show(&Type::Tuple(ts.clone())));
                            self.error(*ispan, msg);
                            err(span)
                        }
                    },
                    Type::Error => err(span),
                    Type::Var(_) => {
                        self.error(obj.span, "type annotation needed before tuple field access");
                        err(span)
                    }
                    other => {
                        let msg = format!("no field `{idx}` on type {}", self.show(&other));
                        self.error(*ispan, msg);
                        err(span)
                    }
                }
            }
            ExprKind::Index(obj, idx) => self.check_index(obj, idx, span),
            ExprKind::Try(inner) => self.check_try(inner, span),
            ExprKind::If { cond, then, else_ } => self.check_if(cond, then, else_.as_deref(), hint, want, span),
            ExprKind::Match { scrutinee, arms, .. } => self.check_match(scrutinee, arms, hint, want, span),
            ExprKind::Block(b) => self.check_block(b, hint, want),
            ExprKind::Loop { label, body: b } => {
                let bt = self.fresh();
                self.enter_loop(label.as_ref(), Some(bt.clone()));
                let body = self.check_block(b, None, false);
                let broke = self.leave_loop();
                let ty = if broke { bt } else { Type::Never };
                mk(HKind::Loop(Box::new(body)), ty, span)
            }
            ExprKind::While { label, cond, body } => {
                self.push_scope();
                let (c, pat) = self.check_cond(cond);
                self.enter_loop(label.as_ref(), None);
                let b = self.check_block(body, None, false);
                self.leave_loop();
                self.pop_scope();
                let kind = match pat {
                    Some(p) => HKind::WhileLet { pat: p, value: Box::new(c), body: Box::new(b) },
                    None => HKind::While { cond: Box::new(c), body: Box::new(b) },
                };
                mk(kind, Type::Unit, span)
            }
            ExprKind::For { label, pattern, iter, body } => self.check_for(label.as_ref(), pattern, iter, body, span),
            ExprKind::Lambda { params, body, .. } => self.check_lambda(params, body, hint, span),
            ExprKind::Return(v) => {
                self.check_not_in_defer("`return`", span);
                let ret = self.fs.ctx.last().map(|c| c.ret.clone()).unwrap_or(Type::Unit);
                if let Some(CtxKind::TopLevel { .. }) = self.fs.ctx.last().map(|c| c.kind) {
                    self.error(span, "`return` is not allowed at the top level; use `exit`");
                }
                let vh = match v {
                    Some(v) => Some(Box::new(self.check_expr(v, &ret))),
                    None => {
                        self.expect_type(&ret, &Type::Unit, span);
                        None
                    }
                };
                mk(HKind::Return(vh), Type::Never, span)
            }
            ExprKind::Break { label, value: v } => {
                let Some(depth) = self.loop_target(label.as_ref(), "break", span) else {
                    if let Some(v) = v {
                        self.infer(v, None);
                    }
                    return mk(HKind::Break { value: None, depth: 0 }, Type::Never, span);
                };
                let loops = &self.fs.ctx.last().unwrap().loops;
                let bt = loops[loops.len() - 1 - depth as usize].break_ty.clone();
                let n = self.loop_breaks.len();
                self.loop_breaks[n - 1 - depth as usize] = true;
                let vh = match (v, bt) {
                    (Some(v), Some(bt)) => Some(Box::new(self.check_expr(v, &bt))),
                    (Some(v), None) => {
                        self.error(v.span, "break with a value is only allowed inside `loop`");
                        None
                    }
                    (None, Some(bt)) => {
                        self.unify(&bt, &Type::Unit);
                        None
                    }
                    (None, None) => None,
                };
                mk(HKind::Break { value: vh, depth }, Type::Never, span)
            }
            ExprKind::Continue(label) => {
                let depth = self.loop_target(label.as_ref(), "continue", span).unwrap_or(0);
                mk(HKind::Continue { depth }, Type::Never, span)
            }
            ExprKind::Spawn(inner) => {
                self.fs.next_lambda += 1;
                let id = self.fs.next_lambda;
                let ret = self.fresh();
                self.fs.ctx.push(FnCtx { ret: ret.clone(), kind: CtxKind::Lambda, loops: vec![], lambda: id, in_defer: None });
                self.push_scope();
                let body = self.infer(inner, None);
                self.join(&ret, &body.ty, inner.span);
                if self.resolve(&body.ty).is_never() && matches!(self.resolve(&ret), Type::Var(_)) {
                    self.unify(&ret, &Type::Unit);
                }
                self.pop_scope();
                self.fs.ctx.pop();
                let lam_ty = Type::func(vec![], ret.clone());
                let lam = mk(
                    HKind::Lambda(Box::new(HLambda {
                        id,
                        name: "<spawn>".into(),
                        params: vec![],
                        body,
                        doc: None,
                        signature: "fn()".into(),
                        self_local: None,
                    })),
                    lam_ty,
                    span,
                );
                mk(HKind::Spawn(Box::new(lam)), Type::Named(self.wk.task, vec![ret]), span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Strings

    fn check_string(&mut self, segs: &[StrSeg], span: Span) -> HExpr {
        if segs.iter().all(|s| matches!(s, StrSeg::Text(_))) {
            let s: String = segs.iter().map(|s| if let StrSeg::Text(t) = s { t.as_str() } else { "" }).collect();
            return mk(HKind::Str(s), Type::Str, span);
        }
        let mut parts = Vec::new();
        for s in segs {
            match s {
                StrSeg::Text(t) => parts.push(InterpPart::Text(t.clone())),
                StrSeg::Expr(e, spec) => {
                    let h = self.infer(e, None);
                    let fs = match spec {
                        Some(spec) => match parse_format_spec(&spec.text) {
                            Ok(f) => {
                                self.validate_spec(&f, &h.ty, spec.span, &spec.text);
                                Some(f)
                            }
                            Err(m) => {
                                self.error(spec.span, m);
                                None
                            }
                        },
                        None => None,
                    };
                    parts.push(InterpPart::Expr(Box::new(h), fs));
                }
            }
        }
        mk(HKind::Interp(parts), Type::Str, span)
    }

    fn validate_spec(&mut self, f: &FormatSpec, ty: &Type, span: Span, text: &str) {
        let t = self.resolve(ty);
        let ok = match f.kind {
            Some('x') | Some('X') | Some('b') | Some('o') => {
                matches!(t, Type::Int | Type::Var(_) | Type::Error)
            }
            Some('e') => matches!(t, Type::Float | Type::Var(_) | Type::Error),
            _ => true,
        };
        if !ok {
            let msg = format!("format spec `{text}` is not valid for {}", self.show(&t));
            self.error(span, msg);
        }
        if f.precision.is_some() && f.kind != Some('?') && matches!(t, Type::Int) {
            self.error(span, format!("format spec `{text}`: precision is not valid for int"));
        }
    }

    // ------------------------------------------------------------------
    // Names

    fn instantiate_local(&mut self, l: LocalId) -> Type {
        let scheme = self.fs.data[l as usize].scheme.clone();
        let t = self.fs.data[l as usize].ty.clone();
        if scheme.is_empty() {
            return t;
        }
        let fresh: Vec<Type> = scheme.iter().map(|_| self.fresh()).collect();
        t.subst_params(&|p| scheme.iter().position(|&q| q == p).map(|i| fresh[i].clone()))
    }

    fn check_ident(&mut self, id: &ast::Ident, hint: Option<&Type>) -> HExpr {
        let span = id.span;
        if id.name == "_" {
            self.error(span, "`_` cannot be used as a value");
            return err(span);
        }
        if let Some(l) = self.find_local(&id.name) {
            self.use_local(l);
            let t = self.instantiate_local(l);
            return mk(HKind::Local(l), t, span);
        }
        match self.lookup_global(&id.name) {
            Some(def) => self.def_value(def, &id.name, hint, span),
            None if self.fs.generics.iter().any(|(n, _)| *n == id.name) => {
                self.error(span, format!("`{}` is a type parameter, not a value", id.name));
                err(span)
            }
            None => {
                self.error(span, format!("undefined name `{}`", id.name));
                err(span)
            }
        }
    }

    /// The value of a resolved definition used in expression position.
    fn def_value(&mut self, def: Def, name: &str, hint: Option<&Type>, span: Span) -> HExpr {
        match def {
            Def::Fn(f) => self.fn_value(f, span),
            Def::Global(g) => {
                self.mark_global_used(g);
                let t = self.globals[g as usize].ty.clone();
                mk(HKind::Global(g), t, span)
            }
            Def::Variant(d, i) => self.variant_value(d, i, hint, span),
            Def::Type(_) => {
                self.error(span, format!("`{name}` is a type, not a value"));
                err(span)
            }
            Def::Module(_) => {
                self.error(span, format!("`{name}` is a module, not a value"));
                err(span)
            }
            Def::Trait(_) => {
                self.error(span, format!("`{name}` is a trait, not a value"));
                err(span)
            }
        }
    }

    fn instantiate_fn(&mut self, f: FnId, span: Span) -> (Vec<Type>, Vec<Type>, Type) {
        let inst = self.instantiate_fn_with(f, span, &[]);
        (inst.fresh, inst.params, inst.ret)
    }

    /// Instantiates `f` with fresh type variables, first unifying those of
    /// the parameters in `pre` with the given types. Built-in bounds become
    /// obligations; the trait evidence the call needs is returned in `dicts`.
    pub(crate) fn instantiate_fn_with(&mut self, f: FnId, span: Span, pre: &[(ParamId, Type)]) -> Instance {
        let fi = self.fns[f as usize].clone();
        let fresh: Vec<Type> = fi.generics.iter().map(|_| self.fresh()).collect();
        if let Some(q) = &mut self.type_query {
            q.origins.extend(fi.generics.iter().zip(&fresh).filter_map(|(&g, v)| match v {
                Type::Var(v) => Some((*v, g)),
                _ => None,
            }));
        }
        for (p, t) in pre {
            if let Some(i) = fi.generics.iter().position(|g| g == p) {
                self.unify(&fresh[i], t);
            }
        }
        let map = |p: ParamId| fi.generics.iter().position(|&g| g == p).map(|i| fresh[i].clone());
        let params: Vec<Type> = fi.params.iter().map(|p| p.ty.subst_params(&map)).collect();
        let ret = fi.ret.subst_params(&map);
        let dicts = fi.dicts.iter().map(|(t, tr)| (t.subst_params(&map), tr.subst_params(&map))).collect();
        for (g, v) in fi.generics.iter().zip(&fresh) {
            let bounds = self.params[*g as usize].bounds.clone();
            for b in bounds {
                self.require_bound(v, b, span);
            }
        }
        Instance { fresh, params, ret, dicts }
    }

    fn fn_value(&mut self, f: FnId, span: Span) -> HExpr {
        if !self.fns[f as usize].dicts.is_empty() {
            // Partially apply the dictionaries.
            return self.wrapper_value(f, &[], span);
        }
        let (_, params, ret) = self.instantiate_fn(f, span);
        let t = Type::func(params, ret);
        mk(HKind::FnValue(f), t, span)
    }

    fn enum_args(&mut self, d: TypeDefId, hint: Option<&Type>) -> Vec<Type> {
        if let Some(h) = hint
            && let Type::Named(hd, args) = self.resolve(h)
            && hd == d
        {
            return args;
        }
        let n = self.types[d as usize].generics.len();
        (0..n).map(|_| self.fresh()).collect()
    }

    fn variant_field_types(&self, d: TypeDefId, i: u32, args: &[Type]) -> Vec<Type> {
        let def = &self.types[d as usize];
        let gens = def.generics.clone();
        def.variants()[i as usize]
            .fields
            .iter()
            .map(|t| t.subst_params(&|p| gens.iter().position(|&g| g == p).map(|k| args[k].clone())))
            .collect()
    }

    fn variant_value(&mut self, d: TypeDefId, i: u32, hint: Option<&Type>, span: Span) -> HExpr {
        let args = self.enum_args(d, hint);
        let v = self.types[d as usize].variants()[i as usize].clone();
        let enum_ty = Type::Named(d, args.clone());
        if v.fields.is_empty() {
            mk(HKind::Tag(v.rt_id), enum_ty, span)
        } else {
            let fields = self.variant_field_types(d, i, &args);
            let n = fields.len();
            mk(HKind::VariantFn { rt_id: v.rt_id, arity: n }, Type::func(fields, enum_ty), span)
        }
    }

    /// Resolves an expression used as a namespace (`module`, `Type`, `module.Type`).
    pub(crate) fn resolve_ns(&mut self, e: &Expr) -> Option<Ns> {
        match &e.kind {
            ExprKind::Ident(id) => {
                if self.find_local(&id.name).is_some() {
                    return None;
                }
                if let Some((_, p)) = self.fs.generics.iter().rev().find(|(n, _)| *n == id.name) {
                    return Some(Ns::Type(Type::Param(*p)));
                }
                match self.lookup_global(&id.name)? {
                    Def::Module(m) => Some(Ns::Module(m)),
                    Def::Type(r) => Some(Ns::Type(self.tyref_open(r, id.span, &id.name))),
                    Def::Trait(t) => Some(Ns::Trait(t)),
                    _ => None,
                }
            }
            ExprKind::SelfType => match self.fs.self_ty.clone() {
                Some(t) => Some(Ns::Type(t)),
                None => {
                    self.error(e.span, "`Self` is only valid inside a type's methods");
                    Some(Ns::Type(Type::Error))
                }
            },
            ExprKind::Field(base, name) => match self.resolve_ns(base)? {
                Ns::Module(m) => {
                    let found = self.mods[m as usize].scope.get(&name.name).cloned();
                    match found {
                        Some((Def::Type(r), true, _)) => Some(Ns::Type(self.tyref_open(r, name.span, &name.name))),
                        Some((Def::Trait(t), true, _)) => Some(Ns::Trait(t)),
                        _ => None,
                    }
                }
                Ns::Type(_) | Ns::Trait(_) => None,
            },
            _ => None,
        }
    }

    /// A type reference with fresh type arguments.
    fn tyref_open(&mut self, r: TyRef, span: Span, name: &str) -> Type {
        match r {
            TyRef::Prim(t) => t,
            TyRef::Set => {
                let v = self.fresh();
                Type::Set(Box::new(v))
            }
            TyRef::Def(d) => {
                let n = self.types[d as usize].generics.len();
                let args = (0..n).map(|_| self.fresh()).collect();
                Type::Named(d, args)
            }
            TyRef::Alias(i) => {
                let n = self.aliases[i].generics.len();
                let args = (0..n).map(|_| self.fresh()).collect();
                self.instantiate_tyref(TyRef::Alias(i), args, span, name)
            }
        }
    }

    fn find_variant(&self, d: TypeDefId, name: &str) -> Option<u32> {
        self.types[d as usize].variants().iter().position(|v| v.name == name).map(|i| i as u32)
    }

    // ------------------------------------------------------------------
    // Fields

    /// (index, type, mutable) of a struct field.
    fn struct_field(&self, t: &Type, name: &str) -> Option<(u32, Type, bool)> {
        let Type::Named(d, args) = t else { return None };
        let def = &self.types[*d as usize];
        let gens = def.generics.clone();
        def.fields().iter().position(|f| f.name == name).map(|i| {
            let f = &def.fields()[i];
            let ty = f.ty.subst_params(&|p| gens.iter().position(|&g| g == p).map(|k| args[k].clone()));
            (i as u32, ty, f.mutable)
        })
    }

    fn check_field(&mut self, obj: &Expr, name: &ast::Ident, hint: Option<&Type>, span: Span) -> HExpr {
        match self.resolve_ns(obj) {
            Some(Ns::Module(m)) => {
                return match self.module_member(m, &name.name, name.span) {
                    Some(def) => {
                        self.lint.used_names.insert((self.cur_mod, self.ns_root_name(obj)));
                        self.def_value(def, &name.name, hint, span)
                    }
                    None => err(span),
                };
            }
            Some(Ns::Type(t)) => {
                let t = self.resolve(&t);
                if let Type::Named(d, _) = t
                    && let Some(i) = self.find_variant(d, &name.name)
                {
                    return self.variant_value(d, i, hint, span);
                }
                if let Some(c) = ctor_of(&t)
                    && let Some(fs) = self.methods.get(&(c, name.name.clone())).cloned()
                {
                    return self.fn_value(fs[0], span);
                }
                match self.find_trait_method(&t, &name.name, name.span) {
                    Ok(Some((tr, slot))) => return self.trait_method_value(tr, slot, t, span),
                    Ok(None) => {}
                    Err(()) => return err(span),
                }
                if t == Type::Error {
                    return err(span);
                }
                let msg = if matches!(t, Type::Named(d, _) if self.types[d as usize].is_enum()) {
                    format!("no variant `{}` on type {}", name.name, self.show(&t))
                } else {
                    format!("no method `{}` on type {}", name.name, self.show(&t))
                };
                self.error(name.span, msg);
                return err(span);
            }
            Some(Ns::Trait(t)) => {
                return match self.trait_method_slot(t, &name.name, name.span) {
                    Some(slot) => {
                        let tr = self.fresh_trait_ref(t);
                        let self_ty = self.fresh();
                        self.trait_method_value(tr, slot, self_ty, span)
                    }
                    None => err(span),
                };
            }
            None => {}
        }
        let o = self.infer(obj, None);
        let ot = self.resolve(&o.ty);
        if let Some((idx, fty, _)) = self.struct_field(&ot, &name.name) {
            return mk(HKind::Field { obj: Box::new(o), index: idx }, fty, span);
        }
        match ot {
            Type::Error => err(span),
            Type::Var(_) => {
                self.error(name.span, "type annotation needed before field access");
                err(span)
            }
            other => {
                let has_method = ctor_of(&other).map(|c| self.methods.contains_key(&(c, name.name.clone()))).unwrap_or(false)
                    || self.has_trait_method(&other, &name.name);
                let msg = if has_method {
                    format!("`{}` is a method; call it with `()`", name.name)
                } else {
                    format!("no field `{}` on type {}", name.name, self.show(&other))
                };
                self.error(name.span, msg);
                err(span)
            }
        }
    }

    fn ns_root_name(&self, e: &Expr) -> String {
        match &e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            ExprKind::Field(b, _) => self.ns_root_name(b),
            _ => String::new(),
        }
    }

    // ------------------------------------------------------------------
    // Calls

    /// Binds call arguments to parameters, checks them and fills defaults.
    #[allow(clippy::too_many_arguments)]
    fn bind_args(
        &mut self,
        what: &str,
        names: &[String],
        ptys: &[Type],
        defaults: &[Option<DefaultArg>],
        first: Option<HExpr>,
        args: &CallArgs,
        span: Span,
        allow_named: bool,
    ) -> Option<Vec<HArg>> {
        let n = ptys.len();
        let offset = usize::from(first.is_some());
        let mut slots: Vec<Option<HArg>> = (0..n).map(|_| None).collect();
        if let Some(f) = first {
            if n == 0 {
                self.error(span, format!("{what} takes no arguments"));
                return None;
            }
            self.expect_type(&ptys[0], &f.ty, f.span);
            slots[0] = Some(HArg::Expr(f));
        }
        if !allow_named && !args.named.is_empty() {
            self.error(args.named[0].0.span, "named arguments are only allowed when calling a function by name");
            return None;
        }
        let user_params = n - offset;
        if args.positional.len() > user_params {
            let plural = if user_params == 1 { "" } else { "s" };
            self.error(span, format!("{what} expected {user_params} argument{plural}, found {}", args.positional.len() + args.named.len()));
            // Still check the arguments for errors.
            for a in &args.positional {
                self.infer(a, None);
            }
            return None;
        }
        // Order of evaluation: positional, then named in source order.
        let mut out_of_order = false;
        let mut last_idx = 0usize;
        for (i, a) in args.positional.iter().enumerate() {
            let pi = i + offset;
            let h = self.check_expr(a, &ptys[pi]);
            slots[pi] = Some(HArg::Expr(h));
            last_idx = pi;
        }
        let mut named_order = Vec::new();
        for (name, a) in &args.named {
            match names.iter().position(|n| *n == name.name) {
                Some(pi) if pi >= offset => {
                    if slots[pi].is_some() {
                        self.error(name.span, format!("argument `{}` given more than once", name.name));
                        self.infer(a, None);
                        continue;
                    }
                    if pi < last_idx {
                        out_of_order = true;
                    }
                    last_idx = pi;
                    let h = self.check_expr(a, &ptys[pi]);
                    named_order.push(pi);
                    slots[pi] = Some(HArg::Expr(h));
                }
                _ => {
                    self.error(name.span, format!("{what} has no parameter named `{}`", name.name));
                    self.infer(a, None);
                }
            }
        }
        let mut ok = true;
        let mut out = Vec::new();
        for i in 0..n {
            match slots[i].take() {
                Some(a) => out.push(a),
                None => match defaults.get(i).cloned().flatten() {
                    Some(DefaultArg::Inline(e)) => {
                        let h = self.check_expr(&e, &ptys[i]);
                        out.push(HArg::Expr(HExpr { span, ..h }));
                    }
                    Some(DefaultArg::Helper(helper)) => {
                        out.push(HArg::Default { helper, inputs: (0..i).collect(), ty: ptys[i].clone() });
                    }
                    None => {
                        self.error(span, format!("missing argument `{}`", names[i]));
                        ok = false;
                    }
                },
            }
        }
        if !ok {
            return None;
        }
        if out_of_order {
            // Evaluate named arguments in source order via temporaries.
            self.reorder_args(&mut out, &named_order, span);
        }
        Some(out)
    }

    fn reorder_args(&mut self, out: &mut [HArg], order: &[usize], span: Span) {
        // Hoist the named arguments into locals in source order. Named
        // arguments always follow the positional ones, so the hoisting
        // statements are attached to the earliest hoisted slot.
        let mut lets = Vec::new();
        for &i in order {
            if let HArg::Expr(h) = &out[i] {
                let ty = h.ty.clone();
                let l = self.hidden_local(ty.clone(), span);
                let value = std::mem::replace(&mut out[i], HArg::Expr(mk(HKind::Local(l), ty.clone(), span)));
                if let HArg::Expr(v) = value {
                    lets.push(mk(
                        HKind::Let { pat: HPat { kind: HPatKind::Bind(l), ty: ty.clone(), span }, init: Box::new(v), else_: None },
                        Type::Unit,
                        span,
                    ));
                }
            }
        }
        let Some(&first) = order.iter().min() else { return };
        if let HArg::Expr(slot) = &mut out[first] {
            let read = std::mem::replace(slot, mk(HKind::Unit, Type::Unit, span));
            let ty = read.ty.clone();
            lets.push(read);
            *slot = mk(HKind::Block(lets), ty, span);
        }
    }

    /// Calls a known function `f`, optionally with a receiver for `self`.
    fn call_fn(&mut self, f: FnId, recv: Option<HExpr>, args: &[Arg], span: Span) -> HExpr {
        self.call_fn_with(f, recv, args, span, &[])
    }

    /// Calls `f` with some of its type parameters fixed (see [`Self::instantiate_fn_with`]).
    pub(crate) fn call_fn_with(&mut self, f: FnId, recv: Option<HExpr>, args: &[Arg], span: Span, pre: &[(ParamId, Type)]) -> HExpr {
        let fi = self.fns[f as usize].clone();
        let Instance { fresh, params: ptys, ret, dicts } = self.instantiate_fn_with(f, span, pre);
        let names: Vec<String> = fi.params.iter().map(|p| p.name.clone()).collect();
        let defaults: Vec<Option<DefaultArg>> = fi.params.iter().map(|p| p.default.clone()).collect();
        let what = format!("`{}`", fi.name);
        let ca = CallArgs::from(args);
        let Some(mut hargs) = self.bind_args(&what, &names, &ptys, &defaults, recv, &ca, span, true) else {
            return mk(HKind::Error, ret, span);
        };
        if !dicts.is_empty() {
            // Dictionaries are passed first; default helpers receive them too.
            let k = dicts.len();
            for a in &mut hargs {
                if let HArg::Default { inputs, .. } = a {
                    *inputs = (0..k).chain(inputs.iter().map(|i| i + k)).collect();
                }
            }
            let mut all: Vec<HArg> = dicts.into_iter().map(|(t, tr)| HArg::Expr(self.evidence_hole(t, tr, span))).collect();
            all.extend(hargs);
            hargs = all;
        }
        match fi.kind {
            FnKind::TraitMethod(trait_id, slot) => mk(HKind::CallTrait { trait_id, slot, args: hargs, impl_id: None }, ret, span),
            FnKind::Native(n) => {
                let mut reflect = Vec::new();
                for (g, t) in fi.generics.iter().zip(&fresh) {
                    if self.params[*g as usize].reflect {
                        reflect.push(t.clone());
                        self.fs.reflect_checks.push((t.clone(), span));
                    }
                }
                mk(HKind::CallNative { native: n, args: hargs, reflect }, ret, span)
            }
            _ => mk(HKind::Call { func: f, args: hargs }, ret, span),
        }
    }

    fn call_variant(&mut self, d: TypeDefId, i: u32, args: &[Arg], _hint: Option<&Type>, span: Span) -> HExpr {
        // Hints are not pushed into payloads so that mismatches are reported
        // on the whole value (`expected str?, found int?`).
        let targs = self.enum_args(d, None);
        let v = self.types[d as usize].variants()[i as usize].clone();
        let enum_ty = Type::Named(d, targs.clone());
        if v.fields.is_empty() {
            self.error(span, format!("`{}` is not a function; it has no payload", v.name));
            return err(span);
        }
        let ptys = self.variant_field_types(d, i, &targs);
        if args.iter().all(|a| a.name.is_none()) && args.len() != ptys.len() {
            let plural = if ptys.len() == 1 { "" } else { "s" };
            self.error(span, format!("`{}` expected {} argument{plural}, found {}", v.name, ptys.len(), args.len()));
            for a in args {
                self.infer(&a.value, None);
            }
            return mk(HKind::Error, enum_ty, span);
        }
        let names: Vec<String> = match &v.field_names {
            Some(ns) => ns.clone(),
            None => (0..ptys.len()).map(|k| k.to_string()).collect(),
        };
        let ca = CallArgs::from(args);
        let what = format!("`{}`", v.name);
        let defaults = vec![None; ptys.len()];
        let Some(hargs) = self.bind_args(&what, &names, &ptys, &defaults, None, &ca, span, true) else {
            return mk(HKind::Error, enum_ty, span);
        };
        let hs = hargs
            .into_iter()
            .map(|a| match a {
                HArg::Expr(e) => e,
                HArg::Default { .. } => unreachable!(),
            })
            .collect();
        mk(HKind::MakeVariant { rt_id: v.rt_id, args: hs }, enum_ty, span)
    }

    fn call_value(&mut self, callee: HExpr, args: &[Arg], span: Span) -> HExpr {
        let ct = self.resolve(&callee.ty);
        let (ptys, ret) = match ct {
            Type::Fn(ps, r) => {
                if args.len() != ps.len() {
                    let plural = if ps.len() == 1 { "" } else { "s" };
                    self.error(span, format!("function expected {} argument{plural}, found {}", ps.len(), args.len()));
                    for a in args {
                        self.infer(&a.value, None);
                    }
                    return mk(HKind::Error, *r, span);
                }
                (ps, *r)
            }
            Type::Var(_) => {
                // Infer a function type from the call.
                let ps: Vec<Type> = args.iter().map(|_| self.fresh()).collect();
                let r = self.fresh();
                self.unify(&callee.ty, &Type::func(ps.clone(), r.clone()));
                (ps, r)
            }
            Type::Error => {
                for a in args {
                    self.infer(&a.value, None);
                }
                return err(span);
            }
            other => {
                let msg = format!("{} is not a function", self.show(&other));
                self.error(callee.span, msg);
                for a in args {
                    self.infer(&a.value, None);
                }
                return err(span);
            }
        };
        let ca = CallArgs::from(args);
        let names: Vec<String> = (0..ptys.len()).map(|i| format!("#{i}")).collect();
        let defaults = vec![None; ptys.len()];
        let Some(hargs) = self.bind_args("function", &names, &ptys, &defaults, None, &ca, span, false) else {
            return mk(HKind::Error, ret, span);
        };
        let hs = hargs
            .into_iter()
            .map(|a| match a {
                HArg::Expr(e) => e,
                HArg::Default { .. } => unreachable!(),
            })
            .collect();
        mk(HKind::CallValue { callee: Box::new(callee), args: hs }, ret, span)
    }

    fn check_call(&mut self, callee: &Expr, args: &[Arg], hint: Option<&Type>, span: Span) -> HExpr {
        match &callee.kind {
            ExprKind::Ident(id) if self.find_local(&id.name).is_none() => match self.lookup_global(&id.name) {
                Some(Def::Fn(f)) => return self.call_fn(f, None, args, span),
                Some(Def::Variant(d, i)) => return self.call_variant(d, i, args, hint, span),
                Some(Def::Type(_)) => {
                    self.error(callee.span, format!("`{}` is a type; use `{} {{ ... }}` to construct it", id.name, id.name));
                    return err(span);
                }
                _ => {}
            },
            ExprKind::Field(obj, name) => {
                if let Some(ns) = self.resolve_ns(obj) {
                    return self.ns_call(ns, obj, name, args, hint, span);
                }
            }
            _ => {}
        }
        let c = self.infer(callee, None);
        self.call_value(c, args, span)
    }

    fn ns_call(&mut self, ns: Ns, obj: &Expr, name: &ast::Ident, args: &[Arg], hint: Option<&Type>, span: Span) -> HExpr {
        match ns {
            Ns::Module(m) => {
                self.lint.used_names.insert((self.cur_mod, self.ns_root_name(obj)));
                match self.module_member(m, &name.name, name.span) {
                    Some(Def::Fn(f)) => self.call_fn(f, None, args, span),
                    Some(Def::Variant(d, i)) => self.call_variant(d, i, args, hint, span),
                    Some(Def::Global(g)) => {
                        let gv = self.def_value(Def::Global(g), &name.name, None, name.span);
                        self.call_value(gv, args, span)
                    }
                    Some(Def::Type(_)) => {
                        self.error(name.span, format!("`{}` is a type, not a function", name.name));
                        err(span)
                    }
                    Some(Def::Module(_)) => {
                        self.error(name.span, format!("`{}` is a module, not a function", name.name));
                        err(span)
                    }
                    Some(Def::Trait(_)) => {
                        self.error(name.span, format!("`{}` is a trait, not a function", name.name));
                        err(span)
                    }
                    None => {
                        for a in args {
                            self.infer(&a.value, None);
                        }
                        err(span)
                    }
                }
            }
            Ns::Type(t) => {
                let t = self.resolve(&t);
                if let Type::Named(d, _) = t
                    && let Some(i) = self.find_variant(d, &name.name)
                {
                    return self.call_variant(d, i, args, hint, span);
                }
                if let Some(c) = ctor_of(&t)
                    && let Some(fs) = self.methods.get(&(c, name.name.clone())).cloned()
                {
                    return self.call_fn(fs[0], None, args, span);
                }
                match self.find_trait_method(&t, &name.name, name.span) {
                    Ok(Some((tr, slot))) => return self.call_trait_method(tr, slot, t, None, args, span),
                    Ok(None) => {}
                    Err(()) => {
                        for a in args {
                            self.infer(&a.value, None);
                        }
                        return err(span);
                    }
                }
                if t == Type::Error {
                    return err(span);
                }
                let msg = if matches!(t, Type::Named(d, _) if self.types[d as usize].is_enum()) {
                    format!("no variant `{}` on type {}", name.name, self.show(&t))
                } else {
                    format!("no method `{}` on type {}", name.name, self.show(&t))
                };
                self.error(name.span, msg);
                err(span)
            }
            Ns::Trait(t) => match self.trait_method_slot(t, &name.name, name.span) {
                // `Trait.method(x, ...)`: the implementing type is inferred.
                Some(slot) => {
                    let tr = self.fresh_trait_ref(t);
                    let self_ty = self.fresh();
                    self.call_trait_method(tr, slot, self_ty, None, args, span)
                }
                None => {
                    for a in args {
                        self.infer(&a.value, None);
                    }
                    err(span)
                }
            },
        }
    }

    fn lookup_method(&mut self, recv_ty: &Type, name: &str) -> Option<FnId> {
        let c = ctor_of(recv_ty)?;
        let fs = self.methods.get(&(c, name.to_string()))?;
        fs.iter().copied().find(|&f| self.fns[f as usize].has_self)
    }

    fn check_method_call(&mut self, receiver: &Expr, method: &ast::Ident, args: &[Arg], hint: Option<&Type>, span: Span) -> HExpr {
        if let Some(ns) = self.resolve_ns(receiver) {
            return self.ns_call(ns, receiver, method, args, hint, span);
        }
        let r = self.infer(receiver, None);
        let rt = self.resolve(&r.ty);
        match rt {
            Type::Error => {
                for a in args {
                    self.infer(&a.value, None);
                }
                return err(span);
            }
            Type::Var(_) => {
                self.error(method.span, format!("type annotation needed before calling `{}`", method.name));
                for a in args {
                    self.infer(&a.value, None);
                }
                return err(span);
            }
            _ => {}
        }
        // A function-typed struct field.
        if let Some((idx, fty, _)) = self.struct_field(&rt, &method.name)
            && matches!(self.resolve(&fty), Type::Fn(..))
        {
            let callee = mk(HKind::Field { obj: Box::new(r), index: idx }, fty, method.span);
            return self.call_value(callee, args, span);
        }
        if let Some(f) = self.lookup_method(&rt, &method.name) {
            // Check the receiver against the declared self type.
            let fi = self.fns[f as usize].clone();
            let (_, ptys, _) = self.instantiate_fn(f, span);
            let probe = ptys[0].clone();
            if !self.unify(&probe, &rt) {
                let msg = format!("no method `{}` on type {}", method.name, self.show(&rt));
                self.error(method.span, msg);
                return err(span);
            }
            let _ = fi;
            let call = self.call_fn(f, Some(r), args, span);
            return self.fuse_consumer(call);
        }
        // Trait methods: through the bounds of a type parameter, or the impls of a type.
        match self.find_trait_method(&rt, &method.name, method.span) {
            Ok(Some((tr, slot))) => {
                let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
                if !self.fns[sig as usize].has_self {
                    let msg = format!("`{}` is a static method of `{}`; call it on the type", method.name, self.show_trait(&tr));
                    self.error(method.span, msg);
                    for a in args {
                        self.infer(&a.value, None);
                    }
                    return err(span);
                }
                return self.call_trait_method(tr, slot, rt, Some(r), args, span);
            }
            Ok(None) => {}
            Err(()) => {
                for a in args {
                    self.infer(&a.value, None);
                }
                return err(span);
            }
        }
        match method.name.as_str() {
            "to_str" | "debug" if args.is_empty() => {
                let kind = if method.name == "to_str" { HKind::ToStr(Box::new(r)) } else { HKind::Debug(Box::new(r)) };
                mk(kind, Type::Str, span)
            }
            _ => {
                let mut msg = format!("no method `{}` on type {}", method.name, self.show(&rt));
                if let Some(t) = self.trait_with_method(&method.name) {
                    if let Type::Param(_) = rt {
                        msg.push_str(&format!("; add a bound `{}: {t}`", self.show(&rt)));
                    } else {
                        msg.push_str(&format!(" (the trait `{t}` has it but is not implemented for {})", self.show(&rt)));
                    }
                }
                self.error(method.span, msg);
                for a in args {
                    self.infer(&a.value, None);
                }
                err(span)
            }
        }
    }

    fn check_pipe(&mut self, l: &Expr, r: &Expr, hint: Option<&Type>, span: Span) -> HExpr {
        let lhs_arg = Arg { name: None, value: l.clone() };
        match &r.kind {
            ExprKind::Call { callee, args } => {
                let mut a = vec![lhs_arg];
                a.extend(args.iter().cloned());
                self.check_call(callee, &a, hint, span)
            }
            ExprKind::MethodCall { receiver, method, args } => {
                let mut a = vec![lhs_arg];
                a.extend(args.iter().cloned());
                self.check_method_call(receiver, method, &a, hint, span)
            }
            ExprKind::Ident(_) | ExprKind::Field(..) => self.check_call(r, &[lhs_arg], hint, span),
            ExprKind::Lambda { .. } | ExprKind::Paren(_) => {
                let c = self.infer(r, None);
                self.call_value(c, &[lhs_arg], span)
            }
            _ => {
                self.error(r.span, "the right side of `|>` must be a function name or call");
                self.infer(l, None);
                err(span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Struct literals

    fn check_struct_lit(
        &mut self,
        path: &[ast::Ident],
        fields: &[ast::FieldInit],
        base: Option<&Expr>,
        hint: Option<&Type>,
        span: Span,
    ) -> HExpr {
        let def = match self.resolve_struct_path(path) {
            Some(d) => d,
            None => {
                for f in fields {
                    if let Some(v) = &f.value {
                        self.infer(v, None);
                    }
                }
                return err(span);
            }
        };
        let tdef = self.types[def as usize].clone();
        if !tdef.is_struct() {
            self.error(path.last().unwrap().span, format!("`{}` is not a struct", tdef.name));
            return err(span);
        }
        let _ = hint;
        let args = self.enum_args(def, None);
        let sty = Type::Named(def, args.clone());
        let gens = tdef.generics.clone();
        let sub = |t: &Type| t.subst_params(&|p| gens.iter().position(|&g| g == p).map(|k| args[k].clone()));
        let fdefs = tdef.fields().to_vec();
        let mut given: Vec<Option<LocalId>> = vec![None; fdefs.len()];
        let mut stmts = Vec::new();
        for f in fields {
            let Some(idx) = fdefs.iter().position(|d| d.name == f.name.name) else {
                let msg = format!("no field `{}` on type {}", f.name.name, tdef.name);
                self.error(f.name.span, msg);
                if let Some(v) = &f.value {
                    self.infer(v, None);
                }
                continue;
            };
            if given[idx].is_some() {
                self.error(f.name.span, format!("field `{}` is given more than once", f.name.name));
                continue;
            }
            let fty = sub(&fdefs[idx].ty);
            let h = match &f.value {
                Some(v) => self.check_expr(v, &fty),
                None => {
                    let id = f.name.clone();
                    let h = self.check_ident(&id, Some(&fty));
                    self.expect_type(&fty, &h.ty, id.span);
                    h
                }
            };
            let l = self.hidden_local(fty.clone(), f.name.span);
            stmts.push(mk(
                HKind::Let { pat: HPat { kind: HPatKind::Bind(l), ty: fty.clone(), span: f.name.span }, init: Box::new(h), else_: None },
                Type::Unit,
                f.name.span,
            ));
            given[idx] = Some(l);
        }
        let base_local = base.map(|b| {
            let h = self.check_expr(b, &sty);
            let l = self.hidden_local(sty.clone(), b.span);
            stmts.push(mk(
                HKind::Let { pat: HPat { kind: HPatKind::Bind(l), ty: sty.clone(), span: b.span }, init: Box::new(h), else_: None },
                Type::Unit,
                b.span,
            ));
            l
        });
        let mut values = Vec::new();
        let mut missing = Vec::new();
        for (i, fd) in fdefs.iter().enumerate() {
            let fty = sub(&fd.ty);
            let v = match (given[i], base_local, fd.default) {
                (Some(l), _, _) => mk(HKind::Local(l), fty, span),
                (None, Some(b), _) => {
                    mk(HKind::Field { obj: Box::new(mk(HKind::Local(b), sty.clone(), span)), index: i as u32 }, fty, span)
                }
                (None, None, Some(helper)) => {
                    // Instantiate the helper with this literal's type arguments.
                    mk(HKind::Call { func: helper, args: vec![] }, fty, span)
                }
                (None, None, None) => {
                    missing.push(fd.name.clone());
                    err(span)
                }
            };
            values.push(v);
        }
        for m in missing {
            self.error(span, format!("missing field `{m}` in `{}` literal", tdef.name));
        }
        let make = mk(HKind::MakeStruct { def, fields: values }, sty.clone(), span);
        if stmts.is_empty() {
            return make;
        }
        stmts.push(make);
        mk(HKind::Block(stmts), sty, span)
    }

    fn resolve_struct_path(&mut self, path: &[ast::Ident]) -> Option<TypeDefId> {
        let name = path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
        let r = if path.len() == 1 {
            if path[0].name == "Self" {
                return match self.fs.self_ty.clone() {
                    Some(Type::Named(d, _)) => Some(d),
                    _ => {
                        self.error(path[0].span, "`Self` is only valid inside a type's methods");
                        None
                    }
                };
            }
            match self.lookup_global(&path[0].name) {
                Some(Def::Type(r)) => Some(r),
                _ => None,
            }
        } else if path.len() == 2 {
            match self.lookup_global(&path[0].name) {
                Some(Def::Module(m)) => {
                    self.lint.used_names.insert((self.cur_mod, path[0].name.clone()));
                    match self.module_member(m, &path[1].name, path[1].span) {
                        Some(Def::Type(r)) => Some(r),
                        _ => return None,
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        match r {
            Some(TyRef::Def(d)) => Some(d),
            Some(TyRef::Alias(i)) => match self.resolve_alias(i, path[0].span) {
                Type::Named(d, _) => Some(d),
                _ => {
                    self.error(path[0].span, format!("`{name}` is not a struct"));
                    None
                }
            },
            Some(_) => {
                self.error(path[0].span, format!("`{name}` is not a struct"));
                None
            }
            None => {
                self.error(path.last().unwrap().span, format!("unknown type `{name}`"));
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // Operators

    fn check_unary(&mut self, op: UnOp, inner: &Expr, span: Span) -> HExpr {
        match op {
            UnOp::Not => {
                let h = self.check_expr(inner, &Type::Bool);
                mk(HKind::Unary { op: HUnOp::Not, e: Box::new(h) }, Type::Bool, span)
            }
            UnOp::BitNot => {
                let h = self.check_expr(inner, &Type::Int);
                mk(HKind::Unary { op: HUnOp::BitNot, e: Box::new(h) }, Type::Int, span)
            }
            UnOp::Neg => {
                let h = self.infer(inner, None);
                let t = self.resolve(&h.ty);
                match t {
                    Type::Int | Type::Float | Type::Duration | Type::Error => {}
                    Type::Var(_) | Type::Param(_) => self.require_bound(&t, Bound::Num, span),
                    other => {
                        let msg = format!("cannot negate a value of type {}", self.show(&other));
                        self.error(span, msg);
                    }
                }
                let ty = h.ty.clone();
                mk(HKind::Unary { op: HUnOp::Neg, e: Box::new(h) }, ty, span)
            }
        }
    }

    /// Result type of an arithmetic operation, reporting errors.
    fn arith_type(&mut self, op: ArithOp, lt: &Type, rt: &Type, span: Span) -> Type {
        let l = self.resolve(lt);
        let r = self.resolve(rt);
        use ArithOp::*;
        match op {
            BitAnd | BitOr | BitXor | Shl | Shr => {
                self.expect_type(&Type::Int, &l, span);
                self.expect_type(&Type::Int, &r, span);
                return Type::Int;
            }
            _ => {}
        }
        if matches!(l, Type::Error) || matches!(r, Type::Error) {
            return Type::Error;
        }
        if l == Type::Int && r == Type::Duration && op == Mul {
            return Type::Duration;
        }
        if l == Type::Duration {
            return match (op, &r) {
                (Add | Sub, _) => {
                    self.expect_type(&Type::Duration, &r, span);
                    Type::Duration
                }
                (Mul, _) => {
                    self.expect_type(&Type::Int, &r, span);
                    Type::Duration
                }
                (Div, Type::Duration) => Type::Float,
                (Div, _) => {
                    self.expect_type(&Type::Int, &r, span);
                    Type::Duration
                }
                _ => {
                    self.error(span, format!("operator `{}` is not defined for duration", arith_text(op)));
                    Type::Error
                }
            };
        }
        if !self.unify(&l, &r) {
            let (e, f) = self.show_pair(&l, &r);
            let msg = format!("type mismatch: expected {e}, found {f}");
            self.error(span, msg);
            return Type::Error;
        }
        let t = self.resolve(&l);
        match (&t, op) {
            (Type::Int | Type::Float, _) => t,
            (Type::Str | Type::Bytes | Type::List(_), Add) => t,
            (Type::Var(_), Add) => {
                // Decided later; numbers and concatenation both work at runtime.
                t
            }
            (Type::Var(_) | Type::Param(_), _) => {
                self.require_bound(&t, Bound::Num, span);
                t
            }
            _ => {
                let msg = format!("operator `{}` is not defined for {}; it requires Num", arith_text(op), self.show(&t));
                self.error(span, msg);
                Type::Error
            }
        }
    }

    fn check_binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> HExpr {
        match op {
            BinOp::And | BinOp::Or => {
                let lh = self.check_expr(l, &Type::Bool);
                let rh = self.check_expr(r, &Type::Bool);
                let kind = if op == BinOp::And { HKind::And(Box::new(lh), Box::new(rh)) } else { HKind::Or(Box::new(lh), Box::new(rh)) };
                mk(kind, Type::Bool, span)
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let lh = self.infer(l, None);
                let rh = self.infer(r, Some(&lh.ty.clone()));
                if !self.unify(&lh.ty, &rh.ty) {
                    let msg = format!("type mismatch: cannot compare {} with {}", self.show(&lh.ty), self.show(&rh.ty));
                    self.error(span, msg);
                }
                let bound = if matches!(op, BinOp::Eq | BinOp::Ne) { Bound::Eq } else { Bound::Ord };
                let t = lh.ty.clone();
                self.require_bound(&t, bound, span);
                let cop = match op {
                    BinOp::Eq => CmpOp::Eq,
                    BinOp::Ne => CmpOp::Ne,
                    BinOp::Lt => CmpOp::Lt,
                    BinOp::Le => CmpOp::Le,
                    BinOp::Gt => CmpOp::Gt,
                    _ => CmpOp::Ge,
                };
                mk(HKind::Cmp { op: cop, l: Box::new(lh), r: Box::new(rh) }, Type::Bool, span)
            }
            _ => {
                let aop = match op {
                    BinOp::Add => ArithOp::Add,
                    BinOp::Sub => ArithOp::Sub,
                    BinOp::Mul => ArithOp::Mul,
                    BinOp::Div => ArithOp::Div,
                    BinOp::Rem => ArithOp::Rem,
                    BinOp::Pow => ArithOp::Pow,
                    BinOp::BitAnd => ArithOp::BitAnd,
                    BinOp::BitOr => ArithOp::BitOr,
                    BinOp::BitXor => ArithOp::BitXor,
                    BinOp::Shl => ArithOp::Shl,
                    _ => ArithOp::Shr,
                };
                let lh = self.infer(l, None);
                let hint = self.resolve(&lh.ty);
                let rh = self.infer(r, if matches!(hint, Type::Var(_) | Type::Duration) { None } else { Some(&hint) });
                let ty = self.arith_type(aop, &lh.ty, &rh.ty, span);
                mk(HKind::Arith { op: aop, l: Box::new(lh), r: Box::new(rh) }, ty, span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Indexing and `?`

    fn check_index(&mut self, obj: &Expr, idx: &Expr, span: Span) -> HExpr {
        let o = self.infer(obj, None);
        let ot = self.resolve(&o.ty);
        if let ExprKind::Range { start, end, inclusive } = &idx.kind {
            let kind = match ot {
                Type::List(_) => SeqKind::List,
                Type::Str => SeqKind::Str,
                Type::Bytes => SeqKind::Bytes,
                Type::Error => return err(span),
                Type::Var(_) => {
                    self.error(obj.span, "type annotation needed before slicing");
                    return err(span);
                }
                other => {
                    let msg = format!("cannot slice a value of type {}", self.show(&other));
                    self.error(span, msg);
                    return err(span);
                }
            };
            let s = start.as_ref().map(|s| Box::new(self.check_expr(s, &Type::Int)));
            let e = end.as_ref().map(|e| Box::new(self.check_expr(e, &Type::Int)));
            if *inclusive && e.is_none() {
                self.error(idx.span, "inclusive range needs an end");
            }
            let ty = o.ty.clone();
            return mk(HKind::Slice { obj: Box::new(o), start: s, end: e, inclusive: *inclusive, kind }, ty, span);
        }
        match ot {
            Type::List(t) => {
                let i = self.check_expr(idx, &Type::Int);
                mk(HKind::Index { obj: Box::new(o), index: Box::new(i), map: false }, *t, span)
            }
            Type::Map(k, v) => {
                let i = self.check_expr(idx, &k);
                mk(HKind::Index { obj: Box::new(o), index: Box::new(i), map: true }, *v, span)
            }
            Type::Str => {
                self.infer(idx, None);
                self.error(span, "cannot index a str by position; use a slice like `s[i..j]` or a method such as `char_at`");
                err(span)
            }
            Type::Bytes => {
                self.infer(idx, None);
                self.error(span, "cannot index bytes by position; use a slice or `get`");
                err(span)
            }
            Type::Error => err(span),
            Type::Var(_) => {
                self.error(obj.span, "type annotation needed before indexing");
                err(span)
            }
            other => {
                let msg = format!("cannot index a value of type {}", self.show(&other));
                self.error(span, msg);
                err(span)
            }
        }
    }

    fn check_try(&mut self, inner: &Expr, span: Span) -> HExpr {
        self.check_not_in_defer("`?`", span);
        let h = self.infer(inner, None);
        let t = self.resolve(&h.ty);
        let ctx_kind = self.fs.ctx.last().map(|c| c.kind).unwrap_or(CtxKind::Function);
        let ret = self.fs.ctx.last().map(|c| c.ret.clone()).unwrap_or(Type::Unit);
        match t {
            Type::Named(d, args) if d == self.wk.option => {
                if let CtxKind::TopLevel { .. } = ctx_kind {
                    self.error(span, "`?` on an Option is not allowed at the top level");
                    return err(span);
                }
                let rr = self.resolve(&ret);
                let ok = match rr {
                    Type::Named(rd, _) if rd == self.wk.option => true,
                    Type::Var(_) => {
                        let v = self.fresh();
                        let o = self.option_of(v);
                        self.unify(&ret, &o)
                    }
                    Type::Error => true,
                    _ => false,
                };
                if !ok {
                    let msg =
                        format!("`?` on an Option requires the enclosing function to return an Option, but it returns {}", self.show(&ret));
                    self.error(span, msg);
                }
                mk(HKind::Try { e: Box::new(h), kind: TryKind::Option }, args[0].clone(), span)
            }
            Type::Named(d, args) if d == self.wk.result => {
                let kind = match ctx_kind {
                    CtxKind::TopLevel { main: true } => TryKind::TopLevel,
                    CtxKind::TopLevel { main: false } => {
                        self.error(span, "`?` is not allowed at the top level of an imported module");
                        return err(span);
                    }
                    _ => {
                        let rr = self.resolve(&ret);
                        match rr {
                            Type::Named(rd, rargs) if rd == self.wk.result => {
                                if !self.unify(&rargs[1], &args[1]) {
                                    let msg = format!(
                                        "type mismatch: `?` cannot convert error type {} into {}",
                                        self.show(&args[1]),
                                        self.show(&rargs[1])
                                    );
                                    self.error(span, msg);
                                }
                            }
                            Type::Var(_) => {
                                let v = self.fresh();
                                let r = self.result_of(v, args[1].clone());
                                self.unify(&ret, &r);
                            }
                            Type::Error => {}
                            _ => {
                                let msg = format!(
                                    "`?` on a Result requires the enclosing function to return a Result, but it returns {}",
                                    self.show(&ret)
                                );
                                self.error(span, msg);
                            }
                        }
                        TryKind::Result
                    }
                };
                mk(HKind::Try { e: Box::new(h), kind }, args[0].clone(), span)
            }
            Type::Error => err(span),
            Type::Var(_) => {
                self.error(inner.span, "type annotation needed before `?`");
                err(span)
            }
            other => {
                let msg = format!("`?` can only be applied to Option or Result values, found {}", self.show(&other));
                self.error(span, msg);
                err(span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Control flow

    /// Checks a condition; for `let` conditions the pattern's bindings are
    /// added to the current scope (the caller pushes one).
    fn check_cond(&mut self, c: &Cond) -> (HExpr, Option<HPat>) {
        match c {
            Cond::Expr(e) => (self.check_expr(e, &Type::Bool), None),
            Cond::Let(p, e) => {
                let v = self.infer(e, None);
                let pat = self.check_pattern(p, &v.ty, false);
                (v, Some(pat))
            }
        }
    }

    fn check_if(&mut self, cond: &Cond, then: &Block, else_: Option<&Expr>, hint: Option<&Type>, want: bool, span: Span) -> HExpr {
        self.push_scope();
        let (c, pat) = self.check_cond(cond);
        let value = want && else_.is_some();
        let th = self.check_block(then, if value { hint } else { None }, value);
        self.pop_scope();
        let eh = else_.map(|e| self.infer_inner(e, if value { hint } else { None }, value));
        let ty = if value {
            let eh_ty = eh.as_ref().unwrap().ty.clone();
            self.join(&th.ty, &eh_ty, else_.unwrap().span)
        } else if th.ty.is_never() && eh.as_ref().map(|e| self.resolve(&e.ty).is_never()).unwrap_or(false) {
            Type::Never
        } else {
            Type::Unit
        };
        let kind = match pat {
            Some(p) => HKind::IfLet { pat: p, value: Box::new(c), then: Box::new(th), else_: eh.map(Box::new) },
            None => HKind::If { cond: Box::new(c), then: Box::new(th), else_: eh.map(Box::new) },
        };
        mk(kind, ty, span)
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm], hint: Option<&Type>, want: bool, span: Span) -> HExpr {
        let s = self.infer(scrutinee, None);
        let st = s.ty.clone();
        let mut harms = Vec::new();
        let mut ty: Option<Type> = None;
        let mut all_never = true;
        for arm in arms {
            self.push_scope();
            let pat = self.check_pattern(&arm.pattern, &st, false);
            let guard = arm.guard.as_ref().map(|g| self.check_expr(g, &Type::Bool));
            let body = self.infer_inner(&arm.body, if want { hint.or(ty.as_ref()) } else { None }, want);
            self.pop_scope();
            if !self.resolve(&body.ty).is_never() {
                all_never = false;
            }
            if want {
                ty = Some(match ty {
                    None => body.ty.clone(),
                    Some(t) => self.join(&t, &body.ty, arm.body.span),
                });
            }
            harms.push(HArm { pat, guard, body });
        }
        self.check_exhaustive(&st, &harms, span);
        let ty = if want {
            ty.unwrap_or(Type::Never)
        } else if all_never && !arms.is_empty() {
            Type::Never
        } else {
            Type::Unit
        };
        mk(HKind::Match { scrut: Box::new(s), arms: harms }, ty, span)
    }

    /// How a `for` loop or comprehension clause iterates over `ih`, and the
    /// element type.
    fn iter_plan(&mut self, ih: &HExpr, span: Span) -> (IterPlan, Type) {
        let builtin = |k: IterKind, t: Type| (IterPlan::Builtin(k), t);
        let ty = self.resolve(&ih.ty);
        match ty.clone() {
            Type::Range => builtin(IterKind::Range, Type::Int),
            Type::List(e) => builtin(IterKind::List, *e),
            Type::Map(k, v) => builtin(IterKind::Map, Type::Tuple(vec![*k, *v])),
            Type::Set(e) => builtin(IterKind::Set, *e),
            Type::Named(d, args) if Some(d) == self.iter_struct => {
                // `Iter<T>`: call its `next` function directly.
                let elem = args[0].clone();
                let local = self.hidden_local(ty.clone(), span);
                let obj = Box::new(mk(HKind::Local(local), ty, span));
                let opt = self.option_of(elem.clone());
                let callee = Box::new(mk(HKind::Field { obj, index: 0 }, Type::func(vec![], opt.clone()), span));
                let next = mk(HKind::CallValue { callee, args: vec![] }, opt, span);
                (IterPlan::Lazy { local, next: Box::new(next) }, elem)
            }
            Type::Named(d, args) if d == self.wk.channel => builtin(IterKind::Channel, args[0].clone()),
            Type::Error => builtin(IterKind::List, Type::Error),
            Type::Var(_) => {
                self.error(span, "type annotation needed before iterating");
                builtin(IterKind::List, Type::Error)
            }
            other => match self.iter_trait.filter(|&t| self.implements_trait(&other, t)) {
                // Any other `Iterator<T>`: call its `next` method.
                Some(t) => {
                    let elem = self.fresh();
                    let local = self.hidden_local(ty.clone(), span);
                    let recv = mk(HKind::Local(local), ty.clone(), span);
                    let slot = self.traits[t as usize].methods.iter().position(|m| m.name == "next").unwrap_or(0) as u32;
                    let tr = TraitRef { trait_id: t, args: vec![elem.clone()] };
                    let next = self.call_trait_method(tr, slot, ty, Some(recv), &[], span);
                    (IterPlan::Lazy { local, next: Box::new(next) }, elem)
                }
                None => {
                    let msg = format!("type {} is not iterable (it does not implement `Iterator`)", self.show(&other));
                    self.error(span, msg);
                    builtin(IterKind::List, Type::Error)
                }
            },
        }
    }

    /// The loop over `ih` that binds `pat` to each element and runs `body`.
    fn iter_loop(&mut self, plan: IterPlan, ih: HExpr, pat: HPat, body: HExpr, span: Span) -> HExpr {
        match plan {
            IterPlan::Builtin(kind) => mk(HKind::For { pat, iter: Box::new(ih), kind, body: Box::new(body) }, Type::Unit, span),
            // `{ let it = ih; while let Some(pat) = <next> { body } }`, or a
            // fused pipeline (see `fuse`).
            IterPlan::Lazy { local, next } => {
                if self.can_fuse(&ih) {
                    return self.fuse_for(ih, pat, body, span);
                }
                let it = HPat { kind: HPatKind::Bind(local), ty: ih.ty.clone(), span };
                let init = mk(HKind::Let { pat: it, init: Box::new(ih), else_: None }, Type::Unit, span);
                let some = self.types[self.wk.option as usize].variants()[0].rt_id;
                let pat = HPat { kind: HPatKind::Variant { rt_id: some, args: vec![pat] }, ty: next.ty.clone(), span };
                let lp = mk(HKind::WhileLet { pat, value: next, body: Box::new(body) }, Type::Unit, span);
                mk(HKind::Block(vec![init, lp]), Type::Unit, span)
            }
        }
    }

    /// Enters a loop (checking its label) for the `break`s and `continue`s in its body.
    fn enter_loop(&mut self, label: Option<&ast::Ident>, break_ty: Option<Type>) {
        let ctx = self.fs.ctx.last_mut().unwrap();
        if let Some(l) = label
            && ctx.loops.iter().any(|x| x.label.as_deref() == Some(l.name.as_str()))
        {
            let msg = format!("label `@{}` is already used by an enclosing loop", l.name);
            self.error(l.span, msg);
        }
        let ctx = self.fs.ctx.last_mut().unwrap();
        ctx.loops.push(LoopCtx { break_ty, label: label.map(|l| l.name.clone()), hidden: false });
        self.loop_breaks.push(false);
    }

    /// Enters the loop of a comprehension clause.
    fn enter_hidden_loop(&mut self) {
        self.fs.ctx.last_mut().unwrap().loops.push(LoopCtx { break_ty: None, label: None, hidden: true });
        self.loop_breaks.push(false);
    }

    /// Leaves a loop; returns whether a `break` targets it.
    fn leave_loop(&mut self) -> bool {
        self.fs.ctx.last_mut().unwrap().loops.pop();
        self.loop_breaks.pop().unwrap_or(false)
    }

    /// The loop a `break` or `continue` refers to, as a depth (0 = innermost).
    /// Labels name loops of the current function or lambda only.
    fn loop_target(&mut self, label: Option<&ast::Ident>, what: &str, span: Span) -> Option<u32> {
        let ctx = self.fs.ctx.last().unwrap();
        let (loops, floor) = (&ctx.loops, ctx.in_defer.unwrap_or(0));
        let found = match label {
            None => loops.iter().rposition(|x| !x.hidden),
            Some(l) => loops.iter().rposition(|x| x.label.as_deref() == Some(l.name.as_str())),
        };
        if let Some(i) = found {
            if i < floor {
                self.error(span, format!("`{what}` cannot leave a deferred expression"));
                return None;
            }
            return Some((loops.len() - 1 - i) as u32);
        }
        let Some(l) = label else {
            self.error(span, format!("{what} outside of a loop"));
            return None;
        };
        let outside = self.fs.ctx.iter().rev().skip(1).any(|c| c.loops.iter().any(|x| x.label.as_deref() == Some(l.name.as_str())));
        let msg = if outside {
            format!("`{what} @{}` cannot leave the enclosing lambda or function", l.name)
        } else {
            format!("unknown label `@{}`", l.name)
        };
        self.error(l.span, msg);
        None
    }

    fn check_for(&mut self, label: Option<&ast::Ident>, pattern: &ast::Pattern, iter: &Expr, body: &Block, span: Span) -> HExpr {
        // Literal ranges are iterated without allocating.
        if let ExprKind::Range { start: Some(s), end: Some(e), inclusive } = &iter.kind {
            let sh = self.check_expr(s, &Type::Int);
            let eh = self.check_expr(e, &Type::Int);
            self.push_scope();
            let pat = self.check_pattern(pattern, &Type::Int, false);
            if !self.pattern_irrefutable(&pat) {
                self.error(pattern.span, "refutable pattern in `for` loop");
            }
            self.enter_loop(label, None);
            let b = self.check_block(body, None, false);
            self.leave_loop();
            self.pop_scope();
            return mk(
                HKind::ForRange { pat, start: Box::new(sh), end: Box::new(eh), inclusive: *inclusive, body: Box::new(b) },
                Type::Unit,
                span,
            );
        }
        let ih = self.infer(iter, None);
        let (plan, elem) = self.iter_plan(&ih, iter.span);
        self.push_scope();
        let pat = self.check_pattern(pattern, &elem, false);
        if !self.pattern_irrefutable(&pat) {
            self.error(pattern.span, "refutable pattern in `for` loop");
        }
        self.enter_loop(label, None);
        let b = self.check_block(body, None, false);
        self.leave_loop();
        self.pop_scope();
        self.iter_loop(plan, ih, pat, b, span)
    }

    fn check_comprehension(
        &mut self,
        a: Option<&Expr>,
        b: Option<&Expr>,
        clauses: &[CompClause],
        hint: Option<&Type>,
        span: Span,
    ) -> HExpr {
        let is_map = b.is_some();
        let (acc_ty, elem_hint, val_hint) = if is_map {
            let (k, v) = match hint.map(|h| self.resolve(h)) {
                Some(Type::Map(k, v)) => (*k, *v),
                _ => (self.fresh(), self.fresh()),
            };
            (Type::map(k.clone(), v.clone()), Some(k), Some(v))
        } else {
            let e = match hint.map(|h| self.resolve(h)) {
                Some(Type::List(e)) => *e,
                _ => self.fresh(),
            };
            (Type::list(e.clone()), Some(e), None)
        };
        let acc = self.hidden_local(acc_ty.clone(), span);
        self.push_scope();
        let inner = self.comp_clauses(clauses, a.unwrap(), b, acc, elem_hint.as_ref().unwrap(), val_hint.as_ref(), span);
        self.pop_scope();
        if is_map {
            self.require_bound(elem_hint.as_ref().unwrap(), Bound::Hash, span);
        }
        let init = if is_map { mk(HKind::Map(vec![]), acc_ty.clone(), span) } else { mk(HKind::List(vec![]), acc_ty.clone(), span) };
        let stmts = vec![
            mk(
                HKind::Let { pat: HPat { kind: HPatKind::Bind(acc), ty: acc_ty.clone(), span }, init: Box::new(init), else_: None },
                Type::Unit,
                span,
            ),
            inner,
            mk(HKind::Local(acc), acc_ty.clone(), span),
        ];
        mk(HKind::Block(stmts), acc_ty, span)
    }

    #[allow(clippy::too_many_arguments)]
    fn comp_clauses(
        &mut self,
        clauses: &[CompClause],
        a: &Expr,
        b: Option<&Expr>,
        acc: LocalId,
        et: &Type,
        vt: Option<&Type>,
        span: Span,
    ) -> HExpr {
        let Some((first, rest)) = clauses.split_first() else {
            let ah = self.check_expr(a, et);
            return match (b, vt) {
                (Some(b), Some(vt)) => {
                    let bh = self.check_expr(b, vt);
                    mk(HKind::InsertLocal { map: acc, key: Box::new(ah), value: Box::new(bh) }, Type::Unit, span)
                }
                _ => mk(HKind::PushLocal { list: acc, value: Box::new(ah) }, Type::Unit, span),
            };
        };
        match first {
            CompClause::If(c) => {
                let ch = self.check_expr(c, &Type::Bool);
                let inner = self.comp_clauses(rest, a, b, acc, et, vt, span);
                mk(HKind::If { cond: Box::new(ch), then: Box::new(inner), else_: None }, Type::Unit, span)
            }
            CompClause::For(p, it) => {
                if let ExprKind::Range { start: Some(s), end: Some(e), inclusive } = &it.kind {
                    let sh = self.check_expr(s, &Type::Int);
                    let eh = self.check_expr(e, &Type::Int);
                    self.push_scope();
                    let pat = self.check_pattern(p, &Type::Int, false);
                    self.enter_hidden_loop();
                    let inner = self.comp_clauses(rest, a, b, acc, et, vt, span);
                    self.leave_loop();
                    self.pop_scope();
                    return mk(
                        HKind::ForRange { pat, start: Box::new(sh), end: Box::new(eh), inclusive: *inclusive, body: Box::new(inner) },
                        Type::Unit,
                        span,
                    );
                }
                let ih = self.infer(it, None);
                let (plan, elem) = self.iter_plan(&ih, it.span);
                self.push_scope();
                let pat = self.check_pattern(p, &elem, false);
                if !self.pattern_irrefutable(&pat) {
                    self.error(p.span, "refutable pattern in comprehension");
                }
                self.enter_hidden_loop();
                let inner = self.comp_clauses(rest, a, b, acc, et, vt, span);
                self.leave_loop();
                self.pop_scope();
                self.iter_loop(plan, ih, pat, inner, span)
            }
        }
    }

    fn check_lambda(&mut self, params: &[ast::LambdaParam], body: &Expr, hint: Option<&Type>, span: Span) -> HExpr {
        let hinted = match hint.map(|h| self.resolve(h)) {
            Some(Type::Fn(ps, r)) if ps.len() == params.len() => Some((ps, *r)),
            _ => None,
        };
        let ret = hinted.as_ref().map(|(_, r)| r.clone()).unwrap_or_else(|| self.fresh());
        self.fs.next_lambda += 1;
        let id = self.fs.next_lambda;
        self.fs.ctx.push(FnCtx { ret: ret.clone(), kind: CtxKind::Lambda, loops: vec![], lambda: id, in_defer: None });
        self.push_scope();
        let mut locals = Vec::new();
        let mut ptys = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let t = match (&p.ty, &hinted) {
                (Some(t), h) => {
                    let t = self.resolve_annotation(t);
                    if let Some((hps, _)) = h {
                        self.expect_type(&hps[i], &t, p.name.span);
                    }
                    t
                }
                (None, Some((hps, _))) => hps[i].clone(),
                (None, None) => self.fresh(),
            };
            ptys.push(t.clone());
            locals.push(self.declare_local(&p.name.name, t, false, p.name.span, true));
        }
        let b = self.infer(body, Some(&ret));
        let rt = self.resolve(&ret);
        if !b.ty.is_never() && !matches!(self.resolve(&b.ty), Type::Never) {
            if rt == Type::Unit {
                // A lambda expected to return unit may end in any expression.
                self.unify(&ret, &Type::Unit);
            } else {
                self.expect_type(&ret, &b.ty, body.span);
            }
        }
        self.pop_scope();
        self.fs.ctx.pop();
        let body_h = if self.resolve(&ret) == Type::Unit && !matches!(self.resolve(&b.ty), Type::Unit | Type::Never) {
            let bspan = b.span;
            mk(HKind::Block(vec![b, unit(bspan)]), Type::Unit, bspan)
        } else {
            b
        };
        let fty = Type::func(ptys.clone(), ret.clone());
        let names: Vec<String> = params.iter().map(|p| p.name.name.clone()).collect();
        let _ = names;
        mk(
            HKind::Lambda(Box::new(HLambda {
                id,
                name: "<lambda>".into(),
                params: locals,
                body: body_h,
                doc: None,
                signature: String::new(),
                self_local: None,
            })),
            fty,
            span,
        )
    }
}

fn is_lambda(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Lambda { .. } => true,
        ExprKind::Paren(x) => is_lambda(x),
        _ => false,
    }
}

fn arith_text(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Rem => "%",
        ArithOp::Pow => "**",
        ArithOp::BitAnd => "&",
        ArithOp::BitOr => "|",
        ArithOp::BitXor => "^",
        ArithOp::Shl => "<<",
        ArithOp::Shr => ">>",
    }
}

/// Parses `[[fill]align][sign][0][width][.precision][type]`.
pub fn parse_format_spec(s: &str) -> Result<FormatSpec, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut spec = FormatSpec { fill: ' ', align: None, sign: false, zero: false, width: None, precision: None, kind: None };
    let is_align = |c: char| matches!(c, '<' | '>' | '^');
    if chars.len() >= 2 && is_align(chars[1]) {
        spec.fill = chars[0];
        spec.align = Some(chars[1]);
        i = 2;
    } else if !chars.is_empty() && is_align(chars[0]) {
        spec.align = Some(chars[0]);
        i = 1;
    }
    if i < chars.len() && chars[i] == '+' {
        spec.sign = true;
        i += 1;
    }
    if i < chars.len() && chars[i] == '0' {
        spec.zero = true;
        i += 1;
    }
    let start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i > start {
        let w: String = chars[start..i].iter().collect();
        spec.width = Some(w.parse().map_err(|_| format!("invalid format spec `{s}`: width too large"))?);
    }
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let ps = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == ps {
            return Err(format!("invalid format spec `{s}`: expected precision after `.`"));
        }
        let p: String = chars[ps..i].iter().collect();
        spec.precision = Some(p.parse().map_err(|_| format!("invalid format spec `{s}`: precision too large"))?);
    }
    if i < chars.len() {
        match chars[i] {
            'x' | 'X' | 'b' | 'o' | 'e' | '?' => {
                spec.kind = Some(chars[i]);
                i += 1;
            }
            _ => return Err(format!("invalid format spec `{s}`")),
        }
    }
    if i != chars.len() {
        return Err(format!("invalid format spec `{s}`"));
    }
    if spec.width.is_some_and(|w| w > 1 << 20) || spec.precision.is_some_and(|p| p > 1 << 10) {
        return Err(format!("invalid format spec `{s}`: width or precision too large"));
    }
    Ok(spec)
}
