//! Garbage-collected heap.
//!
//! The heap stores [`Obj`] values in an arena of slots addressed by `u32`
//! handles. A precise, non-moving, generational mark-and-sweep collector
//! reclaims unreachable objects.
//!
//! ## Safety Rules for Callers
//!
//! The VM runs garbage collection at safe points. It never collects during
//! a native call, and so native functions may hold [`VmValue`] values in local
//! variables without rooting them. Callers must not hold a `&mut Obj` across
//! a safe point.
//!
//! To mutate an object that holds [`VmValue`] values, call [`Heap::get_mut`],
//! [`Heap::get_mut_storing`], or a container method built on them. Those
//! methods record the write barriers required by the collector.
//!
//! ## Generational Collection
//!
//! The collector divides objects into young and old generations using sticky
//! bits in `mark_bits`. New objects start unmarked and enter `young`.
//! Reachable objects keep their mark bits set after a collection and become
//! old.
//!
//! - A minor collection traces from roots and `remembered`. It traces only
//!   unmarked young objects and sweeps only `young`.
//! - A major collection sweeps the full heap to reclaim all unreachable
//!   objects. It runs if `object_bytes` exceeds `next_major_gc_bytes`, or if a
//!   minor collection leaves memory usage above `limit`.
//!
//! ## Incremental Major Collection
//!
//! For large heaps, the collector runs major collections in allocation-paced
//! marking and sweeping steps. Minor collections still run between steps.
//!
//! - **Mark** traces reachable objects into `cycle_mark_bits` using a bounded
//!   worklist in `gray_worklist`. During marking, write barriers mark newly
//!   stored values or requeue mutated objects.
//! - **Complete** rescans the roots and drains `gray_worklist`. It then copies
//!   `cycle_mark_bits` into `mark_bits` and clears `remembered` and `young`.
//! - **Sweep** frees unmarked slots in bounded steps. The allocator sets bits in
//!   `cycle_mark_bits` for objects created during sweeping. If a memory limit
//!   requires immediate reclamation, the collector aborts the incremental cycle
//!   and runs a full major collection.

use super::iomem::IoMemory;
use super::value::*;
use crate::engine::LimitKind;
use compact_str::CompactString;
use std::sync::Arc;
use std::time::Duration;

/// Garbage-collected object arena for a single VM instance.
pub struct Heap {
    // Keep hot arena and allocation fields first for cache locality.
    slots: Vec<Obj>,
    /// Sticky mark bits set for old objects retained by a prior collection.
    mark_bits: BitSet,
    /// Bitmask deduplicating entries in `remembered`.
    remembered_bits: BitSet,
    /// Old object slots mutated since the last collection.
    remembered: Vec<u32>,
    /// Bitmask of remembered containers tracked with partial write barriers.
    partial_barrier_bits: BitSet,
    /// Bitmask of partially remembered containers whose pushed tail is already tracked.
    pushed_tail_bits: BitSet,
    /// Modified indices and pushed tails for partially remembered containers.
    dirty_containers: FxHashMap<u32, DirtyContainer>,
    /// Slots allocated since the last collection.
    young: Vec<u32>,
    free_list_head: u32,
    /// Estimated bytes held by heap objects.
    ///
    /// This value is exact after a major collection. Between collections, it
    /// also includes unreachable objects.
    pub object_bytes: usize,
    /// Triggers a collection if `object_bytes` exceeds this threshold.
    pub next_gc_bytes: usize,
    /// Triggers a major collection while idle if `object_bytes` exceeds this threshold.
    pub next_major_gc_bytes: usize,
    pub limit: Option<usize>,
    /// Byte capacity of collector bitmaps, worklists, and `dirty_containers`.
    table_bytes: usize,
    /// Peak length of `slots` since the collector last shrank the arena.
    arena_peak_slots: usize,
    /// Bytes used by fibers, counted with the heap against `limit`.
    ///
    /// The VM increments this value during fiber allocation and recomputes it
    /// during each collection.
    pub fiber_bytes: usize,
    pub live_objects: usize,
    /// Open resources allocated since the last collection; triggers a collection at [`MAX_YOUNG_RESOURCES`].
    young_resources: usize,
    /// Triggers garbage collection at every safe point for stress testing.
    pub gc_stress: bool,
    /// Forces the next memory-limit check to fail for retry stress testing.
    pub(crate) fail_next_check: std::cell::Cell<bool>,
    /// Disables memory-limit checks during post-effect result conversion.
    pub(crate) ignore_memory_limit: bool,
    /// Memory reserved for pending I/O buffers, counted against `limit`.
    pub io_memory: Arc<IoMemory>,
    worklist: Vec<u32>,
    /// Large containers being traced, paired with the next child index to scan.
    scan_chunks: Vec<(u32, usize)>,
    pub stats: GcStats,
    cycle_phase: IncrementalGcPhase,
    /// Mark bits for the active incremental major collection.
    cycle_mark_bits: BitSet,
    /// Bitmask deduplicating full-scan entries in `gray_worklist`.
    gray_bits: BitSet,
    /// Slots queued for incremental marking, paired with the next child index to scan.
    gray_worklist: Vec<(u32, usize)>,
    /// Next slot index to inspect during incremental sweeping.
    sweep_cursor: usize,
    /// Value of `object_bytes` at the end of the last incremental step.
    bytes_at_last_step: usize,
    /// Value of `object_bytes` at the start of the active incremental cycle.
    cycle_start_bytes: usize,
    /// Triggers a minor collection if `object_bytes` exceeds this threshold.
    next_minor_gc_bytes: usize,
    /// Counter used to interleave minor collections and incremental steps during stress testing.
    stress_tick: u64,
    /// Enables incremental major collections for large heaps; enabled by default.
    pub incremental: bool,
    /// Freed value boxes cached by length for reuse by [`Heap::box_values`].
    ///
    /// Cached bytes count toward memory usage until a major collection drops
    /// them.
    spare_boxes: [Vec<Box<[VmValue]>>; MAX_SPARE_BOX_LEN],
    /// Reusable chunks of [`STAGE_CHUNK_LEN`] entries for [`Staging`], dropped on major collections.
    spare_chunks: Vec<Vec<(u32, Obj)>>,
    /// Total bytes held in `spare_boxes` and `spare_chunks`.
    spare_bytes: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new(None, usize::MAX)
    }
}

