//! Runtime type descriptors for `reflect` type parameters.
//!
//! The compiler builds type descriptors as directed acyclic graphs (DAGs) so
//! that nested struct and enum definitions do not cause exponential size blowup.
//! Each struct or enum instantiation is built once and shared via [`Arc`] within
//! a descriptor and across the program through [`DescCache`].
//!
//! Recursive references to an enclosing struct or enum are represented as
//! [`TypeDesc::Recur`]. Descriptors whose recursive references all resolve to
//! nodes within the subtree are closed and cached across the entire program.
//! Descriptors that reference an outer enclosing type remain open and are reused
//! only while that enclosing type is being built.
//!
//! Descriptor construction is bounded by [`MAX_DESC_NODES`] per descriptor and
//! [`MAX_TOTAL_DESC_NODES`] per compilation.

use crate::check::CheckedProgram;
use crate::check::persist::PMap;
use crate::types::{FieldDef, FnId, Opaque, PARAM_PLACEHOLDER, Ty, TyList, TypeArena, TypeDefId, TypeDefKind, TypeId, TypePrinter};
use crate::vm::value::{DescTemplate, FieldDefault, MAX_DESC_DEPTH, TypeDesc, truncate_type_name};
use std::collections::HashMap;
use std::sync::Arc;

/// Maximum work units allowed for a single type descriptor.
pub const MAX_DESC_NODES: usize = 100_000;

/// Maximum work units allowed across all type descriptors in a single compilation.
pub const MAX_TOTAL_DESC_NODES: usize = 1_000_000;

/// Arena-independent structural key that identifies a type across REPL compilations.
type Key = Vec<u32>;

/// Cache of shared runtime type descriptors across a program or REPL session.
#[derive(Clone, Default)]
pub(crate) struct DescCache {
    /// Struct and enum instance IDs (`TypeDesc::Struct::id`).
    ids: PMap<Key, u32>,
    /// Closed descriptors for struct and enum instances.
    closed: PMap<Key, Arc<TypeDesc>>,
    /// Constant pool indices for reflected types.
    consts: PMap<Key, u32>,
    /// Template indices for parameterized reflected types.
    templates: PMap<Key, u32>,
    /// Helper function prototypes and mask bits for computed struct field defaults.
    struct_fills: PMap<TypeDefId, StructFills>,
    /// Helper function prototype that populates a set from a list.
    set_fill: Option<u32>,
    /// Work units consumed during the current compilation.
    total: usize,
}

/// Per-field `(fill_proto, mask_bit)` pairs for computed defaults of a struct.
type StructFills = Arc<[Option<(u32, u8)>]>;

/// Emits constants, templates, and helper functions referenced by type descriptors.
pub(crate) trait DescSink {
    /// Adds `desc` to the constant pool and returns its constant index.
    fn constant(&mut self, desc: Arc<TypeDesc>) -> u32;
    /// Adds `t` to the program templates and returns its template index.
    fn template(&mut self, t: DescTemplate) -> u32;
    /// Emits helper functions that evaluate the computed field defaults
    /// `(field_index, helper_fn)` of struct `def` for each set bit in a
    /// captured bitmask. Returns the `(fill_proto, mask_bit)` pair for each
    /// entry in `fields`.
    fn struct_fill(&mut self, def: TypeDefId, fields: &[(u32, FnId)]) -> Vec<Option<(u32, u8)>>;
    /// Emits a helper function that inserts elements from a captured list into a captured set.
    fn set_fill(&mut self) -> u32;
}

impl DescCache {
    /// Resets the per-compilation work counter.
    pub(crate) fn start(&mut self) {
        self.total = 0;
    }

    /// Returns the constant pool index for the descriptor of `t`, or `None` if the descriptor exceeds the size budget.
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

