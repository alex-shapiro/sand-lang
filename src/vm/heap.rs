//! The garbage-collected heap: an arena of objects addressed by `u32` handles,
//! collected by a precise, non-moving, generational mark-and-sweep collector.
//!
//! Generations use *sticky mark bits*: an object's mark bit stays set after
//! the collection it survived, and a set bit means "old". Objects allocated
//! since the last collection are young and listed in `young`.
//!
//! * A **minor** collection marks from the roots and from the *remembered
//!   set* (old objects mutated since the last collection, which may point to
//!   young objects), never traversing other old objects, and sweeps only the
//!   young list. Surviving young objects keep their mark: they are promoted.
//! * A **major** collection clears all marks, marks everything reachable and
//!   sweeps the whole arena. It runs when `bytes` exceeds `major_at` (twice
//!   the live size after the previous major collection, capped near the
//!   memory limit), and immediately after a minor collection that leaves the
//!   heap over the limit.
//! * The write barrier lives in `get_mut` (and therefore `list_mut`,
//!   `list_push` and the map/set helpers built on it): the first mutation of
//!   an old object after a collection adds it to the remembered set. Every
//!   change to a value stored inside a heap object must go through these
//!   accessors. Hot paths that store known values use the precise variant
//!   `get_mut_storing`, which only remembers the object if a stored value
//!   refers to a young object. `NativeObject`s never hold `Value`s, so
//!   `native_mut` needs no barrier.
//! * Lists remembered only through `list_push`/`list_mut_storing` (and maps
//!   and sets only through `entries_mut_storing`) are *partially*
//!   remembered: the minor collection scans just the stored indices (entries)
//!   and the pushed tail (`DirtyList`) instead of the whole object. Any other
//!   mutation, or too many scattered stores, makes the remembered entry a
//!   full one.
//!
//! Major collections of large heaps are **incremental**: instead of marking
//! and sweeping the whole heap in one pause, a *cycle* does the work in
//! steps paced by allocation, while minor collections go on as usual:
//!
//! * **Marking** uses separate mark bits (`cmarks`) and a gray worklist
//!   (objects marked but not yet scanned; big lists, maps and sets are
//!   scanned in chunks). The cycle starts by shading the roots; each step
//!   scans a budget of values.
//! * **Barrier** (incremental update, Dijkstra-style): while marking, the
//!   accessors that know the value they store (`get_mut_storing`,
//!   `list_mut_storing`, `list_push`) shade it; `get_mut`, whose stores are
//!   unknown, puts a marked object back on the worklist (to be rescanned).
//!   New objects are allocated unmarked, so minor collections can still free
//!   young garbage (they drop freed objects from the worklist). Fiber stacks
//!   and globals have no barrier: the final step rescans them.
//! * **Final step**: shades the roots again and drains the worklist; the
//!   marked objects are exactly the reachable ones. They all become old
//!   (`marks` = `cmarks`), so the remembered set and the young list restart
//!   empty.
//! * **Sweeping** frees unmarked objects a budget of slots at a time.
//!   Objects allocated meanwhile get a cycle mark so the sweep keeps them.
//! * A forced collection (before a memory limit error) abandons the cycle
//!   and collects everything at once, which is always safe.

use super::iomem::IoMemory;
use super::value::*;
use std::time::Duration;

/// Garbage collector statistics for one VM run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Number of collections.
    pub collections: u64,
    /// Total time spent in collections (including root scanning).
    pub total_pause: Duration,
    /// Longest single collection.
    pub max_pause: Duration,
    /// Number of major (full-heap) collections; the rest were minor.
    pub majors: u64,
    /// Longest major collection.
    pub max_major_pause: Duration,
    /// Longest minor collection.
    pub max_minor_pause: Duration,
    /// Steps of incremental major collections (each completed cycle also
    /// counts as one collection and one major collection).
    pub steps: u64,
    /// Longest step of an incremental major collection.
    pub max_step_pause: Duration,
}

impl GcStats {
    pub fn record(&mut self, pause: Duration, major: bool) {
        self.collections += 1;
        self.total_pause += pause;
        self.max_pause = self.max_pause.max(pause);
        if major {
            self.majors += 1;
            self.max_major_pause = self.max_major_pause.max(pause);
        } else {
            self.max_minor_pause = self.max_minor_pause.max(pause);
        }
    }

    /// Records a step of an incremental major collection.
    pub fn record_step(&mut self, pause: Duration) {
        self.steps += 1;
        self.total_pause += pause;
        self.max_pause = self.max_pause.max(pause);
        self.max_step_pause = self.max_step_pause.max(pause);
    }
}

