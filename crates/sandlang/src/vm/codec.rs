//! The encode and decode walkers of the `Encode` and `Decode` traits.
//!
//! A walker visits a value one part at a time. It builds the dictionary of
//! each part from the dictionary of the whole through the [`CodecShape`] of a
//! structural dictionary. It handles a structural part in Rust. At a part
//! with a custom method, an `init` constructor, or a computed field default,
//! it stops and returns a [`Callback`] for its caller to run.
//!
//! A walker keeps every heap value that it holds in [`Walk::values`], so that
//! its caller can root them during a callback. A pass of a walker checks the
//! memory limit before each part. If the check fails, the pass stops with
//! [`Step::Collect`], and the walk continues after a collection. A failure
//! inside a part ends the walk, because the part can have changed the sink or
//! the source. Only the first pass of a native call, which the VM can run
//! again from the start, returns that failure as a retryable error.

use super::bytecode::{CODEC_SHAPE_SLOT, CodecShape, Fill, PartTemplate, Program, ShapeKind};
use super::meter::{SCRATCH_STEP, SLOW_BYTES_PER_UNIT};
use super::value::{FxHashMap, MapObj, Obj, SetObj, VariantRef, VmValue};
use super::{Fail, Vm, WaitKind};
use crate::engine::LimitKind;
use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::context::NativeContext;
use crate::native::convert::{HostError, IntoVmValue};
use crate::native::func::{Later, NativeError, NativeReturn};
use crate::native::ty::{EncodedArg, Param, TraitDict, VmVal};
use crate::types::{ConstVal, traits};
use crate::value::StructDesc;
use crate::value::ValueSink;
use parking_lot::Mutex;
use std::sync::Arc;

/// Bytes that a walker reserves for each value that it builds.
const NODE_BYTES: usize = 64;

/// The number of values that the decode walker reads between two checks of
/// the memory limit.
const CHECKED_VALUES: u32 = 32;

/// The maximum nesting depth of a value that a walker encodes or decodes.
/// The limit also stops a cyclic value.
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// A call that a walker needs before it can continue.
pub(crate) struct Callback {
    pub callee: VmValue,
    pub args: Vec<VmValue>,
    pub purpose: Purpose,
}

/// The use of the result of a [`Callback`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// A custom `encode` or `decode` method, which returns a `Result`.
    Method,
    /// A computed field or parameter default.
    Fill,
    /// An `init` constructor.
    Init,
}

/// The progress of a walk.
pub(crate) enum Step {
    Done,
    Call(Callback),
    /// A memory check failed before the next part. The walk can continue
    /// after a collection.
    Collect(Fail),
}

/// The heap values of a walker.
#[derive(Default)]
pub(crate) struct Walk {
    /// Every heap value that the walk holds, except `cached`.
    values: Vec<VmValue>,
    /// Whether the walk collected garbage after its last memory check.
    collected: bool,
    /// Part dictionaries built from templates, by shape, part, and the heap
    /// handle of the dictionary of the whole.
    cache: FxHashMap<(u32, u32, u32), VmValue>,
    cached: Vec<VmValue>,
}

impl Walk {
    fn new(values: Vec<VmValue>) -> Walk {
        Walk { values, ..Walk::default() }
    }

    /// Returns the step after the memory check before the next part, if the
    /// check failed.
    fn checked(&mut self, check: Result<(), Fail>) -> Option<CodecResult<Step>> {
        match check {
            Ok(()) => {
                self.collected = false;
                None
            }
            Err(e) if !self.collected && e.as_limit() == Some(LimitKind::Memory) => Some(Ok(Step::Collect(e))),
            Err(e) => Some(Err(e.into())),
        }
    }

    /// Returns the dictionary of part `part` of shape `s`, from `whole`.
    fn part_dict(&mut self, vm: &mut Vm, prog: &Program, s: u32, part: u32, whole: VmValue) -> Result<VmValue, Fail> {
        let Some(t) = prog.codec_shapes[s as usize].parts.get(part as usize) else {
            return Err(Fail::internal("a codec shape has too few parts"));
        };
        match t {
            PartTemplate::Const(k) => Ok(vm.consts[*k as usize]),
            PartTemplate::Bound(i) => dict_slot(vm, whole, *i),
            _ => {
                let key = (s, part, handle(whole));
                if let Some(&d) = self.cache.get(&key) {
                    return Ok(d);
                }
                let d = eval_template(vm, t, whole)?;
                self.cache.insert(key, d);
                self.cached.push(d);
                Ok(d)
            }
        }
    }
}

impl Walk {
    /// Returns the lists of every heap value that the walk holds.
    fn roots(&mut self) -> [&mut Vec<VmValue>; 2] {
        [&mut self.values, &mut self.cached]
    }

    /// Collects garbage and keeps the values of the walk rooted.
    async fn collect(&mut self, cx: &mut NativeContext) {
        cx.collect_keeping(self.roots()).await;
        self.collected = true;
    }
}

fn handle(v: VmValue) -> u32 {
    match v {
        VmValue::Obj(h) => h,
        _ => u32::MAX,
    }
}

fn dict_slot(vm: &Vm, dict: VmValue, i: u32) -> Result<VmValue, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => items.get(i as usize).copied().ok_or_else(|| Fail::internal("a codec dictionary has too few slots")),
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

/// Builds a dictionary from template `t`. The memory check at the start of
/// each pass of a walker covers its allocations.
fn eval_template(vm: &mut Vm, t: &PartTemplate, whole: VmValue) -> Result<VmValue, Fail> {
    Ok(match t {
        PartTemplate::Const(k) => vm.consts[*k as usize],
        PartTemplate::Bound(i) => dict_slot(vm, whole, *i)?,
        PartTemplate::Field(inner, i) => {
            let d = eval_template(vm, inner, whole)?;
            dict_slot(vm, d, *i)?
        }
        PartTemplate::Dict(items) => {
            vm.charge(1)?;
            let items = items.iter().map(|t| eval_template(vm, t, whole)).collect::<Result<Vec<_>, _>>()?;
            vm.heap.alloc(Obj::Tuple(items.into_boxed_slice()))
        }
    })
}

/// The method of a codec dictionary.
enum Method {
    /// The native default method, with the index of the shape.
    Structural(u32),
    /// A method that a Sand impl declares.
    Custom(VmValue),
}

fn method(vm: &Vm, dict: VmValue) -> Result<Method, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => match (items.first(), items.get(CODEC_SHAPE_SLOT)) {
            (_, Some(&VmValue::Int(s))) => Ok(Method::Structural(s as u32)),
            (Some(&m), _) => Ok(Method::Custom(m)),
            (None, _) => Err(Fail::internal("a codec dictionary is empty")),
        },
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

