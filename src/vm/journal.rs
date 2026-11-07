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
use super::{BoxFuture, Ctl, CtlKind, End, Finisher, NResult, Request, Vm};
use crate::journal::{CallResult, Durable, Entry, EntryData, EntryKind, Journal, OpDone, OpEnd, OpKind, OpStart, ParkedCall};
use crate::native::func::{Answer, Call, JournaledFn, Park};
use crate::platform::{Instant, SystemTime};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// How a journaled run does something other than what its journal
/// records: at entry `seq`, as `message` says. Its fields are private to
/// the crate.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub(crate) seq: u64,
    pub(crate) message: String,
}

/// Builds the result of a recorded operation from the journal's encoding of
/// its outcome. It runs in record and in replay alike.
pub(crate) type BuildFn = Box<dyn FnOnce(&mut Vm, &[u8]) -> NResult + Send>;

/// The journal's encoding of an outcome, made on the VM thread.
pub(crate) type EncodeFn = Box<dyn FnOnce(&mut Vm) -> Result<Vec<u8>, Ctl> + Send>;

/// The outcome of a live operation whose result the journal records.
pub(crate) trait Outcome: Send + 'static {
    /// The result as the outcome arrives, in a run without a journal (an
    /// HTTP body streams). `build` makes the result from the journal's
    /// encoding of an outcome.
    fn deliver(self: Box<Self>, vm: &mut Vm, build: BuildFn) -> NResult;

    /// Waits for the whole outcome, which a journaled run records as one
    /// entry before the program gets it (an HTTP body is read to its end).
    fn settle(self: Box<Self>) -> Pin<Box<dyn Future<Output = EncodeFn> + Send>>;
}

/// An outcome that arrives whole: the journal's encoding of it, from which
/// the build function makes the result in every run.
pub(crate) struct Whole(pub Vec<u8>);

impl Outcome for Whole {
    fn deliver(self: Box<Self>, vm: &mut Vm, build: BuildFn) -> NResult {
        build(vm, &self.0)
    }

    fn settle(self: Box<Self>) -> Pin<Box<dyn Future<Output = EncodeFn> + Send>> {
        let payload = self.0;
        Box::pin(std::future::ready(Box::new(move |_: &mut Vm| Ok(payload)) as EncodeFn))
    }
}

/// What a task waits for ([`Request::Wait`]).
pub struct Wait {
    /// The id of the operation in the run's journal, for an operation whose
    /// outcome the journal records. `None` for a local operation, and in a
    /// run without a journal.
    pub(crate) op: Option<u64>,
    pub(crate) parking: Parking,
    pub(crate) work: WaitWork,
}

/// Whether the engine may suspend the run while an operation waits
/// (SPEC §16.7).
pub(crate) enum Parking {
    /// The engine must wait for the operation.
    Never,
    /// A timer of a journaled run, which is due at this time.
    Timer(SystemTime),
    /// A parked call of host function `function`, with the tag that it
    /// gave ([`ParkedCall`]).
    Host { park: Park, function: Arc<str>, tag: String },
}

impl Parking {
    /// How long the run may be suspended, if it may.
    pub(crate) fn park(&self) -> Option<Park> {
        match self {
            Parking::Never => None,
            Parking::Timer(t) => Some(Park::Until(*t)),
            Parking::Host { park, .. } => Some(*park),
        }
    }

    /// The host call as a suspension lists it, if this is one.
    pub(crate) fn parked_call(&self) -> Option<ParkedCall> {
        let Parking::Host { park, function, tag } = self else { return None };
        let wake_at = match *park {
            Park::UntilResumed => None,
            Park::Until(t) => Some(t),
        };
        Some(ParkedCall { function: function.to_string(), tag: tag.clone(), wake_at })
    }
}

/// Whether an operation has effects outside the run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effects {
    /// The operation has no effects outside the run.
    Pure,
    /// The journal is made durable before the operation starts.
    External,
}

pub(crate) enum WaitWork {
    /// The future runs, and its finisher gives the result.
    Run { fut: BoxFuture, effects: Effects },
    /// Replay delivers the recorded outcome at its step, through the build
    /// function.
    Replay(BuildFn),
}

impl Wait {
    /// A wait for local operation `fut`: replay runs it again.
    pub(crate) fn local(fut: BoxFuture) -> Wait {
        Wait { op: None, parking: Parking::Never, work: WaitWork::Run { fut, effects: Effects::Pure } }
    }
}

impl Ctl {
    /// Suspends the task on `wait`.
    pub(crate) fn wait(wait: Wait) -> Ctl {
        Ctl::request(Request::Wait(Box::new(wait)))
    }
}

/// The description of an operation whose outcome the journal records.
pub(crate) struct OpSpec {
    pub kind: OpKind,
    pub effects: Effects,
    /// Whether the run may be suspended while the operation waits. A
    /// timer's parking comes from its wake time, which only a journaled
    /// run has.
    pub parking: Parking,
    /// Fuel that the native spent before it started the operation.
    pub pre_fuel: u64,
    /// How long a timer waits.
    pub timer: Option<Duration>,
}

/// What the journal holds for the next operation ([`Vm::take_start`]).
pub(crate) enum StartEntry {
    /// The journal records the operation's start and end: replay delivers
    /// the recorded end.
    Replayed(Replayed),
    /// The journal records only the operation's start: the operation runs
    /// again.
    Rerun(Rerun),
    /// The journal does not hold the operation: it runs, and the journal
    /// records its start.
    Fresh(Fresh),
    /// The run has no journal: the operation runs.
    Unjournaled,
    /// Replay holds the recorded result of a synchronous call, and
    /// consumed it. `identity` and `label` describe the operation.
    Recorded { identity: u64, label: String, result: Recorded },
    /// Replay holds `next`, an entry other than a start or a result, at
    /// `seq`. `label` describes the operation.
    Other { label: String, seq: u64, next: String },
}

/// The recorded result of a synchronous call, at entry `seq`.
pub(crate) struct Recorded {
    seq: u64,
    result: CallResult,
}

impl Recorded {
    /// The recorded result, if the call that `identity` and `label`
    /// describe is the recorded one. Charges the fuel that the call spent.
    fn deliver(self, vm: &mut Vm, identity: u64, label: &str) -> NResult {
        let Recorded { seq, result: CallResult { identity: id, label: rec_label, fuel, outcome } } = self;
        if id != identity {
            let message = if rec_label == label {
                format!("the run calls `{label}` with other arguments than the journal records")
            } else {
                format!("the run calls `{label}` where the journal records a call of `{rec_label}`")
            };
            return Err(Ctl::diverged(Divergence { seq, message }));
        }
        let v = payload_outcome(vm, &outcome);
        vm.charge(fuel)?;
        v
    }
}

