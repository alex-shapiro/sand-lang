//! Automatic derivation of the built-in traits `Eq`, `Ord`, `Hash`, `Encode`,
//! and `Decode`.
//!
//! A constructed type implements a derived trait if its required type arguments
//! implement that trait. [`Checker::derived_required_params`] returns the
//! required type parameter indices for each type constructor: all type
//! parameters for tuples and collections, and the type parameters used by
//! fields or variant payloads for structs and enums.

use super::traits::Unmet;
use super::{Checker, Ctor, ctor_of};
use crate::types::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Derived {
    Never,
    /// Indices of the type parameters whose arguments must implement the trait.
    Needs(Vec<u32>),
}

impl Checker {
    /// Returns the type argument indices of constructor `c` (with `n` type
    /// arguments) that must implement derived trait `b`, or `None` if `c`
    /// never implements `b`.
    pub(crate) fn derived_required_params(&self, c: Ctor, n: usize, b: Builtin) -> Option<Vec<u32>> {
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
            Ctor::Set | Ctor::Map => matches!(b, Eq | Encode | Decode),
            Ctor::Fn => false,
        };
        ok.then(|| (0..n as u32).collect())
    }

    /// Returns the component types of `ty` that must implement derived trait
    /// `b`, or `None` if `ty` cannot implement `b`.
    pub(crate) fn derived_parts(&self, ty: Ty, b: Builtin) -> Option<Vec<TypeId>> {
        match ty {
            Ty::Never | Ty::Error => Some(Vec::new()),
            _ => Some(self.type_arg_steps(ty, b)?.into_iter().map(Step::child).collect()),
        }
    }

    fn type_arg_steps(&self, ty: Ty, b: Builtin) -> Option<Vec<Step>> {
        let c = ctor_of(ty)?;
        let args = self.type_args(ty);
        let required = self.derived_required_params(c, args.len(), b)?;
        Some(required.into_iter().map(|i| Step::Arg(c, i, args[i as usize])).collect())
    }

    fn type_args(&self, ty: Ty) -> Vec<TypeId> {
        match ty {
            Ty::List(x) | Ty::Set(x) => vec![x],
            Ty::Map(k, v) => vec![k, v],
            Ty::Tuple(l) | Ty::Named(_, l) => self.arena.list(l).to_vec(),
            _ => Vec::new(),
        }
    }

    /// Verifies that `ty` implements derived trait `b` (`tr`) by checking its
    /// component types iteratively with an explicit worklist to avoid stack
    /// overflow on deeply nested types.
    pub(crate) fn resolve_derived(&mut self, ty: TypeId, tr: TraitRef, b: Builtin, depth: u32) -> Result<(), Box<Unmet>> {
        let unmet_err = |c: &Checker| {
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
                    Err(_) => return Err(unmet_err(self)),
                }
                continue;
            }
            match self.derived_parts(node, b) {
                Some(parts) => todo.extend(parts),
                None => return Err(unmet_err(self)),
            }
        }
        unknown.map_or(Ok(()), Err)
    }

    fn members(&self, d: usize) -> Vec<Step> {
        match &self.types[d].kind {
            TypeDefKind::Struct { fields } => fields.iter().map(|f| Step::Field(f.name, f.ty)).collect(),
            TypeDefKind::Enum { variants } => variants.iter().flat_map(|v| v.fields.iter().map(|&t| Step::Payload(v.name, t))).collect(),
            TypeDefKind::Opaque(_) => Vec::new(),
        }
    }

    /// Returns the uninstantiated field or variant payload types of type
    /// definition `d`.
    pub(crate) fn member_types(&self, d: usize) -> Vec<TypeId> {
        self.members(d).into_iter().map(Step::child).collect()
    }

    fn declares_init(&self, d: usize) -> bool {
        self.init_method(Ctor::Def(d as TypeDefId)).is_some()
    }

    fn mutable_field(&self, d: usize) -> Option<Symbol> {
        self.types[d].fields().iter().find(|f| f.mutable).map(|f| f.name)
    }

    /// Computes the derived implementations of `traits` for the type
    /// definitions from `first` onward, and replaces earlier results for
    /// `traits`. The computation finds the greatest fixed point, so mutually
    /// recursive types implement a trait unless a component type does not.
    pub(crate) fn derive_impls(&mut self, first: usize, traits: &[Builtin]) {
        while self.derived.len() < self.types.len() {
            self.derived.push(std::array::from_fn(|_| Derived::Needs(Vec::new())));
        }
        let dependents = self.dependent_types(first);
        for &b in traits {
            for d in first..self.types.len() {
                self.derived[d][b as usize] = Derived::Needs(Vec::new());
            }
            self.propagate_to_fixed_point(first, &dependents, &mut |c, d| {
                let new = c.derive(d, b);
                let changed = new != c.derived[d][b as usize];
                c.derived[d][b as usize] = new;
                changed
            });
        }
    }

    /// Returns, for each type definition at or after `first` (indexed relative
    /// to `first`), the type definitions at or after `first` whose fields or
    /// variant payloads reference it.
    pub(crate) fn dependent_types(&self, first: usize) -> Vec<Vec<usize>> {
        let n = self.types.len();
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n - first];
        for d in first..n {
            for m in self.members(d) {
                self.arena.any(m.child(), &mut |x| {
                    if let Ty::Named(e, _) = x
                        && let Some(u) = (e as usize).checked_sub(first).and_then(|i| dependents.get_mut(i))
                        && u.last() != Some(&d)
                    {
                        u.push(d);
                    }
                    false
                });
            }
        }
        dependents
    }

    /// Runs `update` on each type definition starting at `first` and re-queues
    /// dependent definitions whenever `update` returns `true` until reaching a
    /// fixed point.
    pub(crate) fn propagate_to_fixed_point(
        &mut self,
        first: usize,
        dependents: &[Vec<usize>],
        update: &mut dyn FnMut(&mut Checker, usize) -> bool,
    ) {
        let mut todo: Vec<usize> = (first..first + dependents.len()).collect();
        let mut queued = vec![true; dependents.len()];
        while let Some(d) = todo.pop() {
            queued[d - first] = false;
            if update(self, d) {
                for &u in &dependents[d - first] {
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

    fn derive(&self, d: usize, b: Builtin) -> Derived {
        let def = &self.types[d];
        let mut required = Vec::new();
        let ok = match &def.kind {
            TypeDefKind::Opaque(kind) => match (b, kind) {
                (Builtin::Eq, _) | (Builtin::Hash | Builtin::Encode | Builtin::Decode, Opaque::Value(_)) => true,
                (Builtin::Ord, Opaque::Value(v)) => v.ord,
                _ => false,
            },
            _ => {
                !(b == Builtin::Hash && self.mutable_field(d).is_some())
                    && !(b == Builtin::Decode && self.declares_init(d))
                    && self.members(d).iter().all(|m| self.collect_required_params(m.child(), b, &def.generics, &mut required))
            }
        };
        if !ok {
            return Derived::Never;
        }
        required.sort_unstable();
        required.dedup();
        Derived::Needs(required)
    }

    fn collect_required_params(&self, t: TypeId, b: Builtin, generics: &[ParamId], required: &mut Vec<u32>) -> bool {
        self.budget.charge(1);
        match self.ty(t) {
            Ty::Param(p) => match generics.iter().position(|&g| g == p) {
                Some(i) => {
                    required.push(i as u32);
                    true
                }
                None => self.builtins[b as usize].is_some_and(|tid| self.param_has_trait(p, tid)),
            },
            Ty::Var(_) => true,
            ty => self
                .derived_parts(ty, b)
                .is_some_and(|parts| parts.into_iter().all(|x| self.collect_required_params(x, b, generics, required))),
        }
    }

    /// Populates the diagnostic reason and hint on `m` if `m` records a failed
    /// derived trait check.
    pub(crate) fn explain(&mut self, m: &mut Unmet) {
        let Some((ty, tr, b)) = m.explain.take() else { return };
        let Some((parent, step, leaf)) = self.find_failing_step(ty, tr, b) else { return };
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
            Step::Init => format!("`{owner}` declares `init`, which decoding does not call"),
        });
    }

    /// Finds a non-cyclic component path in `ty` that fails to implement
    /// derived trait `b`. Returns the parent type, the failing step (preferring
    /// the innermost struct field or enum payload step), and the leaf failing
    /// type.
    fn find_failing_step(&mut self, ty: TypeId, tr: TraitRef, b: Builtin) -> Option<(TypeId, Step, TypeId)> {
        struct Frame {
            ty: TypeId,
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
            if let Some(&(Step::Mutable(_) | Step::Init)) = top.parts.first() {
                break;
            }
            if top.next == top.parts.len() {
                if top.failed {
                    stack.pop();
                    continue;
                }
                break;
            }
            let (i, step) = (top.next, top.parts[top.next]);
            top.next += 1;
            // If all earlier components succeeded, the remaining component must be the failing one.
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
        if let Some(&step @ (Step::Mutable(_) | Step::Init)) = stack.last()?.parts.first() {
            return Some((leaf, step, leaf));
        }
        let member = |f: &Frame| matches!(f.via, Some(Step::Field(..) | Step::Payload(..)));
        let at = (1..stack.len()).rev().find(|&i| member(&stack[i])).or((stack.len() > 1).then(|| stack.len() - 1))?;
        Some((stack[at - 1].ty, stack[at].via?, leaf))
    }

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
                if b == Builtin::Decode && self.declares_init(d) {
                    return vec![Step::Init];
                }
                let (generics, args) = (self.types[d].generics.clone(), self.arena.list(args).to_vec());
                let members = self.members(d);
                members.into_iter().map(|m| m.with_child(self.arena.instantiate(m.child(), &generics, &args))).collect()
            }
            _ => self.type_arg_steps(ty, b).unwrap_or_default(),
        }
    }
}

#[derive(Clone, Copy)]
enum Step {
    Arg(Ctor, u32, TypeId),
    Field(Symbol, TypeId),
    Payload(Symbol, TypeId),
    Mutable(Symbol),
    /// The struct declares `init`, so it does not implement `Decode`.
    Init,
}

impl Step {
    fn child(self) -> TypeId {
        match self {
            Step::Arg(_, _, x) | Step::Field(_, x) | Step::Payload(_, x) => x,
            Step::Mutable(_) | Step::Init => TypeId::ERROR,
        }
    }

    fn with_child(self, x: TypeId) -> Step {
        match self {
            Step::Arg(c, i, _) => Step::Arg(c, i, x),
            Step::Field(f, _) => Step::Field(f, x),
            Step::Payload(v, _) => Step::Payload(v, x),
            Step::Mutable(_) | Step::Init => self,
        }
    }
}
