//! The Arm 3 byte accounting and reference-count cycle collector.
//!
//! Every container registers a `Weak` handle in a slot of the cycle table.
//! Dropping a container's last `Arc` drops its contents at once, but its
//! tracked bytes stay counted until a collection prunes the dead slot,
//! because the `Weak` handle keeps the `Arc` allocation alive until then.
//!
//! A collection finds garbage cycles like CPython: it subtracts every
//! reference that one container holds to another from the reference counts.
//! A container with references left over is held from outside the table (by
//! a register, a global, or native code) and is a root. The collector clears
//! the mutable containers that no root reaches, which breaks every garbage
//! cycle, because immutable objects alone cannot form a cycle. A mutable
//! container that native code has locked counts as a root. The collector's
//! tables grow with the slot table, so a collection never allocates.

use super::value::*;
use crate::shared::{Fail, GcPolicy, Result, ShortStr, Tracker};
use std::mem::size_of;
use std::sync::{Arc, Weak};

/// A `Weak` handle to a registered container.
enum WeakObj {
    Struct(Weak<Frozen<StructObj>>),
    MutStruct(Weak<Shared<StructObj>>),
    Variant(Weak<Frozen<VariantObj>>),
    Closure(Weak<Frozen<ClosureObj>>),
    List(Weak<Shared<ListObj>>),
    Map(Weak<Shared<MapObj>>),
    Cell(Weak<Shared<CellObj>>),
    Host(Weak<Shared<HostBox>>),
    Encoder(Weak<Shared<super::codec::EncoderObj>>),
    Decoder(Weak<Shared<super::codec::DecoderObj>>),
}

/// How a visit of a container's children ended.
#[derive(PartialEq)]
enum Visit {
    Done,
    /// The container has no strong handles left.
    Dead,
    /// Native code holds the container's lock.
    Locked,
}

impl WeakObj {
    fn of(v: &Value) -> Option<WeakObj> {
        Some(match v {
            Value::Struct(o) => WeakObj::Struct(Arc::downgrade(o)),
            Value::MutStruct(o) => WeakObj::MutStruct(Arc::downgrade(o)),
            Value::Variant(o) => WeakObj::Variant(Arc::downgrade(o)),
            Value::Closure(o) => WeakObj::Closure(Arc::downgrade(o)),
            Value::List(o) => WeakObj::List(Arc::downgrade(o)),
            Value::Map(o) => WeakObj::Map(Arc::downgrade(o)),
            Value::Cell(o) => WeakObj::Cell(Arc::downgrade(o)),
            Value::Host(o) => WeakObj::Host(Arc::downgrade(o)),
            Value::Encoder(o) => WeakObj::Encoder(Arc::downgrade(o)),
            Value::Decoder(o) => WeakObj::Decoder(Arc::downgrade(o)),
            _ => return None,
        })
    }

    fn strong_count(&self) -> usize {
        match self {
            WeakObj::Struct(w) => w.strong_count(),
            WeakObj::MutStruct(w) => w.strong_count(),
            WeakObj::Variant(w) => w.strong_count(),
            WeakObj::Closure(w) => w.strong_count(),
            WeakObj::List(w) => w.strong_count(),
            WeakObj::Map(w) => w.strong_count(),
            WeakObj::Cell(w) => w.strong_count(),
            WeakObj::Host(w) => w.strong_count(),
            WeakObj::Encoder(w) => w.strong_count(),
            WeakObj::Decoder(w) => w.strong_count(),
        }
    }

