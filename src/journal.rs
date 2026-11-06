//! Durable execution: the journal of the nondeterministic inputs of a run.
//!
//! A host makes runs durable with [`Config::durable`](crate::Config::durable).
//! The engine then records, in order, every input that can differ between two
//! executions of the same program: clock readings, the random seed, the
//! completion of each asynchronous operation and the point in the schedule
//! where the engine delivered it, HTTP responses, standard input, and the
//! results of journaled host functions. When the host runs the same program
//! again with the same journal, the engine replays the recorded entries
//! instead of performing their effects, reaches the same state, and continues
//! live after the last entry. SPEC §16.7 gives the full contract.

use crate::platform::{SystemTime, UNIX_EPOCH};
use crate::vm::journal::{Reader, Writer};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One recorded input of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The position of the entry in the journal, from 0.
    pub seq: u64,
    pub kind: EntryKind,
    /// The engine's encoding of the input. It is stable for one version of
    /// `sandlang`.
    pub data: Vec<u8>,
}

/// What an [`Entry`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// The seed of the random number generator, if `Config::seed` does not set it.
    Seed,
    /// A reading of the wall clock (`time.now`, UUIDv7 timestamps).
    Wall,
    /// A reading of the monotonic clock (`time.Stopwatch`).
    Monotonic,
    /// Fuel that a charge by elapsed time added (regex work).
    Fuel,
    /// The start of an asynchronous operation: its kind, its identity, and a
    /// timer's wake time.
    Start,
    /// The completion of an asynchronous operation: the scheduling step at
    /// which the engine delivered it, and its result.
    Done,
    /// The result of a synchronous journaled call, such as a host function
    /// registered as [`Journaled`](crate::native::Journaled) or an
    /// environment variable read from the process.
    Result,
}

impl EntryKind {
    const ALL: [EntryKind; 7] =
        [EntryKind::Seed, EntryKind::Wall, EntryKind::Monotonic, EntryKind::Fuel, EntryKind::Start, EntryKind::Done, EntryKind::Result];

    /// A one-byte code for the kind.
    pub fn code(self) -> u8 {
        EntryKind::ALL.iter().position(|&k| k == self).unwrap_or(0) as u8
    }

    /// The kind with code `code`, if any.
    pub fn from_code(code: u8) -> Option<EntryKind> {
        EntryKind::ALL.get(usize::from(code)).copied()
    }

    /// The name of the kind in lowercase, such as `"start"`.
    pub fn name(self) -> &'static str {
        match self {
            EntryKind::Seed => "seed",
            EntryKind::Wall => "wall",
            EntryKind::Monotonic => "monotonic",
            EntryKind::Fuel => "fuel",
            EntryKind::Start => "start",
            EntryKind::Done => "done",
            EntryKind::Result => "result",
        }
    }
}

impl Entry {
    /// Encodes the entry as bytes: the sequence number (8 bytes, little
    /// endian), the kind code (1 byte), then the data.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + self.data.len());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.push(self.kind.code());
        out.extend_from_slice(&self.data);
        out
    }

    /// Decodes an entry that [`Entry::to_bytes`] encoded.
    pub fn from_bytes(bytes: &[u8]) -> Option<Entry> {
        let (seq, rest) = bytes.split_first_chunk::<8>()?;
        let (&kind, data) = rest.split_first()?;
        Some(Entry { seq: u64::from_le_bytes(*seq), kind: EntryKind::from_code(kind)?, data: data.to_vec() })
    }

    /// Decodes the data of the entry, or returns `None` if it is malformed.
    pub fn decode(&self) -> Option<EntryData> {
        EntryData::decode(self.kind, &self.data)
    }

    /// A short description of the entry for messages, such as
    /// `the start of http `GET http://example.com/``.
    pub fn describe(&self) -> String {
        match self.decode() {
            Some(d) => d.describe(),
            None => format!("a malformed {} entry", self.kind.name()),
        }
    }
}

