//! User-defined traits: declarations, impls, method lookup and evidence.
//!
//! Values are erased at runtime, so generic code receives *dictionaries*:
//! a function with trait bounds takes one hidden parameter per (type
//! parameter, trait) bound, before its declared parameters. A dictionary is a
//! tuple holding the trait's methods (impl methods or defaults), then the
//! dictionaries of the trait's supertraits for the same type, then those of
//! the impl's own bounds. Trait methods (signatures, default and impl
//! methods) take the dictionary of `Self` as their first hidden parameter.
//!
//! Calls record the evidence they need as [`HKind::Evidence`] placeholders;
//! when the enclosing function has been checked (and its types inferred),
//! each is resolved to a hidden parameter of the function, a projection of
//! one (supertraits, impl bounds) or an impl's [`HKind::Dict`].

use super::expr::mk;
use super::*;

#[derive(Clone, Debug)]
pub struct TraitMethod {
    pub name: String,
    /// The signature ([`FnKind::TraitMethod`]).
    pub sig: FnId,
    /// The default implementation, if any.
    pub default: Option<FnId>,
}

#[derive(Clone, Debug)]
pub struct TraitDef {
    pub name: String,
    pub module: ModId,
    pub is_pub: bool,
    pub doc: Option<String>,
    pub span: Span,
    /// The trait's own type parameters.
    pub generics: Vec<ParamId>,
    /// The type parameter standing for the implementing type (`Self`).
    pub self_param: ParamId,
    /// Direct supertraits, in terms of `generics` and `self_param`.
    pub supers: Vec<TraitRef>,
    pub builtin_supers: Vec<Bound>,
    pub methods: Vec<TraitMethod>,
}

#[derive(Clone, Debug)]
pub struct ImplDef {
    pub trait_ref: TraitRef,
    pub target: Type,
    pub generics: Vec<ParamId>,
    pub module: ModId,
    pub span: Span,
    /// The function for each method of the trait (impl method or default).
    pub slots: Vec<FnId>,
    /// Trait bounds of the impl's type parameters, whose dictionaries follow
    /// the supertrait dictionaries in the impl's dictionary.
    pub evidence: Vec<(Type, TraitRef)>,
}

/// An impl instantiated with fresh type variables.
struct ImplInst {
    target: Type,
    args: Vec<Type>,
    evidence: Vec<(Type, TraitRef)>,
    builtin: Vec<(Type, Bound)>,
}

/// The key under which impls are registered: type constructor and, for
/// tuples, the arity (tuples of different sizes are different constructors).
pub(crate) fn impl_key(t: &Type) -> Option<(Ctor, u32)> {
    match t {
        Type::Fn(..) => None,
        Type::Tuple(ts) => Some((Ctor::Tuple, ts.len() as u32)),
        _ => ctor_of(t).map(|c| (c, 0)),
    }
}

fn mentions(t: &Type, p: ParamId) -> bool {
    let mut found = false;
    t.walk(&mut |x| {
        if *x == Type::Param(p) {
            found = true
        }
    });
    found
}

fn has_params(t: &Type) -> bool {
    let mut found = false;
    t.walk(&mut |x| {
        if matches!(x, Type::Param(_)) {
            found = true
        }
    });
    found
}

fn dict_expr(kind: HKind, span: Span) -> HExpr {
    mk(kind, Type::Unit, span)
}

/// Whether a dictionary expression is built from impls only (a constant).
fn is_const_dict(e: &HExpr) -> bool {
    match &e.kind {
        HKind::Dict { args, .. } => args.iter().all(is_const_dict),
        _ => false,
    }
}

/// A structural key identifying a dictionary expression.
fn dict_key(e: &HExpr) -> String {
    match &e.kind {
        HKind::Dict { impl_id, args } => format!("D{impl_id}({})", args.iter().map(dict_key).collect::<Vec<_>>().join(",")),
        HKind::Local(l) => format!("L{l}"),
        HKind::Field { obj, index } => format!("{}.{index}", dict_key(obj)),
        HKind::Unit => "U".into(),
        _ => "?".into(),
    }
}

