//! Iterative walkers for the `Encode` and `Decode` traits.
//!
//! Each walker processes a value part by part. For a structural
//! implementation, the walker derives each part's dictionary from the outer
//! dictionary and its [`Shape`]. It then processes the part in Rust. If a
//! part uses a custom trait method, an `init` constructor, or a computed
//! default, the walker yields a [`Callback`] to the scheduler.
//!
//! [`WalkState`] stores all live heap values so the caller can root them
//! across callbacks and garbage collections. Before visiting each part, the
//! walker checks the memory limit. If that check fails, the walker returns
//! [`Step::Collect`]. The caller collects garbage and resumes the walker. If a
//! part fails after mutating the sink or source, the walker aborts the walk.
//! Only an initial synchronous pass before any suspension can return a
//! retryable limit error to the VM.

mod plan;

use crate::engine::LimitKind;
use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::context::NativeContext;
use crate::native::convert::{HostError, IntoVmValue};
use crate::native::func::{Later, NativeError, NativeReturn};
use crate::native::ty::{EncodedArg, Param, TraitDict, VmVal};
use crate::types::{ConstVal, traits};
use crate::value::StructDesc;
use crate::value::ValueSink;
use crate::vm::bytecode::{PartFill, PartTemplate, Program, SHAPE_SLOT, Shape, ShapeKind};
use crate::vm::meter::SLOW_BYTES_PER_UNIT;
use crate::vm::value::{FxHashMap, MapObj, Obj, SetObj, VariantRef, VmValue};
use crate::vm::{CtlKind, Fail, Vm, WaitKind};
use parking_lot::Mutex;
use std::sync::{Arc, Weak};

/// Heap byte estimate reserved per visited value.
pub(crate) const NODE_BYTES: usize = 64;

/// Number of visited parts per amortized memory limit check.
const CHECKED_VALUES: u32 = 32;

/// Maximum container nesting depth for encoding and decoding.
///
/// This limit also stops cyclic values.
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// Sand function call requested by a walker before continuing.
pub(crate) struct Callback {
    pub callee: VmValue,
    pub args: Vec<VmValue>,
    pub kind: CallbackKind,
}

/// Role of a [`Callback`] return value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallbackKind {
    /// Custom `encode` or `decode` method.
    Method,
    /// Computed field or parameter default helper.
    Default,
    /// Struct `init` constructor.
    Init,
}

/// Outcome of a single walker pass.
pub(crate) enum Step {
    Done,
    Call(Callback),
    /// Memory limit reached before a step. Collect garbage before resuming.
    Collect(Fail),
}

/// GC-rooted heap state shared by [`EncodeWalker`] and [`DecodeWalker`].
#[derive(Default)]
pub(crate) struct WalkState {
    /// Live heap values on the walker stack.
    values: Vec<VmValue>,
    /// `true` if garbage collection ran since the walker last completed a part.
    collected: bool,
    /// Memoized part dictionaries keyed by `(shape_idx, part, outer_handle)`.
    dict_cache: FxHashMap<(u32, u32, u32), VmValue>,
    /// Heap roots for dictionaries stored in `dict_cache`.
    rooted_dicts: Vec<VmValue>,
}

impl WalkState {
    fn new(values: Vec<VmValue>) -> WalkState {
        WalkState { values, ..WalkState::default() }
    }

    /// Checks the memory limit every [`CHECKED_VALUES`] parts.
    ///
    /// Advances `unchecked_parts` if the check passes. Returns `Some` if the
    /// check stops the current pass.
    fn check_memory(&mut self, vm: &Vm, unchecked_parts: &mut u32) -> Option<CodecResult<Step>> {
        let check = if *unchecked_parts == 0 || vm.heap.fail_next_check.get() {
            let held = self.values.capacity() * size_of::<VmValue>();
            vm.check_memory(CHECKED_VALUES as usize * NODE_BYTES + held)
        } else {
            Ok(())
        };
        let step = self.memory_check_step(check);
        if step.is_none() {
            *unchecked_parts = (*unchecked_parts + 1) % CHECKED_VALUES;
        }
        step
    }

    /// Converts `check` into a walker stop if `check` failed.
    ///
    /// Returns [`Step::Collect`] on the first memory limit failure. Returns a
    /// terminal error if garbage collection already ran for the current part.
    fn memory_check_step(&mut self, check: Result<(), Fail>) -> Option<CodecResult<Step>> {
        match check {
            Ok(()) => None,
            Err(err) if !self.collected && err.as_limit() == Some(LimitKind::Memory) => Some(Ok(Step::Collect(err))),
            Err(err) => Some(Err(err.into())),
        }
    }

    /// Resolves the dictionary for `part` of `shape_idx` from `outer_dict`.
    fn part_dict(&mut self, vm: &mut Vm, prog: &Program, shape_idx: u32, part: u32, outer_dict: VmValue) -> Result<VmValue, Fail> {
        let Some(template) = shape(prog, shape_idx)?.parts.get(part as usize) else {
            return Err(Fail::internal("a codec shape has too few parts"));
        };
        match template {
            PartTemplate::Const(const_idx) => Ok(vm.consts[*const_idx as usize]),
            PartTemplate::Bound(slot) => dict_slot(vm, outer_dict, *slot),
            _ => {
                let key = (shape_idx, part, heap_handle(outer_dict));
                if let Some(&dict) = self.dict_cache.get(&key) {
                    return Ok(dict);
                }
                let dict = eval_template(vm, template, outer_dict)?;
                self.dict_cache.insert(key, dict);
                self.rooted_dicts.push(dict);
                Ok(dict)
            }
        }
    }

    /// Borrows the two root vectors held by this walk.
    fn roots(&mut self) -> [&mut Vec<VmValue>; 2] {
        [&mut self.values, &mut self.rooted_dicts]
    }

    /// Runs garbage collection while keeping all walk values rooted.
    async fn collect(&mut self, context: &mut NativeContext) {
        context.collect_with_roots(self.roots()).await;
        self.collected = true;
    }
}

fn heap_handle(value: VmValue) -> u32 {
    match value {
        VmValue::Obj(obj_handle) => obj_handle,
        _ => u32::MAX,
    }
}

fn dict_slot(vm: &Vm, dict: VmValue, slot: u32) -> Result<VmValue, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => items.get(slot as usize).copied().ok_or_else(|| Fail::internal("a codec dictionary has too few slots")),
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

/// Evaluates `template` against `outer_dict`.
///
/// Each walker pass pre-checks enough heap headroom to cover these tuple
/// allocations.
fn eval_template(vm: &mut Vm, template: &PartTemplate, outer_dict: VmValue) -> Result<VmValue, Fail> {
    Ok(match template {
        PartTemplate::Const(const_idx) => vm.consts[*const_idx as usize],
        PartTemplate::Bound(slot) => dict_slot(vm, outer_dict, *slot)?,
        PartTemplate::Field(inner, slot) => {
            let dict = eval_template(vm, inner, outer_dict)?;
            dict_slot(vm, dict, *slot)?
        }
        PartTemplate::Dict(items) => {
            vm.charge(1)?;
            let items = items.iter().map(|inner| eval_template(vm, inner, outer_dict)).collect::<Result<Vec<_>, _>>()?;
            vm.heap.alloc(Obj::Tuple(items.into_boxed_slice()))
        }
    })
}

