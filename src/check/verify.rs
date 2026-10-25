//! Independent verification of trait bounds and dictionaries in the typed HIR.
//!
//! Debug builds run [`verify`] before code generation to check four invariants
//! directly from item declarations:
//!
//! - Every type argument at a call site, function value, struct literal, or
//!   enum variant satisfies the trait bounds of its type parameter.
//! - Every dictionary expression passed to a call matches the trait and target
//!   type expected by the callee's hidden dictionary parameter.
//! - Every struct field type, enum variant payload type, and `reflect` type
//!   argument satisfies the trait bounds of its type constructors.
//! - Every `impl` target type satisfies all supertraits of the implemented
//!   trait.
//!
//! This pass is independent of the type checker's solver (`traits.rs` and
//! `derive.rs`) so that solver bugs fail verification at compile time.

use super::*;

const DERIVED: [Builtin; 4] = [Builtin::Eq, Builtin::Ord, Builtin::Hash, Builtin::Encode];

/// Verifies all type definitions, `impl` blocks, and function `bodies` in `cp`.
/// Returns an error describing the first violation found.
pub(crate) fn verify<'a>(cp: &'a CheckedProgram, bodies: impl Iterator<Item = (FnId, &'a HFn)>) -> Result<(), String> {
    let mut v = Verifier::new(cp);
    for d in 0..cp.types.len() {
        v.check_type_def(d as TypeDefId)?;
    }
    for i in 0..cp.impls.len() {
        v.check_impl_supertraits(i as ImplId)?;
    }
    for (f, body) in bodies {
        v.check_body(f, body).map_err(|m| format!("in `{}`: {m}", cp.name(cp.fns[f as usize].name)))?;
    }
    Ok(())
}

struct Verifier<'a> {
    cp: &'a CheckedProgram,
    arena: TypeArena,
    natives: HashMap<u32, FnId>,
    builtins: HashMap<Builtin, TraitRef>,
    /// Required type parameter indices for each type definition and derived
    /// trait in [`DERIVED`], or `None` if the type never implements the trait.
    derived: Vec<[Option<Vec<u32>>; 4]>,
}

#[derive(Default)]
struct VerifyCtx {
    f: Option<FnId>,
    /// Maps each dictionary parameter local to the `(type, trait)` pair that it
    /// provides.
    dicts: HashMap<LocalId, (TypeId, TraitRef)>,
    /// Records the `Self` dictionary local and `ImplId` for an `impl` method.
    impl_dict: Option<(LocalId, ImplId)>,
    /// Maps hoisted dictionary locals to their initializer expressions.
    hoisted: HashMap<LocalId, HExpr>,
}

type VerifyResult = Result<(), String>;

impl<'a> Verifier<'a> {
    fn new(cp: &'a CheckedProgram) -> Self {
        let mut natives = HashMap::new();
        for (f, fi) in cp.fns.iter().enumerate() {
            if let FnKind::Native(n) = fi.kind {
                natives.entry(n).or_insert(f as FnId);
            }
        }
        let builtins = (0..cp.traits.len())
            .filter_map(|t| Some((cp.traits[t].builtin?, TraitRef { trait_id: t as TraitId, args: TyList::EMPTY })))
            .collect();
        let mut v = Verifier { cp, arena: cp.arena.clone(), natives, builtins, derived: Vec::new() };
        v.derive_all();
        v
    }

    fn show(&self, t: TypeId) -> String {
        TypePrinter::new(&self.arena, &self.cp.types, &self.cp.params, &self.cp.syms, self.cp.wk).show(t)
    }

    fn show_trait(&self, tr: TraitRef) -> String {
        let name = self.cp.name(self.cp.traits[tr.trait_id as usize].name).to_string();
        let args: Vec<String> = self.arena.list(tr.args).iter().map(|&t| self.show(t)).collect();
        if args.is_empty() { name } else { format!("{name}<{}>", args.join(", ")) }
    }

    fn has_dict(&self, tr: TraitRef) -> bool {
        self.cp.traits[tr.trait_id as usize].has_dict
    }