fn shape(vm: &Vm, s: u32) -> Result<&CodecShape, Fail> {
    vm.prog.codec_shapes.get(s as usize).ok_or_else(|| Fail::internal("unknown codec shape"))
}

/// Fails a walk with kind `"limit"` for a value nested more than
/// [`MAX_NESTING_DEPTH`] levels.
pub(crate) fn too_deep<T>() -> CodecResult<T> {
    Err(HostError::new("limit", "value too deeply nested to encode or decode"))
}

/// Adds the path of the current part of `source` to `e`, the error of a
/// callback.
fn at_path(e: HostError<Fail>, source: &dyn DecodeSource) -> HostError<Fail> {
    match e.sand_error() {
        Some((kind, message)) => HostError::new(kind, format!("{}: {message}", source.path())),
        None => e,
    }
}

fn parse_error<T>(source: &dyn DecodeSource, msg: impl std::fmt::Display) -> CodecResult<T> {
    Err(HostError::new("parse", format!("{}: {msg}", source.path())))
}

/// Returns the payload of `v` if it is an `Err`.
fn err_payload(vm: &Vm, v: VmValue) -> Option<VmValue> {
    (vm.heap.variant_id(v) == vm.prog.ids.err).then(|| vm.heap.payload_if(&v, vm.prog.ids.err))
}

/// Converts the error `e` of a Sand callback into a [`HostError`]. An `Error`
/// keeps its kind. Another error has kind `"invalid"` and its display text.
fn sand_error(vm: &Vm, e: VmValue) -> Result<HostError<Fail>, Fail> {
    let error_def = vm.prog.struct_types.get(&<crate::stdlib::core::Error as crate::native::NamedType>::rust_type()).copied();
    if let Obj::Struct(def, fields) = vm.heap.get(e)
        && Some(*def) == error_def
        && let [message, kind] = fields[..]
    {
        return Ok(HostError::new(vm.heap.str(kind), vm.heap.str(message)));
    }
    Ok(HostError::new("invalid", vm.display(e)?))
}

/// Converts the [`HostError`] of a walk into the [`Fail`] that stops the run.
pub(crate) fn abort(e: HostError<Fail>) -> Fail {
    match e.into_abort() {
        Ok(fail) => fail,
        Err(e) => {
            let (kind, message) = e.sand_error().unwrap_or(("error", ""));
            Fail::runtime_error(kind, message)
        }
    }
}

/// Tracks which `Encoder` or `Decoder` of a walk can take its value.
pub(crate) struct Turn {
    /// The token of the handle that can take the value, or 0.
    active: u64,
    /// Whether the active handle has taken its value.
    used: bool,
    next_token: u64,
    /// The error of a failed nested write or read, which fails the walk.
    failed: Option<(String, String)>,
}

/// The turn that the callback of a custom method interrupts.
#[derive(Clone, Copy)]
struct SavedTurn {
    active: u64,
    used: bool,
}

impl Turn {
    /// Starts the turn of the handle with token `next_token`, and returns
    /// the turn that it interrupts.
    fn begin(&mut self) -> (u64, SavedTurn) {
        let token = self.next_token;
        self.next_token += 1;
        let saved = SavedTurn { active: self.active, used: self.used };
        (self.active, self.used) = (token, false);
        (token, saved)
    }

    /// Gives the value of the walk to the handle with `token`. Fails with
    /// `inactive` if the turn is not the handle's, or with `second` if the
    /// handle has already taken its value.
    fn claim(&mut self, token: u64, inactive: &str, second: &str) -> Result<(), HostError> {
        if self.active != token {
            return Err(self.misuse(inactive));
        }
        if self.used {
            return Err(self.misuse(second));
        }
        self.used = true;
        Ok(())
    }

    /// Ends the turn of `token` and resumes `saved`. Returns `true` if the
    /// handle took no value. Fails with the error of a failed nested write
    /// or read.
    fn end(&mut self, token: u64, saved: SavedTurn) -> CodecResult<bool> {
        if let Some((kind, message)) = self.failed.take() {
            return Err(HostError::new(kind, message));
        }
        let unused = self.active == token && !self.used;
        (self.active, self.used) = (saved.active, saved.used);
        Ok(unused)
    }

    /// Records the error of a nested write or read, so that the walk fails
    /// even if the custom method ignores the error.
    fn record(&mut self, e: &HostError<Fail>) {
        if let Some((kind, message)) = e.sand_error() {
            self.failed = Some((kind.to_string(), message.to_string()));
        }
    }

    /// Returns an `"invalid"` error for a misused handle, and records it.
    fn misuse(&mut self, message: &str) -> HostError {
        self.failed = Some(("invalid".to_string(), message.to_string()));
        HostError::new("invalid", message)
    }
}

/// The state that a walk shares with the `Encoder`s or `Decoder`s of its
/// callbacks: the turn and the sink or source.
pub(crate) struct Shared<IO: ?Sized> {
    turn: Turn,
    pub io: IO,
}

impl<IO> Shared<IO> {
    pub(crate) fn new(io: IO) -> Arc<Mutex<Shared<IO>>> {
        let turn = Turn { active: 0, used: false, next_token: 1, failed: None };
        Arc::new(Mutex::new(Shared { turn, io }))
    }
}

/// A sink of the encode walker.
pub(crate) trait WalkSink: EncodeSink {
    /// Makes writes fail with the memory limit once the sink would allocate
    /// more than `bytes` in total.
    fn set_max_allocated(&mut self, bytes: usize);
}

/// A shared encode state with any sink.
pub(crate) type EncodeCell = Arc<Mutex<Shared<dyn WalkSink>>>;

/// A shared decode state with any source.
pub(crate) type DecodeCell = Arc<Mutex<Shared<dyn DecodeSource>>>;

/// Writes the single value of a custom `encode` method.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub(crate) struct Encoder {
    shared: EncodeCell,
    token: u64,
}

impl crate::native::NativeObject for Encoder {}

/// Reads the single value of a custom `decode` method.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub(crate) struct Decoder {
    shared: DecodeCell,
    token: u64,
}

impl crate::native::NativeObject for Decoder {}

/// An open container of the encode walker.
struct EncodeFrame {
    /// The index of the container in [`Walk::values`]. Its dictionary
    /// follows it, and for a map, the value of the current entry.
    at: usize,
    shape: u32,
    kind: EncodeKind,
    /// The position of the next child.
    next: usize,
    /// For a list, a set, or a map: the length of its slots when the walker
    /// began it. The walker reads no slot past it, as a `for` loop does, so a
    /// callback that adds parts does not change what the walker writes.
    bound: usize,
}