/// Runs custom method `callee` with `args` if it is a native function.
///
/// Returns `None` for a Sand method, which runs as a callback. A native method
/// must finish without waiting.
fn call_native(vm: &mut Vm, callee: VmValue, args: &[VmValue]) -> CodecResult<Option<VmValue>> {
    match vm.call_native_value(callee, args) {
        None => Ok(None),
        Some(Ok(result)) => Ok(Some(result)),
        Some(Err(ctl)) => Err(match ctl.into_kind() {
            CtlKind::Fail(fail) => fail.into(),
            CtlKind::Collect => Fail::limit(LimitKind::Memory).into(),
            _ => Fail::internal("a native `encode` or `decode` method waited").into(),
        }),
    }
}

/// Dispatch target extracted from a codec dictionary.
enum CodecDispatch {
    /// Structural walk for the [`Shape`] at this index.
    Structural(u32),
    /// Custom Sand method closure or native function.
    Custom(VmValue),
}

fn codec_dispatch(vm: &Vm, dict: VmValue) -> Result<CodecDispatch, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => match (items.first(), items.get(SHAPE_SLOT)) {
            (_, Some(&VmValue::Int(shape_idx))) => Ok(CodecDispatch::Structural(shape_idx as u32)),
            (Some(&callee), _) => Ok(CodecDispatch::Custom(callee)),
            (None, _) => Err(Fail::internal("a codec dictionary is empty")),
        },
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

/// Looks up `shape_idx` in `prog.shapes`.
///
/// Validates the index because host callers can supply arbitrary dictionary
/// tuples.
fn shape(prog: &Program, shape_idx: u32) -> Result<&Shape, Fail> {
    prog.shapes.get(shape_idx as usize).ok_or_else(|| Fail::internal("unknown codec shape"))
}

/// Returns a `"limit"` error for values nested deeper than
/// [`MAX_NESTING_DEPTH`].
pub(crate) fn nesting_limit_error<T>() -> CodecResult<T> {
    Err(HostError::new("limit", "value too deeply nested to encode or decode"))
}

/// Prefixes recoverable callback error `err` with the current path of `source`.
fn with_source_path(err: HostError<Fail>, source: &dyn DecodeSource) -> HostError<Fail> {
    match err.sand_error() {
        Some((kind, message)) => HostError::new(kind, format!("{}: {message}", source.path())),
        None => err,
    }
}

fn parse_error<T>(source: &dyn DecodeSource, msg: impl std::fmt::Display) -> CodecResult<T> {
    Err(HostError::new("parse", format!("{}: {msg}", source.path())))
}

/// Extracts the payload if `value` is `Result.Err`.
fn err_payload(vm: &Vm, value: VmValue) -> Option<VmValue> {
    (vm.heap.variant_id(value) == vm.prog.ids.err).then(|| vm.heap.payload_if(&value, vm.prog.ids.err))
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
fn walk_error_to_fail(err: HostError<Fail>) -> Fail {
    match err.into_abort() {
        Ok(fail) => fail,
        Err(err) => {
            let (kind, message) = err.sand_error().unwrap_or(("error", ""));
            Fail::runtime_error(kind, message)
        }
    }
}

/// Tracks single-use turn ownership for `Encoder` and `Decoder` handles.
pub(crate) struct Turn {
    /// Token of the active handle, or 0 if inactive.
    active: u64,
    /// `true` once the active handle has consumed its single operation.
    used: bool,
    next_token: u64,
    /// Count of in-flight nested writes or reads.
    ///
    /// A spawned task can keep an operation in flight after its custom method
    /// returns.
    in_flight: u32,
    /// Saved `(kind, message)` from a failed nested operation.
    failed: Option<(String, String)>,
}

/// Snapshot of [`Turn`] saved across a nested custom method callback.
#[derive(Clone, Copy)]
struct SavedTurn {
    active: u64,
    used: bool,
    in_flight: u32,
}

impl Turn {
    /// Activates a fresh handle token and returns the interrupted
    /// [`SavedTurn`].
    fn begin(&mut self) -> (u64, SavedTurn) {
        let token = self.next_token;
        self.next_token += 1;
        let saved = SavedTurn { active: self.active, used: self.used, in_flight: self.in_flight };
        (self.active, self.used) = (token, false);
        (token, saved)
    }

    /// Claims the single operation of `token`.
    ///
    /// Fails with `inactive` if `token` is not active. Fails with `second` if
    /// `token` was already used.
    fn claim(&mut self, token: u64, inactive: &str, second: &str) -> Result<(), HostError> {
        if self.active != token {
            return Err(self.misuse(inactive));
        }
        if self.used {
            return Err(self.misuse(second));
        }
        self.used = true;
        self.in_flight += 1;
        Ok(())
    }

    /// Restores `saved` after `token`'s callback finishes.
    ///
    /// Returns `Ok(true)` if `token` was never used. Propagates any recorded
    /// nested error. Fails with `unfinished` if a nested operation is still in
    /// flight.
    fn end(&mut self, token: u64, saved: SavedTurn, unfinished: &str) -> CodecResult<bool> {
        let unused = self.active == token && !self.used;
        let has_in_flight = self.in_flight > saved.in_flight;
        (self.active, self.used) = (saved.active, saved.used);
        if let Some((kind, message)) = self.failed.take() {
            return Err(HostError::new(kind, message));
        }
        if has_in_flight {
            return Err(HostError::new("invalid", unfinished));
        }
        Ok(unused)
    }

    /// Saves a recoverable nested error so the outer walk fails even if the
    /// custom method discards the `Result`.
    fn record_error(&mut self, err: &HostError<Fail>) {
        if let Some((kind, message)) = err.sand_error() {
            self.failed = Some((kind.to_string(), message.to_string()));
        }
    }

    /// Records and returns an `"invalid"` error for handle misuse.
    fn misuse(&mut self, message: &str) -> HostError {
        self.failed = Some(("invalid".to_string(), message.to_string()));
        HostError::new("invalid", message)
    }
}

/// Turn state and sink or source shared with `Encoder` or `Decoder` handles.
pub(crate) struct Shared<IO: ?Sized> {
    turn: Turn,
    pub io: IO,
}

impl<IO> Shared<IO> {
    pub(crate) fn new(io: IO) -> Arc<Mutex<Shared<IO>>> {
        let turn = Turn { active: 0, used: false, next_token: 1, in_flight: 0, failed: None };
        Arc::new(Mutex::new(Shared { turn, io }))
    }
}

/// Shared state for an active encode walk.
pub(crate) type SharedEncode = Arc<Mutex<Shared<dyn EncodeSink>>>;

/// Shared state for an active decode walk.
pub(crate) type SharedDecode = Arc<Mutex<Shared<dyn DecodeSource>>>;

/// Handle that writes the single value of a custom `encode` method.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Encoder {
    /// Weak reference to the shared encode state.
    ///
    /// The walk drops the sink after finishing even if Sand code retains the
    /// `Encoder`.
    shared: Weak<Mutex<Shared<dyn EncodeSink>>>,
    token: u64,
}

impl crate::native::NativeObject for Encoder {}

/// Handle that reads the single value of a custom `decode` method.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub struct Decoder {
    /// Weak reference to the shared decode state.
    ///
    /// The walk drops the source after finishing even if Sand code retains the
    /// `Decoder`.
    shared: Weak<Mutex<Shared<dyn DecodeSource>>>,
    token: u64,
}

impl crate::native::NativeObject for Decoder {}

impl Encoder {
    /// Writes the value of a native `encode` method to the sink of `encoder`.
    ///
    /// `write` must write a single value. Fails with kind `"invalid"` if
    /// `encoder` already received a value or if its `encode` call returned.
    pub fn write<T>(
        vm: &mut Vm,
        encoder: VmVal<Encoder>,
        write: impl FnOnce(&mut Vm, &mut dyn EncodeSink) -> CodecResult<T>,
    ) -> Result<T, HostError> {
        let (shared, token) = encoder.get(&vm.heap).map(|encoder_ref| (encoder_ref.shared.upgrade(), encoder_ref.token))?;
        let shared = claim(shared, token, ENCODER_MISUSE)?;
        let result = write(vm, &mut shared.lock().io);
        release(&shared, &result);
        result.map_err(HostError::into_ctl_error)
    }
}

