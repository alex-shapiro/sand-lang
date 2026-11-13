//! Native module registration and Rust-to-Sand type bindings.

pub mod convert;
pub mod func;
pub mod ty;

use crate::check::ModKind;
use crate::engine::Registry;
use crate::syntax::lexer::{is_ident, is_keyword};
use crate::types::ValueTypeInfo;
use crate::value::{
    Decode, DecodeError, StructDesc, StructValue, Value, ValueReader, VariantDesc, VariantValue, qualified_name, qualified_name_module,
};
use crate::vm::heap::Heap;
use crate::vm::value::{ChannelObj, MapObj, NativeCell, Obj, SetObj, TaskObj, VALUE_SIZE, ValueType, VmValue};
use crate::vm::{Ctl, Fail, HostFn, NativeFn, Vm};
use convert::{Conversion, FromVmValue, mismatch};
use func::{NativeFunction, Signature};
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::{Arc, OnceLock};
use ty::{Channel, Num, Param, Range, SandType, Task, Type, VmVal};

/// A Rust type bound to a nominal Sand type in a [`NativeModule`].
///
/// Derive this trait with [`SandEnum`](crate::SandEnum),
/// [`SandObject`](crate::SandObject), [`SandStruct`](crate::SandStruct), or
/// [`SandValue`](crate::SandValue), and register the type with
/// [`NativeModule::bind`].
pub trait NamedType: Sized + 'static {
    /// Name of the Sand module that owns the type (`#[sand(module = "...")]`).
    const MODULE: &'static str;
    /// Sand type name (defaults to the Rust identifier, or `#[sand(name = "...")]`).
    const NAME: &'static str;
    /// Documentation comment attached to the Sand type declaration.
    const DOC: &'static str;
    /// `true` if the type is exported (`pub`), or `false` if marked `#[sand(private)]`.
    const PUBLIC: bool;
    /// Type parameter names for a generic enum or struct.
    const PARAMS: &'static [&'static str] = &[];
    /// Returns a [`TypeId`] that uniquely identifies the Sand type across all generic instantiations.
    #[doc(hidden)]
    fn rust_type() -> TypeId {
        TypeId::of::<Self>()
    }
    #[doc(hidden)]
    fn binding() -> Binding<Self>;
}

#[doc(hidden)]
pub struct Binding<T>(BoundKind, PhantomData<fn() -> T>);

impl<T: EnumType> Binding<T> {
    pub fn of_enum() -> Binding<T> {
        Binding(BoundKind::Enum { rust: T::rust_type(), params: T::PARAMS, variants: T::VARIANTS }, PhantomData)
    }
}

impl<T: ObjectType> Binding<T> {
    pub fn of_object() -> Binding<T> {
        Binding(BoundKind::Object(T::rust_type()), PhantomData)
    }
}

impl<T: StructType> Binding<T> {
    pub fn of_struct() -> Binding<T> {
        let defaults = T::FIELDS.iter().map(|f| f.default.map(|default| default())).collect();
        Binding(BoundKind::Struct { rust: T::rust_type(), params: T::PARAMS, fields: T::FIELDS, defaults }, PhantomData)
    }
}

impl<T: ValueType> Binding<T> {
    pub fn of_value() -> Binding<T> {
        let info = ValueTypeInfo { ord: T::ORDER.is_some(), parse: crate::vm::value::parse_value::<T>, type_id: T::rust_type() };
        Binding(BoundKind::Value(info), PhantomData)
    }
}

/// A Rust enum that maps to a Sand `enum` with matching variants and payloads.
///
/// Derive this trait with [`#[derive(SandEnum)]`](crate::SandEnum) and register
/// the type with [`NativeModule::bind`]:
///
/// ```
/// /// A two-dimensional shape.
/// #[derive(sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// pub enum Shape {
///     /// A circle with its radius.
///     Circle(f64),
///     /// A rectangle (`Rect(w: float, h: float)` in Sand).
///     Rect { w: f64, h: f64 },
///     /// The empty shape.
///     Empty,
/// }
/// ```
pub trait EnumType: NamedType + SandType {
    /// Variants in declaration order.
    const VARIANTS: &'static [EnumVariant];
    /// Returns the zero-based index of the variant of `self` in [`Self::VARIANTS`].
    fn index(&self) -> usize;
}

/// Returns the index of the variant named `name` in `T::VARIANTS` for use in
/// `const` contexts.
///
/// # Panics
///
/// Panics if `name` is not a variant in `T::VARIANTS`.
pub const fn variant_index<T: EnumType>(name: &str) -> usize {
    let mut i = 0;
    while i < T::VARIANTS.len() {
        if same_bytes(T::VARIANTS[i].name, name) {
            return i;
        }
        i += 1;
    }
    panic!("no such variant")
}

