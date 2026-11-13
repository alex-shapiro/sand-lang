//! Synthesized implementations of `Encode` and `Decode`.
//!
//! A struct, enum, tuple, or native value type that has no `impl` of `Encode`
//! or `Decode` derives one from its components (`derive.rs`). The checker
//! registers an ordinary `impl` for it, whose methods have bodies that this
//! module builds as typed HIR ([`FnDeclRef::Derived`]). The bodies call the
//! methods of the prelude's `Encoder` and `Decoder` and the `Encode` and
//! `Decode` implementations of the components.

use super::derive::Derived;
use super::expr::mk;
use super::traits::{add_impl, impl_key};
use super::*;
use crate::codec::fast::{Schema, SchemaId};

/// The deepest type that has a native impl of `Encode` or `Decode`.
const MAX_FAST_TYPE_DEPTH: usize = 64;

/// A derived method to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedMethod {
    Encode,
    Decode,
    /// `encode` of a native impl, which walks the schema with this id.
    FastEncode(SchemaId),
    /// `decode` of a native impl, which walks the schema with this id.
    FastDecode(SchemaId),
}

impl Checker {
    /// Returns the type parameter indices of constructor `c` (with `n` type
    /// arguments) that a hand-written or synthesized `impl` of built-in trait
    /// `b` requires, `Some(None)` if the `impl` cannot be derived through, or
    /// `None` if no `impl` exists.
    pub(crate) fn codec_impl_params(&self, c: Ctor, n: usize, b: Builtin) -> Option<Option<Vec<u32>>> {
        let tr = self.builtin_trait(b)?;
        let ids = self.impls_by.get(&(tr.trait_id, c, if c == Ctor::Tuple { n as u32 } else { 0 }))?;
        let &id = ids.first()?;
        let imp = &self.impls[id as usize];
        let args: Vec<TypeId> = match self.arena.get(imp.target) {
            Ty::List(x) | Ty::Set(x) => vec![x],
            Ty::Map(k, v) => vec![k, v],
            Ty::Tuple(l) | Ty::Named(_, l) => self.arena.list(l).to_vec(),
            _ => Vec::new(),
        };
        let mut required = Vec::new();
        for (i, &a) in args.iter().enumerate() {
            let Ty::Param(p) = self.arena.get(a) else { return Some(None) };
            if !imp.generics.contains(&p) {
                return Some(None);
            }
            if imp.evidence.iter().any(|&(t, x)| self.arena.get(t) == Ty::Param(p) && x.trait_id == tr.trait_id) {
                required.push(i as u32);
            }
        }
        // A bound on another trait with methods cannot be derived through.
        if imp.evidence.iter().any(|(_, x)| x.trait_id != tr.trait_id && self.has_dict(x)) {
            return Some(None);
        }
        Some(Some(required))
    }

    /// Returns `true` if a hand-written `impl` of `b` exists for type
    /// definition `d`.
    pub(crate) fn has_codec_impl(&self, d: TypeDefId, b: Builtin) -> bool {
        self.builtin_trait(b).is_some_and(|tr| self.impls_by.get(&(tr.trait_id, Ctor::Def(d), 0)).is_some_and(|ids| !ids.is_empty()))
    }

    /// Registers the derived `impl` blocks of `Encode` and `Decode` for the
    /// type definitions from `first`.
    pub(crate) fn synthesize_codec_impls(&mut self, first: usize, skip: &HashSet<TypeDefId>) {
        for d in (first..self.types.len()).filter(|&d| !skip.contains(&(d as TypeDefId))) {
            for b in [Builtin::Encode, Builtin::Decode] {
                if self.has_codec_impl(d as TypeDefId, b) {
                    continue;
                }
                let Some(Derived::Needs(required)) = self.derived.get(d).map(|x| x[b as usize].clone()) else { continue };
                let generics = self.types[d].generics.clone();
                let required: Vec<ParamId> = required.iter().map(|&i| generics[i as usize]).collect();
                self.add_derived_impl(b, Ctor::Def(d as TypeDefId), &generics, &required, &|c, ps| {
                    let args: Vec<TypeId> = ps.iter().map(|&p| c.arena.param(p)).collect();
                    c.arena.named(d as TypeDefId, &args)
                });
            }
        }
    }

    /// Returns the derived `impl` of `b` for tuples of `n` elements, and
    /// registers it on first use.
    pub(crate) fn tuple_codec_impl(&mut self, b: Builtin, n: u32) {
        let Some(tr) = self.builtin_trait(b) else { return };
        if self.impls_by.get(&(tr.trait_id, Ctor::Tuple, n)).is_some_and(|ids| !ids.is_empty()) {
            return;
        }
        let generics: Vec<ParamId> = (0..n)
            .map(|i| {
                let name = self.sym(&format!("T{i}"));
                self.new_param(name)
            })
            .collect();
        self.add_derived_impl(b, Ctor::Tuple, &generics, &generics.clone(), &|c, ps| {
            let args: Vec<TypeId> = ps.iter().map(|&p| c.arena.param(p)).collect();
            c.arena.tuple(&args)
        });
    }