    fn subst(&mut self, t: TypeId, from: &[ParamId], to: &[TypeId]) -> TypeId {
        self.arena.instantiate(t, from, to)
    }

    fn subst_trait(&mut self, tr: TraitRef, from: &[ParamId], to: &[TypeId]) -> TraitRef {
        TraitRef { trait_id: tr.trait_id, args: self.arena.map_list(tr.args, &mut |a, t| a.instantiate(t, from, to)) }
    }

    fn supers_of(&mut self, ty: TypeId, tr: TraitRef) -> Vec<TraitRef> {
        let td = &self.cp.traits[tr.trait_id as usize];
        let mut from = td.generics.clone();
        from.push(td.self_param);
        let mut to = self.arena.list(tr.args).to_vec();
        to.push(ty);
        td.supers.clone().into_iter().map(|s| self.subst_trait(s, &from, &to)).collect()
    }

    /// Returns all traits satisfied by type parameter `p` (of type `ty`),
    /// including declared bounds, function dictionary bounds, and their
    /// transitive supertraits.
    fn param_traits(&mut self, cx: &VerifyCtx, p: ParamId, ty: TypeId) -> Vec<TraitRef> {
        let mut out = self.cp.params[p as usize].traits.clone();
        if let Some(f) = cx.f {
            out.extend(self.cp.fns[f as usize].dicts.iter().filter(|(t, _)| *t == ty).map(|&(_, tr)| tr));
        }
        let mut i = 0;
        while i < out.len() {
            for s in self.supers_of(ty, out[i]) {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
            i += 1;
        }
        out
    }

    /// Computes [`Verifier::derived`] using greatest fixed-point iteration so
    /// that mutually recursive types implement a derived trait unless a
    /// component type refutes it.
    fn derive_all(&mut self) {
        let n = self.cp.types.len();
        self.derived = vec![std::array::from_fn(|_| Some(Vec::new())); n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for d in 0..n {
            for m in self.members(d) {
                self.arena.any(m, &mut |t| {
                    if let Ty::Named(e, _) = t
                        && dependents[e as usize].last() != Some(&d)
                    {
                        dependents[e as usize].push(d);
                    }
                    false
                });
            }
        }
        for b in 0..DERIVED.len() {
            let mut todo: Vec<usize> = (0..n).collect();
            while let Some(d) = todo.pop() {
                let new = self.derive_def(d, b);
                if new != self.derived[d][b] {
                    self.derived[d][b] = new;
                    todo.extend(&dependents[d]);
                }
            }
        }
    }

    fn members(&self, d: usize) -> Vec<TypeId> {
        let def = &self.cp.types[d];
        def.fields().iter().map(|f| f.ty).chain(def.variants().iter().flat_map(|v| v.fields.iter().copied())).collect()
    }

    fn derive_def(&mut self, d: usize, b: usize) -> Option<Vec<u32>> {
        let def = &self.cp.types[d];
        let trait_ = DERIVED[b];
        if let TypeDefKind::Opaque(o) = def.kind {
            let ok = match (trait_, o) {
                (Builtin::Eq, _) | (Builtin::Hash | Builtin::Encode, Opaque::Value(_)) => true,
                (Builtin::Ord, Opaque::Value(v)) => v.ord,
                _ => false,
            };
            return ok.then(Vec::new);
        }
        if trait_ == Builtin::Hash && def.fields().iter().any(|f| f.mutable) {
            return None;
        }
        let generics = def.generics.clone();
        let mut required = Vec::new();
        let mut todo = self.members(d);
        while let Some(t) = todo.pop() {
            match self.arena.get(t) {
                Ty::Param(p) => match generics.iter().position(|&g| g == p) {
                    Some(i) => required.push(i as u32),
                    None => {
                        let pt = self.arena.param(p);
                        let need = self.builtins[&trait_];
                        if !self.param_traits(&VerifyCtx::default(), p, pt).contains(&need) {
                            return None;
                        }
                    }
                },
                Ty::Named(e, args) => {
                    let args = self.arena.list(args).to_vec();
                    todo.extend(self.derived[e as usize][b].as_ref()?.iter().map(|&i| args[i as usize]));
                }
                ty => todo.extend(builtin_parts(&self.arena, ty, trait_)?),
            }
        }
        required.sort_unstable();
        required.dedup();
        Some(required)
    }

    /// Verifies that `ty` implements `tr`.
    fn check_trait(&mut self, cx: &VerifyCtx, ty: TypeId, tr: TraitRef, depth: u32) -> VerifyResult {
        let fail = |v: &Self| format!("{} does not implement `{}`", v.show(ty), v.show_trait(tr));
        if self.arena.has_vars(ty) {
            return Ok(());
        }
        if depth > 64 {
            return Err(format!("too deep a proof that {} implements `{}`", self.show(ty), self.show_trait(tr)));
        }
        match self.arena.get(ty) {
            Ty::Error => Err(fail(self)),
            Ty::Never if self.has_dict(tr) => Err(fail(self)),
            Ty::Never => Ok(()),
            Ty::Param(p) if self.param_traits(cx, p, ty).contains(&tr) => Ok(()),
            Ty::Param(_) => Err(fail(self)),
            _ => match self.cp.traits[tr.trait_id as usize].builtin {
                Some(b) if b.is_derived() => self.check_derived_trait(cx, ty, b),
                _ => {
                    let (i, sigma) = self.find_matching_impl(ty, tr).map_err(|why| format!("{}: {why}", fail(self)))?;
                    let imp = &self.cp.impls[i as usize];
                    let gens = imp.generics.clone();
                    for (t, s) in imp.evidence.clone() {
                        let (t, s) = (self.subst(t, &gens, &sigma), self.subst_trait(s, &gens, &sigma));
                        self.check_trait(cx, t, s, depth + 1)?;
                    }
                    Ok(())
                }
            },
        }
    }

    fn check_derived_trait(&mut self, cx: &VerifyCtx, ty: TypeId, b: Builtin) -> VerifyResult {
        let bi = DERIVED.iter().position(|&x| x == b).expect("a derived trait");
        let need = self.builtins[&b];
        let fail = |v: &Self, t: TypeId| {
            let because = if t == ty { String::new() } else { format!(" because {} does not", v.show(t)) };
            Err(format!("{} does not implement `{}`{because}", v.show(ty), b.name()))
        };
        let mut seen = HashSet::new();
        let mut todo = vec![ty];
        while let Some(t) = todo.pop() {
            if !seen.insert(t) || self.arena.has_vars(t) {
                continue;
            }
            match self.arena.get(t) {
                Ty::Error => return fail(self, t),
                Ty::Param(p) => {
                    if !self.param_traits(cx, p, t).contains(&need) {
                        return fail(self, t);
                    }
                }
                Ty::Named(d, args) => {
                    let args = self.arena.list(args).to_vec();
                    match &self.derived[d as usize][bi] {
                        Some(required) => todo.extend(required.iter().map(|&i| args[i as usize])),
                        None => return fail(self, t),
                    }
                }
                node => match builtin_parts(&self.arena, node, b) {
                    Some(parts) => todo.extend(parts),
                    None => return fail(self, t),
                },
            }
        }
        Ok(())
    }

    /// Finds the unique `impl` of `tr` matching `ty` and returns its `ImplId`
    /// and type argument substitutions.
    fn find_matching_impl(&self, ty: TypeId, tr: TraitRef) -> Result<(ImplId, Vec<TypeId>), &'static str> {
        let mut found = None;
        for i in (0..self.cp.impls.len() as ImplId).filter(|&i| !self.cp.impls[i as usize].replaced) {
            if let Some(sigma) = self.match_impl(i, ty, tr) {
                if found.is_some() {
                    return Err("several impls match");
                }
                found = Some((i, sigma));
            }
        }
        found.ok_or("no impl matches")
    }

    fn match_impl(&self, i: ImplId, ty: TypeId, tr: TraitRef) -> Option<Vec<TypeId>> {
        let imp = &self.cp.impls[i as usize];
        if imp.trait_ref.trait_id != tr.trait_id {
            return None;
        }
        let mut sigma = vec![None; imp.generics.len()];
        let pairs = std::iter::once((imp.target, ty))
            .chain(self.arena.list(imp.trait_ref.args).iter().copied().zip(self.arena.list(tr.args).iter().copied()));
        for (pat, t) in pairs.collect::<Vec<_>>() {
            if !self.matches(pat, t, &imp.generics, &mut sigma) {
                return None;
            }
        }
        sigma.into_iter().collect()
    }

    /// Returns `true` if `t` matches pattern `pat`, recording substitutions for
    /// `gens` in `sigma`.
    fn matches(&self, pat: TypeId, t: TypeId, gens: &[ParamId], sigma: &mut [Option<TypeId>]) -> bool {
        if let Ty::Param(p) = self.arena.get(pat)
            && let Some(i) = gens.iter().position(|&g| g == p)
        {
            return *sigma[i].get_or_insert(t) == t;
        }
        if !self.arena.has_params(pat) {
            return pat == t;
        }
        let a = &self.arena;
        let all = |xs: &[TypeId], ys: &[TypeId], sigma: &mut [Option<TypeId>]| {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(&x, &y)| self.matches(x, y, gens, sigma))
        };
        match (a.get(pat), a.get(t)) {
            (Ty::List(x), Ty::List(y)) | (Ty::Set(x), Ty::Set(y)) => self.matches(x, y, gens, sigma),
            (Ty::Map(k, v), Ty::Map(k2, v2)) => self.matches(k, k2, gens, sigma) && self.matches(v, v2, gens, sigma),
            (Ty::Tuple(l), Ty::Tuple(m)) => all(a.list(l), a.list(m), sigma),
            (Ty::Named(d, l), Ty::Named(e, m)) => d == e && all(a.list(l), a.list(m), sigma),
            (Ty::Fn(ps, r), Ty::Fn(qs, s)) => all(a.list(ps), a.list(qs), sigma) && self.matches(r, s, gens, sigma),
            _ => false,
        }
    }