const fn same_bytes(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// A Rust struct that maps to a Sand `struct` with matching fields.
///
/// Derive this trait with [`#[derive(SandStruct)]`](crate::SandStruct) and
/// register the type with [`NativeModule::bind`]. Native functions pass and
/// return the struct by value, copying its fields between Rust and the VM heap.
///
/// ```
/// /// A point in the plane.
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// pub struct Point {
///     /// Horizontal coordinate.
///     pub x: f64,
///     /// Vertical coordinate (`0.0` if omitted in a construction).
///     #[sand(default)]
///     pub y: f64,
///     /// Number of moves applied to the point.
///     #[sand(private, var, default = 0)]
///     moves: i64,
/// }
///
/// /// A value with a label (`Labeled<T>` in Sand).
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// pub struct Labeled<T> {
///     pub label: String,
///     pub value: T,
/// }
/// ```
pub trait StructType: NamedType + SandType {
    /// Fields in declaration order.
    const FIELDS: &'static [StructField];
}

/// Metadata for a field of a [`StructType`].
#[derive(Debug)]
pub struct StructField {
    /// Sand field name (defaults to the Rust identifier, or `#[sand(name = "...")]`).
    pub name: &'static str,
    /// Documentation comment attached to the field.
    pub doc: &'static str,
    /// `true` if the field is exported (`pub`), or `false` if marked `#[sand(private)]`.
    pub public: bool,
    /// `true` if the field is mutable (`var` via `#[sand(var)]`).
    pub var: bool,
    /// Returns the Sand [`Type`] of the field.
    pub ty: fn() -> Type,
    /// Returns the default value of the field (`#[sand(default)]`) as an
    /// encoded [`Value`]. The default must be a Sand constant of the field
    /// type, so it cannot contain a native value type.
    pub default: Option<fn() -> Value>,
}

/// Cached [`StructDesc`] shared across values from a derived `Encode`
/// implementation.
#[doc(hidden)]
pub type StructDescCell = OnceLock<Arc<StructDesc>>;

/// Cached [`VariantDesc`] shared across values from a derived `Encode`
/// implementation.
#[doc(hidden)]
pub type VariantDescCell = OnceLock<Arc<VariantDesc>>;

#[doc(hidden)]
pub fn encode_struct<T: StructType>(ty: &StructDescCell, fields: impl IntoIterator<Item = Value>) -> Value {
    let ty =
        ty.get_or_init(|| StructDesc::new(qualified_name(qualified_name_module(T::MODULE), T::NAME), T::FIELDS.iter().map(|f| f.name)));
    Value::Struct(StructValue { ty: ty.clone(), fields: fields.into_iter().collect() })
}

#[doc(hidden)]
pub fn encode_variant<T: EnumType>(variants: &[VariantDescCell], index: usize, payload: impl IntoIterator<Item = Value>) -> Value {
    let desc = variants[index].get_or_init(|| {
        let (type_name, variant) = (qualified_name(qualified_name_module(T::MODULE), T::NAME), &T::VARIANTS[index]);
        let field_names: Option<Vec<&str>> = variant.fields.iter().map(|f| f.name).collect();
        match field_names.filter(|names| !names.is_empty()) {
            Some(names) => VariantDesc::with_fields(type_name, variant.name, names),
            None => VariantDesc::new(type_name, variant.name),
        }
    });
    Value::Variant(VariantValue { variant: desc.clone(), payload: payload.into_iter().collect() })
}

/// Reads the fields of a struct of type `type_name`, or of a named variant
/// payload if `type_name` is empty, whose fields have the names `names`.
/// Calls `field` with the index of each field, in the order of the input.
#[doc(hidden)]
pub fn decode_fields(
    r: &mut ValueReader<'_>,
    type_name: &str,
    names: &[&str],
    mut field: impl FnMut(&mut ValueReader<'_>, usize) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    r.begin_fields(type_name)?;
    while let Some(i) = r.next_field(names)? {
        field(r, i)?;
    }
    Ok(())
}

/// Returns the value of the omitted field `name` of type `F`, or the error
/// of a missing field.
#[doc(hidden)]
pub fn decode_missing<F: Decode>(r: &ValueReader<'_>, name: &str) -> Result<F, DecodeError> {
    F::decode_missing_field().ok_or_else(|| r.error(format!("missing field `{name}`")))
}

/// Reads a variant of `T`, and returns its index. The reader then reads its
/// payload, if any.
#[doc(hidden)]
pub fn decode_variant<T: EnumType>(r: &mut ValueReader<'_>) -> Result<usize, DecodeError> {
    r.variant_by(T::NAME, |tag| T::VARIANTS.iter().position(|v| v.name == tag).map(|i| (i, T::VARIANTS[i].fields.len())))
}

#[doc(hidden)]
pub fn struct_from_vm_value<T: StructType>(
    vm: &Vm,
    v: VmValue,
    conv: &mut Conversion,
    read: impl FnOnce(&[VmValue], &mut Conversion) -> Result<T, Fail>,
) -> Result<T, Fail> {
    let def = vm.prog.struct_types.get(&T::rust_type());
    let fields = match (def, v) {
        (Some(&def), VmValue::Obj(_)) => match vm.heap.get(v) {
            Obj::Struct(d, fields) if *d == def && fields.len() == T::FIELDS.len() => fields,
            _ => return Err(mismatch::<T>()),
        },
        _ => return Err(mismatch::<T>()),
    };
    vm.charge(1)?;
    conv.enter_struct(|conv| read(fields, conv))
}

#[doc(hidden)]
pub fn struct_into_vm_value<T: StructType>(
    vm: &mut Vm,
    write: impl FnOnce(&mut Vm) -> Result<Box<[VmValue]>, Fail>,
) -> Result<VmValue, Fail> {
    let Some(&def) = vm.prog.struct_types.get(&T::rust_type()) else {
        return Err(Fail::internal(format!("struct {}.{} is not loaded", T::MODULE, T::NAME)));
    };
    vm.struct_depth = convert::enter_struct(vm.struct_depth)?;
    let fields = write(vm);
    vm.struct_depth -= 1;
    let fields = fields?;
    if fields.len() != T::FIELDS.len() {
        return Err(Fail::internal(format!("struct {}.{} takes {} fields, not {}", T::MODULE, T::NAME, T::FIELDS.len(), fields.len())));
    }
    vm.check_memory(fields.len() * std::mem::size_of::<VmValue>())?;
    Ok(vm.heap.alloc(Obj::Struct(def, fields)))
}

#[doc(hidden)]
pub const fn is_type_name(name: &str) -> bool {
    is_name(name) && name.as_bytes()[0].is_ascii_uppercase()
}

#[doc(hidden)]
pub const fn is_name(name: &str) -> bool {
    is_ident(name) && !is_keyword(name)
}

#[doc(hidden)]
pub fn field_from_vm_value<T: for<'a> FromVmValue<Item<'a> = T>>(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<T, Fail> {
    T::from_vm_value(vm, v, conv)
}

#[doc(hidden)]
pub fn enum_from_vm_value<T: EnumType>(
    vm: &Vm,
    v: VmValue,
    conv: &mut Conversion,
    read: impl FnOnce(usize, &mut Conversion) -> Result<T, Fail>,
) -> Result<T, Fail> {
    let id = vm.heap.variant_id(v);
    let index = vm.prog.enum_types.get(&T::rust_type()).and_then(|&first| id.checked_sub(first)).map(|i| i as usize);
    let Some((index, variant)) = index.and_then(|i| Some((i, T::VARIANTS.get(i)?))) else { return Err(mismatch::<T>()) };
    let arity = match vm.heap.variant(v) {
        Some(crate::vm::value::VariantRef::Heap(_, fields)) => fields.len(),
        Some(crate::vm::value::VariantRef::Inline(_, payload)) => usize::from(payload.is_some()),
        None => return Err(mismatch::<T>()),
    };
    if arity != variant.fields.len() {
        return Err(mismatch::<T>());
    }
    if variant.fields.is_empty() {
        return read(index, conv);
    }
    vm.charge(1)?;
    conv.enter_struct(|conv| read(index, conv))
}

#[doc(hidden)]
pub fn variant_field(vm: &Vm, v: VmValue, i: usize) -> Result<VmValue, Fail> {
    vm.heap.field(v, i).ok_or_else(|| Fail::internal("a native function read a missing variant payload"))
}

#[doc(hidden)]
pub fn variant_into_vm_value<T: EnumType, const N: usize>(
    vm: &mut Vm,
    index: usize,
    write: impl FnOnce(&mut Vm) -> Result<[VmValue; N], Fail>,
) -> Result<VmValue, Fail> {
    let Some(&first) = vm.prog.enum_types.get(&T::rust_type()) else {
        return Err(Fail::internal(format!("enum {}.{} is not loaded", T::MODULE, T::NAME)));
    };
    let id = first + index as u32;
    if N == 0 {
        return Ok(VmValue::Tag(id));
    }
    vm.struct_depth = convert::enter_struct(vm.struct_depth)?;
    let fields = write(vm);
    vm.struct_depth -= 1;
    let fields = fields?;
    if let [p] = fields.as_slice()
        && let Some(v) = VmValue::variant1(id, *p)
    {
        return Ok(v);
    }
    vm.check_memory(N * std::mem::size_of::<VmValue>())?;
    Ok(vm.heap.new_variant(id, &fields))
}

/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape { Circle(u32) }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape<'a> { Circle, Named(&'a str) }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape {}
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// enum Shape { Circle }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape { #[sand(name = "Round")] Circle }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo", name = "shape")]
/// enum Shape { Circle }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "db")]
/// struct Conn;
/// // Missing: `impl sandlang::native::NativeObject for Conn {}`.
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "db")]
/// struct Conn<T>(T);
/// impl<T: Send + 'static> sandlang::native::NativeObject for Conn<T> {}
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "text")]
/// struct Word {
///     text: &'static str,
/// }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "db")]
/// struct Row {
///     id: u32,
/// }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "db")]
/// struct Row {
///     r#match: i64,
/// }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "db")]
/// struct Row {
///     #[sand(name = "two words")]
///     id: i64,
/// }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo", name = "Self")]
/// enum Shape { Circle }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// #[allow(non_camel_case_types)]
/// enum Shape { circle }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, PartialEq, Eq, Hash, sandlang::SandValue)]
/// #[sand(module = "money")]
/// struct Cents(i64);
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo", crate = not_sandlang)]
/// enum Shape { Circle }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// struct Point { x: f64 }
/// struct Fake;
/// impl sandlang::native::NamedType for Fake {
///     const MODULE: &'static str = "geo";
///     const NAME: &'static str = "Fake";
///     const DOC: &'static str = "";
///     const PUBLIC: bool = true;
///     fn binding() -> sandlang::native::Binding<Fake> {
///         sandlang::native::Binding::<Point>::of_struct()
///     }
/// }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// struct Point { x: f64 }
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// struct Line { #[sand(default)] start: Point }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// struct Point { #[sand(default = "origin")] x: f64 }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandStruct)]
/// #[sand(module = "geo")]
/// struct Pair<t> { a: t }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape { Rect { r#fn: f64 } }
/// ```
#[cfg(doctest)]
struct DeriveErrors;

