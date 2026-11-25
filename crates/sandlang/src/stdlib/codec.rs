//! Synchronous walkers, plans, and prelude bindings for `Encode` and `Decode`.

use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::context::{CallContext, RootedValue};
use crate::native::convert::{HostError, IntoVmValue};
use crate::native::ty::{EncodedArg, Param, TraitDict, VmVal};
use crate::types::{ConstVal, traits};
use crate::value::{StructDesc, Value, ValueSink, ValueSource};
use crate::vm::bytecode::{Const, MAX_SCANNED_SLOTS, PartFill, PartTemplate, Program, SHAPE_SLOT, Shape, ShapeKind};
use crate::vm::meter::SLOW_BYTES_PER_FUEL_UNIT;
use crate::vm::value::{FxHashMap, MAP_ENTRY_SIZE, MapObj, Obj, SET_ENTRY_SIZE, SetObj, VmValue};
use crate::vm::{Fail, Vm};
use std::any::Any;
use std::mem::size_of;
use std::sync::{Arc, Weak};

/// Heap byte estimate reserved per visited value.
pub(crate) const NODE_BYTES: usize = 64;

/// Number of visited parts per amortized memory limit check.
const CHECKED_VALUES: u32 = 32;

/// Maximum container nesting depth for encoding and decoding.
///
/// This limit also stops cyclic values.
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// Maximum nodes kept in a cached structural plan.
const MAX_CACHED_NODES: usize = 1024;

/// Maximum cached structural plans kept per VM.
const MAX_CACHED_PLANS: usize = 256;

/// Sentinel for an unresolved child entry in [`NodeKind::Structural::parts`].
const UNRESOLVED: u32 = u32::MAX;

/// Error messages for inactive or duplicate `Encoder` writes.
const ENCODER_MISUSE: [&str; 2] =
    ["an `Encoder` accepts a value only during its `encode` call", "an `encode` method wrote a second value to its `Encoder`"];

/// Error messages for inactive or duplicate `Decoder` reads.
const DECODER_MISUSE: [&str; 2] =
    ["a `Decoder` gives a value only during its `decode` call", "a `decode` method read a second value from its `Decoder`"];

/// Returns a `"limit"` error for values nested deeper than
/// [`MAX_NESTING_DEPTH`].
#[cold]
pub(crate) fn nesting_limit_error<T>() -> CodecResult<T> {
    Err(HostError::new("limit", "value too deeply nested to encode or decode"))
}

#[cold]
fn shape_mismatch<T>() -> CodecResult<T> {
    Err(Fail::internal("value does not match its codec shape").into())
}

fn shape(prog: &Program, shape_idx: u32) -> Result<&Shape, Fail> {
    prog.shapes.get(shape_idx as usize).ok_or_else(|| Fail::internal("unknown codec shape"))
}

fn with_source_path(err: HostError<Fail>, source: &(impl DecodeSource + ?Sized)) -> HostError<Fail> {
    match err.sand_error() {
        Some((kind, message)) => HostError::new(kind, format!("{}: {message}", source.path())),
        None => err,
    }
}

fn parse_error<T>(source: &(impl DecodeSource + ?Sized), msg: impl std::fmt::Display) -> CodecResult<T> {
    Err(HostError::new("parse", format!("{}: {msg}", source.path())))
}

fn err_payload(vm: &Vm, value: VmValue) -> Option<VmValue> {
    (vm.heap.variant_id(value) == vm.prog.ids.err).then(|| vm.heap.variant_payload_if(&value, vm.prog.ids.err))
}

/// Converts Sand `Err` payload `err_val` into a [`HostError`].
///
/// Preserves the kind and message of a `prelude.Error` struct. Formats any
/// other error value with kind `"invalid"`.
fn host_error_from_sand(vm: &Vm, err_val: VmValue) -> Result<HostError<Fail>, Fail> {
    let error_def = vm.prog.struct_types.get(&<crate::stdlib::core::Error as crate::native::NamedType>::rust_type()).copied();
    if let Obj::Struct(def, fields) = vm.heap.get(err_val)
        && Some(*def) == error_def
        && let [message, kind] = fields[..]
    {
        return Ok(HostError::new(vm.heap.str(kind), vm.heap.str(message)));
    }
    Ok(HostError::new("invalid", vm.display(err_val)?))
}

/// Converts a walk [`HostError`] into a [`Fail`] that terminates execution.
pub(crate) fn walk_error_to_fail(err: HostError<Fail>) -> Fail {
    match err.into_abort() {
        Ok(fail) => fail,
        Err(err) => {
            let (kind, message) = err.sand_error().unwrap_or(("error", ""));
            Fail::runtime_error(kind, message)
        }
    }
}

/// Returns the [`ShapeKind`] of `dict` if `dict` is structural.
pub(crate) fn structural_kind(vm: &Vm, dict: VmValue) -> Result<Option<&ShapeKind>, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => match (items.first(), items.get(SHAPE_SLOT)) {
            (_, Some(&VmValue::Int(shape_idx))) => Ok(Some(&shape(&vm.prog, shape_idx as u32)?.kind)),
            (Some(_), _) => Ok(None),
            (None, _) => Err(Fail::internal("a codec dictionary is empty")),
        },
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

// ------------------------------------------------------------------
// Lazily resolved dictionary plans

/// Symbolic representation of a codec dictionary.
///
/// A cached [`Plan`] contains only `Const` and `Tuple` entries and holds no
/// heap handles. Ephemeral plans for dynamic dictionaries also use `Live`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DictSpec {
    Const(u32),
    Live(u32),
    Tuple(Box<[u32]>),
}

struct PlanNode {
    spec: u32,
    kind: NodeKind,
}

enum NodeKind {
    Structural {
        shape: u32,
        /// Lazily resolved child node indices matching `Shape::parts`.
        parts: Box<[u32]>,
    },
    Custom,
}

/// Lazily expanded graph of codec dictionaries for one root dictionary.
#[derive(Default)]
struct Plan {
    specs: Vec<DictSpec>,
    spec_of: FxHashMap<DictSpec, u32>,
    nodes: Vec<PlanNode>,
    node_of_spec: FxHashMap<u32, u32>,
}

/// Per-VM cache of constant-root structural plans and constant tuple handles.
#[derive(Default)]
struct PlanCache {
    /// Program that cached plans describe; reset if the VM swaps or mutates `Program`.
    prog: Weak<Program>,
    indexed_consts: usize,
    const_by_handle: FxHashMap<u32, u32>,
    plans: FxHashMap<u32, Plan>,
}

impl PlanCache {
    fn const_idx(&self, value: VmValue) -> Option<u32> {
        let VmValue::Obj(handle) = value else { return None };
        self.const_by_handle.get(&handle).copied()
    }
}

/// Synchronizes `vm`'s [`PlanCache`] with `prog` and newly allocated constant
/// tuples without cloning `vm.consts`.
fn sync_plan_cache(vm: &mut Vm, prog: &Arc<Program>) {
    let total = vm.consts.len();
    let (reset, start) = {
        let cache = vm.native_state::<PlanCache>();
        if cache.prog.as_ptr() != Arc::as_ptr(prog) || total < cache.indexed_consts { (true, 0) } else { (false, cache.indexed_consts) }
    };
    if !reset && start == total {
        return;
    }
    let added: Vec<(u32, u32)> = (start..total)
        .filter_map(|idx| match (prog.consts.get(idx), vm.consts[idx]) {
            (Some(Const::Tuple(_)), VmValue::Obj(handle)) => Some((handle, idx as u32)),
            _ => None,
        })
        .collect();
    let cache = vm.native_state::<PlanCache>();
    if reset {
        *cache = PlanCache { prog: Arc::downgrade(prog), ..PlanCache::default() };
    }
    cache.const_by_handle.extend(added);
    cache.indexed_consts = total;
}

/// Dynamic dictionaries rooted in a single VM list for the duration of a walk.
///
/// Storing dynamic dictionaries in one pre-rooted list keeps the fiber's
/// `native_roots` stack strictly LIFO for open container frames.
#[derive(Default)]
struct LiveDicts {
    list_root: Option<RootedValue>,
    by_handle: FxHashMap<u32, u32>,
}

impl LiveDicts {
    fn is_empty(&self) -> bool {
        self.list_root.is_none()
    }

    fn get(&self, context: &CallContext, idx: u32) -> VmValue {
        let list = context.value(self.list_root.expect("live dict list"));
        context.vm().heap.list(list)[idx as usize]
    }

    fn intern(&mut self, context: &mut CallContext, value: VmValue) -> Result<u32, Fail> {
        if let VmValue::Obj(handle) = value
            && let Some(&idx) = self.by_handle.get(&handle)
        {
            return Ok(idx);
        }
        let list = context.value(self.list_root.expect("live dict list"));
        let mark = context.root_mark();
        let val_root = context.root(value);
        check_list_growth(context, list)?;
        let val = context.value(val_root);
        let idx = context.vm().heap.list(list).len() as u32;
        context.vm_mut().heap.list_push(list, val).map_err(Fail::limit)?;
        context.truncate_roots(mark);
        if let VmValue::Obj(handle) = val {
            self.by_handle.insert(handle, idx);
        }
        Ok(idx)
    }
}

/// Active plan checkout for a single encode or decode walk.
struct WalkPlan {
    root_const: Option<u32>,
    plan: Plan,
    live: LiveDicts,
}

