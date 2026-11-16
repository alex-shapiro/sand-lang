//! Structural implementations of `Encode` and `Decode`.
//!
//! Each codec trait method defaults to a native walker over the parts of a
//! value. An implementation that uses this default method is structural. Its
//! runtime dictionary stores a shape index so the walker can construct each
//! part's dictionary from the outer dictionary. See [`CodecImpl`].
//!
//! The prelude declares structural implementations for built-in types. The
//! checker generates structural implementations on demand for `never` and for
//! each tuple arity. For structs, enums, and native value types without an
//! explicit `impl` block, the checker derives a structural implementation if
//! every part implements the trait.

use super::traits::{ImplDef, impl_key};
use super::*;

/// Part dictionaries and struct decode metadata for a structural codec
/// implementation.
///
/// During bound inference, `T` is an unresolved `(TypeId, TraitRef)` pair.
/// After resolution, `T` is a dictionary [`HExpr`].
#[derive(Clone, Debug)]
pub struct CodecImpl<T = HExpr> {
    /// Dictionary for each part in walk order.
    ///
    /// A bound dictionary is projected as an `HKind::Field` of `Local(0)`.
    pub parts: Vec<T>,
    /// Fallback values for omitted struct fields or `init` parameters during
    /// `Decode`.
    pub fills: Vec<CodecFill<T>>,
    /// Constructor function and its required dictionaries if `Decode` builds a
    /// struct through `init`.
    pub init: Option<(FnId, Vec<T>)>,
}

impl<T> Default for CodecImpl<T> {
    fn default() -> Self {
        CodecImpl { parts: Vec::new(), fills: Vec::new(), init: None }
    }
}

/// Fallback strategy for an omitted struct field or `init` parameter during
/// `Decode`.
#[derive(Clone, Debug)]
pub enum CodecFill<T> {
    /// Omitting the value is an error unless its type is `Option`.
    Required,
    Const(ConstVal),
    /// Default helper function and its required dictionaries.
    ///
    /// Code generation replaces this with a constant default if the field
    /// default folds to a constant.
    Helper(FnId, Vec<T>),
}

impl<T> CodecImpl<T> {
    /// Iterates over all dictionary entries across parts, default helpers, and
    /// `init`.
    fn dicts(&self) -> impl Iterator<Item = &T> {
        let fills = self.fills.iter().flat_map(|f| match f {
            CodecFill::Helper(_, dicts) => dicts.as_slice(),
            _ => &[],
        });
        self.parts.iter().chain(fills).chain(self.init.iter().flat_map(|(_, dicts)| dicts))
    }

    fn map<U>(self, f: &mut impl FnMut(T) -> U) -> CodecImpl<U> {
        let fills = self
            .fills
            .into_iter()
            .map(|fill| match fill {
                CodecFill::Required => CodecFill::Required,
                CodecFill::Const(c) => CodecFill::Const(c),
                CodecFill::Helper(h, dicts) => CodecFill::Helper(h, dicts.into_iter().map(&mut *f).collect()),
            })
            .collect();
        let parts = self.parts.into_iter().map(&mut *f).collect();
        let init = self.init.map(|(func, dicts)| (func, dicts.into_iter().map(&mut *f).collect()));
        CodecImpl { parts, fills, init }
    }
}

/// Unresolved `(TypeId, TraitRef)` requirements of a structural codec
/// implementation.
type UnresolvedCodec = CodecImpl<(TypeId, TraitRef)>;

/// Collects trait bounds required on a structural codec implementation's type
/// parameters. See [`Checker::derive_codec_impls`].
#[derive(Clone)]
pub(crate) struct CodecBoundCollector {
    pub type_params: Vec<ParamId>,
    pub bounds: Vec<(TypeId, TraitRef)>,
}

impl Checker {
    /// Returns `true` if `trait_id` is `Encode` or `Decode`.
    pub(crate) fn is_codec_trait(&self, trait_id: TraitId) -> bool {
        matches!(self.traits[trait_id as usize].builtin, Some(Builtin::Encode | Builtin::Decode))
    }

    /// Returns `true` if implementation `id` uses its codec trait's native
    /// default method.
    pub fn is_structural_impl(&self, id: ImplId) -> bool {
        is_structural_codec_impl(&self.traits, &self.impls[id as usize])
    }