/// An operation whose recorded start replay consumes when the operation
/// runs again ([`Rerun::start`]).
pub(crate) struct Rerun {
    op: u64,
    seq: u64,
    wake: Option<SystemTime>,
}

impl Rerun {
    /// Consumes the recorded start: the operation runs.
    pub fn start(self, vm: &Vm, spec: OpSpec) -> Live {
        if let Some(mut j) = vm.journal_state() {
            j.pos += 1;
            j.ops += 1;
        }
        let parking = self.wake.map_or(spec.parking, Parking::Timer);
        Live { op: Some(self.op), seq: Some(self.seq), wake: self.wake, parking, effects: spec.effects }
    }
}

/// A new operation of a journaled run, with its identity and label.
pub(crate) struct Fresh {
    identity: u64,
    label: String,
}

impl Fresh {
    /// Records the operation's start: the operation runs.
    pub fn start(self, vm: &Vm, spec: OpSpec) -> Live {
        let Fresh { identity, label } = self;
        let mut j = vm.journal_state().expect("only a journaled run has a fresh operation");
        let (op, seq) = (j.ops, j.position());
        let wake = spec.timer.map(|d| j.wall(&*vm.host.clock) + d);
        j.record(EntryData::Start(OpStart { kind: spec.kind, identity, pre_fuel: spec.pre_fuel, wake, label }));
        j.ops += 1;
        let parking = wake.map_or(spec.parking, Parking::Timer);
        Live { op: Some(op), seq: Some(seq), wake, parking, effects: spec.effects }
    }
}

/// An operation that [`Vm::begin_op`] started.
pub(crate) enum Begun {
    /// The journal holds the outcome: replay delivers it.
    Replayed(Replayed),
    /// The operation runs.
    Live(Live),
}

/// An operation whose recorded outcome replay delivers.
pub(crate) struct Replayed {
    op: u64,
    /// The fuel that the native spent before the operation started, as the
    /// journal records it.
    pub pre_fuel: u64,
    /// When a timer expires.
    pub wake: Option<SystemTime>,
}

impl Replayed {
    /// The wait for the recorded outcome, which `build` turns into the result.
    pub fn wait(self, build: BuildFn) -> Ctl {
        Ctl::wait(Wait { op: Some(self.op), parking: Parking::Never, work: WaitWork::Replay(build) })
    }
}

/// An operation that runs.
pub(crate) struct Live {
    /// The id of the operation in the run's journal, if it has one.
    op: Option<u64>,
    /// The sequence number of the operation's `Start` entry.
    #[cfg_attr(not(any(feature = "os", web)), expect(dead_code, reason = "only the HTTP modules read it"))]
    pub seq: Option<u64>,
    /// When a timer expires.
    pub wake: Option<SystemTime>,
    parking: Parking,
    effects: Effects,
}

impl Live {
    /// An operation of a run without a journal.
    fn unjournaled(spec: OpSpec) -> Live {
        Live { op: None, seq: None, wake: None, parking: spec.parking, effects: spec.effects }
    }

    /// The wait for `fut`. Without a journal, the program gets the outcome
    /// as it arrives. With one, the engine waits for the whole outcome,
    /// records it, and gives the program what `build` makes of it, as
    /// replay does.
    pub fn run(self, fut: impl Future<Output = Box<dyn Outcome>> + Send + 'static, build: BuildFn) -> Ctl {
        let fut: BoxFuture = match self.op {
            None => Box::pin(async move {
                let outcome = fut.await;
                Box::new(move |vm: &mut Vm| outcome.deliver(vm, build)) as Finisher
            }),
            Some(id) => Box::pin(async move {
                let encode = fut.await.settle().await;
                Box::new(move |vm: &mut Vm| {
                    let start = vm.meter.used();
                    let payload = encode(vm)?;
                    let fuel = vm.meter.used().saturating_sub(start);
                    vm.record_done(id, fuel, &payload);
                    build(vm, &payload)
                }) as Finisher
            }),
        };
        Ctl::wait(Wait { op: self.op, parking: self.parking, work: WaitWork::Run { fut, effects: self.effects } })
    }
}

/// The outcome of a host call: its finisher, or the encoding of a result
/// that the call returned at once.
struct CallOutcome {
    fin: Finisher,
    label: Arc<str>,
}

impl Outcome for CallOutcome {
    fn deliver(self: Box<Self>, vm: &mut Vm, _: BuildFn) -> NResult {
        (self.fin)(vm)
    }

    fn settle(self: Box<Self>) -> Pin<Box<dyn Future<Output = EncodeFn> + Send>> {
        let CallOutcome { fin, label } = *self;
        Box::pin(std::future::ready(Box::new(move |vm: &mut Vm| {
            let r = fin(vm);
            outcome_payload(vm, r, &label).map(|outcome| outcome.to_payload())
        }) as EncodeFn))
    }
}

/// The outcome of a timer.
struct Expired;

impl Outcome for Expired {
    fn deliver(self: Box<Self>, _: &mut Vm, _: BuildFn) -> NResult {
        Ok(Value::Unit)
    }

    fn settle(self: Box<Self>) -> Pin<Box<dyn Future<Output = EncodeFn> + Send>> {
        Box::pin(std::future::ready(Box::new(|_: &mut Vm| Ok(Vec::new())) as EncodeFn))
    }
}

/// A timer that expires after `d` (`task.sleep`). In a journaled run, the
/// journal records its wake time, and replay delivers its expiry without
/// waiting.
pub(crate) fn sleep(vm: &mut Vm, d: Duration) -> Ctl {
    let spec = OpSpec { kind: OpKind::Timer, effects: Effects::Pure, parking: Parking::Never, pre_fuel: 0, timer: Some(d) };
    let describe = |_: &Vm| {
        let identity = Identity::default().add(&d.as_nanos().to_le_bytes()).finish();
        Ok((identity, format!("sleep {}", crate::vm::display::fmt_duration(d.as_nanos().min(i64::MAX as u128) as i64))))
    };
    let expired = |wake: Option<SystemTime>| -> BuildFn {
        Box::new(move |vm: &mut Vm, _: &[u8]| {
            if let (Some(t), Some(mut j)) = (wake, vm.journal_state()) {
                j.saw_wall(t);
            }
            Ok(Value::Unit)
        })
    };
    match vm.begin_op(spec, describe) {
        Err(c) => c,
        Ok(Begun::Replayed(r)) => {
            let build = expired(r.wake);
            r.wait(build)
        }
        Ok(Begun::Live(live)) => {
            let wait = match live.wake {
                Some(t) => vm.journal_state().map_or(d, |j| t.duration_since(j.wall(&*vm.host.clock)).unwrap_or_default()),
                None => d,
            };
            let sleep = vm.host.clock.sleep(wait);
            let build = expired(live.wake);
            live.run(
                async move {
                    sleep.await;
                    Box::new(Expired) as Box<dyn Outcome>
                },
                build,
            )
        }
    }
}

