//! Sand values in a Rust form that is independent of the VM.

use crate::native::codec::{CodecResult, DecodeSource, EncodeSink, InputKind, PayloadForm};
use crate::native::convert::HostError;
use crate::native::ty::Never;
use crate::vm::Fail;
use crate::vm::value::{FxHashMap, ValueType};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

/// The Rust representation of a Sand value of a type that implements
/// `Encode` or `Decode`.
///
/// A `Value` is independent of the VM and remains valid after a run ends.
/// [`NativeContext::to_value`](crate::native::context::NativeContext::to_value)
/// encodes a Sand value as a `Value`, and
/// [`NativeContext::from_value`](crate::native::context::NativeContext::from_value)
/// decodes one.
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
    /// A value of a native value type, such as `time.PlainDate`.
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
    /// Parses `self` as the native value type `T`, or returns `None` if `self`
    /// is not of type `T`.
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

/// Converts a Rust value into a [`Value`] constant, such as the default of a
/// field of a native struct (`#[sand(default)]`).
///
/// Common Rust types and native value types implement `ToValue`.
/// [`#[derive(SandStruct)]`](crate::SandStruct) and
/// [`#[derive(SandEnum)]`](crate::SandEnum) derive it for structs and enums.
/// A derived `to_value` returns the structure of the Rust value. It does
/// not apply the `Encode` impls of Sand code.
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
/// Add `#[sand(no_value)]` to a derived struct or enum if a field type such as
/// `VmVal` does not implement `ToValue`.
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
            Some(x) => (&SOME, "Some", Box::new([x.to_value()])),
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
    Value::Map(entries.into_iter().map(|(k, v)| (k.to_value(), v.to_value())).collect())
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

/// Bytes counted for each [`Value`] that a [`ValueSink`] builds.
const VALUE_BYTES: usize = size_of::<Value>();

/// The maximum nesting depth of a [`Value`] that a [`ValueSink`] builds,
/// counting each `Some`. The default drop of a `Value` recurses once per
/// level.
const MAX_VALUE_DEPTH: usize = 512;

/// An open container of a [`ValueSink`].
enum Partial {
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    /// Map entries, and whether the last entry has its key but not its value.
    Map(Vec<(Value, Value)>, bool),
    Struct(Arc<StructDesc>, Vec<Value>),
    Variant(Arc<VariantDesc>, Vec<Value>),
    Some(Vec<Value>),
}

/// Builds a [`Value`] from the encode walker.
pub(crate) struct ValueSink {
    pub root: Option<Value>,
    open: Vec<Partial>,
    allocated: usize,
    /// The most bytes that the sink can allocate within the memory limit.
    max_allocated: usize,
    /// Cached qualified names of native value types.
    type_names: FxHashMap<String, Arc<str>>,
}

impl Default for ValueSink {
    fn default() -> ValueSink {
        ValueSink { root: None, open: Vec::new(), allocated: 0, max_allocated: usize::MAX, type_names: FxHashMap::default() }
    }
}

fn option_desc(some: bool) -> Arc<VariantDesc> {
    static SOME: OnceLock<Arc<VariantDesc>> = OnceLock::new();
    static NONE: OnceLock<Arc<VariantDesc>> = OnceLock::new();
    let (cell, name) = if some { (&SOME, "Some") } else { (&NONE, "None") };
    cell.get_or_init(|| VariantDesc::new("Option", name)).clone()
}

impl ValueSink {
    /// Counts `bytes` more against the memory limit.
    fn grow(&mut self, bytes: usize) -> CodecResult {
        self.allocated = self.allocated.saturating_add(bytes);
        if self.allocated > self.max_allocated {
            return crate::native::codec::memory_limit();
        }
        Ok(())
    }

    fn push(&mut self, value: Value) -> CodecResult {
        self.grow(VALUE_BYTES)?;
        match self.open.last_mut() {
            None => self.root = Some(value),
            Some(
                Partial::List(items)
                | Partial::Tuple(items)
                | Partial::Set(items)
                | Partial::Struct(_, items)
                | Partial::Variant(_, items)
                | Partial::Some(items),
            ) => items.push(value),
            Some(Partial::Map(entries, has_key)) => {
                match entries.last_mut() {
                    Some(entry) if *has_key => entry.1 = value,
                    _ => entries.push((value, Value::Unit)),
                }
                *has_key = !*has_key;
            }
        }
        Ok(())
    }