/// The decoded data of an [`Entry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryData {
    /// The seed of the random number generator.
    Seed(u64),
    /// A reading of the wall clock.
    Wall(SystemTime),
    /// A reading of the monotonic clock: the time since the run started.
    Monotonic(Duration),
    /// A charge of `extra` fuel by elapsed time, when the run had used `at`
    /// fuel.
    Fuel {
        at: u64,
        extra: u64,
    },
    Start(OpStart),
    Done(OpDone),
    Result(CallResult),
}

/// What an asynchronous operation is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// `task.sleep` and the functions that use it.
    Timer,
    /// An HTTP request, recorded as a whole message.
    Http,
    /// A read of standard input.
    Stdin,
    /// A call of an asynchronous host function.
    Call,
    /// A local operation, such as a read of a file or a call of a host
    /// function that is not journaled: replay runs it again. Only the
    /// point in the schedule where the engine delivered it is recorded.
    Local,
}

impl OpKind {
    const ALL: [OpKind; 5] = [OpKind::Timer, OpKind::Http, OpKind::Stdin, OpKind::Call, OpKind::Local];

    fn code(self) -> u8 {
        OpKind::ALL.iter().position(|&k| k == self).unwrap_or(0) as u8 + 1
    }

    fn from_code(c: u8) -> Option<OpKind> {
        OpKind::ALL.get(usize::from(c).checked_sub(1)?).copied()
    }

    /// The name of the kind in lowercase, such as `"http"`.
    pub fn name(self) -> &'static str {
        match self {
            OpKind::Timer => "timer",
            OpKind::Http => "http",
            OpKind::Stdin => "stdin",
            OpKind::Call => "call",
            OpKind::Local => "local",
        }
    }
}

/// The start of an asynchronous operation ([`EntryKind::Start`]). The
/// operation's id is the number of operations that the run started before
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpStart {
    pub kind: OpKind,
    /// A hash of what the operation does, such as an HTTP request's method,
    /// URL, headers and body.
    pub identity: u64,
    /// Fuel that the native spent before it started the operation.
    pub pre_fuel: u64,
    /// When a timer expires.
    pub wake: Option<SystemTime>,
    /// A description, such as `GET http://example.com/` or `sleep 5s`.
    pub label: String,
}

/// The end of an asynchronous operation ([`EntryKind::Done`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpDone {
    /// The id of the operation.
    pub op: u64,
    /// The scheduling step after which the engine delivered the result.
    pub step: u64,
    /// Fuel that building the result spent.
    pub fuel: u64,
    pub end: OpEnd,
}

/// How an asynchronous operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpEnd {
    /// The operation completed with this result, in an encoding that
    /// depends on the kind of operation (see [`Operation::http`]).
    Completed(Vec<u8>),
    /// The task that waited for the operation was cancelled.
    Cancelled,
}

/// The result of a synchronous journaled call ([`EntryKind::Result`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CallResult {
    /// A hash of the call: the function and its arguments.
    pub identity: u64,
    /// The name of the function or the input.
    pub label: String,
    /// Fuel that the call spent.
    pub fuel: u64,
    /// The result, in the engine's encoding of values.
    pub outcome: Vec<u8>,
}

impl EntryData {
    pub(crate) fn kind(&self) -> EntryKind {
        match self {
            EntryData::Seed(_) => EntryKind::Seed,
            EntryData::Wall(_) => EntryKind::Wall,
            EntryData::Monotonic(_) => EntryKind::Monotonic,
            EntryData::Fuel { .. } => EntryKind::Fuel,
            EntryData::Start(_) => EntryKind::Start,
            EntryData::Done(_) => EntryKind::Done,
            EntryData::Result(_) => EntryKind::Result,
        }
    }

