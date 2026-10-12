//! Unification, bounds and zonking.
//!
//! Inference variables are nodes of the type arena (`Ty::Var`) whose
//! bindings are kept in `Checker::subst`; types are hash-consed, so equal
//! types are equal ids and most comparisons are integer comparisons.

use super::Checker;
use super::traits::is_const_dict;
use crate::diag::Span;
use crate::hir::*;
use crate::types::*;

impl Checker {
    pub fn fresh(&mut self) -> TypeId {
        self.subst.push(None);
        self.arena.var((self.subst.len() - 1) as VarId)
    }

    /// Follows variable bindings at the top level.
    #[inline]
    pub fn resolve(&self, mut t: TypeId) -> TypeId {
        while let Ty::Var(v) = self.arena.get(t) {
            match self.subst.get(v as usize) {
                Some(Some(b)) => t = *b,
                _ => break,
            }
        }
        t
    }

    /// Resolves the inference variables in `t`, charging the budget one unit
    /// per node visited; once the budget is exhausted the rest of the type
    /// is `ERROR`.
    pub fn zonk(&mut self, t: TypeId) -> TypeId {
        if !self.arena.has_vars(t) {
            return t;
        }
        let Checker { arena, subst, budget, .. } = self;
        zonk_in(arena, subst, budget, t)
    }

    /// Whether the (unresolved) variable `v` occurs in `t`.
    pub(crate) fn occurs_in(&self, v: TypeId, t: TypeId) -> bool {
        match self.rty(v) {
            Ty::Var(x) => self.occurs(x, t),
            _ => false,
        }
    }

    fn occurs(&self, v: VarId, t: TypeId) -> bool {
        if !self.arena.has_vars(t) {
            return false;
        }
        if !self.budget.charge(1) {
            // Out of budget: deny the binding rather than risk a cycle.
            return true;
        }
        match self.rty(t) {
            Ty::Var(w) => w == v,
            Ty::List(a) | Ty::Set(a) => self.occurs(v, a),
            Ty::Map(a, b) => self.occurs(v, a) || self.occurs(v, b),
            Ty::Tuple(l) | Ty::Named(_, l) => self.arena.list(l).iter().any(|&x| self.occurs(v, x)),
            Ty::Fn(l, r) => self.arena.list(l).iter().any(|&x| self.occurs(v, x)) || self.occurs(v, r),
            _ => false,
        }
    }

    pub fn unify(&mut self, a: TypeId, b: TypeId) -> bool {
        if a == b {
            return true;
        }
        if !self.budget.charge(1) {
            // Out of budget: compilation fails anyway; avoid cascading errors.
            return true;
        }
        let a = self.resolve(a);
        let b = self.resolve(b);
        if a == b {
            return true;
        }
        match (self.ty(a), self.ty(b)) {
            (Ty::Error, _) | (_, Ty::Error) => true,
            (Ty::Var(x), _) => self.bind(x, b),
            (_, Ty::Var(y)) => self.bind(y, a),
            (Ty::List(x), Ty::List(y)) | (Ty::Set(x), Ty::Set(y)) => self.unify(x, y),
            (Ty::Map(k1, v1), Ty::Map(k2, v2)) => self.unify(k1, k2) && self.unify(v1, v2),
            (Ty::Tuple(xs), Ty::Tuple(ys)) => self.unify_lists(xs, ys),
            (Ty::Named(d1, xs), Ty::Named(d2, ys)) => d1 == d2 && self.unify_lists(xs, ys),
            (Ty::Fn(p1, r1), Ty::Fn(p2, r2)) => self.unify_lists(p1, p2) && self.unify(r1, r2),
            // Equal primitives and parameters are equal ids.
            _ => false,
        }
    }

    fn unify_lists(&mut self, xs: TyList, ys: TyList) -> bool {
        let n = self.arena.list_len(xs);
        n == self.arena.list_len(ys) && (0..n).all(|i| self.unify(self.arena.elem(xs, i), self.arena.elem(ys, i)))
    }