/// The state of the incremental major collection (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Marking,
    Sweeping,
}
use std::sync::Arc;

pub struct Heap {
    objs: Vec<Obj>,
    /// Sticky mark bits: set for old objects (survivors of a collection).
    marks: Vec<u64>,
    /// Set for objects in `remembered`.
    remembered_bits: Vec<u64>,
    /// Old objects mutated since the last collection.
    remembered: Vec<u32>,
    /// Set for remembered lists with a `dirty` entry (partially remembered).
    partial_bits: Vec<u64>,
    /// Set for partially remembered lists whose `DirtyList::from` is set,
    /// so that further pushes need no bookkeeping.
    tail_bits: Vec<u64>,
    /// What changed in partially remembered lists.
    dirty: FxHashMap<u32, DirtyList>,
    /// Objects allocated since the last collection.
    young: Vec<u32>,
    free_head: u32,
    /// Estimated live bytes (exact after a major collection; between
    /// collections it also counts garbage, and after a minor collection
    /// unreachable old objects).
    pub bytes: usize,
    /// Collect when `bytes` exceeds this.
    pub next_gc: usize,
    /// Make the next collection a major one when `bytes` exceeds this.
    pub major_at: usize,
    pub limit: Option<usize>,
    /// Memory of the fibers (value stacks, frames, waiter and race lists),
    /// counted against `limit` with the heap: incremented as they grow and
    /// recomputed by every collection.
    pub fiber_bytes: usize,
    pub live_objects: usize,
    /// Native resources currently open (sockets, listeners, ...).
    pub open_resources: usize,
    /// Testing aid: collect at every safe point (see `Config::gc_stress`).
    pub stress: bool,
    /// Buffers of pending I/O, counted against `limit` with the heap.
    pub io_memory: Arc<IoMemory>,
    worklist: Vec<u32>,
    pub stats: GcStats,
    phase: Phase,
    /// Mark bits of the incremental cycle.
    cmarks: Vec<u64>,
    /// Set for objects with a pending full scan on `gray`.
    gray_bits: Vec<u64>,
    /// Objects to scan in the incremental marking, with the first child to
    /// scan (chunks of big containers).
    gray: Vec<(u32, u32)>,
    /// The next slot to sweep.
    sweep_at: usize,
    /// `bytes` when the cycle's last step ran (steps are paced by allocation).
    step_bytes: usize,
    /// Live bytes when the cycle started.
    cycle_live: usize,
    /// Run the next minor collection when `bytes` exceeds this.
    minor_at: usize,
    /// Counts collections under stress testing, which interleaves minor
    /// collections, incremental steps and full collections.
    stress_tick: u64,
    /// Major collections of large heaps are incremental (on by default).
    pub incremental: bool,
}

/// The parts of an old list that may hold young values.
#[derive(Default)]
struct DirtyList {
    /// Every index at and above this one (pushes); `NONE` if none.
    from: u32,
    /// Individually stored indices.
    idx: Vec<u32>,
}

/// Beyond this many stored indices (or an eighth of the list's length, if
/// more) a list is scanned in full.
const MAX_DIRTY_INDICES: usize = 1024;

const NONE: u32 = u32::MAX;
const MIN_GC: usize = 4 << 20;
/// Bytes allocated between minor collections, at most (a quarter of the
/// major collection threshold, at least `MIN_NURSERY`).
const NURSERY: usize = 8 << 20;
const MIN_NURSERY: usize = 1 << 20;
/// Major collections are incremental from this heap size on.
const INCREMENTAL_MIN: usize = 8 << 20;
/// Bytes allocated between steps of an incremental collection.
const STEP_BYTES: usize = 256 << 10;
/// Marking does this many units of work (values scanned) per 16 bytes
/// allocated, so a cycle marks the whole heap while allocating about a
/// quarter of its live size.
const MARK_PACE: usize = 4;
const MIN_MARK_WORK: usize = 16 << 10;
const MIN_SWEEP_SLOTS: usize = 32 << 10;
/// Children of a container scanned per chunk.
const SCAN_CHUNK: usize = 4096;

impl Default for Heap {
    fn default() -> Self {
        Heap::new(None)
    }
}

