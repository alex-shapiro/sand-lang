//! Runtime values and heap objects.

use crate::value::{Value, is_qualified_name, rust_type_module};
use compact_str::CompactString;
use hashbrown::HashTable;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{Arc, OnceLock};

/// A Sand value as the VM stores it: an immediate value, or a handle to an
/// object on the VM heap.
///
/// A heap handle is valid only during the native call that received or
/// allocated it. The garbage collector does not trace a `VmValue` held in a
/// native function.
///
/// Variants `VUnit` through `VObj` store single-payload enum variants inline if
/// the payload is not itself an inline variant. Each variant holds the runtime
/// variant ID followed by the payload. Single-payload variants whose payload is
/// an inline variant use a heap [`Obj::Variant`]. Build variants with
/// [`VmValue::variant1`] or [`Heap::new_variant`](super::heap::Heap::new_variant).
/// Inspect variants with [`Heap::variant`](super::heap::Heap::variant),
/// [`Heap::variant_id`](super::heap::Heap::variant_id), and
/// [`Heap::field`](super::heap::Heap::field).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VmValue {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Dur(i64),
    /// A payload-less enum variant (runtime variant id).
    Tag(u32),
    /// A heap object handle.
    Obj(u32),
    /// A global that has not been initialized yet.
    Uninit,
    /// `variant(())`.
    VUnit(u32),
    /// `variant(bool)`.
    VBool(u32, bool),
    /// `variant(int)`.
    VInt(u32, i64),
    /// `variant(float)`.
    VFloat(u32, f64),
    /// `variant(duration)`.
    VDur(u32, i64),
    /// `variant(tag)` where the payload is a payload-less variant.
    VTag(u32, u32),
    /// `variant(object)`: the payload is the heap object with this handle.
    VObj(u32, u32),
    /// An inline 2-tuple of integers whose first element fits in 32 bits. Build
    /// tuples with [`VmValue::tuple2`] or
    /// [`Heap::new_tuple`](super::heap::Heap::new_tuple).
    Pair(i32, i64),
}

// The inline variants pack the discriminant, the u32 variant id and an
// 8-byte payload into the same 16 bytes as the other values.
const _: () = assert!(std::mem::size_of::<VmValue>() == 16);

impl VmValue {
    /// Returns the single-payload variant `id(payload)` in its inline form, or
    /// `None` if `payload` is itself an inline variant.
    #[inline]
    pub fn variant1(id: u32, payload: VmValue) -> Option<VmValue> {
        Some(match payload {
            VmValue::Unit => VmValue::VUnit(id),
            VmValue::Bool(b) => VmValue::VBool(id, b),
            VmValue::Int(i) => VmValue::VInt(id, i),
            VmValue::Float(f) => VmValue::VFloat(id, f),
            VmValue::Dur(d) => VmValue::VDur(id, d),
            VmValue::Tag(t) => VmValue::VTag(id, t),
            VmValue::Obj(h) => VmValue::VObj(id, h),
            _ => return None,
        })
    }

    /// The tuple `(a, b)`, if it is stored inline.
    #[inline]
    pub fn tuple2(a: VmValue, b: VmValue) -> Option<VmValue> {
        match (a, b) {
            (VmValue::Int(x), VmValue::Int(y)) => Some(VmValue::Pair(i32::try_from(x).ok()?, y)),
            _ => None,
        }
    }

    /// For an inline variant: its runtime variant id and payload.
    #[inline]
    pub fn as_inline_variant(self) -> Option<(u32, VmValue)> {
        Some(match self {
            VmValue::VUnit(id) => (id, VmValue::Unit),
            VmValue::VBool(id, b) => (id, VmValue::Bool(b)),
            VmValue::VInt(id, i) => (id, VmValue::Int(i)),
            VmValue::VFloat(id, f) => (id, VmValue::Float(f)),
            VmValue::VDur(id, d) => (id, VmValue::Dur(d)),
            VmValue::VTag(id, t) => (id, VmValue::Tag(t)),
            VmValue::VObj(id, h) => (id, VmValue::Obj(h)),
            _ => return None,
        })
    }

    /// The heap object this value refers to directly (an `Obj` handle or the
    /// payload of a `VObj`), for the garbage collector.
    #[inline]
    pub fn handle(self) -> Option<u32> {
        match self {
            VmValue::Obj(h) | VmValue::VObj(_, h) => Some(h),
            _ => None,
        }
    }

    #[inline]
    pub fn as_int(self) -> i64 {
        match self {
            VmValue::Int(i) => i,
            VmValue::Dur(d) => d,
            _ => 0,
        }
    }
    #[inline]
    pub fn as_float(self) -> f64 {
        match self {
            VmValue::Float(f) => f,
            VmValue::Int(i) => i as f64,
            _ => 0.0,
        }
    }
    #[inline]
    pub fn as_bool(self) -> bool {
        matches!(self, VmValue::Bool(true))
    }
}

/// A script integer used as a count or size (a repeat count, a width, a
/// length to allocate): negative values become 0, and values too large for
/// `usize` (only possible on 32-bit targets) saturate to `usize::MAX`, so an
/// oversized request fails the memory limit or range check it fails on a
/// 64-bit target instead of silently wrapping to a smaller one.
#[inline]
pub fn to_usize_saturating(n: i64) -> usize {
    saturate(n.max(0), usize::MAX)
}

/// A script integer used as an index: `None` if it is negative or too large
/// for `usize` (only possible on 32-bit targets), as such an index is out of
/// bounds of every sequence.
#[inline]
pub fn to_index(i: i64) -> Option<usize> {
    usize::try_from(i).ok()
}

/// A byte count or position from a `u64` (a file size, a cursor position)
/// as `usize`, saturating on 32-bit targets like [`to_usize_saturating`].
#[inline]
pub fn u64_to_usize_saturating(n: u64) -> usize {
    saturate(n, usize::MAX)
}

/// `n` converted to `T`, or `max` if it does not fit. Generic so the 32-bit
/// behavior of the conversions above can be tested on any target with `u32`.
#[inline]
fn saturate<N, T: TryFrom<N>>(n: N, max: T) -> T {
    T::try_from(n).unwrap_or(max)
}

/// A view of an enum variant across inline and heap representations.
#[derive(Clone, Copy, Debug)]
pub enum VariantRef<'a> {
    /// A payload-less variant or an inline single-payload variant.
    Inline(u32, Option<VmValue>),
    Heap(u32, &'a [VmValue]),
}

impl VariantRef<'_> {
    /// The runtime variant id.
    #[inline]
    pub fn id(&self) -> u32 {
        match self {
            VariantRef::Inline(id, _) | VariantRef::Heap(id, _) => *id,
        }
    }

    /// The payload fields.
    #[inline]
    pub fn fields(&self) -> &[VmValue] {
        match self {
            VariantRef::Inline(_, p) => p.as_slice(),
            VariantRef::Heap(_, f) => f,
        }
    }
}

/// Fast hasher for map and set keys. A folded multiply and random seed prevent hash collision attacks.
#[derive(Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

const MULTIPLIER: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The random seed of this process's hashes.
fn hash_seed() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(crate::platform::entropy)
}

impl Default for FxHasher {
    /// A hasher starting from the process seed.
    fn default() -> Self {
        FxHasher { hash: hash_seed() }
    }
}

impl FxHasher {
    /// Returns a hasher initialized to zero so hashes match across processes.
    pub(crate) fn unseeded() -> FxHasher {
        FxHasher { hash: 0 }
    }

    #[inline]
    fn add(&mut self, w: u64) {
        let p = ((self.hash ^ w) as u128).wrapping_mul(MULTIPLIER as u128);
        self.hash = (p as u64) ^ ((p >> 64) as u64);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rem) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        // The last 1 to 7 bytes in one word, in a few loads: injective for a
        // given length (two overlapping halves, or the first, middle and
        // last byte of at most three).
        let n = rem.len();
        let w = match n {
            0 => return,
            1..=3 => rem[0] as u64 | (rem[n / 2] as u64) << 8 | (rem[n - 1] as u64) << 16,
            _ => {
                let head = u32::from_le_bytes([rem[0], rem[1], rem[2], rem[3]]);
                let tail = u32::from_le_bytes([rem[n - 4], rem[n - 3], rem[n - 2], rem[n - 1]]);
                head as u64 | (tail as u64) << 32
            }
        };
        self.add(w);
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64)
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64)
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i)
    }
    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.add(i as u64)
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64)
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// Builds [`FxHasher`]s: holds the process's seed, read once per table.
#[derive(Clone, Copy)]
pub struct FxBuild {
    seed: u64,
}

