//! Sand values in a Rust form that is independent of the VM.

use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::convert::HostError;
use crate::native::ty::Never;
use crate::vm::Fail;
use crate::vm::iomem::{IoMemory, Reservation};
use crate::vm::value::{FxHashMap, ValueType};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

/// Host representation of a Sand value for types that implement `Encode` or
/// `Decode`.
///
/// A `Value` is independent of the VM. It remains valid after a run ends.
/// [`NativeContext::to_value`](crate::native::context::NativeContext::to_value)
/// encodes a Sand value into a `Value`.
/// [`NativeContext::from_value`](crate::native::context::NativeContext::from_value)
/// decodes a `Value` back into the VM.
///
/// `Debug` formats structs and variants with their qualified names. For
/// example, it formats `shop.Order` as `Struct(shop.Order { id: Int(1) })`.
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
    /// elements in order. Sand set equality ignores order.
    Set(Vec<Value>),
    /// A map with its entries in iteration order. `Value` equality compares
    /// entries in order. Sand map equality ignores order.
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

impl ToValue for Never {
    fn to_value(&self) -> Value {
        match *self {}
    }
}

impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}

impl ToValue for () {
    fn to_value(&self) -> Value {
        Value::Unit
    }
}

impl ToValue for bool {
    fn to_value(&self) -> Value {
        Value::Bool(*self)
    }
}

impl ToValue for i64 {
    fn to_value(&self) -> Value {
        Value::Int(*self)
    }
}

impl ToValue for f64 {
    fn to_value(&self) -> Value {
        Value::Float(*self)
    }
}

impl ToValue for jiff::SignedDuration {
    fn to_value(&self) -> Value {
        Value::Duration(*self)
    }
}

impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Str(self.to_string())
    }
}

impl ToValue for String {
    fn to_value(&self) -> Value {
        Value::Str(self.clone())
    }
}

impl ToValue for [u8] {
    fn to_value(&self) -> Value {
        Value::Bytes(self.to_vec())
    }
}

impl ToValue for Vec<u8> {
    fn to_value(&self) -> Value {
        self.as_slice().to_value()
    }
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
        static SOME: OnceLock<Arc<VariantDesc>> = OnceLock::new();
        static NONE: OnceLock<Arc<VariantDesc>> = OnceLock::new();
        let (desc, name, payload): (_, _, Box<[Value]>) = match self {
            Some(inner) => (&SOME, "Some", Box::new([inner.to_value()])),
            None => (&NONE, "None", Box::default()),
        };
        Value::Variant(VariantValue { variant: desc.get_or_init(|| VariantDesc::new("Option", name)).clone(), payload })
    }
}

/// Sorts entries by key for deterministic iteration order.
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
    Value::Map(entries.into_iter().map(|(key, value)| (key.to_value(), value.to_value())).collect())
}

/// Sorts elements for deterministic iteration order.
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

/// Open container under construction in [`ValueSink`].
enum PartialValue {
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    /// Map entries and a pending-key flag.
    ///
    /// The flag is `true` if the last entry has a key and awaits its value.
    Map(Vec<(Value, Value)>, bool),
    Struct(Arc<StructDesc>, Vec<Value>),
    Variant(Arc<VariantDesc>, Vec<Value>),
    Some(Vec<Value>),
}