/// An open container of the encode walker.
#[derive(Clone, Copy)]
enum EncodeKind {
    /// The elements of a list or a set, which share one part.
    Seq,
    /// The elements of a tuple or the fields of a struct, one part each.
    Fields,
    /// A map: its even children are keys and its odd children are values.
    /// The walker reads each value with its key, so a callback of the key
    /// that removes the entry does not leave the key without a value.
    Map,
    /// The payload of a variant whose parts start at `base`.
    Payload { base: u32 },
    /// The payload of `Some`.
    Some,
}

/// Walks a value and writes it to an [`EncodeSink`].
pub(crate) struct EncodeWalk {
    walk: Walk,
    /// The size of the sink at which the walk checks the memory limit next.
    next_check: usize,
    frames: Vec<EncodeFrame>,
    /// The number of open containers other than `Some`.
    depth: usize,
    /// The part to visit next: its value and its dictionary.
    pending: Option<(VmValue, VmValue)>,
    /// The token of the `Encoder` of the callback in progress, and the
    /// encoder state before the callback.
    callback: Option<(u64, SavedTurn)>,
}

impl EncodeWalk {
    pub(crate) fn new(value: VmValue, dict: VmValue) -> EncodeWalk {
        let walk = Walk::new(vec![value, dict]);
        EncodeWalk { walk, next_check: 0, frames: Vec::new(), depth: 0, pending: Some((value, dict)), callback: None }
    }

    /// Walks until the end or the next callback.
    pub(crate) fn run(&mut self, vm: &mut Vm, shared: &EncodeCell) -> CodecResult<Step> {
        let prog = vm.prog.clone();
        let mut state = shared.lock();
        // Callbacks can have changed the heap since the last pass.
        self.next_check = 0;
        loop {
            let allocated = state.io.allocated();
            let check = if allocated >= self.next_check || vm.heap.fail_next_check.get() {
                let held = self.walk.values.capacity() * size_of::<VmValue>();
                vm.check_memory(NODE_BYTES + held + allocated).inspect(|()| {
                    self.next_check = allocated + SCRATCH_STEP;
                    state.io.set_max_allocated(allocated + vm.heap.room());
                })
            } else {
                Ok(())
            };
            if let Some(step) = self.walk.checked(check) {
                return step;
            }
            let (value, dict) = match self.pending {
                Some(part) => part,
                None => match self.next_part(vm, &prog, &mut state.io)? {
                    Some(part) => part,
                    None => return Ok(Step::Done),
                },
            };
            vm.charge(1)?;
            match method(vm, dict)? {
                Method::Custom(callee) => {
                    let token = state.turn.next_token;
                    let encoder = match (Encoder { shared: shared.clone(), token }).into_vm_value(vm) {
                        Ok(e) => e,
                        Err(e) => return self.walk.checked(Err(e)).expect("an error"),
                    };
                    self.callback = Some(state.turn.begin());
                    self.pending = None;
                    return Ok(Step::Call(Callback { callee, args: vec![dict, value, encoder], purpose: Purpose::Method }));
                }
                Method::Structural(s) => {
                    self.pending = None;
                    self.visit(vm, &prog, &mut state.io, s, value, dict)?;
                }
            }
        }
    }

    /// Checks the result of the callback of a custom method.
    fn apply(&mut self, vm: &mut Vm, shared: &EncodeCell, result: VmValue) -> CodecResult {
        let Some((token, saved)) = self.callback.take() else { return Err(Fail::internal("encode: no callback").into()) };
        let unused = shared.lock().turn.end(token, saved)?;
        if let Some(e) = err_payload(vm, result) {
            return Err(sand_error(vm, e)?);
        }
        if unused {
            return Err(HostError::new("invalid", "an `encode` method wrote no value to its `Encoder`"));
        }
        Ok(())
    }

    /// Finishes a walk that waits for callbacks.
    async fn finish(&mut self, cx: &mut NativeContext, shared: &EncodeCell, first: Step) -> CodecResult {
        let mut step = first;
        loop {
            match step {
                Step::Done => return Ok(()),
                Step::Call(callback) => {
                    let result = cx.call_keeping(callback.callee, &callback.args, self.walk.roots()).await;
                    cx.with_vm(|vm| self.apply(vm, shared, result))?;
                }
                Step::Collect(_) => self.walk.collect(cx).await,
            }
            step = cx.with_vm(|vm| self.run(vm, shared))?;
        }
    }

    #[inline(always)]
    fn push_frame(&mut self, value: VmValue, dict: VmValue, shape: u32, kind: EncodeKind, bound: usize) -> CodecResult {
        if !matches!(kind, EncodeKind::Some) {
            if self.depth >= MAX_NESTING_DEPTH {
                return too_deep();
            }
            self.depth += 1;
        }
        let at = self.walk.values.len();
        self.walk.values.extend([value, dict]);
        if matches!(kind, EncodeKind::Map) {
            self.walk.values.push(VmValue::Unit);
        }
        self.frames.push(EncodeFrame { at, shape, kind, next: 0, bound });
        Ok(())
    }