    /// The bytes of an [`Entry`] of kind [`EntryData::kind`].
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            EntryData::Seed(s) => {
                w.u64(*s);
            }
            EntryData::Wall(t) => {
                w.i128(nanos_of(*t));
            }
            EntryData::Monotonic(d) => {
                w.u64(d.as_nanos().min(u64::MAX as u128) as u64);
            }
            EntryData::Fuel { at, extra } => {
                w.u64(*at).u64(*extra);
            }
            EntryData::Start(s) => {
                w.u8(s.kind.code()).u64(s.identity).u64(s.pre_fuel);
                match s.wake {
                    Some(t) => w.u8(1).i128(nanos_of(t)),
                    None => w.u8(0),
                };
                w.str(&s.label);
            }
            EntryData::Done(d) => {
                w.u64(d.op).u64(d.step).u64(d.fuel);
                match &d.end {
                    OpEnd::Completed(payload) => w.u8(0).raw(payload),
                    OpEnd::Cancelled => w.u8(1),
                };
            }
            EntryData::Result(r) => {
                w.u64(r.identity).str(&r.label).u64(r.fuel).raw(&r.outcome);
            }
        }
        w.0
    }

    fn decode(kind: EntryKind, data: &[u8]) -> Option<EntryData> {
        let mut r = Reader::new(data);
        let d = match kind {
            EntryKind::Seed => EntryData::Seed(r.u64()?),
            EntryKind::Wall => EntryData::Wall(time_of(r.i128()?)),
            EntryKind::Monotonic => EntryData::Monotonic(Duration::from_nanos(r.u64()?)),
            EntryKind::Fuel => EntryData::Fuel { at: r.u64()?, extra: r.u64()? },
            EntryKind::Start => {
                let (kind, identity, pre_fuel) = (OpKind::from_code(r.u8()?)?, r.u64()?, r.u64()?);
                let wake = match r.u8()? {
                    0 => None,
                    1 => Some(time_of(r.i128()?)),
                    _ => return None,
                };
                EntryData::Start(OpStart { kind, identity, pre_fuel, wake, label: r.str()?.to_string() })
            }
            EntryKind::Done => {
                let (op, step, fuel) = (r.u64()?, r.u64()?, r.u64()?);
                let end = match r.u8()? {
                    0 => OpEnd::Completed(r.rest().to_vec()),
                    1 => OpEnd::Cancelled,
                    _ => return None,
                };
                EntryData::Done(OpDone { op, step, fuel, end })
            }
            EntryKind::Result => {
                let (identity, label, fuel) = (r.u64()?, r.str()?.to_string(), r.u64()?);
                EntryData::Result(CallResult { identity, label, fuel, outcome: r.rest().to_vec() })
            }
        };
        r.is_empty().then_some(d)
    }

    /// A short description for messages.
    pub fn describe(&self) -> String {
        match self {
            EntryData::Start(s) => format!("the start of {} `{}`", s.kind.name(), s.label),
            EntryData::Done(OpDone { op, end: OpEnd::Cancelled, .. }) => format!("the cancellation of operation {op}"),
            EntryData::Done(OpDone { op, step, .. }) => format!("the completion of operation {op} after step {step}"),
            EntryData::Result(r) => format!("the result of `{}`", r.label),
            EntryData::Seed(_) => "the random seed".into(),
            EntryData::Wall(_) => "a wall-clock reading".into(),
            EntryData::Monotonic(_) => "a monotonic clock reading".into(),
            EntryData::Fuel { .. } => "a fuel charge by time".into(),
        }
    }
}

/// An asynchronous operation of a journal: its start and, if the journal
/// records it, its end.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Operation {
    /// The id of the operation: the number of operations that the run
    /// started before it.
    pub id: u64,
    /// The sequence number of the operation's `Start` entry.
    pub seq: u64,
    pub start: OpStart,
    pub done: Option<OpDone>,
}

impl Operation {
    /// The outcome of an HTTP request that completed.
    pub fn http(&self) -> Option<HttpOutcome> {
        match (&self.start.kind, &self.done) {
            (OpKind::Http, Some(OpDone { end: OpEnd::Completed(payload), .. })) => HttpOutcome::decode(payload),
            _ => None,
        }
    }