impl Heap {
    /// Creates a heap with an optional `limit` in bytes and at most
    /// `max_resources` open resources.
    pub fn new(limit: Option<usize>, max_resources: usize) -> Heap {
        Heap {
            slots: Vec::new(),
            mark_bits: BitSet::default(),
            remembered_bits: BitSet::default(),
            remembered: Vec::new(),
            partial_barrier_bits: BitSet::default(),
            pushed_tail_bits: BitSet::default(),
            dirty_containers: FxHashMap::default(),
            young: Vec::new(),
            free_list_head: FREE_LIST_END,
            object_bytes: 0,
            next_gc_bytes: MIN_GC_BYTES,
            next_major_gc_bytes: MIN_GC_BYTES,
            limit,
            table_bytes: 0,
            arena_peak_slots: 0,
            fiber_bytes: 0,
            live_objects: 0,
            young_resources: 0,
            gc_stress: false,
            fail_next_check: std::cell::Cell::new(false),
            ignore_memory_limit: false,
            io_memory: IoMemory::new(limit, max_resources),
            worklist: Vec::new(),
            scan_chunks: Vec::new(),
            stats: GcStats::default(),
            cycle_phase: IncrementalGcPhase::Idle,
            cycle_mark_bits: BitSet::default(),
            gray_bits: BitSet::default(),
            gray_worklist: Vec::new(),
            sweep_cursor: 0,
            bytes_at_last_step: 0,
            cycle_start_bytes: 0,
            next_minor_gc_bytes: MIN_GC_BYTES,
            stress_tick: 0,
            incremental: true,
            spare_boxes: Default::default(),
            spare_chunks: Vec::new(),
            spare_bytes: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.live_objects
    }

    pub fn is_empty(&self) -> bool {
        self.live_objects == 0
    }

    /// Stores `obj` in the arena as a young object and returns its handle.
    #[inline]
    pub fn alloc(&mut self, obj: Obj) -> VmValue {
        self.object_bytes += obj.size();
        self.live_objects += 1;
        if let Obj::Native(native) = &obj
            && native.is_resource()
        {
            self.record_resource_allocation();
        }
        // Free and newly appended slots have clear mark bits, so the object starts young.
        let handle = if self.free_list_head != FREE_LIST_END {
            let handle = self.free_list_head;
            match self.slots[handle as usize] {
                Obj::Free(next) => self.free_list_head = next,
                _ => unreachable!("corrupt free list"),
            }
            self.slots[handle as usize] = obj;
            handle
        } else {
            self.slots.push(obj);
            if self.mark_bits.slot_capacity() < self.slots.len() {
                self.mark_bits.push_word();
                self.remembered_bits.push_word();
                self.partial_barrier_bits.push_word();
                self.pushed_tail_bits.push_word();
                self.cycle_mark_bits.push_word();
                self.gray_bits.push_word();
                self.update_table_bytes();
            }
            (self.slots.len() - 1) as u32
        };
        if self.cycle_phase == IncrementalGcPhase::Sweeping {
            // Mark the new object so the active sweep retains it.
            self.cycle_mark_bits.insert(handle);
        }
        self.young.push(handle);
        VmValue::Obj(handle)
    }

    /// Runs `f` with shared read access to the heap and a [`Staging`] buffer
    /// for new objects.
    ///
    /// This method lets callers read existing heap objects while building new
    /// objects without intermediate copies. The heap commits all staged objects
    /// after `f` returns. Handles returned by [`Staging::alloc`] may be stored
    /// in other staged objects and become live upon return.
    pub fn staged<R>(&mut self, f: impl FnOnce(&Heap, &mut Staging<'_>) -> R) -> R {
        let spare_chunks = std::mem::take(&mut self.spare_chunks);
        self.spare_bytes -= spare_chunks.len() * STAGE_CHUNK_BYTES;
        let mut staging = Staging {
            heap: self,
            next_free_slot: self.free_list_head,
            next_arena_slot: self.slots.len(),
            chunks: Vec::new(),
            total_capacity: 0,
            spare_chunks,
            staged_bytes: 0,
        };
        let result = f(self, &mut staging);
        let (next_free_slot, chunks, mut spare_chunks) = (staging.next_free_slot, staging.chunks, staging.spare_chunks);
        for mut chunk in chunks {
            for (handle, obj) in chunk.drain(..) {
                let value = self.alloc(obj);
                debug_assert_eq!(value, VmValue::Obj(handle), "staged handles follow the free list, then the arena end");
            }
            if chunk.capacity() == STAGE_CHUNK_LEN {
                spare_chunks.push(chunk);
            }
        }
        debug_assert_eq!(self.free_list_head, next_free_slot);
        self.spare_bytes += spare_chunks.len() * STAGE_CHUNK_BYTES;
        self.spare_chunks = spare_chunks;
        result
    }

    /// Copies `items` into a boxed slice, reusing a cached box if available.
    #[inline]
    pub fn box_values(&mut self, items: &[VmValue]) -> Box<[VmValue]> {
        match items.len().checked_sub(1).and_then(|i| self.spare_boxes.get_mut(i)).and_then(Vec::pop) {
            Some(mut boxed) => {
                self.spare_bytes -= size_of_val(&*boxed);
                boxed.copy_from_slice(items);
                boxed
            }
            None => items.into(),
        }
    }

    pub fn new_str(&mut self, text: impl Into<CompactString>) -> VmValue {
        self.alloc(Obj::Str(text.into()))
    }

    pub fn new_bytes(&mut self, bytes: impl Into<Box<[u8]>>) -> VmValue {
        self.alloc(Obj::Bytes(bytes.into()))
    }

    pub fn new_list(&mut self, items: Vec<VmValue>) -> VmValue {
        self.alloc(Obj::List(items))
    }

    /// Builds a tuple from `items`. Uses [`VmValue::Pair`] if `items` is an
    /// inline pair of integers.
    pub fn new_tuple(&mut self, items: &[VmValue]) -> VmValue {
        match *items {
            [a, b] if let Some(value) = VmValue::tuple2(a, b) => value,
            _ => {
                let items = self.box_values(items);
                self.alloc(Obj::Tuple(items))
            }
        }
    }

    /// Constructs variant `id` with `fields` in its canonical representation.
    ///
    /// Empty variants use a tag, eligible single-field variants use an inline
    /// value, and all other variants allocate a heap object.
    #[inline(never)]
    pub fn new_variant(&mut self, id: u32, fields: &[VmValue]) -> VmValue {
        match fields {
            [] => VmValue::Tag(id),
            [payload] if let Some(value) = VmValue::variant1(id, *payload) => value,
            _ => {
                let fields = self.box_values(fields);
                self.alloc(Obj::Variant(id, fields))
            }
        }
    }

    // ------------------------------------------------------------------
    // Memory accounting and limits

    /// Returns total bytes used by the heap and fibers, excluding pending I/O.
    pub fn used_bytes(&self) -> usize {
        self.object_bytes.saturating_add(self.fiber_bytes).saturating_add(self.spare_bytes).saturating_add(self.overhead_bytes())
    }

    /// Returns `true` if `extra` additional bytes exceed the memory limit, including pending I/O buffers.
    pub fn would_exceed_memory_limit(&self, extra: usize) -> bool {
        if self.fail_next_check.get() && !self.ignore_memory_limit {
            self.fail_next_check.set(false);
            return true;
        }
        match self.limit {
            Some(limit) => {
                let used = self.used_bytes();
                // Publish current heap usage before growth so pending I/O checks see an up-to-date total.
                self.io_memory.publish_heap(used);
                !self.ignore_memory_limit && used.saturating_add(self.io_memory.reserved()).saturating_add(extra) > limit
            }
            None => extra > super::iomem::UNLIMITED,
        }
    }

    /// Returns a memory-limit error if `extra` additional bytes exceed the heap limit.
    /// Allocate the bytes immediately after this check, or use [`Scratch`](super::meter::Scratch) to track a growing buffer.
    pub fn check_memory(&self, extra: usize) -> Result<(), crate::vm::Fail> {
        if self.would_exceed_memory_limit(extra) { Err(crate::vm::Fail::limit(LimitKind::Memory)) } else { Ok(()) }
    }

    /// Returns `true` if current memory usage exceeds `limit`.
    pub fn is_over_memory_limit(&self) -> bool {
        matches!(self.limit, Some(limit) if self.used_bytes().saturating_add(self.io_memory.reserved()) > limit)
    }

    /// Reports an in-place change in an object's memory usage.
    /// Call after mutating an object's internal buffers directly.
    #[inline]
    pub fn report_size_change(&mut self, before: usize, after: usize) {
        if after >= before {
            self.object_bytes += after - before;
        } else {
            self.object_bytes = self.object_bytes.saturating_sub(before - after);
        }
    }

    /// Computes the next capacity for a vector of `elem_size`-byte elements
    /// that must hold `needed` elements alongside `extra_bytes` of allocation.
    ///
    /// This method doubles `cap` if the result fits within the memory limit.
    /// Otherwise, it increases `cap` by 1/8 to preserve amortized constant-time
    /// appends near the limit. Returns [`LimitKind::Memory`] if neither size
    /// fits.
    pub fn checked_grow_capacity(&self, cap: usize, needed: usize, elem_size: usize, extra_bytes: usize) -> Result<usize, LimitKind> {
        let fits =
            |candidate: usize| !self.would_exceed_memory_limit((candidate - cap).saturating_mul(elem_size).saturating_add(extra_bytes));
        let doubled = needed.max(cap.saturating_mul(2)).max(4);
        if fits(doubled) {
            return Ok(doubled);
        }
        let least = needed.max(cap + cap / 8);
        if fits(least) { Ok(least) } else { Err(LimitKind::Memory) }
    }

    // ------------------------------------------------------------------
    // Object access and mutation

    /// Returns the object for `value`, or `Obj::Free(0)` if `value` is not a handle.
    #[inline]
    pub fn get(&self, value: VmValue) -> &Obj {
        match value {
            VmValue::Obj(handle) => &self.slots[handle as usize],
            _ => &Obj::Free(0),
        }
    }

    /// Returns the object at `handle`.
    #[inline]
    pub fn obj(&self, handle: u32) -> &Obj {
        &self.slots[handle as usize]
    }

    /// Returns the object at untraced `handle`, or `None` if `handle` is past the end of the arena.
    pub fn get_obj(&self, handle: u32) -> Option<&Obj> {
        self.slots.get(handle as usize)
    }

    /// Returns a mutable reference to the object for `value` and records a
    /// write barrier.
    ///
    /// Callers must use this method or a specialized barriered accessor to
    /// mutate [`VmValue`] fields inside a heap object.
    #[inline]
    pub fn get_mut(&mut self, value: VmValue) -> &mut Obj {
        match value {
            VmValue::Obj(handle) => {
                self.record_full_barrier(handle);
                if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
                    self.requeue_for_incremental_scan(handle);
                }
                &mut self.slots[handle as usize]
            }
            _ => panic!("internal error: not an object"),
        }
    }

    /// Returns a mutable reference to `value` and records a precise write
    /// barrier for `stored`.
    ///
    /// Callers must store only values from `stored` into the object. The
    /// collector remembers the object only if `stored` contains a young handle.
    #[inline]
    pub fn get_mut_storing(&mut self, value: VmValue, stored: &[VmValue]) -> &mut Obj {
        match value {
            VmValue::Obj(handle) => {
                if stored.iter().any(|item| item.handle().is_some_and(|child| !self.is_marked(child))) {
                    self.record_full_barrier(handle);
                }
                if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
                    stored.iter().for_each(|&item| self.mark_incremental(item));
                }
                &mut self.slots[handle as usize]
            }
            _ => panic!("internal error: not an object"),
        }
    }

    /// Stores `value` into `cell`.
    #[inline]
    pub fn set_cell(&mut self, cell: VmValue, value: VmValue) {
        let obj = match (cell, value.handle()) {
            (VmValue::Obj(handle), None) => &mut self.slots[handle as usize],
            _ => self.get_mut_storing(cell, &[value]),
        };
        if let Obj::Cell(slot) = obj {
            *slot = value;
        }
    }

    // ------------------------------------------------------------------
    // Typed accessors and container operations

    pub fn str(&self, value: VmValue) -> &str {
        match self.get(value) {
            Obj::Str(text) => text,
            Obj::StrBuf(text) => text,
            _ => "",
        }
    }

    /// Returns the byte count for metering [`Heap::append_str`].
    ///
    /// If `base` is a mutable string buffer separate from `suffix`,
    /// returns the length of `suffix`. Otherwise, returns the combined
    /// length of `base` and `suffix`.
    #[inline(never)]
    pub fn append_str_byte_cost(&self, base: VmValue, suffix: VmValue) -> usize {
        let extra = self.str(suffix).len();
        match base {
            VmValue::Obj(handle) if suffix.handle() != Some(handle) && matches!(self.slots[handle as usize], Obj::StrBuf(_)) => extra,
            _ => self.str(base).len().saturating_add(extra),
        }
    }

    /// Appends `suffix` to `base` for `Op::AppendStr`.
    ///
    /// This method appends in place if `base` is a mutable string buffer and
    /// `suffix` is a different object. Otherwise, it allocates a new string
    /// buffer containing `base` followed by `suffix`. On a memory-limit error,
    /// the heap remains unchanged.
    pub fn append_str(&mut self, base: VmValue, suffix: VmValue) -> Result<VmValue, LimitKind> {
        let extra = self.str(suffix).len();
        if let VmValue::Obj(handle) = base
            && suffix.handle() != Some(handle)
            && let Obj::StrBuf(buf) = &self.slots[handle as usize]
        {
            let (len, cap) = (buf.len(), buf.capacity());
            if len < cap
                && let &[byte] = self.str(suffix).as_bytes()
                && byte.is_ascii()
            {
                // Fast path for appending a single ASCII byte.
                if let Obj::StrBuf(buf) = &mut self.slots[handle as usize] {
                    buf.push(byte as char);
                }
                return Ok(base);
            }
            if len + extra <= cap && extra <= 16 {
                // Copy short suffixes through a stack buffer to avoid aliasing borrows in `slots`.
                let mut piece = [0u8; 16];
                piece[..extra].copy_from_slice(self.str(suffix).as_bytes());
                if let (Obj::StrBuf(buf), Ok(piece)) = (&mut self.slots[handle as usize], std::str::from_utf8(&piece[..extra])) {
                    buf.push_str(piece);
                }
                return Ok(base);
            }
            let grow = if len + extra > cap { (len + extra).max(2 * cap) - cap } else { 0 };
            if grow > 0 && self.would_exceed_memory_limit(grow) {
                return Err(LimitKind::Memory);
            }
            let Obj::StrBuf(buf) = &mut self.slots[handle as usize] else { unreachable!() };
            let mut text = std::mem::take(buf);
            text.reserve_exact(cap + grow - len);
            text.push_str(self.str(suffix));
            self.object_bytes += text.capacity() - cap;
            self.slots[handle as usize] = Obj::StrBuf(text);
            return Ok(base);
        }
        let len = self.str(base).len();
        let cap = (2 * (len + extra)).max(16);
        if self.would_exceed_memory_limit(cap) {
            return Err(LimitKind::Memory);
        }
        let mut text = String::with_capacity(cap);
        text.push_str(self.str(base));
        text.push_str(self.str(suffix));
        Ok(self.alloc(Obj::StrBuf(text)))
    }

    /// Appends `text` to `base` for `Op::AppendInterp`, matching [`Heap::append_str`].
    pub fn append_text(&mut self, base: VmValue, text: &str) -> Result<VmValue, LimitKind> {
        if let VmValue::Obj(handle) = base
            && let Obj::StrBuf(buf) = &mut self.slots[handle as usize]
        {
            let (len, cap) = (buf.len(), buf.capacity());
            if len + text.len() <= cap {
                buf.push_str(text);
                return Ok(base);
            }
            let grow = (len + text.len()).max(2 * cap) - cap;
            if self.would_exceed_memory_limit(grow) {
                return Err(LimitKind::Memory);
            }
            let Obj::StrBuf(buf) = &mut self.slots[handle as usize] else { unreachable!() };
            buf.reserve_exact(cap + grow - len);
            buf.push_str(text);
            self.object_bytes += buf.capacity() - cap;
            return Ok(base);
        }
        let len = self.str(base).len();
        let cap = (2 * (len + text.len())).max(16);
        if self.would_exceed_memory_limit(cap) {
            return Err(LimitKind::Memory);
        }
        let mut buf = String::with_capacity(cap);
        buf.push_str(self.str(base));
        buf.push_str(text);
        Ok(self.alloc(Obj::StrBuf(buf)))
    }

    /// Converts a string buffer into an immutable string for `Op::FreezeStr`.
    #[inline]
    pub fn freeze_str(&mut self, value: VmValue) {
        if let VmValue::Obj(handle) = value
            && let Obj::StrBuf(_) = self.slots[handle as usize]
        {
            self.freeze_str_slow(handle);
        }
    }

    pub fn bytes(&self, value: VmValue) -> &[u8] {
        match self.get(value) {
            Obj::Bytes(bytes) => bytes,
            _ => &[],
        }
    }