impl Heap {
    pub fn new(limit: Option<usize>) -> Heap {
        Heap {
            objs: Vec::new(),
            marks: Vec::new(),
            remembered_bits: Vec::new(),
            remembered: Vec::new(),
            partial_bits: Vec::new(),
            tail_bits: Vec::new(),
            dirty: FxHashMap::default(),
            young: Vec::new(),
            free_head: NONE,
            bytes: 0,
            next_gc: MIN_GC,
            major_at: MIN_GC,
            limit,
            fiber_bytes: 0,
            live_objects: 0,
            open_resources: 0,
            stress: false,
            io_memory: IoMemory::new(limit),
            worklist: Vec::new(),
            stats: GcStats::default(),
            phase: Phase::Idle,
            cmarks: Vec::new(),
            gray_bits: Vec::new(),
            gray: Vec::new(),
            sweep_at: 0,
            step_bytes: 0,
            cycle_live: 0,
            minor_at: MIN_GC,
            stress_tick: 0,
            incremental: true,
        }
    }

    #[inline]
    pub fn alloc(&mut self, obj: Obj) -> Value {
        self.bytes += obj.size();
        self.live_objects += 1;
        if let Obj::Native(n) = &obj
            && n.is_resource()
        {
            self.open_resources += 1;
        }
        // Free slots and new slots have clear mark bits: the object is young.
        let h = if self.free_head != NONE {
            let h = self.free_head;
            match self.objs[h as usize] {
                Obj::Free(next) => self.free_head = next,
                _ => unreachable!("corrupt free list"),
            }
            self.objs[h as usize] = obj;
            h
        } else {
            self.objs.push(obj);
            if self.marks.len() * 64 < self.objs.len() {
                self.marks.push(0);
                self.remembered_bits.push(0);
                self.partial_bits.push(0);
                self.tail_bits.push(0);
                self.cmarks.push(0);
                self.gray_bits.push(0);
            }
            (self.objs.len() - 1) as u32
        };
        if self.phase == Phase::Sweeping {
            // Keep it from the sweep.
            set_bit(&mut self.cmarks, h);
        }
        self.young.push(h);
        Value::Obj(h)
    }

    /// True if a collection should run at the next safe point.
    #[inline]
    pub fn wants_gc(&self) -> bool {
        self.bytes > self.next_gc
    }

    /// Whether the next collection should be a major one.
    pub fn wants_major(&self) -> bool {
        self.phase == Phase::Idle && self.bytes > self.major_at
    }

    /// The write barrier: remembers `h` (in full) if it is old.
    #[inline]
    fn barrier(&mut self, h: u32) {
        // Inline: only the test for "old and not already fully remembered".
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        if self.marks[w] & b != 0 && (self.remembered_bits[w] & b == 0 || self.partial_bits[w] & b != 0) {
            self.remember(h);
        }
    }

    /// Remembers old object `h` in full.
    #[cold]
    #[inline(never)]
    fn remember(&mut self, h: u32) {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        if self.remembered_bits[w] & b == 0 {
            self.remembered_bits[w] |= b;
            self.remembered.push(h);
        } else {
            self.remember_fully(h);
        }
    }

    /// Turns a partially remembered list into a fully remembered one.
    #[cold]
    fn remember_fully(&mut self, h: u32) {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        self.partial_bits[w] &= !b;
        self.tail_bits[w] &= !b;
        self.dirty.remove(&h);
    }

    /// The write barrier for storing a young value into list `h` at `index`
    /// (`push`: appending at `index`, the current length).
    #[inline]
    fn list_barrier(&mut self, h: u32, index: usize, push: bool) {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        if self.marks[w] & b != 0 {
            self.list_barrier_old(h, index, push);
        }
    }

    #[cold]
    #[inline(never)]
    fn list_barrier_old(&mut self, h: u32, index: usize, push: bool) {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        if self.remembered_bits[w] & b == 0 {
            self.remembered_bits[w] |= b;
            self.remembered.push(h);
            self.partial_bits[w] |= b;
            self.dirty.insert(h, DirtyList { from: NONE, idx: Vec::new() });
        } else if self.partial_bits[w] & b == 0 || (push && self.tail_bits[w] & b != 0) {
            // Fully remembered, or the pushed tail is already covered.
            return;
        }
        self.mark_dirty(h, index, push);
    }

    #[cold]
    fn mark_dirty(&mut self, h: u32, index: usize, push: bool) {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        let index = index.min(NONE as usize - 1) as u32;
        let len = match &self.objs[h as usize] {
            Obj::List(l) => l.len(),
            Obj::Map(m) => m.entries.len(),
            Obj::Set(s) => s.entries.len(),
            _ => 0,
        };
        let Some(d) = self.dirty.get_mut(&h) else { return };
        if push {
            d.from = d.from.min(index);
            self.tail_bits[w] |= b;
        } else if d.idx.len() < MAX_DIRTY_INDICES.max(len / 8) {
            d.idx.push(index);
        } else {
            self.remember_fully(h);
        }
    }