impl Default for FxBuild {
    fn default() -> Self {
        FxBuild { seed: hash_seed() }
    }
}

impl BuildHasher for FxBuild {
    type Hasher = FxHasher;
    #[inline]
    fn build_hasher(&self) -> FxHasher {
        FxHasher { hash: self.seed }
    }
}

/// `HashMap` seeded once per process by [`FxBuild`]. Use `FxHashMap` for
/// hash tables keyed by script-controlled data to prevent hash-collision
/// denial-of-service attacks.
pub type FxHashMap<K, V> = HashMap<K, V, FxBuild>;

/// The hash index of a map or set: the slots of its entries, found by the
/// hash of their keys (`compare::hash_key`).
///
/// Keys are never copied: `Hash` types are deeply immutable (spec §5.6), so a
/// key is hashed and compared where it lives on the heap. Each table entry
/// packs the slot (low 32 bits) with a 32-bit tag of the key's hash (high
/// bits), which is enough to grow the table without rehashing any key and
/// rejects almost every mismatch before the keys themselves are compared.
#[derive(Clone, Default)]
pub struct KeyTable {
    table: HashTable<u64>,
}

impl std::fmt::Debug for KeyTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyTable({})", self.table.len())
    }
}

/// Whether a map or set can hold an entry (live or removed) at `slot`:
/// slots are stored in 32 bits.
#[inline]
pub fn slot_fits(slot: usize) -> bool {
    u32::try_from(slot).is_ok()
}

/// Bytes per bucket of a [`KeyTable`] (an entry and a control byte).
const TABLE_BUCKET_SIZE: usize = 9;

#[inline]
fn hash_tag(hash: u64) -> u32 {
    (hash ^ (hash >> 32)) as u32
}

