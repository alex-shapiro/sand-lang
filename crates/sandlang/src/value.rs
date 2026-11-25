//! Sand values in a Rust form that is independent of the VM.

use crate::native::ty::Never;
use crate::stdlib::codec::{CodecResult, Container, EncodeSink, Scalar, memory_limit_error, nesting_limit_error};
use crate::vm::iomem::{IoMemory, Reservation};
use crate::vm::value::{FxHashMap, ValueType};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

/// The Rust representation of a Sand value.
///
/// A `Value` is independent of the VM and remains valid after a run ends.
///
/// `Debug` formats structs and variants with their type names, such as
/// `Struct(shop.Order { id: Int(1) })`.
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
    /// A set with its elements in iteration order. `Value` equality compares
    /// elements in order, even though Sand set equality ignores order.
    Set(Vec<Value>),
    /// A map with its entries in iteration order. `Value` equality compares
    /// entries in order, even though Sand map equality ignores order.
    Map(Vec<(Value, Value)>),
    /// An integer range: `start..end`, or `start..=end` if `inclusive` is true.
    Range { start: i64, end: i64, inclusive: bool },
    /// A struct.
    Struct(StructValue),
    /// An enum variant.
    Variant(VariantValue),
    /// A value of a native value type.
    Native(Box<NativeValue>),
}

/// A struct value and its type descriptor.
#[derive(Clone, PartialEq)]
pub struct StructValue {
    /// The struct type descriptor.
    pub ty: Arc<StructDesc>,
    /// The field values in declaration order.
    pub fields: Box<[Value]>,
}

/// The name and field names of a struct type. Instances of the type share this
/// descriptor.
#[derive(Debug, PartialEq, Eq)]
pub struct StructDesc {
    /// The module-qualified type name, such as `shop.Order`.
    ///
    /// The module of a type from the main script is the file stem of the
    /// script, such as `main.Order` for `main.sand`. This prefix distinguishes
    /// script types from host types with the same base name.
    pub type_name: String,
    /// The field names in declaration order.
    pub fields: Vec<String>,
}

/// An enum variant value and its variant descriptor.
#[derive(Clone, PartialEq)]
pub struct VariantValue {
    /// The variant descriptor.
    pub variant: Arc<VariantDesc>,
    /// The payload values in declaration order. A unit variant has an empty
    /// slice.
    pub payload: Box<[Value]>,
}

/// The descriptor of an enum variant. Instances of the variant share this
/// descriptor.
#[derive(Debug, PartialEq, Eq)]
pub struct VariantDesc {
    /// The module-qualified enum name, such as `shop.Status`. See
    /// [`StructDesc::type_name`].
    pub type_name: String,
    /// The variant name, such as `Shipped`.
    pub name: String,
    /// The payload field names, or `None` for a positional or empty payload.
    pub fields: Option<Vec<String>>,
}

impl StructDesc {
    /// Builds a descriptor for the struct `type_name` with `fields`.
    pub fn new(type_name: impl Into<String>, fields: impl IntoIterator<Item = impl Into<String>>) -> Arc<StructDesc> {
        Arc::new(StructDesc { type_name: type_name.into(), fields: fields.into_iter().map(Into::into).collect() })
    }
}

impl StructValue {
    /// Builds a value of the struct `type_name` from `(name, value)` pairs in
    /// declaration order.
    ///
    /// ```
    /// # use sandlang::value::{StructValue, Value};
    /// let order = StructValue::new("shop.Order", [("id", Value::Int(1))]);
    /// assert_eq!(order.field("id"), Some(&Value::Int(1)));
    /// ```
    pub fn new<N: Into<String>>(type_name: impl Into<String>, fields: impl IntoIterator<Item = (N, Value)>) -> StructValue {
        let (names, values): (Vec<String>, Vec<Value>) = fields.into_iter().map(|(n, v)| (n.into(), v)).unzip();
        StructValue { ty: StructDesc::new(type_name, names), fields: values.into() }
    }

    /// Returns the value of the field `name`, or `None` if `name` is not a
    /// field of this struct.
    pub fn field(&self, name: &str) -> Option<&Value> {
        field_by_name(&self.ty.fields, &self.fields, name)
    }

    /// Returns the field names and values in declaration order.
    pub fn named_fields(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.ty.fields.iter().map(String::as_str).zip(&*self.fields)
    }
}