impl Decoder {
    /// Reads the value of a native `decode` method from the source of
    /// `decoder`.
    ///
    /// `read` must read a single value. Fails with kind `"invalid"` if
    /// `decoder` already gave a value or if its `decode` call returned.
    pub fn read<T>(
        vm: &mut Vm,
        decoder: VmVal<Decoder>,
        read: impl FnOnce(&mut Vm, &mut dyn DecodeSource) -> CodecResult<T>,
    ) -> Result<T, HostError> {
        let (shared, token) = decoder.get(&vm.heap).map(|decoder_ref| (decoder_ref.shared.upgrade(), decoder_ref.token))?;
        let shared = claim(shared, token, DECODER_MISUSE)?;
        let result = read(vm, &mut shared.lock().io);
        release(&shared, &result);
        result.map_err(HostError::into_ctl_error)
    }
}

/// Open container frame on the [`EncodeWalker`] stack.
struct EncodeFrame {
    /// Start index of this frame's slots in [`WalkState::values`].
    ///
    /// Frame slots:
    /// - `base`: container value.
    /// - `base + 1`: container dictionary.
    /// - `base + 2`: set or map iterator.
    /// - `base + 3`: pending map value.
    base: usize,
    shape: u32,
    kind: EncodeKind,
    /// Index of the next child to visit.
    next: usize,
    /// Initial list length snapshot.
    ///
    /// The walker stops at `list_len` even if a callback appends to the list.
    /// This matches `for` loop semantics.
    list_len: usize,
}

/// Container kind for an [`EncodeFrame`].
///
/// For sets and maps, the walker iterates over a `Vm::container_iter` snapshot.
/// Mutations during callbacks cannot corrupt traversal.
#[derive(Clone, Copy)]
enum EncodeKind {
    /// List elements. All elements use part 0.
    List,
    /// Set elements. All elements use part 0.
    Set,
    /// Tuple elements or struct fields. Each index uses its own part.
    Fields,
    /// Map entries. Even indices visit keys. Odd indices visit values.
    Map,
    /// Variant payload fields. Their parts start at `part_base` in
    /// `Shape::parts`.
    Payload { part_base: u32 },
    /// Payload of `Option.Some`.
    Some,
}

/// Iterative walker that encodes a Sand value into an [`EncodeSink`].
pub(crate) struct EncodeWalker {
    state: WalkState,
    /// Parts visited since the previous memory limit check.
    unchecked_parts: u32,
    frames: Vec<EncodeFrame>,
    /// Active container depth excluding `Some` wrappers.
    depth: usize,
    /// Next `(value, dict)` pair to visit.
    pending: Option<(VmValue, VmValue)>,
    /// Active `Encoder` token and interrupted [`SavedTurn`] during a custom
    /// `encode` callback.
    callback: Option<(u64, SavedTurn)>,
}

impl EncodeWalker {
    pub(crate) fn new(value: VmValue, dict: VmValue) -> EncodeWalker {
        let state = WalkState::new(vec![value, dict]);
        EncodeWalker { state, unchecked_parts: 0, frames: Vec::new(), depth: 0, pending: Some((value, dict)), callback: None }
    }

    #[inline(always)]
    fn push_frame(&mut self, vm: &mut Vm, value: VmValue, dict: VmValue, shape: u32, kind: EncodeKind, list_len: usize) -> CodecResult {
        if !matches!(kind, EncodeKind::Some) {
            if self.depth >= MAX_NESTING_DEPTH {
                return nesting_limit_error();
            }
            self.depth += 1;
        }
        let base = self.state.values.len();
        self.state.values.extend([value, dict]);
        if matches!(kind, EncodeKind::Set | EncodeKind::Map) {
            // Pre-step memory check covers allocating the iterator.
            let iter = vm.unchecked(|vm| vm.container_iter(value))?;
            self.state.values.extend([iter, VmValue::Unit]);
        }
        self.frames.push(EncodeFrame { base, shape, kind, next: 0, list_len });
        Ok(())
    }

    fn visit(
        &mut self,
        vm: &mut Vm,
        prog: &Program,
        sink: &mut dyn EncodeSink,
        shape_idx: u32,
        value: VmValue,
        dict: VmValue,
    ) -> CodecResult {
        let shape = shape(prog, shape_idx)?;
        let shape_mismatch = || -> CodecResult { Err(Fail::internal("encode: a value does not match its shape").into()) };
        match (&shape.kind, value) {
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
                _ => shape_mismatch()?,
            },
            (ShapeKind::Native(def, _), _) => {
                let Some(text) = vm.heap.get(value).value().map(|native| native.text()) else { return shape_mismatch() };
                vm.meter.charge_bytes(text.len())?;
                sink.native(&prog.types[*def as usize].desc().type_name, &text)?;
            }
            (ShapeKind::List, _) => {
                let len = vm.heap.list(value).len();
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::List, len)?;
                sink.begin_list(len)?;
            }
            (ShapeKind::Set, _) => {
                let Obj::Set(set) = vm.heap.get(value) else { return shape_mismatch() };
                let live = set.live;
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Set, 0)?;
                sink.begin_set(live)?;
            }
            (ShapeKind::Map, _) => {
                let Obj::Map(map) = vm.heap.get(value) else { return shape_mismatch() };
                let live = map.live;
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Map, 0)?;
                sink.begin_map(live)?;
            }
            (ShapeKind::Tuple, _) => {
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Fields, 0)?;
                sink.begin_tuple(shape.parts.len())?;
            }
            (ShapeKind::Struct(def), _) => {
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Fields, 0)?;
                sink.begin_struct(prog.types[*def as usize].desc())?;
            }
            (ShapeKind::Option, _) if vm.heap.variant_id(value) == prog.ids.none => sink.none()?,
            (ShapeKind::Option, _) => {
                sink.begin_some()?;
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Some, 0)?;
            }
            (ShapeKind::Enum { .. }, _) => {
                let id = vm.heap.variant_id(value);
                let Some(desc) = prog.variant_desc(id) else { return shape_mismatch() };
                let variant = &prog.variants[id as usize];
                sink.begin_variant(desc, variant.arity)?;
                if variant.arity == 0 {
                    return sink.end();
                }
                charge_scan(vm, variant.index as usize)?;
                let part_base = (id - variant.index..id).map(|variant_id| prog.variants[variant_id as usize].arity as u32).sum();
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Payload { part_base }, 0)?;
            }
            _ => shape_mismatch()?,
        }
        Ok(())
    }

    /// Advances to the next child `(value, dict)` or closes finished
    /// containers.
    fn next_part(&mut self, vm: &mut Vm, prog: &Program, sink: &mut dyn EncodeSink) -> CodecResult<Option<(VmValue, VmValue)>> {
        loop {
            let Some(frame) = self.frames.last() else { return Ok(None) };
            let (container, dict, shape) = (self.state.values[frame.base], self.state.values[frame.base + 1], frame.shape);
            let (base, next) = (frame.base, frame.next);
            let child = match frame.kind {
                EncodeKind::List => {
                    let items = vm.heap.list(container);
                    items[..frame.list_len.min(items.len())].get(next).map(|&item| (item, 0, next + 1))
                }
                EncodeKind::Set => {
                    let mut element = VmValue::Unit;
                    vm.set_step(self.state.values[base + 2], &mut element).then_some((element, 0, next + 1))
                }
                EncodeKind::Fields => field(vm, container, next)?.map(|item| (item, next as u32, next + 1)),
                EncodeKind::Map if next % 2 == 1 => Some((self.state.values[base + 3], 1, next + 1)),
                EncodeKind::Map => {
                    let mut entry = [VmValue::Unit; 2];
                    let found = vm.map_step(self.state.values[base + 2], &mut entry);
                    self.state.values[base + 3] = entry[1];
                    found.then_some((entry[0], 0, next + 1))
                }
                EncodeKind::Some => (frame.next == 0).then(|| (vm.heap.payload_if(&container, vm.prog.ids.some), 0, 1)),
                EncodeKind::Payload { part_base } => {
                    payload(vm, container, frame.next).map(|item| (item, part_base + frame.next as u32, frame.next + 1))
                }
            };
            match child {
                Some((value, part, next)) => {
                    let dict = self.state.part_dict(vm, prog, shape, part, dict)?;
                    self.frames.last_mut().expect("an open frame").next = next;
                    return Ok(Some((value, dict)));
                }
                None => {
                    sink.end()?;
                    let frame = self.frames.pop().expect("an open frame");
                    self.depth -= usize::from(!matches!(frame.kind, EncodeKind::Some));
                    self.state.values.truncate(frame.base);
                }
            }
        }
    }
}

