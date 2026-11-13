//! Sand values in a Rust form that is independent of the VM.

use crate::native::NamedType;
use crate::native::ty::Never;
use crate::stdlib::json::Json;
use crate::vm::bytecode::Program;
use crate::vm::container_children::{ChildRole, ContainerChildren, INLINE_OPEN_CONTAINERS, MAX_NESTING_DEPTH};
use crate::vm::heap::Heap;
use crate::vm::meter::{Meter, Scratch};
use crate::vm::value::{FxHashMap, Obj, ValueRef, ValueType, VmValue};
use crate::vm::{Fail, Vm, fail, internal};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

/// The Rust representation of a Sand value.
///
/// Any part whose type does not implement the Sand `Encode` trait becomes
/// [`Value::Opaque`]. A `Value` is independent of the VM and remains valid
/// after a run ends.
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
    /// A value whose type does not implement the Sand `Encode` trait, such as a
    /// function, task, channel, or native object.
    Opaque { type_name: Arc<str> },
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
    /// # use sandlang::value::{Encode, Value};
    /// let start = jiff::civil::date(2024, 3, 10).encode();
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

/// Converts a Rust value into a [`Value`]. This trait is the Rust counterpart
/// of the Sand `Encode` trait.
///
/// Common Rust types and native value types implement `Encode`.
/// [`#[derive(SandStruct)]`](crate::SandStruct) and
/// [`#[derive(SandEnum)]`](crate::SandEnum) derive it for structs and enums.
/// For a derived type, `encode` returns the same `Value` that [`Vm::to_value`]
/// copies from the VM. A manual `Encode` implementation for a type that also
/// converts to `VmValue` must preserve this equality.
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
/// assert_eq!(point.ty.type_name, "geo.Point");
/// assert_eq!(point.field("y"), Some(&Value::Float(2.0)));
/// ```
///
/// Add `#[sand(no_value)]` to a derived struct or enum if a field type such as
/// `VmVal` does not implement `Encode`. The attribute also omits [`Decode`].
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not implement `sandlang::value::Encode`",
    note = "add `#[sand(no_value)]` to a `SandStruct` or `SandEnum` whose fields do not implement `Encode`"
)]
pub trait Encode {
    /// Converts `self` into a [`Value`].
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
        Value::Variant(VariantValue { variant: desc.get_or_init(|| VariantDesc::new("Option", name)).clone(), payload })
    }
}

/// Sorts entries by key for deterministic iteration order.
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

/// Sorts elements for deterministic iteration order.
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

/// Converts a [`Value`] into a Rust value. This trait is the Rust counterpart
/// of the Sand `Decode` trait.
///
/// `decode` accepts the `Value` that [`Encode::encode`] produces. It also
/// accepts the form that parsing JSON text produces, so a `Value` that comes
/// from JSON is valid input:
///
/// | Type                       | JSON form                                     |
/// | -------------------------- | --------------------------------------------- |
/// | `int`, `duration`          | an integral `Float`; `Int` nanoseconds for `duration` |
/// | `float`                    | `Int`                                         |
/// | `bytes`                    | standard Base64 `Str`                         |
/// | `Option`                   | `Unit` for `None`, and the payload for `Some` |
/// | tuple, `Set`               | `List`                                        |
/// | map                        | `Map` with `Str` keys, such as `"1"` for `1`  |
/// | range, struct              | `Map` with `Str` keys                         |
/// | enum variant               | `Str` name, or a single-entry `Map` from the name to a `List` or `Map` payload |
/// | native value type          | `Str` text                                    |
///
/// A `json.Json` variant is valid input in its JSON form. Decoding ignores the
/// type names of structs, variants, and native values. It matches struct
/// fields and variants by name, and ignores unknown struct fields, as
/// `json.decode` does. A missing `Option` field decodes as `None`.
///
/// Common Rust types and native value types implement `Decode`.
/// [`#[derive(SandStruct)]`](crate::SandStruct) and
/// [`#[derive(SandEnum)]`](crate::SandEnum) derive it for structs and enums.
///
/// ```
/// # use sandlang::value::{Decode, Encode, Value};
/// /// A point in the plane.
/// #[derive(Debug, PartialEq, sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// pub struct Point {
///     pub x: f64,
///     pub y: f64,
/// }
///
/// let point = Point { x: 1.0, y: 2.0 };
/// assert_eq!(point.encode().decode::<Point>(), Ok(Point { x: 1.0, y: 2.0 }));
///
/// let json = Value::Map(vec![(Value::Str("x".into()), Value::Int(1)), (Value::Str("y".into()), Value::Int(2))]);
/// assert_eq!(json.decode::<Point>(), Ok(Point { x: 1.0, y: 2.0 }));
///
/// let error = Value::Map(vec![(Value::Str("x".into()), Value::Unit)]).decode::<Point>().unwrap_err();
/// assert_eq!(error.to_string(), "$.x: expected float, found ()");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not implement `sandlang::value::Decode`",
    note = "add `#[sand(no_value)]` to a `SandStruct` or `SandEnum` whose fields do not implement `Decode`"
)]
pub trait Decode: Encode + Sized {
    /// Converts `value` into `Self`.
    fn decode(value: &Value) -> Result<Self, DecodeError>;