/// [`EncodeSink`] that assembles a host [`Value`].
pub(crate) struct ValueSink {
    root: Option<Value>,
    open_containers: Vec<PartialValue>,
    /// Total bytes accounted for the value under construction.
    allocated: usize,
    /// Memory reservation tracked against the VM memory limit.
    reservation: Reservation,
    /// Interned qualified names for native value types.
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
        ValueSink {
            root: None,
            open_containers: Vec::new(),
            allocated: 0,
            reservation: Reservation::new(memory),
            type_names: FxHashMap::default(),
        }
    }

    /// Takes the completed [`Value`] and its memory [`Reservation`].
    pub(crate) fn take(&mut self) -> Option<(Value, Reservation)> {
        let root = self.root.take()?;
        let sibling = self.reservation.sibling();
        Some((root, std::mem::replace(&mut self.reservation, sibling)))
    }

    /// Reserves `bytes` additional bytes within the VM memory limit.
    fn grow(&mut self, bytes: usize) -> CodecResult {
        self.allocated = self.allocated.saturating_add(bytes);
        if self.allocated <= self.reservation.bytes() {
            return Ok(());
        }
        let ahead = self.allocated.saturating_mul(2).max(1 << 10);
        match self.reservation.ensure(ahead).or_else(|_| self.reservation.ensure(self.allocated)) {
            Ok(()) => Ok(()),
            Err(_) => crate::native::codec::memory_limit_error(),
        }
    }

    fn push(&mut self, value: Value) -> CodecResult {
        // `open` pre-reserves element slots for every open container.
        if self.open_containers.is_empty() {
            self.grow(VALUE_BYTES)?;
        }
        self.place(value);
        Ok(())
    }

    /// Appends `value` to the innermost open container or stores it as `root`.
    fn place(&mut self, value: Value) {
        match self.open_containers.last_mut() {
            None => self.root = Some(value),
            Some(
                PartialValue::List(items)
                | PartialValue::Tuple(items)
                | PartialValue::Set(items)
                | PartialValue::Struct(_, items)
                | PartialValue::Variant(_, items)
                | PartialValue::Some(items),
            ) => items.push(value),
            Some(PartialValue::Map(entries, has_key)) => {
                match entries.last_mut() {
                    Some(entry) if *has_key => entry.1 = value,
                    _ => entries.push((value, Value::Unit)),
                }
                *has_key = !*has_key;
            }
        }
    }

    /// Pushes a new open container with pre-reserved capacity for `len` items.
    fn open<T>(&mut self, len: usize, build: impl FnOnce(Vec<T>) -> PartialValue) -> CodecResult {
        if self.open_containers.len() >= MAX_VALUE_DEPTH {
            return crate::stdlib::codec::nesting_limit_error();
        }
        self.grow(len.saturating_mul(size_of::<T>()))?;
        self.open_containers.push(build(Vec::with_capacity(len)));
        Ok(())
    }
}

impl EncodeSink for ValueSink {
    fn unit(&mut self) -> CodecResult {
        self.push(Value::Unit)
    }

    fn bool(&mut self, value: bool) -> CodecResult {
        self.push(Value::Bool(value))
    }

    fn int(&mut self, value: i64) -> CodecResult {
        self.push(Value::Int(value))
    }

    fn float(&mut self, value: f64) -> CodecResult {
        self.push(Value::Float(value))
    }

    fn duration(&mut self, nanos: i64) -> CodecResult {
        self.push(Value::Duration(jiff::SignedDuration::from_nanos(nanos)))
    }

    fn str(&mut self, value: &str) -> CodecResult {
        self.grow(value.len())?;
        self.push(Value::Str(value.to_string()))
    }

    fn bytes(&mut self, value: &[u8]) -> CodecResult {
        self.grow(value.len())?;
        self.push(Value::Bytes(value.to_vec()))
    }

    fn range(&mut self, start: i64, end: i64, inclusive: bool) -> CodecResult {
        self.push(Value::Range { start, end, inclusive })
    }

    fn native(&mut self, type_name: &str, text: &str) -> CodecResult {
        let type_name = match self.type_names.get(type_name) {
            Some(interned) => interned.clone(),
            None => {
                self.grow(2 * type_name.len())?;
                let interned: Arc<str> = type_name.into();
                self.type_names.insert(type_name.to_string(), interned.clone());
                interned
            }
        };
        self.grow(text.len() + size_of::<NativeValue>())?;
        self.push(Value::Native(Box::new(NativeValue { type_name, text: text.to_string() })))
    }

    fn none(&mut self) -> CodecResult {
        self.push(Value::Variant(VariantValue { variant: option_desc(false), payload: Box::default() }))
    }

    fn begin_some(&mut self) -> CodecResult {
        self.open(1, PartialValue::Some)
    }

    fn begin_list(&mut self, len: usize) -> CodecResult {
        self.open(len, PartialValue::List)
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        self.open(len, PartialValue::Tuple)
    }

    fn begin_set(&mut self, len: usize) -> CodecResult {
        self.open(len, PartialValue::Set)
    }

    fn begin_map(&mut self, len: usize) -> CodecResult {
        self.open(len, |entries| PartialValue::Map(entries, false))
    }

    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult {
        self.open(desc.fields.len(), |fields| PartialValue::Struct(desc.clone(), fields))
    }

    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult {
        if len == 0 {
            self.open_containers.push(PartialValue::Variant(desc.clone(), Vec::new()));
            return Ok(());
        }
        self.open(len, |payload| PartialValue::Variant(desc.clone(), payload))
    }