impl WalkPlan {
    fn checkout(context: &mut CallContext, prog: &Arc<Program>, root_dict: VmValue) -> Result<(WalkPlan, u32), Fail> {
        let const_idx = {
            let vm = context.vm_mut();
            sync_plan_cache(vm, prog);
            vm.native_state::<PlanCache>().const_idx(root_dict)
        };
        if let Some(const_idx) = const_idx {
            if let Some(plan) = context.vm_mut().native_state::<PlanCache>().plans.remove(&const_idx) {
                return Ok((WalkPlan { root_const: Some(const_idx), plan, live: LiveDicts::default() }, 0));
            }
            let mut walk_plan = WalkPlan { root_const: Some(const_idx), plan: Plan::default(), live: LiveDicts::default() };
            let spec = walk_plan.intern_spec(DictSpec::Const(const_idx));
            let root = walk_plan.node_for_spec(context, prog, spec)?;
            return Ok((walk_plan, root));
        }
        let root_save = context.root(root_dict);
        context.step(|vm| vm.check_memory(NODE_BYTES))?;
        let list = context.vm_mut().heap.new_list(Vec::new());
        let list_root = context.root(list);
        let mut walk_plan = WalkPlan {
            root_const: None,
            plan: Plan::default(),
            live: LiveDicts { list_root: Some(list_root), by_handle: FxHashMap::default() },
        };
        let live_idx = walk_plan.live.intern(context, context.value(root_save))?;
        let spec = walk_plan.intern_spec(DictSpec::Live(live_idx));
        let root = walk_plan.node_for_spec(context, prog, spec)?;
        Ok((walk_plan, root))
    }

    fn return_to_cache(self, vm: &mut Vm) {
        if let Some(const_idx) = self.root_const
            && self.live.is_empty()
            && self.plan.nodes.len() <= MAX_CACHED_NODES
        {
            let plans = &mut vm.native_state::<PlanCache>().plans;
            if plans.len() < MAX_CACHED_PLANS {
                plans.insert(const_idx, self.plan);
            }
        }
    }

    fn intern_spec(&mut self, spec: DictSpec) -> u32 {
        if let Some(&id) = self.plan.spec_of.get(&spec) {
            return id;
        }
        let id = self.plan.specs.len() as u32;
        self.plan.specs.push(spec.clone());
        self.plan.spec_of.insert(spec, id);
        id
    }

    fn shape_of_spec(&self, context: &CallContext, prog: &Program, spec_id: u32) -> Result<Option<u32>, Fail> {
        let const_int = |c: u32| match prog.consts.get(c as usize) {
            Some(Const::Int(idx)) => Some(*idx as u32),
            _ => None,
        };
        match &self.plan.specs[spec_id as usize] {
            DictSpec::Const(c) => match prog.consts.get(*c as usize) {
                Some(Const::Tuple(items)) if !items.is_empty() => Ok(items.get(SHAPE_SLOT).copied().and_then(const_int)),
                Some(Const::Tuple(_)) => Err(Fail::internal("a codec dictionary is empty")),
                _ => Err(Fail::internal("a codec dictionary is not a tuple")),
            },
            DictSpec::Tuple(items) if !items.is_empty() => Ok(match items.get(SHAPE_SLOT).map(|&s| &self.plan.specs[s as usize]) {
                Some(DictSpec::Const(c)) => const_int(*c),
                _ => None,
            }),
            DictSpec::Tuple(_) => Err(Fail::internal("a codec dictionary is empty")),
            DictSpec::Live(live_idx) => match context.vm().heap.get(self.live.get(context, *live_idx)) {
                Obj::Tuple(items) if !items.is_empty() => Ok(match items.get(SHAPE_SLOT) {
                    Some(&VmValue::Int(idx)) => Some(idx as u32),
                    _ => None,
                }),
                Obj::Tuple(_) => Err(Fail::internal("a codec dictionary is empty")),
                _ => Err(Fail::internal("a codec dictionary is not a tuple")),
            },
        }
    }

    fn node_for_spec(&mut self, context: &CallContext, prog: &Program, spec_id: u32) -> Result<u32, Fail> {
        if let Some(&node_id) = self.plan.node_of_spec.get(&spec_id) {
            return Ok(node_id);
        }
        let kind = match self.shape_of_spec(context, prog, spec_id)? {
            Some(shape_idx) => {
                NodeKind::Structural { shape: shape_idx, parts: vec![UNRESOLVED; shape(prog, shape_idx)?.parts.len()].into_boxed_slice() }
            }
            None => NodeKind::Custom,
        };
        let node_id = self.plan.nodes.len() as u32;
        self.plan.nodes.push(PlanNode { spec: spec_id, kind });
        self.plan.node_of_spec.insert(spec_id, node_id);
        Ok(node_id)
    }

    fn resolve_part(&mut self, context: &mut CallContext, prog: &Program, node_id: u32, part_idx: usize) -> Result<u32, Fail> {
        let (outer_spec, shape_idx) = match &self.plan.nodes[node_id as usize] {
            PlanNode { spec, kind: NodeKind::Structural { shape, parts } } => {
                let cached = *parts.get(part_idx).ok_or_else(|| Fail::internal("codec part index out of bounds"))?;
                if cached != UNRESOLVED {
                    return Ok(cached);
                }
                (*spec, *shape)
            }
            _ => return Err(Fail::internal("codec part requested for a non-structural node")),
        };
        let child_spec = self.eval_spec(context, prog, outer_spec, &shape(prog, shape_idx)?.parts[part_idx])?;
        let child_node = self.node_for_spec(context, prog, child_spec)?;
        if let NodeKind::Structural { parts, .. } = &mut self.plan.nodes[node_id as usize].kind {
            parts[part_idx] = child_node;
        }
        Ok(child_node)
    }

    fn eval_spec(&mut self, context: &mut CallContext, prog: &Program, outer_spec: u32, template: &PartTemplate) -> Result<u32, Fail> {
        match template {
            PartTemplate::Const(const_idx) => Ok(self.intern_spec(DictSpec::Const(*const_idx))),
            PartTemplate::Bound(slot) => self.spec_slot(context, prog, outer_spec, *slot as usize),
            PartTemplate::Field(inner, slot) => {
                let inner_spec = self.eval_spec(context, prog, outer_spec, inner)?;
                self.spec_slot(context, prog, inner_spec, *slot as usize)
            }
            PartTemplate::Dict(items) => {
                let resolved: Vec<u32> = items.iter().map(|t| self.eval_spec(context, prog, outer_spec, t)).collect::<Result<_, _>>()?;
                Ok(self.intern_spec(DictSpec::Tuple(resolved.into_boxed_slice())))
            }
        }
    }

    fn spec_slot(&mut self, context: &mut CallContext, prog: &Program, dict_spec: u32, slot: usize) -> Result<u32, Fail> {
        let oob = || Fail::internal("codec dictionary slot out of bounds");
        match self.plan.specs[dict_spec as usize].clone() {
            DictSpec::Const(c) => match prog.consts.get(c as usize) {
                Some(Const::Tuple(items)) => Ok(self.intern_spec(DictSpec::Const(*items.get(slot).ok_or_else(oob)?))),
                _ => Err(Fail::internal("a codec dictionary is not a tuple")),
            },
            DictSpec::Tuple(items) => items.get(slot).copied().ok_or_else(oob),
            DictSpec::Live(live_idx) => {
                let field = context.vm().heap.field(self.live.get(context, live_idx), slot).ok_or_else(oob)?;
                if let Some(const_idx) = context.vm_mut().native_state::<PlanCache>().const_idx(field) {
                    return Ok(self.intern_spec(DictSpec::Const(const_idx)));
                }
                let idx = self.live.intern(context, field)?;
                Ok(self.intern_spec(DictSpec::Live(idx)))
            }
        }
    }

    /// Materializes `spec_id` as a live VM dictionary value, memoizing shared
    /// DAG sub-tuples so repeated references expand linearly.
    fn materialize(&self, context: &mut CallContext, spec_id: u32) -> Result<VmValue, Fail> {
        match &self.plan.specs[spec_id as usize] {
            DictSpec::Const(c) => Ok(context.vm().consts[*c as usize]),
            DictSpec::Live(live_idx) => Ok(self.live.get(context, *live_idx)),
            DictSpec::Tuple(_) => {
                let mark = context.root_mark();
                let mut memo = FxHashMap::default();
                let res = self.materialize_dag(context, spec_id, &mut memo);
                context.truncate_roots(mark);
                res
            }
        }
    }

    fn materialize_dag(&self, context: &mut CallContext, spec_id: u32, memo: &mut FxHashMap<u32, usize>) -> Result<VmValue, Fail> {
        if let Some(&root_idx) = memo.get(&spec_id) {
            return Ok(context.root_at(root_idx));
        }
        match &self.plan.specs[spec_id as usize] {
            DictSpec::Const(c) => Ok(context.vm().consts[*c as usize]),
            DictSpec::Live(live_idx) => Ok(self.live.get(context, *live_idx)),
            DictSpec::Tuple(items) => {
                context.vm().charge(1)?;
                let len = items.len();
                let mut slot_roots = Vec::with_capacity(len);
                for &item_spec in items.iter() {
                    let val = self.materialize_dag(context, item_spec, memo)?;
                    let slot = context.push_roots(1, val)?;
                    slot_roots.push(slot);
                }
                context.step(|vm| vm.check_memory(NODE_BYTES + len * size_of::<VmValue>()))?;
                let values: Vec<VmValue> = slot_roots.iter().map(|&idx| context.root_at(idx)).collect();
                let tuple = context.vm_mut().heap.alloc(Obj::Tuple(values.into_boxed_slice()));
                let memo_root = context.push_roots(1, tuple)?;
                memo.insert(spec_id, memo_root);
                Ok(tuple)
            }
        }
    }

    /// Materializes `templates` onto `fiber.native_roots`, returning the starting root index.
    fn materialize_templates(
        &mut self,
        context: &mut CallContext,
        prog: &Program,
        outer_spec: u32,
        templates: &[PartTemplate],
    ) -> Result<usize, Fail> {
        let base = context.push_roots(templates.len(), VmValue::Unit)?;
        let temp_mark = context.root_mark();
        let mut memo = FxHashMap::default();
        let res = (|| -> Result<(), Fail> {
            for (i, template) in templates.iter().enumerate() {
                let spec = self.eval_spec(context, prog, outer_spec, template)?;
                let dict_val = self.materialize_dag(context, spec, &mut memo)?;
                context.set_root(base + i, dict_val);
            }
            Ok(())
        })();
        context.truncate_roots(temp_mark);
        res?;
        Ok(base)
    }