/// Metadata for a variant of an [`EnumType`].
#[derive(Debug)]
pub struct EnumVariant {
    /// Sand variant name (the Rust identifier).
    pub name: &'static str,
    /// Documentation comment attached to the variant.
    pub doc: &'static str,
    /// Payload fields in declaration order (empty for a unit variant).
    pub fields: &'static [VariantField],
}

/// Metadata for a payload field of an [`EnumVariant`].
#[derive(Debug)]
pub struct VariantField {
    /// Field name for a variant with named fields, or `None` for a tuple variant.
    pub name: Option<&'static str>,
    /// Returns the Sand [`Type`] of the field.
    pub ty: fn() -> Type,
}

/// Memory and resource accounting for a heap-allocated [`ObjectType`].
///
/// An object keeps VM values with [`VmVal::push_value`], which stores them
/// next to the object, where the garbage collector traces them. A VM handle
/// type cannot be a field: handle types are `!Send`. Do not store a raw
/// [`VmValue`] in a field either, because the collector does not trace the
/// fields of an object.
pub trait NativeObject: Any + Send {
    /// Approximate heap memory used by `self`, in bytes.
    ///
    /// The VM reads this value on allocation and deallocation; call
    /// [`Heap::account`] if the object's size changes in place.
    fn size(&self) -> usize {
        64
    }

