//! Durable execution: the journal of the nondeterministic inputs of a run.
//!
//! A host makes runs durable with [`Config::durable`](crate::engine::Config::durable).
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
///
/// The sequence number of an entry is its position in the journal, from 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    /// The engine's encoding of the input. It is stable for one version of
    /// `sandlang`.
    pub data: Vec<u8>,
}

/// Declares a field-less enum with a one-byte code and a lowercase name for
/// each variant. Each variant must have a code, and the compiler rejects a
/// duplicate code.
macro_rules! coded_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $code:literal, $text:literal;)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum $name {
            $($(#[$vmeta])* $variant = $code,)*
        }

        impl $name {
            /// A one-byte code for the kind.
            pub fn code(self) -> u8 {
                self as u8
            }

            /// The kind with code `code`, if any.
            pub fn from_code(code: u8) -> Option<$name> {
                match code {
                    $($code => Some($name::$variant),)*
                    _ => None,
                }
            }

            /// The name of the kind in lowercase.
            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)*
                }
            }
        }
    };
}

coded_enum! {
    /// What an [`Entry`] records.
    pub enum EntryKind {
        /// The seed of the random number generator, if `Config::seed` does not set it.
        Seed = 0, "seed";
        /// A reading of the wall clock (`time.now`, UUIDv7 timestamps).
        Wall = 1, "wall";
        /// A reading of the monotonic clock (`time.Stopwatch`).
        Monotonic = 2, "monotonic";
        /// Fuel that a charge by elapsed time added (regex work).
        Fuel = 3, "fuel";
        /// The start of an asynchronous operation: its kind, its identity, and a
        /// timer's wake time.
        Start = 4, "start";
        /// The completion of an asynchronous operation: the scheduling step at
        /// which the engine delivered it, and its result.
        Done = 5, "done";
        /// The result of a synchronous journaled call, such as a host function
        /// registered as [`Journaled`](crate::native::func::Journaled) or an
        /// environment variable read from the process.
        Result = 6, "result";
    }
}

impl Entry {
    /// Encodes the entry as bytes: the kind code (1 byte), then the data.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        out.push(self.kind.code());
        out.extend_from_slice(&self.data);
        out
    }

    /// The length of [`Entry::to_bytes`].
    pub fn byte_len(&self) -> usize {
        1 + self.data.len()
    }

    /// Decodes an entry that [`Entry::to_bytes`] encoded.
    pub fn from_bytes(bytes: &[u8]) -> Option<Entry> {
        let (&kind, data) = bytes.split_first()?;
        Some(Entry { kind: EntryKind::from_code(kind)?, data: data.to_vec() })
    }

    /// Decodes the data of the entry, or returns `None` if it is malformed.
    pub fn decode(&self) -> Option<EntryData> {
        EntryData::decode(self.kind, &self.data)
    }

    /// A short description of the entry for messages, such as
    /// ``the start of http `GET http://example.com/` ``.
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

coded_enum! {
    /// What an asynchronous operation is.
    pub enum OpKind {
        /// `task.sleep` and the functions that use it.
        Timer = 0, "timer";
        /// An HTTP request, recorded as a whole message.
        Http = 1, "http";
        /// A read of standard input.
        Stdin = 2, "stdin";
        /// A call of an asynchronous host function.
        Call = 3, "call";
        /// A local operation, such as a read of a file or a call of a host
        /// function that is not journaled: replay runs it again. Only the
        /// point in the schedule where the engine delivered it is recorded.
        Local = 4, "local";
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
    for (seq, e) in entries.iter().enumerate() {
        if !matches!(e.kind, EntryKind::Start | EntryKind::Done) {
            continue;
        }
        match e.decode() {
            Some(EntryData::Start(start)) => out.push(Operation { id: out.len() as u64, seq: seq as u64, start, done: None }),
            Some(EntryData::Done(done)) => {
                let op = done.op;
                let slot = out.get_mut(op as usize).ok_or_else(|| format!("entry {seq} ends operation {op}, which has not started"))?;
                if slot.done.is_some() {
                    return Err(format!("entry {seq} ends operation {op} a second time"));
                }
                slot.done = Some(done);
            }
            _ => return Err(format!("entry {seq} is malformed")),
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

/// The durable execution of an engine's runs ([`Config::durable`](crate::engine::Config::durable)).
#[derive(Clone)]
pub struct Durable {
    /// Where the engine records the inputs of each run, and the entries that
    /// it replays.
    pub journal: Arc<dyn Journal>,
    /// Suspends a run when every task waits for a timer or a parked host
    /// call that is due more than this much later, instead of waiting: the
    /// run returns [`Outcome::Suspended`](crate::engine::Outcome::Suspended). `None`
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
pub type FlushFuture = Pin<Box<dyn Future<Output = Result<(), crate::engine::HostAbort>> + Send>>;

/// A journaled run that the engine suspended because every task waits
/// ([`Outcome::Suspended`](crate::engine::Outcome::Suspended)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspension {
    /// When the first timer or parked host call is due, or `None` if only
    /// host calls parked until the host resumes the run wait.
    pub wake_at: Option<SystemTime>,
    /// The host calls that parked and wait. The host may resume the run
    /// before `wake_at`, when the condition of one of them occurs.
    pub parked: Vec<ParkedCall>,
}

/// A host call that waits in a suspended run
/// ([`Later::parked`](crate::native::func::Later::parked)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedCall {
    /// The host function, as `module.name`.
    pub function: String,
    /// The tag that the function gave to [`Later::parked`](crate::native::func::Later::parked).
    pub tag: String,
    /// When the call is due, or `None` if it waits until the host resumes
    /// the run.
    pub wake_at: Option<SystemTime>,
}

/// The storage of a run's journal, supplied by the host.
///
/// The engine calls [`Journal::replay`] once when a run starts,
/// [`Journal::record`] for each new entry, and [`Journal::flush`] before each
/// externally visible effect, before it suspends the run, and when the run
/// ends.
pub trait Journal: Send + Sync {
    /// Returns all entries that earlier executions of the run recorded, in
    /// order. The engine replays them and records new entries after them.
    ///
    /// Replay needs every entry before the run starts: when the run starts
    /// an operation, the engine replays the operation only if a later entry
    /// records its end, and runs it live otherwise.
    fn replay(&self) -> Vec<Entry>;

    /// Records the next entry, after all entries that [`Journal::replay`]
    /// returned and that this execution recorded. The entry needs to be
    /// durable only once a later [`Journal::flush`] completes, so an
    /// implementation can buffer it.
    fn record(&self, entry: Entry);

    /// Makes all recorded entries durable. The engine does not start the
    /// effect that it flushes for until the future completes, and does not
    /// count the wait against the wall-time limit. An error ends the run
    /// with [`Error::Aborted`](crate::engine::Error::Aborted), which holds it.
    ///
    /// An implementation can make the entries durable in parts. If a part
    /// fails, the durable entries are a valid journal: any prefix of the
    /// recorded entries is one. The engine starts an effect only after the
    /// flush before it completes, so no effect depends on an entry after the
    /// prefix. Replay runs an operation live if the journal records no end
    /// for it, so an operation that the prefix cuts runs again.
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