    fn is_known_scalar(&self, prog: &Program, node_id: u32) -> bool {
        matches!(
            self.plan.nodes[node_id as usize].kind,
            NodeKind::Structural { shape, .. }
                if matches!(
                    prog.shapes[shape as usize].kind,
                    ShapeKind::Unit
                        | ShapeKind::Bool
                        | ShapeKind::Int
                        | ShapeKind::Float
                        | ShapeKind::Str
                        | ShapeKind::Bytes
                        | ShapeKind::Duration
                        | ShapeKind::Range
                        | ShapeKind::Native(..)
                )
        )
    }
}

// ------------------------------------------------------------------
// Lock-free Encoder and Decoder sessions

/// Single-use destination passed to `Encode.encode`.
///
/// Write a value from Sand with `encoder.encode(value)`. Write a value from a
/// native `Encode.encode` function with [`Encoder::write`].
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Encoder {
    session: u64,
    turn: u64,
}

impl crate::native::NativeObject for Encoder {}

/// Single-use source passed to `Decode.decode`.
///
/// Read a value from Sand with `decoder.decode()`. Read a value from a native
/// `Decode.decode` function with [`Decoder::read`].
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Decoder {
    session: u64,
    turn: u64,
}

impl crate::native::NativeObject for Decoder {}

trait ErasedSink: EncodeSink {
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}
impl<S: EncodeSink + 'static> ErasedSink for S {
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

trait ErasedSource: DecodeSource {
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}
impl<S: DecodeSource + 'static> ErasedSource for S {
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

trait WalkSink: Sized {
    type Target: EncodeSink + ?Sized;
    fn target_mut(&mut self) -> &mut Self::Target;
    fn into_erased(self) -> Box<dyn ErasedSink>;
    fn from_erased(erased: Box<dyn ErasedSink>) -> Result<Self, Fail>;
}

impl<S: EncodeSink + 'static> WalkSink for S {
    type Target = S;
    #[inline(always)]
    fn target_mut(&mut self) -> &mut S {
        self
    }
    fn into_erased(self) -> Box<dyn ErasedSink> {
        Box::new(self)
    }
    fn from_erased(erased: Box<dyn ErasedSink>) -> Result<Self, Fail> {
        erased.into_any().downcast::<S>().map(|s| *s).map_err(|_| Fail::internal("encode: sink type mismatch"))
    }
}

struct ErasedSinkRef(Box<dyn ErasedSink>);

impl WalkSink for ErasedSinkRef {
    type Target = dyn EncodeSink + 'static;
    #[inline(always)]
    fn target_mut(&mut self) -> &mut (dyn EncodeSink + 'static) {
        &mut *self.0
    }
    fn into_erased(self) -> Box<dyn ErasedSink> {
        self.0
    }
    fn from_erased(erased: Box<dyn ErasedSink>) -> Result<Self, Fail> {
        Ok(ErasedSinkRef(erased))
    }
}

trait WalkSource: Sized {
    type Target: DecodeSource + ?Sized;
    fn target_mut(&mut self) -> &mut Self::Target;
    fn target(&self) -> &Self::Target;
    fn into_erased(self) -> Box<dyn ErasedSource>;
    fn from_erased(erased: Box<dyn ErasedSource>) -> Result<Self, Fail>;
}

impl<S: DecodeSource + 'static> WalkSource for S {
    type Target = S;
    #[inline(always)]
    fn target_mut(&mut self) -> &mut S {
        self
    }
    #[inline(always)]
    fn target(&self) -> &S {
        self
    }
    fn into_erased(self) -> Box<dyn ErasedSource> {
        Box::new(self)
    }
    fn from_erased(erased: Box<dyn ErasedSource>) -> Result<Self, Fail> {
        erased.into_any().downcast::<S>().map(|s| *s).map_err(|_| Fail::internal("decode: source type mismatch"))
    }
}

struct ErasedSourceRef(Box<dyn ErasedSource>);

impl WalkSource for ErasedSourceRef {
    type Target = dyn DecodeSource + 'static;
    #[inline(always)]
    fn target_mut(&mut self) -> &mut (dyn DecodeSource + 'static) {
        &mut *self.0
    }
    #[inline(always)]
    fn target(&self) -> &(dyn DecodeSource + 'static) {
        &*self.0
    }
    fn into_erased(self) -> Box<dyn ErasedSource> {
        self.0
    }
    fn from_erased(erased: Box<dyn ErasedSource>) -> Result<Self, Fail> {
        Ok(ErasedSourceRef(erased))
    }
}

trait SessionDirection: crate::native::NativeObject + crate::native::ObjectType + 'static {
    type Io;
    const MISUSE: [&'static str; 2];
    fn session_turn(&self) -> (u64, u64);
    fn map(sessions: &mut CodecSessions) -> &mut SessionMap<Self::Io>;
    fn make_handle(vm: &mut Vm, session: u64, turn: u64) -> Result<VmValue, Fail>;
}

impl SessionDirection for Encoder {
    type Io = Box<dyn ErasedSink>;
    const MISUSE: [&'static str; 2] = ENCODER_MISUSE;
    fn session_turn(&self) -> (u64, u64) {
        (self.session, self.turn)
    }
    fn map(sessions: &mut CodecSessions) -> &mut SessionMap<Self::Io> {
        &mut sessions.encode
    }
    fn make_handle(vm: &mut Vm, session: u64, turn: u64) -> Result<VmValue, Fail> {
        Encoder { session, turn }.into_vm_value(vm)
    }
}

impl SessionDirection for Decoder {
    type Io = Box<dyn ErasedSource>;
    const MISUSE: [&'static str; 2] = DECODER_MISUSE;
    fn session_turn(&self) -> (u64, u64) {
        (self.session, self.turn)
    }
    fn map(sessions: &mut CodecSessions) -> &mut SessionMap<Self::Io> {
        &mut sessions.decode
    }
    fn make_handle(vm: &mut Vm, session: u64, turn: u64) -> Result<VmValue, Fail> {
        Decoder { session, turn }.into_vm_value(vm)
    }
}

struct Session<IO> {
    io: Option<IO>,
    active_turn: u64,
    used: bool,
    depth: usize,
    misuse: Option<String>,
    stream_error: Option<(String, String)>,
}

impl<IO> Session<IO> {
    fn claim(&mut self, turn: u64, [inactive, second]: [&str; 2]) -> CodecResult<IO> {
        if self.active_turn != turn {
            return Err(self.misuse(inactive));
        }
        if self.used {
            return Err(self.misuse(second));
        }
        self.used = true;
        self.io.take().ok_or_else(|| self.misuse(inactive))
    }

    fn misuse(&mut self, message: &str) -> HostError<Fail> {
        self.misuse.get_or_insert_with(|| message.to_string());
        HostError::new("invalid", message)
    }

    fn record_stream_error(&mut self, err: &HostError<Fail>) {
        if let Some((kind, message)) = err.sand_error() {
            self.stream_error = Some((kind.to_string(), message.to_string()));
        }
    }
}

struct SessionMap<IO> {
    next_id: u64,
    next_turn: u64,
    map: FxHashMap<u64, Session<IO>>,
}

impl<IO> Default for SessionMap<IO> {
    fn default() -> Self {
        SessionMap { next_id: 0, next_turn: 0, map: FxHashMap::default() }
    }
}

struct SavedTurn {
    turn: u64,
    used: bool,
    depth: usize,
    misuse: Option<String>,
    stream_error: Option<(String, String)>,
}

struct TurnOutcome<IO> {
    io: Option<IO>,
    unused: bool,
    misuse: Option<String>,
    stream_error: Option<(String, String)>,
}

impl<IO> SessionMap<IO> {
    fn push_turn(&mut self, outer: Option<u64>, io: IO, depth: usize) -> Result<(u64, u64, Option<SavedTurn>), Fail> {
        self.next_turn += 1;
        let turn = self.next_turn;
        match outer {
            Some(id) => {
                let s = self.map.get_mut(&id).ok_or_else(|| Fail::internal("codec: missing outer session"))?;
                let saved = SavedTurn {
                    turn: s.active_turn,
                    used: s.used,
                    depth: s.depth,
                    misuse: s.misuse.take(),
                    stream_error: s.stream_error.take(),
                };
                (s.io, s.active_turn, s.used, s.depth) = (Some(io), turn, false, depth);
                Ok((id, turn, Some(saved)))
            }
            None => {
                self.next_id += 1;
                let id = self.next_id;
                self.map.insert(id, Session { io: Some(io), active_turn: turn, used: false, depth, misuse: None, stream_error: None });
                Ok((id, turn, None))
            }
        }
    }