    pub fn list(&self, value: VmValue) -> &[VmValue] {
        match self.get(value) {
            Obj::List(list) => list,
            _ => &[],
        }
    }

    /// Returns a mutable reference to `value` and records a full write barrier.
    /// Panics if `value` is not a list.
    pub fn list_mut(&mut self, value: VmValue) -> &mut Vec<VmValue> {
        match self.get_mut(value) {
            Obj::List(list) => list,
            other => panic!("internal error: expected list, found {other:?}"),
        }
    }

    /// Returns a mutable reference to `list` for storing `stored` at `index`,
    /// or `None` if `list` is not a list.
    ///
    /// Callers must store only `stored` at `index`. The collector records a
    /// precise partial write barrier for that slot.
    // Keep out of line so inlining does not slow unrelated interpreter opcodes.
    #[inline(never)]
    pub fn list_mut_storing(&mut self, list: VmValue, index: usize, stored: VmValue) -> Option<&mut Vec<VmValue>> {
        let VmValue::Obj(handle) = list else { return None };
        if stored.handle().is_some_and(|child| !self.is_marked(child)) {
            self.record_partial_container_barrier(handle, index, false);
        }
        if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
            self.mark_incremental(stored);
        }
        match &mut self.slots[handle as usize] {
            Obj::List(items) => Some(items),
            _ => None,
        }
    }

    /// Returns a mutable reference to `list` for `pop` or `clear` without
    /// recording a full write barrier, or `None` if `list` is not a list.
    ///
    /// Callers must not store or shift values.
    pub fn list_mut_truncating(&mut self, list: VmValue) -> Option<&mut Vec<VmValue>> {
        let VmValue::Obj(handle) = list else { return None };
        // Clear the pushed-tail shortcut because truncation can move the list end below `tail_start`.
        self.pushed_tail_bits.remove(handle);
        match &mut self.slots[handle as usize] {
            Obj::List(items) => Some(items),
            _ => None,
        }
    }

    /// Appends `value` to `list`.
    ///
    /// Returns [`LimitKind::Memory`] without modifying `list` if growing the
    /// backing buffer exceeds the memory limit.
    #[inline]
    pub fn list_push(&mut self, list: VmValue, value: VmValue) -> Result<(), LimitKind> {
        let VmValue::Obj(handle) = list else { return Ok(()) };
        let Obj::List(items) = &self.slots[handle as usize] else { return Ok(()) };
        let len = items.len();
        if len == items.capacity() {
            self.list_reserve(list, 1)?;
        }
        if value.handle().is_some_and(|child| !self.is_marked(child)) {
            self.record_partial_container_barrier(handle, len, true);
        }
        if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
            self.mark_incremental(value);
        }
        if let Obj::List(items) = &mut self.slots[handle as usize] {
            items.push(value);
        }
        Ok(())
    }

    /// Reserves capacity in `list` for at least `extra` additional elements.
    ///
    /// Returns [`LimitKind::Memory`] without modifying `list` if the new
    /// capacity exceeds the memory limit.
    #[cold]
    #[inline(never)]
    pub fn list_reserve(&mut self, list: VmValue, extra: usize) -> Result<(), LimitKind> {
        let VmValue::Obj(handle) = list else { return Ok(()) };
        let Obj::List(items) = &self.slots[handle as usize] else { return Ok(()) };
        let (len, cap) = (items.len(), items.capacity());
        let needed = len.checked_add(extra).ok_or(LimitKind::Memory)?;
        if needed <= cap {
            return Ok(());
        }
        let new_cap = self.checked_grow_capacity(cap, needed, VALUE_SIZE, 0)?;
        let Obj::List(items) = &mut self.slots[handle as usize] else { unreachable!() };
        items.reserve_exact(new_cap - len);
        self.object_bytes += (items.capacity() - cap) * VALUE_SIZE;
        Ok(())
    }

    /// Appends all elements of `src` to `dst`.
    ///
    /// Callers must reserve capacity in `dst` first using [`Heap::list_reserve`].
    pub fn list_extend(&mut self, dst: VmValue, src: VmValue) {
        if dst == src {
            self.list_mut(dst).extend_from_within(..);
            return;
        }
        let mut items = std::mem::take(self.list_mut(dst));
        items.extend_from_slice(self.list(src));
        *self.list_mut(dst) = items;
    }

    /// Returns a mutable reference to map or set `container` for storing
    /// `stored` at `entry_index`.
    ///
    /// Set `is_push` to `true` if appending at the end of the entry table.
    /// Callers must store only values from `stored` at `entry_index`.
    pub fn entries_mut_storing(&mut self, container: VmValue, entry_index: usize, is_push: bool, stored: &[VmValue]) -> &mut Obj {
        let VmValue::Obj(handle) = container else { panic!("internal error: not an object") };
        if stored.iter().any(|item| item.handle().is_some_and(|child| !self.is_marked(child))) {
            self.record_partial_container_barrier(handle, entry_index, is_push);
        }
        if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
            stored.iter().for_each(|&item| self.mark_incremental(item));
        }
        &mut self.slots[handle as usize]
    }

    /// Returns a mutable reference to the state of iterator `value`, or `None`
    /// if `value` is not an `Obj::IterFn`.
    ///
    /// Callers must not store a new object handle through this reference, so
    /// this method skips the write barrier.
    #[inline]
    pub fn iter_src_mut(&mut self, value: VmValue) -> Option<&mut IterSrc> {
        match value {
            VmValue::Obj(handle) => match &mut self.slots[handle as usize] {
                Obj::IterFn(src) => Some(src),
                _ => None,
            },
            _ => None,
        }
    }

    /// Returns a mutable reference to the active iterator pins of map or set
    /// `value`, or `None` for other objects.
    ///
    /// Pins do not hold traced VM values, so this method skips the write
    /// barrier.
    pub fn pins_mut(&mut self, value: VmValue) -> Option<&mut Pins> {
        match value {
            VmValue::Obj(handle) => match &mut self.slots[handle as usize] {
                Obj::Map(map) => Some(&mut map.pins),
                Obj::Set(set) => Some(&mut set.pins),
                _ => None,
            },
            _ => None,
        }
    }

    /// Returns a mutable reference to `channel`, or `None` if it is not a
    /// channel.
    ///
    /// Pass `Some(value)` if adding `value` to the queue or blocked-sender list
    /// so the collector records a partial write barrier. Pass `None` otherwise.
    pub fn channel_mut(&mut self, channel: VmValue, stored: Option<VmValue>) -> Option<&mut ChannelObj> {
        let VmValue::Obj(handle) = channel else { return None };
        if let Some(value) = stored {
            if value.handle().is_some_and(|child| !self.is_marked(child)) && self.is_marked(handle) {
                self.remember_channel_for_partial_scan(handle);
            }
            if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
                self.mark_incremental(value);
            }
        }
        match &mut self.slots[handle as usize] {
            Obj::Channel(chan) => Some(chan),
            _ => None,
        }
    }

    /// Appends `value` to the queue of `channel`.
    ///
    /// Returns [`LimitKind::Memory`] without modifying `channel` if growing the
    /// queue exceeds the memory limit.
    pub fn channel_push(&mut self, channel: VmValue, value: VmValue) -> Result<(), LimitKind> {
        let VmValue::Obj(handle) = channel else { return Ok(()) };
        let Obj::Channel(chan) = &self.slots[handle as usize] else { return Ok(()) };
        let (len, cap) = (chan.len(), chan.queue().capacity());
        if len == cap {
            let new_cap = self.checked_grow_capacity(cap, len + 1, VALUE_SIZE, 0)?;
            let Obj::Channel(chan) = &mut self.slots[handle as usize] else { unreachable!() };
            chan.reserve_exact(new_cap - len);
            self.object_bytes += (chan.queue().capacity() - cap) * VALUE_SIZE;
        }
        if let Some(chan) = self.channel_mut(channel, Some(value)) {
            chan.push(value);
        }
        Ok(())
    }

    /// Returns the fields of a heap tuple, struct, variant, or closure. Use
    /// [`Heap::field`] to read fields of inline pairs and inline variants.
    pub fn fields(&self, value: VmValue) -> &[VmValue] {
        match self.get(value) {
            Obj::Tuple(fields) | Obj::Struct(_, fields) | Obj::Variant(_, fields) | Obj::Closure(_, fields) => fields,
            _ => &[],
        }
    }

    /// Returns field `index` of a tuple, struct, closure, or enum variant,
    /// including inline representations.
    #[inline]
    pub fn field(&self, value: VmValue, index: usize) -> Option<VmValue> {
        match value {
            VmValue::Obj(_) => self.fields(value).get(index).copied(),
            VmValue::Pair(a, b) => [VmValue::Int(a as i64), VmValue::Int(b)].get(index).copied(),
            _ => match value.as_inline_variant() {
                Some((_, payload)) if index == 0 => Some(payload),
                _ => None,
            },
        }
    }

    /// Returns a representation-independent view of enum variant `value`, or
    /// `None` for other values.
    #[inline]
    pub fn variant(&self, value: VmValue) -> Option<VariantRef<'_>> {
        match value {
            VmValue::Tag(tag) => Some(VariantRef::Inline(tag, None)),
            VmValue::Obj(_) => match self.get(value) {
                Obj::Variant(id, fields) => Some(VariantRef::Heap(*id, fields)),
                _ => None,
            },
            _ => value.as_inline_variant().map(|(id, payload)| VariantRef::Inline(id, Some(payload))),
        }
    }

    /// Returns the runtime variant ID of `value`, or `u32::MAX` if `value` is
    /// not a variant.
    #[inline]
    pub fn variant_id(&self, value: VmValue) -> u32 {
        match value {
            VmValue::Tag(tag) => tag,
            VmValue::Obj(_) => match self.get(value) {
                Obj::Variant(id, _) => *id,
                _ => u32::MAX,
            },
            _ => match value.as_inline_variant() {
                Some((id, _)) => id,
                None => u32::MAX,
            },
        }
    }

    /// Returns the first payload of `value` if it matches variant `id`, or
    /// `VmValue::Uninit` otherwise.
    // Return `VmValue` instead of `Option<VmValue>` to avoid an unaligned
    // piecewise copy that stalls store-to-load forwarding.
    #[inline]
    pub fn variant_payload_if(&self, value: &VmValue, id: u32) -> VmValue {
        match *value {
            VmValue::VUnit(x) if x == id => VmValue::Unit,
            VmValue::VBool(x, b) if x == id => VmValue::Bool(b),
            VmValue::VInt(x, i) if x == id => VmValue::Int(i),
            VmValue::VFloat(x, f) if x == id => VmValue::Float(f),
            VmValue::VDur(x, d) if x == id => VmValue::Dur(d),
            VmValue::VTag(x, t) if x == id => VmValue::Tag(t),
            VmValue::VObj(x, h) if x == id => VmValue::Obj(h),
            VmValue::Obj(handle) => match &self.slots[handle as usize] {
                Obj::Variant(vid, fields) if *vid == id => match **fields {
                    [payload, ..] => payload,
                    [] => VmValue::Uninit,
                },
                _ => VmValue::Uninit,
            },
            _ => VmValue::Uninit,
        }
    }

    pub fn native<T: 'static>(&self, value: VmValue) -> Option<&T> {
        match self.get(value) {
            Obj::Native(native) => (&**native as &dyn std::any::Any).downcast_ref::<T>(),
            _ => None,
        }
    }

    pub fn native_value<T: ValueType>(&self, value: VmValue) -> Option<T> {
        value_of(self.get(value))
    }

    pub fn native_mut<T: 'static>(&mut self, value: VmValue) -> Option<&mut T> {
        // Native objects do not contain VM values, so a write barrier is unnecessary.
        let VmValue::Obj(handle) = value else { return None };
        match &mut self.slots[handle as usize] {
            Obj::Native(native) => (&mut **native as &mut dyn std::any::Any).downcast_mut::<T>(),
            _ => None,
        }
    }

    // ------------------------------------------------------------------
    // Collection and scheduling

    /// Checks whether the heap needs a collection at the next safe point.
    #[inline]
    pub fn needs_gc(&self) -> bool {
        self.object_bytes > self.next_gc_bytes
    }

    /// Checks whether the heap needs a major collection now.
    pub fn needs_major_gc(&self) -> bool {
        self.cycle_phase == IncrementalGcPhase::Idle && self.object_bytes > self.next_major_gc_bytes
    }

    /// Checks whether the heap needs a minor collection now.
    pub fn needs_minor_gc(&mut self) -> bool {
        if self.gc_stress {
            self.stress_tick += 1;
            return self.stress_tick.is_multiple_of(3);
        }
        self.object_bytes > self.next_minor_gc_bytes
    }

    /// Reclaims objects unreachable from `roots` and drops unreachable native
    /// resources.
    ///
    /// Runs a full-heap major collection if `major` is `true`, aborting any
    /// active incremental cycle. Otherwise runs a minor collection over `young`.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = VmValue>, major: bool) {
        if major {
            self.clear_spares();
            self.abort_incremental_gc();
            self.mark_bits.clear();
        }
        for root in roots {
            mark(&mut self.mark_bits, &mut self.worklist, root);
        }
        let Heap {
            slots,
            mark_bits,
            worklist,
            scan_chunks,
            remembered,
            remembered_bits,
            partial_barrier_bits,
            pushed_tail_bits,
            dirty_containers,
            ..
        } = self;
        for &handle in remembered.iter() {
            let partial = partial_barrier_bits.contains(handle);
            remembered_bits.remove(handle);
            partial_barrier_bits.remove(handle);
            pushed_tail_bits.remove(handle);
            if major {
                continue;
            }
            let mut mark = |value: VmValue| mark(mark_bits, worklist, value);
            match (partial, &slots[handle as usize]) {
                (true, Obj::Channel(chan)) => {
                    let queue = chan.queue();
                    queue.range(queue.len() - chan.pushed.min(queue.len())..).for_each(|&value| mark(value));
                    chan.senders.iter().for_each(|&(_, _, value)| mark(value));
                }
                (true, obj @ (Obj::List(_) | Obj::Map(_) | Obj::Set(_))) => {
                    let dirty = dirty_containers.remove(&handle).unwrap_or_default();
                    for &index in &dirty.indices {
                        match obj {
                            Obj::List(list) => list.get(index as usize).into_iter().for_each(|&value| mark(value)),
                            Obj::Map(map) => map.entries.get(index as usize).into_iter().flatten().for_each(|&(key, value)| {
                                mark(key);
                                mark(value);
                            }),
                            Obj::Set(set) => set.entries.get(index as usize).into_iter().flatten().for_each(|&value| mark(value)),
                            _ => {}
                        }
                    }
                    if dirty.tail_start != UNSET_TAIL_START {
                        scan_chunks.push((handle, dirty.tail_start as usize));
                    }
                }
                _ => scan_chunks.push((handle, 0)),
            }
            // Drain `worklist` and `scan_chunks` after each remembered container so
            // `scan_chunks` holds at most a single root container at a time.
            trace(slots, mark_bits, worklist, scan_chunks);
        }
        dirty_containers.clear();
        remembered.clear();
        self.trace();
        if major {
            self.sweep_all();
            self.young = Vec::new();
            self.remembered = Vec::new();
            self.dirty_containers = FxHashMap::default();
            self.worklist = Vec::new();
            self.scan_chunks = Vec::new();
            self.gray_worklist = Vec::new();
        } else {
            let young = std::mem::take(&mut self.young);
            let mut freed = 0;
            for &handle in &young {
                if !self.is_marked(handle) {
                    freed += self.free_slot(handle);
                }
            }
            self.young = young;
            self.object_bytes = self.object_bytes.saturating_sub(freed);
            if matches!(self.cycle_phase, IncrementalGcPhase::Marking) {
                let slots = &self.slots;
                self.gray_worklist.retain(|&(handle, _)| !matches!(slots[handle as usize], Obj::Free(_)));
            }
        }
        self.young.clear();
        self.update_gc_thresholds(major);
        self.update_table_bytes();
    }

    /// Returns `true` if an incremental major collection is active.
    pub fn is_incremental_gc_active(&self) -> bool {
        self.cycle_phase != IncrementalGcPhase::Idle
    }

    /// Returns `true` if the next major collection should run incrementally.
    pub fn should_start_incremental_gc(&self) -> bool {
        self.incremental
            && if self.gc_stress { self.stats.collections.is_multiple_of(16) } else { self.object_bytes >= MIN_INCREMENTAL_BYTES }
    }

    /// Starts an incremental major collection and marks `roots`.
    pub fn start_incremental_gc(&mut self, roots: impl IntoIterator<Item = VmValue>) {
        self.clear_spares();
        self.cycle_mark_bits.clear();
        self.gray_bits.clear();
        self.gray_worklist.clear();
        self.cycle_phase = IncrementalGcPhase::Marking;
        for root in roots {
            self.mark_incremental(root);
        }
        self.cycle_start_bytes = self.object_bytes;
        self.bytes_at_last_step = self.object_bytes;
        self.update_next_gc_bytes();
        self.update_table_bytes();
    }

    /// Runs a single step of the active incremental collection.
    ///
    /// Returns `true` if marking is complete and the caller must invoke
    /// [`Heap::finish_incremental_marking`].
    pub fn step_incremental_gc(&mut self) -> bool {
        let allocated = self.object_bytes.saturating_sub(self.bytes_at_last_step).max(INCREMENTAL_STEP_BYTES);
        self.bytes_at_last_step = self.object_bytes;
        let done = match self.cycle_phase {
            IncrementalGcPhase::Marking => {
                let budget =
                    if self.gc_stress { (self.live_objects / 8).max(64) } else { (allocated / 16 * MARK_PACE).max(MIN_MARK_STEP_WORK) };
                self.step_marking(budget);
                self.gray_worklist.is_empty()
            }
            IncrementalGcPhase::Sweeping => {
                let slots = self.slots.len();
                let budget = if self.gc_stress {
                    (slots / 8).max(64)
                } else {
                    let share = allocated as u128 * MARK_PACE as u128 * slots as u128 / self.cycle_start_bytes.max(1) as u128;
                    let share = usize::try_from(share).unwrap_or(usize::MAX);
                    share.max(MIN_SWEEP_STEP_SLOTS)
                };
                self.step_sweeping(budget);
                false
            }
            IncrementalGcPhase::Idle => false,
        };
        self.update_table_bytes();
        done
    }

    /// Finishes incremental marking by rescanning `roots` and draining
    /// `gray_worklist`, then enters the sweeping phase.
    pub fn finish_incremental_marking(&mut self, roots: impl IntoIterator<Item = VmValue>) {
        for root in roots {
            self.mark_incremental(root);
        }
        self.step_marking(usize::MAX);
        self.mark_bits.copy_from(&self.cycle_mark_bits);
        for &handle in &self.remembered {
            self.remembered_bits.remove(handle);
            self.partial_barrier_bits.remove(handle);
            self.pushed_tail_bits.remove(handle);
        }
        self.remembered.clear();
        self.dirty_containers.clear();
        self.young.clear();
        self.cycle_phase = IncrementalGcPhase::Sweeping;
        self.sweep_cursor = 0;
        self.update_next_gc_bytes();
        self.update_table_bytes();
    }

    // ------------------------------------------------------------------
    // Internal write barriers

    /// Records a full write barrier for `handle` if it refers to an old object.
    #[inline]
    fn record_full_barrier(&mut self, handle: u32) {
        // Keep the fast path inline so only unremembered old objects call the cold helper.
        if self.mark_bits.contains(handle) && (!self.remembered_bits.contains(handle) || self.partial_barrier_bits.contains(handle)) {
            self.remember_for_full_scan(handle);
        }
    }

    /// Marks old object `handle` for a full scan during the next minor collection.
    #[cold]
    #[inline(never)]
    fn remember_for_full_scan(&mut self, handle: u32) {
        if !self.remembered_bits.contains(handle) {
            self.remembered_bits.insert(handle);
            self.remembered.push(handle);
        } else {
            self.switch_to_full_scan(handle);
        }
    }

    /// Switches a partially remembered container to a full scan.
    #[cold]
    fn switch_to_full_scan(&mut self, handle: u32) {
        self.partial_barrier_bits.remove(handle);
        self.pushed_tail_bits.remove(handle);
        self.dirty_containers.remove(&handle);
    }

    /// Records a partial write barrier for storing a young value in container
    /// `handle` at `index`.
    ///
    /// Set `is_push` to `true` if the write appends at the current container
    /// length.
    #[inline]
    fn record_partial_container_barrier(&mut self, handle: u32, index: usize, is_push: bool) {
        if self.mark_bits.contains(handle) {
            self.remember_for_partial_scan(handle, index, is_push);
        }
    }

    #[cold]
    #[inline(never)]
    fn remember_for_partial_scan(&mut self, handle: u32, index: usize, is_push: bool) {
        if !self.remembered_bits.contains(handle) {
            self.remembered_bits.insert(handle);
            self.remembered.push(handle);
            if self.entries_len(handle) < MIN_PARTIAL_BARRIER_LEN {
                return;
            }
            self.partial_barrier_bits.insert(handle);
            self.dirty_containers.insert(handle, DirtyContainer { tail_start: UNSET_TAIL_START, indices: Vec::new() });
        } else if !self.partial_barrier_bits.contains(handle) || (is_push && self.pushed_tail_bits.contains(handle)) {
            // Fully remembered, or the pushed tail is already covered.
            return;
        }
        self.record_dirty_index(handle, index, is_push);
    }

    #[cold]
    fn record_dirty_index(&mut self, handle: u32, index: usize, is_push: bool) {
        let index = index.min(UNSET_TAIL_START as usize - 1) as u32;
        let len = self.entries_len(handle);
        let Some(dirty) = self.dirty_containers.get_mut(&handle) else { return };
        if is_push {
            dirty.tail_start = dirty.tail_start.min(index);
            self.pushed_tail_bits.insert(handle);
        } else if dirty.indices.len() < len / ELEMS_PER_DIRTY_INDEX {
            dirty.indices.push(index);
        } else {
            self.switch_to_full_scan(handle);
        }
    }

    fn entries_len(&self, handle: u32) -> usize {
        match &self.slots[handle as usize] {
            Obj::List(list) => list.len(),
            Obj::Map(map) => map.entries.len(),
            Obj::Set(set) => set.entries.len(),
            _ => 0,
        }
    }

    #[cold]
    #[inline(never)]
    fn remember_channel_for_partial_scan(&mut self, handle: u32) {
        if !self.remembered_bits.contains(handle) {
            self.remembered_bits.insert(handle);
            self.remembered.push(handle);
            self.partial_barrier_bits.insert(handle);
            if let Obj::Channel(chan) = &mut self.slots[handle as usize] {
                chan.pushed = 0;
            }
        }
    }

    // ------------------------------------------------------------------
    // Internal collection helpers

    #[inline]
    fn is_marked(&self, handle: u32) -> bool {
        self.mark_bits.contains(handle)
    }

    fn trace(&mut self) {
        trace(&self.slots, &mut self.mark_bits, &mut self.worklist, &mut self.scan_chunks);
    }

    /// Sweeps the full arena in reverse order to build an ascending free list
    /// and truncate trailing free slots.
    fn sweep_all(&mut self) {
        let mut live_bytes = 0;
        let mut live = 0;
        let mut end = self.slots.len();
        self.free_list_head = FREE_LIST_END;
        for index in (0..self.slots.len()).rev() {
            if self.is_marked(index as u32) {
                live_bytes += self.slots[index].size();
                live += 1;
                continue;
            }
            let obj = std::mem::replace(&mut self.slots[index], Obj::Free(self.free_list_head));
            if end == index + 1 {
                end = index;
            } else {
                self.free_list_head = index as u32;
            }
            self.recycle_box(obj);
        }
        self.object_bytes = live_bytes;
        self.live_objects = live;
        self.truncate_arena(end);
    }

    /// Truncates trailing free slots starting at `len`.
    ///
    /// Shrinks arena and bitmap allocations if total truncated slots since the
    /// last shrink reach [`MIN_SHRINK_SLOTS`].
    fn truncate_arena(&mut self, len: usize) {
        if len == self.slots.len() {
            return;
        }
        self.arena_peak_slots = self.arena_peak_slots.max(self.slots.len());
        let shrink = self.arena_peak_slots - len >= MIN_SHRINK_SLOTS;
        if shrink {
            self.arena_peak_slots = len;
        }
        self.slots.truncate(len);
        for bits in [
            &mut self.mark_bits,
            &mut self.remembered_bits,
            &mut self.partial_barrier_bits,
            &mut self.pushed_tail_bits,
            &mut self.cycle_mark_bits,
            &mut self.gray_bits,
        ] {
            bits.truncate(len);
            if shrink {
                bits.shrink_to_fit();
            }
        }
        if shrink {
            self.slots.shrink_to_fit();
        }
    }

    /// Frees slot `handle` and returns its accounted size in bytes.
    fn free_slot(&mut self, handle: u32) -> usize {
        let index = handle as usize;
        let size = self.slots[index].size();
        self.release_slot(handle);
        self.live_objects -= 1;
        self.cycle_mark_bits.remove(handle);
        self.gray_bits.remove(handle);
        size
    }

    fn release_slot(&mut self, handle: u32) {
        let obj = std::mem::replace(&mut self.slots[handle as usize], Obj::Free(self.free_list_head));
        self.free_list_head = handle;
        self.recycle_box(obj);
    }

    fn recycle_box(&mut self, obj: Obj) {
        if let Obj::Tuple(boxed) | Obj::Struct(_, boxed) | Obj::Variant(_, boxed) | Obj::Closure(_, boxed) = obj
            && let Some(cache) = boxed.len().checked_sub(1).and_then(|i| self.spare_boxes.get_mut(i))
            && cache.len() < MAX_SPARE_BOXES_PER_LEN
        {
            self.spare_bytes += size_of_val(&*boxed);
            cache.push(boxed);
        }
    }

    fn clear_spares(&mut self) {
        self.spare_boxes = Default::default();
        self.spare_chunks = Vec::new();
        self.spare_bytes = 0;
    }

    fn update_gc_thresholds(&mut self, major: bool) {
        self.young_resources = 0;
        if major {
            let live_bytes = self.object_bytes;
            self.next_major_gc_bytes = (live_bytes * 2).max(MIN_GC_BYTES);
            if let Some(limit) = self.limit {
                // Trigger major collections earlier as usage approaches `limit`.
                if self.next_major_gc_bytes > limit {
                    self.next_major_gc_bytes = limit.max(live_bytes + (limit - live_bytes.min(limit)) / 2);
                }
            }
        }
        // Scale the nursery to a quarter of `next_major_gc_bytes` so small heaps still run minor collections.
        let nursery = (self.next_major_gc_bytes / 4).clamp(MIN_NURSERY_BYTES, MAX_NURSERY_BYTES);
        self.next_minor_gc_bytes = self.object_bytes.saturating_add(nursery);
        self.update_next_gc_bytes();
    }

    fn update_next_gc_bytes(&mut self) {
        self.next_gc_bytes = if self.gc_stress {
            0
        } else if self.cycle_phase == IncrementalGcPhase::Idle {
            self.next_minor_gc_bytes.min(self.next_major_gc_bytes)
        } else {
            self.next_minor_gc_bytes.min(self.object_bytes.saturating_add(INCREMENTAL_STEP_BYTES))
        };
    }

    /// Schedules a collection after [`MAX_YOUNG_RESOURCES`] resource allocations
    /// so unclosed files and sockets are reclaimed promptly.
    ///
    /// Schedules a major collection if open resources reach half of the
    /// configured resource limit.
    #[cold]
    fn record_resource_allocation(&mut self) {
        self.young_resources += 1;
        if self.young_resources >= MAX_YOUNG_RESOURCES {
            self.next_gc_bytes = 0;
            if self.io_memory.open_resources() >= self.io_memory.max_resources() / 2 {
                self.next_major_gc_bytes = 0;
            }
        }
    }

    // ------------------------------------------------------------------
    // Internal incremental collection helpers

    /// Aborts any active incremental major collection.
    fn abort_incremental_gc(&mut self) {
        self.cycle_phase = IncrementalGcPhase::Idle;
        self.gray_worklist.clear();
    }

    /// Marks `value` in `cycle_mark_bits` and queues it on `gray_worklist` if unmarked.
    #[inline]
    fn mark_incremental(&mut self, value: VmValue) {
        if let Some(handle) = value.handle()
            && !self.cycle_mark_bits.contains(handle)
        {
            self.cycle_mark_bits.insert(handle);
            self.gray_bits.insert(handle);
            self.gray_worklist.push((handle, 0));
        }
    }

    /// Requeues a marked object on `gray_worklist` after a mutation during
    /// incremental marking.
    #[cold]
    #[inline(never)]
    fn requeue_for_incremental_scan(&mut self, handle: u32) {
        if self.cycle_mark_bits.contains(handle) && !self.gray_bits.contains(handle) {
            self.gray_bits.insert(handle);
            self.gray_worklist.push((handle, 0));
        }
    }

    /// Scans queued gray objects until consuming `budget` child-value units.
    fn step_marking(&mut self, mut budget: usize) {
        let mut children = Vec::new();
        while budget > 0 {
            let Some((handle, start)) = self.gray_worklist.pop() else { break };
            if start == 0 {
                self.gray_bits.remove(handle);
            }
            children.clear();
            let next = visit_children_chunk(&self.slots[handle as usize], start, |value| children.push(value));
            budget = budget.saturating_sub(children.len() + 1);
            if let Some(next_index) = next {
                self.gray_worklist.push((handle, next_index));
            }
            for &child in &children {
                self.mark_incremental(child);
            }
        }
    }

    /// Sweeps up to `budget` slots and finishes the cycle upon reaching the end
    /// of the arena.
    fn step_sweeping(&mut self, budget: usize) {
        let end = self.sweep_cursor.saturating_add(budget).min(self.slots.len());
        let mut freed = 0;
        for index in self.sweep_cursor..end {
            if !matches!(self.slots[index], Obj::Free(_)) && !self.cycle_mark_bits.contains(index as u32) {
                freed += self.free_slot(index as u32);
            }
        }
        self.object_bytes = self.object_bytes.saturating_sub(freed);
        self.sweep_cursor = end;
        if end == self.slots.len() {
            self.cycle_phase = IncrementalGcPhase::Idle;
            self.stats.collections += 1;
            self.stats.major_collections += 1;
            self.update_gc_thresholds(true);
        }
    }

    // ------------------------------------------------------------------
    // Internal accounting and slow paths

    /// Returns byte capacity of unused arena slots and collector tables.
    #[inline]
    fn overhead_bytes(&self) -> usize {
        self.arena_peak_slots.max(self.slots.len()).saturating_sub(self.live_objects) * OBJ_SLOT_BYTES
            + self.young.capacity() * std::mem::size_of::<u32>()
            + self.table_bytes
    }

    #[cold]
    #[inline(never)]
    fn update_table_bytes(&mut self) {
        let bitmaps = [
            &self.mark_bits,
            &self.remembered_bits,
            &self.partial_barrier_bits,
            &self.pushed_tail_bits,
            &self.cycle_mark_bits,
            &self.gray_bits,
        ];
        let bitmap_bytes: usize = bitmaps.iter().map(|bits| bits.capacity_bytes()).sum();
        // `HashMap` allocates 8 buckets and 8 control bytes for every 7 entries of capacity.
        let dirty = self.dirty_containers.capacity() * (std::mem::size_of::<(u32, DirtyContainer)>() + 1) * 8 / 7;
        self.table_bytes = bitmap_bytes
            + self.remembered.capacity() * std::mem::size_of::<u32>()
            + dirty
            + self.gray_worklist.capacity() * std::mem::size_of::<(u32, usize)>()
            + self.worklist.capacity() * std::mem::size_of::<u32>()
            + self.scan_chunks.capacity() * std::mem::size_of::<(u32, usize)>();
    }

    #[cold]
    #[inline(never)]
    fn freeze_str_slow(&mut self, handle: u32) {
        if let Obj::StrBuf(buf) = &mut self.slots[handle as usize] {
            let text = std::mem::take(buf);
            let cap = text.capacity();
            let frozen = CompactString::from_string_buffer(text);
            let kept = if frozen.is_heap_allocated() { frozen.capacity() } else { 0 };
            self.object_bytes = self.object_bytes.saturating_sub(cap - kept);
            self.slots[handle as usize] = Obj::Str(frozen);
        }
    }
}

