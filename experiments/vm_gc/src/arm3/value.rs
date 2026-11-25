//! Arm 3 values: immutable objects behind `Arc`, mutable objects behind
//! `Arc` and a `Mutex`, and long strings behind an `Arc` that returns its
//! bytes to the engine's tracker on drop.

use super::codec::{DecoderObj, EncoderObj};
use crate::shared::{Fail, Result, ShortStr, Tracker};
use parking_lot::{Mutex, MutexGuard};
use std::any::Any;
use std::mem::size_of;
use std::sync::Arc;

/// Bytes of the reference counts at the start of every `Arc` allocation.
pub const ARC_COUNTS: usize = 2 * size_of::<usize>();

/// A long string. Dropping the last handle returns its bytes to the tracker.
pub struct LongStr {
    pub text: Box<str>,
    tracker: Arc<Tracker>,
}

impl LongStr {
    /// Tracked bytes of a long string of `len` bytes.
    pub const fn size(len: usize) -> usize {
        ARC_COUNTS + size_of::<LongStr>() + len
    }

    /// Creates a string whose bytes the caller reserved on `tracker`.
    pub fn new(text: Box<str>, tracker: Arc<Tracker>) -> LongStr {
        LongStr { text, tracker }
    }
}

impl Drop for LongStr {
    fn drop(&mut self) {
        self.tracker.sub(LongStr::size(self.text.len()));
    }
}

/// An immutable object and its cycle-table slot.
pub struct Frozen<T> {
    pub slot: u32,
    pub data: T,
}

/// A mutable object and its cycle-table slot.
pub struct Shared<T> {
    pub slot: u32,
    data: Mutex<T>,
}

impl<T> Shared<T> {
    pub fn new(slot: u32, data: T) -> Shared<T> {
        Shared { slot, data: Mutex::new(data) }
    }

    /// Locks the object, or fails if this thread already holds the lock.
    #[inline]
    pub fn lock(&self) -> Result<MutexGuard<'_, T>> {
        self.data.try_lock().ok_or_else(|| Fail::error("value is in use by a native call"))
    }

    /// Locks the object if no one holds the lock.
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.data.try_lock()
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

/// A Rust object stored in the engine.
pub trait HostObject: Any + Send {
    /// Visits every value that the object holds.
    fn visit(&self, _visit: &mut dyn FnMut(&Value)) {}

    /// Moves every value that the object holds into `out`, to break a cycle.
    fn clear(&mut self, _out: &mut Vec<Value>) {}

    /// Bytes owned by the object, counted against the memory limit.
    fn size(&self) -> usize {
        0
    }
}

pub struct StructObj {
    pub ty: u16,
    pub fields: Box<[Value]>,
}

pub struct VariantObj {
    pub tag: u16,
    pub fields: Box<[Value]>,
}

pub struct ClosureObj {
    pub func: u16,
    pub captures: Box<[Value]>,
}

#[derive(Default)]
pub struct ListObj(pub Vec<Value>);

#[derive(Default)]
pub struct MapObj(pub Vec<(Value, Value)>);

pub struct CellObj(pub Value);

pub type HostBox = Box<dyn HostObject>;

/// An immediate value or a reference-counted object.
#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Unit,
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A payload-less enum variant.
    Tag(u16),
    Short(ShortStr),
    Str(Arc<LongStr>),
    Struct(Arc<Frozen<StructObj>>),
    /// A struct with `var` fields.
    MutStruct(Arc<Shared<StructObj>>),
    Variant(Arc<Frozen<VariantObj>>),
    Closure(Arc<Frozen<ClosureObj>>),
    List(Arc<Shared<ListObj>>),
    Map(Arc<Shared<MapObj>>),
    Cell(Arc<Shared<CellObj>>),
    Host(Arc<Shared<HostBox>>),
    Encoder(Arc<Shared<EncoderObj>>),
    Decoder(Arc<Shared<DecoderObj>>),
}

const _: () = assert!(size_of::<Value>() == 16);

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Unit => write!(f, "Unit"),
            Value::None => write!(f, "None"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Tag(t) => write!(f, "Tag({t})"),
            Value::Short(s) => write!(f, "{s:?}"),
            Value::Str(s) => write!(f, "{:?}", s.text),
            _ => write!(f, "object"),
        }
    }
}

impl Value {
    /// Returns whether the value owns no object.
    #[inline(always)]
    pub fn is_immediate(&self) -> bool {
        matches!(self, Value::Unit | Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Tag(_) | Value::Short(_))
    }

