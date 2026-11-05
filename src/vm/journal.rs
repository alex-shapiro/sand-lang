//! The VM side of the journal (`crate::journal`): recording and replaying
//! inputs, journaled asynchronous operations, and the encoding of results.
//!
//! Replay depends on one property of the VM: given the same inputs, it runs
//! the same instructions, in the same tasks, in the same order. The inputs
//! are of two sorts. Synchronous inputs (clock readings, results of
//! journaled calls) occur inside a task's slice, at a point fixed by the
//! instructions before them, so replay consumes them in journal order.
//! Completions of asynchronous operations occur between slices, at a point
//! the host's timing decides, so each `Done` entry holds the scheduling step
//! (the count of slices run) after which the engine delivered it, and replay
//! delivers it after the same step.

use super::bytecode::Program;
use super::iomem::IoMemory;
use super::value::{MapObj, Obj, SetObj, Value};
use super::{BoxFuture, Ctl, CtlKind, Finisher, NResult, Vm};
pub(crate) use crate::journal::OpKind;
use crate::journal::{CallResult, Entry, EntryData, EntryKind, Journal, OpDone, OpEnd, OpStart};
use crate::platform::{Instant, SystemTime};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Why a journaled run stops before it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalStop {
    /// The run does something other than what the journal records.
    Diverged { seq: u64, message: String },
    /// Every task waits for a timer or a parked host call (SPEC §16.7).
    Suspended(Option<SystemTime>),
    /// The journal failed to flush.
    Failed(String),
}

/// The work of a journaled operation.
pub(crate) enum OpWork {
    /// A timer that expires after the duration.
    Timer(Duration),
    /// A future that yields the operation's outcome as bytes, which the
    /// journal records, and the function that turns those bytes into the
    /// result. The function runs in record and in replay alike.
    Bytes { fut: Pin<Box<dyn Future<Output = Vec<u8>> + Send>>, build: BuildFn },
    /// A host operation: the journal records the value its finisher returns.
    Value(BoxFuture),
    /// A journaled host call that replay does not make (its `Done` entry
    /// holds the result).
    Replayed,
}

pub(crate) type BuildFn = Box<dyn FnOnce(&mut Vm, &[u8]) -> NResult + Send>;

/// Until when a journaled run may be suspended while an operation waits:
/// `None` if it may not be, `Some(None)` until the host resumes it.
pub(crate) type Park = Option<Option<SystemTime>>;

/// An asynchronous operation that a native asks the scheduler to start
/// under the journal.
pub struct JournalOp {
    pub(crate) kind: OpKind,
    pub(crate) identity: u64,
    pub(crate) label: String,
    /// Whether the operation has effects outside the run: the journal is
    /// flushed before it starts.
    pub(crate) effectful: bool,
    /// The run can be suspended while the operation waits, until this time
    /// (`Some(None)`: until the host resumes it).
    pub(crate) park: Park,
    /// Fuel that the native spent before it asked for the operation, if
    /// replay does not call the native (journaled host calls).
    pub(crate) pre_fuel: u64,
    pub(crate) work: OpWork,
}

impl Ctl {
    /// Starts a journaled asynchronous operation (the run has a journal).
    pub(crate) fn journaled(op: JournalOp) -> Ctl {
        Ctl::from(CtlKind::Journaled(Box::new(op)))
    }
}

/// A journaled timer that expires after `d` (`task.sleep`).
pub(crate) fn timer(d: Duration) -> Ctl {
    let identity = Identity::default().add(&d.as_nanos().to_le_bytes()).finish();
    let label = format!("sleep {}", crate::vm::display::fmt_duration(d.as_nanos().min(i64::MAX as u128) as i64));
    Ctl::journaled(JournalOp {
        kind: OpKind::Timer,
        identity,
        label,
        effectful: false,
        park: Some(None),
        pre_fuel: 0,
        work: OpWork::Timer(d),
    })
}