    /// Derives structural codec implementations for type definitions starting
    /// at `first_type`.
    ///
    /// Also resolves part dictionaries for every structural implementation
    /// starting at `first_impl`.
    pub(crate) fn derive_codec_impls(&mut self, first_type: usize, first_impl: usize) {
        for builtin in [Builtin::Encode, Builtin::Decode] {
            let Some(trait_ref) = self.builtin_trait(builtin) else { continue };
            if self.traits[trait_ref.trait_id as usize].methods.first().and_then(|m| m.default).is_none() {
                continue;
            }
            let mut candidates = Vec::new();
            for def_idx in first_type..self.types.len() {
                let def_id = def_idx as TypeDefId;
                let declared = self.impls_by.get(&(trait_ref.trait_id, Ctor::Def(def_id), 0)).is_some_and(|ids| !ids.is_empty());
                if declared {
                    continue;
                }
                match self.codec_form(def_id, builtin) {
                    Some(derived) => {
                        let id = self.add_codec_impl(trait_ref, def_id);
                        if derived {
                            candidates.push(id);
                        }
                    }
                    None if matches!(self.types[def_id as usize].kind, TypeDefKind::Struct { .. }) => {
                        let reason =
                            format!("the parameters of `{}.init` differ from its fields", self.name(self.types[def_id as usize].name));
                        self.codec_failure_reasons.insert((trait_ref.trait_id, def_id), reason.into());
                    }
                    None => {}
                }
            }
            self.find_codec_bounds(&candidates);
        }
        for id in first_impl..self.impls.len() {
            let id = id as ImplId;
            if !self.impls[id as usize].replaced && self.is_structural_impl(id) && self.impls[id as usize].codec.is_none() {
                self.resolve_codec_parts(id);
            }
        }
    }

    /// Classifies how type definition `def_id` implements codec trait
    /// `builtin`:
    ///
    /// - `Some(true)` if the checker derives it from the type's parts.
    /// - `Some(false)` if a manual Rust codec function implements it.
    /// - `None` if the type cannot implement `builtin` without an explicit
    ///   `impl` block.
    fn codec_form(&mut self, def_id: TypeDefId, builtin: Builtin) -> Option<bool> {
        if let Some(codecs) = self.types[def_id as usize].codecs {
            let manual = match builtin {
                Builtin::Encode => matches!(codecs.encode?, NativeCodec::Manual(_)),
                _ => matches!(codecs.decode?, NativeCodec::Manual(_)),
            };
            return Some(!manual);
        }
        match &self.types[def_id as usize].kind {
            TypeDefKind::Struct { .. } => (builtin == Builtin::Decode || self.init_matches_fields(def_id)).then_some(true),
            TypeDefKind::Enum { .. } => Some(true),
            TypeDefKind::Opaque(_) => None,
        }
    }

    /// Returns `true` if struct `def_id` lacks an `init` method or if its
    /// `init` parameters match its fields by name and type in any order.
    fn init_matches_fields(&mut self, def_id: TypeDefId) -> bool {
        let Some(init) = self.init_method(Ctor::Def(def_id)) else { return true };
        let (fi, def) = (self.fns[init as usize].clone(), self.types[def_id as usize].clone());
        let fields = def.fields();
        let own: Vec<TypeId> = def.generics.iter().map(|&p| self.arena.param(p)).collect();
        fi.generics.len() == def.generics.len()
            && fi.params.len() == fields.len()
            && fi.params.iter().all(|p| {
                let ty = self.arena.instantiate(p.ty, &fi.generics, &own);
                fields.iter().any(|f| p.name == f.name && ty == f.ty)
            })
    }

    fn add_codec_impl(&mut self, trait_ref: TraitRef, def_id: TypeDefId) -> ImplId {
        let mut generics = Vec::new();
        for p in self.types[def_id as usize].generics.clone() {
            generics.push(self.new_param(self.params[p as usize].name));
        }
        let args: Vec<TypeId> = generics.iter().map(|&p| self.param_ty(p)).collect();
        let target = self.arena.named(def_id, &args);
        let (module, span) = (self.types[def_id as usize].module, self.types[def_id as usize].span);
        self.push_codec_impl(trait_ref, target, generics, Vec::new(), module, span)
    }

    fn push_codec_impl(
        &mut self,
        trait_ref: TraitRef,
        target: TypeId,
        generics: Vec<ParamId>,
        evidence: Vec<(TypeId, TraitRef)>,
        module: ModId,
        span: Span,
    ) -> ImplId {
        let method = self.traits[trait_ref.trait_id as usize].methods[0].default.expect("a codec trait has a native default method");
        let id = self.impls.len() as ImplId;
        self.impls.push(ImplDef {
            trait_ref,
            target,
            generics,
            module,
            span,
            slots: vec![method],
            evidence,
            replaced: false,
            codec: None,
            derived: true,
        });
        if target != TypeId::NEVER
            && let Some(key) = impl_key(&self.arena, target)
        {
            super::traits::add_impl(&mut self.impls_by, (trait_ref.trait_id, key.0, key.1), id);
            super::traits::add_impl(&mut self.impls_by_ctor, key, id);
        }
        id
    }

