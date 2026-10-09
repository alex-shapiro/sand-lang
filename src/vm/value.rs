//! Runtime values and heap objects.

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
    *SEED.get_or_init(|| std::collections::hash_map::RandomState::new().hash_one(0x5eed_u64))
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

/// An owned, hashable copy of a key value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HKey {
    Unit,
    Bool(bool),
    Int(i64),
    Dur(i64),
    Str(Box<str>),
    Bytes(Box<[u8]>),
    Tag(u32),
    Range(i64, i64, bool),
    Tuple(Box<[HKey]>),
    Variant(u32, Box<[HKey]>),
    Struct(u32, Box<[HKey]>),
    /// Identity of an object (tasks, channels).
    Ident(u32),
}

/// Key index specialised for the common key types.
#[derive(Debug, Default)]
pub enum KeyIndex {
    #[default]
    Empty,
    Int(FxHashMap<i64, usize>),
    Str(FxHashMap<Box<str>, usize>),
    Other(FxHashMap<HKey, usize>),
}

/// An insertion-ordered hash map.
#[derive(Debug, Default)]
pub struct MapObj {
    pub index: KeyIndex,
    pub entries: Vec<Option<(Value, Value)>>,
    pub live: usize,
    /// Bytes owned by the copies of the keys in `index` (strings, nested
    /// key nodes), besides a fixed per-entry cost.
    pub key_bytes: usize,
}

/// An insertion-ordered hash set.
#[derive(Debug, Default)]
pub struct SetObj {
    pub index: KeyIndex,
    pub entries: Vec<Option<Value>>,
    pub live: usize,
    /// See [`MapObj::key_bytes`].
    pub key_bytes: usize,
}

/// Key lookup argument: borrowed for the fast paths.
pub enum KeyRef<'a> {
    Int(i64),
    Str(&'a str),
    Other(HKey),
}

impl KeyIndex {
    pub fn get(&self, k: &KeyRef) -> Option<usize> {
        match (self, k) {
            (KeyIndex::Empty, _) => None,
            (KeyIndex::Int(m), KeyRef::Int(i)) => m.get(i).copied(),
            (KeyIndex::Str(m), KeyRef::Str(s)) => m.get(*s).copied(),
            (KeyIndex::Other(m), KeyRef::Other(h)) => m.get(h).copied(),
            (KeyIndex::Other(m), KeyRef::Int(i)) => m.get(&HKey::Int(*i)).copied(),
            (KeyIndex::Other(m), KeyRef::Str(s)) => m.get(&HKey::Str((*s).into())).copied(),
            _ => None,
        }
    }

    pub fn insert(&mut self, k: KeyRef, slot: usize) {
        if let KeyIndex::Empty = self {
            *self = match &k {
                KeyRef::Int(_) => KeyIndex::Int(FxHashMap::default()),
                KeyRef::Str(_) => KeyIndex::Str(FxHashMap::default()),
                KeyRef::Other(_) => KeyIndex::Other(FxHashMap::default()),
            };
        }
        match (self, k) {
            (KeyIndex::Int(m), KeyRef::Int(i)) => {
                m.insert(i, slot);
            }
            (KeyIndex::Str(m), KeyRef::Str(s)) => {
                m.insert(s.into(), slot);
            }
            (KeyIndex::Other(m), KeyRef::Other(h)) => {
                m.insert(h, slot);
            }
            (KeyIndex::Other(m), KeyRef::Int(i)) => {
                m.insert(HKey::Int(i), slot);
            }
            (KeyIndex::Other(m), KeyRef::Str(s)) => {
                m.insert(HKey::Str(s.into()), slot);
            }
            _ => {}
        }
    }

    pub fn remove(&mut self, k: &KeyRef) -> Option<usize> {
        match (self, k) {
            (KeyIndex::Int(m), KeyRef::Int(i)) => m.remove(i),
            (KeyIndex::Str(m), KeyRef::Str(s)) => m.remove(*s),
            (KeyIndex::Other(m), KeyRef::Other(h)) => m.remove(h),
            (KeyIndex::Other(m), KeyRef::Int(i)) => m.remove(&HKey::Int(*i)),
            (KeyIndex::Other(m), KeyRef::Str(s)) => m.remove(&HKey::Str((*s).into())),
            _ => None,
        }
    }

    pub fn clear(&mut self) {
        *self = KeyIndex::Empty;
    }

    pub fn len(&self) -> usize {
        match self {
            KeyIndex::Empty => 0,
            KeyIndex::Int(m) => m.len(),
            KeyIndex::Str(m) => m.len(),
            KeyIndex::Other(m) => m.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rebuilds slot numbers after compaction.
    pub fn renumber(&mut self, f: impl Fn(usize) -> usize) {
        match self {
            KeyIndex::Empty => {}
            KeyIndex::Int(m) => m.values_mut().for_each(|v| *v = f(*v)),
            KeyIndex::Str(m) => m.values_mut().for_each(|v| *v = f(*v)),
            KeyIndex::Other(m) => m.values_mut().for_each(|v| *v = f(*v)),
        }
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
    /// The `next` function of the iterator of a list, range or string: a
    /// function value (`fn() -> T?`) that the interpreter steps directly.
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

/// Fixed cost of an entry of a map's or set's key index (the key's own
/// allocations are counted in `key_bytes`).
pub const INDEX_ENTRY_SIZE: usize = 48;

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
            Obj::Map(m) => m.entries.capacity() * (2 * VALUE_SIZE + 8) + m.index.len() * INDEX_ENTRY_SIZE + m.key_bytes,
            Obj::Set(s) => s.entries.capacity() * (VALUE_SIZE + 8) + s.index.len() * INDEX_ENTRY_SIZE + s.key_bytes,
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
                IterSrc::List { list: h, .. } | IterSrc::Chars { s: h, .. } => f(Value::Obj(h)),
                IterSrc::Range { .. } => {}
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