/// Calls host function `name` (`inner`), registered as journaled, with
/// `args` (SPEC §16.7). In replay, returns its recorded result without
/// calling it. An effectful function is called only once the journal is
/// durable.
pub(crate) fn journaled_call(vm: &mut Vm, inner: &super::HostFn, name: &str, effectful: bool, args: &[Value]) -> NResult {
    if !vm.journaling() {
        return inner.call(vm, args);
    }
    let identity = call_identity(vm, name, args);
    let op = |work: OpWork, pre_fuel: u64, park: Park| {
        Ctl::journaled(JournalOp { kind: OpKind::Call, identity, label: name.to_string(), effectful, park, pre_fuel, work })
    };
    match vm.next_op_replayed() {
        Some(true) => return Err(op(OpWork::Replayed, 0, None)),
        Some(false) => {
            // The journal records the start of the call, not its end: call
            // it again, and deliver even a result it returns at once as the
            // completion of the recorded operation.
            let start = vm.call_start();
            let r = inner.call(vm, args);
            if vm.retried(&r, start) {
                return r;
            }
            let pre_fuel = vm.meter.used().saturating_sub(start);
            let work = match r {
                Err(c) if matches!(c.kind(), CtlKind::Async(_)) => match c.into_kind() {
                    CtlKind::Async(fut) => OpWork::Value(fut),
                    _ => unreachable!("matched above"),
                },
                r => {
                    let (payload, _) = outcome_payload(vm, r, name)?;
                    OpWork::Value(Box::pin(std::future::ready(Box::new(move |vm: &mut Vm| payload_outcome(vm, &payload)) as Finisher)))
                }
            };
            let park = vm.op_park.take();
            return Err(op(work, pre_fuel, park));
        }
        None => {}
    }
    if vm.replaying() {
        return vm.journaled_result(identity, name, |vm| inner.call(vm, args));
    }
    if effectful && vm.must_flush() {
        return Err(Ctl::from(CtlKind::Flush));
    }
    let start = vm.call_start();
    let r = inner.call(vm, args);
    if matches!(&r, Err(c) if matches!(c.kind(), CtlKind::Async(_))) {
        let Err(c) = r else { unreachable!("matched above") };
        let CtlKind::Async(fut) = c.into_kind() else { unreachable!("matched above") };
        let pre_fuel = vm.meter.used().saturating_sub(start);
        let park = vm.op_park.take();
        return Err(op(OpWork::Value(fut), pre_fuel, park));
    }
    vm.record_result(identity, name, start, r)
}

/// How replay builds the result of an operation from its `Done` entry.
pub(crate) enum ReplayBuild {
    /// A timer that expires at the time, if the journal records one.
    Timer(Option<SystemTime>),
    Bytes(BuildFn),
    Value,
}

/// The journal state of a run.
pub(crate) struct JournalState {
    sink: Arc<dyn Journal>,
    replay: Vec<EntryData>,
    pos: usize,
    seq: u64,
    pub unflushed: bool,
    /// Journaled operations started so far (the id of the next one).
    ops: u64,
    /// Scheduling steps (slices) run so far.
    pub steps: u64,
    /// The operations whose `Done` entry is in `replay`, by id.
    done: HashMap<u64, usize>,
    /// Operations waiting for their recorded `Done` entry, by id: the wait
    /// (fiber, generation) and how to build the result.
    pub replayed: HashMap<u64, (u32, u64, ReplayBuild)>,
    /// Completions of operations that run live while replay is not over:
    /// they are delivered when it is.
    pub held: Vec<(u32, u64, Finisher)>,
    /// The monotonic time when the run started.
    origin: Instant,
    /// The last monotonic reading that replay returned, after `origin`.
    mono_last: Duration,
    /// What live monotonic readings add to keep time moving forward from
    /// the replayed ones (set at the first live reading).
    mono_skew: Option<Duration>,
    /// The latest wall-clock time that the run has seen: its latest
    /// reading, or the wake time of its latest completed timer.
    wall_floor: SystemTime,
    pub suspend_after: Option<Duration>,
    /// The budget of the buffers of live operations, apart from the heap's,
    /// so that the heap's memory checks are the same in replay.
    pub live_memory: Arc<IoMemory>,
    /// The fuel used when a journaled call that is retried after a
    /// collection first started.
    pub call_start: Option<u64>,
    /// A divergence found where it could not be reported.
    pending: Option<JournalStop>,
}

impl JournalState {
    pub fn replaying(&self) -> bool {
        self.pos < self.replay.len()
    }

    /// The divergence that a cancellation found, if any.
    pub fn take_pending(&mut self) -> Result<(), JournalStop> {
        match self.pending.take() {
            Some(s) => Err(s),
            None => Ok(()),
        }
    }

    fn peek(&self) -> Option<&EntryData> {
        self.replay.get(self.pos)
    }

    fn diverged(&self, message: String) -> JournalStop {
        JournalStop::Diverged { seq: self.pos as u64, message }
    }

    /// The next replayed entry, which must be of `kind` (`what` describes
    /// the input for the error), or `None` if replay is over.
    fn take(&mut self, kind: EntryKind, what: &dyn Fn() -> String) -> Result<Option<EntryData>, JournalStop> {
        self.take_pending()?;
        let Some(e) = self.peek() else { return Ok(None) };
        if e.kind() != kind {
            let msg = format!("the run reads {} where the journal records {}", what(), e.describe());
            return Err(self.diverged(msg));
        }
        let e = e.clone();
        self.pos += 1;
        Ok(Some(e))
    }

    fn record_done(&mut self, op: u64, fuel: u64, end: OpEnd) {
        let step = self.steps;
        self.record(EntryData::Done(OpDone { op, step, fuel, end }));
    }

    fn record(&mut self, data: EntryData) {
        let seq = self.seq;
        self.seq += 1;
        self.unflushed = true;
        self.sink.record(Entry { seq, kind: data.kind(), data: data.encode() });
    }

    pub fn flush(&mut self) -> Option<crate::journal::FlushFuture> {
        if !self.unflushed {
            return None;
        }
        self.unflushed = false;
        Some(self.sink.flush())
    }
}