    /// Ensures that a structural codec implementation of `trait_ref` exists for
    /// tuples of `arity` elements.
    pub(crate) fn ensure_tuple_codec_impl(&mut self, trait_ref: TraitRef, arity: u32) {
        if self.impls_by.get(&(trait_ref.trait_id, Ctor::Tuple, arity)).is_some_and(|ids| !ids.is_empty()) {
            return;
        }
        let mut generics = Vec::new();
        for i in 0..arity {
            let name = self.syms.intern(&format!("T{i}"));
            generics.push(self.new_param(name));
        }
        let elems: Vec<TypeId> = generics.iter().map(|&p| self.param_ty(p)).collect();
        let target = self.arena.tuple(&elems);
        let evidence = elems.iter().map(|&t| (t, trait_ref)).collect();
        let module = self.prelude.unwrap_or(self.cur_mod);
        let id = self.push_codec_impl(trait_ref, target, generics, evidence, module, Span::default());
        self.resolve_codec_parts(id);
    }

    /// Returns the structural implementation of codec trait `trait_ref` for
    /// `never`.
    pub(crate) fn never_codec_impl(&mut self, trait_ref: TraitRef) -> ImplId {
        let builtin = self.traits[trait_ref.trait_id as usize].builtin.expect("a codec trait is built in");
        if let Some(id) = self.never_codec_impls[builtin as usize] {
            return id;
        }
        let module = self.prelude.unwrap_or(self.cur_mod);
        let id = self.push_codec_impl(trait_ref, TypeId::NEVER, Vec::new(), Vec::new(), module, Span::default());
        self.impls[id as usize].codec = Some(Arc::new(CodecImpl::default()));
        self.never_codec_impls[builtin as usize] = Some(id);
        id
    }

    /// Infers type parameter bounds for each structural codec implementation in
    /// `ids` using greatest fixed-point iteration.
    ///
    /// Mutually recursive types implement the trait unless a part fails the
    /// bound check. Removes any implementation whose parts fail to implement
    /// the trait.
    fn find_codec_bounds(&mut self, ids: &[ImplId]) {
        // Candidate indices to recheck if a type definition's codec implementation changes.
        let mut dependents: HashMap<TypeDefId, Vec<usize>> = HashMap::new();
        for (i, &id) in ids.iter().enumerate() {
            let imp = self.impls[id as usize].clone();
            for &(t, _) in self.unresolved_codec(&imp).unwrap_or_default().dicts() {
                self.arena.any(t, &mut |x| {
                    if let Ty::Named(d, _) = x {
                        let users = dependents.entry(d).or_default();
                        if users.last() != Some(&i) {
                            users.push(i);
                        }
                    }
                    false
                });
            }
        }
        let mut todo: Vec<usize> = (0..ids.len()).rev().collect();
        let mut queued = vec![true; ids.len()];
        while let Some(i) = todo.pop() {
            queued[i] = false;
            if !self.budget.check() {
                return;
            }
            let id = ids[i];
            if self.impls[id as usize].replaced {
                continue;
            }
            let changed = match self.infer_codec_bounds(id) {
                Ok(bounds) => {
                    let mut changed = false;
                    for bound in bounds {
                        if !self.impls[id as usize].evidence.contains(&bound) {
                            self.impls[id as usize].evidence.push(bound);
                            changed = true;
                        }
                    }
                    changed
                }
                Err(part) => {
                    let reason = self.refuting_part(id, part);
                    let imp = &self.impls[id as usize];
                    if let (Ty::Named(d, _), Some(reason)) = (self.arena.get(imp.target), reason) {
                        self.codec_failure_reasons.insert((imp.trait_ref.trait_id, d), reason.into());
                    }
                    self.remove_codec_impl(id);
                    true
                }
            };
            if changed && let Ty::Named(d, _) = self.arena.get(self.impls[id as usize].target) {
                for &u in dependents.get(&d).map(Vec::as_slice).unwrap_or_default() {
                    if !std::mem::replace(&mut queued[u], true) {
                        todo.push(u);
                    }
                }
            }
        }
    }

    fn remove_codec_impl(&mut self, id: ImplId) {
        let imp = &self.impls[id as usize];
        let (t, key) = (imp.trait_ref.trait_id, impl_key(&self.arena, imp.target).unwrap_or((Ctor::Unit, 0)));
        super::traits::remove_impl(&mut self.impls_by, &(t, key.0, key.1), id);
        super::traits::remove_impl(&mut self.impls_by_ctor, &key, id);
        self.impls[id as usize].replaced = true;
    }

