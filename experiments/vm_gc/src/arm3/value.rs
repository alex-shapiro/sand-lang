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
        len.saturating_add(ARC_COUNTS + size_of::<LongStr>())
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

/// Where a container lives: its cycle-table slot and its engine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub slot: u32,
    /// The engine whose cycle table holds the slot.
    pub engine: u32,
}

/// The values inside an object, for the cycle collector and for drops.
pub trait Contents: Send + 'static {
    /// Gives `f` mutable access to every value that the object holds.
    fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value));
}

/// The values inside an immutable object, which the collector reads without a lock.
pub trait FrozenContents: Contents + Sync {
    fn each(&self, f: &mut dyn FnMut(&Value));
}

/// A container that the cycle collector tracks.
pub trait Container: Send + Sync {
    fn header(&self) -> Header;
    /// Visits the values that the container holds. Returns `false` if native
    /// code holds the container's lock.
    fn visit(&self, f: &mut dyn FnMut(&Value)) -> bool;
    /// Moves the values out of a mutable container into `out`, to break a cycle.
    fn clear(&self, out: &mut Vec<Value>);
}

/// An immutable object and its header.
pub struct Frozen<T> {
    pub header: Header,
    pub data: T,
}

/// A mutable object and its header.
pub struct Shared<T> {
    pub header: Header,
    data: Mutex<T>,
}

impl<T> Shared<T> {
    pub fn new(header: Header, data: T) -> Shared<T> {
        Shared { header, data: Mutex::new(data) }
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

impl<T: FrozenContents> Container for Frozen<T> {
    fn header(&self) -> Header {
        self.header
    }
    fn visit(&self, f: &mut dyn FnMut(&Value)) -> bool {
        self.data.each(f);
        true
    }
    fn clear(&self, _out: &mut Vec<Value>) {}
}

impl<T: Contents> Container for Shared<T> {
    fn header(&self) -> Header {
        self.header
    }
    fn visit(&self, f: &mut dyn FnMut(&Value)) -> bool {
        let Some(mut data) = self.try_lock() else { return false };
        data.each_mut(&mut |v| f(v));
        true
    }
    fn clear(&self, out: &mut Vec<Value>) {
        if let Some(mut data) = self.try_lock() {
            data.each_mut(&mut |v| out.push(std::mem::take(v)));
        }
    }
}

/// A Rust object stored in the engine.
pub trait HostObject: Any + Send {
    /// Gives `f` mutable access to every value that the object holds. The
    /// collector reads them, breaks cycles through them, and drops them.
    fn values_mut(&mut self, f: &mut dyn FnMut(&mut Value));

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

/// A host object, which drops its values without recursion.
pub struct HostCell(pub Box<dyn HostObject>);

macro_rules! fields_contents {
    ($ty:ty, $field:ident) => {
        impl Contents for $ty {
            fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
                self.$field.iter_mut().for_each(f);
            }
        }
        impl FrozenContents for $ty {
            fn each(&self, f: &mut dyn FnMut(&Value)) {
                self.$field.iter().for_each(f);
            }
        }
        impl Drop for $ty {
            fn drop(&mut self) {
                if self.$field.iter().any(|v| v.header().is_some()) {
                    drop_values(std::mem::take(&mut self.$field).into_vec());
                }
            }
        }
    };
}

fields_contents!(StructObj, fields);
fields_contents!(VariantObj, fields);
fields_contents!(ClosureObj, captures);

impl Contents for ListObj {
    fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        self.0.iter_mut().for_each(f);
    }
}

impl Contents for MapObj {
    fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        for (k, v) in &mut self.0 {
            f(k);
            f(v);
        }
    }
}

impl Contents for CellObj {
    fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        f(&mut self.0);
    }
}

impl Contents for HostCell {
    fn each_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        self.0.values_mut(f);
    }
}

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
    Host(Arc<Shared<HostCell>>),
    Encoder(Arc<Shared<EncoderObj>>),
    Decoder(Arc<Shared<DecoderObj>>),
}

const _: () = assert!(size_of::<Value>() == 16);

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

    /// Returns the container that the value refers to.
    #[inline]
    pub fn container(&self) -> Option<&dyn Container> {
        Some(match self {
            Value::Struct(o) => &**o,
            Value::MutStruct(o) => &**o,
            Value::Variant(o) => &**o,
            Value::Closure(o) => &**o,
            Value::List(o) => &**o,
            Value::Map(o) => &**o,
            Value::Cell(o) => &**o,
            Value::Host(o) => &**o,
            Value::Encoder(o) => &**o,
            Value::Decoder(o) => &**o,
            _ => return None,
        })
    }

    /// Returns the header of a container.
    #[inline]
    pub fn header(&self) -> Option<Header> {
        self.container().map(Container::header)
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
            _ => match (self.container(), other.container()) {
                (Some(a), Some(b)) => std::ptr::addr_eq(a, b),
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

/// Moves the containers that `data` holds onto `stack`.
fn take_contents(data: &mut dyn Contents, stack: &mut Vec<Value>) {
    data.each_mut(&mut |v| {
        // Strings drop in place without recursion; only containers can nest.
        if v.header().is_some() {
            stack.push(std::mem::take(v));
        }
    });
}

/// Drops `values` without recursion: each container whose last handle is
/// dropped moves its values onto the work stack instead of dropping them.
pub fn drop_values(mut stack: Vec<Value>) {
    macro_rules! frozen {
        ($o:expr) => {
            if let Some(mut o) = Arc::into_inner($o) {
                take_contents(&mut o.data, &mut stack);
            }
        };
    }
    macro_rules! shared {
        ($o:expr) => {
            if let Some(o) = Arc::into_inner($o) {
                take_contents(&mut o.into_inner(), &mut stack);
            }
        };
    }
    while let Some(v) = stack.pop() {
        match v {
            Value::Struct(o) => frozen!(o),
            Value::Variant(o) => frozen!(o),
            Value::Closure(o) => frozen!(o),
            Value::MutStruct(o) => shared!(o),
            Value::List(o) => shared!(o),
            Value::Map(o) => shared!(o),
            Value::Cell(o) => shared!(o),
            Value::Host(o) => shared!(o),
            other => drop(other),
        }
    }
}

impl Drop for ListObj {
    fn drop(&mut self) {
        if self.0.iter().any(|v| v.header().is_some()) {
            drop_values(std::mem::take(&mut self.0));
        }
    }
}

impl Drop for MapObj {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        take_contents(self, &mut stack);
        drop_values(stack);
    }
}

impl Drop for CellObj {
    fn drop(&mut self) {
        if self.0.header().is_some() {
            drop_values(vec![std::mem::take(&mut self.0)]);
        }
    }
}

impl Drop for HostCell {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        take_contents(self, &mut stack);
        drop_values(stack);
    }
}
