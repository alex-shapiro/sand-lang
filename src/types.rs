//! Types, type definitions, bounds and interned names.
//!
//! Types are hash-consed: a [`TypeId`] indexes a node of a [`TypeArena`],
//! and structurally equal types have the same id, so types are `Copy` and
//! compared as integers. Lists of types are interned too ([`TyList`]). Each
//! node records whether it contains inference variables or type parameters,
//! so substitution and zonking skip the types that have none. Names are interned as
//! [`Symbol`]s in [`Symbols`]. Both tables are persistent (see
//! `check::persist`): the standard library's are shared by every
//! compilation that extends them.

use crate::check::persist::{PHash, PVec};
use crate::diag::Span;
use crate::vm::value::FxBuild;
use std::fmt::Write as _;
use std::hash::BuildHasher;
use std::sync::Arc;

pub type TypeDefId = u32;
pub type ParamId = u32;
pub type VarId = u32;
pub type FnId = u32;
pub type ModId = u32;
pub type GlobalId = u32;
pub type TraitId = u32;
pub type ImplId = u32;

/// A type: a node of a [`TypeArena`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TypeId(u32);

impl TypeId {
    pub const INT: TypeId = TypeId(0);
    pub const FLOAT: TypeId = TypeId(1);
    pub const BOOL: TypeId = TypeId(2);
    pub const STR: TypeId = TypeId(3);
    pub const BYTES: TypeId = TypeId(4);
    pub const DURATION: TypeId = TypeId(5);
    pub const UNIT: TypeId = TypeId(6);
    pub const NEVER: TypeId = TypeId(7);
    pub const RANGE: TypeId = TypeId(8);
    /// Poison type after an error; unifies with everything.
    pub const ERROR: TypeId = TypeId(9);

    #[inline]
    pub fn is_never(self) -> bool {
        self == TypeId::NEVER
    }

    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Debug for TypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "t{}", self.0)
    }
}

/// An interned list of types.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TyList(u32);

impl TyList {
    pub const EMPTY: TyList = TyList(0);
}

/// A type node; its components are ids.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Duration,
    Unit,
    Never,
    Range,
    Error,
    List(TypeId),
    Set(TypeId),
    Map(TypeId, TypeId),
    Tuple(TyList),
    /// Parameter types and result.
    Fn(TyList, TypeId),
    Named(TypeDefId, TyList),
    /// A rigid type parameter of the function being checked.
    Param(ParamId),
    /// An inference variable (its binding is kept by the checker).
    Var(VarId),
}

const PRIMS: [Ty; 10] = [Ty::Int, Ty::Float, Ty::Bool, Ty::Str, Ty::Bytes, Ty::Duration, Ty::Unit, Ty::Never, Ty::Range, Ty::Error];

const HAS_VAR: u8 = 1;
const HAS_PARAM: u8 = 2;

#[derive(Clone, Copy)]
struct Node {
    ty: Ty,
    flags: u8,
    /// Nodes of the type written out as a tree (saturating).
    size: u32,
}

#[derive(Clone, Copy)]
struct ListRec {
    chunk: u32,
    start: u32,
    len: u32,
    flags: u8,
    size: u32,
}

/// Elements of interned lists, in chunks that never move once shared.
#[derive(Clone, Default)]
struct ListPool {
    chunks: Vec<Arc<Vec<TypeId>>>,
}

const POOL_CHUNK: usize = 4096;

impl ListPool {
    fn add(&mut self, ids: &[TypeId]) -> (u32, u32) {
        if let Some(last) = self.chunks.last_mut()
            && let Some(v) = Arc::get_mut(last)
            && v.capacity() - v.len() >= ids.len()
        {
            let start = v.len();
            v.extend_from_slice(ids);
            return ((self.chunks.len() - 1) as u32, start as u32);
        }
        // A chunk shared with another copy of the arena is left as it is.
        let mut v = Vec::with_capacity(POOL_CHUNK.max(ids.len()));
        v.extend_from_slice(ids);
        self.chunks.push(Arc::new(v));
        ((self.chunks.len() - 1) as u32, 0)
    }
}

/// The hash-consed types of a checker (and of the compilations that extend
/// it).
#[derive(Clone)]
pub struct TypeArena {
    nodes: PVec<Node>,
    index: PHash<TypeId>,
    lists: PVec<ListRec>,
    list_index: PHash<TyList>,
    pool: ListPool,
    /// The node of each inference variable made so far.
    vars: PVec<TypeId>,
    hasher: FxBuild,
}