    /// Returns `true` if `self` holds an open host resource (such as a file or
    /// socket with an `iomem::ResourceSlot` counted against
    /// `max_open_resources`). Allocating resource objects paces garbage
    /// collection so unreachable handles are closed promptly.
    fn is_resource(&self) -> bool {
        false
    }
}

/// A Rust type exposed to scripts as an opaque reference type.
///
/// Derive this trait with [`#[derive(SandObject)]`](crate::SandObject),
/// implement [`NativeObject`], and register the type with
/// [`NativeModule::bind`]:
///
/// ```
/// /// A shared atomic counter.
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "stats")]
/// pub struct Counter(std::sync::atomic::AtomicI64);
///
/// impl sandlang::native::NativeObject for Counter {}
/// ```
///
/// A native function borrows an object as `&T` and returns a new object by
/// value as `T`. A native function that takes `&mut Vm` accepts a [`VmVal<T>`]
/// handle.
pub trait ObjectType: NativeObject + NamedType + SandType {}

/// Type-erased heap representation of an [`ObjectType`] and the VM values
/// that it holds (a [`NativeCell`]).
pub trait DynObject: Any + Send + sealed::Sealed {
    fn module_name(&self) -> &'static str;
    fn type_name(&self) -> &'static str;
    /// The bytes of the object and of its values.
    fn size(&self) -> usize;
    fn is_resource(&self) -> bool;
    /// The VM values of the object, which the collector traces.
    fn values(&self) -> &crate::vm::value::ObjectValues;
}