impl VariantDesc {
    /// Builds a descriptor for the variant `name` of the enum `type_name` with
    /// a positional or empty payload.
    pub fn new(type_name: impl Into<String>, name: impl Into<String>) -> Arc<VariantDesc> {
        Arc::new(VariantDesc { type_name: type_name.into(), name: name.into(), fields: None })
    }

    /// Builds a descriptor for the variant `name` of the enum `type_name` with
    /// the named payload `fields`.
    pub fn with_fields(
        type_name: impl Into<String>,
        name: impl Into<String>,
        fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Arc<VariantDesc> {
        let fields = Some(fields.into_iter().map(Into::into).collect());
        Arc::new(VariantDesc { type_name: type_name.into(), name: name.into(), fields })
    }
}

impl VariantValue {
    /// Builds the variant `name` of the enum `type_name` with the positional
    /// `payload`.
    pub fn new(type_name: impl Into<String>, name: impl Into<String>, payload: impl IntoIterator<Item = Value>) -> VariantValue {
        VariantValue { variant: VariantDesc::new(type_name, name), payload: payload.into_iter().collect() }
    }

    /// Builds the variant `name` of the enum `type_name` from `(name, value)`
    /// payload pairs.
    pub fn with_fields<N: Into<String>>(
        type_name: impl Into<String>,
        name: impl Into<String>,
        fields: impl IntoIterator<Item = (N, Value)>,
    ) -> VariantValue {
        let (names, payload): (Vec<String>, Vec<Value>) = fields.into_iter().map(|(n, v)| (n.into(), v)).unzip();
        VariantValue { variant: VariantDesc::with_fields(type_name, name, names), payload: payload.into() }
    }

    /// Returns the value of the payload field `name`, or `None` if `name` is
    /// not a payload field of this variant.
    pub fn field(&self, name: &str) -> Option<&Value> {
        field_by_name(self.variant.fields.as_deref().unwrap_or_default(), &self.payload, name)
    }
}

fn field_by_name<'a>(names: &[String], values: &'a [Value], name: &str) -> Option<&'a Value> {
    values.get(names.iter().position(|n| n == name)?)
}

/// A value of a native value type with its type name and text.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeValue {
    /// The module-qualified type name, such as `bank.Money`.
    pub type_name: Arc<str>,
    /// The `Display` text of the value. [`ValueType::parse`] reconstructs an
    /// equal value from this text.
    pub text: String,
}

impl Value {
    /// Parses `self` as the native value type `T`.
    ///
    /// Returns `None` if `self` is not a valid `T` value.
    ///
    /// ```
    /// # use sandlang::value::{ToValue, Value};
    /// let start = jiff::civil::date(2024, 3, 10).to_value();
    /// assert_eq!(start.native::<jiff::civil::Date>(), Some(jiff::civil::date(2024, 3, 10)));
    /// assert_eq!(start.native::<jiff::Timestamp>(), None);
    /// ```
    pub fn native<T: ValueType>(&self) -> Option<T> {
        let Value::Native(native) = self else { return None };
        let is_t = is_qualified_name(&native.type_name, qualified_name_module(T::MODULE), T::NAME);
        if is_t { T::parse(&native.text).ok() } else { None }
    }
}

impl fmt::Debug for StructValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_named_fields(f.debug_struct(&self.ty.type_name), &self.ty.fields, &self.fields)
    }
}

impl fmt::Debug for VariantValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.variant.type_name)?;
        f.write_str(".")?;
        match &self.variant.fields {
            Some(names) => debug_named_fields(f.debug_struct(&self.variant.name), names, &self.payload),
            None => self.payload.iter().fold(&mut f.debug_tuple(&self.variant.name), |out, value| out.field(value)).finish(),
        }
    }
}

/// Formats `values` with their field names in `names`, or with `_` if `names`
/// is shorter than `values`.
fn debug_named_fields(mut out: fmt::DebugStruct<'_, '_>, names: &[String], values: &[Value]) -> fmt::Result {
    for (i, value) in values.iter().enumerate() {
        out.field(names.get(i).map_or("_", String::as_str), value);
    }
    out.finish()
}