    /// Clones the value. Copies an immediate without a call to `clone`.
    #[inline(always)]
    pub fn dup(&self) -> Value {
        match self {
            Value::Unit => Value::Unit,
            Value::None => Value::None,
            Value::Bool(b) => Value::Bool(*b),
            Value::Int(i) => Value::Int(*i),
            Value::Float(f) => Value::Float(*f),
            Value::Tag(t) => Value::Tag(*t),
            Value::Short(s) => Value::Short(*s),
            other => other.clone(),
        }
    }

    /// Returns the text of a string value.
    #[inline]
    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Short(s) => Some(s.as_str()),
            Value::Str(s) => Some(&s.text),
            _ => None,
        }
    }

    /// Returns the cycle-table slot of a container.
    #[inline]
    pub fn slot(&self) -> Option<u32> {
        Some(match self {
            Value::Struct(o) => o.slot,
            Value::MutStruct(o) => o.slot,
            Value::Variant(o) => o.slot,
            Value::Closure(o) => o.slot,
            Value::List(o) => o.slot,
            Value::Map(o) => o.slot,
            Value::Cell(o) => o.slot,
            Value::Host(o) => o.slot,
            Value::Encoder(o) => o.slot,
            Value::Decoder(o) => o.slot,
            _ => return None,
        })
    }

    /// Returns whether two values are the same scalar, equal strings, or the same object.
    pub fn same(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) | (Value::None, Value::None) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Tag(a), Value::Tag(b)) => a == b,
            (Value::Short(a), Value::Short(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => Arc::ptr_eq(a, b) || a.text == b.text,
            _ => match (self.slot(), other.slot()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        }
    }
}

/// Stores `v` in `slot`. Skips the drop of an immediate old value.
#[inline(always)]
pub fn store(slot: &mut Value, v: Value) {
    if slot.is_immediate() {
        std::mem::forget(std::mem::replace(slot, v));
    } else {
        *slot = v;
    }
}

/// Moves `fields` onto `stack` if any field can own a container.
#[inline]
fn take_fields(fields: &mut Box<[Value]>, stack: &mut Vec<Value>) {
    if fields.iter().any(|v| v.slot().is_some()) {
        stack.extend(std::mem::take(fields).into_vec());
    }
}

/// Drops `values` without recursion: each container whose last handle is
/// dropped moves its children onto the work stack instead of dropping them.
pub fn drop_values(mut stack: Vec<Value>) {
    while let Some(v) = stack.pop() {
        match v {
            Value::Struct(o) => {
                if let Some(mut o) = Arc::into_inner(o) {
                    take_fields(&mut o.data.fields, &mut stack);
                }
            }
            Value::MutStruct(o) => {
                if let Some(o) = Arc::into_inner(o) {
                    take_fields(&mut o.into_inner().fields, &mut stack);
                }
            }
            Value::Variant(o) => {
                if let Some(mut o) = Arc::into_inner(o) {
                    take_fields(&mut o.data.fields, &mut stack);
                }
            }
            Value::Closure(o) => {
                if let Some(mut o) = Arc::into_inner(o) {
                    take_fields(&mut o.data.captures, &mut stack);
                }
            }
            Value::List(o) => {
                if let Some(o) = Arc::into_inner(o) {
                    stack.append(&mut o.into_inner().0);
                }
            }
            Value::Map(o) => {
                if let Some(o) = Arc::into_inner(o) {
                    for (k, v) in std::mem::take(&mut o.into_inner().0) {
                        stack.push(k);
                        stack.push(v);
                    }
                }
            }
            Value::Cell(o) => {
                if let Some(o) = Arc::into_inner(o) {
                    stack.push(std::mem::take(&mut o.into_inner().0));
                }
            }
            other => drop(other),
        }
    }
}

impl Drop for StructObj {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        take_fields(&mut self.fields, &mut stack);
        drop_values(stack);
    }
}

impl Drop for VariantObj {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        take_fields(&mut self.fields, &mut stack);
        drop_values(stack);
    }
}

impl Drop for ClosureObj {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        take_fields(&mut self.captures, &mut stack);
        drop_values(stack);
    }
}

impl Drop for ListObj {
    fn drop(&mut self) {
        if self.0.iter().any(|v| v.slot().is_some()) {
            drop_values(std::mem::take(&mut self.0));
        }
    }
}

impl Drop for MapObj {
    fn drop(&mut self) {
        if self.0.iter().any(|(k, v)| k.slot().is_some() || v.slot().is_some()) {
            let mut stack = Vec::with_capacity(self.0.len() * 2);
            for (k, v) in std::mem::take(&mut self.0) {
                stack.push(k);
                stack.push(v);
            }
            drop_values(stack);
        }
    }
}

impl Drop for CellObj {
    fn drop(&mut self) {
        if self.0.slot().is_some() {
            drop_values(vec![std::mem::take(&mut self.0)]);
        }
    }
}