impl Walker for EncodeWalker {
    type IO = dyn EncodeSink;
    type Output = ();
    /// Advances until completion, a memory collection stop, or a callback.
    fn run(&mut self, vm: &mut Vm, shared: &SharedEncode) -> CodecResult<Step> {
        let prog = vm.prog.clone();
        let mut shared_guard = shared.lock();
        // Reset `unchecked_parts` because callbacks can allocate on the heap.
        self.unchecked_parts = 0;
        loop {
            if let Some(step) = self.state.check_memory(vm, &mut self.unchecked_parts) {
                return step;
            }
            let (value, dict) = match self.pending {
                Some(part) => part,
                None => match self.next_part(vm, &prog, &mut shared_guard.io)? {
                    Some(part) => part,
                    None => return Ok(Step::Done),
                },
            };
            vm.charge(1)?;
            match codec_dispatch(vm, dict)? {
                CodecDispatch::Custom(callee) => {
                    let token = shared_guard.turn.next_token;
                    // Allocate `Encoder` without a limit check because `next_part`
                    // already advanced the frame.
                    let encoder = vm.unchecked(|vm| (Encoder { shared: Arc::downgrade(shared), token }).into_vm_value(vm))?;
                    self.callback = Some(shared_guard.turn.begin());
                    self.pending = None;
                    let args = vec![dict, value, encoder];
                    // A native method writes through the `Encoder`, which locks `shared`.
                    drop(shared_guard);
                    let Some(result) = call_native(vm, callee, &args)? else {
                        return Ok(Step::Call(Callback { callee, args, kind: CallbackKind::Method }));
                    };
                    self.apply(vm, shared, CallbackKind::Method, result)?;
                    shared_guard = shared.lock();
                }
                CodecDispatch::Structural(shape_idx) => {
                    self.pending = None;
                    self.visit(vm, &prog, &mut shared_guard.io, shape_idx, value, dict)?;
                    self.state.collected = false;
                }
            }
        }
    }

    /// Validates completion of a custom `encode` callback.
    fn apply(&mut self, vm: &mut Vm, shared: &SharedEncode, _: CallbackKind, result: VmValue) -> CodecResult {
        let Some((token, saved)) = self.callback.take() else { return Err(Fail::internal("encode: no callback").into()) };
        let unused = shared.lock().turn.end(token, saved, "an `encode` method returned before its write ended")?;
        if let Some(err_val) = err_payload(vm, result) {
            return Err(host_error_from_sand(vm, err_val)?);
        }
        if unused {
            return Err(HostError::new("invalid", "an `encode` method wrote no value to its `Encoder`"));
        }
        Ok(())
    }

    fn output(&self) -> Result<(), Fail> {
        Ok(())
    }

    fn state(&mut self) -> &mut WalkState {
        &mut self.state
    }
}

/// Reads element or field `index` from a tuple or struct.
fn field(vm: &Vm, container: VmValue, index: usize) -> Result<Option<VmValue>, Fail> {
    Ok(match container {
        VmValue::Pair(first, second) => [VmValue::Int(i64::from(first)), VmValue::Int(second)].get(index).copied(),
        _ => match vm.heap.get(container) {
            Obj::Tuple(items) | Obj::Struct(_, items) => items.get(index).copied(),
            _ => return Err(Fail::internal("encode: not a tuple or a struct")),
        },
    })
}

fn payload(vm: &Vm, value: VmValue, index: usize) -> Option<VmValue> {
    match vm.heap.variant(value)? {
        VariantRef::Heap(_, fields) => fields.get(index).copied(),
        VariantRef::Inline(_, inner) => inner.filter(|_| index == 0),
    }
}

/// Open container frame on the [`DecodeWalker`] stack.
///
/// Slot `base` in [`WalkState::values`] stores the container's dictionary.
/// Subsequent slots store its decoded children.
struct DecodeFrame {
    base: usize,
    shape: u32,
    kind: DecodeKind,
    /// `true` once the source reports the end of this container.
    ended: bool,
}

enum DecodeKind {
    /// List or set at `base + 1`. The walker appends elements as they arrive.
    Sequence,
    Tuple,
    /// Map at `base + 1` and pending key slot at `base + 2`.
    Map {
        /// `true` if `base + 2` holds a decoded key awaiting its value.
        has_key: bool,
    },
    Some,
    /// Pre-allocated slots for struct fields, `init` parameters, or variant
    /// payloads.
    Slots {
        target: SlotTarget,
        /// Marks which slots have received values.
        seen: Vec<bool>,
        /// Slot index that receives the current decoded value.
        active_slot: usize,
    },
}

/// Destination constructed from [`DecodeKind::Slots`].
enum SlotTarget {
    /// Struct `def` with its descriptor.
    Struct(u32, Arc<StructDesc>),
    /// Parameters of a struct `init` constructor.
    Init(Arc<[String]>),
    /// Variant `id`. Its payload parts start at `part_base` in
    /// `Shape::parts`.
    Variant { id: u32, part_base: u32, named: bool },
}

impl SlotTarget {
    /// Returns field or parameter names for named slots, or `None` for
    /// positional payloads.
    fn names<'a>(&'a self, prog: &'a Program) -> Option<&'a [String]> {
        match self {
            SlotTarget::Struct(_, desc) => Some(&desc.fields),
            SlotTarget::Init(names) => Some(names),
            SlotTarget::Variant { id, named: true, .. } => prog.variants[*id as usize].field_names.as_deref(),
            SlotTarget::Variant { .. } => None,
        }
    }

    /// Returns the starting index in `Shape::parts` for slot 0.
    fn part_base(&self) -> u32 {
        match self {
            SlotTarget::Variant { part_base, .. } => *part_base,
            _ => 0,
        }
    }
}

/// Iterative walker that decodes a Sand value from a [`DecodeSource`].
pub(crate) struct DecodeWalker {
    state: WalkState,
    /// Parts visited since the previous memory limit check.
    unchecked_parts: u32,
    frames: Vec<DecodeFrame>,
    /// Active container depth excluding `Some` wrappers.
    depth: usize,
    /// Dictionary of the next part to decode.
    pending: Option<VmValue>,
    /// Active `Decoder` token and interrupted [`SavedTurn`] during a custom
    /// `decode` callback.
    callback: Option<(u64, SavedTurn)>,
}