    fn pop_turn(&mut self, id: u64, turn: u64, saved: Option<SavedTurn>) -> TurnOutcome<IO> {
        match saved {
            Some(prev) => {
                let s = self.map.get_mut(&id).expect("outer codec session");
                let out = TurnOutcome {
                    io: s.io.take(),
                    unused: s.active_turn == turn && !s.used,
                    misuse: s.misuse.take(),
                    stream_error: s.stream_error.take(),
                };
                (s.active_turn, s.used, s.depth, s.misuse, s.stream_error) =
                    (prev.turn, prev.used, prev.depth, prev.misuse, prev.stream_error);
                out
            }
            None => {
                let mut s = self.map.remove(&id).expect("top codec session");
                TurnOutcome {
                    io: s.io.take(),
                    unused: s.active_turn == turn && !s.used,
                    misuse: s.misuse.take(),
                    stream_error: s.stream_error.take(),
                }
            }
        }
    }
}

#[derive(Default)]
struct CodecSessions {
    encode: SessionMap<Box<dyn ErasedSink>>,
    decode: SessionMap<Box<dyn ErasedSource>>,
}

/// Claims the session I/O for `handle`, runs `f`, and restores the I/O.
fn with_session<D: SessionDirection, R>(
    context: &mut CallContext,
    handle: VmVal<D>,
    f: impl FnOnce(&mut CallContext, u64, D::Io, usize) -> (Option<D::Io>, CodecResult<R>),
) -> CodecResult<R> {
    let (session_id, turn) = handle.get(&context.vm().heap)?.session_turn();
    let (io, depth) = {
        let map = D::map(context.vm_mut().native_state::<CodecSessions>());
        let Some(session) = map.map.get_mut(&session_id) else {
            return Err(HostError::new("invalid", D::MISUSE[0]));
        };
        let depth = session.depth;
        (session.claim(turn, D::MISUSE)?, depth)
    };
    let (io_back, result) = f(context, session_id, io, depth);
    if let Some(session) = D::map(context.vm_mut().native_state::<CodecSessions>()).map.get_mut(&session_id) {
        session.io = io_back;
        if let Err(err) = &result {
            session.record_stream_error(err);
        }
    }
    result
}

impl Encoder {
    /// Writes the value of a native `encode` method to the sink of `encoder`.
    ///
    /// Use this inside a native `Encode.encode` function.
    pub fn write<T>(
        context: &mut CallContext,
        encoder: VmVal<Encoder>,
        write: impl FnOnce(&mut CallContext, &mut dyn EncodeSink) -> CodecResult<T>,
    ) -> CodecResult<T> {
        with_session(context, encoder, |context, _, mut sink, _| {
            let res = write(context, &mut *sink);
            (Some(sink), res)
        })
    }
}

impl Decoder {
    /// Reads the value of a native `decode` method from the source of
    /// `decoder`.
    ///
    /// Use this inside a native `Decode.decode` function.
    pub fn read<T>(
        context: &mut CallContext,
        decoder: VmVal<Decoder>,
        read: impl FnOnce(&mut CallContext, &mut dyn DecodeSource) -> CodecResult<T>,
    ) -> CodecResult<T> {
        with_session(context, decoder, |context, _, mut source, _| {
            let res = read(context, &mut *source);
            (Some(source), res)
        })
    }
}

trait WalkIo<D: SessionDirection>: Sized {
    fn into_io(self) -> D::Io;
    fn from_io(io: D::Io) -> Result<Self, Fail>;
}

impl<S: WalkSink> WalkIo<Encoder> for S {
    fn into_io(self) -> Box<dyn ErasedSink> {
        self.into_erased()
    }
    fn from_io(io: Box<dyn ErasedSink>) -> Result<Self, Fail> {
        Self::from_erased(io)
    }
}

impl<S: WalkSource> WalkIo<Decoder> for S {
    fn into_io(self) -> Box<dyn ErasedSource> {
        self.into_erased()
    }
    fn from_io(io: Box<dyn ErasedSource>) -> Result<Self, Fail> {
        Self::from_erased(io)
    }
}

struct CustomCallResult {
    value: VmValue,
    unused: bool,
    stream_error: Option<(String, String)>,
}

/// Runs a custom Sand or native codec callback with automatic session cleanup.
#[cold]
#[inline(never)]
fn run_custom_callback<D: SessionDirection, S: WalkIo<D>>(
    context: &mut CallContext,
    plan: &WalkPlan,
    spec_id: u32,
    extra_arg: Option<VmValue>,
    io_slot: &mut Option<S>,
    outer: (Option<u64>, usize),
) -> CodecResult<CustomCallResult> {
    let mark = context.root_mark();
    let extra_root = extra_arg.map(|v| context.root(v));
    let dict_val = plan.materialize(context, spec_id)?;
    let dict_root = context.root(dict_val);
    let callee = context.vm().heap.field(dict_val, 0).ok_or_else(|| Fail::internal("a codec dictionary is empty"))?;
    let callee_root = context.root(callee);

    let io = io_slot.take().ok_or_else(|| Fail::internal("codec: missing stream"))?.into_io();
    let (session_id, turn, saved) = D::map(context.vm_mut().native_state::<CodecSessions>()).push_turn(outer.0, io, outer.1)?;

    let call_res = match context.step(|vm| D::make_handle(vm, session_id, turn)) {
        Ok(handle_val) => match extra_root {
            Some(val_root) => context.call(context.value(callee_root), &[context.value(dict_root), context.value(val_root), handle_val]),
            None => context.call(context.value(callee_root), &[context.value(dict_root), handle_val]),
        },
        Err(err) => Err(err),
    };

    let outcome = D::map(context.vm_mut().native_state::<CodecSessions>()).pop_turn(session_id, turn, saved);
    if let Some(io_back) = outcome.io {
        *io_slot = Some(S::from_io(io_back)?);
    }
    context.truncate_roots(mark);

    let value = call_res?;
    if let Some(message) = outcome.misuse {
        return Err(HostError::new("invalid", message));
    }
    Ok(CustomCallResult { value, unused: outcome.unused, stream_error: outcome.stream_error })
}

// ------------------------------------------------------------------
// Synchronous encode walker

struct EncodeWalk<'a, 'c, S> {
    context: &'a mut CallContext<'c>,
    prog: &'a Program,
    plan: &'a mut WalkPlan,
    sink: &'a mut Option<S>,
    outer_session: Option<u64>,
    unchecked: u32,
}

impl<S: WalkSink> EncodeWalk<'_, '_, S> {
    fn sink(&mut self) -> CodecResult<&mut S::Target> {
        self.sink.as_mut().map(WalkSink::target_mut).ok_or_else(|| Fail::internal("encode: missing sink").into())
    }

    fn ensure_memory(&mut self, value: VmValue, bytes: usize) -> Result<VmValue, Fail> {
        let mark = self.context.root_mark();
        let scratch = self.context.root(value);
        self.context.step(|vm| vm.check_memory(bytes))?;
        let val = self.context.value(scratch);
        self.context.truncate_roots(mark);
        Ok(val)
    }