/// Calls host function `name` (`inner`), registered as journaled, with
/// `args` (SPEC §16.7). In replay, returns its recorded result without
/// calling it. The future of an effectful function starts only once the
/// journal, with the call's `Start` entry, is durable.
pub(crate) fn journaled_call(vm: &mut Vm, call: Call, inner: &JournaledFn, name: &Arc<str>, effects: Effects, args: &[Value]) -> NResult {
    // A journaled function reads no journal input (it has no `Call`), so
    // the entry that the call records or replays is the next one.
    let call = Call { seq: vm.journal_state().map(|j| j.position()), ..call };
    let inner = |vm: &mut Vm| inner(vm, call, args);
    let spec = |pre_fuel: u64, parking: Parking| OpSpec { kind: OpKind::Call, effects, parking, pre_fuel, timer: None };
    let run = |live: Live, fin: BoxFuture| {
        let label = name.clone();
        live.run(async move { Box::new(CallOutcome { fin: fin.await, label }) as Box<dyn Outcome> }, Box::new(payload_outcome))
    };
    match vm.take_start(OpKind::Call, |vm| Ok((call_identity(vm, name, args)?, name.to_string())))? {
        StartEntry::Replayed(r) => {
            vm.meter.charge(r.pre_fuel)?;
            Err(r.wait(Box::new(payload_outcome)))
        }
        StartEntry::Recorded { identity, label, result } => result.deliver(vm, identity, &label),
        StartEntry::Other { label, seq, next } => {
            let message = format!("the run reads the result of `{label}` where the journal records {next}");
            Err(Ctl::diverged(Divergence { seq, message }))
        }
        StartEntry::Rerun(rerun) => {
            // The journal records the start of the call, not its end: call
            // it again, and deliver even a result it returns at once as the
            // completion of the recorded operation.
            let start = vm.meter.used();
            let r = match inner(vm) {
                Err(c) if is_retry(&c) => return Err(c),
                r => r,
            };
            let pre_fuel = vm.meter.used().saturating_sub(start);
            let (fin, parking) = match waiting(r, name) {
                Ok(w) => w,
                Err(r) => {
                    let payload = outcome_payload(vm, r, name)?.to_payload();
                    let ready: Finisher = Box::new(move |vm: &mut Vm| payload_outcome(vm, &payload));
                    (Box::pin(std::future::ready(ready)) as BoxFuture, Parking::Never)
                }
            };
            Err(run(rerun.start(vm, spec(pre_fuel, parking)), fin))
        }
        StartEntry::Fresh(fresh) => {
            let start = vm.meter.used();
            match waiting(inner(vm), name) {
                Ok((fut, parking)) => {
                    let pre_fuel = vm.meter.used().saturating_sub(start);
                    Err(run(fresh.start(vm, spec(pre_fuel, parking)), fut))
                }
                Err(r) => vm.record_result(name, start, r, fresh.identity, fresh.label),
            }
        }
        StartEntry::Unjournaled => match waiting(inner(vm), name) {
            Ok((fut, parking)) => Err(run(Live::unjournaled(spec(0, parking)), fut)),
            Err(r) => r,
        },
    }
}

/// The future that the answer `r` of a call of host function `name` asks
/// the task to wait for, and how the call parks. Gives back any other
/// result.
fn waiting(r: Result<Answer, Ctl>, name: &Arc<str>) -> Result<(BoxFuture, Parking), NResult> {
    match r.map_err(Ctl::into_kind) {
        Ok(Answer::Value(v)) => Err(Ok(v)),
        Ok(Answer::Parked(fut, park, tag)) => Ok((fut, Parking::Host { park, function: name.clone(), tag })),
        Err(CtlKind::Request(Request::Async(fut))) => Ok((fut, Parking::Never)),
        Err(kind) => Err(Err(Ctl::new(kind))),
    }
}

/// An operation whose `Done` entry replay delivers at its step.
pub(crate) enum Replaying {
    /// The journal records the outcome, which the build function turns into
    /// the result.
    Recorded(BuildFn),
    /// A local operation, whose future runs again: its finisher, once the
    /// future completes.
    Local(Option<Finisher>),
}

/// What replay delivers next ([`Vm::next_due`]).
pub(crate) enum Due {
    /// Nothing at this step.
    Nothing,
    /// The completion of an operation.
    Ready(u32, u64, Finisher),
    /// A local operation completes at this step, and its future has not
    /// completed yet.
    Waiting,
}

/// The journal state of a run.
pub(crate) struct JournalState {
    sink: Arc<dyn Journal>,
    replay: Arc<[Entry]>,
    pos: usize,
    /// The number of entries in the journal: the sequence number of the
    /// next recorded entry.
    len: u64,
    pub unflushed: bool,
    /// Journaled operations started so far (the id of the next one).
    ops: u64,
    /// Scheduling steps (slices) run so far.
    pub steps: u64,
    /// The operations whose `Done` entry is in `replay`, by id.
    done: HashMap<u64, usize>,
    /// Operations waiting for their recorded `Done` entry, by id: the wait
    /// (fiber, generation) and how to deliver the result.
    pub replayed: HashMap<u64, (u32, u64, Replaying)>,
    /// Live local operations: the journal records their `Done` entry when
    /// they complete.
    local: std::collections::HashSet<u64>,
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
    live_memory: Arc<IoMemory>,
}

impl JournalState {
    pub fn replaying(&self) -> bool {
        self.pos < self.replay.len()
    }

    /// The sequence number of the entry that the run reads or records next.
    pub fn position(&self) -> u64 {
        if self.replaying() { self.pos as u64 } else { self.len }
    }

    fn peek(&self) -> Option<&EntryData> {
        self.replay.get(self.pos).map(Entry::data)
    }

    fn diverged(&self, message: String) -> Divergence {
        Divergence { seq: self.pos as u64, message }
    }

    /// The next replayed entry, which must be of `kind` (`what` describes
    /// the input for the error), or `None` if replay is over.
    fn take(&mut self, kind: EntryKind, what: &dyn Fn() -> String) -> Result<Option<EntryData>, Divergence> {
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
        self.len += 1;
        self.unflushed = true;
        self.sink.record(Entry::from(data));
    }

