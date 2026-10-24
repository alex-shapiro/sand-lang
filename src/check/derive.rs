//! Derived impls of the built-in traits `Eq`, `Ord`, `Hash` and `Encode`.
//!
//! The checker derives these impls instead of reading them from source. A
//! type implements the trait if its components do: the elements of tuples
//! and collections, and the fields and payloads of structs and enums, as if
//! a struct `Pair<A, B>` declared `impl<A: Eq, B: Eq> Eq for Pair<A, B>`.
//! [`Checker::derived_parts`] gives the components of each type, or `None`
//! if the type cannot implement the trait (functions, `float: Hash`, a
//! struct with `var` fields: `Hash`).

use super::Checker;
use super::traits::{Unmet, impl_key};
use crate::types::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Derived {
    Never,
    /// Indices of the type parameters whose arguments must also implement the trait.
    Needs(Vec<u32>),
}

/// The derived built-in traits, by [`Builtin::derived_index`].
pub(crate) const DERIVED: [Builtin; 4] = [Builtin::Eq, Builtin::Ord, Builtin::Hash, Builtin::Encode];

impl Checker {
    /// The components of `ty` that must implement derived trait `k` for `ty`
    /// to implement it, or `None` if `ty` cannot implement it.
    pub(crate) fn derived_parts(&self, ty: Ty, k: usize) -> Option<Vec<TypeId>> {
        use Builtin::*;
        let b = DERIVED[k];
        match ty {
            Ty::Int | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Never | Ty::Error => Some(Vec::new()),
            Ty::Float => (b != Hash).then(Vec::new),
            Ty::Range => (b != Ord).then(Vec::new),
            Ty::List(x) => (b != Hash).then(|| vec![x]),
            Ty::Set(x) => matches!(b, Eq | Encode).then(|| vec![x]),
            Ty::Map(x, y) => matches!(b, Eq | Encode).then(|| vec![x, y]),
            Ty::Tuple(l) => Some(self.arena.list(l).to_vec()),
            Ty::Named(d, l) => match self.derived.get(d as usize).map(|x| &x[k]) {
                Some(Derived::Needs(ps)) => Some(ps.iter().map(|&i| self.arena.elem(l, i as usize)).collect()),
                _ => None,
            },
            Ty::Fn(..) | Ty::Param(_) | Ty::Var(_) => None,
        }
    }

    /// Proves `ty: tr` (zonked) for derived trait `k` through the components
    /// of `ty`. The components are visited with a work list rather than by
    /// recursion, because types nest deeply (`int???...`).
    pub(crate) fn resolve_derived(&mut self, ty: TypeId, tr: TraitRef, k: usize, depth: u32) -> Result<(), Box<Unmet>> {
        let underived = |c: &Checker| {
            let mut m = Unmet::new(format!("{} does not implement `{}`", c.show(ty), c.show_trait(&tr)));
            m.explain = Some((ty, tr, k));
            m
        };
        let mut unknown = None;
        let mut todo = vec![ty];
        let mut first = true;
        while let Some(t) = todo.pop() {
            if !self.budget.charge(1) {
                return Ok(());
            }
            let t = self.resolve(t);
            let node = self.ty(t);
            let explicit = !first && impl_key(&self.arena, t).is_some_and(|(c, n)| self.impls_by.contains_key(&(tr.trait_id, c, n)));
            first = false;
            if explicit || matches!(node, Ty::Param(_) | Ty::Var(_)) {
                match self.resolve_evidence(t, tr, depth + 1) {
                    Ok(_) => {}
                    Err(m) if m.unknown => unknown = Some(m),
                    Err(_) => return Err(underived(self)),
                }
                continue;
            }
            match self.derived_parts(node, k) {
                Some(parts) => todo.extend(parts),
                None => return Err(underived(self)),
            }
        }
        unknown.map_or(Ok(()), Err)
    }