    /// Registers an `impl` of `b` for the type that `target` builds from the
    /// `impl` type parameters, which copy `generics` and add `b` to those in
    /// `required`.
    fn add_derived_impl(
        &mut self,
        b: Builtin,
        ctor: Ctor,
        generics: &[ParamId],
        required: &[ParamId],
        target: &dyn Fn(&mut Checker, &[ParamId]) -> TypeId,
    ) {
        let Some(tr) = self.builtin_trait(b) else { return };
        let ps: Vec<ParamId> = generics
            .iter()
            .map(|&g| {
                let name = self.params[g as usize].name;
                let mut traits = self.params[g as usize].traits.clone();
                if required.contains(&g) && !traits.contains(&tr) {
                    traits.push(tr);
                }
                self.params.push(ParamInfo { name, traits });
                (self.params.len() - 1) as ParamId
            })
            .collect();
        let target = target(self, &ps);
        // A use of the impl proves the type's own bounds too, such as `Hash`
        // for the elements of a `Set<T>` field.
        let mut evidence = Vec::new();
        for &p in &ps {
            let pt = self.param_ty(p);
            evidence.extend(self.params[p as usize].traits.iter().map(|&x| (pt, x)));
        }
        let arity = match ctor {
            Ctor::Tuple => generics.len() as u32,
            _ => 0,
        };
        let id = self.impls.len() as ImplId;
        let span = match ctor {
            Ctor::Def(d) => self.types[d as usize].span,
            _ => Span::default(),
        };
        let module = match ctor {
            Ctor::Def(d) => self.types[d as usize].module,
            _ => self.cur_mod,
        };
        self.impls.push(ImplDef {
            trait_ref: tr,
            target,
            generics: ps,
            module,
            span,
            slots: Vec::new(),
            evidence,
            replaced: false,
            derived: true,
            fast: false,
        });
        add_impl(&mut self.impls_by, (tr.trait_id, ctor, arity), id);
        add_impl(&mut self.impls_by_ctor, (ctor, arity), id);
    }

    /// Declares the methods of the synthesized `impl` `id` on first use. A
    /// program pays for the code of only the impls that it uses.
    pub(crate) fn ensure_derived_methods(&mut self, id: ImplId) {
        let imp = &self.impls[id as usize];
        if !imp.derived || !imp.slots.is_empty() {
            return;
        }
        let method = match self.traits[imp.trait_ref.trait_id as usize].builtin {
            Some(Builtin::Decode) => DerivedMethod::Decode,
            _ => DerivedMethod::Encode,
        };
        self.add_codec_methods(id, method);
    }

    /// Declares the methods of the `impl` `id` of `Encode` or `Decode`, whose
    /// first method is `method`, and fills its slots.
    fn add_codec_methods(&mut self, id: ImplId, method: DerivedMethod) {
        let imp = &self.impls[id as usize];
        let (tr, target, ps, module, span) = (imp.trait_ref, imp.target, imp.generics.clone(), imp.module, imp.span);
        let ctor = impl_key(&self.arena, target).map_or(Ctor::Unit, |(c, _)| c);
        let (name, params, ret, has_self) = match method {
            DerivedMethod::Encode | DerivedMethod::FastEncode(_) => {
                let enc = self.codec_type("Encoder");
                let params = vec![
                    ParamSig { name: Symbol::SELF, ty: target, default: None },
                    ParamSig { name: self.sym("e"), ty: enc, default: None },
                ];
                (self.sym("encode"), params, TypeId::UNIT, true)
            }
            DerivedMethod::Decode | DerivedMethod::FastDecode(_) => {
                let dec = self.codec_type("Decoder");
                let err = self.error_ty();
                let ret = self.result_of(target, err);
                (self.sym("decode"), vec![ParamSig { name: self.sym("d"), ty: dec, default: None }], ret, false)
            }
        };
        let f = self.add_fn(FnInfo {
            name,
            module,
            generics: ps.clone(),
            outer: ps.len(),
            params,
            ret,
            has_self,
            kind: FnKind::Script,
            doc: None,
            span,
            is_pub: true,
            owner: Some(ctor),
            self_ty: Some(target),
            decl: Some(FnDeclRef::Derived(method)),
            dicts: vec![(target, tr)],
            impl_of: Some(id),
            trait_of: None,
        });
        let mut slots = vec![f];
        // Default methods, such as `decode_missing`, fill the other slots.
        slots.extend(self.traits[tr.trait_id as usize].methods.iter().skip(1).map(|m| m.default.unwrap_or(m.sig)));
        self.impls[id as usize].slots = slots;
    }