    pub fn flush(&mut self) -> Option<crate::journal::FlushFuture> {
        if !self.unflushed {
            return None;
        }
        self.unflushed = false;
        Some(self.sink.flush())
    }
}

/// Creates the journal state of a durable run.
/// `origin` is the monotonic clock reading when the run starts.
/// The journal state of a durable run that starts ([`Durability`](crate::journal::Durability)).
#[doc(hidden)]
pub struct Started(pub(crate) JournalState);

pub(crate) fn start(durable: Durable, origin: Instant, limits: &crate::engine::Limits) -> JournalState {
    let mut done = HashMap::new();
    for (i, e) in durable.replay.iter().enumerate() {
        if let EntryData::Done(d) = e.data() {
            done.insert(d.op, i);
        }
    }
    JournalState {
        len: durable.replay.len() as u64,
        sink: durable.journal.clone(),
        replay: durable.replay.clone(),
        pos: 0,
        unflushed: false,
        ops: 0,
        steps: 0,
        done,
        replayed: HashMap::new(),
        local: Default::default(),
        held: Vec::new(),
        origin,
        mono_last: Duration::ZERO,
        mono_skew: None,
        wall_floor: crate::platform::UNIX_EPOCH,
        suspend_after: durable.suspend_after,
        live_memory: IoMemory::new(limits.max_memory, limits.max_open_resources),
    }
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
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A type with one encoding in the payloads of journal entries.
pub(crate) trait Payload: Sized {
    fn encode(&self, w: &mut Writer);

    /// Reads a value that [`Payload::encode`] wrote; `None` if it is
    /// malformed.
    fn decode(r: &mut Reader<'_>) -> Option<Self>;

    /// The value as a whole payload.
    fn to_payload(&self) -> Vec<u8> {
        let mut w = Writer::default();
        self.encode(&mut w);
        w.0
    }

    /// The value that a whole payload holds; `None` if it is malformed.
    fn from_payload(data: &[u8]) -> Option<Self> {
        let mut r = Reader::new(data);
        let v = Self::decode(&mut r)?;
        r.is_empty().then_some(v)
    }
}

impl Payload for Vec<u8> {
    fn encode(&self, w: &mut Writer) {
        w.bytes(self);
    }
    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        r.bytes().map(<[u8]>::to_vec)
    }
}

impl Payload for String {
    fn encode(&self, w: &mut Writer) {
        w.str(self);
    }
    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        r.str().map(str::to_string)
    }
}

impl<T: Payload> Payload for Option<T> {
    fn encode(&self, w: &mut Writer) {
        match self {
            None => {
                w.u8(0);
            }
            Some(x) => {
                w.u8(1);
                x.encode(w);
            }
        }
    }
    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        match r.u8()? {
            0 => Some(None),
            1 => Some(Some(T::decode(r)?)),
            _ => None,
        }
    }
}

impl<T: Payload, E: Payload> Payload for Result<T, E> {
    fn encode(&self, w: &mut Writer) {
        match self {
            Ok(x) => {
                w.u8(0);
                x.encode(w);
            }
            Err(e) => {
                w.u8(1);
                e.encode(w);
            }
        }
    }
    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        match r.u8()? {
            0 => Some(Ok(T::decode(r)?)),
            1 => Some(Err(E::decode(r)?)),
            _ => None,
        }
    }
}

impl Payload for crate::stdlib::core::Error {
    fn encode(&self, w: &mut Writer) {
        w.str(&self.kind).str(&self.message);
    }
    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        Some(crate::stdlib::core::Error { kind: String::decode(r)?, message: String::decode(r)? })
    }
}

/// An input that can be unavailable yet ([`Vm::ready_input`]).
pub(crate) enum Ready {
    /// The input is not available: the caller waits for it.
    Pending,
    /// The input is at its end.
    End,
    Data(Vec<u8>),
}