    /// Computes the derived impls of the type definitions from `first` onward,
    /// using greatest fixed-point iteration so that mutually recursive types
    /// implement a trait unless a component type refutes it.
    pub(crate) fn derive_impls(&mut self, first: usize) {
        let n = self.types.len();
        while self.derived.len() < n {
            self.derived.push(std::array::from_fn(|_| Derived::Needs(Vec::new())));
        }
        let mut users: Vec<Vec<usize>> = vec![Vec::new(); n - first];
        for d in first..n {
            let members: Vec<TypeId> = match &self.types[d].kind {
                TypeDefKind::Struct { fields } => fields.iter().map(|f| f.ty).collect(),
                TypeDefKind::Enum { variants } => variants.iter().flat_map(|v| v.fields.iter().copied()).collect(),
                TypeDefKind::Opaque(_) => Vec::new(),
            };
            for t in members {
                self.arena.any(t, &mut |x| {
                    if let Ty::Named(e, _) = x
                        && let Some(u) = (e as usize).checked_sub(first).and_then(|i| users.get_mut(i))
                        && u.last() != Some(&d)
                    {
                        u.push(d);
                    }
                    false
                });
            }
        }
        for k in 0..DERIVED.len() {
            let mut todo: Vec<usize> = (first..n).collect();
            let mut queued = vec![true; n - first];
            while let Some(d) = todo.pop() {
                queued[d - first] = false;
                let new = self.derive(d, k);
                if new != self.derived[d][k] {
                    self.derived[d][k] = new;
                    for &u in &users[d - first] {
                        if !std::mem::replace(&mut queued[u - first], true) {
                            todo.push(u);
                        }
                    }
                }
                if !self.budget.check() {
                    return;
                }
            }
        }
    }

    /// The derived impl of trait `k` for type definition `d`, given the
    /// current state of `self.derived`.
    fn derive(&self, d: usize, k: usize) -> Derived {
        let def = &self.types[d];
        let b = DERIVED[k];
        let mut needs = Vec::new();
        let ok = match &def.kind {
            TypeDefKind::Opaque(kind) => match (b, kind) {
                (Builtin::Eq, _) | (Builtin::Hash | Builtin::Encode, Opaque::Value(_)) => true,
                (Builtin::Ord, Opaque::Value(v)) => v.ord,
                _ => false,
            },
            TypeDefKind::Struct { fields } => {
                !(b == Builtin::Hash && fields.iter().any(|f| f.mutable))
                    && fields.iter().all(|f| self.needs(f.ty, k, &def.generics, &mut needs))
            }
            TypeDefKind::Enum { variants } => variants.iter().flat_map(|v| &v.fields).all(|&t| self.needs(t, k, &def.generics, &mut needs)),
        };
        if !ok {
            return Derived::Never;
        }
        needs.sort_unstable();
        needs.dedup();
        Derived::Needs(needs)
    }

    /// Returns `true` if member type `t` can implement derived trait `k`, and
    /// appends the indices in `generics` of the type parameters it needs to `needs`.
    fn needs(&self, t: TypeId, k: usize, generics: &[ParamId], needs: &mut Vec<u32>) -> bool {
        self.budget.charge(1);
        match self.ty(t) {
            Ty::Param(p) => match generics.iter().position(|&g| g == p) {
                Some(i) => {
                    needs.push(i as u32);
                    true
                }
                None => self.builtins[DERIVED[k] as usize].is_some_and(|tid| self.param_has_trait(p, tid)),
            },
            Ty::Var(_) => true,
            ty => self.derived_parts(ty, k).is_some_and(|parts| parts.into_iter().all(|x| self.needs(x, k, generics, needs))),
        }
    }

    /// Fills in why `ty` does not implement derived trait `k`, if `m` asks for it.
    pub(crate) fn explain(&mut self, m: &mut Unmet) {
        let Some((ty, tr, k)) = m.explain.take() else { return };
        let Some((parent, step, leaf)) = self.failing_part(ty, tr, k) else { return };
        if let Ty::Param(p) = self.rty(leaf) {
            let n = self.name(self.params[p as usize].name);
            m.hint = Some(format!("add a bound `{n}: {}`", self.show_trait(&tr)));
        }
        let owner = self.show(parent);
        let child = self.show(step.child());
        m.because = Some(match step {
            Step::Elements(_) => format!("the elements of `{owner}` have type `{child}`"),
            Step::Keys(_) => format!("the keys of `{owner}` have type `{child}`"),
            Step::Values(_) => format!("the values of `{owner}` have type `{child}`"),
            Step::Element(i, _) => format!("element {i} of `{owner}` has type `{child}`"),
            Step::Field(f, _) => format!("field `{}` of `{owner}` has type `{child}`", self.name(f)),
            Step::Payload(v, _) => format!("the payload of variant `{}` of `{owner}` has type `{child}`", self.name(v)),
            Step::Mutable(f) => format!("field `{}` of `{owner}` is mutable", self.name(f)),
        });
    }

