//! Sand values in a Rust form that does not reference the VM.

use crate::native::ty::Never;
use crate::stdlib::time::{PlainMonthDay, PlainYearMonth, Span, TimeZone, ZonedDateTime};
use crate::vm::bytecode::StructRt;
use crate::vm::encode::{self, Container, EncodeSink, EnumVariant, Leaf, OpaqueKind, Opened, Role};
use crate::vm::heap::Heap;
use crate::vm::meter::{Meter, Scratch};
use crate::vm::value::{FxHashMap, ValueRef, ValueType, VmValue};
use crate::vm::{Fail, Vm, internal};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::{size_of, size_of_val};
use std::sync::{Arc, OnceLock};

/// The Rust form of a Sand value.
///
/// Each part of the value whose type does not implement `Encode` is a
/// [`Value::Opaque`]. A `Value` does not reference the VM, so it remains valid
/// after the run ends.
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
    /// A set, with its elements in iteration order. Equality of two `Value`s
    /// compares the elements in order, although Sand set equality ignores
    /// order.
    Set(Vec<Value>),
    /// A map, with its entries in iteration order. Equality of two `Value`s
    /// compares the entries in order, although Sand map equality ignores
    /// order.
    Map(Vec<(Value, Value)>),
    /// An integer range: `start..end`, or `start..=end` if `inclusive` is true.
    Range { start: i64, end: i64, inclusive: bool },
    /// A struct.
    Struct(StructValue),
    /// An enum variant.
    Variant(VariantValue),
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
    /// A part whose type does not implement `Encode`, such as a function, a
    /// task, a channel, or a native object. It holds only the type name.
    Opaque { type_name: Arc<str> },
}

/// A struct value: its type and one value for each field.
#[derive(Clone, Debug, PartialEq)]
pub struct StructValue {
    /// The type of the struct.
    pub ty: Arc<StructDesc>,
    /// The field values in declaration order.
    pub fields: Box<[Value]>,
}

/// The name and field names of a struct type. All values of the type share it.
#[derive(Debug, PartialEq, Eq)]
pub struct StructDesc {
    /// The module-qualified type name, such as `shop.Order`. The name is a
    /// label, as [`NativeValue::type_name`] describes.
    pub name: String,
    /// The field names in declaration order.
    pub fields: Vec<String>,
}

/// An enum variant value: its variant and its payload.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantValue {
    /// The variant.
    pub ty: Arc<VariantDesc>,
    /// The payload values in declaration order. A variant without a payload
    /// has none.
    pub payload: Box<[Value]>,
}

/// One variant of an enum type. All values of the variant share it.
#[derive(Debug, PartialEq, Eq)]
pub struct VariantDesc {
    /// The module-qualified enum name, such as `shop.Status`. The name is a
    /// label, as [`NativeValue::type_name`] describes.
    pub enum_name: String,
    /// The variant name, such as `Shipped`.
    pub name: String,
    /// The names of the payload fields, or `None` for a positional payload.
    pub fields: Option<Vec<String>>,
}

impl StructDesc {
    /// Returns the description of the struct type `name` with the fields
    /// `fields`.
    pub fn new(name: impl Into<String>, fields: impl IntoIterator<Item = impl Into<String>>) -> Arc<StructDesc> {
        Arc::new(StructDesc { name: name.into(), fields: fields.into_iter().map(Into::into).collect() })
    }
}

impl StructValue {
    /// Returns the value of the field `name`, or `None` if the struct has no
    /// such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        field_named(&self.ty.fields, &self.fields, name)
    }

    /// Returns the field names and values in declaration order.
    pub fn named_fields(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.ty.fields.iter().map(String::as_str).zip(&*self.fields)
    }
}

impl VariantDesc {
    /// Returns the description of the variant `name` of the enum `enum_name`,
    /// with a positional payload or none.
    pub fn new(enum_name: impl Into<String>, name: impl Into<String>) -> Arc<VariantDesc> {
        Arc::new(VariantDesc { enum_name: enum_name.into(), name: name.into(), fields: None })
    }

    /// Returns the description of the variant `name` of the enum `enum_name`,
    /// with the named payload fields `fields`.
    pub fn with_fields(
        enum_name: impl Into<String>,
        name: impl Into<String>,
        fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Arc<VariantDesc> {
        let fields = Some(fields.into_iter().map(Into::into).collect());
        Arc::new(VariantDesc { enum_name: enum_name.into(), name: name.into(), fields })
    }
}

impl VariantValue {
    /// Returns the value of the named payload field `name`, or `None` if the
    /// payload has no such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        field_named(self.ty.fields.as_deref().unwrap_or_default(), &self.payload, name)
    }
}