/// The table hash of a tag: its bits spread over 64 (hashbrown picks the
/// bucket from the low bits and the control byte from the top 7).
#[inline]
fn tag_hash(tag: u32) -> u64 {
    (tag as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[inline]
fn entry_slot(e: u64) -> usize {
    e as u32 as usize
}

#[inline]
fn entry_tag(e: u64) -> u32 {
    (e >> 32) as u32
}

impl KeyTable {
    /// The slot whose key has hash `hash` and satisfies `eq(slot)`.
    #[inline]
    pub fn find(&self, hash: u64, mut eq: impl FnMut(usize) -> bool) -> Option<usize> {
        let tag = hash_tag(hash);
        self.table.find(tag_hash(tag), |&e| entry_tag(e) == tag && eq(entry_slot(e))).map(|&e| entry_slot(e))
    }

    /// Adds `slot` (see [`slot_fits`]) for a key with hash `hash` that is not
    /// in the table.
    #[inline]
    pub fn insert(&mut self, hash: u64, slot: usize) {
        debug_assert!(slot_fits(slot));
        let tag = hash_tag(hash);
        self.table.insert_unique(tag_hash(tag), (tag as u64) << 32 | slot as u64, |&e| tag_hash(entry_tag(e)));
    }

    /// Removes and returns the slot whose key has hash `hash` and satisfies `eq(slot)`.
    pub fn remove(&mut self, hash: u64, mut eq: impl FnMut(usize) -> bool) -> Option<usize> {
        let tag = hash_tag(hash);
        match self.table.find_entry(tag_hash(tag), |&e| entry_tag(e) == tag && eq(entry_slot(e))) {
            Ok(entry) => Some(entry_slot(entry.remove().0)),
            Err(_) => None,
        }
    }

    /// Renumbers the slots after the entries were compacted.
    pub fn renumber(&mut self, f: impl Fn(usize) -> usize) {
        for e in self.table.iter_mut() {
            *e = (*e & !(u32::MAX as u64)) | f(entry_slot(*e)) as u64;
        }
    }

    /// Makes room for `extra` more keys.
    pub fn reserve(&mut self, extra: usize) {
        self.table.reserve(extra, |&e| tag_hash(entry_tag(e)));
    }

    pub fn clear(&mut self) {
        self.table = HashTable::new();
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// The memory the table owns.
    pub fn bytes(&self) -> usize {
        Self::bytes_for(self.capacity())
    }

    /// The number of keys the table can hold without growing.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.table.capacity()
    }

    /// The memory of a table with capacity `cap`.
    pub fn bytes_for(cap: usize) -> usize {
        // A table of n buckets allocates n entries and n + 16 control bytes.
        match cap {
            0 => 0,
            cap => (cap * 8 / 7).next_power_of_two() * TABLE_BUCKET_SIZE + 16,
        }
    }
}

/// The hash of an int key (`compare::hash_key`).
#[inline]
pub fn int_hash(i: i64) -> u64 {
    let mut h = FxHasher::default();
    h.write_i64(i);
    h.finish()
}

/// Entry slot index for a map or set.
#[derive(Clone, Debug)]
pub enum KeyIndex {
    /// Direct lookup table where `slots[k]` stores `slot + 1`, or `0` if empty.
    Dense {
        slots: Vec<u32>,
        count: usize,
    },
    Hashed(KeyTable),
}

impl Default for KeyIndex {
    fn default() -> Self {
        KeyIndex::Dense { slots: Vec::new(), count: 0 }
    }
}

#[inline]
fn dense_fits(k: i64, count: usize) -> bool {
    0 <= k && (k as u64) < 8 * count as u64 + 64
}

impl KeyIndex {
    /// Returns an empty dense index if all `keys` fit in a dense table for
    /// `count` entries, or a preallocated hash index.
    pub fn for_keys(count: usize, mut keys: impl Iterator<Item = VmValue>) -> KeyIndex {
        if keys.all(|k| matches!(k, VmValue::Int(i) if dense_fits(i, count))) {
            return KeyIndex::default();
        }
        let mut t = KeyTable::default();
        t.reserve(count);
        KeyIndex::Hashed(t)
    }

    /// The slot of int key `k` (`eq(slot)` tells whether it holds `k`).
    #[inline]
    pub fn find_int(&self, k: i64, eq: impl FnMut(usize) -> bool) -> Option<usize> {
        match self {
            KeyIndex::Dense { slots, .. } => match slots.get(usize::try_from(k).ok()?) {
                Some(&s) if s != 0 => Some(s as usize - 1),
                _ => None,
            },
            KeyIndex::Hashed(t) => t.find(int_hash(k), eq),
        }
    }

    /// The slot of key `k`, with hash `hash`, satisfying `eq(slot)`.
    #[inline]
    pub fn find(&self, k: VmValue, hash: u64, eq: impl FnMut(usize) -> bool) -> Option<usize> {
        match (self, k) {
            (KeyIndex::Dense { .. }, VmValue::Int(i)) => self.find_int(i, eq),
            // A dense index holds only ints.
            (KeyIndex::Dense { .. }, _) => None,
            (KeyIndex::Hashed(t), _) => t.find(hash, eq),
        }
    }

    /// Adds `slot` (see [`slot_fits`]) for key `k`, with hash `hash`, which
    /// is not in the index.
    pub fn insert(&mut self, k: VmValue, hash: u64, slot: usize) {
        if let KeyIndex::Dense { slots, count } = self {
            if let VmValue::Int(i) = k
                && dense_fits(i, *count)
            {
                let i = i as usize;
                if i == slots.len() {
                    slots.push(slot as u32 + 1);
                } else {
                    if i > slots.len() {
                        slots.resize(i + 1, 0);
                    }
                    slots[i] = slot as u32 + 1;
                }
                *count += 1;
                return;
            }
            self.hash_all();
        }
        let KeyIndex::Hashed(t) = self else { unreachable!() };
        t.insert(hash, slot);
    }

    /// Turns a dense index into a hashed one.
    fn hash_all(&mut self) {
        let KeyIndex::Dense { slots, count } = self else { return };
        let mut t = KeyTable::default();
        t.reserve(*count + 1);
        for (k, &s) in slots.iter().enumerate() {
            if s != 0 {
                t.insert(int_hash(k as i64), s as usize - 1);
            }
        }
        *self = KeyIndex::Hashed(t);
    }

    /// Where a hashed index would grow, makes it dense if its `keys` (with
    /// their slots) allow `count` keys: those and one more to be inserted.
    pub fn densify(&mut self, count: usize, keys: impl Iterator<Item = (usize, VmValue)> + Clone) {
        let KeyIndex::Hashed(t) = self else { return };
        if t.len() < t.capacity() || !keys.clone().all(|(_, k)| matches!(k, VmValue::Int(i) if dense_fits(i, count))) {
            return;
        }
        let mut slots = Vec::new();
        let mut n = 0;
        for (s, k) in keys {
            let VmValue::Int(i) = k else { unreachable!() };
            if i as usize >= slots.len() {
                slots.resize(i as usize + 1, 0);
            }
            slots[i as usize] = s as u32 + 1;
            n += 1;
        }
        *self = KeyIndex::Dense { slots, count: n };
    }

    /// Removes and returns the slot of key `k`, with hash `hash`, satisfying `eq(slot)`.
    pub fn remove(&mut self, k: VmValue, hash: u64, eq: impl FnMut(usize) -> bool) -> Option<usize> {
        match self {
            KeyIndex::Dense { slots, count } => {
                let VmValue::Int(i) = k else { return None };
                let s = slots.get_mut(usize::try_from(i).ok()?)?;
                let slot = (*s as usize).checked_sub(1)?;
                *s = 0;
                *count -= 1;
                Some(slot)
            }
            KeyIndex::Hashed(t) => t.remove(hash, eq),
        }
    }

    /// Renumbers the slots after the entries were compacted.
    pub fn renumber(&mut self, f: impl Fn(usize) -> usize) {
        match self {
            KeyIndex::Dense { slots, .. } => slots.iter_mut().filter(|s| **s != 0).for_each(|s| *s = f(*s as usize - 1) as u32 + 1),
            KeyIndex::Hashed(t) => t.renumber(f),
        }
    }

    /// Makes room for `extra` more keys.
    pub fn reserve(&mut self, extra: usize) {
        if let KeyIndex::Hashed(t) = self {
            t.reserve(extra);
        }
    }

    /// For adding key `k` as the `count`th key, densifying first as
    /// `map_push` does: an upper bound of the bytes the index allocates, and
    /// the index's length if it is then dense.
    fn insert_growth(&self, k: VmValue, count: usize) -> (usize, Option<usize>) {
        const SLOT: usize = std::mem::size_of::<u32>();
        match (self, k) {
            (KeyIndex::Dense { slots, count: n }, VmValue::Int(i)) if dense_fits(i, *n) => {
                let len = slots.len().max(i as usize + 1);
                let cap = slots.capacity();
                (if len <= cap { 0 } else { len.max(2 * cap).max(4) * SLOT }, Some(len))
            }
            (KeyIndex::Dense { count: n, .. }, _) => (KeyTable::bytes_for(n + 1), None),
            (KeyIndex::Hashed(t), _) if t.len() < t.capacity() => (0, None),
            (KeyIndex::Hashed(t), _) => {
                let dense = 8 * count + 64;
                (KeyTable::bytes_for(t.capacity() + 1).max(dense * SLOT), Some(dense))
            }
        }
    }

    /// The memory the index owns.
    pub fn bytes(&self) -> usize {
        match self {
            KeyIndex::Dense { slots, .. } => slots.capacity() * std::mem::size_of::<u32>(),
            KeyIndex::Hashed(t) => t.bytes(),
        }
    }
}

/// Removes the tombstones of `entries` if they are more than half of them,
/// renumbering `table`.
pub fn compact_entries<T: Copy>(entries: &mut Vec<Option<T>>, live: usize, table: &mut KeyIndex) {
    if entries.len() <= 32 || live * 2 >= entries.len() {
        return;
    }
    let mut remap = vec![u32::MAX; entries.len()];
    let mut fresh = Vec::with_capacity(live);
    for (i, e) in entries.iter().enumerate() {
        if let Some(e) = e {
            remap[i] = fresh.len() as u32;
            fresh.push(Some(*e));
        }
    }
    *entries = fresh;
    table.renumber(|i| remap[i] as usize);
}

/// An insertion-ordered hash map: entries in insertion order (`None` for
/// removed ones, compacted when they are the majority), indexed by key.
///
/// Iterators read the entries in place while the map is unchanged; every
/// change first calls `Vm::unpin`, which moves the iterators still reading
/// it (its `pins`) to a copy of the entries (see `vm/iter.rs`).
#[derive(Debug, Default, Clone)]
pub struct MapObj {
    pub index: KeyIndex,
    pub entries: Vec<Option<(VmValue, VmValue)>>,
    pub live: usize,
    pub pins: Pins,
    /// Direct key-to-value table used while `index` is dense and at least 25%
    /// full. Absent keys hold [`VmValue::Uninit`]. Lookups read `vals[k]`
    /// directly. [`MapObj::set_dense`] updates `vals` on every write.
    pub vals: Vec<VmValue>,
}

/// An insertion-ordered hash set (see [`MapObj`]).
#[derive(Debug, Default, Clone)]
pub struct SetObj {
    pub index: KeyIndex,
    pub entries: Vec<Option<VmValue>>,
    pub live: usize,
    pub pins: Pins,
}

/// Weak handles of `Obj::IterFn` objects that may be reading a map or set in
/// place. Because the collector does not trace these handles, a handle can
/// become stale after its iterator finishes, detaches to a copy, or is
/// collected and its arena slot is reused or truncated.
#[derive(Debug, Default)]
pub struct Pins {
    pub its: Vec<u32>,
    /// Length of `its` at which stale handles are pruned.
    pub prune_at: usize,
    /// Active iterators created since the last detach. This count changes only
    /// as iterators start, finish, or detach.
    pub open: usize,
}

impl Clone for Pins {
    fn clone(&self) -> Pins {
        Pins::default()
    }
}

impl Pins {
    /// The memory the pins own.
    pub fn bytes(&self) -> usize {
        self.its.capacity() * std::mem::size_of::<u32>()
    }
}

impl MapObj {
    pub fn clear(&mut self) {
        *self = MapObj::default();
    }

    /// The memory of the index and the dense values.
    pub fn index_bytes(&self) -> usize {
        self.index.bytes() + self.vals.capacity() * VALUE_SIZE
    }

    /// An upper bound of the bytes that adding the new key `k` allocates for
    /// the index and the dense values (not the entries).
    pub fn index_growth(&self, k: VmValue) -> usize {
        let count = self.live + 1;
        let (index, dense) = self.index.insert_growth(k, count);
        // The dense values, sized as `set_dense` sizes them.
        let vals = match dense {
            Some(len) if count * 8 >= len => {
                let cap = self.vals.capacity();
                if self.vals.is_empty() {
                    if count * 4 >= len { len * VALUE_SIZE } else { 0 }
                } else if len > cap {
                    len.max(2 * cap) * VALUE_SIZE
                } else {
                    0
                }
            }
            _ => 0,
        };
        index + vals
    }

    /// Updates `vals` after setting key `k` to `v`, or removing `k` with
    /// [`VmValue::Uninit`]. The caller must update `index` and `entries` first.
    pub fn set_dense(&mut self, k: VmValue, v: VmValue) {
        let KeyIndex::Dense { slots, count } = &self.index else {
            self.vals = Vec::new();
            return;
        };
        if *count * 8 < slots.len() {
            self.vals = Vec::new();
            return;
        }
        if self.vals.is_empty() {
            if *count * 4 < slots.len() {
                return;
            }
            self.vals = vec![VmValue::Uninit; slots.len()];
            for (key, &s) in slots.iter().enumerate() {
                if let Some(Some((_, x))) = s.checked_sub(1).map(|s| &self.entries[s as usize]) {
                    self.vals[key] = *x;
                }
            }
            return;
        }
        if let VmValue::Int(i) = k
            && i as usize == self.vals.len()
            && slots.len() == self.vals.len() + 1
        {
            self.vals.push(v);
            return;
        }
        if self.vals.len() < slots.len() {
            self.vals.resize(slots.len(), VmValue::Uninit);
        }
        if let VmValue::Int(i) = k
            && let Some(x) = self.vals.get_mut(i as usize)
        {
            *x = v;
        }
    }
}

impl SetObj {
    pub fn clear(&mut self) {
        *self = SetObj::default();
    }

    /// An upper bound of the bytes that adding the new key `k` allocates for
    /// the index (not the entries).
    pub fn index_growth(&self, k: VmValue) -> usize {
        self.index.insert_growth(k, self.live + 1).0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Done,
    Cancelled,
}

#[derive(Debug)]
pub struct TaskObj {
    pub fiber: u32,
    pub state: TaskState,
    pub result: VmValue,
}

/// A channel. Values enter its queue only at the back ([`ChannelObj::push`])
/// and leave it only at the front. The collector relies on this (see
/// `Heap::channel_mut`).
#[derive(Debug, Default)]
pub struct ChannelObj {
    queue: VecDeque<VmValue>,
    /// The number of values pushed since the heap last remembered the
    /// channel. Only these values of the queue can be young.
    pub(super) pushed: usize,
    /// The most values the queue holds, or 0 if it is unbounded.
    pub capacity: usize,
    pub closed: bool,
    /// Fibers blocked in `recv`: (fiber, wait generation).
    pub receivers: VecDeque<(u32, u64)>,
    /// Stale `receivers` (of cancelled tasks) are pruned when it reaches this length.
    pub receivers_prune_at: usize,
    /// Fibers blocked in `send`, with their pending values.
    pub senders: VecDeque<(u32, u64, VmValue)>,
}

impl ChannelObj {
    pub fn new(capacity: usize) -> ChannelObj {
        ChannelObj { capacity, ..Default::default() }
    }

    /// The number of values queued.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Whether a value can be queued without blocking the sender.
    pub fn has_room(&self) -> bool {
        self.capacity == 0 || self.queue.len() < self.capacity
    }

    pub(super) fn queue(&self) -> &VecDeque<VmValue> {
        &self.queue
    }

    /// Makes room in the queue for `extra` more values.
    pub(super) fn reserve_exact(&mut self, extra: usize) {
        self.queue.reserve_exact(extra);
    }

    /// Appends `v` to the queue.
    pub(super) fn push(&mut self, v: VmValue) {
        self.queue.push_back(v);
        self.pushed = self.pushed.saturating_add(1);
    }

    /// Takes the value at the front of the queue and moves the first blocked
    /// sender's value into the queue. If the queue is empty, takes the first
    /// blocked sender's value directly. Returns the value and the sender to
    /// wake, if any.
    pub fn take(&mut self) -> Option<(VmValue, Option<(u32, u64)>)> {
        match self.queue.pop_front() {
            Some(v) => {
                let woken = self.senders.pop_front().map(|(f, g, sv)| {
                    self.push(sv);
                    (f, g)
                });
                Some((v, woken))
            }
            None => self.senders.pop_front().map(|(f, g, v)| (v, Some((f, g)))),
        }
    }
}

/// A Rust type that implements an immutable Sand value type.
///
/// Derive [`NamedType`](crate::native::NamedType) and the value conversions
/// with [`#[derive(SandValue)]`](crate::SandValue). Implement `ValueType`.
/// Register the type with [`NativeModule::bind`](crate::native::NativeModule::bind):
///
/// ```
/// use sandlang::vm::value::ValueType;
///
/// /// An amount of money in cents, such as `12.50`.
/// #[derive(Clone, PartialEq, Eq, Hash, sandlang::SandValue)]
/// #[sand(module = "bank")]
/// pub struct Money(u64);
///
/// impl std::fmt::Display for Money {
///     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
///         write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
///     }
/// }
///
/// impl ValueType for Money {
///     fn parse(text: &str) -> Result<Money, String> {
///         let parts = text.split_once('.').filter(|(_, cents)| cents.len() == 2);
///         let (units, cents) = parts.ok_or("expected `units.cents`")?;
///         let n = |s: &str| s.parse::<u64>().map_err(|e| e.to_string());
///         let total = n(units)?.checked_mul(100).and_then(|u| u.checked_add(n(cents).ok()?));
///         total.map(Money).ok_or_else(|| format!("invalid amount `{text}`"))
///     }
/// }
/// ```
///
/// The runtime uses `Eq`, `Hash`, and `Display` to compare, hash, and format
/// values. It encodes and decodes values in JSON as strings. Equal values must
/// produce identical `Display` output. [`Self::parse`] must parse that output
/// into an equal value. Execute `Eq`, `Hash`, and `Display` in constant time,
/// because the runtime treats these operations as constant-cost.
pub trait ValueType:
    crate::native::NamedType
    + crate::native::convert::IntoVmValue
    + for<'a> crate::native::convert::FromVmValue<Item<'a> = Self>
    + Clone
    + Eq
    + Hash
    + std::fmt::Display
    + Send
    + Sync
    + 'static
{
    /// Comparison function for types that satisfy `Ord` (typically `Some(Ord::cmp)`).
    const ORDER: Option<fn(&Self, &Self) -> std::cmp::Ordering> = None;
    /// Optional 16-byte inline heap representation.
    const PACKING: Option<Packing<Self>> = None;
    /// Parses the text output of `Display`.
    fn parse(text: &str) -> Result<Self, String>;
    /// Approximate heap memory used by `self`, in bytes.
    fn size(&self) -> usize {
        std::mem::size_of::<Self>()
    }
    /// Returns `self` as the case of [`Value`] that is dedicated to the type,
    /// such as `Value::PlainDate`, or `None` if the type has no dedicated
    /// case, so that the [`Value`] of `self` is a [`Value::Native`].
    #[doc(hidden)]
    fn to_dedicated_value(&self) -> Option<Value> {
        None
    }
    /// Returns `value` as `Self`, or `None` if `value` is not a `Self`. See
    /// [`Value::native`].
    #[doc(hidden)]
    fn from_value(value: &Value) -> Option<Self> {
        let Value::Native(native) = value else { return None };
        let is_self = is_qualified_name(&native.type_name, rust_type_module(Self::MODULE), Self::NAME);
        if is_self { Self::parse(&native.text).ok() } else { None }
    }
}

/// Packs a [`ValueType`] into 16 bytes so it can be stored inline in a heap slot.
///
/// `pack` must map equal values to identical `[u64; 2]` words, and `unpack`
/// must reconstruct an equal value. Set `ordered` to `true` if lexicographic
/// comparison of the `[u64; 2]` words matches [`ValueType::ORDER`].
pub struct Packing<T> {
    pub pack: fn(&T) -> [u64; 2],
    pub unpack: fn([u64; 2]) -> T,
    pub ordered: bool,
}

impl<T> Clone for Packing<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Packing<T> {}

/// Object-safe trait for boxed [`ValueType`] values.
pub trait ValueObj: Any + Send {
    fn module_name(&self) -> &'static str;
    fn type_name(&self) -> &'static str;
    fn value_eq(&self, other: &dyn ValueObj) -> bool;
    fn value_cmp(&self, other: &dyn ValueObj) -> Option<std::cmp::Ordering>;
    fn value_hash(&self) -> u64;
    fn write_text(&self, out: &mut String);
    fn size(&self) -> usize;
    fn to_dedicated_value(&self) -> Option<Value>;
}

impl<T: ValueType> ValueObj for T {
    fn module_name(&self) -> &'static str {
        T::MODULE
    }
    fn type_name(&self) -> &'static str {
        T::NAME
    }
    fn value_eq(&self, other: &dyn ValueObj) -> bool {
        (other as &dyn Any).downcast_ref::<T>() == Some(self)
    }
    fn value_cmp(&self, other: &dyn ValueObj) -> Option<std::cmp::Ordering> {
        Some(T::ORDER?(self, (other as &dyn Any).downcast_ref::<T>()?))
    }
    fn value_hash(&self) -> u64 {
        let mut h = FxHasher::default();
        self.hash(&mut h);
        h.finish()
    }
    fn write_text(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = write!(out, "{self}");
    }
    fn size(&self) -> usize {
        ValueType::size(self)
    }
    fn to_dedicated_value(&self) -> Option<Value> {
        ValueType::to_dedicated_value(self)
    }
}

type PackedOrder = fn([u64; 2], [u64; 2]) -> std::cmp::Ordering;

/// Type descriptor for an inline 16-byte [`ValueType`].
pub struct PackedType {
    module: &'static str,
    name: &'static str,
    order: Option<PackedOrder>,
    pub(crate) ordered_bits: bool,
    write_text: fn([u64; 2], &mut String),
    to_dedicated_value: fn([u64; 2]) -> Option<Value>,
    type_id: fn() -> std::any::TypeId,
}

impl PackedType {
    pub(crate) fn is_same_type(&self, other: &PackedType) -> bool {
        std::ptr::eq(self, other) || (self.type_id)() == (other.type_id)()
    }
}

struct PackedTypeOf<T>(std::marker::PhantomData<T>);

impl<T: ValueType> PackedTypeOf<T> {
    const ORDERED_BITS: bool = match T::PACKING {
        Some(p) => p.ordered && T::ORDER.is_some(),
        None => false,
    };
    const TYPE: PackedType = PackedType {
        module: T::MODULE,
        name: T::NAME,
        order: match (T::ORDER, Self::ORDERED_BITS) {
            (None, _) => None,
            (Some(_), true) => Some(|a, b| a.cmp(&b)),
            (Some(_), false) => Some(|a, b| match (unpack::<T>(a), unpack::<T>(b), T::ORDER) {
                (Some(x), Some(y), Some(order)) => order(&x, &y),
                _ => a.cmp(&b),
            }),
        },
        ordered_bits: Self::ORDERED_BITS,
        write_text: |a, out| {
            if let Some(x) = unpack::<T>(a) {
                x.write_text(out)
            }
        },
        to_dedicated_value: |a| unpack::<T>(a).and_then(|x| x.to_dedicated_value()),
        type_id: std::any::TypeId::of::<T>,
    };
}

fn unpack<T: ValueType>(bits: [u64; 2]) -> Option<T> {
    T::PACKING.map(|p| (p.unpack)(bits))
}

#[derive(Clone, Copy)]
pub enum ValueRef<'a> {
    Boxed(&'a dyn ValueObj),
    Packed(&'static PackedType, [u64; 2]),
}

impl ValueRef<'_> {
    pub fn module_name(self) -> &'static str {
        match self {
            ValueRef::Boxed(x) => x.module_name(),
            ValueRef::Packed(t, _) => t.module,
        }
    }

    pub fn type_name(self) -> &'static str {
        match self {
            ValueRef::Boxed(x) => x.type_name(),
            ValueRef::Packed(t, _) => t.name,
        }
    }

    pub fn value_eq(self, other: ValueRef<'_>) -> bool {
        match (self, other) {
            (ValueRef::Boxed(x), ValueRef::Boxed(y)) => x.value_eq(y),
            (ValueRef::Packed(t, a), ValueRef::Packed(u, b)) => a == b && t.is_same_type(u),
            _ => false,
        }
    }

    pub fn value_cmp(self, other: ValueRef<'_>) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (ValueRef::Boxed(x), ValueRef::Boxed(y)) => x.value_cmp(y),
            (ValueRef::Packed(t, a), ValueRef::Packed(u, b)) if t.is_same_type(u) => t.order.map(|order| order(a, b)),
            _ => None,
        }
    }

    pub fn value_hash(self) -> u64 {
        match self {
            ValueRef::Boxed(x) => x.value_hash(),
            ValueRef::Packed(_, [a, b]) => {
                let mut h = FxHasher::default();
                h.write_u64(a);
                h.write_u64(b);
                h.finish()
            }
        }
    }

    pub fn write_text(self, out: &mut String) {
        match self {
            ValueRef::Boxed(x) => x.write_text(out),
            ValueRef::Packed(t, a) => (t.write_text)(a, out),
        }
    }

    pub fn text(self) -> String {
        let mut s = String::new();
        self.write_text(&mut s);
        s
    }

    /// Returns the value as the case of [`Value`] that is dedicated to its
    /// type, or `None` if the type has no dedicated case. See
    /// [`ValueType::to_dedicated_value`].
    pub(crate) fn to_dedicated_value(self) -> Option<Value> {
        match self {
            ValueRef::Boxed(x) => x.to_dedicated_value(),
            ValueRef::Packed(t, a) => (t.to_dedicated_value)(a),
        }
    }
}

