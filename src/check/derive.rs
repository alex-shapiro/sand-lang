//! Derived impls of the built-in traits `Eq`, `Ord`, `Hash` and `Encode`.
//!
//! The checker derives these impls instead of reading them from source. An
//! instance of a type constructor implements the trait if some of its type
//! arguments do, as if `struct Pair<A, B>` declared
//! `impl<A: Eq, B: Eq> Eq for Pair<A, B>`. [`Checker::derived_needs`] gives
//! these type arguments for each constructor: all of them for tuples and
//! collections, and for structs and enums those that their fields and
//! payloads need.

use super::traits::Unmet;
use super::{Checker, Ctor, ctor_of};
use crate::types::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Derived {
    Never,
    /// Indices of the type parameters whose arguments must also implement the trait.
    Needs(Vec<u32>),
}

impl Checker {
    /// The indices of the type arguments of constructor `c` (with `n` type
    /// arguments) that must implement derived trait `b` for an instance of
    /// `c` to implement it, or `None` if no instance implements it.
    pub(crate) fn derived_needs(&self, c: Ctor, n: usize, b: Builtin) -> Option<Vec<u32>> {
        use Builtin::*;
        let ok = match c {
            Ctor::Def(d) => {
                return match self.derived.get(d as usize).map(|x| &x[b as usize]) {
                    Some(Derived::Needs(ps)) => Some(ps.clone()),
                    _ => None,
                };
            }
            Ctor::Int | Ctor::Bool | Ctor::Str | Ctor::Bytes | Ctor::Duration | Ctor::Unit | Ctor::Tuple => true,
            Ctor::Float | Ctor::List => b != Hash,
            Ctor::Range => b != Ord,
            Ctor::Set | Ctor::Map => matches!(b, Eq | Encode),
            Ctor::Fn => false,
        };
        ok.then(|| (0..n as u32).collect())
    }

    /// The components of `ty` that must implement derived trait `b` for `ty`
    /// to implement it, or `None` if `ty` cannot implement it.
    pub(crate) fn derived_parts(&self, ty: Ty, b: Builtin) -> Option<Vec<TypeId>> {
        match ty {
            Ty::Never | Ty::Error => Some(Vec::new()),
            _ => Some(self.arg_steps(ty, b)?.into_iter().map(Step::child).collect()),
        }
    }

    /// The type arguments of `ty` that derived trait `b` needs, or `None` if
    /// `ty` cannot implement `b`.
    fn arg_steps(&self, ty: Ty, b: Builtin) -> Option<Vec<Step>> {
        let c = ctor_of(ty)?;
        let args = self.type_args(ty);
        let needs = self.derived_needs(c, args.len(), b)?;
        Some(needs.into_iter().map(|i| Step::Arg(c, i, args[i as usize])).collect())
    }

    fn type_args(&self, ty: Ty) -> Vec<TypeId> {
        match ty {
            Ty::List(x) | Ty::Set(x) => vec![x],
            Ty::Map(k, v) => vec![k, v],
            Ty::Tuple(l) | Ty::Named(_, l) => self.arena.list(l).to_vec(),
            _ => Vec::new(),
        }
    }

    /// Proves `ty: tr` (zonked) for derived trait `b` through the components
    /// of `ty`. The components are visited with a work list rather than by
    /// recursion, because types nest deeply (`int???...`).
    pub(crate) fn resolve_derived(&mut self, ty: TypeId, tr: TraitRef, b: Builtin, depth: u32) -> Result<(), Box<Unmet>> {
        let underived = |c: &Checker| {
            let mut m = Unmet::new(format!("{} does not implement `{}`", c.show(ty), c.show_trait(&tr)));
            m.explain = Some((ty, tr, b));
            m
        };
        let mut unknown = None;
        let mut todo = vec![ty];
        while let Some(t) = todo.pop() {
            if !self.budget.charge(1) {
                return Ok(());
            }
            let t = self.resolve(t);
            let node = self.ty(t);
            if matches!(node, Ty::Param(_) | Ty::Var(_)) {
                match self.resolve_evidence(t, tr, depth + 1) {
                    Ok(_) => {}
                    Err(m) if m.unknown => unknown = Some(m),
                    Err(_) => return Err(underived(self)),
                }
                continue;
            }
            match self.derived_parts(node, b) {
                Some(parts) => todo.extend(parts),
                None => return Err(underived(self)),
            }
        }
        unknown.map_or(Ok(()), Err)
    }

    /// The fields of struct `d` or the payloads of enum `d`, in terms of its
    /// type parameters.
    fn members(&self, d: usize) -> Vec<Step> {
        match &self.types[d].kind {
            TypeDefKind::Struct { fields } => fields.iter().map(|f| Step::Field(f.name, f.ty)).collect(),
            TypeDefKind::Enum { variants } => variants.iter().flat_map(|v| v.fields.iter().map(|&t| Step::Payload(v.name, t))).collect(),
            TypeDefKind::Opaque(_) => Vec::new(),
        }
    }

