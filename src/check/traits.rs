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
    pub name: Symbol,
    /// The signature ([`FnKind::TraitMethod`]).
    pub sig: FnId,
    pub default: Option<FnId>,
}

#[derive(Clone, Debug)]
pub struct TraitDef {
    pub name: Symbol,
    pub module: ModId,
    pub is_pub: bool,
    pub doc: Option<Arc<str>>,
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

impl TraitDef {
    pub fn slot(&self, name: Symbol) -> Option<u32> {
        self.methods.iter().position(|m| m.name == name).map(|i| i as u32)
    }
}

#[derive(Clone, Debug)]
pub struct ImplDef {
    pub trait_ref: TraitRef,
    pub target: TypeId,
    pub generics: Vec<ParamId>,
    pub module: ModId,
    pub span: Span,
    /// The function for each method of the trait (impl method or default).
    pub slots: Vec<FnId>,
    /// Trait bounds of the impl's type parameters, whose dictionaries follow
    /// the supertrait dictionaries in the impl's dictionary.
    pub evidence: Vec<(TypeId, TraitRef)>,
}

/// An impl instantiated with fresh type variables.
struct ImplInst {
    target: TypeId,
    args: TyList,
    evidence: Vec<(TypeId, TraitRef)>,
    builtin: Vec<(TypeId, Bound)>,
}

/// The key under which impls are registered: type constructor and, for
/// tuples, the arity (tuples of different sizes are different constructors).
pub(crate) fn impl_key(arena: &TypeArena, t: TypeId) -> Option<(Ctor, u32)> {
    match arena.get(t) {
        Ty::Fn(..) => None,
        Ty::Tuple(l) => Some((Ctor::Tuple, arena.list_len(l) as u32)),
        ty => ctor_of(ty).map(|c| (c, 0)),
    }
}

fn dict_expr(kind: HKind, span: Span) -> HExpr {
    mk(kind, TypeId::UNIT, span)
}

/// Whether a dictionary expression is built from impls only (a constant).
pub(crate) fn is_const_dict(e: &HExpr) -> bool {
    match &e.kind {
        HKind::Dict { args, .. } => args.iter().all(is_const_dict),
        _ => false,
    }
}

/// Whether two dictionary expressions build the same dictionary.
fn same_dict(a: &HExpr, b: &HExpr) -> bool {
    match (&a.kind, &b.kind) {
        (HKind::Dict { impl_id: i, args: xs }, HKind::Dict { impl_id: j, args: ys }) => {
            i == j && xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same_dict(x, y))
        }
        (HKind::Local(l), HKind::Local(m)) => l == m,
        (HKind::Field { obj: a, index: i }, HKind::Field { obj: b, index: j }) => i == j && same_dict(a, b),
        (HKind::Unit, HKind::Unit) => true,
        _ => false,
    }
}

