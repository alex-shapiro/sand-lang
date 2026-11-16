//! Plans: walks of values whose dictionaries never run Sand code.
//!
//! A plan resolves the dictionaries below a root dictionary once, into a node
//! for each distinct dictionary. A walk then visits values by the plan without
//! dictionary reads. A plan walk cannot pause, so it runs only in a
//! synchronous native call. If it reaches the memory limit, the VM collects
//! garbage and runs the call again.
//!
//! The codec module caches the plans of constant root dictionaries in the VM.
//! Each call charges a fuel unit for each dictionary that its plan visits,
//! whether or not the plan is cached, so the fuel of a call depends only on
//! its type and value.

use super::{CHECKED_VALUES, CodecDispatch, MAX_NESTING_DEPTH, NODE_BYTES, charge_scan, codec_dispatch, constant, eval_template, field};
use super::{field_index, heap_handle, nesting_limit_error, parse_error, shape};
use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, PayloadForm};
use crate::vm::bytecode::{PartFill, Program, ShapeKind};
use crate::vm::meter::SLOW_BYTES_PER_UNIT;
use crate::vm::value::{FxHashMap, MapObj, Obj, SetObj, VariantRef, VmValue};
use crate::vm::{Fail, Vm};
use std::sync::{Arc, Weak};

/// Maximum count of nodes in a plan.
///
/// A polymorphically recursive type such as `enum Nest<T> { Leaf(T),
/// Deep(Nest<[T]>) }` needs a node for each nesting level, so its walks use the
/// pausing walker.
const MAX_PLAN_NODES: usize = 1024;

/// Nodes for the dictionaries below a root dictionary. Node 0 is the root.
pub(super) struct Plan {
    nodes: Vec<Node>,
}

/// A distinct dictionary in a [`Plan`].
struct Node {
    /// Index of the dictionary's shape in `Program::shapes`.
    shape: u32,
    kind: ShapeKind,
    /// Node index of each part.
    parts: Vec<u32>,
    /// For an enum, the index in `parts` of the first payload part of each
    /// variant.
    variant_parts: Vec<u32>,
}

/// Plans of constant root dictionaries in a VM.
#[derive(Default)]
struct PlanCache {
    /// Program that the cached plans describe.
    ///
    /// A weak reference makes `Arc::make_mut` move a changed program to a new
    /// allocation, so a changed program never matches.
    prog: Weak<Program>,
    /// Handles of the program's constants.
    consts: FxHashMap<u32, ()>,
    /// Plan of each constant root dictionary.
    plans: FxHashMap<u32, Planned>,
}

/// Result of building a plan.
#[derive(Clone)]
struct Planned {
    /// The plan, or `None` if a dictionary below the root runs Sand code or
    /// the plan exceeds [`MAX_PLAN_NODES`].
    plan: Option<Arc<Plan>>,
    /// Count of dictionaries that the build visited.
    visited: u64,
}

/// Returns the plan of `dict`, or `None` if a dictionary below it runs Sand
/// code or the plan exceeds [`MAX_PLAN_NODES`].
///
/// Charges a fuel unit for each dictionary that the plan visits.
pub(super) fn plan_of(vm: &mut Vm, dict: VmValue) -> Result<Option<Arc<Plan>>, Fail> {
    let prog = vm.prog.clone();
    if vm.native_state::<PlanCache>().prog.as_ptr() != Arc::as_ptr(&prog) {
        let consts = vm.consts.iter().map(|&value| (heap_handle(value), ())).collect();
        *vm.native_state::<PlanCache>() = PlanCache { prog: Arc::downgrade(&prog), consts, plans: FxHashMap::default() };
    }
    let cache = vm.native_state::<PlanCache>();
    let handle = heap_handle(dict);
    let is_const = cache.consts.contains_key(&handle);
    let planned = match cache.plans.get(&handle).filter(|_| is_const) {
        Some(planned) => planned.clone(),
        None => {
            // `eval_template` charges for each dictionary that it builds. Refund
            // that charge, so that a cached plan and a new plan cost the same.
            let fuel = vm.meter.fuel();
            let planned = build(vm, &prog, dict)?;
            vm.meter.refund_to(fuel);
            if is_const {
                vm.native_state::<PlanCache>().plans.insert(handle, planned.clone());
            }
            planned
        }
    };
    vm.charge(planned.visited)?;
    Ok(planned.plan)
}