    fn visit(&mut self, vm: &mut Vm, prog: &Program, sink: &mut dyn EncodeSink, s: u32, value: VmValue, dict: VmValue) -> CodecResult {
        let shape = &prog.codec_shapes[s as usize];
        let wrong = || -> CodecResult { Err(Fail::internal("encode: a value does not match its shape").into()) };
        match (&shape.kind, value) {
            (ShapeKind::Unit, _) => sink.unit()?,
            (ShapeKind::Bool, VmValue::Bool(b)) => sink.bool(b)?,
            (ShapeKind::Int, VmValue::Int(i)) => sink.int(i)?,
            (ShapeKind::Float, VmValue::Float(f)) => sink.float(f)?,
            (ShapeKind::Duration, VmValue::Dur(d)) => sink.duration(d)?,
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
                _ => wrong()?,
            },
            (ShapeKind::Native(def, _), _) => {
                let Some(text) = vm.heap.get(value).value().map(|v| v.text()) else { return wrong() };
                vm.meter.charge_bytes(text.len())?;
                sink.native(&prog.types[*def as usize].desc().type_name, &text)?;
            }
            (ShapeKind::ManualEncode(f), _) => f(vm, value, sink)?,
            (ShapeKind::List, _) => {
                let len = vm.heap.list(value).len();
                self.push_frame(value, dict, s, EncodeKind::Seq, len)?;
                sink.begin_list(len)?;
            }
            (ShapeKind::Set, _) => {
                let Obj::Set(set) = vm.heap.get(value) else { return wrong() };
                let (live, bound) = (set.live, set.entries.len());
                self.push_frame(value, dict, s, EncodeKind::Seq, bound)?;
                sink.begin_set(live)?;
            }
            (ShapeKind::Map, _) => {
                let Obj::Map(map) = vm.heap.get(value) else { return wrong() };
                let (live, bound) = (map.live, map.entries.len());
                self.push_frame(value, dict, s, EncodeKind::Map, bound)?;
                sink.begin_map(live)?;
            }
            (ShapeKind::Tuple, _) => {
                self.push_frame(value, dict, s, EncodeKind::Fields, 0)?;
                sink.begin_tuple(shape.parts.len())?;
            }
            (ShapeKind::Struct(def) | ShapeKind::Init(def), _) => {
                self.push_frame(value, dict, s, EncodeKind::Fields, 0)?;
                sink.begin_struct(prog.types[*def as usize].desc())?;
            }
            (ShapeKind::Option, _) if vm.heap.variant_id(value) == prog.ids.none => sink.none()?,
            (ShapeKind::Option, _) => {
                sink.begin_some()?;
                self.push_frame(value, dict, s, EncodeKind::Some, 0)?;
            }
            (ShapeKind::Enum(_), _) => {
                let id = vm.heap.variant_id(value);
                let Some(desc) = prog.variant_desc(id) else { return wrong() };
                let variant = &prog.variants[id as usize];
                sink.begin_variant(desc, variant.arity)?;
                if variant.arity == 0 {
                    return sink.end();
                }
                let base = (id - variant.index..id).map(|v| prog.variants[v as usize].arity as u32).sum();
                self.push_frame(value, dict, s, EncodeKind::Payload { base }, 0)?;
            }
            _ => wrong()?,
        }
        Ok(())
    }

    /// Returns the next part of the innermost open container, and ends the
    /// containers that have no more parts.
    fn next_part(&mut self, vm: &mut Vm, prog: &Program, sink: &mut dyn EncodeSink) -> CodecResult<Option<(VmValue, VmValue)>> {
        loop {
            let Some(frame) = self.frames.last() else { return Ok(None) };
            let (container, dict, shape) = (self.walk.values[frame.at], self.walk.values[frame.at + 1], frame.shape);
            let child = match frame.kind {
                EncodeKind::Seq => seq_child(vm, container, frame.next, frame.bound)?,
                EncodeKind::Fields => field(vm, container, frame.next)?.map(|v| (v, frame.next as u32, frame.next + 1)),
                EncodeKind::Map if frame.next % 2 == 1 => Some((self.walk.values[frame.at + 2], 1, frame.next + 1)),
                EncodeKind::Map => match map_entry(vm, container, frame.next / 2, frame.bound) {
                    Some((key, value, entry)) => {
                        self.walk.values[frame.at + 2] = value;
                        Some((key, 0, 2 * entry + 1))
                    }
                    None => None,
                },
                EncodeKind::Some => (frame.next == 0).then(|| (vm.heap.payload_if(&container, vm.prog.ids.some), 0, 1)),
                EncodeKind::Payload { base } => payload(vm, container, frame.next).map(|v| (v, base + frame.next as u32, frame.next + 1)),
            };
            match child {
                Some((value, part, next)) => {
                    let dict = self.walk.part_dict(vm, prog, shape, part, dict)?;
                    self.frames.last_mut().expect("an open frame").next = next;
                    return Ok(Some((value, dict)));
                }
                None => {
                    sink.end()?;
                    let frame = self.frames.pop().expect("an open frame");
                    self.depth -= usize::from(!matches!(frame.kind, EncodeKind::Some));
                    self.walk.values.truncate(frame.at);
                }
            }
        }
    }
}

/// Returns the element of a list or a set at or after slot `next` and before
/// slot `bound`, its part index, and the slot after it.
fn seq_child(vm: &Vm, container: VmValue, next: usize, bound: usize) -> Result<Option<(VmValue, u32, usize)>, Fail> {
    let slots: &[Option<VmValue>] = match vm.heap.get(container) {
        Obj::List(items) => return Ok(items[..bound.min(items.len())].get(next).map(|&v| (v, 0, next + 1))),
        Obj::Set(set) => &set.entries,
        _ => return Err(Fail::internal("encode: not a list or a set")),
    };
    let slots = &slots[next.min(bound).min(slots.len())..bound.min(slots.len())];
    Ok(slots.iter().enumerate().find_map(|(i, e)| e.map(|v| (v, 0, next + i + 1))))
}

/// Returns the key and the value of the entry of a map at or after `entry`
/// and before `bound`, and the index of the entry.
fn map_entry(vm: &Vm, map: VmValue, entry: usize, bound: usize) -> Option<(VmValue, VmValue, usize)> {
    let Obj::Map(m) = vm.heap.get(map) else { return None };
    let entries = &m.entries[entry.min(bound).min(m.entries.len())..bound.min(m.entries.len())];
    entries.iter().enumerate().find_map(|(i, e)| e.map(|(k, v)| (k, v, entry + i)))
}

/// Returns field `i` of a tuple or a struct, whose fields a callback cannot
/// add or remove.
fn field(vm: &Vm, container: VmValue, i: usize) -> Result<Option<VmValue>, Fail> {
    Ok(match container {
        VmValue::Pair(a, b) => [VmValue::Int(i64::from(a)), VmValue::Int(b)].get(i).copied(),
        _ => match vm.heap.get(container) {
            Obj::Tuple(items) | Obj::Struct(_, items) => items.get(i).copied(),
            _ => return Err(Fail::internal("encode: not a tuple or a struct")),
        },
    })
}

fn payload(vm: &Vm, v: VmValue, i: usize) -> Option<VmValue> {
    match vm.heap.variant(v)? {
        VariantRef::Heap(_, fields) => fields.get(i).copied(),
        VariantRef::Inline(_, p) => p.filter(|_| i == 0),
    }
}

/// An open container of the decode walker. Its decoded children follow its
/// dictionary in [`Walk::values`].
struct DecodeFrame {
    /// The index of the dictionary of the container in [`Walk::values`].
    at: usize,
    shape: u32,
    kind: DecodeKind,
    /// Whether the input of the container has ended.
    ended: bool,
}

enum DecodeKind {
    /// A list or a set.
    Seq,
    Tuple,
    /// A map, whose children alternate between keys and values.
    Map,
    Some,
    /// The fields of a struct, the parameters of `init`, or the payload of
    /// a variant, in slots. `seen` marks the slots that hold values. `next`
    /// is the slot of the part that the walk reads.
    Slots {
        names: Names,
        form: PayloadForm,
        seen: Vec<bool>,
        next: usize,
        end: SlotsEnd,
    },
}

/// The names of the slots of a [`DecodeKind::Slots`].
enum Names {
    Fields(Arc<StructDesc>),
    Params(Arc<[String]>),
    /// The named payload of variant `id`.
    Payload(u32),
    Positional,
}