    /// Visits the values that the container holds.
    fn visit(&self, f: &mut dyn FnMut(&Value)) -> Visit {
        fn each(values: &[Value], f: &mut dyn FnMut(&Value)) -> Visit {
            values.iter().for_each(f);
            Visit::Done
        }
        macro_rules! locked {
            ($w:expr, |$g:ident| $body:expr) => {
                match $w.upgrade() {
                    None => Visit::Dead,
                    Some(o) => match o.try_lock() {
                        Some($g) => $body,
                        None => Visit::Locked,
                    },
                }
            };
        }
        match self {
            WeakObj::Struct(w) => w.upgrade().map_or(Visit::Dead, |o| each(&o.data.fields, f)),
            WeakObj::Variant(w) => w.upgrade().map_or(Visit::Dead, |o| each(&o.data.fields, f)),
            WeakObj::Closure(w) => w.upgrade().map_or(Visit::Dead, |o| each(&o.data.captures, f)),
            WeakObj::MutStruct(w) => locked!(w, |g| each(&g.fields, f)),
            WeakObj::List(w) => locked!(w, |g| each(&g.0, f)),
            WeakObj::Map(w) => locked!(w, |g| {
                g.0.iter().for_each(|(k, v)| {
                    f(k);
                    f(v);
                });
                Visit::Done
            }),
            WeakObj::Cell(w) => locked!(w, |g| {
                f(&g.0);
                Visit::Done
            }),
            WeakObj::Host(w) => locked!(w, |g| {
                g.visit(f);
                Visit::Done
            }),
            WeakObj::Encoder(w) => w.upgrade().map_or(Visit::Dead, |_| Visit::Done),
            WeakObj::Decoder(w) => w.upgrade().map_or(Visit::Dead, |_| Visit::Done),
        }
    }

    /// Moves the contents out of a mutable container so that its garbage
    /// cycle falls apart, then drops them.
    fn clear(&self) {
        match self {
            WeakObj::MutStruct(w) => {
                let fields = w.upgrade().and_then(|o| o.try_lock().map(|mut g| std::mem::take(&mut g.fields)));
                drop_values(fields.map(Vec::from).unwrap_or_default());
            }
            WeakObj::List(w) => {
                let items = w.upgrade().and_then(|o| o.try_lock().map(|mut g| std::mem::take(&mut g.0)));
                drop_values(items.unwrap_or_default());
            }
            WeakObj::Map(w) => drop(w.upgrade().and_then(|o| o.try_lock().map(|mut g| std::mem::take(&mut *g)))),
            WeakObj::Cell(w) => {
                let v = w.upgrade().and_then(|o| o.try_lock().map(|mut g| std::mem::take(&mut g.0)));
                drop_values(v.into_iter().collect());
            }
            WeakObj::Host(w) => {
                let mut out = Vec::new();
                if let Some(o) = w.upgrade()
                    && let Some(mut g) = o.try_lock()
                {
                    g.clear(&mut out);
                }
                drop_values(out);
            }
            WeakObj::Struct(_) | WeakObj::Variant(_) | WeakObj::Closure(_) | WeakObj::Encoder(_) | WeakObj::Decoder(_) => {}
        }
    }
}

struct Entry {
    weak: Option<WeakObj>,
    /// Tracked bytes of the container, released when the slot is pruned.
    bytes: usize,
}

/// Tracked bytes per slot: the entry, its reference count scratch, its
/// worklist entry, and its free-list entry.
pub const SLOT_BYTES: usize = size_of::<Entry>() + size_of::<i64>() + 2 * size_of::<u32>();

const DEAD: i64 = i64::MIN;
const REACHABLE: i64 = i64::MAX;

pub struct Heap {
    pub tracker: Arc<Tracker>,
    pub policy: GcPolicy,
    /// Tracked byte count at which `reserve` leaves its fast path.
    next_check: usize,
    entries: Vec<Entry>,
    free: Vec<u32>,
    /// Per-slot reference counts during a collection.
    refs: Vec<i64>,
    worklist: Vec<u32>,
    pub live_containers: usize,
}

impl Heap {
    pub fn new(limit: usize, gc_min_threshold: usize) -> Heap {
        let mut heap = Heap {
            tracker: Arc::new(Tracker::new(limit)),
            policy: GcPolicy::new(gc_min_threshold),
            next_check: 0,
            entries: Vec::new(),
            free: Vec::new(),
            refs: Vec::new(),
            worklist: Vec::new(),
            live_containers: 0,
        };
        heap.update_next_check();
        heap
    }

    pub fn update_next_check(&mut self) {
        self.next_check = self.policy.threshold().min(self.tracker.limit());
    }

    /// Reserves `bytes` of tracked memory. May collect garbage.
    #[inline(always)]
    pub fn reserve(&mut self, bytes: usize) -> Result<()> {
        if bytes <= self.next_check.saturating_sub(self.tracker.used()) {
            self.tracker.add(bytes);
            return Ok(());
        }
        self.reserve_slow(bytes)
    }

