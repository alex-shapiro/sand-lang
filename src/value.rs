//! Sand values in a Rust form that does not reference the VM.

use crate::stdlib::time::{PlainMonthDay, PlainYearMonth, Span, TimeZone, ZonedDateTime};
use crate::vm::bytecode::StructRt;
use crate::vm::encode::{self, Container, EncodeSink, EnumVariant, Leaf, MAX_DEPTH, OpaqueKind, Opened};
use crate::vm::heap::Heap;
use crate::vm::meter::{Meter, Scratch};
use crate::vm::value::{FxBuild, FxHashMap, ValueRef, VmValue};
use crate::vm::{Fail, Vm, fail, internal};
use std::collections::HashSet;
use std::mem::size_of;
use std::rc::Rc;
use std::sync::Arc;

/// The Rust form of a Sand value whose type implements `Encode`.
///
/// A `Value` does not reference the VM, so it remains valid after the run ends.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// The unit value `()`.
    Unit,
    /// A `bool`.
    Bool(bool),
    /// An `int`.
    Int(i64),
    /// A `float`.
    Float(f64),
    /// A `duration`.
    Duration(jiff::SignedDuration),
    /// A `str`.
    Str(String),
    /// A `bytes` value.
    Bytes(Vec<u8>),
    /// A list.
    List(Vec<Value>),
    /// A tuple.
    Tuple(Vec<Value>),
    /// A set, with its elements in iteration order.
    Set(Vec<Value>),
    /// A map, with its entries in iteration order.
    Map(Vec<(Value, Value)>),
    /// An integer range: `start..end`, or `start..=end` if `inclusive` is true.
    Range { start: i64, end: i64, inclusive: bool },
    /// A struct.
    Struct(Box<StructValue>),
    /// An enum variant.
    Variant(Box<VariantValue>),
    /// A `time.Instant`.
    Instant(jiff::Timestamp),
    /// A `time.PlainDateTime`.
    PlainDateTime(jiff::civil::DateTime),
    /// A `time.PlainDate`.
    PlainDate(jiff::civil::Date),
    /// A `time.PlainTime`.
    PlainTime(jiff::civil::Time),
    /// A `time.PlainYearMonth`.
    PlainYearMonth(PlainYearMonth),
    /// A `time.PlainMonthDay`.
    PlainMonthDay(PlainMonthDay),
    /// A `time.ZonedDateTime`.
    ZonedDateTime(Box<ZonedDateTime>),
    /// A `time.Span`.
    Span(Box<Span>),
    /// A `time.TimeZone`.
    TimeZone(TimeZone),
    /// A value of a native value type without its own variant, from the
    /// standard library or a host.
    Native(Box<NativeValue>),
    /// A value whose type does not implement `Encode`, such as a function, a
    /// task, a channel, or a native object. It holds only the type name.
    Opaque { type_name: Arc<str> },
}

/// A struct with its type name and fields.
#[derive(Clone, Debug, PartialEq)]
pub struct StructValue {
    /// The module-qualified type name, such as `shop.Order`.
    pub type_name: Arc<str>,
    /// The field names and values in declaration order.
    pub fields: Vec<(Arc<str>, Value)>,
}

/// An enum variant with its enum name, variant name, and payload.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantValue {
    /// The module-qualified enum name, such as `shop.Status`.
    pub type_name: Arc<str>,
    /// The variant name, such as `Shipped`.
    pub name: Arc<str>,
    /// The payload of the variant.
    pub payload: Payload,
}

/// The payload of an enum variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Positional fields in declaration order. A variant without a payload has
    /// no fields.
    Positional(Vec<Value>),
    /// Named fields in declaration order.
    Named(Vec<(Arc<str>, Value)>),
}

/// A value of a native value type with its type name and text.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeValue {
    /// The module-qualified type name, such as `net.SocketAddr`.
    pub type_name: Arc<str>,
    /// The `Display` text of the value.
    /// [`ValueType::parse`](crate::vm::value::ValueType::parse) turns it back
    /// into an equal value.
    pub text: String,
}

/// Copies `value` out of the heap of `vm`. See [`Vm::to_value`].
pub(crate) fn copy_from_vm(vm: &Vm, value: VmValue) -> Result<Value, Fail> {
    let mut sink = ValueSink {
        heap: &vm.heap,
        meter: &vm.meter,
        allocated: 0,
        scratch: Scratch::new(),
        depth: 0,
        type_names: HashSet::default(),
        structs: FxHashMap::default(),
        variants: FxHashMap::default(),
    };
    match encode::walk(&vm.prog, &vm.heap, &mut sink, OpenValue::Root(Value::Unit), value)? {
        OpenValue::Root(value) => Ok(value),
        _ => internal("to_value: the walk did not return the root"),
    }
}

/// Returns the name of a type for a reader outside its module: `module.Name`,
/// or `Name` for a prelude type, which no script can qualify.
fn qualified_name(module: &str, name: &str) -> String {
    if module == "prelude" { name.to_string() } else { format!("{module}.{name}") }
}

