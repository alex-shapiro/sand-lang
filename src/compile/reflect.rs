//! Runtime type descriptions for `reflect` type parameters (e.g. `json.decode`).
//!
//! A description expands struct and enum definitions, so written out as a
//! tree it can be exponentially larger than the type (`struct B { x: A, y:
//! A }`, `struct C { x: B, y: B }`, ...). Descriptions are therefore built as
//! DAGs: the description of each struct or enum *instance* (a definition
//! with its type arguments) is built once and shared (`Arc`) by every place
//! that refers to it, within one description and, through [`DescCache`],
//! across all the `reflect` call sites of a program (and the inputs of a
//! REPL session). Each reflected type becomes one shared constant.
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
use crate::types::{Type, TypeDefKind, TypePrinter};
use crate::vm::value::TypeDesc;
use std::collections::HashMap;
use std::sync::Arc;

/// Maximum work (nodes built, plus type key components examined) for the
/// description of one reflected type.
pub const MAX_DESC_NODES: usize = 100_000;

/// Maximum work for all the descriptions built by one compilation.
pub const MAX_TOTAL_DESC_NODES: usize = 1_000_000;

/// A structural key identifying a type (`Type` is not hashable).
type Key = Vec<u32>;

/// Type descriptions shared between the `reflect` call sites of a program
/// (kept between the compilations of a REPL session).
#[derive(Clone, Default)]
pub(crate) struct DescCache {
    /// Ids of struct and enum instances (`TypeDesc::Struct::id`).
    ids: HashMap<Key, u32>,
    /// Closed descriptions of struct and enum instances.
    closed: HashMap<Key, Arc<TypeDesc>>,
    /// The constant holding the description of each reflected type.
    consts: HashMap<Key, u32>,
    /// Work done by the current compilation (reset by [`DescCache::start`]).
    total: usize,
}

impl DescCache {
    /// Starts a compilation (which may do another `MAX_TOTAL_DESC_NODES`).
    pub(crate) fn start(&mut self) {
        self.total = 0;
    }

    /// The index of the constant describing `t`, creating it with `add` if
    /// needed; `None` if the description is too large.
    pub(crate) fn const_for(&mut self, cp: &CheckedProgram, t: &Type, add: impl FnOnce(Arc<TypeDesc>) -> u32) -> Option<u32> {
        let mut key = Vec::new();
        type_key(t, &mut key);
        self.total = self.total.saturating_add(key.len());
        if self.total > MAX_TOTAL_DESC_NODES {
            return None;
        }
        if let Some(&k) = self.consts.get(&key) {
            return Some(k);
        }
        let desc = self.describe(cp, t)?;
        let k = add(desc);
        self.consts.insert(key, k);
        Some(k)
    }

    /// The description of `t`, or `None` if it is too large.
    fn describe(&mut self, cp: &CheckedProgram, t: &Type) -> Option<Arc<TypeDesc>> {
        let budget = MAX_DESC_NODES.min(MAX_TOTAL_DESC_NODES.saturating_sub(self.total));
        let mut b =
            Builder { cp, cache: self, stack: HashMap::new(), open: HashMap::new(), open_at: Vec::new(), work: 0, budget, failed: false };
        let (d, _) = b.desc(t);
        let (work, failed) = (b.work, b.failed);
        self.total = self.total.saturating_add(work);
        (!failed).then_some(d)
    }
}

/// Appends the structural key of `t` to `out`.
fn type_key(t: &Type, out: &mut Key) {
    match t {
        Type::Int => out.push(0),
        Type::Float => out.push(1),
        Type::Bool => out.push(2),
        Type::Str => out.push(3),
        Type::Bytes => out.push(4),
        Type::Duration => out.push(5),
        Type::Unit => out.push(6),
        Type::Never => out.push(7),
        Type::Range => out.push(8),
        Type::Error => out.push(9),
        Type::List(e) => {
            out.push(10);
            type_key(e, out);
        }
        Type::Set(e) => {
            out.push(11);
            type_key(e, out);
        }
        Type::Map(k, v) => {
            out.push(12);
            type_key(k, out);
            type_key(v, out);
        }
        Type::Tuple(ts) => {
            out.extend([13, ts.len() as u32]);
            ts.iter().for_each(|t| type_key(t, out));
        }
        Type::Fn(ps, r) => {
            out.extend([14, ps.len() as u32]);
            ps.iter().for_each(|t| type_key(t, out));
            type_key(r, out);
        }
        Type::Named(d, args) => {
            out.extend([15, *d, args.len() as u32]);
            args.iter().for_each(|t| type_key(t, out));
        }
        Type::Param(p) => out.extend([16, *p]),
        Type::Var(v) => out.extend([17, *v]),
    }
}