    /// Converts the map key `key` into `Self`. The default calls
    /// [`Decode::decode`]. A type whose JSON object key is text, such as
    /// `int` with the key `"1"`, also accepts that `Value::Str`.
    fn decode_key(key: &Value) -> Result<Self, DecodeError> {
        Self::decode(key)
    }

    /// Returns the value of an omitted struct field of type `Self`, or `None`
    /// if the field is required. `Option` returns `Some(None)`.
    fn decode_missing_field() -> Option<Self> {
        None
    }
}

impl Value {
    /// Converts `self` into a `T`. See [`Decode`].
    ///
    /// ```
    /// # use sandlang::value::Value;
    /// assert_eq!(Value::Int(3).decode::<f64>(), Ok(3.0));
    /// assert_eq!(Value::Str("3".into()).decode::<i64>().unwrap_err().to_string(), "$: expected int, found str");
    /// ```
    pub fn decode<T: Decode>(&self) -> Result<T, DecodeError> {
        T::decode(self)
    }
}

/// An error from [`Decode::decode`]: a part of a [`Value`] does not match the
/// target type.
///
/// `Display` writes the path of the part and the message, such as
/// `$.items[2].price: expected float, found str`. `json.decode` writes the same
/// paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    message: String,
    /// The path segments, from the innermost to the outermost.
    path: Vec<PathSegment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathSegment {
    Index(usize),
    Key(String),
}

impl DecodeError {
    /// Builds an error with `message` for the value that a [`Decode`]
    /// implementation reads.
    pub fn new(message: impl Into<String>) -> DecodeError {
        DecodeError { message: message.into(), path: Vec::new() }
    }

    /// Builds the error for a value `found` that is not of the type
    /// `expected`, such as "expected int, found str".
    pub fn expected(expected: &str, found: &Value) -> DecodeError {
        DecodeError::new(format!("expected {expected}, found {}", kind_name(found)))
    }

    /// Moves the error into the item `index` of a list, tuple, set, or
    /// positional payload.
    pub fn in_index(mut self, index: usize) -> DecodeError {
        self.path.push(PathSegment::Index(index));
        self
    }

    /// Moves the error into the struct field, payload field, or map entry
    /// `key`.
    pub fn in_key(mut self, key: impl Into<String>) -> DecodeError {
        self.path.push(PathSegment::Key(key.into()));
        self
    }

    /// Returns the message without the path, such as "expected int, found
    /// str".
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the path of the part that does not match, such as
    /// `$.items[2].price`.
    pub fn path(&self) -> String {
        let mut out = String::from("$");
        for segment in self.path.iter().rev() {
            match segment {
                PathSegment::Index(i) => {
                    let _ = fmt::Write::write_fmt(&mut out, format_args!("[{i}]"));
                }
                PathSegment::Key(key) => write_key_segment(&mut out, key),
            }
        }
        out
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path(), self.message)
    }
}

impl std::error::Error for DecodeError {}

/// The most characters of a key that a path shows.
const PATH_KEY_CHARS: usize = 64;

/// Writes the path segment for the object key `key`, such as `.price` or
/// `["unit price"]`.
pub(crate) fn write_key_segment(out: &mut String, key: &str) {
    let key = crate::stdlib::truncate(key, PATH_KEY_CHARS);
    if crate::syntax::lexer::is_ident(key) {
        out.push('.');
        out.push_str(key);
    } else {
        out.push('[');
        crate::stdlib::json::write_str(out, key);
        out.push(']');
    }
}

