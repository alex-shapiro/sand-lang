//! Runtime values and heap objects.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

/// A runtime value. Immediate values are stored inline; everything else is a
/// handle into the engine's heap.
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
    /// A global that has not been initialised yet.
    Uninit,
}

impl Value {
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

/// A fast, deterministic hasher (FxHash).
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(SEED);
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
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            self.add(u64::from_le_bytes(buf));
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

pub type FxBuild = BuildHasherDefault<FxHasher>;
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
}

/// An insertion-ordered hash set.
#[derive(Debug, Default)]
pub struct SetObj {
    pub index: KeyIndex,
    pub entries: Vec<Option<Value>>,
    pub live: usize,
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
pub type FieldDesc = (String, TypeDesc, Option<crate::types::ConstVal>);

/// An enum variant: name, runtime id, payload types and payload field names.
pub type VariantDesc = (String, u32, Vec<TypeDesc>, Option<Vec<String>>);

/// A runtime description of a type, for `reflect` type parameters.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeDesc {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Duration,
    Unit,
    List(Box<TypeDesc>),
    Map(Box<TypeDesc>, Box<TypeDesc>),
    Set(Box<TypeDesc>),
    Tuple(Vec<TypeDesc>),
    Option(Box<TypeDesc>),
    Struct {
        def: u32,
        name: String,
        fields: Vec<FieldDesc>,
    },
    Enum {
        name: String,
        variants: Vec<VariantDesc>,
    },
    Json,
    /// Recursive reference to an enclosing struct/enum by nesting depth.
    Recur(String),
    Unsupported(String),
}

pub enum Obj {
    Free(u32),
    Str(Box<str>),
    Bytes(Box<[u8]>),
    List(Vec<Value>),
    Map(Box<MapObj>),
    Set(Box<SetObj>),
    Tuple(Box<[Value]>),
    Struct(u32, Box<[Value]>),
    Variant(u32, Box<[Value]>),
    Closure(u32, Box<[Value]>),
    NativeFn(u32),
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
            Obj::Bytes(b) => write!(f, "Bytes({b:?})"),
            Obj::List(l) => write!(f, "List({l:?})"),
            Obj::Map(_) => write!(f, "Map"),
            Obj::Set(_) => write!(f, "Set"),
            Obj::Tuple(t) => write!(f, "Tuple({t:?})"),
            Obj::Struct(d, v) => write!(f, "Struct({d}, {v:?})"),
            Obj::Variant(d, v) => write!(f, "Variant({d}, {v:?})"),
            Obj::Closure(p, v) => write!(f, "Closure({p}, {v:?})"),
            Obj::NativeFn(n) => write!(f, "NativeFn({n})"),
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

impl Obj {
    /// Approximate memory footprint in bytes.
    pub fn size(&self) -> usize {
        let base = std::mem::size_of::<Obj>();
        base + match self {
            Obj::Free(_) | Obj::NativeFn(_) | Obj::Cell(_) | Obj::Range(..) => 0,
            Obj::Str(s) => s.len(),
            Obj::Bytes(b) => b.len(),
            Obj::List(l) => l.capacity() * VALUE_SIZE,
            Obj::Map(m) => m.entries.capacity() * (2 * VALUE_SIZE + 8) + m.index.len() * 48,
            Obj::Set(s) => s.entries.capacity() * (VALUE_SIZE + 8) + s.index.len() * 48,
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t.len() * VALUE_SIZE,
            Obj::Task(_) => 32,
            Obj::Channel(c) => 64 + c.queue.capacity() * VALUE_SIZE,
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
            Obj::Task(t) => f(t.result),
            Obj::Channel(c) => {
                c.queue.iter().for_each(|v| f(*v));
                c.senders.iter().for_each(|(_, _, v)| f(*v));
            }
            _ => {}
        }
    }
}
