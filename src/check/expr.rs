//! Type checking of statements and expressions.

use super::*;
use crate::syntax::ast::{Arg, AssignOp, BinOp, Block, CompClause, Cond, Expr, ExprKind, Stmt, StmtKind, StrSeg, UnOp};

/// What a name or path in expression position refers to as a namespace.
#[derive(Clone, Debug)]
pub(crate) enum Ns {
    Module(ModId),
    Type(TypeId),
    Trait(TraitId),
}

/// The arguments of a call, split into positional and named ones.
#[derive(Default)]
pub(crate) struct CallArgs<'e> {
    pub(crate) positional: Vec<&'e Expr>,
    pub(crate) named: Vec<(&'e ast::Ident, &'e Expr)>,
}

impl<'e> CallArgs<'e> {
    /// `args`, after `first` if given (the left side of `|>`).
    pub(crate) fn new(first: Option<&'e Expr>, args: &'e [Arg]) -> CallArgs<'e> {
        let mut positional: Vec<&Expr> = first.into_iter().collect();
        let mut named = Vec::new();
        for a in args {
            match &a.name {
                Some(n) => named.push((n, &a.value)),
                None => positional.push(&a.value),
            }
        }
        CallArgs { positional, named }
    }

    fn len(&self) -> usize {
        self.positional.len() + self.named.len()
    }

    fn exprs(&self) -> impl Iterator<Item = &'e Expr> + '_ {
        self.positional.iter().copied().chain(self.named.iter().map(|&(_, e)| e))
    }
}

/// What is called, for binding arguments to parameters.
#[derive(Clone, Copy)]
enum Callee {
    Fn(FnId),
    Variant(TypeDefId, u32),
    /// A function value: positional arguments only.
    Value,
}

/// How a `for` loop or comprehension clause iterates.
pub(crate) enum IterPlan {
    /// Ranges, lists, maps, sets and channels, with dedicated instructions.
    Builtin(IterKind),
    /// An `Iterator`: the value is kept in a hidden local, and `next` (a call
    /// of type `T?` on it) runs before each iteration. For an `Iter`, the
    /// local holds its `next` function instead (`next_fn`; the field is
    /// immutable, so it is loaded once).
    Lazy { local: LocalId, next: Box<HExpr>, next_fn: bool },
}

/// An instantiated function signature.
pub(crate) struct Instance {
    pub fresh: Vec<TypeId>,
    pub params: Vec<TypeId>,
    pub ret: TypeId,
    /// Evidence needed for the hidden dictionary parameters.
    pub dicts: Vec<(TypeId, TraitRef)>,
}

pub(crate) fn mk(kind: HKind, ty: TypeId, span: Span) -> HExpr {
    HExpr { kind, ty, span }
}

fn unit(span: Span) -> HExpr {
    mk(HKind::Unit, TypeId::UNIT, span)
}

fn err(span: Span) -> HExpr {
    mk(HKind::Error, TypeId::ERROR, span)
}

impl Checker {
    // ------------------------------------------------------------------
    // Entry points

    /// Checks `e` against `expected`, reporting a mismatch.
    pub(crate) fn check_expr(&mut self, e: &Expr, expected: TypeId) -> HExpr {
        let h = self.infer_inner(e, Some(expected), true);
        if !self.coerce(expected, h.ty) {
            let (want, got) = self.show_pair(expected, h.ty);
            let msg = format!("type mismatch: expected {want}, found {got}");
            self.error(e.span, msg);
        }
        h
    }

    pub(crate) fn infer(&mut self, e: &Expr, hint: Option<TypeId>) -> HExpr {
        self.infer_inner(e, hint, true)
    }

    pub(crate) fn check_block(&mut self, b: &Block, hint: Option<TypeId>, want: bool) -> HExpr {
        // Bindings inside nested blocks are locals, even at module level.
        let saved_top = std::mem::replace(&mut self.fs.top_level, false);
        let h = self.check_block_inner(b, hint, want);
        self.fs.top_level = saved_top;
        h
    }