    /// A `var` field of `d`, which keeps `d` from implementing `Hash`.
    fn mutable_field(&self, d: usize) -> Option<Symbol> {
        self.types[d].fields().iter().find(|f| f.mutable).map(|f| f.name)
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
            for m in self.members(d) {
                self.arena.any(m.child(), &mut |x| {
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
        for b in Builtin::ALL.iter().copied().filter(|b| b.is_derived()) {
            let mut todo: Vec<usize> = (first..n).collect();
            let mut queued = vec![true; n - first];
            while let Some(d) = todo.pop() {
                queued[d - first] = false;
                let new = self.derive(d, b);
                if new != self.derived[d][b as usize] {
                    self.derived[d][b as usize] = new;
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

    /// The derived impl of trait `b` for type definition `d`, given the
    /// current state of `self.derived`.
    fn derive(&self, d: usize, b: Builtin) -> Derived {
        let def = &self.types[d];
        let mut needs = Vec::new();
        let ok = match &def.kind {
            TypeDefKind::Opaque(kind) => match (b, kind) {
                (Builtin::Eq, _) | (Builtin::Hash | Builtin::Encode, Opaque::Value(_)) => true,
                (Builtin::Ord, Opaque::Value(v)) => v.ord,
                _ => false,
            },
            _ => {
                !(b == Builtin::Hash && self.mutable_field(d).is_some())
                    && self.members(d).iter().all(|m| self.needs(m.child(), b, &def.generics, &mut needs))
            }
        };
        if !ok {
            return Derived::Never;
        }
        needs.sort_unstable();
        needs.dedup();
        Derived::Needs(needs)
    }

    /// Returns `true` if member type `t` can implement derived trait `b`, and
    /// appends the indices in `generics` of the type parameters it needs to `needs`.
    fn needs(&self, t: TypeId, b: Builtin, generics: &[ParamId], needs: &mut Vec<u32>) -> bool {
        self.budget.charge(1);
        match self.ty(t) {
            Ty::Param(p) => match generics.iter().position(|&g| g == p) {
                Some(i) => {
                    needs.push(i as u32);
                    true
                }
                None => self.builtins[b as usize].is_some_and(|tid| self.param_has_trait(p, tid)),
            },
            Ty::Var(_) => true,
            ty => self.derived_parts(ty, b).is_some_and(|parts| parts.into_iter().all(|x| self.needs(x, b, generics, needs))),
        }
    }

    /// Fills in why `ty` does not implement derived trait `b`, if `m` asks for it.
    pub(crate) fn explain(&mut self, m: &mut Unmet) {
        let Some((ty, tr, b)) = m.explain.take() else { return };
        let Some((parent, step, leaf)) = self.failing_part(ty, tr, b) else { return };
        if let Ty::Param(p) = self.rty(leaf) {
            let n = self.name(self.params[p as usize].name);
            m.hint = Some(format!("add `{n}: {}`", self.show_trait(&tr)));
        }
        let owner = self.show(parent);
        let child = self.show(step.child());
        m.because = Some(match step {
            Step::Arg(Ctor::Map, 0, _) => format!("the keys of `{owner}` have type `{child}`"),
            Step::Arg(Ctor::Map, _, _) => format!("the values of `{owner}` have type `{child}`"),
            Step::Arg(Ctor::Tuple, i, _) => format!("element {i} of `{owner}` has type `{child}`"),
            Step::Arg(..) => format!("the elements of `{owner}` have type `{child}`"),
            Step::Field(f, _) => format!("field `{}` of `{owner}` has type `{child}`", self.name(f)),
            Step::Payload(v, _) => format!("the payload of variant `{}` of `{owner}` has type `{child}`", self.name(v)),
            Step::Mutable(f) => format!("field `{}` of `{owner}` is mutable", self.name(f)),
        });
    }

    /// Finds a component of `ty` that does not implement derived trait `b`,
    /// along a path of failing components that ends at a type that fails by
    /// itself. Returns the innermost field or payload step of the path (or its
    /// innermost step if it has none), the type that the step belongs to, and
    /// the type at the end of the path. Returns `None` if `ty` fails by itself
    /// or every path is a cycle.
    fn failing_part(&mut self, ty: TypeId, tr: TraitRef, b: Builtin) -> Option<(TypeId, Step, TypeId)> {
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
        let mut stack = vec![Frame { ty, via: None, parts: self.steps(ty, b), next: 0, failed: false }];
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
                let parts = self.steps(x, b);
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

    /// The components of `t` that derived trait `b` needs, or a single
    /// [`Step::Mutable`] if `t` is a struct that cannot implement `Hash`.
    fn steps(&mut self, t: TypeId, b: Builtin) -> Vec<Step> {
        let ty = self.rty(t);
        match ty {
            Ty::Named(d, args) if !matches!(self.types[d as usize].kind, TypeDefKind::Opaque(_)) => {
                let d = d as usize;
                if b == Builtin::Hash
                    && let Some(f) = self.mutable_field(d)
                {
                    return vec![Step::Mutable(f)];
                }
                let (generics, args) = (self.types[d].generics.clone(), self.arena.list(args).to_vec());
                let members = self.members(d);
                members.into_iter().map(|m| m.with_child(self.arena.instantiate(m.child(), &generics, &args))).collect()
            }
            _ => self.arg_steps(ty, b).unwrap_or_default(),
        }
    }
}

/// A step from a type to one of its components.
#[derive(Clone, Copy)]
enum Step {
    /// The type argument at an index of a constructor.
    Arg(Ctor, u32, TypeId),
    Field(Symbol, TypeId),
    Payload(Symbol, TypeId),
    /// The struct has a mutable field, so it cannot implement `Hash`.
    Mutable(Symbol),
}

impl Step {
    fn child(self) -> TypeId {
        match self {
            Step::Arg(_, _, x) | Step::Field(_, x) | Step::Payload(_, x) => x,
            Step::Mutable(_) => TypeId::ERROR,
        }
    }

    fn with_child(self, x: TypeId) -> Step {
        match self {
            Step::Arg(c, i, _) => Step::Arg(c, i, x),
            Step::Field(f, _) => Step::Field(f, x),
            Step::Payload(v, _) => Step::Payload(v, x),
            Step::Mutable(f) => Step::Mutable(f),
        }
    }
}