impl Obj {
    /// Returns the object kind name for internal error messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Obj::Str(_) => "str",
            Obj::StrBuf(_) => "string buffer",
            Obj::Bytes(_) => "bytes",
            Obj::List(_) => "list",
            Obj::Map(_) => "map",
            Obj::Set(_) => "set",
            Obj::Tuple(_) => "tuple",
            Obj::Struct(..) => "struct",
            Obj::Variant(..) => "variant",
            Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) => "function",
            Obj::Range(..) => "range",
            Obj::Task(_) => "task",
            Obj::Channel(_) => "channel",
            Obj::Native(_) => "native object",
            Obj::Value(_) | Obj::Packed(..) => "value type",
            _ => "internal object",
        }
    }

    pub fn value(&self) -> Option<ValueRef<'_>> {
        match self {
            Obj::Value(x) => Some(ValueRef::Boxed(&**x)),
            Obj::Packed(t, bits) => Some(ValueRef::Packed(t, *bits)),
            _ => None,
        }
    }
}

pub(crate) fn value_obj<T: ValueType>(x: T) -> Obj {
    match T::PACKING {
        Some(p) => Obj::Packed(&PackedTypeOf::<T>::TYPE, (p.pack)(&x)),
        None => Obj::Value(Box::new(x)),
    }
}

