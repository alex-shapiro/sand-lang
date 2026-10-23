//! Runtime type descriptions for `reflect` type parameters (e.g. `json.decode`).
//!
//! A description expands struct and enum definitions, so written out as a
//! tree it can be exponentially larger than the type (`struct B { x: A, y:
//! A }`, `struct C { x: B, y: B }`, ...). Descriptions are therefore built as
//! DAGs: the description of each struct or enum *instance* (a definition
//! with its type arguments) is built once and shared (`Arc`) by every place
//! that refers to it, within one description and, through [`DescCache`],
//! across all the `reflect` call sites of a program. Each reflected type
//! becomes one shared constant.
//!
//! A recursive reference to an enclosing struct or enum is a
//! `TypeDesc::Recur(id)`, resolved by the decoder against the instances it
//! is inside. A shared subtree must therefore never contain a `Recur` to an
//! instance outside itself unless it is only reused below that instance:
//! descriptions whose recursive references all point inside them are
//! *closed* and cached for the whole program; the others are reused only
//! while the instance they refer to is still being described.
//!
//! The work is bounded: every node built, cache lookup and key component
//! costs a unit, at most [`MAX_DESC_NODES`] per description and
//! [`MAX_TOTAL_DESC_NODES`] per compilation.

use crate::check::CheckedProgram;
use crate::check::persist::PMap;
use crate::types::{FnId, Opaque, Ty, TyList, TypeArena, TypeDefId, TypeDefKind, TypeId, TypePrinter};
use crate::vm::value::{FieldDefault, TypeDesc};
use std::collections::HashMap;
use std::sync::Arc;

/// Maximum work (nodes built, plus type key components examined) for the
/// description of one reflected type.
pub const MAX_DESC_NODES: usize = 100_000;

/// Maximum work for all the descriptions built by one compilation.
pub const MAX_TOTAL_DESC_NODES: usize = 1_000_000;

/// A structural key identifying a type (independent of the ids of the
/// arena, which differ between the compilations that share the cache).
type Key = Vec<u32>;

/// Type descriptions shared between the `reflect` call sites of a program
/// (kept between the compilations of a REPL session).
#[derive(Clone, Default)]
pub(crate) struct DescCache {
    /// Ids of struct and enum instances (`TypeDesc::Struct::id`).
    ids: PMap<Key, u32>,
    /// Closed descriptions of struct and enum instances.
    closed: PMap<Key, Arc<TypeDesc>>,
    /// The constant holding the description of each reflected type.
    consts: PMap<Key, u32>,
    /// The function that fills each field (struct definition, field index)
    /// with its computed default (see [`DescSink::field_fill`]).
    field_fills: PMap<(TypeDefId, u32), u32>,
    /// The function that adds elements to a set (see [`DescSink::set_fill`]).
    set_fill: Option<u32>,
    /// Work done by the current compilation (reset by [`DescCache::start`]).
    total: usize,
}

/// Adds to the program what descriptions refer to.
pub(crate) trait DescSink {
    /// Adds the constant holding `desc` and returns its index.
    fn constant(&mut self, desc: Arc<TypeDesc>) -> u32;
    /// Adds the function (with the struct as its one capture) that sets field
    /// `index` of struct `def` to the value of the field's default `helper`,
    /// and returns its proto, or `None` if `helper` has no code.
    fn field_fill(&mut self, def: TypeDefId, index: u32, helper: FnId) -> Option<u32>;
    /// Adds the function (with a set and a list as its captures) that adds
    /// the elements of the list to the set, and returns its proto.
    fn set_fill(&mut self) -> u32;
}

impl DescCache {
    /// Starts a compilation (which may do another `MAX_TOTAL_DESC_NODES`).
    pub(crate) fn start(&mut self) {
        self.total = 0;
    }

    /// The index of the constant describing `t`, adding it to `sink` if
    /// needed; `None` if the description is too large.
    pub(crate) fn const_for(&mut self, cp: &CheckedProgram, arena: &mut TypeArena, t: TypeId, sink: &mut dyn DescSink) -> Option<u32> {
        let mut key = Vec::new();
        type_key(arena, t, &mut key);
        self.total = self.total.saturating_add(key.len());
        if self.total > MAX_TOTAL_DESC_NODES {
            return None;
        }
        if let Some(&k) = self.consts.get(&key) {
            return Some(k);
        }
        let desc = self.describe(cp, arena, t, sink)?;
        let k = sink.constant(desc);
        self.consts.insert(key, k);
        Some(k)
    }