    /// Returns the template index for the parameterized descriptor of `t`, or `None` if the descriptor exceeds the size budget.
    pub(crate) fn template_for(
        &mut self,
        cp: &CheckedProgram,
        arena: &mut TypeArena,
        t: TypeId,
        params: Vec<u32>,
        sink: &mut dyn DescSink,
    ) -> Option<u32> {
        let mut key = Vec::new();
        type_key(arena, t, &mut key);
        self.total = self.total.saturating_add(key.len());
        if self.total > MAX_TOTAL_DESC_NODES {
            return None;
        }
        if let Some(&k) = self.templates.get(&key) {
            return Some(k);
        }
        let template = DescTemplate::new(self.describe(cp, arena, t, sink)?, params);
        self.total = self.total.saturating_add(template.size());
        if template.depth() > MAX_DESC_DEPTH || self.total > MAX_TOTAL_DESC_NODES {
            return None;
        }
        let k = sink.template(template);
        self.templates.insert(key, k);
        Some(k)
    }

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

struct Builder<'a, 'c> {
    cp: &'a CheckedProgram,
    arena: &'c mut TypeArena,
    cache: &'c mut DescCache,
    sink: &'c mut dyn DescSink,
    /// Active struct and enum instances on the recursion stack, mapped to their stack depth.
    stack: HashMap<Key, usize>,
    /// Open descriptors that reference active instances on `stack`, paired with the minimum referenced stack depth.
    open: HashMap<Key, (Arc<TypeDesc>, usize)>,
    /// Keys in `open` grouped by the depth of the enclosing instance that owns them.
    open_at: Vec<Vec<Key>>,
    work: usize,
    budget: usize,
    failed: bool,
}

fn min_depth(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl Builder<'_, '_> {
    fn charge(&mut self, units: usize) -> bool {
        self.work = self.work.saturating_add(units);
        if self.work > self.budget {
            self.failed = true;
        }
        !self.failed
    }

    /// Returns the descriptor for `t` and the minimum `stack` depth that it references.
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
                (Arc::new(TypeDesc::Map(k, v)), min_depth(lk, lv))
            }
            Ty::Tuple(ts) => {
                let mut l = None;
                let ts = self.arena.list(ts).to_vec();
                let items = ts
                    .into_iter()
                    .map(|x| {
                        let (d, lx) = self.desc(x);
                        l = min_depth(l, lx);
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
            Ty::Param(p) => leaf(TypeDesc::Param(p)),
            _ => leaf(TypeDesc::Unsupported(self.show(t))),
        }
    }

    fn struct_fills(&mut self, d: TypeDefId, fields: &[FieldDef]) -> StructFills {
        if let Some(f) = self.cache.struct_fills.get(&d) {
            return f.clone();
        }
        let computed: Vec<(u32, FnId)> =
            fields.iter().enumerate().filter_map(|(i, f)| Some((i as u32, f.default.filter(|_| f.const_default.is_none())?))).collect();
        let mut fills = vec![None; fields.len()];
        for ((i, _), fill) in computed.iter().zip(self.sink.struct_fill(d, &computed)) {
            fills[*i as usize] = fill;
        }
        let fills: Arc<[_]> = fills.into();
        self.cache.struct_fills.insert(d, fills.clone());
        fills
    }

    fn show(&self, t: TypeId) -> String {
        TypePrinter::new(self.arena, &self.cp.types, &self.cp.params, &self.cp.syms, self.cp.wk).show(t)
    }

    /// Formats the name of `t` with placeholder markers for type parameters so [`DescTemplate`] can substitute concrete names at runtime.
    fn instance_name(&self, t: TypeId) -> String {
        TypePrinter { placeholder_params: true, ..TypePrinter::new(self.arena, &self.cp.types, &self.cp.params, &self.cp.syms, self.cp.wk) }
            .show(t)
    }

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
        let mut name = match def.kind {
            TypeDefKind::Opaque(_) => self.show(t),
            _ => self.instance_name(t),
        };
        if !self.charge(name.len() / 8) {
            return (Arc::new(TypeDesc::Unit), None);
        }
        // Parameterized names are truncated after template instantiation.
        if !name.contains(PARAM_PLACEHOLDER) {
            truncate_type_name(&mut name);
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
                let fills = self.struct_fills(d, fields);
                let fields = fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let ft = self.arena.instantiate(f.ty, &def.generics, &args);
                        let (fd, lf) = self.desc(ft);
                        l = min_depth(l, lf);
                        let default = match (&f.const_default, fills[i]) {
                            (Some(c), _) => FieldDefault::Const(c.clone()),
                            (None, Some((fill, bit))) => FieldDefault::Computed { fill, bit },
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
                                l = min_depth(l, lf);
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
        for k in self.open_at.pop().unwrap_or_default() {
            self.open.remove(&k);
        }
        self.stack.remove(&key);
        let desc = Arc::new(desc);
        let l = l.filter(|&x| x < depth);
        if !self.failed {
            match l {
                None => {
                    self.cache.closed.insert(key, desc.clone());
                }
                Some(x) => {
                    self.open.insert(key.clone(), (desc.clone(), x));
                    self.open_at[depth - 1].push(key);
                }
            }
        }
        (desc, l)
    }
}