    /// Opens a container of `len` parts, which `partial` builds in a vector
    /// with that capacity.
    fn open<T>(&mut self, len: usize, partial: impl FnOnce(Vec<T>) -> Partial) -> CodecResult {
        if self.open.len() >= MAX_VALUE_DEPTH {
            return Err(Fail::runtime_error("limit", "value too deeply nested to encode as a host value").into());
        }
        self.grow(len.saturating_mul(size_of::<T>()))?;
        self.open.push(partial(Vec::with_capacity(len)));
        Ok(())
    }
}

impl crate::vm::codec::WalkSink for ValueSink {
    fn set_max_allocated(&mut self, bytes: usize) {
        self.max_allocated = bytes;
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
            Some(n) => n.clone(),
            None => {
                self.grow(2 * type_name.len())?;
                let n: Arc<str> = type_name.into();
                self.type_names.insert(type_name.to_string(), n.clone());
                n
            }
        };
        self.grow(text.len() + size_of::<NativeValue>())?;
        self.push(Value::Native(Box::new(NativeValue { type_name, text: text.to_string() })))
    }

    fn none(&mut self) -> CodecResult {
        self.push(Value::Variant(VariantValue { variant: option_desc(false), payload: Box::default() }))
    }

    fn begin_some(&mut self) -> CodecResult {
        self.open(1, Partial::Some)
    }

    fn begin_list(&mut self, len: usize) -> CodecResult {
        self.open(len, Partial::List)
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        self.open(len, Partial::Tuple)
    }

    fn begin_set(&mut self, len: usize) -> CodecResult {
        self.open(len, Partial::Set)
    }

    fn begin_map(&mut self, len: usize) -> CodecResult {
        self.open(len, |entries| Partial::Map(entries, false))
    }

    fn begin_struct(&mut self, desc: &Arc<StructDesc>) -> CodecResult {
        self.open(desc.fields.len(), |fields| Partial::Struct(desc.clone(), fields))
    }

    fn begin_variant(&mut self, desc: &Arc<VariantDesc>, len: usize) -> CodecResult {
        if len == 0 {
            self.open.push(Partial::Variant(desc.clone(), Vec::new()));
            return Ok(());
        }
        self.open(len, |payload| Partial::Variant(desc.clone(), payload))
    }

    fn end(&mut self) -> CodecResult {
        let value = match self.open.pop() {
            Some(Partial::List(items)) => Value::List(items),
            Some(Partial::Tuple(items)) => Value::Tuple(items),
            Some(Partial::Set(items)) => Value::Set(items),
            Some(Partial::Map(entries, _)) => Value::Map(entries),
            Some(Partial::Struct(ty, fields)) => Value::Struct(StructValue { ty, fields: fields.into() }),
            Some(Partial::Variant(variant, payload)) => Value::Variant(VariantValue { variant, payload: payload.into() }),
            Some(Partial::Some(payload)) => Value::Variant(VariantValue { variant: option_desc(true), payload: payload.into() }),
            None => return Ok(()),
        };
        self.allocated -= VALUE_BYTES.min(self.allocated);
        self.push(value)
    }

    fn allocated(&self) -> usize {
        self.allocated
    }
}

/// An open container of a [`ValueSource`].
struct Level {
    kind: LevelKind,
    /// The path segment of the current member.
    segment: Option<String>,
    count: usize,
}

enum LevelKind {
    /// A list, set, tuple, or positional payload.
    Items(std::vec::IntoIter<Value>),
    /// A map, and the value of the entry whose key the walker reads.
    Map(std::vec::IntoIter<(Value, Value)>, Option<Value>),
    /// The fields of a struct or of a named payload.
    Fields(Vec<String>, std::vec::IntoIter<Value>),
    /// `Some`, whose payload is the value to read next.
    Some,
    /// A variant, whose payload level follows it.
    Variant,
}

/// Reads a [`Value`] for the decode walker. It accepts only the exact form of
/// each Sand type, such as `Value::Bytes` for `bytes`.
pub(crate) struct ValueSource {
    /// The value to read next.
    next: Option<Value>,
    levels: Vec<Level>,
    /// The payload of the variant from the last `begin_variant`, and its
    /// field names.
    payload: Vec<Value>,
    payload_names: Vec<String>,
}

impl ValueSource {
    pub(crate) fn new(value: Value) -> ValueSource {
        ValueSource { next: Some(value), levels: Vec::new(), payload: Vec::new(), payload_names: Vec::new() }
    }

    fn fail<T>(&self, msg: impl fmt::Display) -> CodecResult<T> {
        Err(HostError::new("parse", format!("{}: {msg}", self.path())))
    }