fn field_named<'a>(names: &[String], values: &'a [Value], name: &str) -> Option<&'a Value> {
    values.get(names.iter().position(|n| n == name)?)
}

/// A value of a native value type with its type name and text.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeValue {
    /// The module-qualified type name, such as `net.SocketAddr`.
    ///
    /// A type name is a label, not a reference to a type. The module of a
    /// type from the main script is the file stem of the script, such as
    /// `main.Order` for a type of `main.sand`, so a script can declare a type
    /// whose name equals the name of a host type.
    pub type_name: Arc<str>,
    /// The `Display` text of the value. [`ValueType::parse`] turns it back
    /// into an equal value.
    pub text: String,
}

impl Value {
    /// Returns the value as the native value type `T`, or `None` if the value
    /// is not a `T`.
    ///
    /// ```
    /// # use sandlang::value::Value;
    /// let start = Value::PlainDate(jiff::civil::date(2024, 3, 10));
    /// assert_eq!(start.native::<jiff::civil::Date>(), Some(jiff::civil::date(2024, 3, 10)));
    /// assert_eq!(start.native::<jiff::Timestamp>(), None);
    /// ```
    pub fn native<T: ValueType>(&self) -> Option<T> {
        T::from_value(self)
    }

    /// Returns the bytes that a value of a native value type with its own
    /// variant allocates, or 0 for any other value.
    fn own_variant_bytes(&self) -> usize {
        match self {
            Value::ZonedDateTime(_) => size_of::<ZonedDateTime>(),
            Value::Span(_) => size_of::<Span>(),
            Value::Instant(_)
            | Value::PlainDateTime(_)
            | Value::PlainDate(_)
            | Value::PlainTime(_)
            | Value::PlainYearMonth(_)
            | Value::PlainMonthDay(_)
            | Value::TimeZone(_)
            | Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Duration(_)
            | Value::Str(_)
            | Value::Bytes(_)
            | Value::List(_)
            | Value::Tuple(_)
            | Value::Set(_)
            | Value::Map(_)
            | Value::Range { .. }
            | Value::Struct(_)
            | Value::Variant(_)
            | Value::Native(_)
            | Value::Opaque { .. } => 0,
        }
    }
}

/// A Rust type whose values have a [`Value`] form, the counterpart of the Sand
/// trait `Encode`.
///
/// Primitives, strings, `Vec<u8>` and `[u8]` (as `bytes`), collections,
/// slices, tuples, `Option`, references, and native value types implement
/// `Encode`.
/// [`#[derive(SandStruct)]`](crate::SandStruct) and
/// [`#[derive(SandEnum)]`](crate::SandEnum) implement it for a struct or an
/// enum. The `Value` of a derived type is equal to the `Value` that
/// [`Vm::to_value`] copies for the same Sand value. A hand-written `encode`
/// for a type that also converts to a `VmValue` must return the same `Value`
/// that `Vm::to_value` returns for the converted value.
///
/// ```
/// # use sandlang::value::{Encode, Value};
/// /// A point in the plane.
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// pub struct Point {
///     pub x: f64,
///     pub y: f64,
/// }
///
/// let Value::Struct(point) = Point { x: 1.0, y: 2.0 }.encode() else { unreachable!() };
/// assert_eq!(point.ty.name, "geo.Point");
/// assert_eq!(point.field("y"), Some(&Value::Float(2.0)));
/// ```
///
/// A derived struct or enum with a field that has no `Value` form, such as a
/// `VmVal` handle, takes `#[sand(no_value)]`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no `Value` form",
    note = "a `SandStruct` or `SandEnum` with a field such as a `VmVal` handle takes `#[sand(no_value)]`"
)]
pub trait Encode {
    /// Returns the [`Value`] form of `self`.
    fn encode(&self) -> Value;
}

impl Encode for Never {
    fn encode(&self) -> Value {
        match *self {}
    }
}

impl Encode for Value {
    fn encode(&self) -> Value {
        self.clone()
    }
}

impl Encode for () {
    fn encode(&self) -> Value {
        Value::Unit
    }
}

impl Encode for bool {
    fn encode(&self) -> Value {
        Value::Bool(*self)
    }
}

impl Encode for i64 {
    fn encode(&self) -> Value {
        Value::Int(*self)
    }
}

impl Encode for f64 {
    fn encode(&self) -> Value {
        Value::Float(*self)
    }
}

impl Encode for jiff::SignedDuration {
    fn encode(&self) -> Value {
        Value::Duration(*self)
    }
}

impl Encode for str {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
}

