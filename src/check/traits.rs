//! Trait declarations, implementations, method resolution, and dictionary passing.
//!
//! Because generic values are type-erased at runtime, the compiler passes a
//! hidden *dictionary* parameter for each trait bound that declares methods
//! (`T: Describe`) before a function's explicit parameters. Each dictionary is
//! a tuple containing the trait's method closures (from the `impl` or trait
//! defaults), followed by the supertrait dictionaries for the same target type,
//! followed by the dictionaries required by the `impl` block's own type
//! parameter bounds. Trait method signatures, default methods, and `impl`
//! methods accept the `Self` dictionary as their first hidden parameter. A
//! trait without methods in itself or its supertraits does not have a runtime
//! dictionary ([`TraitDef::has_dict`]).
//!
//! A type satisfies a trait bound through a type parameter bound, an `impl`
//! block, or an implementation of `Eq`, `Ord`, `Hash`, or `Encode` derived from
//! the type's components (`derive.rs`).
//!
//! Generic calls emit [`HKind::Evidence`] placeholders during expression
//! checking. After type inference completes for the enclosing function, the
//! checker resolves each placeholder to a hidden dictionary parameter, a field
//! projection from a parameter dictionary, or an [`HKind::Dict`] constructor.

use super::expr::mk;
use super::*;

#[derive(Clone, Debug)]
pub struct TraitMethod {
    pub name: Symbol,
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
    pub generics: Vec<ParamId>,
    pub self_param: ParamId,
    /// Direct supertraits parameterized by `generics` and `self_param`.
    pub supers: Vec<TraitRef>,
    pub methods: Vec<TraitMethod>,
    pub builtin: Option<Builtin>,
    /// Indicates whether this trait or any of its supertraits declares methods.
    pub has_dict: bool,
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
    /// Functions implementing each method slot of the trait.
    pub slots: Vec<FnId>,
    /// Trait bounds required on the `impl` block's type parameters. Bounds that
    /// have runtime dictionaries are stored after supertrait dictionaries in
    /// the `impl` dictionary.
    pub evidence: Vec<(TypeId, TraitRef)>,
    /// Indicates whether a later REPL input replaced this `impl`.
    pub replaced: bool,
}

struct Requirement {
    owner: TypeId,
    arg: TypeId,
    tr: TraitRef,
}

struct ImplInst {
    target: TypeId,
    args: TyList,
    evidence: Vec<(TypeId, TraitRef)>,
}

/// Diagnostic details for an unsatisfied trait bound.
#[derive(Clone, Debug)]
pub(crate) struct Unmet {
    pub unknown: bool,
    pub head: String,
    pub because: Option<String>,
    pub required: Option<String>,
    pub hint: Option<String>,
    /// Records a failed derived trait check so [`Checker::explain`] can locate
    /// the failing component type if the diagnostic is emitted.
    pub explain: Option<(TypeId, TraitRef, Builtin)>,
}

impl Unmet {
    pub fn new(head: String) -> Box<Unmet> {
        Box::new(Unmet { unknown: false, head, because: None, required: None, hint: None, explain: None })
    }

    /// Formats the diagnostic message with `context` appended to the outermost
    /// requirement. Call [`Checker::explain`] before formatting.
    fn message(&self, context: &str) -> String {
        let because = self.because.as_ref().map(|b| format!(" because {b}")).unwrap_or_default();
        let mut m = match &self.required {
            Some(r) => format!("{}{because} (required for {r}){context}", self.head),
            None if because.is_empty() || context.is_empty() => format!("{}{context}{because}", self.head),
            None => format!("{}{context},{because}", self.head),
        };
        if let Some(h) = &self.hint {
            m.push_str(&format!("; {h}"));
        }
        m
    }
}

/// Returns the lookup key `(constructor, tuple_arity)` used to index `impl`
/// blocks for `t`, or `None` if `t` cannot implement traits.
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

/// Returns `true` if `e` is a constant dictionary constructed solely from
/// static `impl` dictionaries.
pub(crate) fn is_const_dict(e: &HExpr) -> bool {
    match &e.kind {
        HKind::Dict { args, .. } => args.iter().all(is_const_dict),
        _ => false,
    }
}

fn same_dict(a: &HExpr, b: &HExpr) -> bool {
    match (&a.kind, &b.kind) {
        (HKind::Dict { impl_id: i, args: xs }, HKind::Dict { impl_id: j, args: ys }) => {
            i == j && xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same_dict(x, y))
        }
        (HKind::Local(l), HKind::Local(m)) => l == m,
        (HKind::Field { obj: a, index: i }, HKind::Field { obj: b, index: j }) => i == j && same_dict(a, b),
        _ => false,
    }
}

impl Checker {
    pub(crate) fn unmet_message(&mut self, mut m: Box<Unmet>, context: &str) -> String {
        self.explain(&mut m);
        m.message(context)
    }

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

