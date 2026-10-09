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
//! * Lists remembered only through `list_push`/`list_mut_storing` are *partially*
//!   remembered: the minor collection scans just the stored indices and the
//!   pushed tail (`DirtyList`) instead of the whole list. Any other mutation,
//!   or too many scattered stores, makes the remembered entry a full one.

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
/// Bytes allocated between minor collections.
const NURSERY: usize = 8 << 20;

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
            }
            (self.objs.len() - 1) as u32
        };
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
        self.bytes > self.major_at
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
        match &mut self.objs[h as usize] {
            Obj::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn list_push(&mut self, list: Value, v: Value) {
        let Value::Obj(h) = list else { return };
        if v.handle().is_some_and(|c| !self.is_marked(c)) {
            let len = self.list(list).len();
            self.list_barrier(h, len, true);
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
        size
    }

    /// Collects garbage reachable from `roots`: a minor collection (young
    /// objects only) or, if `major`, a full one. Unreachable native resources
    /// are dropped, which closes them.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = Value>, major: bool) {
        if major {
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
        self.next_gc = if self.stress { 0 } else { self.bytes.saturating_add(NURSERY).min(self.major_at) };
    }
}