    /// Whether the task that waited for the operation was cancelled.
    pub fn cancelled(&self) -> bool {
        matches!(&self.done, Some(OpDone { end: OpEnd::Cancelled, .. }))
    }
}

/// The asynchronous operations that `entries` record, by id. Fails with a
/// message if an entry does not decode or an operation ends twice or before
/// it starts.
pub fn operations(entries: &[Entry]) -> Result<Vec<Operation>, String> {
    let mut out: Vec<Operation> = Vec::new();
    for e in entries {
        if !matches!(e.kind, EntryKind::Start | EntryKind::Done) {
            continue;
        }
        match e.decode() {
            Some(EntryData::Start(start)) => out.push(Operation { id: out.len() as u64, seq: e.seq, start, done: None }),
            Some(EntryData::Done(done)) => {
                let op = done.op;
                let slot = out.get_mut(op as usize).ok_or_else(|| format!("entry {} ends operation {op}, which has not started", e.seq))?;
                if slot.done.is_some() {
                    return Err(format!("entry {} ends operation {op} a second time", e.seq));
                }
                slot.done = Some(done);
            }
            _ => return Err(format!("entry {} is malformed", e.seq)),
        }
    }
    Ok(out)
}

/// The outcome of a journaled HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpOutcome {
    /// The response, with its whole body.
    Response { status: i64, headers: Vec<(String, String)>, body: Vec<u8> },
    /// The request failed with an error of `kind`.
    Failed { kind: String, message: String },
}

impl HttpOutcome {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            HttpOutcome::Response { status, headers, body } => {
                w.u8(0).i64(*status).u64(headers.len() as u64);
                for (k, v) in headers {
                    w.str(k).str(v);
                }
                w.bytes(body);
            }
            HttpOutcome::Failed { kind, message } => {
                w.u8(1).str(kind).str(message);
            }
        }
        w.0
    }

    pub(crate) fn decode(data: &[u8]) -> Option<HttpOutcome> {
        let mut r = Reader::new(data);
        let out = match r.u8()? {
            0 => {
                let status = r.i64()?;
                let n = r.u64()?;
                let mut headers = Vec::new();
                for _ in 0..n {
                    headers.push((r.str()?.to_string(), r.str()?.to_string()));
                }
                HttpOutcome::Response { status, headers, body: r.bytes()?.to_vec() }
            }
            1 => HttpOutcome::Failed { kind: r.str()?.to_string(), message: r.str()?.to_string() },
            _ => return None,
        };
        r.is_empty().then_some(out)
    }
}

pub(crate) fn nanos_of(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

pub(crate) fn time_of(nanos: i128) -> SystemTime {
    let d = Duration::from_nanos(nanos.unsigned_abs().min(u64::MAX as u128) as u64);
    if nanos >= 0 { UNIX_EPOCH + d } else { UNIX_EPOCH - d }
}

/// The durable execution of an engine's runs ([`Config::durable`](crate::Config::durable)).
#[derive(Clone)]
pub struct Durable {
    /// Where the engine records the inputs of each run, and the entries that
    /// it replays.
    pub journal: Arc<dyn Journal>,
    /// Suspends a run when every task waits for a timer or a parked host
    /// call that is due more than this much later, instead of waiting: the
    /// run returns [`Outcome::Suspended`](crate::Outcome::Suspended). `None`
    /// waits for every operation.
    pub suspend_after: Option<Duration>,
}

impl Durable {
    /// Durable execution with `journal`, without suspension.
    pub fn new(journal: Arc<dyn Journal>) -> Durable {
        Durable { journal, suspend_after: None }
    }

    /// Sets [`Durable::suspend_after`].
    pub fn suspend_after(mut self, threshold: Duration) -> Durable {
        self.suspend_after = Some(threshold);
        self
    }
}

/// The future that [`Journal::flush`] returns.
pub type FlushFuture = Pin<Box<dyn Future<Output = Result<(), JournalError>> + Send>>;

/// Why a [`Journal`] failed to flush: the host's error, which the run's
/// [`Error::Journal`](crate::Error::Journal) gives back to the host.
#[derive(Clone)]
pub struct JournalError(Arc<dyn std::error::Error + Send + Sync>);

impl JournalError {
    pub fn new(e: impl std::error::Error + Send + Sync + 'static) -> JournalError {
        JournalError(Arc::new(e))
    }

    /// An error with only a message.
    pub fn message(m: impl Into<String>) -> JournalError {
        JournalError::new(Message(m.into()))
    }

    /// The host's error, if it has type `T`.
    pub fn downcast_ref<T: std::error::Error + 'static>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Debug for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JournalError({})", self.0)
    }
}