    /// Resolves trait bounds written after `:` (`T: Ord + geo.Shape`).
    pub(crate) fn resolve_traits(&mut self, ts: &[TypeExpr], scope: &[(Symbol, ParamId)], self_ty: Option<TypeId>) -> Vec<TraitRef> {
        let mut traits = Vec::new();
        for t in ts {
            let TypeExprKind::Named { path, args } = &t.kind else {
                self.error(t.span, "expected a trait");
                continue;
            };
            let Some(tid) = self.resolve_trait_path(path, t.span) else {
                continue;
            };
            let args: Vec<TypeId> = args.iter().map(|t| self.resolve_type_in(t, scope, self_ty)).collect();
            let n = self.traits[tid as usize].generics.len();
            if args.len() != n {
                let name = self.name(self.traits[tid as usize].name).to_string();
                let msg = match n {
                    0 => format!("`{name}` does not take type arguments"),
                    1 => format!("expected 1 type argument for trait `{name}`, found {}", args.len()),
                    _ => format!("expected {n} type arguments for trait `{name}`, found {}", args.len()),
                };
                self.error(t.span, msg);
                continue;
            }
            let args = self.arena.intern_list(&args);
            traits.push(TraitRef { trait_id: tid, args });
        }
        traits
    }

    fn resolve_trait_path(&mut self, path: &[ast::Ident], span: Span) -> Option<TraitId> {
        let name = || path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
        let def = match path {
            // Only traits can appear after `:`, so a module-local type (`struct Hash`)
            // does not shadow a prelude trait of the same name.
            [one] => match (self.lookup_global(&one.name), self.prelude) {
                (Some(Def::Type(_)), Some(p)) if p != self.cur_mod => match self.scope_def(p, &one.name) {
                    Some(Def::Trait(t)) if self.traits[t as usize].is_pub => Some(Def::Trait(t)),
                    _ => self.lookup_global(&one.name),
                },
                (def, _) => def,
            },
            [m, item] => match self.lookup_global(&m.name) {
                Some(Def::Module(m)) => Some(self.module_member(m, &item.name, item.span)?),
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
                self.error(span, format!("unknown trait `{}`", name()));
                None
            }
        }
    }

    pub(crate) fn add_traits(&mut self, p: ParamId, traits: Vec<TraitRef>) {
        let info = &mut self.params[p as usize];
        for tr in traits {
            if !info.traits.contains(&tr) {
                info.traits.push(tr);
            }
        }
    }