impl DecodeWalker {
    pub(crate) fn new(dict: VmValue) -> DecodeWalker {
        DecodeWalker {
            state: WalkState::new(vec![dict]),
            unchecked_parts: 0,
            frames: Vec::new(),
            depth: 0,
            pending: Some(dict),
            callback: None,
        }
    }

    /// Returns the finished root value.
    pub(crate) fn result(&self) -> Result<VmValue, Fail> {
        match self.state.values[..] {
            [_, value] => Ok(value),
            _ => Err(Fail::internal("decode: no result")),
        }
    }

    /// Stores decoded `value` into the innermost open frame or as the root.
    fn place(&mut self, vm: &mut Vm, value: VmValue) -> CodecResult {
        let Some(frame) = self.frames.last_mut() else {
            self.state.values.push(value);
            return Ok(());
        };
        let base = frame.base;
        match &mut frame.kind {
            DecodeKind::Slots { active_slot, .. } => self.state.values[base + 1 + *active_slot] = value,
            DecodeKind::Sequence => {
                let container = self.state.values[base + 1];
                match vm.heap.get(container) {
                    Obj::List(_) => vm.heap.list_push(container, value).map_err(Fail::limit)?,
                    _ => _ = vm.set_add(container, value)?,
                }
            }
            DecodeKind::Map { has_key: has_key @ false } => {
                *has_key = true;
                self.state.values[base + 2] = value;
            }
            DecodeKind::Map { has_key } => {
                *has_key = false;
                let key_val = self.state.values[base + 2];
                vm.map_insert(self.state.values[base + 1], key_val, value)?;
            }
            DecodeKind::Tuple | DecodeKind::Some => self.state.values.push(value),
        }
        Ok(())
    }

    /// Writes `value` to slot `slot` of the innermost frame.
    fn set_slot(&mut self, slot: usize, value: VmValue) -> CodecResult {
        let base = self.frames.last().expect("an open frame").base;
        self.state.values[base + slot] = value;
        Ok(())
    }

    fn push_frame(&mut self, dict: VmValue, shape: u32, kind: DecodeKind, slot_count: usize) -> CodecResult {
        if !matches!(kind, DecodeKind::Some) {
            if self.depth >= MAX_NESTING_DEPTH {
                return nesting_limit_error();
            }
            self.depth += 1;
        }
        let base = self.state.values.len();
        self.state.values.push(dict);
        self.state.values.extend(std::iter::repeat_n(VmValue::Uninit, slot_count));
        self.frames.push(DecodeFrame { base, shape, kind, ended: false });
        Ok(())
    }

    /// Decodes a scalar or pushes a container frame for `shape_idx`.
    fn visit(&mut self, vm: &mut Vm, prog: &Program, source: &mut dyn DecodeSource, shape_idx: u32, dict: VmValue) -> CodecResult {
        let shape = shape(prog, shape_idx)?;
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
                let container = match shape.kind {
                    ShapeKind::Set => vm.heap.alloc(Obj::Set(Box::<SetObj>::default())),
                    _ => vm.heap.new_list(Vec::new()),
                };
                self.push_frame(dict, shape_idx, DecodeKind::Sequence, 1)?;
                return self.set_slot(1, container);
            }
            ShapeKind::Tuple => {
                source.begin_tuple(shape.parts.len())?;
                return self.push_frame(dict, shape_idx, DecodeKind::Tuple, 0);
            }
            ShapeKind::Map => {
                source.begin_map()?;
                let map = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
                self.push_frame(dict, shape_idx, DecodeKind::Map { has_key: false }, 2)?;
                return self.set_slot(1, map);
            }
            ShapeKind::Option => match source.option()? {
                false => VmValue::Tag(prog.ids.none),
                true => {
                    self.push_frame(dict, shape_idx, DecodeKind::Some, 0)?;
                    self.pending = Some(self.state.part_dict(vm, prog, shape_idx, 0, dict)?);
                    return Ok(());
                }
            },
            ShapeKind::Struct(def) => {
                let desc = prog.types[*def as usize].desc();
                source.begin_struct(desc)?;
                let (target, slot_count) = match &shape.init {
                    Some(init) => (SlotTarget::Init(init.names.clone()), init.names.len()),
                    None => (SlotTarget::Struct(*def, desc.clone()), desc.fields.len()),
                };
                return self.push_frame(
                    dict,
                    shape_idx,
                    DecodeKind::Slots { target, seen: vec![false; slot_count], active_slot: 0 },
                    slot_count,
                );
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
                let part_base = (first_variant as u32..id).map(|variant_id| prog.variants[variant_id as usize].arity as u32).sum();
                let form = match (variant.arity, &variant.field_names) {
                    (0, _) => PayloadForm::Unit,
                    (_, Some(_)) => PayloadForm::Named,
                    (arity, None) => PayloadForm::Positional(arity),
                };
                source.begin_payload(form)?;
                if form == PayloadForm::Unit {
                    source.end()?;
                    self.place(vm, VmValue::Tag(id))?;
                    return Ok(());
                }
                let target = SlotTarget::Variant { id, part_base, named: form == PayloadForm::Named };
                let slot_count = variant.arity;
                return self.push_frame(
                    dict,
                    shape_idx,
                    DecodeKind::Slots { target, seen: vec![false; slot_count], active_slot: 0 },
                    slot_count,
                );
            }
        };
        self.place(vm, value)
    }

    /// Advances the innermost open container.
    ///
    /// Reads the next part, fills an omitted slot, or assembles the finished
    /// value.
    fn advance(&mut self, vm: &mut Vm, prog: &Program, source: &mut dyn DecodeSource) -> CodecResult<Option<Step>> {
        let frame = self.frames.last_mut().expect("an open frame");
        let (frame_base, shape_idx) = (frame.base, frame.shape);
        let dict = self.state.values[frame_base];
        let children = self.state.values.len() - frame_base - 1;
        let shape = shape(prog, shape_idx)?;
        if !frame.ended {
            let part = match &mut frame.kind {
                DecodeKind::Sequence => {
                    // Check memory for doubling a full list before advancing `source`
                    // so garbage collection can run safely.
                    if let Obj::List(items) = vm.heap.get(self.state.values[frame_base + 1])
                        && items.len() == items.capacity()
                        && let Some(step) = self.state.memory_check_step(vm.check_memory(2 * items.capacity() * size_of::<VmValue>()))
                    {
                        return step.map(Some);
                    }
                    source.next()?.then_some(0)
                }
                DecodeKind::Tuple => (children < shape.parts.len() && source.next()?).then_some(children as u32),
                DecodeKind::Map { has_key: true } => Some(1),
                DecodeKind::Map { has_key: false } => source.next()?.then_some(0),
                DecodeKind::Some => (children == 0).then_some(0),
                DecodeKind::Slots { target, seen, active_slot } => {
                    let part_base = target.part_base();
                    match target.names(prog) {
                        Some(names) => match source.next_field()? {
                            Some(name) => match match &shape.slot_index {
                                Some(index) if !matches!(target, SlotTarget::Variant { .. }) => {
                                    index.get(&*name).map(|&slot| slot as usize)
                                }
                                _ => field_index(vm, names, &name, *active_slot + usize::from(seen.get(*active_slot) == Some(&true)))?,
                            } {
                                Some(slot) if seen[slot] => {
                                    let name = crate::stdlib::truncate(&name, 64).to_string();
                                    return parse_error(source, format!("duplicate field `{name}`"));
                                }
                                Some(slot) => {
                                    (seen[slot], *active_slot) = (true, slot);
                                    Some(part_base + slot as u32)
                                }
                                None => {
                                    drop(name);
                                    source.skip()?;
                                    return Ok(None);
                                }
                            },
                            None => None,
                        },
                        None => match charge_scan(vm, seen.len()).map(|()| seen.iter().position(|filled| !filled))? {
                            Some(slot) if source.next()? => {
                                (seen[slot], *active_slot) = (true, slot);
                                Some(part_base + slot as u32)
                            }
                            _ => None,
                        },
                    }
                }
            };
            match part {
                Some(part) => {
                    self.pending = Some(self.state.part_dict(vm, prog, shape_idx, part, dict)?);
                    return Ok(None);
                }
                None => frame.ended = true,
            }
        }
        if let DecodeKind::Slots { target, seen, active_slot } = &mut frame.kind
            && let Some(missing) = charge_scan(vm, seen.len()).map(|()| seen.iter().position(|filled| !filled))?
        {
            let part_base = target.part_base();
            let names = target.names(prog);
            let value = match shape.fills.get(missing).cloned().unwrap_or(PartFill::Required) {
                PartFill::Required => {
                    let part_dict = self.state.part_dict(vm, prog, shape_idx, part_base + missing as u32, dict)?;
                    match codec_dispatch(vm, part_dict)? {
                        CodecDispatch::Structural(part_shape) if matches!(shape_kind(vm, part_shape)?, ShapeKind::Option) => {
                            VmValue::Tag(vm.prog.ids.none)
                        }
                        _ => match names {
                            Some(names) => return parse_error(source, format!("missing field `{}`", names[missing])),
                            None => return parse_error(source, format!("expected {} payload values", seen.len())),
                        },
                    }
                }
                PartFill::Const(const_val) => constant(vm, &const_val)?,
                PartFill::Call(helper_const, dicts) => {
                    let mut args = dicts.iter().map(|template| eval_template(vm, template, dict)).collect::<Result<Vec<_>, _>>()?;
                    // Pass preceding parameter values to an `init` parameter default helper.
                    if matches!(target, SlotTarget::Init(_)) {
                        args.extend_from_slice(&self.state.values[frame_base + 1..frame_base + 1 + missing]);
                    }
                    (seen[missing], *active_slot) = (true, missing);
                    return Ok(Some(Step::Call(Callback { callee: vm.consts[helper_const as usize], args, kind: CallbackKind::Default })));
                }
            };
            let frame = self.frames.last_mut().expect("an open frame");
            if let DecodeKind::Slots { seen, active_slot, .. } = &mut frame.kind {
                (seen[missing], *active_slot) = (true, missing);
            }
            self.state.values[frame_base + 1 + missing] = value;
            return Ok(None);
        }
        let children = &self.state.values[frame_base + 1..];
        let value = match &self.frames.last().expect("an open frame").kind {
            DecodeKind::Sequence | DecodeKind::Map { .. } => children[0],
            DecodeKind::Tuple if children.len() != shape.parts.len() => {
                return parse_error(source, format!("expected {} elements, found {}", shape.parts.len(), children.len()));
            }
            DecodeKind::Tuple => vm.heap.new_tuple(children),
            DecodeKind::Some => vm.some(children[0]),
            DecodeKind::Slots { target: SlotTarget::Struct(def, _), .. } => vm.heap.alloc(Obj::Struct(*def, children.into())),
            DecodeKind::Slots { target: SlotTarget::Variant { id, .. }, .. } => vm.heap.new_variant(*id, children),
            DecodeKind::Slots { target: SlotTarget::Init(_), .. } => {
                let Some(init) = &shape.init else { return Err(Fail::internal("decode: an init shape without init").into()) };
                let mut args = init.dicts.iter().map(|template| eval_template(vm, template, dict)).collect::<Result<Vec<_>, _>>()?;
                args.extend_from_slice(children);
                source.end()?;
                return Ok(Some(Step::Call(Callback { callee: vm.consts[init.func as usize], args, kind: CallbackKind::Init })));
            }
        };
        source.end()?;
        self.pop_frame();
        self.place(vm, value)?;
        Ok(None)
    }

    /// Pops the innermost frame. Truncates its temporary stack slots.
    fn pop_frame(&mut self) {
        let frame = self.frames.pop().expect("an open frame");
        self.depth -= usize::from(!matches!(frame.kind, DecodeKind::Some));
        self.state.values.truncate(frame.base);
    }
}