    /// Takes the value to read next.
    fn take(&mut self, what: &str) -> CodecResult<Value> {
        if let Some(v) = self.next.take() {
            return Ok(v);
        }
        if let Some(Level { kind: LevelKind::Map(_, value @ Some(_)), .. }) = self.levels.last_mut() {
            return Ok(value.take().unwrap_or(Value::Unit));
        }
        self.fail(format!("expected {what}, found no value"))
    }

    fn expected<T>(&mut self, what: &str, found: &Value) -> CodecResult<T> {
        self.fail(format!("expected {what}, found {}", kind_name(found)))
    }

    fn open(&mut self, kind: LevelKind) {
        self.levels.push(Level { kind, segment: None, count: 0 });
    }

    fn peek(&self) -> Option<&Value> {
        match (&self.next, self.levels.last()) {
            (Some(v), _) => Some(v),
            (None, Some(Level { kind: LevelKind::Map(_, Some(v)), .. })) => Some(v),
            _ => None,
        }
    }
}

fn kind_name(v: &Value) -> String {
    match v {
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
        Value::Struct(s) => format!("`{}`", s.ty.type_name),
        Value::Variant(v) => format!("`{}.{}`", v.variant.type_name, v.variant.name),
        Value::Native(n) => format!("`{}`", n.type_name),
    }
}

impl DecodeSource for ValueSource {
    fn kind(&mut self) -> CodecResult<InputKind> {
        Ok(match self.peek() {
            Some(Value::Unit) => InputKind::Null,
            Some(Value::Variant(v)) if v.variant.type_name == "Option" && v.variant.name == "None" => InputKind::Null,
            Some(Value::Bool(_)) => InputKind::Bool,
            Some(Value::Int(_)) => InputKind::Int,
            Some(Value::Float(_)) => InputKind::Float,
            Some(Value::Duration(_)) => InputKind::Duration,
            Some(Value::Str(_) | Value::Native(_)) => InputKind::Str,
            Some(Value::Bytes(_)) => InputKind::Bytes,
            Some(Value::List(_) | Value::Tuple(_) | Value::Set(_)) => InputKind::List,
            Some(Value::Map(_) | Value::Struct(_) | Value::Range { .. }) => InputKind::Map,
            Some(Value::Variant(_)) => InputKind::Variant,
            None => return self.fail("expected a value"),
        })
    }

    fn unit(&mut self) -> CodecResult {
        match self.take("()")? {
            Value::Unit => Ok(()),
            v => self.expected("()", &v),
        }
    }

    fn bool(&mut self) -> CodecResult<bool> {
        match self.take("bool")? {
            Value::Bool(b) => Ok(b),
            v => self.expected("bool", &v),
        }
    }

    fn int(&mut self) -> CodecResult<i64> {
        match self.take("int")? {
            Value::Int(i) => Ok(i),
            v => self.expected("int", &v),
        }
    }

    fn float(&mut self) -> CodecResult<f64> {
        match self.take("float")? {
            Value::Float(f) => Ok(f),
            v => self.expected("float", &v),
        }
    }

    fn duration(&mut self) -> CodecResult<i64> {
        match self.take("duration")? {
            Value::Duration(d) => match i64::try_from(d.as_nanos()) {
                Ok(n) => Ok(n),
                Err(_) => self.fail("duration out of range"),
            },
            v => self.expected("duration", &v),
        }
    }