    /// Whether `extra` more bytes would exceed the memory limit (which also
    /// covers buffers reserved by pending I/O).
    pub fn would_exceed(&self, extra: usize) -> bool {
        match self.limit {
            Some(l) => self.used().saturating_add(self.io_memory.reserved()).saturating_add(extra) > l,
            None => extra > (1usize << 40),
        }
    }

    pub fn over_limit(&self) -> bool {
        matches!(self.limit, Some(l) if self.used().saturating_add(self.io_memory.reserved()) > l)
    }

    /// Memory used by heap objects and fibers (not counting pending I/O).
    pub fn used(&self) -> usize {
        self.bytes.saturating_add(self.fiber_bytes)
    }

    #[inline]
    pub fn get(&self, v: Value) -> &Obj {
        match v {
            Value::Obj(h) => &self.objs[h as usize],
            _ => &Obj::Free(0),
        }
    }

    /// Mutable access to an object, through the write barrier.
    #[inline]
    pub fn get_mut(&mut self, v: Value) -> &mut Obj {
        match v {
            Value::Obj(h) => {
                self.barrier(h);
                if self.phase == Phase::Marking {
                    self.regray(h);
                }
                &mut self.objs[h as usize]
            }
            _ => panic!("internal error: not an object"),
        }
    }

    /// Mutable access to an object for storing the values `stored` (and
    /// nothing else) into it: a precise write barrier that only remembers
    /// the object if one of the stored values refers to a young object.
    #[inline]
    pub fn get_mut_storing(&mut self, v: Value, stored: &[Value]) -> &mut Obj {
        match v {
            Value::Obj(h) => {
                if stored.iter().any(|s| s.handle().is_some_and(|c| !self.is_marked(c))) {
                    self.barrier(h);
                }
                if self.phase == Phase::Marking {
                    stored.iter().for_each(|&s| self.shade(s));
                }
                &mut self.objs[h as usize]
            }
            _ => panic!("internal error: not an object"),
        }
    }

    /// Adjusts accounting after an object grew or shrank.
    #[inline]
    pub fn account(&mut self, before: usize, after: usize) {
        if after >= before {
            self.bytes += after - before;
        } else {
            self.bytes = self.bytes.saturating_sub(before - after);
        }
    }

    // ------------------------------------------------------------------
    // Typed accessors

    pub fn str(&self, v: Value) -> &str {
        match self.get(v) {
            Obj::Str(s) => s,
            _ => "",
        }
    }

    pub fn bytes_of(&self, v: Value) -> &[u8] {
        match self.get(v) {
            Obj::Bytes(b) => b,
            _ => &[],
        }
    }

    pub fn list(&self, v: Value) -> &[Value] {
        match self.get(v) {
            Obj::List(l) => l,
            _ => &[],
        }
    }

    pub fn list_mut(&mut self, v: Value) -> &mut Vec<Value> {
        match self.get_mut(v) {
            Obj::List(l) => l,
            other => panic!("internal error: expected list, found {other:?}"),
        }
    }

    pub fn fields(&self, v: Value) -> &[Value] {
        match self.get(v) {
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t,
            _ => &[],
        }
    }

    pub fn new_str(&mut self, s: impl Into<Box<str>>) -> Value {
        self.alloc(Obj::Str(s.into()))
    }

