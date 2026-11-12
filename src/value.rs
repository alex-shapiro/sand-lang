//! Sand values in a Rust form that does not reference the VM.

use crate::native::ty::Never;
use crate::stdlib::time::{PlainMonthDay, PlainYearMonth, Span, TimeZone, ZonedDateTime};
use crate::vm::bytecode::Program;
use crate::vm::encode::{self, Container, EncodeSink, Leaf, OpaqueKind, Opened, Role};
use crate::vm::heap::Heap;
use crate::vm::meter::{Meter, Scratch};
use crate::vm::value::{FxHashMap, ValueRef, ValueType, VmValue};
use crate::vm::{Fail, Vm, internal};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

/// The Rust form of a Sand value.
///
/// Each part of the value whose type does not implement the Sand trait
/// `Encode` is a [`Value::Opaque`]. A `Value` does not reference the VM, so it remains valid
/// after the run ends.
///
/// The `Debug` output shows a struct or a variant with its name, such as
/// `shop.Order { id: Int(1), note: Option.None }`.
#[derive(Clone, PartialEq)]
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
    /// A value of a native value type without a dedicated case, from the
    /// standard library or a host.
    Native(Box<NativeValue>),
    /// A part whose type does not implement the Sand trait `Encode`, such as
    /// a function, a task, a channel, or a native object. It holds only the
    /// type name.
    Opaque { type_name: Arc<str> },
}

/// A struct value: its type and one value for each field.
#[derive(Clone, PartialEq)]
pub struct StructValue {
    /// The type of the struct.
    pub ty: Arc<StructDesc>,
    /// The field values in declaration order.
    pub fields: Box<[Value]>,
}

/// The name and field names of a struct type. All values of the type share it.
#[derive(Debug, PartialEq, Eq)]
pub struct StructDesc {
    /// The module-qualified type name, such as `shop.Order`.
    ///
    /// A type name is a label, not a reference to a type. The module of a
    /// type from the main script is the file stem of the script, such as
    /// `main.Order` for a type of `main.sand`, so a script can declare a type
    /// whose name equals the name of a host type.
    pub type_name: String,
    /// The field names in declaration order.
    pub fields: Vec<String>,
}

/// An enum variant value: its variant and its payload.
#[derive(Clone, PartialEq)]
pub struct VariantValue {
    /// The variant.
    pub variant: Arc<VariantDesc>,
    /// The payload values in declaration order. A variant without a payload
    /// has none.
    pub payload: Box<[Value]>,
}

/// One variant of an enum type. All values of the variant share it.
#[derive(Debug, PartialEq, Eq)]
pub struct VariantDesc {
    /// The module-qualified name of the enum, such as `shop.Status`. The
    /// name is a label, as [`StructDesc::type_name`] describes.
    pub type_name: String,
    /// The variant name, such as `Shipped`.
    pub name: String,
    /// The names of the payload fields, or `None` for a positional payload.
    pub fields: Option<Vec<String>>,
}

impl StructDesc {
    /// Returns the description of the struct type `type_name` with the
    /// fields `fields`.
    pub fn new(type_name: impl Into<String>, fields: impl IntoIterator<Item = impl Into<String>>) -> Arc<StructDesc> {
        Arc::new(StructDesc { type_name: type_name.into(), fields: fields.into_iter().map(Into::into).collect() })
    }
}

impl StructValue {
    /// Returns a value of the struct type `type_name` with `fields`, each a
    /// field name and its value, in declaration order.
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
    /// Returns the description of the variant `name` of the enum `type_name`,
    /// with a positional payload or none.
    pub fn new(type_name: impl Into<String>, name: impl Into<String>) -> Arc<VariantDesc> {
        Arc::new(VariantDesc { type_name: type_name.into(), name: name.into(), fields: None })
    }

    /// Returns the description of the variant `name` of the enum `type_name`,
    /// with the named payload fields `fields`.
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
    /// Returns the variant `name` of the enum `type_name` with the positional
    /// payload `payload`. An empty payload gives a variant without a payload.
    pub fn new(type_name: impl Into<String>, name: impl Into<String>, payload: impl IntoIterator<Item = Value>) -> VariantValue {
        VariantValue { variant: VariantDesc::new(type_name, name), payload: payload.into_iter().collect() }
    }

    /// Returns the variant `name` of the enum `type_name` with the named
    /// payload `fields`, each a field name and its value.
    pub fn with_fields<N: Into<String>>(
        type_name: impl Into<String>,
        name: impl Into<String>,
        fields: impl IntoIterator<Item = (N, Value)>,
    ) -> VariantValue {
        let (names, payload): (Vec<String>, Vec<Value>) = fields.into_iter().map(|(n, v)| (n.into(), v)).unzip();
        VariantValue { variant: VariantDesc::with_fields(type_name, name, names), payload: payload.into() }
    }