mod sealed {
    pub trait Sealed {}
    impl<T: super::ObjectType> Sealed for crate::vm::value::NativeCell<T> {}
}

impl<T: ObjectType> DynObject for NativeCell<T> {
    fn module_name(&self) -> &'static str {
        T::MODULE
    }
    fn type_name(&self) -> &'static str {
        T::NAME
    }
    fn size(&self) -> usize {
        NativeObject::size(&self.object) + self.values.capacity() * VALUE_SIZE
    }
    fn is_resource(&self) -> bool {
        NativeObject::is_resource(&self.object)
    }
    fn values(&self) -> &crate::vm::value::ObjectValues {
        &self.values
    }
}

impl<T: ObjectType> VmVal<T> {
    pub fn get(self, heap: &Heap) -> Result<&T, Fail> {
        heap.native::<T>(self.raw()).ok_or_else(mismatch::<T>)
    }

    pub fn get_mut(self, heap: &mut Heap) -> Result<&mut T, Fail> {
        heap.native_mut::<T>(self.raw()).ok_or_else(mismatch::<T>)
    }

    /// Returns the VM values that the object holds, in the order of
    /// [`VmVal::push_value`]. The handles in them are valid during the
    /// native call.
    pub fn values(self, heap: &Heap) -> Result<&[VmValue], Fail> {
        heap.native_values::<T>(self.raw()).map(|values| values.as_slice()).ok_or_else(mismatch::<T>)
    }

    /// Adds `value` to the VM values that the object holds. The collector
    /// keeps them alive and up to date while the object is alive, so the
    /// object can keep Sand values, such as callbacks or a partly built
    /// result, between native calls. Fails with a memory limit error before
    /// any change.
    pub fn push_value(self, vm: &mut Vm, value: VmValue) -> Result<(), Fail> {
        let values = vm.heap.native_values::<T>(self.raw()).ok_or_else(mismatch::<T>)?;
        let (len, capacity) = (values.len(), values.capacity());
        if len == capacity {
            let grown = capacity.saturating_mul(2).max(4);
            vm.check_memory((grown - capacity) * VALUE_SIZE)?;
            vm.heap.account(capacity * VALUE_SIZE, grown * VALUE_SIZE);
            self.values_mut(&mut vm.heap, &[])?.reserve_exact(grown - len);
        }
        self.values_mut(&mut vm.heap, &[value])?.push(value);
        Ok(())
    }

    /// Replaces the VM value at `index` with `value`.
    pub fn set_value(self, heap: &mut Heap, index: usize, value: VmValue) -> Result<(), Fail> {
        let Some(slot) = self.values_mut(heap, &[value])?.get_mut(index) else {
            return Err(Fail::internal(format!("{}: no value {index}", T::NAME)));
        };
        *slot = value;
        Ok(())
    }

    /// Keeps the first `len` VM values that the object holds.
    pub fn truncate_values(self, heap: &mut Heap, len: usize) -> Result<(), Fail> {
        self.values_mut(heap, &[])?.truncate(len);
        Ok(())
    }

    fn values_mut<'h>(self, heap: &'h mut Heap, stored: &[VmValue]) -> Result<&'h mut crate::vm::value::ObjectValues, Fail> {
        heap.native_values_mut::<T>(self.raw(), stored).ok_or_else(mismatch::<T>)
    }
}

/// Method names of the prelude `Num` trait in dictionary slot order.
pub(crate) const NUM_METHODS: [&str; 2] = ["zero", "one"];

impl<const N: u8> Num<N> {
    /// Returns the additive identity (`0` or `0.0`) for type parameter `N`.
    pub fn zero(self, vm: &mut Vm) -> Result<Param<N>, Ctl> {
        self.call_method(vm, 0)
    }

    /// Returns the multiplicative identity (`1` or `1.0`) for type parameter `N`.
    pub fn one(self, vm: &mut Vm) -> Result<Param<N>, Ctl> {
        self.call_method(vm, 1)
    }

    fn call_method(self, vm: &mut Vm, slot: usize) -> Result<Param<N>, Ctl> {
        let f = match vm.heap.get(self.raw()) {
            Obj::Tuple(slots) => slots.get(slot).map(|&f| vm.heap.get(f)),
            _ => None,
        };
        let Some(&Obj::NativeFn(n)) = f else { return Err(Fail::internal("a native function expected `Num` evidence").into()) };
        let f = vm.natives[n as usize].f.clone();
        f.call(vm, &[]).map(Param::from_raw)
    }
}

impl VmVal<String> {
    pub fn str(self, heap: &Heap) -> &str {
        heap.str(self.raw())
    }
}

