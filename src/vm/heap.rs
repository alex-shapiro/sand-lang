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
//!   full one. A channel remembered through `channel_mut` is partially
//!   remembered too. Values enter its queue only at the back, so the young
//!   values are among the last `ChannelObj::pushed` values of the queue.
//! * Tracing scans big containers `SCAN_CHUNK` children at a time, so that
//!   its worklist stays small.
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
use crate::LimitKind;
use compact_str::CompactString;
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
    /// Steps of incremental major collections.
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

/// The state of the incremental major collection.
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
    /// Resources (open files, sockets, ...) allocated since the last
    /// collection; enough of them trigger one (see [`RESOURCE_BURST`]).
    young_resources: usize,
    /// Testing aid: collect at every safe point (see `Config::gc_stress`).
    pub stress: bool,
    /// Disables memory-limit checks during post-effect result conversion (see [`Vm::unchecked`](crate::vm::Vm::unchecked)).
    pub(crate) unchecked: bool,
    /// Buffers of pending I/O, counted against `limit` with the heap.
    pub io_memory: Arc<IoMemory>,
    worklist: Vec<u32>,
    /// Big containers being traced, each with the index of the next child to scan.
    scan: Vec<(u32, usize)>,
    pub stats: GcStats,
    phase: Phase,
    /// Mark bits of the incremental cycle.
    cmarks: Vec<u64>,
    /// Set for objects with a pending full scan on `gray`.
    gray_bits: Vec<u64>,
    /// Objects to scan in the incremental marking, with the first child to
    /// scan (chunks of big containers).
    gray: Vec<(u32, usize)>,
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
    /// The boxes of freed tuples, structs, variants and closures of 1 to
    /// `SPARE_LENS` values (by length), reused by [`Heap::boxed`]: such
    /// objects are made and dropped all the time. Counted as used memory;
    /// dropped by major collections.
    spare: [Vec<Box<[Value]>>; SPARE_LENS],
    spare_bytes: usize,
}

const SPARE_LENS: usize = 4;
/// The most boxes kept of each length: about what a minor collection frees.
const SPARE_MAX: usize = 1 << 14;

/// New objects being built by [`Heap::staged`]. Their handles are taken in
/// advance from the heap's free list (which cannot change while it is
/// borrowed) and then past the end of the arena, exactly as `Heap::alloc`
/// would take them.
pub struct Staging<'h> {
    heap: &'h Heap,
    /// The next free slot (`NONE`: none left).
    free: u32,
    /// The next slot past the end of the arena.
    end: usize,
    objs: Vec<(u32, Obj)>,
    /// Accounted size of the staged objects.
    bytes: usize,
}

impl Staging<'_> {
    /// Stages `obj`, returning the value it will have.
    pub fn alloc(&mut self, obj: Obj) -> Value {
        self.bytes += obj.size();
        let h = if self.free != NONE {
            let h = self.free;
            self.free = match self.heap.objs[h as usize] {
                Obj::Free(next) => next,
                _ => unreachable!("corrupt free list"),
            };
            h
        } else {
            self.end += 1;
            (self.end - 1) as u32
        };
        self.objs.push((h, obj));
        Value::Obj(h)
    }

    pub fn new_str(&mut self, s: impl Into<CompactString>) -> Value {
        self.alloc(Obj::Str(s.into()))
    }

    pub fn new_bytes(&mut self, b: impl Into<Box<[u8]>>) -> Value {
        self.alloc(Obj::Bytes(b.into()))
    }

    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        self.alloc(Obj::List(items))
    }

    pub fn new_tuple(&mut self, items: Vec<Value>) -> Value {
        match items[..] {
            [a, b] if let Some(v) = Value::tuple2(a, b) => v,
            _ => self.alloc(Obj::Tuple(items.into_boxed_slice())),
        }
    }

    /// [`Heap::new_variant`], staged.
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

    /// Whether `extra` more bytes, besides those staged, would exceed the
    /// memory limit of the heap.
    pub fn would_exceed(&self, extra: usize) -> bool {
        self.heap.would_exceed(self.bytes.saturating_add(extra))
    }

    /// Fails with the memory limit if `extra` more bytes do not fit.
    pub fn reserve(&self, extra: usize) -> Result<(), crate::vm::Ctl> {
        if self.would_exceed(extra) { Err(crate::vm::Ctl::limit(LimitKind::Memory)) } else { Ok(()) }
    }

    /// Makes room for `n` more staged objects.
    pub fn expect(&mut self, n: usize) {
        self.objs.reserve(n);
    }

    /// The number of objects staged.
    pub fn len(&self) -> usize {
        self.objs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objs.is_empty()
    }
}

/// The parts of an old list that may hold young values.
#[derive(Default)]
struct DirtyList {
    /// Every index at and above this one (pushes); `NONE` if none.
    from: u32,
    /// Individually stored indices.
    idx: Vec<u32>,
}

/// A partially remembered list (or a map's or set's entries) records at most
/// one stored index per this many elements (at least one); beyond that it
/// is scanned in full. The index lists are not counted against the memory
/// limit, and stores allocate nothing, so they need not lead to the
/// collection that empties the lists. The lists must therefore stay small
/// relative to their objects: a fixed allowance would let a script hold
/// unaccounted memory by storing young values into many small old lists.
const ELEMS_PER_DIRTY_INDEX: usize = 8;