    /// Returns the value of the named payload field `name`, or `None` if the
    /// payload has no such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        field_named(self.variant.fields.as_deref().unwrap_or_default(), &self.payload, name)
    }
}

fn field_named<'a>(names: &[String], values: &'a [Value], name: &str) -> Option<&'a Value> {
    values.get(names.iter().position(|n| n == name)?)
}

/// A value of a native value type with its type name and text.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeValue {
    /// The module-qualified type name, such as `bank.Money` for a type
    /// `Money` of a host module `bank`.
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
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (variant, inner): (&str, &dyn fmt::Debug) = match self {
            Value::Unit => return f.write_str("Unit"),
            Value::Range { start, end, inclusive } => {
                return f.debug_struct("Range").field("start", start).field("end", end).field("inclusive", inclusive).finish();
            }
            Value::Struct(s) => return s.fmt(f),
            Value::Variant(v) => return v.fmt(f),
            Value::Opaque { type_name } => return f.debug_struct("Opaque").field("type_name", type_name).finish(),
            Value::Bool(b) => ("Bool", b),
            Value::Int(i) => ("Int", i),
            Value::Float(x) => ("Float", x),
            Value::Duration(d) => ("Duration", d),
            Value::Str(s) => ("Str", s),
            Value::Bytes(b) => ("Bytes", b),
            Value::List(items) => ("List", items),
            Value::Tuple(items) => ("Tuple", items),
            Value::Set(items) => ("Set", items),
            Value::Map(entries) => ("Map", entries),
            Value::Instant(t) => ("Instant", t),
            Value::PlainDateTime(t) => ("PlainDateTime", t),
            Value::PlainDate(t) => ("PlainDate", t),
            Value::PlainTime(t) => ("PlainTime", t),
            Value::PlainYearMonth(t) => ("PlainYearMonth", t),
            Value::PlainMonthDay(t) => ("PlainMonthDay", t),
            Value::ZonedDateTime(t) => ("ZonedDateTime", t),
            Value::Span(t) => ("Span", t),
            Value::TimeZone(t) => ("TimeZone", t),
            Value::Native(native) => ("Native", native),
        };
        f.debug_tuple(variant).field(inner).finish()
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

/// Writes each of `values` with its name in `names`, or with `_` if `names`
/// is too short.
fn debug_named_fields(mut out: fmt::DebugStruct<'_, '_>, names: &[String], values: &[Value]) -> fmt::Result {
    for (i, value) in values.iter().enumerate() {
        out.field(names.get(i).map_or("_", String::as_str), value);
    }
    out.finish()
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
/// assert_eq!(point.ty.type_name, "geo.Point");
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
        Value::Variant(VariantValue { variant: desc.get_or_init(|| VariantDesc::new("Option", name)).clone(), payload })
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
        prog: &vm.prog,
        heap: &vm.heap,
        meter: &vm.meter,
        allocated: 0,
        scratch: Scratch::new(),
        type_names: FxHashMap::default(),
    };
    match encode::walk(&vm.prog, &vm.heap, &vm.meter, &mut sink, PartialValue::Root(Value::Unit), value)? {
        PartialValue::Root(value) => Ok(value),
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

/// Returns the module part of the qualified name of a type that Rust
/// declares in `rust_module`, for [`qualified_name`]: `None` for the prelude,
/// whose types have no module part. The engine reserves the module name
/// `prelude`, so no host module has it.
pub(crate) fn qualified_name_module(rust_module: &str) -> Option<&str> {
    (rust_module != "prelude").then_some(rust_module)
}

/// The [`EncodeSink`] that builds a [`Value`].
struct ValueSink<'h> {
    prog: &'h Program,
    heap: &'h Heap,
    meter: &'h Meter,
    /// The bytes of Rust memory that the copy has allocated.
    allocated: usize,
    scratch: Scratch,
    /// The shared name of each native value type and each type of an opaque
    /// value in the copy, by its module and name.
    type_names: FxHashMap<(Option<&'static str>, &'static str), Arc<str>>,
}

/// A [`Value`] that waits for its children.
enum PartialValue {
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

    /// Returns the description of the variant with the runtime ID `id`,
    /// which the program owns.
    fn variant_desc(&self, id: u32) -> Result<Arc<VariantDesc>, Fail> {
        match self.prog.variant_desc(id) {
            Some(desc) => Ok(desc.clone()),
            None => internal("to_value: unknown variant ID"),
        }
    }