    pub(crate) fn apply_where<'w>(
        &mut self,
        preds: impl IntoIterator<Item = &'w ast::WherePredicate>,
        scope: &[(Symbol, ParamId)],
        self_ty: Option<TypeId>,
    ) {
        for w in preds {
            let traits = self.resolve_traits(&w.traits, scope, self_ty);
            let sym = self.syms.get(&w.name.name);
            match scope.iter().rev().find(|(n, _)| Some(*n) == sym) {
                Some((_, p)) => self.add_traits(*p, traits),
                None => self.error(w.name.span, format!("unknown type parameter `{}`", w.name.name)),
            }
        }
    }

    pub(crate) fn trait_as_type_error(&mut self, id: &ast::Ident) {
        self.error(id.span, format!("`{}` is a trait, not a type; use a type parameter that implements it (`<T: {}>`)", id.name, id.name));
    }

    pub(crate) fn builtin_trait(&self, b: Builtin) -> Option<TraitRef> {
        self.builtins[b as usize].map(|trait_id| TraitRef { trait_id, args: TyList::EMPTY })
    }

    fn trait_and_supers(&self, t: TraitId) -> Vec<TraitId> {
        let mut out = vec![t];
        let mut i = 0;
        while i < out.len() {
            for s in &self.traits[out[i] as usize].supers {
                if !out.contains(&s.trait_id) {
                    out.push(s.trait_id);
                }
            }
            i += 1;
        }
        out
    }

    /// Returns `true` if trait `t` equals `target` or has `target` as a
    /// supertrait.
    pub(crate) fn trait_reaches(&self, t: TraitId, target: TraitId) -> bool {
        self.trait_and_supers(t).contains(&target)
    }

    /// Returns `true` if type parameter `p` requires trait `t` directly or
    /// through a supertrait.
    pub(crate) fn param_has_trait(&self, p: ParamId, t: TraitId) -> bool {
        self.params[p as usize].traits.iter().any(|tr| self.trait_reaches(tr.trait_id, t))
    }

    /// Filters `traits` by removing duplicates and traits implied by other
    /// traits in the slice.
    pub(crate) fn minimal_traits(&self, traits: &[TraitRef]) -> Vec<TraitRef> {
        let mut out: Vec<TraitRef> = Vec::new();
        for (i, tr) in traits.iter().enumerate() {
            let implied = traits
                .iter()
                .enumerate()
                .any(|(j, x)| (x.trait_id != tr.trait_id && self.trait_reaches(x.trait_id, tr.trait_id)) || (x == tr && j < i));
            if !implied {
                out.push(*tr);
            }
        }
        out
    }

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

    pub(crate) fn param_traits(&mut self, p: ParamId) -> Vec<TraitRef> {
        let traits = self.params[p as usize].traits.clone();
        let pt = self.param_ty(p);
        self.with_supers(pt, traits)
    }

    fn with_supers(&mut self, ty: TypeId, traits: Vec<TraitRef>) -> Vec<TraitRef> {
        self.supertrait_closure(ty, traits).into_iter().map(|(tr, _)| tr).collect()
    }

    /// Returns `roots` and all of their transitive supertraits for `ty` in
    /// breadth-first order without duplicates. Each non-root entry stores the
    /// index of its parent entry and the dictionary field index that holds its
    /// supertrait dictionary.
    fn supertrait_closure(&mut self, ty: TypeId, roots: Vec<TraitRef>) -> Vec<(TraitRef, Option<(usize, u32)>)> {
        let mut out: Vec<(TraitRef, Option<(usize, u32)>)> = Vec::new();
        for r in roots {
            if !out.iter().any(|(x, _)| *x == r) {
                out.push((r, None));
            }
        }
        let mut i = 0;
        while i < out.len() && self.budget.charge(1) {
            let mut field = self.traits[out[i].0.trait_id as usize].methods.len() as u32;
            for s in self.supers_of(ty, out[i].0) {
                let link = Some((i, field));
                if self.has_dict(&s) {
                    field += 1;
                }
                if !out.iter().any(|(x, _)| *x == s) {
                    out.push((s, link));
                }
            }
            i += 1;
        }
        out
    }

    pub(crate) fn declare_trait(&mut self, t: &ast::TraitDecl) -> TraitId {
        let builtin = Builtin::parse(&t.name.name);
        let in_prelude = Some(self.cur_mod) == self.prelude;
        if builtin.is_some() && !in_prelude {
            self.error(t.name.span, format!("`{}` is a built-in trait and cannot be redefined", t.name.name));
        }
        let mut generics = Vec::new();
        for g in &t.generics {
            if !g.traits.is_empty() {
                self.error(g.name.span, "the type parameters of a trait cannot require traits; require them on the methods or impls");
            }
            let name = self.sym(&g.name.name);
            generics.push(self.new_param(name, false));
        }
        let self_param = self.new_param(Symbol::SELF_TYPE, false);
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
            methods: Vec::new(),
            builtin: builtin.filter(|_| in_prelude),
            has_dict: !t.methods.is_empty(),
        });
        let id = (self.traits.len() - 1) as TraitId;
        if let Some(b) = builtin.filter(|_| in_prelude) {
            self.builtins[b as usize] = Some(id);
        }
        id
    }

    fn trait_scope(&self, t: TraitId) -> Vec<(Symbol, ParamId)> {
        self.generic_scope(&self.traits[t as usize].generics)
    }

    pub(crate) fn resolve_supertraits(&mut self, ids: &[TraitId], decls: &[(u32, &ast::TraitDecl)]) {
        for (&id, (_, d)) in ids.iter().zip(decls) {
            let scope = self.trait_scope(id);
            let self_ty = self.param_ty(self.traits[id as usize].self_param);
            let supers = self.resolve_traits(&d.supers, &scope, Some(self_ty));
            self.traits[id as usize].supers = supers;
        }
        for (&id, (_, d)) in ids.iter().zip(decls) {
            if self.traits[id as usize].supers.iter().any(|s| self.trait_reaches(s.trait_id, id)) {
                self.error(d.name.span, format!("trait `{}` is its own supertrait", d.name.name));
                self.traits[id as usize].supers.clear();
            }
        }
        // Propagate `has_dict` from supertraits to subtraits.
        for &id in ids {
            self.traits[id as usize].has_dict = self.trait_and_supers(id).iter().any(|&t| self.traits[t as usize].has_dict);
        }
        for &id in ids {
            let (generics, sp) = (self.traits[id as usize].generics.clone(), self.traits[id as usize].self_param);
            let args: Vec<TypeId> = generics.iter().map(|&p| self.param_ty(p)).collect();
            let tr = TraitRef { trait_id: id, args: self.arena.intern_list(&args) };
            self.params[sp as usize].traits = vec![tr];
        }
    }

    pub(crate) fn declare_trait_methods(&mut self, id: TraitId, d: &ast::TraitDecl, item: u32) {
        let self_param = self.traits[id as usize].self_param;
        let mut outer = self.trait_scope(id);
        outer.push((Symbol::SELF_TYPE, self_param));
        let self_ty = self.param_ty(self_param);
        let rules = DeclRules { skip_where: &outer, defaults: DefaultRule::Const };
        let mut methods: Vec<TraitMethod> = Vec::new();
        for (j, m) in d.methods.iter().enumerate() {
            self.reject_reflect_params(m);
            if m.name.name == "init" {
                self.error(m.name.span, "a trait cannot declare `init`; only a type declares its `init`");
            }
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
        if self.traits[id as usize].builtin == Some(Builtin::Num)
            && !methods.iter().map(|m| self.name(m.name)).eq(crate::native::NUM_METHODS)
        {
            let names = crate::native::NUM_METHODS.map(|n| format!("`{n}`")).join(", ");
            self.error(
                d.name.span,
                format!("the `Num` trait must declare the methods {names} in this order, because `native::Num` reads them by position"),
            );
        }
        self.traits[id as usize].methods = methods;
    }

    // ------------------------------------------------------------------
    // Impls

    pub(crate) fn declare_impl(&mut self, d: &ast::ImplDecl, item: u32) -> Option<ImplId> {
        let span = d.trait_ref.span;
        let generics = self.declare_params(&d.generics, &[], None);
        let scope = self.generic_scope(&generics);
        self.apply_where(&d.where_clause, &scope, None);
        let TypeExprKind::Named { path, args: targs } = &d.trait_ref.kind else {
            self.error(span, "expected a trait");
            return None;
        };
        let tid = self.resolve_trait_path(path, span)?;
        if let Some(b) = self.traits[tid as usize].builtin
            && (b.is_derived() || Some(self.cur_mod) != self.prelude)
        {
            let msg = if b.is_derived() {
                format!("`{}` is a built-in trait; the compiler derives its impls, so it cannot be implemented", b.name())
            } else {
                format!("`{}` is a built-in trait and cannot be implemented", b.name())
            };
            self.error(span, msg);
            return None;
        }
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
        // Built-in types belong to the prelude for the orphan rule check.
        let type_mod = match ctor {
            Ctor::Def(dd) => self.types[dd as usize].module,
            _ => self.prelude.unwrap_or(u32::MAX),
        };
        if self.traits[tid as usize].module != self.cur_mod && type_mod != self.cur_mod {
            let msg = format!("the impl of {} must be in the module that declares the trait or the type", shown(self));
            self.error(span, msg);
            return None;
        }
        let key = (tid, ctor, arity);
        for other in self.impls_by.get(&key).map(|v| v.to_vec()).unwrap_or_default() {
            if self.impls_overlap(trait_ref, &generics, other) {
                if self.impls[other as usize].module == self.cur_mod && self.defined_by_earlier_input(self.impls[other as usize].span) {
                    // Replace an overlapping `impl` from an earlier REPL input.
                    remove_impl(&mut self.impls_by, &key, other);
                    remove_impl(&mut self.impls_by_ctor, &(ctor, arity), other);
                    self.impls[other as usize].replaced = true;
                    continue;
                }
                let other_span = self.impls[other as usize].span;
                let line = self.sources.get(&other_span.file).map_or(0, |f| f.line_col(other_span.lo).0);
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
        self.impls.push(ImplDef {
            trait_ref,
            target,
            generics: generics.clone(),
            module: self.cur_mod,
            span,
            slots: Vec::new(),
            evidence,
            replaced: false,
        });
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
                    self.error(w.name.span, "impl methods cannot require traits of the impl's type parameters; require them on the impl");
                }
            }
            for p in m.params.iter().filter(|p| p.default.is_some()) {
                self.error(p.name.span, "default values are declared by the trait, not the impl");
            }
            self.reject_reflect_params(m);
            let at = DeclPath { item, method: j as u32 };
            let f = self.declare_fn(m, at, rules, Some(ctor), &scope, Some(target));
            if self.traits[tid as usize].builtin == Some(Builtin::Num) && !m.is_native {
                self.error(m.name.span, "the methods of a `Num` impl must be native");
            }
            // Trait dispatch passes the impl dictionary as the first hidden
            // argument, which conflicts with the `Num` evidence slot in native functions.
            if m.is_native && !self.fns[f as usize].dicts.is_empty() {
                self.error(m.name.span, "the Rust function of a native impl method cannot take `Num` evidence");
            }
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

    /// Returns `true` if a new `impl` (with type parameters `generics`) has
    /// trait arguments that unify with `other`.
    fn impls_overlap(&mut self, tr: TraitRef, generics: &[ParamId], other: ImplId) -> bool {
        let fresh: Vec<TypeId> = generics.iter().map(|_| self.fresh()).collect();
        let mine = self.elems(tr.args);
        let mine: Vec<TypeId> = mine.into_iter().map(|t| self.arena.instantiate(t, generics, &fresh)).collect();
        let theirs = self.instantiate_impl(other).args;
        let theirs = self.elems(theirs);
        let pairs: Vec<(TypeId, TypeId)> = mine.into_iter().zip(theirs).collect();
        self.unify_probe(&pairs)
    }

    /// Verifies that an `impl` method signature matches the trait declaration
    /// and populates the method's hidden dictionary list.
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
                let ta: Vec<TraitRef> = self.params[a as usize].traits.clone();
                let ta: Vec<TraitRef> = ta.iter().map(|t| t.subst_params(&mut self.arena, &map)).collect();
                let pb = self.param_ty(b);
                let ta = self.with_supers(pb, ta);
                let tb = self.param_traits(b);
                ok &= ta.iter().all(|x| tb.contains(x)) && tb.iter().all(|x| ta.contains(x));
            }
        }
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
                format!("the type parameters of method `{}` require different traits than in trait `{tname}`", m.name.name)
            } else {
                format!("method `{}` does not match its declaration in trait `{tname}`: expected `{want}`, found `{found}`", m.name.name)
            };
            self.error(m.name.span, msg);
        }
        self.fns[f as usize].dicts = dicts;
    }

    /// Verifies that the target type of `impl` `id` satisfies all supertraits
    /// of the implemented trait.
    pub(crate) fn check_impl_supertraits(&mut self, id: ImplId) {
        let imp = &self.impls[id as usize];
        let evidence = imp.evidence.iter().map(|&(t, tr)| (t, tr, dict_expr(HKind::Error, imp.span))).collect();
        let (target, trait_ref, span) = (imp.target, imp.trait_ref, imp.span);
        let name = self.name(self.traits[trait_ref.trait_id as usize].name).to_string();
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        for s in self.supers_of(target, trait_ref) {
            if let Err(m) = self.resolve_evidence(target, s, 0) {
                let msg = self.unmet_message(m, &format!(", which trait `{name}` requires"));
                self.error(span, msg);
            }
        }
        self.fs.evidence = saved;
    }

    fn instantiate_impl(&mut self, id: ImplId) -> ImplInst {
        let fresh: Vec<TypeId> = (0..self.impls[id as usize].generics.len()).map(|_| self.fresh()).collect();
        let imp = self.impls[id as usize].clone();
        let map = |p: ParamId| imp.generics.iter().position(|&g| g == p).map(|i| fresh[i]);
        let target = self.arena.subst_params(imp.target, &map);
        let args = self.arena.map_list(imp.trait_ref.args, &mut |a, t| a.subst_params(t, &map));
        let evidence =
            imp.evidence.iter().map(|&(t, tr)| (self.arena.subst_params(t, &map), tr.subst_params(&mut self.arena, &map))).collect();
        ImplInst { target, args, evidence }
    }

    /// Resolves trait method `name` on receiver type `ty` through type
    /// parameter bounds or visible `impl` blocks. Emits an error and returns
    /// `Err(())` if multiple traits match.
    pub(crate) fn find_trait_method(&mut self, ty: TypeId, name: &str, span: Span) -> Result<Option<(TraitRef, u32)>, ()> {
        let ty = self.resolve(ty);
        let Some(sym) = self.syms.get(name) else { return Ok(None) };
        let mut found: Vec<(TraitRef, u32)> = Vec::new();
        match self.ty(ty) {
            Ty::Param(p) => {
                for tr in self.param_traits(p) {
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
                // Trait methods from non-prelude modules take precedence over prelude trait methods.
                if found.iter().any(|(tr, _)| Some(self.traits[tr.trait_id as usize].module) != self.prelude) {
                    found.retain(|(tr, _)| Some(self.traits[tr.trait_id as usize].module) != self.prelude);
                }
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

    pub(crate) fn has_trait_method(&mut self, ty: TypeId, name: &str) -> bool {
        let Some(sym) = self.syms.get(name) else { return false };
        let t = self.resolve(ty);
        match self.ty(t) {
            Ty::Param(p) => self.param_traits(p).iter().any(|tr| self.traits[tr.trait_id as usize].slot(sym).is_some()),
            _ => impl_key(&self.arena, t).and_then(|k| self.impls_by_ctor.get(&k)).is_some_and(|ids| {
                ids.iter().any(|&i| self.traits[self.impls[i as usize].trait_ref.trait_id as usize].slot(sym).is_some())
            }),
        }
    }

    /// Returns `true` if `ty` has a candidate implementation of non-derived
    /// trait `t` (through a type parameter bound or a constructor `impl`),
    /// without checking `impl` bounds or unifying trait arguments.
    pub(crate) fn implements_trait(&mut self, ty: TypeId, t: TraitId) -> bool {
        debug_assert!(!self.traits[t as usize].builtin.is_some_and(Builtin::is_derived));
        let ty = self.resolve(ty);
        match self.ty(ty) {
            Ty::Param(p) => self.param_has_trait(p, t),
            _ => impl_key(&self.arena, ty).and_then(|(c, n)| self.impls_by.get(&(t, c, n))).is_some_and(|ids| !ids.is_empty()),
        }
    }

    /// Returns the name of a visible trait that declares method `name`.
    pub(crate) fn trait_with_method(&self, name: &str) -> Option<String> {
        let sym = self.syms.get(name)?;
        self.traits.iter().find(|t| (t.is_pub || t.module == self.cur_mod) && t.slot(sym).is_some()).map(|t| self.name(t.name).to_string())
    }

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

    fn trait_type_bindings(&self, tr: TraitRef, self_ty: TypeId) -> Vec<(ParamId, TypeId)> {
        let td = &self.traits[tr.trait_id as usize];
        let mut bindings = vec![(td.self_param, self_ty)];
        bindings.extend(td.generics.iter().copied().zip(self.arena.list(tr.args).iter().copied()));
        bindings
    }

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
        let bindings = self.trait_type_bindings(tr, self_ty);
        self.call_fn_with(sig, recv, args, span, &bindings)
    }

    pub(crate) fn trait_method_value(&mut self, tr: TraitRef, slot: u32, self_ty: TypeId, span: Span) -> HExpr {
        let sig = self.traits[tr.trait_id as usize].methods[slot as usize].sig;
        let bindings = self.trait_type_bindings(tr, self_ty);
        self.wrapper_value(sig, &bindings, span)
    }

    /// Builds a closure that supplies hidden `reflect` and dictionary arguments
    /// if generic function or trait method `f` is used as a first-class
    /// function value.
    pub(crate) fn wrapper_value(&mut self, f: FnId, pre: &[(ParamId, TypeId)], span: Span) -> HExpr {
        let inst = self.instantiate_fn_with(f, span, pre);
        let id = self.new_lambda();
        let params_ty = inst.params.clone();
        let lenient = move |c: &Self, t: TypeId| matches!(c.rty(t), Ty::Var(_)) && params_ty.iter().any(|&p| c.occurs_in(t, p));
        let mut args = self.hidden_args(f, &inst.fresh, &inst.dicts, "function value", &lenient, span);
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
            FnKind::TraitMethod(trait_id, slot) => HKind::CallTrait { trait_id, slot, targs: inst.fresh, args, impl_id: None },
            FnKind::Native(native) => HKind::CallNative { native, targs: inst.fresh, args },
            _ => HKind::Call { func: f, targs: inst.fresh, args },
        };
        let name = self.name(fi.name).to_string();
        let body = mk(kind, inst.ret, span);
        let lam = HLambda { id, name, params, body, self_local: None };
        let fty = self.fn_ty(&inst.params, inst.ret);
        mk(HKind::Lambda(Box::new(lam)), fty, span)
    }

    /// Builds hidden call arguments for `f` (`reflect` type descriptions
    /// followed by trait evidence placeholders).
    pub(crate) fn hidden_args(
        &mut self,
        f: FnId,
        fresh: &[TypeId],
        dicts: &[(TypeId, TraitRef)],
        what: &'static str,
        lenient: &dyn Fn(&Self, TypeId) -> bool,
        span: Span,
    ) -> Vec<HArg> {
        let mut out = Vec::new();
        for (k, &t) in fresh.iter().enumerate() {
            if self.params[self.fns[f as usize].generics[k] as usize].reflect {
                self.fs.reflect_checks.push((t, span, what));
                out.push(HArg::Expr(mk(HKind::TypeDesc { ty: t, args: Vec::new() }, TypeId::UNIT, span)));
            }
        }
        for &(t, tr) in dicts {
            let lenient = lenient(self, t);
            out.push(HArg::Expr(self.evidence_hole(t, tr, lenient, span)));
        }
        out
    }

    fn reject_reflect_params(&mut self, m: &ast::FnDecl) {
        for g in m.generics.iter().filter(|g| g.reflect) {
            self.error(g.name.span, "the type parameters of trait and impl methods cannot be `reflect`");
        }
    }

    pub(crate) fn declare_reflect_params(&mut self, f: FnId) -> Vec<LocalId> {
        let span = self.fns[f as usize].span;
        let mut out = Vec::new();
        for k in 0..self.fns[f as usize].generics.len() {
            let p = self.fns[f as usize].generics[k];
            if self.params[p as usize].reflect {
                let l = self.declare_local_sym(Symbol::DESC, TypeId::UNIT, false, span, true);
                self.fs.data[l as usize].used = true;
                self.fs.reflect_locals.push((p, l));
                out.push(l);
            }
        }
        out
    }

    pub(crate) fn reflect_local(&self, t: TypeId) -> Option<LocalId> {
        let Ty::Param(p) = self.rty(t) else { return None };
        self.reflect_param_local(p)
    }

    pub(crate) fn reflect_param_local(&self, p: ParamId) -> Option<LocalId> {
        self.fs.reflect_locals.iter().find(|&&(q, _)| q == p).map(|&(_, l)| l)
    }

    /// Declares hidden dictionary parameters for `f` and records the trait
    /// evidence that those parameters (and any `impl` dictionary fields)
    /// provide.
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
            let base =
                self.supertraits_with_dicts(imp.trait_ref.trait_id).count() + self.traits[imp.trait_ref.trait_id as usize].methods.len();
            let evidence: Vec<(TypeId, TraitRef)> = imp.evidence.iter().copied().filter(|(_, tr)| self.has_dict(tr)).collect();
            for (j, (ty, tr)) in evidence.into_iter().enumerate() {
                let obj = Box::new(dict_expr(HKind::Local(d0), span));
                let e = dict_expr(HKind::Field { obj, index: (base + j) as u32 }, span);
                self.fs.evidence.push((ty, tr, e));
            }
        }
        out
    }

    pub(crate) fn has_dict(&self, tr: &TraitRef) -> bool {
        self.traits[tr.trait_id as usize].has_dict
    }

    fn supertraits_with_dicts(&self, t: TraitId) -> impl Iterator<Item = usize> + '_ {
        let supers = &self.traits[t as usize].supers;
        (0..supers.len()).filter(move |&i| self.has_dict(&supers[i]))
    }

    pub(crate) fn evidence_hole(&mut self, ty: TypeId, tr: TraitRef, lenient: bool, span: Span) -> HExpr {
        self.improve_evidence(ty, tr);
        self.fs.holes.push(EvidenceReq { ty, tr, lenient, span });
        dict_expr(HKind::Evidence((self.fs.holes.len() - 1) as u32), span)
    }

    /// Unifies trait and target type arguments eagerly if `ty` has a unique
    /// matching bound or `impl` for `tr.trait_id`.
    fn improve_evidence(&mut self, ty: TypeId, tr: TraitRef) {
        let t = self.resolve(ty);
        match self.ty(t) {
            Ty::Var(_) | Ty::Error | Ty::Never => {}
            Ty::Param(_) => {
                if tr.args == TyList::EMPTY {
                    return;
                }
                let mut cands: Vec<TraitRef> = Vec::new();
                let direct = self.fs.evidence.iter().filter(|(x, _, _)| *x == t).map(|(_, tr, _)| *tr).collect();
                for f in self.with_supers(t, direct) {
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

    /// Resolves all [`HKind::Evidence`] placeholders in `self.fs.holes` and
    /// returns their resolved dictionary expressions.
    pub(crate) fn resolve_holes(&mut self) -> Vec<HExpr> {
        let reqs = std::mem::take(&mut self.fs.holes);
        let mut found: Vec<Option<HExpr>> = vec![None; reqs.len()];
        // Resolving a placeholder can unify type variables used by earlier
        // placeholders, so iterate until reaching a fixed point.
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
                    Err(m) => {
                        let msg = self.unmet_message(m, "");
                        self.error(r.span, msg);
                        mk(HKind::Error, TypeId::UNIT, r.span)
                    }
                },
            };
            out.push(e);
        }
        out
    }

    /// Hoists non-constant dictionary expressions into entry-block local
    /// bindings, deduplicating identical dictionaries.
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

    /// Resolves a dictionary expression proving `ty: tr` in the current
    /// function (`()` for traits without methods).
    pub(crate) fn resolve_evidence(&mut self, ty: TypeId, tr: TraitRef, depth: u32) -> Result<HExpr, Box<Unmet>> {
        let ty = self.zonk(ty);
        let tr = self.zonk_trait(tr);
        let span = Span::default();
        if depth > 32 {
            return Err(Unmet::new(format!("overflow while checking that {} implements `{}`", self.show(ty), self.show_trait(&tr))));
        }
        // Prefer static `impl` dictionaries for concrete types so that trait
        // calls can be devirtualized.
        let ground = !self.arena.has_params(ty);
        if !ground && let Some(e) = self.find_evidence_in_scope(ty, tr) {
            return Ok(e);
        }
        match self.ty(ty) {
            Ty::Error => return Ok(dict_expr(HKind::Error, span)),
            Ty::Never if !self.has_dict(&tr) => return Ok(dict_expr(HKind::Unit, span)),
            Ty::Var(_) => {
                let mut m = Unmet::new(format!("cannot infer the type that must implement `{}`", self.show_trait(&tr)));
                m.unknown = true;
                m.hint = Some("add a type annotation".into());
                return Err(m);
            }
            Ty::Param(p) => {
                let n = self.name(self.params[p as usize].name).to_string();
                let t = self.show_trait(&tr);
                if self.fs.field_default && self.param_has_trait(p, tr.trait_id) {
                    let msg = format!(
                        "field defaults cannot use `{t}`, which type parameter `{n}` requires, because defaults receive no trait dictionaries"
                    );
                    return Err(Unmet::new(msg));
                }
                let mut m = Unmet::new(format!("type parameter `{n}` does not implement `{t}`"));
                m.hint = Some(format!("add `{n}: {t}`"));
                return Err(m);
            }
            _ => {}
        }
        match self.resolve_by_impl(ty, tr, depth) {
            Err(m)
                if !m.unknown
                    && ground
                    && let Some(e) = self.find_evidence_in_scope(ty, tr) =>
            {
                Ok(e)
            }
            r => r,
        }
    }

    /// Returns `Some(true)` if `ty` implements `tr`, `Some(false)` if it does
    /// not, or `None` if `ty` contains unresolved inference variables.
    pub(crate) fn implements(&mut self, ty: TypeId, tr: TraitRef) -> Option<bool> {
        match self.resolve_evidence(ty, tr, 0) {
            Ok(_) => Some(true),
            Err(m) if m.unknown => None,
            Err(_) => Some(false),
        }
    }

    /// Collects all trait bounds required by type constructors in `t` (`Hash`
    /// on `Set` elements and `Map` keys, and declared type parameter bounds on
    /// named types).
    fn written_requirements(&mut self, t: TypeId) -> Vec<Requirement> {
        let hash = self.builtin_trait(Builtin::Hash);
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut todo = vec![t];
        while let Some(t) = todo.pop() {
            if !self.budget.charge(1) {
                break;
            }
            if !seen.insert(t) {
                continue;
            }
            match self.ty(t) {
                Ty::List(x) => todo.push(x),
                Ty::Set(x) => {
                    todo.push(x);
                    out.extend(hash.map(|tr| Requirement { owner: t, arg: x, tr }));
                }
                Ty::Map(k, v) => {
                    todo.extend([k, v]);
                    out.extend(hash.map(|tr| Requirement { owner: t, arg: k, tr }));
                }
                Ty::Tuple(l) => todo.extend(self.elems(l)),
                Ty::Fn(ps, r) => {
                    todo.extend(self.elems(ps));
                    todo.push(r);
                }
                Ty::Named(d, args) => {
                    let (gens, args) = (self.types[d as usize].generics.clone(), self.elems(args));
                    let map = |p: ParamId| gens.iter().position(|&g| g == p).map(|i| args[i]);
                    for (&g, &arg) in gens.iter().zip(&args) {
                        for tr in self.params[g as usize].traits.clone() {
                            let tr = tr.subst_params(&mut self.arena, &map);
                            out.push(Requirement { owner: t, arg, tr });
                        }
                    }
                    todo.extend(args);
                }
                _ => {}
            }
        }
        out
    }

    /// Returns the first unsatisfied type-constructor trait bound in `ty`.
    pub(crate) fn first_unmet_requirement(&mut self, ty: TypeId) -> Option<Box<Unmet>> {
        for r in self.written_requirements(ty) {
            if let Err(mut m) = self.resolve_evidence(r.arg, r.tr, 0) {
                let owner = match self.ty(r.owner) {
                    Ty::Set(_) => format!("the elements of {}", self.show(r.owner)),
                    Ty::Map(..) => format!("the keys of {}", self.show(r.owner)),
                    _ => self.show(r.owner),
                };
                m.required.get_or_insert(owner);
                return Some(m);
            }
        }
        None
    }

    /// Propagates trait bounds required by field and variant payload types onto
    /// the type parameters of type definitions starting at `first`.
    pub(crate) fn infer_member_requirements(&mut self, first: usize) {
        let dependents = self.dependent_types(first);
        self.propagate_to_fixed_point(first, &dependents, &mut |c, d| {
            let gens = c.types[d].generics.clone();
            let mut changed = false;
            for m in c.member_types(d) {
                for r in c.written_requirements(m) {
                    if let Ty::Param(p) = c.ty(r.arg)
                        && gens.contains(&p)
                        && !c.param_traits(p).contains(&r.tr)
                    {
                        c.add_traits(p, vec![r.tr]);
                        changed = true;
                    }
                }
            }
            changed
        });
    }

    pub(crate) fn check_bound_member_types(&mut self, d: TypeDefId) {
        let (span, owner) = (self.types[d as usize].span, self.ctor_name(Ctor::Def(d), 0));
        let def = &self.types[d as usize];
        let mut members: Vec<(TypeId, String)> = def.fields().iter().map(|f| (f.ty, format!("field `{}`", self.name(f.name)))).collect();
        for v in def.variants() {
            members.extend(v.fields.iter().map(|&t| (t, format!("the payload of variant `{}`", self.name(v.name)))));
        }
        for (t, what) in members {
            self.check_member_type(d, t, span, &format!("{what} of `{owner}`: "));
        }
    }

    /// Verifies that field or variant payload type `ty` of type definition `d`
    /// satisfies all type-constructor trait bounds under `d`'s type parameter
    /// bounds.
    pub(crate) fn check_member_type(&mut self, d: TypeDefId, ty: TypeId, span: Span, prefix: &str) {
        let mut evidence = Vec::new();
        for &p in &self.types[d as usize].generics {
            let pt = self.arena.param(p);
            evidence.extend(self.params[p as usize].traits.iter().map(|&tr| (pt, tr, dict_expr(HKind::Error, span))));
        }
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        if let Some(m) = self.first_unmet_requirement(ty) {
            let msg = format!("{prefix}{}", self.unmet_message(m, ""));
            self.error(span, msg);
        }
        self.fs.evidence = saved;
    }

    fn zonk_trait(&mut self, tr: TraitRef) -> TraitRef {
        let Checker { arena, subst, budget, .. } = self;
        TraitRef { trait_id: tr.trait_id, args: arena.map_list(tr.args, &mut |a, t| super::unify::zonk_in(a, subst, budget, t)) }
    }

    fn resolve_by_impl(&mut self, ty: TypeId, tr: TraitRef, depth: u32) -> Result<HExpr, Box<Unmet>> {
        let span = Span::default();
        let not_impl = |c: &Checker| Unmet::new(format!("{} does not implement `{}`", c.show(ty), c.show_trait(&tr)));
        let Some(key) = impl_key(&self.arena, ty) else { return Err(not_impl(self)) };
        if let Some(b) = self.traits[tr.trait_id as usize].builtin.filter(|b| b.is_derived()) {
            return self.resolve_derived(ty, tr, b, depth).map(|()| dict_expr(HKind::Unit, span));
        }
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
            return Err(Unmet::new(format!(
                "cannot infer the type arguments of `{}` for {}: it has several impls; add a type annotation",
                self.show_trait(&tr),
                self.show(ty)
            )));
        }
        let Some(id) = matching.pop() else { return Err(not_impl(self)) };
        let inst = self.instantiate_impl(id);
        let mut pairs = vec![(inst.target, ty)];
        pairs.extend(self.arena.list(inst.args).iter().copied().zip(targs.iter().copied()));
        self.try_unify_all(&pairs);
        let context = |c: &Checker, mut m: Box<Unmet>| {
            if m.required.is_none() {
                m.required = Some(format!("{} to implement `{}`", c.show(ty), c.show_trait(&tr)));
            }
            m
        };
        let mut args = Vec::new();
        let supers = self.supers_of(ty, tr);
        for (s, e) in supers.into_iter().map(|s| (s, None)).chain(inst.evidence.into_iter().map(|(t, s)| (s, Some(t)))) {
            match self.resolve_evidence(e.unwrap_or(ty), s, depth + 1) {
                Ok(d) if self.has_dict(&s) => args.push(d),
                Ok(_) => {}
                // `check_impl_supertraits` reports missing supertraits on the `impl` itself.
                Err(m) if e.is_none() && !m.unknown => {
                    if self.has_dict(&s) {
                        args.push(dict_expr(HKind::Error, span));
                    }
                }
                Err(m) => return Err(context(self, m)),
            }
        }
        Ok(dict_expr(HKind::Dict { impl_id: id, args }, span))
    }

    /// Searches in-scope dictionary parameters, type parameter bounds, and
    /// their supertraits for evidence that `ty` implements `tr`.
    fn find_evidence_in_scope(&mut self, ty: TypeId, tr: TraitRef) -> Option<HExpr> {
        let mut roots: Vec<TraitRef> = Vec::new();
        let mut dicts: Vec<HExpr> = Vec::new();
        for (t, f, e) in &self.fs.evidence {
            if *t == ty && !roots.contains(f) {
                roots.push(*f);
                dicts.push(e.clone());
            }
        }
        if let Ty::Param(p) = self.ty(ty) {
            for f in self.params[p as usize].traits.clone() {
                if !self.has_dict(&f) && !roots.contains(&f) {
                    roots.push(f);
                    dicts.push(dict_expr(HKind::Unit, Span::default()));
                }
            }
        }
        let nodes = self.supertrait_closure(ty, roots);
        let mut at = nodes.iter().position(|(x, _)| *x == tr)?;
        if !self.has_dict(&tr) {
            return Some(dict_expr(HKind::Unit, Span::default()));
        }
        let mut fields = Vec::new();
        while let Some((parent, field)) = nodes[at].1 {
            fields.push(field);
            at = parent;
        }
        let mut e = dicts[at].clone();
        for index in fields.into_iter().rev() {
            let span = e.span;
            e = dict_expr(HKind::Field { obj: Box::new(e), index }, span);
        }
        Some(e)
    }

    /// Unifies all pairs in `pairs`, rolling back all bindings if any pair
    /// fails to unify.
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

    /// Tests whether all pairs in `pairs` unify, rolling back all bindings
    /// afterward.
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