impl Names {
    fn get<'a>(&'a self, prog: &'a Program) -> Option<&'a [String]> {
        match self {
            Names::Fields(desc) => Some(&desc.fields),
            Names::Params(names) => Some(names),
            Names::Payload(id) => prog.variants[*id as usize].field_names.as_deref(),
            Names::Positional => None,
        }
    }
}

/// What the decode walker builds from the slots of a [`DecodeKind::Slots`].
#[derive(Clone, Copy)]
enum SlotsEnd {
    Struct(u32),
    Init,
    /// Variant `id`, whose parts start at `base`.
    Variant {
        id: u32,
        base: u32,
    },
}

/// Builds a value from a [`DecodeSource`].
pub(crate) struct DecodeWalk {
    walk: Walk,
    /// The values read since the last check of the memory limit.
    unchecked: u32,
    frames: Vec<DecodeFrame>,
    /// The number of open containers other than `Some`.
    depth: usize,
    /// The dictionary of the part to read next.
    pending: Option<VmValue>,
    /// The token of the `Decoder` of the callback in progress, and the
    /// decoder state before the callback.
    callback: Option<(u64, SavedTurn)>,
}

impl DecodeWalk {
    pub(crate) fn new(dict: VmValue) -> DecodeWalk {
        DecodeWalk { walk: Walk::new(vec![dict]), unchecked: 0, frames: Vec::new(), depth: 0, pending: Some(dict), callback: None }
    }

    /// Walks until the end or the next callback.
    pub(crate) fn run(&mut self, vm: &mut Vm, shared: &DecodeCell) -> CodecResult<Step> {
        let prog = vm.prog.clone();
        let mut state = shared.lock();
        loop {
            let check = if self.unchecked == 0 || vm.heap.fail_next_check.get() {
                let held = self.walk.values.capacity() * size_of::<VmValue>();
                vm.check_memory(CHECKED_VALUES as usize * NODE_BYTES + held + state.io.allocated())
            } else {
                Ok(())
            };
            if let Some(step) = self.walk.checked(check) {
                return step;
            }
            self.unchecked = (self.unchecked + 1) % CHECKED_VALUES;
            vm.charge(1 + (state.io.work() / SLOW_BYTES_PER_UNIT) as u64)?;
            if let Some(dict) = self.pending.take() {
                match method(vm, dict)? {
                    Method::Custom(callee) => {
                        let token = state.turn.next_token;
                        let decoder = match (Decoder { shared: shared.clone(), token }).into_vm_value(vm) {
                            Ok(d) => d,
                            Err(e) => {
                                self.pending = Some(dict);
                                return self.walk.checked(Err(e)).expect("an error");
                            }
                        };
                        self.callback = Some(state.turn.begin());
                        return Ok(Step::Call(Callback { callee, args: vec![dict, decoder], purpose: Purpose::Method }));
                    }
                    Method::Structural(s) => self.visit(vm, &prog, &mut state.io, s, dict)?,
                }
                continue;
            }
            if self.frames.is_empty() {
                return Ok(Step::Done);
            }
            if let Some(callback) = self.advance(vm, &prog, &mut state.io)? {
                return Ok(Step::Call(callback));
            }
        }
    }

    /// Returns the decoded value of a walk that is done.
    pub(crate) fn result(&self) -> Result<VmValue, Fail> {
        match self.walk.values[..] {
            [_, v] => Ok(v),
            _ => Err(Fail::internal("decode: no result")),
        }
    }

    /// Places the result of a callback.
    fn apply(&mut self, vm: &mut Vm, shared: &DecodeCell, purpose: Purpose, result: VmValue) -> CodecResult {
        let value = match purpose {
            Purpose::Method => {
                let Some((token, saved)) = self.callback.take() else { return Err(Fail::internal("decode: no callback").into()) };
                let mut state = shared.lock();
                let unread = state.turn.end(token, saved)?;
                if let Some(e) = err_payload(vm, result) {
                    return Err(at_path(sand_error(vm, e)?, &state.io));
                }
                if unread {
                    state.io.skip()?;
                }
                vm.heap.payload_if(&result, vm.prog.ids.ok)
            }
            Purpose::Fill => result,
            Purpose::Init => {
                if let Some(e) = err_payload(vm, result) {
                    return Err(at_path(sand_error(vm, e)?, &shared.lock().io));
                }
                match vm.heap.variant_id(result) {
                    id if id == vm.prog.ids.ok => vm.heap.payload_if(&result, id),
                    _ => result,
                }
            }
        };
        if purpose == Purpose::Init {
            self.pop_frame();
        }
        self.place(value);
        Ok(())
    }

    /// Finishes a walk that waits for callbacks.
    async fn finish(&mut self, cx: &mut NativeContext, shared: &DecodeCell, first: Step) -> CodecResult<VmValue> {
        let mut step = first;
        loop {
            match step {
                Step::Done => return Ok(self.result()?),
                Step::Call(callback) => {
                    let result = cx.call_keeping(callback.callee, &callback.args, self.walk.roots()).await;
                    cx.with_vm(|vm| self.apply(vm, shared, callback.purpose, result))?;
                }
                Step::Collect(_) => self.walk.collect(cx).await,
            }
            step = cx.with_vm(|vm| self.run(vm, shared))?;
        }
    }

    /// Places decoded value `v` in the slot of the innermost frame that it
    /// fills, or after the children of that frame.
    fn place(&mut self, v: VmValue) {
        match self.frames.last() {
            Some(DecodeFrame { at, kind: DecodeKind::Slots { next, .. }, .. }) => self.walk.values[at + 1 + next] = v,
            _ => self.walk.values.push(v),
        }
    }

    fn push_frame(&mut self, dict: VmValue, shape: u32, kind: DecodeKind, slots: usize) -> CodecResult {
        if !matches!(kind, DecodeKind::Some) {
            if self.depth >= MAX_NESTING_DEPTH {
                return too_deep();
            }
            self.depth += 1;
        }
        let at = self.walk.values.len();
        self.walk.values.push(dict);
        self.walk.values.extend(std::iter::repeat_n(VmValue::Uninit, slots));
        self.frames.push(DecodeFrame { at, shape, kind, ended: false });
        Ok(())
    }