    /// Infers the type parameter bounds required by the parts and default
    /// helpers of structural implementation `id`.
    ///
    /// Returns `Err(index)` with the first unsatisfied requirement index if a
    /// concrete requirement fails.
    fn infer_codec_bounds(&mut self, id: ImplId) -> Result<Vec<(TypeId, TraitRef)>, usize> {
        let imp = self.impls[id as usize].clone();
        let reqs: Vec<(TypeId, TraitRef)> = self.unresolved_codec(&imp).ok_or(usize::MAX)?.dicts().copied().collect();
        let evidence = imp.evidence.iter().map(|&(t, tr)| (t, tr, dict_expr(HKind::Error))).collect();
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        let outer = self.codec_bound_collector.replace(CodecBoundCollector { type_params: imp.generics.clone(), bounds: Vec::new() });
        let failed = reqs.into_iter().position(|(t, tr)| match self.resolve_evidence(t, tr, 0) {
            Ok(_) => false,
            Err(m) => !m.unknown,
        });
        let bounds = std::mem::replace(&mut self.codec_bound_collector, outer).map(|c| c.bounds).unwrap_or_default();
        self.fs.evidence = saved;
        match failed {
            Some(i) => Err(i),
            None => Ok(bounds),
        }
    }

    /// Formats diagnostic reason text if `part` is an unsatisfied part of
    /// structural implementation `id`. See [`CodecImpl::dicts`].
    fn refuting_part(&mut self, id: ImplId, part: usize) -> Option<String> {
        let imp = self.impls[id as usize].clone();
        let Ty::Named(d, _) = self.ty(imp.target) else { return None };
        let owner = self.show(imp.target);
        let decode = self.traits[imp.trait_ref.trait_id as usize].builtin == Some(Builtin::Decode);
        let (t, tr) = *self.unresolved_codec(&imp)?.parts.get(part)?;
        let mut inner = None;
        self.arena.any(t, &mut |x| {
            if let Ty::Named(e, _) = x
                && e != d
                && inner.is_none()
            {
                inner = self.codec_failure_reasons.get(&(tr.trait_id, e)).cloned();
            }
            false
        });
        if let Some(reason) = inner {
            return Some(reason.to_string());
        }
        let shown = self.show(t);
        let def = self.types[d as usize].clone();
        if let Some(init) = self.init_method(Ctor::Def(d)).filter(|_| decode) {
            let p = self.fns[init as usize].params.get(part)?.name;
            return Some(format!("parameter `{}` of `{owner}.init` has type `{shown}`", self.name(p)));
        }
        match &def.kind {
            TypeDefKind::Struct { fields } => {
                let f = fields.get(part)?.name;
                Some(format!("field `{}` of `{owner}` has type `{shown}`", self.name(f)))
            }
            TypeDefKind::Enum { variants } => {
                let mut i = part;
                let v = variants.iter().find(|v| {
                    let found = i < v.fields.len();
                    if !found {
                        i -= v.fields.len();
                    }
                    found
                })?;
                Some(format!("the payload of variant `{}` of `{owner}` has type `{shown}`", self.name(v.name)))
            }
            TypeDefKind::Opaque(_) => None,
        }
    }

    /// Resolves the dictionary expressions for every part and default helper of
    /// structural implementation `id`.
    fn resolve_codec_parts(&mut self, id: ImplId) {
        let imp = self.impls[id as usize].clone();
        let Some(unresolved) = self.unresolved_codec(&imp) else {
            let msg = format!("the parts of {} cannot be encoded or decoded", self.show(imp.target));
            self.error(imp.span, msg);
            return;
        };
        let mut evidence = Vec::new();
        let mut slot = crate::vm::bytecode::CODEC_SHAPE_SLOT as u32 + 1;
        for &(t, tr) in &imp.evidence {
            let e = match self.has_dict(&tr) {
                true => {
                    slot += 1;
                    let obj = Box::new(dict_expr(HKind::Local(0)));
                    dict_expr(HKind::Field { obj, index: slot - 1 })
                }
                false => dict_expr(HKind::Unit),
            };
            evidence.push((t, tr, e));
        }
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        let codec = unresolved.map(&mut |(t, tr)| match self.resolve_evidence(t, tr, 0) {
            Ok(e) => e,
            Err(m) => {
                let shown = format!("`{}` for {}", self.show_trait(&imp.trait_ref), self.show(imp.target));
                let msg = self.unmet_message(m, &format!(" (required by the default method of the impl of {shown})"));
                self.error(imp.span, msg);
                dict_expr(HKind::Error)
            }
        });
        self.fs.evidence = saved;
        self.impls[id as usize].codec = Some(Arc::new(codec));
    }