/// Creates the journal state of a run that replays `sink`'s entries.
/// `origin` is the monotonic clock reading when the run starts.
pub(crate) fn start(
    sink: Arc<dyn Journal>,
    origin: Instant,
    suspend_after: Option<Duration>,
    limits: &crate::Limits,
) -> Result<JournalState, JournalStop> {
    let mut done = HashMap::new();
    let mut replay = Vec::new();
    for (i, e) in sink.replay().into_iter().enumerate() {
        let seq = i as u64;
        if e.seq != seq {
            return Err(JournalStop::Diverged { seq, message: format!("entry {i} has sequence number {}", e.seq) });
        }
        let data = e
            .decode()
            .ok_or_else(|| JournalStop::Diverged { seq, message: format!("entry {i} is a malformed {} entry", e.kind.name()) })?;
        if let EntryData::Done(d) = &data {
            done.insert(d.op, i);
        }
        replay.push(data);
    }
    Ok(JournalState {
        seq: replay.len() as u64,
        sink,
        replay,
        pos: 0,
        unflushed: false,
        ops: 0,
        steps: 0,
        done,
        replayed: HashMap::new(),
        held: Vec::new(),
        origin,
        mono_last: Duration::ZERO,
        mono_skew: None,
        wall_floor: crate::platform::UNIX_EPOCH,
        suspend_after,
        live_memory: IoMemory::new(limits.max_memory, limits.max_open_resources),
        call_start: None,
        pending: None,
    })
}

impl JournalState {
    /// The live wall-clock time of the run: `clock`'s time, or the latest
    /// time that the run has seen if the clock is behind it.
    pub fn wall(&self, clock: &dyn crate::platform::Clock) -> SystemTime {
        clock.wall().max(self.wall_floor)
    }

    /// Notes that the run has seen wall-clock time `t`.
    fn saw_wall(&mut self, t: SystemTime) {
        self.wall_floor = self.wall_floor.max(t);
    }
}

// ----------------------------------------------------------------------
// Byte encoding

/// Appends little-endian fields to a buffer.
#[derive(Default)]
pub(crate) struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, b: u8) -> &mut Self {
        self.0.push(b);
        self
    }
    pub fn u64(&mut self, x: u64) -> &mut Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    pub fn i64(&mut self, x: i64) -> &mut Self {
        self.u64(x as u64)
    }
    pub fn i128(&mut self, x: i128) -> &mut Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }
    /// Appends `b` without a length: the rest of the data.
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
}

/// Reads the fields a [`Writer`] wrote; `None` at a malformed field.
pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader(data)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    }
    pub fn i64(&mut self) -> Option<i64> {
        self.u64().map(|x| x as i64)
    }
    pub fn i128(&mut self) -> Option<i128> {
        self.take(16).map(|b| i128::from_le_bytes(b.try_into().unwrap_or_default()))
    }
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.take(n)
    }
    pub fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }
    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A stable 64-bit hash (FNV-1a) of the identity of an input.
#[derive(Clone, Copy)]
pub(crate) struct Identity(u64);

impl Default for Identity {
    fn default() -> Self {
        Identity(0xcbf2_9ce4_8422_2325)
    }
}