/// Returns the JSON object key text of the map key `key`, for an error path.
pub(crate) fn key_text(key: &Value) -> Cow<'_, str> {
    match without_json_variant(key) {
        Value::Str(s) => Cow::Borrowed(s),
        Value::Int(i) => Cow::Owned(i.to_string()),
        Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        Value::Duration(d) => Cow::Owned(d.as_nanos().to_string()),
        Value::Variant(v) => Cow::Borrowed(&v.variant.name),
        Value::Native(native) => Cow::Borrowed(&native.text),
        Value::Tuple(items) => Cow::Owned(format!("({})", items.iter().map(key_text).collect::<Vec<_>>().join(", "))),
        key => kind_name(key),
    }
}

fn invalid_map_key(text: &str) -> DecodeError {
    DecodeError::new(format!("invalid map key `{}`", crate::stdlib::truncate(text, PATH_KEY_CHARS)))
}

impl Decode for Never {
    fn decode(value: &Value) -> Result<Never, DecodeError> {
        Err(DecodeError::expected("no value", value))
    }
}

impl Decode for Value {
    fn decode(value: &Value) -> Result<Value, DecodeError> {
        Ok(value.clone())
    }
}

impl Decode for () {
    fn decode(value: &Value) -> Result<(), DecodeError> {
        match without_json_variant(value) {
            Value::Unit => Ok(()),
            value => Err(DecodeError::expected("()", value)),
        }
    }
}

impl Decode for bool {
    fn decode(value: &Value) -> Result<bool, DecodeError> {
        match without_json_variant(value) {
            Value::Bool(b) => Ok(*b),
            value => Err(DecodeError::expected("bool", value)),
        }
    }

    fn decode_key(key: &Value) -> Result<bool, DecodeError> {
        match without_json_variant(key) {
            Value::Str(text) if text == "true" => Ok(true),
            Value::Str(text) if text == "false" => Ok(false),
            Value::Str(text) => Err(invalid_map_key(text)),
            key => bool::decode(key),
        }
    }
}

/// Converts the integer or integral float `value` into an `i64`.
fn decode_int(value: &Value, expected: &str) -> Result<i64, DecodeError> {
    match without_json_variant(value) {
        Value::Int(i) => Ok(*i),
        Value::Float(f) => match integral_float_to_int(*f) {
            Some(i) => Ok(i),
            None if f.is_finite() && f.fract() == 0.0 => Err(DecodeError::new("integer out of range")),
            None => Err(DecodeError::expected(expected, value)),
        },
        value => Err(DecodeError::expected(expected, value)),
    }
}

/// Converts the map key text `key` into an `i64`.
fn decode_int_key(key: &Value) -> Option<Result<i64, DecodeError>> {
    match without_json_variant(key) {
        Value::Str(text) => Some(text.parse().map_err(|_| invalid_map_key(text))),
        _ => None,
    }
}

impl Decode for i64 {
    fn decode(value: &Value) -> Result<i64, DecodeError> {
        decode_int(value, "int")
    }

    fn decode_key(key: &Value) -> Result<i64, DecodeError> {
        decode_int_key(key).unwrap_or_else(|| i64::decode(key))
    }
}

impl Decode for f64 {
    fn decode(value: &Value) -> Result<f64, DecodeError> {
        match without_json_variant(value) {
            Value::Float(f) => Ok(*f),
            Value::Int(i) => Ok(*i as f64),
            value => Err(DecodeError::expected("float", value)),
        }
    }
}

impl Decode for jiff::SignedDuration {
    fn decode(value: &Value) -> Result<jiff::SignedDuration, DecodeError> {
        match without_json_variant(value) {
            Value::Duration(d) => Ok(*d),
            value => decode_int(value, "duration").map(jiff::SignedDuration::from_nanos),
        }
    }

    fn decode_key(key: &Value) -> Result<jiff::SignedDuration, DecodeError> {
        match decode_int_key(key) {
            Some(nanos) => nanos.map(jiff::SignedDuration::from_nanos),
            None => jiff::SignedDuration::decode(key),
        }
    }
}

impl Decode for String {
    fn decode(value: &Value) -> Result<String, DecodeError> {
        match without_json_variant(value) {
            Value::Str(s) => Ok(s.clone()),
            value => Err(DecodeError::expected("str", value)),
        }
    }
}