    /// Collects the unresolved `(TypeId, TraitRef)` requirements for structural
    /// implementation `imp`.
    ///
    /// Returns `None` if the target type cannot have a structural codec
    /// implementation.
    fn unresolved_codec(&mut self, imp: &ImplDef) -> Option<UnresolvedCodec> {
        let tr = imp.trait_ref;
        let decode = self.traits[tr.trait_id as usize].builtin == Some(Builtin::Decode);
        let mut codec = UnresolvedCodec::default();
        let target = self.resolve(imp.target);
        match self.ty(target) {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Range | Ty::Never => {}
            Ty::List(x) | Ty::Set(x) => codec.parts.push((x, tr)),
            Ty::Map(k, v) => codec.parts.extend([(k, tr), (v, tr)]),
            Ty::Tuple(l) => codec.parts.extend(self.elems(l).into_iter().map(|t| (t, tr))),
            Ty::Named(d, args) => {
                let args = self.elems(args);
                let def = self.types[d as usize].clone();
                if self.codec_form(d, if decode { Builtin::Decode } else { Builtin::Encode }) == Some(false) {
                    return Some(codec);
                }
                let inst = |c: &mut Checker, t: TypeId, gens: &[ParamId]| c.arena.instantiate(t, gens, &args[..gens.len().min(args.len())]);
                // Instantiates the dictionary bounds of `init` or a default helper with `args`.
                let dicts = |c: &mut Checker, ds: &[(TypeId, TraitRef)], gens: &[ParamId]| -> Vec<(TypeId, TraitRef)> {
                    ds.iter()
                        .map(|&(t, dtr)| {
                            let t = inst(c, t, gens);
                            (t, dtr.subst_params(&mut c.arena, &|q| gens.iter().position(|&g| g == q).map(|i| args[i])))
                        })
                        .collect()
                };
                match &def.kind {
                    TypeDefKind::Struct { fields } => match self.init_method(Ctor::Def(d)).filter(|_| decode) {
                        Some(init) => {
                            let fi = self.fns[init as usize].clone();
                            if fi.generics.len() != def.generics.len() {
                                return None;
                            }
                            for p in &fi.params {
                                let t = inst(self, p.ty, &fi.generics);
                                codec.parts.push((t, tr));
                                codec.fills.push(match &p.default {
                                    Some(DefaultArg::Helper(h)) => {
                                        let helper = self.fns[*h as usize].dicts.clone();
                                        CodecFill::Helper(*h, dicts(self, &helper, &fi.generics))
                                    }
                                    Some(DefaultArg::Inline(e)) => closed_const(e).map_or(CodecFill::Required, CodecFill::Const),
                                    None => CodecFill::Required,
                                });
                            }
                            codec.init = Some((init, dicts(self, &fi.dicts, &fi.generics)));
                        }
                        None => {
                            for f in fields {
                                let t = inst(self, f.ty, &def.generics);
                                codec.parts.push((t, tr));
                                if decode {
                                    codec.fills.push(match f.default {
                                        Some(h) => {
                                            let helper = self.fns[h as usize].dicts.clone();
                                            CodecFill::Helper(h, dicts(self, &helper, &def.generics))
                                        }
                                        None => CodecFill::Required,
                                    });
                                }
                            }
                        }
                    },
                    TypeDefKind::Enum { variants } => {
                        for v in variants {
                            for &t in &v.fields {
                                let t = inst(self, t, &def.generics);
                                codec.parts.push((t, tr));
                            }
                        }
                    }
                    TypeDefKind::Opaque(Opaque::Value(_)) => {}
                    TypeDefKind::Opaque(_) => return None,
                }
            }
            _ => return None,
        }
        Some(codec)
    }
}

/// Returns `true` if `imp` implements `Encode` or `Decode` using the trait's
/// native default method.
pub(crate) fn is_structural_codec_impl(traits: &PVec<TraitDef>, imp: &ImplDef) -> bool {
    let td = &traits[imp.trait_ref.trait_id as usize];
    matches!(td.builtin, Some(Builtin::Encode | Builtin::Decode))
        && td.methods.first().and_then(|m| m.default) == imp.slots.first().copied()
}

fn dict_expr(kind: HKind) -> HExpr {
    super::expr::mk(kind, TypeId::UNIT, Span::default())
}