    fn str(&mut self) -> CodecResult<Cow<'_, str>> {
        match self.take("str")? {
            Value::Str(s) => Ok(Cow::Owned(s)),
            v => self.expected("str", &v),
        }
    }

    fn bytes(&mut self) -> CodecResult<Cow<'_, [u8]>> {
        match self.take("bytes")? {
            Value::Bytes(b) => Ok(Cow::Owned(b)),
            v => self.expected("bytes", &v),
        }
    }

    fn range(&mut self) -> CodecResult<(i64, i64, bool)> {
        match self.take("a range")? {
            Value::Range { start, end, inclusive } => Ok((start, end, inclusive)),
            v => self.expected("a range", &v),
        }
    }

    fn native(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        match self.take(type_name)? {
            Value::Native(n) if &*n.type_name == type_name => Ok(Cow::Owned(n.text)),
            v => self.expected(&format!("`{type_name}`"), &v),
        }
    }

    fn option(&mut self) -> CodecResult<bool> {
        match self.take("an option")? {
            Value::Variant(v) if v.variant.type_name == "Option" && v.variant.name == "None" => Ok(false),
            Value::Variant(v) if v.variant.type_name == "Option" && v.variant.name == "Some" && v.payload.len() == 1 => {
                self.next = v.payload.into_vec().pop();
                self.open(LevelKind::Some);
                Ok(true)
            }
            v => self.expected("an option", &v),
        }
    }

    fn begin_list(&mut self) -> CodecResult {
        match self.take("a list")? {
            Value::List(items) | Value::Set(items) => {
                self.open(LevelKind::Items(items.into_iter()));
                Ok(())
            }
            v => self.expected("a list", &v),
        }
    }

    fn begin_tuple(&mut self, len: usize) -> CodecResult {
        match self.take("a tuple")? {
            Value::Tuple(items) if items.len() == len => {
                self.open(LevelKind::Items(items.into_iter()));
                Ok(())
            }
            v => self.expected(&format!("a tuple of {len} elements"), &v),
        }
    }

    fn begin_map(&mut self) -> CodecResult {
        match self.take("a map")? {
            Value::Map(entries) => {
                self.open(LevelKind::Map(entries.into_iter(), None));
                Ok(())
            }
            v => self.expected("a map", &v),
        }
    }

    fn next(&mut self) -> CodecResult<bool> {
        let Some(level) = self.levels.last_mut() else { return Ok(false) };
        let (next, segment) = match &mut level.kind {
            LevelKind::Items(items) => match items.next() {
                Some(v) => (v, Some(format!("[{}]", level.count))),
                None => return Ok(false),
            },
            LevelKind::Map(entries, value) => match entries.next() {
                Some((k, v)) => {
                    let segment = match &k {
                        Value::Str(s) => format!("[{s:?}]"),
                        _ => format!("[{}]", level.count),
                    };
                    *value = Some(v);
                    (k, Some(segment))
                }
                None => return Ok(false),
            },
            _ => return Err(Fail::internal("value: `next` outside a container").into()),
        };
        level.count += 1;
        level.segment = segment;
        self.next = Some(next);
        Ok(true)
    }

    fn begin_struct(&mut self, desc: &StructDesc) -> CodecResult {
        match self.take(&desc.type_name)? {
            Value::Struct(s) if s.ty.type_name == desc.type_name => {
                self.open(LevelKind::Fields(s.ty.fields.clone(), s.fields.into_vec().into_iter()));
                Ok(())
            }
            v => self.expected(&format!("`{}`", desc.type_name), &v),
        }
    }

    fn next_field(&mut self) -> CodecResult<Option<Cow<'_, str>>> {
        let Some(level) = self.levels.last_mut() else { return Ok(None) };
        let LevelKind::Fields(names, values) = &mut level.kind else {
            return Err(Fail::internal("value: a field outside a struct").into());
        };
        let Some(v) = values.next() else { return Ok(None) };
        let name = names.get(level.count).cloned().unwrap_or_default();
        level.count += 1;
        level.segment = Some(format!(".{name}"));
        self.next = Some(v);
        Ok(Some(Cow::Owned(name)))
    }

    fn begin_variant(&mut self, type_name: &str) -> CodecResult<Cow<'_, str>> {
        match self.take(type_name)? {
            Value::Variant(v) if v.variant.type_name == type_name => {
                self.payload = v.payload.into_vec();
                self.payload_names = v.variant.fields.clone().unwrap_or_default();
                self.open(LevelKind::Variant);
                Ok(Cow::Owned(v.variant.name.clone()))
            }
            v => self.expected(&format!("a variant of `{type_name}`"), &v),
        }
    }

    fn begin_payload(&mut self, form: PayloadForm) -> CodecResult {
        let payload = std::mem::take(&mut self.payload);
        match form {
            PayloadForm::Unit if payload.is_empty() => Ok(()),
            PayloadForm::Positional(n) if payload.len() == n => {
                self.open(LevelKind::Items(payload.into_iter()));
                Ok(())
            }
            PayloadForm::Named => {
                let names = std::mem::take(&mut self.payload_names);
                self.open(LevelKind::Fields(names, payload.into_iter()));
                Ok(())
            }
            _ => self.fail(format!("expected {form:?} payload, found {} values", payload.len())),
        }
    }

    fn skip(&mut self) -> CodecResult {
        self.take("a value").map(drop)
    }

    fn end(&mut self) -> CodecResult {
        let level = self.levels.pop();
        if let Some(Level { kind: LevelKind::Items(..) | LevelKind::Fields(..), .. }) = level
            && let Some(Level { kind: LevelKind::Variant, .. }) = self.levels.last()
        {
            self.levels.pop();
        }
        Ok(())
    }

    fn path(&self) -> String {
        let mut s = String::from("$");
        for level in &self.levels {
            if let Some(seg) = &level.segment {
                s.push_str(seg);
            }
        }
        s
    }

    fn allocated(&self) -> usize {
        0
    }

    fn work(&mut self) -> usize {
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