impl Checker<'_> {
    pub fn show_trait(&self, tr: &TraitRef) -> String {
        let mut s = self.traits[tr.trait_id as usize].name.clone();
        if !tr.args.is_empty() {
            let args: Vec<String> = tr.args.iter().map(|t| self.show(t)).collect();
            s.push('<');
            s.push_str(&args.join(", "));
            s.push('>');
        }
        s
    }

    // ------------------------------------------------------------------
    // Bounds

    /// Resolves bounds written in source: built-in bounds and traits.
    pub(crate) fn resolve_bounds(
        &mut self,
        bs: &[TypeExpr],
        scope: &[(String, ParamId)],
        self_ty: Option<&Type>,
    ) -> (Vec<Bound>, Vec<TraitRef>) {
        let mut bounds = Vec::new();
        let mut traits = Vec::new();
        for b in bs {
            let TypeExprKind::Named { path, args } = &b.kind else {
                self.error(b.span, "expected a bound");
                continue;
            };
            if path.len() == 1
                && let Some(x) = Bound::parse(&path[0].name)
            {
                if !args.is_empty() {
                    self.error(b.span, format!("`{}` does not take type arguments", path[0].name));
                }
                bounds.push(x);
                continue;
            }
            let Some(tid) = self.resolve_trait_path(path, b.span) else {
                continue;
            };
            let args: Vec<Type> = args.iter().map(|t| self.resolve_type_in(t, scope, self_ty)).collect();
            let n = self.traits[tid as usize].generics.len();
            if args.len() != n {
                let plural = if n == 1 { "" } else { "s" };
                let name = self.traits[tid as usize].name.clone();
                self.error(b.span, format!("expected {n} type argument{plural} for trait `{name}`, found {}", args.len()));
                continue;
            }
            traits.push(TraitRef { trait_id: tid, args });
        }
        (bounds, traits)
    }

    /// Resolves `Trait` or `module.Trait`.
    fn resolve_trait_path(&mut self, path: &[ast::Ident], span: Span) -> Option<TraitId> {
        let name = path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
        let def = match path {
            [one] => self.lookup_global(&one.name),
            [m, item] => match self.lookup_global(&m.name) {
                Some(Def::Module(m)) => {
                    // Reports private or missing members itself.
                    Some(self.module_member(m, &item.name, item.span)?)
                }
                _ => None,
            },
            _ => None,
        };
        match def {
            Some(Def::Trait(t)) => Some(t),
            Some(Def::Type(_)) => {
                self.error(span, format!("`{name}` is a type, not a trait"));
                None
            }
            _ => {
                self.error(span, format!("unknown bound `{name}`"));
                None
            }
        }
    }

    /// Adds bounds to a type parameter, with the built-in bounds its traits
    /// imply. A `Num` bound also adds the trait that carries its evidence.
    pub(crate) fn add_bounds(&mut self, p: ParamId, mut bounds: Vec<Bound>, mut traits: Vec<TraitRef>) {
        // (A trait with `Num` as a supertrait carries its evidence itself.)
        if let Some(n) = self.num_trait.filter(|_| bounds.contains(&Bound::Num)) {
            traits.push(TraitRef { trait_id: n, args: vec![] });
        }
        for tr in &traits {
            bounds.extend(self.builtin_closure(tr.trait_id));
        }
        let info = &mut self.params[p as usize];
        for b in bounds {
            if !info.bounds.contains(&b) {
                info.bounds.push(b);
            }
        }
        for tr in traits {
            if !info.traits.contains(&tr) {
                info.traits.push(tr);
            }
        }
    }

    /// Applies a `where` clause to the type parameters in `scope`.
    pub(crate) fn apply_where(&mut self, preds: &[ast::WherePredicate], scope: &[(String, ParamId)], self_ty: Option<&Type>) {
        for w in preds {
            let (bounds, traits) = self.resolve_bounds(&w.bounds, scope, self_ty);
            match scope.iter().rev().find(|(n, _)| *n == w.name.name) {
                Some((_, p)) => self.add_bounds(*p, bounds, traits),
                None => self.error(w.name.span, format!("unknown type parameter `{}`", w.name.name)),
            }
        }
    }

    pub(crate) fn trait_as_type_error(&mut self, id: &ast::Ident) {
        self.error(id.span, format!("`{}` is a trait, not a type; use a type parameter bounded by it (`<T: {}>`)", id.name, id.name));
    }

    /// Built-in bounds implied by a trait (through its supertraits).
    fn builtin_closure(&self, t: TraitId) -> Vec<Bound> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![t];
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            let td = &self.traits[t as usize];
            for b in &td.builtin_supers {
                if !out.contains(b) {
                    out.push(*b);
                }
            }
            stack.extend(td.supers.iter().map(|s| s.trait_id));
        }
        out
    }

    /// The direct supertraits of `ty: tr`.
    fn supers_of(&self, ty: &Type, tr: &TraitRef) -> Vec<TraitRef> {
        let t = &self.traits[tr.trait_id as usize];
        let map = |p: ParamId| {
            if p == t.self_param { Some(ty.clone()) } else { t.generics.iter().position(|&g| g == p).map(|i| tr.args[i].clone()) }
        };
        t.supers.iter().map(|s| s.subst_params(&map)).collect()
    }

    /// The traits a type parameter is known to implement (with supertraits).
    fn bound_traits(&self, p: ParamId) -> Vec<TraitRef> {
        self.with_supers(&Type::Param(p), self.params[p as usize].traits.clone())
    }

    /// `traits` of `ty` and all their supertraits, in breadth-first order.
    fn with_supers(&self, ty: &Type, traits: Vec<TraitRef>) -> Vec<TraitRef> {
        let mut out: Vec<TraitRef> = Vec::new();
        let mut queue = std::collections::VecDeque::from(traits);
        while let Some(tr) = queue.pop_front() {
            if out.contains(&tr) {
                continue;
            }
            queue.extend(self.supers_of(ty, &tr));
            out.push(tr);
        }
        out
    }

    // ------------------------------------------------------------------
    // Trait declarations

    pub(crate) fn declare_trait(&mut self, t: &ast::TraitDecl) -> TraitId {
        // The prelude declares the (private) trait carrying the evidence of `Num`.
        let num_evidence = Some(self.cur_mod) == self.prelude && t.name.name == "Num";
        if Bound::parse(&t.name.name).is_some() && !num_evidence {
            self.error(t.name.span, format!("`{}` is a built-in bound and cannot be redefined", t.name.name));
        }
        let mut generics = Vec::new();
        for g in &t.generics {
            if !g.bounds.is_empty() {
                self.error(g.name.span, "bounds on trait type parameters are not supported; put them on the methods or impls");
            }
            generics.push(self.new_param(&g.name.name, vec![], false));
        }
        let self_param = self.new_param("Self", vec![], false);
        self.traits.push(TraitDef {
            name: t.name.name.clone(),
            module: self.cur_mod,
            is_pub: t.is_pub,
            doc: t.doc.clone(),
            span: t.name.span,
            generics,
            self_param,
            supers: Vec::new(),
            builtin_supers: Vec::new(),
            methods: Vec::new(),
        });
        let id = (self.traits.len() - 1) as TraitId;
        if num_evidence {
            self.num_trait = Some(id);
        }
        id
    }

    fn trait_scope(&self, t: TraitId) -> Vec<(String, ParamId)> {
        self.traits[t as usize].generics.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect()
    }

    /// Resolves the supertraits of the traits declared by the current module.
    pub(crate) fn resolve_supertraits(&mut self, ids: &[TraitId], decls: &[&ast::TraitDecl]) {
        for (&id, d) in ids.iter().zip(decls) {
            let scope = self.trait_scope(id);
            let self_ty = Type::Param(self.traits[id as usize].self_param);
            let (bounds, mut supers) = self.resolve_bounds(&d.supers, &scope, Some(&self_ty));
            // `Num` carries evidence (`zero`, `one`), so its trait is a real supertrait.
            if let Some(n) = self.num_trait.filter(|_| bounds.contains(&Bound::Num)) {
                supers.push(TraitRef { trait_id: n, args: vec![] });
            }
            let td = &mut self.traits[id as usize];
            td.builtin_supers = bounds;
            td.supers = supers;
        }
        for (&id, d) in ids.iter().zip(decls) {
            if self.super_reaches(id, id) {
                self.error(d.name.span, format!("trait `{}` is its own supertrait", d.name.name));
                self.traits[id as usize].supers.clear();
            }
        }
        // Inside the trait, `Self` implements it.
        for &id in ids {
            let td = &self.traits[id as usize];
            let tr = TraitRef { trait_id: id, args: td.generics.iter().map(|&p| Type::Param(p)).collect() };
            let sp = td.self_param;
            self.params[sp as usize].traits = vec![tr];
            self.params[sp as usize].bounds = self.builtin_closure(id);
        }
    }

    fn super_reaches(&self, from: TraitId, target: TraitId) -> bool {
        let mut seen = HashSet::new();
        let mut stack: Vec<TraitId> = self.traits[from as usize].supers.iter().map(|s| s.trait_id).collect();
        while let Some(t) = stack.pop() {
            if t == target {
                return true;
            }
            if seen.insert(t) {
                stack.extend(self.traits[t as usize].supers.iter().map(|s| s.trait_id));
            }
        }
        false
    }

    /// Declares the method signatures and default methods of a trait.
    pub(crate) fn declare_trait_methods(&mut self, id: TraitId, d: &ast::TraitDecl) {
        let module_name = self.mods[self.cur_mod as usize].name.clone();
        let td = self.traits[id as usize].clone();
        let mut outer = self.trait_scope(id);
        outer.push(("Self".into(), td.self_param));
        let self_ty = Type::Param(td.self_param);
        let mut methods: Vec<TraitMethod> = Vec::new();
        for m in &d.methods {
            if methods.iter().any(|x| x.name == m.name.name) {
                self.error(m.name.span, format!("method `{}` is already defined in trait `{}`", m.name.name, td.name));
                continue;
            }
            let mut m2 = m.clone();
            for w in &m.where_clause {
                if outer.iter().any(|(n, _)| *n == w.name.name) {
                    self.error(w.name.span, "`where` clauses in trait methods may only constrain the method's own type parameters");
                }
            }
            m2.where_clause.retain(|w| !outer.iter().any(|(n, _)| *n == w.name.name));
            for p in &mut m2.params {
                if let Some(dv) = &p.default
                    && !is_closed_const(dv)
                {
                    self.error(dv.span, "default values of trait method parameters must be constants");
                    p.default = None;
                }
            }
            let slot = methods.len() as u32;
            let sig = self.declare_fn(&m2, None, &outer, Some(self_ty.clone()));
            let f = &mut self.fns[sig as usize];
            f.kind = FnKind::TraitMethod(id, slot);
            f.decl = None;
            f.trait_of = Some(id);
            f.qual_name = format!("{module_name}.{}.{}", td.name, m.name.name);
            let default = if m.body.is_some() {
                let mut fi = self.fns[sig as usize].clone();
                fi.kind = FnKind::Script;
                fi.decl = Some(FnDeclRef::Fn(self.cur_mod, m2.clone()));
                Some(self.add_fn(fi))
            } else {
                None
            };
            methods.push(TraitMethod { name: m.name.name.clone(), sig, default });
        }
        self.traits[id as usize].methods = methods;
    }

    // ------------------------------------------------------------------
    // Impls

    /// Declares an impl and its methods, checking them against the trait.
    pub(crate) fn declare_impl(&mut self, d: &ast::ImplDecl) -> Option<ImplId> {
        let span = d.trait_ref.span;
        let generics = self.declare_params(&d.generics, &[], None);
        let scope: Vec<(String, ParamId)> = generics.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect();
        self.apply_where(&d.where_clause, &scope, None);
        let TypeExprKind::Named { path, args: targs } = &d.trait_ref.kind else {
            self.error(span, "expected a trait");
            return None;
        };
        if let [one] = path.as_slice()
            && Bound::parse(&one.name).is_some()
            && Some(self.cur_mod) != self.prelude
        {
            self.error(span, format!("`{}` is a built-in bound; it is satisfied automatically and cannot be implemented", one.name));
            return None;
        }
        let tid = self.resolve_trait_path(path, span)?;
        let args: Vec<Type> = targs.iter().map(|t| self.resolve_type_in(t, &scope, None)).collect();
        let tdef = self.traits[tid as usize].clone();
        if args.len() != tdef.generics.len() {
            let n = tdef.generics.len();
            let plural = if n == 1 { "" } else { "s" };
            self.error(span, format!("expected {n} type argument{plural} for trait `{}`, found {}", tdef.name, args.len()));
            return None;
        }
        let trait_ref = TraitRef { trait_id: tid, args };
        let target = self.resolve_type_in(&d.target, &scope, None);
        if target == Type::Error {
            return None;
        }
        let shown = format!("`{}` for {}", self.show_trait(&trait_ref), self.show(&target));
        let Some((ctor, arity)) = impl_key(&target).filter(|_| !matches!(target, Type::Param(_))) else {
            let msg = format!("cannot implement traits for {}; the type must have a type constructor", self.show(&target));
            self.error(d.target.span, msg);
            return None;
        };
        for (g, &p) in d.generics.iter().zip(&generics) {
            if !mentions(&target, p) && !trait_ref.args.iter().any(|a| mentions(a, p)) {
                self.error(g.name.span, format!("type parameter `{}` is not used by the impl's type or trait arguments", g.name.name));
            }
        }
        // Orphan rule: built-in types count as declared by the prelude.
        let type_mod = match ctor {
            Ctor::Def(dd) => self.types[dd as usize].module,
            _ => self.prelude.unwrap_or(u32::MAX),
        };
        if tdef.module != self.cur_mod && type_mod != self.cur_mod {
            self.error(span, format!("the impl of {shown} must be in the module that declares the trait or the type"));
            return None;
        }
        // Coherence: at most one impl per trait (and trait arguments) per type constructor.
        for other in self.impls_by.get(&(tid, ctor, arity)).cloned().unwrap_or_default() {
            if self.impls_overlap(&trait_ref, &generics, other) {
                if self.impls[other as usize].module == self.cur_mod && self.defined_by_earlier_input(self.impls[other as usize].span) {
                    // An impl of an earlier session input is replaced (code
                    // already checked keeps using it).
                    for ids in
                        [self.impls_by.get_mut(&(tid, ctor, arity)), self.impls_by_ctor.get_mut(&(ctor, arity))].into_iter().flatten()
                    {
                        ids.retain(|&i| i != other);
                    }
                    continue;
                }
                let line = self.sm.file(self.impls[other as usize].span.file).line_col(self.impls[other as usize].span.lo).0;
                self.error(span, format!("conflicting impls of {shown}: it is already implemented (line {line})"));
                return None;
            }
        }
        let id = self.impls.len() as ImplId;
        let evidence: Vec<(Type, TraitRef)> =
            generics.iter().flat_map(|&p| self.params[p as usize].traits.iter().map(move |tr| (Type::Param(p), tr.clone()))).collect();
        self.impls.push(ImplDef {
            trait_ref: trait_ref.clone(),
            target: target.clone(),
            generics: generics.clone(),
            module: self.cur_mod,
            span,
            slots: Vec::new(),
            evidence,
        });
        self.impls_by.entry((tid, ctor, arity)).or_default().push(id);
        self.impls_by_ctor.entry((ctor, arity)).or_default().push(id);

        // Methods.
        let mut slots: Vec<Option<FnId>> = vec![None; tdef.methods.len()];
        for m in &d.methods {
            let Some(slot) = tdef.methods.iter().position(|x| x.name == m.name.name) else {
                self.error(m.name.span, format!("`{}` is not a method of trait `{}`", m.name.name, tdef.name));
                continue;
            };
            if slots[slot].is_some() {
                self.error(m.name.span, format!("method `{}` is already defined", m.name.name));
                continue;
            }
            let mut m2 = m.clone();
            for w in &m.where_clause {
                if scope.iter().any(|(n, _)| *n == w.name.name) {
                    self.error(w.name.span, "impl methods cannot add bounds to the impl's type parameters; add them to the impl");
                }
            }
            m2.where_clause.retain(|w| !scope.iter().any(|(n, _)| *n == w.name.name));
            for p in &mut m2.params {
                if p.default.take().is_some() {
                    self.error(p.name.span, "default values are declared by the trait, not the impl");
                }
            }
            let f = self.declare_fn(&m2, Some(ctor), &scope, Some(target.clone()));
            self.fns[f as usize].impl_of = Some(id);
            self.check_impl_method(id, slot as u32, f, m);
            slots[slot] = Some(f);
        }
        let mut missing = Vec::new();
        let slots: Vec<FnId> = slots
            .iter()
            .enumerate()
            .map(|(i, s)| match (s, tdef.methods[i].default) {
                (Some(f), _) => *f,
                (None, Some(d)) => d,
                (None, None) => {
                    missing.push(format!("`{}`", tdef.methods[i].name));
                    tdef.methods[i].sig
                }
            })
            .collect();
        if !missing.is_empty() {
            let plural = if missing.len() == 1 { "" } else { "s" };
            self.error(span, format!("missing method{plural} {} in the impl of {shown}", missing.join(", ")));
        }
        self.impls[id as usize].slots = slots;
        Some(id)
    }

    /// Whether a new impl (with type parameters `generics`) has trait
    /// arguments that unify with those of impl `other`.
    fn impls_overlap(&mut self, tr: &TraitRef, generics: &[ParamId], other: ImplId) -> bool {
        let fresh: Vec<Type> = generics.iter().map(|_| self.fresh()).collect();
        let mine: Vec<Type> =
            tr.args.iter().map(|t| t.subst_params(&|p| generics.iter().position(|&g| g == p).map(|i| fresh[i].clone()))).collect();
        let theirs = self.instantiate_impl(other).args;
        let pairs: Vec<(Type, Type)> = mine.into_iter().zip(theirs).collect();
        self.unify_probe(&pairs)
    }

    /// Checks an impl method's signature against the trait's and sets its dictionaries.
    fn check_impl_method(&mut self, id: ImplId, slot: u32, f: FnId, m: &ast::FnDecl) {
        let imp = self.impls[id as usize].clone();
        let td = self.traits[imp.trait_ref.trait_id as usize].clone();
        let sig = self.fns[td.methods[slot as usize].sig as usize].clone();
        let fi = self.fns[f as usize].clone();
        let sig_own = &sig.generics[td.generics.len() + 1..];
        let fi_own = &fi.generics[imp.generics.len()..];
        let map = |p: ParamId| {
            if p == td.self_param {
                Some(imp.target.clone())
            } else if let Some(i) = td.generics.iter().position(|&g| g == p) {
                Some(imp.trait_ref.args[i].clone())
            } else {
                sig_own.iter().position(|&g| g == p).and_then(|i| fi_own.get(i)).map(|&q| Type::Param(q))
            }
        };
        let want_params: Vec<(String, Type, bool)> =
            sig.params.iter().map(|p| (p.name.clone(), p.ty.subst_params(&map), p.default.is_some())).collect();
        let want_ret = sig.ret.subst_params(&map);
        let same = |a: &Type, b: &Type| a == b || *a == Type::Error || *b == Type::Error;
        let mut ok = sig.has_self == fi.has_self
            && sig_own.len() == fi_own.len()
            && want_params.len() == fi.params.len()
            && want_params.iter().zip(&fi.params).all(|((_, t, _), p)| same(t, &p.ty))
            && same(&want_ret, &fi.ret);
        if ok {
            for (&a, &b) in sig_own.iter().zip(fi_own) {
                let (pa, pb) = (&self.params[a as usize], &self.params[b as usize]);
                let ta: Vec<TraitRef> = pa.traits.iter().map(|t| t.subst_params(&map)).collect();
                let same_bounds = pa.bounds.iter().all(|x| pb.bounds.contains(x)) && pb.bounds.iter().all(|x| pa.bounds.contains(x));
                let same_traits = ta.iter().all(|x| pb.traits.contains(x)) && pb.traits.iter().all(|x| ta.contains(x));
                ok &= same_bounds && same_traits;
            }
        }
        if !ok {
            let want = self.printer().signature(&sig.name, &want_params, &want_ret);
            let found = self.fn_signature(f);
            let msg = if want == found {
                format!("the type parameters of method `{}` have different bounds than in trait `{}`", m.name.name, td.name)
            } else {
                format!(
                    "method `{}` does not match its declaration in trait `{}`: expected `{want}`, found `{found}`",
                    m.name.name, td.name
                )
            };
            self.error(m.name.span, msg);
        }
        // The impl's dictionary, then the method's own bounds in the trait's order.
        let mut dicts = vec![(imp.target.clone(), imp.trait_ref.clone())];
        if ok {
            dicts.extend(sig.dicts[1..].iter().map(|(t, tr)| (t.subst_params(&map), tr.subst_params(&map))));
        }
        self.fns[f as usize].dicts = dicts;
    }

    /// Checks that the type of an impl satisfies the trait's supertraits.
    pub(crate) fn check_impl_supertraits(&mut self, id: ImplId) {
        let imp = self.impls[id as usize].clone();
        let tid = imp.trait_ref.trait_id;
        let name = self.traits[tid as usize].name.clone();
        let saved = std::mem::take(&mut self.fs.evidence);
        self.fs.evidence = imp.evidence.iter().map(|(t, tr)| (t.clone(), tr.clone(), dict_expr(HKind::Unit, imp.span))).collect();
        for b in self.builtin_closure(tid) {
            if self.satisfies(&imp.target, b) == Some(false) {
                let msg = format!("{} does not satisfy {}, which trait `{name}` requires", self.show(&imp.target), b.name());
                self.error(imp.span, msg);
            }
        }
        for s in self.supers_of(&imp.target, &imp.trait_ref) {
            // A missing `Num` was reported as a built-in bound above.
            if Some(s.trait_id) == self.num_trait {
                continue;
            }
            if let Err(m) = self.resolve_evidence(&imp.target, &s, 0) {
                self.error(imp.span, format!("{m}, which trait `{name}` requires"));
            }
        }
        self.fs.evidence = saved;
    }

    fn instantiate_impl(&mut self, id: ImplId) -> ImplInst {
        let imp = self.impls[id as usize].clone();
        let fresh: Vec<Type> = imp.generics.iter().map(|_| self.fresh()).collect();
        let map = |p: ParamId| imp.generics.iter().position(|&g| g == p).map(|i| fresh[i].clone());
        let builtin = imp
            .generics
            .iter()
            .zip(&fresh)
            .flat_map(|(&p, v)| self.params[p as usize].bounds.iter().map(move |&b| (v.clone(), b)))
            .collect();
        ImplInst {
            target: imp.target.subst_params(&map),
            args: imp.trait_ref.args.iter().map(|t| t.subst_params(&map)).collect(),
            evidence: imp.evidence.iter().map(|(t, tr)| (t.subst_params(&map), tr.subst_params(&map))).collect(),
            builtin,
        }
    }

    // ------------------------------------------------------------------
    // Method lookup

    /// Finds the trait providing method `name` for values of type `ty`: through
    /// the bounds of a type parameter, or the impls of a concrete type.
    /// Reports ambiguity (and returns `Err`).
    pub(crate) fn find_trait_method(&mut self, ty: &Type, name: &str, span: Span) -> Result<Option<(TraitRef, u32)>, ()> {
        let ty = self.resolve(ty);
        let has = |c: &Checker, t: TraitId| c.traits[t as usize].methods.iter().position(|m| m.name == name).map(|i| i as u32);
        let mut found: Vec<(TraitRef, u32)> = Vec::new();
        match &ty {
            Type::Param(p) => {
                for tr in self.bound_traits(*p) {
                    if let Some(slot) = has(self, tr.trait_id) {
                        found.push((tr, slot));
                    }
                }
            }
            _ => {
                let Some(key) = impl_key(&ty) else { return Ok(None) };
                let mut ids: Vec<TraitId> = self
                    .impls_by_ctor
                    .get(&key)
                    .map(|v| v.iter().map(|&i| self.impls[i as usize].trait_ref.trait_id).collect())
                    .unwrap_or_default();
                ids.sort_unstable();
                ids.dedup();
                for t in ids {
                    let visible = self.traits[t as usize].is_pub || self.traits[t as usize].module == self.cur_mod;
                    if visible && let Some(slot) = has(self, t) {
                        let tr = self.fresh_trait_ref(t);
                        found.push((tr, slot));
                    }
                }
                // The method (and its impls) may be all that an import is used for.
                if let [(tr, _)] = found.as_slice() {
                    let mut ms = vec![self.traits[tr.trait_id as usize].module];
                    let ids = self.impls_by.get(&(tr.trait_id, key.0, key.1)).cloned().unwrap_or_default();
                    ms.extend(ids.iter().map(|&i| self.impls[i as usize].module));
                    for m in ms {
                        self.mark_import_used(m);
                    }
                }
            }
        }
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => {
                let names: Vec<String> = found.iter().map(|(tr, _)| format!("`{}`", self.show_trait(tr))).collect();
                self.error(
                    span,
                    format!(
                        "method `{name}` is ambiguous: it is provided by traits {}; call it as `Trait.{name}(...)` to choose one",
                        names.join(" and ")
                    ),
                );
                Err(())
            }
        }
    }

    /// Whether `ty` has trait method `name` (without reporting anything).
    pub(crate) fn has_trait_method(&self, ty: &Type, name: &str) -> bool {
        let has = |t: TraitId| self.traits[t as usize].methods.iter().any(|m| m.name == name);
        match self.resolve(ty) {
            Type::Param(p) => self.bound_traits(p).iter().any(|tr| has(tr.trait_id)),
            t => impl_key(&t)
                .and_then(|k| self.impls_by_ctor.get(&k))
                .is_some_and(|ids| ids.iter().any(|&i| has(self.impls[i as usize].trait_ref.trait_id))),
        }
    }

    /// Whether `ty` implements trait `t` (through a bound of a type parameter
    /// or an impl for its constructor), whatever the trait's arguments.
    pub(crate) fn implements_trait(&self, ty: &Type, t: TraitId) -> bool {
        match self.resolve(ty) {
            Type::Param(p) => self.bound_traits(p).iter().any(|tr| tr.trait_id == t),
            ty => impl_key(&ty).and_then(|(c, n)| self.impls_by.get(&(t, c, n))).is_some_and(|ids| !ids.is_empty()),
        }
    }

    /// A visible trait declaring method `name` (for suggestions).
    pub(crate) fn trait_with_method(&self, name: &str) -> Option<String> {
        self.traits
            .iter()
            .find(|t| (t.is_pub || t.module == self.cur_mod) && t.methods.iter().any(|m| m.name == name))
            .map(|t| t.name.clone())
    }

    /// Marks the imports of the current module that bind module `m` as used.
    fn mark_import_used(&mut self, m: ModId) {
        let cur = self.cur_mod;
        let names: Vec<String> = self.mods[cur as usize]
            .scope
            .iter()
            .filter(|(_, (d, _, _))| matches!(d, Def::Module(x) if *x == m))
            .map(|(n, _)| n.clone())
            .collect();
        for n in names {
            self.lint.used_names.insert((cur, n));
        }
    }

    /// The slot of method `name` in trait `t`, reporting an error if absent.
    pub(crate) fn trait_method_slot(&mut self, t: TraitId, name: &str, span: Span) -> Option<u32> {
        let td = &self.traits[t as usize];
        match td.methods.iter().position(|m| m.name == name) {
            Some(i) => Some(i as u32),
            None => {
                let msg = format!("trait `{}` has no method `{name}`", td.name);
                self.error(span, msg);
                None
            }
        }
    }

    pub(crate) fn fresh_trait_ref(&mut self, t: TraitId) -> TraitRef {
        let n = self.traits[t as usize].generics.len();
        TraitRef { trait_id: t, args: (0..n).map(|_| self.fresh()).collect() }
    }

    fn trait_pre(&self, tr: &TraitRef, self_ty: Type) -> Vec<(ParamId, Type)> {
        let td = &self.traits[tr.trait_id as usize];
        let mut pre = vec![(td.self_param, self_ty)];
        pre.extend(td.generics.iter().copied().zip(tr.args.iter().cloned()));
        pre
    }

    /// Calls trait method `slot` with `Self = self_ty`.
    pub(crate) fn call_trait_method(
        &mut self,
        tr: TraitRef,
        slot: u32,
        self_ty: Type,
        recv: Option<HExpr>,
        args: &[ast::Arg],
        span: Span,
    ) -> HExpr {
        let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
        let pre = self.trait_pre(&tr, self_ty);
        self.call_fn_with(sig, recv, args, span, &pre)
    }

    /// A trait method used as a function value (`Point.describe`, `Describe.describe`).
    pub(crate) fn trait_method_value(&mut self, tr: TraitRef, slot: u32, self_ty: Type, span: Span) -> HExpr {
        let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
        let pre = self.trait_pre(&tr, self_ty.clone());
        let mut h = self.wrapper_value(sig, &pre, span);
        // Prefer the doc comment of the impl's method when the impl is known.
        let rt = self.resolve(&self_ty);
        if let Some(key) = impl_key(&rt)
            && let Some([imp]) = self.impls_by.get(&(tr.trait_id, key.0, key.1)).map(|v| v.as_slice())
            && let Some(doc) = self.fns[self.impls[*imp as usize].slots[slot as usize] as usize].doc.clone()
            && let HKind::Lambda(l) = &mut h.kind
        {
            l.doc = Some(doc);
        }
        h
    }

    /// A function value for `f` that supplies its dictionaries: a closure
    /// taking the declared parameters and calling `f`.
    pub(crate) fn wrapper_value(&mut self, f: FnId, pre: &[(ParamId, Type)], span: Span) -> HExpr {
        let inst = self.instantiate_fn_with(f, span, pre);
        let fi = self.fns[f as usize].clone();
        let id = self.new_lambda();
        let mut args: Vec<HArg> = Vec::new();
        for (t, tr) in inst.dicts {
            let lenient = matches!(self.resolve(&t), Type::Var(_)) && inst.params.iter().any(|p| self.occurs_in(&t, p));
            let mut hole = self.evidence_hole(t, tr, span);
            if let HKind::Evidence(b) = &mut hole.kind {
                b.lenient = lenient;
            }
            args.push(HArg::Expr(hole));
        }
        let mut params = Vec::new();
        for t in &inst.params {
            self.fs.locals.push(LocalInfo { name: "<arg>".into(), mutable: false, owner: id, cell: false, span });
            self.fs.data.push(LocalData { ty: t.clone(), scheme: vec![], used: true, assigned: false, is_param: true, decl_span: span });
            let l = (self.fs.locals.len() - 1) as LocalId;
            params.push(l);
            args.push(HArg::Expr(mk(HKind::Local(l), t.clone(), span)));
        }
        let kind = match fi.kind {
            FnKind::TraitMethod(trait_id, slot) => HKind::CallTrait { trait_id, slot, args, impl_id: None },
            _ => HKind::Call { func: f, args },
        };
        let body = mk(kind, inst.ret.clone(), span);
        let lam =
            HLambda { id, name: fi.name.clone(), params, body, doc: fi.doc.clone(), signature: self.fn_signature(f), self_local: None };
        mk(HKind::Lambda(Box::new(lam)), Type::func(inst.params, inst.ret), span)
    }

    // ------------------------------------------------------------------
    // Evidence

    /// Declares the hidden dictionary parameters of the function being
    /// checked and records the evidence they (and impl dictionaries) provide.
    pub(crate) fn declare_dict_params(&mut self, fi: &FnInfo) -> Vec<LocalId> {
        let mut out = Vec::new();
        for (t, tr) in &fi.dicts {
            let l = self.declare_local("<dict>", Type::Unit, false, fi.span, true);
            self.fs.data[l as usize].used = true;
            out.push(l);
            self.fs.evidence.push((t.clone(), tr.clone(), dict_expr(HKind::Local(l), fi.span)));
        }
        if let (Some(impl_id), Some(&d0)) = (fi.impl_of, out.first()) {
            let imp = self.impls[impl_id as usize].clone();
            let td = &self.traits[imp.trait_ref.trait_id as usize];
            let base = td.methods.len() + td.supers.len();
            for (j, (ty, tr)) in imp.evidence.iter().enumerate() {
                let obj = Box::new(dict_expr(HKind::Local(d0), fi.span));
                let e = dict_expr(HKind::Field { obj, index: (base + j) as u32 }, fi.span);
                self.fs.evidence.push((ty.clone(), tr.clone(), e));
            }
        }
        out
    }

    /// A placeholder for the evidence that `ty` implements `tr`.
    pub(crate) fn evidence_hole(&mut self, ty: Type, tr: TraitRef, span: Span) -> HExpr {
        self.improve_evidence(&ty, &tr);
        self.fs.holes = true;
        dict_expr(HKind::Evidence(Box::new(EvidenceReq { ty, tr, lenient: false })), span)
    }

    /// If only one impl or bound can provide the evidence, uses it to infer
    /// the trait arguments (and type arguments) early.
    fn improve_evidence(&mut self, ty: &Type, tr: &TraitRef) {
        let t = self.resolve(ty);
        match &t {
            Type::Var(_) | Type::Error | Type::Never => {}
            Type::Param(_) => {
                if tr.args.is_empty() {
                    return;
                }
                let mut cands: Vec<TraitRef> = Vec::new();
                for f in self.fact_traits(&t) {
                    if f.trait_id == tr.trait_id && !cands.contains(&f) {
                        cands.push(f);
                    }
                }
                if let [only] = cands.as_slice() {
                    let pairs: Vec<(Type, Type)> = only.args.iter().cloned().zip(tr.args.iter().cloned()).collect();
                    self.try_unify_all(&pairs);
                }
            }
            _ => {
                let Some(key) = impl_key(&t) else { return };
                if let Some(&[id]) = self.impls_by.get(&(tr.trait_id, key.0, key.1)).map(|v| v.as_slice()) {
                    let inst = self.instantiate_impl(id);
                    let mut pairs = vec![(inst.target, t.clone())];
                    pairs.extend(inst.args.into_iter().zip(tr.args.iter().cloned()));
                    self.try_unify_all(&pairs);
                }
            }
        }
    }

    /// Traits that the evidence of the current function provides for `ty`.
    fn fact_traits(&self, ty: &Type) -> Vec<TraitRef> {
        let direct = self.fs.evidence.iter().filter(|(t, _, _)| t == ty).map(|(_, tr, _)| tr.clone()).collect();
        self.with_supers(ty, direct)
    }

    /// Replaces the evidence placeholders of a finished function body.
    pub(crate) fn resolve_holes(&mut self, body: &mut HExpr) {
        if !self.fs.holes {
            return;
        }
        // Resolving a placeholder can determine the type of another one (that
        // may come earlier), so retry until nothing changes before reporting.
        loop {
            let (mut progress, mut pending) = (false, false);
            body.visit_mut(&mut |e| {
                if let HKind::Evidence(b) = &e.kind {
                    let (ty, tr) = (b.ty.clone(), b.tr.clone());
                    match self.resolve_evidence(&ty, &tr, 0) {
                        Ok(x) => {
                            *e = HExpr { span: e.span, ..x };
                            progress = true;
                        }
                        Err(_) => pending = true,
                    }
                }
            });
            if !pending || !progress {
                break;
            }
        }
        body.visit_mut(&mut |e| {
            if let HKind::Evidence(b) = &e.kind {
                let EvidenceReq { ty, tr, lenient } = (**b).clone();
                let span = e.span;
                if lenient && matches!(self.resolve(&ty), Type::Var(_)) {
                    *e = dict_expr(HKind::Unit, span);
                    return;
                }
                *e = match self.resolve_evidence(&ty, &tr, 0) {
                    Ok(x) => HExpr { span, ..x },
                    Err(msg) => {
                        // A type that is not `Num` is reported as an unsatisfied
                        // built-in bound; only a type left unknown is reported here.
                        if Some(tr.trait_id) != self.num_trait || matches!(self.resolve(&ty), Type::Var(_)) {
                            self.error(span, msg);
                        }
                        mk(HKind::Error, Type::Unit, span)
                    }
                };
            }
        });
        // Note statically known impls (before dictionaries move to locals).
        body.visit_mut(&mut |e| {
            if let HKind::CallTrait { args, impl_id, .. } = &mut e.kind
                && let Some(HArg::Expr(HExpr { kind: HKind::Dict { impl_id: id, .. }, .. })) = args.first()
            {
                *impl_id = Some(*id);
            }
        });
        // Dictionaries that depend on the function's own dictionaries and are
        // used in loops or closures are built once, on entry.
        if self.fs.evidence.is_empty() {
            return;
        }
        let mut hoisted: Vec<(String, LocalId, HExpr)> = Vec::new();
        body.visit_mut(&mut |e| {
            if matches!(
                e.kind,
                HKind::Loop(_)
                    | HKind::While { .. }
                    | HKind::WhileLet { .. }
                    | HKind::For { .. }
                    | HKind::ForRange { .. }
                    | HKind::Lambda(_)
            ) {
                e.visit_mut(&mut |x| {
                    if matches!(x.kind, HKind::Dict { .. }) && !is_const_dict(x) {
                        self.hoist_dict(x, &mut hoisted);
                    }
                });
            }
        });
        if !hoisted.is_empty() {
            let span = body.span;
            let ty = body.ty.clone();
            let mut stmts: Vec<HExpr> = hoisted
                .into_iter()
                .map(|(_, l, init)| {
                    let pat = HPat { kind: HPatKind::Bind(l), ty: Type::Unit, span };
                    mk(HKind::Let { pat, init: Box::new(init), else_: None }, Type::Unit, span)
                })
                .collect();
            stmts.push(std::mem::replace(body, mk(HKind::Unit, Type::Unit, span)));
            *body = mk(HKind::Block(stmts), ty, span);
        }
    }

    /// Moves a dictionary (and the non-constant dictionaries in it) to hidden
    /// locals initialized on entry, sharing identical ones.
    fn hoist_dict(&mut self, e: &mut HExpr, hoisted: &mut Vec<(String, LocalId, HExpr)>) {
        if let HKind::Dict { args, .. } = &mut e.kind {
            for a in args.iter_mut() {
                if matches!(a.kind, HKind::Dict { .. }) && !is_const_dict(a) {
                    self.hoist_dict(a, hoisted);
                }
            }
        }
        let key = dict_key(e);
        let l = match hoisted.iter().find(|(k, _, _)| *k == key) {
            Some((_, l, _)) => *l,
            None => {
                self.fs.locals.push(LocalInfo { name: "<dict>".into(), mutable: false, owner: 0, cell: false, span: e.span });
                let data = LocalData { ty: Type::Unit, scheme: vec![], used: true, assigned: false, is_param: false, decl_span: e.span };
                self.fs.data.push(data);
                let l = (self.fs.locals.len() - 1) as LocalId;
                hoisted.push((key, l, e.clone()));
                l
            }
        };
        *e = dict_expr(HKind::Local(l), e.span);
    }

    /// The dictionary expression proving `ty: tr` in the current function.
    pub(crate) fn resolve_evidence(&mut self, ty: &Type, tr: &TraitRef, depth: u32) -> Result<HExpr, String> {
        let ty = self.zonk(ty);
        let tr = TraitRef { trait_id: tr.trait_id, args: tr.args.iter().map(|t| self.zonk(t)).collect() };
        let span = Span::default();
        if depth > 32 {
            return Err(format!("overflow while checking that {} implements `{}`", self.show(&ty), self.show_trait(&tr)));
        }
        // A type without type parameters is served by its impl, whose
        // dictionary is constant (so calls on it are direct).
        let ground = !has_params(&ty);
        if !ground && let Some(e) = self.find_fact(&ty, &tr) {
            return Ok(e);
        }
        match &ty {
            Type::Error => return Ok(dict_expr(HKind::Error, span)),
            // A value of type `never` is never produced, so its methods never run.
            Type::Never => return Ok(dict_expr(HKind::Unit, span)),
            Type::Var(_) => {
                return Err(format!("cannot infer the type that must implement `{}`; add a type annotation", self.show_trait(&tr)));
            }
            Type::Param(p) => {
                let n = self.params[*p as usize].name.clone();
                let t = self.show_trait(&tr);
                return Err(format!("type parameter `{n}` does not implement `{t}`; add a bound `{n}: {t}`"));
            }
            _ => {}
        }
        match self.resolve_by_impl(&ty, &tr, depth) {
            Err(_) if ground && let Some(e) = self.find_fact(&ty, &tr) => Ok(e),
            r => r,
        }
    }

    /// Evidence for `ty: tr` (zonked) from the impl for the type's constructor.
    fn resolve_by_impl(&mut self, ty: &Type, tr: &TraitRef, depth: u32) -> Result<HExpr, String> {
        let span = Span::default();
        let (ty, tr) = (ty.clone(), tr.clone());
        let not_impl = |c: &Checker| format!("the trait `{}` is not implemented for {}", c.show_trait(&tr), c.show(&ty));
        let Some(key) = impl_key(&ty) else { return Err(not_impl(self)) };
        let ids = self.impls_by.get(&(tr.trait_id, key.0, key.1)).cloned().unwrap_or_default();
        let mut matching = Vec::new();
        for id in ids {
            let inst = self.instantiate_impl(id);
            let mut pairs = vec![(inst.target, ty.clone())];
            pairs.extend(inst.args.into_iter().zip(tr.args.iter().cloned()));
            if self.unify_probe(&pairs) {
                matching.push(id);
            }
        }
        if matching.len() > 1 {
            return Err(format!(
                "cannot infer the type arguments of `{}` for {}: it has several impls; add a type annotation",
                self.show_trait(&tr),
                self.show(&ty)
            ));
        }
        if let Some(id) = matching.pop() {
            let inst = self.instantiate_impl(id);
            let mut pairs = vec![(inst.target, ty.clone())];
            pairs.extend(inst.args.into_iter().zip(tr.args.iter().cloned()));
            self.try_unify_all(&pairs);
            let context = |c: &Checker, m: String| {
                if m.contains("(required") { m } else { format!("{m} (required for {} to implement `{}`)", c.show(&ty), c.show_trait(&tr)) }
            };
            for (v, b) in inst.builtin {
                match self.satisfies(&v, b) {
                    Some(false) => return Err(context(self, format!("{} does not satisfy {}", self.show(&v), b.name()))),
                    None => self.fs.obligations.push((v, b, span)),
                    Some(true) => {}
                }
            }
            let mut args = Vec::new();
            for s in self.supers_of(&ty, &tr) {
                args.push(self.resolve_evidence(&ty, &s, depth + 1).map_err(|m| context(self, m))?);
            }
            for (et, etr) in inst.evidence {
                args.push(self.resolve_evidence(&et, &etr, depth + 1).map_err(|m| context(self, m))?);
            }
            return Ok(dict_expr(HKind::Dict { impl_id: id, args }, span));
        }
        Err(not_impl(self))
    }

    /// Evidence from the function's dictionaries (and their supertraits).
    fn find_fact(&self, ty: &Type, tr: &TraitRef) -> Option<HExpr> {
        let mut queue: Vec<(TraitRef, HExpr)> =
            self.fs.evidence.iter().filter(|(t, _, _)| t == ty).map(|(_, f, e)| (f.clone(), e.clone())).collect();
        let mut steps = 0;
        while !queue.is_empty() && steps < 1000 {
            let (ftr, e) = queue.remove(0);
            steps += 1;
            if ftr == *tr {
                return Some(e);
            }
            let n = self.traits[ftr.trait_id as usize].methods.len();
            for (i, s) in self.supers_of(ty, &ftr).into_iter().enumerate() {
                let obj = Box::new(e.clone());
                queue.push((s, dict_expr(HKind::Field { obj, index: (n + i) as u32 }, e.span)));
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // Tentative unification

    /// Unifies all pairs, or none (undoing partial bindings on failure).
    pub(crate) fn try_unify_all(&mut self, pairs: &[(Type, Type)]) -> bool {
        let outer = self.unify_log.replace(Vec::new());
        let ok = pairs.iter().all(|(a, b)| self.unify(a, b));
        let log = std::mem::replace(&mut self.unify_log, outer).unwrap_or_default();
        if ok {
            if let Some(o) = &mut self.unify_log {
                o.extend(log);
            }
        } else {
            for v in log {
                self.subst[v as usize] = None;
            }
        }
        ok
    }

    /// Whether all pairs unify, without keeping any bindings.
    fn unify_probe(&mut self, pairs: &[(Type, Type)]) -> bool {
        let outer = self.unify_log.replace(Vec::new());
        let ok = pairs.iter().all(|(a, b)| self.unify(a, b));
        for v in std::mem::replace(&mut self.unify_log, outer).unwrap_or_default() {
            self.subst[v as usize] = None;
        }
        ok
    }
}