    /// Returns the native `impl` of `b` (`Encode` or `Decode`) for the closed
    /// type `ty`, and registers it on first use. Only a list, set, map,
    /// tuple, struct, or enum other than `Option` has one, and only if
    /// every type in it has a synthesized or built-in implementation of `b`
    /// (and, for `Decode`, no field default). The impl's method walks the
    /// value natively, with the same format operations as the Sand code.
    pub(crate) fn fast_codec_impl(&mut self, ty: TypeId, b: Builtin) -> Option<ImplId> {
        if !self.codec_fast_path || self.arena.has_params(ty) || self.arena.has_vars(ty) {
            return None;
        }
        match self.arena.get(ty) {
            Ty::List(_) | Ty::Set(_) | Ty::Map(..) | Ty::Tuple(_) => {}
            Ty::Named(d, _)
                if d != self.wk.option && !self.is_json_def(d) && !matches!(self.types[d as usize].kind, TypeDefKind::Opaque(_)) => {}
            _ => return None,
        }
        let decode = b == Builtin::Decode;
        if let Some(&known) = self.fast_impls.get(&(ty, decode)) {
            return known;
        }
        let found = self.codec_is_native(ty, b).then(|| self.add_fast_impl(ty, b)).flatten();
        self.fast_impls.insert((ty, decode), found);
        found
    }

    /// Returns `true` if every type in the closed type `root` has a
    /// synthesized or built-in implementation of `b` and satisfies the bounds
    /// of its type constructor, and, for `Decode`, no struct in it has a
    /// field default.
    fn codec_is_native(&mut self, root: TypeId, b: Builtin) -> bool {
        let decode = b == Builtin::Decode;
        let Some(hash) = self.builtin_trait(Builtin::Hash) else { return false };
        let mut todo = vec![root];
        let mut seen = HashSet::new();
        while let Some(t) = todo.pop() {
            if !seen.insert(t) || self.fast_impls.get(&(t, decode)).is_some_and(Option::is_some) {
                continue;
            }
            // Types that grow without end, such as `P<[T]>` in `P<T>`, are
            // left to the synthesized impls.
            if !self.budget.charge(1) || self.type_depth(t, MAX_FAST_TYPE_DEPTH) >= MAX_FAST_TYPE_DEPTH {
                return false;
            }
            let mut bounds: Vec<(TypeId, TraitRef)> = Vec::new();
            match self.arena.get(t) {
                // The verifier proves tuple bounds with the synthesized impl.
                Ty::Tuple(l) => {
                    let n = self.arena.list_len(l) as u32;
                    self.tuple_codec_impl(b, n);
                }
                Ty::Set(x) | Ty::Map(x, _) => bounds.push((x, hash)),
                Ty::Named(d, args) if d != self.wk.option && !self.is_json_def(d) => {
                    let def = &self.types[d as usize];
                    let derived = self.builtin_trait(b).is_some_and(|tr| {
                        let ids = self.impls_by.get(&(tr.trait_id, Ctor::Def(d), 0));
                        ids.and_then(|ids| ids.first()).is_some_and(|&i| self.impls[i as usize].derived)
                    });
                    if !derived || (decode && def.fields().iter().any(|f| f.default.is_some())) {
                        return false;
                    }
                    let gens = def.generics.clone();
                    for (g, a) in gens.into_iter().zip(self.arena.list(args).to_vec()) {
                        bounds.extend(self.params[g as usize].traits.iter().map(|&tr| (a, tr)));
                    }
                }
                _ => {}
            }
            for (x, tr) in bounds {
                if self.implements(x, tr) != Some(true) {
                    return false;
                }
            }
            match self.codec_parts(t) {
                Some(parts) => todo.extend(parts),
                None => return false,
            }
        }
        true
    }

    /// Returns the nesting depth of type `t`, up to `cap`.
    fn type_depth(&self, t: TypeId, cap: usize) -> usize {
        if cap == 0 {
            return 0;
        }
        let below = |ts: &[TypeId]| ts.iter().map(|&x| self.type_depth(x, cap - 1)).max().unwrap_or(0);
        1 + match self.arena.get(t) {
            Ty::List(x) | Ty::Set(x) => below(&[x]),
            Ty::Map(k, v) => below(&[k, v]),
            Ty::Tuple(l) | Ty::Named(_, l) => below(self.arena.list(l)),
            Ty::Fn(l, r) => below(self.arena.list(l)).max(below(&[r])),
            _ => 0,
        }
    }

    /// Returns `true` if `d` is `json.Json`, whose impls are native.
    fn is_json_def(&self, d: TypeDefId) -> bool {
        let def = &self.types[d as usize];
        self.name(def.name) == "Json"
            && self.name(def.module_name) == "json"
            && self.builtin_trait(Builtin::Encode).is_some_and(|tr| {
                let ids = self.impls_by.get(&(tr.trait_id, Ctor::Def(d), 0));
                ids.and_then(|ids| ids.first())
                    .and_then(|&i| self.impls[i as usize].slots.first())
                    .is_some_and(|&f| matches!(self.fns[f as usize].kind, FnKind::Native(_)))
            })
    }

