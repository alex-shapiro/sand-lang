//! The fast path of `Encode` and `Decode` in generic code.
//!
//! The checker gives the fast path only to closed types. Generic code, such
//! as `fn load<T: Decode>(text: str) -> Result<[T]>`, builds the dictionary
//! of `[T]` at run time from the dictionary of `T`. When the engine builds
//! the dictionary of a generic `impl` whose type arguments all have the fast
//! path, it builds the schema of the whole type from the [`SchemaTemplate`]
//! of the `impl`. The new dictionary then has the fast path too.

use super::fast::{Schema, SchemaId};
use crate::native::NativeModule;
use crate::native::ty::Param;
use crate::vm::value::{FxHashMap, Obj, VmValue};
use crate::vm::{Fail, Vm, internal};
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::sync::OnceLock;

/// The index of a [`SchemaTemplate`] in the program's table of templates.
pub(crate) type TemplateId = u32;

/// The [`SchemaId`] of the first schema that the engine builds at run time.
pub(crate) const RUNTIME_SCHEMA_BASE: SchemaId = 1 << 31;

/// The most schemas that the engine builds at run time for one program. A
/// dictionary that needs more uses the synthesized implementations.
pub(crate) const MAX_RUNTIME_SCHEMAS: usize = 1024;

/// A part of a [`SchemaTemplate`] node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TemplateRef {
    /// A closed type, by its schema in the program.
    Closed(SchemaId),
    /// Another node of the template, by its index.
    Node(u32),
    /// A type argument, by the index of its hole.
    Hole(u32),
}

/// The schema of the target type of a generic `impl` of `Encode` or
/// `Decode`, with a hole for each type argument.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaTemplate {
    /// The schemas of the types of the template that contain a type
    /// argument. Node 0 is the target type.
    pub nodes: Box<[Schema<TemplateRef>]>,
    /// For each hole, the index of the dictionary of its type argument in
    /// the dictionary of the `impl`.
    pub holes: Box<[u16]>,
    /// The number of methods of the trait, which come first in a
    /// dictionary.
    pub methods: u16,
}

/// The schemas that the engine builds from templates at run time, by
/// [`SchemaId`] from [`RUNTIME_SCHEMA_BASE`]. All runs of the program share
/// them, and a schema never changes once built.
#[derive(Debug, Default)]
pub struct RuntimeSchemas {
    nodes: OnceLock<Box<[OnceLock<Schema>]>>,
    built: Mutex<Built>,
}

#[derive(Clone, Debug, Default)]
struct Built {
    len: usize,
    /// The schema of each template for the schemas of its holes.
    by_holes: FxHashMap<(TemplateId, SmallVec<[SchemaId; 4]>), SchemaId>,
}

impl Clone for RuntimeSchemas {
    fn clone(&self) -> Self {
        RuntimeSchemas { nodes: self.nodes.clone(), built: Mutex::new(self.built.lock().clone()) }
    }
}

impl RuntimeSchemas {
    /// Returns the schema with id `id - RUNTIME_SCHEMA_BASE`.
    pub(crate) fn get(&self, index: u32) -> Option<&Schema> {
        self.nodes.get()?.get(index as usize)?.get()
    }

    /// Returns the number of schemas built.
    pub(crate) fn len(&self) -> usize {
        self.built.lock().len
    }

    /// Removes the schemas after the first `len`, and the results of the
    /// templates from `templates`.
    pub(crate) fn truncate(&mut self, len: usize, templates: usize) {
        let built = self.built.get_mut();
        built.len = built.len.min(len);
        built.by_holes.retain(|&(t, _), &mut id| (t as usize) < templates && ((id - RUNTIME_SCHEMA_BASE) as usize) < len);
        if let Some(nodes) = self.nodes.get_mut() {
            nodes.iter_mut().skip(len).for_each(|n| drop(n.take()));
        }
    }

    /// Returns the schema of template `id` for the hole schemas `holes`,
    /// and builds it on first use. Returns `None` if the table is full.
    fn instantiate(&self, id: TemplateId, template: &SchemaTemplate, holes: SmallVec<[SchemaId; 4]>) -> Option<SchemaId> {
        let mut built = self.built.lock();
        let key = (id, holes);
        if let Some(&known) = built.by_holes.get(&key) {
            return Some(known);
        }
        let start = built.len;
        if start + template.nodes.len() > MAX_RUNTIME_SCHEMAS {
            return None;
        }
        let nodes = self.nodes.get_or_init(|| (0..MAX_RUNTIME_SCHEMAS).map(|_| OnceLock::new()).collect());
        let at = |r: &TemplateRef| match *r {
            TemplateRef::Closed(s) => s,
            TemplateRef::Node(k) => RUNTIME_SCHEMA_BASE + (start as u32) + k,
            TemplateRef::Hole(h) => key.1[h as usize],
        };
        for (k, node) in template.nodes.iter().enumerate() {
            // Each index from the length on is empty, so `set` succeeds.
            let _ = nodes[start + k].set(map_parts(node, &at));
        }
        built.len = start + template.nodes.len();
        let root = RUNTIME_SCHEMA_BASE + start as u32;
        built.by_holes.insert(key, root);
        Some(root)
    }
}