impl Walker for DecodeWalker {
    type IO = dyn DecodeSource;
    type Output = VmValue;
    /// Advances until completion, a memory collection stop, or a callback.
    fn run(&mut self, vm: &mut Vm, shared: &SharedDecode) -> CodecResult<Step> {
        let prog = vm.prog.clone();
        let mut shared_guard = shared.lock();
        loop {
            if let Some(step) = self.state.check_memory(vm, &mut self.unchecked_parts) {
                return step;
            }
            vm.charge(1 + (shared_guard.io.take_consumed_bytes() / SLOW_BYTES_PER_UNIT) as u64)?;
            if let Some(dict) = self.pending.take() {
                match codec_dispatch(vm, dict)? {
                    CodecDispatch::Custom(callee) => {
                        let token = shared_guard.turn.next_token;
                        // Pre-step memory check covers allocating `Decoder`.
                        let decoder = vm.unchecked(|vm| (Decoder { shared: Arc::downgrade(shared), token }).into_vm_value(vm))?;
                        self.callback = Some(shared_guard.turn.begin());
                        let args = vec![dict, decoder];
                        // A native method reads through the `Decoder`, which locks `shared`.
                        drop(shared_guard);
                        let Some(result) = call_native(vm, callee, &args)? else {
                            return Ok(Step::Call(Callback { callee, args, kind: CallbackKind::Method }));
                        };
                        self.apply(vm, shared, CallbackKind::Method, result)?;
                        shared_guard = shared.lock();
                    }
                    CodecDispatch::Structural(shape_idx) => self.visit(vm, &prog, &mut shared_guard.io, shape_idx, dict)?,
                }
                self.state.collected = false;
                continue;
            }
            if self.frames.is_empty() {
                return Ok(Step::Done);
            }
            if let Some(step) = self.advance(vm, &prog, &mut shared_guard.io)? {
                return Ok(step);
            }
            self.state.collected = false;
        }
    }

    /// Stores the result of a completed [`Callback`].
    fn apply(&mut self, vm: &mut Vm, shared: &SharedDecode, kind: CallbackKind, result: VmValue) -> CodecResult {
        let value = match kind {
            CallbackKind::Method => {
                let Some((token, saved)) = self.callback.take() else { return Err(Fail::internal("decode: no callback").into()) };
                let mut shared_guard = shared.lock();
                let unread = shared_guard.turn.end(token, saved, "a `decode` method returned before its read ended")?;
                if let Some(err_val) = err_payload(vm, result) {
                    return Err(with_source_path(host_error_from_sand(vm, err_val)?, &shared_guard.io));
                }
                if unread {
                    shared_guard.io.skip()?;
                }
                vm.heap.payload_if(&result, vm.prog.ids.ok)
            }
            CallbackKind::Default => result,
            CallbackKind::Init => {
                if let Some(err_val) = err_payload(vm, result) {
                    return Err(with_source_path(host_error_from_sand(vm, err_val)?, &shared.lock().io));
                }
                match vm.heap.variant_id(result) {
                    id if id == vm.prog.ids.ok => vm.heap.payload_if(&result, id),
                    _ => result,
                }
            }
        };
        if kind == CallbackKind::Init {
            self.pop_frame();
        }
        self.place(vm, value)
    }

    fn output(&self) -> Result<VmValue, Fail> {
        self.result()
    }

    fn state(&mut self) -> &mut WalkState {
        &mut self.state
    }
}