impl Default for TypeArena {
    fn default() -> Self {
        Self::new()
    }
}

/// A small buffer of type ids (most lists are short).
pub(crate) struct Ids {
    len: usize,
    inline: [TypeId; 8],
    heap: Vec<TypeId>,
}

impl Ids {
    pub(crate) fn new() -> Ids {
        Ids { len: 0, inline: [TypeId::INT; 8], heap: Vec::new() }
    }
    pub(crate) fn push(&mut self, t: TypeId) {
        if self.len < 8 {
            self.inline[self.len] = t;
        } else {
            if self.len == 8 {
                self.heap.extend_from_slice(&self.inline);
            }
            self.heap.push(t);
        }
        self.len += 1;
    }
    pub(crate) fn as_slice(&self) -> &[TypeId] {
        if self.len <= 8 { &self.inline[..self.len] } else { &self.heap }
    }
}

impl TypeArena {
    pub fn new() -> TypeArena {
        let mut a = TypeArena {
            nodes: PVec::new(),
            index: PHash::default(),
            lists: PVec::new(),
            list_index: PHash::default(),
            pool: ListPool::default(),
            vars: PVec::new(),
            hasher: FxBuild::default(),
        };
        for (i, ty) in PRIMS.into_iter().enumerate() {
            assert_eq!(a.intern(ty), TypeId(i as u32));
        }
        assert_eq!(a.intern_list(&[]), TyList::EMPTY);
        a
    }