/// Returns `node` with each part `r` replaced by `at(r)`.
fn map_parts<R, S>(node: &Schema<R>, at: &impl Fn(&R) -> S) -> Schema<S> {
    let all = |parts: &[R]| parts.iter().map(at).collect();
    match node {
        Schema::Unit => Schema::Unit,
        Schema::Bool => Schema::Bool,
        Schema::Int => Schema::Int,
        Schema::Float => Schema::Float,
        Schema::Duration => Schema::Duration,
        Schema::Str => Schema::Str,
        Schema::Bytes => Schema::Bytes,
        Schema::Range => Schema::Range,
        Schema::Native(d) => Schema::Native(*d),
        Schema::Json => Schema::Json,
        Schema::List(x) => Schema::List(at(x)),
        Schema::Set(x) => Schema::Set(at(x)),
        Schema::Map(k, v) => Schema::Map(at(k), at(v)),
        Schema::Option(x) => Schema::Option(at(x)),
        Schema::Tuple(xs) => Schema::Tuple(all(xs)),
        Schema::Struct(d, fields, defaults) => Schema::Struct(*d, all(fields), defaults.clone()),
        Schema::Enum(first, payloads) => Schema::Enum(*first, payloads.iter().map(|p| all(p)).collect()),
    }
}

/// Returns the fast-path schema of the type whose `Encode` or `Decode`
/// dictionary is `dict`, or `None` if the type has no fast path.
pub(crate) fn dict_schema(vm: &Vm, dict: VmValue) -> Option<SchemaId> {
    let Obj::Tuple(slots) = vm.heap.get(dict) else { return None };
    // A dictionary that this module builds holds its schema last.
    if let Some(&VmValue::Int(schema)) = slots.last() {
        return SchemaId::try_from(schema).ok();
    }
    match vm.heap.get(*slots.first()?) {
        Obj::Closure(proto, _) => vm.prog.proto_schemas.get(*proto as usize).copied().filter(|&s| s != super::fast::NO_SCHEMA),
        _ => None,
    }
}

/// Registers the native that gives a dictionary built at run time its fast
/// path.
pub(crate) fn register(m: &mut NativeModule) {
    m.function("compose_codec_dict", |vm: &mut Vm, dict: Param<0>, method: Param<1>, template: i64| -> Result<Param<0>, Fail> {
        compose(vm, dict.raw(), method.raw(), template).map(Param::from_raw)
    });
}

/// Returns `dict`, the dictionary of a generic `impl` with the template
/// `template`, with the fast path if the dictionary of each type argument
/// has it. The new dictionary has `method` in place of the first method of
/// the `impl`, and the schema last. Otherwise returns `dict`.
fn compose(vm: &mut Vm, dict: VmValue, method: VmValue, template: i64) -> Result<VmValue, Fail> {
    let prog = vm.prog.clone();
    let id = template as TemplateId;
    let Some(t) = prog.schema_templates.get(id as usize) else { return internal("codec: unknown schema template") };
    let Obj::Tuple(slots) = vm.heap.get(dict) else { return internal("codec: a dictionary is not a tuple") };
    let mut holes = SmallVec::new();
    for &at in &t.holes {
        let Some(&arg) = slots.get(at as usize) else { return internal("codec: a dictionary has no argument") };
        match dict_schema(vm, arg) {
            Some(s) => holes.push(s),
            None => return Ok(dict),
        }
    }
    let Some(schema) = prog.runtime_schemas.instantiate(id, t, holes) else { return Ok(dict) };
    let Obj::Tuple(slots) = vm.heap.get(dict) else { return internal("codec: a dictionary is not a tuple") };
    let Some(rest) = slots.get(1..t.methods as usize) else { return internal("codec: a dictionary has no methods") };
    let mut items: SmallVec<[VmValue; 4]> = SmallVec::new();
    items.push(method);
    items.extend_from_slice(rest);
    items.push(VmValue::Int(schema as i64));
    vm.check_memory(size_of::<Obj>() + items.len() * size_of::<VmValue>())?;
    Ok(vm.heap.new_tuple(&items))
}