/// Buffers new objects built inside [`Heap::staged`] while holding read access
/// to the heap.
///
/// Staged objects reserve handles from the heap free list first, then from
/// slots past the end of the arena. This order matches [`Heap::alloc`].
pub struct Staging<'h> {
    heap: &'h Heap,
    /// Next free slot index, or [`FREE_LIST_END`] if the free list is exhausted.
    next_free_slot: u32,
    /// Next slot index past the end of the arena.
    next_arena_slot: usize,
    /// Staged handles and objects grouped in chunks up to [`STAGE_CHUNK_LEN`].
    ///
    /// Chunked storage avoids large vector reallocations that leave freed
    /// buffers in the allocator.
    chunks: Vec<Vec<(u32, Obj)>>,
    /// Total entry capacity across `chunks`.
    total_capacity: usize,
    /// Reusable empty chunks of [`STAGE_CHUNK_LEN`] entries.
    spare_chunks: Vec<Vec<(u32, Obj)>>,
    /// Accounted object bytes for staged entries.
    staged_bytes: usize,
}

impl Staging<'_> {
    /// Stages `obj` and returns the handle value it will receive on commit.
    pub fn alloc(&mut self, obj: Obj) -> VmValue {
        self.staged_bytes += obj.size();
        let handle = if self.next_free_slot != FREE_LIST_END {
            let handle = self.next_free_slot;
            self.next_free_slot = match self.heap.slots[handle as usize] {
                Obj::Free(next) => next,
                _ => unreachable!("corrupt free list"),
            };
            // `obj.size()` includes `OBJ_SLOT_BYTES`, which `Heap::overhead_bytes` already counts for free slots.
            self.staged_bytes -= OBJ_SLOT_BYTES;
            handle
        } else {
            self.next_arena_slot += 1;
            (self.next_arena_slot - 1) as u32
        };
        match self.chunks.last_mut() {
            Some(chunk) if chunk.len() < chunk.capacity() => chunk.push((handle, obj)),
            _ => {
                let len = self.next_chunk_capacity();
                let mut chunk =
                    if len == STAGE_CHUNK_LEN { self.spare_chunks.pop() } else { None }.unwrap_or_else(|| Vec::with_capacity(len));
                self.total_capacity += chunk.capacity();
                chunk.push((handle, obj));
                self.chunks.push(chunk);
            }
        }
        VmValue::Obj(handle)
    }

    pub fn new_str(&mut self, text: impl Into<CompactString>) -> VmValue {
        self.alloc(Obj::Str(text.into()))
    }

    pub fn new_bytes(&mut self, bytes: impl Into<Box<[u8]>>) -> VmValue {
        self.alloc(Obj::Bytes(bytes.into()))
    }

    pub fn new_list(&mut self, items: Vec<VmValue>) -> VmValue {
        self.alloc(Obj::List(items))
    }

    pub fn new_tuple(&mut self, items: Vec<VmValue>) -> VmValue {
        match items[..] {
            [a, b] if let Some(value) = VmValue::tuple2(a, b) => value,
            _ => self.alloc(Obj::Tuple(items.into_boxed_slice())),
        }
    }

    /// Stages a variant in its canonical representation, matching [`Heap::new_variant`].
    pub fn new_variant(&mut self, id: u32, fields: &[VmValue]) -> VmValue {
        match fields {
            [] => VmValue::Tag(id),
            [payload] => match VmValue::variant1(id, *payload) {
                Some(value) => value,
                None => self.alloc(Obj::Variant(id, Box::new([*payload]))),
            },
            _ => self.alloc(Obj::Variant(id, fields.into())),
        }
    }

    /// Checks if adding `extra` bytes would exceed the heap limit.
    pub fn would_exceed_memory_limit(&self, extra: usize) -> bool {
        self.heap.would_exceed_memory_limit(self.staged_bytes.saturating_add(self.buffer_bytes()).saturating_add(extra))
    }

    /// Returns a memory-limit error if adding `extra` bytes would exceed the heap limit.
    pub fn check_memory(&self, extra: usize) -> Result<(), crate::vm::Fail> {
        if self.would_exceed_memory_limit(extra) { Err(crate::vm::Fail::limit(LimitKind::Memory)) } else { Ok(()) }
    }

    fn next_chunk_capacity(&self) -> usize {
        self.chunks.last().map_or(4, |chunk| (chunk.capacity() * 2).min(STAGE_CHUNK_LEN))
    }

    fn buffer_bytes(&self) -> usize {
        let full = self.chunks.last().is_none_or(|chunk| chunk.len() == chunk.capacity());
        let next = if full { self.next_chunk_capacity() } else { 0 };
        let entries = self.total_capacity + next + self.spare_chunks.len() * STAGE_CHUNK_LEN;
        let chunks = self.chunks.capacity().max(self.chunks.len() + 1);
        entries * std::mem::size_of::<(u32, Obj)>() + chunks * std::mem::size_of::<Vec<(u32, Obj)>>()
    }
}

