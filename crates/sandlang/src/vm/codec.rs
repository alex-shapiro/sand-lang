//! Iterative walkers for the `Encode` and `Decode` traits.
//!
//! Each walker processes a value part by part. For a structural
//! implementation, the walker derives each part's dictionary from the outer
//! dictionary using its [`CodecShape`]. It then processes the part in Rust. If
//! a part uses a custom trait method, an `init` constructor, or a computed
//! default, the walker yields a [`Callback`] to the scheduler.
//!
//! [`WalkState`] stores every live heap value so the caller can root them
//! across callbacks and garbage collections. Before visiting each part, the
//! walker checks the memory limit. If that check fails, the walker returns
//! [`Step::Collect`] and resumes after garbage collection. A failure after a
//! part mutates the sink or source aborts the walk. Only an initial
//! synchronous pass before any suspension can return a retryable limit error
//! to the VM.

use super::bytecode::{CODEC_SHAPE_SLOT, CodecShape, DecodeFill, PartTemplate, Program, ShapeKind};
use super::meter::SLOW_BYTES_PER_UNIT;
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
use std::sync::{Arc, Weak};

/// Heap byte estimate reserved per visited value.
pub(crate) const NODE_BYTES: usize = 64;

/// Number of visited parts per amortized memory limit check.
const CHECKED_VALUES: u32 = 32;

/// Maximum container nesting depth for encoding and decoding.
///
/// Enforcing this limit also halts cyclic values.
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
    /// Custom `encode` or `decode` method returning `Result`.
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
    /// Pre-step memory check failed. Collect garbage before resuming.
    Collect(Fail),
}

/// GC-rooted heap state shared by [`EncodeWalker`] and [`DecodeWalker`].
#[derive(Default)]
pub(crate) struct WalkState {
    /// Live heap values on the walker stack.
    values: Vec<VmValue>,
    /// `true` if garbage collection ran since the walker last completed a part.
    collected: bool,
    /// Memoized part dictionaries keyed by `(shape_idx, part, whole_handle)`.
    cache: FxHashMap<(u32, u32, u32), VmValue>,
    cached: Vec<VmValue>,
}

impl WalkState {
    fn new(values: Vec<VmValue>) -> WalkState {
        WalkState { values, ..WalkState::default() }
    }

    /// Checks the memory limit every [`CHECKED_VALUES`] parts and advances
    /// `unchecked`.
    ///
    /// Returns `Some` if the check stops the current pass.
    fn check_memory(&mut self, vm: &Vm, unchecked: &mut u32) -> Option<CodecResult<Step>> {
        let check = if *unchecked == 0 || vm.heap.fail_next_check.get() {
            let held = self.values.capacity() * size_of::<VmValue>();
            vm.check_memory(CHECKED_VALUES as usize * NODE_BYTES + held)
        } else {
            Ok(())
        };
        let step = self.checked(check);
        if step.is_none() {
            *unchecked = (*unchecked + 1) % CHECKED_VALUES;
        }
        step
    }

    /// Converts a pre-step memory check error into [`Step::Collect`] on the
    /// first failure or a terminal error if a collection just ran.
    fn checked(&mut self, check: Result<(), Fail>) -> Option<CodecResult<Step>> {
        match check {
            Ok(()) => None,
            Err(err) if !self.collected && err.as_limit() == Some(LimitKind::Memory) => Some(Ok(Step::Collect(err))),
            Err(err) => Some(Err(err.into())),
        }
    }

    /// Resolves the dictionary for `part` of `shape_idx` from `whole`.
    fn part_dict(&mut self, vm: &mut Vm, prog: &Program, shape_idx: u32, part: u32, whole: VmValue) -> Result<VmValue, Fail> {
        let Some(template) = shape(prog, shape_idx)?.parts.get(part as usize) else {
            return Err(Fail::internal("a codec shape has too few parts"));
        };
        match template {
            PartTemplate::Const(k) => Ok(vm.consts[*k as usize]),
            PartTemplate::Bound(i) => dict_slot(vm, whole, *i),
            _ => {
                let key = (shape_idx, part, handle(whole));
                if let Some(&dict) = self.cache.get(&key) {
                    return Ok(dict);
                }
                let dict = eval_template(vm, template, whole)?;
                self.cache.insert(key, dict);
                self.cached.push(dict);
                Ok(dict)
            }
        }
    }

    /// Borrows the two root vectors held by this walk.
    fn roots(&mut self) -> [&mut Vec<VmValue>; 2] {
        [&mut self.values, &mut self.cached]
    }

    /// Runs garbage collection while keeping all walk values rooted.
    async fn collect(&mut self, context: &mut NativeContext) {
        context.collect_keeping(self.roots()).await;
        self.collected = true;
    }
}

