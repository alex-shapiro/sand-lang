//! An independent check of the trait evidence in checked HIR, which debug
//! builds run before compiling, as GHC runs Core Lint. From the declarations
//! alone it proves again that:
//!
//! - each type argument of a call, function value, struct literal and variant
//!   implements the traits that its type parameter requires;
//! - each dictionary passed to a call is evidence for the trait and type that
//!   the callee's hidden parameter needs;
//! - the field and payload types of type definitions, and `reflect` type
//!   arguments, meet the requirements of their type constructors;
//! - the type of each impl implements the trait's supertraits.
//!
//! It shares no code with the checker's evidence search (`traits.rs`,
//! `derive.rs`), so a bug there makes verification fail instead of giving a
//! program a wrong dictionary or a set of values that cannot be hashed.

use super::*;

/// The traits whose impls the compiler derives.
const DERIVED: [Builtin; 4] = [Builtin::Eq, Builtin::Ord, Builtin::Hash, Builtin::Encode];

/// Verifies all type definitions and impls of `cp`, and `bodies`. Returns
/// the first violation found.
pub(crate) fn verify<'a>(cp: &CheckedProgram, bodies: impl Iterator<Item = (FnId, &'a HFn)>) -> Result<(), String> {
    let mut v = Verifier::new(cp);
    for d in 0..cp.types.len() {
        v.type_def(d as TypeDefId)?;
    }
    for i in 0..cp.impls.len() {
        v.impl_supertraits(i as ImplId)?;
    }
    for (f, body) in bodies {
        v.body(f, body).map_err(|m| format!("in `{}`: {m}", cp.name(cp.fns[f as usize].name)))?;
    }
    Ok(())
}

struct Verifier<'a> {
    cp: &'a CheckedProgram,
    /// A copy of the program's types, to which substitutions add.
    arena: TypeArena,
    natives: HashMap<u32, FnId>,
    builtins: HashMap<Builtin, TraitRef>,
    /// For each type definition and derived trait (in the order of
    /// [`DERIVED`]): the indices of the type parameters that must implement
    /// the trait for an instance to implement it, or `None` if none does.
    derived: Vec<[Option<Vec<u32>>; 4]>,
}

/// What the code being verified knows.
#[derive(Default)]
struct Cx {
    /// The function whose body is verified.
    f: Option<FnId>,
    /// The dictionary parameters of the function and what each proves.
    dicts: HashMap<LocalId, (TypeId, TraitRef)>,
    /// The dictionary of `Self` of an impl method.
    impl_dict: Option<(LocalId, ImplId)>,
    /// The dictionaries that the function builds on entry.
    hoisted: HashMap<LocalId, HExpr>,
}

