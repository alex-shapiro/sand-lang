//! Unification, bounds and zonking.

use super::{Checker, zonk_budget};
use crate::diag::Span;
use crate::hir::*;
use crate::types::*;
use std::collections::HashSet;

impl Checker<'_> {
    pub fn fresh(&mut self) -> Type {
        self.subst.push(None);
        Type::Var((self.subst.len() - 1) as VarId)
    }

    /// Follows variable bindings at the top level.
    pub fn resolve(&self, t: &Type) -> Type {
        let mut t = t.clone();
        while let Type::Var(v) = t {
            match &self.subst[v as usize] {
                Some(b) => t = b.clone(),
                None => break,
            }
        }
        t
    }

    pub fn zonk(&self, t: &Type) -> Type {
        zonk_budget(&self.subst, t, &self.budget)
    }

    /// Whether the (unresolved) variable `v` occurs in `t`.
    pub(crate) fn occurs_in(&self, v: &Type, t: &Type) -> bool {
        match self.resolve(v) {
            Type::Var(x) => self.occurs(x, t),
            _ => false,
        }
    }

    fn occurs(&self, v: VarId, t: &Type) -> bool {
        if !self.budget.charge(1) {
            // Out of budget: refuse the binding rather than risk a cycle.
            return true;
        }
        let t = self.resolve(t);
        match &t {
            Type::Var(w) => *w == v,
            Type::List(a) | Type::Set(a) => self.occurs(v, a),
            Type::Map(a, b) => self.occurs(v, a) || self.occurs(v, b),
            Type::Tuple(ts) | Type::Named(_, ts) => ts.iter().any(|x| self.occurs(v, x)),
            Type::Fn(ps, r) => ps.iter().any(|x| self.occurs(v, x)) || self.occurs(v, r),
            _ => false,
        }
    }

    pub fn unify(&mut self, a: &Type, b: &Type) -> bool {
        if !self.budget.charge(1) {
            // Out of budget: compilation fails anyway; avoid cascading errors.
            return true;
        }
        let a = self.resolve(a);
        let b = self.resolve(b);
        match (&a, &b) {
            (Type::Error, _) | (_, Type::Error) => true,
            (Type::Never, _) | (_, Type::Never) => true,
            (Type::Var(x), Type::Var(y)) if x == y => true,
            (Type::Var(x), t) | (t, Type::Var(x)) => {
                if self.occurs(*x, t) {
                    return false;
                }
                self.subst[*x as usize] = Some(t.clone());
                if let Some(log) = &mut self.unify_log {
                    log.push(*x);
                }
                true
            }
            (Type::Int, Type::Int)
            | (Type::Float, Type::Float)
            | (Type::Bool, Type::Bool)
            | (Type::Str, Type::Str)
            | (Type::Bytes, Type::Bytes)
            | (Type::Duration, Type::Duration)
            | (Type::Unit, Type::Unit)
            | (Type::Range, Type::Range) => true,
            (Type::List(x), Type::List(y)) | (Type::Set(x), Type::Set(y)) => self.unify(x, y),
            (Type::Map(k1, v1), Type::Map(k2, v2)) => self.unify(k1, k2) && self.unify(v1, v2),
            (Type::Tuple(xs), Type::Tuple(ys)) => xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| self.unify(x, y)),
            (Type::Named(d1, xs), Type::Named(d2, ys)) => {
                d1 == d2 && xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| self.unify(x, y))
            }
            (Type::Fn(p1, r1), Type::Fn(p2, r2)) => {
                p1.len() == p2.len() && p1.iter().zip(p2).all(|(x, y)| self.unify(x, y)) && self.unify(r1, r2)
            }
            (Type::Param(p), Type::Param(q)) => p == q,
            _ => false,
        }
    }

    /// Unifies, reporting `type mismatch: expected X, found Y` on failure.
    pub fn expect_type(&mut self, expected: &Type, found: &Type, span: Span) -> bool {
        if self.unify(expected, found) {
            return true;
        }
        let (e, f) = self.show_pair(expected, found);
        let msg = format!("type mismatch: expected {e}, found {f}");
        self.error(span, msg);
        false
    }

    /// The type of a value that is either `a` or `b` (branches).
    pub fn join(&mut self, a: &Type, b: &Type, span: Span) -> Type {
        let ra = self.resolve(a);
        let rb = self.resolve(b);
        if ra.is_never() {
            return rb;
        }
        if rb.is_never() {
            return ra;
        }
        self.expect_type(&ra, &rb, span);
        ra
    }

    /// Whether `t` satisfies `b`; `None` if unknown (unresolved variable).
    pub fn satisfies(&self, t: &Type, b: Bound) -> Option<bool> {
        let mut visiting = HashSet::new();
        self.satisfies_inner(t, b, &mut visiting)
    }

    fn satisfies_inner(&self, t: &Type, b: Bound, visiting: &mut HashSet<(TypeDefId, Bound)>) -> Option<bool> {
        if !self.budget.charge(1) {
            return Some(true);
        }
        let t = self.resolve(t);
        let all = |c: &Checker, ts: &[Type], visiting: &mut HashSet<(TypeDefId, Bound)>| -> Option<bool> {
            let mut unknown = false;
            for x in ts {
                match c.satisfies_inner(x, b, visiting) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    _ => {}
                }
            }
            if unknown { None } else { Some(true) }
        };
        Some(match &t {
            Type::Var(_) => return None,
            Type::Error | Type::Never => true,
            Type::Int => true,
            Type::Float => b != Bound::Hash,
            Type::Bool | Type::Str | Type::Bytes | Type::Duration | Type::Unit => b != Bound::Num,
            Type::Range => matches!(b, Bound::Eq | Bound::Hash),
            Type::List(x) => match b {
                Bound::Eq | Bound::Ord => return self.satisfies_inner(x, b, visiting),
                _ => false,
            },
            Type::Set(x) => match b {
                Bound::Eq => return self.satisfies_inner(x, b, visiting),
                _ => false,
            },
            Type::Map(k, v) => match b {
                Bound::Eq => return all(self, &[(**k).clone(), (**v).clone()], visiting),
                _ => false,
            },
            Type::Tuple(ts) => {
                if b == Bound::Num {
                    false
                } else {
                    return all(self, ts, visiting);
                }
            }
            Type::Fn(..) => false,
            Type::Param(p) => self.params[*p as usize].has_bound(b),
            Type::Named(d, args) => {
                if b == Bound::Num {
                    return Some(false);
                }
                if !visiting.insert((*d, b)) {
                    return Some(true);
                }
                let def = &self.types[*d as usize];
                let gens = def.generics.clone();
                let sub = |ty: &Type| ty.subst_params(&|p| gens.iter().position(|&g| g == p).map(|i| args[i].clone()));
                let r = match &def.kind {
                    TypeDefKind::Opaque => b == Bound::Eq,
                    TypeDefKind::Struct { fields } => {
                        if b == Bound::Hash && fields.iter().any(|f| f.mutable) {
                            false
                        } else {
                            let tys: Vec<Type> = fields.iter().map(|f| sub(&f.ty)).collect();
                            match all(self, &tys, visiting) {
                                Some(x) => x,
                                None => {
                                    visiting.remove(&(*d, b));
                                    return None;
                                }
                            }
                        }
                    }
                    TypeDefKind::Enum { variants } => {
                        let tys: Vec<Type> = variants.iter().flat_map(|v| v.fields.iter().map(sub)).collect();
                        match all(self, &tys, visiting) {
                            Some(x) => x,
                            None => {
                                visiting.remove(&(*d, b));
                                return None;
                            }
                        }
                    }
                };
                visiting.remove(&(*d, b));
                r
            }
        })
    }

    /// Checks a bound now, or defers it if the type is not yet known.
    pub fn require_bound(&mut self, t: &Type, b: Bound, span: Span) {
        match self.satisfies(t, b) {
            Some(true) => {}
            Some(false) => self.report_bound(t, b, span),
            None => self.fs.obligations.push((t.clone(), b, span)),
        }
    }

    pub fn check_bound(&mut self, t: &Type, b: Bound, span: Span, _final: bool) {
        if self.satisfies(t, b) == Some(false) {
            self.report_bound(t, b, span);
        }
    }

    fn report_bound(&mut self, t: &Type, b: Bound, span: Span) {
        let z = self.zonk(t);
        let msg = match z {
            Type::Param(p) => {
                let n = self.params[p as usize].name.clone();
                format!("type parameter `{n}` does not satisfy {}; add a bound `{n}: {}`", b.name(), b.name())
            }
            _ => format!("{} does not satisfy {}", self.show(&z), b.name()),
        };
        self.error(span, msg);
    }

    // ------------------------------------------------------------------
    // Zonking HIR

    pub fn zonk_expr(&self, e: &mut HExpr) {
        e.ty = self.zonk(&e.ty);
        match &mut e.kind {
            HKind::Let { pat, init, else_ } => {
                self.zonk_pat(pat);
                self.zonk_expr(init);
                if let Some(x) = else_ {
                    self.zonk_expr(x)
                }
            }
            HKind::LocalFns { lambdas, .. } => lambdas.iter_mut().for_each(|l| self.zonk_expr(l)),
            HKind::Assign { place, op_ty, value, .. } => {
                *op_ty = self.zonk(op_ty);
                self.zonk_place(place);
                self.zonk_expr(value);
            }
            HKind::Block(xs) | HKind::Tuple(xs) | HKind::List(xs) => xs.iter_mut().for_each(|x| self.zonk_expr(x)),
            HKind::If { cond, then, else_ } => {
                self.zonk_expr(cond);
                self.zonk_expr(then);
                if let Some(x) = else_ {
                    self.zonk_expr(x)
                }
            }
            HKind::IfLet { pat, value, then, else_ } => {
                self.zonk_pat(pat);
                self.zonk_expr(value);
                self.zonk_expr(then);
                if let Some(x) = else_ {
                    self.zonk_expr(x)
                }
            }
            HKind::Match { scrut, arms } => {
                self.zonk_expr(scrut);
                for a in arms {
                    self.zonk_pat(&mut a.pat);
                    if let Some(g) = &mut a.guard {
                        self.zonk_expr(g);
                    }
                    self.zonk_expr(&mut a.body);
                }
            }
            HKind::Loop(b) => self.zonk_expr(b),
            HKind::While { cond, body } => {
                self.zonk_expr(cond);
                self.zonk_expr(body);
            }
            HKind::WhileLet { pat, value, body } => {
                self.zonk_pat(pat);
                self.zonk_expr(value);
                self.zonk_expr(body);
            }
            HKind::For { pat, iter, body, .. } => {
                self.zonk_pat(pat);
                self.zonk_expr(iter);
                self.zonk_expr(body);
            }
            HKind::ForRange { pat, start, end, body, .. } => {
                self.zonk_pat(pat);
                self.zonk_expr(start);
                self.zonk_expr(end);
                self.zonk_expr(body);
            }
            HKind::Break { value: x, .. } | HKind::Return(x) => {
                if let Some(x) = x {
                    self.zonk_expr(x)
                }
            }
            HKind::Call { args, .. } | HKind::CallTrait { args, .. } => self.zonk_args(args),
            HKind::Evidence(b) => {
                b.ty = self.zonk(&b.ty);
                for t in &mut b.tr.args {
                    *t = self.zonk(t);
                }
            }
            HKind::Dict { args, .. } => args.iter_mut().for_each(|x| self.zonk_expr(x)),
            HKind::CallNative { args, reflect, .. } => {
                self.zonk_args(args);
                for t in reflect {
                    *t = self.zonk(t);
                }
            }
            HKind::CallValue { callee, args } => {
                self.zonk_expr(callee);
                args.iter_mut().for_each(|x| self.zonk_expr(x));
            }
            HKind::MakeVariant { args, .. } | HKind::MakeStruct { fields: args, .. } => args.iter_mut().for_each(|x| self.zonk_expr(x)),
            HKind::Map(kvs) => {
                for (k, v) in kvs {
                    self.zonk_expr(k);
                    self.zonk_expr(v);
                }
            }
            HKind::Repeat { elem, count } => {
                self.zonk_expr(elem);
                self.zonk_expr(count);
            }
            HKind::PushLocal { value, .. } => self.zonk_expr(value),
            HKind::InsertLocal { key, value, .. } => {
                self.zonk_expr(key);
                self.zonk_expr(value);
            }
            HKind::Field { obj, .. } => self.zonk_expr(obj),
            HKind::Index { obj, index, .. } => {
                self.zonk_expr(obj);
                self.zonk_expr(index);
            }
            HKind::Slice { obj, start, end, .. } => {
                self.zonk_expr(obj);
                if let Some(x) = start {
                    self.zonk_expr(x)
                }
                if let Some(x) = end {
                    self.zonk_expr(x)
                }
            }
            HKind::Unary { e, .. } | HKind::ToStr(e) | HKind::Debug(e) | HKind::Spawn(e) | HKind::Defer(e) => self.zonk_expr(e),
            HKind::Try { e, .. } => self.zonk_expr(e),
            HKind::Arith { l, r, .. } | HKind::Cmp { l, r, .. } | HKind::And(l, r) | HKind::Or(l, r) | HKind::Coalesce { l, r, .. } => {
                self.zonk_expr(l);
                self.zonk_expr(r);
            }
            HKind::Range { start, end, .. } => {
                self.zonk_expr(start);
                self.zonk_expr(end);
            }
            HKind::Lambda(l) => self.zonk_expr(&mut l.body),
            HKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        self.zonk_expr(x);
                    }
                }
            }
            HKind::Unit
            | HKind::Bool(_)
            | HKind::Int(_)
            | HKind::Float(_)
            | HKind::Dur(_)
            | HKind::Str(_)
            | HKind::Bytes(_)
            | HKind::Local(_)
            | HKind::Global(_)
            | HKind::FnValue(_)
            | HKind::VariantFn { .. }
            | HKind::Tag(_)
            | HKind::Continue { .. }
            | HKind::Error => {}
        }
    }

    fn zonk_args(&self, args: &mut [HArg]) {
        for a in args {
            match a {
                HArg::Expr(e) => self.zonk_expr(e),
                HArg::Default { ty, .. } => *ty = self.zonk(ty),
            }
        }
    }

    fn zonk_place(&self, p: &mut Place) {
        match p {
            Place::Field { obj, .. } => self.zonk_expr(obj),
            Place::Index { obj, index, .. } => {
                self.zonk_expr(obj);
                self.zonk_expr(index);
            }
            _ => {}
        }
    }

    pub fn zonk_pat(&self, p: &mut HPat) {
        p.ty = self.zonk(&p.ty);
        match &mut p.kind {
            HPatKind::Tuple(ps) | HPatKind::Or(ps) => ps.iter_mut().for_each(|x| self.zonk_pat(x)),
            HPatKind::Variant { args, .. } => args.iter_mut().for_each(|x| self.zonk_pat(x)),
            HPatKind::Struct { fields } => fields.iter_mut().for_each(|(_, x)| self.zonk_pat(x)),
            HPatKind::List { before, after, .. } => {
                before.iter_mut().for_each(|x| self.zonk_pat(x));
                after.iter_mut().for_each(|x| self.zonk_pat(x));
            }
            _ => {}
        }
    }
}