impl Encode for String {
    fn encode(&self) -> Value {
        Value::Str(self.clone())
    }
}

impl Encode for [u8] {
    fn encode(&self) -> Value {
        Value::Bytes(self.to_vec())
    }
}

impl Encode for Vec<u8> {
    fn encode(&self) -> Value {
        self.as_slice().encode()
    }
}

impl<T: Encode> Encode for [T] {
    fn encode(&self) -> Value {
        Value::List(self.iter().map(T::encode).collect())
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode(&self) -> Value {
        self.as_slice().encode()
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn encode(&self) -> Value {
        (**self).encode()
    }
}

impl<T: Encode + ?Sized> Encode for Box<T> {
    fn encode(&self) -> Value {
        (**self).encode()
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode(&self) -> Value {
        static SOME: OnceLock<Arc<VariantDesc>> = OnceLock::new();
        static NONE: OnceLock<Arc<VariantDesc>> = OnceLock::new();
        let (desc, name, payload): (_, _, Box<[Value]>) = match self {
            Some(x) => (&SOME, "Some", Box::new([x.encode()])),
            None => (&NONE, "None", Box::default()),
        };
        Value::Variant(VariantValue { ty: desc.get_or_init(|| VariantDesc::new("Option", name)).clone(), payload })
    }
}

/// Sorts the entries by key, so that the map has the same order on each call.
impl<K: Encode + Ord, V: Encode> Encode for HashMap<K, V> {
    fn encode(&self) -> Value {
        let mut entries: Vec<(&K, &V)> = self.iter().collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        encode_map(entries)
    }
}

impl<K: Encode, V: Encode> Encode for BTreeMap<K, V> {
    fn encode(&self) -> Value {
        encode_map(self)
    }
}

impl<K: Encode, V: Encode> Encode for indexmap::IndexMap<K, V> {
    fn encode(&self) -> Value {
        encode_map(self)
    }
}

fn encode_map<'a, K: Encode + 'a, V: Encode + 'a>(entries: impl IntoIterator<Item = (&'a K, &'a V)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.encode(), v.encode())).collect())
}

/// Sorts the elements, so that the set has the same order on each call.
impl<T: Encode + Ord> Encode for HashSet<T> {
    fn encode(&self) -> Value {
        let mut items: Vec<&T> = self.iter().collect();
        items.sort_unstable();
        Value::Set(items.into_iter().map(T::encode).collect())
    }
}

impl<T: Encode> Encode for BTreeSet<T> {
    fn encode(&self) -> Value {
        Value::Set(self.iter().map(T::encode).collect())
    }
}

macro_rules! encode_tuples {
    ($(($($n:ident $i:tt),+)),* $(,)?) => {$(
        impl<$($n: Encode),+> Encode for ($($n,)+) {
            fn encode(&self) -> Value {
                Value::Tuple(vec![$(self.$i.encode()),+])
            }
        }
    )*};
}
encode_tuples! {
    (A 0, B 1),
    (A 0, B 1, C 2),
    (A 0, B 1, C 2, D 3),
    (A 0, B 1, C 2, D 3, E 4),
    (A 0, B 1, C 2, D 3, E 4, F 5),
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6),
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7),
}

/// Copies `value` out of the heap of `vm`. See [`Vm::to_value`].
pub(crate) fn copy_from_vm(vm: &Vm, value: VmValue) -> Result<Value, Fail> {
    let mut sink = ValueSink {
        heap: &vm.heap,
        meter: &vm.meter,
        allocated: 0,
        scratch: Scratch::new(),
        type_names: FxHashMap::default(),
        structs: FxHashMap::default(),
        variants: FxHashMap::default(),
    };
    match encode::walk(&vm.prog, &vm.heap, &mut sink, OpenValue::Root(Value::Unit), value)? {
        OpenValue::Root(value) => Ok(value),
        _ => internal("to_value: the walk did not return the root"),
    }
}

/// Returns the name of a type for a reader outside its module: `module.Name`,
/// or `Name` for a type of the prelude, which has the module `None`.
pub(crate) fn qualified_name(module: Option<&str>, name: &str) -> String {
    match module {
        Some(module) => format!("{module}.{name}"),
        None => name.to_string(),
    }
}

/// Returns `true` if `text` is the name of the type `name` of `module`, as
/// [`qualified_name`] writes it.
pub(crate) fn is_qualified_name(text: &str, module: Option<&str>, name: &str) -> bool {
    match module {
        Some(module) => text.strip_prefix(module).and_then(|rest| rest.strip_prefix('.')) == Some(name),
        None => text == name,
    }
}