    fn visit(&mut self, mut value: VmValue, mut node_id: u32, depth: usize) -> CodecResult {
        let mut some_layers = 0usize;
        loop {
            if self.unchecked == 0 {
                value = self.ensure_memory(value, NODE_BYTES)?;
                self.unchecked = CHECKED_VALUES;
            }
            self.unchecked -= 1;
            self.context.vm().charge(1)?;

            match self.plan.plan.nodes[node_id as usize].kind {
                NodeKind::Custom => {
                    self.visit_custom(value, self.plan.plan.nodes[node_id as usize].spec, depth)?;
                    break;
                }
                NodeKind::Structural { shape: shape_idx, .. } => {
                    let shape = &self.prog.shapes[shape_idx as usize];
                    match (&shape.kind, value) {
                        (ShapeKind::Never, _) => return shape_mismatch(),
                        (ShapeKind::Unit, _) => self.sink()?.unit()?,
                        (ShapeKind::Bool, VmValue::Bool(flag)) => self.sink()?.bool(flag)?,
                        (ShapeKind::Int, VmValue::Int(num)) => self.sink()?.int(num)?,
                        (ShapeKind::Float, VmValue::Float(num)) => self.sink()?.float(num)?,
                        (ShapeKind::Duration, VmValue::Dur(nanos)) => self.sink()?.duration(nanos)?,
                        (ShapeKind::Str, _) => {
                            let len = self.context.vm().heap.str(value).len();
                            if len >= 1024 {
                                value = self.ensure_memory(value, len)?;
                            }
                            let text = self.context.vm().heap.str(value);
                            self.context.vm().charge_bytes(text.len())?;
                            self.sink.as_mut().ok_or_else(|| Fail::internal("encode: missing sink"))?.target_mut().str(text)?;
                        }
                        (ShapeKind::Bytes, _) => {
                            let len = self.context.vm().heap.bytes(value).len();
                            if len >= 1024 {
                                value = self.ensure_memory(value, len)?;
                            }
                            let bytes = self.context.vm().heap.bytes(value);
                            self.context.vm().charge_slow_bytes(bytes.len())?;
                            self.sink.as_mut().ok_or_else(|| Fail::internal("encode: missing sink"))?.target_mut().bytes(bytes)?;
                        }
                        (ShapeKind::Range, _) => match self.context.vm().heap.get(value) {
                            Obj::Range(start, end, inclusive) => {
                                let (s, e, inc) = (*start, *end, *inclusive);
                                self.sink()?.range(s, e, inc)?;
                            }
                            _ => return shape_mismatch(),
                        },
                        (ShapeKind::Native(def, _), _) => {
                            let Some(text) = self.context.vm().heap.get(value).value().map(|native| native.text()) else {
                                return shape_mismatch();
                            };
                            if text.len() >= 1024 {
                                self.context.step(|vm| vm.check_memory(text.len()))?;
                            }
                            self.context.vm().charge_bytes(text.len())?;
                            let name = &self.prog.types[*def as usize].desc().type_name;
                            self.sink()?.native(name, &text)?;
                        }
                        (ShapeKind::Option, _) => {
                            let vid = self.context.vm().heap.variant_id(value);
                            if vid == self.prog.ids.none {
                                self.sink()?.none()?;
                            } else if vid == self.prog.ids.some {
                                value = self.context.vm().heap.field(value, 0).ok_or_else(|| Fail::internal("encode: empty Some"))?;
                                node_id = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
                                self.sink()?.begin_some()?;
                                some_layers += 1;
                                continue;
                            } else {
                                return shape_mismatch();
                            }
                        }
                        (ShapeKind::List, _) => self.visit_list(value, node_id, depth)?,
                        (ShapeKind::Set, _) => self.visit_set(value, node_id, depth)?,
                        (ShapeKind::Map, _) => self.visit_map(value, node_id, depth)?,
                        (ShapeKind::Tuple, _) => {
                            check_depth(depth)?;
                            let len = shape.parts.len();
                            match (value, self.context.vm().heap.get(value)) {
                                (VmValue::Pair(..), _) if len == 2 => {}
                                (_, Obj::Tuple(items)) if items.len() == len => {}
                                _ => return shape_mismatch(),
                            }
                            self.sink()?.begin_tuple(len)?;
                            self.visit_fields(value, node_id, 0, len, depth + 1)?;
                        }
                        (ShapeKind::Struct(def), _) => {
                            check_depth(depth)?;
                            let Obj::Struct(d, fields) = self.context.vm().heap.get(value) else { return shape_mismatch() };
                            if *d != *def || fields.len() != shape.parts.len() {
                                return shape_mismatch();
                            }
                            let len = fields.len();
                            let desc = self.prog.types[*def as usize].desc();
                            self.sink.as_mut().ok_or_else(|| Fail::internal("encode: missing sink"))?.target_mut().begin_struct(desc)?;
                            self.visit_fields(value, node_id, 0, len, depth + 1)?;
                        }
                        (ShapeKind::Enum { def, first_variant }, _) => {
                            let Some(variant) = self.context.vm().heap.variant(value) else { return shape_mismatch() };
                            let id = variant.id();
                            let Some(v_rt) = self.prog.variants.get(id as usize) else { return shape_mismatch() };
                            if id < *first_variant || v_rt.def != *def {
                                return shape_mismatch();
                            }
                            let arity = v_rt.arity;
                            if arity > 0 {
                                check_depth(depth)?;
                            }
                            let desc = self.prog.variant_desc(id).ok_or_else(|| Fail::internal("encode: unknown variant"))?;
                            self.sink()?.begin_variant(desc, arity)?;
                            let part_base: usize = (*first_variant..id).map(|vid| self.prog.variants[vid as usize].arity).sum();
                            self.visit_fields(value, node_id, part_base, arity, depth + 1)?;
                        }
                        _ => return shape_mismatch(),
                    }
                    break;
                }
            }
        }
        for _ in 0..some_layers {
            self.sink()?.end()?;
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn visit_custom(&mut self, value: VmValue, spec_id: u32, depth: usize) -> CodecResult {
        let out = run_custom_callback::<Encoder, S>(self.context, self.plan, spec_id, Some(value), self.sink, (self.outer_session, depth))?;
        match self.context.vm().as_result(out.value) {
            Err(err_val) => Err(host_error_from_sand(self.context.vm(), err_val)?),
            Ok(_) => {
                if let Some((kind, message)) = out.stream_error {
                    return Err(HostError::new(kind, message));
                }
                if out.unused {
                    return Err(HostError::new("invalid", "an `encode` method wrote no value to its `Encoder`"));
                }
                Ok(())
            }
        }
    }

    #[inline(never)]
    fn visit_list(&mut self, value: VmValue, node_id: u32, depth: usize) -> CodecResult {
        check_depth(depth)?;
        let Obj::List(items) = self.context.vm().heap.get(value) else { return shape_mismatch() };
        let len = items.len();
        self.sink()?.begin_list(len)?;
        if len > 0 {
            let elem_node = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
            let base = self.context.push_roots(1, value)?;
            for idx in 0..len {
                let list_val = self.context.root_at(base);
                let Some(&item) = self.context.vm().heap.list(list_val).get(idx) else { break };
                self.visit(item, elem_node, depth + 1)?;
            }
            self.context.truncate_roots(base);
        }
        self.sink()?.end()
    }

    #[inline(never)]
    fn visit_set(&mut self, value: VmValue, node_id: u32, depth: usize) -> CodecResult {
        check_depth(depth)?;
        let Obj::Set(set) = self.context.vm().heap.get(value) else { return shape_mismatch() };
        let live = set.live;
        self.sink()?.begin_set(live)?;
        if live > 0 {
            let elem_node = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
            let base = self.context.push_roots(1, value)?;
            if self.plan.is_known_scalar(self.prog, elem_node) {
                let mut slot = 0;
                loop {
                    let set_val = self.context.root_at(base);
                    let Obj::Set(set) = self.context.vm().heap.get(set_val) else { return shape_mismatch() };
                    let Some((off, &Some(elem))) = set.entries[slot..].iter().enumerate().find(|(_, e)| e.is_some()) else { break };
                    slot += off + 1;
                    self.visit(elem, elem_node, depth + 1)?;
                }
            } else {
                self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
                let set_val = self.context.root_at(base);
                let iter = self.context.vm_mut().container_iter(set_val)?;
                self.context.set_root(base, iter);
                let mut elem = VmValue::Unit;
                loop {
                    let iter = self.context.root_at(base);
                    if !self.context.vm_mut().set_step(iter, &mut elem) {
                        break;
                    }
                    self.visit(elem, elem_node, depth + 1)?;
                }
            }
            self.context.truncate_roots(base);
        }
        self.sink()?.end()
    }

    #[inline(never)]
    fn visit_map(&mut self, value: VmValue, node_id: u32, depth: usize) -> CodecResult {
        check_depth(depth)?;
        let Obj::Map(map) = self.context.vm().heap.get(value) else { return shape_mismatch() };
        let live = map.live;
        self.sink()?.begin_map(live)?;
        if live > 0 {
            let key_node = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
            let val_node = self.plan.resolve_part(self.context, self.prog, node_id, 1)?;
            if self.plan.is_known_scalar(self.prog, key_node) && self.plan.is_known_scalar(self.prog, val_node) {
                let base = self.context.push_roots(1, value)?;
                let mut slot = 0;
                loop {
                    let map_val = self.context.root_at(base);
                    let Obj::Map(map) = self.context.vm().heap.get(map_val) else { return shape_mismatch() };
                    let Some((off, &Some((k, v)))) = map.entries[slot..].iter().enumerate().find(|(_, e)| e.is_some()) else { break };
                    slot += off + 1;
                    self.visit(k, key_node, depth + 1)?;
                    self.visit(v, val_node, depth + 1)?;
                }
                self.context.truncate_roots(base);
            } else {
                let base = self.context.push_roots(2, VmValue::Unit)?;
                self.context.set_root(base, value);
                self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
                let map_val = self.context.root_at(base);
                let iter = self.context.vm_mut().container_iter(map_val)?;
                self.context.set_root(base, iter);
                let mut pair = [VmValue::Unit; 2];
                loop {
                    let iter = self.context.root_at(base);
                    if !self.context.vm_mut().map_step(iter, &mut pair) {
                        break;
                    }
                    self.context.set_root(base + 1, pair[1]);
                    self.visit(pair[0], key_node, depth + 1)?;
                    let entry_val = self.context.root_at(base + 1);
                    self.visit(entry_val, val_node, depth + 1)?;
                }
                self.context.truncate_roots(base);
            }
        }
        self.sink()?.end()
    }

    #[inline(never)]
    fn visit_fields(&mut self, value: VmValue, node_id: u32, part_base: usize, len: usize, next_depth: usize) -> CodecResult {
        if len > 0 {
            let base = self.context.push_roots(1, value)?;
            for idx in 0..len {
                let container = self.context.root_at(base);
                let field_val =
                    self.context.vm().heap.field(container, idx).ok_or_else(|| Fail::internal("encode: field out of bounds"))?;
                let child_node = self.plan.resolve_part(self.context, self.prog, node_id, part_base + idx)?;
                self.visit(field_val, child_node, next_depth)?;
            }
            self.context.truncate_roots(base);
        }
        self.sink()?.end()
    }
}

/// Encodes `value` using `dict` into `sink` and passes the finished `sink` to
/// `finish`.
pub(crate) fn encode<S: EncodeSink + 'static, R>(
    context: &mut CallContext,
    sink: S,
    value: VmValue,
    dict: VmValue,
    finish: impl FnOnce(&mut CallContext, S) -> CodecResult<R>,
) -> CodecResult<R> {
    let mut sink_slot = Some(sink);
    encode_inner(context, &mut sink_slot, None, 0, value, dict)?;
    let sink = sink_slot.take().ok_or_else(|| Fail::internal("encode: missing sink"))?;
    finish(context, sink)
}

fn encode_inner<S: WalkSink>(
    context: &mut CallContext,
    sink_slot: &mut Option<S>,
    outer_session: Option<u64>,
    base_depth: usize,
    value: VmValue,
    dict: VmValue,
) -> CodecResult {
    let mark = context.root_mark();
    let root_val = context.root(value);
    let root_dict = context.root(dict);
    let prog = context.vm().prog.clone();
    let result = (|| -> CodecResult {
        let (mut plan, root_node) = WalkPlan::checkout(context, &prog, context.value(root_dict))?;
        let val = context.value(root_val);
        let walk_res = EncodeWalk { context, prog: &prog, plan: &mut plan, sink: sink_slot, outer_session, unchecked: 0 }
            .visit(val, root_node, base_depth);
        plan.return_to_cache(context.vm_mut());
        walk_res
    })();
    context.truncate_roots(mark);
    result
}

// ------------------------------------------------------------------
// Synchronous decode walker

enum SlotTarget {
    Struct(u32, Arc<StructDesc>),
    Init(Arc<[String]>),
    Variant { id: u32, part_base: usize, named: bool },
}

impl SlotTarget {
    fn names<'a>(&'a self, prog: &'a Program) -> Option<&'a [String]> {
        match self {
            SlotTarget::Struct(_, desc) => Some(&desc.fields),
            SlotTarget::Init(names) => Some(names),
            SlotTarget::Variant { id, named: true, .. } => prog.variants[*id as usize].field_names.as_deref(),
            SlotTarget::Variant { named: false, .. } => None,
        }
    }

