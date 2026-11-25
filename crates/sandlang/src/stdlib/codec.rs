//! Synchronous walkers and prelude bindings for `Encode` and `Decode`.

use crate::native::context::CallContext;
use crate::native::convert::{HostError, IntoVmValue};
use crate::native::ty::{EncodedArg, Param, TraitDict, VmVal};
use crate::stdlib::json::JsonSource;
use crate::types::traits;
use crate::value::{StructDesc, Value, ValueSink, VariantDesc};
use crate::vm::bytecode::{MAX_SCANNED_SLOTS, PartFill, PartTemplate, Program, SHAPE_SLOT, Shape, ShapeKind};
use crate::vm::meter::SLOW_BYTES_PER_FUEL_UNIT;
use crate::vm::value::{MAP_ENTRY_SIZE, MapObj, Obj, SET_ENTRY_SIZE, SetObj, VmValue};
use crate::vm::{Fail, Vm};
use std::any::Any;
use std::mem::size_of;
use std::sync::Arc;

pub(crate) const NODE_BYTES: usize = 64;
const CHECKED_VALUES: u32 = 32;
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// Result returned by codec walker and format methods.
pub(crate) type CodecResult<T = ()> = Result<T, HostError<Fail>>;

/// Kind of the next value available from a `Decoder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, crate::SandEnum)]
#[sand(module = "prelude", derive(Encode, Decode))]
pub enum InputKind {
    Null,
    Bool,
    Int,
    Float,
    Duration,
    Str,
    Bytes,
    List,
    Map,
    Tuple,
    Struct,
    Range,
    Native,
    Variant,
}