impl Decode for Vec<u8> {
    fn decode(value: &Value) -> Result<Vec<u8>, DecodeError> {
        match without_json_variant(value) {
            Value::Bytes(b) => Ok(b.clone()),
            Value::Str(text) => crate::stdlib::base64::decode(text, false).map_err(|_| DecodeError::new("invalid base64")),
            value => Err(DecodeError::expected("bytes", value)),
        }
    }
}

/// Decodes each item of `items` with its index in the error path.
fn decode_items<'v, T: Decode>(items: impl IntoIterator<Item = &'v Value>) -> impl Iterator<Item = Result<T, DecodeError>> {
    items.into_iter().enumerate().map(|(i, item)| T::decode(item).map_err(|e| e.in_index(i)))
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(value: &Value) -> Result<Vec<T>, DecodeError> {
        match without_json_variant(value) {
            Value::List(items) => decode_items(items).collect(),
            value => Err(DecodeError::expected("list", value)),
        }
    }
}

impl<T: Decode> Decode for Box<T> {
    fn decode(value: &Value) -> Result<Box<T>, DecodeError> {
        T::decode(value).map(Box::new)
    }

    fn decode_key(key: &Value) -> Result<Box<T>, DecodeError> {
        T::decode_key(key).map(Box::new)
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(value: &Value) -> Result<Option<T>, DecodeError> {
        match without_json_variant(value) {
            Value::Unit => Ok(None),
            Value::Variant(v) if is_option_variant(v) => match &*v.payload {
                [] => Ok(None),
                [payload] => T::decode(payload).map(Some),
                _ => T::decode(value).map(Some),
            },
            value => T::decode(value).map(Some),
        }
    }

    fn decode_missing_field() -> Option<Option<T>> {
        Some(None)
    }
}

/// Decodes the entries of the map `value` into `C`.
fn decode_map<K: Decode, V: Decode, C: FromIterator<(K, V)>>(value: &Value) -> Result<C, DecodeError> {
    let Value::Map(entries) = without_json_variant(value) else { return Err(DecodeError::expected("map", value)) };
    entries.iter().map(|(k, v)| Ok((K::decode_key(k)?, V::decode(v).map_err(|e| e.in_key(key_text(k)))?))).collect()
}

impl<K: Decode + Ord + std::hash::Hash, V: Decode> Decode for HashMap<K, V> {
    fn decode(value: &Value) -> Result<HashMap<K, V>, DecodeError> {
        decode_map(value)
    }
}

impl<K: Decode + Ord, V: Decode> Decode for BTreeMap<K, V> {
    fn decode(value: &Value) -> Result<BTreeMap<K, V>, DecodeError> {
        decode_map(value)
    }
}

impl<K: Decode + std::hash::Hash + Eq, V: Decode> Decode for indexmap::IndexMap<K, V> {
    fn decode(value: &Value) -> Result<indexmap::IndexMap<K, V>, DecodeError> {
        decode_map(value)
    }
}

/// Decodes the elements of the set or list `value` into `C`.
fn decode_set<T: Decode, C: FromIterator<T>>(value: &Value) -> Result<C, DecodeError> {
    match without_json_variant(value) {
        Value::Set(items) | Value::List(items) => decode_items(items).collect(),
        value => Err(DecodeError::expected("set", value)),
    }
}

impl<T: Decode + Ord + std::hash::Hash> Decode for HashSet<T> {
    fn decode(value: &Value) -> Result<HashSet<T>, DecodeError> {
        decode_set(value)
    }
}

impl<T: Decode + Ord> Decode for BTreeSet<T> {
    fn decode(value: &Value) -> Result<BTreeSet<T>, DecodeError> {
        decode_set(value)
    }
}

/// Returns the items of the tuple or list `value` if it has `len` items.
fn tuple_items(value: &Value, len: usize) -> Result<&[Value], DecodeError> {
    match without_json_variant(value) {
        Value::Tuple(items) | Value::List(items) if items.len() == len => Ok(items),
        Value::Tuple(_) | Value::List(_) => Err(DecodeError::new(format!("expected list of length {len}"))),
        value => Err(DecodeError::expected("tuple", value)),
    }
}

macro_rules! decode_tuples {
    ($($len:literal: ($($n:ident $i:tt),+)),* $(,)?) => {$(
        impl<$($n: Decode),+> Decode for ($($n,)+) {
            fn decode(value: &Value) -> Result<Self, DecodeError> {
                let items = tuple_items(value, $len)?;
                Ok(($($n::decode(&items[$i]).map_err(|e| e.in_index($i))?,)+))
            }
        }
    )*};
}
decode_tuples! {
    2: (A 0, B 1),
    3: (A 0, B 1, C 2),
    4: (A 0, B 1, C 2, D 3),
    5: (A 0, B 1, C 2, D 3, E 4),
    6: (A 0, B 1, C 2, D 3, E 4, F 5),
    7: (A 0, B 1, C 2, D 3, E 4, F 5, G 6),
    8: (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7),
}

/// Returns the JSON form of `value` if it is a `json.Json` variant, such as
/// `Value::Int(1)` for `Json.Int(1)`, or `value` otherwise.
pub(crate) fn without_json_variant(mut value: &Value) -> &Value {
    static UNIT: Value = Value::Unit;
    while let Value::Variant(v) = value
        && is_qualified_name(&v.variant.type_name, Some(<Json as NamedType>::MODULE), <Json as NamedType>::NAME)
    {
        value = v.payload.first().unwrap_or(&UNIT);
    }
    value
}

/// Returns `true` if `v` is a variant of the prelude `Option` type.
pub(crate) fn is_option_variant(v: &VariantValue) -> bool {
    v.variant.type_name == "Option"
}

/// Returns the name of the kind of `value` for an error message, such as
/// `str`, `list`, or `shop.Order`.
pub(crate) fn kind_name(value: &Value) -> Cow<'_, str> {
    Cow::Borrowed(match value {
        Value::Unit => "()",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Duration(_) => "duration",
        Value::Str(_) => "str",
        Value::Bytes(_) => "bytes",
        Value::List(_) => "list",
        Value::Tuple(_) => "tuple",
        Value::Set(_) => "set",
        Value::Map(_) => "map",
        Value::Range { .. } => "range",
        Value::Struct(s) => &s.ty.type_name,
        Value::Variant(v) => return Cow::Owned(format!("{}.{}", v.variant.type_name, v.variant.name)),
        Value::Native(native) => &native.type_name,
        Value::Opaque { type_name } => type_name,
    })
}

