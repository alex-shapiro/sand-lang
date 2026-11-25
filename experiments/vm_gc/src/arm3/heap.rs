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
//! container that native code has locked counts as a root, and so does a
//! container of another engine. The collector's
//! tables grow with the slot table, so they never allocate during a
//! collection. Dropping the cleared contents can grow a work stack.

use super::value::*;
use crate::shared::{Fail, GcPolicy, Limits, Result, ShortStr, Tracker};
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};

struct Entry {
    weak: Option<Weak<dyn Container>>,
    /// Tracked bytes of the container, released when the slot is pruned.
    bytes: usize,
}

/// Tracked bytes per slot: the entry, its reference count scratch, its
/// worklist entry, and its free-list entry.
pub const SLOT_BYTES: usize = size_of::<Entry>() + size_of::<i64>() + 2 * size_of::<u32>();

const DEAD: i64 = i64::MIN;
const REACHABLE: i64 = i64::MAX;

/// Source of the engine ids in container headers.
static NEXT_ENGINE: AtomicU32 = AtomicU32::new(0);

pub struct Heap {
    pub tracker: Arc<Tracker>,
    pub policy: GcPolicy,
    /// Tracked byte count at which `reserve` leaves its fast path.
    next_check: usize,
    engine: u32,
    entries: Vec<Entry>,
    free: Vec<u32>,
    /// Per-slot reference counts during a collection.
    refs: Vec<i64>,
    worklist: Vec<u32>,
    pub live_containers: usize,
}