fn shape_kind(vm: &Vm, shape_idx: u32) -> Result<&ShapeKind, Fail> {
    Ok(&shape(&vm.prog, shape_idx)?.kind)
}

/// Returns the [`ShapeKind`] of `dict` if `dict` is structural.
pub(crate) fn structural_kind(vm: &Vm, dict: VmValue) -> Result<Option<&ShapeKind>, Fail> {
    match codec_dispatch(vm, dict)? {
        CodecDispatch::Structural(shape_idx) => Ok(Some(shape_kind(vm, shape_idx)?)),
        CodecDispatch::Custom(_) => Ok(None),
    }
}

/// Charges fuel proportional to `count` for a linear scan of fields or
/// variants.
fn charge_scan(vm: &Vm, count: usize) -> Result<(), Fail> {
    vm.charge((count / 16) as u64)
}

/// Returns the index of `name` in `names`.
///
/// Checks `guess` first. Charges fuel only if it falls back to a linear scan.
fn field_index(vm: &Vm, names: &[String], name: &str, guess: usize) -> Result<Option<usize>, Fail> {
    if names.get(guess).is_some_and(|candidate| candidate == name) {
        return Ok(Some(guess));
    }
    charge_scan(vm, names.len())?;
    Ok(names.iter().position(|candidate| candidate == name))
}

/// Allocates a runtime [`VmValue`] from compile-time `const_val`.
pub(crate) fn constant(vm: &mut Vm, const_val: &ConstVal) -> Result<VmValue, Fail> {
    vm.charge(1)?;
    let alloc_items = |vm: &mut Vm, items: &[ConstVal]| items.iter().map(|item| constant(vm, item)).collect::<Result<Vec<_>, _>>();
    Ok(match const_val {
        ConstVal::Unit => VmValue::Unit,
        ConstVal::Bool(flag) => VmValue::Bool(*flag),
        ConstVal::Int(num) => VmValue::Int(*num),
        ConstVal::Float(num) => VmValue::Float(*num),
        ConstVal::Dur(nanos) => VmValue::Dur(*nanos),
        ConstVal::Str(text) => {
            vm.charge_bytes(text.len())?;
            vm.heap.new_str(text.as_str())
        }
        ConstVal::Bytes(bytes) => {
            vm.charge_bytes(bytes.len())?;
            vm.heap.new_bytes(bytes.as_slice())
        }
        ConstVal::Range(start, end, inclusive) => vm.heap.alloc(Obj::Range(*start, *end, *inclusive)),
        ConstVal::Tuple(items) => {
            let items = alloc_items(vm, items)?;
            vm.heap.new_tuple(&items)
        }
        ConstVal::List(items) => {
            let items = alloc_items(vm, items)?;
            vm.heap.new_list(items)
        }
        ConstVal::Set(items) => {
            let items = alloc_items(vm, items)?;
            let set = vm.heap.alloc(Obj::Set(Box::<SetObj>::default()));
            for item in items {
                vm.set_add(set, item)?;
            }
            set
        }
        ConstVal::Map(entries) => {
            let map = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
            for (key, value) in entries {
                let (key, value) = (constant(vm, key)?, constant(vm, value)?);
                vm.map_insert(map, key, value)?;
            }
            map
        }
        ConstVal::Variant(id, payload) => {
            let payload = alloc_items(vm, payload)?;
            vm.heap.new_variant(*id, &payload)
        }
        ConstVal::Struct(def, fields) => {
            let fields = alloc_items(vm, fields)?;
            vm.heap.alloc(Obj::Struct(*def, fields.into_boxed_slice()))
        }
    })
}

/// Error conversion for native functions that drive a codec walk.
///
/// [`HostError`] converts recoverable errors into Sand `Err` values. [`Fail`]
/// aborts execution.
pub(crate) trait WalkError: NativeError {
    fn from_walk(err: HostError<Fail>) -> Self;
    fn from_future_walk(err: HostError<Fail>) -> Self::FutureError;
}

impl WalkError for HostError {
    fn from_walk(err: HostError<Fail>) -> Self {
        err.into_ctl_error()
    }

    fn from_future_walk(err: HostError<Fail>) -> HostError<Fail> {
        err
    }
}

impl WalkError for Fail {
    fn from_walk(err: HostError<Fail>) -> Self {
        walk_error_to_fail(err)
    }

    fn from_future_walk(err: HostError<Fail>) -> Fail {
        walk_error_to_fail(err)
    }
}

/// Common interface implemented by [`EncodeWalker`] and [`DecodeWalker`].
trait Walker: Send + 'static {
    /// Underlying sink or source trait object.
    type IO: ?Sized + Send;
    /// Final output produced once the walk completes.
    type Output: Send;
    fn run(&mut self, vm: &mut Vm, shared: &Arc<Mutex<Shared<Self::IO>>>) -> CodecResult<Step>;
    /// Applies the return value of a completed [`Callback`].
    fn apply(&mut self, vm: &mut Vm, shared: &Arc<Mutex<Shared<Self::IO>>>, kind: CallbackKind, result: VmValue) -> CodecResult;
    fn output(&self) -> Result<Self::Output, Fail>;
    fn state(&mut self) -> &mut WalkState;
}

/// Drives `walker` asynchronously across callbacks and garbage collections.
async fn finish_walk<W: Walker>(
    walker: &mut W,
    context: &mut NativeContext,
    shared: &Arc<Mutex<Shared<W::IO>>>,
    first: Step,
) -> CodecResult<W::Output> {
    let mut step = first;
    loop {
        match step {
            Step::Done => return Ok(walker.output()?),
            Step::Call(callback) => {
                let result = context.call_with_roots(callback.callee, &callback.args, walker.state().roots()).await;
                context.with_vm(|vm| walker.apply(vm, shared, callback.kind, result))?;
                walker.state().collected = false;
            }
            Step::Collect(_) => walker.state().collect(context).await,
        }
        step = context.with_vm(|vm| walker.run(vm, shared))?;
    }
}