    fn end(&mut self) -> CodecResult {
        let value = match self.open_containers.pop() {
            Some(PartialValue::List(items)) => Value::List(items),
            Some(PartialValue::Tuple(items)) => Value::Tuple(items),
            Some(PartialValue::Set(items)) => Value::Set(items),
            Some(PartialValue::Map(entries, _)) => Value::Map(entries),
            Some(PartialValue::Struct(ty, fields)) => Value::Struct(StructValue { ty, fields: fields.into() }),
            Some(PartialValue::Variant(variant, payload)) => Value::Variant(VariantValue { variant, payload: payload.into() }),
            Some(PartialValue::Some(payload)) => Value::Variant(VariantValue { variant: option_desc(true), payload: payload.into() }),
            None => return Ok(()),
        };
        self.place(value);
        Ok(())
    }
}

/// Active container frame in [`ValueSource`].
struct ValueSourceFrame {
    kind: ValueSourceFrameKind,
    /// Path segment for the current element or field.
    segment: Option<String>,
    count: usize,
}

enum ValueSourceFrameKind {
    /// Elements of a list, set, tuple, or positional variant payload.
    Items(std::vec::IntoIter<Value>),
    /// Remaining map entries and the pending value for the current key.
    Map(std::vec::IntoIter<(Value, Value)>, Option<Value>),
    /// Field names and values of a struct or named variant payload.
    Fields(Vec<String>, std::vec::IntoIter<Value>),
    /// `Some` wrapper around the value in `ValueSource::pending`.
    Some,
    /// Enum variant awaiting closure of its payload frame.
    Variant,
}

/// [`DecodeSource`] that reads from a host [`Value`].
///
/// Requires the exact [`Value`] variant for each Sand type. For example,
/// `bytes` requires [`Value::Bytes`].
pub(crate) struct ValueSource {
    pending: Option<Value>,
    frames: Vec<ValueSourceFrame>,
    /// Payload values stashed by `begin_variant` until `begin_payload`.
    payload: Vec<Value>,
    payload_names: Vec<String>,
}

impl ValueSource {
    pub(crate) fn new(value: Value) -> ValueSource {
        ValueSource { pending: Some(value), frames: Vec::new(), payload: Vec::new(), payload_names: Vec::new() }
    }

    fn fail<T>(&self, msg: impl fmt::Display) -> CodecResult<T> {
        Err(HostError::new("parse", format!("{}: {msg}", self.path())))
    }

    /// Consumes and returns the next pending value.
    fn take(&mut self, expected: &str) -> CodecResult<Value> {
        if let Some(value) = self.pending.take() {
            return Ok(value);
        }
        if let Some(ValueSourceFrame { kind: ValueSourceFrameKind::Map(_, value @ Some(_)), .. }) = self.frames.last_mut() {
            return Ok(value.take().unwrap_or(Value::Unit));
        }
        self.fail(format!("expected {expected}, found no value"))
    }

    fn expected<T>(&mut self, expected: &str, found: &Value) -> CodecResult<T> {
        self.fail(format!("expected {expected}, found {}", kind_name(found)))
    }

    fn open(&mut self, kind: ValueSourceFrameKind) {
        self.frames.push(ValueSourceFrame { kind, segment: None, count: 0 });
    }

    fn peek(&self) -> Option<&Value> {
        match (&self.pending, self.frames.last()) {
            (Some(value), _) => Some(value),
            (None, Some(ValueSourceFrame { kind: ValueSourceFrameKind::Map(_, Some(value)), .. })) => Some(value),
            _ => None,
        }
    }
}

fn kind_name(value: &Value) -> String {
    match value {
        Value::Unit => "()".into(),
        Value::Bool(_) => "bool".into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Duration(_) => "duration".into(),
        Value::Str(_) => "str".into(),
        Value::Bytes(_) => "bytes".into(),
        Value::List(_) => "a list".into(),
        Value::Tuple(_) => "a tuple".into(),
        Value::Set(_) => "a set".into(),
        Value::Map(_) => "a map".into(),
        Value::Range { .. } => "a range".into(),
        Value::Struct(struct_val) => format!("`{}`", struct_val.ty.type_name),
        Value::Variant(variant_val) => format!("`{}.{}`", variant_val.variant.type_name, variant_val.variant.name),
        Value::Native(native_val) => format!("`{}`", native_val.type_name),
    }
}

