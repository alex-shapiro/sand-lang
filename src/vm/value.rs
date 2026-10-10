//! Runtime values and heap objects.

use hashbrown::HashTable;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, OnceLock};

/// A runtime value. Immediate values are stored inline; everything else is a
/// handle into the engine's heap.
///
/// Enum variants with exactly one payload are stored inline (`VUnit` ..
/// `VObj`: runtime variant id, then the payload) whenever the payload is not
/// itself an inline variant; e.g. `Some(1)` is `VInt(some, 1)`, `Ok("x")` is
/// `VObj(ok, handle)` and `Some(None)` is `VTag(some, none)`, while
/// `Some(Some(1))` is a heap `Obj::Variant`. This representation is
/// canonical: always build variants with `Value::variant1`/`Heap::new_variant`
/// (or `Vm::some`/`ok`/`err`), and inspect them with `Heap::variant`,
/// `Heap::variant_id` and `Heap::field`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
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
}

// The inline variants pack the discriminant, the u32 variant id and an
// 8-byte payload into the same 16 bytes as the other values.
const _: () = assert!(std::mem::size_of::<Value>() == 16);

impl Value {
    /// The single-payload variant `id(payload)` stored inline, or `None` if
    /// the payload cannot be stored inline (it is itself an inline variant)
    /// and the variant must be heap-allocated.
    #[inline]
    pub fn variant1(id: u32, payload: Value) -> Option<Value> {
        Some(match payload {
            Value::Unit => Value::VUnit(id),
            Value::Bool(b) => Value::VBool(id, b),
            Value::Int(i) => Value::VInt(id, i),
            Value::Float(f) => Value::VFloat(id, f),
            Value::Dur(d) => Value::VDur(id, d),
            Value::Tag(t) => Value::VTag(id, t),
            Value::Obj(h) => Value::VObj(id, h),
            _ => return None,
        })
    }

    /// For an inline variant: its runtime variant id and payload.
    #[inline]
    pub fn as_inline_variant(self) -> Option<(u32, Value)> {
        Some(match self {
            Value::VUnit(id) => (id, Value::Unit),
            Value::VBool(id, b) => (id, Value::Bool(b)),
            Value::VInt(id, i) => (id, Value::Int(i)),
            Value::VFloat(id, f) => (id, Value::Float(f)),
            Value::VDur(id, d) => (id, Value::Dur(d)),
            Value::VTag(id, t) => (id, Value::Tag(t)),
            Value::VObj(id, h) => (id, Value::Obj(h)),
            _ => return None,
        })
    }

    /// The heap object this value refers to directly (an `Obj` handle or the
    /// payload of a `VObj`), for the garbage collector.
    #[inline]
    pub fn handle(self) -> Option<u32> {
        match self {
            Value::Obj(h) | Value::VObj(_, h) => Some(h),
            _ => None,
        }
    }

    #[inline]
    pub fn as_int(self) -> i64 {
        match self {
            Value::Int(i) => i,
            Value::Dur(d) => d,
            _ => 0,
        }
    }
    #[inline]
    pub fn as_float(self) -> f64 {
        match self {
            Value::Float(f) => f,
            Value::Int(i) => i as f64,
            _ => 0.0,
        }
    }
    #[inline]
    pub fn as_bool(self) -> bool {
        matches!(self, Value::Bool(true))
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
/// for `usize` (only possible on 32-bit targets), when it is out of bounds
/// of every sequence.
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

/// A view of an enum variant value, independent of its representation
/// (`Value::Tag`, an inline variant or a heap `Obj::Variant`).
#[derive(Clone, Copy, Debug)]
pub enum VariantRef<'a> {
    /// A payload-less variant or an inline single-payload variant.
    Inline(u32, Option<Value>),
    Heap(u32, &'a [Value]),
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
    pub fn fields(&self) -> &[Value] {
        match self {
            VariantRef::Inline(_, p) => p.as_slice(),
            VariantRef::Heap(_, f) => f,
        }
    }
}

/// The fast hasher of map and set keys. FxHash, but each 8-byte word is mixed
/// in with a folded multiply (the two halves of a 128-bit product XORed, as in
/// foldhash), starting from a random per-process seed.
///
/// Scripts choose map and set keys, so they must not be able to choose keys
/// that collide: in a table full of collisions every operation, charged as
/// a unit or so, scans all the colliding keys. Plain FxHash failed both
/// ways: the low bits of its product, which pick the bucket, depend only on
/// the low bits of the key (so all multiples of 2^32 collided), and any set
/// of colliding keys could be computed in advance. The folded multiply
/// makes every bit of the key affect the low bits, and the seed, which
/// scripts cannot observe (no iteration order depends on hashes), keeps
/// collisions from being computed in advance.
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
        if !rem.is_empty() {
            let w = rem.iter().rev().fold(0u64, |w, &b| (w << 8) | b as u64);
            self.add(w);
        }
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
        // A table of n buckets allocates n entries and n + 16 control bytes.
        match self.table.capacity() {
            0 => 0,
            cap => (cap * 8 / 7).next_power_of_two() * TABLE_BUCKET_SIZE + 16,
        }
    }
}