/// Converts a Rust value into a constant [`Value`] for host APIs and
/// `#[sand(default)]` field defaults.
///
/// Common Rust types and native value types implement `ToValue`.
/// [`#[derive(SandStruct)]`](crate::SandStruct) and
/// [`#[derive(SandEnum)]`](crate::SandEnum) derive `ToValue` for structs and
/// enums. Derived `to_value` implementations convert each Rust field or variant
/// directly. They bypass Sand `Encode` implementations.
///
/// ```
/// # use sandlang::value::{ToValue, Value};
/// /// A point in the plane.
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// pub struct Point {
///     pub x: f64,
///     pub y: f64,
/// }
///
/// let Value::Struct(point) = Point { x: 1.0, y: 2.0 }.to_value() else { unreachable!() };
/// assert_eq!(point.ty.type_name, "geo.Point");
/// assert_eq!(point.field("y"), Some(&Value::Float(2.0)));
/// ```
///
/// Add `#[sand(no_value)]` to omit the `ToValue` implementation on a derived
/// struct or enum with VM-only fields such as `VmVal`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not implement `sandlang::value::ToValue`",
    note = "add `#[sand(no_value)]` to a `SandStruct` or `SandEnum` whose fields do not implement `ToValue`"
)]
pub trait ToValue {
    /// Converts `self` into a [`Value`].
    fn to_value(&self) -> Value;
}

macro_rules! impl_to_value {
    ($($ty:ty => |$v:ident| $body:expr),* $(,)?) => {
        $(impl ToValue for $ty {
            fn to_value(&self) -> Value { let $v = self; $body }
        })*
    };
}

impl_to_value! {
    Never => |v| match *v {},
    Value => |v| v.clone(),
    () => |_v| Value::Unit,
    bool => |v| Value::Bool(*v),
    i64 => |v| Value::Int(*v),
    f64 => |v| Value::Float(*v),
    jiff::SignedDuration => |v| Value::Duration(*v),
    str => |v| Value::Str(v.to_string()),
    String => |v| Value::Str(v.clone()),
    [u8] => |v| Value::Bytes(v.to_vec()),
    Vec<u8> => |v| v.as_slice().to_value(),
}

impl<T: ToValue> ToValue for [T] {
    fn to_value(&self) -> Value {
        Value::List(self.iter().map(T::to_value).collect())
    }
}
impl<T: ToValue> ToValue for Vec<T> {
    fn to_value(&self) -> Value {
        self.as_slice().to_value()
    }
}
impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}
impl<T: ToValue + ?Sized> ToValue for Box<T> {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        let (some, payload): (_, Box<[Value]>) = match self {
            Some(inner) => (true, Box::new([inner.to_value()])),
            None => (false, Box::default()),
        };
        Value::Variant(VariantValue { variant: option_desc(some), payload })
    }
}
impl<K: ToValue + Ord, V: ToValue> ToValue for HashMap<K, V> {
    fn to_value(&self) -> Value {
        let mut entries: Vec<(&K, &V)> = self.iter().collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        map_to_value(entries)
    }
}
impl<K: ToValue, V: ToValue> ToValue for BTreeMap<K, V> {
    fn to_value(&self) -> Value {
        map_to_value(self)
    }
}
impl<K: ToValue, V: ToValue> ToValue for indexmap::IndexMap<K, V> {
    fn to_value(&self) -> Value {
        map_to_value(self)
    }
}
fn map_to_value<'a, K: ToValue + 'a, V: ToValue + 'a>(entries: impl IntoIterator<Item = (&'a K, &'a V)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.to_value(), v.to_value())).collect())
}
impl<T: ToValue + Ord> ToValue for HashSet<T> {
    fn to_value(&self) -> Value {
        let mut items: Vec<&T> = self.iter().collect();
        items.sort_unstable();
        Value::Set(items.into_iter().map(T::to_value).collect())
    }
}
impl<T: ToValue> ToValue for BTreeSet<T> {
    fn to_value(&self) -> Value {
        Value::Set(self.iter().map(T::to_value).collect())
    }
}