    /// Returns the types that the schema of `t` refers to, or `None` if `t`
    /// has no schema.
    fn codec_parts(&mut self, t: TypeId) -> Option<Vec<TypeId>> {
        Some(match self.arena.get(t) {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Duration | Ty::Unit | Ty::Range => Vec::new(),
            Ty::List(x) | Ty::Set(x) => vec![x],
            Ty::Map(k, v) => vec![k, v],
            Ty::Tuple(l) => self.arena.list(l).to_vec(),
            Ty::Named(d, args) => {
                let args = self.arena.list(args).to_vec();
                if d == self.wk.option || self.is_json_def(d) {
                    return Some(if d == self.wk.option { args } else { Vec::new() });
                }
                match self.types[d as usize].kind.clone() {
                    TypeDefKind::Struct { fields } => (0..fields.len()).map(|i| self.field_type(d, i, &args)).collect(),
                    TypeDefKind::Enum { variants } => {
                        let gens = self.types[d as usize].generics.clone();
                        variants.iter().flat_map(|v| v.fields.clone()).map(|t| self.arena.instantiate(t, &gens, &args)).collect()
                    }
                    TypeDefKind::Opaque(Opaque::Value(_)) => Vec::new(),
                    TypeDefKind::Opaque(_) => return None,
                }
            }
            _ => return None,
        })
    }

    /// Registers the native impl of `b` for `ty` and its schema.
    fn add_fast_impl(&mut self, ty: TypeId, b: Builtin) -> Option<ImplId> {
        let tr = self.builtin_trait(b)?;
        let schema = self.schema_of(ty)?;
        let id = self.impls.len() as ImplId;
        self.impls.push(ImplDef {
            trait_ref: tr,
            target: ty,
            generics: Vec::new(),
            module: self.cur_mod,
            span: Span::default(),
            slots: Vec::new(),
            evidence: Vec::new(),
            replaced: false,
            derived: false,
            fast: true,
        });
        let method = if b == Builtin::Decode { DerivedMethod::FastDecode(schema) } else { DerivedMethod::FastEncode(schema) };
        self.add_codec_methods(id, method);
        Some(id)
    }

    /// Returns the id of the schema of the closed type `root`, and adds the
    /// schemas of the types in it that have none yet.
    fn schema_of(&mut self, root: TypeId) -> Option<SchemaId> {
        let mut added = Vec::new();
        let mut todo = vec![root];
        while let Some(t) = todo.pop() {
            if self.schema_ids.contains_key(&t) {
                continue;
            }
            let id = self.schemas.len() as SchemaId;
            self.schemas.push(Schema::Unit);
            self.schema_ids.insert(t, id);
            added.push((t, id));
            todo.extend(self.codec_parts(t)?);
        }
        for (t, id) in added {
            let node = self.schema_node(t)?;
            *self.schemas.get_mut(id as usize) = node;
        }
        self.schema_ids.get(&root).copied()
    }

    /// Builds the schema of `t` from the schemas of its parts.
    fn schema_node(&mut self, t: TypeId) -> Option<Schema> {
        let parts = self.codec_parts(t)?;
        let ids: Vec<SchemaId> = parts.iter().map(|p| self.schema_ids.get(p).copied()).collect::<Option<_>>()?;
        Some(match self.arena.get(t) {
            Ty::Int => Schema::Int,
            Ty::Float => Schema::Float,
            Ty::Bool => Schema::Bool,
            Ty::Str => Schema::Str,
            Ty::Bytes => Schema::Bytes,
            Ty::Duration => Schema::Duration,
            Ty::Unit => Schema::Unit,
            Ty::Range => Schema::Range,
            Ty::List(_) => Schema::List(ids[0]),
            Ty::Set(_) => Schema::Set(ids[0]),
            Ty::Map(..) => Schema::Map(ids[0], ids[1]),
            Ty::Tuple(_) => Schema::Tuple(ids.into()),
            Ty::Named(d, _) if d == self.wk.option => Schema::Option(ids[0]),
            Ty::Named(d, _) if self.is_json_def(d) => Schema::Json,
            Ty::Named(d, _) => match &self.types[d as usize].kind {
                TypeDefKind::Struct { .. } => Schema::Struct(d, ids.into()),
                TypeDefKind::Enum { variants } => {
                    let mut rest = &ids[..];
                    let mut payloads = Vec::with_capacity(variants.len());
                    for v in variants {
                        let (now, later) = rest.split_at(v.fields.len());
                        payloads.push(now.into());
                        rest = later;
                    }
                    Schema::Enum(variants.first().map_or(0, |v| v.rt_id), payloads.into())
                }
                TypeDefKind::Opaque(_) => Schema::Native(d),
            },
            _ => return None,
        })
    }

    /// Replaces each argument of native function `f` whose Rust type is
    /// `Encoded<N>` with its encoding, `encode_value(arg)`. `args` starts with
    /// the hidden arguments, and `targs` instantiates `f`.
    pub(crate) fn encode_native_args(&mut self, f: FnId, targs: &[TypeId], args: &mut [HArg], span: Span) {
        let Some(positions) = self.encoded_params.get(&f).cloned() else { return };
        let Some(Def::Fn(encode)) = self.prelude.and_then(|p| self.scope_def(p, "encode_value")) else { return };
        let Some(tr) = self.builtin_trait(Builtin::Encode) else { return };
        let hidden = self.fns[f as usize].dicts.len();
        let gens = self.fns[f as usize].generics.clone();
        let encoder = self.codec_type("Encoder");
        for i in positions.iter() {
            let ty = self.fns[f as usize].params[*i as usize].ty;
            let ty = self.arena.instantiate(ty, &gens, targs);
            let Some(HArg::Expr(arg)) = args.get_mut(hidden + *i as usize) else { continue };
            let value = std::mem::replace(arg, mk(HKind::Unit, TypeId::UNIT, span));
            let dict = self.evidence_hole(ty, tr, false, span);
            let call = HKind::Call { func: encode, targs: vec![ty], args: vec![HArg::Expr(dict), HArg::Expr(value)] };
            *arg = mk(call, encoder, span);
        }
    }