impl Heap {
    pub fn new(limits: &Limits) -> Heap {
        let mut heap = Heap {
            tracker: Arc::new(Tracker::new(limits.max_memory)),
            policy: GcPolicy::new(limits),
            next_check: 0,
            engine: NEXT_ENGINE.fetch_add(1, Ordering::Relaxed),
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
        if bytes < self.next_check.saturating_sub(self.tracker.used()) {
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
        let collect = self.policy.should_collect(self.tracker.used().saturating_add(bytes));
        if collect {
            self.collect();
        }
        if !self.tracker.fits(bytes) {
            if !collect {
                self.collect();
            }
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

    /// Returns the header for the next container, whose bytes the caller reserved.
    #[inline(always)]
    fn next_header(&self) -> Header {
        let slot = match self.free.last() {
            Some(&slot) => slot,
            None => self.entries.len() as u32,
        };
        Header { slot, engine: self.engine }
    }

    /// Registers `container` in the slot from [`Heap::next_header`].
    #[inline(always)]
    fn register<C: Container + 'static>(&mut self, container: &Arc<C>, bytes: usize) {
        let weak: Weak<dyn Container> = Arc::downgrade(container) as Weak<C>;
        let entry = Entry { weak: Some(weak), bytes };
        match self.free.pop() {
            Some(slot) => self.entries[slot as usize] = entry,
            None => {
                debug_assert!(self.entries.len() < self.entries.capacity());
                self.entries.push(entry);
            }
        }
        self.live_containers += 1;
    }

    /// Creates an immutable container whose bytes the caller reserved with
    /// [`Heap::reserve_container`] and [`Heap::frozen_size`].
    #[inline]
    pub fn insert_frozen<T: FrozenContents>(&mut self, data: T, bytes: usize) -> Arc<Frozen<T>> {
        let container = Arc::new(Frozen { header: self.next_header(), data });
        self.register(&container, bytes);
        container
    }

    /// Creates a mutable container whose bytes the caller reserved with
    /// [`Heap::reserve_container`] and [`Heap::shared_size`].
    #[inline]
    pub fn insert_shared<T: Contents>(&mut self, data: T, bytes: usize) -> Arc<Shared<T>> {
        let container = Arc::new(Shared::new(self.next_header(), data));
        self.register(&container, bytes);
        container
    }

    /// Creates a struct, mutable if it has `var` fields.
    pub fn insert_struct(&mut self, ty: u16, fields: Box<[Value]>, mutable: bool, bytes: usize) -> Value {
        let data = StructObj { ty, fields };
        if mutable { Value::MutStruct(self.insert_shared(data, bytes)) } else { Value::Struct(self.insert_frozen(data, bytes)) }
    }

    /// Fails unless this engine's table holds the container in `header`.
    /// A container can grow only in the engine that tracks its bytes.
    #[inline]
    pub fn check_own(&self, header: Header) -> Result<()> {
        if header.engine == self.engine { Ok(()) } else { Err(Fail::error("value belongs to another engine")) }
    }

    /// Adds `bytes` to the tracked size of the container in `header` after its buffer grew.
    #[inline]
    pub fn add_container_bytes(&mut self, header: Header, bytes: usize) {
        self.entries[header.slot as usize].bytes += bytes;
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
        if let Err(e) = self.reserve(LongStr::size(0)) {
            self.release(capacity);
            return Err(e);
        }
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

    // ---------------------------------------------------------------------
    // Collection
    // ---------------------------------------------------------------------

    fn prune_slot(&mut self, i: usize) {
        let entry = &mut self.entries[i];
        if entry.weak.take().is_some() {
            self.tracker.sub(entry.bytes);
            entry.bytes = 0;
            debug_assert!(self.free.len() < self.free.capacity());
            self.free.push(i as u32);
            self.live_containers -= 1;
        }
    }

    /// Prunes the slots of dead containers.
    pub fn prune(&mut self) {
        for i in 0..self.entries.len() {
            if self.entries[i].weak.as_ref().is_some_and(|w| w.strong_count() == 0) {
                self.prune_slot(i);
            }
        }
    }

    /// Prunes dead slots and clears garbage cycles.
    pub fn collect(&mut self) {
        let n = self.entries.len();
        self.refs.clear();
        for i in 0..n {
            let count = self.entries[i].weak.as_ref().map_or(0, Weak::strong_count);
            if count == 0 {
                self.prune_slot(i);
                self.refs.push(DEAD);
            } else {
                self.refs.push(count as i64);
            }
        }
        // Subtract the references that this engine's containers hold to each other.
        for i in 0..n {
            if self.refs[i] == DEAD {
                continue;
            }
            let Some(container) = self.entries[i].weak.as_ref().and_then(Weak::upgrade) else { continue };
            let (refs, engine) = (&mut self.refs, self.engine);
            let visited = container.visit(&mut |child| {
                if let Some(h) = child.header().filter(|h| h.engine == engine) {
                    refs[h.slot as usize] -= 1;
                }
            });
            if !visited {
                refs[i] = REACHABLE;
            }
        }
        // Mark everything that a root reaches.
        self.worklist.clear();
        for i in 0..n {
            if self.refs[i] > 0 && self.refs[i] != DEAD {
                self.refs[i] = REACHABLE;
                self.worklist.push(i as u32);
            }
        }
        while let Some(i) = self.worklist.pop() {
            let Some(container) = self.entries[i as usize].weak.as_ref().and_then(Weak::upgrade) else { continue };
            let (refs, worklist, engine) = (&mut self.refs, &mut self.worklist, self.engine);
            container.visit(&mut |child| {
                if let Some(h) = child.header().filter(|h| h.engine == engine)
                    && refs[h.slot as usize] != REACHABLE
                {
                    refs[h.slot as usize] = REACHABLE;
                    debug_assert!(worklist.len() < worklist.capacity());
                    worklist.push(h.slot);
                }
            });
        }
        // Break the unreachable cycles.
        let mut cleared = Vec::new();
        for i in 0..n {
            if self.refs[i] != REACHABLE
                && self.refs[i] != DEAD
                && let Some(container) = self.entries[i].weak.as_ref().and_then(Weak::upgrade)
            {
                container.clear(&mut cleared);
                drop(container);
                drop_values(std::mem::take(&mut cleared));
            }
        }
        self.prune();
        self.policy.collected(self.tracker.used());
        self.update_next_check();
    }
}