/// Garbage collector statistics for a VM run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Total number of completed collections.
    pub collections: u64,
    /// Total time spent in collections, incremental steps, and root scans.
    pub total_pause: Duration,
    /// Longest single collection or incremental step.
    pub max_pause: Duration,
    /// Number of full-heap major collections.
    pub major_collections: u64,
    /// Duration of the longest non-incremental major collection.
    pub max_major_pause: Duration,
    /// Duration of the longest minor collection.
    pub max_minor_pause: Duration,
    /// Number of incremental major collection steps.
    pub incremental_steps: u64,
    /// Duration of the longest incremental major collection step.
    pub max_step_pause: Duration,
}

impl GcStats {
    /// Records a completed minor or non-incremental major collection.
    pub fn record(&mut self, pause: Duration, major: bool) {
        self.collections += 1;
        self.total_pause += pause;
        self.max_pause = self.max_pause.max(pause);
        if major {
            self.major_collections += 1;
            self.max_major_pause = self.max_major_pause.max(pause);
        } else {
            self.max_minor_pause = self.max_minor_pause.max(pause);
        }
    }

    /// Records an incremental major collection step.
    pub fn record_step(&mut self, pause: Duration) {
        self.incremental_steps += 1;
        self.total_pause += pause;
        self.max_pause = self.max_pause.max(pause);
        self.max_step_pause = self.max_step_pause.max(pause);
    }
}

