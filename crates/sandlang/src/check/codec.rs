//! Structural implementations of `Encode` and `Decode`.
//!
//! The method of each codec trait defaults to a native function that walks the
//! parts of a value. An implementation that uses this default is structural.
//! Its dictionary also holds a shape, which tells the walker how to build the
//! dictionary of each part from the dictionary of the whole (see
//! [`CodecImpl`]).
//!
//! The prelude declares the structural implementations of the built-in types.
//! The checker adds one for each tuple arity at its first use, one for
//! `never`, and one for each struct, enum, and native value type that has no
//! implementation of its own and whose parts implement the trait.

use super::traits::{ImplDef, impl_key};
use super::*;

/// The resolved parts of a structural implementation.
#[derive(Clone, Debug, Default)]
pub struct CodecImpl {
    /// The dictionary of each part, in walk order. A field of the dictionary
    /// of the implementation (a `Field` of `Local(0)`) is one of its bounds.
    pub parts: Vec<HExpr>,
    /// For `Decode` of a struct: the dictionaries of the default helper of
    /// each field or `init` parameter.
    pub fills: Vec<Vec<HExpr>>,
    /// For `Decode` of a struct that declares `init`.
    pub init: Option<CodecInit>,
}

/// The decode of a struct through its `init` constructor.
#[derive(Clone, Debug)]
pub struct CodecInit {
    pub func: FnId,
    /// The dictionaries that `init` requires.
    pub dicts: Vec<HExpr>,
    /// The constant default of each parameter that has one.
    pub consts: Vec<Option<ConstVal>>,
}

/// The bounds on the type parameters of a structural impl that its parts
/// need (see [`Checker::derive_codec_impls`]).
#[derive(Clone)]
pub(crate) struct CodecReqs {
    pub generics: Vec<ParamId>,
    pub found: Vec<(TypeId, TraitRef)>,
}

/// The index of the first bound in the dictionary of a structural
/// implementation, after its method and its shape.
pub const CODEC_BOUNDS: u32 = 2;

impl Checker {
    /// Returns `true` if `t` is `Encode` or `Decode`.
    pub(crate) fn is_codec_trait(&self, t: TraitId) -> bool {
        matches!(self.traits[t as usize].builtin, Some(Builtin::Encode | Builtin::Decode))
    }

    /// Returns `true` if impl `id` uses the native default method of its trait.
    pub fn is_structural_impl(&self, id: ImplId) -> bool {
        is_structural(&self.traits, &self.impls[id as usize])
    }