impl VmVal<Vec<u8>> {
    pub fn bytes(self, heap: &Heap) -> &[u8] {
        heap.bytes_of(self.raw())
    }
}

impl<T: SandType> VmVal<Vec<T>> {
    pub fn items(self, heap: &Heap) -> &[VmValue] {
        heap.list(self.raw())
    }
}

impl<K: SandType, V: SandType> VmVal<std::collections::HashMap<K, V>> {
    pub(crate) fn map(self, heap: &Heap) -> Result<&MapObj, Fail> {
        match heap.get(self.raw()) {
            Obj::Map(m) => Ok(m),
            _ => Err(mismatch::<std::collections::HashMap<K, V>>()),
        }
    }
}

impl<T: SandType> VmVal<std::collections::HashSet<T>> {
    pub(crate) fn set(self, heap: &Heap) -> Result<&SetObj, Fail> {
        match heap.get(self.raw()) {
            Obj::Set(s) => Ok(s),
            _ => Err(mismatch::<std::collections::HashSet<T>>()),
        }
    }
}

impl VmVal<Range> {
    /// Returns the `(start, end, inclusive)` bounds of the range.
    pub(crate) fn bounds(self, heap: &Heap) -> Result<(i64, i64, bool), Fail> {
        match *heap.get(self.raw()) {
            Obj::Range(a, b, inclusive) => Ok((a, b, inclusive)),
            _ => Err(mismatch::<Range>()),
        }
    }
}

impl<T: SandType> VmVal<Channel<T>> {
    pub(crate) fn channel(self, heap: &Heap) -> Result<&ChannelObj, Fail> {
        match heap.get(self.raw()) {
            Obj::Channel(c) => Ok(c),
            _ => Err(mismatch::<Channel<T>>()),
        }
    }

    pub(crate) fn channel_mut(self, heap: &mut Heap) -> Result<&mut ChannelObj, Fail> {
        heap.channel_mut(self.raw(), None).ok_or_else(mismatch::<Channel<T>>)
    }
}

impl<T: SandType> VmVal<Task<T>> {
    pub(crate) fn task(self, heap: &Heap) -> Result<&TaskObj, Fail> {
        match heap.get(self.raw()) {
            Obj::Task(t) => Ok(t),
            _ => Err(mismatch::<Task<T>>()),
        }
    }
}

impl Vm {
    /// Allocates `x` on the heap after checking `x.size()` bytes against the
    /// memory limit.
    pub fn new_object<T: ObjectType>(&mut self, x: T) -> Result<VmVal<T>, Fail> {
        self.check_memory(NativeObject::size(&x))?;
        Ok(VmVal::from_raw(self.heap.alloc(Obj::Native(Box::new(NativeCell { values: Default::default(), object: x })))))
    }
}

/// A Rust-backed type registered on a [`NativeModule`].
#[derive(Clone, Debug)]
pub(crate) struct BoundType {
    pub module: &'static str,
    pub name: &'static str,
    pub doc: &'static str,
    pub public: bool,
    pub kind: BoundKind,
}

#[derive(Clone, Debug)]
pub(crate) enum BoundKind {
    Value(ValueTypeInfo),
    Enum {
        rust: TypeId,
        params: &'static [&'static str],
        variants: &'static [EnumVariant],
    },
    Object(TypeId),
    Struct {
        rust: TypeId,
        params: &'static [&'static str],
        fields: &'static [StructField],
        defaults: Arc<[Option<Value>]>,
    },
    /// Built-in generic type (`Task<T>` or `Channel<T>`) and its type parameter names.
    Builtin(&'static [&'static str]),
}

impl BoundKind {
    pub(crate) fn rust_type(&self) -> Option<TypeId> {
        match self {
            BoundKind::Value(v) => Some(v.type_id),
            BoundKind::Enum { rust, .. } | BoundKind::Struct { rust, .. } | BoundKind::Object(rust) => Some(*rust),
            BoundKind::Builtin(_) => None,
        }
    }

    pub(crate) fn member_types(&self) -> Vec<Type> {
        match self {
            BoundKind::Struct { fields, .. } => fields.iter().map(|f| (f.ty)()).collect(),
            BoundKind::Enum { variants, .. } => variants.iter().flat_map(|v| v.fields).map(|f| (f.ty)()).collect(),
            _ => Vec::new(),
        }
    }
}

/// Checker metadata for a registered native function.
#[derive(Clone, Debug)]
pub(crate) struct NativeDecl {
    pub id: u32,
    /// `module.name` or `module.Type.name`.
    pub key: Cow<'static, str>,
    pub sig: Arc<Signature>,
    pub names: Option<Arc<[String]>>,
    pub doc: Option<Arc<str>>,
}