fn handle(value: VmValue) -> u32 {
    match value {
        VmValue::Obj(h) => h,
        _ => u32::MAX,
    }
}

fn dict_slot(vm: &Vm, dict: VmValue, slot: u32) -> Result<VmValue, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => items.get(slot as usize).copied().ok_or_else(|| Fail::internal("a codec dictionary has too few slots")),
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

/// Evaluates `template` against outer dictionary `whole`.
///
/// Each walker pass pre-checks enough heap headroom to cover these tuple
/// allocations.
fn eval_template(vm: &mut Vm, template: &PartTemplate, whole: VmValue) -> Result<VmValue, Fail> {
    Ok(match template {
        PartTemplate::Const(k) => vm.consts[*k as usize],
        PartTemplate::Bound(i) => dict_slot(vm, whole, *i)?,
        PartTemplate::Field(inner, i) => {
            let dict = eval_template(vm, inner, whole)?;
            dict_slot(vm, dict, *i)?
        }
        PartTemplate::Dict(items) => {
            vm.charge(1)?;
            let items = items.iter().map(|inner| eval_template(vm, inner, whole)).collect::<Result<Vec<_>, _>>()?;
            vm.heap.alloc(Obj::Tuple(items.into_boxed_slice()))
        }
    })
}

/// Dispatch target extracted from a codec dictionary.
enum Method {
    /// Structural walker using the given [`CodecShape`] index.
    Structural(u32),
    /// Custom Sand method closure or native function.
    Custom(VmValue),
}

fn method(vm: &Vm, dict: VmValue) -> Result<Method, Fail> {
    match vm.heap.get(dict) {
        Obj::Tuple(items) => match (items.first(), items.get(CODEC_SHAPE_SLOT)) {
            (_, Some(&VmValue::Int(shape_idx))) => Ok(Method::Structural(shape_idx as u32)),
            (Some(&callee), _) => Ok(Method::Custom(callee)),
            (None, _) => Err(Fail::internal("a codec dictionary is empty")),
        },
        _ => Err(Fail::internal("a codec dictionary is not a tuple")),
    }
}

/// Looks up `shape_idx` in `prog.codec_shapes`.
///
/// Validates the index because host callers can supply arbitrary dictionary
/// tuples.
fn shape(prog: &Program, shape_idx: u32) -> Result<&CodecShape, Fail> {
    prog.codec_shapes.get(shape_idx as usize).ok_or_else(|| Fail::internal("unknown codec shape"))
}

/// Returns a `"limit"` error for values nested deeper than
/// [`MAX_NESTING_DEPTH`].
pub(crate) fn too_deep<T>() -> CodecResult<T> {
    Err(HostError::new("limit", "value too deeply nested to encode or decode"))
}