/// Returns the module of a type that Rust declares in `module`, for
/// [`qualified_name`]: `None` for the prelude. The engine reserves the module
/// name `prelude`, so no host module has it.
pub(crate) fn rust_type_module(module: &str) -> Option<&str> {
    (module != "prelude").then_some(module)
}

/// The [`EncodeSink`] that builds a [`Value`].
struct ValueSink<'h> {
    heap: &'h Heap,
    meter: &'h Meter,
    /// The bytes of Rust memory that the copy has allocated.
    allocated: usize,
    scratch: Scratch,
    /// The shared name of each native value type and each type of an opaque
    /// value in the copy, by its module and name.
    type_names: FxHashMap<(Option<&'static str>, &'static str), Arc<str>>,
    /// The description of each struct type in the copy, by runtime type ID.
    structs: FxHashMap<u32, Arc<StructDesc>>,
    /// The description of each variant in the copy, by runtime variant ID.
    variants: FxHashMap<u32, Arc<VariantDesc>>,
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
    Struct(Arc<StructDesc>, Vec<Value>),
    Variant(Arc<VariantDesc>, Vec<Value>),
}

impl ValueSink<'_> {
    /// Counts `bytes` of new Rust memory against the memory limit. Call this
    /// function before each allocation for the copy, except for a native
    /// value: only the value itself gives its size, so the copy counts it
    /// right after the value exists.
    fn reserve(&mut self, bytes: usize) -> Result<(), Fail> {
        self.allocated = self.allocated.saturating_add(bytes);
        self.scratch.check(self.heap, self.allocated)
    }

    /// Charges fuel for a copy of `content` bytes of text, and counts the
    /// copy and `header` more bytes of new Rust memory against the memory
    /// limit. The fuel does not depend on the pointer width of the host.
    fn reserve_copy(&mut self, content: usize, header: usize) -> Result<(), Fail> {
        self.meter.charge_bytes(content)?;
        self.reserve(content.saturating_add(header))
    }

    /// Charges fuel and memory for a description of `size` bytes in an `Arc`,
    /// with `text` bytes of names and the field names `fields`.
    fn reserve_desc(&mut self, size: usize, text: usize, fields: &[String]) -> Result<(), Fail> {
        let text = fields.iter().fold(text, |sum, field| sum.saturating_add(field.len()));
        self.reserve_copy(text, 2 * size_of::<usize>() + size + size_of_val(fields))
    }

    fn items(&mut self, len: usize) -> Result<Vec<Value>, Fail> {
        self.reserve(len.saturating_mul(size_of::<Value>()))?;
        Ok(Vec::with_capacity(len))
    }

    /// Returns the shared name of the type `name` of `module`, which Rust
    /// declares.
    fn type_name(&mut self, module: Option<&'static str>, name: &'static str) -> Result<Arc<str>, Fail> {
        if let Some(type_name) = self.type_names.get(&(module, name)) {
            return Ok(type_name.clone());
        }
        let type_name = qualified_name(module, name);
        self.reserve_copy(type_name.len(), 2 * size_of::<usize>())?;
        let type_name: Arc<str> = type_name.into();
        self.type_names.insert((module, name), type_name.clone());
        Ok(type_name)
    }

    fn struct_desc(&mut self, def: u32, ty: &StructRt) -> Result<Arc<StructDesc>, Fail> {
        if let Some(desc) = self.structs.get(&def) {
            return Ok(desc.clone());
        }
        let name = qualified_name(ty.module.as_deref(), &ty.name);
        self.reserve_desc(size_of::<StructDesc>(), name.len(), &ty.fields)?;
        let desc = Arc::new(StructDesc { name, fields: ty.fields.clone() });
        self.structs.insert(def, desc.clone());
        Ok(desc)
    }

    fn variant_desc(&mut self, variant: &EnumVariant<'_>) -> Result<Arc<VariantDesc>, Fail> {
        if let Some(desc) = self.variants.get(&variant.id) {
            return Ok(desc.clone());
        }
        let ty = variant.enum_type();
        let enum_name = qualified_name(ty.module.as_deref(), &ty.name);
        let fields = variant.field_names.unwrap_or_default();
        self.reserve_desc(size_of::<VariantDesc>(), enum_name.len() + variant.name.len(), fields)?;
        let fields = variant.field_names.map(<[String]>::to_vec);
        let desc = Arc::new(VariantDesc { enum_name, name: variant.name.to_string(), fields });
        self.variants.insert(variant.id, desc.clone());
        Ok(desc)
    }

    fn native(&mut self, native: ValueRef<'_>) -> Result<Value, Fail> {
        if let Some(value) = native.to_own_variant() {
            self.reserve(value.own_variant_bytes())?;
            return Ok(value);
        }
        let text = native.text();
        self.reserve_copy(text.len(), size_of::<NativeValue>())?;
        let type_name = self.type_name(rust_type_module(native.module_name()), native.type_name())?;
        Ok(Value::Native(Box::new(NativeValue { type_name, text })))
    }

    fn opaque(&mut self, kind: OpaqueKind) -> Result<Value, Fail> {
        let type_name = match kind {
            OpaqueKind::Function => self.type_name(None, "fn")?,
            OpaqueKind::Task => self.type_name(None, "Task")?,
            OpaqueKind::Channel => self.type_name(None, "Channel")?,
            OpaqueKind::Object { module, name } => self.type_name(rust_type_module(module), name)?,
            OpaqueKind::Internal => return internal("to_value: an internal heap object has no `Value` form"),
        };
        Ok(Value::Opaque { type_name })
    }
}