    pub fn new_bytes(&mut self, b: impl Into<Box<[u8]>>) -> Value {
        self.alloc(Obj::Bytes(b.into()))
    }

    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        self.alloc(Obj::List(items))
    }

    pub fn new_tuple(&mut self, items: Vec<Value>) -> Value {
        self.alloc(Obj::Tuple(items.into_boxed_slice()))
    }

    /// Mutable access to list `list` for storing `stored` at `index` (and
    /// nothing else): the precise, card-marking write barrier. `None` if
    /// `list` is not a list. (Kept out of line: inlining it into the
    /// interpreter loop measurably slows down unrelated instructions.)
    #[inline(never)]
    pub fn list_mut_storing(&mut self, list: Value, index: usize, stored: Value) -> Option<&mut Vec<Value>> {
        let Value::Obj(h) = list else { return None };
        if stored.handle().is_some_and(|c| !self.is_marked(c)) {
            self.list_barrier(h, index, false);
        }
        if self.phase == Phase::Marking {
            self.shade(stored);
        }
        match &mut self.objs[h as usize] {
            Obj::List(l) => Some(l),
            _ => None,
        }
    }

    /// Mutable access to list `list` for removing elements from its end
    /// only (`pop`, `clear`): no write barrier is needed, as no value is
    /// stored and none moves.
    pub fn list_shrink(&mut self, list: Value) -> Option<&mut Vec<Value>> {
        let Value::Obj(h) = list else { return None };
        match &mut self.objs[h as usize] {
            Obj::List(l) => Some(l),
            _ => None,
        }
    }

    /// Mutable access to map or set `obj` for storing `stored` (and nothing
    /// else) into its entry `slot` (`push`: appending the entry at `slot`,
    /// the current number of entries): the precise, card-marking write
    /// barrier of `list_mut_storing`, for entries.
    pub fn entries_mut_storing(&mut self, obj: Value, slot: usize, push: bool, stored: &[Value]) -> &mut Obj {
        let Value::Obj(h) = obj else { panic!("internal error: not an object") };
        if stored.iter().any(|s| s.handle().is_some_and(|c| !self.is_marked(c))) {
            self.list_barrier(h, slot, push);
        }
        if self.phase == Phase::Marking {
            stored.iter().for_each(|&s| self.shade(s));
        }
        &mut self.objs[h as usize]
    }

    pub fn list_push(&mut self, list: Value, v: Value) {
        let Value::Obj(h) = list else { return };
        if v.handle().is_some_and(|c| !self.is_marked(c)) {
            let len = self.list(list).len();
            self.list_barrier(h, len, true);
        }
        if self.phase == Phase::Marking {
            self.shade(v);
        }
        let Obj::List(l) = &mut self.objs[h as usize] else { return };
        let before = l.capacity();
        l.push(v);
        let after = l.capacity();
        if after != before {
            self.bytes += (after - before) * std::mem::size_of::<Value>();
        }
    }

    /// The runtime variant id of an enum value (`u32::MAX` if not a variant).
    #[inline]
    pub fn variant_id(&self, v: Value) -> u32 {
        match v {
            Value::Tag(t) => t,
            Value::Obj(_) => match self.get(v) {
                Obj::Variant(id, _) => *id,
                _ => u32::MAX,
            },
            _ => match v.as_inline_variant() {
                Some((id, _)) => id,
                None => u32::MAX,
            },
        }
    }

    /// A representation-independent view of an enum variant value.
    #[inline]
    pub fn variant(&self, v: Value) -> Option<VariantRef<'_>> {
        match v {
            Value::Tag(t) => Some(VariantRef::Inline(t, None)),
            Value::Obj(_) => match self.get(v) {
                Obj::Variant(id, f) => Some(VariantRef::Heap(*id, f)),
                _ => None,
            },
            _ => v.as_inline_variant().map(|(id, p)| VariantRef::Inline(id, Some(p))),
        }
    }

    /// Field `i` of a tuple, struct, closure or enum variant (inline or not).
    #[inline]
    pub fn field(&self, v: Value, i: usize) -> Option<Value> {
        match v {
            Value::Obj(_) => self.fields(v).get(i).copied(),
            _ => match v.as_inline_variant() {
                Some((_, p)) if i == 0 => Some(p),
                _ => None,
            },
        }
    }

    /// Builds the variant `id(fields...)` in its canonical representation:
    /// a tag, an inline single-payload variant, or a heap object.
    #[inline(never)]
    pub fn new_variant(&mut self, id: u32, fields: &[Value]) -> Value {
        match fields {
            [] => Value::Tag(id),
            [p] => match Value::variant1(id, *p) {
                Some(v) => v,
                None => self.alloc(Obj::Variant(id, Box::new([*p]))),
            },
            _ => self.alloc(Obj::Variant(id, fields.into())),
        }
    }

    pub fn native<T: 'static>(&self, v: Value) -> Option<&T> {
        match self.get(v) {
            Obj::Native(n) => n.as_any().downcast_ref::<T>(),
            _ => None,
        }
    }

    pub fn native_mut<T: 'static>(&mut self, v: Value) -> Option<&mut T> {
        // Native objects hold no values: no write barrier needed.
        let Value::Obj(h) = v else { panic!("internal error: not an object") };
        match &mut self.objs[h as usize] {
            Obj::Native(n) => n.as_any_mut().downcast_mut::<T>(),
            _ => None,
        }
    }

    /// Releases a native resource early (e.g. `conn.close()`), keeping the object.
    pub fn resource_closed(&mut self) {
        self.open_resources = self.open_resources.saturating_sub(1);
    }

    pub fn len(&self) -> usize {
        self.live_objects
    }

    pub fn is_empty(&self) -> bool {
        self.live_objects == 0
    }

    // ------------------------------------------------------------------
    // Collection

    #[inline]
    fn is_marked(&self, h: u32) -> bool {
        self.marks[(h / 64) as usize] & (1 << (h % 64)) != 0
    }

    #[inline]
    fn mark_value(&mut self, v: Value) {
        if let Some(h) = v.handle() {
            let (w, b) = ((h / 64) as usize, h % 64);
            if self.marks[w] & (1 << b) == 0 {
                self.marks[w] |= 1 << b;
                self.worklist.push(h);
            }
        }
    }

    /// Marks everything reachable from the worklist that is not yet marked.
    fn trace(&mut self) {
        let mut children = Vec::new();
        while let Some(h) = self.worklist.pop() {
            children.clear();
            self.objs[h as usize].children(|v| children.push(v));
            for c in &children {
                self.mark_value(*c);
            }
        }
    }

    /// Frees object `h`, returning its accounted size.
    fn free(&mut self, h: u32) -> usize {
        let i = h as usize;
        let size = self.objs[i].size();
        if let Obj::Native(n) = &self.objs[i]
            && n.is_resource()
        {
            self.open_resources = self.open_resources.saturating_sub(1);
        }
        self.objs[i] = Obj::Free(self.free_head);
        self.free_head = h;
        self.live_objects -= 1;
        // Free slots have clear mark bits (of both kinds).
        clear_bit(&mut self.cmarks, h);
        clear_bit(&mut self.gray_bits, h);
        size
    }

    /// Collects garbage reachable from `roots`: a minor collection (young
    /// objects only) or, if `major`, a full one. Unreachable native resources
    /// are dropped, which closes them.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = Value>, major: bool) {
        if major {
            self.abandon_cycle();
            self.marks.iter_mut().for_each(|m| *m = 0);
        }
        for r in roots {
            self.mark_value(r);
        }
        let remembered = std::mem::take(&mut self.remembered);
        let mut children = Vec::new();
        for &h in &remembered {
            let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
            self.remembered_bits[w] &= !b;
            self.partial_bits[w] &= !b;
            self.tail_bits[w] &= !b;
            if !major {
                // Old objects that may point to young ones: scan their fields
                // (only the dirty parts of partially remembered lists).
                children.clear();
                match (self.dirty.remove(&h), &self.objs[h as usize]) {
                    (Some(d), Obj::List(l)) => {
                        children.extend(d.idx.iter().filter_map(|&i| l.get(i as usize).copied()));
                        if let Some(tail) = l.get(d.from as usize..) {
                            children.extend_from_slice(tail);
                        }
                    }
                    (Some(d), Obj::Map(m)) => {
                        let entries = d
                            .idx
                            .iter()
                            .filter_map(|&i| m.entries.get(i as usize))
                            .chain(m.entries.get(d.from as usize..).unwrap_or_default());
                        for (k, v) in entries.flatten() {
                            children.push(*k);
                            children.push(*v);
                        }
                    }
                    (Some(d), Obj::Set(st)) => {
                        let entries = d
                            .idx
                            .iter()
                            .filter_map(|&i| st.entries.get(i as usize))
                            .chain(st.entries.get(d.from as usize..).unwrap_or_default());
                        children.extend(entries.flatten());
                    }
                    (_, obj) => obj.children(|v| children.push(v)),
                }
                for c in &children {
                    self.mark_value(*c);
                }
            }
        }
        self.dirty.clear();
        self.remembered = remembered;
        self.remembered.clear();
        self.trace();
        if major {
            self.sweep_all();
        } else {
            let young = std::mem::take(&mut self.young);
            let mut freed = 0;
            for &h in &young {
                if !self.is_marked(h) {
                    freed += self.free(h);
                }
            }
            self.young = young;
            self.bytes = self.bytes.saturating_sub(freed);
            if self.phase == Phase::Marking {
                // Freed objects leave the worklist (their slots may be reused).
                let objs = &self.objs;
                self.gray.retain(|&(h, _)| !matches!(objs[h as usize], Obj::Free(_)));
            }
        }
        self.young.clear();
        self.set_thresholds(major);
    }

    fn sweep_all(&mut self) {
        let mut live_bytes = 0;
        let mut live = 0;
        let mut open = 0;
        for i in 0..self.objs.len() {
            if matches!(self.objs[i], Obj::Free(_)) {
                continue;
            }
            if self.is_marked(i as u32) {
                live_bytes += self.objs[i].size();
                live += 1;
                if let Obj::Native(n) = &self.objs[i]
                    && n.is_resource()
                {
                    open += 1;
                }
            } else {
                self.objs[i] = Obj::Free(self.free_head);
                self.free_head = i as u32;
            }
        }
        // Resources closed explicitly are still objects; keep the explicit count if lower.
        self.open_resources = self.open_resources.min(open);
        self.bytes = live_bytes;
        self.live_objects = live;
    }

    fn set_thresholds(&mut self, major: bool) {
        if major {
            let live_bytes = self.bytes;
            self.major_at = (live_bytes * 2).max(MIN_GC);
            if let Some(l) = self.limit {
                // Collect more eagerly as we approach the limit.
                if self.major_at > l {
                    self.major_at = l.max(live_bytes + (l - live_bytes.min(l)) / 2);
                }
            }
        }
        // Small heaps get a small nursery, so that they see minor
        // collections between major ones.
        let nursery = (self.major_at / 4).clamp(MIN_NURSERY, NURSERY);
        self.minor_at = self.bytes.saturating_add(nursery);
        self.update_next_gc();
    }

    fn update_next_gc(&mut self) {
        self.next_gc = if self.stress {
            0
        } else if self.phase == Phase::Idle {
            self.minor_at.min(self.major_at)
        } else {
            self.minor_at.min(self.bytes.saturating_add(STEP_BYTES))
        };
    }

    // ------------------------------------------------------------------
    // Incremental major collection

    /// Whether `v` is an object that the sweep of an incremental collection
    /// will free: unreachable, so it must not be made reachable again (as a
    /// cache that does not hold its entries alive could do).
    pub fn condemned(&self, v: Value) -> bool {
        self.phase == Phase::Sweeping && v.handle().is_some_and(|h| !test_bit(&self.cmarks, h))
    }

    /// Whether an incremental major collection is under way.
    pub fn cycle_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Whether a minor collection is due (during an incremental cycle).
    pub fn minor_due(&mut self) -> bool {
        if self.stress {
            self.stress_tick += 1;
            return self.stress_tick.is_multiple_of(3);
        }
        self.bytes > self.minor_at
    }

    /// Whether the major collection due should be incremental. Under stress
    /// testing, every other one is.
    pub fn major_is_incremental(&self) -> bool {
        self.incremental && if self.stress { self.stats.collections.is_multiple_of(16) } else { self.bytes >= INCREMENTAL_MIN }
    }

    /// Starts an incremental major collection: shades the roots.
    pub fn start_cycle(&mut self, roots: impl IntoIterator<Item = Value>) {
        self.cmarks.iter_mut().for_each(|m| *m = 0);
        self.gray_bits.iter_mut().for_each(|m| *m = 0);
        self.gray.clear();
        self.phase = Phase::Marking;
        for r in roots {
            self.shade(r);
        }
        self.cycle_live = self.bytes;
        self.step_bytes = self.bytes;
        self.update_next_gc();
    }

    /// Runs one step of the incremental collection. Returns true if marking
    /// is complete and `finish_marking` must run (it needs the roots).
    pub fn cycle_step(&mut self) -> bool {
        let allocated = self.bytes.saturating_sub(self.step_bytes).max(STEP_BYTES);
        self.step_bytes = self.bytes;
        match self.phase {
            Phase::Marking => {
                // (Under stress testing, cycles take a few steps, so that
                // they interleave with mutation but leave most safe points
                // to minor and full collections.)
                let budget = if self.stress { (self.live_objects / 8).max(64) } else { (allocated / 16 * MARK_PACE).max(MIN_MARK_WORK) };
                self.mark_some(budget);
                self.gray.is_empty()
            }
            Phase::Sweeping => {
                // Sweep the arena while allocating a quarter of the live size.
                let slots = self.objs.len();
                let budget = if self.stress {
                    (slots / 8).max(64)
                } else {
                    let share = (allocated as u128 * MARK_PACE as u128 * slots as u128 / self.cycle_live.max(1) as u128) as usize;
                    share.max(MIN_SWEEP_SLOTS)
                };
                self.sweep_some(budget);
                false
            }
            Phase::Idle => false,
        }
    }

    /// Completes the marking: shades the roots again (fiber stacks and
    /// globals have no write barrier) and drains the worklist. Everything
    /// marked is reachable and becomes old; sweeping starts.
    pub fn finish_marking(&mut self, roots: impl IntoIterator<Item = Value>) {
        for r in roots {
            self.shade(r);
        }
        self.mark_some(usize::MAX);
        // Survivors are old; with no young objects left, nothing old points
        // to a young object, so the remembered set restarts empty.
        self.marks.copy_from_slice(&self.cmarks);
        for &h in &self.remembered {
            let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
            self.remembered_bits[w] &= !b;
            self.partial_bits[w] &= !b;
            self.tail_bits[w] &= !b;
        }
        self.remembered.clear();
        self.dirty.clear();
        self.young.clear();
        self.phase = Phase::Sweeping;
        self.sweep_at = 0;
        self.update_next_gc();
    }

    /// Abandons the incremental collection (a full collection follows).
    fn abandon_cycle(&mut self) {
        self.phase = Phase::Idle;
        self.gray.clear();
    }

    /// Shades `v`: marks it and queues it for scanning, if not yet marked.
    #[inline]
    fn shade(&mut self, v: Value) {
        if let Some(h) = v.handle()
            && !test_bit(&self.cmarks, h)
        {
            set_bit(&mut self.cmarks, h);
            set_bit(&mut self.gray_bits, h);
            self.gray.push((h, 0));
        }
    }

    /// The barrier of `get_mut` while marking: an object that was marked
    /// (and maybe scanned) is scanned again, as its new contents are unknown.
    #[cold]
    #[inline(never)]
    fn regray(&mut self, h: u32) {
        if test_bit(&self.cmarks, h) && !test_bit(&self.gray_bits, h) {
            set_bit(&mut self.gray_bits, h);
            self.gray.push((h, 0));
        }
    }

    /// Scans gray objects for about `budget` units of work (values).
    fn mark_some(&mut self, mut budget: usize) {
        let mut children = Vec::new();
        while budget > 0 {
            let Some((h, start)) = self.gray.pop() else { break };
            if start == 0 {
                clear_bit(&mut self.gray_bits, h);
            }
            children.clear();
            let next = chunk_children(&self.objs[h as usize], start as usize, &mut children);
            budget = budget.saturating_sub(children.len() + 1);
            if let Some(k) = next {
                self.gray.push((h, k as u32));
            }
            for &c in &children {
                self.shade(c);
            }
        }
    }

    /// Sweeps `budget` slots; finishes the cycle at the end of the arena.
    fn sweep_some(&mut self, budget: usize) {
        let end = self.sweep_at.saturating_add(budget).min(self.objs.len());
        let mut freed = 0;
        for i in self.sweep_at..end {
            if !matches!(self.objs[i], Obj::Free(_)) && !test_bit(&self.cmarks, i as u32) {
                freed += self.free(i as u32);
            }
        }
        self.bytes = self.bytes.saturating_sub(freed);
        self.sweep_at = end;
        if end == self.objs.len() {
            self.phase = Phase::Idle;
            self.stats.collections += 1;
            self.stats.majors += 1;
            self.set_thresholds(true);
        }
    }
}