    /// The number of types made.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    pub fn get(&self, t: TypeId) -> Ty {
        self.nodes[t.0 as usize].ty
    }

    #[inline]
    pub fn has_vars(&self, t: TypeId) -> bool {
        self.nodes[t.0 as usize].flags & HAS_VAR != 0
    }

    #[inline]
    pub fn has_params(&self, t: TypeId) -> bool {
        self.nodes[t.0 as usize].flags & HAS_PARAM != 0
    }

    /// The number of nodes of `t` written out as a tree (saturating),
    /// without resolving inference variables.
    #[inline]
    pub fn size(&self, t: TypeId) -> u32 {
        self.nodes[t.0 as usize].size
    }

    #[inline]
    pub fn list(&self, l: TyList) -> &[TypeId] {
        let r = &self.lists[l.0 as usize];
        &self.pool.chunks[r.chunk as usize][r.start as usize..(r.start + r.len) as usize]
    }

    #[inline]
    pub fn elem(&self, l: TyList, i: usize) -> TypeId {
        self.list(l)[i]
    }

    #[inline]
    pub fn list_len(&self, l: TyList) -> usize {
        self.lists[l.0 as usize].len as usize
    }

    pub fn intern(&mut self, ty: Ty) -> TypeId {
        let h = self.hasher.hash_one(ty);
        let nodes = &self.nodes;
        if let Some(&id) = self.index.find(h, |id| nodes[id.0 as usize].ty == ty) {
            return id;
        }
        let one = |n: &Node| (n.flags, n.size);
        let (flags, size) = match ty {
            Ty::List(e) | Ty::Set(e) => one(&self.nodes[e.0 as usize]),
            Ty::Map(k, v) => {
                let (a, b) = (&self.nodes[k.0 as usize], &self.nodes[v.0 as usize]);
                (a.flags | b.flags, a.size.saturating_add(b.size))
            }
            Ty::Tuple(l) | Ty::Named(_, l) => {
                let r = &self.lists[l.0 as usize];
                (r.flags, r.size)
            }
            Ty::Fn(l, ret) => {
                let (r, n) = (&self.lists[l.0 as usize], &self.nodes[ret.0 as usize]);
                (r.flags | n.flags, r.size.saturating_add(n.size))
            }
            Ty::Param(_) => (HAS_PARAM, 0),
            Ty::Var(_) => (HAS_VAR, 0),
            _ => (0, 0),
        };
        let id = TypeId(self.nodes.len() as u32);
        self.nodes.push(Node { ty, flags, size: size.saturating_add(1) });
        self.index.insert(h, id, |_| false);
        id
    }

    pub fn intern_list(&mut self, ids: &[TypeId]) -> TyList {
        let h = self.hasher.hash_one(ids);
        let (lists, pool) = (&self.lists, &self.pool);
        let same = |l: &TyList| {
            let r = &lists[l.0 as usize];
            pool.chunks[r.chunk as usize][r.start as usize..(r.start + r.len) as usize] == *ids
        };
        if let Some(&l) = self.list_index.find(h, same) {
            return l;
        }
        let (mut flags, mut size) = (0u8, 0u32);
        for t in ids {
            let n = &self.nodes[t.0 as usize];
            flags |= n.flags;
            size = size.saturating_add(n.size);
        }
        let (chunk, start) = self.pool.add(ids);
        let l = TyList(self.lists.len() as u32);
        self.lists.push(ListRec { chunk, start, len: ids.len() as u32, flags, size });
        self.list_index.insert(h, l, |_| false);
        l
    }

    pub fn list_of(&mut self, e: TypeId) -> TypeId {
        self.intern(Ty::List(e))
    }

    pub fn set_of(&mut self, e: TypeId) -> TypeId {
        self.intern(Ty::Set(e))
    }

    pub fn map_of(&mut self, k: TypeId, v: TypeId) -> TypeId {
        self.intern(Ty::Map(k, v))
    }

    pub fn tuple(&mut self, ts: &[TypeId]) -> TypeId {
        let l = self.intern_list(ts);
        self.intern(Ty::Tuple(l))
    }

    pub fn func(&mut self, ps: &[TypeId], ret: TypeId) -> TypeId {
        let l = self.intern_list(ps);
        self.intern(Ty::Fn(l, ret))
    }

    pub fn named(&mut self, d: TypeDefId, args: &[TypeId]) -> TypeId {
        let l = self.intern_list(args);
        self.intern(Ty::Named(d, l))
    }

    pub fn param(&mut self, p: ParamId) -> TypeId {
        self.intern(Ty::Param(p))
    }

    /// The node of inference variable `v`.
    pub fn var(&mut self, v: VarId) -> TypeId {
        while self.vars.len() <= v as usize {
            let t = self.intern(Ty::Var(self.vars.len() as VarId));
            self.vars.push(t);
        }
        self.vars[v as usize]
    }

    pub fn map_list(&mut self, l: TyList, f: &mut impl FnMut(&mut TypeArena, TypeId) -> TypeId) -> TyList {
        let n = self.list_len(l);
        let mut out = Ids::new();
        let mut changed = false;
        for i in 0..n {
            let x = self.elem(l, i);
            let y = f(self, x);
            changed |= x != y;
            out.push(y);
        }
        if changed { self.intern_list(out.as_slice()) } else { l }
    }

    /// `t` with its direct components mapped by `f`.
    pub fn rebuild(&mut self, t: TypeId, f: &mut impl FnMut(&mut TypeArena, TypeId) -> TypeId) -> TypeId {
        let ty = match self.get(t) {
            Ty::List(e) => Ty::List(f(self, e)),
            Ty::Set(e) => Ty::Set(f(self, e)),
            Ty::Map(k, v) => {
                let k = f(self, k);
                Ty::Map(k, f(self, v))
            }
            Ty::Tuple(l) => Ty::Tuple(self.map_list(l, f)),
            Ty::Named(d, l) => Ty::Named(d, self.map_list(l, f)),
            Ty::Fn(l, r) => {
                let l = self.map_list(l, f);
                Ty::Fn(l, f(self, r))
            }
            _ => return t,
        };
        if ty == self.get(t) { t } else { self.intern(ty) }
    }

    /// Replaces type parameters according to `f`.
    pub fn subst_params(&mut self, t: TypeId, f: &impl Fn(ParamId) -> Option<TypeId>) -> TypeId {
        if !self.has_params(t) {
            return t;
        }
        match self.get(t) {
            Ty::Param(p) => f(p).unwrap_or(t),
            _ => self.rebuild(t, &mut |a, x| a.subst_params(x, f)),
        }
    }

    /// Replaces the type parameters `generics` by `args` (by position).
    pub fn instantiate(&mut self, t: TypeId, generics: &[ParamId], args: &[TypeId]) -> TypeId {
        self.subst_params(t, &|p| generics.iter().position(|&g| g == p).map(|i| args[i]))
    }

    /// Whether `f` holds for any component node of `t` (written out as a
    /// tree, stopping at the first).
    pub fn any(&self, t: TypeId, f: &mut impl FnMut(Ty) -> bool) -> bool {
        let ty = self.get(t);
        f(ty)
            || match ty {
                Ty::List(e) | Ty::Set(e) => self.any(e, f),
                Ty::Map(k, v) => self.any(k, f) || self.any(v, f),
                Ty::Tuple(l) | Ty::Named(_, l) => self.list(l).iter().any(|&x| self.any(x, f)),
                Ty::Fn(l, r) => self.list(l).iter().any(|&x| self.any(x, f)) || self.any(r, f),
                _ => false,
            }
    }

    pub fn mentions(&self, t: TypeId, p: ParamId) -> bool {
        self.has_params(t) && self.any(t, &mut |x| x == Ty::Param(p))
    }
}