    /// Binds unbound variable `x` to `t` (resolved, not `x` itself).
    fn bind(&mut self, x: VarId, t: TypeId) -> bool {
        if self.occurs(x, t) {
            return false;
        }
        self.subst[x as usize] = Some(t);
        if x < self.fs.var_mark {
            self.fs.old_bound.push(x);
        }
        if let Some(log) = &mut self.unify_log {
            log.push(x);
        }
        true
    }

    /// Whether a value of type `found` can be used where `expected` is
    /// required: the types unify, or the value is `never` (its expression
    /// diverges). Only a whole value's type may be `never` here: `[never]`
    /// or `fn() -> never` unify with nothing else.
    pub fn coerce(&mut self, expected: TypeId, found: TypeId) -> bool {
        self.resolve(found).is_never() || self.unify(expected, found)
    }

    /// Coerces (see [`Checker::coerce`]), reporting `type mismatch: expected
    /// X, found Y` on failure.
    pub fn expect_type(&mut self, expected: TypeId, found: TypeId, span: Span) -> bool {
        if self.coerce(expected, found) {
            return true;
        }
        let (e, f) = self.show_pair(expected, found);
        let msg = format!("type mismatch: expected {e}, found {f}");
        self.error(span, msg);
        false
    }

    /// The type of a value that is either `a` or `b`.
    pub fn join(&mut self, a: TypeId, b: TypeId, span: Span) -> TypeId {
        let ra = self.resolve(a);
        let rb = self.resolve(b);
        if ra.is_never() {
            return rb;
        }
        if rb.is_never() {
            return ra;
        }
        self.expect_type(ra, rb, span);
        ra
    }