impl Identity {
    pub fn add(mut self, data: &[u8]) -> Self {
        for chunk in [&(data.len() as u64).to_le_bytes()[..], data] {
            for &b in chunk {
                self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        self
    }
    pub fn finish(self) -> u64 {
        self.0
    }
}

// ----------------------------------------------------------------------
// The encoding of Sand values

const MAX_DEPTH: usize = 256;

mod tag {
    pub const UNIT: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const INT: u8 = 2;
    pub const FLOAT: u8 = 3;
    pub const DUR: u8 = 4;
    pub const STR: u8 = 5;
    pub const BYTES: u8 = 6;
    pub const LIST: u8 = 7;
    pub const MAP: u8 = 8;
    pub const SET: u8 = 9;
    pub const TUPLE: u8 = 10;
    pub const STRUCT: u8 = 11;
    pub const VARIANT: u8 = 12;
    pub const RANGE: u8 = 13;
}

fn struct_name(prog: &Program, id: u32) -> Option<String> {
    let t = prog.types.get(id as usize)?;
    Some(format!("{}.{}", t.module, t.name))
}

fn variant_name(prog: &Program, id: u32) -> Option<String> {
    let v = prog.variants.get(id as usize)?;
    let t = prog.types.get(v.def as usize)?;
    Some(format!("{}.{}.{}", t.module, t.name, v.name))
}

/// Encodes `v`, a value of plain data (no functions, tasks, channels or
/// opaque objects), in at most `max` bytes.
pub(crate) fn encode_value(vm: &Vm, v: Value, max: usize) -> Result<Vec<u8>, String> {
    let mut w = Writer::default();
    encode_into(vm, v, &mut w, 0, max)?;
    Ok(w.0)
}

fn encode_into(vm: &Vm, v: Value, w: &mut Writer, depth: usize, max: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("it nests deeper than {MAX_DEPTH} levels"));
    }
    if w.0.len() > max {
        return Err(format!("it takes more than {max} bytes"));
    }
    let variant = |w: &mut Writer, id: u32, fields: &[Value]| -> Result<(), String> {
        w.u8(tag::VARIANT).u64(u64::from(id)).str(&variant_name(&vm.prog, id).unwrap_or_default()).u64(fields.len() as u64);
        fields.iter().try_for_each(|&f| encode_into(vm, f, w, depth + 1, max))
    };
    match v {
        Value::Unit => {
            w.u8(tag::UNIT);
        }
        Value::Bool(b) => {
            w.u8(tag::BOOL).u8(u8::from(b));
        }
        Value::Int(i) => {
            w.u8(tag::INT).i64(i);
        }
        Value::Float(f) => {
            w.u8(tag::FLOAT).u64(f.to_bits());
        }
        Value::Dur(d) => {
            w.u8(tag::DUR).i64(d);
        }
        Value::Tag(id) => variant(w, id, &[])?,
        Value::Pair(a, b) => {
            w.u8(tag::TUPLE).u64(2).u8(tag::INT).i64(i64::from(a)).u8(tag::INT).i64(b);
        }
        Value::Uninit => return Err("it holds an uninitialized value".into()),
        Value::Obj(_) => match vm.heap.get(v) {
            Obj::Str(s) => {
                w.u8(tag::STR).str(s);
            }
            Obj::Bytes(b) => {
                w.u8(tag::BYTES).bytes(b);
            }
            Obj::List(items) => {
                w.u8(tag::LIST).u64(items.len() as u64);
                for &x in items {
                    encode_into(vm, x, w, depth + 1, max)?;
                }
            }
            Obj::Tuple(items) => {
                w.u8(tag::TUPLE).u64(items.len() as u64);
                for &x in items.iter() {
                    encode_into(vm, x, w, depth + 1, max)?;
                }
            }
            Obj::Map(m) => {
                w.u8(tag::MAP).u64(m.live as u64);
                for &(k, x) in m.entries.iter().flatten() {
                    encode_into(vm, k, w, depth + 1, max)?;
                    encode_into(vm, x, w, depth + 1, max)?;
                }
            }
            Obj::Set(s) => {
                w.u8(tag::SET).u64(s.live as u64);
                for &k in s.entries.iter().flatten() {
                    encode_into(vm, k, w, depth + 1, max)?;
                }
            }
            Obj::Struct(id, fields) => {
                w.u8(tag::STRUCT).u64(u64::from(*id)).str(&struct_name(&vm.prog, *id).unwrap_or_default()).u64(fields.len() as u64);
                for &x in fields.iter() {
                    encode_into(vm, x, w, depth + 1, max)?;
                }
            }
            Obj::Variant(id, fields) => variant(w, *id, fields)?,
            Obj::Range(a, b, inclusive) => {
                w.u8(tag::RANGE).i64(*a).i64(*b).u8(u8::from(*inclusive));
            }
            other => return Err(format!("it holds a value of kind {}", other.kind_name())),
        },
        _ => match v.as_inline_variant() {
            Some((id, payload)) => variant(w, id, &[payload])?,
            None => return Err("it holds a value that cannot be journaled".into()),
        },
    }
    Ok(())
}

/// Decodes a value that [`encode_value`] encoded, allocating it on the heap
/// (memory checks are off: the caller checks the limit after rooting it).
pub(crate) fn decode_value(vm: &mut Vm, data: &[u8]) -> NResult {
    let mut r = Reader::new(data);
    let mut types = None;
    vm.unchecked(|vm| decode_from(vm, &mut r, 0, &mut types))
}

fn malformed() -> Ctl {
    Ctl::internal("a journaled value is malformed")
}

/// The runtime id of struct (`is_variant` false) or variant `id`, named
/// `name`: `id` itself if it still has that name, or else the id that has
/// it in this program.
fn resolve(vm: &Vm, id: u64, name: &str, is_variant: bool, types: &mut Option<HashMap<(bool, String), u32>>) -> Result<u32, Ctl> {
    let prog = &vm.prog;
    let named = |i: u32| if is_variant { variant_name(prog, i) } else { struct_name(prog, i) };
    if let Ok(i) = u32::try_from(id)
        && named(i).as_deref() == Some(name)
    {
        return Ok(i);
    }
    let table = types.get_or_insert_with(|| {
        let mut t = HashMap::new();
        for i in 0..prog.types.len() as u32 {
            if let Some(n) = struct_name(prog, i) {
                t.entry((false, n)).or_insert(i);
            }
        }
        for i in 0..prog.variants.len() as u32 {
            if let Some(n) = variant_name(prog, i) {
                t.entry((true, n)).or_insert(i);
            }
        }
        t
    });
    table
        .get(&(is_variant, name.to_string()))
        .copied()
        .ok_or_else(|| Ctl::internal(format!("the journal holds a value of type `{name}`, which this program does not define")))
}