/// Converts `f` to an `int` if it is integral and in the `int` range.
pub(crate) fn integral_float_to_int(f: f64) -> Option<i64> {
    (f.fract() == 0.0 && (-9223372036854775808.0..9223372036854775808.0).contains(&f)).then_some(f as i64)
}

/// Copies `value` from the heap of `vm`. See [`Vm::to_value`].
pub(crate) fn copy_from_vm(vm: &Vm, value: VmValue) -> Result<Value, Fail> {
    let (allocated, scratch, type_names) = (0, Scratch::new(), FxHashMap::default());
    ValueCopier { prog: &vm.prog, heap: &vm.heap, meter: &vm.meter, allocated, scratch, type_names }.copy_value(value)
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

/// Copies a Sand value from the VM heap into a [`Value`].
struct ValueCopier<'h> {
    prog: &'h Program,
    heap: &'h Heap,
    meter: &'h Meter,
    /// Bytes of Rust memory allocated by the copy.
    allocated: usize,
    scratch: Scratch,
    /// Cached qualified names for native value types and opaque types.
    type_names: FxHashMap<(Option<&'static str>, &'static str), Arc<str>>,
}

/// A container being copied and its remaining children.
struct OpenContainer<'h> {
    children: ContainerChildren<'h>,
    partial: PartialValue,
}

/// A [`Value`] container being assembled from its children.
enum PartialValue {
    /// Slot for the root value of the copy.
    Root(Value),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    /// Map entries and a flag that is `true` while the last entry awaits its
    /// value.
    Map(Vec<(Value, Value)>, bool),
    Struct(Arc<StructDesc>, Vec<Value>),
    Variant(Arc<VariantDesc>, Vec<Value>),
}