/// Builds one description.
struct Builder<'a, 'c> {
    cp: &'a CheckedProgram,
    cache: &'c mut DescCache,
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
    fn desc(&mut self, t: &Type) -> (Arc<TypeDesc>, Option<usize>) {
        if !self.charge(1) {
            return (Arc::new(TypeDesc::Unit), None);
        }
        let leaf = |d: TypeDesc| (Arc::new(d), None);
        match t {
            Type::Int => leaf(TypeDesc::Int),
            Type::Float => leaf(TypeDesc::Float),
            Type::Bool => leaf(TypeDesc::Bool),
            Type::Str => leaf(TypeDesc::Str),
            Type::Bytes => leaf(TypeDesc::Bytes),
            Type::Duration => leaf(TypeDesc::Duration),
            Type::Unit => leaf(TypeDesc::Unit),
            Type::List(e) => {
                let (e, l) = self.desc(e);
                (Arc::new(TypeDesc::List(e)), l)
            }
            Type::Set(e) => {
                let (e, l) = self.desc(e);
                (Arc::new(TypeDesc::Set(e)), l)
            }
            Type::Map(k, v) => {
                let ((k, lk), (v, lv)) = (self.desc(k), self.desc(v));
                (Arc::new(TypeDesc::Map(k, v)), low(lk, lv))
            }
            Type::Tuple(ts) => {
                let mut l = None;
                let items = ts
                    .iter()
                    .map(|x| {
                        let (d, lx) = self.desc(x);
                        l = low(l, lx);
                        d
                    })
                    .collect();
                (Arc::new(TypeDesc::Tuple(items)), l)
            }
            Type::Named(d, args) => {
                if *d == self.cp.wk.option {
                    let (inner, l) = self.desc(&args[0]);
                    return (Arc::new(TypeDesc::Option(inner)), l);
                }
                let def = &self.cp.types[*d as usize];
                if def.name == "Json" && def.module_name == "json" {
                    return leaf(TypeDesc::Json);
                }
                self.named(t, *d, args)
            }
            other => leaf(TypeDesc::Unsupported(self.show(other))),
        }
    }

    fn show(&self, t: &Type) -> String {
        TypePrinter { defs: &self.cp.types, params: &self.cp.params, wk: self.cp.wk }.show(t)
    }

    /// The description of the struct or enum instance `t` = `d<args>`.
    fn named(&mut self, t: &Type, d: u32, args: &[Type]) -> (Arc<TypeDesc>, Option<usize>) {
        let mut key = Vec::new();
        type_key(t, &mut key);
        if !self.charge(key.len()) {
            return (Arc::new(TypeDesc::Unit), None);
        }
        let next_id = self.cache.ids.len() as u32;
        let id = *self.cache.ids.entry(key.clone()).or_insert(next_id);
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
        let gens = def.generics.clone();
        let sub = |ty: &Type| ty.subst_params(&|p| gens.iter().position(|&g| g == p).map(|i| args[i].clone()));
        let depth = self.stack.len();
        self.stack.insert(key.clone(), depth);
        self.open_at.push(Vec::new());
        let mut l = None;
        let desc = match &def.kind {
            TypeDefKind::Struct { fields } => {
                let fields = fields
                    .iter()
                    .map(|f| {
                        let (fd, lf) = self.desc(&sub(&f.ty));
                        l = low(l, lf);
                        (f.name.clone(), fd, f.const_default.clone())
                    })
                    .collect();
                TypeDesc::Struct { id, def: d, name, fields }
            }
            TypeDefKind::Enum { variants } => {
                let variants = variants
                    .iter()
                    .map(|v| {
                        let payload = v
                            .fields
                            .iter()
                            .map(|x| {
                                let (fd, lf) = self.desc(&sub(x));
                                l = low(l, lf);
                                fd
                            })
                            .collect();
                        (v.name.clone(), v.rt_id, payload, v.field_names.clone())
                    })
                    .collect();
                TypeDesc::Enum { id, name, variants }
            }
            TypeDefKind::Opaque => TypeDesc::Unsupported(def.name.clone()),
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