    #[cold]
    #[inline(never)]
    fn reserve_slow(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.tracker.limit() {
            return Err(Fail::OutOfMemory);
        }
        if self.policy.should_collect(self.tracker.used().saturating_add(bytes)) {
            self.collect();
        }
        if !self.tracker.fits(bytes) {
            self.collect();
            if !self.tracker.fits(bytes) {
                return Err(Fail::OutOfMemory);
            }
        }
        self.tracker.add(bytes);
        Ok(())
    }

    #[inline]
    pub fn release(&mut self, bytes: usize) {
        self.tracker.sub(bytes);
    }

    /// Reserves a container's `bytes` and a free slot. May collect garbage.
    #[inline(always)]
    pub fn reserve_container(&mut self, bytes: usize) -> Result<()> {
        if self.free.is_empty() && self.entries.len() == self.entries.capacity() {
            self.grow_slots()?;
        }
        self.reserve(bytes)
    }

    #[cold]
    fn grow_slots(&mut self) -> Result<()> {
        let extra = self.entries.capacity().max(256);
        self.reserve(extra * SLOT_BYTES)?;
        self.entries.reserve_exact(extra);
        let capacity = self.entries.capacity();
        self.free.reserve_exact(capacity - self.free.len());
        self.refs.reserve_exact(capacity - self.refs.len());
        self.worklist.reserve_exact(capacity - self.worklist.len());
        Ok(())
    }

    /// Returns the slot for the next container, whose bytes the caller reserved.
    #[inline(always)]
    fn next_slot(&mut self) -> u32 {
        match self.free.last() {
            Some(&slot) => slot,
            None => self.entries.len() as u32,
        }
    }

    /// Registers the container `v` in the slot from [`Heap::next_slot`].
    #[inline(always)]
    fn register(&mut self, v: &Value, bytes: usize) {
        let entry = Entry { weak: WeakObj::of(v), bytes };
        match self.free.pop() {
            Some(slot) => self.entries[slot as usize] = entry,
            None => {
                debug_assert!(self.entries.len() < self.entries.capacity());
                self.entries.push(entry);
            }
        }
        self.live_containers += 1;
    }

    /// Adds `bytes` to the tracked size of the container in `slot` after it grew.
    #[inline]
    pub fn grew(&mut self, slot: u32, bytes: usize) {
        self.entries[slot as usize].bytes += bytes;
    }

    // ---------------------------------------------------------------------
    // Constructors
    // ---------------------------------------------------------------------

    pub fn new_str(&mut self, s: &str) -> Result<Value> {
        if let Some(short) = ShortStr::new(s) {
            return Ok(Value::Short(short));
        }
        self.reserve(LongStr::size(s.len()))?;
        Ok(Value::Str(Arc::new(LongStr::new(s.into(), self.tracker.clone()))))
    }

    /// Stores `s`, whose capacity the caller reserved, as a string.
    pub fn new_str_from_reserved(&mut self, s: String) -> Result<Value> {
        let capacity = s.capacity();
        if let Some(short) = ShortStr::new(&s) {
            self.release(capacity);
            return Ok(Value::Short(short));
        }
        self.reserve(LongStr::size(0))?;
        let text = s.into_boxed_str();
        self.release(capacity - text.len());
        Ok(Value::Str(Arc::new(LongStr::new(text, self.tracker.clone()))))
    }

    /// Bytes of a frozen container with `values` values.
    pub const fn frozen_size<T>(values: usize) -> usize {
        ARC_COUNTS + size_of::<Frozen<T>>() + values * size_of::<Value>()
    }

    /// Bytes of a shared container that owns `buffer` bytes.
    pub const fn shared_size<T>(buffer: usize) -> usize {
        buffer.saturating_add(ARC_COUNTS + size_of::<Shared<T>>())
    }

    /// Creates a struct whose bytes the caller reserved with
    /// [`Heap::reserve_container`] and [`Heap::frozen_size`] or [`Heap::shared_size`].
    pub fn insert_struct(&mut self, ty: u16, fields: Box<[Value]>, mutable: bool, bytes: usize) -> Value {
        let slot = self.next_slot();
        let obj = StructObj { ty, fields };
        let v = if mutable { Value::MutStruct(Arc::new(Shared::new(slot, obj))) } else { Value::Struct(Arc::new(Frozen { slot, data: obj })) };
        self.register(&v, bytes);
        v
    }