/// An interned name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Debug)]
pub struct Symbol(u32);

/// Names every symbol table has, with fixed symbols.
const KNOWN: [&str; 10] = ["", "Self", "self", "_", "<tmp>", "<dict>", "<arg>", "next", "close", "<desc>"];

impl Symbol {
    pub const EMPTY: Symbol = Symbol(0);
    pub const SELF_TYPE: Symbol = Symbol(1);
    pub const SELF: Symbol = Symbol(2);
    pub const UNDERSCORE: Symbol = Symbol(3);
    /// Hidden temporaries.
    pub const TMP: Symbol = Symbol(4);
    /// Hidden dictionary parameters.
    pub const DICT: Symbol = Symbol(5);
    /// Parameters of the closures made for function values.
    pub const ARG: Symbol = Symbol(6);
    pub const NEXT: Symbol = Symbol(7);
    pub const CLOSE: Symbol = Symbol(8);
    /// Hidden parameters holding the descriptions of `reflect` type arguments.
    pub const DESC: Symbol = Symbol(9);
}

/// Interned names (persistent, like the [`TypeArena`]).
#[derive(Clone)]
pub struct Symbols {
    strs: PVec<Arc<str>>,
    index: PHash<Symbol>,
    hasher: FxBuild,
}

impl Default for Symbols {
    fn default() -> Self {
        Self::new()
    }
}

impl Symbols {
    pub fn new() -> Symbols {
        let mut s = Symbols { strs: PVec::new(), index: PHash::default(), hasher: FxBuild::default() };
        for (i, name) in KNOWN.into_iter().enumerate() {
            assert_eq!(s.intern(name), Symbol(i as u32));
        }
        s
    }

    /// The symbol of `name`, if it was interned.
    #[inline]
    pub fn get(&self, name: &str) -> Option<Symbol> {
        let strs = &self.strs;
        self.index.find(self.hasher.hash_one(name), |s| &*strs[s.0 as usize] == name).copied()
    }

    pub fn intern(&mut self, name: &str) -> Symbol {
        let h = self.hasher.hash_one(name);
        let strs = &self.strs;
        if let Some(&s) = self.index.find(h, |s| &*strs[s.0 as usize] == name) {
            return s;
        }
        let s = Symbol(self.strs.len() as u32);
        self.strs.push(Arc::from(name));
        self.index.insert(h, s, |_| false);
        s
    }