macro_rules! tuples_to_value {
    ($(($($n:ident $i:tt),+)),* $(,)?) => {$(
        impl<$($n: ToValue),+> ToValue for ($($n,)+) {
            fn to_value(&self) -> Value {
                Value::Tuple(vec![$(self.$i.to_value()),+])
            }
        }
    )*};
}
tuples_to_value! {
    (A 0, B 1),
    (A 0, B 1, C 2),
    (A 0, B 1, C 2, D 3),
    (A 0, B 1, C 2, D 3, E 4),
    (A 0, B 1, C 2, D 3, E 4, F 5),
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6),
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7),
}

/// Formats a qualified type name: `module.Name`, or `Name` if `module` is
/// `None`.
pub(crate) fn qualified_name(module: Option<&str>, name: &str) -> String {
    match module {
        Some(module) => format!("{module}.{name}"),
        None => name.to_string(),
    }
}

/// Returns `true` if `text` matches `module` and `name` as formatted by
/// [`qualified_name`].
pub(crate) fn is_qualified_name(text: &str, module: Option<&str>, name: &str) -> bool {
    match module {
        Some(module) => text.strip_prefix(module).and_then(|rest| rest.strip_prefix('.')) == Some(name),
        None => text == name,
    }
}

/// Returns `Some(rust_module)` for a non-prelude module, or `None` for
/// `"prelude"`. The engine reserves the module name `"prelude"`.
pub(crate) fn qualified_name_module(rust_module: &str) -> Option<&str> {
    (rust_module != "prelude").then_some(rust_module)
}

/// Memory byte estimate charged for each [`Value`] node.
const VALUE_BYTES: usize = size_of::<Value>();

/// Maximum container nesting depth accepted by [`ValueSink`].
///
/// Counts `Some` wrappers toward the depth. This limit prevents stack overflow
/// during recursive `Drop` of [`Value`].
const MAX_VALUE_DEPTH: usize = 512;

enum PartialValue {
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    Map(Vec<(Value, Value)>, bool),
    Struct(Arc<StructDesc>, Vec<Value>),
    Variant(Arc<VariantDesc>, Vec<Value>),
    Some(Vec<Value>),
}

/// [`EncodeSink`] that assembles a host [`Value`].
pub(crate) struct ValueSink {
    root: Option<Value>,
    open: Vec<PartialValue>,
    allocated: usize,
    reservation: Reservation,
    type_names: FxHashMap<String, Arc<str>>,
}

fn option_desc(some: bool) -> Arc<VariantDesc> {
    static SOME: OnceLock<Arc<VariantDesc>> = OnceLock::new();
    static NONE: OnceLock<Arc<VariantDesc>> = OnceLock::new();
    let (cell, name) = if some { (&SOME, "Some") } else { (&NONE, "None") };
    cell.get_or_init(|| VariantDesc::new("Option", name)).clone()
}

impl ValueSink {
    pub(crate) fn new(memory: &Arc<IoMemory>) -> ValueSink {
        ValueSink { root: None, open: Vec::new(), allocated: 0, reservation: Reservation::new(memory), type_names: FxHashMap::default() }
    }

    pub(crate) fn take(&mut self) -> Option<(Value, Reservation)> {
        let root = self.root.take()?;
        let sibling = self.reservation.sibling();
        Some((root, std::mem::replace(&mut self.reservation, sibling)))
    }

    fn grow(&mut self, bytes: usize) -> CodecResult {
        self.allocated = self.allocated.saturating_add(bytes);
        if self.allocated <= self.reservation.bytes() {
            return Ok(());
        }
        let ahead = self.allocated.saturating_mul(2).max(1 << 10);
        if self.reservation.ensure(ahead).or_else(|_| self.reservation.ensure(self.allocated)).is_err() {
            return memory_limit_error();
        }
        Ok(())
    }

    fn place(&mut self, value: Value) {
        match self.open.last_mut() {
            None => self.root = Some(value),
            Some(PartialValue::Map(entries, has_key)) => {
                match entries.last_mut() {
                    Some(entry) if *has_key => entry.1 = value,
                    _ => entries.push((value, Value::Unit)),
                }
                *has_key = !*has_key;
            }
            Some(
                PartialValue::List(v)
                | PartialValue::Tuple(v)
                | PartialValue::Set(v)
                | PartialValue::Struct(_, v)
                | PartialValue::Variant(_, v)
                | PartialValue::Some(v),
            ) => v.push(value),
        }
    }