/// Removes the tombstones of `entries` if they are more than half of them,
/// renumbering `table`.
pub fn compact_entries<T: Copy>(entries: &mut Vec<Option<T>>, live: usize, table: &mut KeyTable) {
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
    pub index: KeyTable,
    pub entries: Vec<Option<(Value, Value)>>,
    pub live: usize,
    pub pins: Pins,
}

/// An insertion-ordered hash set (see [`MapObj`]).
#[derive(Debug, Default, Clone)]
pub struct SetObj {
    pub index: KeyTable,
    pub entries: Vec<Option<Value>>,
    pub live: usize,
    pub pins: Pins,
}

/// The iterators that may be reading a map or set in place: handles of
/// `Obj::IterFn`s, held weakly (not traced). A handle is stale once its
/// iterator is exhausted, has moved to a copy, or was collected (and its
/// slot maybe reused): `Vm::unpin` checks each one is still an iterator
/// reading this object in place. A copy of the map or set has no pins.
#[derive(Debug, Default)]
pub struct Pins {
    pub its: Vec<u32>,
    /// Stale handles are dropped when `its` reaches this length.
    pub prune_at: usize,
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
}

impl SetObj {
    pub fn clear(&mut self) {
        *self = SetObj::default();
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
    pub result: Value,
}

#[derive(Debug, Default)]
pub struct ChannelObj {
    pub queue: VecDeque<Value>,
    pub capacity: usize,
    pub closed: bool,
    /// Fibers blocked in `recv`: (fiber, wait generation).
    pub receivers: VecDeque<(u32, u64)>,
    /// Stale `receivers` (of cancelled tasks) are pruned when it reaches this length.
    pub receivers_prune_at: usize,
    /// Fibers blocked in `send`, with their pending values.
    pub senders: VecDeque<(u32, u64, Value)>,
}

/// An object owned by a native module (sockets, compiled regexes, ...).
pub trait NativeObject: Any + Send {
    fn type_name(&self) -> &'static str;
    /// Approximate memory used, for accounting.
    fn size(&self) -> usize {
        64
    }
    /// Whether this object counts against the open-resource limit.
    fn is_resource(&self) -> bool {
        false
    }
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// A struct field: name, type and constant default.
pub type FieldDesc = (String, Arc<TypeDesc>, Option<crate::types::ConstVal>);

/// An enum variant: name, runtime id, payload types and payload field names.
pub type VariantDesc = (String, u32, Vec<Arc<TypeDesc>>, Option<Vec<String>>);

/// A runtime description of a type, for `reflect` type parameters (built by
/// `compile::reflect`). Descriptions are DAGs: the description of a struct
/// or enum instance is shared by every place that uses it, so walking one as
/// a tree can take exponential time. `Debug` shows structs and enums by name.
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
    Set(Arc<TypeDesc>),
    Tuple(Vec<Arc<TypeDesc>>),
    Option(Arc<TypeDesc>),
    Struct {
        /// Identifies the struct instance (definition and type arguments)
        /// for `Recur`.
        id: u32,
        def: u32,
        name: String,
        fields: Vec<FieldDesc>,
    },
    Enum {
        id: u32,
        name: String,
        variants: Vec<VariantDesc>,
    },
    Json,
    /// Recursive reference to the enclosing struct or enum with this `id`.
    Recur(u32),
    Unsupported(String),
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
            TypeDesc::Set(e) => write!(f, "Set({e:?})"),
            TypeDesc::Tuple(ts) => f.debug_tuple("Tuple").field(ts).finish(),
            TypeDesc::Option(e) => write!(f, "Option({e:?})"),
            TypeDesc::Struct { id, name, .. } => write!(f, "Struct({name} #{id})"),
            TypeDesc::Enum { id, name, .. } => write!(f, "Enum({name} #{id})"),
            TypeDesc::Json => write!(f, "Json"),
            TypeDesc::Recur(id) => write!(f, "Recur(#{id})"),
            TypeDesc::Unsupported(name) => write!(f, "Unsupported({name})"),
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
    /// The integers from `i` below `end`.
    Range { i: i64, end: i64 },
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
    Str(Box<str>),
    /// The growable buffer of a string local being appended to (see
    /// `Op::AppendStr`); only that local's register ever holds it, and it is
    /// turned into a `Str` before the value can be read anywhere else.
    StrBuf(String),
    Bytes(Box<[u8]>),
    List(Vec<Value>),
    Map(Box<MapObj>),
    Set(Box<SetObj>),
    Tuple(Box<[Value]>),
    Struct(u32, Box<[Value]>),
    Variant(u32, Box<[Value]>),
    Closure(u32, Box<[Value]>),
    NativeFn(u32),
    /// The `next` function of the iterator of a list, range, string, map
    /// or set: a function value (`fn() -> T?`) that the interpreter steps
    /// directly.
    IterFn(IterSrc),
    Cell(Value),
    Range(i64, i64, bool),
    Task(Box<TaskObj>),
    Channel(Box<ChannelObj>),
    Native(Box<dyn NativeObject>),
    TypeDesc(Arc<TypeDesc>),
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
            Obj::TypeDesc(t) => write!(f, "TypeDesc({t:?})"),
        }
    }
}