    /// Finds a component of `ty` that does not implement derived trait `k`,
    /// along a path of failing components that ends at a type that fails by
    /// itself. Returns the innermost field or payload step of the path (or its
    /// innermost step if it has none), the type that the step belongs to, and
    /// the type at the end of the path. Returns `None` if `ty` fails by itself
    /// or every path is a cycle.
    fn failing_part(&mut self, ty: TypeId, tr: TraitRef, k: usize) -> Option<(TypeId, Step, TypeId)> {
        struct Frame {
            ty: TypeId,
            /// The step from the previous frame's type.
            via: Option<Step>,
            parts: Vec<Step>,
            next: usize,
            failed: bool,
        }
        let mut seen = std::collections::HashSet::new();
        let ty = self.resolve(ty);
        seen.insert(ty);
        let mut stack = vec![Frame { ty, via: None, parts: self.steps(ty, k), next: 0, failed: false }];
        loop {
            if !self.budget.charge(1) {
                return None;
            }
            let top = stack.last_mut()?;
            if let Some(&Step::Mutable(_)) = top.parts.first() {
                break;
            }
            if top.next == top.parts.len() {
                if top.failed {
                    // Every failing component led to a cycle.
                    stack.pop();
                    continue;
                }
                break;
            }
            let (i, step) = (top.next, top.parts[top.next]);
            top.next += 1;
            // A failing type with a single component left unchecked fails through it.
            let deduced = !top.failed && i + 1 == top.parts.len();
            let x = self.resolve(step.child());
            if !deduced && self.implements(x, tr) != Some(false) {
                continue;
            }
            stack.last_mut()?.failed = true;
            if seen.insert(x) {
                let parts = self.steps(x, k);
                stack.push(Frame { ty: x, via: Some(step), parts, next: 0, failed: false });
            }
        }
        let leaf = stack.last()?.ty;
        if let Some(&step @ Step::Mutable(_)) = stack.last()?.parts.first() {
            return Some((leaf, step, leaf));
        }
        let member = |f: &Frame| matches!(f.via, Some(Step::Field(..) | Step::Payload(..)));
        let at = (1..stack.len()).rev().find(|&i| member(&stack[i])).or((stack.len() > 1).then(|| stack.len() - 1))?;
        Some((stack[at - 1].ty, stack[at].via?, leaf))
    }

    /// The components of `t` that derived trait `k` needs, or a single
    /// [`Step::Mutable`] if `t` is a struct that cannot implement `Hash`.
    fn steps(&mut self, t: TypeId, k: usize) -> Vec<Step> {
        let ty = self.rty(t);
        let structural = self.derived_parts(ty, k).is_some();
        match ty {
            Ty::List(x) | Ty::Set(x) if structural => vec![Step::Elements(x)],
            Ty::Map(x, y) if structural => vec![Step::Keys(x), Step::Values(y)],
            Ty::Tuple(l) => self.arena.list(l).iter().enumerate().map(|(i, &x)| Step::Element(i, x)).collect(),
            Ty::Named(d, args) => {
                let (generics, args) = (self.types[d as usize].generics.clone(), self.arena.list(args).to_vec());
                let members: Vec<(Step, TypeId)> = match &self.types[d as usize].kind {
                    TypeDefKind::Struct { fields } => {
                        if DERIVED[k] == Builtin::Hash
                            && let Some(f) = fields.iter().find(|f| f.mutable)
                        {
                            return vec![Step::Mutable(f.name)];
                        }
                        fields.iter().map(|f| (Step::Field(f.name, f.ty), f.ty)).collect()
                    }
                    TypeDefKind::Enum { variants } => {
                        variants.iter().flat_map(|v| v.fields.iter().map(|&ft| (Step::Payload(v.name, ft), ft))).collect()
                    }
                    TypeDefKind::Opaque(_) => Vec::new(),
                };
                members.into_iter().map(|(step, ft)| step.with_child(self.arena.instantiate(ft, &generics, &args))).collect()
            }
            _ => Vec::new(),
        }
    }
}

/// A step from a type to one of its components.
#[derive(Clone, Copy)]
enum Step {
    Elements(TypeId),
    Keys(TypeId),
    Values(TypeId),
    Element(usize, TypeId),
    Field(Symbol, TypeId),
    Payload(Symbol, TypeId),
    /// The struct has a mutable field, so it cannot implement `Hash`.
    Mutable(Symbol),
}

impl Step {
    fn child(self) -> TypeId {
        match self {
            Step::Elements(x) | Step::Keys(x) | Step::Values(x) | Step::Element(_, x) | Step::Field(_, x) | Step::Payload(_, x) => x,
            Step::Mutable(_) => TypeId::ERROR,
        }
    }

    fn with_child(self, x: TypeId) -> Step {
        match self {
            Step::Elements(_) => Step::Elements(x),
            Step::Keys(_) => Step::Keys(x),
            Step::Values(_) => Step::Values(x),
            Step::Element(i, _) => Step::Element(i, x),
            Step::Field(f, _) => Step::Field(f, x),
            Step::Payload(v, _) => Step::Payload(v, x),
            Step::Mutable(f) => Step::Mutable(f),
        }
    }
}