/// Runs `walker` synchronously if possible. Spawns an async continuation if a
/// callback is needed.
fn drive_walk<W, S, T, E>(
    vm: &mut Vm,
    shared_io: Arc<Mutex<Shared<S>>>,
    erased_io: Arc<Mutex<Shared<W::IO>>>,
    mut walker: W,
    finish: impl FnOnce(&mut Vm, W::Output, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    W: Walker,
    S: Send + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    match walker.run(vm, &erased_io).map_err(E::from_walk)? {
        Step::Done => {
            let output = walker.output().map_err(|err| E::from_walk(err.into()))?;
            Ok(Later::ready(finish(vm, output, &mut shared_io.lock().io).map_err(E::from_future_walk)))
        }
        Step::Collect(err) => Err(E::from_walk(err.into())),
        step => Ok(Later::new(WaitKind::Io, |mut context| async move {
            let output = finish_walk(&mut walker, &mut context, &erased_io, step).await.map_err(E::from_future_walk)?;
            context.with_vm(|vm| finish(vm, output, &mut shared_io.lock().io)).map_err(E::from_future_walk)
        })),
    }
}

/// Encodes `value` with `dict` into `shared`. Passes the finished sink to
/// `finish`.
pub(crate) fn encode<S, T, E>(
    vm: &mut Vm,
    shared: Arc<Mutex<Shared<S>>>,
    value: VmValue,
    dict: VmValue,
    finish: impl FnOnce(&mut Vm, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    S: EncodeSink + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    if let Some(plan) = plan::plan_of(vm, dict).map_err(|err| E::from_walk(err.into()))? {
        let prog = vm.prog.clone();
        plan::PlanWalk::new(&plan, &prog).encode_root(vm, &mut shared.lock().io, value).map_err(E::from_walk)?;
        return Ok(Later::ready(finish(vm, &mut shared.lock().io).map_err(E::from_future_walk)));
    }
    let erased_io: SharedEncode = shared.clone();
    drive_walk(vm, shared, erased_io, EncodeWalker::new(value, dict), |vm, (), sink| finish(vm, sink))
}

/// Decodes a value with `dict` from `shared`. Passes the decoded value and
/// source to `finish`.
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
    if let Some(plan) = plan::plan_of(vm, dict).map_err(|err| E::from_walk(err.into()))? {
        let prog = vm.prog.clone();
        let mut shared_guard = shared.lock();
        let source = &mut shared_guard.io;
        let value = plan::PlanWalk::new(&plan, &prog).decode_root(vm, source).map_err(E::from_walk)?;
        vm.charge((source.take_consumed_bytes() / SLOW_BYTES_PER_UNIT) as u64).map_err(|err| E::from_walk(err.into()))?;
        return Ok(Later::ready(finish(vm, value, source).map_err(E::from_future_walk)));
    }
    let erased_io: SharedDecode = shared.clone();
    drive_walk(vm, shared, erased_io, DecodeWalker::new(dict), finish)
}

/// Runs `walker` to completion inside an async native call.
async fn run_async<W: Walker>(context: &mut NativeContext, shared: &Arc<Mutex<Shared<W::IO>>>, mut walker: W) -> CodecResult<W::Output> {
    let first = context.with_vm(|vm| walker.run(vm, shared))?;
    finish_walk(&mut walker, context, shared, first).await
}

/// Encodes `value` with `dict` into a host [`Value`](crate::value::Value).
pub(crate) async fn to_value(context: &mut NativeContext, value: VmValue, dict: VmValue) -> CodecResult<crate::value::Value> {
    let memory = context.with_vm(|vm| vm.heap.io_memory.clone());
    let shared = Shared::new(ValueSink::new(&memory));
    let erased_io: SharedEncode = shared.clone();
    run_async(context, &erased_io, EncodeWalker::new(value, dict)).await?;
    let root = shared.lock().io.take();
    root.map(|(value, _)| value).ok_or_else(|| Fail::internal("to_value: no value").into())
}

/// Decodes host `value` with `dict` into a [`VmValue`].
///
/// The returned value remains rooted until the next wait.
pub(crate) async fn from_value(context: &mut NativeContext, value: crate::value::Value, dict: VmValue) -> CodecResult<VmValue> {
    let shared: SharedDecode = Shared::new(crate::value::ValueSource::new(value));
    run_async(context, &shared, DecodeWalker::new(dict)).await
}

/// Error messages for inactive or duplicate `Encoder` writes.
const ENCODER_MISUSE: [&str; 2] =
    ["an `Encoder` accepts a value only during its `encode` call", "an `encode` method wrote a second value to its `Encoder`"];

/// Error messages for inactive or duplicate `Decoder` reads.
const DECODER_MISUSE: [&str; 2] =
    ["a `Decoder` gives a value only during its `decode` call", "a `decode` method read a second value from its `Decoder`"];

/// Claims the single operation of `token` on `shared`.
fn claim<IO: ?Sized>(
    shared: Option<Arc<Mutex<Shared<IO>>>>,
    token: u64,
    [inactive, second]: [&str; 2],
) -> Result<Arc<Mutex<Shared<IO>>>, HostError> {
    let Some(shared) = shared else { return Err(HostError::new("invalid", inactive)) };
    shared.lock().turn.claim(token, inactive, second)?;
    Ok(shared)
}

/// Ends an operation that [`claim`] started.
///
/// Records a recoverable failure in `Turn`, so that the outer walk fails even
/// if the custom method ignores the returned `Result`.
fn release<IO: ?Sized, T>(shared: &Mutex<Shared<IO>>, result: &CodecResult<T>) {
    let mut shared_guard = shared.lock();
    shared_guard.turn.in_flight -= 1;
    if let Err(err) = result {
        shared_guard.turn.record_error(err);
    }
}

/// Claims `token` on `shared`. Runs nested `walker` asynchronously.
fn start_nested_walk<W: Walker, T: IntoVmValue + 'static>(
    shared: Option<Arc<Mutex<Shared<W::IO>>>>,
    token: u64,
    messages: [&str; 2],
    walker: W,
    output: impl FnOnce(W::Output) -> T + Send + 'static,
) -> Result<Later<T, HostError>, HostError> {
    let shared = claim(shared, token, messages)?;
    Ok(Later::new(WaitKind::Io, |mut context| async move {
        let result = run_async(&mut context, &shared, walker).await;
        release(&shared, &result);
        result.map(output)
    }))
}

/// Encodes `value` through `encoder`.
fn encode_with_encoder(vm: &mut Vm, dict: VmValue, value: VmValue, encoder: VmVal<Encoder>) -> Result<Later<(), HostError>, HostError> {
    let encoder_ref = encoder.get(&vm.heap)?;
    start_nested_walk(encoder_ref.shared.upgrade(), encoder_ref.token, ENCODER_MISUSE, EncodeWalker::new(value, dict), |()| ())
}

/// Decodes a value from `decoder`.
fn decode_with_decoder(vm: &mut Vm, dict: VmValue, decoder: VmVal<Decoder>) -> Result<Later<Param<0>, HostError>, HostError> {
    let decoder_ref = decoder.get(&vm.heap)?;
    start_nested_walk(decoder_ref.shared.upgrade(), decoder_ref.token, DECODER_MISUSE, DecodeWalker::new(dict), Param::from_raw)
}

/// Registers prelude types and native functions for `Encode` and `Decode`.
pub(crate) fn register(module: &mut crate::native::NativeModule) {
    module.bind::<Encoder>().bind::<Decoder>().bind::<InputKind>().bind::<EncodedArg>();
    module.function("encode_arg", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, value: Param<0>| {
        encode::<_, _, Fail>(vm, Shared::new(ValueSink::new(&vm.heap.io_memory)), value.raw(), dict.raw(), |vm, sink| {
            let (value, reservation) = sink.take().ok_or_else(|| Fail::internal("encode_arg: no value"))?;
            Ok(VmVal::<EncodedArg>::from_raw(EncodedArg { value: Arc::new(value), _reservation: reservation }.into_vm_value(vm)?))
        })
    });
    module.function("Encode.encode", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, value: Param<0>, encoder: VmVal<Encoder>| {
        encode_with_encoder(vm, dict.raw(), value.raw(), encoder)
    });
    module.function("Encoder.encode", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, encoder: VmVal<Encoder>, value: Param<0>| {
        encode_with_encoder(vm, dict.raw(), value.raw(), encoder)
    });
    module.function("Decode.decode", |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        decode_with_decoder(vm, dict.raw(), decoder)
    });
    module.function("Decoder.decode", |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        decode_with_decoder(vm, dict.raw(), decoder)
    });
    module.function("Decoder.kind", |vm: &mut Vm, decoder: VmVal<Decoder>| -> Result<InputKind, HostError> {
        let decoder_ref = decoder.get(&vm.heap)?;
        let Some(shared) = decoder_ref.shared.upgrade() else { return Err(HostError::new("invalid", DECODER_MISUSE[0])) };
        let mut shared_guard = shared.lock();
        if shared_guard.turn.active != decoder_ref.token {
            return Err(shared_guard.turn.misuse(DECODER_MISUSE[0]));
        }
        if shared_guard.turn.used {
            return Err(shared_guard.turn.misuse("a `decode` method asked for the kind of a value that it read"));
        }
        shared_guard.io.kind().map_err(HostError::into_ctl_error)
    });
}