    /// Returns the type of the prelude type `name`.
    fn codec_type(&mut self, name: &str) -> TypeId {
        match self.prelude.and_then(|p| self.scope_def(p, name)) {
            Some(Def::Type(TyRef::Def(d))) => self.arena.named(d, &[]),
            _ => TypeId::ERROR,
        }
    }

    /// Returns the method `name` of the prelude type `owner`.
    fn codec_method(&mut self, owner: &str, name: &str) -> Option<FnId> {
        let Some(Def::Type(TyRef::Def(d))) = self.prelude.and_then(|p| self.scope_def(p, owner)) else { return None };
        let s = self.syms.get(name)?;
        self.methods.get(&(Ctor::Def(d), s)).and_then(|ids| ids.first().copied())
    }

    /// Builds the body of the derived method `f`, whose parameters are the
    /// locals `params` after its hidden dictionary.
    pub(crate) fn derived_body(&mut self, f: FnId, method: DerivedMethod, params: &[LocalId], span: Span) -> HExpr {
        let target = self.fns[f as usize].self_ty.unwrap_or(TypeId::ERROR);
        let mut b = Synth { span };
        match method {
            DerivedMethod::Encode => {
                let (value, enc) = (params[params.len() - 2], params[params.len() - 1]);
                b.encode_body(self, target, value, enc)
            }
            DerivedMethod::Decode => {
                let dec = params[params.len() - 1];
                b.decode_body(self, target, dec)
            }
            DerivedMethod::FastEncode(schema) => {
                let (value, enc) = (params[params.len() - 2], params[params.len() - 1]);
                let args = vec![b.local(self, enc), b.local(self, value), b.int(schema as i64)];
                b.method(self, "Encoder", "write_fast", vec![target], args)
            }
            DerivedMethod::FastDecode(schema) => {
                let dec = params[params.len() - 1];
                let args = vec![b.local(self, dec), b.int(schema as i64)];
                b.method(self, "Decoder", "read_fast", vec![target], args)
            }
        }
    }
}

/// Builds HIR for a derived method.
struct Synth {
    span: Span,
}

impl Synth {
    fn e(&self, kind: HKind, ty: TypeId) -> HExpr {
        mk(kind, ty, self.span)
    }

    fn unit(&self) -> HExpr {
        self.e(HKind::Unit, TypeId::UNIT)
    }

    fn int(&self, i: i64) -> HExpr {
        self.e(HKind::Int(i), TypeId::INT)
    }

    fn local(&self, c: &Checker, l: LocalId) -> HExpr {
        self.e(HKind::Local(l), c.fs.data[l as usize].ty)
    }

    fn block(&self, items: Vec<HExpr>, ty: TypeId) -> HExpr {
        self.e(HKind::Block(items), ty)
    }

    /// Calls the prelude method `owner.name` with `args`.
    fn method(&self, c: &mut Checker, owner: &str, name: &str, targs: Vec<TypeId>, args: Vec<HExpr>) -> HExpr {
        let Some(f) = c.codec_method(owner, name) else { return self.e(HKind::Error, TypeId::ERROR) };
        let ret = c.fns[f as usize].ret;
        let gens = c.fns[f as usize].generics.clone();
        let ret = c.arena.instantiate(ret, &gens, &targs);
        let kind = c.call_kind(f, targs, args.into_iter().map(HArg::Expr).collect());
        self.e(kind, ret)
    }

    /// `e?` for a `Result` with payload type `ty`.
    fn try_(&self, e: HExpr, ty: TypeId) -> HExpr {
        self.e(HKind::Try { e: Box::new(e), kind: TryKind::Result }, ty)
    }

    /// Calls method `slot` of built-in trait `b` for type `ty`.
    fn trait_call(&self, c: &mut Checker, b: Builtin, slot: u32, ty: TypeId, args: Vec<HExpr>, ret: TypeId) -> HExpr {
        let Some(tr) = c.builtin_trait(b) else { return self.e(HKind::Error, ret) };
        let dict = c.evidence_hole(ty, tr, false, self.span);
        let mut all = vec![HArg::Expr(dict)];
        all.extend(args.into_iter().map(HArg::Expr));
        self.e(HKind::CallTrait { trait_id: tr.trait_id, slot, targs: vec![ty], args: all, impl_id: None }, ret)
    }

    fn encode(&self, c: &mut Checker, ty: TypeId, value: HExpr, enc: LocalId) -> HExpr {
        let e = self.local(c, enc);
        self.trait_call(c, Builtin::Encode, 0, ty, vec![value, e], TypeId::UNIT)
    }