type Res = Result<(), String>;

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

    /// The direct supertraits of `ty: tr`.
    fn supers_of(&mut self, ty: TypeId, tr: TraitRef) -> Vec<TraitRef> {
        let td = &self.cp.traits[tr.trait_id as usize];
        let mut from = td.generics.clone();
        from.push(td.self_param);
        let mut to = self.arena.list(tr.args).to_vec();
        to.push(ty);
        td.supers.clone().into_iter().map(|s| self.subst_trait(s, &from, &to)).collect()
    }

    /// The traits that type parameter `p` (of type `ty`) implements: those
    /// it requires, those of the function's dictionaries for it, and their
    /// supertraits.
    fn param_traits(&mut self, cx: &Cx, p: ParamId, ty: TypeId) -> Vec<TraitRef> {
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

    // ------------------------------------------------------------------
    // Derived impls

    /// Computes [`Verifier::derived`] as a greatest fixed point: a type
    /// definition implements a derived trait unless a member type refutes it.
    fn derive_all(&mut self) {
        let n = self.cp.types.len();
        self.derived = vec![std::array::from_fn(|_| Some(Vec::new())); n];
        let mut users: Vec<Vec<usize>> = vec![Vec::new(); n];
        for d in 0..n {
            for m in self.members(d) {
                self.arena.any(m, &mut |t| {
                    if let Ty::Named(e, _) = t
                        && users[e as usize].last() != Some(&d)
                    {
                        users[e as usize].push(d);
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
                    todo.extend(&users[d]);
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
        let mut needs = Vec::new();
        let mut todo = self.members(d);
        while let Some(t) = todo.pop() {
            match self.arena.get(t) {
                Ty::Param(p) => match generics.iter().position(|&g| g == p) {
                    Some(i) => needs.push(i as u32),
                    None => {
                        let pt = self.arena.param(p);
                        let need = self.builtins[&trait_];
                        if !self.param_traits(&Cx::default(), p, pt).contains(&need) {
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
        needs.sort_unstable();
        needs.dedup();
        Some(needs)
    }

    // ------------------------------------------------------------------
    // Implemented traits

    /// Proves `ty: tr`.
    fn holds(&mut self, cx: &Cx, ty: TypeId, tr: TraitRef, depth: u32) -> Res {
        let fail = |v: &Self| format!("{} does not implement `{}`", v.show(ty), v.show_trait(tr));
        if self.arena.has_vars(ty) {
            // Left unknown by inference: no value of the type exists.
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
                Some(b) if b.is_derived() => self.derived_holds(cx, ty, b),
                _ => {
                    let (i, sigma) = self.impl_for(ty, tr).ok_or_else(|| fail(self))?;
                    let imp = &self.cp.impls[i as usize];
                    let gens = imp.generics.clone();
                    for (t, s) in imp.evidence.clone() {
                        let (t, s) = (self.subst(t, &gens, &sigma), self.subst_trait(s, &gens, &sigma));
                        self.holds(cx, t, s, depth + 1)?;
                    }
                    Ok(())
                }
            },
        }
    }

    /// Proves `ty: b` for a derived trait `b` from the components of `ty`.
    fn derived_holds(&mut self, cx: &Cx, ty: TypeId, b: Builtin) -> Res {
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
                        Some(needs) => todo.extend(needs.iter().map(|&i| args[i as usize])),
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

    /// The impl of `tr` that matches `ty`, and the type arguments of its
    /// type parameters. An impl of a later session input replaces an earlier
    /// one, so the last match applies.
    fn impl_for(&self, ty: TypeId, tr: TraitRef) -> Option<(ImplId, Vec<TypeId>)> {
        (0..self.cp.impls.len() as ImplId).rev().find_map(|i| Some((i, self.match_impl(i, ty, tr)?)))
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

    /// Whether `t` is an instance of `pat`, binding the type parameters
    /// `gens` of `pat` in `sigma`.
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

    /// Checks that the type arguments in `ty` meet the requirements of their
    /// type constructors: set elements and map keys implement `Hash`, and
    /// `Bag<X>` gives `X` the traits that `Bag`'s parameter requires.
    fn well_formed(&mut self, cx: &Cx, ty: TypeId) -> Res {
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
                    self.holds(cx, x, hash, 0).map_err(|m| format!("{m} (required for the elements of {})", self.show(t)))?;
                    todo.push(x);
                }
                Ty::Map(k, x) => {
                    self.holds(cx, k, hash, 0).map_err(|m| format!("{m} (required for the keys of {})", self.show(t)))?;
                    todo.extend([k, x]);
                }
                Ty::Tuple(l) => todo.extend(self.arena.list(l)),
                Ty::Fn(ps, r) => {
                    todo.extend(self.arena.list(ps));
                    todo.push(r);
                }
                Ty::Named(..) => {
                    self.constructed(cx, t)?;
                    let Ty::Named(_, args) = self.arena.get(t) else { unreachable!() };
                    todo.extend(self.arena.list(args));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Checks that the type arguments of `Named` type `ty` implement the
    /// traits that the definition's type parameters require.
    fn constructed(&mut self, cx: &Cx, ty: TypeId) -> Res {
        let Ty::Named(d, args) = self.arena.get(ty) else { return Ok(()) };
        let gens = self.cp.types[d as usize].generics.clone();
        let args = self.arena.list(args).to_vec();
        for (&g, &arg) in gens.iter().zip(&args) {
            for tr in self.cp.params[g as usize].traits.clone() {
                let tr = self.subst_trait(tr, &gens, &args);
                self.holds(cx, arg, tr, 0).map_err(|m| format!("{m} (required for {})", self.show(ty)))?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Dictionaries

    /// Checks that `e` is a dictionary for `ty: tr`.
    fn dict(&mut self, cx: &Cx, e: &HExpr, ty: TypeId, tr: TraitRef) -> Res {
        if self.arena.has_vars(ty) || self.arena.list(tr.args).iter().any(|&a| self.arena.has_vars(a)) {
            // The type is unknown, so the dictionary is never used.
            return Ok(());
        }
        let want = |v: &Self| format!("the dictionary for {} implementing `{}`", v.show(ty), v.show_trait(tr));
        if !self.has_dict(tr) {
            return Err(format!("{} is passed, but the trait has no methods", want(self)));
        }
        match &e.kind {
            HKind::Local(l) if !cx.dicts.contains_key(l) => match cx.hoisted.get(l) {
                Some(init) => self.dict(cx, &init.clone(), ty, tr),
                None => Err(format!("{} is local {l}, which holds no dictionary", want(self))),
            },
            HKind::Local(_) | HKind::Field { .. } => {
                let (t, have) = self.proof(cx, e)?;
                if (t, have) == (ty, tr) {
                    Ok(())
                } else {
                    Err(format!("{} is the dictionary for {} implementing `{}`", want(self), self.show(t), self.show_trait(have)))
                }
            }
            HKind::Dict { impl_id, args } => self.impl_dict(cx, *impl_id, args, ty, tr).map_err(|m| format!("{}: {m}", want(self))),
            _ => Err(format!("{} is not a dictionary expression", want(self))),
        }
    }

    /// What a dictionary parameter, or a projection of one, proves.
    fn proof(&mut self, cx: &Cx, e: &HExpr) -> Result<(TypeId, TraitRef), String> {
        match &e.kind {
            HKind::Local(l) => cx.dicts.get(l).copied().ok_or_else(|| format!("local {l} is not a dictionary parameter")),
            HKind::Field { obj, index } => {
                let (ty, tr) = self.proof(cx, obj)?;
                let methods = self.cp.traits[tr.trait_id as usize].methods.len();
                let field = (*index as usize).checked_sub(methods).ok_or("a method is used as a dictionary")?;
                let supers: Vec<TraitRef> = self.supers_of(ty, tr).into_iter().filter(|&s| self.has_dict(s)).collect();
                if let Some(&s) = supers.get(field) {
                    return Ok((ty, s));
                }
                // The impl dictionary of an impl method holds the dictionaries
                // of what the impl requires of its type parameters.
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

    /// Checks that the dictionary of impl `i` with fields `args` proves `ty: tr`.
    fn impl_dict(&mut self, cx: &Cx, i: ImplId, args: &[HExpr], ty: TypeId, tr: TraitRef) -> Res {
        let sigma = self.match_impl(i, ty, tr).ok_or("its impl does not match")?;
        let imp = &self.cp.impls[i as usize];
        let (gens, evidence) = (imp.generics.clone(), imp.evidence.clone());
        let mut needed: Vec<(TypeId, TraitRef)> = self.supers_of(ty, tr).into_iter().map(|s| (ty, s)).collect();
        for (t, s) in evidence {
            needed.push((self.subst(t, &gens, &sigma), self.subst_trait(s, &gens, &sigma)));
        }
        let mut fields = args.iter();
        for (t, s) in needed {
            if self.has_dict(s) {
                let e = fields.next().ok_or("it has too few fields")?;
                self.dict(cx, e, t, s)?;
            } else {
                self.holds(cx, t, s, 0)?;
            }
        }
        if fields.next().is_some() {
            return Err("it has too many fields".into());
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Declarations and bodies

    fn type_def(&mut self, d: TypeDefId) -> Res {
        let cx = Cx::default();
        for t in self.members(d as usize) {
            self.well_formed(&cx, t).map_err(|m| format!("a member of `{}`: {m}", self.cp.name(self.cp.types[d as usize].name)))?;
        }
        Ok(())
    }

    fn impl_supertraits(&mut self, i: ImplId) -> Res {
        let imp = &self.cp.impls[i as usize];
        let (target, tr) = (imp.target, imp.trait_ref);
        for s in self.supers_of(target, tr) {
            self.holds(&Cx::default(), target, s, 0)
                .map_err(|m| format!("the impl of `{}` for {}: {m}", self.show_trait(tr), self.show(target)))?;
        }
        Ok(())
    }

    fn body(&mut self, f: FnId, b: &HFn) -> Res {
        let fi = &self.cp.fns[f as usize];
        let reflect = fi.generics.iter().filter(|&&g| self.cp.params[g as usize].reflect).count();
        let mut cx = Cx { f: Some(f), ..Cx::default() };
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
                result = self.expr(&mut cx, &b.locals, e);
            }
        });
        result
    }

    fn expr(&mut self, cx: &mut Cx, locals: &[LocalInfo], e: &HExpr) -> Res {
        match &e.kind {
            HKind::Error | HKind::Evidence(_) => Err("a placeholder remains".into()),
            HKind::Let { pat: HPat { kind: HPatKind::Bind(l), .. }, init, .. } if locals[*l as usize].name == Symbol::DICT => {
                cx.hoisted.insert(*l, (**init).clone());
                Ok(())
            }
            HKind::Call { func, targs, args } => self.instance(cx, *func, targs, Some(args)),
            HKind::CallNative { native, targs, args } => match self.natives.get(native) {
                Some(&f) => self.instance(cx, f, targs, Some(args)),
                None => Err(format!("native {native} has no declaration")),
            },
            HKind::CallTrait { trait_id, slot, targs, args, impl_id } => {
                let td = &self.cp.traits[*trait_id as usize];
                let sig = td.methods.get(*slot as usize).ok_or("a trait method slot does not exist")?.sig;
                self.instance(cx, sig, targs, Some(args))?;
                if let Some(i) = impl_id {
                    let gens = &self.cp.fns[sig as usize].generics;
                    let self_ty = gens.iter().position(|&g| g == td.self_param).map(|k| targs[k]).ok_or("a trait method has no `Self`")?;
                    let tr = TraitRef { trait_id: *trait_id, args: self.arena.intern_list(&targs[..td.generics.len()]) };
                    if !self.arena.has_vars(self_ty) && self.match_impl(*i, self_ty, tr).is_none() {
                        return Err(format!("a direct call of `{}` for {} goes to another impl", self.show_trait(tr), self.show(self_ty)));
                    }
                }
                Ok(())
            }
            HKind::FnValue { func, targs } => self.instance(cx, *func, targs, None),
            HKind::MakeStruct { .. } | HKind::MakeVariant { .. } | HKind::Tag(_) => self.constructed(cx, e.ty),
            HKind::VariantFn { .. } => match self.arena.get(e.ty) {
                Ty::Fn(_, ret) => self.constructed(cx, ret),
                _ => Ok(()),
            },
            HKind::TypeDesc { ty, .. } => self.well_formed(cx, *ty),
            _ => Ok(()),
        }
    }

    /// Checks a use of function `f` with type arguments `targs`: a call with
    /// arguments `args` (hidden ones first) or, without them, a function value.
    fn instance(&mut self, cx: &Cx, f: FnId, targs: &[TypeId], args: Option<&[HArg]>) -> Res {
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
                self.holds(cx, t, tr, 0).map_err(|m| format!("a use of `{name}`: {m}"))?;
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
                Some(HArg::Expr(e)) => self.dict(cx, e, t, tr).map_err(|m| format!("a call of `{name}`: {m}"))?,
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

/// The components of built-in type `ty` that must implement derived trait
/// `b` for `ty` to implement it, or `None` if `ty` cannot implement it.
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