    /// Verifies that all type constructors in `ty` satisfy their trait bounds
    /// (`Set` elements and `Map` keys implement `Hash`, and named types satisfy
    /// their parameter bounds).
    fn check_well_formed(&mut self, cx: &VerifyCtx, ty: TypeId) -> VerifyResult {
        let hash = self.builtins[&Builtin::Hash];
        let mut seen = HashSet::new();
        let mut todo = vec![ty];
        while let Some(t) = todo.pop() {
            if !seen.insert(t) {
                continue;
            }
            match self.arena.get(t) {
                Ty::List(x) => todo.push(x),
                Ty::Set(x) => {
                    self.check_trait(cx, x, hash, 0).map_err(|m| format!("{m} (required for the elements of {})", self.show(t)))?;
                    todo.push(x);
                }
                Ty::Map(k, x) => {
                    self.check_trait(cx, k, hash, 0).map_err(|m| format!("{m} (required for the keys of {})", self.show(t)))?;
                    todo.extend([k, x]);
                }
                Ty::Tuple(l) => todo.extend(self.arena.list(l)),
                Ty::Fn(ps, r) => {
                    todo.extend(self.arena.list(ps));
                    todo.push(r);
                }
                Ty::Named(..) => {
                    self.check_type_args(cx, t)?;
                    let Ty::Named(_, args) = self.arena.get(t) else { unreachable!() };
                    todo.extend(self.arena.list(args));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn check_type_args(&mut self, cx: &VerifyCtx, ty: TypeId) -> VerifyResult {
        let Ty::Named(d, args) = self.arena.get(ty) else { return Ok(()) };
        let gens = self.cp.types[d as usize].generics.clone();
        let args = self.arena.list(args).to_vec();
        for (&g, &arg) in gens.iter().zip(&args) {
            for tr in self.cp.params[g as usize].traits.clone() {
                let tr = self.subst_trait(tr, &gens, &args);
                self.check_trait(cx, arg, tr, 0).map_err(|m| format!("{m} (required for {})", self.show(ty)))?;
            }
        }
        Ok(())
    }

    /// Verifies that expression `e` evaluates to a dictionary for `ty: tr`.
    fn check_dict(&mut self, cx: &VerifyCtx, e: &HExpr, ty: TypeId, tr: TraitRef) -> VerifyResult {
        if self.arena.has_vars(ty) || self.arena.list(tr.args).iter().any(|&a| self.arena.has_vars(a)) {
            return Ok(());
        }
        let expected_desc = |v: &Self| format!("the dictionary for {} implementing `{}`", v.show(ty), v.show_trait(tr));
        if !self.has_dict(tr) {
            return Err(format!("{} is passed, but the trait has no methods", expected_desc(self)));
        }
        match &e.kind {
            HKind::Local(l) if !cx.dicts.contains_key(l) => match cx.hoisted.get(l) {
                Some(init) => self.check_dict(cx, &init.clone(), ty, tr),
                None => Err(format!("{} is local {l}, which holds no dictionary", expected_desc(self))),
            },
            HKind::Local(_) | HKind::Field { .. } => {
                let (t, have) = self.resolve_dict_param(cx, e)?;
                if (t, have) == (ty, tr) {
                    Ok(())
                } else {
                    Err(format!("{} is the dictionary for {} implementing `{}`", expected_desc(self), self.show(t), self.show_trait(have)))
                }
            }
            HKind::Dict { impl_id, args } => {
                self.check_impl_dict(cx, *impl_id, args, ty, tr).map_err(|m| format!("{}: {m}", expected_desc(self)))
            }
            _ => Err(format!("{} is not a dictionary expression", expected_desc(self))),
        }
    }

    /// Resolves the `(type, trait)` pair provided by a dictionary parameter or
    /// supertrait/evidence field projection.
    fn resolve_dict_param(&mut self, cx: &VerifyCtx, e: &HExpr) -> Result<(TypeId, TraitRef), String> {
        match &e.kind {
            HKind::Local(l) => cx.dicts.get(l).copied().ok_or_else(|| format!("local {l} is not a dictionary parameter")),
            HKind::Field { obj, index } => {
                let (ty, tr) = self.resolve_dict_param(cx, obj)?;
                let methods = self.cp.traits[tr.trait_id as usize].methods.len();
                let field = (*index as usize).checked_sub(methods).ok_or("a method is used as a dictionary")?;
                let supers: Vec<TraitRef> = self.supers_of(ty, tr).into_iter().filter(|&s| self.has_dict(s)).collect();
                if let Some(&s) = supers.get(field) {
                    return Ok((ty, s));
                }
                // After supertrait dictionaries, an impl method's `Self` dictionary
                // stores dictionaries for the impl's own trait bounds.
                match (&obj.kind, cx.impl_dict) {
                    (HKind::Local(l), Some((d0, i))) if *l == d0 => {
                        let imp = &self.cp.impls[i as usize];
                        let ev: Vec<(TypeId, TraitRef)> = imp.evidence.iter().copied().filter(|&(_, s)| self.has_dict(s)).collect();
                        ev.get(field - supers.len()).copied().ok_or_else(|| format!("field {index} of an impl dictionary does not exist"))
                    }
                    _ => Err(format!("field {index} of a dictionary does not exist")),
                }
            }
            _ => Err("a dictionary is not a parameter or a projection of one".into()),
        }
    }

    fn check_impl_dict(&mut self, cx: &VerifyCtx, i: ImplId, args: &[HExpr], ty: TypeId, tr: TraitRef) -> VerifyResult {
        let imp = &self.cp.impls[i as usize];
        if imp.replaced {
            return Err("its impl is replaced by a later one".into());
        }
        let sigma = self.match_impl(i, ty, tr).ok_or("its impl does not match")?;
        let (gens, evidence) = (imp.generics.clone(), imp.evidence.clone());
        let mut needed: Vec<(TypeId, TraitRef)> = self.supers_of(ty, tr).into_iter().map(|s| (ty, s)).collect();
        for (t, s) in evidence {
            needed.push((self.subst(t, &gens, &sigma), self.subst_trait(s, &gens, &sigma)));
        }
        let mut fields = args.iter();
        for (t, s) in needed {
            if self.has_dict(s) {
                let e = fields.next().ok_or("it has too few fields")?;
                self.check_dict(cx, e, t, s)?;
            } else {
                self.check_trait(cx, t, s, 0)?;
            }
        }
        if fields.next().is_some() {
            return Err("it has too many fields".into());
        }
        Ok(())
    }

    fn check_type_def(&mut self, d: TypeDefId) -> VerifyResult {
        let cx = VerifyCtx::default();
        for t in self.members(d as usize) {
            self.check_well_formed(&cx, t).map_err(|m| format!("a member of `{}`: {m}", self.cp.name(self.cp.types[d as usize].name)))?;
        }
        Ok(())
    }

    fn check_impl_supertraits(&mut self, i: ImplId) -> VerifyResult {
        let imp = &self.cp.impls[i as usize];
        let (target, tr) = (imp.target, imp.trait_ref);
        for s in self.supers_of(target, tr) {
            self.check_trait(&VerifyCtx::default(), target, s, 0)
                .map_err(|m| format!("the impl of `{}` for {}: {m}", self.show_trait(tr), self.show(target)))?;
        }
        Ok(())
    }

    fn check_body(&mut self, f: FnId, b: &HFn) -> VerifyResult {
        let fi = &self.cp.fns[f as usize];
        let reflect = fi.generics.iter().filter(|&&g| self.cp.params[g as usize].reflect).count();
        let mut cx = VerifyCtx { f: Some(f), ..VerifyCtx::default() };
        for (k, &d) in fi.dicts.iter().enumerate() {
            let l = *b.params.get(reflect + k).ok_or("a dictionary parameter is missing")?;
            cx.dicts.insert(l, d);
        }
        if let (Some(i), Some(&d0)) = (fi.impl_of, b.params.get(reflect)) {
            cx.impl_dict = Some((d0, i));
        }
        let mut body = b.body.clone();
        let mut result = Ok(());
        body.visit_mut(&mut |e| {
            if result.is_ok() {
                result = self.check_expr(&mut cx, &b.locals, e);
            }
        });
        result
    }

    fn check_expr(&mut self, cx: &mut VerifyCtx, locals: &[LocalInfo], e: &HExpr) -> VerifyResult {
        match &e.kind {
            HKind::Error | HKind::Evidence(_) => Err("a placeholder remains".into()),
            HKind::Let { pat: HPat { kind: HPatKind::Bind(l), .. }, init, .. } if locals[*l as usize].name == Symbol::DICT => {
                cx.hoisted.insert(*l, (**init).clone());
                Ok(())
            }
            HKind::Call { func, targs, args } => self.check_fn_instance(cx, *func, targs, Some(args)),
            HKind::CallNative { native, targs, args } => match self.natives.get(native) {
                Some(&f) => self.check_fn_instance(cx, f, targs, Some(args)),
                None => Err(format!("native {native} has no declaration")),
            },
            HKind::CallTrait { trait_id, slot, targs, args, impl_id } => {
                let td = &self.cp.traits[*trait_id as usize];
                let sig = td.methods.get(*slot as usize).ok_or("a trait method slot does not exist")?.sig;
                self.check_fn_instance(cx, sig, targs, Some(args))?;
                if let Some(i) = impl_id {
                    let gens = &self.cp.fns[sig as usize].generics;
                    let self_ty = gens.iter().position(|&g| g == td.self_param).map(|k| targs[k]).ok_or("a trait method has no `Self`")?;
                    let tr = TraitRef { trait_id: *trait_id, args: self.arena.intern_list(&targs[..td.generics.len()]) };
                    if self.cp.impls[*i as usize].replaced || !self.arena.has_vars(self_ty) && self.match_impl(*i, self_ty, tr).is_none() {
                        return Err(format!("a direct call of `{}` for {} goes to another impl", self.show_trait(tr), self.show(self_ty)));
                    }
                }
                Ok(())
            }
            HKind::FnValue { func, targs } => self.check_fn_instance(cx, *func, targs, None),
            HKind::MakeStruct { .. } | HKind::MakeVariant { .. } | HKind::Tag(_) => self.check_type_args(cx, e.ty),
            HKind::VariantFn { .. } => match self.arena.get(e.ty) {
                Ty::Fn(_, ret) => self.check_type_args(cx, ret),
                _ => Ok(()),
            },
            HKind::TypeDesc { ty, .. } => self.check_well_formed(cx, *ty),
            _ => Ok(()),
        }
    }

    /// Verifies an instantiation of function `f` with `targs`, checking hidden
    /// `reflect` and dictionary arguments if `args` is `Some`.
    fn check_fn_instance(&mut self, cx: &VerifyCtx, f: FnId, targs: &[TypeId], args: Option<&[HArg]>) -> VerifyResult {
        let cp = self.cp;
        let fi = &cp.fns[f as usize];
        let name = cp.name(fi.name);
        if targs.len() != fi.generics.len() {
            return Err(format!("`{name}` has {} type parameters, but {} type arguments", fi.generics.len(), targs.len()));
        }
        let gens = &fi.generics;
        for (&g, &t) in gens.iter().zip(targs) {
            for tr in cp.params[g as usize].traits.iter().copied() {
                let tr = self.subst_trait(tr, gens, targs);
                self.check_trait(cx, t, tr, 0).map_err(|m| format!("a use of `{name}`: {m}"))?;
            }
        }
        let Some(args) = args else { return Ok(()) };
        let mut hidden = 0;
        for (&g, &t) in gens.iter().zip(targs) {
            if cp.params[g as usize].reflect {
                match args.get(hidden) {
                    Some(HArg::Expr(HExpr { kind: HKind::TypeDesc { ty, .. }, .. })) if *ty == t => {}
                    Some(HArg::Expr(HExpr { kind: HKind::Local(_), .. })) => {}
                    _ => return Err(format!("a call of `{name}` lacks the description of {}", self.show(t))),
                }
                hidden += 1;
            }
        }
        for &(t, tr) in &fi.dicts {
            let (t, tr) = (self.subst(t, gens, targs), self.subst_trait(tr, gens, targs));
            match args.get(hidden) {
                Some(HArg::Expr(e)) => self.check_dict(cx, e, t, tr).map_err(|m| format!("a call of `{name}`: {m}"))?,
                _ => return Err(format!("a call of `{name}` lacks a dictionary")),
            }
            hidden += 1;
        }
        for a in args {
            if let HArg::Default { helper, inputs, .. } = a {
                let h = &cp.fns[*helper as usize];
                if h.generics != fi.generics || h.dicts != fi.dicts || !inputs.iter().copied().take(hidden).eq(0..hidden) {
                    return Err(format!("a default argument of `{name}` gets other dictionaries than the call"));
                }
            }
        }
        Ok(())
    }
}

/// Returns the component types of built-in type `ty` that must implement
/// derived trait `b`, or `None` if `ty` cannot implement `b`.
fn builtin_parts(arena: &TypeArena, ty: Ty, b: Builtin) -> Option<Vec<TypeId>> {
    use Builtin::*;
    Some(match ty {
        Ty::Int | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Never | Ty::Var(_) => Vec::new(),
        Ty::Float if b != Hash => Vec::new(),
        Ty::Range if b != Ord => Vec::new(),
        Ty::List(x) if b != Hash => vec![x],
        Ty::Set(x) if matches!(b, Eq | Encode) => vec![x],
        Ty::Map(k, v) if matches!(b, Eq | Encode) => vec![k, v],
        Ty::Tuple(l) => arena.list(l).to_vec(),
        _ => return None,
    })
}