/// The [`EncodeSink`] that builds a [`Value`].
struct ValueSink<'h> {
    heap: &'h Heap,
    meter: &'h Meter,
    /// The bytes of Rust memory that the copy has allocated.
    allocated: usize,
    scratch: Scratch,
    depth: usize,
    type_names: HashSet<Arc<str>, FxBuild>,
    /// The names of each struct type that the walk has reported, by runtime
    /// type ID.
    structs: FxHashMap<u32, Rc<Names>>,
    /// The name of each variant that the walk has reported and the names of
    /// its enum and payload fields, by runtime variant ID.
    variants: FxHashMap<u32, (Arc<str>, Rc<Names>)>,
}

/// The names that all copies of the values of a struct type or of a variant
/// share.
struct Names {
    /// The module-qualified name of the struct type or of the enum.
    type_name: Arc<str>,
    fields: Vec<Arc<str>>,
}

/// A [`Value`] that waits for its children.
enum OpenValue {
    Root(Value),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    /// The entries of a map. If the flag is true, the last entry has its key
    /// and waits for its value.
    Map(Vec<(Value, Value)>, bool),
    Struct(Box<StructValue>, Rc<Names>),
    Variant(Box<VariantValue>, Rc<Names>),
}

impl ValueSink<'_> {
    /// Counts `bytes` of new Rust memory against the memory limit. Call this
    /// function before the allocation.
    fn reserve(&mut self, bytes: usize) -> Result<(), Fail> {
        self.allocated = self.allocated.saturating_add(bytes);
        self.scratch.check(self.heap, self.allocated)
    }

    /// Charges fuel and memory for a copy of `bytes` bytes.
    fn reserve_copy(&mut self, bytes: usize) -> Result<(), Fail> {
        self.meter.charge_bytes(bytes)?;
        self.reserve(bytes)
    }

    fn items(&mut self, len: usize) -> Result<Vec<Value>, Fail> {
        self.reserve(len.saturating_mul(size_of::<Value>()))?;
        Ok(Vec::with_capacity(len))
    }

    fn name(&mut self, text: &str) -> Result<Arc<str>, Fail> {
        self.reserve_copy(text.len() + 2 * size_of::<usize>())?;
        Ok(Arc::from(text))
    }

    fn type_name(&mut self, text: &str) -> Result<Arc<str>, Fail> {
        if let Some(name) = self.type_names.get(text) {
            return Ok(name.clone());
        }
        let name = self.name(text)?;
        self.type_names.insert(name.clone());
        Ok(name)
    }

    fn names(&mut self, ty: &StructRt, fields: &[String]) -> Result<Rc<Names>, Fail> {
        let type_name = self.type_name(&qualified_name(&ty.module, &ty.name))?;
        let fields = fields.iter().map(|field| self.name(field)).collect::<Result<_, _>>()?;
        Ok(Rc::new(Names { type_name, fields }))
    }

    fn struct_names(&mut self, def: u32, ty: &StructRt) -> Result<Rc<Names>, Fail> {
        if let Some(names) = self.structs.get(&def) {
            return Ok(names.clone());
        }
        let names = self.names(ty, &ty.fields)?;
        self.structs.insert(def, names.clone());
        Ok(names)
    }

    fn variant_names(&mut self, variant: &EnumVariant<'_>) -> Result<(Arc<str>, Rc<Names>), Fail> {
        if let Some(names) = self.variants.get(&variant.id) {
            return Ok(names.clone());
        }
        let names = (self.name(variant.name)?, self.names(variant.enum_type(), variant.field_names.unwrap_or_default())?);
        self.variants.insert(variant.id, names.clone());
        Ok(names)
    }

    /// Returns an empty variant value that waits for `len` payload values.
    fn variant(&mut self, variant: &EnumVariant<'_>, len: usize) -> Result<(Box<VariantValue>, Rc<Names>), Fail> {
        let (name, names) = self.variant_names(variant)?;
        let payload = match variant.field_names {
            Some(_) => {
                self.reserve(size_of::<VariantValue>() + len.saturating_mul(size_of::<(Arc<str>, Value)>()))?;
                Payload::Named(Vec::with_capacity(len))
            }
            None => {
                self.reserve(size_of::<VariantValue>())?;
                Payload::Positional(self.items(len)?)
            }
        };
        Ok((Box::new(VariantValue { type_name: names.type_name.clone(), name, payload }), names))
    }

    fn native(&mut self, native: ValueRef<'_>) -> Result<Value, Fail> {
        let Some(mut value) = native.to_value() else { return internal("to_value: a packed value without an unpacked form") };
        match &mut value {
            Value::Native(native) => {
                self.reserve_copy(size_of::<NativeValue>() + native.text.len())?;
                native.type_name = self.type_name(&native.type_name)?;
            }
            Value::ZonedDateTime(_) => self.reserve(size_of::<ZonedDateTime>())?,
            Value::Span(_) => self.reserve(size_of::<Span>())?,
            _ => {}
        }
        Ok(value)
    }

    fn opaque(&mut self, kind: OpaqueKind) -> Result<Value, Fail> {
        let type_name = match kind {
            OpaqueKind::Function => self.type_name("fn")?,
            OpaqueKind::Task => self.type_name("Task")?,
            OpaqueKind::Channel => self.type_name("Channel")?,
            OpaqueKind::Object { module, name } => self.type_name(&qualified_name(module, name))?,
            OpaqueKind::Internal => return internal("to_value: an internal heap object has no `Value` form"),
        };
        Ok(Value::Opaque { type_name })
    }
}