    fn part_base(&self) -> usize {
        match self {
            SlotTarget::Variant { part_base, .. } => *part_base,
            SlotTarget::Struct(..) | SlotTarget::Init(_) => 0,
        }
    }
}

struct DecodeWalk<'a, 'c, S> {
    context: &'a mut CallContext<'c>,
    prog: &'a Program,
    plan: &'a mut WalkPlan,
    source: &'a mut Option<S>,
    outer_session: Option<u64>,
    scratch: usize,
    unchecked: u32,
}

impl<S: WalkSource> DecodeWalk<'_, '_, S> {
    fn source(&mut self) -> CodecResult<&mut S::Target> {
        self.source.as_mut().map(WalkSource::target_mut).ok_or_else(|| Fail::internal("decode: missing source").into())
    }

    fn visit(&mut self, mut node_id: u32, depth: usize) -> CodecResult<VmValue> {
        let mut some_layers = 0usize;
        let mut value = loop {
            if self.unchecked == 0 {
                self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
                self.unchecked = CHECKED_VALUES;
            }
            self.unchecked -= 1;
            let consumed = self.source()?.take_consumed_bytes();
            self.context.vm().charge(1 + (consumed / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;

            match self.plan.plan.nodes[node_id as usize].kind {
                NodeKind::Custom => break self.visit_custom(self.plan.plan.nodes[node_id as usize].spec, depth)?,
                NodeKind::Structural { shape: shape_idx, .. } => {
                    let shape = &self.prog.shapes[shape_idx as usize];
                    break match &shape.kind {
                        ShapeKind::Never => return parse_error(self.source()?, "cannot decode a value of type never"),
                        ShapeKind::Unit => {
                            self.source()?.unit()?;
                            VmValue::Unit
                        }
                        ShapeKind::Bool => VmValue::Bool(self.source()?.bool()?),
                        ShapeKind::Int => VmValue::Int(self.source()?.int()?),
                        ShapeKind::Float => VmValue::Float(self.source()?.float()?),
                        ShapeKind::Duration => VmValue::Dur(self.source()?.duration()?),
                        ShapeKind::Str => {
                            let text = self.source.as_mut().ok_or_else(|| Fail::internal("decode: missing source"))?.target_mut().str()?;
                            self.context.vm().charge_bytes(text.len())?;
                            self.context.step(|vm| {
                                vm.check_memory(NODE_BYTES.saturating_add(text.len()))?;
                                Ok::<_, Fail>(vm.new_str(&*text))
                            })?
                        }
                        ShapeKind::Bytes => {
                            let bytes =
                                self.source.as_mut().ok_or_else(|| Fail::internal("decode: missing source"))?.target_mut().bytes()?;
                            self.context.vm().charge_bytes(bytes.len())?;
                            self.context.step(|vm| {
                                vm.check_memory(NODE_BYTES.saturating_add(bytes.len()))?;
                                Ok::<_, Fail>(vm.heap.new_bytes(&*bytes))
                            })?
                        }
                        ShapeKind::Range => {
                            let (start, end, inclusive) = self.source()?.range()?;
                            self.context.step(|vm| {
                                vm.check_memory(NODE_BYTES)?;
                                Ok::<_, Fail>(vm.heap.alloc(Obj::Range(start, end, inclusive)))
                            })?
                        }
                        ShapeKind::Native(def, parse) => {
                            let type_name = &self.prog.types[*def as usize].desc().type_name;
                            let text = self
                                .source
                                .as_mut()
                                .ok_or_else(|| Fail::internal("decode: missing source"))?
                                .target_mut()
                                .native(type_name)?;
                            self.context.vm().charge_bytes(text.len())?;
                            match parse(&text) {
                                Ok(obj) => {
                                    let size = obj.size();
                                    let mut obj = Some(obj);
                                    self.context.step(|vm| {
                                        vm.check_memory(size)?;
                                        Ok::<_, Fail>(vm.heap.alloc(obj.take().expect("native object")))
                                    })?
                                }
                                Err(msg) => {
                                    drop(text);
                                    return parse_error(self.source()?, msg);
                                }
                            }
                        }
                        ShapeKind::Option => match self.source()?.option()? {
                            false => VmValue::Tag(self.prog.ids.none),
                            true => {
                                node_id = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
                                some_layers += 1;
                                continue;
                            }
                        },
                        ShapeKind::List => self.visit_list(node_id, depth)?,
                        ShapeKind::Set => self.visit_set(node_id, depth)?,
                        ShapeKind::Map => self.visit_map(node_id, depth)?,
                        ShapeKind::Tuple => self.visit_tuple(node_id, shape.parts.len(), depth)?,
                        ShapeKind::Struct(def) => {
                            check_depth(depth)?;
                            let desc = self.prog.types[*def as usize].desc();
                            self.source()?.begin_struct(desc)?;
                            let (target, slot_count) = match &shape.init {
                                Some(init) => (SlotTarget::Init(init.names.clone()), init.names.len()),
                                None => (SlotTarget::Struct(*def, desc.clone()), desc.fields.len()),
                            };
                            self.visit_slots(node_id, shape_idx, target, slot_count, depth + 1)?
                        }
                        ShapeKind::Enum { def, first_variant } => self.visit_enum(node_id, shape_idx, *def, *first_variant, depth)?,
                    };
                }
            }
        };
        for _ in 0..some_layers {
            self.source()?.end()?;
            self.context.set_root(self.scratch, value);
            self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
            let inner = self.context.root_at(self.scratch);
            value = self.context.vm_mut().some(inner);
        }
        Ok(value)
    }

    #[cold]
    #[inline(never)]
    fn visit_custom(&mut self, spec_id: u32, depth: usize) -> CodecResult<VmValue> {
        let out = run_custom_callback::<Decoder, S>(self.context, self.plan, spec_id, None, self.source, (self.outer_session, depth))?;
        match self.context.vm().as_result(out.value) {
            Err(err_val) => {
                let err = host_error_from_sand(self.context.vm(), err_val)?;
                if let Some((kind, message)) = &out.stream_error
                    && err.sand_error() == Some((kind.as_str(), message.as_str()))
                {
                    return Err(err);
                }
                let source = self.source.as_ref().ok_or_else(|| Fail::internal("decode: missing source"))?.target();
                Err(with_source_path(err, source))
            }
            Ok(ok_val) => {
                if let Some((kind, message)) = out.stream_error {
                    return Err(HostError::new(kind, message));
                }
                if out.unused {
                    self.source()?.skip()?;
                }
                Ok(ok_val)
            }
        }
    }

    #[inline(never)]
    fn visit_list(&mut self, node_id: u32, depth: usize) -> CodecResult<VmValue> {
        check_depth(depth)?;
        self.source()?.begin_list()?;
        let elem_node = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
        let base = self.context.push_roots(1, VmValue::Unit)?;
        let list = self.context.step(|vm| {
            vm.check_memory(NODE_BYTES)?;
            Ok::<_, Fail>(vm.heap.new_list(Vec::new()))
        })?;
        self.context.set_root(base, list);
        while self.source()?.next()? {
            let elem = self.visit(elem_node, depth + 1)?;
            self.context.set_root(self.scratch, elem);
            let list = self.context.root_at(base);
            check_list_growth(self.context, list)?;
            let elem = self.context.root_at(self.scratch);
            self.context.vm_mut().heap.list_push(list, elem).map_err(Fail::limit)?;
        }
        self.source()?.end()?;
        let out = self.context.root_at(base);
        self.context.truncate_roots(base);
        Ok(out)
    }

    #[inline(never)]
    fn visit_set(&mut self, node_id: u32, depth: usize) -> CodecResult<VmValue> {
        check_depth(depth)?;
        self.source()?.begin_list()?;
        let elem_node = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
        let base = self.context.push_roots(1, VmValue::Unit)?;
        let set = self.context.step(|vm| {
            vm.check_memory(NODE_BYTES)?;
            Ok::<_, Fail>(vm.heap.alloc(Obj::Set(Box::<SetObj>::default())))
        })?;
        self.context.set_root(base, set);
        while self.source()?.next()? {
            let elem = self.visit(elem_node, depth + 1)?;
            self.context.set_root(self.scratch, elem);
            let set = self.context.root_at(base);
            self.context.step(|vm| check_set_or_map_growth(vm, set, elem))?;
            let elem = self.context.root_at(self.scratch);
            self.context.vm_mut().set_add(set, elem)?;
        }
        self.source()?.end()?;
        let out = self.context.root_at(base);
        self.context.truncate_roots(base);
        Ok(out)
    }

    #[inline(never)]
    fn visit_map(&mut self, node_id: u32, depth: usize) -> CodecResult<VmValue> {
        check_depth(depth)?;
        self.source()?.begin_map()?;
        let key_node = self.plan.resolve_part(self.context, self.prog, node_id, 0)?;
        let val_node = self.plan.resolve_part(self.context, self.prog, node_id, 1)?;
        let base = self.context.push_roots(2, VmValue::Unit)?;
        let map = self.context.step(|vm| {
            vm.check_memory(NODE_BYTES)?;
            Ok::<_, Fail>(vm.heap.alloc(Obj::Map(Box::<MapObj>::default())))
        })?;
        self.context.set_root(base, map);
        while self.source()?.next()? {
            let key = self.visit(key_node, depth + 1)?;
            self.context.set_root(base + 1, key);
            let val = self.visit(val_node, depth + 1)?;
            self.context.set_root(self.scratch, val);
            let pair = self.context.roots_slice(base..base + 2);
            let (map, key) = (pair[0], pair[1]);
            self.context.step(|vm| check_set_or_map_growth(vm, map, key))?;
            let entry_val = self.context.root_at(self.scratch);
            self.context.vm_mut().map_insert(map, key, entry_val)?;
        }
        self.source()?.end()?;
        let out = self.context.root_at(base);
        self.context.truncate_roots(base);
        Ok(out)
    }

    #[inline(never)]
    fn visit_tuple(&mut self, node_id: u32, len: usize, depth: usize) -> CodecResult<VmValue> {
        check_depth(depth)?;
        self.source()?.begin_tuple(len)?;
        let base = self.context.push_roots(len, VmValue::Uninit)?;
        let mut count = 0;
        while count < len && self.source()?.next()? {
            let part_node = self.plan.resolve_part(self.context, self.prog, node_id, count)?;
            let elem = self.visit(part_node, depth + 1)?;
            self.context.set_root(base + count, elem);
            count += 1;
        }
        if count != len {
            self.context.truncate_roots(base);
            return parse_error(self.source()?, format!("expected {len} elements, found {count}"));
        }
        self.source()?.end()?;
        self.context.step(|vm| vm.check_memory(NODE_BYTES + len * size_of::<VmValue>()))?;
        let (vm, roots) = self.context.vm_and_roots();
        let tuple = vm.heap.new_tuple(&roots[base..base + len]);
        self.context.truncate_roots(base);
        Ok(tuple)
    }

    #[inline(never)]
    fn visit_enum(&mut self, node_id: u32, shape_idx: u32, def: u32, first_variant: u32, depth: usize) -> CodecResult<VmValue> {
        let first = first_variant as usize;
        let enum_rt = &self.prog.types[def as usize];
        let name = self
            .source
            .as_mut()
            .ok_or_else(|| Fail::internal("decode: missing source"))?
            .target_mut()
            .begin_variant(&enum_rt.desc().type_name)?;
        let found = self.prog.variants[first..].iter().take_while(|v| v.def == def).position(|v| v.name == *name);
        let Some(variant_offset) = found else {
            let name = crate::stdlib::truncate(&name, 64).to_string();
            return parse_error(self.source()?, format!("unknown variant `{name}` of {}", enum_rt.name));
        };
        drop(name);
        charge_scan(self.context.vm(), variant_offset)?;
        let id = (first + variant_offset) as u32;
        let variant = &self.prog.variants[id as usize];
        let form = match (variant.arity, &variant.field_names) {
            (0, _) => PayloadForm::Unit,
            (_, Some(_)) => PayloadForm::Named,
            (arity, None) => PayloadForm::Positional(arity),
        };
        self.source()?.begin_payload(form)?;
        if form == PayloadForm::Unit {
            self.source()?.end()?;
            return Ok(VmValue::Tag(id));
        }
        check_depth(depth)?;
        let part_base: usize = (first..id as usize).map(|vid| self.prog.variants[vid].arity).sum();
        let target = SlotTarget::Variant { id, part_base, named: form == PayloadForm::Named };
        self.visit_slots(node_id, shape_idx, target, variant.arity, depth + 1)
    }

    #[inline(never)]
    fn visit_slots(
        &mut self,
        node_id: u32,
        shape_idx: u32,
        target: SlotTarget,
        slot_count: usize,
        next_depth: usize,
    ) -> CodecResult<VmValue> {
        let base = self.context.push_roots(slot_count, VmValue::Uninit)?;
        let mut seen = vec![false; slot_count];
        let part_base = target.part_base();
        let shape = &self.prog.shapes[shape_idx as usize];
        let outer_spec = self.plan.plan.nodes[node_id as usize].spec;

        match target.names(self.prog) {
            Some(names) => {
                let mut guess = 0usize;
                loop {
                    let next = self.source.as_mut().ok_or_else(|| Fail::internal("decode: missing source"))?.target_mut().next_field()?;
                    let Some(name) = next else { break };
                    let found = match &shape.slot_index {
                        Some(index) if !matches!(target, SlotTarget::Variant { .. }) => index.get(&*name).map(|&s| s as usize),
                        _ => field_index(self.context.vm(), names, &name, guess)?,
                    };
                    match found {
                        Some(slot) if seen[slot] => {
                            let name = crate::stdlib::truncate(&name, 64).to_string();
                            self.context.truncate_roots(base);
                            return parse_error(self.source()?, format!("duplicate field `{name}`"));
                        }
                        Some(slot) => {
                            drop(name);
                            seen[slot] = true;
                            guess = slot + 1;
                            let part_node = self.plan.resolve_part(self.context, self.prog, node_id, part_base + slot)?;
                            let val = self.visit(part_node, next_depth)?;
                            self.context.set_root(base + slot, val);
                        }
                        None => {
                            drop(name);
                            let consumed = self.source()?.take_consumed_bytes();
                            self.context.vm().charge(1 + (consumed / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
                            self.source()?.skip()?;
                        }
                    }
                }
            }
            None => {
                for (slot, filled) in seen.iter_mut().enumerate() {
                    if !self.source()?.next()? {
                        break;
                    }
                    *filled = true;
                    let part_node = self.plan.resolve_part(self.context, self.prog, node_id, part_base + slot)?;
                    let val = self.visit(part_node, next_depth)?;
                    self.context.set_root(base + slot, val);
                }
            }
        }

        for slot in 0..slot_count {
            if seen[slot] {
                continue;
            }
            let fill_val = match shape.fills.get(slot).cloned().unwrap_or(PartFill::Required) {
                PartFill::Required => {
                    let part_node = self.plan.resolve_part(self.context, self.prog, node_id, part_base + slot)?;
                    match self.plan.plan.nodes[part_node as usize].kind {
                        NodeKind::Structural { shape: part_shape, .. }
                            if matches!(self.prog.shapes[part_shape as usize].kind, ShapeKind::Option) =>
                        {
                            VmValue::Tag(self.prog.ids.none)
                        }
                        _ => {
                            let err = match target.names(self.prog) {
                                Some(names) => parse_error(self.source()?, format!("missing field `{}`", names[slot])),
                                None => parse_error(self.source()?, format!("expected {slot_count} payload values")),
                            };
                            self.context.truncate_roots(base);
                            return err;
                        }
                    }
                }
                PartFill::Const(const_val) => materialize_const_val(self.context, &const_val)?,
                PartFill::Call(helper_const, dicts) => {
                    let dict_base = self.plan.materialize_templates(self.context, self.prog, outer_spec, &dicts)?;
                    let mut args = Vec::with_capacity(dicts.len() + slot);
                    args.extend_from_slice(self.context.roots_slice(dict_base..dict_base + dicts.len()));
                    if matches!(target, SlotTarget::Init(_)) {
                        args.extend_from_slice(self.context.roots_slice(base..base + slot));
                    }
                    let val = self.context.call(self.context.vm().consts[helper_const as usize], &args)?;
                    self.context.truncate_roots(dict_base);
                    val
                }
            };
            seen[slot] = true;
            self.context.set_root(base + slot, fill_val);
        }

        self.source()?.end()?;
        let result = match target {
            SlotTarget::Struct(def, _) => {
                self.context.step(|vm| vm.check_memory(NODE_BYTES + slot_count * size_of::<VmValue>()))?;
                let (vm, roots) = self.context.vm_and_roots();
                Ok(vm.heap.new_struct(def, &roots[base..base + slot_count]))
            }
            SlotTarget::Variant { id, .. } => {
                self.context.step(|vm| vm.check_memory(NODE_BYTES + slot_count * size_of::<VmValue>()))?;
                let (vm, roots) = self.context.vm_and_roots();
                Ok(vm.heap.new_variant(id, &roots[base..base + slot_count]))
            }
            SlotTarget::Init(_) => {
                let Some(init) = &shape.init else {
                    self.context.truncate_roots(base);
                    return Err(Fail::internal("decode: an init shape without init").into());
                };
                let dict_base = self.plan.materialize_templates(self.context, self.prog, outer_spec, &init.dicts)?;
                let mut args = Vec::with_capacity(init.dicts.len() + slot_count);
                args.extend_from_slice(self.context.roots_slice(dict_base..dict_base + init.dicts.len()));
                args.extend_from_slice(self.context.roots_slice(base..base + slot_count));
                let res = self.context.call(self.context.vm().consts[init.func as usize], &args)?;
                self.context.truncate_roots(dict_base);
                if let Some(err_val) = err_payload(self.context.vm(), res) {
                    let source = self.source.as_ref().ok_or_else(|| Fail::internal("decode: missing source"))?.target();
                    Err(with_source_path(host_error_from_sand(self.context.vm(), err_val)?, source))
                } else if self.context.vm().heap.variant_id(res) == self.prog.ids.ok {
                    Ok(self.context.vm().heap.variant_payload_if(&res, self.prog.ids.ok))
                } else {
                    Ok(res)
                }
            }
        };
        self.context.truncate_roots(base);
        result
    }
}

/// Decodes a Sand value of type `dict` from `source` and passes `(value, source)`
/// to `finish`.
pub(crate) fn decode<S: DecodeSource + 'static, R>(
    context: &mut CallContext,
    source: S,
    dict: VmValue,
    finish: impl FnOnce(&mut CallContext, VmValue, S) -> CodecResult<R>,
) -> CodecResult<R> {
    let mut source_slot = Some(source);
    let value = decode_inner(context, &mut source_slot, None, 0, dict)?;
    let source = source_slot.take().ok_or_else(|| Fail::internal("decode: missing source"))?;
    finish(context, value, source)
}

fn decode_inner<S: WalkSource>(
    context: &mut CallContext,
    source_slot: &mut Option<S>,
    outer_session: Option<u64>,
    base_depth: usize,
    dict: VmValue,
) -> CodecResult<VmValue> {
    let mark = context.root_mark();
    let root_dict = context.root(dict);
    let scratch = context.push_roots(1, VmValue::Unit)?;
    let prog = context.vm().prog.clone();
    let result = (|| -> CodecResult<VmValue> {
        let (mut plan, root_node) = WalkPlan::checkout(context, &prog, context.value(root_dict))?;
        let walk_res = DecodeWalk { context, prog: &prog, plan: &mut plan, source: source_slot, outer_session, scratch, unchecked: 0 }
            .visit(root_node, base_depth);
        plan.return_to_cache(context.vm_mut());
        walk_res
    })();
    context.truncate_roots(mark);
    result
}

fn check_depth(depth: usize) -> CodecResult {
    if depth >= MAX_NESTING_DEPTH { nesting_limit_error() } else { Ok(()) }
}

fn check_list_growth(context: &mut CallContext, list: VmValue) -> Result<(), Fail> {
    context.step(|vm| {
        if let Obj::List(items) = vm.heap.get(list)
            && items.len() == items.capacity()
        {
            vm.heap.checked_grow_capacity(items.capacity(), items.len() + 1, size_of::<VmValue>(), 0).map_err(Fail::limit)?;
        }
        Ok::<_, Fail>(())
    })
}

fn check_set_or_map_growth(vm: &mut Vm, table: VmValue, key: VmValue) -> Result<(), Fail> {
    let (len, cap, elem_size, index) = match vm.heap.get(table) {
        Obj::Set(so) => (so.entries.len(), so.entries.capacity(), SET_ENTRY_SIZE, so.index_growth(key)),
        Obj::Map(mo) => (mo.entries.len(), mo.entries.capacity(), MAP_ENTRY_SIZE, mo.index_growth(key)),
        _ => return Ok(()),
    };
    if len < cap {
        if index > 0 {
            vm.check_memory(index)?;
        }
    } else {
        vm.heap.checked_grow_capacity(cap, len + 1, elem_size, index).map_err(Fail::limit)?;
    }
    Ok(())
}

fn charge_scan(vm: &Vm, count: usize) -> Result<(), Fail> {
    vm.charge((count / 16) as u64)
}

fn field_index(vm: &Vm, names: &[String], name: &str, guess: usize) -> Result<Option<usize>, Fail> {
    if names.get(guess).is_some_and(|field| field == name) {
        return Ok(Some(guess));
    }
    if names.len() > MAX_SCANNED_SLOTS {
        charge_scan(vm, names.len())?;
    }
    Ok(names.iter().position(|field| field == name))
}

/// Builds a Sand value from a compile-time constant default.
fn materialize_const_val(context: &mut CallContext, val: &ConstVal) -> Result<VmValue, Fail> {
    context.vm().charge(1)?;
    Ok(match val {
        ConstVal::Unit => VmValue::Unit,
        ConstVal::Bool(b) => VmValue::Bool(*b),
        ConstVal::Int(i) => VmValue::Int(*i),
        ConstVal::Float(f) => VmValue::Float(*f),
        ConstVal::Dur(d) => VmValue::Dur(*d),
        ConstVal::Str(s) => {
            context.vm().charge_bytes(s.len())?;
            context.step(|vm| {
                vm.check_memory(NODE_BYTES + s.len())?;
                Ok::<_, Fail>(vm.new_str(s.as_str()))
            })?
        }
        ConstVal::Bytes(b) => {
            context.vm().charge_bytes(b.len())?;
            context.step(|vm| {
                vm.check_memory(NODE_BYTES + b.len())?;
                Ok::<_, Fail>(vm.heap.new_bytes(b.as_slice()))
            })?
        }
        ConstVal::Range(a, b, inc) => context.step(|vm| {
            vm.check_memory(NODE_BYTES)?;
            Ok::<_, Fail>(vm.heap.alloc(Obj::Range(*a, *b, *inc)))
        })?,
        ConstVal::Tuple(items) | ConstVal::List(items) | ConstVal::Struct(_, items) | ConstVal::Variant(_, items) => {
            let base = context.push_roots(items.len(), VmValue::Unit)?;
            for (i, item) in items.iter().enumerate() {
                let v = materialize_const_val(context, item)?;
                context.set_root(base + i, v);
            }
            context.step(|vm| vm.check_memory(NODE_BYTES + items.len() * size_of::<VmValue>()))?;
            let (vm, roots) = context.vm_and_roots();
            let slice = &roots[base..base + items.len()];
            let built = match val {
                ConstVal::Tuple(_) => vm.heap.new_tuple(slice),
                ConstVal::List(_) => vm.heap.new_list_from_slice(slice),
                ConstVal::Struct(def, _) => vm.heap.new_struct(*def, slice),
                ConstVal::Variant(id, _) => vm.heap.new_variant(*id, slice),
                _ => unreachable!(),
            };
            context.truncate_roots(base);
            built
        }
        ConstVal::Set(items) => {
            let base = context.push_roots(2, VmValue::Unit)?;
            let set = context.step(|vm| {
                vm.check_memory(NODE_BYTES)?;
                Ok::<_, Fail>(vm.heap.alloc(Obj::Set(Box::<SetObj>::default())))
            })?;
            context.set_root(base, set);
            for item in items {
                let v = materialize_const_val(context, item)?;
                context.set_root(base + 1, v);
                context.step(|vm| check_set_or_map_growth(vm, set, v))?;
                context.vm_mut().set_add(set, v)?;
            }
            context.truncate_roots(base);
            set
        }
        ConstVal::Map(entries) => {
            let base = context.push_roots(3, VmValue::Unit)?;
            let map = context.step(|vm| {
                vm.check_memory(NODE_BYTES)?;
                Ok::<_, Fail>(vm.heap.alloc(Obj::Map(Box::<MapObj>::default())))
            })?;
            context.set_root(base, map);
            for (k, v) in entries {
                let kv = materialize_const_val(context, k)?;
                context.set_root(base + 1, kv);
                let vv = materialize_const_val(context, v)?;
                context.set_root(base + 2, vv);
                context.step(|vm| check_set_or_map_growth(vm, map, kv))?;
                context.vm_mut().map_insert(map, kv, vv)?;
            }
            context.truncate_roots(base);
            map
        }
    })
}

// ------------------------------------------------------------------
// Host Value conversion and prelude registration

/// Encodes `value` into a host [`Value`] using `dict`.
pub(crate) fn to_value(context: &mut CallContext, value: VmValue, dict: VmValue) -> CodecResult<Value> {
    encode(context, ValueSink::new(&context.vm().heap.io_memory), value, dict, |_, mut sink| {
        sink.take().map(|(v, _)| v).ok_or_else(|| Fail::internal("to_value: no value").into())
    })
}

/// Decodes host `value` into a Sand value using `dict`.
pub(crate) fn from_value(context: &mut CallContext, value: Value, dict: VmValue) -> CodecResult<VmValue> {
    decode(context, ValueSource::new(value), dict, |_, val, _| Ok(val))
}

fn encode_with_encoder(context: &mut CallContext, dict: VmValue, value: VmValue, encoder: VmVal<Encoder>) -> CodecResult {
    with_session(context, encoder, |context, session_id, sink, depth| {
        let mut slot = Some(ErasedSinkRef(sink));
        let res = encode_inner(context, &mut slot, Some(session_id), depth, value, dict);
        (slot.take().map(|b| b.0), res)
    })
}

fn decode_with_decoder(context: &mut CallContext, dict: VmValue, decoder: VmVal<Decoder>) -> CodecResult<Param<0>> {
    with_session(context, decoder, |context, session_id, source, depth| {
        let mut slot = Some(ErasedSourceRef(source));
        let res = decode_inner(context, &mut slot, Some(session_id), depth, dict);
        (slot.take().map(|b| b.0), res.map(Param::from_raw))
    })
}

/// Registers prelude types and native functions for `Encode` and `Decode`.
pub(crate) fn register(module: &mut crate::native::NativeModule) {
    module.bind::<Encoder>().bind::<Decoder>().bind::<InputKind>().bind::<EncodedArg>();
    module.function(
        "encode_arg",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>| -> Result<VmVal<EncodedArg>, Fail> {
            let sink = ValueSink::new(&context.vm().heap.io_memory);
            encode(context, sink, value.raw(), dict.raw(), |context, mut sink| {
                let (val, reservation) = sink.take().ok_or_else(|| Fail::internal("encode_arg: no value"))?;
                context.step(|vm| vm.check_memory(NODE_BYTES))?;
                let raw = EncodedArg { value: Arc::new(val), _reservation: reservation }.into_vm_value(context.vm_mut())?;
                Ok(VmVal::<EncodedArg>::from_raw(raw))
            })
            .map_err(walk_error_to_fail)
        },
    );
    module.function(
        "Encode.encode",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>, encoder: VmVal<Encoder>| {
            encode_with_encoder(context, dict.raw(), value.raw(), encoder)
        },
    );
    module.function(
        "Encoder.encode",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, encoder: VmVal<Encoder>, value: Param<0>| {
            encode_with_encoder(context, dict.raw(), value.raw(), encoder)
        },
    );
    module.function("Decode.decode", |context: &mut CallContext, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        decode_with_decoder(context, dict.raw(), decoder)
    });
    module.function("Decoder.decode", |context: &mut CallContext, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        decode_with_decoder(context, dict.raw(), decoder)
    });
    module.function("Decoder.kind", |context: &mut CallContext, decoder: VmVal<Decoder>| -> CodecResult<InputKind> {
        let (session_id, turn) = {
            let handle = decoder.get(&context.vm().heap)?;
            (handle.session, handle.turn)
        };
        let Some(session) = context.vm_mut().native_state::<CodecSessions>().decode.map.get_mut(&session_id) else {
            return Err(HostError::new("invalid", DECODER_MISUSE[0]));
        };
        if session.active_turn != turn {
            return Err(session.misuse(DECODER_MISUSE[0]));
        }
        if session.used {
            return Err(session.misuse("a `decode` method asked for the kind of a value that it read"));
        }
        let Some(source) = session.io.as_mut() else {
            return Err(session.misuse(DECODER_MISUSE[0]));
        };
        source.kind()
    });
}