fn decode_from(vm: &mut Vm, r: &mut Reader<'_>, depth: usize, types: &mut Option<HashMap<(bool, String), u32>>) -> NResult {
    if depth > MAX_DEPTH {
        return Err(malformed());
    }
    let count = |r: &mut Reader<'_>| -> Result<usize, Ctl> {
        let n = usize::try_from(r.u64().ok_or_else(malformed)?).map_err(|_| malformed())?;
        // Each item takes at least one byte.
        if n > r.0.len() { Err(malformed()) } else { Ok(n) }
    };
    // Nothing collects garbage while a value is decoded.
    let items = |vm: &mut Vm, r: &mut Reader<'_>, n: usize, types: &mut Option<HashMap<(bool, String), u32>>| -> Result<Vec<Value>, Ctl> {
        (0..n).map(|_| decode_from(vm, r, depth + 1, types)).collect()
    };
    let t = r.u8().ok_or_else(malformed)?;
    Ok(match t {
        tag::UNIT => Value::Unit,
        tag::BOOL => Value::Bool(r.u8().ok_or_else(malformed)? != 0),
        tag::INT => Value::Int(r.i64().ok_or_else(malformed)?),
        tag::FLOAT => Value::Float(f64::from_bits(r.u64().ok_or_else(malformed)?)),
        tag::DUR => Value::Dur(r.i64().ok_or_else(malformed)?),
        tag::STR => {
            let s = r.str().ok_or_else(malformed)?;
            vm.charge_bytes(s.len())?;
            vm.short_str(s)
        }
        tag::BYTES => {
            let b = r.bytes().ok_or_else(malformed)?;
            vm.charge_bytes(b.len())?;
            vm.heap.new_bytes(b.to_vec())
        }
        tag::LIST => {
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            vm.charge(n as u64)?;
            vm.heap.new_list(xs)
        }
        tag::TUPLE => {
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            vm.heap.new_tuple(&xs)
        }
        tag::MAP => {
            let n = count(r)?;
            let xs = items(vm, r, 2 * n, types)?;
            let m = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
            xs.chunks_exact(2).try_for_each(|kv| vm.map_insert(m, kv[0], kv[1]).map(drop))?;
            m
        }
        tag::SET => {
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            let s = vm.heap.alloc(Obj::Set(Box::<SetObj>::default()));
            xs.iter().try_for_each(|&k| vm.set_add(s, k).map(drop))?;
            s
        }
        tag::STRUCT | tag::VARIANT => {
            let id = r.u64().ok_or_else(malformed)?;
            let name = r.str().ok_or_else(malformed)?.to_string();
            let id = resolve(vm, id, &name, t == tag::VARIANT, types)?;
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            if t == tag::VARIANT {
                vm.heap.new_variant(id, &xs)
            } else {
                let fields = vm.heap.boxed(&xs);
                vm.heap.alloc(Obj::Struct(id, fields))
            }
        }
        tag::RANGE => {
            let (a, b, inclusive) = (r.i64().ok_or_else(malformed)?, r.i64().ok_or_else(malformed)?, r.u8().ok_or_else(malformed)?);
            vm.heap.alloc(Obj::Range(a, b, inclusive != 0))
        }
        _ => return Err(malformed()),
    })
}

/// The most bytes one journaled value may take.
fn max_value_bytes(vm: &Vm) -> usize {
    vm.host.limits.max_memory.unwrap_or(1 << 30)
}

/// The identity of a call of native `name` with `args`: their encoding
/// where they have one, and otherwise their debug form, so that a value of
/// a bound value type such as `time.Instant` still takes part.
pub(crate) fn call_identity(vm: &Vm, name: &str, args: &[Value]) -> u64 {
    let mut id = Identity::default().add(name.as_bytes());
    for &a in args {
        id = match encode_value(vm, a, 1 << 20) {
            Ok(bytes) => id.add(&bytes),
            Err(_) => match crate::vm::display::to_debug_metered(&vm.prog, &vm.heap, &crate::vm::meter::Unmetered, a) {
                Ok(text) => id.add(b"?").add(text.as_bytes()),
                Err(_) => id.add(b"?"),
            },
        };
    }
    id.finish()
}

/// Records a call's outcome `r` as the payload of an entry: a status byte,
/// then the value's encoding or the error message. Returns the value to
/// use, decoded from the encoding so that record and replay allocate alike.
fn outcome_payload(vm: &mut Vm, r: NResult, label: &str) -> Result<(Vec<u8>, Option<Vec<u8>>), Ctl> {
    match r {
        Ok(v) => match encode_value(vm, v, max_value_bytes(vm)) {
            Ok(bytes) => {
                let mut w = Writer::default();
                w.u8(0).0.extend_from_slice(&bytes);
                Ok((w.0, Some(bytes)))
            }
            Err(why) => Err(Ctl::fail("unsupported", format!("cannot journal the result of `{label}`: {why}"))),
        },
        Err(c) => match c.into_kind() {
            CtlKind::RuntimeError(m) => {
                let mut w = Writer::default();
                w.u8(1).str(&m);
                Ok((w.0, None))
            }
            other => Err(Ctl::from(other)),
        },
    }
}

/// The result that an outcome payload holds.
fn payload_outcome(vm: &mut Vm, payload: &[u8]) -> NResult {
    let mut r = Reader::new(payload);
    match r.u8() {
        Some(0) => decode_value(vm, r.rest()),
        Some(1) => Err(Ctl::from(CtlKind::RuntimeError(r.str().ok_or_else(malformed)?.to_string()))),
        _ => Err(malformed()),
    }
}

