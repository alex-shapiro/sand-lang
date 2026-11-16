//! Structural implementations of `Encode` and `Decode`.
//!
//! The prelude declares a native default method on each codec trait to walk the
//! parts of a value. An implementation that keeps this default method is
//! structural. The compiler stores a shape index in each structural dictionary.
//! The walker reads that shape to build each part's dictionary from the outer
//! dictionary. See [`CodecImpl`].
//!
//! The prelude declares structural implementations for built-in types. The
//! checker generates structural implementations on demand for `never` and for
//! each tuple arity. The checker also derives a structural implementation for
//! each struct, enum, or native value type if every part implements the trait.
//! An explicit `impl` block overrides derivation.

use super::traits::{ImplDef, impl_key};
use super::*;

/// Part dictionaries and struct decode metadata for a structural codec
/// implementation.
///
/// During bound inference, `T` is an unresolved `(TypeId, TraitRef)` pair.
/// After resolution, `T` is a dictionary [`HExpr`].
#[derive(Clone, Debug)]
pub struct CodecImpl<T = HExpr> {
    /// Dictionary entry for each part in walk order.
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
    /// The input must supply a value unless the target type is `Option`.
    Required,
    Const(ConstVal),
    /// Default helper function and its required dictionaries.
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
/// parameters. See [`Checker::infer_codec_bounds`].
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

    /// Returns `true` if implementation `impl_id` implements `Encode` or
    /// `Decode` with the trait's native default method.
    pub fn is_structural_codec_impl(&self, impl_id: ImplId) -> bool {
        is_structural_codec_impl(&self.traits, &self.impls[impl_id as usize])
    }

