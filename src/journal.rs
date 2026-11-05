//! Durable execution: the journal of the nondeterministic inputs of a run.
//!
//! A host sets a [`Journal`] with [`Config::journal`](crate::Config::journal).
//! The engine then records, in order, every input that can differ between two
//! executions of the same program: clock readings, the random seed, the
//! completion of each asynchronous operation and the point in the schedule
//! where the engine delivered it, HTTP responses, standard input, and the
//! results of journaled host functions. When the host runs the same program
//! again with the same journal, the engine replays the recorded entries
//! instead of performing their effects, reaches the same state, and continues
//! live after the last entry. SPEC §16.7 gives the full contract.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

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
    /// marked with [`NativeModule::journaled`](crate::NativeModule::journaled)
    /// or an environment variable read from the process.
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

    /// A short description of the entry for messages, such as
    /// `start of http GET http://example.com/`.
    pub fn describe(&self) -> String {
        crate::vm::journal::describe(self)
    }
}

/// The future that [`Journal::flush`] returns.
pub type FlushFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

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
    /// effect that it flushes for until the future completes. An error ends
    /// the run with [`Error::Journal`](crate::Error::Journal).
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