fn stop(s: JournalStop) -> Ctl {
    Ctl::from(CtlKind::Journal(s))
}

impl Vm {
    /// Whether the run has a journal (SPEC §16.7).
    #[inline]
    pub fn journaling(&self) -> bool {
        self.journal.is_some()
    }

    pub(crate) fn journal_state(&self) -> Option<std::cell::RefMut<'_, JournalState>> {
        self.journal.as_ref().map(|j| j.borrow_mut())
    }

    /// Whether the run replays journal entries.
    pub(crate) fn replaying(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.borrow().replaying())
    }

    /// Attaches the journal state `j`. Takes the random seed from the
    /// journal, or records it, unless the configuration fixes it.
    pub(crate) fn attach_journal(&mut self, mut j: JournalState, seeded: bool) -> Result<(), JournalStop> {
        if !seeded {
            let seed = match j.take(EntryKind::Seed, &|| "the random seed".into())? {
                Some(EntryData::Seed(seed)) => seed,
                _ => {
                    let seed = crate::platform::entropy();
                    j.record(EntryData::Seed(seed));
                    seed
                }
            };
            self.host.rng = super::Rng::new(seed);
        }
        self.journal = Some(Box::new(RefCell::new(j)));
        Ok(())
    }

    /// The wall-clock time, as the script observes it: journaled.
    pub fn wall_time(&self) -> Result<SystemTime, Ctl> {
        let Some(j) = &self.journal else { return Ok(self.host.clock.wall()) };
        let mut j = j.borrow_mut();
        if let Some(EntryData::Wall(t)) = j.take(EntryKind::Wall, &|| "the wall clock".into()).map_err(stop)? {
            j.saw_wall(t);
            return Ok(t);
        }
        let t = j.wall(&*self.host.clock);
        j.saw_wall(t);
        j.record(EntryData::Wall(t));
        Ok(t)
    }

    /// The monotonic time, as the script observes it: journaled. Readings
    /// after replay continue from the last replayed one.
    pub fn monotonic(&self) -> Result<Instant, Ctl> {
        let Some(j) = &self.journal else { return Ok(self.host.clock.now()) };
        let mut j = j.borrow_mut();
        if let Some(EntryData::Monotonic(d)) = j.take(EntryKind::Monotonic, &|| "the monotonic clock".into()).map_err(stop)? {
            j.mono_last = j.mono_last.max(d);
            return Ok(j.origin + d);
        }
        let raw = self.host.clock.now().saturating_duration_since(j.origin);
        let last = j.mono_last;
        let skew = *j.mono_skew.get_or_insert(last.saturating_sub(raw));
        let d = raw + skew;
        j.record(EntryData::Monotonic(d));
        Ok(j.origin + d)
    }

    /// Fuel to add for a charge by elapsed time at this point: the recorded
    /// amount in replay, or else `live()`, recorded if it is not zero.
    pub(crate) fn time_fuel(&self, live: impl FnOnce() -> u64) -> Result<u64, Ctl> {
        let Some(j) = &self.journal else { return Ok(live()) };
        let mut j = j.borrow_mut();
        let at = self.meter.used();
        if j.replaying() {
            let Some(&EntryData::Fuel { at: rec_at, extra }) = j.peek() else { return Ok(0) };
            if rec_at > at {
                return Ok(0);
            }
            if rec_at < at {
                return Err(stop(j.diverged(format!("the journal records a fuel charge at {rec_at} units, and the run is at {at}"))));
            }
            j.pos += 1;
            return Ok(extra);
        }
        let extra = live();
        if extra > 0 {
            j.record(EntryData::Fuel { at, extra });
        }
        Ok(extra)
    }

    /// Runs `f`, a synchronous input of the run identified by `identity`
    /// and described by `label`, and records its result; in replay, returns
    /// the recorded result without running `f`. The value must be plain
    /// data (see [`encode_value`]).
    pub(crate) fn journaled_result(&mut self, identity: u64, label: &str, f: impl FnOnce(&mut Vm) -> NResult) -> NResult {
        if self.journal.is_none() {
            return f(self);
        }
        if let Some(r) = self.replay_result(identity, label)? {
            return r;
        }
        let start = self.call_start();
        let r = f(self);
        self.record_result(identity, label, start, r)
    }

    /// Consumes the recorded result of a synchronous call in replay.
    fn replay_result(&mut self, identity: u64, label: &str) -> Result<Option<NResult>, Ctl> {
        let Some(j) = &self.journal else { return Ok(None) };
        let mut j = j.borrow_mut();
        let seq = j.pos as u64;
        let replayed = j.take(EntryKind::Result, &|| format!("the result of `{label}`")).map_err(stop)?;
        drop(j);
        let Some(EntryData::Result(CallResult { identity: id, label: rec_label, fuel, outcome })) = replayed else { return Ok(None) };
        if id != identity {
            let msg = if rec_label == label {
                format!("the run calls `{label}` with other arguments than the journal records")
            } else {
                format!("the run calls `{label}` where the journal records a call of `{rec_label}`")
            };
            return Err(stop(JournalStop::Diverged { seq, message: msg }));
        }
        let v = payload_outcome(self, &outcome);
        self.charge(fuel)?;
        Ok(Some(v))
    }

    /// The fuel used when the current journaled call started: now, or when
    /// it first started if this is its retry after a collection.
    fn call_start(&self) -> u64 {
        let saved = self.journal_state().and_then(|mut j| j.call_start.take());
        match saved {
            Some(s) if self.collected => s,
            _ => self.meter.used(),
        }
    }

    /// Whether `r` asks for the call to run again after a collection; then
    /// keeps `start` for the retry.
    fn retried(&self, r: &NResult, start: u64) -> bool {
        let retry = matches!(r, Err(c) if matches!(c.kind(), CtlKind::Collect | CtlKind::Limit(crate::LimitKind::Memory)));
        if retry && let Some(mut j) = self.journal_state() {
            j.call_start = Some(start);
        }
        retry
    }

    /// Records the result `r` of a synchronous call that started at fuel
    /// `start`, and returns it.
    fn record_result(&mut self, identity: u64, label: &str, start: u64, r: NResult) -> NResult {
        if self.retried(&r, start) {
            return r;
        }
        let fuel = self.meter.used().saturating_sub(start);
        let (payload, value) = outcome_payload(self, r, label)?;
        if let Some(mut j) = self.journal_state() {
            j.record(EntryData::Result(CallResult { identity, label: label.to_string(), fuel, outcome: payload.clone() }));
        }
        match value {
            Some(bytes) => decode_value(self, &bytes),
            None => payload_outcome(self, &payload),
        }
    }

    /// The next journaled operation to start, if replay holds its
    /// completion: replay does not make journaled host calls for it.
    pub(crate) fn next_op_replayed(&self) -> Option<bool> {
        let j = self.journal.as_ref()?.borrow();
        if !matches!(j.peek()?, EntryData::Start(_)) {
            return None;
        }
        Some(j.done.contains_key(&j.ops))
    }

    /// Whether entries recorded since the last flush must be made durable
    /// before an effect.
    pub(crate) fn must_flush(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| {
            let j = j.borrow();
            j.unflushed && !j.replaying()
        })
    }

    /// Records the completion of operation `op` with `payload` and `fuel`.
    fn record_done(&self, op: u64, fuel: u64, payload: &[u8]) {
        if let Some(j) = &self.journal {
            j.borrow_mut().record_done(op, fuel, OpEnd::Completed(payload.to_vec()));
        }
    }

    /// Notes that the task waiting for journaled operation `op` was
    /// cancelled: the operation never completes. Records that, so that a
    /// resumed run does not start the operation again; replay consumes the
    /// record.
    pub(crate) fn cancel_op(&self, op: u64) {
        let Some(j) = &self.journal else { return };
        let mut j = j.borrow_mut();
        if !j.replaying() {
            j.record_done(op, 0, OpEnd::Cancelled);
            return;
        }
        if j.replayed.remove(&op).is_none() {
            // A live operation that replay started: the journal does not
            // record its start's end, so the cancellation lies beyond it.
            return;
        }
        match j.peek() {
            Some(EntryData::Done(OpDone { op: o, end: OpEnd::Cancelled, .. })) if *o == op => j.pos += 1,
            _ if j.pending.is_none() => {
                let msg = format!(
                    "the run cancels operation {op} where the journal records {}",
                    j.peek().map_or("nothing".into(), |e| e.describe())
                );
                j.pending = Some(j.diverged(msg));
            }
            _ => {}
        }
    }

    /// Starts the bookkeeping of journaled operation `op` for the fiber
    /// waiting as `(fid, wgen)`: consumes its `Start` entry in replay, or
    /// records one. Returns the future to run live and until when the run
    /// may be suspended while it waits, or `None` if replay holds the
    /// completion.
    pub(crate) fn begin_op(&mut self, fid: u32, wgen: u64, op: JournalOp) -> Result<Option<(BoxFuture, Park)>, Ctl> {
        let Some(j) = &self.journal else { return Err(Ctl::internal("a journaled operation without a journal")) };
        let mut j = j.borrow_mut();
        let id = j.ops;
        let label = op.label.clone();
        let seq = j.pos as u64;
        let replayed = j.take(EntryKind::Start, &|| format!("the start of {} `{label}`", op.kind.name())).map_err(stop)?;
        let wake = match &replayed {
            Some(EntryData::Start(s)) => {
                if s.kind != op.kind || s.identity != op.identity {
                    let msg = if s.kind == op.kind && s.label == label {
                        format!("the run starts {} `{label}` with other arguments than the journal records", op.kind.name())
                    } else {
                        format!(
                            "the run starts {} `{label}` where the journal records the start of {} `{}`",
                            op.kind.name(),
                            s.kind.name(),
                            s.label
                        )
                    };
                    return Err(stop(JournalStop::Diverged { seq, message: msg }));
                }
                if matches!(op.work, OpWork::Replayed) {
                    self.meter.charge(s.pre_fuel)?;
                }
                s.wake
            }
            _ => {
                let wake = match op.work {
                    OpWork::Timer(d) => Some(j.wall(&*self.host.clock) + d),
                    _ => None,
                };
                j.record(EntryData::Start(OpStart {
                    kind: op.kind,
                    identity: op.identity,
                    pre_fuel: op.pre_fuel,
                    wake,
                    label: label.clone(),
                }));
                wake
            }
        };
        j.ops += 1;
        if let Some(Some(f)) = self.fibers.get_mut(fid as usize) {
            f.jop = Some(id);
        }
        let in_replay = replayed.is_some();
        if in_replay && j.done.contains_key(&id) {
            let build = match op.work {
                OpWork::Timer(_) => ReplayBuild::Timer(wake),
                OpWork::Bytes { build, .. } => ReplayBuild::Bytes(build),
                OpWork::Value(_) | OpWork::Replayed => ReplayBuild::Value,
            };
            j.replayed.insert(id, (fid, wgen, build));
            return Ok(None);
        }
        let label = op.label;
        let park = match op.work {
            OpWork::Timer(_) => Some(wake),
            _ => op.park,
        };
        let fut: BoxFuture = match op.work {
            OpWork::Timer(d) => {
                let sleep = match (in_replay, wake) {
                    (true, Some(t)) => self.host.clock.sleep(t.duration_since(j.wall(&*self.host.clock)).unwrap_or_default()),
                    _ => self.host.clock.sleep(d),
                };
                Box::pin(async move {
                    sleep.await;
                    Box::new(move |vm: &mut Vm| {
                        vm.record_done(id, 0, &[]);
                        if let (Some(t), Some(mut j)) = (wake, vm.journal_state()) {
                            j.saw_wall(t);
                        }
                        Ok(Value::Unit)
                    }) as Finisher
                })
            }
            OpWork::Bytes { fut, build } => Box::pin(async move {
                let payload = fut.await;
                Box::new(move |vm: &mut Vm| {
                    vm.record_done(id, 0, &payload);
                    build(vm, &payload)
                }) as Finisher
            }),
            OpWork::Value(fut) => Box::pin(async move {
                let fin = fut.await;
                Box::new(move |vm: &mut Vm| {
                    let start = vm.meter.used();
                    let r = fin(vm);
                    let fuel = vm.meter.used().saturating_sub(start);
                    let (payload, value) = outcome_payload(vm, r, &label)?;
                    vm.record_done(id, fuel, &payload);
                    match value {
                        Some(bytes) => decode_value(vm, &bytes),
                        None => payload_outcome(vm, &payload),
                    }
                }) as Finisher
            }),
            OpWork::Replayed => return Err(Ctl::internal("a replayed call has no recorded completion")),
        };
        Ok(Some((fut, park)))
    }

    /// The next completion that replay delivers now: one recorded after the
    /// current step. Fails if one recorded after an earlier step is pending.
    pub(crate) fn next_due(&mut self) -> Result<Option<(u32, u64, Finisher)>, Ctl> {
        let Some(j) = &self.journal else { return Ok(None) };
        let mut j = j.borrow_mut();
        j.take_pending().map_err(stop)?;
        let Some(EntryData::Done(OpDone { op, step, fuel, end })) = j.peek() else { return Ok(None) };
        let (op, step, fuel) = (*op, *step, *fuel);
        let OpEnd::Completed(payload) = end else {
            // Consumed where the run cancels the operation.
            return Ok(None);
        };
        let payload = payload.clone();
        if step > j.steps {
            return Ok(None);
        }
        if step < j.steps {
            let msg = format!("the journal records the completion of operation {op} after step {step}, and the run is at step {}", j.steps);
            return Err(stop(j.diverged(msg)));
        }
        let Some((fid, wgen, build)) = j.replayed.remove(&op) else {
            let msg = format!("the journal records the completion of operation {op}, which the run has not started");
            return Err(stop(j.diverged(msg)));
        };
        j.pos += 1;
        let fin: Finisher = match build {
            ReplayBuild::Timer(wake) => Box::new(move |vm: &mut Vm| {
                if let (Some(t), Some(mut j)) = (wake, vm.journal_state()) {
                    j.saw_wall(t);
                }
                Ok(Value::Unit)
            }),
            ReplayBuild::Bytes(build) => Box::new(move |vm: &mut Vm| build(vm, &payload)),
            ReplayBuild::Value => Box::new(move |vm: &mut Vm| {
                let v = payload_outcome(vm, &payload);
                vm.charge(fuel)?;
                v
            }),
        };
        Ok(Some((fid, wgen, fin)))
    }

    /// The divergence error for a run that can do nothing more while
    /// replay holds more entries.
    pub(crate) fn blocked_in_replay(&self) -> Option<JournalStop> {
        let mut j = self.journal.as_ref()?.borrow_mut();
        if let Err(s) = j.take_pending() {
            return Some(s);
        }
        let e = j.peek()?;
        Some(j.diverged(format!("every task is blocked, and the journal records {} next", e.describe())))
    }
}