const NONE: u32 = u32::MAX;
const MIN_GC: usize = 4 << 20;
/// Bytes allocated between minor collections, at most.
const NURSERY: usize = 8 << 20;
/// Resources allocated between collections at most (see
/// `Heap::resource_allocated`).
const RESOURCE_BURST: usize = 64;
const MIN_NURSERY: usize = 1 << 20;
/// Major collections are incremental from this heap size on.
const INCREMENTAL_MIN: usize = 8 << 20;
/// Bytes allocated between steps of an incremental collection.
const STEP_BYTES: usize = 256 << 10;
/// Marking does this many units of work per 16 bytes allocated, so a cycle
/// marks the whole heap while allocating about a quarter of its live size.
const MARK_PACE: usize = 4;
const MIN_MARK_WORK: usize = 16 << 10;
const MIN_SWEEP_SLOTS: usize = 32 << 10;
/// Children of a container scanned per chunk.
const SCAN_CHUNK: usize = 4096;

impl Default for Heap {
    fn default() -> Self {
        Heap::new(None, usize::MAX)
    }
}

impl Heap {
    /// A heap for a memory limit of `limit` bytes and at most
    /// `max_resources` open resources.
    pub fn new(limit: Option<usize>, max_resources: usize) -> Heap {
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
            young_resources: 0,
            stress: false,
            unchecked: false,
            io_memory: IoMemory::new(limit, max_resources),
            worklist: Vec::new(),
            scan: Vec::new(),
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
            spare: Default::default(),
            spare_bytes: 0,
        }
    }

    /// A box of `items`, reusing a freed one when there is one.
    #[inline]
    pub fn boxed(&mut self, items: &[Value]) -> Box<[Value]> {
        match items.len().checked_sub(1).and_then(|i| self.spare.get_mut(i)).and_then(Vec::pop) {
            Some(mut b) => {
                self.spare_bytes -= size_of_val(&*b);
                b.copy_from_slice(items);
                b
            }
            None => items.into(),
        }
    }

    #[inline]
    pub fn alloc(&mut self, obj: Obj) -> Value {
        self.bytes += obj.size();
        self.live_objects += 1;
        if let Obj::Native(n) = &obj
            && n.is_resource()
        {
            self.resource_allocated();
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

    /// Runs `f` with read access to the heap and a [`Staging`] area for new
    /// objects, which are added to the heap when `f` returns. Natives use it
    /// to build values from the contents of others (`split`, `lines`, JSON
    /// parsing, regex matches) without first copying those contents out of
    /// the heap. The values `Staging::alloc` returns are valid once `f` has
    /// returned (they may be stored in other staged objects meanwhile).
    pub fn staged<R>(&mut self, f: impl FnOnce(&Heap, &mut Staging<'_>) -> R) -> R {
        let mut st = Staging { heap: self, free: self.free_head, end: self.objs.len(), objs: Vec::new(), bytes: 0 };
        let r = f(self, &mut st);
        let (free, objs) = (st.free, st.objs);
        for (h, obj) in objs {
            let v = self.alloc(obj);
            debug_assert_eq!(v, Value::Obj(h), "staged handles follow the free list, then the arena's end");
        }
        debug_assert_eq!(self.free_head, free);
        r
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
        } else if d.idx.len() < (len / ELEMS_PER_DIRTY_INDEX).max(1) {
            d.idx.push(index);
        } else {
            self.remember_fully(h);
        }
    }

    /// Whether `extra` more bytes would exceed the memory limit (which also
    /// covers buffers reserved by pending I/O).
    pub fn would_exceed(&self, extra: usize) -> bool {
        match self.limit {
            Some(l) => {
                let used = self.used();
                // Publishing on every check before the heap grows keeps the
                // size that pending I/O checks against up to date.
                self.io_memory.publish_heap(used);
                !self.unchecked && used.saturating_add(self.io_memory.reserved()).saturating_add(extra) > l
            }
            None => extra > super::iomem::UNLIMITED,
        }
    }

    pub fn over_limit(&self) -> bool {
        matches!(self.limit, Some(l) if self.used().saturating_add(self.io_memory.reserved()) > l)
    }

    /// Memory used by heap objects and fibers (not counting pending I/O).
    pub fn used(&self) -> usize {
        self.bytes.saturating_add(self.fiber_bytes).saturating_add(self.spare_bytes)
    }

    #[inline]
    pub fn get(&self, v: Value) -> &Obj {
        match v {
            Value::Obj(h) => &self.objs[h as usize],
            _ => &Obj::Free(0),
        }
    }

    /// The object with handle `h`.
    #[inline]
    pub fn obj(&self, h: u32) -> &Obj {
        &self.objs[h as usize]
    }

    /// Mutable access to an object, through the write barrier.
    #[inline]
    pub fn get_mut(&mut self, v: Value) -> &mut Obj {
        match v {
            Value::Obj(h) => {
                self.barrier(h);
                if matches!(self.phase, Phase::Marking) {
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
                if matches!(self.phase, Phase::Marking) {
                    stored.iter().for_each(|&s| self.shade(s));
                }
                &mut self.objs[h as usize]
            }
            _ => panic!("internal error: not an object"),
        }
    }

    /// Stores `v` into cell `c`.
    #[inline]
    pub fn set_cell(&mut self, c: Value, v: Value) {
        let obj = match (c, v.handle()) {
            (Value::Obj(h), None) => &mut self.objs[h as usize],
            _ => self.get_mut_storing(c, &[v]),
        };
        if let Obj::Cell(x) = obj {
            *x = v;
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
            Obj::StrBuf(s) => s,
            _ => "",
        }
    }

    /// The bytes `append_str(cur, add)` copies (what `Op::AppendStr` is
    /// charged for): `add`, if it is appended in place (growing the buffer
    /// is amortised); otherwise all of `cur` too, which is copied into a new
    /// buffer (e.g. when `cur` was read, and so frozen, since the last append).
    #[inline(never)]
    pub fn append_copies(&self, cur: Value, add: Value) -> usize {
        let extra = self.str(add).len();
        match cur {
            Value::Obj(h) if add.handle() != Some(h) && matches!(self.objs[h as usize], Obj::StrBuf(_)) => extra,
            _ => self.str(cur).len().saturating_add(extra),
        }
    }

    /// `Op::AppendStr`: appends string `add` to `cur`, in place if `cur` is a
    /// string buffer, otherwise into a new buffer holding `cur` followed by
    /// `add` (the result). Fails (changing nothing) if the memory needed
    /// does not fit.
    pub fn append_str(&mut self, cur: Value, add: Value) -> Result<Value, LimitKind> {
        let extra = self.str(add).len();
        if let Value::Obj(h) = cur
            && add.handle() != Some(h)
            && let Obj::StrBuf(buf) = &self.objs[h as usize]
        {
            let (len, cap) = (buf.len(), buf.capacity());
            if len < cap
                && let &[b] = self.str(add).as_bytes()
                && b.is_ascii()
            {
                // One ASCII character: the commonest piece.
                if let Obj::StrBuf(buf) = &mut self.objs[h as usize] {
                    buf.push(b as char);
                }
                return Ok(cur);
            }
            if len + extra <= cap && extra <= 16 {
                // The common case (a short piece that fits): copied through
                // a small buffer, as `add` is in the same arena.
                let mut piece = [0u8; 16];
                piece[..extra].copy_from_slice(self.str(add).as_bytes());
                if let (Obj::StrBuf(buf), Ok(piece)) = (&mut self.objs[h as usize], std::str::from_utf8(&piece[..extra])) {
                    buf.push_str(piece);
                }
                return Ok(cur);
            }
            let grow = if len + extra > cap { (len + extra).max(2 * cap) - cap } else { 0 };
            if grow > 0 && self.would_exceed(grow) {
                return Err(LimitKind::Memory);
            }
            let Obj::StrBuf(buf) = &mut self.objs[h as usize] else { unreachable!() };
            let mut text = std::mem::take(buf);
            text.reserve_exact(cap + grow - len);
            text.push_str(self.str(add));
            self.bytes += text.capacity() - cap;
            self.objs[h as usize] = Obj::StrBuf(text);
            return Ok(cur);
        }
        let len = self.str(cur).len();
        let cap = (2 * (len + extra)).max(16);
        if self.would_exceed(cap) {
            return Err(LimitKind::Memory);
        }
        let mut text = String::with_capacity(cap);
        text.push_str(self.str(cur));
        text.push_str(self.str(add));
        Ok(self.alloc(Obj::StrBuf(text)))
    }

    /// `Op::AppendInterp`: appends `text` to `cur` like [`Heap::append_str`].
    pub fn append_text(&mut self, cur: Value, text: &str) -> Result<Value, LimitKind> {
        if let Value::Obj(h) = cur
            && let Obj::StrBuf(buf) = &mut self.objs[h as usize]
        {
            let (len, cap) = (buf.len(), buf.capacity());
            if len + text.len() <= cap {
                buf.push_str(text);
                return Ok(cur);
            }
            let grow = (len + text.len()).max(2 * cap) - cap;
            if self.would_exceed(grow) {
                return Err(LimitKind::Memory);
            }
            let Obj::StrBuf(buf) = &mut self.objs[h as usize] else { unreachable!() };
            buf.reserve_exact(cap + grow - len);
            buf.push_str(text);
            self.bytes += buf.capacity() - cap;
            return Ok(cur);
        }
        let len = self.str(cur).len();
        let cap = (2 * (len + text.len())).max(16);
        if self.would_exceed(cap) {
            return Err(LimitKind::Memory);
        }
        let mut buf = String::with_capacity(cap);
        buf.push_str(self.str(cur));
        buf.push_str(text);
        Ok(self.alloc(Obj::StrBuf(buf)))
    }

    /// `Op::FreezeStr`: turns string buffer `v` into an ordinary string.
    #[inline]
    pub fn freeze_str(&mut self, v: Value) {
        if let Value::Obj(h) = v
            && let Obj::StrBuf(_) = self.objs[h as usize]
        {
            self.freeze_slow(h);
        }
    }

    #[cold]
    #[inline(never)]
    fn freeze_slow(&mut self, h: u32) {
        if let Obj::StrBuf(buf) = &mut self.objs[h as usize] {
            let text = std::mem::take(buf);
            let cap = text.capacity();
            let frozen = CompactString::from_string_buffer(text);
            let kept = if frozen.is_heap_allocated() { frozen.capacity() } else { 0 };
            self.bytes = self.bytes.saturating_sub(cap - kept);
            self.objs[h as usize] = Obj::Str(frozen);
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

    /// The fields of a heap tuple, struct, variant or closure. Inline
    /// values (`Value::Pair`, inline variants) have none: see [`Heap::field`].
    pub fn fields(&self, v: Value) -> &[Value] {
        match self.get(v) {
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t,
            _ => &[],
        }
    }

    pub fn new_str(&mut self, s: impl Into<CompactString>) -> Value {
        self.alloc(Obj::Str(s.into()))
    }

    pub fn new_bytes(&mut self, b: impl Into<Box<[u8]>>) -> Value {
        self.alloc(Obj::Bytes(b.into()))
    }

    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        self.alloc(Obj::List(items))
    }

    /// The tuple of `items` (inline for a pair of ints, see `Value::Pair`).
    pub fn new_tuple(&mut self, items: &[Value]) -> Value {
        match *items {
            [a, b] if let Some(v) = Value::tuple2(a, b) => v,
            _ => {
                let items = self.boxed(items);
                self.alloc(Obj::Tuple(items))
            }
        }
    }

    /// Mutable access to list `list` for storing `stored` at `index` (and
    /// nothing else): the precise, card-marking write barrier. `None` if
    /// `list` is not a list. Out of line because inlining it into the
    /// interpreter loop slows down unrelated instructions.
    #[inline(never)]
    pub fn list_mut_storing(&mut self, list: Value, index: usize, stored: Value) -> Option<&mut Vec<Value>> {
        let Value::Obj(h) = list else { return None };
        if stored.handle().is_some_and(|c| !self.is_marked(c)) {
            self.list_barrier(h, index, false);
        }
        if matches!(self.phase, Phase::Marking) {
            self.shade(stored);
        }
        match &mut self.objs[h as usize] {
            Obj::List(l) => Some(l),
            _ => None,
        }
    }

    /// The state of iterator function `v` (an `Obj::IterFn`), to update it:
    /// it holds no new object, so no write barrier is needed.
    #[inline]
    pub fn iter_src_mut(&mut self, v: Value) -> Option<&mut IterSrc> {
        match v {
            Value::Obj(h) => match &mut self.objs[h as usize] {
                Obj::IterFn(src) => Some(src),
                _ => None,
            },
            _ => None,
        }
    }

    /// The pins of map or set `v`, to update them: they are not values, so
    /// no write barrier is needed.
    pub fn pins_mut(&mut self, v: Value) -> Option<&mut Pins> {
        match v {
            Value::Obj(h) => match &mut self.objs[h as usize] {
                Obj::Map(m) => Some(&mut m.pins),
                Obj::Set(s) => Some(&mut s.pins),
                _ => None,
            },
            _ => None,
        }
    }

    /// Mutable access to list `list` for removing elements from its end
    /// only (`pop`, `clear`): no write barrier is needed, as no value is
    /// stored and none moves. But a partially remembered list's pushed tail
    /// may now start past the end, so the next push must record where it
    /// stores (the tail shortcut of `list_barrier_old` assumes growth).
    pub fn list_shrink(&mut self, list: Value) -> Option<&mut Vec<Value>> {
        let Value::Obj(h) = list else { return None };
        clear_bit(&mut self.tail_bits, h);
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
        if matches!(self.phase, Phase::Marking) {
            stored.iter().for_each(|&s| self.shade(s));
        }
        &mut self.objs[h as usize]
    }

    /// Appends `v` to list `list`. If the list must grow and the growth does
    /// not fit in the memory limit, fails without changing anything.
    #[inline]
    pub fn list_push(&mut self, list: Value, v: Value) -> Result<(), LimitKind> {
        let Value::Obj(h) = list else { return Ok(()) };
        let Obj::List(l) = &self.objs[h as usize] else { return Ok(()) };
        let len = l.len();
        if len == l.capacity() {
            self.list_reserve(list, 1)?;
        }
        if v.handle().is_some_and(|c| !self.is_marked(c)) {
            self.list_barrier(h, len, true);
        }
        if matches!(self.phase, Phase::Marking) {
            self.shade(v);
        }
        if let Obj::List(l) = &mut self.objs[h as usize] {
            l.push(v);
        }
        Ok(())
    }

    /// Makes room in list `list` for `extra` more elements (see
    /// [`Heap::grown_capacity`]). If they do not fit in the memory limit,
    /// fails without changing anything.
    #[cold]
    #[inline(never)]
    pub fn list_reserve(&mut self, list: Value, extra: usize) -> Result<(), LimitKind> {
        let Value::Obj(h) = list else { return Ok(()) };
        let Obj::List(l) = &self.objs[h as usize] else { return Ok(()) };
        let (len, cap) = (l.len(), l.capacity());
        let need = len.checked_add(extra).ok_or(LimitKind::Memory)?;
        if need <= cap {
            return Ok(());
        }
        let new_cap = self.grown_capacity(cap, need, VALUE_SIZE, 0)?;
        let Obj::List(l) = &mut self.objs[h as usize] else { unreachable!() };
        l.reserve_exact(new_cap - len);
        self.bytes += (l.capacity() - cap) * VALUE_SIZE;
        Ok(())
    }

    /// Appends the elements of list `src` to list `dst`. The caller must
    /// first make room for them in `dst` (see [`Heap::list_reserve`]).
    pub fn list_extend(&mut self, dst: Value, src: Value) {
        if dst == src {
            self.list_mut(dst).extend_from_within(..);
            return;
        }
        let mut l = std::mem::take(self.list_mut(dst));
        l.extend_from_slice(self.list(src));
        *self.list_mut(dst) = l;
    }

    /// The new capacity for a vector of capacity `cap` and `elem`-byte
    /// elements that must hold `need` elements, when `extra` more bytes are
    /// allocated along with it. The capacity normally doubles. If doubling
    /// does not fit in the memory limit, it grows by an eighth instead, which
    /// keeps appends amortized constant time and lets a script use almost
    /// all of its memory. Fails if that does not fit either.
    pub fn grown_capacity(&self, cap: usize, need: usize, elem: usize, extra: usize) -> Result<usize, LimitKind> {
        let fits = |c: usize| !self.would_exceed((c - cap).saturating_mul(elem).saturating_add(extra));
        let doubled = need.max(cap.saturating_mul(2)).max(4);
        if fits(doubled) {
            return Ok(doubled);
        }
        let least = need.max(cap + cap / 8);
        if fits(least) { Ok(least) } else { Err(LimitKind::Memory) }
    }

    /// Mutable access to channel `ch`, for taking values from it and for
    /// storing `stored`, if any, as a blocked sender's value. Values must
    /// enter the queue only through [`Heap::channel_push`] and
    /// `ChannelObj::take`. The write barrier remembers the channel partially
    /// (see the module documentation).
    pub fn channel_mut(&mut self, ch: Value, stored: Option<Value>) -> Option<&mut ChannelObj> {
        let Value::Obj(h) = ch else { return None };
        if let Some(v) = stored {
            if v.handle().is_some_and(|c| !self.is_marked(c)) && self.is_marked(h) {
                self.channel_barrier_old(h);
            }
            if matches!(self.phase, Phase::Marking) {
                self.shade(v);
            }
        }
        match &mut self.objs[h as usize] {
            Obj::Channel(c) => Some(c),
            _ => None,
        }
    }

    #[cold]
    #[inline(never)]
    fn channel_barrier_old(&mut self, h: u32) {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        if self.remembered_bits[w] & b == 0 {
            self.remembered_bits[w] |= b;
            self.remembered.push(h);
            self.partial_bits[w] |= b;
            if let Obj::Channel(c) = &mut self.objs[h as usize] {
                c.pushed = 0;
            }
        }
    }

    /// Appends `v` to the queue of channel `ch`. If the queue must grow and
    /// the growth does not fit in the memory limit, fails without changing
    /// anything.
    pub fn channel_push(&mut self, ch: Value, v: Value) -> Result<(), LimitKind> {
        let Value::Obj(h) = ch else { return Ok(()) };
        let Obj::Channel(c) = &self.objs[h as usize] else { return Ok(()) };
        let (len, cap) = (c.len(), c.queue().capacity());
        if len == cap {
            let new_cap = self.grown_capacity(cap, len + 1, VALUE_SIZE, 0)?;
            let Obj::Channel(c) = &mut self.objs[h as usize] else { unreachable!() };
            c.reserve_exact(new_cap - len);
            self.bytes += (c.queue().capacity() - cap) * VALUE_SIZE;
        }
        if let Some(c) = self.channel_mut(ch, Some(v)) {
            c.push(v);
        }
        Ok(())
    }

    /// The payload of `v` if it is the single-payload variant `id`, else
    /// `Value::Uninit` (which no value holds): LLVM copies a value out of an
    /// `Option` in unaligned pieces, which stalls the load that reads it back.
    #[inline]
    pub fn payload_if(&self, v: &Value, id: u32) -> Value {
        match *v {
            Value::VUnit(x) if x == id => Value::Unit,
            Value::VBool(x, b) if x == id => Value::Bool(b),
            Value::VInt(x, i) if x == id => Value::Int(i),
            Value::VFloat(x, f) if x == id => Value::Float(f),
            Value::VDur(x, d) if x == id => Value::Dur(d),
            Value::VTag(x, t) if x == id => Value::Tag(t),
            Value::VObj(x, h) if x == id => Value::Obj(h),
            Value::Obj(h) => match &self.objs[h as usize] {
                Obj::Variant(vid, f) if *vid == id => match **f {
                    [p, ..] => p,
                    [] => Value::Uninit,
                },
                _ => Value::Uninit,
            },
            _ => Value::Uninit,
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
            Value::Pair(a, b) => [Value::Int(a as i64), Value::Int(b)].get(i).copied(),
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
            [p] if let Some(v) = Value::variant1(id, *p) => v,
            _ => {
                let fields = self.boxed(fields);
                self.alloc(Obj::Variant(id, fields))
            }
        }
    }

    pub fn native<T: 'static>(&self, v: Value) -> Option<&T> {
        match self.get(v) {
            Obj::Native(n) => (&**n as &dyn std::any::Any).downcast_ref::<T>(),
            _ => None,
        }
    }

    pub fn value<T: ValueType>(&self, v: Value) -> Option<T> {
        value_of(self.get(v))
    }

    pub fn native_mut<T: 'static>(&mut self, v: Value) -> Option<&mut T> {
        // Native objects hold no values: no write barrier needed.
        let Value::Obj(h) = v else { return None };
        match &mut self.objs[h as usize] {
            Obj::Native(n) => (&mut **n as &mut dyn std::any::Any).downcast_mut::<T>(),
            _ => None,
        }
    }

    /// Paces collections by the resources allocated: memory alone may not
    /// call for one for a long time, while each resource that a script
    /// dropped without closing it holds a file or socket until collected.
    /// After [`RESOURCE_BURST`] of them, the next safe point collects (a
    /// major collection once half of `max_open_resources` are open).
    #[cold]
    fn resource_allocated(&mut self) {
        self.young_resources += 1;
        if self.young_resources >= RESOURCE_BURST {
            self.next_gc = 0;
            if self.io_memory.open_resources() >= self.io_memory.max_resources() / 2 {
                self.major_at = 0;
            }
        }
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

    /// Marks every unmarked object reachable from the worklist and from the
    /// containers on `scan`. The worklist is emptied first, so that it never
    /// holds more than one chunk of a big container's children.
    fn trace(&mut self) {
        let Heap { objs, marks, worklist, scan, .. } = self;
        loop {
            let (h, start) = match worklist.pop() {
                Some(h) => (h, 0),
                None => match scan.pop() {
                    Some(c) => c,
                    None => break,
                },
            };
            if let Some(next) = chunk_children(&objs[h as usize], start, |v| mark(marks, worklist, v)) {
                scan.push((h, next));
            }
        }
    }

    /// Puts slot `h` on the free list, keeping a small box for reuse.
    fn release(&mut self, h: u32) {
        let obj = std::mem::replace(&mut self.objs[h as usize], Obj::Free(self.free_head));
        self.free_head = h;
        if let Obj::Tuple(b) | Obj::Struct(_, b) | Obj::Variant(_, b) | Obj::Closure(_, b) = obj
            && let Some(spare) = b.len().checked_sub(1).and_then(|i| self.spare.get_mut(i))
            && spare.len() < SPARE_MAX
        {
            self.spare_bytes += size_of_val(&*b);
            spare.push(b);
        }
    }

    fn drop_spares(&mut self) {
        self.spare = Default::default();
        self.spare_bytes = 0;
    }

    /// Frees object `h`, returning its accounted size.
    fn free(&mut self, h: u32) -> usize {
        let i = h as usize;
        let size = self.objs[i].size();
        self.release(h);
        self.live_objects -= 1;
        clear_bit(&mut self.cmarks, h);
        clear_bit(&mut self.gray_bits, h);
        size
    }

    /// Collects garbage reachable from `roots`: a minor collection (young
    /// objects only) or, if `major`, a full one. Unreachable native resources
    /// are dropped, which closes them.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = Value>, major: bool) {
        if major {
            self.drop_spares();
            self.abandon_cycle();
            self.marks.iter_mut().for_each(|m| *m = 0);
        }
        for r in roots {
            mark(&mut self.marks, &mut self.worklist, r);
        }
        let Heap { objs, marks, worklist, scan, remembered, remembered_bits, partial_bits, tail_bits, dirty, .. } = self;
        for &h in remembered.iter() {
            let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
            let partial = partial_bits[w] & b != 0;
            remembered_bits[w] &= !b;
            partial_bits[w] &= !b;
            tail_bits[w] &= !b;
            if major {
                continue;
            }
            // Scan the old objects that may point to young ones. For a
            // partially remembered object, scan only the changed parts.
            let mut mark = |v: Value| mark(marks, worklist, v);
            match (partial, &objs[h as usize]) {
                (true, Obj::Channel(c)) => {
                    let q = c.queue();
                    q.range(q.len() - c.pushed.min(q.len())..).for_each(|&v| mark(v));
                    c.senders.iter().for_each(|&(_, _, v)| mark(v));
                }
                (true, obj @ (Obj::List(_) | Obj::Map(_) | Obj::Set(_))) => {
                    let d = dirty.remove(&h).unwrap_or_default();
                    for &i in &d.idx {
                        match obj {
                            Obj::List(l) => l.get(i as usize).into_iter().for_each(|&v| mark(v)),
                            Obj::Map(m) => m.entries.get(i as usize).into_iter().flatten().for_each(|&(k, v)| {
                                mark(k);
                                mark(v);
                            }),
                            Obj::Set(s) => s.entries.get(i as usize).into_iter().flatten().for_each(|&v| mark(v)),
                            _ => {}
                        }
                    }
                    if d.from != NONE {
                        scan.push((h, d.from as usize));
                    }
                }
                _ => scan.push((h, 0)),
            }
        }
        dirty.clear();
        remembered.clear();
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
            if matches!(self.phase, Phase::Marking) {
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
        for i in 0..self.objs.len() {
            if matches!(self.objs[i], Obj::Free(_)) {
                continue;
            }
            if self.is_marked(i as u32) {
                live_bytes += self.objs[i].size();
                live += 1;
            } else {
                self.release(i as u32);
            }
        }
        self.bytes = live_bytes;
        self.live_objects = live;
    }

    fn set_thresholds(&mut self, major: bool) {
        self.young_resources = 0;
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
        // Small heaps get a nursery that sees minor collections between major ones.
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

    /// Whether `v` is an object that the sweep of an incremental collection
    /// will free: unreachable, so it must not be made reachable again.
    pub fn condemned(&self, v: Value) -> bool {
        self.phase == Phase::Sweeping && v.handle().is_some_and(|h| !test_bit(&self.cmarks, h))
    }

    /// Whether an incremental major collection is under way.
    pub fn cycle_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Whether a minor collection is due during an incremental cycle.
    pub fn minor_due(&mut self) -> bool {
        if self.stress {
            self.stress_tick += 1;
            return self.stress_tick.is_multiple_of(3);
        }
        self.bytes > self.minor_at
    }

    /// Whether the major collection due should be incremental.
    pub fn major_is_incremental(&self) -> bool {
        self.incremental && if self.stress { self.stats.collections.is_multiple_of(16) } else { self.bytes >= INCREMENTAL_MIN }
    }

    /// Starts an incremental major collection: shades the roots.
    pub fn start_cycle(&mut self, roots: impl IntoIterator<Item = Value>) {
        self.drop_spares();
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
    /// is complete and `finish_marking` must run.
    pub fn cycle_step(&mut self) -> bool {
        let allocated = self.bytes.saturating_sub(self.step_bytes).max(STEP_BYTES);
        self.step_bytes = self.bytes;
        match self.phase {
            Phase::Marking => {
                let budget = if self.stress { (self.live_objects / 8).max(64) } else { (allocated / 16 * MARK_PACE).max(MIN_MARK_WORK) };
                self.mark_some(budget);
                self.gray.is_empty()
            }
            Phase::Sweeping => {
                let slots = self.objs.len();
                let budget = if self.stress {
                    (slots / 8).max(64)
                } else {
                    let share = allocated as u128 * MARK_PACE as u128 * slots as u128 / self.cycle_live.max(1) as u128;
                    let share = usize::try_from(share).unwrap_or(usize::MAX);
                    share.max(MIN_SWEEP_SLOTS)
                };
                self.sweep_some(budget);
                false
            }
            Phase::Idle => false,
        }
    }

    /// Completes the marking: shades the roots again and drains the worklist.
    /// Everything marked is reachable and becomes old; sweeping starts.
    pub fn finish_marking(&mut self, roots: impl IntoIterator<Item = Value>) {
        for r in roots {
            self.shade(r);
        }
        self.mark_some(usize::MAX);
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

    /// Abandons incremental collection.
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
            let next = chunk_children(&self.objs[h as usize], start, |v| children.push(v));
            budget = budget.saturating_sub(children.len() + 1);
            if let Some(k) = next {
                self.gray.push((h, k));
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

/// Calls `f` on the children of `obj`, starting at child `start`. For a big
/// container it stops after `SCAN_CHUNK` children and returns the index to
/// continue from.
///
/// A channel's queue is scanned from the back: child `i` is the `i`th value
/// from the back. Values taken from the front between two chunks then do not
/// shift the values not yet scanned past the scan's position.
#[inline]
fn chunk_children(obj: &Obj, start: usize, mut f: impl FnMut(Value)) -> Option<usize> {
    let chunk = |len: usize| {
        let end = start.saturating_add(SCAN_CHUNK).min(len);
        (start.min(end)..end, (end < len).then_some(end))
    };
    match obj {
        Obj::List(l) => {
            let (r, next) = chunk(l.len());
            l[r].iter().for_each(|&v| f(v));
            next
        }
        Obj::Map(m) => {
            let (r, next) = chunk(m.entries.len());
            for &(k, v) in m.entries[r].iter().flatten() {
                f(k);
                f(v);
            }
            next
        }
        Obj::Set(s) => {
            let (r, next) = chunk(s.entries.len());
            s.entries[r].iter().flatten().for_each(|&v| f(v));
            next
        }
        Obj::Channel(c) => {
            let q = c.queue();
            let (r, next) = chunk(q.len());
            q.range(q.len() - r.end..q.len() - r.start).for_each(|&v| f(v));
            if start == 0 {
                c.senders.iter().for_each(|&(_, _, v)| f(v));
            }
            next
        }
        _ => {
            if start == 0 {
                obj.children(f);
            }
            None
        }
    }
}

/// Marks `v` and queues it for tracing, if it was not already marked. Mark
/// bits stay set between minor collections.
#[inline]
fn mark(marks: &mut [u64], worklist: &mut Vec<u32>, v: Value) {
    if let Some(h) = v.handle() {
        let (w, b) = ((h / 64) as usize, 1u64 << (h % 64));
        if marks[w] & b == 0 {
            marks[w] |= b;
            worklist.push(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_objects_get_the_handles_promised() {
        let mut heap = Heap::new(None, usize::MAX);
        let a = heap.new_str("a");
        let b = heap.new_str("b");
        heap.collect([b], true);
        // `a`'s slot is free: the first staged object takes it, the next
        // ones new slots, as `alloc` would.
        let (s, l) = heap.staged(|h, st| {
            let s = st.new_str(h.str(b).repeat(3));
            let l = st.new_list(vec![s, b]);
            (s, l)
        });
        assert_eq!(heap.str(s), "bbb");
        assert_eq!(heap.list(l), &[s, b]);
        assert_eq!(s, a, "the free slot is reused");
        let c = heap.new_str("c");
        assert_eq!(c.handle(), l.handle().map(|h| h + 1));
    }

    #[test]
    fn dirty_index_lists_stay_small() {
        let mut heap = Heap::new(None, usize::MAX);
        let lists: Vec<Value> = (0..100).map(|_| heap.new_list(vec![Value::Int(0)])).collect();
        let big = heap.new_list(vec![Value::Int(0); 800]);
        let mut roots = lists.clone();
        roots.push(big);
        heap.collect(roots.iter().copied(), false);
        let young = heap.new_str("young");
        for &l in lists.iter().chain([&big]) {
            for _ in 0..1000 {
                heap.list_mut_storing(l, 0, young).unwrap()[0] = young;
            }
        }
        let recorded: usize = heap.dirty.values().map(|d| d.idx.len()).sum();
        assert!(recorded <= 200, "{recorded} indices recorded");
        heap.collect(roots.iter().copied(), false);
        assert_eq!(heap.str(heap.list(big)[0]), "young");
        assert_eq!(heap.str(heap.list(lists[7])[0]), "young");
    }

    fn new_channel(heap: &mut Heap) -> Value {
        heap.alloc(Obj::Channel(Box::new(ChannelObj::new(0))))
    }

    fn queued(heap: &Heap, ch: Value) -> Vec<String> {
        let Obj::Channel(c) = heap.get(ch) else { panic!("not a channel") };
        c.queue().iter().map(|&v| heap.str(v).to_string()).collect()
    }

    #[test]
    fn young_values_in_old_channels_survive_minor_collections() {
        let mut heap = Heap::new(None, usize::MAX);
        let ch = new_channel(&mut heap);
        for i in 0..1000 {
            let v = heap.new_str(format!("old value {i}"));
            heap.channel_push(ch, v).unwrap();
        }
        heap.collect([ch], false);
        let v = heap.new_str("a young value, then ints");
        heap.channel_push(ch, v).unwrap();
        for i in 0..100 {
            heap.channel_push(ch, Value::Int(i)).unwrap();
        }
        let Obj::Channel(c) = heap.get(ch) else { unreachable!() };
        assert_eq!(c.pushed, 101, "only the values pushed since the channel was remembered are scanned");
        // After values are taken from the front, the young value is still among the last pushed.
        for _ in 0..500 {
            heap.channel_mut(ch, None).unwrap().take();
        }
        heap.collect([ch], false);
        let Obj::Channel(c) = heap.get(ch) else { unreachable!() };
        assert_eq!(heap.str(c.queue()[500]), "a young value, then ints");
    }

    #[test]
    fn values_of_blocked_senders_survive_minor_collections() {
        let mut heap = Heap::new(None, usize::MAX);
        let ch = heap.alloc(Obj::Channel(Box::new(ChannelObj::new(1))));
        let first = heap.new_str("first");
        heap.channel_push(ch, first).unwrap();
        heap.collect([ch], false);
        let blocked = heap.new_str("blocked");
        heap.channel_mut(ch, Some(blocked)).unwrap().senders.push_back((7, 1, blocked));
        heap.collect([ch], false);
        // Taking a value moves the blocked sender's value into the queue.
        let (v, woken) = heap.channel_mut(ch, None).unwrap().take().unwrap();
        assert_eq!((heap.str(v), woken), ("first", Some((7, 1))));
        let other = heap.new_str("other");
        heap.collect([ch, other], false);
        heap.collect([ch], false);
        assert_eq!(queued(&heap, ch), ["blocked"]);
        let more = heap.new_str("more");
        heap.channel_mut(ch, Some(more)).unwrap().senders.push_back((8, 1, more));
        heap.channel_mut(ch, None).unwrap().take();
        heap.collect([ch], false);
        assert_eq!(queued(&heap, ch), ["more"]);
    }

    #[test]
    fn incremental_marking_of_a_channel_skips_nothing_taken_meanwhile() {
        let mut heap = Heap::new(None, usize::MAX);
        let ch = new_channel(&mut heap);
        let n = 3 * SCAN_CHUNK;
        for i in 0..n {
            let v = heap.new_str(format!("value number {i}"));
            heap.channel_push(ch, v).unwrap();
        }
        heap.collect([ch], true);
        heap.start_cycle([ch]);
        heap.mark_some(SCAN_CHUNK + 2);
        // Take values from the front while the queue is partly scanned.
        for _ in 0..SCAN_CHUNK {
            heap.channel_mut(ch, None).unwrap().take();
        }
        for i in 0..10 {
            let v = heap.new_str(format!("pushed meanwhile {i}"));
            heap.channel_push(ch, v).unwrap();
        }
        heap.finish_marking([ch]);
        while heap.cycle_active() {
            heap.cycle_step();
        }
        let q = queued(&heap, ch);
        assert_eq!(q.len(), n - SCAN_CHUNK + 10);
        assert_eq!(q[0], format!("value number {SCAN_CHUNK}"));
        assert_eq!(q[n - SCAN_CHUNK - 1], format!("value number {}", n - 1));
        assert_eq!(q[n - SCAN_CHUNK + 9], "pushed meanwhile 9");
    }

    #[test]
    fn tracing_keeps_big_containers_in_chunks() {
        let mut heap = Heap::new(None, usize::MAX);
        let items: Vec<Value> = (0..10 * SCAN_CHUNK).map(|i| heap.new_str(format!("an item, {i}"))).collect();
        let list = heap.new_list(items.clone());
        let ch = new_channel(&mut heap);
        for &v in &items {
            heap.channel_push(ch, v).unwrap();
        }
        heap.collect([list, ch], true);
        assert!(heap.worklist.capacity() <= 4 * SCAN_CHUNK, "worklist of {}", heap.worklist.capacity());
        assert_eq!(heap.str(heap.list(list)[10 * SCAN_CHUNK - 1]), format!("an item, {}", 10 * SCAN_CHUNK - 1));
        // A minor collection also scans a fully remembered old container in chunks.
        let young = heap.new_str("young");
        heap.list_mut(list)[0] = young;
        heap.worklist = Vec::new();
        heap.collect([list], false);
        assert!(heap.worklist.capacity() <= 4 * SCAN_CHUNK, "worklist of {}", heap.worklist.capacity());
        assert_eq!(heap.str(heap.list(list)[0]), "young");
    }

    #[test]
    fn containers_grow_only_within_the_memory_limit() {
        let limit = 1 << 20;
        let mut heap = Heap::new(Some(limit), usize::MAX);
        let list = heap.new_list(Vec::new());
        let mut n = 0;
        while heap.list_push(list, Value::Int(n)).is_ok() {
            n += 1;
        }
        let cap = heap.list(list).len();
        assert!(heap.used() <= limit, "{} bytes used", heap.used());
        // Near the limit, the list grew by less than double.
        assert!(cap * VALUE_SIZE > limit * 3 / 4, "only {cap} elements fit");
        let Obj::List(l) = heap.get(list) else { unreachable!() };
        assert_eq!(l.len(), l.capacity(), "a failed push changes nothing");
        assert_eq!(heap.list_reserve(list, 1), Err(LimitKind::Memory));
        let ch = new_channel(&mut heap);
        while heap.channel_push(ch, Value::Int(0)).is_ok() {}
        assert!(heap.used() <= limit, "{} bytes used", heap.used());
        heap.collect([ch], true);
        heap.channel_push(ch, Value::Int(0)).unwrap();
    }
}