// ------------------------------------------------------------------
// Internal collector state and tracing helpers

/// Current phase of an incremental major collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncrementalGcPhase {
    Idle,
    Marking,
    Sweeping,
}

/// Modified indices and pushed tail of a partially remembered container.
///
/// Minor collections scan only these entries instead of the full list, map, or
/// set.
#[derive(Default)]
struct DirtyContainer {
    /// First index of the appended tail, or [`UNSET_TAIL_START`] if the tail is empty.
    tail_start: u32,
    /// Individually modified element indices.
    indices: Vec<u32>,
}

/// Dense bitset backed by 64-bit words, indexed by arena slot handles.
#[derive(Default)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    #[inline(always)]
    fn contains(&self, handle: u32) -> bool {
        self.words[(handle / 64) as usize] & (1u64 << (handle % 64)) != 0
    }

    #[inline(always)]
    fn insert(&mut self, handle: u32) {
        self.words[(handle / 64) as usize] |= 1u64 << (handle % 64);
    }

    #[inline(always)]
    fn remove(&mut self, handle: u32) {
        self.words[(handle / 64) as usize] &= !(1u64 << (handle % 64));
    }

    fn clear(&mut self) {
        self.words.fill(0);
    }

    fn copy_from(&mut self, src: &BitSet) {
        self.words.copy_from_slice(&src.words);
    }

    #[inline(always)]
    fn slot_capacity(&self) -> usize {
        self.words.len() * 64
    }

    fn push_word(&mut self) {
        self.words.push(0);
    }

    fn truncate(&mut self, slot_len: usize) {
        self.words.truncate(slot_len.div_ceil(64));
        if let Some(last) = self.words.last_mut()
            && !slot_len.is_multiple_of(64)
        {
            *last &= (1u64 << (slot_len % 64)) - 1;
        }
    }

    fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
    }

    fn capacity_bytes(&self) -> usize {
        self.words.capacity() * std::mem::size_of::<u64>()
    }
}

/// Marks all objects reachable from `worklist` and `scan_chunks`.
///
/// Drains `worklist` before advancing `scan_chunks` to bound `worklist` to a
/// single container chunk at a time.
fn trace(slots: &[Obj], mark_bits: &mut BitSet, worklist: &mut Vec<u32>, scan_chunks: &mut Vec<(u32, usize)>) {
    loop {
        let (handle, start) = match worklist.pop() {
            Some(handle) => (handle, 0),
            None => match scan_chunks.pop() {
                Some(chunk) => chunk,
                None => break,
            },
        };
        if let Some(next) = visit_children_chunk(&slots[handle as usize], start, |value| mark(mark_bits, worklist, value)) {
            scan_chunks.push((handle, next));
        }
    }
}