impl<'h> ValueCopier<'h> {
    /// Copies `root` into a [`Value`]. Charges a fuel unit per value, except
    /// map keys, and charges for copied byte slices and strings.
    fn copy_value(&mut self, root: VmValue) -> Result<Value, Fail> {
        let (prog, heap) = (self.prog, self.heap);
        let root = ContainerChildren::Single(ChildRole::Item, Some(root));
        let mut innermost = OpenContainer { children: root, partial: PartialValue::Root(Value::Unit) };
        let mut parents = SmallVec::<[OpenContainer<'h>; INLINE_OPEN_CONTAINERS]>::new();
        'next_value: loop {
            let Some((role, v)) = innermost.children.next() else {
                let Some(parent) = parents.pop() else { return Ok(innermost.partial.into_value()) };
                let complete = std::mem::replace(&mut innermost, parent).partial;
                innermost.partial.push_child(complete.into_value());
                continue;
            };
            if !matches!(role, ChildRole::MapKey) {
                self.meter.charge(1)?;
            }
            let copy = 'without_children: {
                let container = match v {
                    VmValue::Unit | VmValue::Uninit => break 'without_children Value::Unit,
                    VmValue::Bool(b) => break 'without_children Value::Bool(b),
                    VmValue::Int(i) => break 'without_children Value::Int(i),
                    VmValue::Float(f) => break 'without_children Value::Float(f),
                    VmValue::Dur(nanos) => break 'without_children Value::Duration(jiff::SignedDuration::from_nanos(nanos)),
                    VmValue::Tag(id) => {
                        break 'without_children Value::Variant(VariantValue { variant: self.variant_desc(id)?, payload: Box::default() });
                    }
                    VmValue::Pair(first, second) => {
                        self.meter.charge(2)?;
                        let mut ints = self.value_buffer(2)?;
                        ints.extend([Value::Int(i64::from(first)), Value::Int(second)]);
                        break 'without_children Value::Tuple(ints);
                    }
                    VmValue::Obj(_) => match heap.get(v) {
                        Obj::Str(s) => break 'without_children self.copy_str(s)?,
                        Obj::StrBuf(s) => break 'without_children self.copy_str(s)?,
                        Obj::Bytes(bytes) => {
                            self.reserve_copy(bytes.len(), 0)?;
                            break 'without_children Value::Bytes(bytes.to_vec());
                        }
                        Obj::Range(start, end, inclusive) => {
                            break 'without_children Value::Range { start: *start, end: *end, inclusive: *inclusive };
                        }
                        Obj::Value(native) => break 'without_children self.native_value(ValueRef::Boxed(&**native))?,
                        Obj::Packed(ty, bits) => break 'without_children self.native_value(ValueRef::Packed(ty, *bits))?,
                        Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) => break 'without_children self.opaque_value(None, "fn")?,
                        Obj::Task(_) => break 'without_children self.opaque_value(None, "Task")?,
                        Obj::Channel(_) => break 'without_children self.opaque_value(None, "Channel")?,
                        Obj::Native(object) => {
                            break 'without_children self.opaque_value(qualified_name_module(object.module_name()), object.type_name())?;
                        }
                        Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_) => {
                            return internal("to_value: unexpected internal heap object");
                        }
                        Obj::List(items) => OpenContainer {
                            children: ContainerChildren::Items(items.iter()),
                            partial: PartialValue::List(self.value_buffer(items.len())?),
                        },
                        Obj::Tuple(items) => OpenContainer {
                            children: ContainerChildren::Items(items.iter()),
                            partial: PartialValue::Tuple(self.value_buffer(items.len())?),
                        },
                        Obj::Set(set) => OpenContainer {
                            children: ContainerChildren::Set(set.entries.iter()),
                            partial: PartialValue::Set(self.value_buffer(set.live)?),
                        },
                        Obj::Map(map) => {
                            self.reserve(map.live.saturating_mul(size_of::<(Value, Value)>()))?;
                            OpenContainer {
                                children: ContainerChildren::Map(map.entries.iter(), None),
                                partial: PartialValue::Map(Vec::with_capacity(map.live), false),
                            }
                        }
                        Obj::Struct(def, values) => {
                            let desc = prog.types[*def as usize].desc().clone();
                            OpenContainer {
                                children: ContainerChildren::Items(values.iter()),
                                partial: PartialValue::Struct(desc, self.value_buffer(values.len())?),
                            }
                        }
                        Obj::Variant(id, values) => {
                            let desc = self.variant_desc(*id)?;
                            OpenContainer {
                                children: ContainerChildren::Items(values.iter()),
                                partial: PartialValue::Variant(desc, self.value_buffer(values.len())?),
                            }
                        }
                    },
                    VmValue::VUnit(..)
                    | VmValue::VBool(..)
                    | VmValue::VInt(..)
                    | VmValue::VFloat(..)
                    | VmValue::VDur(..)
                    | VmValue::VTag(..)
                    | VmValue::VObj(..) => {
                        let Some((id, payload)) = v.as_inline_variant() else { return internal("to_value: not an inline variant") };
                        let desc = self.variant_desc(id)?;
                        OpenContainer {
                            children: ContainerChildren::Single(ChildRole::Item, Some(payload)),
                            partial: PartialValue::Variant(desc, self.value_buffer(1)?),
                        }
                    }
                };
                if parents.len() >= MAX_NESTING_DEPTH {
                    return fail("limit", "value too deeply nested to copy");
                }
                parents.push(std::mem::replace(&mut innermost, container));
                continue 'next_value;
            };
            innermost.partial.push_child(copy);
        }
    }

    /// Reserves memory for `len` values and returns a vector with that
    /// capacity.
    #[inline]
    fn value_buffer(&mut self, len: usize) -> Result<Vec<Value>, Fail> {
        self.reserve(len.saturating_mul(size_of::<Value>()))?;
        Ok(Vec::with_capacity(len))
    }

    /// Counts `bytes` of Rust memory against the memory limit.
    #[inline]
    fn reserve(&mut self, bytes: usize) -> Result<(), Fail> {
        self.allocated = self.allocated.saturating_add(bytes);
        self.scratch.check(self.heap, self.allocated)
    }

    /// Charges fuel for `content` bytes and reserves `content + header` bytes
    /// against the memory limit.
    #[inline]
    fn reserve_copy(&mut self, content: usize, header: usize) -> Result<(), Fail> {
        self.meter.charge_bytes(content)?;
        self.reserve(content.saturating_add(header))
    }

    /// Copies `text` into a `Value::Str`.
    fn copy_str(&mut self, text: &str) -> Result<Value, Fail> {
        self.reserve_copy(text.len(), 0)?;
        Ok(Value::Str(text.to_string()))
    }

    /// Returns the cached qualified name of `name` in `module`.
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

    /// Returns the program descriptor for the variant with runtime ID `id`.
    fn variant_desc(&self, id: u32) -> Result<Arc<VariantDesc>, Fail> {
        match self.prog.variant_desc(id) {
            Some(desc) => Ok(desc.clone()),
            None => internal("to_value: unknown variant ID"),
        }
    }

    /// Copies `native` into a `Value::Native`.
    fn native_value(&mut self, native: ValueRef<'_>) -> Result<Value, Fail> {
        let text = native.text();
        self.reserve_copy(text.len(), size_of::<NativeValue>())?;
        let type_name = self.type_name(qualified_name_module(native.module_name()), native.type_name())?;
        Ok(Value::Native(Box::new(NativeValue { type_name, text })))
    }

    /// Builds a `Value::Opaque` for the type `name` in `module`.
    fn opaque_value(&mut self, module: Option<&'static str>, name: &'static str) -> Result<Value, Fail> {
        Ok(Value::Opaque { type_name: self.type_name(module, name)? })
    }
}