    /// `T.decode(d)?` for type `ty`.
    fn decode(&self, c: &mut Checker, ty: TypeId, dec: LocalId) -> HExpr {
        let err = c.error_ty();
        let ret = c.result_of(ty, err);
        let d = self.local(c, dec);
        let call = self.trait_call(c, Builtin::Decode, 0, ty, vec![d], ret);
        self.try_(call, ty)
    }

    /// Calls `Decoder.name` with the decoder and `args`, and applies `?`.
    fn dec_op(&self, c: &mut Checker, dec: LocalId, name: &str, args: Vec<HExpr>, ty: TypeId) -> HExpr {
        let mut all = vec![self.local(c, dec)];
        all.extend(args);
        let call = self.method(c, "Decoder", name, Vec::new(), all);
        self.try_(call, ty)
    }

    fn enc_op(&self, c: &mut Checker, enc: LocalId, name: &str, args: Vec<HExpr>) -> HExpr {
        let mut all = vec![self.local(c, enc)];
        all.extend(args);
        self.method(c, "Encoder", name, Vec::new(), all)
    }

    fn encode_body(&mut self, c: &mut Checker, target: TypeId, value: LocalId, enc: LocalId) -> HExpr {
        let mut out = Vec::new();
        match c.arena.get(target) {
            Ty::Tuple(l) => {
                let elems = c.arena.list(l).to_vec();
                out.push(self.enc_op(c, enc, "begin_tuple", vec![self.int(elems.len() as i64)]));
                for (i, &t) in elems.iter().enumerate() {
                    let x = self.e(HKind::Field { obj: Box::new(self.local(c, value)), index: i as u32 }, t);
                    out.push(self.encode(c, t, x, enc));
                }
                out.push(self.enc_op(c, enc, "end", vec![]));
            }
            Ty::Named(d, args) => {
                let args = c.arena.list(args).to_vec();
                match c.types[d as usize].kind.clone() {
                    TypeDefKind::Struct { fields } => {
                        out.push(self.enc_op(c, enc, "begin_struct", vec![self.int(d as i64)]));
                        for i in 0..fields.len() {
                            let t = c.field_type(d, i, &args);
                            out.push(self.enc_op(c, enc, "field", vec![self.int(i as i64)]));
                            let x = self.e(HKind::Field { obj: Box::new(self.local(c, value)), index: i as u32 }, t);
                            out.push(self.encode(c, t, x, enc));
                        }
                        out.push(self.enc_op(c, enc, "end", vec![]));
                    }
                    TypeDefKind::Enum { variants } => {
                        let gens = c.types[d as usize].generics.clone();
                        let mut arms = Vec::new();
                        for v in &variants {
                            let tys: Vec<TypeId> = v.fields.iter().map(|&t| c.arena.instantiate(t, &gens, &args)).collect();
                            let binds: Vec<LocalId> = tys.iter().map(|&t| c.hidden_local(t, self.span)).collect();
                            let pats =
                                binds.iter().zip(&tys).map(|(&l, &t)| HPat { kind: HPatKind::Bind(l), ty: t, span: self.span }).collect();
                            let pat = HPat { kind: HPatKind::Variant { rt_id: v.rt_id, args: pats }, ty: target, span: self.span };
                            let body = if tys.is_empty() {
                                self.enc_op(c, enc, "write_variant", vec![self.int(v.rt_id as i64)])
                            } else {
                                let mut items = vec![self.enc_op(c, enc, "begin_variant", vec![self.int(v.rt_id as i64)])];
                                for (j, (&l, &t)) in binds.iter().zip(&tys).enumerate() {
                                    if v.field_names.is_some() {
                                        items.push(self.enc_op(c, enc, "field", vec![self.int(j as i64)]));
                                    }
                                    let x = self.local(c, l);
                                    items.push(self.encode(c, t, x, enc));
                                }
                                items.push(self.enc_op(c, enc, "end", vec![]));
                                self.block(items, TypeId::UNIT)
                            };
                            arms.push(HArm { pat, guard: None, body });
                        }
                        let scrut = Box::new(self.local(c, value));
                        out.push(self.e(HKind::Match { scrut, arms }, TypeId::UNIT));
                    }
                    TypeDefKind::Opaque(_) => {
                        let mut all = vec![self.local(c, enc), self.local(c, value)];
                        let call = self.method(c, "Encoder", "write_native", vec![target], std::mem::take(&mut all));
                        out.push(call);
                    }
                }
            }
            _ => out.push(self.e(HKind::Error, TypeId::UNIT)),
        }
        self.block(out, TypeId::UNIT)
    }