pub(crate) fn value_of<T: ValueType>(obj: &Obj) -> Option<T> {
    match obj {
        Obj::Value(x) => (&**x as &dyn Any).downcast_ref::<T>().cloned(),
        Obj::Packed(t, bits) if (t.type_id)() == std::any::TypeId::of::<T>() => unpack(*bits),
        _ => None,
    }
}

pub type ParseValue = fn(&str) -> Result<Obj, String>;

pub(crate) fn parse_value<T: ValueType>(text: &str) -> Result<Obj, String> {
    Ok(value_obj(T::parse(text)?))
}

/// Struct field descriptor: `(name, type, default)`.
pub type FieldDesc = (String, Arc<TypeDesc>, FieldDefault);

/// Default value for a struct field used when a field is omitted during `json.decode`.
#[derive(Clone, Debug)]
pub enum FieldDefault {
    None,
    /// Compile-time constant shared across [`DescTemplate`] instantiations.
    Const(Arc<crate::types::ConstVal>),
    /// Non-constant default evaluated after decoding by helper prototype `fill`
    /// if bit `bit` is set in the captured field bitmask.
    Computed {
        fill: u32,
        bit: u8,
    },
}

/// A variant of an enum [`TypeDesc`], which `reflect` and `json.decode` use.
#[derive(Clone)]
pub struct ReflectVariantDesc {
    /// The variant name, such as `Shipped`.
    pub name: String,
    /// The runtime ID of the variant, which a [`VmValue::Tag`] holds.
    pub runtime_id: u32,
    /// The type of each payload value in declaration order.
    pub payload_types: Vec<Arc<TypeDesc>>,
    /// The names of the payload fields, or `None` for a positional payload.
    pub field_names: Option<Vec<String>>,
}

/// Runtime type descriptor for `reflect` type parameters, constructed as a
/// directed acyclic graph (DAG) by `compile::reflect`.
#[derive(Clone)]
pub enum TypeDesc {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Duration,
    Unit,
    List(Arc<TypeDesc>),
    Map(Arc<TypeDesc>, Arc<TypeDesc>),
    /// Set element descriptor and the helper function prototype that populates a set from a list.
    Set(Arc<TypeDesc>, u32),
    Tuple(Vec<Arc<TypeDesc>>),
    Option(Arc<TypeDesc>),
    Range,
    Struct {
        /// Instance ID used to resolve [`TypeDesc::Recur`] references.
        id: u32,
        def: u32,
        name: String,
        fields: Vec<FieldDesc>,
        /// [`NamedType::rust_type`](crate::native::NamedType::rust_type) for a Rust-bound struct.
        rust: Option<std::any::TypeId>,
    },
    Enum {
        id: u32,
        name: String,
        variants: Vec<ReflectVariantDesc>,
        /// [`NamedType::rust_type`](crate::native::NamedType::rust_type) for a Rust-bound enum.
        rust: Option<std::any::TypeId>,
    },
    Value {
        name: String,
        parse: ParseValue,
    },
    /// Recursive reference to an enclosing struct or enum with matching `id`.
    Recur(u32),
    /// Non-encodable type (function, task, channel, or native object).
    Unsupported(String),
    /// Uninstantiated type parameter inside a [`DescTemplate`].
    Param(u32),
}

impl TypeDesc {
    /// Formats the Sand type name for `self`, using `recur` to resolve
    /// [`TypeDesc::Recur`] IDs. Names longer than [`MAX_TYPE_NAME_LEN`] bytes
    /// are truncated with `…`.
    pub fn format(&self, recur: &dyn Fn(u32) -> String) -> String {
        let mut out = String::new();
        self.write_name(recur, &mut out);
        truncate_type_name(&mut out);
        out
    }