    /// Whether `t` satisfies `b`; `None` if unknown (unresolved variable).
    pub fn satisfies(&self, t: TypeId, b: Bound) -> Option<bool> {
        if !self.budget.charge(1) {
            return Some(true);
        }
        let all = |ts: &mut dyn Iterator<Item = TypeId>| {
            let mut unknown = false;
            for x in ts {
                match self.satisfies(x, b) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        };
        let ty = self.rty(t);
        match ty {
            Ty::Var(_) => None,
            Ty::List(x) if matches!(b, Bound::Eq | Bound::Ord) => self.satisfies(x, b),
            Ty::Set(x) if b == Bound::Eq => self.satisfies(x, b),
            Ty::Map(k, v) if b == Bound::Eq => all(&mut [k, v].into_iter()),
            Ty::Tuple(l) if !matches!(b, Bound::Num | Bound::Add) => all(&mut self.arena.list(l).iter().copied()),
            Ty::Param(p) => Some(self.params[p as usize].has_bound(b)),
            Ty::Named(d, l) => match derived_index(b).and_then(|k| self.derived.get(d as usize).map(|x| &x[k])) {
                Some(Derived::Never) => Some(false),
                Some(Derived::Needs(ps)) => all(&mut ps.iter().map(|&i| self.arena.elem(l, i as usize))),
                // `Num` and `Add`; the bounds of a type still being declared are not known.
                None => Some(!matches!(b, Bound::Num | Bound::Add)),
            },
            _ => Some(prim_satisfies(ty, b)),
        }
    }

    /// Works out which built-in bounds the type definitions from `first` on
    /// (those of the module being checked) satisfy, once their fields and
    /// variants are known: see [`Derived`].
    pub(crate) fn derive_bounds(&mut self, first: usize) {
        let n = self.types.len();
        // From the optimistic start (every bound holds, requiring nothing),
        // refine until nothing changes: the greatest fixed point, so that a
        // recursive type satisfies a bound unless something in it does not.
        while self.derived.len() < n {
            self.derived.push(std::array::from_fn(|_| Derived::Needs(Vec::new())));
        }
        loop {
            let mut changed = false;
            for d in first..n {
                for (k, b) in DERIVED.into_iter().enumerate() {
                    let new = self.derive(d, b);
                    if new != self.derived[d][k] {
                        self.derived[d][k] = new;
                        changed = true;
                    }
                }
            }
            if !changed || !self.budget.check() {
                break;
            }
        }
    }

    /// What type definition `d` needs to satisfy `b`, from its fields and
    /// variants and what the other definitions need (so far).
    fn derive(&self, d: usize, b: Bound) -> Derived {
        let def = &self.types[d];
        let mut needs = Vec::new();
        let ok = match &def.kind {
            TypeDefKind::Opaque => b == Bound::Eq,
            TypeDefKind::Struct { fields } => {
                !(b == Bound::Hash && fields.iter().any(|f| f.mutable))
                    && fields.iter().all(|f| self.needs(f.ty, b, &def.generics, &mut needs))
            }
            TypeDefKind::Enum { variants } => variants.iter().flat_map(|v| &v.fields).all(|&t| self.needs(t, b, &def.generics, &mut needs)),
        };
        if !ok {
            return Derived::Never;
        }
        needs.sort_unstable();
        needs.dedup();
        Derived::Needs(needs)
    }

    /// Whether field type `t` of a definition with type parameters
    /// `generics` can satisfy `b`, adding to `needs` the parameters (as
    /// indices into `generics`) whose arguments must satisfy it too.
    fn needs(&self, t: TypeId, b: Bound, generics: &[ParamId], needs: &mut Vec<u32>) -> bool {
        self.budget.charge(1);
        let mut go = |t: TypeId| self.needs(t, b, generics, needs);
        let ty = self.ty(t);
        match ty {
            Ty::Param(p) => match generics.iter().position(|&g| g == p) {
                Some(i) => {
                    needs.push(i as u32);
                    true
                }
                None => self.params[p as usize].has_bound(b),
            },
            Ty::List(x) => matches!(b, Bound::Eq | Bound::Ord) && go(x),
            Ty::Set(x) => b == Bound::Eq && go(x),
            Ty::Map(k, v) => b == Bound::Eq && go(k) && go(v),
            Ty::Tuple(l) => !matches!(b, Bound::Num | Bound::Add) && self.arena.list(l).iter().all(|&x| go(x)),
            Ty::Named(e, l) => match derived_index(b).map(|k| &self.derived[e as usize][k]) {
                Some(Derived::Needs(ps)) => ps.iter().all(|&i| go(self.arena.elem(l, i as usize))),
                _ => false,
            },
            Ty::Var(_) => true,
            _ => prim_satisfies(ty, b),
        }
    }

    /// Checks a bound now, or defers it if the type is not yet known.
    pub fn require_bound(&mut self, t: TypeId, b: Bound, span: Span) {
        match self.satisfies(t, b) {
            Some(true) => {}
            Some(false) => self.report_bound(t, b, span),
            None => self.fs.obligations.push((t, b, span)),
        }
    }

    pub(crate) fn report_bound(&mut self, t: TypeId, b: Bound, span: Span) {
        let msg = match self.rty(t) {
            Ty::Param(p) if b == Bound::Add => {
                let n = self.name(self.params[p as usize].name);
                format!("operator `+` is not defined for type parameter `{n}`; add a bound `{n}: Num`")
            }
            _ if b == Bound::Add => format!("operator `+` is not defined for {}", self.show(t)),
            Ty::Param(p) => {
                let n = self.name(self.params[p as usize].name);
                format!("type parameter `{n}` does not satisfy {}; add a bound `{n}: {}`", b.name(), b.name())
            }
            _ => format!("{} does not satisfy {}", self.show(t), b.name()),
        };
        self.error(span, msg);
    }

    // ------------------------------------------------------------------
    // The final walk

    /// The final walk over a checked function body: resolves inference
    /// variables, puts the dictionaries in place of the evidence placeholders
    /// (noting the statically known impls of trait calls, and building the
    /// non-constant dictionaries used in loops or lambdas once, on entry),
    /// and marks the mutable locals that lambdas capture as cells. `lambda` is the lambda being walked
    /// (0: the function itself); `nested` tells whether it is in a loop or lambda.
    pub(crate) fn finish_expr(&mut self, e: &mut HExpr, fin: &mut Finish, lambda: u32, nested: bool) {
        e.ty = self.zonk(e.ty);
        match &mut e.kind {
            HKind::Evidence(i) => {
                let i = *i as usize;
                *e = HExpr { span: e.span, ..fin.dicts[i].clone() };
                if nested && fin.hoist && matches!(e.kind, HKind::Dict { .. }) && !is_const_dict(e) {
                    self.hoist_dict(e, &mut fin.hoisted);
                }
                return;
            }
            HKind::CallTrait { args, impl_id, .. } => {
                if let Some(HArg::Expr(HExpr { kind: HKind::Evidence(i), .. })) = args.first()
                    && let HKind::Dict { impl_id: id, .. } = fin.dicts[*i as usize].kind
                {
                    *impl_id = Some(id);
                }
            }
            HKind::Local(l)
            | HKind::Assign { place: Place::Local(l), .. }
            | HKind::PushLocal { list: l, .. }
            | HKind::InsertLocal { map: l, .. } => {
                let info = &mut self.fs.locals[*l as usize];
                if info.mutable && info.owner != lambda {
                    info.cell = true;
                }
            }
            _ => {}
        }
        let (lambda, nested) = match &e.kind {
            HKind::Lambda(l) => (l.id, true),
            HKind::Loop(_) | HKind::While { .. } | HKind::WhileLet { .. } | HKind::For { .. } | HKind::ForRange { .. } => (lambda, true),
            _ => (lambda, nested),
        };
        e.for_each_part(&mut |p| match p {
            Part::Expr(x) => self.finish_expr(x, fin, lambda, nested),
            Part::Pat(p) => self.zonk_pat(p),
            Part::Type(t) => *t = self.zonk(*t),
        });
    }

    pub fn zonk_pat(&mut self, p: &mut HPat) {
        p.ty = self.zonk(p.ty);
        p.for_each_subpattern(&mut |x| self.zonk_pat(x));
    }
}

/// Resolves the inference variables in `t` (see [`Checker::zonk`]).
pub(crate) fn zonk_in(arena: &mut TypeArena, subst: &[Option<TypeId>], budget: &super::budget::CompileBudget, t: TypeId) -> TypeId {
    if !arena.has_vars(t) {
        return t;
    }
    if !budget.charge(1) {
        return TypeId::ERROR;
    }
    match arena.get(t) {
        Ty::Var(v) => match subst.get(v as usize) {
            Some(Some(b)) => zonk_in(arena, subst, budget, *b),
            _ => t,
        },
        _ => arena.rebuild(t, &mut |a, x| zonk_in(a, subst, budget, x)),
    }
}

/// The state of the final walk of a function body ([`Checker::finish_expr`]).
pub(crate) struct Finish {
    /// The dictionary of each evidence placeholder.
    pub dicts: Vec<HExpr>,
    /// Whether non-constant dictionaries may occur (the function has dictionaries).
    pub hoist: bool,
    /// Dictionaries built on entry, in hidden locals.
    pub hoisted: Vec<(LocalId, HExpr)>,
}

/// What a type definition needs to satisfy a built-in bound.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Derived {
    /// A field type never satisfies the bound.
    Never,
    /// The arguments of these type parameters (indices) must satisfy it.
    Needs(Vec<u32>),
}

/// The bounds that type definitions may satisfy, in the order of `Checker::derived`.
const DERIVED: [Bound; 3] = [Bound::Eq, Bound::Ord, Bound::Hash];

fn derived_index(b: Bound) -> Option<usize> {
    DERIVED.iter().position(|&x| x == b)
}

/// Whether a type without components that may vary satisfies `b`.
fn prim_satisfies(t: Ty, b: Bound) -> bool {
    match t {
        Ty::Error | Ty::Never | Ty::Int => true,
        Ty::Float => b != Bound::Hash,
        Ty::Str | Ty::Bytes | Ty::Duration => b != Bound::Num,
        Ty::Bool | Ty::Unit => !matches!(b, Bound::Num | Bound::Add),
        Ty::Range => matches!(b, Bound::Eq | Bound::Hash),
        Ty::List(_) => b == Bound::Add,
        _ => false,
    }
}