fn put(parent: &mut OpenValue, value: Value) {
    match parent {
        OpenValue::Root(root) => *root = value,
        OpenValue::List(items)
        | OpenValue::Tuple(items)
        | OpenValue::Set(items)
        | OpenValue::Struct(_, items)
        | OpenValue::Variant(_, items) => items.push(value),
        OpenValue::Map(entries, has_key) => {
            match entries.last_mut() {
                Some(entry) if *has_key => entry.1 = value,
                _ => entries.push((value, Value::Unit)),
            }
            *has_key = !*has_key;
        }
    }
}

impl<'h> EncodeSink<'h> for ValueSink<'h> {
    type Open = OpenValue;

    const TOO_DEEP: &'static str = "value too deeply nested to copy";

    #[inline(always)]
    fn leaf(&mut self, parent: &mut OpenValue, _: Role<'h>, leaf: Leaf<'h>) -> Result<(), Fail> {
        self.meter.charge(1)?;
        let value = match leaf {
            Leaf::Unit => Value::Unit,
            Leaf::Bool(b) => Value::Bool(b),
            Leaf::Int(i) => Value::Int(i),
            Leaf::Float(f) => Value::Float(f),
            Leaf::Duration(nanos) => Value::Duration(jiff::SignedDuration::from_nanos(nanos)),
            Leaf::Str { text, .. } => {
                self.reserve_copy(text.len(), 0)?;
                Value::Str(text.to_string())
            }
            Leaf::Bytes(bytes) => {
                self.reserve_copy(bytes.len(), 0)?;
                Value::Bytes(bytes.to_vec())
            }
            Leaf::Range { start, end, inclusive } => Value::Range { start, end, inclusive },
            Leaf::Tag(variant) => Value::Variant(VariantValue { ty: self.variant_desc(&variant)?, payload: Box::default() }),
            Leaf::Native(native) => self.native(native)?,
            Leaf::Opaque(kind) => self.opaque(kind)?,
        };
        put(parent, value);
        Ok(())
    }

    #[inline(always)]
    fn open(&mut self, _: &mut OpenValue, _: Role<'h>, container: Container<'h>, len: usize) -> Result<Opened<OpenValue>, Fail> {
        self.meter.charge(1)?;
        Ok(Opened::Children(match container {
            Container::List => OpenValue::List(self.items(len)?),
            Container::Tuple => OpenValue::Tuple(self.items(len)?),
            Container::Set => OpenValue::Set(self.items(len)?),
            Container::Map => {
                self.reserve(len.saturating_mul(size_of::<(Value, Value)>()))?;
                OpenValue::Map(Vec::with_capacity(len), false)
            }
            Container::Struct { def, ty } => OpenValue::Struct(self.struct_desc(def, ty)?, self.items(len)?),
            Container::Variant(variant) => OpenValue::Variant(self.variant_desc(&variant)?, self.items(len)?),
        }))
    }

    #[inline(always)]
    fn close(&mut self, parent: &mut OpenValue, open: OpenValue) -> Result<(), Fail> {
        let value = match open {
            OpenValue::Root(_) => return internal("to_value: the walk closed the root"),
            OpenValue::List(items) => Value::List(items),
            OpenValue::Tuple(items) => Value::Tuple(items),
            OpenValue::Set(items) => Value::Set(items),
            OpenValue::Map(entries, _) => Value::Map(entries),
            OpenValue::Struct(ty, fields) => Value::Struct(StructValue { ty, fields: fields.into_boxed_slice() }),
            OpenValue::Variant(ty, payload) => Value::Variant(VariantValue { ty, payload: payload.into_boxed_slice() }),
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