    fn write_name(&self, recur: &dyn Fn(u32) -> String, out: &mut String) {
        if out.len() > MAX_TYPE_NAME_LEN {
            return;
        }
        match self {
            TypeDesc::Int => out.push_str("int"),
            TypeDesc::Float => out.push_str("float"),
            TypeDesc::Bool => out.push_str("bool"),
            TypeDesc::Str => out.push_str("str"),
            TypeDesc::Bytes => out.push_str("bytes"),
            TypeDesc::Duration => out.push_str("duration"),
            TypeDesc::Unit => out.push_str("()"),
            TypeDesc::List(e) => {
                out.push('[');
                e.write_name(recur, out);
                out.push(']');
            }
            TypeDesc::Map(k, v) => {
                out.push('[');
                k.write_name(recur, out);
                out.push_str(": ");
                v.write_name(recur, out);
                out.push(']');
            }
            TypeDesc::Set(e, _) => {
                out.push_str("Set<");
                e.write_name(recur, out);
                out.push('>');
            }
            TypeDesc::Tuple(ts) => {
                out.push('(');
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    t.write_name(recur, out);
                }
                out.push(')');
            }
            TypeDesc::Option(_) => {
                let (mut inner, mut layers) = (self, 0);
                while let TypeDesc::Option(e) = inner {
                    (inner, layers) = (&**e, layers + 1);
                }
                inner.write_name(recur, out);
                for _ in 0..layers.min(MAX_TYPE_NAME_LEN + 1) {
                    out.push('?');
                }
            }
            TypeDesc::Range => out.push_str("Range"),
            TypeDesc::Struct { name, .. } | TypeDesc::Enum { name, .. } | TypeDesc::Value { name, .. } | TypeDesc::Unsupported(name) => {
                out.push_str(name)
            }
            TypeDesc::Recur(id) => out.push_str(&recur(*id)),
            TypeDesc::Param(_) => out.push('_'),
        }
    }
}

/// Maximum byte length of a formatted type name in a [`TypeDesc`].
const MAX_TYPE_NAME_LEN: usize = 256;

/// Truncates `name` to [`MAX_TYPE_NAME_LEN`] bytes on a UTF-8 character boundary and appends `…`.
pub(crate) fn truncate_type_name(name: &mut String) {
    if name.len() > MAX_TYPE_NAME_LEN {
        let mut end = MAX_TYPE_NAME_LEN;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name.push('…');
    }
}

/// Heap-allocated runtime type descriptor.
#[derive(Clone)]
pub struct TypeDescObj {
    pub desc: Arc<TypeDesc>,
    /// Metadata for a template-instantiated descriptor, or `None` for a constant descriptor.
    pub instantiated: Option<Box<InstantiatedDesc>>,
}

/// Memory accounting and GC references for a [`TypeDescObj`] produced by [`DescTemplate::instantiate`].
#[derive(Clone)]
pub struct InstantiatedDesc {
    /// Heap bytes allocated for newly constructed [`TypeDesc`] nodes.
    bytes: u32,
    /// Maximum nesting depth of instantiated template nodes.
    depth: u16,
    /// Argument descriptor handles retained so the GC accounts for their shared subgraphs.
    args: Box<[VmValue]>,
}

impl TypeDescObj {
    pub fn constant(desc: Arc<TypeDesc>) -> TypeDescObj {
        TypeDescObj { desc, instantiated: None }
    }

    fn depth(&self) -> usize {
        self.instantiated.as_ref().map_or(0, |b| b.depth as usize)
    }

    fn size(&self) -> usize {
        self.instantiated.as_ref().map_or(0, |b| std::mem::size_of::<InstantiatedDesc>() + b.bytes as usize + b.args.len() * VALUE_SIZE)
    }

    fn args(&self) -> &[VmValue] {
        self.instantiated.as_ref().map_or(&[], |b| &b.args)
    }
}

/// Maximum nesting depth of template-instantiated [`TypeDesc`] nodes.
pub const MAX_DESC_DEPTH: usize = 256;

/// Parameterized [`TypeDesc`] template instantiated at runtime by `Op::Describe`.
#[derive(Clone, Debug)]
pub struct DescTemplate {
    desc: Arc<TypeDesc>,
    params: Vec<u32>,
    /// Addresses of [`TypeDesc`] nodes in `desc` that contain a [`TypeDesc::Param`].
    spine: std::collections::HashSet<usize, FxBuild>,
    /// Maximum path length of nodes cloned during instantiation.
    depth: usize,
}

fn addr(d: &Arc<TypeDesc>) -> usize {
    Arc::as_ptr(d) as usize
}

impl DescTemplate {
    pub fn new(desc: Arc<TypeDesc>, params: Vec<u32>) -> DescTemplate {
        fn depth(d: &Arc<TypeDesc>, seen: &mut FxHashMap<usize, Option<usize>>) -> Option<usize> {
            if let Some(&n) = seen.get(&addr(d)) {
                return n;
            }
            let mut deepest = |ds: &mut dyn Iterator<Item = &Arc<TypeDesc>>| ds.filter_map(|x| depth(x, seen)).max().map(|n| n + 1);
            let n = match &**d {
                TypeDesc::Param(_) => Some(0),
                TypeDesc::List(e) | TypeDesc::Set(e, _) | TypeDesc::Option(e) => deepest(&mut std::iter::once(e)),
                TypeDesc::Map(k, v) => deepest(&mut [k, v].into_iter()),
                TypeDesc::Tuple(ts) => deepest(&mut ts.iter()),
                TypeDesc::Struct { fields, .. } => deepest(&mut fields.iter().map(|f| &f.1)),
                TypeDesc::Enum { variants, .. } => deepest(&mut variants.iter().flat_map(|v| &v.payload_types)),
                _ => None,
            };
            seen.insert(addr(d), n);
            n
        }
        let mut seen = FxHashMap::default();
        let depth = depth(&desc, &mut seen).unwrap_or(0);
        let spine = seen.into_iter().filter(|(_, n)| n.is_some()).map(|(a, _)| a).collect();
        DescTemplate { desc, params, spine, depth }
    }

    pub fn arity(&self) -> usize {
        self.params.len()
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn size(&self) -> usize {
        self.spine.len()
    }

    /// Substitutes `args` for `self.params`, retaining `handles` for GC tracing.
    pub(crate) fn instantiate(&self, args: &[&TypeDescObj], handles: Box<[VmValue]>) -> Result<TypeDescObj, crate::vm::Fail> {
        let depth = self.depth + args.iter().map(|a| a.depth()).max().unwrap_or(0);
        if depth > MAX_DESC_DEPTH {
            return Err(crate::vm::Fail::runtime_error("limit", "the description of a type argument is nested too deeply"));
        }
        if args.len() != self.params.len() {
            return Err(crate::vm::Fail::internal("a type description template has the wrong number of arguments"));
        }
        let mut b = TemplateInstantiator { t: self, args, memo: FxHashMap::default(), allocated_bytes: 0, missing: false };
        let desc = b.node(&self.desc);
        if b.missing {
            return Err(crate::vm::Fail::internal("a type description template has no argument for a type parameter"));
        }
        let instantiated =
            InstantiatedDesc { bytes: u32::try_from(b.allocated_bytes).unwrap_or(u32::MAX), depth: depth as u16, args: handles };
        Ok(TypeDescObj { desc, instantiated: Some(Box::new(instantiated)) })
    }
}

struct TemplateInstantiator<'a> {
    t: &'a DescTemplate,
    args: &'a [&'a TypeDescObj],
    memo: FxHashMap<usize, Arc<TypeDesc>>,
    allocated_bytes: usize,
    missing: bool,
}