#[derive(Debug)]
struct Message(String);

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Message {}

/// A journaled run that the engine suspended because every task waits
/// ([`Outcome::Suspended`](crate::Outcome::Suspended)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Suspension {
    /// When the first timer or parked host call is due, or `None` if only
    /// host calls parked until the host resumes the run wait.
    pub wake_at: Option<SystemTime>,
    /// Whether a host call that parked waits, so that the host may resume
    /// the run before `wake_at`, when the call's condition occurs.
    pub parked: bool,
}

/// The storage of a run's journal, supplied by the host.
///
/// The engine calls [`Journal::replay`] once when a run starts,
/// [`Journal::record`] for each new entry, and [`Journal::flush`] before each
/// externally visible effect, before it suspends the run, and when the run
/// ends.
pub trait Journal: Send + Sync {
    /// Returns the entries that earlier executions of the run recorded, in
    /// order. The engine replays them and records new entries after them.
    fn replay(&self) -> Vec<Entry>;

    /// Records a new entry. The entry needs to be durable only once a later
    /// [`Journal::flush`] completes, so an implementation can buffer it.
    fn record(&self, entry: Entry);

    /// Makes all recorded entries durable. The engine does not start the
    /// effect that it flushes for until the future completes, and does not
    /// count the wait against the wall-time limit. An error ends the run
    /// with [`Error::Journal`](crate::Error::Journal), which holds it.
    fn flush(&self) -> FlushFuture;
}

/// A [`Journal`] that keeps its entries in memory, for tests and for hosts
/// that store the entries themselves after the run.
///
/// Clones share the same entries.
#[derive(Clone, Default)]
pub struct MemoryJournal(Arc<Mutex<MemoryState>>);

#[derive(Default)]
struct MemoryState {
    entries: Vec<Entry>,
    durable: usize,
    flushes: u64,
}

impl MemoryJournal {
    /// An empty journal: the first run records from the start.
    pub fn new() -> MemoryJournal {
        MemoryJournal::default()
    }

    /// A journal that holds `entries`, all durable: the next run replays them.
    pub fn with_entries(entries: Vec<Entry>) -> MemoryJournal {
        let durable = entries.len();
        MemoryJournal(Arc::new(Mutex::new(MemoryState { entries, durable, flushes: 0 })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// All entries, including those not flushed yet.
    pub fn entries(&self) -> Vec<Entry> {
        self.state().entries.clone()
    }

    /// The entries that a flush made durable.
    pub fn durable(&self) -> Vec<Entry> {
        let s = self.state();
        s.entries[..s.durable].to_vec()
    }

    /// The number of completed flushes.
    pub fn flushes(&self) -> u64 {
        self.state().flushes
    }
}

impl Journal for MemoryJournal {
    fn replay(&self) -> Vec<Entry> {
        self.entries()
    }

    fn record(&self, entry: Entry) {
        self.state().entries.push(entry);
    }

    fn flush(&self) -> FlushFuture {
        let mut s = self.state();
        s.durable = s.entries.len();
        s.flushes += 1;
        Box::pin(std::future::ready(Ok(())))
    }
}