    fn open<T>(&mut self, len: usize, build: impl FnOnce(Vec<T>) -> PartialValue) -> CodecResult {
        if self.open.len() >= MAX_VALUE_DEPTH {
            return nesting_limit_error();
        }
        self.grow(len.saturating_mul(size_of::<T>()))?;
        self.open.push(build(Vec::with_capacity(len)));
        Ok(())
    }
}

impl EncodeSink for ValueSink {
    #[inline(always)]
    fn scalar(&mut self, value: Scalar<'_>) -> CodecResult {
        let v = match value {
            Scalar::Unit => Value::Unit,
            Scalar::Bool(b) => Value::Bool(b),
            Scalar::Int(n) => Value::Int(n),
            Scalar::Float(f) => Value::Float(f),
            Scalar::Duration(n) => Value::Duration(jiff::SignedDuration::from_nanos(n)),
            Scalar::Str(s) => {
                self.grow(s.len())?;
                Value::Str(s.to_string())
            }
            Scalar::Bytes(b) => {
                self.grow(b.len())?;
                Value::Bytes(b.to_vec())
            }
            Scalar::Range(start, end, inclusive) => Value::Range { start, end, inclusive },
            Scalar::Native(type_name, text) => {
                let name = match self.type_names.get(type_name) {
                    Some(n) => n.clone(),
                    None => {
                        self.grow(2 * type_name.len())?;
                        let n: Arc<str> = type_name.into();
                        self.type_names.insert(type_name.to_string(), n.clone());
                        n
                    }
                };
                self.grow(text.len() + size_of::<NativeValue>())?;
                Value::Native(Box::new(NativeValue { type_name: name, text: text.to_string() }))
            }
            Scalar::Variant(desc) => Value::Variant(VariantValue { variant: desc.clone(), payload: Box::default() }),
            Scalar::None => Value::Variant(VariantValue { variant: option_desc(false), payload: Box::default() }),
        };
        if self.open.is_empty() {
            self.grow(VALUE_BYTES)?;
        }
        self.place(v);
        Ok(())
    }

    #[inline(always)]
    fn begin(&mut self, kind: Container<'_>) -> CodecResult {
        match kind {
            Container::Some => self.open(1, PartialValue::Some),
            Container::List(n) => self.open(n, PartialValue::List),
            Container::Tuple(n) => self.open(n, PartialValue::Tuple),
            Container::Set(n) => self.open(n, PartialValue::Set),
            Container::Map(n) => self.open(n, |e| PartialValue::Map(e, false)),
            Container::Struct(d) => self.open(d.fields.len(), |f| PartialValue::Struct(d.clone(), f)),
            Container::Variant(d, n) => self.open(n, |p| PartialValue::Variant(d.clone(), p)),
        }
    }

    #[inline(always)]
    fn end(&mut self) -> CodecResult {
        let value = match self.open.pop() {
            Some(PartialValue::List(v)) => Value::List(v),
            Some(PartialValue::Tuple(v)) => Value::Tuple(v),
            Some(PartialValue::Set(v)) => Value::Set(v),
            Some(PartialValue::Map(e, _)) => Value::Map(e),
            Some(PartialValue::Struct(ty, f)) => Value::Struct(StructValue { ty, fields: f.into() }),
            Some(PartialValue::Variant(variant, p)) => Value::Variant(VariantValue { variant, payload: p.into() }),
            Some(PartialValue::Some(p)) => Value::Variant(VariantValue { variant: option_desc(true), payload: p.into() }),
            None => return Ok(()),
        };
        self.place(value);
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

    #[test]
    fn debug_shows_structs_and_variants_by_name() {
        let none = Value::Variant(VariantValue::new("Option", "None", []));
        let some = Value::Variant(VariantValue::new("Option", "Some", [Value::Int(1)]));
        let order = Value::Struct(StructValue::new("shop.Order", [("id", Value::Int(1)), ("note", none)]));
        let moved = Value::Variant(VariantValue::with_fields("shop.Event", "Moved", [("x", Value::Int(1))]));
        assert_eq!(format!("{order:?}"), "Struct(shop.Order { id: Int(1), note: Variant(Option.None) })");
        assert_eq!(format!("{moved:?}"), "Variant(shop.Event.Moved { x: Int(1) })");
        assert_eq!(format!("{:?}", Value::List(vec![some])), "List([Variant(Option.Some(Int(1)))])");
    }
}