    fn native(&mut self, native: ValueRef<'_>) -> Result<Value, Fail> {
        if let Some(value) = native.to_dedicated_value() {
            // A dedicated case holds a packed value inline and a boxed value
            // in a `Box`, so the heap size of the value bounds its allocation.
            self.reserve(match native {
                ValueRef::Boxed(x) => x.size(),
                ValueRef::Packed(..) => 0,
            })?;
            return Ok(value);
        }
        let text = native.text();
        self.reserve_copy(text.len(), size_of::<NativeValue>())?;
        let type_name = self.type_name(qualified_name_module(native.module_name()), native.type_name())?;
        Ok(Value::Native(Box::new(NativeValue { type_name, text })))
    }

    fn opaque(&mut self, kind: OpaqueKind) -> Result<Value, Fail> {
        let type_name = match kind {
            OpaqueKind::Function => self.type_name(None, "fn")?,
            OpaqueKind::Task => self.type_name(None, "Task")?,
            OpaqueKind::Channel => self.type_name(None, "Channel")?,
            OpaqueKind::Object { module, name } => self.type_name(qualified_name_module(module), name)?,
            OpaqueKind::Internal => return internal("to_value: an internal heap object has no `Value` form"),
        };
        Ok(Value::Opaque { type_name })
    }
}

fn put(parent: &mut PartialValue, value: Value) {
    match parent {
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

impl<'h> EncodeSink<'h> for ValueSink<'h> {
    type Open = PartialValue;

    const TOO_DEEP: &'static str = "value too deeply nested to copy";

    #[inline(always)]
    fn leaf(&mut self, parent: &mut PartialValue, _: Role<'h>, leaf: Leaf<'h>) -> Result<(), Fail> {
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
            Leaf::Tag(variant) => Value::Variant(VariantValue { variant: self.variant_desc(variant.id)?, payload: Box::default() }),
            Leaf::Native(native) => self.native(native)?,
            Leaf::Opaque { kind, .. } => self.opaque(kind)?,
        };
        put(parent, value);
        Ok(())
    }

    #[inline(always)]
    fn open(&mut self, _: &mut PartialValue, _: Role<'h>, container: Container<'h>, len: usize) -> Result<Opened<PartialValue>, Fail> {
        Ok(Opened::Children(match container {
            Container::List => PartialValue::List(self.items(len)?),
            Container::Tuple => PartialValue::Tuple(self.items(len)?),
            Container::Set => PartialValue::Set(self.items(len)?),
            Container::Map => {
                self.reserve(len.saturating_mul(size_of::<(Value, Value)>()))?;
                PartialValue::Map(Vec::with_capacity(len), false)
            }
            Container::Struct { ty, .. } => PartialValue::Struct(ty.desc().clone(), self.items(len)?),
            Container::Variant(variant) => PartialValue::Variant(self.variant_desc(variant.id)?, self.items(len)?),
        }))
    }

    #[inline(always)]
    fn close(&mut self, parent: &mut PartialValue, open: PartialValue) -> Result<(), Fail> {
        let value = match open {
            PartialValue::Root(_) => return internal("to_value: the walk closed the root"),
            PartialValue::List(items) => Value::List(items),
            PartialValue::Tuple(items) => Value::Tuple(items),
            PartialValue::Set(items) => Value::Set(items),
            PartialValue::Map(entries, _) => Value::Map(entries),
            PartialValue::Struct(ty, fields) => Value::Struct(StructValue { ty, fields: fields.into_boxed_slice() }),
            PartialValue::Variant(variant, payload) => Value::Variant(VariantValue { variant, payload: payload.into_boxed_slice() }),
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

    #[test]
    fn debug_shows_structs_and_variants_by_name() {
        let none = Value::Variant(VariantValue::new("Option", "None", []));
        let some = Value::Variant(VariantValue::new("Option", "Some", [Value::Int(1)]));
        let order = Value::Struct(StructValue::new("shop.Order", [("id", Value::Int(1)), ("note", none)]));
        let moved = Value::Variant(VariantValue::with_fields("shop.Event", "Moved", [("x", Value::Int(1))]));
        assert_eq!(format!("{order:?}"), "shop.Order { id: Int(1), note: Option.None }");
        assert_eq!(format!("{some:?}"), "Option.Some(Int(1))");
        assert_eq!(format!("{moved:?}"), "shop.Event.Moved { x: Int(1) }");
        assert_eq!(format!("{moved:#?}"), "shop.Event.Moved {\n    x: Int(\n        1,\n    ),\n}");
        assert_eq!(format!("{:?}", Value::List(vec![some])), "List([Option.Some(Int(1))])");
    }
}