    fn decode_body(&mut self, c: &mut Checker, target: TypeId, dec: LocalId) -> HExpr {
        let err = c.error_ty();
        let ret = c.result_of(target, err);
        let ok = c.types[c.wk.result as usize].variants()[0].rt_id;
        let value = match c.arena.get(target) {
            Ty::Tuple(l) => {
                let elems = c.arena.list(l).to_vec();
                let mut items = vec![self.dec_op(c, dec, "begin_tuple", vec![self.int(elems.len() as i64)], TypeId::UNIT)];
                let mut locals = Vec::new();
                for &t in &elems {
                    items.push(self.dec_op(c, dec, "element", vec![], TypeId::UNIT));
                    let l = c.hidden_local(t, self.span);
                    let init = self.decode(c, t, dec);
                    items.push(self.let_(l, t, init));
                    locals.push(self.local(c, l));
                }
                items.push(self.dec_op(c, dec, "end_tuple", vec![], TypeId::UNIT));
                items.push(self.e(HKind::Tuple(locals), target));
                self.block(items, target)
            }
            Ty::Named(d, args) => {
                let args = c.arena.list(args).to_vec();
                match c.types[d as usize].kind.clone() {
                    TypeDefKind::Struct { fields } => {
                        let tys: Vec<TypeId> = (0..fields.len()).map(|i| c.field_type(d, i, &args)).collect();
                        let open = self.dec_op(c, dec, "begin_struct", vec![self.int(d as i64)], TypeId::UNIT);
                        let defaults: Vec<Option<FnId>> = fields.iter().map(|f| f.default).collect();
                        let (mut items, values) = self.fields(c, dec, open, (d, false), &tys, &defaults, &args);
                        items.push(self.e(HKind::MakeStruct { def: d, fields: values }, target));
                        self.block(items, target)
                    }
                    TypeDefKind::Enum { variants } => {
                        let gens = c.types[d as usize].generics.clone();
                        let first = variants.first().map_or(0, |v| v.rt_id);
                        let index =
                            self.dec_op(c, dec, "variant", vec![self.int(first as i64), self.int(variants.len() as i64)], TypeId::INT);
                        let mut arms = Vec::new();
                        for (k, v) in variants.iter().enumerate() {
                            let tys: Vec<TypeId> = v.fields.iter().map(|&t| c.arena.instantiate(t, &gens, &args)).collect();
                            let body = if tys.is_empty() {
                                self.e(HKind::Tag(v.rt_id), target)
                            } else if v.field_names.is_some() {
                                let open = self.dec_op(c, dec, "begin_fields", vec![self.int(v.rt_id as i64)], TypeId::UNIT);
                                let (mut items, values) = self.fields(c, dec, open, (v.rt_id, true), &tys, &vec![None; tys.len()], &args);
                                items.push(self.dec_op(c, dec, "end_variant", vec![], TypeId::UNIT));
                                items.push(self.e(HKind::MakeVariant { rt_id: v.rt_id, args: values }, target));
                                self.block(items, target)
                            } else {
                                let mut items = vec![self.dec_op(c, dec, "begin_tuple", vec![self.int(tys.len() as i64)], TypeId::UNIT)];
                                let mut values = Vec::new();
                                for &t in &tys {
                                    items.push(self.dec_op(c, dec, "element", vec![], TypeId::UNIT));
                                    let l = c.hidden_local(t, self.span);
                                    let init = self.decode(c, t, dec);
                                    items.push(self.let_(l, t, init));
                                    values.push(self.local(c, l));
                                }
                                items.push(self.dec_op(c, dec, "end_tuple", vec![], TypeId::UNIT));
                                items.push(self.dec_op(c, dec, "end_variant", vec![], TypeId::UNIT));
                                items.push(self.e(HKind::MakeVariant { rt_id: v.rt_id, args: values }, target));
                                self.block(items, target)
                            };
                            let kind = if k + 1 == variants.len() { HPatKind::Wildcard } else { HPatKind::Int(k as i64) };
                            arms.push(HArm { pat: HPat { kind, ty: TypeId::INT, span: self.span }, guard: None, body });
                        }
                        self.e(HKind::Match { scrut: Box::new(index), arms }, target)
                    }
                    TypeDefKind::Opaque(_) => {
                        let all = vec![self.local(c, dec), self.int(d as i64)];
                        return self.method(c, "Decoder", "read_native", vec![target], all);
                    }
                }
            }
            _ => return self.e(HKind::Error, ret),
        };
        self.e(HKind::MakeVariant { rt_id: ok, args: vec![value] }, ret)
    }

    fn let_(&self, l: LocalId, ty: TypeId, init: HExpr) -> HExpr {
        let pat = HPat { kind: HPatKind::Bind(l), ty, span: self.span };
        self.e(HKind::Let { pat, init: Box::new(init), else_: None }, TypeId::UNIT)
    }

    fn assign(&self, l: LocalId, ty: TypeId, value: HExpr) -> HExpr {
        self.e(HKind::Assign { place: Place::Local(l), op: None, op_ty: ty, value: Box::new(value) }, TypeId::UNIT)
    }