    #[inline]
    pub fn str(&self, s: Symbol) -> &str {
        &self.strs[s.0 as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bound {
    Eq,
    Ord,
    Hash,
    Num,
    /// Values that `json.encode` can represent: all but functions, tasks,
    /// channels, and native objects.
    Encode,
    /// Supports `+`: numbers, durations, strings, bytes and lists. Not
    /// written in source: the checker requires it of the operands of `+`
    /// whose type is not known yet (`(a, b) => a + b`).
    Add,
}

impl Bound {
    pub fn parse(s: &str) -> Option<Bound> {
        Some(match s {
            "Eq" => Bound::Eq,
            "Ord" => Bound::Ord,
            "Hash" => Bound::Hash,
            "Num" => Bound::Num,
            "Encode" => Bound::Encode,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Bound::Eq => "Eq",
            Bound::Ord => "Ord",
            Bound::Hash => "Hash",
            Bound::Num => "Num",
            Bound::Encode => "Encode",
            Bound::Add => "Add",
        }
    }
}

/// A user-defined trait applied to type arguments (`Into<str>`); the
/// implementing type is given separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraitRef {
    pub trait_id: TraitId,
    pub args: TyList,
}

impl TraitRef {
    pub fn subst_params(self, arena: &mut TypeArena, f: &impl Fn(ParamId) -> Option<TypeId>) -> TraitRef {
        TraitRef { trait_id: self.trait_id, args: arena.map_list(self.args, &mut |a, t| a.subst_params(t, f)) }
    }
}

#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub name: Symbol,
    /// Built-in bounds, including those implied by the supertraits of `traits`.
    pub bounds: Vec<Bound>,
    /// User-defined trait bounds.
    pub traits: Vec<TraitRef>,
    pub reflect: bool,
}

impl ParamInfo {
    pub fn has_bound(&self, b: Bound) -> bool {
        self.bounds.iter().any(|&x| {
            x == b
                || (b == Bound::Eq && matches!(x, Bound::Ord | Bound::Hash | Bound::Num))
                || (matches!(b, Bound::Ord | Bound::Add | Bound::Encode) && x == Bound::Num)
        })
    }
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: Symbol,
    pub ty: TypeId,
    pub mutable: bool,
    pub is_pub: bool,
    /// Helper function computing the default value.
    pub default: Option<FnId>,
    /// The default if it is a constant, which `json.decode` uses.
    pub const_default: Option<ConstVal>,
    pub doc: Option<Arc<str>>,
}

/// A value built only from literals, ranges, and constructors, such as
/// `(1, "a")`, `["k": [2]]`, `Shape.Circle(1.0)`, or `Point { x: 0 }`. A
/// struct field default that is a constant fills a missing field in
/// `json.decode`. A program identifies a variant by its runtime id and a
/// struct by its definition (`V` and `S`); a native module identifies them by
/// Rust type.
#[derive(Clone, Debug, PartialEq)]
pub enum ConstVal<V = u32, S = TypeDefId> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Dur(i64),
    /// `a..b`, or `a..=b` if inclusive.
    Range(i64, i64, bool),
    Tuple(Vec<ConstVal<V, S>>),
    List(Vec<ConstVal<V, S>>),
    Map(Vec<(ConstVal<V, S>, ConstVal<V, S>)>),
    /// A variant and its payload.
    Variant(V, Vec<ConstVal<V, S>>),
    /// A struct and its fields in declaration order.
    Struct(S, Vec<ConstVal<V, S>>),
}

impl<V, S> ConstVal<V, S> {
    /// `self` with each variant identified by `variant` and each struct by
    /// `def`, or `None` if one of them returns `None`.
    pub fn resolve<V2, S2>(&self, variant: &impl Fn(&V) -> Option<V2>, def: &impl Fn(&S) -> Option<S2>) -> Option<ConstVal<V2, S2>> {
        let all = |xs: &[ConstVal<V, S>]| xs.iter().map(|x| x.resolve(variant, def)).collect::<Option<Vec<_>>>();
        Some(match self {
            ConstVal::Unit => ConstVal::Unit,
            ConstVal::Bool(b) => ConstVal::Bool(*b),
            ConstVal::Int(i) => ConstVal::Int(*i),
            ConstVal::Float(f) => ConstVal::Float(*f),
            ConstVal::Str(s) => ConstVal::Str(s.clone()),
            ConstVal::Bytes(b) => ConstVal::Bytes(b.clone()),
            ConstVal::Dur(d) => ConstVal::Dur(*d),
            ConstVal::Range(a, b, inclusive) => ConstVal::Range(*a, *b, *inclusive),
            ConstVal::Tuple(xs) => ConstVal::Tuple(all(xs)?),
            ConstVal::List(xs) => ConstVal::List(all(xs)?),
            ConstVal::Map(kvs) => {
                ConstVal::Map(kvs.iter().map(|(k, v)| Some((k.resolve(variant, def)?, v.resolve(variant, def)?))).collect::<Option<_>>()?)
            }
            ConstVal::Variant(v, xs) => ConstVal::Variant(variant(v)?, all(xs)?),
            ConstVal::Struct(s, xs) => ConstVal::Struct(def(s)?, all(xs)?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct VariantDef {
    pub name: Symbol,
    pub fields: Vec<TypeId>,
    pub field_names: Option<Vec<Symbol>>,
    /// Program-wide runtime id of this variant.
    pub rt_id: u32,
    pub doc: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
pub enum TypeDefKind {
    Struct {
        fields: Vec<FieldDef>,
    },
    Enum {
        variants: Vec<VariantDef>,
    },
    /// An opaque type registered by a native module or implemented by the VM.
    Opaque(Opaque),
}

#[derive(Clone, Copy, Debug)]
pub enum Opaque {
    Value(ValueTypeInfo),
    Object(std::any::TypeId),
    /// Built-in VM type (`Task<T>`, `Channel<T>`).
    Builtin,
}

/// Metadata for a registered [`crate::vm::value::ValueType`].
#[derive(Clone, Copy, Debug)]
pub struct ValueTypeInfo {
    pub ord: bool,
    pub parse: crate::vm::value::ParseValue,
    pub type_id: std::any::TypeId,
}

#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: Symbol,
    pub module: ModId,
    pub module_name: Symbol,
    pub generics: Vec<ParamId>,
    pub kind: TypeDefKind,
    pub is_pub: bool,
    pub doc: Option<Arc<str>>,
    pub span: Span,
}

impl TypeDef {
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            TypeDefKind::Struct { fields } => fields,
            _ => &[],
        }
    }
    pub fn variants(&self) -> &[VariantDef] {
        match &self.kind {
            TypeDefKind::Enum { variants } => variants,
            _ => &[],
        }
    }
    pub fn is_struct(&self) -> bool {
        matches!(self.kind, TypeDefKind::Struct { .. })
    }
    pub fn is_enum(&self) -> bool {
        matches!(self.kind, TypeDefKind::Enum { .. })
    }
}

/// The well-known type definitions every program has.
#[derive(Clone, Copy, Debug, Default)]
pub struct WellKnown {
    pub option: TypeDefId,
    pub result: TypeDefId,
    pub error: TypeDefId,
    pub task: TypeDefId,
    pub channel: TypeDefId,
}

/// Formats types for diagnostics, signatures and help.
#[derive(Clone, Copy)]
pub struct TypePrinter<'a> {
    pub arena: &'a TypeArena,
    pub defs: &'a PVec<TypeDef>,
    pub params: &'a PVec<ParamInfo>,
    pub syms: &'a Symbols,
    pub wk: WellKnown,
    /// Bindings of inference variables, followed when printing.
    pub subst: &'a [Option<TypeId>],
    /// Names for inference variables shown as type parameters (others are `_`).
    pub var_names: &'a [(VarId, String)],
}

impl<'a> TypePrinter<'a> {
    pub fn new(arena: &'a TypeArena, defs: &'a PVec<TypeDef>, params: &'a PVec<ParamInfo>, syms: &'a Symbols, wk: WellKnown) -> Self {
        TypePrinter { arena, defs, params, syms, wk, subst: &[], var_names: &[] }
    }

    pub fn show(&self, t: TypeId) -> String {
        self.show_tagged(t, &|_| None)
    }

    /// Like `show`, adding `tag(d)` (if any) in parentheses after the name
    /// of each type definition `d`.
    pub fn show_tagged(&self, t: TypeId, tag: &dyn Fn(TypeDefId) -> Option<String>) -> String {
        let mut s = String::new();
        Tagged { p: self, tag }.write(&mut s, t);
        s
    }

    /// `t` with inference variables resolved (at the top).
    fn resolve(&self, mut t: TypeId) -> TypeId {
        while let Ty::Var(v) = self.arena.get(t) {
            match self.subst.get(v as usize) {
                Some(Some(b)) => t = *b,
                _ => break,
            }
        }
        t
    }

    /// `fn name(a: int, b: str = ...) -> T`
    pub fn signature(&self, name: &str, params: &[(&str, TypeId, bool)], ret: TypeId) -> String {
        let mut s = format!("fn {name}(");
        for (i, (n, t, d)) in params.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            if *n == "self" {
                s.push_str("self");
                continue;
            }
            let _ = write!(s, "{n}: {}", self.show(*t));
            if *d {
                s.push_str(" = ...");
            }
        }
        s.push(')');
        if self.resolve(ret) != TypeId::UNIT {
            let _ = write!(s, " -> {}", self.show(ret));
        }
        s
    }
}

struct Tagged<'p, 'a> {
    p: &'p TypePrinter<'a>,
    tag: &'p dyn Fn(TypeDefId) -> Option<String>,
}

impl Tagged<'_, '_> {
    fn list(&self, s: &mut String, l: TyList) {
        for (i, &t) in self.p.arena.list(l).iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            self.write(s, t);
        }
    }