    /// Reads the part of shape `s` with dictionary `dict`. Places a scalar,
    /// or opens a frame for a container.
    fn visit(&mut self, vm: &mut Vm, prog: &Program, source: &mut dyn DecodeSource, s: u32, dict: VmValue) -> CodecResult {
        let shape = &prog.codec_shapes[s as usize];
        let value = match &shape.kind {
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
                vm.heap.new_str(&*text)
            }
            ShapeKind::Bytes => {
                let bytes = source.bytes()?;
                vm.meter.charge_bytes(bytes.len())?;
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
            ShapeKind::ManualDecode(f) => f(vm, source)?,
            ShapeKind::List | ShapeKind::Set => {
                source.begin_list()?;
                return self.push_frame(dict, s, DecodeKind::Seq, 0);
            }
            ShapeKind::Tuple => {
                source.begin_tuple(shape.parts.len())?;
                return self.push_frame(dict, s, DecodeKind::Tuple, 0);
            }
            ShapeKind::Map => {
                source.begin_map()?;
                return self.push_frame(dict, s, DecodeKind::Map, 0);
            }
            ShapeKind::Option => match source.option()? {
                false => VmValue::Tag(prog.ids.none),
                true => {
                    self.push_frame(dict, s, DecodeKind::Some, 0)?;
                    self.pending = Some(self.walk.part_dict(vm, prog, s, 0, dict)?);
                    return Ok(());
                }
            },
            ShapeKind::Struct(def) | ShapeKind::Init(def) => {
                let desc = prog.types[*def as usize].desc();
                source.begin_struct(desc)?;
                let (names, end, n) = match (&shape.kind, &shape.init) {
                    (ShapeKind::Init(_), Some(init)) => (Names::Params(init.names.clone()), SlotsEnd::Init, init.names.len()),
                    _ => (Names::Fields(desc.clone()), SlotsEnd::Struct(*def), desc.fields.len()),
                };
                let kind = DecodeKind::Slots { names, form: PayloadForm::Named, seen: vec![false; n], next: 0, end };
                return self.push_frame(dict, s, kind, n);
            }
            ShapeKind::Enum(def) => {
                let t = &prog.types[*def as usize];
                let name = source.begin_variant(&t.desc().type_name)?;
                let first = prog.variants.iter().position(|v| v.def == *def).unwrap_or(0);
                let found = prog.variants[first..].iter().take_while(|v| v.def == *def).position(|v| v.name == *name);
                let Some(i) = found else {
                    let name = crate::stdlib::truncate(&name, 64).to_string();
                    return parse_error(source, format!("unknown variant `{name}` of {}", t.name));
                };
                let id = (first + i) as u32;
                let variant = &prog.variants[id as usize];
                let base = (first as u32..id).map(|v| prog.variants[v as usize].arity as u32).sum();
                let form = match (variant.arity, &variant.field_names) {
                    (0, _) => PayloadForm::Unit,
                    (_, Some(_)) => PayloadForm::Named,
                    (n, None) => PayloadForm::Positional(n),
                };
                source.begin_payload(form)?;
                if form == PayloadForm::Unit {
                    source.end()?;
                    self.place(VmValue::Tag(id));
                    return Ok(());
                }
                let names = if variant.field_names.is_some() { Names::Payload(id) } else { Names::Positional };
                let n = variant.arity;
                let kind = DecodeKind::Slots { names, form, seen: vec![false; n], next: 0, end: SlotsEnd::Variant { id, base } };
                return self.push_frame(dict, s, kind, n);
            }
            ShapeKind::ManualEncode(_) => return Err(Fail::internal("decode: an encode shape").into()),
        };
        self.place(value);
        Ok(())
    }

    /// Continues the innermost frame: requests its next part, fills a
    /// missing slot, or ends the frame. Returns a callback for a computed
    /// default or for `init`.
    fn advance(&mut self, vm: &mut Vm, prog: &Program, source: &mut dyn DecodeSource) -> CodecResult<Option<Callback>> {
        let frame = self.frames.last_mut().expect("an open frame");
        let (at, s) = (frame.at, frame.shape);
        let dict = self.walk.values[at];
        let children = self.walk.values.len() - at - 1;
        let shape = &prog.codec_shapes[s as usize];
        if !frame.ended {
            let part = match &mut frame.kind {
                DecodeKind::Seq => source.next()?.then_some(0),
                DecodeKind::Tuple => (children < shape.parts.len() && source.next()?).then_some(children as u32),
                DecodeKind::Map if children % 2 == 1 => Some(1),
                DecodeKind::Map => source.next()?.then_some(0),
                DecodeKind::Some => (children == 0).then_some(0),
                DecodeKind::Slots { names, form, seen, next, end } => {
                    let base = match end {
                        SlotsEnd::Variant { base, .. } => *base,
                        _ => 0,
                    };
                    match form {
                        PayloadForm::Named => match source.next_field()? {
                            Some(name) => match names
                                .get(prog)
                                .and_then(|ns| field_index(ns, &name, *next + usize::from(seen.get(*next) == Some(&true))))
                            {
                                Some(i) if seen[i] => {
                                    let name = crate::stdlib::truncate(&name, 64).to_string();
                                    return parse_error(source, format!("duplicate field `{name}`"));
                                }
                                Some(i) => {
                                    (seen[i], *next) = (true, i);
                                    Some(base + i as u32)
                                }
                                None => {
                                    drop(name);
                                    source.skip()?;
                                    return Ok(None);
                                }
                            },
                            None => None,
                        },
                        PayloadForm::Unit => None,
                        PayloadForm::Positional(_) => match seen.iter().position(|s| !s) {
                            Some(i) if source.next()? => {
                                (seen[i], *next) = (true, i);
                                Some(base + i as u32)
                            }
                            _ => None,
                        },
                    }
                }
            };
            match part {
                Some(part) => {
                    self.pending = Some(self.walk.part_dict(vm, prog, s, part, dict)?);
                    return Ok(None);
                }
                None => frame.ended = true,
            }
        }
        if let DecodeKind::Slots { names, seen, next, end, .. } = &mut frame.kind
            && let Some(missing) = seen.iter().position(|s| !s)
        {
            let base = match end {
                SlotsEnd::Variant { base, .. } => *base,
                _ => 0,
            };
            let name = || names.get(prog).map_or_else(|| missing.to_string(), |ns| ns[missing].clone());
            let value = match shape.fills.get(missing).cloned().unwrap_or(Fill::Required) {
                Fill::Required => {
                    let d = self.walk.part_dict(vm, prog, s, base + missing as u32, dict)?;
                    match method(vm, d)? {
                        Method::Structural(p) if matches!(shape_kind(vm, p)?, ShapeKind::Option) => VmValue::Tag(vm.prog.ids.none),
                        _ if matches!(names, Names::Positional) => {
                            let arity = seen.len();
                            return parse_error(source, format!("expected {arity} payload values"));
                        }
                        _ => return parse_error(source, format!("missing field `{}`", name())),
                    }
                }
                Fill::Const(c) => constant(vm, &c)?,
                Fill::Call(f, dicts) => {
                    let mut args = dicts.iter().map(|t| eval_template(vm, t, dict)).collect::<Result<Vec<_>, _>>()?;
                    // The default helper of an `init` parameter takes the parameters before it.
                    if matches!(names, Names::Params(_)) {
                        args.extend_from_slice(&self.walk.values[at + 1..at + 1 + missing]);
                    }
                    (seen[missing], *next) = (true, missing);
                    return Ok(Some(Callback { callee: vm.consts[f as usize], args, purpose: Purpose::Fill }));
                }
            };
            let frame = self.frames.last_mut().expect("an open frame");
            if let DecodeKind::Slots { seen, next, .. } = &mut frame.kind {
                (seen[missing], *next) = (true, missing);
            }
            self.walk.values[at + 1 + missing] = value;
            return Ok(None);
        }
        let children = &self.walk.values[at + 1..];
        let value = match &self.frames.last().expect("an open frame").kind {
            DecodeKind::Seq if matches!(shape.kind, ShapeKind::Set) => {
                vm.check_memory(NODE_BYTES + children.len() * NODE_BYTES)?;
                let children = children.to_vec();
                let set = vm.heap.alloc(Obj::Set(Box::<SetObj>::default()));
                for x in children {
                    vm.set_add(set, x)?;
                }
                set
            }
            DecodeKind::Seq => vm.heap.new_list(children.to_vec()),
            DecodeKind::Tuple if children.len() != shape.parts.len() => {
                return parse_error(source, format!("expected {} elements, found {}", shape.parts.len(), children.len()));
            }
            DecodeKind::Tuple => vm.heap.new_tuple(children),
            DecodeKind::Map => {
                vm.check_memory(NODE_BYTES + children.len() * NODE_BYTES)?;
                let children = children.to_vec();
                let map = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
                for &[k, v] in children.as_chunks::<2>().0 {
                    vm.map_insert(map, k, v)?;
                }
                map
            }
            DecodeKind::Some => vm.some(children[0]),
            DecodeKind::Slots { end: SlotsEnd::Struct(def), .. } => vm.heap.alloc(Obj::Struct(*def, children.into())),
            DecodeKind::Slots { end: SlotsEnd::Variant { id, .. }, .. } => vm.heap.new_variant(*id, children),
            DecodeKind::Slots { end: SlotsEnd::Init, .. } => {
                let Some(init) = &shape.init else { return Err(Fail::internal("decode: an init shape without init").into()) };
                let mut args = init.dicts.iter().map(|t| eval_template(vm, t, dict)).collect::<Result<Vec<_>, _>>()?;
                args.extend_from_slice(children);
                source.end()?;
                return Ok(Some(Callback { callee: vm.consts[init.func as usize], args, purpose: Purpose::Init }));
            }
        };
        source.end()?;
        self.pop_frame();
        self.place(value);
        Ok(None)
    }