    /// Builds the statements that read the fields of types `tys` after `open`,
    /// fill each omitted field, and return the field values.
    #[allow(clippy::too_many_arguments)]
    fn fields(
        &self,
        c: &mut Checker,
        dec: LocalId,
        open: HExpr,
        owner: (u32, bool),
        tys: &[TypeId],
        defaults: &[Option<FnId>],
        targs: &[TypeId],
    ) -> (Vec<HExpr>, Vec<HExpr>) {
        let mut items = vec![open];
        let slots: Vec<LocalId> = tys.iter().map(|&t| c.hidden_local(t, self.span)).collect();
        let seen: Vec<LocalId> = tys.iter().map(|_| c.hidden_local(TypeId::BOOL, self.span)).collect();
        for (i, &t) in tys.iter().enumerate() {
            // The slot holds no value until the field is read or filled.
            let placeholder = self.e(HKind::Unit, t);
            items.push(self.let_(slots[i], t, placeholder));
            items.push(self.let_(seen[i], TypeId::BOOL, self.e(HKind::Bool(false), TypeId::BOOL)));
        }
        let index = c.hidden_local(TypeId::INT, self.span);
        let mut body = vec![self.let_(index, TypeId::INT, self.dec_op(c, dec, "field", vec![], TypeId::INT))];
        let lt = self.e(HKind::Cmp { op: CmpOp::Lt, l: Box::new(self.local(c, index)), r: Box::new(self.int(0)) }, TypeId::BOOL);
        let brk = self.e(HKind::Break { value: None, depth: 0 }, TypeId::NEVER);
        body.push(self.e(HKind::If { cond: Box::new(lt), then: Box::new(brk), else_: None }, TypeId::UNIT));
        let mut arms = Vec::new();
        for (i, &t) in tys.iter().enumerate() {
            let value = self.decode(c, t, dec);
            let set = vec![self.assign(slots[i], t, value), self.assign(seen[i], TypeId::BOOL, self.e(HKind::Bool(true), TypeId::BOOL))];
            arms.push(HArm {
                pat: HPat { kind: HPatKind::Int(i as i64), ty: TypeId::INT, span: self.span },
                guard: None,
                body: self.block(set, TypeId::UNIT),
            });
        }
        arms.push(HArm { pat: HPat { kind: HPatKind::Wildcard, ty: TypeId::INT, span: self.span }, guard: None, body: self.unit() });
        body.push(self.e(HKind::Match { scrut: Box::new(self.local(c, index)), arms }, TypeId::UNIT));
        items.push(self.e(HKind::Loop(Box::new(self.block(body, TypeId::UNIT))), TypeId::UNIT));
        for (i, &t) in tys.iter().enumerate() {
            let missing = self.missing(c, dec, owner, i, t, defaults.get(i).copied().flatten(), targs);
            let unseen = self.e(HKind::Unary { op: HUnOp::Not, e: Box::new(self.local(c, seen[i])) }, TypeId::BOOL);
            let fill = self.assign(slots[i], t, missing);
            items.push(self.e(HKind::If { cond: Box::new(unseen), then: Box::new(fill), else_: None }, TypeId::UNIT));
        }
        let values = slots.iter().map(|&l| self.local(c, l)).collect();
        (items, values)
    }

    /// The value of the omitted field `i` of type `ty`: its default, `None`
    /// for an `Option`, `T.decode_missing()` for a type parameter, or a
    /// return with a missing-field error.
    #[allow(clippy::too_many_arguments)]
    fn missing(
        &self,
        c: &mut Checker,
        dec: LocalId,
        owner: (u32, bool),
        i: usize,
        ty: TypeId,
        default: Option<FnId>,
        targs: &[TypeId],
    ) -> HExpr {
        if let Some(helper) = default {
            let args = c.default_helper_evidence(helper, targs, self.span);
            return self.e(HKind::Call { func: helper, targs: targs.to_vec(), args }, ty);
        }
        let err = c.error_ty();
        let ret_err = |s: &Self, c: &mut Checker| {
            let args = vec![s.local(c, dec), s.int(owner.0 as i64), s.e(HKind::Bool(owner.1), TypeId::BOOL), s.int(i as i64)];
            let error = s.method(c, "Decoder", "missing", Vec::new(), args);
            let err_id = c.types[c.wk.result as usize].variants()[1].rt_id;
            let fty = c.fs.ctx.last().map_or(TypeId::ERROR, |x| x.ret);
            let e = s.e(HKind::MakeVariant { rt_id: err_id, args: vec![error] }, fty);
            s.e(HKind::Return(Some(Box::new(e))), TypeId::NEVER)
        };
        let none = c.types[c.wk.option as usize].variants()[1].rt_id;
        let some = c.types[c.wk.option as usize].variants()[0].rt_id;
        match c.arena.get(ty) {
            Ty::Named(d, _) if d == c.wk.option => self.e(HKind::Tag(none), ty),
            Ty::Param(_) => {
                let _ = err;
                let opt = c.option_of(ty);
                let call = self.trait_call(c, Builtin::Decode, 1, ty, vec![], opt);
                let v = c.hidden_local(ty, self.span);
                let some_arm = HArm {
                    pat: HPat {
                        kind: HPatKind::Variant { rt_id: some, args: vec![HPat { kind: HPatKind::Bind(v), ty, span: self.span }] },
                        ty: opt,
                        span: self.span,
                    },
                    guard: None,
                    body: self.local(c, v),
                };
                let none_arm =
                    HArm { pat: HPat { kind: HPatKind::Wildcard, ty: opt, span: self.span }, guard: None, body: ret_err(self, c) };
                self.e(HKind::Match { scrut: Box::new(call), arms: vec![some_arm, none_arm] }, ty)
            }
            _ => ret_err(self, c),
        }
    }
}