impl PartialValue {
    /// Appends `value` as the next child.
    #[inline]
    fn push_child(&mut self, value: Value) {
        match self {
            PartialValue::Root(root) => *root = value,
            PartialValue::List(items)
            | PartialValue::Tuple(items)
            | PartialValue::Set(items)
            | PartialValue::Struct(_, items)
            | PartialValue::Variant(_, items) => items.push(value),
            PartialValue::Map(entries, has_key) => {
                match entries.last_mut() {
                    Some(entry) if *has_key => entry.1 = value,
                    _ => entries.push((value, Value::Unit)),
                }
                *has_key = !*has_key;
            }
        }
    }

    /// Converts a completed container into a [`Value`].
    fn into_value(self) -> Value {
        match self {
            PartialValue::Root(root) => root,
            PartialValue::List(items) => Value::List(items),
            PartialValue::Tuple(items) => Value::Tuple(items),
            PartialValue::Set(items) => Value::Set(items),
            PartialValue::Map(entries, _) => Value::Map(entries),
            PartialValue::Struct(ty, fields) => Value::Struct(StructValue { ty, fields: fields.into_boxed_slice() }),
            PartialValue::Variant(variant, payload) => Value::Variant(VariantValue { variant, payload: payload.into_boxed_slice() }),
        }
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