/// Builds the plan of `dict`.
///
/// Treats dictionaries with equal slots as one node, so that recursive generic
/// types terminate.
fn build(vm: &mut Vm, prog: &Program, dict: VmValue) -> Result<Planned, Fail> {
    let mut nodes = Vec::new();
    let no_plan = |nodes: &Vec<Node>| Ok(Planned { plan: None, visited: nodes.len() as u64 + 1 });
    let mut node_of: FxHashMap<Vec<u64>, u32> = FxHashMap::default();
    let mut pending = vec![(dict, None::<(usize, usize)>)];
    while let Some((dict, parent)) = pending.pop() {
        let CodecDispatch::Structural(shape_idx) = codec_dispatch(vm, dict)? else { return no_plan(&nodes) };
        let Obj::Tuple(slots) = vm.heap.get(dict) else { return Err(Fail::internal("a codec dictionary is not a tuple")) };
        let key: Vec<u64> =
            slots.iter().map(|&slot| if let VmValue::Int(num) = slot { num as u64 } else { u64::from(heap_handle(slot)) }).collect();
        let node = match node_of.get(&key) {
            Some(&node) => node,
            None => {
                let shape = shape(prog, shape_idx)?;
                if nodes.len() == MAX_PLAN_NODES
                    || shape.init.is_some()
                    || shape.fills.iter().any(|fill| matches!(fill, PartFill::Call(..)))
                {
                    return no_plan(&nodes);
                }
                vm.check_memory(NODE_BYTES * (1 + shape.parts.len()))?;
                let node = nodes.len() as u32;
                node_of.insert(key, node);
                let variant_parts = match shape.kind {
                    ShapeKind::Enum { def, first_variant } => {
                        let variants = prog.variants[first_variant as usize..].iter().take_while(|variant| variant.def == def);
                        variants.scan(0, |next, variant| Some(std::mem::replace(next, *next + variant.arity as u32))).collect()
                    }
                    _ => Vec::new(),
                };
                nodes.push(Node { shape: shape_idx, kind: shape.kind.clone(), parts: vec![0; shape.parts.len()], variant_parts });
                for (part, template) in shape.parts.iter().enumerate().rev() {
                    pending.push((eval_template(vm, template, dict)?, Some((node as usize, part))));
                }
                node
            }
        };
        if let Some((parent, part)) = parent {
            nodes[parent].parts[part] = node;
        }
    }
    Ok(Planned { visited: nodes.len() as u64, plan: Some(Arc::new(Plan { nodes })) })
}

/// A synchronous walk of values by a [`Plan`].
pub(super) struct PlanWalk<'p> {
    plan: &'p Plan,
    prog: &'p Program,
    /// Parts visited since the previous memory limit check.
    unchecked_parts: u32,
    /// Open containers, excluding `Some` layers.
    depth: usize,
    /// Marks of the struct and payload slots that received values, for each
    /// open struct or payload.
    seen: Vec<bool>,
}