impl DecodeSource for ValueSource {
    fn kind(&mut self) -> CodecResult<InputKind> {
        Ok(match self.peek() {
            Some(Value::Unit) => InputKind::Null,
            Some(Value::Variant(value)) if value.variant.type_name == "Option" && value.variant.name == "None" => InputKind::Null,
            Some(Value::Bool(_)) => InputKind::Bool,
            Some(Value::Int(_)) => InputKind::Int,
            Some(Value::Float(_)) => InputKind::Float,
            Some(Value::Duration(_)) => InputKind::Duration,
            Some(Value::Str(_)) => InputKind::Str,
            Some(Value::Bytes(_)) => InputKind::Bytes,
            Some(Value::List(_) | Value::Set(_)) => InputKind::List,
            Some(Value::Map(_)) => InputKind::Map,
            Some(Value::Tuple(_)) => InputKind::Tuple,
            Some(Value::Struct(_)) => InputKind::Struct,
            Some(Value::Range { .. }) => InputKind::Range,
            Some(Value::Native(_)) => InputKind::Native,
            Some(Value::Variant(_)) => InputKind::Variant,
            None => return self.fail("expected a value"),
        })
    }

    fn unit(&mut self) -> CodecResult {
        match self.take("()")? {
            Value::Unit => Ok(()),
            value => self.expected("()", &value),
        }
    }

    fn bool(&mut self) -> CodecResult<bool> {
        match self.take("bool")? {
            Value::Bool(flag) => Ok(flag),
            value => self.expected("bool", &value),
        }
    }

    fn int(&mut self) -> CodecResult<i64> {
        match self.take("int")? {
            Value::Int(num) => Ok(num),
            value => self.expected("int", &value),
        }
    }

    fn float(&mut self) -> CodecResult<f64> {
        match self.take("float")? {
            Value::Float(num) => Ok(num),
            value => self.expected("float", &value),
        }
    }

    fn duration(&mut self) -> CodecResult<i64> {
        match self.take("duration")? {
            Value::Duration(duration) => match i64::try_from(duration.as_nanos()) {
                Ok(nanos) => Ok(nanos),
                Err(_) => self.fail("duration out of range"),
            },
            value => self.expected("duration", &value),
        }
    }