const VALUE_SIZE: usize = std::mem::size_of::<Value>();

// Objects are kept small.
const _: () = assert!(std::mem::size_of::<Obj>() == 32);

/// Bytes of an entry of a map (`Option<(Value, Value)>`).
pub const MAP_ENTRY_SIZE: usize = std::mem::size_of::<Option<(Value, Value)>>();
/// Bytes of an entry of a set (`Option<Value>`).
pub const SET_ENTRY_SIZE: usize = std::mem::size_of::<Option<Value>>();

impl Obj {
    /// Approximate memory footprint in bytes.
    pub fn size(&self) -> usize {
        let base = std::mem::size_of::<Obj>();
        base + match self {
            Obj::Free(_) | Obj::NativeFn(_) | Obj::Cell(_) | Obj::Range(..) => 0,
            Obj::Str(s) => s.len(),
            Obj::StrBuf(s) => s.capacity(),
            Obj::Bytes(b) => b.len(),
            Obj::List(l) => l.capacity() * VALUE_SIZE,
            Obj::Map(m) => m.entries.capacity() * MAP_ENTRY_SIZE + m.index.bytes() + m.pins.bytes(),
            Obj::Set(s) => s.entries.capacity() * SET_ENTRY_SIZE + s.index.bytes() + s.pins.bytes(),
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t.len() * VALUE_SIZE,
            Obj::IterFn(_) => 0,
            Obj::Task(_) => 32,
            Obj::Channel(c) => {
                64 + c.queue.capacity() * VALUE_SIZE
                    + c.receivers.capacity() * std::mem::size_of::<(u32, u64)>()
                    + c.senders.capacity() * std::mem::size_of::<(u32, u64, Value)>()
            }
            Obj::Native(n) => n.size(),
            Obj::TypeDesc(_) => 64,
        }
    }

    /// Calls `f` on each value referenced by this object.
    #[inline]
    pub fn children(&self, mut f: impl FnMut(Value)) {
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
                    f(Value::Obj(h))
                }
                IterSrc::Range { .. } | IterSrc::Done => {}
            },
            Obj::Task(t) => f(t.result),
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