    /// The description of `t`, or `None` if it is too large.
    fn describe(&mut self, cp: &CheckedProgram, arena: &mut TypeArena, t: TypeId, sink: &mut dyn DescSink) -> Option<Arc<TypeDesc>> {
        let budget = MAX_DESC_NODES.min(MAX_TOTAL_DESC_NODES.saturating_sub(self.total));
        let mut b = Builder {
            cp,
            arena,
            cache: self,
            sink,
            stack: HashMap::new(),
            open: HashMap::new(),
            open_at: Vec::new(),
            work: 0,
            budget,
            failed: false,
        };
        let (d, _) = b.desc(t);
        let (work, failed) = (b.work, b.failed);
        self.total = self.total.saturating_add(work);
        (!failed).then_some(d)
    }
}

fn type_key(arena: &TypeArena, t: TypeId, out: &mut Key) {
    let list = |l: TyList, out: &mut Key| {
        let ts = arena.list(l);
        out.push(ts.len() as u32);
        ts.iter().for_each(|&t| type_key(arena, t, out));
    };
    match arena.get(t) {
        Ty::Int => out.push(0),
        Ty::Float => out.push(1),
        Ty::Bool => out.push(2),
        Ty::Str => out.push(3),
        Ty::Bytes => out.push(4),
        Ty::Duration => out.push(5),
        Ty::Unit => out.push(6),
        Ty::Never => out.push(7),
        Ty::Range => out.push(8),
        Ty::Error => out.push(9),
        Ty::List(e) => {
            out.push(10);
            type_key(arena, e, out);
        }
        Ty::Set(e) => {
            out.push(11);
            type_key(arena, e, out);
        }
        Ty::Map(k, v) => {
            out.push(12);
            type_key(arena, k, out);
            type_key(arena, v, out);
        }
        Ty::Tuple(ts) => {
            out.push(13);
            list(ts, out);
        }
        Ty::Fn(ps, r) => {
            out.push(14);
            list(ps, out);
            type_key(arena, r, out);
        }
        Ty::Named(d, args) => {
            out.extend([15, d]);
            list(args, out);
        }
        Ty::Param(p) => out.extend([16, p]),
        Ty::Var(v) => out.extend([17, v]),
    }
}

/// Builds one description.
struct Builder<'a, 'c> {
    cp: &'a CheckedProgram,
    arena: &'c mut TypeArena,
    cache: &'c mut DescCache,
    sink: &'c mut dyn DescSink,
    /// The struct and enum instances being described, by their depth.
    stack: HashMap<Key, usize>,
    /// Descriptions that refer to instances on the stack, with the lowest
    /// stack depth they refer to. Each is reused only while the instance
    /// that contains it is being described (the stack below is unchanged).
    open: HashMap<Key, (Arc<TypeDesc>, usize)>,
    /// The keys in `open` by the depth of the instance containing them, to
    /// drop them when that instance is done.
    open_at: Vec<Vec<Key>>,
    work: usize,
    budget: usize,
    failed: bool,
}