impl TemplateInstantiator<'_> {
    fn arg(&mut self, p: u32) -> Arc<TypeDesc> {
        match self.t.params.iter().position(|&q| q == p) {
            Some(i) => self.args[i].desc.clone(),
            None => {
                self.missing = true;
                Arc::new(TypeDesc::Param(p))
            }
        }
    }

    /// Substitutes argument type names for [`crate::types::PARAM_PLACEHOLDER`] markers and truncates the result to [`MAX_TYPE_NAME_LEN`] bytes.
    fn name(&mut self, name: &str) -> String {
        let mut out = String::new();
        let mut parts = name.split(crate::types::PARAM_PLACEHOLDER);
        out.push_str(parts.next().unwrap_or_default());
        while let (Some(p), Some(text)) = (parts.next(), parts.next())
            && out.len() <= MAX_TYPE_NAME_LEN
        {
            if let Ok(p) = p.parse() {
                self.arg(p).write_name(&|_| String::new(), &mut out);
            }
            out.push_str(text);
        }
        truncate_type_name(&mut out);
        out.shrink_to_fit();
        self.allocated_bytes += out.capacity();
        out
    }

    fn node(&mut self, d: &Arc<TypeDesc>) -> Arc<TypeDesc> {
        if !self.t.spine.contains(&addr(d)) {
            return d.clone();
        }
        if let Some(x) = self.memo.get(&addr(d)) {
            return x.clone();
        }
        let new = match &**d {
            TypeDesc::Param(p) => return self.arg(*p),
            TypeDesc::List(e) => TypeDesc::List(self.node(e)),
            TypeDesc::Set(e, fill) => TypeDesc::Set(self.node(e), *fill),
            TypeDesc::Option(e) => TypeDesc::Option(self.node(e)),
            TypeDesc::Map(k, v) => TypeDesc::Map(self.node(k), self.node(v)),
            TypeDesc::Tuple(ts) => {
                self.allocated_bytes += ts.len() * std::mem::size_of::<Arc<TypeDesc>>();
                TypeDesc::Tuple(ts.iter().map(|t| self.node(t)).collect())
            }
            TypeDesc::Struct { id, def, name, fields, rust } => {
                let fields: Vec<FieldDesc> = fields.iter().map(|(n, t, d)| (n.clone(), self.node(t), d.clone())).collect();
                self.allocated_bytes += fields.iter().map(|f| f.0.capacity() + std::mem::size_of::<FieldDesc>()).sum::<usize>();
                TypeDesc::Struct { id: *id, def: *def, name: self.name(name), fields, rust: *rust }
            }
            TypeDesc::Enum { id, name, variants, rust } => {
                let variants: Vec<ReflectVariantDesc> = variants
                    .iter()
                    .map(|v| ReflectVariantDesc {
                        name: v.name.clone(),
                        runtime_id: v.runtime_id,
                        payload_types: v.payload_types.iter().map(|t| self.node(t)).collect(),
                        field_names: v.field_names.clone(),
                    })
                    .collect();
                self.allocated_bytes += variants
                    .iter()
                    .map(|v| {
                        let names = v.field_names.iter().flatten().map(|n| n.len() + std::mem::size_of::<String>()).sum::<usize>();
                        v.name.len()
                            + v.payload_types.len() * std::mem::size_of::<Arc<TypeDesc>>()
                            + names
                            + std::mem::size_of::<ReflectVariantDesc>()
                    })
                    .sum::<usize>();
                TypeDesc::Enum { id: *id, name: self.name(name), variants, rust: *rust }
            }
            other => other.clone(),
        };
        self.allocated_bytes += std::mem::size_of::<TypeDesc>() + 2 * std::mem::size_of::<usize>();
        let new = Arc::new(new);
        self.memo.insert(addr(d), new.clone());
        new
    }
}

impl Drop for TypeDesc {
    /// Iteratively drops uniquely owned child nodes so deeply nested descriptors do not overflow the call stack.
    fn drop(&mut self) {
        fn take(d: &mut TypeDesc, out: &mut Vec<Arc<TypeDesc>>) {
            static HOLE: std::sync::LazyLock<Arc<TypeDesc>> = std::sync::LazyLock::new(|| Arc::new(TypeDesc::Unit));
            let mut one = |e: &mut Arc<TypeDesc>| {
                if Arc::strong_count(e) == 1 {
                    out.push(std::mem::replace(e, HOLE.clone()));
                }
            };
            match d {
                TypeDesc::List(e) | TypeDesc::Set(e, _) | TypeDesc::Option(e) => one(e),
                TypeDesc::Map(k, v) => {
                    one(k);
                    one(v);
                }
                TypeDesc::Tuple(ts) => ts.iter_mut().for_each(one),
                TypeDesc::Struct { fields, .. } => fields.iter_mut().for_each(|f| one(&mut f.1)),
                TypeDesc::Enum { variants, .. } => variants.iter_mut().flat_map(|v| &mut v.payload_types).for_each(one),
                _ => {}
            }
        }
        let mut todo = Vec::new();
        take(self, &mut todo);
        while let Some(d) = todo.pop() {
            if let Some(mut d) = Arc::into_inner(d) {
                take(&mut d, &mut todo);
            }
        }
    }
}

impl std::fmt::Debug for TypeDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeDesc::Int => write!(f, "Int"),
            TypeDesc::Float => write!(f, "Float"),
            TypeDesc::Bool => write!(f, "Bool"),
            TypeDesc::Str => write!(f, "Str"),
            TypeDesc::Bytes => write!(f, "Bytes"),
            TypeDesc::Duration => write!(f, "Duration"),
            TypeDesc::Unit => write!(f, "Unit"),
            TypeDesc::List(e) => write!(f, "List({e:?})"),
            TypeDesc::Map(k, v) => write!(f, "Map({k:?}, {v:?})"),
            TypeDesc::Set(e, _) => write!(f, "Set({e:?})"),
            TypeDesc::Tuple(ts) => f.debug_tuple("Tuple").field(ts).finish(),
            TypeDesc::Option(e) => write!(f, "Option({e:?})"),
            TypeDesc::Struct { id, name, .. } => write!(f, "Struct({name} #{id})"),
            TypeDesc::Enum { id, name, .. } => write!(f, "Enum({name} #{id})"),
            TypeDesc::Range => write!(f, "Range"),
            TypeDesc::Value { name, .. } => write!(f, "{name}"),
            TypeDesc::Recur(id) => write!(f, "Recur(#{id})"),
            TypeDesc::Unsupported(name) => write!(f, "Unsupported({name})"),
            TypeDesc::Param(p) => write!(f, "Param({p})"),
        }
    }
}

/// The state of an [`Obj::IterFn`].
#[derive(Debug, Clone, Copy)]
pub enum IterSrc {
    /// The indices of list `list` (a heap handle) below `n` (its length
    /// when the iterator was made), stopping early if it shrinks; `i` is
    /// the next one.
    List { list: u32, i: i64, n: i64 },
    /// The integers from `i` to `last`, inclusive.
    Range { i: i64, last: i64 },
    /// The characters of string `s` (a heap handle) from byte offset `i`.
    Chars { s: u32, i: i64 },
    /// The (key, value) entries of map `map` (a heap handle: the map
    /// itself, pinned, or a copy of its entries) from slot `slot` on.
    Map { map: u32, slot: usize },
    /// The elements of set `set` (as for `Map`) from slot `slot` on.
    Set { set: u32, slot: usize },
    /// An exhausted map or set iterator.
    Done,
}

pub enum Obj {
    Free(u32),
    /// Strings of up to 24 bytes are stored inline: making and freeing them
    /// allocates nothing.
    Str(CompactString),
    /// The growable buffer of a string local being appended to (see
    /// `Op::AppendStr`); only that local's register ever holds it, and it is
    /// turned into a `Str` before the value can be read anywhere else.
    StrBuf(String),
    Bytes(Box<[u8]>),
    List(Vec<VmValue>),
    Map(Box<MapObj>),
    Set(Box<SetObj>),
    Tuple(Box<[VmValue]>),
    Struct(u32, Box<[VmValue]>),
    Variant(u32, Box<[VmValue]>),
    Closure(u32, Box<[VmValue]>),
    NativeFn(u32),
    /// The `next` function of the iterator of a list, range, string, map
    /// or set: a function value (`fn() -> T?`) that the interpreter steps
    /// directly.
    IterFn(IterSrc),
    Cell(VmValue),
    Range(i64, i64, bool),
    Task(Box<TaskObj>),
    Channel(Box<ChannelObj>),
    Native(Box<dyn crate::native::DynObject>),
    Value(Box<dyn ValueObj>),
    Packed(&'static PackedType, [u64; 2]),
    TypeDesc(TypeDescObj),
}

impl std::fmt::Debug for Obj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Obj::Free(_) => write!(f, "Free"),
            Obj::Str(s) => write!(f, "Str({s:?})"),
            Obj::StrBuf(s) => write!(f, "StrBuf({s:?})"),
            Obj::Bytes(b) => write!(f, "Bytes({b:?})"),
            Obj::List(l) => write!(f, "List({l:?})"),
            Obj::Map(_) => write!(f, "Map"),
            Obj::Set(_) => write!(f, "Set"),
            Obj::Tuple(t) => write!(f, "Tuple({t:?})"),
            Obj::Struct(d, v) => write!(f, "Struct({d}, {v:?})"),
            Obj::Variant(d, v) => write!(f, "Variant({d}, {v:?})"),
            Obj::Closure(p, v) => write!(f, "Closure({p}, {v:?})"),
            Obj::NativeFn(n) => write!(f, "NativeFn({n})"),
            Obj::IterFn(it) => write!(f, "IterFn({it:?})"),
            Obj::Cell(v) => write!(f, "Cell({v:?})"),
            Obj::Range(a, b, i) => write!(f, "Range({a}, {b}, {i})"),
            Obj::Task(t) => write!(f, "Task({t:?})"),
            Obj::Channel(_) => write!(f, "Channel"),
            Obj::Native(n) => write!(f, "Native({})", n.type_name()),
            Obj::Value(v) => write!(f, "Value({})", v.type_name()),
            Obj::Packed(t, _) => write!(f, "Packed({})", t.name),
            Obj::TypeDesc(t) => write!(f, "TypeDesc({:?})", t.desc),
        }
    }
}