    fn write(&self, s: &mut String, t: TypeId) {
        let TypePrinter { arena, defs, params, syms, wk, var_names, .. } = self.p;
        let t = self.p.resolve(t);
        match arena.get(t) {
            Ty::Int => s.push_str("int"),
            Ty::Float => s.push_str("float"),
            Ty::Bool => s.push_str("bool"),
            Ty::Str => s.push_str("str"),
            Ty::Bytes => s.push_str("bytes"),
            Ty::Duration => s.push_str("duration"),
            Ty::Unit => s.push_str("()"),
            Ty::Never => s.push_str("never"),
            Ty::Range => s.push_str("Range"),
            Ty::Error => s.push_str("{error}"),
            Ty::Var(v) => match var_names.iter().find(|(w, _)| *w == v) {
                Some((_, n)) => s.push_str(n),
                None => s.push('_'),
            },
            Ty::Param(p) => s.push_str(params.get(p as usize).map(|p| syms.str(p.name)).unwrap_or("?")),
            Ty::List(t) => {
                s.push('[');
                self.write(s, t);
                s.push(']');
            }
            Ty::Map(k, v) => {
                s.push('[');
                self.write(s, k);
                s.push_str(": ");
                self.write(s, v);
                s.push(']');
            }
            Ty::Set(t) => {
                s.push_str("Set<");
                self.write(s, t);
                s.push('>');
            }
            Ty::Tuple(l) => {
                s.push('(');
                self.list(s, l);
                s.push(')');
            }
            Ty::Fn(l, r) => {
                s.push_str("fn(");
                self.list(s, l);
                s.push(')');
                if self.p.resolve(r) != TypeId::UNIT {
                    s.push_str(" -> ");
                    self.write(s, r);
                }
            }
            Ty::Named(d, l) => {
                let args = arena.list(l);
                let first = args.first().map(|&a| arena.get(self.p.resolve(a)));
                let tagged = args.len() == 1 && matches!(first, Some(Ty::Named(e, _)) if (self.tag)(e).is_some());
                if d == wk.option && args.len() == 1 && !tagged {
                    let needs_parens = matches!(first, Some(Ty::Fn(..)));
                    if needs_parens {
                        s.push('(');
                    }
                    self.write(s, args[0]);
                    if needs_parens {
                        s.push(')');
                    }
                    s.push('?');
                    return;
                }
                let name = defs.get(d as usize).map(|d| syms.str(d.name)).unwrap_or("?");
                s.push_str(name);
                if !args.is_empty() {
                    s.push('<');
                    self.list(s, l);
                    s.push('>');
                }
                if let Some(tag) = (self.tag)(d) {
                    let _ = write!(s, " ({tag})");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_hash_consed() {
        let mut a = TypeArena::new();
        let l1 = a.list_of(TypeId::INT);
        let l2 = a.list_of(TypeId::INT);
        assert_eq!(l1, l2);
        let t1 = a.tuple(&[l1, TypeId::STR]);
        let b = a.clone();
        let t2 = a.tuple(&[l2, TypeId::STR]);
        assert_eq!(t1, t2);
        assert_eq!(a.size(t1), 4);
        let v = a.var(3);
        let f = a.func(&[v], t1);
        assert!(a.has_vars(f));
        assert!(!a.has_vars(t1));
        let p = a.param(7);
        let g = a.named(0, &[p, v]);
        assert!(a.has_params(g) && a.has_vars(g));
        let h = a.subst_params(g, &|q| (q == 7).then_some(TypeId::BOOL));
        assert_eq!(a.get(h), Ty::Named(0, a.intern_list(&[TypeId::BOOL, v])));
        assert!(!a.has_params(h));
        // The copy made before is unaffected.
        assert_eq!(b.get(t1), a.get(t1));
        assert!(b.len() < a.len());
    }

    #[test]
    fn symbols_are_interned() {
        let mut s = Symbols::new();
        let a = s.intern("alpha");
        assert_eq!(s.intern("alpha"), a);
        assert_eq!(s.get("alpha"), Some(a));
        assert_eq!(s.get("beta"), None);
        assert_eq!(s.str(a), "alpha");
        assert_eq!(s.get("self"), Some(Symbol::SELF));
    }
}