impl From<Option<Vec<u8>>> for Ready {
    fn from(data: Option<Vec<u8>>) -> Ready {
        data.map_or(Ready::End, Ready::Data)
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

crate::journal::coded_enum! {
    /// The kind of an encoded Sand value.
    enum Tag {
        Unit = 0;
        Bool = 1;
        Int = 2;
        Float = 3;
        Dur = 4;
        Str = 5;
        Bytes = 6;
        List = 7;
        Map = 8;
        Set = 9;
        Tuple = 10;
        Struct = 11;
        Variant = 12;
        Range = 13;
        /// A value of a bound value type, in a call identity only.
        Value = 14;
    }
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
    encode_into(vm, v, &mut w, 0, Encoding { max, values: false })?;
    Ok(w.0)
}

/// How [`encode_into`] encodes.
#[derive(Clone, Copy)]
struct Encoding {
    /// The most bytes.
    max: usize,
    /// Whether values of bound value types are encoded as their type and
    /// text, which identifies them but does not decode.
    values: bool,
}

fn encode_into(vm: &Vm, v: Value, w: &mut Writer, depth: usize, enc: Encoding) -> Result<(), String> {
    let max = enc.max;
    if depth > MAX_DEPTH {
        return Err(format!("it nests deeper than {MAX_DEPTH} levels"));
    }
    if w.0.len() > max {
        return Err(format!("it takes more than {max} bytes"));
    }
    let variant = |w: &mut Writer, id: u32, fields: &[Value]| -> Result<(), String> {
        w.u8(Tag::Variant.code()).u64(u64::from(id)).str(&variant_name(&vm.prog, id).unwrap_or_default()).u64(fields.len() as u64);
        fields.iter().try_for_each(|&f| encode_into(vm, f, w, depth + 1, enc))
    };
    match v {
        Value::Unit => {
            w.u8(Tag::Unit.code());
        }
        Value::Bool(b) => {
            w.u8(Tag::Bool.code()).u8(u8::from(b));
        }
        Value::Int(i) => {
            w.u8(Tag::Int.code()).i64(i);
        }
        Value::Float(f) => {
            w.u8(Tag::Float.code()).u64(f.to_bits());
        }
        Value::Dur(d) => {
            w.u8(Tag::Dur.code()).i64(d);
        }
        Value::Tag(id) => variant(w, id, &[])?,
        Value::Pair(a, b) => {
            w.u8(Tag::Tuple.code()).u64(2).u8(Tag::Int.code()).i64(i64::from(a)).u8(Tag::Int.code()).i64(b);
        }
        Value::Uninit => return Err("it holds an uninitialized value".into()),
        Value::Obj(_) => match vm.heap.get(v) {
            Obj::Str(s) => {
                w.u8(Tag::Str.code()).str(s);
            }
            Obj::Bytes(b) => {
                w.u8(Tag::Bytes.code()).bytes(b);
            }
            Obj::List(items) => {
                w.u8(Tag::List.code()).u64(items.len() as u64);
                for &x in items {
                    encode_into(vm, x, w, depth + 1, enc)?;
                }
            }
            Obj::Tuple(items) => {
                w.u8(Tag::Tuple.code()).u64(items.len() as u64);
                for &x in items.iter() {
                    encode_into(vm, x, w, depth + 1, enc)?;
                }
            }
            Obj::Map(m) => {
                w.u8(Tag::Map.code()).u64(m.live as u64);
                for &(k, x) in m.entries.iter().flatten() {
                    encode_into(vm, k, w, depth + 1, enc)?;
                    encode_into(vm, x, w, depth + 1, enc)?;
                }
            }
            Obj::Set(s) => {
                w.u8(Tag::Set.code()).u64(s.live as u64);
                for &k in s.entries.iter().flatten() {
                    encode_into(vm, k, w, depth + 1, enc)?;
                }
            }
            Obj::Struct(id, fields) => {
                w.u8(Tag::Struct.code()).u64(u64::from(*id)).str(&struct_name(&vm.prog, *id).unwrap_or_default()).u64(fields.len() as u64);
                for &x in fields.iter() {
                    encode_into(vm, x, w, depth + 1, enc)?;
                }
            }
            Obj::Variant(id, fields) => variant(w, *id, fields)?,
            Obj::Range(a, b, inclusive) => {
                w.u8(Tag::Range.code()).i64(*a).i64(*b).u8(u8::from(*inclusive));
            }
            Obj::Value(x) if enc.values => {
                let mut text = String::new();
                x.write_text(&mut text);
                w.u8(Tag::Value.code()).str(x.type_name()).str(&text);
            }
            Obj::Packed(t, bits) if enc.values => {
                w.u8(Tag::Value.code()).str(t.name()).u64(bits[0]).u64(bits[1]);
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
/// Decoding costs no fuel: the call charged for converting its result, live
/// or as the recorded fuel in replay, so a journaled run uses the fuel of a
/// run without a journal.
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
    let t = Tag::from_code(r.u8().ok_or_else(malformed)?).ok_or_else(malformed)?;
    Ok(match t {
        Tag::Unit => Value::Unit,
        Tag::Bool => Value::Bool(r.u8().ok_or_else(malformed)? != 0),
        Tag::Int => Value::Int(r.i64().ok_or_else(malformed)?),
        Tag::Float => Value::Float(f64::from_bits(r.u64().ok_or_else(malformed)?)),
        Tag::Dur => Value::Dur(r.i64().ok_or_else(malformed)?),
        Tag::Str => {
            let s = r.str().ok_or_else(malformed)?;
            vm.short_str(s)
        }
        Tag::Bytes => {
            let b = r.bytes().ok_or_else(malformed)?;
            vm.heap.new_bytes(b.to_vec())
        }
        Tag::List => {
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            vm.heap.new_list(xs)
        }
        Tag::Tuple => {
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            vm.heap.new_tuple(&xs)
        }
        Tag::Map => {
            let n = count(r)?;
            let xs = items(vm, r, 2 * n, types)?;
            let m = vm.heap.alloc(Obj::Map(Box::<MapObj>::default()));
            xs.chunks_exact(2).try_for_each(|kv| vm.map_insert(m, kv[0], kv[1]).map(drop))?;
            m
        }
        Tag::Set => {
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            let s = vm.heap.alloc(Obj::Set(Box::<SetObj>::default()));
            xs.iter().try_for_each(|&k| vm.set_add(s, k).map(drop))?;
            s
        }
        Tag::Struct | Tag::Variant => {
            let id = r.u64().ok_or_else(malformed)?;
            let name = r.str().ok_or_else(malformed)?.to_string();
            let id = resolve(vm, id, &name, t == Tag::Variant, types)?;
            let n = count(r)?;
            let xs = items(vm, r, n, types)?;
            if t == Tag::Variant {
                vm.heap.new_variant(id, &xs)
            } else {
                let fields = vm.heap.boxed(&xs);
                vm.heap.alloc(Obj::Struct(id, fields))
            }
        }
        Tag::Range => {
            let (a, b, inclusive) = (r.i64().ok_or_else(malformed)?, r.i64().ok_or_else(malformed)?, r.u8().ok_or_else(malformed)?);
            vm.heap.alloc(Obj::Range(a, b, inclusive != 0))
        }
        Tag::Value => return Err(malformed()),
    })
}

/// The most bytes one journaled value may take.
fn max_value_bytes(vm: &Vm) -> usize {
    vm.host.limits.max_memory.unwrap_or(1 << 30)
}

/// The identity of a call of native `name` with `args`: their encoding.
/// Registration checks that the journal can encode the types of the
/// arguments; a call fails if an argument holds a value that it cannot
/// encode all the same, such as a function in a field of a struct.
pub(crate) fn call_identity(vm: &Vm, name: &str, args: &[Value]) -> Result<u64, Ctl> {
    let mut id = Identity::default().add(name.as_bytes());
    for &a in args {
        let mut w = Writer::default();
        encode_into(vm, a, &mut w, 0, Encoding { max: 1 << 20, values: true })
            .map_err(|why| Ctl::fail("unsupported", format!("cannot journal the arguments of `{name}`: {why}")))?;
        id = id.add(&w.0);
    }
    Ok(id.finish())
}

/// A Sand value in the journal encoding of values ([`encode_value`]).
struct Encoded(Vec<u8>);

impl Payload for Encoded {
    fn encode(&self, w: &mut Writer) {
        self.0.encode(w);
    }
    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        Vec::decode(r).map(Encoded)
    }
}

/// The outcome of a call as the journal records it: the value that it
/// returned, or the message of the runtime error that it raised.
type CallPayload = Result<Encoded, String>;

/// The payload that records a call's outcome `r`.
fn outcome_payload(vm: &mut Vm, r: NResult, label: &str) -> Result<CallPayload, Ctl> {
    match r {
        Ok(v) => match encode_value(vm, v, max_value_bytes(vm)) {
            Ok(bytes) => Ok(Ok(Encoded(bytes))),
            Err(why) => Err(Ctl::fail("unsupported", format!("cannot journal the result of `{label}`: {why}"))),
        },
        Err(c) => match c.into_kind() {
            CtlKind::End(End::RuntimeError(m)) => Ok(Err(m)),
            other => Err(Ctl::new(other)),
        },
    }
}

/// The result of a call that `outcome` records. The value is decoded
/// from its encoding also in a live run, so that record and replay
/// allocate alike.
fn call_outcome(vm: &mut Vm, outcome: &CallPayload) -> NResult {
    match outcome {
        Ok(Encoded(bytes)) => decode_value(vm, bytes),
        Err(m) => Err(Ctl::end(End::RuntimeError(m.clone()))),
    }
}

/// The result of a call that the bytes of a [`CallPayload`] record.
fn payload_outcome(vm: &mut Vm, payload: &[u8]) -> NResult {
    call_outcome(vm, &CallPayload::from_payload(payload).ok_or_else(malformed)?)
}

/// Whether `r` asks for the call to run again after a collection: the
/// journal records only the call that completes.
fn is_retry(c: &Ctl) -> bool {
    matches!(c.kind(), CtlKind::Request(Request::Collect) | CtlKind::End(End::Limit(crate::engine::LimitKind::Memory)))
}

impl Vm {
    /// Fails with kind `"unsupported"` in a journaled run: the journal
    /// cannot record `what`, a raw stream such as a `net` connection, as
    /// whole messages (SPEC §16.7).
    #[cfg(feature = "os")]
    pub(crate) fn refuse_unrecordable(&self, what: &str) -> Result<(), crate::native::convert::HostError> {
        match self.journal {
            Some(_) => Err(crate::native::convert::HostError::new("unsupported", format!("{what} are not available in a journaled run"))),
            None => Ok(()),
        }
    }

    pub(crate) fn journal_state(&self) -> Option<std::cell::RefMut<'_, JournalState>> {
        self.journal.as_ref().map(|j| j.borrow_mut())
    }

    /// Whether the run replays journal entries.
    pub(crate) fn replaying(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.borrow().replaying())
    }

    /// Counts a scheduling step: a slice that a task runs.
    pub(crate) fn count_step(&self) {
        if let Some(j) = &self.journal {
            j.borrow_mut().steps += 1;
        }
    }

    /// Attaches the journal state `j`. Takes the random seed from the
    /// journal, or records it, unless the configuration fixes it.
    pub(crate) fn attach_journal(&mut self, mut j: JournalState, seeded: bool) -> Result<(), Divergence> {
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

    /// The wall-clock time, as the script observes it: journaled. It takes
    /// the [`Call`] of the reading function, which only a local function
    /// has: replay does not call a journaled function.
    pub fn wall_time(&self, _: Call) -> Result<SystemTime, Ctl> {
        let Some(j) = &self.journal else { return Ok(self.host.clock.wall()) };
        let mut j = j.borrow_mut();
        if let Some(EntryData::Wall(t)) = j.take(EntryKind::Wall, &|| "the wall clock".into()).map_err(Ctl::diverged)? {
            j.saw_wall(t);
            return Ok(t);
        }
        let t = j.wall(&*self.host.clock);
        j.saw_wall(t);
        j.record(EntryData::Wall(t));
        Ok(t)
    }

    /// The monotonic time, as the script observes it: journaled. Readings
    /// after replay continue from the last replayed one. It
    /// takes the [`Call`] of the reading function, as [`Vm::wall_time`] does.
    pub fn monotonic(&self, _: Call) -> Result<Instant, Ctl> {
        let Some(j) = &self.journal else { return Ok(self.host.clock.now()) };
        let mut j = j.borrow_mut();
        if let Some(EntryData::Monotonic(d)) = j.take(EntryKind::Monotonic, &|| "the monotonic clock".into()).map_err(Ctl::diverged)? {
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
                return Err(Ctl::diverged(
                    j.diverged(format!("the journal records a fuel charge at {rec_at} units, and the run is at {at}")),
                ));
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

    /// Runs `f`, a synchronous input of the run described by `describe`
    /// (its identity and label), and records its result; in replay, returns
    /// the recorded result without running `f`. Without a journal, returns
    /// what `f` returns. The value must be plain data (see
    /// [`encode_value`]).
    pub(crate) fn input(
        &mut self,
        describe: impl FnOnce(&Vm) -> Result<(u64, String), Ctl>,
        f: impl FnOnce(&mut Vm) -> NResult,
    ) -> NResult {
        if self.journal.is_none() {
            return f(self);
        }
        let (identity, label) = describe(self)?;
        self.journaled_result(identity, &label, f)
    }

    /// Reads `f`, an input of bytes that can be unavailable yet, described
    /// by `describe`. The journal records only an input that is available:
    /// replay returns it if the journal records it next, and otherwise
    /// returns [`Ready::Pending`] without running `f`. Without a journal,
    /// returns what `f` returns.
    pub(crate) fn ready_input(
        &mut self,
        describe: impl FnOnce(&Vm) -> Result<(u64, String), Ctl>,
        f: impl FnOnce(&mut Vm) -> Ready,
    ) -> Result<Ready, Ctl> {
        if self.journal.is_none() {
            return Ok(f(self));
        }
        let (identity, label) = describe(self)?;
        if self.replaying() {
            let recorded = match self.journal_state().as_deref_mut().and_then(|j| match j.peek() {
                Some(EntryData::Result(r)) if r.identity == identity => {
                    let r = r.clone();
                    j.pos += 1;
                    Some(r)
                }
                _ => None,
            }) {
                Some(r) => r,
                None => return Ok(Ready::Pending),
            };
            self.charge(recorded.fuel)?;
            return Option::<Vec<u8>>::from_payload(&recorded.outcome).map(Ready::from).ok_or_else(malformed);
        }
        let start = self.meter.used();
        let data = match f(self) {
            Ready::Pending => return Ok(Ready::Pending),
            Ready::End => None,
            Ready::Data(b) => Some(b),
        };
        let fuel = self.meter.used().saturating_sub(start);
        if let Some(mut j) = self.journal_state() {
            j.record(EntryData::Result(CallResult { identity, label, fuel, outcome: data.to_payload() }));
        }
        Ok(Ready::from(data))
    }

    /// The buffers (`bytes`) and, with `slot: Some((buffers, what))`, an
    /// open-resource slot with `buffers` bytes of fixed buffers for `what`,
    /// of a live operation that the calling native starts. Without a
    /// journal, claims them now from the heap's budget, and asks for a
    /// collection if they do not fit. With one, they come from the
    /// journal's budget of live operations when the operation starts.
    pub(crate) fn claim(
        &mut self,
        call: Call,
        slot: Option<(usize, &str)>,
        bytes: usize,
    ) -> Result<super::iomem::Claim, crate::native::convert::HostError> {
        if let Some(j) = &self.journal {
            return Ok(super::iomem::Claim::Deferred { mem: j.borrow().live_memory.clone(), buffers: slot.map(|s| s.0), bytes });
        }
        let res = if bytes == 0 { self.io_reservation() } else { self.reserve_io(call, bytes)? };
        let slot = match slot {
            Some((buffers, what)) => Some(self.resource_slot_with(call, buffers, what)?),
            None => None,
        };
        Ok(super::iomem::Claim::Held { slot, res })
    }

    /// Runs `f`, a synchronous input of the run identified by `identity`
    /// and described by `label`, and records its result; in replay, returns
    /// the recorded result without running `f`. The value must be plain
    /// data (see [`encode_value`]).
    fn journaled_result(&mut self, identity: u64, label: &str, f: impl FnOnce(&mut Vm) -> NResult) -> NResult {
        if let Some(r) = self.replay_result(identity, label)? {
            return r;
        }
        let start = self.meter.used();
        let r = f(self);
        self.record_result(label, start, r, identity, label.to_string())
    }

    /// Consumes the recorded result of a synchronous call in replay.
    fn replay_result(&mut self, identity: u64, label: &str) -> Result<Option<NResult>, Ctl> {
        let Some(j) = &self.journal else { return Ok(None) };
        let mut j = j.borrow_mut();
        let seq = j.pos as u64;
        let replayed = j.take(EntryKind::Result, &|| format!("the result of `{label}`")).map_err(Ctl::diverged)?;
        drop(j);
        let Some(EntryData::Result(result)) = replayed else { return Ok(None) };
        Ok(Some(Recorded { seq, result }.deliver(self, identity, label)))
    }

    /// Records the result `r` of a synchronous call that started at fuel
    /// `start`, with the call's `identity` and `label`, and returns it.
    fn record_result(&mut self, name: &str, start: u64, r: NResult, identity: u64, label: String) -> NResult {
        if matches!(&r, Err(c) if is_retry(c)) {
            return r;
        }
        let fuel = self.meter.used().saturating_sub(start);
        let outcome = outcome_payload(self, r, name)?;
        if let Some(mut j) = self.journal_state() {
            j.record(EntryData::Result(CallResult { identity, label, fuel, outcome: outcome.to_payload() }));
        }
        call_outcome(self, &outcome)
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
    /// record, and fails if the journal records something else next.
    pub(crate) fn cancel_op(&self, op: u64) -> Result<(), Divergence> {
        let Some(j) = &self.journal else { return Ok(()) };
        let mut j = j.borrow_mut();
        j.local.remove(&op);
        if !j.replaying() {
            j.record_done(op, 0, OpEnd::Cancelled);
            return Ok(());
        }
        if j.replayed.remove(&op).is_none() {
            // A live operation that replay started: the journal does not
            // record its start's end, so the cancellation lies beyond it.
            return Ok(());
        }
        match j.peek() {
            Some(EntryData::Done(OpDone { op: o, end: OpEnd::Cancelled, .. })) if *o == op => {
                j.pos += 1;
                Ok(())
            }
            next => {
                let msg =
                    format!("the run cancels operation {op} where the journal records {}", next.map_or("nothing".into(), |e| e.describe()));
                Err(j.diverged(msg))
            }
        }
    }

    /// What the journal holds for the next operation, of `kind`. `describe`
    /// gives the operation's identity and label, which only a journaled run
    /// reads. Consumes the operation's `Start` entry if the journal also
    /// records its end; otherwise consumes nothing.
    pub(crate) fn take_start(&self, kind: OpKind, describe: impl FnOnce(&Vm) -> Result<(u64, String), Ctl>) -> Result<StartEntry, Ctl> {
        let Some(journal) = &self.journal else { return Ok(StartEntry::Unjournaled) };
        let (identity, label) = describe(self)?;
        let mut j = journal.borrow_mut();
        let s = match j.peek() {
            None => return Ok(StartEntry::Fresh(Fresh { identity, label })),
            Some(EntryData::Start(s)) => s,
            Some(EntryData::Result(r)) => {
                let result = Recorded { seq: j.pos as u64, result: r.clone() };
                j.pos += 1;
                return Ok(StartEntry::Recorded { identity, label, result });
            }
            Some(e) => return Ok(StartEntry::Other { label, seq: j.pos as u64, next: e.describe() }),
        };
        let (op, seq) = (j.ops, j.pos as u64);
        if s.kind != kind || s.identity != identity {
            let message = if s.kind == kind && s.label == label {
                format!("the run starts {} `{label}` with other arguments than the journal records", kind.name())
            } else {
                format!("the run starts {} `{label}` where the journal records the start of {} `{}`", kind.name(), s.kind.name(), s.label)
            };
            return Err(Ctl::diverged(Divergence { seq, message }));
        }
        let (pre_fuel, wake) = (s.pre_fuel, s.wake);
        if !j.done.contains_key(&op) {
            return Ok(StartEntry::Rerun(Rerun { op, seq, wake }));
        }
        j.pos += 1;
        j.ops += 1;
        Ok(StartEntry::Replayed(Replayed { op, pre_fuel, wake }))
    }

    /// Starts operation `spec`, an input of the run whose outcome the
    /// journal records, for the calling native. `describe` gives the
    /// operation's identity and label, which only a journaled run reads.
    /// Consumes the operation's `Start` entry in replay, or records one.
    /// Without a journal, the operation runs.
    pub(crate) fn begin_op(&mut self, spec: OpSpec, describe: impl FnOnce(&Vm) -> Result<(u64, String), Ctl>) -> Result<Begun, Ctl> {
        match self.take_start(spec.kind, describe)? {
            StartEntry::Replayed(r) => Ok(Begun::Replayed(r)),
            StartEntry::Rerun(r) => Ok(Begun::Live(r.start(self, spec))),
            StartEntry::Fresh(f) => Ok(Begun::Live(f.start(self, spec))),
            StartEntry::Unjournaled => Ok(Begun::Live(Live::unjournaled(spec))),
            StartEntry::Recorded { label, result: Recorded { seq, result }, .. } => {
                let next = EntryData::Result(result).describe();
                let message = format!("the run reads the start of {} `{label}` where the journal records {next}", spec.kind.name());
                Err(Ctl::diverged(Divergence { seq, message }))
            }
            StartEntry::Other { label, seq, next } => {
                let message = format!("the run reads the start of {} `{label}` where the journal records {next}", spec.kind.name());
                Err(Ctl::diverged(Divergence { seq, message }))
            }
        }
    }

    /// Notes that the local operation that fiber `fid` waits for, as wait
    /// generation `wgen`, did not complete at once. The journal records
    /// its start and, when it completes, the step at which the engine
    /// delivers it. Replay consumes the start, runs the future again, and
    /// delivers its result after the recorded step, so that tasks
    /// interleave as they did.
    pub(crate) fn local_pending(&mut self, fid: u32, wgen: u64) -> Result<(), Ctl> {
        let Some(j) = &self.journal else { return Ok(()) };
        let mut j = j.borrow_mut();
        let id = j.ops;
        let seq = j.pos as u64;
        let what = || format!("the start of {}", OpKind::Local.name());
        let replayed = match j.take(EntryKind::Start, &what).map_err(Ctl::diverged)? {
            Some(EntryData::Start(s)) if s.kind == OpKind::Local => true,
            Some(EntryData::Start(s)) => {
                let message =
                    format!("the run waits for a local operation where the journal records the start of {} `{}`", s.kind.name(), s.label);
                return Err(Ctl::diverged(Divergence { seq, message }));
            }
            _ => {
                let start = OpStart { kind: OpKind::Local, identity: 0, pre_fuel: 0, wake: None, label: "wait".to_string() };
                j.record(EntryData::Start(start));
                false
            }
        };
        j.ops += 1;
        if replayed && j.done.contains_key(&id) {
            j.replayed.insert(id, (fid, wgen, Replaying::Local(None)));
        } else {
            j.local.insert(id);
        }
        drop(j);
        if let Some(Some(f)) = self.fibers.get_mut(fid as usize) {
            f.jop = Some(id);
        }
        Ok(())
    }

    /// Records the `Done` entry of the local operation that fiber `fid`
    /// waits for, if it is live, before the engine delivers its result.
    pub(crate) fn local_done(&mut self, fid: u32) {
        let Some(j) = &self.journal else { return };
        let Some(Some(f)) = self.fibers.get(fid as usize) else { return };
        let Some(op) = f.jop else { return };
        let mut j = j.borrow_mut();
        if j.local.remove(&op) {
            j.record_done(op, 0, OpEnd::Completed(Vec::new()));
        }
    }

    /// The completions in `done` to deliver now. Replay delivers each
    /// recorded completion at its step, and holds the completions of
    /// operations that run live until it is over.
    pub(crate) fn hold_in_replay(&mut self, done: Vec<(u32, u64, Finisher)>) -> Vec<(u32, u64, Finisher)> {
        let Some(j) = &self.journal else { return done };
        let mut j = j.borrow_mut();
        if !j.replaying() {
            return done;
        }
        let mut now = Vec::new();
        for (fid, wgen, fin) in done {
            let op = match self.fibers.get(fid as usize) {
                Some(Some(f)) if f.wgen == wgen => f.jop,
                _ => None,
            };
            match op.and_then(|op| j.replayed.get_mut(&op)) {
                Some((_, _, Replaying::Local(slot @ None))) => *slot = Some(fin),
                _ if op.is_none() => now.push((fid, wgen, fin)),
                _ => j.held.push((fid, wgen, fin)),
            }
        }
        now
    }

    /// The next completion that replay delivers now: one recorded after the
    /// current step. Fails if one recorded after an earlier step is pending.
    pub(crate) fn next_due(&mut self) -> Result<Due, Ctl> {
        let Some(j) = &self.journal else { return Ok(Due::Nothing) };
        let mut j = j.borrow_mut();
        let Some(EntryData::Done(OpDone { op, step, fuel, end })) = j.peek() else { return Ok(Due::Nothing) };
        let (op, step, fuel) = (*op, *step, *fuel);
        let OpEnd::Completed(payload) = end else {
            // Consumed where the run cancels the operation.
            return Ok(Due::Nothing);
        };
        let payload = payload.clone();
        if step > j.steps {
            return Ok(Due::Nothing);
        }
        if step < j.steps {
            let msg = format!("the journal records the completion of operation {op} after step {step}, and the run is at step {}", j.steps);
            return Err(Ctl::diverged(j.diverged(msg)));
        }
        match j.replayed.remove(&op) {
            None => {
                let msg = format!("the journal records the completion of operation {op}, which the run has not started");
                Err(Ctl::diverged(j.diverged(msg)))
            }
            Some((fid, wgen, Replaying::Local(None))) => {
                j.replayed.insert(op, (fid, wgen, Replaying::Local(None)));
                Ok(Due::Waiting)
            }
            Some((fid, wgen, Replaying::Local(Some(fin)))) => {
                j.pos += 1;
                Ok(Due::Ready(fid, wgen, fin))
            }
            Some((fid, wgen, Replaying::Recorded(build))) => {
                j.pos += 1;
                let fin = Box::new(move |vm: &mut Vm| {
                    let v = build(vm, &payload);
                    vm.charge(fuel)?;
                    v
                });
                Ok(Due::Ready(fid, wgen, fin))
            }
        }
    }

    /// The divergence error for a run that can do nothing more while
    /// replay holds more entries.
    pub(crate) fn blocked_in_replay(&self) -> Option<Divergence> {
        let j = self.journal.as_ref()?.borrow();
        let e = j.peek()?;
        Some(j.diverged(format!("every task is blocked, and the journal records {} next", e.describe())))
    }
}

#[cfg(test)]
mod tests {
    use super::{Payload, Ready};
    use crate::journal::{HttpOutcome, HttpResponse};
    use crate::stdlib::core::Error;

    fn round_trip<P: Payload + PartialEq + std::fmt::Debug>(p: P) {
        let bytes = p.to_payload();
        assert_eq!(P::from_payload(&bytes), Some(p));
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(P::from_payload(&longer), None, "a payload with trailing bytes is malformed");
        assert_eq!(P::from_payload(&bytes[..bytes.len() - 1]), None, "a cut payload is malformed");
    }

    #[test]
    fn payloads_of_results_share_one_encoding() {
        let failed = || Error { kind: "timeout".into(), message: "late".into() };
        round_trip::<Result<Option<Vec<u8>>, Error>>(Ok(Some(b"line".to_vec())));
        round_trip::<Result<Option<Vec<u8>>, Error>>(Ok(None));
        round_trip::<Result<Option<Vec<u8>>, Error>>(Err(failed()));
        round_trip::<HttpOutcome>(Ok(HttpResponse { status: 200, headers: vec![("a".into(), "b".into())], body: b"hi".to_vec() }));
        round_trip::<HttpOutcome>(Err(failed()));
        let stdin_err = Result::<Option<Vec<u8>>, Error>::Err(failed()).to_payload();
        let http_err = HttpOutcome::Err(failed()).to_payload();
        assert_eq!(stdin_err, http_err);
    }

    #[test]
    fn a_ready_input_records_end_and_data() {
        assert!(matches!(Ready::from(None), Ready::End));
        assert!(matches!(Ready::from(Some(vec![1])), Ready::Data(d) if d == [1]));
    }
}