    /// Ends the innermost frame and drops its heap values.
    fn pop_frame(&mut self) {
        let frame = self.frames.pop().expect("an open frame");
        self.depth -= usize::from(!matches!(frame.kind, DecodeKind::Some));
        self.walk.values.truncate(frame.at);
    }
}

fn shape_kind(vm: &Vm, s: u32) -> Result<&ShapeKind, Fail> {
    Ok(&shape(vm, s)?.kind)
}

/// Returns the index of the field `name` in `names`, trying `guess` first.
fn field_index(names: &[String], name: &str, guess: usize) -> Option<usize> {
    if names.get(guess).is_some_and(|n| n == name) {
        return Some(guess);
    }
    names.iter().position(|n| n == name)
}

/// Builds the heap value of constant `c`.
pub(crate) fn constant(vm: &mut Vm, c: &ConstVal) -> Result<VmValue, Fail> {
    vm.charge(1)?;
    let all = |vm: &mut Vm, xs: &[ConstVal]| xs.iter().map(|x| constant(vm, x)).collect::<Result<Vec<_>, _>>();
    Ok(match c {
        ConstVal::Unit => VmValue::Unit,
        ConstVal::Bool(b) => VmValue::Bool(*b),
        ConstVal::Int(i) => VmValue::Int(*i),
        ConstVal::Float(f) => VmValue::Float(*f),
        ConstVal::Dur(d) => VmValue::Dur(*d),
        ConstVal::Str(s) => {
            vm.charge_bytes(s.len())?;
            vm.heap.new_str(s.as_str())
        }
        ConstVal::Bytes(b) => {
            vm.charge_bytes(b.len())?;
            vm.heap.new_bytes(b.as_slice())
        }
        ConstVal::Range(a, b, inclusive) => vm.heap.alloc(Obj::Range(*a, *b, *inclusive)),
        ConstVal::Tuple(xs) => {
            let items = all(vm, xs)?;
            vm.heap.new_tuple(&items)
        }
        ConstVal::List(xs) => {
            let items = all(vm, xs)?;
            vm.heap.new_list(items)
        }
        ConstVal::Set(xs) => {
            let items = all(vm, xs)?;
            let set = vm.heap.alloc(Obj::Set(Box::<SetObj>::default()));
            for x in items {
                vm.set_add(set, x)?;
            }
            set
        }
        ConstVal::Map(kvs) => {
            let map = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
            for (k, v) in kvs {
                let (k, v) = (constant(vm, k)?, constant(vm, v)?);
                vm.map_insert(map, k, v)?;
            }
            map
        }
        ConstVal::Variant(id, payload) => {
            let payload = all(vm, payload)?;
            vm.heap.new_variant(*id, &payload)
        }
        ConstVal::Struct(def, fields) => {
            let fields = all(vm, fields)?;
            vm.heap.alloc(Obj::Struct(*def, fields.into_boxed_slice()))
        }
    })
}

/// The error type of a native function that runs a walk: [`HostError`]
/// for a Sand `Result`, or [`Fail`] to stop the run on error.
pub(crate) trait WalkError: NativeError {
    fn from_walk(e: HostError<Fail>) -> Self;
    fn from_future_walk(e: HostError<Fail>) -> Self::FutureError;
}

impl WalkError for HostError {
    fn from_walk(e: HostError<Fail>) -> Self {
        e.into_ctl_error()
    }

    fn from_future_walk(e: HostError<Fail>) -> HostError<Fail> {
        e
    }
}

impl WalkError for Fail {
    fn from_walk(e: HostError<Fail>) -> Self {
        abort(e)
    }

    fn from_future_walk(e: HostError<Fail>) -> Fail {
        abort(e)
    }
}