/// The lower of two "lowest stack depth referred to" (`None`: no reference).
fn low(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl Builder<'_, '_> {
    /// Charges `units` of work; false once over budget.
    fn charge(&mut self, units: usize) -> bool {
        self.work = self.work.saturating_add(units);
        if self.work > self.budget {
            self.failed = true;
        }
        !self.failed
    }

    /// The description of `t` and the lowest depth of `stack` it refers to.
    fn desc(&mut self, t: TypeId) -> (Arc<TypeDesc>, Option<usize>) {
        if !self.charge(1) {
            return (Arc::new(TypeDesc::Unit), None);
        }
        let leaf = |d: TypeDesc| (Arc::new(d), None);
        match self.arena.get(t) {
            Ty::Int => leaf(TypeDesc::Int),
            Ty::Float => leaf(TypeDesc::Float),
            Ty::Bool => leaf(TypeDesc::Bool),
            Ty::Str => leaf(TypeDesc::Str),
            Ty::Bytes => leaf(TypeDesc::Bytes),
            Ty::Duration => leaf(TypeDesc::Duration),
            Ty::Unit => leaf(TypeDesc::Unit),
            Ty::Range => leaf(TypeDesc::Range),
            Ty::List(e) => {
                let (e, l) = self.desc(e);
                (Arc::new(TypeDesc::List(e)), l)
            }
            Ty::Set(e) => {
                let (e, l) = self.desc(e);
                let fill = *self.cache.set_fill.get_or_insert_with(|| self.sink.set_fill());
                (Arc::new(TypeDesc::Set(e, fill)), l)
            }
            Ty::Map(k, v) => {
                let ((k, lk), (v, lv)) = (self.desc(k), self.desc(v));
                (Arc::new(TypeDesc::Map(k, v)), low(lk, lv))
            }
            Ty::Tuple(ts) => {
                let mut l = None;
                let ts = self.arena.list(ts).to_vec();
                let items = ts
                    .into_iter()
                    .map(|x| {
                        let (d, lx) = self.desc(x);
                        l = low(l, lx);
                        d
                    })
                    .collect();
                (Arc::new(TypeDesc::Tuple(items)), l)
            }
            Ty::Named(d, args) => {
                if d == self.cp.wk.option {
                    let (inner, l) = self.desc(self.arena.elem(args, 0));
                    return (Arc::new(TypeDesc::Option(inner)), l);
                }
                self.named(t, d, args)
            }
            _ => leaf(TypeDesc::Unsupported(self.show(t))),
        }
    }

    /// The function that fills field `index` of struct `d` with its computed
    /// default `helper`.
    fn field_fill(&mut self, d: TypeDefId, index: u32, helper: FnId) -> Option<u32> {
        if let Some(&p) = self.cache.field_fills.get(&(d, index)) {
            return Some(p);
        }
        let p = self.sink.field_fill(d, index, helper)?;
        self.cache.field_fills.insert((d, index), p);
        Some(p)
    }

    fn show(&self, t: TypeId) -> String {
        TypePrinter::new(self.arena, &self.cp.types, &self.cp.params, &self.cp.syms, self.cp.wk).show(t)
    }

    /// The description of the struct or enum instance `t` = `d<args>`.
    fn named(&mut self, t: TypeId, d: u32, args: TyList) -> (Arc<TypeDesc>, Option<usize>) {
        let mut key = Vec::new();
        type_key(self.arena, t, &mut key);
        if !self.charge(key.len()) {
            return (Arc::new(TypeDesc::Unit), None);
        }
        let next_id = self.cache.ids.len() as u32;
        let id = self.cache.ids.get_or_insert(key.clone(), next_id);
        if let Some(&depth) = self.stack.get(&key) {
            return (Arc::new(TypeDesc::Recur(id)), Some(depth));
        }
        if let Some(desc) = self.cache.closed.get(&key) {
            return (desc.clone(), None);
        }
        if let Some((desc, x)) = self.open.get(&key) {
            return (desc.clone(), Some(*x));
        }
        let def = &self.cp.types[d as usize];
        let name = self.show(t);
        if !self.charge(name.len() / 8) {
            return (Arc::new(TypeDesc::Unit), None);
        }
        let cp = self.cp;
        let args = self.arena.list(args).to_vec();
        let name_of = |s| cp.name(s).to_string();
        let depth = self.stack.len();
        self.stack.insert(key.clone(), depth);
        self.open_at.push(Vec::new());
        let mut l = None;
        let desc = match &def.kind {
            TypeDefKind::Struct { fields } => {
                let fields = fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let ft = self.arena.instantiate(f.ty, &def.generics, &args);
                        let (fd, lf) = self.desc(ft);
                        l = low(l, lf);
                        let default = match (&f.const_default, f.default) {
                            (Some(c), _) => FieldDefault::Const(c.clone()),
                            (None, Some(helper)) => match self.field_fill(d, i as u32, helper) {
                                Some(fill) => FieldDefault::Computed(fill),
                                None => FieldDefault::None,
                            },
                            (None, None) => FieldDefault::None,
                        };
                        (name_of(f.name), fd, default)
                    })
                    .collect();
                TypeDesc::Struct { id, def: d, name, fields, rust: self.cp.rust_type(d) }
            }
            TypeDefKind::Enum { variants } => {
                let variants = variants
                    .iter()
                    .map(|v| {
                        let payload = v
                            .fields
                            .iter()
                            .map(|&x| {
                                let ft = self.arena.instantiate(x, &def.generics, &args);
                                let (fd, lf) = self.desc(ft);
                                l = low(l, lf);
                                fd
                            })
                            .collect();
                        (name_of(v.name), v.rt_id, payload, v.field_names.as_ref().map(|ns| ns.iter().map(|&n| name_of(n)).collect()))
                    })
                    .collect();
                TypeDesc::Enum { id, name, variants, rust: self.cp.rust_type(d) }
            }
            TypeDefKind::Opaque(Opaque::Value(v)) => TypeDesc::Value { name, parse: v.parse },
            TypeDefKind::Opaque(_) => TypeDesc::Unsupported(name),
        };
        // Descriptions made inside this instance are not reused outside it.
        for k in self.open_at.pop().unwrap_or_default() {
            self.open.remove(&k);
        }
        self.stack.remove(&key);
        let desc = Arc::new(desc);
        // References to this instance itself are resolved inside it.
        let l = l.filter(|&x| x < depth);
        if !self.failed {
            match l {
                None => {
                    self.cache.closed.insert(key, desc.clone());
                }
                Some(x) => {
                    // Reusable while the enclosing instance (`x < depth`,
                    // so there is one) is being described.
                    self.open.insert(key.clone(), (desc.clone(), x));
                    self.open_at[depth - 1].push(key);
                }
            }
        }
        (desc, l)
    }
}