    pub fn insert_variant(&mut self, tag: u16, fields: Box<[Value]>, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Variant(Arc::new(Frozen { slot, data: VariantObj { tag, fields } }));
        self.register(&v, bytes);
        v
    }

    pub fn insert_closure(&mut self, func: u16, captures: Box<[Value]>, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Closure(Arc::new(Frozen { slot, data: ClosureObj { func, captures } }));
        self.register(&v, bytes);
        v
    }

    pub fn insert_list(&mut self, items: Vec<Value>, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::List(Arc::new(Shared::new(slot, ListObj(items))));
        self.register(&v, bytes);
        v
    }

    pub fn insert_map(&mut self, entries: Vec<(Value, Value)>, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Map(Arc::new(Shared::new(slot, MapObj(entries))));
        self.register(&v, bytes);
        v
    }

    pub fn insert_cell(&mut self, value: Value, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Cell(Arc::new(Shared::new(slot, CellObj(value))));
        self.register(&v, bytes);
        v
    }

    pub fn insert_host(&mut self, obj: HostBox, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Host(Arc::new(Shared::new(slot, obj)));
        self.register(&v, bytes);
        v
    }

    pub fn insert_encoder(&mut self, obj: super::codec::EncoderObj, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Encoder(Arc::new(Shared::new(slot, obj)));
        self.register(&v, bytes);
        v
    }

    pub fn insert_decoder(&mut self, obj: super::codec::DecoderObj, bytes: usize) -> Value {
        let slot = self.next_slot();
        let v = Value::Decoder(Arc::new(Shared::new(slot, obj)));
        self.register(&v, bytes);
        v
    }

    // ---------------------------------------------------------------------
    // Collection
    // ---------------------------------------------------------------------

    fn prune(&mut self, i: usize) {
        let entry = &mut self.entries[i];
        if entry.weak.take().is_some() {
            self.tracker.sub(entry.bytes);
            entry.bytes = 0;
            debug_assert!(self.free.len() < self.free.capacity());
            self.free.push(i as u32);
            self.live_containers -= 1;
        }
    }

    /// Prunes dead slots and clears garbage cycles. Returns the number of
    /// pruned containers.
    pub fn collect(&mut self) -> usize {
        let before = self.live_containers;
        let n = self.entries.len();
        self.refs.clear();
        for i in 0..n {
            let count = self.entries[i].weak.as_ref().map_or(0, WeakObj::strong_count);
            if count == 0 {
                self.prune(i);
                self.refs.push(DEAD);
            } else {
                self.refs.push(count as i64);
            }
        }
        // Subtract the references that containers hold to each other.
        let Heap { entries, refs, worklist, .. } = self;
        for i in 0..n {
            if refs[i] == DEAD {
                continue;
            }
            let weak = entries[i].weak.as_ref().expect("live slot");
            if weak.visit(&mut |child| {
                if let Some(slot) = child.slot() {
                    refs[slot as usize] -= 1;
                }
            }) == Visit::Locked
            {
                refs[i] = REACHABLE;
            }
        }
        // Mark everything that a root reaches.
        worklist.clear();
        for (i, r) in refs.iter_mut().enumerate() {
            if *r > 0 && *r != DEAD {
                *r = REACHABLE;
                worklist.push(i as u32);
            }
        }
        while let Some(i) = worklist.pop() {
            let weak = entries[i as usize].weak.as_ref().expect("live slot");
            weak.visit(&mut |child| {
                if let Some(slot) = child.slot()
                    && refs[slot as usize] != REACHABLE
                {
                    refs[slot as usize] = REACHABLE;
                    debug_assert!(worklist.len() < worklist.capacity());
                    worklist.push(slot);
                }
            });
        }
        // Break the unreachable cycles.
        for i in 0..n {
            if refs[i] != REACHABLE && refs[i] != DEAD {
                if let Some(weak) = &entries[i].weak {
                    weak.clear();
                }
            }
        }
        for i in 0..n {
            if self.entries[i].weak.as_ref().is_some_and(|w| w.strong_count() == 0) {
                self.prune(i);
            }
        }
        self.policy.collected(self.tracker.used());
        self.update_next_check();
        before - self.live_containers
    }
}