    /// Creates the structural codec impls of the type definitions from
    /// `first_type`, and resolves the parts of each structural impl from
    /// `first_impl`.
    pub(crate) fn derive_codec_impls(&mut self, first_type: usize, first_impl: usize) {
        for b in [Builtin::Encode, Builtin::Decode] {
            let Some(tr) = self.builtin_trait(b) else { continue };
            if self.traits[tr.trait_id as usize].methods.first().and_then(|m| m.default).is_none() {
                continue;
            }
            let mut candidates = Vec::new();
            for d in first_type..self.types.len() {
                let d = d as TypeDefId;
                let declared = self.impls_by.get(&(tr.trait_id, Ctor::Def(d), 0)).is_some_and(|ids| !ids.is_empty());
                if declared {
                    continue;
                }
                match self.codec_form(d, b) {
                    Some(derived) => {
                        let id = self.add_codec_impl(tr, d);
                        if derived {
                            candidates.push(id);
                        }
                    }
                    None if matches!(self.types[d as usize].kind, TypeDefKind::Struct { .. }) => {
                        let reason = format!("the parameters of `{}.init` differ from its fields", self.name(self.types[d as usize].name));
                        self.codec_refuted.insert((tr.trait_id, d), reason.into());
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

    /// Returns whether type definition `d` implements codec trait `b`
    /// structurally: `Some(true)` through its parts, `Some(false)` through a
    /// Rust function, or `None` if it does not implement `b` by itself.
    fn codec_form(&mut self, d: TypeDefId, b: Builtin) -> Option<bool> {
        if let Some(codecs) = self.types[d as usize].codecs {
            let manual = match b {
                Builtin::Encode => matches!(codecs.encode?, NativeCodec::Manual(_)),
                _ => matches!(codecs.decode?, NativeCodec::Manual(_)),
            };
            return Some(!manual);
        }
        match &self.types[d as usize].kind {
            TypeDefKind::Struct { .. } => (b == Builtin::Decode || self.init_matches_fields(d)).then_some(true),
            TypeDefKind::Enum { .. } => Some(true),
            TypeDefKind::Opaque(_) => None,
        }
    }

    /// Returns `true` if struct `d` declares no `init`, or if the parameters
    /// of its `init` have the names and types of its fields.
    fn init_matches_fields(&mut self, d: TypeDefId) -> bool {
        let Some(init) = self.init_method(Ctor::Def(d)) else { return true };
        let (fi, def) = (self.fns[init as usize].clone(), self.types[d as usize].clone());
        let fields = def.fields();
        let own: Vec<TypeId> = def.generics.iter().map(|&p| self.arena.param(p)).collect();
        fi.generics.len() == def.generics.len()
            && fi.params.len() == fields.len()
            && fi.params.iter().zip(fields).all(|(p, f)| p.name == f.name && self.arena.instantiate(p.ty, &fi.generics, &own) == f.ty)
    }

    fn add_codec_impl(&mut self, tr: TraitRef, d: TypeDefId) -> ImplId {
        let mut generics = Vec::new();
        for p in self.types[d as usize].generics.clone() {
            generics.push(self.new_param(self.params[p as usize].name));
        }
        let args: Vec<TypeId> = generics.iter().map(|&p| self.param_ty(p)).collect();
        let target = self.arena.named(d, &args);
        let (module, span) = (self.types[d as usize].module, self.types[d as usize].span);
        self.push_codec_impl(tr, target, generics, Vec::new(), module, span, (Ctor::Def(d), 0))
    }

    #[allow(clippy::too_many_arguments)]
    fn push_codec_impl(
        &mut self,
        tr: TraitRef,
        target: TypeId,
        generics: Vec<ParamId>,
        evidence: Vec<(TypeId, TraitRef)>,
        module: ModId,
        span: Span,
        key: (Ctor, u32),
    ) -> ImplId {
        let method = self.traits[tr.trait_id as usize].methods[0].default.expect("a codec trait has a native default method");
        let id = self.impls.len() as ImplId;
        let derived = true;
        self.impls.push(ImplDef {
            trait_ref: tr,
            target,
            generics,
            module,
            span,
            slots: vec![method],
            evidence,
            replaced: false,
            codec: None,
            derived,
        });
        if target != TypeId::NEVER {
            super::traits::add_impl(&mut self.impls_by, (tr.trait_id, key.0, key.1), id);
            super::traits::add_impl(&mut self.impls_by_ctor, key, id);
        }
        id
    }

    /// Returns the structural impl of codec trait `tr` for tuples of `arity`
    /// elements, and creates it at its first use.
    pub(crate) fn tuple_codec_impl(&mut self, tr: TraitRef, arity: u32) {
        if self.impls_by.get(&(tr.trait_id, Ctor::Tuple, arity)).is_some_and(|ids| !ids.is_empty()) {
            return;
        }
        let mut generics = Vec::new();
        for i in 0..arity {
            let name = self.syms.intern(&format!("T{i}"));
            generics.push(self.new_param(name));
        }
        let elems: Vec<TypeId> = generics.iter().map(|&p| self.param_ty(p)).collect();
        let target = self.arena.tuple(&elems);
        let evidence = elems.iter().map(|&t| (t, tr)).collect();
        let module = self.prelude.unwrap_or(self.cur_mod);
        let id = self.push_codec_impl(tr, target, generics, evidence, module, Span::default(), (Ctor::Tuple, arity));
        self.resolve_codec_parts(id);
    }

    /// Returns the structural impl of codec trait `tr` for `never`.
    pub(crate) fn never_codec_impl(&mut self, tr: TraitRef) -> ImplId {
        let b = self.traits[tr.trait_id as usize].builtin.expect("a codec trait is built in");
        if let Some(id) = self.never_impls[b as usize] {
            return id;
        }
        let module = self.prelude.unwrap_or(self.cur_mod);
        let id = self.push_codec_impl(tr, TypeId::NEVER, Vec::new(), Vec::new(), module, Span::default(), (Ctor::Unit, 0));
        self.impls[id as usize].codec = Some(Arc::new(CodecImpl::default()));
        self.never_impls[b as usize] = Some(id);
        id
    }

    /// Finds the bounds of each structural impl in `ids` with greatest
    /// fixed-point iteration, so that mutually recursive types implement the
    /// trait unless a part refutes it. Removes the impls whose parts do not
    /// implement the trait.
    fn find_codec_bounds(&mut self, ids: &[ImplId]) {
        // The impls to check again when the impl of a type definition changes.
        let mut dependents: HashMap<TypeDefId, Vec<usize>> = HashMap::new();
        for (i, &id) in ids.iter().enumerate() {
            let imp = self.impls[id as usize].clone();
            for (t, _) in self.codec_needs(&imp).unwrap_or_default() {
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
            let changed = match self.codec_requirements(id) {
                Ok(reqs) => {
                    let mut changed = false;
                    for r in reqs {
                        if !self.impls[id as usize].evidence.contains(&r) {
                            self.impls[id as usize].evidence.push(r);
                            changed = true;
                        }
                    }
                    changed
                }
                Err(part) => {
                    let reason = self.refuting_part(id, part);
                    let imp = &self.impls[id as usize];
                    if let (Ty::Named(d, _), Some(reason)) = (self.arena.get(imp.target), reason) {
                        self.codec_refuted.insert((imp.trait_ref.trait_id, d), reason.into());
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

    /// Returns the bounds on its type parameters that structural impl `id`
    /// needs for its parts and default helpers, or the index of the first
    /// need that does not hold.
    fn codec_requirements(&mut self, id: ImplId) -> Result<Vec<(TypeId, TraitRef)>, usize> {
        let imp = self.impls[id as usize].clone();
        let needs = self.codec_needs(&imp).ok_or(usize::MAX)?;
        let evidence = imp.evidence.iter().map(|&(t, tr)| (t, tr, mk_dict(HKind::Error))).collect();
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        let outer = self.codec_reqs.replace(CodecReqs { generics: imp.generics.clone(), found: Vec::new() });
        let failed = needs.into_iter().position(|(t, tr)| match self.resolve_evidence(t, tr, 0) {
            Ok(_) => false,
            Err(m) => !m.unknown,
        });
        let reqs = std::mem::replace(&mut self.codec_reqs, outer).map(|r| r.found).unwrap_or_default();
        self.fs.evidence = saved;
        match failed {
            Some(i) => Err(i),
            None => Ok(reqs),
        }
    }

    /// Describes need `part` of structural impl `id`, which does not hold,
    /// such as "field `run` of `Job` has type `fn() -> int`".
    fn refuting_part(&mut self, id: ImplId, part: usize) -> Option<String> {
        let imp = self.impls[id as usize].clone();
        let Ty::Named(d, _) = self.ty(imp.target) else { return None };
        let owner = self.show(imp.target);
        let decode = self.traits[imp.trait_ref.trait_id as usize].builtin == Some(Builtin::Decode);
        let (t, tr) = *self.codec_needs(&imp)?.get(part)?;
        let mut inner = None;
        self.arena.any(t, &mut |x| {
            if let Ty::Named(e, _) = x
                && e != d
                && inner.is_none()
            {
                inner = self.codec_refuted.get(&(tr.trait_id, e)).cloned();
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

    /// Resolves the dictionary of each part and default helper of
    /// structural impl `id` from the bounds of the impl.
    fn resolve_codec_parts(&mut self, id: ImplId) {
        let imp = self.impls[id as usize].clone();
        let Some(needs) = self.codec_needs(&imp) else {
            let msg = format!("the parts of {} cannot be encoded or decoded", self.show(imp.target));
            self.error(imp.span, msg);
            return;
        };
        let mut evidence = Vec::new();
        let mut slot = CODEC_BOUNDS;
        for &(t, tr) in &imp.evidence {
            let e = match self.has_dict(&tr) {
                true => {
                    slot += 1;
                    let obj = Box::new(mk_dict(HKind::Local(0)));
                    mk_dict(HKind::Field { obj, index: slot - 1 })
                }
                false => mk_dict(HKind::Unit),
            };
            evidence.push((t, tr, e));
        }
        let saved = std::mem::replace(&mut self.fs.evidence, evidence);
        let mut resolved = Vec::new();
        let shown = |c: &Checker| format!("`{}` for {}", c.show_trait(&imp.trait_ref), c.show(imp.target));
        for (t, tr) in needs.iter().copied() {
            match self.resolve_evidence(t, tr, 0) {
                Ok(e) => resolved.push(e),
                Err(m) => {
                    let context = format!(" (required by the default method of the impl of {})", shown(self));
                    let msg = self.unmet_message(m, &context);
                    self.error(imp.span, msg);
                    resolved.push(mk_dict(HKind::Error));
                }
            }
        }
        self.fs.evidence = saved;
        let codec = self.split_codec_needs(&imp, resolved);
        self.impls[id as usize].codec = Some(Arc::new(codec));
    }

    /// Returns each `(type, trait)` dictionary that structural impl `imp`
    /// needs: its parts, then for `Decode` the dictionaries of its default
    /// helpers and of `init`. Returns `None` if the target type cannot have
    /// a structural impl.
    fn codec_needs(&mut self, imp: &ImplDef) -> Option<Vec<(TypeId, TraitRef)>> {
        let tr = imp.trait_ref;
        let decode = self.traits[tr.trait_id as usize].builtin == Some(Builtin::Decode);
        let mut needs: Vec<(TypeId, TraitRef)> = Vec::new();
        let target = self.resolve(imp.target);
        match self.ty(target) {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Range | Ty::Never => {}
            Ty::List(x) | Ty::Set(x) => needs.push((x, tr)),
            Ty::Map(k, v) => needs.extend([(k, tr), (v, tr)]),
            Ty::Tuple(l) => needs.extend(self.elems(l).into_iter().map(|t| (t, tr))),
            Ty::Named(d, args) => {
                let args = self.elems(args);
                let def = self.types[d as usize].clone();
                if matches!(def.codecs.map(|c| if decode { c.decode.map(|_| ()) } else { c.encode.map(|_| ()) }), Some(Some(())))
                    && self.codec_form(d, if decode { Builtin::Decode } else { Builtin::Encode }) == Some(false)
                {
                    return Some(needs);
                }
                let inst = |c: &mut Checker, t: TypeId, gens: &[ParamId]| c.arena.instantiate(t, gens, &args[..gens.len().min(args.len())]);
                match &def.kind {
                    TypeDefKind::Struct { fields } => match self.init_method(Ctor::Def(d)).filter(|_| decode) {
                        Some(init) => {
                            let fi = self.fns[init as usize].clone();
                            if fi.generics.len() != def.generics.len() {
                                return None;
                            }
                            for p in &fi.params {
                                let t = inst(self, p.ty, &fi.generics);
                                needs.push((t, tr));
                            }
                            for p in &fi.params {
                                if let Some(DefaultArg::Helper(h)) = &p.default {
                                    for &(t, htr) in &self.fns[*h as usize].dicts.clone() {
                                        let t = inst(self, t, &fi.generics);
                                        let htr = htr
                                            .subst_params(&mut self.arena, &|q| fi.generics.iter().position(|&g| g == q).map(|i| args[i]));
                                        needs.push((t, htr));
                                    }
                                }
                            }
                            for &(t, itr) in &fi.dicts {
                                let t = inst(self, t, &fi.generics);
                                let itr = itr.subst_params(&mut self.arena, &|q| fi.generics.iter().position(|&g| g == q).map(|i| args[i]));
                                needs.push((t, itr));
                            }
                        }
                        None => {
                            for f in fields {
                                let t = inst(self, f.ty, &def.generics);
                                needs.push((t, tr));
                            }
                            if decode {
                                for f in fields {
                                    for &(t, htr) in &f.default.map(|h| self.fns[h as usize].dicts.clone()).unwrap_or_default() {
                                        let t = inst(self, t, &def.generics);
                                        let htr = htr
                                            .subst_params(&mut self.arena, &|q| def.generics.iter().position(|&g| g == q).map(|i| args[i]));
                                        needs.push((t, htr));
                                    }
                                }
                            }
                        }
                    },
                    TypeDefKind::Enum { variants } => {
                        for v in variants {
                            for &t in &v.fields {
                                let t = inst(self, t, &def.generics);
                                needs.push((t, tr));
                            }
                        }
                    }
                    TypeDefKind::Opaque(Opaque::Value(_)) => {}
                    TypeDefKind::Opaque(_) => return None,
                }
            }
            _ => return None,
        }
        Some(needs)
    }

    /// Splits the resolved dictionaries of [`Checker::codec_needs`] into the
    /// parts, default helpers, and `init` of a [`CodecImpl`].
    fn split_codec_needs(&mut self, imp: &ImplDef, resolved: Vec<HExpr>) -> CodecImpl {
        let decode = self.traits[imp.trait_ref.trait_id as usize].builtin == Some(Builtin::Decode);
        let mut resolved = resolved.into_iter();
        let mut take = |n: usize| resolved.by_ref().take(n).collect::<Vec<_>>();
        let Ty::Named(d, _) = self.ty(imp.target) else {
            let n = self.codec_needs(imp).map_or(0, |n| n.len());
            return CodecImpl { parts: take(n), ..CodecImpl::default() };
        };
        let def = self.types[d as usize].clone();
        if def.codecs.is_some() && self.codec_form(d, if decode { Builtin::Decode } else { Builtin::Encode }) == Some(false) {
            return CodecImpl::default();
        }
        match &def.kind {
            TypeDefKind::Struct { fields } => match self.init_method(Ctor::Def(d)).filter(|_| decode) {
                Some(init) => {
                    let fi = self.fns[init as usize].clone();
                    let parts = take(fi.params.len());
                    let mut fills = Vec::new();
                    let mut consts = Vec::new();
                    for p in &fi.params {
                        match &p.default {
                            Some(DefaultArg::Helper(h)) => {
                                fills.push(take(self.fns[*h as usize].dicts.len()));
                                consts.push(None);
                            }
                            Some(DefaultArg::Inline(e)) => {
                                fills.push(Vec::new());
                                consts.push(closed_const(e));
                            }
                            None => {
                                fills.push(Vec::new());
                                consts.push(None);
                            }
                        }
                    }
                    let dicts = take(fi.dicts.len());
                    CodecImpl { parts, fills, init: Some(CodecInit { func: init, dicts, consts }) }
                }
                None => {
                    let parts = take(fields.len());
                    let fills = match decode {
                        true => fields.iter().map(|f| take(f.default.map_or(0, |h| self.fns[h as usize].dicts.len()))).collect(),
                        false => Vec::new(),
                    };
                    CodecImpl { parts, fills, init: None }
                }
            },
            TypeDefKind::Enum { variants } => {
                CodecImpl { parts: take(variants.iter().map(|v| v.fields.len()).sum()), ..CodecImpl::default() }
            }
            TypeDefKind::Opaque(_) => CodecImpl::default(),
        }
    }
}

/// Returns `true` if `imp` is an impl of `Encode` or `Decode` that uses the
/// native default method of the trait.
pub(crate) fn is_structural(traits: &PVec<TraitDef>, imp: &ImplDef) -> bool {
    let td = &traits[imp.trait_ref.trait_id as usize];
    matches!(td.builtin, Some(Builtin::Encode | Builtin::Decode))
        && td.methods.first().and_then(|m| m.default) == imp.slots.first().copied()
}

fn mk_dict(kind: HKind) -> HExpr {
    super::expr::mk(kind, TypeId::UNIT, Span::default())
}

/// Returns the value of the closed constant expression `e`, which a
/// parameter default declares.
fn closed_const(e: &ast::Expr) -> Option<ConstVal> {
    use ast::ExprKind;
    Some(match &e.kind {
        ExprKind::Unit => ConstVal::Unit,
        ExprKind::Bool(b) => ConstVal::Bool(*b),
        ExprKind::Int(i) => ConstVal::Int(*i),
        ExprKind::Float(f) => ConstVal::Float(*f),
        ExprKind::Duration(d) => ConstVal::Dur(*d),
        ExprKind::Bytes(b) => ConstVal::Bytes(b.clone()),
        ExprKind::Str(segs) => ConstVal::Str(
            segs.iter()
                .map(|s| match s {
                    ast::StrSeg::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Option<String>>()?,
        ),
        ExprKind::List(xs) if xs.is_empty() => ConstVal::List(Vec::new()),
        ExprKind::Map(xs) if xs.is_empty() => ConstVal::Map(Vec::new()),
        ExprKind::Unary(ast::UnOp::Neg, inner) => match closed_const(inner)? {
            ConstVal::Int(i) => ConstVal::Int(i.checked_neg()?),
            ConstVal::Float(f) => ConstVal::Float(-f),
            ConstVal::Dur(d) => ConstVal::Dur(d.checked_neg()?),
            _ => return None,
        },
        _ => return None,
    })
}