#[derive(Clone)]
struct FnDef {
    name: String,
    sig: Signature,
    f: HostFn,
    names: Option<Arc<[String]>>,
    doc: Option<Arc<str>>,
}

/// A module of Rust functions and types that scripts can import with
/// `import <name>`.
///
/// The engine infers the Sand signature of each registered function from its
/// Rust parameter and return types. It exports functions that are absent from
/// [`NativeModule::source`] as `pub native fn`. [`NativeModule::params`] sets
/// their parameter names, which default to `arg1`, `arg2`, and so on.
/// Registering a function as `"Type.name"` defines a method on `Type`. The
/// method takes `self` if its first parameter has type `Type`. Use `native fn`
/// declarations in [`NativeModule::source`] to specify default parameter
/// values or generic type parameters.
/// [`HostModules::new`](crate::engine::HostModules::new) checks all
/// declarations against the registered Rust signatures.
///
/// The VM re-runs a registered function from the start after a permission
/// prompt or after a garbage collection triggered by a memory or resource
/// limit. Check permissions with [`Vm::require`], reserve memory, and acquire
/// resource slots before performing any side effect. Avoid panicking on script
/// input. The engine catches panics and aborts the run with
/// [`Error::Internal`](crate::engine::Error::Internal).
#[derive(Clone)]
pub struct NativeModule {
    name: String,
    kind: ModKind,
    source: String,
    functions: Vec<FnDef>,
    types: Vec<BoundType>,
}

impl NativeModule {
    /// Creates a host native module imported as `name`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a valid identifier or conflicts with the prelude
    /// or a standard library module.
    pub fn new(name: &str) -> NativeModule {
        assert!(is_name(name), "a host module cannot be named `{name}`: it is not an identifier, or is a keyword");
        assert!(!crate::stdlib::STD_MODULES.contains(&name), "a host module cannot be named `{name}`: it is the name of a standard module");
        NativeModule::of_kind(name, ModKind::Host)
    }

    pub(crate) fn std(name: &str, kind: ModKind, source: &str) -> NativeModule {
        let mut m = NativeModule::of_kind(name, kind);
        m.source(source);
        m
    }

    fn of_kind(name: &str, kind: ModKind) -> NativeModule {
        NativeModule { name: name.to_string(), kind, source: String::new(), functions: Vec::new(), types: Vec::new() }
    }