    fn str(&mut self) -> CodecResult<Cow<'_, str>> {
        match self.take("str")? {
            Value::Str(text) => Ok(Cow::Owned(text)),
            value => self.expected("str", &value),
        }
    }

    fn bytes(&mut self) -> CodecResult<Cow<'_, [u8]>> {
        match self.take("bytes")? {
            Value::Bytes(bytes) => Ok(Cow::Owned(bytes)),
            value => self.expected("bytes", &value),
        }
    }

    fn range(&mut self) -> CodecResult<(i64, i64, bool)> {
        match self.take("a range")? {
            Value::Range { start, end, inclusive } => Ok((start, end, inclusive)),
            value => self.expected("a range", &value),
        }
    }

    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        match self.take(type_name)? {
            Value::Native(native_val) if &*native_val.type_name == type_name => Ok(Cow::Owned(native_val.text)),
            value => self.expected(&format!("`{type_name}`"), &value),
        }
    }

    fn option(&mut self) -> CodecResult<bool> {
        match self.take("an option")? {
            Value::Variant(variant_val) if variant_val.variant.type_name == "Option" && variant_val.variant.name == "None" => Ok(false),
            Value::Variant(variant_val)
                if variant_val.variant.type_name == "Option" && variant_val.variant.name == "Some" && variant_val.payload.len() == 1 =>
            {
                self.pending = variant_val.payload.into_vec().pop();
                self.open(ValueSourceFrameKind::Some);
                Ok(true)
            }
            value => self.expected("an option", &value),
        }
    }

    fn begin_list(&mut self) -> CodecResult {
        match self.take("a list")? {
            Value::List(items) | Value::Set(items) => {
                self.open(ValueSourceFrameKind::Items(items.into_iter()));
                Ok(())
            }
            value => self.expected("a list", &value),
        }
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        match self.take("a tuple")? {
            Value::Tuple(items) if items.len() == len => {
                self.open(ValueSourceFrameKind::Items(items.into_iter()));
                Ok(())
            }
            value => self.expected(&format!("a tuple of {len} elements"), &value),
        }
    }

    fn begin_map(&mut self) -> CodecResult {
        match self.take("a map")? {
            Value::Map(entries) => {
                self.open(ValueSourceFrameKind::Map(entries.into_iter(), None));
                Ok(())
            }
            value => self.expected("a map", &value),
        }
    }

    fn next(&mut self) -> CodecResult<bool> {
        let Some(frame) = self.frames.last_mut() else { return Ok(false) };
        let (next, segment) = match &mut frame.kind {
            ValueSourceFrameKind::Items(items) => match items.next() {
                Some(value) => (value, Some(format!("[{}]", frame.count))),
                None => return Ok(false),
            },
            ValueSourceFrameKind::Map(entries, pending_value) => match entries.next() {
                Some((key, value)) => {
                    let segment = match &key {
                        Value::Str(text) => format!("[{text:?}]"),
                        _ => format!("[{}]", frame.count),
                    };
                    *pending_value = Some(value);
                    (key, Some(segment))
                }
                None => return Ok(false),
            },
            _ => return Err(Fail::internal("value: `next` outside a container").into()),
        };
        frame.count += 1;
        frame.segment = segment;
        self.pending = Some(next);
        Ok(true)
    }

    fn begin_struct(&mut self, desc: &StructDesc) -> CodecResult {
        match self.take(&desc.type_name)? {
            Value::Struct(struct_val) if struct_val.ty.type_name == desc.type_name => {
                self.open(ValueSourceFrameKind::Fields(struct_val.ty.fields.clone(), struct_val.fields.into_vec().into_iter()));
                Ok(())
            }
            value => self.expected(&format!("`{}`", desc.type_name), &value),
        }
    }

    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>> {
        let Some(frame) = self.frames.last_mut() else { return Ok(None) };
        let ValueSourceFrameKind::Fields(names, values) = &mut frame.kind else {
            return Err(Fail::internal("value: a field outside a struct").into());
        };
        let Some(value) = values.next() else { return Ok(None) };
        let name = names.get(frame.count).cloned().unwrap_or_default();
        frame.count += 1;
        frame.segment = Some(format!(".{name}"));
        self.pending = Some(value);
        Ok(Some(Cow::Owned(name)))
    }

    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        match self.take(type_name)? {
            Value::Variant(variant_val) if variant_val.variant.type_name == type_name => {
                self.payload = variant_val.payload.into_vec();
                self.payload_names = variant_val.variant.fields.clone().unwrap_or_default();
                self.open(ValueSourceFrameKind::Variant);
                Ok(Cow::Owned(variant_val.variant.name.clone()))
            }
            value => self.expected(&format!("a variant of `{type_name}`"), &value),
        }
    }

    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult {
        let payload = std::mem::take(&mut self.payload);
        match form {
            PayloadForm::Unit if payload.is_empty() => Ok(()),
            PayloadForm::Positional(len) if payload.len() == len => {
                self.open(ValueSourceFrameKind::Items(payload.into_iter()));
                Ok(())
            }
            PayloadForm::Named => {
                let names = std::mem::take(&mut self.payload_names);
                self.open(ValueSourceFrameKind::Fields(names, payload.into_iter()));
                Ok(())
            }
            _ => {
                let want = match form {
                    PayloadForm::Unit => "no payload".to_string(),
                    PayloadForm::Positional(len) => format!("{len} payload values"),
                    PayloadForm::Named => "named payload fields".to_string(),
                };
                self.fail(format!("expected {want}, found {} payload values", payload.len()))
            }
        }
    }

    fn skip(&mut self) -> CodecResult {
        self.take("a value").map(drop)
    }

    fn end(&mut self) -> CodecResult {
        let frame = self.frames.pop();
        if let Some(ValueSourceFrame { kind: ValueSourceFrameKind::Items(..) | ValueSourceFrameKind::Fields(..), .. }) = frame
            && let Some(ValueSourceFrame { kind: ValueSourceFrameKind::Variant, .. }) = self.frames.last()
        {
            self.frames.pop();
        }
        Ok(())
    }

    fn path(&self) -> String {
        let mut path = String::from("$");
        for frame in &self.frames {
            if let Some(segment) = &frame.segment {
                path.push_str(segment);
            }
        }
        path
    }

    fn take_consumed_bytes(&mut self) -> usize {
        0
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