    /// Derives structural codec implementations for type definitions from
    /// `first_type` onward.
    ///
    /// Also resolves part dictionaries for every structural implementation from
    /// `first_impl` onward.
    pub(crate) fn derive_codec_impls(&mut self, first_type: usize, first_impl: usize) {
        for builtin in [Builtin::Encode, Builtin::Decode] {
            let Some(trait_ref) = self.builtin_trait(builtin) else { continue };
            if self.traits[trait_ref.trait_id as usize].methods.first().and_then(|m| m.default).is_none() {
                continue;
            }
            let mut candidates = Vec::new();
            for def_idx in first_type..self.types.len() {
                let def_id = def_idx as TypeDefId;
                let has_declared_impl = self.impls_by.get(&(trait_ref.trait_id, Ctor::Def(def_id), 0)).is_some_and(|ids| !ids.is_empty());
                if has_declared_impl {
                    continue;
                }
                match self.codec_form(def_id, builtin) {
                    Some(is_derived) => {
                        let impl_id = self.add_codec_impl(trait_ref, def_id);
                        if is_derived {
                            candidates.push(impl_id);
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
            self.propagate_codec_bounds(&candidates);
        }
        for impl_idx in first_impl..self.impls.len() {
            let impl_id = impl_idx as ImplId;
            if !self.impls[impl_id as usize].replaced
                && self.is_structural_codec_impl(impl_id)
                && self.impls[impl_id as usize].codec.is_none()
            {
                self.resolve_codec_impl(impl_id);
            }
        }
    }

    /// Classifies how type definition `def_id` implements codec trait
    /// `builtin`:
    ///
    /// - `Some(true)` if the checker derives it from the type's parts.
    /// - `Some(false)` if a manual Rust codec function implements it.
    /// - `None` if `def_id` requires an explicit `impl` block to implement
    ///   `builtin`.
    fn codec_form(&mut self, def_id: TypeDefId, builtin: Builtin) -> Option<bool> {
        if let Some(native_codecs) = self.types[def_id as usize].native_codecs {
            let manual = match builtin {
                Builtin::Encode => matches!(native_codecs.encode?, NativeCodec::Manual(_)),
                _ => matches!(native_codecs.decode?, NativeCodec::Manual(_)),
            };
            return Some(!manual);
        }
        match &self.types[def_id as usize].kind {
            TypeDefKind::Struct { .. } => (builtin == Builtin::Decode || self.init_matches_fields(def_id)).then_some(true),
            TypeDefKind::Enum { .. } => Some(true),
            TypeDefKind::Opaque(_) => None,
        }
    }

    /// Returns `true` if struct `def_id` does not declare `init`, or if its
    /// `init` parameters match its fields by name and type.
    fn init_matches_fields(&mut self, def_id: TypeDefId) -> bool {
        let Some(init) = self.init_method(Ctor::Def(def_id)) else { return true };
        let (init_fn, def) = (self.fns[init as usize].clone(), self.types[def_id as usize].clone());
        let fields = def.fields();
        let type_params: Vec<TypeId> = def.generics.iter().map(|&p| self.arena.param(p)).collect();
        init_fn.generics.len() == def.generics.len()
            && init_fn.params.len() == fields.len()
            && init_fn.params.iter().all(|p| {
                let ty = self.arena.instantiate(p.ty, &init_fn.generics, &type_params);
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
        let impl_id = self.push_codec_impl(trait_ref, target, generics, evidence, module, Span::default());
        self.resolve_codec_impl(impl_id);
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

    /// Propagates type parameter bounds across the structural codec
    /// implementations in `impl_ids` to a greatest fixed point.
    ///
    /// Removes any implementation with a part that fails its trait bound. Keeps
    /// implementations for mutually recursive types if every part satisfies its
    /// bound.
    fn propagate_codec_bounds(&mut self, impl_ids: &[ImplId]) {
        let mut dependent_candidates: HashMap<TypeDefId, Vec<usize>> = HashMap::new();
        for (i, &impl_id) in impl_ids.iter().enumerate() {
            let imp = self.impls[impl_id as usize].clone();
            for &(t, _) in self.unresolved_codec(&imp).unwrap_or_default().dicts() {
                self.arena.any(t, &mut |x| {
                    if let Ty::Named(d, _) = x {
                        let users = dependent_candidates.entry(d).or_default();
                        if users.last() != Some(&i) {
                            users.push(i);
                        }
                    }
                    false
                });
            }
        }
        let mut todo: Vec<usize> = (0..impl_ids.len()).rev().collect();
        let mut queued = vec![true; impl_ids.len()];
        while let Some(i) = todo.pop() {
            queued[i] = false;
            if !self.budget.check() {
                return;
            }
            let impl_id = impl_ids[i];
            if self.impls[impl_id as usize].replaced {
                continue;
            }
            let changed = match self.infer_codec_bounds(impl_id) {
                Ok(bounds) => {
                    let mut changed = false;
                    for bound in bounds {
                        if !self.impls[impl_id as usize].evidence.contains(&bound) {
                            self.impls[impl_id as usize].evidence.push(bound);
                            changed = true;
                        }
                    }
                    changed
                }
                Err(part_index) => {
                    let reason = self.codec_failure_reason(impl_id, part_index);
                    let imp = &self.impls[impl_id as usize];
                    if let (Ty::Named(d, _), Some(reason)) = (self.arena.get(imp.target), reason) {
                        self.codec_failure_reasons.insert((imp.trait_ref.trait_id, d), reason.into());
                    }
                    self.remove_codec_impl(impl_id);
                    true
                }
            };
            if changed && let Ty::Named(d, _) = self.arena.get(self.impls[impl_id as usize].target) {
                for &candidate_idx in dependent_candidates.get(&d).map(Vec::as_slice).unwrap_or_default() {
                    if !std::mem::replace(&mut queued[candidate_idx], true) {
                        todo.push(candidate_idx);
                    }
                }
            }
        }
    }

    fn remove_codec_impl(&mut self, impl_id: ImplId) {
        let imp = &self.impls[impl_id as usize];
        let (trait_id, key) = (imp.trait_ref.trait_id, impl_key(&self.arena, imp.target).unwrap_or((Ctor::Unit, 0)));
        super::traits::remove_impl(&mut self.impls_by, &(trait_id, key.0, key.1), impl_id);
        super::traits::remove_impl(&mut self.impls_by_ctor, &key, impl_id);
        self.impls[impl_id as usize].replaced = true;
    }

    /// Infers the type parameter bounds required by `impl_id`.
    ///
    /// Returns `Err(index)` with the first failing requirement index if a
    /// concrete requirement is unsatisfied.
    fn infer_codec_bounds(&mut self, impl_id: ImplId) -> Result<Vec<(TypeId, TraitRef)>, usize> {
        let imp = self.impls[impl_id as usize].clone();
        let requirements: Vec<(TypeId, TraitRef)> = self.unresolved_codec(&imp).ok_or(usize::MAX)?.dicts().copied().collect();
        let evidence = imp.evidence.iter().map(|&(bound_ty, trait_ref)| (bound_ty, trait_ref, dict_expr(HKind::Error))).collect();
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        let outer_collector =
            self.codec_bound_collector.replace(CodecBoundCollector { type_params: imp.generics.clone(), bounds: Vec::new() });
        let failed_index = requirements.into_iter().position(|(bound_ty, trait_ref)| match self.resolve_evidence(bound_ty, trait_ref, 0) {
            Ok(_) => false,
            Err(unmet) => !unmet.unknown,
        });
        let bounds =
            std::mem::replace(&mut self.codec_bound_collector, outer_collector).map(|collector| collector.bounds).unwrap_or_default();
        self.fs.evidence = saved;
        match failed_index {
            Some(index) => Err(index),
            None => Ok(bounds),
        }
    }

    /// Formats the diagnostic reason why part `part_index` prevents `impl_id`
    /// from implementing its codec trait.
    fn codec_failure_reason(&mut self, impl_id: ImplId, part_index: usize) -> Option<String> {
        let imp = self.impls[impl_id as usize].clone();
        let Ty::Named(def_id, _) = self.ty(imp.target) else { return None };
        let owner = self.show(imp.target);
        let is_decode = self.traits[imp.trait_ref.trait_id as usize].builtin == Some(Builtin::Decode);
        let (part_ty, trait_ref) = *self.unresolved_codec(&imp)?.parts.get(part_index)?;
        let mut inner_reason = None;
        self.arena.any(part_ty, &mut |sub_ty| {
            if let Ty::Named(other_def, _) = sub_ty
                && other_def != def_id
                && inner_reason.is_none()
            {
                inner_reason = self.codec_failure_reasons.get(&(trait_ref.trait_id, other_def)).cloned();
            }
            false
        });
        if let Some(reason) = inner_reason {
            return Some(reason.to_string());
        }
        let shown_part_ty = self.show(part_ty);
        let def = self.types[def_id as usize].clone();
        if let Some(init) = self.init_method(Ctor::Def(def_id)).filter(|_| is_decode) {
            let param_name = self.fns[init as usize].params.get(part_index)?.name;
            return Some(format!("parameter `{}` of `{owner}.init` has type `{shown_part_ty}`", self.name(param_name)));
        }
        match &def.kind {
            TypeDefKind::Struct { fields } => {
                let field_name = fields.get(part_index)?.name;
                Some(format!("field `{}` of `{owner}` has type `{shown_part_ty}`", self.name(field_name)))
            }
            TypeDefKind::Enum { variants } => {
                let mut remaining = part_index;
                let variant = variants.iter().find(|variant| {
                    let found = remaining < variant.fields.len();
                    if !found {
                        remaining -= variant.fields.len();
                    }
                    found
                })?;
                Some(format!("the payload of variant `{}` of `{owner}` has type `{shown_part_ty}`", self.name(variant.name)))
            }
            TypeDefKind::Opaque(_) => None,
        }
    }

    /// Populates the resolved [`CodecImpl`] for structural implementation
    /// `impl_id`.
    fn resolve_codec_impl(&mut self, impl_id: ImplId) {
        let imp = self.impls[impl_id as usize].clone();
        let Some(unresolved) = self.unresolved_codec(&imp) else {
            let msg = format!("the parts of {} cannot be encoded or decoded", self.show(imp.target));
            self.error(imp.span, msg);
            return;
        };
        let mut evidence = Vec::new();
        let mut slot = crate::vm::bytecode::CODEC_SHAPE_SLOT as u32 + 1;
        for &(bound_ty, trait_ref) in &imp.evidence {
            let dict = match self.has_dict(&trait_ref) {
                true => {
                    slot += 1;
                    let obj = Box::new(dict_expr(HKind::Local(0)));
                    dict_expr(HKind::Field { obj, index: slot - 1 })
                }
                false => dict_expr(HKind::Unit),
            };
            evidence.push((bound_ty, trait_ref, dict));
        }
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        let codec = unresolved.map(&mut |(bound_ty, trait_ref)| match self.resolve_evidence(bound_ty, trait_ref, 0) {
            Ok(dict) => dict,
            Err(unmet) => {
                let shown = format!("`{}` for {}", self.show_trait(&imp.trait_ref), self.show(imp.target));
                let msg = self.unmet_message(unmet, &format!(" (required by the default method of the impl of {shown})"));
                self.error(imp.span, msg);
                dict_expr(HKind::Error)
            }
        });
        self.fs.evidence = saved;
        self.impls[impl_id as usize].codec = Some(Arc::new(codec));
    }

    /// Collects the unresolved `(TypeId, TraitRef)` requirements for structural
    /// implementation `imp`.
    ///
    /// Returns `None` if the target type cannot have a structural codec
    /// implementation.
    fn unresolved_codec(&mut self, imp: &ImplDef) -> Option<UnresolvedCodec> {
        let trait_ref = imp.trait_ref;
        let is_decode = self.traits[trait_ref.trait_id as usize].builtin == Some(Builtin::Decode);
        let mut codec = UnresolvedCodec::default();
        let target = self.resolve(imp.target);
        match self.ty(target) {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Range | Ty::Never => {}
            Ty::List(elem) | Ty::Set(elem) => codec.parts.push((elem, trait_ref)),
            Ty::Map(key, value) => codec.parts.extend([(key, trait_ref), (value, trait_ref)]),
            Ty::Tuple(elems) => codec.parts.extend(self.elems(elems).into_iter().map(|part_ty| (part_ty, trait_ref))),
            Ty::Named(def_id, args) => {
                let args = self.elems(args);
                let def = self.types[def_id as usize].clone();
                if self.codec_form(def_id, if is_decode { Builtin::Decode } else { Builtin::Encode }) == Some(false) {
                    return Some(codec);
                }
                let instantiate_ty = |checker: &mut Checker, ty: TypeId, generics: &[ParamId]| {
                    checker.arena.instantiate(ty, generics, &args[..generics.len().min(args.len())])
                };
                let instantiate_dicts =
                    |checker: &mut Checker, dicts: &[(TypeId, TraitRef)], generics: &[ParamId]| -> Vec<(TypeId, TraitRef)> {
                        dicts
                            .iter()
                            .map(|&(bound_ty, bound_trait)| {
                                let bound_ty = instantiate_ty(checker, bound_ty, generics);
                                let bound_trait = bound_trait.subst_params(&mut checker.arena, &|param| {
                                    generics.iter().position(|&generic| generic == param).map(|idx| args[idx])
                                });
                                (bound_ty, bound_trait)
                            })
                            .collect()
                    };
                match &def.kind {
                    TypeDefKind::Struct { fields } => match self.init_method(Ctor::Def(def_id)).filter(|_| is_decode) {
                        Some(init) => {
                            let init_fn = self.fns[init as usize].clone();
                            if init_fn.generics.len() != def.generics.len() {
                                return None;
                            }
                            for param in &init_fn.params {
                                let part_ty = instantiate_ty(self, param.ty, &init_fn.generics);
                                codec.parts.push((part_ty, trait_ref));
                                codec.fills.push(match &param.default {
                                    Some(DefaultArg::Helper(helper)) => {
                                        let helper_dicts = self.fns[*helper as usize].dicts.clone();
                                        CodecFill::Helper(*helper, instantiate_dicts(self, &helper_dicts, &init_fn.generics))
                                    }
                                    Some(DefaultArg::Inline(expr)) => closed_const(expr).map_or(CodecFill::Required, CodecFill::Const),
                                    None => CodecFill::Required,
                                });
                            }
                            codec.init = Some((init, instantiate_dicts(self, &init_fn.dicts, &init_fn.generics)));
                        }
                        None => {
                            for field in fields {
                                let part_ty = instantiate_ty(self, field.ty, &def.generics);
                                codec.parts.push((part_ty, trait_ref));
                                if is_decode {
                                    codec.fills.push(match field.default {
                                        Some(helper) => {
                                            let helper_dicts = self.fns[helper as usize].dicts.clone();
                                            CodecFill::Helper(helper, instantiate_dicts(self, &helper_dicts, &def.generics))
                                        }
                                        None => CodecFill::Required,
                                    });
                                }
                            }
                        }
                    },
                    TypeDefKind::Enum { variants } => {
                        for variant in variants {
                            for &field_ty in &variant.fields {
                                let part_ty = instantiate_ty(self, field_ty, &def.generics);
                                codec.parts.push((part_ty, trait_ref));
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

/// Returns `true` if `imp` implements `Encode` or `Decode` with the trait's
/// native default method.
pub(crate) fn is_structural_codec_impl(traits: &PVec<TraitDef>, imp: &ImplDef) -> bool {
    let trait_def = &traits[imp.trait_ref.trait_id as usize];
    matches!(trait_def.builtin, Some(Builtin::Encode | Builtin::Decode))
        && trait_def.methods.first().and_then(|m| m.default) == imp.slots.first().copied()
}

fn dict_expr(kind: HKind) -> HExpr {
    super::expr::mk(kind, TypeId::UNIT, Span::default())
}