#[inline]
fn test_bit(bits: &[u64], h: u32) -> bool {
    bits[(h / 64) as usize] & (1 << (h % 64)) != 0
}

#[inline]
fn set_bit(bits: &mut [u64], h: u32) {
    bits[(h / 64) as usize] |= 1 << (h % 64);
}

#[inline]
fn clear_bit(bits: &mut [u64], h: u32) {
    bits[(h / 64) as usize] &= !(1 << (h % 64));
}

/// Appends the children of `obj` from child `start` on to `out`, at most
/// `SCAN_CHUNK` of them for big containers; returns where to continue.
fn chunk_children(obj: &Obj, start: usize, out: &mut Vec<Value>) -> Option<usize> {
    let chunk = |len: usize| {
        let end = start.saturating_add(SCAN_CHUNK).min(len);
        (start.min(end)..end, (end < len).then_some(end))
    };
    match obj {
        Obj::List(l) => {
            let (r, next) = chunk(l.len());
            out.extend_from_slice(&l[r]);
            next
        }
        Obj::Map(m) => {
            let (r, next) = chunk(m.entries.len());
            for (k, v) in m.entries[r].iter().flatten() {
                out.push(*k);
                out.push(*v);
            }
            next
        }
        Obj::Set(s) => {
            let (r, next) = chunk(s.entries.len());
            out.extend(s.entries[r].iter().flatten());
            next
        }
        _ => {
            if start == 0 {
                obj.children(|v| out.push(v));
            }
            None
        }
    }
}