/// The size of a value in bytes.
pub const VALUE_SIZE: usize = std::mem::size_of::<VmValue>();

const _: () = assert!(std::mem::size_of::<Obj>() == 32);

/// Size of a map entry in bytes.
pub const MAP_ENTRY_SIZE: usize = std::mem::size_of::<Option<(VmValue, VmValue)>>();
/// Size of a set entry in bytes.
pub const SET_ENTRY_SIZE: usize = std::mem::size_of::<Option<VmValue>>();

impl Obj {
    /// Approximate memory footprint in bytes.
    pub fn size(&self) -> usize {
        let base = std::mem::size_of::<Obj>();
        base + match self {
            Obj::Free(_) | Obj::NativeFn(_) | Obj::Cell(_) | Obj::Range(..) | Obj::Packed(..) => 0,
            Obj::Str(s) if s.is_heap_allocated() => s.capacity(),
            Obj::Str(_) => 0,
            Obj::StrBuf(s) => s.capacity(),
            Obj::Bytes(b) => b.len(),
            Obj::List(l) => l.capacity() * VALUE_SIZE,
            Obj::Map(m) => m.entries.capacity() * MAP_ENTRY_SIZE + m.index_bytes() + m.pins.bytes(),
            Obj::Set(s) => s.entries.capacity() * SET_ENTRY_SIZE + s.index.bytes() + s.pins.bytes(),
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t.len() * VALUE_SIZE,
            Obj::IterFn(_) => 0,
            Obj::Task(_) => 32,
            Obj::Channel(c) => {
                64 + c.queue.capacity() * VALUE_SIZE
                    + c.receivers.capacity() * std::mem::size_of::<(u32, u64)>()
                    + c.senders.capacity() * std::mem::size_of::<(u32, u64, VmValue)>()
            }
            Obj::Native(n) => n.size(),
            Obj::Value(v) => v.size(),
            Obj::TypeDesc(t) => 64 + t.size(),
        }
    }

    /// Calls `f` on each value referenced by this object.
    #[inline]
    pub fn children(&self, mut f: impl FnMut(VmValue)) {
        match self {
            Obj::List(l) => l.iter().for_each(|v| f(*v)),
            Obj::Map(m) => {
                for (k, v) in m.entries.iter().flatten() {
                    f(*k);
                    f(*v);
                }
            }
            Obj::Set(s) => s.entries.iter().flatten().for_each(|v| f(*v)),
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t.iter().for_each(|v| f(*v)),
            Obj::Cell(v) => f(*v),
            Obj::IterFn(it) => match *it {
                IterSrc::List { list: h, .. } | IterSrc::Chars { s: h, .. } | IterSrc::Map { map: h, .. } | IterSrc::Set { set: h, .. } => {
                    f(VmValue::Obj(h))
                }
                IterSrc::Range { .. } | IterSrc::Done => {}
            },
            Obj::Task(t) => f(t.result),
            Obj::TypeDesc(d) => d.args().iter().for_each(|v| f(*v)),
            Obj::Channel(c) => {
                c.queue.iter().for_each(|v| f(*v));
                c.senders.iter().for_each(|(_, _, v)| f(*v));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_keys_selects_dense_or_preallocated_hashed_index() {
        let strs = (0..100).map(VmValue::Obj);
        let KeyIndex::Hashed(t) = KeyIndex::for_keys(100, strs) else { panic!("string keys need a hashed index") };
        assert!(t.capacity() >= 100, "{}", t.capacity());
        let KeyIndex::Hashed(t) = KeyIndex::for_keys(3, [0, 1, 1 << 40].map(VmValue::Int).into_iter()) else {
            panic!("sparse int keys need a hashed index")
        };
        assert!(t.capacity() >= 3, "{}", t.capacity());
        assert!(matches!(KeyIndex::for_keys(3, [2, 0, 1].map(VmValue::Int).into_iter()), KeyIndex::Dense { .. }));
        assert!(matches!(KeyIndex::for_keys(0, std::iter::empty()), KeyIndex::Dense { .. }));
    }

    #[test]
    fn deep_descriptions_drop_on_a_small_stack() {
        let shared = Arc::new(TypeDesc::Int);
        let mut d = Arc::new(TypeDesc::Unit);
        for i in 0..200_000 {
            d = Arc::new(match i % 4 {
                0 => TypeDesc::List(d),
                1 => TypeDesc::Map(shared.clone(), d),
                2 => TypeDesc::Tuple(vec![shared.clone(), d]),
                _ => TypeDesc::Struct { id: 0, def: 0, name: "S".into(), fields: vec![("f".into(), d, FieldDefault::None)], rust: None },
            });
        }
        std::thread::Builder::new().stack_size(64 << 10).spawn(move || drop(d)).unwrap().join().unwrap();
        assert_eq!(Arc::strong_count(&shared), 1);
    }

    #[test]
    fn instances_of_a_description_template_share_its_constant_defaults() {
        let table = Arc::new(crate::types::ConstVal::List(vec![crate::types::ConstVal::Int(0); 1000]));
        let fields = vec![
            ("v".to_string(), Arc::new(TypeDesc::Param(0)), FieldDefault::None),
            ("table".to_string(), Arc::new(TypeDesc::List(Arc::new(TypeDesc::Int))), FieldDefault::Const(table.clone())),
        ];
        let desc = Arc::new(TypeDesc::Struct { id: 0, def: 0, name: "Big".into(), fields, rust: None });
        let template = DescTemplate::new(desc, vec![0]);
        let arg = TypeDescObj::constant(Arc::new(TypeDesc::Str));
        let instance = template.instantiate(&[&arg], Box::default()).unwrap();
        let TypeDesc::Struct { fields, .. } = &*instance.desc else { panic!("{:?}", instance.desc) };
        assert!(matches!(&*fields[0].1, TypeDesc::Str));
        assert!(matches!(&fields[1].2, FieldDefault::Const(c) if Arc::ptr_eq(c, &table)));
        assert!(instance.size() < 1000, "{}", instance.size());
    }

    #[test]
    fn saturating_conversions_never_wrap() {
        // On a 32-bit target `usize` is `u32`: oversized counts must
        // saturate, not wrap to a small value (`1 << 32` would become 0).
        assert_eq!(saturate::<i64, u32>(1 << 32, u32::MAX), u32::MAX);
        assert_eq!(saturate::<i64, u32>(5_000_000_001, u32::MAX), u32::MAX);
        assert_eq!(saturate::<i64, u32>(i64::MAX, u32::MAX), u32::MAX);
        assert_eq!(saturate::<i64, u32>(u32::MAX as i64, u32::MAX), u32::MAX);
        assert_eq!(saturate::<i64, u32>(7, u32::MAX), 7);
        assert_eq!(saturate::<u64, u32>(1 << 32, u32::MAX), u32::MAX);
        assert_eq!(saturate::<u64, u32>(u64::MAX, u32::MAX), u32::MAX);
        assert_eq!(saturate::<u64, u32>(12, u32::MAX), 12);
        assert_eq!(u32::try_from(1i64 << 32).ok(), None, "an index past u32::MAX is out of bounds, not index 0");

        assert_eq!(to_usize_saturating(-5), 0);
        assert_eq!(to_usize_saturating(0), 0);
        assert_eq!(to_usize_saturating(42), 42);
        assert_eq!(to_usize_saturating(i64::MIN), 0);
        assert_eq!(to_index(-1), None);
        assert_eq!(to_index(3), Some(3));
        assert_eq!(u64_to_usize_saturating(9), 9);
        // Values past `u32::MAX` keep their magnitude or saturate, whatever the target.
        let big = 5_000_000_000i64;
        assert!(to_usize_saturating(big) as u128 >= u32::MAX as u128);
        assert!(to_index(big).is_none_or(|i| i as u64 == big as u64));
        assert!(u64_to_usize_saturating(u64::MAX) == usize::MAX);
        assert_eq!(to_usize_saturating(i64::MAX) as u128, (i64::MAX as u128).min(usize::MAX as u128));
    }
}