impl Checker<'_> {
    pub fn show_trait(&self, tr: &TraitRef) -> String {
        let mut s = self.name(self.traits[tr.trait_id as usize].name).to_string();
        let args = self.arena.list(tr.args);
        if !args.is_empty() {
            let args: Vec<String> = args.iter().map(|&t| self.show(t)).collect();
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
        scope: &[(Symbol, ParamId)],
        self_ty: Option<TypeId>,
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
            let args: Vec<TypeId> = args.iter().map(|t| self.resolve_type_in(t, scope, self_ty)).collect();
            let n = self.traits[tid as usize].generics.len();
            if args.len() != n {
                let plural = if n == 1 { "" } else { "s" };
                let name = self.name(self.traits[tid as usize].name).to_string();
                self.error(b.span, format!("expected {n} type argument{plural} for trait `{name}`, found {}", args.len()));
                continue;
            }
            let args = self.arena.intern_list(&args);
            traits.push(TraitRef { trait_id: tid, args });
        }
        (bounds, traits)
    }

    /// Resolves `Trait` or `module.Trait`.
    fn resolve_trait_path(&mut self, path: &[ast::Ident], span: Span) -> Option<TraitId> {
        let name = || path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
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
                self.error(span, format!("`{}` is a type, not a trait", name()));
                None
            }
            _ => {
                self.error(span, format!("unknown bound `{}`", name()));
                None
            }
        }
    }

    /// Adds bounds to a type parameter, with the built-in bounds its traits
    /// imply. A `Num` bound also adds the trait that carries its evidence.
    pub(crate) fn add_bounds(&mut self, p: ParamId, mut bounds: Vec<Bound>, mut traits: Vec<TraitRef>) {
        // A trait with `Num` as a supertrait carries its evidence itself.
        if let Some(n) = self.num_trait.filter(|_| bounds.contains(&Bound::Num)) {
            traits.push(TraitRef { trait_id: n, args: TyList::EMPTY });
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
    pub(crate) fn apply_where<'w>(
        &mut self,
        preds: impl IntoIterator<Item = &'w ast::WherePredicate>,
        scope: &[(Symbol, ParamId)],
        self_ty: Option<TypeId>,
    ) {
        for w in preds {
            let (bounds, traits) = self.resolve_bounds(&w.bounds, scope, self_ty);
            let sym = self.syms.get(&w.name.name);
            match scope.iter().rev().find(|(n, _)| Some(*n) == sym) {
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
    fn supers_of(&mut self, ty: TypeId, tr: TraitRef) -> Vec<TraitRef> {
        let t = &self.traits[tr.trait_id as usize];
        if t.supers.is_empty() {
            return Vec::new();
        }
        let args = self.arena.list(tr.args).to_vec();
        let map = |p: ParamId| if p == t.self_param { Some(ty) } else { t.generics.iter().position(|&g| g == p).map(|i| args[i]) };
        let arena = &mut self.arena;
        t.supers.iter().map(|s| s.subst_params(arena, &map)).collect()
    }

    /// The traits a type parameter is known to implement (with supertraits).
    pub(crate) fn bound_traits(&mut self, p: ParamId) -> Vec<TraitRef> {
        let traits = self.params[p as usize].traits.clone();
        let pt = self.param_ty(p);
        self.with_supers(pt, traits)
    }

    /// `traits` of `ty` and all their supertraits, in breadth-first order.
    fn with_supers(&mut self, ty: TypeId, traits: Vec<TraitRef>) -> Vec<TraitRef> {
        let mut out: Vec<TraitRef> = Vec::new();
        let mut queue = std::collections::VecDeque::from(traits);
        while let Some(tr) = queue.pop_front() {
            if out.contains(&tr) {
                continue;
            }
            queue.extend(self.supers_of(ty, tr));
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
            let name = self.sym(&g.name.name);
            generics.push(self.new_param(name, vec![], false));
        }
        let self_param = self.new_param(Symbol::SELF_TYPE, vec![], false);
        let name = self.sym(&t.name.name);
        self.traits.push(TraitDef {
            name,
            module: self.cur_mod,
            is_pub: t.is_pub,
            doc: t.doc.as_deref().map(Arc::from),
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

    fn trait_scope(&self, t: TraitId) -> Vec<(Symbol, ParamId)> {
        self.generic_scope(&self.traits[t as usize].generics)
    }

    /// Resolves the supertraits of the traits declared by the current module.
    pub(crate) fn resolve_supertraits(&mut self, ids: &[TraitId], decls: &[(u32, &ast::TraitDecl)]) {
        for (&id, (_, d)) in ids.iter().zip(decls) {
            let scope = self.trait_scope(id);
            let self_ty = self.param_ty(self.traits[id as usize].self_param);
            let (bounds, mut supers) = self.resolve_bounds(&d.supers, &scope, Some(self_ty));
            // `Num` carries evidence (`zero`, `one`), so its trait is a real supertrait.
            if let Some(n) = self.num_trait.filter(|_| bounds.contains(&Bound::Num)) {
                supers.push(TraitRef { trait_id: n, args: TyList::EMPTY });
            }
            let td = &mut self.traits[id as usize];
            td.builtin_supers = bounds;
            td.supers = supers;
        }
        for (&id, (_, d)) in ids.iter().zip(decls) {
            if self.super_reaches(id, id) {
                self.error(d.name.span, format!("trait `{}` is its own supertrait", d.name.name));
                self.traits[id as usize].supers.clear();
            }
        }
        // Inside the trait, `Self` implements it.
        for &id in ids {
            let (generics, sp) = (self.traits[id as usize].generics.clone(), self.traits[id as usize].self_param);
            let args: Vec<TypeId> = generics.iter().map(|&p| self.param_ty(p)).collect();
            let tr = TraitRef { trait_id: id, args: self.arena.intern_list(&args) };
            let bounds = self.builtin_closure(id);
            let info = &mut self.params[sp as usize];
            info.traits = vec![tr];
            info.bounds = bounds;
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

    /// Declares the method signatures and default methods of trait `id`,
    /// declared by item `item`.
    pub(crate) fn declare_trait_methods(&mut self, id: TraitId, d: &ast::TraitDecl, item: u32) {
        let self_param = self.traits[id as usize].self_param;
        let mut outer = self.trait_scope(id);
        outer.push((Symbol::SELF_TYPE, self_param));
        let self_ty = self.param_ty(self_param);
        let rules = DeclRules { skip_where: &outer, defaults: DefaultRule::Const };
        let mut methods: Vec<TraitMethod> = Vec::new();
        for (j, m) in d.methods.iter().enumerate() {
            if methods.iter().any(|x| self.name(x.name) == m.name.name) {
                let msg = format!("method `{}` is already defined in trait `{}`", m.name.name, self.name(self.traits[id as usize].name));
                self.error(m.name.span, msg);
                continue;
            }
            for w in &m.where_clause {
                if outer.iter().any(|(n, _)| self.name(*n) == w.name.name) {
                    self.error(w.name.span, "`where` clauses in trait methods may only constrain the method's own type parameters");
                }
            }
            for dv in m.params.iter().filter_map(|p| p.default.as_ref()) {
                if !is_closed_const(dv) {
                    self.error(dv.span, "default values of trait method parameters must be constants");
                }
            }
            let slot = methods.len() as u32;
            let at = DeclPath { item, method: j as u32 };
            let sig = self.declare_fn(m, at, rules, None, &outer, Some(self_ty));
            let f = &mut self.fns[sig as usize];
            f.kind = FnKind::TraitMethod(id, slot);
            f.decl = None;
            f.trait_of = Some(id);
            let default = if m.body.is_some() {
                let mut fi = self.fns[sig as usize].clone();
                fi.kind = FnKind::Script;
                fi.decl = Some(FnDeclRef::Fn(at));
                Some(self.add_fn(fi))
            } else {
                None
            };
            let name = self.sym(&m.name.name);
            methods.push(TraitMethod { name, sig, default });
        }
        self.traits[id as usize].methods = methods;
    }

    // ------------------------------------------------------------------
    // Impls

    /// Declares an impl and its methods (item `item`), checking them against the trait.
    pub(crate) fn declare_impl(&mut self, d: &ast::ImplDecl, item: u32) -> Option<ImplId> {
        let span = d.trait_ref.span;
        let generics = self.declare_params(&d.generics, &[], None);
        let scope = self.generic_scope(&generics);
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
        let args: Vec<TypeId> = targs.iter().map(|t| self.resolve_type_in(t, &scope, None)).collect();
        let n = self.traits[tid as usize].generics.len();
        if args.len() != n {
            let plural = if n == 1 { "" } else { "s" };
            let tname = self.name(self.traits[tid as usize].name);
            let msg = format!("expected {n} type argument{plural} for trait `{tname}`, found {}", args.len());
            self.error(span, msg);
            return None;
        }
        let trait_ref = TraitRef { trait_id: tid, args: self.arena.intern_list(&args) };
        let target = self.resolve_type_in(&d.target, &scope, None);
        if target == TypeId::ERROR {
            return None;
        }
        let shown = |c: &Checker| format!("`{}` for {}", c.show_trait(&trait_ref), c.show(target));
        let Some((ctor, arity)) = impl_key(&self.arena, target).filter(|_| !matches!(self.ty(target), Ty::Param(_))) else {
            let msg = format!("cannot implement traits for {}; the type must have a type constructor", self.show(target));
            self.error(d.target.span, msg);
            return None;
        };
        for (g, &p) in d.generics.iter().zip(&generics) {
            if !self.arena.mentions(target, p) && !args.iter().any(|&a| self.arena.mentions(a, p)) {
                self.error(g.name.span, format!("type parameter `{}` is not used by the impl's type or trait arguments", g.name.name));
            }
        }
        // Orphan rule: built-in types count as declared by the prelude.
        let type_mod = match ctor {
            Ctor::Def(dd) => self.types[dd as usize].module,
            _ => self.prelude.unwrap_or(u32::MAX),
        };
        if self.traits[tid as usize].module != self.cur_mod && type_mod != self.cur_mod {
            let msg = format!("the impl of {} must be in the module that declares the trait or the type", shown(self));
            self.error(span, msg);
            return None;
        }
        // Coherence: at most one impl per trait (and trait arguments) per type constructor.
        let key = (tid, ctor, arity);
        for other in self.impls_by.get(&key).map(|v| v.to_vec()).unwrap_or_default() {
            if self.impls_overlap(trait_ref, &generics, other) {
                if self.impls[other as usize].module == self.cur_mod && self.defined_by_earlier_input(self.impls[other as usize].span) {
                    // An impl of an earlier session input is replaced (code
                    // already checked keeps using it).
                    remove_impl(&mut self.impls_by, &key, other);
                    remove_impl(&mut self.impls_by_ctor, &(ctor, arity), other);
                    continue;
                }
                let line = self.sm.file(self.impls[other as usize].span.file).line_col(self.impls[other as usize].span.lo).0;
                let msg = format!("conflicting impls of {}: it is already implemented (line {line})", shown(self));
                self.error(span, msg);
                return None;
            }
        }
        let id = self.impls.len() as ImplId;
        let mut evidence: Vec<(TypeId, TraitRef)> = Vec::new();
        for &p in &generics {
            let pt = self.param_ty(p);
            evidence.extend(self.params[p as usize].traits.iter().map(|&tr| (pt, tr)));
        }
        self.impls.push(ImplDef { trait_ref, target, generics: generics.clone(), module: self.cur_mod, span, slots: Vec::new(), evidence });
        add_impl(&mut self.impls_by, key, id);
        add_impl(&mut self.impls_by_ctor, (ctor, arity), id);

        let mut slots: Vec<Option<FnId>> = vec![None; self.traits[tid as usize].methods.len()];
        let rules = DeclRules { skip_where: &scope, defaults: DefaultRule::None };
        for (j, m) in d.methods.iter().enumerate() {
            let slot = self.syms.get(&m.name.name).and_then(|s| self.traits[tid as usize].slot(s));
            let Some(slot) = slot.map(|s| s as usize) else {
                let msg = format!("`{}` is not a method of trait `{}`", m.name.name, self.name(self.traits[tid as usize].name));
                self.error(m.name.span, msg);
                continue;
            };
            if slots[slot].is_some() {
                self.error(m.name.span, format!("method `{}` is already defined", m.name.name));
                continue;
            }
            for w in &m.where_clause {
                if scope.iter().any(|(n, _)| self.name(*n) == w.name.name) {
                    self.error(w.name.span, "impl methods cannot add bounds to the impl's type parameters; add them to the impl");
                }
            }
            for p in m.params.iter().filter(|p| p.default.is_some()) {
                self.error(p.name.span, "default values are declared by the trait, not the impl");
            }
            let at = DeclPath { item, method: j as u32 };
            let f = self.declare_fn(m, at, rules, Some(ctor), &scope, Some(target));
            self.fns[f as usize].impl_of = Some(id);
            self.check_impl_method(id, slot as u32, f, m);
            slots[slot] = Some(f);
        }
        let mut missing = Vec::new();
        let methods = &self.traits[tid as usize].methods;
        let slots: Vec<FnId> = slots
            .iter()
            .zip(methods)
            .map(|(s, m)| match (s, m.default) {
                (Some(f), _) => *f,
                (None, Some(d)) => d,
                (None, None) => {
                    missing.push(format!("`{}`", self.syms.str(m.name)));
                    m.sig
                }
            })
            .collect();
        if !missing.is_empty() {
            let plural = if missing.len() == 1 { "" } else { "s" };
            let msg = format!("missing method{plural} {} in the impl of {}", missing.join(", "), shown(self));
            self.error(span, msg);
        }
        self.impls[id as usize].slots = slots;
        Some(id)
    }

    /// Whether a new impl (with type parameters `generics`) has trait
    /// arguments that unify with those of impl `other`.
    fn impls_overlap(&mut self, tr: TraitRef, generics: &[ParamId], other: ImplId) -> bool {
        let fresh: Vec<TypeId> = generics.iter().map(|_| self.fresh()).collect();
        let mine = self.elems(tr.args);
        let mine: Vec<TypeId> = mine.into_iter().map(|t| self.arena.instantiate(t, generics, &fresh)).collect();
        let theirs = self.instantiate_impl(other).args;
        let theirs = self.elems(theirs);
        let pairs: Vec<(TypeId, TypeId)> = mine.into_iter().zip(theirs).collect();
        self.unify_probe(&pairs)
    }

    /// Checks an impl method's signature against the trait's and sets its dictionaries.
    fn check_impl_method(&mut self, id: ImplId, slot: u32, f: FnId, m: &ast::FnDecl) {
        let imp = self.impls[id as usize].clone();
        let td = &self.traits[imp.trait_ref.trait_id as usize];
        let (self_param, tgens, tname) = (td.self_param, td.generics.clone(), td.name);
        let sig = self.fns[td.methods[slot as usize].sig as usize].clone();
        let fi = self.fns[f as usize].clone();
        let sig_own = &sig.generics[tgens.len() + 1..];
        let fi_own = &fi.generics[imp.generics.len()..];
        let targs = self.elems(imp.trait_ref.args);
        let fi_own_tys: Vec<TypeId> = fi_own.iter().map(|&q| self.param_ty(q)).collect();
        let map = |p: ParamId| {
            if p == self_param {
                Some(imp.target)
            } else if let Some(i) = tgens.iter().position(|&g| g == p) {
                Some(targs[i])
            } else {
                sig_own.iter().position(|&g| g == p).and_then(|i| fi_own_tys.get(i).copied())
            }
        };
        let want_params: Vec<(Symbol, TypeId, bool)> =
            sig.params.iter().map(|p| (p.name, self.arena.subst_params(p.ty, &map), p.default.is_some())).collect();
        let want_ret = self.arena.subst_params(sig.ret, &map);
        let same = |a: TypeId, b: TypeId| a == b || a == TypeId::ERROR || b == TypeId::ERROR;
        let mut ok = sig.has_self == fi.has_self
            && sig_own.len() == fi_own.len()
            && want_params.len() == fi.params.len()
            && want_params.iter().zip(&fi.params).all(|((_, t, _), p)| same(*t, p.ty))
            && same(want_ret, fi.ret);
        if ok {
            for (&a, &b) in sig_own.iter().zip(fi_own) {
                let (pa, pb) = (self.params[a as usize].clone(), self.params[b as usize].clone());
                let ta: Vec<TraitRef> = pa.traits.iter().map(|t| t.subst_params(&mut self.arena, &map)).collect();
                let same_bounds = pa.bounds.iter().all(|x| pb.bounds.contains(x)) && pb.bounds.iter().all(|x| pa.bounds.contains(x));
                let same_traits = ta.iter().all(|x| pb.traits.contains(x)) && pb.traits.iter().all(|x| ta.contains(x));
                ok &= same_bounds && same_traits;
            }
        }
        // The impl's dictionary, then the method's own bounds in the trait's order.
        let mut dicts = vec![(imp.target, imp.trait_ref)];
        if ok {
            for &(t, tr) in &sig.dicts[1..] {
                let t = self.arena.subst_params(t, &map);
                dicts.push((t, tr.subst_params(&mut self.arena, &map)));
            }
        } else {
            let params: Vec<(&str, TypeId, bool)> = want_params.iter().map(|&(n, t, d)| (self.name(n), t, d)).collect();
            let want = self.printer().signature(self.name(sig.name), &params, want_ret);
            let found = self.fn_signature(f);
            let tname = self.name(tname);
            let msg = if want == found {
                format!("the type parameters of method `{}` have different bounds than in trait `{tname}`", m.name.name)
            } else {
                format!("method `{}` does not match its declaration in trait `{tname}`: expected `{want}`, found `{found}`", m.name.name)
            };
            self.error(m.name.span, msg);
        }
        self.fns[f as usize].dicts = dicts;
    }

    /// Checks that the type of an impl satisfies the trait's supertraits.
    pub(crate) fn check_impl_supertraits(&mut self, id: ImplId) {
        let imp = &self.impls[id as usize];
        let evidence = imp.evidence.iter().map(|&(t, tr)| (t, tr, dict_expr(HKind::Unit, imp.span))).collect();
        let (target, trait_ref, span) = (imp.target, imp.trait_ref, imp.span);
        let tid = trait_ref.trait_id;
        let name = self.name(self.traits[tid as usize].name).to_string();
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        for b in self.builtin_closure(tid) {
            if self.satisfies(target, b) == Some(false) {
                let msg = format!("{} does not satisfy {}, which trait `{name}` requires", self.show(target), b.name());
                self.error(span, msg);
            }
        }
        for s in self.supers_of(target, trait_ref) {
            // A missing `Num` was reported as a built-in bound above.
            if Some(s.trait_id) == self.num_trait {
                continue;
            }
            if let Err(m) = self.resolve_evidence(target, s, 0) {
                self.error(span, format!("{m}, which trait `{name}` requires"));
            }
        }
        self.fs.evidence = saved;
    }

    fn instantiate_impl(&mut self, id: ImplId) -> ImplInst {
        let fresh: Vec<TypeId> = (0..self.impls[id as usize].generics.len()).map(|_| self.fresh()).collect();
        let imp = self.impls[id as usize].clone();
        let builtin =
            imp.generics.iter().zip(&fresh).flat_map(|(&p, &v)| self.params[p as usize].bounds.iter().map(move |&b| (v, b))).collect();
        let map = |p: ParamId| imp.generics.iter().position(|&g| g == p).map(|i| fresh[i]);
        let target = self.arena.subst_params(imp.target, &map);
        let args = self.arena.map_list(imp.trait_ref.args, &mut |a, t| a.subst_params(t, &map));
        let evidence =
            imp.evidence.iter().map(|&(t, tr)| (self.arena.subst_params(t, &map), tr.subst_params(&mut self.arena, &map))).collect();
        ImplInst { target, args, evidence, builtin }
    }

    // ------------------------------------------------------------------
    // Method lookup

    /// Finds the trait providing method `name` for values of type `ty`: through
    /// the bounds of a type parameter, or the impls of a concrete type.
    /// Reports ambiguity (and returns `Err`).
    pub(crate) fn find_trait_method(&mut self, ty: TypeId, name: &str, span: Span) -> Result<Option<(TraitRef, u32)>, ()> {
        let ty = self.resolve(ty);
        let Some(sym) = self.syms.get(name) else { return Ok(None) };
        let mut found: Vec<(TraitRef, u32)> = Vec::new();
        match self.ty(ty) {
            Ty::Param(p) => {
                for tr in self.bound_traits(p) {
                    if let Some(slot) = self.traits[tr.trait_id as usize].slot(sym) {
                        found.push((tr, slot));
                    }
                }
            }
            _ => {
                let Some(key) = impl_key(&self.arena, ty) else { return Ok(None) };
                let mut ids: Vec<TraitId> = self
                    .impls_by_ctor
                    .get(&key)
                    .map(|v| v.iter().map(|&i| self.impls[i as usize].trait_ref.trait_id).collect())
                    .unwrap_or_default();
                ids.sort_unstable();
                ids.dedup();
                for t in ids {
                    let visible = self.traits[t as usize].is_pub || self.traits[t as usize].module == self.cur_mod;
                    if visible && let Some(slot) = self.traits[t as usize].slot(sym) {
                        let tr = self.fresh_trait_ref(t);
                        found.push((tr, slot));
                    }
                }
                // The method (and its impls) may be all that an import is used for.
                if let [(tr, _)] = found.as_slice() {
                    let mut ms = vec![self.traits[tr.trait_id as usize].module];
                    if let Some(ids) = self.impls_by.get(&(tr.trait_id, key.0, key.1)) {
                        ms.extend(ids.iter().map(|&i| self.impls[i as usize].module));
                    }
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
    pub(crate) fn has_trait_method(&mut self, ty: TypeId, name: &str) -> bool {
        let Some(sym) = self.syms.get(name) else { return false };
        let t = self.resolve(ty);
        match self.ty(t) {
            Ty::Param(p) => self.bound_traits(p).iter().any(|tr| self.traits[tr.trait_id as usize].slot(sym).is_some()),
            _ => impl_key(&self.arena, t).and_then(|k| self.impls_by_ctor.get(&k)).is_some_and(|ids| {
                ids.iter().any(|&i| self.traits[self.impls[i as usize].trait_ref.trait_id as usize].slot(sym).is_some())
            }),
        }
    }

    /// Whether `ty` implements trait `t` (through a bound of a type parameter
    /// or an impl for its constructor), whatever the trait's arguments.
    pub(crate) fn implements_trait(&mut self, ty: TypeId, t: TraitId) -> bool {
        let ty = self.resolve(ty);
        match self.ty(ty) {
            Ty::Param(p) => self.bound_traits(p).iter().any(|tr| tr.trait_id == t),
            _ => impl_key(&self.arena, ty).and_then(|(c, n)| self.impls_by.get(&(t, c, n))).is_some_and(|ids| !ids.is_empty()),
        }
    }

    /// A visible trait declaring method `name` (for suggestions).
    pub(crate) fn trait_with_method(&self, name: &str) -> Option<String> {
        let sym = self.syms.get(name)?;
        self.traits.iter().find(|t| (t.is_pub || t.module == self.cur_mod) && t.slot(sym).is_some()).map(|t| self.name(t.name).to_string())
    }

    /// Marks the imports of the current module that bind module `m` as used.
    fn mark_import_used(&mut self, m: ModId) {
        if !self.linting() {
            return;
        }
        let cur = self.cur_mod;
        let names: Vec<String> =
            self.mods[cur as usize].module_imports.iter().filter(|(x, _)| *x == m).map(|&(_, n)| self.name(n).to_string()).collect();
        for name in names {
            self.lint.used_names.insert((cur, name));
        }
    }

    /// The slot of method `name` in trait `t`, reporting an error if absent.
    pub(crate) fn trait_method_slot(&mut self, t: TraitId, name: &str, span: Span) -> Option<u32> {
        let td = &self.traits[t as usize];
        match self.syms.get(name).and_then(|s| td.slot(s)) {
            Some(i) => Some(i),
            None => {
                let msg = format!("trait `{}` has no method `{name}`", self.name(td.name));
                self.error(span, msg);
                None
            }
        }
    }

    pub(crate) fn fresh_trait_ref(&mut self, t: TraitId) -> TraitRef {
        let n = self.traits[t as usize].generics.len();
        let args: Vec<TypeId> = (0..n).map(|_| self.fresh()).collect();
        TraitRef { trait_id: t, args: self.arena.intern_list(&args) }
    }

    fn trait_pre(&self, tr: TraitRef, self_ty: TypeId) -> Vec<(ParamId, TypeId)> {
        let td = &self.traits[tr.trait_id as usize];
        let mut pre = vec![(td.self_param, self_ty)];
        pre.extend(td.generics.iter().copied().zip(self.arena.list(tr.args).iter().copied()));
        pre
    }

    /// Calls trait method `slot` with `Self = self_ty`.
    pub(crate) fn call_trait_method(
        &mut self,
        tr: TraitRef,
        slot: u32,
        self_ty: TypeId,
        recv: Option<HExpr>,
        args: &super::expr::CallArgs,
        span: Span,
    ) -> HExpr {
        let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
        let pre = self.trait_pre(tr, self_ty);
        self.call_fn_with(sig, recv, args, span, &pre)
    }

    /// A trait method used as a function value (`Point.describe`, `Describe.describe`).
    pub(crate) fn trait_method_value(&mut self, tr: TraitRef, slot: u32, self_ty: TypeId, span: Span) -> HExpr {
        let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
        let pre = self.trait_pre(tr, self_ty);
        self.wrapper_value(sig, &pre, span)
    }

    /// A function value for `f` that supplies its dictionaries: a closure
    /// taking the declared parameters and calling `f`.
    pub(crate) fn wrapper_value(&mut self, f: FnId, pre: &[(ParamId, TypeId)], span: Span) -> HExpr {
        let inst = self.instantiate_fn_with(f, span, pre);
        let id = self.new_lambda();
        let mut args: Vec<HArg> = Vec::new();
        for &(t, tr) in &inst.dicts {
            let lenient = matches!(self.rty(t), Ty::Var(_)) && inst.params.iter().any(|&p| self.occurs_in(t, p));
            args.push(HArg::Expr(self.evidence_hole(t, tr, lenient, span)));
        }
        let mut params = Vec::new();
        for &t in &inst.params {
            self.fs.locals.push(LocalInfo { name: Symbol::ARG, mutable: false, owner: id, cell: false, span });
            self.fs.data.push(LocalData { ty: t, scheme: vec![], used: true, assigned: false, is_param: true, decl_span: span });
            let l = (self.fs.locals.len() - 1) as LocalId;
            params.push(l);
            args.push(HArg::Expr(mk(HKind::Local(l), t, span)));
        }
        let fi = &self.fns[f as usize];
        let kind = match fi.kind {
            FnKind::TraitMethod(trait_id, slot) => HKind::CallTrait { trait_id, slot, args, impl_id: None },
            _ => HKind::Call { func: f, args },
        };
        let name = self.name(fi.name).to_string();
        let body = mk(kind, inst.ret, span);
        let lam = HLambda { id, name, params, body, self_local: None };
        let fty = self.fn_ty(&inst.params, inst.ret);
        mk(HKind::Lambda(Box::new(lam)), fty, span)
    }

    // ------------------------------------------------------------------
    // Evidence

    /// Declares the hidden dictionary parameters of the function being
    /// checked and records the evidence they (and impl dictionaries) provide.
    pub(crate) fn declare_dict_params(&mut self, f: FnId) -> Vec<LocalId> {
        let (span, impl_of) = (self.fns[f as usize].span, self.fns[f as usize].impl_of);
        let mut out = Vec::new();
        for k in 0..self.fns[f as usize].dicts.len() {
            let l = self.declare_local_sym(Symbol::DICT, TypeId::UNIT, false, span, true);
            self.fs.data[l as usize].used = true;
            out.push(l);
            let (t, tr) = self.fns[f as usize].dicts[k];
            self.fs.evidence.push((t, tr, dict_expr(HKind::Local(l), span)));
        }
        if let (Some(impl_id), Some(&d0)) = (impl_of, out.first()) {
            let imp = &self.impls[impl_id as usize];
            let td = &self.traits[imp.trait_ref.trait_id as usize];
            let base = td.methods.len() + td.supers.len();
            for (j, &(ty, tr)) in imp.evidence.iter().enumerate() {
                let obj = Box::new(dict_expr(HKind::Local(d0), span));
                let e = dict_expr(HKind::Field { obj, index: (base + j) as u32 }, span);
                self.fs.evidence.push((ty, tr, e));
            }
        }
        out
    }

    /// A placeholder for the evidence that `ty` implements `tr` (see
    /// [`EvidenceReq::lenient`]).
    pub(crate) fn evidence_hole(&mut self, ty: TypeId, tr: TraitRef, lenient: bool, span: Span) -> HExpr {
        self.improve_evidence(ty, tr);
        self.fs.holes.push(EvidenceReq { ty, tr, lenient, span });
        dict_expr(HKind::Evidence((self.fs.holes.len() - 1) as u32), span)
    }

    /// If only one impl or bound can provide the evidence, uses it to infer
    /// the trait arguments (and type arguments) early.
    fn improve_evidence(&mut self, ty: TypeId, tr: TraitRef) {
        let t = self.resolve(ty);
        match self.ty(t) {
            Ty::Var(_) | Ty::Error | Ty::Never => {}
            Ty::Param(_) => {
                if tr.args == TyList::EMPTY {
                    return;
                }
                let mut cands: Vec<TraitRef> = Vec::new();
                for f in self.fact_traits(t) {
                    if f.trait_id == tr.trait_id && !cands.contains(&f) {
                        cands.push(f);
                    }
                }
                if let [only] = cands.as_slice() {
                    let pairs: Vec<(TypeId, TypeId)> = self.arena.list(only.args).iter().copied().zip(self.elems(tr.args)).collect();
                    self.try_unify_all(&pairs);
                }
            }
            _ => {
                let Some(key) = impl_key(&self.arena, t) else { return };
                if let Some(&[id]) = self.impls_by.get(&(tr.trait_id, key.0, key.1)).map(|v| &v[..]) {
                    let inst = self.instantiate_impl(id);
                    let mut pairs = vec![(inst.target, t)];
                    pairs.extend(self.arena.list(inst.args).iter().copied().zip(self.elems(tr.args)));
                    self.try_unify_all(&pairs);
                }
            }
        }
    }

    /// Traits that the evidence of the current function provides for `ty`.
    fn fact_traits(&mut self, ty: TypeId) -> Vec<TraitRef> {
        let direct = self.fs.evidence.iter().filter(|(t, _, _)| *t == ty).map(|(_, tr, _)| *tr).collect();
        self.with_supers(ty, direct)
    }

    /// Resolves the evidence placeholders of the function being checked,
    /// reporting failures: the dictionary expression for each (in the order
    /// of `FnState::holes`), which the final walk of the body puts in place.
    pub(crate) fn resolve_holes(&mut self) -> Vec<HExpr> {
        let reqs = std::mem::take(&mut self.fs.holes);
        let mut found: Vec<Option<HExpr>> = vec![None; reqs.len()];
        // Resolving a placeholder can determine the type of another one (that
        // may come earlier), so retry until nothing changes before reporting.
        'retry: loop {
            let (mut progress, mut pending) = (false, false);
            for (r, slot) in reqs.iter().zip(&mut found).filter(|(_, s)| s.is_none()) {
                if !self.budget.charge(1) {
                    break 'retry;
                }
                match self.resolve_evidence(r.ty, r.tr, 0) {
                    Ok(x) => {
                        *slot = Some(x);
                        progress = true;
                    }
                    Err(_) => pending = true,
                }
            }
            if !pending || !progress {
                break;
            }
        }
        let mut out = Vec::with_capacity(reqs.len());
        for (r, slot) in reqs.iter().zip(found) {
            let e = match slot {
                Some(e) => e,
                None if r.lenient && matches!(self.rty(r.ty), Ty::Var(_)) => dict_expr(HKind::Unit, r.span),
                None => match self.resolve_evidence(r.ty, r.tr, 0) {
                    Ok(e) => e,
                    Err(msg) => {
                        // A type that is not `Num` is reported as an unsatisfied
                        // built-in bound; only a type left unknown is reported here.
                        if Some(r.tr.trait_id) != self.num_trait || matches!(self.rty(r.ty), Ty::Var(_)) {
                            self.error(r.span, msg);
                        }
                        mk(HKind::Error, TypeId::UNIT, r.span)
                    }
                },
            };
            out.push(e);
        }
        out
    }

    /// Moves a dictionary (and the non-constant dictionaries in it) to hidden
    /// locals initialized on entry, sharing identical ones.
    pub(crate) fn hoist_dict(&mut self, e: &mut HExpr, hoisted: &mut Vec<(LocalId, HExpr)>) {
        if let HKind::Dict { args, .. } = &mut e.kind {
            for a in args.iter_mut() {
                if matches!(a.kind, HKind::Dict { .. }) && !is_const_dict(a) {
                    self.hoist_dict(a, hoisted);
                }
            }
        }
        let l = match hoisted.iter().find(|(_, x)| same_dict(x, e)) {
            Some((l, _)) => *l,
            None => {
                self.fs.locals.push(LocalInfo { name: Symbol::DICT, mutable: false, owner: 0, cell: false, span: e.span });
                let data = LocalData { ty: TypeId::UNIT, scheme: vec![], used: true, assigned: false, is_param: false, decl_span: e.span };
                self.fs.data.push(data);
                let l = (self.fs.locals.len() - 1) as LocalId;
                hoisted.push((l, e.clone()));
                l
            }
        };
        *e = dict_expr(HKind::Local(l), e.span);
    }

    /// The dictionary expression proving `ty: tr` in the current function.
    pub(crate) fn resolve_evidence(&mut self, ty: TypeId, tr: TraitRef, depth: u32) -> Result<HExpr, String> {
        let ty = self.zonk(ty);
        let tr = self.zonk_trait(tr);
        let span = Span::default();
        if depth > 32 {
            return Err(format!("overflow while checking that {} implements `{}`", self.show(ty), self.show_trait(&tr)));
        }
        // A type without type parameters is served by its impl, whose
        // dictionary is constant (so calls on it are direct).
        let ground = !self.arena.has_params(ty);
        if !ground && let Some(e) = self.find_fact(ty, tr) {
            return Ok(e);
        }
        match self.ty(ty) {
            Ty::Error => return Ok(dict_expr(HKind::Error, span)),
            // A value of type `never` is never produced, so its methods never run.
            Ty::Never => return Ok(dict_expr(HKind::Unit, span)),
            Ty::Var(_) => {
                return Err(format!("cannot infer the type that must implement `{}`; add a type annotation", self.show_trait(&tr)));
            }
            Ty::Param(p) => {
                let n = self.name(self.params[p as usize].name).to_string();
                let t = self.show_trait(&tr);
                return Err(format!("type parameter `{n}` does not implement `{t}`; add a bound `{n}: {t}`"));
            }
            _ => {}
        }
        match self.resolve_by_impl(ty, tr, depth) {
            Err(_) if ground && let Some(e) = self.find_fact(ty, tr) => Ok(e),
            r => r,
        }
    }

    fn zonk_trait(&mut self, tr: TraitRef) -> TraitRef {
        let Checker { arena, subst, budget, .. } = self;
        TraitRef { trait_id: tr.trait_id, args: arena.map_list(tr.args, &mut |a, t| super::unify::zonk_in(a, subst, budget, t)) }
    }

    /// Evidence for `ty: tr` (zonked) from the impl for the type's constructor.
    fn resolve_by_impl(&mut self, ty: TypeId, tr: TraitRef, depth: u32) -> Result<HExpr, String> {
        let span = Span::default();
        let not_impl = |c: &Checker| format!("the trait `{}` is not implemented for {}", c.show_trait(&tr), c.show(ty));
        let Some(key) = impl_key(&self.arena, ty) else { return Err(not_impl(self)) };
        let ids = self.impls_by.get(&(tr.trait_id, key.0, key.1)).map(|v| v.to_vec()).unwrap_or_default();
        let targs = self.elems(tr.args);
        let mut matching = Vec::new();
        for id in ids {
            let inst = self.instantiate_impl(id);
            let mut pairs = vec![(inst.target, ty)];
            pairs.extend(self.arena.list(inst.args).iter().copied().zip(targs.iter().copied()));
            if self.unify_probe(&pairs) {
                matching.push(id);
            }
        }
        if matching.len() > 1 {
            return Err(format!(
                "cannot infer the type arguments of `{}` for {}: it has several impls; add a type annotation",
                self.show_trait(&tr),
                self.show(ty)
            ));
        }
        if let Some(id) = matching.pop() {
            let inst = self.instantiate_impl(id);
            let mut pairs = vec![(inst.target, ty)];
            pairs.extend(self.arena.list(inst.args).iter().copied().zip(targs.iter().copied()));
            self.try_unify_all(&pairs);
            let context = |c: &Checker, m: String| {
                if m.contains("(required") { m } else { format!("{m} (required for {} to implement `{}`)", c.show(ty), c.show_trait(&tr)) }
            };
            for (v, b) in inst.builtin {
                match self.satisfies(v, b) {
                    Some(false) => return Err(context(self, format!("{} does not satisfy {}", self.show(v), b.name()))),
                    None => self.fs.obligations.push((v, b, span)),
                    Some(true) => {}
                }
            }
            let mut args = Vec::new();
            for s in self.supers_of(ty, tr) {
                args.push(self.resolve_evidence(ty, s, depth + 1).map_err(|m| context(self, m))?);
            }
            for (et, etr) in inst.evidence {
                args.push(self.resolve_evidence(et, etr, depth + 1).map_err(|m| context(self, m))?);
            }
            return Ok(dict_expr(HKind::Dict { impl_id: id, args }, span));
        }
        Err(not_impl(self))
    }

    /// Evidence from the function's dictionaries (and their supertraits).
    fn find_fact(&mut self, ty: TypeId, tr: TraitRef) -> Option<HExpr> {
        // Breadth first over the facts about `ty` and their supertraits,
        // visiting each trait once. A node is (trait, parent node, field of
        // the parent's dictionary holding it), or (trait, None, fact index).
        let mut nodes: Vec<(TraitRef, Option<usize>, usize)> = Vec::new();
        for (k, (t, f, _)) in self.fs.evidence.iter().enumerate() {
            if *t == ty && !nodes.iter().any(|(x, _, _)| x == f) {
                nodes.push((*f, None, k));
            }
        }
        let mut next = 0;
        while next < nodes.len() {
            if !self.budget.charge(1) {
                return None;
            }
            let i = next;
            next += 1;
            if nodes[i].0 == tr {
                // The fact's dictionary, then the supertrait fields down to `tr`.
                let mut fields = Vec::new();
                let mut at = i;
                while let (_, Some(parent), field) = &nodes[at] {
                    fields.push(*field as u32);
                    at = *parent;
                }
                let mut e = self.fs.evidence[nodes[at].2].2.clone();
                for index in fields.into_iter().rev() {
                    let span = e.span;
                    e = dict_expr(HKind::Field { obj: Box::new(e), index }, span);
                }
                return Some(e);
            }
            let n = self.traits[nodes[i].0.trait_id as usize].methods.len();
            for (j, s) in self.supers_of(ty, nodes[i].0).into_iter().enumerate() {
                if !nodes.iter().any(|(x, _, _)| *x == s) {
                    nodes.push((s, Some(i), n + j));
                }
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // Tentative unification

    /// Unifies all pairs, or none (undoing partial bindings on failure).
    pub(crate) fn try_unify_all(&mut self, pairs: &[(TypeId, TypeId)]) -> bool {
        let outer = self.unify_log.replace(Vec::new());
        let ok = pairs.iter().all(|&(a, b)| self.unify(a, b));
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
    fn unify_probe(&mut self, pairs: &[(TypeId, TypeId)]) -> bool {
        let outer = self.unify_log.replace(Vec::new());
        let ok = pairs.iter().all(|&(a, b)| self.unify(a, b));
        for v in std::mem::replace(&mut self.unify_log, outer).unwrap_or_default() {
            self.subst[v as usize] = None;
        }
        ok
    }
}

fn add_impl<K: std::hash::Hash + Eq + Clone>(table: &mut PMap<K, Arc<[ImplId]>>, key: K, id: ImplId) {
    let mut ids: Vec<ImplId> = table.get(&key).map(|v| v.to_vec()).unwrap_or_default();
    ids.push(id);
    table.insert(key, ids.into());
}

fn remove_impl<K: std::hash::Hash + Eq + Clone>(table: &mut PMap<K, Arc<[ImplId]>>, key: &K, id: ImplId) {
    if let Some(ids) = table.get(key) {
        let kept: Vec<ImplId> = ids.iter().copied().filter(|&i| i != id).collect();
        table.insert(key.clone(), kept.into());
    }
}