/// Prefixes recoverable callback error `err` with the current path of `source`.
fn at_path(err: HostError<Fail>, source: &dyn DecodeSource) -> HostError<Fail> {
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
/// Preserves the kind and message of a `prelude.Error` struct and formats any
/// other error value with kind `"invalid"`.
fn sand_error(vm: &Vm, err_val: VmValue) -> Result<HostError<Fail>, Fail> {
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
pub(crate) fn abort(err: HostError<Fail>) -> Fail {
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
    /// Token of the currently active handle, or `0` if none is active.
    active: u64,
    /// `true` once the active handle has consumed its single operation.
    used: bool,
    next_token: u64,
    /// Count of in-flight nested writes or reads.
    ///
    /// A spawned task can keep an operation in flight after its custom method
    /// returns.
    open: u32,
    /// Saved `(kind, message)` from a failed nested operation.
    failed: Option<(String, String)>,
}

/// Snapshot of [`Turn`] saved across a nested custom method callback.
#[derive(Clone, Copy)]
struct SavedTurn {
    active: u64,
    used: bool,
    open: u32,
}

impl Turn {
    /// Activates a fresh handle token and returns the interrupted
    /// [`SavedTurn`].
    fn begin(&mut self) -> (u64, SavedTurn) {
        let token = self.next_token;
        self.next_token += 1;
        let saved = SavedTurn { active: self.active, used: self.used, open: self.open };
        (self.active, self.used) = (token, false);
        (token, saved)
    }

    /// Claims the single operation of `token`, or fails with `inactive` or
    /// `second`.
    fn claim(&mut self, token: u64, inactive: &str, second: &str) -> Result<(), HostError> {
        if self.active != token {
            return Err(self.misuse(inactive));
        }
        if self.used {
            return Err(self.misuse(second));
        }
        self.used = true;
        self.open += 1;
        Ok(())
    }

    /// Restores `saved` after `token`'s callback finishes.
    ///
    /// Returns `Ok(true)` if `token` was never used. Propagates any recorded
    /// nested error, or fails with `unfinished` if a nested operation is still
    /// in flight.
    fn end(&mut self, token: u64, saved: SavedTurn, unfinished: &str) -> CodecResult<bool> {
        let unused = self.active == token && !self.used;
        let open = self.open > saved.open;
        (self.active, self.used) = (saved.active, saved.used);
        if let Some((kind, message)) = self.failed.take() {
            return Err(HostError::new(kind, message));
        }
        if open {
            return Err(HostError::new("invalid", unfinished));
        }
        Ok(unused)
    }

    /// Saves a recoverable nested error so the outer walk fails even if the
    /// custom method discards the `Result`.
    fn record(&mut self, err: &HostError<Fail>) {
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
        let turn = Turn { active: 0, used: false, next_token: 1, open: 0, failed: None };
        Arc::new(Mutex::new(Shared { turn, io }))
    }
}

/// Shared state for an active encode walk.
pub(crate) type EncodeCell = Arc<Mutex<Shared<dyn EncodeSink>>>;

/// Shared state for an active decode walk.
pub(crate) type DecodeCell = Arc<Mutex<Shared<dyn DecodeSource>>>;

/// Handle that writes the single value of a custom `encode` method.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub(crate) struct Encoder {
    /// Weak reference so retaining an `Encoder` does not retain the sink.
    shared: Weak<Mutex<Shared<dyn EncodeSink>>>,
    token: u64,
}

impl crate::native::NativeObject for Encoder {}

/// Handle that reads the single value of a custom `decode` method.
#[derive(crate::SandObject)]
#[sand(module = "prelude")]
pub(crate) struct Decoder {
    /// Weak reference so retaining a `Decoder` does not retain the source.
    shared: Weak<Mutex<Shared<dyn DecodeSource>>>,
    token: u64,
}

impl crate::native::NativeObject for Decoder {}

/// Open container frame on the [`EncodeWalker`] stack.
struct EncodeFrame {
    /// Start index of this frame's slots in [`WalkState::values`].
    ///
    /// Slot `base` holds the container and `base + 1` holds its dictionary.
    /// Set and map frames also store the iterator at `base + 2` and the
    /// pending map value at `base + 3`.
    base: usize,
    shape: u32,
    kind: EncodeKind,
    /// Index of the next child to visit.
    next: usize,
    /// Initial list length snapshot.
    ///
    /// Matching `for` loop semantics, the walker stops at this bound even if a
    /// callback appends to the list.
    bound: usize,
}

/// Container kind for an [`EncodeFrame`].
///
/// Sets and maps iterate through `Vm::container_iter` snapshots so mutations
/// during callbacks do not corrupt traversal.
#[derive(Clone, Copy)]
enum EncodeKind {
    /// List elements sharing part 0.
    List,
    /// Set elements sharing part 0.
    Set,
    /// Tuple elements or struct fields with one part per index.
    Fields,
    /// Map entries alternating between keys at even indices and values at odd
    /// indices.
    Map,
    /// Variant payload values starting at `base` in `CodecShape::parts`.
    Payload { base: u32 },
    /// Payload of `Option.Some`.
    Some,
}

/// Iterative walker that encodes a Sand value into an [`EncodeSink`].
pub(crate) struct EncodeWalker {
    state: WalkState,
    /// Parts visited since the previous memory limit check.
    unchecked: u32,
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
        EncodeWalker { state, unchecked: 0, frames: Vec::new(), depth: 0, pending: Some((value, dict)), callback: None }
    }

    #[inline(always)]
    fn push_frame(&mut self, vm: &mut Vm, value: VmValue, dict: VmValue, shape: u32, kind: EncodeKind, bound: usize) -> CodecResult {
        if !matches!(kind, EncodeKind::Some) {
            if self.depth >= MAX_NESTING_DEPTH {
                return too_deep();
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
        self.frames.push(EncodeFrame { base, shape, kind, next: 0, bound });
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
            (ShapeKind::ManualEncode(encode_fn), _) => encode_fn(vm, value, sink)?,
            (ShapeKind::List, _) => {
                let len = vm.heap.list(value).len();
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::List, len)?;
                sink.begin_list(len)?;
            }
            (ShapeKind::Set, _) => {
                let Obj::Set(set) = vm.heap.get(value) else { return wrong() };
                let live = set.live;
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Set, 0)?;
                sink.begin_set(live)?;
            }
            (ShapeKind::Map, _) => {
                let Obj::Map(map) = vm.heap.get(value) else { return wrong() };
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
                let Some(desc) = prog.variant_desc(id) else { return wrong() };
                let variant = &prog.variants[id as usize];
                sink.begin_variant(desc, variant.arity)?;
                if variant.arity == 0 {
                    return sink.end();
                }
                charge_scan(vm, variant.index as usize)?;
                let base = (id - variant.index..id).map(|v| prog.variants[v as usize].arity as u32).sum();
                self.push_frame(vm, value, dict, shape_idx, EncodeKind::Payload { base }, 0)?;
            }
            _ => wrong()?,
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
                    items[..frame.bound.min(items.len())].get(next).map(|&v| (v, 0, next + 1))
                }
                EncodeKind::Set => {
                    let mut element = VmValue::Unit;
                    vm.set_step(self.state.values[base + 2], &mut element).then_some((element, 0, next + 1))
                }
                EncodeKind::Fields => field(vm, container, next)?.map(|v| (v, next as u32, next + 1)),
                EncodeKind::Map if next % 2 == 1 => Some((self.state.values[base + 3], 1, next + 1)),
                EncodeKind::Map => {
                    let mut entry = [VmValue::Unit; 2];
                    let found = vm.map_step(self.state.values[base + 2], &mut entry);
                    self.state.values[base + 3] = entry[1];
                    found.then_some((entry[0], 0, next + 1))
                }
                EncodeKind::Some => (frame.next == 0).then(|| (vm.heap.payload_if(&container, vm.prog.ids.some), 0, 1)),
                EncodeKind::Payload { base: part_base } => {
                    payload(vm, container, frame.next).map(|v| (v, part_base + frame.next as u32, frame.next + 1))
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
    fn run(&mut self, vm: &mut Vm, shared: &EncodeCell) -> CodecResult<Step> {
        let prog = vm.prog.clone();
        let mut shared_guard = shared.lock();
        // Reset `unchecked` because callbacks can allocate on the heap.
        self.unchecked = 0;
        loop {
            if let Some(step) = self.state.check_memory(vm, &mut self.unchecked) {
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
            match method(vm, dict)? {
                Method::Custom(callee) => {
                    let token = shared_guard.turn.next_token;
                    // Allocate `Encoder` without a limit check because `next_part`
                    // already advanced the frame.
                    let encoder = vm.unchecked(|vm| (Encoder { shared: Arc::downgrade(shared), token }).into_vm_value(vm))?;
                    self.callback = Some(shared_guard.turn.begin());
                    self.pending = None;
                    return Ok(Step::Call(Callback { callee, args: vec![dict, value, encoder], kind: CallbackKind::Method }));
                }
                Method::Structural(shape_idx) => {
                    self.pending = None;
                    self.visit(vm, &prog, &mut shared_guard.io, shape_idx, value, dict)?;
                    self.state.collected = false;
                }
            }
        }
    }

    /// Validates completion of a custom `encode` callback.
    fn apply(&mut self, vm: &mut Vm, shared: &EncodeCell, _: CallbackKind, result: VmValue) -> CodecResult {
        let Some((token, saved)) = self.callback.take() else { return Err(Fail::internal("encode: no callback").into()) };
        let unused = shared.lock().turn.end(token, saved, "an `encode` method returned before its write ended")?;
        if let Some(err_val) = err_payload(vm, result) {
            return Err(sand_error(vm, err_val)?);
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
        VmValue::Pair(a, b) => [VmValue::Int(i64::from(a)), VmValue::Int(b)].get(index).copied(),
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
/// Slot `base` in [`WalkState::values`] holds the container's dictionary,
/// followed by its decoded child slots.
struct DecodeFrame {
    base: usize,
    shape: u32,
    kind: DecodeKind,
    /// `true` once the source reports the end of this container.
    ended: bool,
}

enum DecodeKind {
    /// List or set stored at `base + 1` and populated incrementally.
    Seq,
    Tuple,
    /// Map stored at `base + 1` with any pending key stored at `base + 2`.
    Map {
        /// `true` if `base + 2` holds a decoded key awaiting its value.
        key: bool,
    },
    Some,
    /// Pre-allocated slots for struct fields, `init` parameters, or variant
    /// payloads.
    Slots {
        target: SlotTarget,
        /// Marks which slots have received values.
        seen: Vec<bool>,
        /// Slot index receiving the current decoded value.
        next: usize,
    },
}

/// Destination constructed from [`DecodeKind::Slots`].
enum SlotTarget {
    /// Struct `def` with its descriptor.
    Struct(u32, Arc<StructDesc>),
    /// Parameters of a struct `init` constructor.
    Init(Arc<[String]>),
    /// Variant `id` whose payload parts start at `base` in
    /// `CodecShape::parts`.
    Variant { id: u32, base: u32, named: bool },
}

impl SlotTarget {
    /// Returns field or parameter names, or `None` for positional payloads.
    fn names<'a>(&'a self, prog: &'a Program) -> Option<&'a [String]> {
        match self {
            SlotTarget::Struct(_, desc) => Some(&desc.fields),
            SlotTarget::Init(names) => Some(names),
            SlotTarget::Variant { id, named: true, .. } => prog.variants[*id as usize].field_names.as_deref(),
            SlotTarget::Variant { .. } => None,
        }
    }

    /// Returns the starting index in `CodecShape::parts` for slot 0.
    fn base(&self) -> u32 {
        match self {
            SlotTarget::Variant { base, .. } => *base,
            _ => 0,
        }
    }
}

/// Iterative walker that decodes a Sand value from a [`DecodeSource`].
pub(crate) struct DecodeWalker {
    state: WalkState,
    /// Parts visited since the previous memory limit check.
    unchecked: u32,
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
        DecodeWalker { state: WalkState::new(vec![dict]), unchecked: 0, frames: Vec::new(), depth: 0, pending: Some(dict), callback: None }
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
            DecodeKind::Slots { next, .. } => self.state.values[base + 1 + *next] = value,
            DecodeKind::Seq => {
                let container = self.state.values[base + 1];
                match vm.heap.get(container) {
                    Obj::List(_) => vm.heap.list_push(container, value).map_err(Fail::limit)?,
                    _ => _ = vm.set_add(container, value)?,
                }
            }
            DecodeKind::Map { key: key @ false } => {
                *key = true;
                self.state.values[base + 2] = value;
            }
            DecodeKind::Map { key } => {
                *key = false;
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
                return too_deep();
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
            ShapeKind::ManualDecode(decode_fn) => decode_fn(vm, source)?,
            ShapeKind::List | ShapeKind::Set => {
                source.begin_list()?;
                let container = match shape.kind {
                    ShapeKind::Set => vm.heap.alloc(Obj::Set(Box::<SetObj>::default())),
                    _ => vm.heap.new_list(Vec::new()),
                };
                self.push_frame(dict, shape_idx, DecodeKind::Seq, 1)?;
                return self.set_slot(1, container);
            }
            ShapeKind::Tuple => {
                source.begin_tuple(shape.parts.len())?;
                return self.push_frame(dict, shape_idx, DecodeKind::Tuple, 0);
            }
            ShapeKind::Map => {
                source.begin_map()?;
                let map = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
                self.push_frame(dict, shape_idx, DecodeKind::Map { key: false }, 2)?;
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
                return self.push_frame(dict, shape_idx, DecodeKind::Slots { target, seen: vec![false; slot_count], next: 0 }, slot_count);
            }
            ShapeKind::Enum { def, first } => {
                let (def, first) = (*def, *first as usize);
                let struct_rt = &prog.types[def as usize];
                let name = source.begin_variant(&struct_rt.desc().type_name)?;
                let found = prog.variants[first..].iter().take_while(|v| v.def == def).position(|v| v.name == *name);
                let Some(variant_offset) = found else {
                    let name = crate::stdlib::truncate(&name, 64).to_string();
                    return parse_error(source, format!("unknown variant `{name}` of {}", struct_rt.name));
                };
                charge_scan(vm, variant_offset)?;
                let id = (first + variant_offset) as u32;
                let variant = &prog.variants[id as usize];
                let base = (first as u32..id).map(|v| prog.variants[v as usize].arity as u32).sum();
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
                let target = SlotTarget::Variant { id, base, named: form == PayloadForm::Named };
                let slot_count = variant.arity;
                return self.push_frame(dict, shape_idx, DecodeKind::Slots { target, seen: vec![false; slot_count], next: 0 }, slot_count);
            }
            ShapeKind::ManualEncode(_) => return Err(Fail::internal("decode: an encode shape").into()),
        };
        self.place(vm, value)
    }

    /// Advances the innermost open container by reading the next part, filling
    /// an omitted slot, or assembling the finished value.
    fn advance(&mut self, vm: &mut Vm, prog: &Program, source: &mut dyn DecodeSource) -> CodecResult<Option<Step>> {
        let frame = self.frames.last_mut().expect("an open frame");
        let (frame_base, shape_idx) = (frame.base, frame.shape);
        let dict = self.state.values[frame_base];
        let children = self.state.values.len() - frame_base - 1;
        let shape = shape(prog, shape_idx)?;
        if !frame.ended {
            let part = match &mut frame.kind {
                DecodeKind::Seq => {
                    // Check memory for doubling a full list before advancing `source`
                    // so garbage collection can run safely.
                    if let Obj::List(items) = vm.heap.get(self.state.values[frame_base + 1])
                        && items.len() == items.capacity()
                        && let Some(step) = self.state.checked(vm.check_memory(2 * items.capacity() * size_of::<VmValue>()))
                    {
                        return step.map(Some);
                    }
                    source.next()?.then_some(0)
                }
                DecodeKind::Tuple => (children < shape.parts.len() && source.next()?).then_some(children as u32),
                DecodeKind::Map { key: true } => Some(1),
                DecodeKind::Map { key: false } => source.next()?.then_some(0),
                DecodeKind::Some => (children == 0).then_some(0),
                DecodeKind::Slots { target, seen, next } => {
                    let part_base = target.base();
                    match target.names(prog) {
                        Some(names) => match source.next_field()? {
                            Some(name) => match match &shape.slot_index {
                                Some(index) if !matches!(target, SlotTarget::Variant { .. }) => index.get(&*name).map(|&i| i as usize),
                                _ => field_index(vm, names, &name, *next + usize::from(seen.get(*next) == Some(&true)))?,
                            } {
                                Some(slot) if seen[slot] => {
                                    let name = crate::stdlib::truncate(&name, 64).to_string();
                                    return parse_error(source, format!("duplicate field `{name}`"));
                                }
                                Some(slot) => {
                                    (seen[slot], *next) = (true, slot);
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
                        None => match charge_scan(vm, seen.len()).map(|()| seen.iter().position(|s| !s))? {
                            Some(slot) if source.next()? => {
                                (seen[slot], *next) = (true, slot);
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
        if let DecodeKind::Slots { target, seen, next } = &mut frame.kind
            && let Some(missing) = charge_scan(vm, seen.len()).map(|()| seen.iter().position(|s| !s))?
        {
            let part_base = target.base();
            let names = target.names(prog);
            let value = match shape.fills.get(missing).cloned().unwrap_or(DecodeFill::Required) {
                DecodeFill::Required => {
                    let part_dict = self.state.part_dict(vm, prog, shape_idx, part_base + missing as u32, dict)?;
                    match method(vm, part_dict)? {
                        Method::Structural(part_shape) if matches!(shape_kind(vm, part_shape)?, ShapeKind::Option) => {
                            VmValue::Tag(vm.prog.ids.none)
                        }
                        _ => match names {
                            Some(names) => return parse_error(source, format!("missing field `{}`", names[missing])),
                            None => return parse_error(source, format!("expected {} payload values", seen.len())),
                        },
                    }
                }
                DecodeFill::Const(const_val) => constant(vm, &const_val)?,
                DecodeFill::Call(helper_const, dicts) => {
                    let mut args = dicts.iter().map(|tmpl| eval_template(vm, tmpl, dict)).collect::<Result<Vec<_>, _>>()?;
                    // Pass preceding parameter values to an `init` parameter default helper.
                    if matches!(target, SlotTarget::Init(_)) {
                        args.extend_from_slice(&self.state.values[frame_base + 1..frame_base + 1 + missing]);
                    }
                    (seen[missing], *next) = (true, missing);
                    return Ok(Some(Step::Call(Callback { callee: vm.consts[helper_const as usize], args, kind: CallbackKind::Default })));
                }
            };
            let frame = self.frames.last_mut().expect("an open frame");
            if let DecodeKind::Slots { seen, next, .. } = &mut frame.kind {
                (seen[missing], *next) = (true, missing);
            }
            self.state.values[frame_base + 1 + missing] = value;
            return Ok(None);
        }
        let children = &self.state.values[frame_base + 1..];
        let value = match &self.frames.last().expect("an open frame").kind {
            DecodeKind::Seq | DecodeKind::Map { .. } => children[0],
            DecodeKind::Tuple if children.len() != shape.parts.len() => {
                return parse_error(source, format!("expected {} elements, found {}", shape.parts.len(), children.len()));
            }
            DecodeKind::Tuple => vm.heap.new_tuple(children),
            DecodeKind::Some => vm.some(children[0]),
            DecodeKind::Slots { target: SlotTarget::Struct(def, _), .. } => vm.heap.alloc(Obj::Struct(*def, children.into())),
            DecodeKind::Slots { target: SlotTarget::Variant { id, .. }, .. } => vm.heap.new_variant(*id, children),
            DecodeKind::Slots { target: SlotTarget::Init(_), .. } => {
                let Some(init) = &shape.init else { return Err(Fail::internal("decode: an init shape without init").into()) };
                let mut args = init.dicts.iter().map(|tmpl| eval_template(vm, tmpl, dict)).collect::<Result<Vec<_>, _>>()?;
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

    /// Pops the innermost frame and truncates its temporary stack slots.
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
    fn run(&mut self, vm: &mut Vm, shared: &DecodeCell) -> CodecResult<Step> {
        let prog = vm.prog.clone();
        let mut shared_guard = shared.lock();
        loop {
            if let Some(step) = self.state.check_memory(vm, &mut self.unchecked) {
                return step;
            }
            vm.charge(1 + (shared_guard.io.work() / SLOW_BYTES_PER_UNIT) as u64)?;
            if let Some(dict) = self.pending.take() {
                match method(vm, dict)? {
                    Method::Custom(callee) => {
                        let token = shared_guard.turn.next_token;
                        // Pre-step memory check covers allocating `Decoder`.
                        let decoder = vm.unchecked(|vm| (Decoder { shared: Arc::downgrade(shared), token }).into_vm_value(vm))?;
                        self.callback = Some(shared_guard.turn.begin());
                        return Ok(Step::Call(Callback { callee, args: vec![dict, decoder], kind: CallbackKind::Method }));
                    }
                    Method::Structural(shape_idx) => self.visit(vm, &prog, &mut shared_guard.io, shape_idx, dict)?,
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
    fn apply(&mut self, vm: &mut Vm, shared: &DecodeCell, kind: CallbackKind, result: VmValue) -> CodecResult {
        let value = match kind {
            CallbackKind::Method => {
                let Some((token, saved)) = self.callback.take() else { return Err(Fail::internal("decode: no callback").into()) };
                let mut shared_guard = shared.lock();
                let unread = shared_guard.turn.end(token, saved, "a `decode` method returned before its read ended")?;
                if let Some(err_val) = err_payload(vm, result) {
                    return Err(at_path(sand_error(vm, err_val)?, &shared_guard.io));
                }
                if unread {
                    shared_guard.io.skip()?;
                }
                vm.heap.payload_if(&result, vm.prog.ids.ok)
            }
            CallbackKind::Default => result,
            CallbackKind::Init => {
                if let Some(err_val) = err_payload(vm, result) {
                    return Err(at_path(sand_error(vm, err_val)?, &shared.lock().io));
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
    match method(vm, dict)? {
        Method::Structural(shape_idx) => Ok(Some(shape_kind(vm, shape_idx)?)),
        Method::Custom(_) => Ok(None),
    }
}

/// Charges fuel proportional to `count` for a linear scan of fields or
/// variants.
fn charge_scan(vm: &Vm, count: usize) -> Result<(), Fail> {
    vm.charge((count / 16) as u64)
}

/// Finds `name` in `names`.
///
/// Checks `guess` first before charging fuel for a linear scan.
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
    let all = |vm: &mut Vm, items: &[ConstVal]| items.iter().map(|item| constant(vm, item)).collect::<Result<Vec<_>, _>>();
    Ok(match const_val {
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
            for item in items {
                vm.set_add(set, item)?;
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
        abort(err)
    }

    fn from_future_walk(err: HostError<Fail>) -> Fail {
        abort(err)
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
async fn finish<W: Walker>(
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
                let result = context.call_keeping(callback.callee, &callback.args, walker.state().roots()).await;
                context.with_vm(|vm| walker.apply(vm, shared, callback.kind, result))?;
                walker.state().collected = false;
            }
            Step::Collect(_) => walker.state().collect(context).await,
        }
        step = context.with_vm(|vm| walker.run(vm, shared))?;
    }
}

/// Runs `walker` synchronously if possible, or spawns an async continuation if
/// a callback is needed.
fn drive<W, S, T, E>(
    vm: &mut Vm,
    shared: Arc<Mutex<Shared<S>>>,
    cell: Arc<Mutex<Shared<W::IO>>>,
    mut walker: W,
    then: impl FnOnce(&mut Vm, W::Output, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    W: Walker,
    S: Send + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    match walker.run(vm, &cell).map_err(E::from_walk)? {
        Step::Done => {
            let output = walker.output().map_err(|err| E::from_walk(err.into()))?;
            Ok(Later::ready(then(vm, output, &mut shared.lock().io).map_err(E::from_future_walk)))
        }
        Step::Collect(err) => Err(E::from_walk(err.into())),
        step => Ok(Later::new(WaitKind::Io, |mut context| async move {
            let output = finish(&mut walker, &mut context, &cell, step).await.map_err(E::from_future_walk)?;
            context.with_vm(|vm| then(vm, output, &mut shared.lock().io)).map_err(E::from_future_walk)
        })),
    }
}

/// Encodes `value` with `dict` into `shared` and passes the sink to `then`.
pub(crate) fn encode<S, T, E>(
    vm: &mut Vm,
    shared: Arc<Mutex<Shared<S>>>,
    value: VmValue,
    dict: VmValue,
    then: impl FnOnce(&mut Vm, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    S: EncodeSink + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    let cell: EncodeCell = shared.clone();
    drive(vm, shared, cell, EncodeWalker::new(value, dict), |vm, (), sink| then(vm, sink))
}

/// Decodes a value with `dict` from `shared` and passes the result to `then`.
pub(crate) fn decode<S, T, E>(
    vm: &mut Vm,
    shared: Arc<Mutex<Shared<S>>>,
    dict: VmValue,
    then: impl FnOnce(&mut Vm, VmValue, &mut S) -> CodecResult<T> + Send + 'static,
) -> Result<Later<T, E>, E>
where
    S: DecodeSource + 'static,
    T: IntoVmValue + 'static,
    E: WalkError,
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    let cell: DecodeCell = shared.clone();
    drive(vm, shared, cell, DecodeWalker::new(dict), then)
}

/// Runs `walker` to completion inside an async native call.
async fn run_async<W: Walker>(context: &mut NativeContext, shared: &Arc<Mutex<Shared<W::IO>>>, mut walker: W) -> CodecResult<W::Output> {
    let first = context.with_vm(|vm| walker.run(vm, shared))?;
    finish(&mut walker, context, shared, first).await
}

/// Encodes `value` with `dict` into a host [`Value`](crate::value::Value).
pub(crate) async fn to_value(context: &mut NativeContext, value: VmValue, dict: VmValue) -> CodecResult<crate::value::Value> {
    let memory = context.with_vm(|vm| vm.heap.io_memory.clone());
    let shared = Shared::new(ValueSink::new(&memory));
    let cell: EncodeCell = shared.clone();
    run_async(context, &cell, EncodeWalker::new(value, dict)).await?;
    let root = shared.lock().io.take();
    root.map(|(value, _)| value).ok_or_else(|| Fail::internal("to_value: no value").into())
}

/// Decodes host `value` with `dict` into a [`VmValue`] valid until the next
/// wait.
pub(crate) async fn from_value(context: &mut NativeContext, value: crate::value::Value, dict: VmValue) -> CodecResult<VmValue> {
    let shared: DecodeCell = Shared::new(crate::value::ValueSource::new(value));
    run_async(context, &shared, DecodeWalker::new(dict)).await
}

/// Error messages for inactive or duplicate `Encoder` writes.
const ENCODER_MISUSE: [&str; 2] =
    ["an `Encoder` accepts a value only during its `encode` call", "an `encode` method wrote a second value to its `Encoder`"];

/// Error messages for inactive or duplicate `Decoder` reads.
const DECODER_MISUSE: [&str; 2] =
    ["a `Decoder` gives a value only during its `decode` call", "a `decode` method read a second value from its `Decoder`"];

/// Claims `token` on `shared` and runs nested `walker` asynchronously.
///
/// Records any recoverable failure in `Turn` so the outer walk fails even if
/// the custom method ignores the returned `Result`.
fn nested<W: Walker, T: IntoVmValue + 'static>(
    shared: Option<Arc<Mutex<Shared<W::IO>>>>,
    token: u64,
    [inactive, second]: [&str; 2],
    walker: W,
    output: impl FnOnce(W::Output) -> T + Send + 'static,
) -> Result<Later<T, HostError>, HostError> {
    let Some(shared) = shared else { return Err(HostError::new("invalid", inactive)) };
    shared.lock().turn.claim(token, inactive, second)?;
    Ok(Later::new(WaitKind::Io, |mut context| async move {
        let result = run_async(&mut context, &shared, walker).await;
        let mut shared_guard = shared.lock();
        shared_guard.turn.open -= 1;
        if let Err(err) = &result {
            shared_guard.turn.record(err);
        }
        drop(shared_guard);
        result.map(output)
    }))
}

/// Encodes `value` through `encoder`.
fn write(vm: &mut Vm, dict: VmValue, value: VmValue, encoder: VmVal<Encoder>) -> Result<Later<(), HostError>, HostError> {
    let encoder_ref = encoder.get(&vm.heap)?;
    nested(encoder_ref.shared.upgrade(), encoder_ref.token, ENCODER_MISUSE, EncodeWalker::new(value, dict), |()| ())
}

/// Decodes a value from `decoder`.
fn read(vm: &mut Vm, dict: VmValue, decoder: VmVal<Decoder>) -> Result<Later<Param<0>, HostError>, HostError> {
    let decoder_ref = decoder.get(&vm.heap)?;
    nested(decoder_ref.shared.upgrade(), decoder_ref.token, DECODER_MISUSE, DecodeWalker::new(dict), Param::from_raw)
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
        write(vm, dict.raw(), value.raw(), encoder)
    });
    module.function("Encoder.encode", |vm: &mut Vm, dict: TraitDict<0, traits::Encode>, encoder: VmVal<Encoder>, value: Param<0>| {
        write(vm, dict.raw(), value.raw(), encoder)
    });
    module.function("Decode.decode", |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        read(vm, dict.raw(), decoder)
    });
    module.function("Decoder.decode", |vm: &mut Vm, dict: TraitDict<0, traits::Decode>, decoder: VmVal<Decoder>| {
        read(vm, dict.raw(), decoder)
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