/// Encodes `value` with `dict` into the sink of `shared`, and then runs
/// `finish` on the sink. Waits for callbacks if the walk reaches a custom
/// method.
pub(crate) fn encode<S, T, E>(
    vm: &mut Vm,
    shared: Arc<Mutex<Shared<S>>>,
    value: VmValue,
    dict: VmValue,
    finish: impl FnOnce(&mut Vm, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    S: WalkSink + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    let cell: EncodeCell = shared.clone();
    let mut walk = EncodeWalk::new(value, dict);
    match walk.run(vm, &cell).map_err(E::from_walk)? {
        Step::Done => Ok(Later::ready(finish(vm, &mut shared.lock().io).map_err(E::from_future_walk))),
        Step::Collect(e) => Err(E::from_walk(e.into())),
        step => Ok(Later::new(WaitKind::Io, |mut cx| async move {
            walk.finish(&mut cx, &cell, step).await.map_err(E::from_future_walk)?;
            cx.with_vm(|vm| finish(vm, &mut shared.lock().io)).map_err(E::from_future_walk)
        })),
    }
}

/// Decodes a value with `dict` from the source of `shared`, and then runs
/// `finish` on the value and the source. Waits for callbacks if the walk
/// reaches a custom method, an `init` constructor, or a computed default.
pub(crate) fn decode<S, T, E>(
    vm: &mut Vm,
    shared: Arc<Mutex<Shared<S>>>,
    dict: VmValue,
    finish: impl FnOnce(&mut Vm, VmValue, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    S: DecodeSource + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    let cell: DecodeCell = shared.clone();
    let mut walk = DecodeWalk::new(dict);
    match walk.run(vm, &cell).map_err(E::from_walk)? {
        Step::Done => {
            let v = walk.result().map_err(|e| E::from_walk(e.into()))?;
            Ok(Later::ready(finish(vm, v, &mut shared.lock().io).map_err(E::from_future_walk)))
        }
        Step::Collect(e) => Err(E::from_walk(e.into())),
        step => Ok(Later::new(WaitKind::Io, |mut cx| async move {
            let v = walk.finish(&mut cx, &cell, step).await.map_err(E::from_future_walk)?;
            cx.with_vm(|vm| finish(vm, v, &mut shared.lock().io)).map_err(E::from_future_walk)
        })),
    }
}

/// Encodes `value` with `dict` as a [`Value`](crate::value::Value).
pub(crate) async fn to_value(cx: &mut NativeContext, value: VmValue, dict: VmValue) -> CodecResult<crate::value::Value> {
    let shared = Shared::new(crate::value::ValueSink::default());
    let cell: EncodeCell = shared.clone();
    let mut walk = EncodeWalk::new(value, dict);
    let first = cx.with_vm(|vm| walk.run(vm, &cell))?;
    walk.finish(cx, &cell, first).await?;
    let root = shared.lock().io.root.take();
    root.ok_or_else(|| Fail::internal("to_value: no value").into())
}

/// Decodes `value` with `dict`. Returns the decoded heap value, which is
/// valid until the next wait.
pub(crate) async fn from_value(cx: &mut NativeContext, value: crate::value::Value, dict: VmValue) -> CodecResult<VmValue> {
    let shared: DecodeCell = Shared::new(crate::value::ValueSource::new(value));
    let mut walk = DecodeWalk::new(dict);
    let first = cx.with_vm(|vm| walk.run(vm, &shared))?;
    walk.finish(cx, &shared, first).await
}

/// Writes `value` as the single value of `encoder`.
fn write(vm: &mut Vm, dict: VmValue, value: VmValue, encoder: VmVal<Encoder>) -> Result<Later<(), HostError>, HostError> {
    let e = encoder.get(&vm.heap)?;
    let (shared, token) = (e.shared.clone(), e.token);
    shared.lock().turn.claim(
        token,
        "an `Encoder` accepts a value only during its `encode` call",
        "an `encode` method wrote a second value to its `Encoder`",
    )?;
    let mut walk = EncodeWalk::new(value, dict);
    Ok(Later::new(WaitKind::Io, |mut cx| async move {
        let first = cx.with_vm(|vm| walk.run(vm, &shared));
        let r = match first {
            Ok(step) => walk.finish(&mut cx, &shared, step).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &r {
            shared.lock().turn.record(e);
        }
        r
    }))
}

/// Reads the single value of `decoder`.
fn read(vm: &mut Vm, dict: VmValue, decoder: VmVal<Decoder>) -> Result<Later<Param<0>, HostError>, HostError> {
    let d = decoder.get(&vm.heap)?;
    let (shared, token) = (d.shared.clone(), d.token);
    shared.lock().turn.claim(
        token,
        "a `Decoder` gives a value only during its `decode` call",
        "a `decode` method read a second value from its `Decoder`",
    )?;
    let mut walk = DecodeWalk::new(dict);
    Ok(Later::new(WaitKind::Io, |mut cx| async move {
        let first = cx.with_vm(|vm| walk.run(vm, &shared));
        let r = match first {
            Ok(step) => walk.finish(&mut cx, &shared, step).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &r {
            shared.lock().turn.record(e);
        }
        r.map(Param::from_raw)
    }))
}

/// Registers the prelude natives of `Encode` and `Decode`.
pub(crate) fn register(m: &mut crate::native::NativeModule) {
    m.bind::<Encoder>().bind::<Decoder>().bind::<InputKind>().bind::<EncodedArg>();
    m.function("encode_arg", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, value: Param<0>| {
        encode::<_, _, Fail>(vm, Shared::new(ValueSink::default()), value.raw(), dict.raw(), |vm, sink| {
            let value = sink.root.take().ok_or_else(|| Fail::internal("encode_arg: no value"))?;
            Ok(VmVal::<EncodedArg>::from_raw(EncodedArg(Arc::new(value)).into_vm_value(vm)?))
        })
    });
    m.function("Encode.encode", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, value: Param<0>, encoder: VmVal<Encoder>| {
        write(vm, dict.raw(), value.raw(), encoder)
    });
    m.function("Encoder.encode", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, encoder: VmVal<Encoder>, value: Param<0>| {
        write(vm, dict.raw(), value.raw(), encoder)
    });
    m.function("Decode.decode", |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| read(vm, dict.raw(), decoder));
    m.function("Decoder.decode", |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| read(vm, dict.raw(), decoder));
    m.function("Decoder.kind", |vm: &mut Vm, decoder: VmVal<Decoder>| -> Result<InputKind, HostError> {
        let d = decoder.get(&vm.heap)?;
        let mut state = d.shared.lock();
        if state.turn.active != d.token {
            return Err(state.turn.misuse("a `Decoder` gives a value only during its `decode` call"));
        }
        if state.turn.used {
            return Err(state.turn.misuse("a `decode` method asked for the kind of a value that it read"));
        }
        state.io.kind().map_err(HostError::into_ctl_error)
    });
}