fn put(parent: &mut OpenValue, value: Value) {
    match parent {
        OpenValue::Root(root) => *root = value,
        OpenValue::List(items) | OpenValue::Tuple(items) | OpenValue::Set(items) => items.push(value),
        OpenValue::Map(entries, has_key) => {
            match entries.last_mut() {
                Some(entry) if *has_key => entry.1 = value,
                _ => entries.push((value, Value::Unit)),
            }
            *has_key = !*has_key;
        }
        OpenValue::Struct(s, names) => put_field(&mut s.fields, names, value),
        OpenValue::Variant(v, names) => match &mut v.payload {
            Payload::Positional(items) => items.push(value),
            Payload::Named(fields) => put_field(fields, names, value),
        },
    }
}

fn put_field(fields: &mut Vec<(Arc<str>, Value)>, names: &Names, value: Value) {
    if let Some(name) = names.fields.get(fields.len()) {
        fields.push((name.clone(), value));
    }
}

impl<'h> EncodeSink<'h> for ValueSink<'h> {
    type Open = OpenValue;

    #[inline(always)]
    fn leaf(&mut self, parent: &mut OpenValue, leaf: Leaf<'h>) -> Result<(), Fail> {
        self.meter.charge(1)?;
        let value = match leaf {
            Leaf::Unit => Value::Unit,
            Leaf::Bool(b) => Value::Bool(b),
            Leaf::Int(i) => Value::Int(i),
            Leaf::Float(f) => Value::Float(f),
            Leaf::Duration(nanos) => Value::Duration(jiff::SignedDuration::from_nanos(nanos)),
            Leaf::Str { text, .. } => {
                self.reserve_copy(text.len())?;
                Value::Str(text.to_string())
            }
            Leaf::Bytes(bytes) => {
                self.reserve_copy(bytes.len())?;
                Value::Bytes(bytes.to_vec())
            }
            Leaf::Range { start, end, inclusive } => Value::Range { start, end, inclusive },
            Leaf::Tag(variant) => Value::Variant(self.variant(&variant, 0)?.0),
            Leaf::Native(native) => self.native(native)?,
            Leaf::Opaque(kind) => self.opaque(kind)?,
        };
        put(parent, value);
        Ok(())
    }

    #[inline(always)]
    fn open(&mut self, _: &mut OpenValue, container: Container<'h>, len: usize) -> Result<Opened<OpenValue>, Fail> {
        self.meter.charge(1)?;
        if self.depth >= MAX_DEPTH {
            return fail("limit", "value too deeply nested to copy");
        }
        self.depth += 1;
        Ok(Opened::Children(match container {
            Container::List => OpenValue::List(self.items(len)?),
            Container::Tuple => OpenValue::Tuple(self.items(len)?),
            Container::Set => OpenValue::Set(self.items(len)?),
            Container::Map => {
                self.reserve(len.saturating_mul(size_of::<(Value, Value)>()))?;
                OpenValue::Map(Vec::with_capacity(len), false)
            }
            Container::Struct { def, ty } => {
                let names = self.struct_names(def, ty)?;
                self.reserve(size_of::<StructValue>() + len.saturating_mul(size_of::<(Arc<str>, Value)>()))?;
                OpenValue::Struct(Box::new(StructValue { type_name: names.type_name.clone(), fields: Vec::with_capacity(len) }), names)
            }
            Container::Variant(variant) => {
                let (value, names) = self.variant(&variant, len)?;
                OpenValue::Variant(value, names)
            }
        }))
    }

    #[inline(always)]
    fn close(&mut self, parent: &mut OpenValue, open: OpenValue) -> Result<(), Fail> {
        self.depth -= 1;
        let value = match open {
            OpenValue::Root(_) => return internal("to_value: the walk closed the root"),
            OpenValue::List(items) => Value::List(items),
            OpenValue::Tuple(items) => Value::Tuple(items),
            OpenValue::Set(items) => Value::Set(items),
            OpenValue::Map(entries, _) => Value::Map(entries),
            OpenValue::Struct(value, _) => Value::Struct(value),
            OpenValue::Variant(value, _) => Value::Variant(value),
        };
        put(parent, value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn value_is_at_most_32_bytes() {
        assert!(size_of::<Value>() <= 32, "Value is {} bytes", size_of::<Value>());
    }
}