    /// Returns the module name used in `import` declarations.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn add(&mut self, name: &str, sig: Signature, f: Box<NativeFn>) -> &mut Self {
        let parts: Vec<&str> = name.split('.').collect();
        let valid = parts.len() <= 2 && parts.iter().all(|p| is_ident(p)) && parts.last() != Some(&"self");
        assert!(valid, "function name `{name}` is not `name` or `Type.name`");
        assert!(!self.functions.iter().any(|d| d.name == name), "function `{name}` is registered twice in module `{}`", self.name);
        let f = if self.kind == ModKind::Host { HostFn::Shared(f.into()) } else { HostFn::Static(Box::leak(f)) };
        self.functions.push(FnDef { name: name.to_string(), sig, f, names: None, doc: None });
        self
    }

    /// Registers the Rust function `f` as `name` or `"Type.method"`.
    ///
    /// The function signature selects one of three kinds of [`NativeFunction`]:
    /// a synchronous function, a synchronous function that takes `&mut Vm`
    /// first, or an asynchronous function that returns a future. A function
    /// that takes `&mut Vm` must check permissions and limits before any side
    /// effect, because the engine can run it again from the start:
    ///
    /// ```
    /// # use sandlang::native::NativeModule;
    /// # use sandlang::native::convert::HostError;
    /// # use sandlang::policy::Capability;
    /// # use sandlang::vm::Vm;
    /// let mut module = NativeModule::new("door");
    /// // A synchronous function. Parameters can borrow from the VM heap.
    /// module.function("width", |name: &str| name.len() as i64);
    /// // A synchronous function that accesses the VM.
    /// module.function("open", |vm: &mut Vm, name: String| -> Result<String, HostError> {
    ///     vm.require(&Capability::Custom { name: "door".into(), detail: name.clone() })?;
    ///     Ok(format!("{name} is open"))
    /// });
    /// // An asynchronous function. Only the calling Sand task waits.
    /// module.function("knock", |name: String| async move { format!("{name} answered") });
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `name` is not `name` or `Type.name`, or if `name` is already
    /// registered in this module.
    pub fn function<Kind, F: NativeFunction<Kind>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_native_fn())
    }

    /// Sets the parameter names (excluding `self`) for the most recently
    /// registered function if it is not declared in [`NativeModule::source`].
    ///
    /// # Panics
    ///
    /// Panics if this module has not registered any function yet, if
    /// `names.len()` does not match the function's parameter count, or if any
    /// name is not a valid unique identifier.
    pub fn params(&mut self, names: &[&str]) -> &mut Self {
        let module = self.name.clone();
        let f = self.last("params");
        let n = f.sig.params.len() - usize::from(f.sig.takes_self(&module, &f.name));
        assert_eq!(names.len(), n, "function `{}` has {n} parameters", f.name);
        for (i, n) in names.iter().enumerate() {
            assert!(is_name(n), "parameter name `{n}` of function `{}` is not an identifier, or is a keyword", f.name);
            assert!(!names[..i].contains(n), "parameter name `{n}` of function `{}` is given twice", f.name);
        }
        f.names = Some(names.iter().map(|n| n.to_string()).collect());
        self
    }

    /// Sets the documentation comment for the most recently registered
    /// function if it is not declared in [`NativeModule::source`].
    ///
    /// # Panics
    ///
    /// Panics if this module has not registered any function yet.
    pub fn doc(&mut self, text: &str) -> &mut Self {
        self.last("doc").doc = Some(text.into());
        self
    }

    fn last(&mut self, method: &str) -> &mut FnDef {
        match self.functions.last_mut() {
            Some(f) => f,
            None => panic!("`{method}` applies to the function registered last, and no function is registered"),
        }
    }

    /// Registers the Rust-bound type `T` in this module (see [`NamedType`]).
    ///
    /// # Panics
    ///
    /// Panics if `T::MODULE` does not match [`NativeModule::name`], if the
    /// variant, field, or type parameter names of `T` are invalid or
    /// duplicated, or if `T::NAME` is already registered in this module.
    pub fn bind<T: NamedType>(&mut self) -> &mut Self {
        let kind = T::binding().0;
        let names = |what: &str, owner: &str, names: &[&str], type_names: bool| {
            for (i, n) in names.iter().enumerate() {
                match type_names {
                    true => assert!(is_type_name(n), "{what} `{n}` of `{owner}` is not a capitalized identifier, or is a keyword"),
                    false => assert!(is_name(n), "{what} `{n}` of `{owner}` is not an identifier, or is a keyword"),
                }
                assert!(!names[..i].contains(n), "{what} `{n}` of `{owner}` is declared twice");
            }
        };
        match &kind {
            BoundKind::Enum { params, variants, .. } => {
                assert!(!variants.is_empty(), "enum `{}` has no variants", T::NAME);
                for v in variants.iter() {
                    let fields: Vec<&str> = v.fields.iter().filter_map(|f| f.name).collect();
                    assert!(
                        fields.is_empty() || fields.len() == v.fields.len(),
                        "variant `{}` of `{}` names only some fields",
                        v.name,
                        T::NAME
                    );
                    names("field", &format!("{}.{}", T::NAME, v.name), &fields, false);
                }
                let vs: Vec<&str> = variants.iter().map(|v| v.name).collect();
                names("variant", T::NAME, &vs, true);
                names("type parameter", T::NAME, params, true);
            }
            BoundKind::Struct { params, fields, .. } => {
                let fs: Vec<&str> = fields.iter().map(|f| f.name).collect();
                names("field", T::NAME, &fs, false);
                names("type parameter", T::NAME, params, true);
            }
            _ => {}
        }
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: T::PUBLIC, kind })
    }

    pub(crate) fn builtin_type(&mut self, name: &'static str, doc: &'static str, params: &'static [&'static str]) -> &mut Self {
        assert!(self.kind == ModKind::Prelude, "only the prelude has built-in types");
        self.add_type(BoundType { module: "prelude", name, doc, public: true, kind: BoundKind::Builtin(params) })
    }

    fn add_type(&mut self, t: BoundType) -> &mut Self {
        assert_eq!(t.module, self.name, "type `{}` belongs to module `{}`", t.name, t.module);
        assert!(is_type_name(t.name), "type name `{}` is not a capitalized identifier, or is a keyword", t.name);
        assert!(!self.types.iter().any(|u| u.name == t.name), "type `{}` is registered twice in module `{}`", t.name, self.name);
        self.types.push(t);
        self
    }

    /// Appends Sand source code (types, helper functions, and `native fn`
    /// declarations for registered functions) to this module.
    pub fn source(&mut self, src: &str) -> &mut Self {
        self.source.push_str(src);
        self.source.push('\n');
        self
    }

    pub(crate) fn register_types(&self, r: &mut Registry) {
        for t in &self.types {
            r.add_bound_type(t.clone());
        }
    }

    pub(crate) fn register(&self, r: &mut Registry) {
        for f in &self.functions {
            r.add_native(format!("{}.{}", self.name, f.name), f.f.clone(), f.sig.clone(), f.names.clone(), f.doc.clone());
        }
        r.source(self.name.clone(), self.kind, self.source.clone());
    }
}