/// Scalar value passed to [`EncodeSink::scalar`].
pub(crate) enum Scalar<'a> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Duration(i64),
    Str(&'a str),
    Bytes(&'a [u8]),
    Range(i64, i64, bool),
    Native(&'a str, &'a str),
    Variant(&'a Arc<VariantDesc>),
    None,
}

/// Container header passed to [`EncodeSink::begin`].
pub(crate) enum Container<'a> {
    Some,
    List(usize),
    Tuple(usize),
    Set(usize),
    Map(usize),
    Struct(&'a Arc<StructDesc>),
    Variant(&'a Arc<VariantDesc>, usize),
}

/// Target format written by the encode walker.
pub(crate) trait EncodeSink: Any + Send {
    fn scalar(&mut self, value: Scalar<'_>) -> CodecResult;
    fn begin(&mut self, kind: Container<'_>) -> CodecResult;
    fn field(&mut self, _name: &str) -> CodecResult {
        Ok(())
    }
    fn end(&mut self) -> CodecResult;
}

const ENCODER_MISUSE: [&str; 2] =
    ["an `Encoder` accepts a value only during its `encode` call", "an `encode` method wrote a second value to its `Encoder`"];
const DECODER_MISUSE: [&str; 2] =
    ["a `Decoder` gives a value only during its `decode` call", "a `decode` method read a second value from its `Decoder`"];

#[cold]
pub(crate) fn memory_limit_error<T>() -> CodecResult<T> {
    Err(Fail::limit(crate::engine::LimitKind::Memory).into())
}

#[cold]
pub(crate) fn nesting_limit_error<T>() -> CodecResult<T> {
    Err(HostError::new("limit", "value too deeply nested to encode or decode"))
}

#[cold]
fn shape_mismatch<T>() -> CodecResult<T> {
    Err(Fail::internal("value does not match its codec shape").into())
}

fn check_depth(depth: usize) -> CodecResult {
    if depth >= MAX_NESTING_DEPTH { nesting_limit_error() } else { Ok(()) }
}

#[cold]
#[inline(never)]
fn parse_error<T>(source: &JsonSource, message: impl std::fmt::Display) -> CodecResult<T> {
    Err(HostError::new("parse", format!("{}: {message}", source.path())))
}

fn host_error(vm: &Vm, value: VmValue) -> Result<HostError<Fail>, Fail> {
    let error_def = vm.prog.struct_types.get(&<crate::stdlib::core::Error as crate::native::NamedType>::rust_type()).copied();
    if let Obj::Struct(def, fields) = vm.heap.get(value)
        && Some(*def) == error_def
        && let [message, kind] = fields[..]
    {
        return Ok(HostError::new(vm.heap.str(kind), vm.heap.str(message)));
    }
    Ok(HostError::new("invalid", vm.display(value)?))
}

pub(crate) fn walk_error_to_fail(error: HostError<Fail>) -> Fail {
    error.into_abort().unwrap_or_else(|error| {
        let (kind, message) = error.sand_error().unwrap_or(("error", ""));
        Fail::runtime_error(kind, message)
    })
}

pub(crate) fn alloc_buffer(
    context: &mut CallContext,
    len: usize,
    nodes: usize,
    build: impl FnOnce(&mut Vm) -> VmValue,
) -> Result<VmValue, Fail> {
    context.vm().charge_bytes(len)?;
    let mut build = Some(build);
    context.step(|vm| {
        vm.check_memory(NODE_BYTES.saturating_mul(nodes).saturating_add(len))?;
        Ok::<_, Fail>((build.take().expect("called once"))(vm))
    })
}

fn dict_shape(vm: &Vm, dict: VmValue) -> Result<Option<u32>, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(slots) => match (slots.first(), slots.get(SHAPE_SLOT)) {
            (_, Some(&VmValue::Int(shape_idx))) => Ok(Some(shape_idx as u32)),
            (Some(_), _) => Ok(None),
            (None, _) => Err(Fail::internal("a codec dictionary is empty")),
        },
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

fn eval_template(context: &mut CallContext, outer_dict: VmValue, template: &PartTemplate) -> Result<VmValue, Fail> {
    match template {
        PartTemplate::Const(const_idx) => Ok(context.vm().consts[*const_idx as usize]),
        PartTemplate::Bound(slot) => {
            context.vm().heap.field(outer_dict, *slot as usize).ok_or_else(|| Fail::internal("codec: bound slot out of bounds"))
        }
        PartTemplate::Field(inner, slot) => {
            let inner_dict = eval_template(context, outer_dict, inner)?;
            context.vm().heap.field(inner_dict, *slot as usize).ok_or_else(|| Fail::internal("codec: field slot out of bounds"))
        }
        PartTemplate::Dict(items) => {
            let outer_root = context.root(outer_dict).0 as usize;
            let base = context.push_roots(items.len(), VmValue::Unit)?;
            for (index, item) in items.iter().enumerate() {
                let slot_value = eval_template(context, context.root_at(outer_root), item)?;
                context.set_root(base + index, slot_value);
            }
            context.step(|vm| vm.check_memory(NODE_BYTES + items.len() * size_of::<VmValue>()))?;
            let (vm, roots) = context.vm_and_roots();
            let dict = vm.heap.new_tuple(&roots[base..base + items.len()]);
            context.truncate_roots(outer_root);
            Ok(dict)
        }
    }
}

/// Single-use destination passed to `Encode.encode`.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Encoder {
    turn: u64,
}
impl crate::native::NativeObject for Encoder {}

/// Single-use source passed to `Decode.decode`.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Decoder {
    turn: u64,
}
impl crate::native::NativeObject for Decoder {}

fn sink_mut(sink: &mut Option<Box<dyn EncodeSink>>) -> CodecResult<&mut dyn EncodeSink> {
    sink.as_deref_mut().ok_or_else(|| Fail::internal("encode: missing sink").into())
}

fn source_mut(source: &mut Option<JsonSource>) -> CodecResult<&mut JsonSource> {
    source.as_mut().ok_or_else(|| Fail::internal("decode: missing source").into())
}

struct ActiveTurn<IO> {
    turn: u64,
    io: Option<IO>,
    used: bool,
    depth: usize,
    misuse: Option<&'static str>,
    stream_error: Option<(String, String)>,
}

impl<IO> ActiveTurn<IO> {
    fn record_misuse(&mut self, message: &'static str) -> HostError<Fail> {
        self.misuse.get_or_insert(message);
        HostError::new("invalid", message)
    }

    fn record_error(&mut self, error: &HostError<Fail>) {
        if let Some((kind, message)) = error.sand_error() {
            self.stream_error = Some((kind.to_string(), message.to_string()));
        }
    }
}

#[derive(Default)]
struct CodecSessions {
    next_turn: u64,
    encode: Vec<ActiveTurn<Box<dyn EncodeSink>>>,
    decode: Vec<ActiveTurn<JsonSource>>,
}

fn top_turn<'a, IO>(stack: &'a mut [ActiveTurn<IO>], turn: u64, inactive: &'static str) -> CodecResult<&'a mut ActiveTurn<IO>> {
    if stack.last().is_some_and(|entry| entry.turn == turn) {
        return Ok(stack.last_mut().expect("non-empty"));
    }
    if let Some(entry) = stack.iter_mut().find(|entry| entry.turn == turn) {
        return Err(entry.record_misuse(inactive));
    }
    Err(HostError::new("invalid", inactive))
}

trait CodecIo: Sized {
    const MISUSE: [&'static str; 2];
    fn stack(sessions: &mut CodecSessions) -> &mut Vec<ActiveTurn<Self>>;
    fn make_handle(vm: &mut Vm, turn: u64) -> Result<VmValue, Fail>;
    fn path(&self) -> Option<String> {
        None
    }
}

impl CodecIo for Box<dyn EncodeSink> {
    const MISUSE: [&'static str; 2] = ENCODER_MISUSE;
    fn stack(sessions: &mut CodecSessions) -> &mut Vec<ActiveTurn<Self>> {
        &mut sessions.encode
    }
    fn make_handle(vm: &mut Vm, turn: u64) -> Result<VmValue, Fail> {
        Encoder { turn }.into_vm_value(vm)
    }
}

impl CodecIo for JsonSource {
    const MISUSE: [&'static str; 2] = DECODER_MISUSE;
    fn stack(sessions: &mut CodecSessions) -> &mut Vec<ActiveTurn<Self>> {
        &mut sessions.decode
    }
    fn make_handle(vm: &mut Vm, turn: u64) -> Result<VmValue, Fail> {
        Decoder { turn }.into_vm_value(vm)
    }
    fn path(&self) -> Option<String> {
        Some(JsonSource::path(self))
    }
}

fn with_session<IO: CodecIo, R>(
    context: &mut CallContext,
    turn: u64,
    run: impl FnOnce(&mut CallContext, IO, usize) -> (Option<IO>, CodecResult<R>),
) -> CodecResult<R> {
    let [inactive, second] = IO::MISUSE;
    let top = top_turn(IO::stack(context.vm_mut().native_state::<CodecSessions>()), turn, inactive)?;
    if top.used {
        return Err(top.record_misuse(second));
    }
    top.used = true;
    let depth = top.depth;
    let io = top.io.take().ok_or_else(|| top.record_misuse(inactive))?;
    let (io_back, result) = run(context, io, depth);
    if let Some(top) = IO::stack(context.vm_mut().native_state::<CodecSessions>()).last_mut().filter(|entry| entry.turn == turn) {
        top.io = io_back;
        if let Err(error) = &result {
            top.record_error(error);
        }
    }
    result
}

impl Encoder {
    pub(crate) fn write<T>(
        context: &mut CallContext,
        encoder: VmVal<Encoder>,
        write: impl FnOnce(&mut CallContext, &mut dyn EncodeSink) -> CodecResult<T>,
    ) -> CodecResult<T> {
        let turn = encoder.get(&context.vm().heap)?.turn;
        with_session(context, turn, |context, mut sink: Box<dyn EncodeSink>, _| {
            let result = write(context, &mut *sink);
            (Some(sink), result)
        })
    }
}

impl Decoder {
    pub(crate) fn read<T>(
        context: &mut CallContext,
        decoder: VmVal<Decoder>,
        read: impl FnOnce(&mut CallContext, &mut JsonSource) -> CodecResult<T>,
    ) -> CodecResult<T> {
        let turn = decoder.get(&context.vm().heap)?.turn;
        with_session(context, turn, |context, mut source: JsonSource, _| {
            let result = read(context, &mut source);
            (Some(source), result)
        })
    }
}

#[cold]
#[inline(never)]
fn run_custom<IO: CodecIo>(
    context: &mut CallContext,
    dict: VmValue,
    extra_arg: Option<VmValue>,
    io_slot: &mut Option<IO>,
    depth: usize,
) -> CodecResult<(VmValue, bool)> {
    let mark = context.root_mark();
    let dict_root = context.root(dict);
    let extra_root = extra_arg.map(|value| context.root(value));
    let callee = context.vm().heap.field(dict, 0).ok_or_else(|| Fail::internal("a codec dictionary is empty"))?;
    let callee_root = context.root(callee);
    let io = io_slot.take().ok_or_else(|| Fail::internal("codec: missing stream"))?;
    let sessions = context.vm_mut().native_state::<CodecSessions>();
    sessions.next_turn += 1;
    let turn = sessions.next_turn;
    IO::stack(sessions).push(ActiveTurn { turn, io: Some(io), used: false, depth, misuse: None, stream_error: None });
    let call_result = context.step(|vm| IO::make_handle(vm, turn)).and_then(|handle| match extra_root {
        Some(value_root) => context.call(context.value(callee_root), &[context.value(dict_root), context.value(value_root), handle]),
        None => context.call(context.value(callee_root), &[context.value(dict_root), handle]),
    });
    let popped = IO::stack(context.vm_mut().native_state::<CodecSessions>()).pop().expect("active turn");
    if let Some(io_back) = popped.io {
        *io_slot = Some(io_back);
    }
    context.truncate_roots(mark);
    let result = call_result?;
    if let Some(message) = popped.misuse {
        return Err(HostError::new("invalid", message));
    }
    match context.vm().as_result(result) {
        Err(error_value) => {
            let error = host_error(context.vm(), error_value)?;
            if let Some(path) = io_slot.as_ref().and_then(IO::path)
                && !popped.stream_error.as_ref().is_some_and(|(k, m)| error.sand_error() == Some((k.as_str(), m.as_str())))
                && let Some((kind, message)) = error.sand_error()
            {
                return Err(HostError::new(kind, format!("{path}: {message}")));
            }
            Err(error)
        }
        Ok(value) => popped.stream_error.map_or(Ok((value, !popped.used)), |(kind, message)| Err(HostError::new(kind, message))),
    }
}

struct EncodeWalk<'a, 'c> {
    context: &'a mut CallContext<'c>,
    prog: &'a Program,
    sink: &'a mut Option<Box<dyn EncodeSink>>,
    unchecked: u32,
}

impl EncodeWalk<'_, '_> {
    fn visit(&mut self, mut value: VmValue, mut dict: VmValue, depth: usize) -> CodecResult {
        let mut somes = 0usize;
        loop {
            if self.unchecked == 0 {
                let mark = self.context.root_mark();
                let (value_root, dict_root) = (self.context.root(value), self.context.root(dict));
                self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
                (value, dict) = (self.context.value(value_root), self.context.value(dict_root));
                self.context.truncate_roots(mark);
                self.unchecked = CHECKED_VALUES;
            }
            self.unchecked -= 1;
            self.context.vm().charge(1)?;
            let Some(shape_idx) = dict_shape(self.context.vm(), dict)? else {
                let (_, unused) = run_custom(self.context, dict, Some(value), self.sink, depth)?;
                if unused {
                    return Err(HostError::new("invalid", "an `encode` method wrote no value to its `Encoder`"));
                }
                break;
            };
            let shape = self.prog.shapes.get(shape_idx as usize).ok_or_else(|| Fail::internal("unknown codec shape"))?;
            match (&shape.kind, value) {
                (ShapeKind::Never, _) => return shape_mismatch(),
                (ShapeKind::Unit, _) => sink_mut(self.sink)?.scalar(Scalar::Unit)?,
                (ShapeKind::Bool, VmValue::Bool(b)) => sink_mut(self.sink)?.scalar(Scalar::Bool(b))?,
                (ShapeKind::Int, VmValue::Int(n)) => sink_mut(self.sink)?.scalar(Scalar::Int(n))?,
                (ShapeKind::Float, VmValue::Float(n)) => sink_mut(self.sink)?.scalar(Scalar::Float(n))?,
                (ShapeKind::Duration, VmValue::Dur(n)) => sink_mut(self.sink)?.scalar(Scalar::Duration(n))?,
                (ShapeKind::Str, _) => {
                    let text = self.context.vm().heap.str(value);
                    self.context.vm().charge_bytes(text.len())?;
                    sink_mut(self.sink)?.scalar(Scalar::Str(text))?;
                }
                (ShapeKind::Bytes, _) => {
                    let bytes = self.context.vm().heap.bytes(value);
                    self.context.vm().charge_slow_bytes(bytes.len())?;
                    sink_mut(self.sink)?.scalar(Scalar::Bytes(bytes))?;
                }
                (ShapeKind::Range, _) => {
                    let &Obj::Range(start, end, inclusive) = self.context.vm().heap.get(value) else { return shape_mismatch() };
                    sink_mut(self.sink)?.scalar(Scalar::Range(start, end, inclusive))?;
                }
                (ShapeKind::Native(def, _), _) => {
                    let Some(text) = self.context.vm().heap.get(value).value().map(|native| native.text()) else {
                        return shape_mismatch();
                    };
                    self.context.vm().charge_bytes(text.len())?;
                    sink_mut(self.sink)?.scalar(Scalar::Native(&self.prog.types[*def as usize].desc().type_name, &text))?;
                }
                (ShapeKind::Option, _) => match self.context.vm().heap.variant_id(value) {
                    id if id == self.prog.ids.none => sink_mut(self.sink)?.scalar(Scalar::None)?,
                    id if id == self.prog.ids.some => {
                        let mark = self.context.root_mark();
                        let inner = self.context.vm().heap.field(value, 0).ok_or_else(|| Fail::internal("encode: empty Some"))?;
                        let inner_root = self.context.root(inner);
                        dict = eval_template(self.context, dict, &shape.parts[0])?;
                        value = self.context.value(inner_root);
                        self.context.truncate_roots(mark);
                        sink_mut(self.sink)?.begin(Container::Some)?;
                        somes += 1;
                        continue;
                    }
                    _ => return shape_mismatch(),
                },
                (ShapeKind::List, _) => self.visit_seq(value, dict, shape, false, depth)?,
                (ShapeKind::Set, _) => self.visit_seq(value, dict, shape, true, depth)?,
                (ShapeKind::Map, _) => self.visit_map(value, dict, shape, depth)?,
                (ShapeKind::Tuple, _) => {
                    check_depth(depth)?;
                    let len = shape.parts.len();
                    if !matches!((value, self.context.vm().heap.get(value)), (VmValue::Pair(..), _) if len == 2)
                        && !matches!(self.context.vm().heap.get(value), Obj::Tuple(items) if items.len() == len)
                    {
                        return shape_mismatch();
                    }
                    sink_mut(self.sink)?.begin(Container::Tuple(len))?;
                    self.visit_fields(value, dict, &shape.parts, None, depth + 1)?;
                }
                (ShapeKind::Struct(def), _) => {
                    check_depth(depth)?;
                    let Obj::Struct(actual_def, fields) = self.context.vm().heap.get(value) else { return shape_mismatch() };
                    if *actual_def != *def || fields.len() != shape.parts.len() {
                        return shape_mismatch();
                    }
                    let desc = self.prog.types[*def as usize].desc();
                    sink_mut(self.sink)?.begin(Container::Struct(desc))?;
                    self.visit_fields(value, dict, &shape.parts, Some(&desc.fields), depth + 1)?;
                }
                (ShapeKind::Enum { def, first_variant }, _) => {
                    let Some(variant_id) = self.context.vm().heap.variant(value).map(|variant| variant.id()) else {
                        return shape_mismatch();
                    };
                    let Some(variant) =
                        self.prog.variants.get(variant_id as usize).filter(|v| variant_id >= *first_variant && v.def == *def)
                    else {
                        return shape_mismatch();
                    };
                    let desc = self.prog.variant_desc(variant_id).ok_or_else(|| Fail::internal("encode: unknown variant"))?;
                    if variant.arity == 0 {
                        sink_mut(self.sink)?.scalar(Scalar::Variant(desc))?;
                    } else {
                        check_depth(depth)?;
                        sink_mut(self.sink)?.begin(Container::Variant(desc, variant.arity))?;
                        let part_base: usize = (*first_variant..variant_id).map(|vid| self.prog.variants[vid as usize].arity).sum();
                        let parts = &shape.parts[part_base..part_base + variant.arity];
                        self.visit_fields(value, dict, parts, desc.fields.as_deref(), depth + 1)?;
                    }
                }
                _ => return shape_mismatch(),
            }
            break;
        }
        for _ in 0..somes {
            sink_mut(self.sink)?.end()?;
        }
        Ok(())
    }

    #[inline(never)]
    fn visit_seq(&mut self, value: VmValue, dict: VmValue, shape: &Shape, is_set: bool, depth: usize) -> CodecResult {
        check_depth(depth)?;
        let len = match (is_set, self.context.vm().heap.get(value)) {
            (false, Obj::List(items)) => items.len(),
            (true, Obj::Set(set)) => set.live,
            _ => return shape_mismatch(),
        };
        sink_mut(self.sink)?.begin(if is_set { Container::Set(len) } else { Container::List(len) })?;
        if len > 0 {
            let base = self.context.root(value).0 as usize;
            let elem_dict = eval_template(self.context, dict, &shape.parts[0])?;
            self.context.root(elem_dict);
            if is_set {
                self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
                let set_value = self.context.root_at(base);
                let iter = self.context.vm_mut().container_iter(set_value)?;
                self.context.set_root(base, iter);
                let mut elem = VmValue::Unit;
                loop {
                    let iter = self.context.root_at(base);
                    if !self.context.vm_mut().set_step(iter, &mut elem) {
                        break;
                    }
                    self.visit(elem, self.context.root_at(base + 1), depth + 1)?;
                }
            } else {
                for index in 0..len {
                    let Some(&item) = self.context.vm().heap.list(self.context.root_at(base)).get(index) else { break };
                    self.visit(item, self.context.root_at(base + 1), depth + 1)?;
                }
            }
            self.context.truncate_roots(base);
        }
        sink_mut(self.sink)?.end()
    }

    #[inline(never)]
    fn visit_map(&mut self, value: VmValue, dict: VmValue, shape: &Shape, depth: usize) -> CodecResult {
        check_depth(depth)?;
        let Obj::Map(map) = self.context.vm().heap.get(value) else { return shape_mismatch() };
        let live = map.live;
        sink_mut(self.sink)?.begin(Container::Map(live))?;
        if live > 0 {
            let base = self.context.root(value).0 as usize;
            self.context.root(dict);
            for part in &shape.parts[..2] {
                let part_dict = eval_template(self.context, self.context.root_at(base + 1), part)?;
                self.context.root(part_dict);
            }
            self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
            let map_value = self.context.root_at(base);
            let iter = self.context.vm_mut().container_iter(map_value)?;
            self.context.set_root(base, iter);
            let mut pair = [VmValue::Unit; 2];
            loop {
                let iter = self.context.root_at(base);
                if !self.context.vm_mut().map_step(iter, &mut pair) {
                    break;
                }
                self.context.set_root(base + 1, pair[1]);
                self.visit(pair[0], self.context.root_at(base + 2), depth + 1)?;
                self.visit(self.context.root_at(base + 1), self.context.root_at(base + 3), depth + 1)?;
            }
            self.context.truncate_roots(base);
        }
        sink_mut(self.sink)?.end()
    }

    #[inline(never)]
    fn visit_fields(
        &mut self,
        value: VmValue,
        dict: VmValue,
        parts: &[PartTemplate],
        names: Option<&[String]>,
        next_depth: usize,
    ) -> CodecResult {
        if !parts.is_empty() {
            let base = self.context.root(value).0 as usize;
            self.context.root(dict);
            for (index, part) in parts.iter().enumerate() {
                if let Some(names) = names {
                    sink_mut(self.sink)?.field(&names[index])?;
                }
                let child_dict = eval_template(self.context, self.context.root_at(base + 1), part)?;
                let field_val = self
                    .context
                    .vm()
                    .heap
                    .field(self.context.root_at(base), index)
                    .ok_or_else(|| Fail::internal("encode: field out of bounds"))?;
                self.visit(field_val, child_dict, next_depth)?;
            }
            self.context.truncate_roots(base);
        }
        sink_mut(self.sink)?.end()
    }
}

pub(crate) fn encode<S: EncodeSink + 'static, R>(
    context: &mut CallContext,
    sink: S,
    value: VmValue,
    dict: VmValue,
    finish: impl FnOnce(&mut CallContext, S) -> CodecResult<R>,
) -> CodecResult<R> {
    let mut slot: Option<Box<dyn EncodeSink>> = Some(Box::new(sink));
    encode_inner(context, &mut slot, true, 0, value, dict)?;
    let sink = (slot.take().ok_or_else(|| Fail::internal("encode: missing sink"))? as Box<dyn Any>)
        .downcast::<S>()
        .map_err(|_| Fail::internal("encode: sink type changed"))?;
    finish(context, *sink)
}

fn encode_inner(
    context: &mut CallContext,
    sink: &mut Option<Box<dyn EncodeSink>>,
    top_level: bool,
    depth: usize,
    value: VmValue,
    dict: VmValue,
) -> CodecResult {
    let mark = context.root_mark();
    let (value_root, dict_root) = (context.root(value), context.root(dict));
    if top_level && context.vm().heap.limit.is_some_and(|limit| context.vm().heap.object_bytes >= limit / 2) {
        context.collect_garbage();
    }
    let prog = context.vm().prog.clone();
    let (value, dict) = (context.value(value_root), context.value(dict_root));
    let result = EncodeWalk { context, prog: &prog, sink, unchecked: CHECKED_VALUES }.visit(value, dict, depth);
    context.truncate_roots(mark);
    result
}

enum SlotTarget {
    Tuple,
    Struct(u32),
    Init,
    Variant { id: u32, part_base: usize, enum_name: u32 },
}

struct DecodeWalk<'a, 'c> {
    context: &'a mut CallContext<'c>,
    prog: &'a Program,
    source: &'a mut Option<JsonSource>,
    scratch: usize,
    unchecked: u32,
}

impl DecodeWalk<'_, '_> {
    fn visit(&mut self, mut dict: VmValue, depth: usize) -> CodecResult<VmValue> {
        let mut somes = 0usize;
        let mut value = loop {
            if self.unchecked == 0 {
                let mark = self.context.root_mark();
                let dict_root = self.context.root(dict);
                self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
                dict = self.context.value(dict_root);
                self.context.truncate_roots(mark);
                self.unchecked = CHECKED_VALUES;
            }
            self.unchecked -= 1;
            let consumed = source_mut(self.source)?.take_consumed_bytes();
            self.context.vm().charge(1 + (consumed / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
            let Some(shape_idx) = dict_shape(self.context.vm(), dict)? else {
                let (decoded, unread) = run_custom(self.context, dict, None, self.source, depth)?;
                if unread {
                    source_mut(self.source)?.skip()?;
                }
                break decoded;
            };
            let shape = self.prog.shapes.get(shape_idx as usize).ok_or_else(|| Fail::internal("unknown codec shape"))?;
            break match &shape.kind {
                ShapeKind::Never => return parse_error(source_mut(self.source)?, "cannot decode a value of type never"),
                ShapeKind::Unit => {
                    source_mut(self.source)?.read_null()?;
                    VmValue::Unit
                }
                ShapeKind::Bool => VmValue::Bool(source_mut(self.source)?.read_bool()?),
                ShapeKind::Int => VmValue::Int(source_mut(self.source)?.read_int()?),
                ShapeKind::Duration => VmValue::Dur(source_mut(self.source)?.read_int()?),
                ShapeKind::Float => VmValue::Float(source_mut(self.source)?.read_float()?),
                ShapeKind::Str => {
                    let text = source_mut(self.source)?.read_str("str")?;
                    alloc_buffer(self.context, text.len(), 1, |vm| vm.new_str(&*text))?
                }
                ShapeKind::Bytes => {
                    let bytes = source_mut(self.source)?.read_bytes()?;
                    alloc_buffer(self.context, bytes.len(), 1, |vm| vm.heap.new_bytes(bytes))?
                }
                ShapeKind::Range => {
                    let (start, end, inclusive) = source_mut(self.source)?.read_range()?;
                    self.context.vm_mut().heap.alloc(Obj::Range(start, end, inclusive))
                }
                ShapeKind::Native(def, parse) => {
                    let type_name = &self.prog.types[*def as usize].desc().type_name;
                    let short_name = type_name.rsplit('.').next().unwrap_or(type_name);
                    let text = source_mut(self.source)?.read_str(short_name)?;
                    self.context.vm().charge_bytes(text.len())?;
                    let obj = match parse(&text) {
                        Ok(obj) => obj,
                        Err(message) => {
                            drop(text);
                            return parse_error(source_mut(self.source)?, message);
                        }
                    };
                    let size = obj.size();
                    let mut obj = Some(obj);
                    self.context.step(|vm| {
                        vm.check_memory(size)?;
                        Ok::<_, Fail>(vm.heap.alloc(obj.take().expect("native")))
                    })?
                }
                ShapeKind::Option => match source_mut(self.source)?.option()? {
                    false => VmValue::Tag(self.prog.ids.none),
                    true => {
                        dict = eval_template(self.context, dict, &shape.parts[0])?;
                        somes += 1;
                        continue;
                    }
                },
                ShapeKind::List => self.visit_seq(dict, shape, false, depth)?,
                ShapeKind::Set => self.visit_seq(dict, shape, true, depth)?,
                ShapeKind::Map => self.visit_map(dict, shape, depth)?,
                ShapeKind::Tuple => {
                    check_depth(depth)?;
                    let len = shape.parts.len();
                    source_mut(self.source)?.open_seq(Some(len))?;
                    self.visit_slots(dict, shape, SlotTarget::Tuple, None, len, depth + 1)?
                }
                ShapeKind::Struct(def) => {
                    check_depth(depth)?;
                    source_mut(self.source)?.open_map(true)?;
                    let (target, names) = match &shape.init {
                        Some(init) => (SlotTarget::Init, init.names.as_ref()),
                        None => (SlotTarget::Struct(*def), self.prog.types[*def as usize].desc().fields.as_slice()),
                    };
                    self.visit_slots(dict, shape, target, Some(names), names.len(), depth + 1)?
                }
                ShapeKind::Enum { def, first_variant } => self.visit_enum(dict, shape, *def, *first_variant, depth)?,
            };
        };
        for _ in 0..somes {
            source_mut(self.source)?.end(None)?;
            self.context.set_root(self.scratch, value);
            self.context.step(|vm| vm.check_memory(NODE_BYTES))?;
            let inner = self.context.root_at(self.scratch);
            value = self.context.vm_mut().heap.new_variant(self.prog.ids.some, &[inner]);
        }
        Ok(value)
    }

    #[inline(never)]
    fn visit_seq(&mut self, dict: VmValue, shape: &Shape, is_set: bool, depth: usize) -> CodecResult<VmValue> {
        check_depth(depth)?;
        source_mut(self.source)?.open_seq(None)?;
        let base = self.context.root(VmValue::Unit).0 as usize;
        let elem_dict = eval_template(self.context, dict, &shape.parts[0])?;
        self.context.root(elem_dict);
        let collection = if is_set {
            self.context.vm_mut().heap.alloc(Obj::Set(Box::<SetObj>::default()))
        } else {
            self.context.vm_mut().heap.new_list(Vec::new())
        };
        self.context.set_root(base, collection);
        while source_mut(self.source)?.next()? {
            let item = self.visit(self.context.root_at(base + 1), depth + 1)?;
            self.context.set_root(self.scratch, item);
            let collection = self.context.root_at(base);
            if is_set {
                self.context.step(|vm| check_set_or_map_growth(vm, collection, item))?;
                let item = self.context.root_at(self.scratch);
                self.context.vm_mut().set_add(collection, item)?;
            } else {
                check_list_growth(self.context, collection)?;
                let item = self.context.root_at(self.scratch);
                self.context.vm_mut().heap.list_push(collection, item).map_err(Fail::limit)?;
            }
        }
        source_mut(self.source)?.end(None)?;
        let out = self.context.root_at(base);
        self.context.truncate_roots(base);
        Ok(out)
    }

    #[inline(never)]
    fn visit_map(&mut self, dict: VmValue, shape: &Shape, depth: usize) -> CodecResult<VmValue> {
        check_depth(depth)?;
        source_mut(self.source)?.open_map(false)?;
        let base = self.context.root(VmValue::Unit).0 as usize;
        self.context.root(dict);
        for part in &shape.parts[..2] {
            let part_dict = eval_template(self.context, self.context.root_at(base + 1), part)?;
            self.context.root(part_dict);
        }
        let map = self.context.vm_mut().heap.alloc(Obj::Map(Box::<MapObj>::default()));
        self.context.set_root(base, map);
        while source_mut(self.source)?.next()? {
            let key = self.visit(self.context.root_at(base + 2), depth + 1)?;
            self.context.set_root(base + 1, key);
            let value = self.visit(self.context.root_at(base + 3), depth + 1)?;
            self.context.set_root(self.scratch, value);
            let (map, key) = (self.context.root_at(base), self.context.root_at(base + 1));
            self.context.step(|vm| check_set_or_map_growth(vm, map, key))?;
            let value = self.context.root_at(self.scratch);
            self.context.vm_mut().map_insert(map, key, value)?;
        }
        source_mut(self.source)?.end(None)?;
        let out = self.context.root_at(base);
        self.context.truncate_roots(base);
        Ok(out)
    }

    #[inline(never)]
    fn visit_enum(&mut self, dict: VmValue, shape: &Shape, def: u32, first_variant: u32, depth: usize) -> CodecResult<VmValue> {
        let type_rt = &self.prog.types[def as usize];
        let variant_name = source_mut(self.source)?.begin_variant(&type_rt.name)?;
        let Some(offset) =
            self.prog.variants[first_variant as usize..].iter().take_while(|v| v.def == def).position(|v| v.name == variant_name)
        else {
            let name = crate::stdlib::truncate(variant_name, 64).to_string();
            return parse_error(source_mut(self.source)?, format!("unknown variant `{name}` of {}", type_rt.name));
        };
        self.context.vm().charge((offset / 16) as u64)?;
        let variant_id = first_variant + offset as u32;
        let variant = &self.prog.variants[variant_id as usize];
        let named = variant.field_names.is_some();
        source_mut(self.source)?.begin_payload(&type_rt.name, variant.arity, named)?;
        if variant.arity == 0 {
            source_mut(self.source)?.end(None)?;
            return Ok(VmValue::Tag(variant_id));
        }
        check_depth(depth)?;
        let part_base = (first_variant as usize..variant_id as usize).map(|vid| self.prog.variants[vid].arity).sum();
        self.visit_slots(
            dict,
            shape,
            SlotTarget::Variant { id: variant_id, part_base, enum_name: def },
            variant.field_names.as_deref(),
            variant.arity,
            depth + 1,
        )
    }

    fn call_with_dicts(
        &mut self,
        dict_root: usize,
        fn_const: u32,
        dicts: &[PartTemplate],
        extra_roots: std::ops::Range<usize>,
    ) -> Result<VmValue, Fail> {
        let dict_base = self.context.push_roots(dicts.len(), VmValue::Unit)?;
        for (index, template) in dicts.iter().enumerate() {
            let dict = eval_template(self.context, self.context.root_at(dict_root), template)?;
            self.context.set_root(dict_base + index, dict);
        }
        let mut args = Vec::with_capacity(dicts.len() + extra_roots.len());
        args.extend_from_slice(self.context.roots_slice(dict_base..dict_base + dicts.len()));
        args.extend_from_slice(self.context.roots_slice(extra_roots));
        let value = self.context.call(self.context.vm().consts[fn_const as usize], &args)?;
        self.context.truncate_roots(dict_base);
        Ok(value)
    }

    #[inline(always)]
    fn read_slot(&mut self, dict_root: usize, base: usize, part: &PartTemplate, slot: usize, depth: usize) -> CodecResult {
        let part_dict = eval_template(self.context, self.context.root_at(dict_root), part)?;
        let value = self.visit(part_dict, depth)?;
        self.context.set_root(base + slot, value);
        Ok(())
    }

    #[inline(never)]
    fn visit_slots(
        &mut self,
        dict: VmValue,
        shape: &Shape,
        target: SlotTarget,
        names: Option<&[String]>,
        count: usize,
        next_depth: usize,
    ) -> CodecResult<VmValue> {
        let dict_root = self.context.root(dict).0 as usize;
        let base = self.context.push_roots(count, VmValue::Uninit)?;
        let part_base = match target {
            SlotTarget::Variant { part_base, .. } => part_base,
            _ => 0,
        };
        if let Some(names) = names {
            let mut next_guess = 0usize;
            while let Some(name) = source_mut(self.source)?.next_field()? {
                let found = if names.get(next_guess).is_some_and(|field| field == name) {
                    Some(next_guess)
                } else if let Some(index) = shape.slot_index.as_ref().filter(|_| !matches!(target, SlotTarget::Variant { .. })) {
                    index.get(name).map(|&slot| slot as usize)
                } else {
                    if names.len() > MAX_SCANNED_SLOTS {
                        self.context.vm().charge((names.len() / 16) as u64)?;
                    }
                    names.iter().position(|field| field == name)
                };
                match found {
                    Some(slot) if !matches!(self.context.root_at(base + slot), VmValue::Uninit) => {
                        let shown = crate::stdlib::truncate(name, 64).to_string();
                        self.context.truncate_roots(dict_root);
                        return parse_error(source_mut(self.source)?, format!("duplicate field `{shown}`"));
                    }
                    Some(slot) => {
                        next_guess = slot + 1;
                        self.read_slot(dict_root, base, &shape.parts[part_base + slot], slot, next_depth)?;
                    }
                    None => {
                        let consumed = source_mut(self.source)?.take_consumed_bytes();
                        self.context.vm().charge(1 + (consumed / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
                        source_mut(self.source)?.skip()?;
                    }
                }
            }
        } else {
            for slot in 0..count {
                if !source_mut(self.source)?.next()? {
                    break;
                }
                self.read_slot(dict_root, base, &shape.parts[part_base + slot], slot, next_depth)?;
            }
        }
        for slot in 0..count {
            if !matches!(self.context.root_at(base + slot), VmValue::Uninit) {
                continue;
            }
            if matches!(target, SlotTarget::Tuple) {
                self.context.truncate_roots(dict_root);
                return parse_error(source_mut(self.source)?, format!("expected array of length {count}, found {slot}"));
            }
            let fill_val = match shape.fills.get(slot) {
                None | Some(PartFill::Required) => {
                    let part_dict = eval_template(self.context, self.context.root_at(dict_root), &shape.parts[part_base + slot])?;
                    if dict_shape(self.context.vm(), part_dict)?
                        .is_some_and(|shape_idx| matches!(self.prog.shapes[shape_idx as usize].kind, ShapeKind::Option))
                    {
                        VmValue::Tag(self.prog.ids.none)
                    } else {
                        self.context.truncate_roots(dict_root);
                        let message = names
                            .map_or_else(|| format!("expected {count} payload values"), |names| format!("missing field `{}`", names[slot]));
                        return parse_error(source_mut(self.source)?, message);
                    }
                }
                Some(PartFill::Call(helper_const, dicts)) => {
                    let extra_roots = if matches!(target, SlotTarget::Init) { base..base + slot } else { base..base };
                    self.call_with_dicts(dict_root, *helper_const, dicts, extra_roots)?
                }
            };
            self.context.set_root(base + slot, fill_val);
        }
        let enum_name = match target {
            SlotTarget::Variant { enum_name, .. } => Some(self.prog.types[enum_name as usize].name.as_str()),
            _ => None,
        };
        source_mut(self.source)?.end(enum_name)?;
        let result = match target {
            SlotTarget::Init => {
                let init = shape.init.as_ref().ok_or_else(|| Fail::internal("decode: missing init descriptor"))?;
                let value = self.call_with_dicts(dict_root, init.func, &init.dicts, base..base + count)?;
                match self.context.vm().heap.variant_id(value) {
                    id if id == self.prog.ids.err => {
                        let payload = self.context.vm().heap.variant_payload_if(&value, self.prog.ids.err);
                        let error = host_error(self.context.vm(), payload)?;
                        let path = source_mut(self.source)?.path();
                        let prefixed = match error.sand_error() {
                            Some((kind, message)) => HostError::new(kind, format!("{path}: {message}")),
                            None => error,
                        };
                        Err(prefixed)
                    }
                    id if id == self.prog.ids.ok => Ok(self.context.vm().heap.variant_payload_if(&value, self.prog.ids.ok)),
                    _ => Ok(value),
                }
            }
            target => {
                self.context.step(|vm| vm.check_memory(NODE_BYTES + count * size_of::<VmValue>()))?;
                let (vm, roots) = self.context.vm_and_roots();
                let slots = &roots[base..base + count];
                Ok(match target {
                    SlotTarget::Tuple => vm.heap.new_tuple(slots),
                    SlotTarget::Struct(def) => vm.heap.new_struct(def, slots),
                    SlotTarget::Variant { id, .. } => vm.heap.new_variant(id, slots),
                    SlotTarget::Init => unreachable!(),
                })
            }
        };
        self.context.truncate_roots(dict_root);
        result
    }
}

pub(crate) fn decode<R>(
    context: &mut CallContext,
    source: JsonSource,
    dict: VmValue,
    finish: impl FnOnce(&mut CallContext, VmValue, JsonSource) -> CodecResult<R>,
) -> CodecResult<R> {
    let mut slot = Some(source);
    let value = decode_inner(context, &mut slot, 0, dict)?;
    let source = slot.take().ok_or_else(|| Fail::internal("decode: missing source"))?;
    finish(context, value, source)
}

fn decode_inner(context: &mut CallContext, source: &mut Option<JsonSource>, depth: usize, dict: VmValue) -> CodecResult<VmValue> {
    let mark = context.root_mark();
    let dict_root = context.root(dict);
    let scratch = context.root(VmValue::Unit).0 as usize;
    let prog = context.vm().prog.clone();
    let dict = context.value(dict_root);
    let result = DecodeWalk { context, prog: &prog, source, scratch, unchecked: CHECKED_VALUES }.visit(dict, depth);
    if let Some(source) = source.as_mut() {
        context.vm().charge((source.take_consumed_bytes() / SLOW_BYTES_PER_FUEL_UNIT) as u64)?;
    }
    context.truncate_roots(mark);
    result
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
    let (len, cap, elem_size, index_growth) = match vm.heap.get(table) {
        Obj::Set(set) => (set.entries.len(), set.entries.capacity(), SET_ENTRY_SIZE, set.index_growth(key)),
        Obj::Map(map) => (map.entries.len(), map.entries.capacity(), MAP_ENTRY_SIZE, map.index_growth(key)),
        _ => return Ok(()),
    };
    if len < cap {
        if index_growth > 0 {
            vm.check_memory(index_growth)?;
        }
    } else {
        vm.heap.checked_grow_capacity(cap, len + 1, elem_size, index_growth).map_err(Fail::limit)?;
    }
    Ok(())
}

pub(crate) fn to_value(context: &mut CallContext, value: VmValue, dict: VmValue) -> CodecResult<Value> {
    encode(context, ValueSink::new(&context.vm().heap.io_memory), value, dict, |_, mut sink| {
        sink.take().map(|(value, _)| value).ok_or_else(|| Fail::internal("to_value: no value").into())
    })
}

pub(crate) fn register(module: &mut crate::native::NativeModule) {
    module.bind::<Encoder>().bind::<Decoder>().bind::<InputKind>().bind::<EncodedArg>();
    let encode_value = |context: &mut CallContext, dict: VmValue, value: VmValue, encoder: VmVal<Encoder>| -> CodecResult {
        let turn = encoder.get(&context.vm().heap)?.turn;
        with_session(context, turn, |context, sink: Box<dyn EncodeSink>, depth| {
            let mut slot = Some(sink);
            let result = encode_inner(context, &mut slot, false, depth, value, dict);
            (slot, result)
        })
    };
    let decode_value = |context: &mut CallContext, dict: VmValue, decoder: VmVal<Decoder>| -> CodecResult<Param<0>> {
        let turn = decoder.get(&context.vm().heap)?.turn;
        with_session(context, turn, |context, source: JsonSource, depth| {
            let mut slot = Some(source);
            let result = decode_inner(context, &mut slot, depth, dict);
            (slot, result.map(Param::from_raw))
        })
    };
    module.function(
        "encode_arg",
        |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>| -> Result<VmVal<EncodedArg>, Fail> {
            encode(context, ValueSink::new(&context.vm().heap.io_memory), value.raw(), dict.raw(), |context, mut sink| {
                let (value, _reservation) = sink.take().ok_or_else(|| Fail::internal("encode_arg: no value"))?;
                context.step(|vm| vm.check_memory(NODE_BYTES))?;
                let encoded = EncodedArg { value: Arc::new(value), _reservation }.into_vm_value(context.vm_mut())?;
                Ok(VmVal::<EncodedArg>::from_raw(encoded))
            })
            .map_err(walk_error_to_fail)
        },
    );
    module.function(
        "Encode.encode",
        move |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>, encoder: VmVal<Encoder>| {
            encode_value(context, dict.raw(), value.raw(), encoder)
        },
    );
    module.function(
        "Encoder.encode",
        move |context: &mut CallContext, dict: TraitDict<0, traits::Encode>, encoder: VmVal<Encoder>, value: Param<0>| {
            encode_value(context, dict.raw(), value.raw(), encoder)
        },
    );
    module.function("Decode.decode", move |context: &mut CallContext, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        decode_value(context, dict.raw(), decoder)
    });
    module.function("Decoder.decode", move |context: &mut CallContext, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        decode_value(context, dict.raw(), decoder)
    });
    module.function("Decoder.kind", |context: &mut CallContext, decoder: VmVal<Decoder>| -> CodecResult<InputKind> {
        let turn = decoder.get(&context.vm().heap)?.turn;
        let top = top_turn(&mut context.vm_mut().native_state::<CodecSessions>().decode, turn, DECODER_MISUSE[0])?;
        if top.used {
            return Err(top.record_misuse("a `decode` method asked for the kind of a value that it read"));
        }
        let Some(source) = top.io.as_mut() else { return Err(top.record_misuse(DECODER_MISUSE[0])) };
        let result = source.kind();
        if let Err(error) = &result {
            top.record_error(error);
        }
        result
    });
}