/// Invokes `f` on up to [`SCAN_CHUNK_LEN`] child values of `obj`, starting at
/// `start`.
///
/// Returns the next child index if more children remain, or `None` if all
/// children have been visited. Channel queues are scanned from back to front so
/// front pops between incremental chunks do not skip remaining elements.
#[inline]
fn visit_children_chunk(obj: &Obj, start: usize, mut f: impl FnMut(VmValue)) -> Option<usize> {
    let chunk = |len: usize| {
        let end = start.saturating_add(SCAN_CHUNK_LEN).min(len);
        (start.min(end)..end, (end < len).then_some(end))
    };
    match obj {
        Obj::List(list) => {
            let (range, next) = chunk(list.len());
            list[range].iter().for_each(|&value| f(value));
            next
        }
        Obj::Map(map) => {
            let (range, next) = chunk(map.entries.len());
            for &(key, value) in map.entries[range].iter().flatten() {
                f(key);
                f(value);
            }
            next
        }
        Obj::Set(set) => {
            let (range, next) = chunk(set.entries.len());
            set.entries[range].iter().flatten().for_each(|&value| f(value));
            next
        }
        Obj::Channel(chan) => {
            let queue = chan.queue();
            let (range, next) = chunk(queue.len());
            queue.range(queue.len() - range.end..queue.len() - range.start).for_each(|&value| f(value));
            if start == 0 {
                chan.senders.iter().for_each(|&(_, _, value)| f(value));
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

/// Marks `value` and pushes its handle onto `worklist` if unmarked.
#[inline]
fn mark(mark_bits: &mut BitSet, worklist: &mut Vec<u32>, value: VmValue) {
    if let Some(handle) = value.handle()
        && !mark_bits.contains(handle)
    {
        mark_bits.insert(handle);
        worklist.push(handle);
    }
}

// ------------------------------------------------------------------
// Internal constants

const OBJ_SLOT_BYTES: usize = std::mem::size_of::<Obj>();
const FREE_LIST_END: u32 = u32::MAX;
const UNSET_TAIL_START: u32 = u32::MAX;
/// Minimum truncated trailing slots required before shrinking arena and bitmap capacity.
const MIN_SHRINK_SLOTS: usize = 4096;

const MIN_GC_BYTES: usize = 4 << 20;
const MIN_NURSERY_BYTES: usize = 1 << 20;
/// Maximum bytes allocated between minor collections.
const MAX_NURSERY_BYTES: usize = 8 << 20;
/// Maximum resources allocated before triggering a collection.
const MAX_YOUNG_RESOURCES: usize = 64;
/// Minimum `object_bytes` required to run a major collection incrementally.
const MIN_INCREMENTAL_BYTES: usize = 8 << 20;
/// Allocation interval in bytes between incremental collection steps.
const INCREMENTAL_STEP_BYTES: usize = 256 << 10;
/// Marking work units per 16 allocated bytes during an incremental cycle.
const MARK_PACE: usize = 4;
const MIN_MARK_STEP_WORK: usize = 16 << 10;
const MIN_SWEEP_STEP_SLOTS: usize = 32 << 10;
/// Maximum container elements scanned per tracing chunk.
const SCAN_CHUNK_LEN: usize = 4096;

/// Minimum container length for partial write-barrier tracking.
///
/// Minor collections scan shorter containers in full because a
/// [`DirtyContainer`] entry costs more memory than scanning the elements.
const MIN_PARTIAL_BARRIER_LEN: usize = 128;

/// Ratio of container elements to tracked indices in [`DirtyContainer`].
///
/// If writes exceed this ratio, the collector switches the container to a full
/// scan. This cap keeps `dirty_containers` bounded by container size.
const ELEMS_PER_DIRTY_INDEX: usize = 8;

/// Maximum number of staged entries per chunk in [`Staging`].
const STAGE_CHUNK_LEN: usize = 4096;
const STAGE_CHUNK_BYTES: usize = STAGE_CHUNK_LEN * std::mem::size_of::<(u32, Obj)>();
/// Maximum slice length cached in `spare_boxes`.
const MAX_SPARE_BOX_LEN: usize = 4;
/// Maximum number of freed boxes cached per slice length.
const MAX_SPARE_BOXES_PER_LEN: usize = 1 << 14;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_objects_receive_reserved_handles() {
        let mut heap = Heap::new(None, usize::MAX);
        let a = heap.new_str("a");
        let b = heap.new_str("b");
        heap.collect([b], true);
        // Reuse the freed slot of `a` first, then allocate new slots at the arena end.
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
    fn staging_buffer_bytes_counted_until_major_collection() {
        let n = (STAGE_CHUNK_LEN - 4) + 3 * STAGE_CHUNK_LEN;
        let size = std::mem::size_of::<(u32, Obj)>();
        let mut heap = Heap::new(None, usize::MAX);
        let used = |heap: &Heap| heap.used_bytes() - heap.overhead_bytes();
        let before = used(&heap);
        let buffer = heap.staged(|_, st| {
            for i in 0..n {
                st.new_list(vec![VmValue::Int(i as i64)]);
            }
            assert_eq!(st.chunks.iter().map(Vec::len).sum::<usize>(), n);
            st.buffer_bytes()
        });
        let objs = (n + STAGE_CHUNK_LEN) * size;
        assert!(buffer >= objs && buffer < objs + 1024, "{buffer}");
        let lists = n * Obj::List(vec![VmValue::Int(0)]).size();
        assert_eq!(used(&heap) - before, lists + 3 * STAGE_CHUNK_BYTES);
        heap.collect((0..n).map(|i| VmValue::Obj(i as u32)), true);
        assert_eq!(used(&heap) - before, lists);
    }

    #[test]
    fn dirty_index_lists_stay_small() {
        let mut heap = Heap::new(None, usize::MAX);
        let lists: Vec<VmValue> = (0..100).map(|_| heap.new_list(vec![VmValue::Int(0)])).collect();
        let big = heap.new_list(vec![VmValue::Int(0); 800]);
        let mut roots = lists.clone();
        roots.push(big);
        heap.collect(roots.iter().copied(), false);
        let young = heap.new_str("young");
        for &l in lists.iter().chain([&big]) {
            for _ in 0..1000 {
                heap.list_mut_storing(l, 0, young).unwrap()[0] = young;
            }
        }
        let recorded: usize = heap.dirty_containers.values().map(|d| d.indices.len()).sum();
        assert!(recorded <= 200, "{recorded} indices recorded");
        heap.collect(roots.iter().copied(), false);
        assert_eq!(heap.str(heap.list(big)[0]), "young");
        assert_eq!(heap.str(heap.list(lists[7])[0]), "young");
    }

    fn barrier_tables(heap: &Heap) -> usize {
        heap.dirty_containers.capacity() * std::mem::size_of::<(u32, DirtyContainer)>()
            + heap.dirty_containers.values().map(|d| d.indices.capacity() * std::mem::size_of::<u32>()).sum::<usize>()
            + heap.remembered.capacity() * std::mem::size_of::<u32>()
    }

    #[test]
    fn write_barrier_tables_are_bounded_by_container_size() {
        let mut heap = Heap::new(None, usize::MAX);
        let small: Vec<VmValue> = (0..100_000).map(|_| heap.new_list(vec![VmValue::Int(0)])).collect();
        let big: Vec<VmValue> = (0..1000).map(|_| heap.new_list(vec![VmValue::Int(0); 1000])).collect();
        let roots: Vec<VmValue> = small.iter().chain(&big).copied().collect();
        heap.collect(roots.iter().copied(), true);
        let young = heap.new_str("a young string, long enough to be on the heap");
        for &l in &small {
            heap.list_mut_storing(l, 0, young).unwrap()[0] = young;
        }
        for &l in &big {
            for i in (0..1000).step_by(7) {
                heap.list_mut_storing(l, i, young).unwrap()[i] = young;
            }
        }
        let tables = barrier_tables(&heap);
        assert!(tables <= heap.object_bytes / 8, "{tables} bytes of tables for {} bytes of objects", heap.object_bytes);
        heap.collect(roots.iter().copied(), false);
        assert!(heap.list(small[5])[0] == young && heap.list(big[5])[994] == young);
        let work =
            heap.scan_chunks.capacity() * std::mem::size_of::<(u32, usize)>() + heap.worklist.capacity() * std::mem::size_of::<u32>();
        assert!(work <= SCAN_CHUNK_LEN * 64, "{work} bytes of work lists");
        heap.collect(roots.iter().copied(), true);
        assert_eq!(barrier_tables(&heap), 0);
    }

    #[test]
    fn minor_collection_accounts_for_retained_worklists() {
        let mut heap = Heap::new(None, usize::MAX);
        let roots: Vec<VmValue> = (0..200_000).map(|i| heap.new_str(format!("a young string, long enough, {i}"))).collect();
        heap.collect(roots.iter().copied(), false);
        let work =
            heap.worklist.capacity() * std::mem::size_of::<u32>() + heap.scan_chunks.capacity() * std::mem::size_of::<(u32, usize)>();
        assert!(work > 0);
        assert!(heap.table_bytes >= work, "{} bytes of tables counted, {work} bytes of work lists", heap.table_bytes);
    }

    fn new_channel(heap: &mut Heap) -> VmValue {
        heap.alloc(Obj::Channel(Box::new(ChannelObj::new(0))))
    }

    fn queued(heap: &Heap, ch: VmValue) -> Vec<String> {
        let Obj::Channel(c) = heap.get(ch) else { panic!("not a channel") };
        c.queue().iter().map(|&v| heap.str(v).to_string()).collect()
    }

    #[test]
    fn young_values_in_old_channels_retained_by_minor_collection() {
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
            heap.channel_push(ch, VmValue::Int(i)).unwrap();
        }
        let Obj::Channel(c) = heap.get(ch) else { unreachable!() };
        assert_eq!(c.pushed, 101, "only the values pushed since the channel was remembered are scanned");
        // Popping from the front keeps the young value within the tracked tail.
        for _ in 0..500 {
            heap.channel_mut(ch, None).unwrap().take();
        }
        heap.collect([ch], false);
        let Obj::Channel(c) = heap.get(ch) else { unreachable!() };
        assert_eq!(heap.str(c.queue()[500]), "a young value, then ints");
    }

    #[test]
    fn blocked_sender_values_retained_by_minor_collection() {
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
    fn incremental_marking_retains_channel_values_after_takes() {
        let mut heap = Heap::new(None, usize::MAX);
        let ch = new_channel(&mut heap);
        let n = 3 * SCAN_CHUNK_LEN;
        for i in 0..n {
            let v = heap.new_str(format!("value number {i}"));
            heap.channel_push(ch, v).unwrap();
        }
        heap.collect([ch], true);
        heap.start_incremental_gc([ch]);
        heap.step_marking(SCAN_CHUNK_LEN + 2);
        // Take values from the front while the queue is partly scanned.
        for _ in 0..SCAN_CHUNK_LEN {
            heap.channel_mut(ch, None).unwrap().take();
        }
        for i in 0..10 {
            let v = heap.new_str(format!("pushed during marking {i}"));
            heap.channel_push(ch, v).unwrap();
        }
        heap.finish_incremental_marking([ch]);
        while heap.is_incremental_gc_active() {
            heap.step_incremental_gc();
        }
        let q = queued(&heap, ch);
        assert_eq!(q.len(), n - SCAN_CHUNK_LEN + 10);
        assert_eq!(q[0], format!("value number {SCAN_CHUNK_LEN}"));
        assert_eq!(q[n - SCAN_CHUNK_LEN - 1], format!("value number {}", n - 1));
        assert_eq!(q[n - SCAN_CHUNK_LEN + 9], "pushed during marking 9");
    }

    #[test]
    fn tracing_scans_large_containers_in_chunks() {
        let mut heap = Heap::new(None, usize::MAX);
        let items: Vec<VmValue> = (0..10 * SCAN_CHUNK_LEN).map(|i| heap.new_str(format!("an item, {i}"))).collect();
        let list = heap.new_list(items.clone());
        let ch = new_channel(&mut heap);
        for &v in &items {
            heap.channel_push(ch, v).unwrap();
        }
        heap.collect([list, ch], true);
        assert!(heap.worklist.capacity() <= 4 * SCAN_CHUNK_LEN, "worklist of {}", heap.worklist.capacity());
        assert_eq!(heap.str(heap.list(list)[10 * SCAN_CHUNK_LEN - 1]), format!("an item, {}", 10 * SCAN_CHUNK_LEN - 1));
        // A minor collection also scans a fully remembered old container in chunks.
        let young = heap.new_str("young");
        heap.list_mut(list)[0] = young;
        heap.worklist = Vec::new();
        heap.collect([list], false);
        assert!(heap.worklist.capacity() <= 4 * SCAN_CHUNK_LEN, "worklist of {}", heap.worklist.capacity());
        assert_eq!(heap.str(heap.list(list)[0]), "young");
    }

    #[test]
    fn containers_respect_memory_limit_on_growth() {
        let limit = 1 << 20;
        let mut heap = Heap::new(Some(limit), usize::MAX);
        let list = heap.new_list(Vec::new());
        let mut n = 0;
        while heap.list_push(list, VmValue::Int(n)).is_ok() {
            n += 1;
        }
        let cap = heap.list(list).len();
        assert!(heap.used_bytes() <= limit, "{} bytes used", heap.used_bytes());
        // Near the limit, the allocator grows the list by less than double.
        assert!(cap * VALUE_SIZE > limit * 3 / 4, "only {cap} elements fit");
        let Obj::List(l) = heap.get(list) else { unreachable!() };
        assert_eq!(l.len(), l.capacity(), "failed push must leave list unchanged");
        assert_eq!(heap.list_reserve(list, 1), Err(LimitKind::Memory));
        let ch = new_channel(&mut heap);
        while heap.channel_push(ch, VmValue::Int(0)).is_ok() {}
        assert!(heap.used_bytes() <= limit, "{} bytes used", heap.used_bytes());
        heap.collect([ch], true);
        heap.channel_push(ch, VmValue::Int(0)).unwrap();
    }

    fn free_list(heap: &Heap) -> Vec<u32> {
        let mut out = Vec::new();
        let mut h = heap.free_list_head;
        while h != FREE_LIST_END {
            out.push(h);
            let Obj::Free(next) = heap.slots[h as usize] else { panic!("slot {h} on the free list is in use") };
            h = next;
        }
        out
    }

    #[test]
    fn free_slots_are_counted_until_reused() {
        let mut heap = Heap::new(None, usize::MAX);
        let keep = heap.new_str("keep");
        let garbage: Vec<VmValue> = (0..1000).map(|i| heap.new_list(vec![VmValue::Int(i)])).collect();
        let top = heap.new_str("top");
        heap.collect([keep, top], true);
        assert_eq!(free_list(&heap), (1..=1000).collect::<Vec<u32>>());
        let used = heap.used_bytes();
        assert!(used >= heap.object_bytes + 1000 * OBJ_SLOT_BYTES, "{used} bytes used");
        let young = |heap: &Heap| heap.young.capacity() * std::mem::size_of::<u32>();
        let ranges: Vec<VmValue> = (0..10).map(|i| heap.alloc(Obj::Range(i, i, false))).collect();
        assert_eq!(ranges, garbage[..10]);
        assert_eq!(heap.used_bytes() - young(&heap), used);
        let staged = heap.staged(|_, st| st.alloc(Obj::Range(0, 0, false)));
        assert_eq!(staged, garbage[10]);
        assert_eq!(heap.used_bytes() - young(&heap), used);
    }

    #[test]
    fn major_collection_truncates_trailing_free_slots() {
        let mut heap = Heap::new(None, usize::MAX);
        let keep = heap.new_str("keep");
        let n = 4 * MIN_SHRINK_SLOTS;
        let list = heap.new_list((0..n as i64).map(VmValue::Int).collect());
        for i in 0..n {
            let item = heap.new_str(format!("an item, {i}"));
            heap.list_mut(list)[i] = item;
        }
        heap.collect([keep, list], true);
        let full = heap.used_bytes();
        heap.collect([keep], true);
        assert_eq!(heap.slots.len(), 1);
        assert!(heap.slots.capacity() < MIN_SHRINK_SLOTS, "{} slots kept", heap.slots.capacity());
        assert!(full - heap.used_bytes() >= (n + 1) * OBJ_SLOT_BYTES, "{full} bytes, then {}", heap.used_bytes());
        assert_eq!(heap.str(keep), "keep");
        assert_eq!(free_list(&heap), Vec::<u32>::new());
        let young = heap.new_str("young");
        assert!(!heap.is_marked(young.handle().unwrap()));
        heap.collect([keep], false);
        assert_eq!(heap.slots.len(), 2);
        assert_eq!(free_list(&heap), vec![1]);
    }

    #[test]
    fn truncated_slots_are_counted_until_arena_shrinks() {
        let mut heap = Heap::new(None, usize::MAX);
        let n = 64 * MIN_SHRINK_SLOTS;
        let mut live: Vec<VmValue> = (0..n).map(|i| heap.new_list(vec![VmValue::Int(i as i64)])).collect();
        heap.collect(live.iter().copied(), true);
        while live.len() > MIN_SHRINK_SLOTS {
            live.truncate(live.len() - (MIN_SHRINK_SLOTS - 1));
            heap.collect(live.iter().copied(), true);
            let resident = heap.slots.capacity().min(n) - heap.live_objects;
            assert!(
                heap.overhead_bytes() >= resident * OBJ_SLOT_BYTES,
                "{} bytes counted, {resident} slots resident",
                heap.overhead_bytes()
            );
        }
        assert!(heap.slots.capacity() < 2 * MIN_SHRINK_SLOTS, "{} slots kept", heap.slots.capacity());
    }

    #[derive(Clone, Debug)]
    enum Op {
        Str(bool),
        List(Vec<usize>),
        Stage(usize),
        Drop(usize),
        Collect(bool),
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            8 => any::<bool>().prop_map(Op::Str),
            2 => prop::collection::vec(any::<usize>(), 0..4).prop_map(Op::List),
            1 => (0usize..40).prop_map(Op::Stage),
            4 => any::<usize>().prop_map(Op::Drop),
            1 => any::<bool>().prop_map(Op::Collect),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        #[test]
        fn heap_accounts_for_all_slots_and_preserves_roots(ops in proptest::collection::vec(op(), 1..200)) {
            let mut heap = Heap::new(None, usize::MAX);
            let mut roots: Vec<(VmValue, Result<String, Vec<VmValue>>)> = Vec::new();
            let mut n = 0;
            for op in ops {
                match op {
                    Op::Str(rooted) => {
                        n += 1;
                        let v = heap.new_str(format!("string {n} with some text"));
                        if rooted {
                            roots.push((v, Ok(format!("string {n} with some text"))));
                        }
                    }
                    Op::List(items) => {
                        let items: Vec<VmValue> = items.iter().filter(|_| !roots.is_empty()).map(|i| roots[i % roots.len()].0).collect();
                        let v = heap.new_list(items.clone());
                        roots.push((v, Err(items)));
                    }
                    Op::Stage(k) => {
                        let texts: Vec<String> = (0..k).map(|i| format!("staged {n}.{i}")).collect();
                        let vs = heap.staged(|_, st| texts.iter().map(|t| st.new_str(t.as_str())).collect::<Vec<_>>());
                        roots.extend(vs.into_iter().zip(texts).step_by(2).map(|(v, t)| (v, Ok(t))));
                        n += 1;
                    }
                    Op::Drop(i) if !roots.is_empty() => {
                        drop(roots.remove(i % roots.len()));
                    }
                    Op::Drop(_) => {}
                    Op::Collect(major) => {
                        let mut all: Vec<VmValue> = roots.iter().map(|r| r.0).collect();
                        for (_, c) in &roots {
                            if let Err(items) = c {
                                all.extend(items);
                            }
                        }
                        heap.collect(all, major);
                        let free = free_list(&heap);
                        if major {
                            proptest::prop_assert!(free.windows(2).all(|w| w[0] < w[1]), "free list out of order: {free:?}");
                            proptest::prop_assert!(heap.slots.last().is_none_or(|o| !matches!(o, Obj::Free(_))), "free slot at the end");
                            let sizes: usize = heap.slots.iter().filter(|o| !matches!(o, Obj::Free(_))).map(Obj::size).sum();
                            proptest::prop_assert_eq!(heap.object_bytes, sizes);
                        }
                    }
                }
                let free = free_list(&heap);
                let in_use = heap.slots.iter().filter(|o| !matches!(o, Obj::Free(_))).count();
                proptest::prop_assert_eq!(free.len() + in_use, heap.slots.len(), "every free slot is on the free list once");
                proptest::prop_assert_eq!(heap.live_objects, in_use);
                proptest::prop_assert!(heap.overhead_bytes() >= free.len() * OBJ_SLOT_BYTES);
                for (v, c) in &roots {
                    match c {
                        Ok(text) => proptest::prop_assert_eq!(heap.str(*v), text.as_str()),
                        Err(items) => proptest::prop_assert_eq!(heap.list(*v), items.as_slice()),
                    }
                }
            }
        }
    }

    #[derive(Clone, Debug)]
    enum BarrierOp {
        Store(usize, usize, bool),
        Push(usize),
        Pop(usize),
        Clear(usize),
        Insert(usize, usize),
        Remove(usize, usize),
        MapStore(usize, usize),
        MapPush(usize),
        MapRemove(usize, usize),
        Garbage(usize),
        Collect(bool),
        Cycle,
    }

    fn barrier_op() -> impl proptest::strategy::Strategy<Value = BarrierOp> {
        use proptest::prelude::*;
        let c = 0usize..9;
        prop_oneof![
            8 => (c.clone(), any::<usize>(), any::<bool>()).prop_map(|(c, i, s)| BarrierOp::Store(c, i, s)),
            4 => c.clone().prop_map(BarrierOp::Push),
            3 => c.clone().prop_map(BarrierOp::Pop),
            1 => c.clone().prop_map(BarrierOp::Clear),
            1 => (c.clone(), any::<usize>()).prop_map(|(c, i)| BarrierOp::Insert(c, i)),
            1 => (c.clone(), any::<usize>()).prop_map(|(c, i)| BarrierOp::Remove(c, i)),
            4 => (0usize..2, any::<usize>()).prop_map(|(m, s)| BarrierOp::MapStore(m, s)),
            2 => (0usize..2).prop_map(BarrierOp::MapPush),
            1 => (0usize..2, any::<usize>()).prop_map(|(m, s)| BarrierOp::MapRemove(m, s)),
            2 => (0usize..20).prop_map(BarrierOp::Garbage),
            3 => any::<bool>().prop_map(BarrierOp::Collect),
            3 => Just(BarrierOp::Cycle),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        #[test]
        fn write_barrier_preserves_young_values_in_old_containers(ops in proptest::collection::vec(barrier_op(), 1..300)) {
            let mut heap = Heap::new(None, usize::MAX);
            heap.gc_stress = true;
            let lens = [0, 5, MIN_PARTIAL_BARRIER_LEN - 1, MIN_PARTIAL_BARRIER_LEN, MIN_PARTIAL_BARRIER_LEN + 1, 200, 300, 1000, SCAN_CHUNK_LEN + 10];
            let lists: Vec<VmValue> = lens.iter().map(|&n| heap.new_list(vec![VmValue::Int(0); n])).collect();
            let mut model: Vec<Vec<Option<String>>> = lens.iter().map(|&n| vec![None; n]).collect();
            let map_lens = [10, 300];
            let maps: Vec<VmValue> = map_lens
                .iter()
                .map(|&n| {
                    let entries = (0..n).map(|i| Some((VmValue::Int(i as i64), VmValue::Int(0)))).collect();
                    heap.alloc(Obj::Map(Box::new(MapObj { entries, live: n, ..MapObj::default() })))
                })
                .collect();
            let mut map_model: Vec<Vec<Option<String>>> = map_lens.iter().map(|&n| vec![None; n]).collect();
            let roots: Vec<VmValue> = lists.iter().chain(&maps).copied().collect();
            heap.collect(roots.iter().copied(), true);
            let mut n = 0;
            let mut text = || {
                n += 1;
                format!("young string number {n}, long enough to be on the heap")
            };
            for op in ops {
                match op {
                    BarrierOp::Store(c, i, s) => {
                        if model[c].is_empty() {
                            continue;
                        }
                        let i = i % model[c].len();
                        let (v, t) = if s {
                            let t = text();
                            (heap.new_str(t.as_str()), Some(t))
                        } else {
                            (VmValue::Int(0), None)
                        };
                        heap.list_mut_storing(lists[c], i, v).unwrap()[i] = v;
                        model[c][i] = t;
                    }
                    BarrierOp::Push(c) => {
                        let t = text();
                        let v = heap.new_str(t.as_str());
                        heap.list_push(lists[c], v).unwrap();
                        model[c].push(Some(t));
                    }
                    BarrierOp::Pop(c) => {
                        heap.list_mut_truncating(lists[c]).unwrap().pop();
                        model[c].pop();
                    }
                    BarrierOp::Clear(c) => {
                        heap.list_mut_truncating(lists[c]).unwrap().clear();
                        model[c].clear();
                    }
                    BarrierOp::Insert(c, i) => {
                        let i = i % (model[c].len() + 1);
                        let t = text();
                        let v = heap.new_str(t.as_str());
                        heap.list_mut(lists[c]).insert(i, v);
                        model[c].insert(i, Some(t));
                    }
                    BarrierOp::Remove(c, i) => {
                        if model[c].is_empty() {
                            continue;
                        }
                        let i = i % model[c].len();
                        heap.list_mut(lists[c]).remove(i);
                        model[c].remove(i);
                    }
                    BarrierOp::MapStore(m, s) => {
                        if map_model[m].is_empty() {
                            continue;
                        }
                        let s = s % map_model[m].len();
                        let t = text();
                        let v = heap.new_str(t.as_str());
                        let Obj::Map(mo) = heap.entries_mut_storing(maps[m], s, false, &[VmValue::Int(s as i64), v]) else { unreachable!() };
                        mo.entries[s] = Some((VmValue::Int(s as i64), v));
                        map_model[m][s] = Some(t);
                    }
                    BarrierOp::MapPush(m) => {
                        let s = map_model[m].len();
                        let t = text();
                        let v = heap.new_str(t.as_str());
                        let Obj::Map(mo) = heap.entries_mut_storing(maps[m], s, true, &[VmValue::Int(s as i64), v]) else { unreachable!() };
                        mo.entries.push(Some((VmValue::Int(s as i64), v)));
                        map_model[m].push(Some(t));
                    }
                    BarrierOp::MapRemove(m, s) => {
                        if map_model[m].is_empty() {
                            continue;
                        }
                        let s = s % map_model[m].len();
                        let Obj::Map(mo) = heap.get_mut(maps[m]) else { unreachable!() };
                        mo.entries.remove(s);
                        map_model[m].remove(s);
                    }
                    BarrierOp::Garbage(k) => {
                        for _ in 0..k {
                            let t = text();
                            heap.new_str(t);
                        }
                    }
                    BarrierOp::Collect(major) => heap.collect(roots.iter().copied(), major),
                    BarrierOp::Cycle => {
                        if !heap.is_incremental_gc_active() {
                            heap.start_incremental_gc(roots.iter().copied());
                        } else if heap.step_incremental_gc() {
                            heap.finish_incremental_marking(roots.iter().copied());
                        }
                    }
                }
                for (c, items) in model.iter().enumerate() {
                    let list = heap.list(lists[c]);
                    proptest::prop_assert_eq!(list.len(), items.len());
                    for (i, (&v, t)) in list.iter().zip(items).enumerate() {
                        match t {
                            Some(t) => proptest::prop_assert_eq!(heap.str(v), t.as_str(), "list {} index {}", c, i),
                            None => proptest::prop_assert_eq!(v, VmValue::Int(0)),
                        }
                    }
                }
                for (m, items) in map_model.iter().enumerate() {
                    let Obj::Map(mo) = heap.get(maps[m]) else { unreachable!() };
                    proptest::prop_assert_eq!(mo.entries.len(), items.len());
                    for (s, (e, t)) in mo.entries.iter().zip(items).enumerate() {
                        let v = e.unwrap().1;
                        match t {
                            Some(t) => proptest::prop_assert_eq!(heap.str(v), t.as_str(), "map {} entry {}", m, s),
                            None => proptest::prop_assert_eq!(v, VmValue::Int(0)),
                        }
                    }
                }
            }
        }
    }
}