impl<'p> PlanWalk<'p> {
    pub(super) fn new(plan: &'p Plan, prog: &'p Program) -> PlanWalk<'p> {
        PlanWalk { plan, prog, unchecked_parts: 0, depth: 0, seen: Vec::new() }
    }

    /// Charges `fuel` for a visited part and checks the memory limit every
    /// [`CHECKED_VALUES`] parts.
    #[inline(always)]
    fn visit(&mut self, vm: &Vm, fuel: u64) -> Result<(), Fail> {
        vm.charge(fuel)?;
        let due = self.unchecked_parts == 0 || vm.heap.fail_next_check.get();
        self.unchecked_parts = (self.unchecked_parts + 1) % CHECKED_VALUES;
        if due { vm.check_memory(CHECKED_VALUES as usize * NODE_BYTES) } else { Ok(()) }
    }

    /// Increments the container depth. Fails above [`MAX_NESTING_DEPTH`].
    fn enter(&mut self) -> CodecResult {
        if self.depth >= MAX_NESTING_DEPTH {
            return nesting_limit_error();
        }
        self.depth += 1;
        Ok(())
    }

    /// Encodes `value` at plan node 0.
    pub(super) fn encode_root(&mut self, vm: &Vm, sink: &mut dyn EncodeSink, value: VmValue) -> CodecResult {
        self.encode(vm, sink, value, 0)
    }

    /// Writes `value` if `node` is a scalar. Returns `false` otherwise.
    #[inline(always)]
    fn encode_scalar(&mut self, vm: &Vm, sink: &mut dyn EncodeSink, value: VmValue, node: u32) -> CodecResult<bool> {
        match (&self.plan.nodes[node as usize].kind, value) {
            (ShapeKind::Int, VmValue::Int(num)) => sink.int(num)?,
            (ShapeKind::Bool, VmValue::Bool(flag)) => sink.bool(flag)?,
            (ShapeKind::Float, VmValue::Float(num)) => sink.float(num)?,
            (ShapeKind::Str, _) => {
                let text = vm.heap.str(value);
                vm.meter.charge_bytes(text.len())?;
                sink.str(text)?;
            }
            _ => return Ok(false),
        }
        self.visit(vm, 1)?;
        Ok(true)
    }

    /// Encodes `value` at plan node `node`.
    fn encode(&mut self, vm: &Vm, sink: &mut dyn EncodeSink, mut value: VmValue, mut node: u32) -> CodecResult {
        if self.encode_scalar(vm, sink, value, node)? {
            return Ok(());
        }
        let (plan, prog) = (self.plan, self.prog);
        let mut layers = 0;
        loop {
            self.visit(vm, 1)?;
            let some = &plan.nodes[node as usize];
            if !matches!(some.kind, ShapeKind::Option) {
                break;
            }
            if vm.heap.variant_id(value) == prog.ids.none {
                sink.none()?;
                return (0..layers).try_for_each(|_| sink.end());
            }
            sink.begin_some()?;
            layers += 1;
            value = vm.heap.payload_if(&value, prog.ids.some);
            node = some.parts[0];
        }
        let node = &plan.nodes[node as usize];
        let shape_mismatch = || -> CodecResult { Err(Fail::internal("encode: a value does not match its shape").into()) };
        match (&node.kind, value) {
            (ShapeKind::Unit, _) => sink.unit()?,
            (ShapeKind::Bool, VmValue::Bool(flag)) => sink.bool(flag)?,
            (ShapeKind::Int, VmValue::Int(num)) => sink.int(num)?,
            (ShapeKind::Float, VmValue::Float(num)) => sink.float(num)?,
            (ShapeKind::Duration, VmValue::Dur(nanos)) => sink.duration(nanos)?,
            (ShapeKind::Str, _) => {
                let text = vm.heap.str(value);
                vm.meter.charge_bytes(text.len())?;
                sink.str(text)?;
            }
            (ShapeKind::Bytes, _) => {
                let bytes = vm.heap.bytes_of(value);
                vm.meter.charge_slow_bytes(bytes.len())?;
                sink.bytes(bytes)?;
            }
            (ShapeKind::Range, _) => match vm.heap.get(value) {
                Obj::Range(start, end, inclusive) => sink.range(*start, *end, *inclusive)?,
                _ => return shape_mismatch(),
            },
            (ShapeKind::Native(def, _), _) => {
                let Some(text) = vm.heap.get(value).value().map(|native| native.text()) else { return shape_mismatch() };
                vm.meter.charge_bytes(text.len())?;
                sink.native(&prog.types[*def as usize].desc().type_name, &text)?;
            }
            (ShapeKind::List, _) => {
                self.enter()?;
                let items = vm.heap.list(value);
                sink.begin_list(items.len())?;
                self.encode_parts(vm, sink, items.iter().map(|&item| (item, node.parts[0])))?;
            }
            (ShapeKind::Set, _) => {
                let Obj::Set(set) = vm.heap.get(value) else { return shape_mismatch() };
                self.enter()?;
                sink.begin_set(set.live)?;
                self.encode_parts(vm, sink, set.entries.iter().flatten().map(|&item| (item, node.parts[0])))?;
            }
            (ShapeKind::Map, _) => {
                let Obj::Map(map) = vm.heap.get(value) else { return shape_mismatch() };
                self.enter()?;
                sink.begin_map(map.live)?;
                let entries = map.entries.iter().flatten();
                self.encode_parts(vm, sink, entries.flat_map(|&(key, entry)| [(key, node.parts[0]), (entry, node.parts[1])]))?;
            }
            (ShapeKind::Tuple | ShapeKind::Struct(_), _) => {
                self.enter()?;
                match node.kind {
                    ShapeKind::Struct(def) => sink.begin_struct(prog.types[def as usize].desc())?,
                    _ => sink.begin_tuple(node.parts.len())?,
                }
                match value {
                    VmValue::Obj(_) => match vm.heap.get(value) {
                        Obj::Tuple(items) | Obj::Struct(_, items) => {
                            self.encode_parts(vm, sink, items.iter().copied().zip(node.parts.iter().copied()))?;
                        }
                        _ => return shape_mismatch(),
                    },
                    _ => {
                        let items = (0..node.parts.len()).map_while(|index| field(vm, value, index).ok().flatten());
                        self.encode_parts(vm, sink, items.zip(node.parts.iter().copied()))?;
                    }
                }
            }
            (ShapeKind::Enum { .. }, _) => {
                let id = vm.heap.variant_id(value);
                let Some(desc) = prog.variant_desc(id) else { return shape_mismatch() };
                let variant = &prog.variants[id as usize];
                sink.begin_variant(desc, variant.arity)?;
                if variant.arity == 0 {
                    sink.end()?;
                } else {
                    self.enter()?;
                    let first = node.variant_parts[variant.index as usize] as usize;
                    let parts = &node.parts[first..first + variant.arity];
                    match vm.heap.variant(value) {
                        Some(VariantRef::Heap(_, items)) => {
                            self.encode_parts(vm, sink, items.iter().copied().zip(parts.iter().copied()))?
                        }
                        Some(VariantRef::Inline(_, item)) => self.encode_parts(vm, sink, item.into_iter().zip(parts.iter().copied()))?,
                        None => return shape_mismatch(),
                    }
                }
            }
            _ => return shape_mismatch(),
        }
        (0..layers).try_for_each(|_| sink.end())
    }

    /// Encodes each `(value, node)` part of an open container, then closes it.
    fn encode_parts(&mut self, vm: &Vm, sink: &mut dyn EncodeSink, parts: impl Iterator<Item = (VmValue, u32)>) -> CodecResult {
        for (value, node) in parts {
            if !self.encode_scalar(vm, sink, value, node)? {
                self.encode(vm, sink, value, node)?;
            }
        }
        self.depth -= 1;
        sink.end()
    }

    /// Decodes a value at plan node 0.
    pub(super) fn decode_root(&mut self, vm: &mut Vm, source: &mut dyn DecodeSource) -> CodecResult<VmValue> {
        self.decode(vm, source, 0)
    }

    /// Decodes a value at plan node `node`.
    #[inline(always)]
    fn decode(&mut self, vm: &mut Vm, source: &mut dyn DecodeSource, node: u32) -> CodecResult<VmValue> {
        let scalar = match self.plan.nodes[node as usize].kind {
            ShapeKind::Int => VmValue::Int(source.int()?),
            ShapeKind::Bool => VmValue::Bool(source.bool()?),
            ShapeKind::Float => VmValue::Float(source.float()?),
            _ => return self.decode_value(vm, source, node),
        };
        self.visit(vm, 1 + (source.take_consumed_bytes() / SLOW_BYTES_PER_UNIT) as u64)?;
        Ok(scalar)
    }

    /// Decodes a value at plan node `node`, including its `Some` layers.
    fn decode_value(&mut self, vm: &mut Vm, source: &mut dyn DecodeSource, mut node: u32) -> CodecResult<VmValue> {
        let (plan, prog) = (self.plan, self.prog);
        let mut layers = 0;
        let mut value = loop {
            self.visit(vm, 1 + (source.take_consumed_bytes() / SLOW_BYTES_PER_UNIT) as u64)?;
            let some = &plan.nodes[node as usize];
            match some.kind {
                ShapeKind::Option if source.option()? => {
                    layers += 1;
                    node = some.parts[0];
                }
                ShapeKind::Option => break VmValue::Tag(prog.ids.none),
                _ => break self.decode_node(vm, source, node)?,
            }
        };
        for _ in 0..layers {
            source.end()?;
            value = vm.some(value);
        }
        Ok(value)
    }

    /// Decodes a value at plan node `node`, which is not an `Option`.
    fn decode_node(&mut self, vm: &mut Vm, source: &mut dyn DecodeSource, node: u32) -> CodecResult<VmValue> {
        let (plan, prog) = (self.plan, self.prog);
        let node = &plan.nodes[node as usize];
        Ok(match &node.kind {
            ShapeKind::Never => return parse_error(source, "cannot decode a value of type never"),
            ShapeKind::Unit => {
                source.unit()?;
                VmValue::Unit
            }
            ShapeKind::Bool => VmValue::Bool(source.bool()?),
            ShapeKind::Int => VmValue::Int(source.int()?),
            ShapeKind::Float => VmValue::Float(source.float()?),
            ShapeKind::Duration => VmValue::Dur(source.duration()?),
            ShapeKind::Str => {
                let text = source.str()?;
                vm.meter.charge_bytes(text.len())?;
                vm.check_memory(text.len())?;
                vm.heap.new_str(&*text)
            }
            ShapeKind::Bytes => {
                let bytes = source.bytes()?;
                vm.meter.charge_bytes(bytes.len())?;
                vm.check_memory(bytes.len())?;
                vm.heap.new_bytes(&*bytes)
            }
            ShapeKind::Range => {
                let (start, end, inclusive) = source.range()?;
                vm.heap.alloc(Obj::Range(start, end, inclusive))
            }
            ShapeKind::Native(def, parse) => {
                let text = source.native(&prog.types[*def as usize].desc().type_name)?;
                vm.meter.charge_bytes(text.len())?;
                match parse(&text) {
                    Ok(obj) => vm.heap.alloc(obj),
                    Err(msg) => {
                        drop(text);
                        return parse_error(source, msg);
                    }
                }
            }
            ShapeKind::List | ShapeKind::Set => {
                source.begin_list()?;
                self.enter()?;
                let mut items = Vec::new();
                while source.next()? {
                    if items.len() == items.capacity() {
                        vm.check_memory(2 * items.capacity().max(4) * size_of::<VmValue>())?;
                    }
                    items.push(self.decode(vm, source, node.parts[0])?);
                }
                self.leave(source)?;
                match node.kind {
                    ShapeKind::Set => {
                        let set = vm.heap.alloc(Obj::Set(Box::<SetObj>::default()));
                        for item in items {
                            vm.set_add(set, item)?;
                        }
                        set
                    }
                    _ => vm.heap.new_list(items),
                }
            }
            ShapeKind::Map => {
                source.begin_map()?;
                self.enter()?;
                let map = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
                while source.next()? {
                    let key = self.decode(vm, source, node.parts[0])?;
                    let entry = self.decode(vm, source, node.parts[1])?;
                    vm.map_insert(map, key, entry)?;
                }
                self.leave(source)?;
                map
            }
            ShapeKind::Tuple => {
                source.begin_tuple(node.parts.len())?;
                self.enter()?;
                let mut items = Vec::with_capacity(node.parts.len());
                while items.len() < node.parts.len() && source.next()? {
                    items.push(self.decode(vm, source, node.parts[items.len()])?);
                }
                if items.len() != node.parts.len() {
                    return parse_error(source, format!("expected {} elements, found {}", node.parts.len(), items.len()));
                }
                self.leave(source)?;
                vm.heap.new_tuple(&items)
            }
            ShapeKind::Struct(def) => {
                let desc = prog.types[*def as usize].desc().clone();
                source.begin_struct(&desc)?;
                self.enter()?;
                let fields = self.decode_named(vm, source, node, &node.parts, Some(&desc.fields))?;
                self.leave(source)?;
                vm.heap.alloc(Obj::Struct(*def, fields.into_boxed_slice()))
            }
            ShapeKind::Enum { def, first_variant } => {
                let (def, first_variant) = (*def, *first_variant as usize);
                let enum_rt = &prog.types[def as usize];
                let name = source.begin_variant(&enum_rt.desc().type_name)?;
                let found = prog.variants[first_variant..]
                    .iter()
                    .take_while(|variant| variant.def == def)
                    .position(|variant| variant.name == *name);
                let Some(variant_offset) = found else {
                    let name = crate::stdlib::truncate(&name, 64).to_string();
                    return parse_error(source, format!("unknown variant `{name}` of {}", enum_rt.name));
                };
                charge_scan(vm, variant_offset)?;
                let id = (first_variant + variant_offset) as u32;
                let variant = &prog.variants[id as usize];
                let form = match (variant.arity, &variant.field_names) {
                    (0, _) => PayloadForm::Unit,
                    (_, Some(_)) => PayloadForm::Named,
                    (arity, None) => PayloadForm::Positional(arity),
                };
                source.begin_payload(form)?;
                if form == PayloadForm::Unit {
                    source.end()?;
                    return Ok(VmValue::Tag(id));
                }
                self.enter()?;
                let first = node.variant_parts[variant_offset] as usize;
                let parts = &node.parts[first..first + variant.arity];
                let payload = match &variant.field_names {
                    Some(names) => self.decode_named(vm, source, node, parts, Some(names))?,
                    None => self.decode_named(vm, source, node, parts, None)?,
                };
                self.leave(source)?;
                vm.heap.new_variant(id, &payload)
            }
            ShapeKind::Option => return Err(Fail::internal("decode: an Option node outside its layers").into()),
        })
    }

    /// Decodes the slots of a struct or variant payload with nodes `parts`.
    ///
    /// Reads fields by name if `names` is `Some`, or in order otherwise. Fills
    /// each omitted slot from the shape of `node`.
    fn decode_named(
        &mut self,
        vm: &mut Vm,
        source: &mut dyn DecodeSource,
        node: &Node,
        parts: &[u32],
        names: Option<&[String]>,
    ) -> CodecResult<Vec<VmValue>> {
        let shape = shape(self.prog, node.shape)?;
        let mut slots = vec![VmValue::Uninit; parts.len()];
        let base = self.seen.len();
        self.seen.resize(base + parts.len(), false);
        match names {
            Some(names) => {
                let mut guess = 0;
                while let Some(name) = source.next_field()? {
                    let slot = match &shape.slot_index {
                        Some(index) if matches!(node.kind, ShapeKind::Struct(_)) => index.get(&*name).map(|&slot| slot as usize),
                        _ => field_index(vm, names, &name, guess)?,
                    };
                    match slot {
                        Some(slot) if self.seen[base + slot] => {
                            let name = crate::stdlib::truncate(&name, 64).to_string();
                            return parse_error(source, format!("duplicate field `{name}`"));
                        }
                        Some(slot) => {
                            drop(name);
                            (self.seen[base + slot], guess) = (true, slot + 1);
                            slots[slot] = self.decode(vm, source, parts[slot])?;
                        }
                        None => {
                            drop(name);
                            source.skip()?;
                        }
                    }
                }
            }
            None => {
                for slot in 0..parts.len() {
                    if !source.next()? {
                        break;
                    }
                    self.seen[base + slot] = true;
                    slots[slot] = self.decode(vm, source, parts[slot])?;
                }
            }
        }
        charge_scan(vm, parts.len())?;
        for missing in (0..parts.len()).filter(|&slot| !self.seen[base + slot]) {
            slots[missing] = match shape.fills.get(missing).filter(|_| matches!(node.kind, ShapeKind::Struct(_))) {
                Some(PartFill::Const(const_val)) => constant(vm, const_val)?,
                _ if matches!(self.plan.nodes[parts[missing] as usize].kind, ShapeKind::Option) => VmValue::Tag(self.prog.ids.none),
                _ => match names {
                    Some(names) => return parse_error(source, format!("missing field `{}`", names[missing])),
                    None => return parse_error(source, format!("expected {} payload values", parts.len())),
                },
            };
        }
        self.seen.truncate(base);
        Ok(slots)
    }

    /// Closes an open container of `source`.
    fn leave(&mut self, source: &mut dyn DecodeSource) -> CodecResult {
        self.depth -= 1;
        source.end()
    }
}