    fn check_block_inner(&mut self, b: &Block, hint: Option<TypeId>, want: bool) -> HExpr {
        self.push_scope();
        let mut out = Vec::new();
        let n = b.stmts.len();
        let mut ty = TypeId::UNIT;
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
                ty = TypeId::UNIT;
                continue;
            }
            let last = i + 1 == n;
            if last
                && want
                && let StmtKind::Expr(e) = &s.kind
            {
                let h = self.infer_inner(e, hint, true);
                // A unit-typed statement after a diverging one is not a value.
                ty = if diverged && self.resolve(h.ty) == TypeId::UNIT { TypeId::NEVER } else { h.ty };
                out.push(h);
                i += 1;
                continue;
            }
            let hs = self.check_stmt(s);
            if let Some(h) = hs.last() {
                if self.resolve(h.ty).is_never() {
                    diverged = true;
                }
                ty = if h.ty.is_never() || diverged { TypeId::NEVER } else { TypeId::UNIT };
            }
            out.extend(hs);
            i += 1;
        }
        self.pop_scope();
        if !want {
            // The value of the block is discarded: make it unit.
            if !matches!(out.last().map(|h| self.resolve(h.ty)), Some(TypeId::UNIT) | Some(TypeId::NEVER) | None) {
                out.push(unit(b.span));
            }
            if !ty.is_never() {
                ty = TypeId::UNIT;
            }
        }
        mk(HKind::Block(out), ty, b.span)
    }

    pub(crate) fn check_stmt(&mut self, s: &Stmt) -> Vec<HExpr> {
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
                    self.fs_unused.push((h.ty, e.span));
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
            StmtKind::Use { name, ty, value } => self.check_use(s, name, ty.as_ref(), value),
        }
    }

    /// `use name = value`: `let name = value` (a local, also at the top
    /// level), then `defer Close.close(name)`. The value's type must
    /// implement `Close`; the trait's method is called even if the type has
    /// an inherent `close`.
    fn check_use(&mut self, s: &Stmt, name: &ast::Ident, ty: Option<&ast::TypeExpr>, value: &Expr) -> Vec<HExpr> {
        let annotated = ty.map(|t| self.resolve_annotation(t));
        let init = match annotated {
            Some(t) => self.check_expr(value, t),
            None => self.infer(value, None),
        };
        let vty = annotated.unwrap_or(init.ty);
        let rty = self.resolve(vty);
        let close = self.close_trait.filter(|&t| self.implements_trait(rty, t));
        if close.is_none() {
            self.not_closeable(rty, value.span);
        }
        if self.fs.top_level && self.in_session() {
            let g = self.globals.len() as GlobalId;
            let sym = self.sym(&name.name);
            self.globals.push(GlobalInfo {
                name: sym,
                module: self.cur_mod,
                ty: vty,
                mutable: false,
                is_pub: false,
                span: name.span,
                doc: None,
            });
            if name.name != "_" {
                self.define(sym, Def::Global(g), false, name.span);
            }
            self.global_fact(g);
            return vec![mk(
                HKind::Assign { place: Place::Global(g), op: None, op_ty: TypeId::UNIT, value: Box::new(init) },
                TypeId::UNIT,
                s.span,
            )];
        }
        let local = if name.name == "_" {
            self.hidden_local(vty, name.span)
        } else {
            let l = self.declare_local(&name.name, vty, false, name.span, false);
            // Read by the deferred `close`.
            self.fs.data[l as usize].used = true;
            l
        };
        let pat = HPat { kind: HPatKind::Bind(local), ty: vty, span: name.span };
        let bind = mk(HKind::Let { pat, init: Box::new(init), else_: None }, TypeId::UNIT, s.span);
        let Some(t) = close else {
            return vec![bind];
        };
        let recv = mk(HKind::Local(local), vty, name.span);
        let slot = self.traits[t as usize].slot(Symbol::CLOSE).unwrap_or(0);
        let tr = TraitRef { trait_id: t, args: self.arena.intern_list(&[]) };
        let call = self.call_trait_method(tr, slot, rty, Some(recv), &CallArgs::default(), s.span);
        vec![bind, mk(HKind::Defer(Box::new(call)), TypeId::UNIT, s.span)]
    }

    /// A top-level `return`, which ends the program or the session input. It
    /// returns a value of any type; an `Err` fails with its error, as `?` does.
    fn check_top_return(&mut self, v: Option<&Expr>, main: bool, span: Span) -> HExpr {
        if !main {
            self.error(span, "`return` is not allowed at the top level of an imported module");
        }
        let Some(v) = v else { return mk(HKind::Return(None), TypeId::NEVER, span) };
        let h = self.infer(v, None);
        self.cell_returns = true;
        let t = self.resolve(h.ty);
        let Ty::Named(d, args) = self.ty(t) else { return mk(HKind::Return(Some(Box::new(h))), TypeId::NEVER, span) };
        if d != self.wk.result {
            return mk(HKind::Return(Some(Box::new(h))), TypeId::NEVER, span);
        }
        // `return Err(e)` leaves the `Ok` type open.
        let ok = self.arena.elem(args, 0);
        if matches!(self.rty(ok), Ty::Var(_)) {
            self.unify(ok, TypeId::UNIT);
        }
        let local = self.hidden_local(t, span);
        let pat = HPat { kind: HPatKind::Bind(local), ty: t, span };
        let bind = mk(HKind::Let { pat, init: Box::new(h), else_: None }, TypeId::UNIT, span);
        let read = || Box::new(mk(HKind::Local(local), t, span));
        let fail = mk(HKind::Try { e: read(), kind: TryKind::TopLevel }, ok, span);
        let ret = mk(HKind::Return(Some(read())), TypeId::NEVER, span);
        mk(HKind::Block(vec![bind, fail, ret]), TypeId::NEVER, span)
    }

    /// Reports `use` on a value whose type does not implement `Close`.
    fn not_closeable(&mut self, t: TypeId, span: Span) {
        match self.ty(t) {
            Ty::Error => {}
            Ty::Var(_) => self.infer_error(span, "type annotation needed: a `use` binding's type must be known where it is bound"),
            ty => {
                let ok = match ty {
                    Ty::Named(d, args) if d == self.wk.result => Some(self.arena.elem(args, 0)),
                    _ => None,
                };
                let hint = match ok {
                    Some(ok) if self.close_trait.is_some_and(|c| self.implements_trait(ok, c)) => {
                        "; add `?` to bind the value of `Ok` (and return the error)"
                    }
                    _ => "",
                };
                let msg = format!("`use` needs a value whose type implements `Close`, found {}{hint}", self.show(t));
                self.error(span, msg);
            }
        }
    }

    /// `defer e`: `e` must have type `()` and cannot leave the deferred
    /// expression (with `return`, `?`, or `break`/`continue` to an outer loop).
    fn check_defer(&mut self, e: &Expr, span: Span) -> HExpr {
        let ctx = self.fs.ctx.last_mut().unwrap();
        let saved = ctx.in_defer.replace(ctx.loops.len());
        let saved_top = std::mem::replace(&mut self.fs.top_level, false);
        let h = self.infer(e, Some(TypeId::UNIT));
        self.fs.top_level = saved_top;
        self.fs.ctx.last_mut().unwrap().in_defer = saved;
        let t = self.resolve(h.ty);
        if !t.is_never() && !self.unify(TypeId::UNIT, t) {
            let msg = format!("a deferred expression must have type (), found {}; discard the value with `let _ = ...`", self.show(t));
            self.error(e.span, msg);
        }
        mk(HKind::Defer(Box::new(h)), TypeId::UNIT, span)
    }

    /// Reports `return` or `?` inside a deferred expression.
    fn check_not_in_defer(&mut self, what: &str, span: Span) {
        if self.fs.ctx.last().is_some_and(|c| c.in_defer.is_some()) {
            self.error(span, format!("{what} cannot be used in a deferred expression"));
        }
    }

    fn resolve_annotation(&mut self, t: &ast::TypeExpr) -> TypeId {
        let generics = self.fs.generics.clone();
        let self_ty = self.fs.self_ty;
        self.resolve_type_in(t, &generics, self_ty)
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
        let doc = match &s.kind {
            StmtKind::Let { doc, .. } => doc.as_deref().map(Arc::from),
            _ => None,
        };
        let annotated = ty.map(|t| self.resolve_annotation(t));
        let init = match annotated {
            Some(t) => self.check_expr(value, t),
            None => self.infer(value, None),
        };
        let vty = annotated.unwrap_or(init.ty);
        // Top-level bindings only need global slots if functions (or other
        // modules, or later session inputs) can see them; otherwise they are
        // initializer locals.
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
            let name = self.sym(&id.name);
            self.globals.push(GlobalInfo { name, module: self.cur_mod, ty: vty, mutable, is_pub, span: id.span, doc });
            self.define(name, Def::Global(g), is_pub, id.span);
            self.global_fact(g);
            return vec![mk(
                HKind::Assign { place: Place::Global(g), op: None, op_ty: TypeId::UNIT, value: Box::new(init) },
                TypeId::UNIT,
                s.span,
            )];
        }
        let hp = self.check_pattern(pattern, vty, mutable);
        let refutable = !self.pattern_irrefutable(&hp);
        let else_h = match else_block {
            Some(b) => {
                // Bindings are not visible in the else block.
                let names = self.pattern_names(&hp);
                let hidden: Vec<(Symbol, LocalId)> = names.into_iter().filter_map(|n| Some((n, self.fs.scopes.remove(n)?))).collect();
                let h = self.check_block(b, None, false);
                for (n, id) in hidden {
                    self.fs.scopes.declare(n, id);
                }
                if !self.resolve(h.ty).is_never() {
                    self.error(b.span, "the `else` block of `let ... else` must diverge (return, break, continue or fail)");
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
        let bound = self.pattern_bindings(&hp);
        let mut out = vec![mk(HKind::Let { pat: hp, init: Box::new(init), else_: else_h }, TypeId::UNIT, s.span)];
        if top {
            // Bindings of a top-level destructuring pattern become globals.
            for &(n, _) in &bound {
                self.fs.scopes.remove(n);
            }
            for (name, l) in bound {
                let lty = self.fs.data[l as usize].ty;
                let g = self.globals.len() as GlobalId;
                let span = self.fs.locals[l as usize].span;
                self.globals.push(GlobalInfo { name, module: self.cur_mod, ty: lty, mutable, is_pub, span, doc: doc.clone() });
                self.define(name, Def::Global(g), is_pub, span);
                self.fs.data[l as usize].used = true;
                self.global_fact(g);
                out.push(mk(
                    HKind::Assign {
                        place: Place::Global(g),
                        op: None,
                        op_ty: TypeId::UNIT,
                        value: Box::new(mk(HKind::Local(l), lty, span)),
                    },
                    TypeId::UNIT,
                    span,
                ));
            }
        } else {
            for (_, l) in bound {
                let t = self.fs.data[l as usize].ty;
                let span = self.fs.locals[l as usize].span;
                self.fs.infer_checks.push((t, span));
            }
        }
        out
    }

    fn check_assign(&mut self, target: &Expr, op: AssignOp, value: &Expr, span: Span) -> HExpr {
        // The place assigned, unless it is invalid (an error is reported).
        let (place, tty): (Option<Place>, TypeId) = match &target.kind {
            ExprKind::Ident(id) => {
                if let Some(l) = self.find_local(&id.name) {
                    self.fs.data[l as usize].assigned = true;
                    if !self.fs.locals[l as usize].mutable {
                        self.error(target.span, format!("cannot assign to immutable binding `{}`", id.name));
                    }
                    (Some(Place::Local(l)), self.fs.data[l as usize].ty)
                } else {
                    match self.lookup_global(&id.name) {
                        Some(Def::Global(g)) => {
                            let gi = &self.globals[g as usize];
                            let (mutable, foreign, ty) = (gi.mutable, gi.module != self.cur_mod, gi.ty);
                            if !mutable {
                                self.error(target.span, format!("cannot assign to immutable binding `{}`", id.name));
                            } else if foreign {
                                self.error(target.span, format!("cannot assign to `{}` from another module", id.name));
                            }
                            self.mark_global_assigned(g);
                            (Some(Place::Global(g)), ty)
                        }
                        Some(_) => {
                            self.error(target.span, format!("cannot assign to `{}`", id.name));
                            (None, TypeId::ERROR)
                        }
                        None => {
                            self.error(target.span, format!("undefined name `{}`", id.name));
                            (None, TypeId::ERROR)
                        }
                    }
                }
            }
            ExprKind::Field(obj, name) => {
                if let Some(Ns::Module(m)) = self.resolve_ns(obj) {
                    if let Some(Def::Global(g)) = self.module_member(m, &name.name, name.span) {
                        let ty = self.globals[g as usize].ty;
                        self.error(target.span, format!("cannot assign to `{}` from another module", name.name));
                        (Some(Place::Global(g)), ty)
                    } else {
                        self.error(target.span, "cannot assign to this expression");
                        (None, TypeId::ERROR)
                    }
                } else {
                    let o = self.infer(obj, None);
                    let ot = self.resolve(o.ty);
                    match self.struct_field(ot, &name.name) {
                        Some((idx, fty, mutable)) => {
                            if !mutable {
                                self.error(name.span, format!("cannot assign to immutable field `{}`", name.name));
                            }
                            (Some(Place::Field { obj: Box::new(o), index: idx }), fty)
                        }
                        None => {
                            if ot != TypeId::ERROR {
                                let msg = if matches!(self.ty(ot), Ty::Var(_)) {
                                    "type annotation needed before field access".to_string()
                                } else {
                                    format!("no field `{}` on type {}", name.name, self.show(ot))
                                };
                                self.error(name.span, msg);
                            }
                            (None, TypeId::ERROR)
                        }
                    }
                }
            }
            ExprKind::Index(obj, idx) => {
                let o = self.infer(obj, None);
                let ot = self.resolve(o.ty);
                match self.ty(ot) {
                    Ty::List(t) => {
                        let i = self.check_expr(idx, TypeId::INT);
                        (Some(Place::Index { obj: Box::new(o), index: Box::new(i), map: false }), t)
                    }
                    Ty::Map(k, v) => {
                        let i = self.check_expr(idx, k);
                        (Some(Place::Index { obj: Box::new(o), index: Box::new(i), map: true }), v)
                    }
                    Ty::Error => (None, TypeId::ERROR),
                    Ty::Var(_) => {
                        self.infer_error(obj.span, "type annotation needed before indexing");
                        (None, TypeId::ERROR)
                    }
                    _ => {
                        let msg = format!("cannot assign to an element of {}", self.show(ot));
                        self.error(target.span, msg);
                        (None, TypeId::ERROR)
                    }
                }
            }
            ExprKind::TupleField(..) => {
                self.error(target.span, "cannot assign to a tuple element; tuples are immutable");
                (None, TypeId::ERROR)
            }
            _ => {
                self.error(target.span, "cannot assign to this expression");
                (None, TypeId::ERROR)
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
            None => self.check_expr(value, tty),
            Some(aop) => {
                let r = self.infer(value, Some(tty));
                let res = self.arith_type(aop, tty, r.ty, span);
                self.expect_type(tty, res, span);
                r
            }
        };
        match place {
            Some(place) => mk(HKind::Assign { place, op: arith, op_ty: tty, value: Box::new(v) }, TypeId::UNIT, span),
            None => mk(HKind::Error, TypeId::UNIT, span),
        }
    }

    /// Records a global of the module being checked for the linter.
    fn global_fact(&mut self, g: GlobalId) {
        if !self.linting() {
            return;
        }
        let gi = &self.globals[g as usize];
        self.lint.locals.push(LocalFact {
            span: gi.span,
            name: self.syms.str(gi.name).to_string(),
            used: gi.is_pub,
            mutable: gi.mutable,
            assigned: false,
            ty: String::new(),
            is_param: false,
            global: true,
            func: u32::MAX,
            variants: Vec::new(),
        });
        self.global_facts.insert(g, self.lint.locals.len() - 1);
    }

    fn mark_global_assigned(&mut self, g: GlobalId) {
        if let Some(&i) = self.global_facts.get(&g) {
            self.lint.locals[i].assigned = true;
        }
    }

    pub(crate) fn mark_global_used(&mut self, g: GlobalId) {
        if let Some(&i) = self.global_facts.get(&g) {
            self.lint.locals[i].used = true;
        }
    }

    // ------------------------------------------------------------------
    // Local functions

    fn check_local_fns(&mut self, group: &[&ast::FnDecl]) -> HExpr {
        let mut locals = Vec::new();
        let mut sigs = Vec::new();
        for f in group {
            let mut scope = self.fs.generics.clone();
            let self_ty = self.fs.self_ty;
            let own = self.declare_params(&f.generics, &scope, self_ty);
            for (g, &p) in f.generics.iter().zip(&own) {
                scope.push((self.params[p as usize].name, p));
                if !self.params[p as usize].traits.is_empty() {
                    self.error(g.name.span, "local functions cannot have trait bounds; declare the function at the top level");
                    self.params[p as usize].traits.clear();
                }
            }
            let mut ptys = Vec::new();
            self.check_param_names(f.params.iter().map(|p| &p.name));
            for p in &f.params {
                let t = match &p.ty {
                    Some(t) => self.resolve_type_in(t, &scope, self_ty),
                    None => {
                        self.error(p.name.span, "`self` is only allowed in methods");
                        TypeId::ERROR
                    }
                };
                if p.default.is_some() {
                    self.error(p.name.span, "local functions cannot have default parameters");
                }
                ptys.push(t);
            }
            let ret = f.ret.as_ref().map(|t| self.resolve_type_in(t, &scope, self_ty)).unwrap_or(TypeId::UNIT);
            let fty = self.fn_ty(&ptys, ret);
            let l = self.declare_local(&f.name.name, fty, false, f.name.span, false);
            self.fs.locals[l as usize].cell = true;
            self.fs.data[l as usize].scheme = own.clone();
            self.fs.data[l as usize].used = true;
            locals.push(l);
            sigs.push((ptys, ret, scope, fty));
        }
        let mut lambdas = Vec::new();
        for ((f, (ptys, ret, scope, fty)), &self_local) in group.iter().zip(sigs).zip(&locals) {
            let saved_generics = std::mem::replace(&mut self.fs.generics, scope);
            let id = self.new_lambda();
            self.fs.ctx.push(FnCtx { ret, kind: CtxKind::Lambda, loops: vec![], lambda: id, in_defer: None });
            self.push_scope();
            let mut params = Vec::new();
            for (p, &t) in f.params.iter().zip(&ptys) {
                params.push(self.declare_local(&p.name.name, t, false, p.name.span, true));
            }
            let body_ast = f.body.as_ref().unwrap();
            let want = ret != TypeId::UNIT;
            let body = self.check_block(body_ast, if want { Some(ret) } else { None }, want);
            if want {
                self.expect_type(ret, body.ty, self.block_tail_span(body_ast));
            }
            self.pop_scope();
            self.fs.ctx.pop();
            self.fs.generics = saved_generics;
            lambdas.push(mk(
                HKind::Lambda(Box::new(HLambda { id, name: f.name.name.clone(), params, body, self_local: Some(self_local) })),
                fty,
                f.span,
            ));
        }
        mk(HKind::LocalFns { locals, lambdas }, TypeId::UNIT, group[0].span)
    }

    // ------------------------------------------------------------------
    // Expressions

    /// Checks an expression within the compile budget: each expression costs
    /// fuel, and its type may not grow beyond `MAX_TYPE_SIZE` nodes.
    fn infer_inner(&mut self, e: &Expr, hint: Option<TypeId>, want: bool) -> HExpr {
        if !self.budget.charge(1) {
            return err(e.span);
        }
        let mut h = self.infer_kind(e, hint, want);
        self.limit_type_size(&mut h.ty, e.span);
        h
    }

    fn infer_kind(&mut self, e: &Expr, hint: Option<TypeId>, want: bool) -> HExpr {
        let span = e.span;
        match &e.kind {
            ExprKind::Error => err(span),
            ExprKind::Unit => unit(span),
            ExprKind::Bool(b) => mk(HKind::Bool(*b), TypeId::BOOL, span),
            ExprKind::Int(i) => mk(HKind::Int(*i), TypeId::INT, span),
            ExprKind::IntOverflow(_) => {
                self.error(span, "integer literal out of range");
                mk(HKind::Int(0), TypeId::INT, span)
            }
            ExprKind::Float(f) => mk(HKind::Float(*f), TypeId::FLOAT, span),
            ExprKind::Duration(d) => mk(HKind::Dur(*d), TypeId::DURATION, span),
            ExprKind::Bytes(b) => mk(HKind::Bytes(b.clone()), TypeId::BYTES, span),
            ExprKind::Str(segs) => self.check_string(segs, span),
            ExprKind::Ident(id) => self.check_ident(id, hint),
            ExprKind::SelfValue => match self.fs.scopes.find(Symbol::SELF) {
                Some(l) => {
                    self.use_local(l);
                    mk(HKind::Local(l), self.fs.data[l as usize].ty, span)
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
            ExprKind::Macro(m) => self.infer_inner(&m.expansion, hint, want),
            ExprKind::ModuleRef(name) => {
                self.error(span, format!("`{name}` is a module, not a value"));
                err(span)
            }
            ExprKind::List(items) => {
                let elem_hint = match hint.map(|h| self.rty(h)) {
                    Some(Ty::List(t)) => Some(t),
                    _ => None,
                };
                let elem = self.fresh();
                let mut hs = Vec::new();
                for it in items {
                    let h = if is_lambda(it) { elem_hint } else { None };
                    let x = self.infer(it, h.or(Some(elem)));
                    self.expect_elem(elem, x.ty, it.span, "elements of a list");
                    hs.push(x);
                }
                let t = self.list_ty(elem);
                mk(HKind::List(hs), t, span)
            }
            ExprKind::Repeat(elem, count) => {
                let elem_hint = match hint.map(|h| self.rty(h)) {
                    Some(Ty::List(t)) => Some(t),
                    _ => None,
                };
                let eh = self.infer(elem, elem_hint);
                let ch = self.check_expr(count, TypeId::INT);
                let t = self.list_ty(eh.ty);
                mk(HKind::Repeat { elem: Box::new(eh), count: Box::new(ch) }, t, span)
            }
            ExprKind::Map(pairs) => {
                let vhint = match hint.map(|h| self.rty(h)) {
                    Some(Ty::Map(_, v)) => Some(v),
                    _ => None,
                };
                let (kh, vh) = (self.fresh(), self.fresh());
                let mut hs = Vec::new();
                for (k, v) in pairs {
                    let kk = self.check_expr(k, kh);
                    let h = if is_lambda(v) { vhint } else { None };
                    let vv = self.infer(v, h.or(Some(vh)));
                    self.expect_elem(vh, vv.ty, v.span, "values of a map");
                    hs.push((kk, vv));
                }
                if let Some((k, _)) = pairs.first() {
                    self.require_bound(kh, Bound::Hash, k.span);
                } else {
                    self.fs.obligations.push((kh, Bound::Hash, span));
                }
                let t = self.map_ty(kh, vh);
                mk(HKind::Map(hs), t, span)
            }
            ExprKind::ListComp(elem, clauses) => self.check_comprehension(Some(elem), None, clauses, hint, span),
            ExprKind::MapComp(k, v, clauses) => self.check_comprehension(Some(k), Some(v), clauses, hint, span),
            ExprKind::Tuple(items) => {
                let hints: Option<Vec<TypeId>> = match hint.map(|h| self.rty(h)) {
                    Some(Ty::Tuple(ts)) if self.arena.list_len(ts) == items.len() => Some(self.elems(ts)),
                    _ => None,
                };
                let mut hs = Vec::new();
                for (i, it) in items.iter().enumerate() {
                    let h = hints.as_ref().map(|ts| ts[i]);
                    hs.push(self.infer(it, h));
                }
                let tys: Vec<TypeId> = hs.iter().map(|h| h.ty).collect();
                let t = self.tuple_ty(&tys);
                mk(HKind::Tuple(hs), t, span)
            }
            ExprKind::StructLit { path, fields, base } => self.check_struct_lit(path, fields, base.as_deref(), span),
            ExprKind::Unary(op, inner) => self.check_unary(*op, inner, span),
            ExprKind::Binary(op, l, r) => self.check_binary(*op, l, r, span),
            ExprKind::Pipe(l, r) => self.check_pipe(l, r, span),
            ExprKind::Coalesce(l, r) => {
                let lh = self.infer(l, None);
                let lt = self.resolve(lh.ty);
                match self.ty(lt) {
                    Ty::Named(d, args) if d == self.wk.option || d == self.wk.result => {
                        let inner = self.arena.elem(args, 0);
                        let rh = self.check_expr(r, inner);
                        let result = d == self.wk.result;
                        mk(HKind::Coalesce { l: Box::new(lh), r: Box::new(rh), result }, inner, span)
                    }
                    Ty::Error => err(span),
                    Ty::Var(_) => {
                        self.infer_error(l.span, "type annotation needed before `??`");
                        err(span)
                    }
                    _ => {
                        let msg = format!("`??` requires an Option or Result, found {}", self.show(lt));
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
                let sh = self.check_expr(s, TypeId::INT);
                let eh = self.check_expr(en, TypeId::INT);
                mk(HKind::Range { start: Box::new(sh), end: Box::new(eh), inclusive: *inclusive }, TypeId::RANGE, span)
            }
            ExprKind::Call { callee, args } => self.check_call(callee, &CallArgs::new(None, args), span),
            ExprKind::MethodCall { receiver, method, args } => self.check_method_call(receiver, method, &CallArgs::new(None, args), span),
            ExprKind::Field(obj, name) => self.check_field(obj, name, hint, span),
            ExprKind::TupleField(obj, idx, ispan) => {
                let o = self.infer(obj, None);
                let ot = self.resolve(o.ty);
                match self.ty(ot) {
                    Ty::Tuple(ts) => match self.arena.list(ts).get(*idx as usize) {
                        Some(&t) => mk(HKind::Field { obj: Box::new(o), index: *idx }, t, span),
                        None => {
                            let msg = format!("no field `{idx}` on type {}", self.show(ot));
                            self.error(*ispan, msg);
                            err(span)
                        }
                    },
                    Ty::Error => err(span),
                    Ty::Var(_) => {
                        self.infer_error(obj.span, "type annotation needed before tuple field access");
                        err(span)
                    }
                    _ => {
                        let msg = format!("no field `{idx}` on type {}", self.show(ot));
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
                self.enter_loop(label.as_ref(), Some(bt));
                let body = self.check_block(b, None, false);
                let broke = self.leave_loop();
                let ty = if broke { bt } else { TypeId::NEVER };
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
                mk(kind, TypeId::UNIT, span)
            }
            ExprKind::For { label, pattern, iter, body } => self.check_for(label.as_ref(), pattern, iter, body, span),
            ExprKind::Lambda { params, body, .. } => self.check_lambda(params, body, hint, span),
            ExprKind::Return(v) => {
                self.check_not_in_defer("`return`", span);
                if let Some(CtxKind::TopLevel { main }) = self.fs.ctx.last().map(|c| c.kind) {
                    return self.check_top_return(v.as_deref(), main, span);
                }
                let ret = self.fs.ctx.last().map(|c| c.ret).unwrap_or(TypeId::UNIT);
                let vh = match v {
                    Some(v) => Some(Box::new(self.check_expr(v, ret))),
                    None => {
                        self.expect_type(ret, TypeId::UNIT, span);
                        None
                    }
                };
                mk(HKind::Return(vh), TypeId::NEVER, span)
            }
            ExprKind::Break { label, value: v } => {
                let Some(depth) = self.loop_target(label.as_ref(), "break", span) else {
                    if let Some(v) = v {
                        self.infer(v, None);
                    }
                    return mk(HKind::Break { value: None, depth: 0 }, TypeId::NEVER, span);
                };
                let loops = &mut self.fs.ctx.last_mut().unwrap().loops;
                let target = loops.len() - 1 - depth as usize;
                loops[target].broke = true;
                let bt = loops[target].break_ty;
                let vh = match (v, bt) {
                    (Some(v), Some(bt)) => Some(Box::new(self.check_expr(v, bt))),
                    (Some(v), None) => {
                        self.error(v.span, "break with a value is only allowed inside `loop`");
                        None
                    }
                    (None, Some(bt)) => {
                        self.unify(bt, TypeId::UNIT);
                        None
                    }
                    (None, None) => None,
                };
                mk(HKind::Break { value: vh, depth }, TypeId::NEVER, span)
            }
            ExprKind::Continue(label) => {
                let depth = self.loop_target(label.as_ref(), "continue", span).unwrap_or(0);
                mk(HKind::Continue { depth }, TypeId::NEVER, span)
            }
            ExprKind::Spawn(inner) => {
                let id = self.new_lambda();
                let ret = self.fresh();
                self.fs.ctx.push(FnCtx { ret, kind: CtxKind::Lambda, loops: vec![], lambda: id, in_defer: None });
                self.push_scope();
                let body = self.infer(inner, None);
                self.join(ret, body.ty, inner.span);
                if self.resolve(body.ty).is_never() && matches!(self.rty(ret), Ty::Var(_)) {
                    self.unify(ret, TypeId::UNIT);
                }
                self.pop_scope();
                self.fs.ctx.pop();
                let lam_ty = self.fn_ty(&[], ret);
                let lam = mk(
                    HKind::Lambda(Box::new(HLambda { id, name: "<spawn>".into(), params: vec![], body, self_local: None })),
                    lam_ty,
                    span,
                );
                let task = self.named_ty(self.wk.task, &[ret]);
                mk(HKind::Spawn(Box::new(lam)), task, span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Strings

    fn check_string(&mut self, segs: &[StrSeg], span: Span) -> HExpr {
        if segs.iter().all(|s| matches!(s, StrSeg::Text(_))) {
            let s: String = segs.iter().map(|s| if let StrSeg::Text(t) = s { t.as_str() } else { "" }).collect();
            return mk(HKind::Str(s), TypeId::STR, span);
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
                                self.validate_spec(&f, h.ty, spec.span, &spec.text);
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
        mk(HKind::Interp(parts), TypeId::STR, span)
    }

    fn validate_spec(&mut self, f: &FormatSpec, ty: TypeId, span: Span, text: &str) {
        let t = self.resolve(ty);
        let ok = match f.kind {
            Some('x') | Some('X') | Some('b') | Some('o') => {
                matches!(self.ty(t), Ty::Int | Ty::Var(_) | Ty::Error)
            }
            Some('e') => matches!(self.ty(t), Ty::Float | Ty::Var(_) | Ty::Error),
            _ => true,
        };
        if !ok {
            let msg = format!("format spec `{text}` is not valid for {}", self.show(t));
            self.error(span, msg);
        }
        if f.precision.is_some() && f.kind != Some('?') && t == TypeId::INT {
            self.error(span, format!("format spec `{text}`: precision is not valid for int"));
        }
    }

    // ------------------------------------------------------------------
    // Names

    fn instantiate_local(&mut self, l: LocalId) -> TypeId {
        let t = self.fs.data[l as usize].ty;
        if self.fs.data[l as usize].scheme.is_empty() {
            return t;
        }
        let scheme = self.fs.data[l as usize].scheme.clone();
        let fresh: Vec<TypeId> = scheme.iter().map(|_| self.fresh()).collect();
        self.arena.instantiate(t, &scheme, &fresh)
    }

    fn check_ident(&mut self, id: &ast::Ident, hint: Option<TypeId>) -> HExpr {
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
            None if self.fs.generics.iter().any(|&(n, _)| self.name(n) == id.name) => {
                self.error(span, format!("`{}` is a type parameter, not a value", id.name));
                err(span)
            }
            None => {
                let hint = self.import_hint(&id.name);
                self.error(span, format!("undefined name `{}`{hint}", id.name));
                err(span)
            }
        }
    }

    /// The value of a resolved definition used in expression position.
    fn def_value(&mut self, def: Def, name: &str, hint: Option<TypeId>, span: Span) -> HExpr {
        match def {
            Def::Fn(f) if let FnKind::Intrinsic(i) = self.fns[f as usize].kind => {
                let msg = match i {
                    Intrinsic::Help => "`help` can only be called: `help()` gives an overview, `help(x)` describes x",
                    Intrinsic::Members => "`members` can only be called: `members(x)` lists the members of x",
                };
                self.error(span, msg);
                err(span)
            }
            Def::Fn(f) => self.fn_value(f, span),
            Def::Global(g) => {
                self.mark_global_used(g);
                let t = self.globals[g as usize].ty;
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

    /// Instantiates `f` with fresh type variables, first unifying those of
    /// the parameters in `pre` with the given types. Built-in bounds become
    /// obligations; the trait evidence the call needs is returned in `dicts`.
    pub(crate) fn instantiate_fn_with(&mut self, f: FnId, span: Span, pre: &[(ParamId, TypeId)]) -> Instance {
        let n = self.fns[f as usize].generics.len();
        let first = self.subst.len() as VarId;
        let fresh: Vec<TypeId> = (0..n).map(|_| self.fresh()).collect();
        let fi = &self.fns[f as usize];
        if let Some(q) = &mut self.type_query {
            q.origins.extend(fi.generics.iter().enumerate().map(|(i, &g)| (first + i as VarId, g)));
        }
        let arena = &mut self.arena;
        let gens = &fi.generics;
        let params: Vec<TypeId> = fi.params.iter().map(|p| arena.instantiate(p.ty, gens, &fresh)).collect();
        let ret = arena.instantiate(fi.ret, gens, &fresh);
        let map = |p: ParamId| gens.iter().position(|&g| g == p).map(|i| fresh[i]);
        let dicts = fi.dicts.iter().map(|&(t, tr)| (arena.subst_params(t, &map), tr.subst_params(arena, &map))).collect();
        for &(p, t) in pre {
            if let Some(i) = self.fns[f as usize].generics.iter().position(|&g| g == p) {
                self.unify(fresh[i], t);
            }
        }
        for (k, &v) in fresh.iter().enumerate() {
            let g = self.fns[f as usize].generics[k] as usize;
            for j in 0..self.params[g].bounds.len() {
                let b = self.params[g].bounds[j];
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
        let inst = self.instantiate_fn_with(f, span, &[]);
        let t = self.fn_ty(&inst.params, inst.ret);
        mk(HKind::FnValue(f), t, span)
    }

    fn enum_args(&mut self, d: TypeDefId, hint: Option<TypeId>) -> Vec<TypeId> {
        if let Some(h) = hint
            && let Ty::Named(hd, args) = self.rty(h)
            && hd == d
        {
            return self.elems(args);
        }
        let n = self.types[d as usize].generics.len();
        (0..n).map(|_| self.fresh()).collect()
    }

    pub(crate) fn variant_field_types(&mut self, d: TypeDefId, i: u32, args: &[TypeId]) -> Vec<TypeId> {
        let def = &self.types[d as usize];
        let arena = &mut self.arena;
        def.variants()[i as usize].fields.iter().map(|&t| arena.instantiate(t, &def.generics, args)).collect()
    }

    fn variant_value(&mut self, d: TypeDefId, i: u32, hint: Option<TypeId>, span: Span) -> HExpr {
        let args = self.enum_args(d, hint);
        let rt_id = self.types[d as usize].variants()[i as usize].rt_id;
        let fields = self.variant_field_types(d, i, &args);
        let enum_ty = self.named_ty(d, &args);
        if fields.is_empty() {
            mk(HKind::Tag(rt_id), enum_ty, span)
        } else {
            let arity = fields.len();
            let t = self.fn_ty(&fields, enum_ty);
            mk(HKind::VariantFn { rt_id, arity }, t, span)
        }
    }

    /// Resolves an expression used as a namespace (`module`, `Type`, `module.Type`).
    pub(crate) fn resolve_ns(&mut self, e: &Expr) -> Option<Ns> {
        match &e.kind {
            ExprKind::Ident(id) => {
                if self.find_local(&id.name).is_some() {
                    return None;
                }
                let sym = self.syms.get(&id.name);
                if let Some(p) = self.fs.generics.iter().rev().find(|(n, _)| Some(*n) == sym).map(|&(_, p)| p) {
                    return Some(Ns::Type(self.param_ty(p)));
                }
                match self.lookup_global(&id.name)? {
                    Def::Module(m) => Some(Ns::Module(m)),
                    Def::Type(r) => Some(Ns::Type(self.tyref_open(r, id.span, &id.name))),
                    Def::Trait(t) => Some(Ns::Trait(t)),
                    _ => None,
                }
            }
            ExprKind::ModuleRef(name) => match self.module_named(name) {
                Some(m) => Some(Ns::Module(m)),
                None => {
                    self.error(e.span, format!("module `{name}` is not loaded"));
                    None
                }
            },
            ExprKind::SelfType => match self.fs.self_ty {
                Some(t) => Some(Ns::Type(t)),
                None => {
                    self.error(e.span, "`Self` is only valid inside a type's methods");
                    Some(Ns::Type(TypeId::ERROR))
                }
            },
            ExprKind::Field(base, name) => match self.resolve_ns(base)? {
                Ns::Module(m) => {
                    let found = self.syms.get(&name.name).and_then(|s| self.mods[m as usize].scope.get(&s).copied());
                    match found {
                        Some(Binding { def: Def::Type(r), is_pub: true, .. }) => Some(Ns::Type(self.tyref_open(r, name.span, &name.name))),
                        Some(Binding { def: Def::Trait(t), is_pub: true, .. }) => Some(Ns::Trait(t)),
                        _ => None,
                    }
                }
                Ns::Type(_) | Ns::Trait(_) => None,
            },
            _ => None,
        }
    }

    /// A type reference with fresh type arguments.
    fn tyref_open(&mut self, r: TyRef, span: Span, name: &str) -> TypeId {
        match r {
            TyRef::Prim(t) => t,
            TyRef::Set => {
                let v = self.fresh();
                self.arena.set_of(v)
            }
            TyRef::Def(d) => {
                let n = self.types[d as usize].generics.len();
                let args: Vec<TypeId> = (0..n).map(|_| self.fresh()).collect();
                self.named_ty(d, &args)
            }
            TyRef::Alias(i) => {
                let n = self.aliases[i].generics.len();
                let args = (0..n).map(|_| self.fresh()).collect();
                self.instantiate_tyref(TyRef::Alias(i), args, span, name)
            }
        }
    }

    fn find_variant(&self, d: TypeDefId, name: &str) -> Option<u32> {
        let s = self.syms.get(name)?;
        self.types[d as usize].variants().iter().position(|v| v.name == s).map(|i| i as u32)
    }

    // ------------------------------------------------------------------
    // Fields

    /// (index, type, mutable) of a struct field.
    fn struct_field(&mut self, t: TypeId, name: &str) -> Option<(u32, TypeId, bool)> {
        let Ty::Named(d, args) = self.ty(t) else { return None };
        let s = self.syms.get(name)?;
        let i = self.types[d as usize].fields().iter().position(|f| f.name == s)?;
        let args = self.elems(args);
        Some((i as u32, self.field_type(d, i, &args), self.types[d as usize].fields()[i].mutable))
    }

    pub(crate) fn field_type(&mut self, d: TypeDefId, i: usize, args: &[TypeId]) -> TypeId {
        let def = &self.types[d as usize];
        self.arena.instantiate(def.fields()[i].ty, &def.generics, args)
    }

    fn check_field(&mut self, obj: &Expr, name: &ast::Ident, hint: Option<TypeId>, span: Span) -> HExpr {
        match self.resolve_ns(obj) {
            Some(Ns::Module(m)) => {
                return match self.module_member(m, &name.name, name.span) {
                    Some(def) => {
                        self.note_name_used(ns_root_name(obj));
                        self.def_value(def, &name.name, hint, span)
                    }
                    None => err(span),
                };
            }
            Some(Ns::Type(t)) => {
                let t = self.resolve(t);
                if let Ty::Named(d, _) = self.ty(t)
                    && let Some(i) = self.find_variant(d, &name.name)
                {
                    return self.variant_value(d, i, hint, span);
                }
                if let Some(c) = ctor_of(self.ty(t))
                    && let Some(fs) = self.methods_named(c, &name.name)
                {
                    return self.fn_value(fs[0], span);
                }
                match self.find_trait_method(t, &name.name, name.span) {
                    Ok(Some((tr, slot))) => return self.trait_method_value(tr, slot, t, span),
                    Ok(None) => {}
                    Err(()) => return err(span),
                }
                if t == TypeId::ERROR {
                    return err(span);
                }
                let msg = if matches!(self.ty(t), Ty::Named(d, _) if self.types[d as usize].is_enum()) {
                    format!("no variant `{}` on type {}", name.name, self.show(t))
                } else {
                    format!("no method `{}` on type {}", name.name, self.show(t))
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
        let ot = self.resolve(o.ty);
        if let Some((idx, fty, _)) = self.struct_field(ot, &name.name) {
            return mk(HKind::Field { obj: Box::new(o), index: idx }, fty, span);
        }
        match self.ty(ot) {
            Ty::Error => err(span),
            Ty::Var(_) => {
                self.infer_error(name.span, "type annotation needed before field access");
                err(span)
            }
            other => {
                let has_method =
                    ctor_of(other).is_some_and(|c| self.methods_named(c, &name.name).is_some()) || self.has_trait_method(ot, &name.name);
                let msg = if has_method {
                    format!("`{}` is a method; call it with `()`", name.name)
                } else {
                    format!("no field `{}` on type {}", name.name, self.show(ot))
                };
                self.error(name.span, msg);
                err(span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Calls

    /// How a callee is named in diagnostics.
    fn callee_what(&self, c: Callee) -> String {
        match c {
            Callee::Fn(f) => format!("`{}`", self.name(self.fns[f as usize].name)),
            Callee::Variant(d, i) => format!("`{}`", self.name(self.types[d as usize].variants()[i as usize].name)),
            Callee::Value => "function".into(),
        }
    }

    fn param_name(&self, c: Callee, i: usize) -> Option<&str> {
        match c {
            Callee::Fn(f) => Some(self.name(self.fns[f as usize].params[i].name)),
            Callee::Variant(d, v) => self.types[d as usize].variants()[v as usize].field_names.as_ref().map(|ns| self.name(ns[i])),
            Callee::Value => None,
        }
    }

    /// Binds call arguments to the parameters of `callee` (of types `ptys`),
    /// checks them and fills defaults.
    fn bind_args(&mut self, callee: Callee, ptys: &[TypeId], first: Option<HExpr>, args: &CallArgs, span: Span) -> Option<Vec<HArg>> {
        let n = ptys.len();
        let offset = usize::from(first.is_some());
        let mut slots: Vec<Option<HArg>> = (0..n).map(|_| None).collect();
        if let Some(f) = first {
            if n == 0 {
                self.error(span, format!("{} takes no arguments", self.callee_what(callee)));
                return None;
            }
            self.expect_type(ptys[0], f.ty, f.span);
            slots[0] = Some(HArg::Expr(f));
        }
        if matches!(callee, Callee::Value) && !args.named.is_empty() {
            self.error(args.named[0].0.span, "named arguments are only allowed when calling a function by name");
            return None;
        }
        let user_params = n - offset;
        if args.positional.len() > user_params {
            let plural = if user_params == 1 { "" } else { "s" };
            let what = self.callee_what(callee);
            self.error(span, format!("{what} expected {user_params} argument{plural}, found {}", args.len()));
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
            let h = self.check_expr(a, ptys[pi]);
            slots[pi] = Some(HArg::Expr(h));
            last_idx = pi;
        }
        let mut named_order = Vec::new();
        for (name, a) in &args.named {
            match (0..n).find(|&i| self.param_name(callee, i) == Some(name.name.as_str())) {
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
                    let h = self.check_expr(a, ptys[pi]);
                    named_order.push(pi);
                    slots[pi] = Some(HArg::Expr(h));
                }
                _ => {
                    self.error(name.span, format!("{} has no parameter named `{}`", self.callee_what(callee), name.name));
                    self.infer(a, None);
                }
            }
        }
        let mut ok = true;
        let mut out = Vec::new();
        for i in 0..n {
            match slots[i].take() {
                Some(a) => out.push(a),
                None => match self.param_default(callee, i) {
                    Some(DefaultArg::Inline(e)) => {
                        let h = self.check_expr(&e, ptys[i]);
                        out.push(HArg::Expr(HExpr { span, ..h }));
                    }
                    Some(DefaultArg::Helper(helper)) => {
                        out.push(HArg::Default { helper, inputs: (0..i).collect(), ty: ptys[i] });
                    }
                    None => {
                        let name = match self.param_name(callee, i) {
                            Some(n) => n.to_string(),
                            None if matches!(callee, Callee::Value) => format!("#{i}"),
                            None => i.to_string(),
                        };
                        self.error(span, format!("missing argument `{name}`"));
                        ok = false;
                    }
                },
            }
        }
        if !ok {
            return None;
        }
        if out_of_order {
            self.reorder_args(&mut out, &named_order, span);
        }
        Some(out)
    }

    fn param_default(&self, c: Callee, i: usize) -> Option<DefaultArg> {
        match c {
            Callee::Fn(f) => self.fns[f as usize].params[i].default.clone(),
            _ => None,
        }
    }

    fn reorder_args(&mut self, out: &mut [HArg], order: &[usize], span: Span) {
        // Hoist the named arguments into locals in source order. Named
        // arguments always follow the positional ones, so the hoisting
        // statements are attached to the earliest hoisted slot.
        let mut lets = Vec::new();
        for &i in order {
            if let HArg::Expr(h) = &out[i] {
                let ty = h.ty;
                let l = self.hidden_local(ty, span);
                let value = std::mem::replace(&mut out[i], HArg::Expr(mk(HKind::Local(l), ty, span)));
                if let HArg::Expr(v) = value {
                    lets.push(mk(
                        HKind::Let { pat: HPat { kind: HPatKind::Bind(l), ty, span }, init: Box::new(v), else_: None },
                        TypeId::UNIT,
                        span,
                    ));
                }
            }
        }
        let Some(&first) = order.iter().min() else { return };
        if let HArg::Expr(slot) = &mut out[first] {
            let read = std::mem::replace(slot, mk(HKind::Unit, TypeId::UNIT, span));
            let ty = read.ty;
            lets.push(read);
            *slot = mk(HKind::Block(lets), ty, span);
        }
    }

    /// Calls a known function `f`, optionally with a receiver for `self`.
    fn call_fn(&mut self, f: FnId, recv: Option<HExpr>, args: &CallArgs, span: Span) -> HExpr {
        self.call_fn_with(f, recv, args, span, &[])
    }

    /// Calls `f` with some of its type parameters fixed (see [`Self::instantiate_fn_with`]).
    pub(crate) fn call_fn_with(&mut self, f: FnId, recv: Option<HExpr>, args: &CallArgs, span: Span, pre: &[(ParamId, TypeId)]) -> HExpr {
        let inst = self.instantiate_fn_with(f, span, pre);
        self.call_instance(f, inst, recv, args, span)
    }

    fn call_instance(&mut self, f: FnId, inst: Instance, recv: Option<HExpr>, args: &CallArgs, span: Span) -> HExpr {
        let Instance { fresh, params: ptys, ret, dicts } = inst;
        let Some(mut hargs) = self.bind_args(Callee::Fn(f), &ptys, recv, args, span) else {
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
            let mut all: Vec<HArg> = dicts.into_iter().map(|(t, tr)| HArg::Expr(self.evidence_hole(t, tr, false, span))).collect();
            all.extend(hargs);
            hargs = all;
        }
        match self.fns[f as usize].kind {
            FnKind::TraitMethod(trait_id, slot) => mk(HKind::CallTrait { trait_id, slot, args: hargs, impl_id: None }, ret, span),
            FnKind::Native(n) => {
                let mut reflect = Vec::new();
                for (k, &t) in fresh.iter().enumerate() {
                    if self.params[self.fns[f as usize].generics[k] as usize].reflect {
                        reflect.push(t);
                        self.fs.reflect_checks.push((t, span));
                    }
                }
                mk(HKind::CallNative { native: n, args: hargs, reflect }, ret, span)
            }
            _ => mk(HKind::Call { func: f, args: hargs }, ret, span),
        }
    }

    fn call_variant(&mut self, d: TypeDefId, i: u32, args: &CallArgs, span: Span) -> HExpr {
        // Hints are not pushed into payloads so that mismatches are reported
        // on the whole value (`expected str?, found int?`).
        let targs = self.enum_args(d, None);
        let rt_id = self.types[d as usize].variants()[i as usize].rt_id;
        let enum_ty = self.named_ty(d, &targs);
        let ptys = self.variant_field_types(d, i, &targs);
        if ptys.is_empty() {
            let name = self.name(self.types[d as usize].variants()[i as usize].name);
            self.error(span, format!("`{name}` is not a function; it has no payload"));
            self.infer_args(args);
            return err(span);
        }
        if args.named.is_empty() && args.len() != ptys.len() {
            let plural = if ptys.len() == 1 { "" } else { "s" };
            let what = self.callee_what(Callee::Variant(d, i));
            self.error(span, format!("{what} expected {} argument{plural}, found {}", ptys.len(), args.len()));
            self.infer_args(args);
            return mk(HKind::Error, enum_ty, span);
        }
        let Some(hargs) = self.bind_args(Callee::Variant(d, i), &ptys, None, args, span) else {
            return mk(HKind::Error, enum_ty, span);
        };
        let hs = hargs
            .into_iter()
            .map(|a| match a {
                HArg::Expr(e) => e,
                HArg::Default { .. } => unreachable!(),
            })
            .collect();
        mk(HKind::MakeVariant { rt_id, args: hs }, enum_ty, span)
    }

    /// Infers the arguments of a failed call. Lambdas get error-typed parameters, so they report no extra errors.
    /// Coerces an element of a collection literal, with a hint for mixed values.
    fn expect_elem(&mut self, expected: TypeId, found: TypeId, span: Span, what: &str) {
        if !self.coerce(expected, found) {
            let (e, f) = self.show_pair(expected, found);
            self.error(
                span,
                format!("type mismatch: expected {e}, found {f}; all {what} have one type, so use a struct or a tuple for mixed values"),
            );
        }
    }

    fn infer_args(&mut self, args: &CallArgs) {
        for a in args.exprs() {
            let hint = match &a.kind {
                ExprKind::Lambda { params, .. } => Some(self.fn_ty(&vec![TypeId::ERROR; params.len()], TypeId::ERROR)),
                _ => None,
            };
            self.infer(a, hint);
        }
    }

    fn call_value(&mut self, callee: HExpr, args: &CallArgs, span: Span) -> HExpr {
        let ct = self.resolve(callee.ty);
        let (ptys, ret) = match self.ty(ct) {
            Ty::Fn(ps, r) => {
                let n = self.arena.list_len(ps);
                if args.len() != n {
                    let plural = if n == 1 { "" } else { "s" };
                    self.error(span, format!("function expected {n} argument{plural}, found {}", args.len()));
                    self.infer_args(args);
                    return mk(HKind::Error, r, span);
                }
                (self.elems(ps), r)
            }
            Ty::Var(_) => {
                // Infer a function type from the call.
                let ps: Vec<TypeId> = (0..args.len()).map(|_| self.fresh()).collect();
                let r = self.fresh();
                let f = self.fn_ty(&ps, r);
                self.unify(callee.ty, f);
                (ps, r)
            }
            Ty::Error => {
                self.infer_args(args);
                return err(span);
            }
            _ => {
                let msg = format!("{} is not a function", self.show(ct));
                self.error(callee.span, msg);
                self.infer_args(args);
                return err(span);
            }
        };
        let Some(hargs) = self.bind_args(Callee::Value, &ptys, None, args, span) else {
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

    fn check_call(&mut self, callee: &Expr, args: &CallArgs, span: Span) -> HExpr {
        match &callee.kind {
            ExprKind::Ident(id) if self.find_local(&id.name).is_none() => match self.lookup_global(&id.name) {
                Some(Def::Fn(f)) => {
                    if let FnKind::Intrinsic(i) = self.fns[f as usize].kind {
                        return self.check_intrinsic(i, args, span);
                    }
                    return self.call_fn(f, None, args, span);
                }
                Some(Def::Variant(d, i)) => return self.call_variant(d, i, args, span),
                Some(Def::Type(_)) => {
                    self.error(callee.span, format!("`{}` is a type; use `{} {{ ... }}` to construct it", id.name, id.name));
                    return err(span);
                }
                _ => {}
            },
            ExprKind::Field(obj, name) => {
                if let Some(ns) = self.resolve_ns(obj) {
                    return self.ns_call(ns, obj, name, args, span);
                }
            }
            _ => {}
        }
        let c = self.infer(callee, None);
        self.call_value(c, args, span)
    }

    fn ns_call(&mut self, ns: Ns, obj: &Expr, name: &ast::Ident, args: &CallArgs, span: Span) -> HExpr {
        match ns {
            Ns::Module(m) => {
                self.note_name_used(ns_root_name(obj));
                match self.module_member(m, &name.name, name.span) {
                    Some(Def::Fn(f)) => self.call_fn(f, None, args, span),
                    Some(Def::Variant(d, i)) => self.call_variant(d, i, args, span),
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
                        self.infer_args(args);
                        err(span)
                    }
                }
            }
            Ns::Type(t) => {
                let t = self.resolve(t);
                if let Ty::Named(d, _) = self.ty(t)
                    && let Some(i) = self.find_variant(d, &name.name)
                {
                    return self.call_variant(d, i, args, span);
                }
                if let Some(c) = ctor_of(self.ty(t))
                    && let Some(fs) = self.methods_named(c, &name.name)
                {
                    return self.call_fn(fs[0], None, args, span);
                }
                match self.find_trait_method(t, &name.name, name.span) {
                    Ok(Some((tr, slot))) => return self.call_trait_method(tr, slot, t, None, args, span),
                    Ok(None) => {}
                    Err(()) => {
                        self.infer_args(args);
                        return err(span);
                    }
                }
                if t == TypeId::ERROR {
                    return err(span);
                }
                let msg = if matches!(self.ty(t), Ty::Named(d, _) if self.types[d as usize].is_enum()) {
                    format!("no variant `{}` on type {}", name.name, self.show(t))
                } else {
                    format!("no method `{}` on type {}", name.name, self.show(t))
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
                    self.infer_args(args);
                    err(span)
                }
            },
        }
    }

    fn lookup_method(&mut self, recv_ty: TypeId, name: &str) -> Option<FnId> {
        let c = ctor_of(self.ty(recv_ty))?;
        let fs = self.methods_named(c, name)?;
        fs.iter().copied().find(|&f| self.fns[f as usize].has_self)
    }

    fn check_method_call(&mut self, receiver: &Expr, method: &ast::Ident, args: &CallArgs, span: Span) -> HExpr {
        if let Some(ns) = self.resolve_ns(receiver) {
            return self.ns_call(ns, receiver, method, args, span);
        }
        let r = self.infer(receiver, None);
        let rt = self.resolve(r.ty);
        match self.ty(rt) {
            Ty::Error => {
                self.infer_args(args);
                return err(span);
            }
            Ty::Var(_) => {
                self.infer_error(method.span, format!("type annotation needed before calling `{}`", method.name));
                self.infer_args(args);
                return err(span);
            }
            _ => {}
        }
        // A function-typed struct field.
        if let Some((idx, fty, _)) = self.struct_field(rt, &method.name)
            && matches!(self.rty(fty), Ty::Fn(..))
        {
            let callee = mk(HKind::Field { obj: Box::new(r), index: idx }, fty, method.span);
            return self.call_value(callee, args, span);
        }
        if let Some(f) = self.lookup_method(rt, &method.name) {
            // The receiver must fit the declared self type (`extend [int]`
            // has no methods for `[str]`).
            let inst = self.instantiate_fn_with(f, span, &[]);
            if !self.try_unify_all(&[(inst.params[0], rt)]) {
                let msg = format!("no method `{}` on type {}", method.name, self.show(rt));
                self.error(method.span, msg);
                self.infer_args(args);
                return err(span);
            }
            let call = self.call_instance(f, inst, Some(r), args, span);
            return self.fuse_consumer(call);
        }
        // Trait methods: through the bounds of a type parameter, or the impls of a type.
        match self.find_trait_method(rt, &method.name, method.span) {
            Ok(Some((tr, slot))) => {
                let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
                if !self.fns[sig as usize].has_self {
                    let msg = format!("`{}` is a static method of `{}`; call it on the type", method.name, self.show_trait(&tr));
                    self.error(method.span, msg);
                    self.infer_args(args);
                    return err(span);
                }
                return self.call_trait_method(tr, slot, rt, Some(r), args, span);
            }
            Ok(None) => {}
            Err(()) => {
                self.infer_args(args);
                return err(span);
            }
        }
        match method.name.as_str() {
            "to_str" | "debug" if args.len() == 0 => {
                let kind = if method.name == "to_str" { HKind::ToStr(Box::new(r)) } else { HKind::Debug(Box::new(r)) };
                mk(kind, TypeId::STR, span)
            }
            _ => {
                let mut msg = format!("no method `{}` on type {}", method.name, self.show(rt));
                if let Some(t) = self.trait_with_method(&method.name) {
                    if let Ty::Param(_) = self.ty(rt) {
                        msg.push_str(&format!("; add a bound `{}: {t}`", self.show(rt)));
                    } else {
                        msg.push_str(&format!(" (the trait `{t}` has it but is not implemented for {})", self.show(rt)));
                    }
                }
                let range_iter = match (self.ty(rt), self.iter_struct) {
                    (Ty::Range, Some(iter)) => {
                        let iter = self.named_ty(iter, &[TypeId::INT]);
                        self.lookup_method(iter, &method.name).is_some()
                    }
                    _ => false,
                };
                let as_list = match self.ty(rt) {
                    Ty::Map(..) => Some("`entries()`, a list of (key, value) pairs"),
                    Ty::Set(_) | Ty::Range => Some("`to_list()`"),
                    _ => None,
                };
                if range_iter {
                    msg.push_str("; call it on `iter()`, a lazy iterator");
                } else if let Some(as_list) = as_list {
                    let list = self.list_ty(TypeId::ERROR);
                    if self.lookup_method(list, &method.name).is_some() {
                        msg.push_str(&format!("; call it on {as_list}"));
                    }
                }
                self.error(method.span, msg);
                self.infer_args(args);
                err(span)
            }
        }
    }

    /// `l |> r`: `l` is the first argument of the call `r`.
    fn check_pipe(&mut self, l: &Expr, r: &Expr, span: Span) -> HExpr {
        match &r.kind {
            ExprKind::Call { callee, args } => self.check_call(callee, &CallArgs::new(Some(l), args), span),
            ExprKind::MethodCall { receiver, method, args } => {
                self.check_method_call(receiver, method, &CallArgs::new(Some(l), args), span)
            }
            ExprKind::Ident(_) | ExprKind::Field(..) => self.check_call(r, &CallArgs::new(Some(l), &[]), span),
            ExprKind::Lambda { .. } | ExprKind::Paren(_) => {
                let c = self.infer(r, None);
                self.call_value(c, &CallArgs::new(Some(l), &[]), span)
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

    fn check_struct_lit(&mut self, path: &[ast::Ident], fields: &[ast::FieldInit], base: Option<&Expr>, span: Span) -> HExpr {
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
        if !self.types[def as usize].is_struct() {
            let msg = format!("`{}` is not a struct", self.name(self.types[def as usize].name));
            self.error(path.last().unwrap().span, msg);
            return err(span);
        }
        let args = self.enum_args(def, None);
        let sty = self.named_ty(def, &args);
        let n = self.types[def as usize].fields().len();
        let mut given: Vec<Option<LocalId>> = vec![None; n];
        let mut stmts = Vec::new();
        for f in fields {
            let sym = self.syms.get(&f.name.name);
            let Some(idx) = self.types[def as usize].fields().iter().position(|d| Some(d.name) == sym) else {
                let msg = format!("no field `{}` on type {}", f.name.name, self.name(self.types[def as usize].name));
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
            let fty = self.field_type(def, idx, &args);
            let h = match &f.value {
                Some(v) => self.check_expr(v, fty),
                None => {
                    let h = self.check_ident(&f.name, Some(fty));
                    self.expect_type(fty, h.ty, f.name.span);
                    h
                }
            };
            let l = self.hidden_local(fty, f.name.span);
            stmts.push(mk(
                HKind::Let { pat: HPat { kind: HPatKind::Bind(l), ty: fty, span: f.name.span }, init: Box::new(h), else_: None },
                TypeId::UNIT,
                f.name.span,
            ));
            given[idx] = Some(l);
        }
        let base_local = base.map(|b| {
            let h = self.check_expr(b, sty);
            let l = self.hidden_local(sty, b.span);
            stmts.push(mk(
                HKind::Let { pat: HPat { kind: HPatKind::Bind(l), ty: sty, span: b.span }, init: Box::new(h), else_: None },
                TypeId::UNIT,
                b.span,
            ));
            l
        });
        let mut values = Vec::new();
        for (i, &given) in given.iter().enumerate() {
            let fty = self.field_type(def, i, &args);
            let v = match (given, base_local, self.types[def as usize].fields()[i].default) {
                (Some(l), _, _) => mk(HKind::Local(l), fty, span),
                (None, Some(b), _) => mk(HKind::Field { obj: Box::new(mk(HKind::Local(b), sty, span)), index: i as u32 }, fty, span),
                (None, None, Some(helper)) => {
                    // Instantiate the helper with this literal's type arguments.
                    mk(HKind::Call { func: helper, args: vec![] }, fty, span)
                }
                (None, None, None) => {
                    let def = &self.types[def as usize];
                    let msg = format!("missing field `{}` in `{}` literal", self.name(def.fields()[i].name), self.name(def.name));
                    self.error(span, msg);
                    err(span)
                }
            };
            values.push(v);
        }
        let make = mk(HKind::MakeStruct { def, fields: values }, sty, span);
        if stmts.is_empty() {
            return make;
        }
        stmts.push(make);
        mk(HKind::Block(stmts), sty, span)
    }

    fn resolve_struct_path(&mut self, path: &[ast::Ident]) -> Option<TypeDefId> {
        let name = || path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
        let r = if path.len() == 1 {
            if path[0].name == "Self" {
                return match self.fs.self_ty.map(|t| self.ty(t)) {
                    Some(Ty::Named(d, _)) => Some(d),
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
                    self.note_name_used(&path[0].name);
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
            Some(TyRef::Alias(i)) => {
                let t = self.resolve_alias(i, path[0].span);
                match self.ty(t) {
                    Ty::Named(d, _) => Some(d),
                    _ => {
                        self.error(path[0].span, format!("`{}` is not a struct", name()));
                        None
                    }
                }
            }
            Some(_) => {
                self.error(path[0].span, format!("`{}` is not a struct", name()));
                None
            }
            None => {
                self.error(path.last().unwrap().span, format!("unknown type `{}`", name()));
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // Operators

    fn check_unary(&mut self, op: UnOp, inner: &Expr, span: Span) -> HExpr {
        match op {
            UnOp::Not => {
                let h = self.check_expr(inner, TypeId::BOOL);
                mk(HKind::Unary { op: HUnOp::Not, e: Box::new(h) }, TypeId::BOOL, span)
            }
            UnOp::BitNot => {
                let h = self.check_expr(inner, TypeId::INT);
                mk(HKind::Unary { op: HUnOp::BitNot, e: Box::new(h) }, TypeId::INT, span)
            }
            UnOp::Neg => {
                let h = self.infer(inner, None);
                let t = self.resolve(h.ty);
                match self.ty(t) {
                    Ty::Int | Ty::Float | Ty::Duration | Ty::Error => {}
                    Ty::Var(_) | Ty::Param(_) => self.require_bound(t, Bound::Num, span),
                    _ => {
                        let msg = format!("cannot negate a value of type {}", self.show(t));
                        self.error(span, msg);
                    }
                }
                let ty = h.ty;
                mk(HKind::Unary { op: HUnOp::Neg, e: Box::new(h) }, ty, span)
            }
        }
    }

    /// Whether `t` can scale a duration. A type parameter must then be `Num`.
    fn num_factor(&mut self, t: TypeId, span: Span) -> bool {
        match self.rty(t) {
            Ty::Int | Ty::Float => true,
            Ty::Param(_) => {
                self.require_bound(t, Bound::Num, span);
                true
            }
            _ => false,
        }
    }

    /// Result type of an arithmetic operation, reporting errors.
    fn arith_type(&mut self, op: ArithOp, lt: TypeId, rt: TypeId, span: Span) -> TypeId {
        let l = self.resolve(lt);
        let r = self.resolve(rt);
        use ArithOp::*;
        match op {
            BitAnd | BitOr | BitXor | Shl | Shr => {
                self.expect_type(TypeId::INT, l, span);
                self.expect_type(TypeId::INT, r, span);
                return TypeId::INT;
            }
            _ => {}
        }
        if l == TypeId::ERROR || r == TypeId::ERROR {
            return TypeId::ERROR;
        }
        // A diverging operand fits the other one.
        let (l, r) = match (l.is_never(), r.is_never()) {
            (true, true) => return TypeId::NEVER,
            (true, false) => (r, r),
            (false, true) => (l, l),
            (false, false) => (l, r),
        };
        if op == Mul && r == TypeId::DURATION && self.num_factor(l, span) {
            return TypeId::DURATION;
        }
        if l == TypeId::DURATION {
            return match op {
                Add | Sub => {
                    self.expect_type(TypeId::DURATION, r, span);
                    TypeId::DURATION
                }
                Div if r == TypeId::DURATION => TypeId::FLOAT,
                Mul | Div => {
                    if !self.num_factor(r, span) {
                        match self.rty(r) {
                            Ty::Var(_) => {
                                self.expect_type(TypeId::INT, r, span);
                            }
                            Ty::Error => {}
                            _ => {
                                let msg = format!("expected Num, found {}", self.show(r));
                                self.error(span, msg);
                            }
                        }
                    }
                    TypeId::DURATION
                }
                _ => {
                    self.error(span, format!("operator `{}` is not defined for duration", arith_text(op)));
                    TypeId::ERROR
                }
            };
        }
        if let Some(t) = [l, r].into_iter().find(|&t| !self.has_arith(t, op)) {
            return self.no_operator(op, t, span);
        }
        if !self.unify(l, r) {
            let (e, f) = self.show_pair(l, r);
            let msg = format!("type mismatch: expected {e}, found {f}");
            self.error(span, msg);
            return TypeId::ERROR;
        }
        let t = self.resolve(l);
        match (self.ty(t), op) {
            (Ty::Int | Ty::Float, _) => t,
            (Ty::Str | Ty::Bytes | Ty::List(_), Add) => t,
            (Ty::Var(_), Add) => {
                // Decided later: numbers, durations and concatenation all work.
                self.require_bound(t, Bound::Add, span);
                t
            }
            (Ty::Var(_) | Ty::Param(_), _) => {
                self.require_bound(t, Bound::Num, span);
                t
            }
            _ => self.no_operator(op, t, span),
        }
    }

    fn has_arith(&self, t: TypeId, op: ArithOp) -> bool {
        match self.rty(t) {
            Ty::Int | Ty::Float | Ty::Duration | Ty::Param(_) | Ty::Var(_) | Ty::Error | Ty::Never => true,
            Ty::Str | Ty::Bytes | Ty::List(_) => op == ArithOp::Add,
            _ => false,
        }
    }

    fn no_operator(&mut self, op: ArithOp, t: TypeId, span: Span) -> TypeId {
        let msg = format!("operator `{}` is not defined for {}", arith_text(op), self.show(t));
        self.error(span, msg);
        TypeId::ERROR
    }

    fn check_binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> HExpr {
        match op {
            BinOp::And | BinOp::Or => {
                let lh = self.check_expr(l, TypeId::BOOL);
                let rh = self.check_expr(r, TypeId::BOOL);
                let kind = if op == BinOp::And { HKind::And(Box::new(lh), Box::new(rh)) } else { HKind::Or(Box::new(lh), Box::new(rh)) };
                mk(kind, TypeId::BOOL, span)
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let lh = self.infer(l, None);
                let rh = self.infer(r, Some(lh.ty));
                if !self.resolve(lh.ty).is_never() && !self.coerce(lh.ty, rh.ty) {
                    let msg = format!("type mismatch: cannot compare {} with {}", self.show(lh.ty), self.show(rh.ty));
                    self.error(span, msg);
                }
                let bound = if matches!(op, BinOp::Eq | BinOp::Ne) { Bound::Eq } else { Bound::Ord };
                self.require_bound(lh.ty, bound, span);
                let cop = match op {
                    BinOp::Eq => CmpOp::Eq,
                    BinOp::Ne => CmpOp::Ne,
                    BinOp::Lt => CmpOp::Lt,
                    BinOp::Le => CmpOp::Le,
                    BinOp::Gt => CmpOp::Gt,
                    _ => CmpOp::Ge,
                };
                mk(HKind::Cmp { op: cop, l: Box::new(lh), r: Box::new(rh) }, TypeId::BOOL, span)
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
                let hint = self.resolve(lh.ty);
                let rh = self.infer(r, if matches!(self.ty(hint), Ty::Var(_) | Ty::Duration) { None } else { Some(hint) });
                let ty = self.arith_type(aop, lh.ty, rh.ty, span);
                mk(HKind::Arith { op: aop, l: Box::new(lh), r: Box::new(rh) }, ty, span)
            }
        }
    }

    // ------------------------------------------------------------------
    // Indexing and `?`

    fn check_index(&mut self, obj: &Expr, idx: &Expr, span: Span) -> HExpr {
        let o = self.infer(obj, None);
        let ot = self.resolve(o.ty);
        if let ExprKind::Range { start, end, inclusive } = &idx.kind {
            let kind = match self.ty(ot) {
                Ty::List(_) => SeqKind::List,
                Ty::Str => SeqKind::Str,
                Ty::Bytes => SeqKind::Bytes,
                Ty::Error => return err(span),
                Ty::Var(_) => {
                    self.infer_error(obj.span, "type annotation needed before slicing");
                    return err(span);
                }
                _ => {
                    let msg = format!("cannot slice a value of type {}", self.show(ot));
                    self.error(span, msg);
                    return err(span);
                }
            };
            let s = start.as_ref().map(|s| Box::new(self.check_expr(s, TypeId::INT)));
            let e = end.as_ref().map(|e| Box::new(self.check_expr(e, TypeId::INT)));
            if *inclusive && e.is_none() {
                self.error(idx.span, "inclusive range needs an end");
            }
            let ty = o.ty;
            return mk(HKind::Slice { obj: Box::new(o), start: s, end: e, inclusive: *inclusive, kind }, ty, span);
        }
        match self.ty(ot) {
            Ty::List(t) => {
                let i = self.check_expr(idx, TypeId::INT);
                mk(HKind::Index { obj: Box::new(o), index: Box::new(i), map: false }, t, span)
            }
            Ty::Map(k, v) => {
                let i = self.check_expr(idx, k);
                mk(HKind::Index { obj: Box::new(o), index: Box::new(i), map: true }, v, span)
            }
            Ty::Str => {
                self.infer(idx, None);
                self.error(span, "cannot index a str by position; use a slice like `s[i..j]` or a method such as `char_at`");
                err(span)
            }
            Ty::Bytes => {
                self.infer(idx, None);
                self.error(span, "cannot index bytes by position; use a slice or `get`");
                err(span)
            }
            Ty::Error => err(span),
            Ty::Var(_) => {
                self.infer_error(obj.span, "type annotation needed before indexing");
                err(span)
            }
            _ => {
                let msg = format!("cannot index a value of type {}", self.show(ot));
                self.error(span, msg);
                err(span)
            }
        }
    }

    fn check_try(&mut self, inner: &Expr, span: Span) -> HExpr {
        self.check_not_in_defer("`?`", span);
        let h = self.infer(inner, None);
        let t = self.resolve(h.ty);
        let ctx_kind = self.fs.ctx.last().map(|c| c.kind).unwrap_or(CtxKind::Function);
        let ret = self.fs.ctx.last().map(|c| c.ret).unwrap_or(TypeId::UNIT);
        match self.ty(t) {
            Ty::Named(d, args) if d == self.wk.option => {
                if let CtxKind::TopLevel { .. } = ctx_kind {
                    self.error(span, "`?` on an Option is not allowed at the top level");
                    return err(span);
                }
                let ok = match self.rty(ret) {
                    Ty::Named(rd, _) if rd == self.wk.option => true,
                    Ty::Var(_) => {
                        let v = self.fresh();
                        let o = self.option_of(v);
                        self.unify(ret, o)
                    }
                    Ty::Error => true,
                    _ => false,
                };
                if !ok {
                    let msg =
                        format!("`?` on an Option requires the enclosing function to return an Option, but it returns {}", self.show(ret));
                    self.error(span, msg);
                }
                mk(HKind::Try { e: Box::new(h), kind: TryKind::Option }, self.arena.elem(args, 0), span)
            }
            Ty::Named(d, args) if d == self.wk.result => {
                let err_ty = self.arena.elem(args, 1);
                let kind = match ctx_kind {
                    CtxKind::TopLevel { main: true } | CtxKind::Test => TryKind::TopLevel,
                    CtxKind::TopLevel { main: false } => {
                        self.error(span, "`?` is not allowed at the top level of an imported module");
                        return err(span);
                    }
                    _ => {
                        match self.rty(ret) {
                            Ty::Named(rd, rargs) if rd == self.wk.result => {
                                let ret_err = self.arena.elem(rargs, 1);
                                if !self.unify(ret_err, err_ty) {
                                    let msg = format!(
                                        "type mismatch: `?` cannot convert error type {} into {}",
                                        self.show(err_ty),
                                        self.show(ret_err)
                                    );
                                    self.error(span, msg);
                                }
                            }
                            Ty::Var(_) => {
                                let v = self.fresh();
                                let r = self.result_of(v, err_ty);
                                self.unify(ret, r);
                            }
                            Ty::Error => {}
                            _ => {
                                let msg = format!(
                                    "`?` on a Result requires the enclosing function to return a Result, but it returns {}",
                                    self.show(ret)
                                );
                                self.error(span, msg);
                            }
                        }
                        TryKind::Result
                    }
                };
                mk(HKind::Try { e: Box::new(h), kind }, self.arena.elem(args, 0), span)
            }
            Ty::Error => err(span),
            Ty::Var(_) => {
                self.infer_error(inner.span, "type annotation needed before `?`");
                err(span)
            }
            _ => {
                let msg = format!("`?` can only be applied to Option or Result values, found {}", self.show(t));
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
            Cond::Expr(e) => (self.check_expr(e, TypeId::BOOL), None),
            Cond::Let(p, e) => {
                let v = self.infer(e, None);
                let pat = self.check_pattern(p, v.ty, false);
                (v, Some(pat))
            }
        }
    }

    fn check_if(&mut self, cond: &Cond, then: &Block, else_: Option<&Expr>, hint: Option<TypeId>, want: bool, span: Span) -> HExpr {
        self.push_scope();
        let (c, pat) = self.check_cond(cond);
        let value = want && else_.is_some();
        let th = self.check_block(then, if value { hint } else { None }, value);
        self.pop_scope();
        let eh = else_.map(|e| self.infer_inner(e, if value { hint } else { None }, value));
        let ty = if value {
            let eh_ty = eh.as_ref().unwrap().ty;
            self.join(th.ty, eh_ty, else_.unwrap().span)
        } else if th.ty.is_never() && eh.as_ref().map(|e| self.resolve(e.ty).is_never()).unwrap_or(false) {
            TypeId::NEVER
        } else {
            TypeId::UNIT
        };
        let kind = match pat {
            Some(p) => HKind::IfLet { pat: p, value: Box::new(c), then: Box::new(th), else_: eh.map(Box::new) },
            None => HKind::If { cond: Box::new(c), then: Box::new(th), else_: eh.map(Box::new) },
        };
        mk(kind, ty, span)
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm], hint: Option<TypeId>, want: bool, span: Span) -> HExpr {
        let s = self.infer(scrutinee, None);
        let st = s.ty;
        let mut harms = Vec::new();
        let mut ty: Option<TypeId> = None;
        let mut all_never = true;
        for arm in arms {
            self.push_scope();
            let pat = self.check_pattern(&arm.pattern, st, false);
            let guard = arm.guard.as_ref().map(|g| self.check_expr(g, TypeId::BOOL));
            let body = self.infer_inner(&arm.body, if want { hint.or(ty) } else { None }, want);
            self.pop_scope();
            if !self.resolve(body.ty).is_never() {
                all_never = false;
            }
            if want {
                ty = Some(match ty {
                    None => body.ty,
                    Some(t) => self.join(t, body.ty, arm.body.span),
                });
            }
            harms.push(HArm { pat, guard, body });
        }
        self.check_exhaustive(st, &harms, span);
        let ty = if want {
            ty.unwrap_or(TypeId::NEVER)
        } else if all_never && !arms.is_empty() {
            TypeId::NEVER
        } else {
            TypeId::UNIT
        };
        mk(HKind::Match { scrut: Box::new(s), arms: harms }, ty, span)
    }

    /// How a `for` loop or comprehension clause iterates over `ih`, and the
    /// element type.
    fn iter_plan(&mut self, ih: &HExpr, span: Span) -> (IterPlan, TypeId) {
        let builtin = |k: IterKind, t: TypeId| (IterPlan::Builtin(k), t);
        let ty = self.resolve(ih.ty);
        match self.ty(ty) {
            Ty::Range => builtin(IterKind::Range, TypeId::INT),
            Ty::List(e) => builtin(IterKind::List, e),
            Ty::Map(k, v) => {
                let pair = self.tuple_ty(&[k, v]);
                builtin(IterKind::Map, pair)
            }
            Ty::Set(e) => builtin(IterKind::Set, e),
            Ty::Named(d, args) if Some(d) == self.iter_struct => {
                // `Iter<T>`: call its `next` function directly.
                let elem = self.arena.elem(args, 0);
                let local = self.hidden_local(ty, span);
                let opt = self.option_of(elem);
                let fty = self.fn_ty(&[], opt);
                self.fs.data[local as usize].ty = fty;
                let callee = Box::new(mk(HKind::Local(local), fty, span));
                let next = mk(HKind::CallValue { callee, args: vec![] }, opt, span);
                (IterPlan::Lazy { local, next: Box::new(next), next_fn: true }, elem)
            }
            Ty::Named(d, args) if d == self.wk.channel => builtin(IterKind::Channel, self.arena.elem(args, 0)),
            Ty::Error => builtin(IterKind::List, TypeId::ERROR),
            Ty::Var(_) => {
                self.infer_error(span, "type annotation needed before iterating");
                builtin(IterKind::List, TypeId::ERROR)
            }
            _ => {
                let iter_trait = self.iter_trait;
                match iter_trait.filter(|&t| self.implements_trait(ty, t)) {
                    // Any other `Iterator<T>`: call its `next` method.
                    Some(t) => {
                        let elem = self.fresh();
                        let local = self.hidden_local(ty, span);
                        let recv = mk(HKind::Local(local), ty, span);
                        let slot = self.traits[t as usize].slot(Symbol::NEXT).unwrap_or(0);
                        let tr = TraitRef { trait_id: t, args: self.arena.intern_list(&[elem]) };
                        let next = self.call_trait_method(tr, slot, ty, Some(recv), &CallArgs::default(), span);
                        (IterPlan::Lazy { local, next: Box::new(next), next_fn: false }, elem)
                    }
                    None => {
                        let msg = format!("type {} is not iterable (it does not implement `Iterator`)", self.show(ty));
                        self.error(span, msg);
                        builtin(IterKind::List, TypeId::ERROR)
                    }
                }
            }
        }
    }

    /// The loop over `ih` that binds `pat` to each element and runs `body`.
    fn iter_loop(&mut self, plan: IterPlan, ih: HExpr, pat: HPat, body: HExpr, span: Span) -> HExpr {
        match plan {
            IterPlan::Builtin(IterKind::List) if self.is_enumerate_for(&ih, &pat) => self.enumerate_for(ih, pat, body, span),
            IterPlan::Builtin(IterKind::List) => {
                let (ih, kind) = match self.chars_source(ih) {
                    Ok(s) => (s, IterKind::Chars),
                    Err(ih) => (ih, IterKind::List),
                };
                mk(HKind::For { pat, iter: Box::new(ih), kind, body: Box::new(body) }, TypeId::UNIT, span)
            }
            IterPlan::Builtin(kind) => mk(HKind::For { pat, iter: Box::new(ih), kind, body: Box::new(body) }, TypeId::UNIT, span),
            // `{ let it = ih; while let Some(pat) = <next> { body } }`, or a
            // fused pipeline (see `fuse`).
            IterPlan::Lazy { local, next, next_fn } => {
                if self.can_fuse(&ih) {
                    return self.fuse_for(ih, pat, body, span);
                }
                let ih = if next_fn {
                    let fty = self.fs.data[local as usize].ty;
                    mk(HKind::Field { obj: Box::new(ih), index: 0 }, fty, span)
                } else {
                    ih
                };
                let it = HPat { kind: HPatKind::Bind(local), ty: ih.ty, span };
                let init = mk(HKind::Let { pat: it, init: Box::new(ih), else_: None }, TypeId::UNIT, span);
                let some = self.types[self.wk.option as usize].variants()[0].rt_id;
                let pat = HPat { kind: HPatKind::Variant { rt_id: some, args: vec![pat] }, ty: next.ty, span };
                let lp = mk(HKind::WhileLet { pat, value: next, body: Box::new(body) }, TypeId::UNIT, span);
                mk(HKind::Block(vec![init, lp]), TypeId::UNIT, span)
            }
        }
    }

    /// Enters a loop (checking its label) for the `break`s and `continue`s in its body.
    fn enter_loop(&mut self, label: Option<&ast::Ident>, break_ty: Option<TypeId>) {
        let ctx = self.fs.ctx.last_mut().unwrap();
        if let Some(l) = label
            && ctx.loops.iter().any(|x| x.label.as_deref() == Some(l.name.as_str()))
        {
            let msg = format!("label `@{}` is already used by an enclosing loop", l.name);
            self.error(l.span, msg);
        }
        let ctx = self.fs.ctx.last_mut().unwrap();
        ctx.loops.push(LoopCtx { break_ty, label: label.map(|l| l.name.clone()), hidden: false, broke: false });
    }

    /// Enters the loop of a comprehension clause.
    fn enter_hidden_loop(&mut self) {
        self.fs.ctx.last_mut().unwrap().loops.push(LoopCtx { break_ty: None, label: None, hidden: true, broke: false });
    }

    /// Leaves a loop; returns whether a `break` targets it.
    fn leave_loop(&mut self) -> bool {
        self.fs.ctx.last_mut().unwrap().loops.pop().is_some_and(|l| l.broke)
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
        if let ExprKind::Range { start: Some(s), end: Some(e), inclusive } = &iter.kind
            && self.optimize
        {
            let sh = self.check_expr(s, TypeId::INT);
            let eh = self.check_expr(e, TypeId::INT);
            self.push_scope();
            let pat = self.check_pattern(pattern, TypeId::INT, false);
            if !self.pattern_irrefutable(&pat) {
                self.error(pattern.span, "refutable pattern in `for` loop");
            }
            self.enter_loop(label, None);
            let b = self.check_block(body, None, false);
            self.leave_loop();
            self.pop_scope();
            return mk(
                HKind::ForRange { pat, start: Box::new(sh), end: Box::new(eh), inclusive: *inclusive, body: Box::new(b) },
                TypeId::UNIT,
                span,
            );
        }
        let ih = self.infer(iter, None);
        let (plan, elem) = self.iter_plan(&ih, iter.span);
        self.push_scope();
        let pat = self.check_pattern(pattern, elem, false);
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
        hint: Option<TypeId>,
        span: Span,
    ) -> HExpr {
        let is_map = b.is_some();
        let (acc_ty, elem_hint, val_hint) = if is_map {
            let (k, v) = match hint.map(|h| self.rty(h)) {
                Some(Ty::Map(k, v)) => (k, v),
                _ => (self.fresh(), self.fresh()),
            };
            (self.map_ty(k, v), k, Some(v))
        } else {
            let e = match hint.map(|h| self.rty(h)) {
                Some(Ty::List(e)) => e,
                _ => self.fresh(),
            };
            (self.list_ty(e), e, None)
        };
        let acc = self.hidden_local(acc_ty, span);
        self.push_scope();
        let inner = self.comp_clauses(clauses, a.unwrap(), b, acc, elem_hint, val_hint, span);
        self.pop_scope();
        if is_map {
            self.require_bound(elem_hint, Bound::Hash, span);
        }
        let init = if is_map { mk(HKind::Map(vec![]), acc_ty, span) } else { mk(HKind::List(vec![]), acc_ty, span) };
        let stmts = vec![
            mk(
                HKind::Let { pat: HPat { kind: HPatKind::Bind(acc), ty: acc_ty, span }, init: Box::new(init), else_: None },
                TypeId::UNIT,
                span,
            ),
            inner,
            mk(HKind::Local(acc), acc_ty, span),
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
        et: TypeId,
        vt: Option<TypeId>,
        span: Span,
    ) -> HExpr {
        let Some((first, rest)) = clauses.split_first() else {
            let ah = self.check_expr(a, et);
            return match (b, vt) {
                (Some(b), Some(vt)) => {
                    let bh = self.check_expr(b, vt);
                    mk(HKind::InsertLocal { map: acc, key: Box::new(ah), value: Box::new(bh) }, TypeId::UNIT, span)
                }
                _ => mk(HKind::PushLocal { list: acc, value: Box::new(ah) }, TypeId::UNIT, span),
            };
        };
        match first {
            CompClause::If(c) => {
                let ch = self.check_expr(c, TypeId::BOOL);
                let inner = self.comp_clauses(rest, a, b, acc, et, vt, span);
                mk(HKind::If { cond: Box::new(ch), then: Box::new(inner), else_: None }, TypeId::UNIT, span)
            }
            CompClause::For(p, it) => {
                if let ExprKind::Range { start: Some(s), end: Some(e), inclusive } = &it.kind
                    && self.optimize
                {
                    let sh = self.check_expr(s, TypeId::INT);
                    let eh = self.check_expr(e, TypeId::INT);
                    self.push_scope();
                    let pat = self.check_pattern(p, TypeId::INT, false);
                    self.enter_hidden_loop();
                    let inner = self.comp_clauses(rest, a, b, acc, et, vt, span);
                    self.leave_loop();
                    self.pop_scope();
                    return mk(
                        HKind::ForRange { pat, start: Box::new(sh), end: Box::new(eh), inclusive: *inclusive, body: Box::new(inner) },
                        TypeId::UNIT,
                        span,
                    );
                }
                let ih = self.infer(it, None);
                let (plan, elem) = self.iter_plan(&ih, it.span);
                self.push_scope();
                let pat = self.check_pattern(p, elem, false);
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

    fn check_lambda(&mut self, params: &[ast::LambdaParam], body: &Expr, hint: Option<TypeId>, span: Span) -> HExpr {
        let hinted = match hint.map(|h| self.rty(h)) {
            Some(Ty::Fn(ps, r)) if self.arena.list_len(ps) == params.len() => Some((self.elems(ps), r)),
            _ => None,
        };
        let ret = match &hinted {
            Some((_, r)) => *r,
            None => self.fresh(),
        };
        let id = self.new_lambda();
        self.fs.ctx.push(FnCtx { ret, kind: CtxKind::Lambda, loops: vec![], lambda: id, in_defer: None });
        self.push_scope();
        let mut locals = Vec::new();
        let mut ptys = Vec::new();
        self.check_param_names(params.iter().map(|p| &p.name));
        for (i, p) in params.iter().enumerate() {
            let t = match (&p.ty, &hinted) {
                (Some(t), h) => {
                    let t = self.resolve_annotation(t);
                    if let Some((hps, _)) = h {
                        self.expect_type(hps[i], t, p.name.span);
                    }
                    t
                }
                (None, Some((hps, _))) => hps[i],
                (None, None) => self.fresh(),
            };
            ptys.push(t);
            locals.push(self.declare_local(&p.name.name, t, false, p.name.span, true));
        }
        let b = self.infer(body, Some(ret));
        let rt = self.resolve(ret);
        if !self.resolve(b.ty).is_never() {
            if rt == TypeId::UNIT {
                // A lambda expected to return unit may end in any expression.
                self.unify(ret, TypeId::UNIT);
            } else {
                self.expect_type(ret, b.ty, body.span);
            }
        }
        self.pop_scope();
        self.fs.ctx.pop();
        let body_h = if self.resolve(ret) == TypeId::UNIT && !matches!(self.rty(b.ty), Ty::Unit | Ty::Never) {
            let bspan = b.span;
            mk(HKind::Block(vec![b, unit(bspan)]), TypeId::UNIT, bspan)
        } else {
            b
        };
        let fty = self.fn_ty(&ptys, ret);
        mk(HKind::Lambda(Box::new(HLambda { id, name: "<lambda>".into(), params: locals, body: body_h, self_local: None })), fty, span)
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
/// A format width or precision. Parsed as `u64` and saturated, so that a
/// number too large for a 32-bit `usize` gets the same "too large" error as
/// on 64-bit targets.
fn parse_spec_number(digits: &str) -> Option<usize> {
    digits.parse::<u64>().ok().map(crate::vm::value::u64_to_usize_saturating)
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
        spec.width = Some(parse_spec_number(&w).ok_or_else(|| format!("invalid format spec `{s}`: width too large"))?);
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
        spec.precision = Some(parse_spec_number(&p).ok_or_else(|| format!("invalid format spec `{s}`: precision too large"))?);
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

/// The name at the root of a namespace path (`m` of `m.T`).
fn ns_root_name(e: &Expr) -> &str {
    match &e.kind {
        ExprKind::Ident(id) => &id.name,
        ExprKind::Field(b, _) => ns_root_name(b),
        _ => "",
    }
}
