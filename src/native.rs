//! Host native modules: Rust functions callable from Sand with automatically
//! derived type signatures.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::Capability;
use crate::syntax::ast::{Import, ImportSource, ItemKind};
use crate::vm::meter::Scratch;
use crate::vm::value::{Obj, Value, ValueType};
use crate::vm::{Ctl, Finisher, HostFn, NResult, NativeImpl, Vm};
use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;

/// A Rust type with a Sand equivalent.
pub trait SandType {
    /// Returns the Sand type name and records any required imports in `cx`.
    fn sand_type(cx: &mut TypeContext) -> String;
}

/// Tracks module imports and host-registered types referenced in a derived
/// signature.
pub struct TypeContext {
    module: String,
    imports: BTreeSet<&'static str>,
    used_types: Vec<(String, TypeId)>,
}

impl TypeContext {
    /// Returns the Sand name of the type `T` registered as `module.name`,
    /// importing `module` if it is another module.
    fn registered<T: 'static>(&mut self, module: &'static str, name: &'static str) -> String {
        let qual = format!("{module}.{name}");
        self.used_types.push((qual.clone(), TypeId::of::<T>()));
        if module == self.module {
            return name.into();
        }
        self.imports.insert(module);
        qual
    }

    /// Returns the Sand name of the enum type `T` (see [`sand_enum!`](crate::sand_enum)).
    pub fn enum_type<T: EnumType>(&mut self) -> String {
        self.registered::<T>(T::MODULE, T::NAME)
    }
}

/// A Rust enum bound to a Sand enum whose variants have no payloads, so that
/// host functions can take and return it. Implement it with
/// [`sand_enum!`](crate::sand_enum) and register it with
/// [`NativeModule::enum_type`].
pub trait EnumType: Copy + 'static {
    /// Name of the module that declares the Sand enum (e.g. `"fs"`).
    const MODULE: &'static str;
    /// Name of the Sand enum (e.g. `"Mode"`).
    const NAME: &'static str;
    /// The name and Rust value of each Sand variant, in declaration order.
    const VARIANTS: &'static [(&'static str, Self)];
}

/// Binds a Rust enum to a Sand enum whose variants have no payloads and the
/// listed names, in the listed order:
///
/// ```
/// #[derive(Clone, Copy)]
/// pub enum Shape {
///     Circle,
///     Square,
/// }
/// sandlang::sand_enum!(Shape => geo.Shape { Circle, Square });
/// ```
///
/// The macro implements [`EnumType`](crate::native::EnumType) and the
/// conversions for host functions. The list must name every Rust variant,
/// unless it ends with `..` (as it must for a `#[non_exhaustive]` enum of
/// another crate). A host function that returns an unlisted variant fails
/// with an internal error. The checker rejects a Sand declaration with other
/// variants.
#[macro_export]
macro_rules! sand_enum {
    ($ty:ty => $module:ident . $name:ident { $($variant:ident),+ , .. }) => {
        $crate::sand_enum!(@bind $ty => $module.$name { $($variant),+ });
    };
    ($ty:ty => $module:ident . $name:ident { $($variant:ident),+ $(,)? }) => {
        const _: fn($ty) = |x| match x {
            $(<$ty>::$variant => {})+
        };
        $crate::sand_enum!(@bind $ty => $module.$name { $($variant),+ });
    };
    (@bind $ty:ty => $module:ident . $name:ident { $($variant:ident),+ }) => {
        impl $crate::native::EnumType for $ty {
            const MODULE: &'static str = stringify!($module);
            const NAME: &'static str = stringify!($name);
            const VARIANTS: &'static [(&'static str, Self)] = &[$((stringify!($variant), <$ty>::$variant)),+];
        }
        impl $crate::native::SandType for $ty {
            fn sand_type(cx: &mut $crate::native::TypeContext) -> String {
                cx.enum_type::<Self>()
            }
        }
        impl $crate::native::FromValue for $ty {
            type Item<'a> = Self;
            fn from_value(
                vm: &$crate::vm::Vm,
                v: $crate::Value,
                _: &mut $crate::native::Conversion,
            ) -> Result<Self, $crate::vm::Ctl> {
                vm.enum_value(v)
            }
        }
        impl $crate::native::IntoValue for $ty {
            fn into_value(self, vm: &mut $crate::vm::Vm) -> Result<$crate::Value, $crate::vm::Ctl> {
                vm.new_enum(self)
            }
        }
        impl $crate::native::ListElem for $ty {}
    };
}

impl Vm {
    /// Converts `v`, a value of the Sand enum bound to `T`.
    pub fn enum_value<T: EnumType>(&self, v: Value) -> Result<T, Ctl> {
        let index = match v {
            Value::Tag(id) => self.prog.enum_types.get(&TypeId::of::<T>()).and_then(|&first| id.checked_sub(first)),
            _ => None,
        };
        match index.and_then(|i| T::VARIANTS.get(i as usize)) {
            Some(&(_, x)) => Ok(x),
            None => mismatch(&format!("{}.{}", T::MODULE, T::NAME)),
        }
    }

    /// The value of the Sand enum bound to `T` for `x`.
    pub fn new_enum<T: EnumType>(&self, x: T) -> Result<Value, Ctl> {
        let d = std::mem::discriminant(&x);
        let Some(index) = T::VARIANTS.iter().position(|(_, v)| std::mem::discriminant(v) == d) else {
            return Err(Ctl::internal(format!("a value of {} has no {}.{} variant", std::any::type_name::<T>(), T::MODULE, T::NAME)));
        };
        match self.prog.enum_types.get(&TypeId::of::<T>()) {
            Some(&first) => Ok(Value::Tag(first + index as u32)),
            None => Err(Ctl::internal(format!("enum {}.{} is not loaded", T::MODULE, T::NAME))),
        }
    }
}

/// Conversion of an argument from a Sand value.
///
/// The converted [`FromValue::Item`] may borrow from the heap for the
/// duration of the call (`&str`, `&[u8]`): host functions have no access to
/// the VM, so nothing can change the heap while they run. Owned types
/// (`String`, `Vec<T>`, ...) copy the value.
pub trait FromValue: SandType {
    type Item<'a>;

    /// Converts `v`, charging the meter for the work and counting the memory
    /// the conversion allocates in `conv` (which checks it against the
    /// memory limit as it grows, before allocating).
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl>;
}

/// The memory allocated converting a call's arguments, checked against the
/// memory limit as it grows.
pub struct Conversion {
    bytes: usize,
    scratch: Scratch,
}

impl Conversion {
    fn new() -> Conversion {
        Conversion { bytes: 0, scratch: Scratch::new() }
    }

    /// Counts `bytes` about to be allocated, failing with the memory limit
    /// if the conversion's allocations do not fit.
    pub fn alloc(&mut self, vm: &Vm, bytes: usize) -> Result<(), Ctl> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.scratch.check(&vm.heap, self.bytes)
    }
}

/// Conversion into a Sand value: checks the memory limit and charges the
/// meter for what it builds.
pub trait IntoValue: SandType {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl>;
}

/// An error returned by a host function: becomes `Err(Error { kind, message })`.
/// Any other error type `E: Display` returned in a `Result<T, E>` becomes an
/// error of kind `"error"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    pub kind: String,
    pub message: String,
}

impl HostError {
    /// An error of kind `kind`; the standard kinds are listed in SPEC §13
    /// (`not_found`, `invalid`, `permission_denied`, ...).
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> HostError {
        HostError { kind: kind.into(), message: message.into() }
    }
}

/// The error types a host function may return in a `Result`.
pub trait IntoHostError {
    fn into_host_error(self) -> HostError;
}

impl IntoHostError for HostError {
    fn into_host_error(self) -> HostError {
        self
    }
}

impl<E: Display> IntoHostError for E {
    fn into_host_error(self) -> HostError {
        HostError::new("error", self.to_string())
    }
}

macro_rules! sand_type {
    ($($t:ty => $name:expr),* $(,)?) => {$(
        impl SandType for $t {
            fn sand_type(_: &mut TypeContext) -> String {
                $name.into()
            }
        }
    )*};
}

sand_type! {
    i64 => "int",
    f64 => "float",
    bool => "bool",
    String => "str",
    &str => "str",
    Vec<u8> => "bytes",
    &[u8] => "bytes",
    () => "()",
}

fn mismatch<T>(what: &str) -> Result<T, Ctl> {
    Err(Ctl::internal(format!("host function argument: expected {what}")))
}

/// Scalars convert without allocating.
macro_rules! scalar {
    ($($t:ty: $p:pat => $e:expr),* $(,)?) => {$(
        impl FromValue for $t {
            type Item<'a> = $t;
            fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<$t, Ctl> {
                match v {
                    $p => Ok($e),
                    _ => mismatch(stringify!($t)),
                }
            }
        }
    )*};
}

scalar! {
    i64: Value::Int(i) => i,
    f64: Value::Float(f) => f,
    bool: Value::Bool(b) => b,
}

impl FromValue for () {
    type Item<'a> = ();
    fn from_value(_: &Vm, _: Value, _: &mut Conversion) -> Result<(), Ctl> {
        Ok(())
    }
}

impl IntoValue for i64 {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        Ok(Value::Int(self))
    }
}
impl IntoValue for f64 {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        Ok(Value::Float(self))
    }
}
impl IntoValue for bool {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        Ok(Value::Bool(self))
    }
}
impl IntoValue for () {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        Ok(Value::Unit)
    }
}

impl FromValue for &str {
    type Item<'a> = &'a str;
    fn from_value<'a>(vm: &'a Vm, v: Value, _: &mut Conversion) -> Result<&'a str, Ctl> {
        match vm.heap.get(v) {
            Obj::Str(s) => Ok(s),
            _ => mismatch("str"),
        }
    }
}
impl FromValue for String {
    type Item<'a> = String;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<String, Ctl> {
        let s = <&str>::from_value(vm, v, conv)?;
        conv.alloc(vm, s.len())?;
        vm.charge_bytes(s.len())?;
        Ok(s.to_string())
    }
}
impl IntoValue for String {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.reserve(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_str(self))
    }
}
impl IntoValue for &str {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.reserve(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_str(self))
    }
}

impl FromValue for &[u8] {
    type Item<'a> = &'a [u8];
    fn from_value<'a>(vm: &'a Vm, v: Value, _: &mut Conversion) -> Result<&'a [u8], Ctl> {
        match vm.heap.get(v) {
            Obj::Bytes(b) => Ok(b),
            _ => mismatch("bytes"),
        }
    }
}
impl FromValue for Vec<u8> {
    type Item<'a> = Vec<u8>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Vec<u8>, Ctl> {
        let b = <&[u8]>::from_value(vm, v, conv)?;
        conv.alloc(vm, b.len())?;
        vm.charge_bytes(b.len())?;
        Ok(b.to_vec())
    }
}
impl IntoValue for Vec<u8> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.reserve(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_bytes(self))
    }
}
impl IntoValue for &[u8] {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        self.to_vec().into_value(vm)
    }
}

/// Element types of lists (`Vec<T>`, `[T]` in Sand): every type with a
/// Sand equivalent except `u8` (`Vec<u8>` is `bytes`).
pub trait ListElem: SandType {}
impl ListElem for i64 {}
impl ListElem for f64 {}
impl ListElem for bool {}
impl ListElem for () {}
impl ListElem for String {}
impl ListElem for &str {}
impl ListElem for &[u8] {}
impl ListElem for Vec<u8> {}
impl<T: ListElem> ListElem for Vec<T> {}
impl<T: SandType> ListElem for Option<T> {}
impl<T: SandType> ListElem for HashMap<String, T> {}
impl<T: SandType> ListElem for BTreeMap<String, T> {}
impl<T: ValueType> ListElem for T {}

impl<T: ListElem> SandType for Vec<T> {
    fn sand_type(cx: &mut TypeContext) -> String {
        format!("[{}]", T::sand_type(cx))
    }
}
impl<T: ListElem + FromValue> FromValue for Vec<T> {
    type Item<'a> = Vec<T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        let Obj::List(items) = vm.heap.get(v) else { return mismatch("list") };
        conv.alloc(vm, items.len().saturating_mul(std::mem::size_of::<T::Item<'a>>()))?;
        vm.charge(items.len() as u64)?;
        items.iter().map(|&x| T::from_value(vm, x, conv)).collect()
    }
}
impl<T: ListElem + IntoValue> IntoValue for Vec<T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.reserve(self.len().saturating_mul(std::mem::size_of::<Value>()))?;
        vm.charge(self.len() as u64)?;
        let mut items = Vec::with_capacity(self.len());
        for x in self {
            items.push(x.into_value(vm)?);
        }
        Ok(vm.heap.new_list(items))
    }
}

impl<T: SandType> SandType for Option<T> {
    fn sand_type(cx: &mut TypeContext) -> String {
        format!("{}?", T::sand_type(cx))
    }
}
impl<T: FromValue> FromValue for Option<T> {
    type Item<'a> = Option<T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        vm.as_option(v).map(|x| T::from_value(vm, x, conv)).transpose()
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        Ok(match self {
            Some(x) => {
                let v = x.into_value(vm)?;
                vm.some(v)
            }
            None => vm.none(),
        })
    }
}

impl<T: SandType, E> SandType for Result<T, E> {
    fn sand_type(cx: &mut TypeContext) -> String {
        format!("Result<{}, Error>", T::sand_type(cx))
    }
}
impl<T: IntoValue, E: IntoHostError> IntoValue for Result<T, E> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        match self {
            Ok(x) => {
                let v = x.into_value(vm)?;
                Ok(vm.ok(v))
            }
            Err(e) => {
                let e = e.into_host_error();
                vm.reserve(e.kind.len() + e.message.len())?;
                Ok(vm.err_value(&e.kind, &e.message))
            }
        }
    }
}

impl<T: SandType> SandType for HashMap<String, T> {
    fn sand_type(cx: &mut TypeContext) -> String {
        format!("[str: {}]", T::sand_type(cx))
    }
}
impl<T: SandType> SandType for BTreeMap<String, T> {
    fn sand_type(cx: &mut TypeContext) -> String {
        format!("[str: {}]", T::sand_type(cx))
    }
}
/// The entries of a `[str: T]` map.
fn read_map<'a, T: FromValue>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Vec<(String, T::Item<'a>)>, Ctl> {
    let Obj::Map(m) = vm.heap.get(v) else { return mismatch("map") };
    conv.alloc(vm, m.live.saturating_mul(std::mem::size_of::<(String, T::Item<'a>)>()))?;
    vm.charge(m.live as u64)?;
    m.entries.iter().flatten().map(|&(k, x)| Ok((String::from_value(vm, k, conv)?, T::from_value(vm, x, conv)?))).collect()
}
fn write_map<T: IntoValue>(vm: &mut Vm, len: usize, items: impl IntoIterator<Item = (String, T)>) -> Result<Value, Ctl> {
    vm.reserve(len.saturating_mul(4 * std::mem::size_of::<Value>()))?;
    let m = vm.heap.alloc(Obj::Map(Box::default()));
    for (k, v) in items {
        let k = k.into_value(vm)?;
        let v = v.into_value(vm)?;
        vm.map_insert(m, k, v)?;
    }
    Ok(m)
}
impl<T: FromValue> FromValue for HashMap<String, T> {
    type Item<'a> = HashMap<String, T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        Ok(read_map::<T>(vm, v, conv)?.into_iter().collect())
    }
}
impl<T: IntoValue> IntoValue for HashMap<String, T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        let mut items: Vec<(String, T)> = self.into_iter().collect();
        vm.charge(items.len() as u64)?;
        items.sort_by(|a, b| a.0.cmp(&b.0));
        write_map(vm, items.len(), items)
    }
}
impl<T: FromValue> FromValue for BTreeMap<String, T> {
    type Item<'a> = BTreeMap<String, T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        Ok(read_map::<T>(vm, v, conv)?.into_iter().collect())
    }
}
impl<T: IntoValue> IntoValue for BTreeMap<String, T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        write_map(vm, self.len(), self)
    }
}

macro_rules! tuple_impls {
    ($($n:ident $i:tt),+) => {
        impl<$($n: SandType),+> SandType for ($($n,)+) {
            fn sand_type(cx: &mut TypeContext) -> String {
                let parts: Vec<String> = vec![$($n::sand_type(cx)),+];
                format!("({})", parts.join(", "))
            }
        }
        impl<$($n: SandType),+> ListElem for ($($n,)+) {}
        impl<$($n: FromValue),+> FromValue for ($($n,)+) {
            type Item<'a> = ($($n::Item<'a>,)+);
            fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
                let pair;
                let f: &[Value] = match v {
                    Value::Pair(a, b) => {
                        pair = [Value::Int(a as i64), Value::Int(b)];
                        &pair
                    }
                    _ => match vm.heap.get(v) {
                        Obj::Tuple(f) => f,
                        _ => return mismatch("tuple"),
                    },
                };
                Ok(($($n::from_value(vm, *f.get($i).ok_or_else(|| Ctl::internal("tuple too short"))?, conv)?,)+))
            }
        }
        impl<$($n: IntoValue),+> IntoValue for ($($n,)+) {
            fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
                let items = [$(self.$i.into_value(vm)?),+];
                vm.reserve(items.len() * std::mem::size_of::<Value>())?;
                Ok(vm.heap.new_tuple(&items))
            }
        }
    };
}
tuple_impls!(A 0, B 1);
tuple_impls!(A 0, B 1, C 2);
tuple_impls!(A 0, B 1, C 2, D 3);

/// A Rust closure usable as a native function: `Fn(A, B, ...) -> R` where
/// the arguments implement [`FromValue`] and the result [`IntoValue`].
pub trait HostFunction<Marker>: Send + Sync + 'static {
    fn param_types(cx: &mut TypeContext) -> Vec<String>;
    fn ret_type(cx: &mut TypeContext) -> String;
    fn into_host(self) -> HostFn;
}

/// A Rust closure returning a future, usable as an async native function.
/// Its arguments must be owned (the future outlives the call).
pub trait AsyncHostFunction<Marker>: Send + Sync + 'static {
    fn param_types(cx: &mut TypeContext) -> Vec<String>;
    fn ret_type(cx: &mut TypeContext) -> String;
    fn into_host(self) -> HostFn;
}

/// A host function whose calls are guarded by a capability that `G`
/// computes from the arguments (see [`NativeModule::guarded_function`]).
pub trait GuardedFunction<Marker, G>: Send + Sync + 'static {
    fn param_types(cx: &mut TypeContext) -> Vec<String>;
    fn ret_type(cx: &mut TypeContext) -> String;
    fn into_host(self, guard: G) -> HostFn;
}

/// The error for too few arguments, possible only when the module's source
/// declares the function.
fn missing() -> Ctl {
    Ctl::internal("host function called with too few arguments")
}

macro_rules! host_fn_impls {
    ($($a:ident),*) => {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit)]
        impl<F, R, $($a,)*> HostFunction<($($a,)*)> for F
        where
            F: Fn($($a),*) -> R + for<'a> Fn($($a::Item<'a>),*) -> R + Send + Sync + 'static,
            R: IntoValue,
            $($a: FromValue + 'static,)*
        {
            fn param_types(cx: &mut TypeContext) -> Vec<String> {
                vec![$($a::sand_type(cx)),*]
            }
            fn ret_type(cx: &mut TypeContext) -> String {
                R::sand_type(cx)
            }
            fn into_host(self) -> HostFn {
                // Calls through the higher-ranked signature (the arguments
                // borrow from the heap).
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Arc::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let r = {
                        let (mut it, mut conv) = (args.iter(), Conversion::new());
                        $(let $a = $a::from_value(vm, *it.next().ok_or_else(missing)?, &mut conv)?;)*
                        call(&self, $($a),*)
                    };
                    r.into_value(vm)
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut)]
        impl<F, Fut, R, $($a,)*> AsyncHostFunction<(Fut, $($a,)*)> for F
        where
            F: Fn($($a),*) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoValue + Send + 'static,
            $($a: for<'a> FromValue<Item<'a> = $a> + 'static,)*
        {
            fn param_types(cx: &mut TypeContext) -> Vec<String> {
                vec![$($a::sand_type(cx)),*]
            }
            fn ret_type(cx: &mut TypeContext) -> String {
                R::sand_type(cx)
            }
            fn into_host(self) -> HostFn {
                Arc::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let (mut it, mut conv) = (args.iter(), Conversion::new());
                    $(let $a = $a::from_value(vm, *it.next().ok_or_else(missing)?, &mut conv)?;)*
                    let fut = self($($a),*);
                    Err(Ctl::suspend(Box::pin(async move {
                        let r = fut.await;
                        Box::new(move |vm: &mut Vm| r.into_value(vm)) as Finisher
                    })))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit)]
        impl<F, G, R, $($a,)*> GuardedFunction<($($a,)*), G> for F
        where
            F: Fn($($a),*) -> R + for<'a> Fn($($a::Item<'a>),*) -> R + Send + Sync + 'static,
            G: for<'a> Fn($(&$a::Item<'a>),*) -> Capability + Send + Sync + 'static,
            R: IntoValue,
            $($a: FromValue + 'static,)*
        {
            fn param_types(cx: &mut TypeContext) -> Vec<String> {
                vec![$($a::sand_type(cx)),*]
            }
            fn ret_type(cx: &mut TypeContext) -> String {
                format!("Result<{}, Error>", R::sand_type(cx))
            }
            fn into_host(self, guard: G) -> HostFn {
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Arc::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let cap = {
                        let (mut it, mut conv) = (args.iter(), Conversion::new());
                        $(let $a = $a::from_value(vm, *it.next().ok_or_else(missing)?, &mut conv)?;)*
                        guard($(&$a),*)
                    };
                    if !vm.permit(&cap)? {
                        return Err(Ctl::native_err("permission_denied", format!("permission denied: {cap}")));
                    }
                    let r = {
                        let (mut it, mut conv) = (args.iter(), Conversion::new());
                        $(let $a = $a::from_value(vm, *it.next().ok_or_else(missing)?, &mut conv)?;)*
                        call(&self, $($a),*)
                    };
                    let v = r.into_value(vm)?;
                    Ok(vm.ok(v))
                })
            }
        }
    };
}
host_fn_impls!();
host_fn_impls!(A);
host_fn_impls!(A, B);
host_fn_impls!(A, B, C);
host_fn_impls!(A, B, C, D);
host_fn_impls!(A, B, C, D, E);
host_fn_impls!(A, B, C, D, E, G1);

/// A function registered with a derived signature.
#[derive(Clone)]
struct Decl {
    name: String,
    params: Vec<String>,
    names: Option<Vec<String>>,
    ret: String,
}

impl Decl {
    fn render(&self) -> String {
        let ps: Vec<String> = self
            .params
            .iter()
            .enumerate()
            .map(|(i, t)| match self.names.as_ref().and_then(|n| n.get(i)) {
                Some(n) => format!("{n}: {t}"),
                None => format!("arg{}: {t}", i + 1),
            })
            .collect();
        format!("pub native fn {}({}) -> {}", self.name, ps.join(", "), self.ret)
    }
}

impl<T: ValueType> SandType for T {
    fn sand_type(cx: &mut TypeContext) -> String {
        cx.registered::<T>(T::MODULE, T::NAME)
    }
}
impl<T: ValueType> FromValue for T {
    type Item<'a> = T;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<T, Ctl> {
        match vm.heap.value::<T>(v) {
            Some(x) => Ok(x),
            None => mismatch(T::NAME),
        }
    }
}
impl<T: ValueType> IntoValue for T {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        if T::PACKING.is_none() {
            vm.reserve(ValueType::size(&self))?;
        }
        Ok(vm.new_value(self))
    }
}

/// A value or enum type of a host module.
#[derive(Clone)]
struct HostType {
    name: &'static str,
    register: fn(&mut Registry),
    /// The declaration added if the module source does not declare the type.
    decl: String,
}

/// A host-provided module, imported by scripts with `import <name>`.
#[derive(Clone)]
pub struct NativeModule {
    name: String,
    source: String,
    decls: Vec<Decl>,
    functions: Vec<(String, HostFn)>,
    types: Vec<HostType>,
    imports: BTreeSet<&'static str>,
    used_types: Vec<(String, TypeId)>,
    docs: HashMap<String, String>,
}

impl NativeModule {
    /// A module imported as `name`.
    ///
    /// # Panics
    ///
    /// If `name` is the name of a standard module (or of the prelude).
    pub fn new(name: &str) -> NativeModule {
        assert!(!crate::stdlib::STD_MODULES.contains(&name), "a host module cannot be named `{name}`: it is the name of a standard module");
        NativeModule {
            name: name.to_string(),
            source: String::new(),
            decls: Vec::new(),
            functions: Vec::new(),
            types: Vec::new(),
            imports: BTreeSet::new(),
            used_types: Vec::new(),
            docs: HashMap::new(),
        }
    }

    /// The name scripts import the module by.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn add(&mut self, name: &str, decl: Option<Decl>, f: HostFn) {
        assert!(!self.functions.iter().any(|(n, _)| n == name), "function `{name}` is registered twice in host module `{}`", self.name);
        self.decls.extend(decl);
        self.functions.push((name.to_string(), f));
    }

    /// Registers `T` as a value type of this module (see [`ValueType`]).
    /// Adds `pub native type <T::NAME>` if the module source does not declare it.
    ///
    /// # Panics
    ///
    /// Panics if `T::MODULE` does not match this module's name, if `T::NAME` is
    /// not a capitalized identifier, or if `T::NAME` is already registered.
    pub fn value_type<T: ValueType>(&mut self) -> &mut Self {
        self.add_type(T::MODULE, T::NAME, |r| r.add_value_type::<T>(), format!("pub native type {}", T::NAME))
    }

    /// Registers `T` as an enum type of this module (see [`EnumType`]). Adds
    /// `pub enum <T::NAME> { ... }` with the variants of `T` if the module
    /// source does not declare it.
    ///
    /// # Panics
    ///
    /// Panics if `T::MODULE` does not match this module's name, if `T::NAME` is
    /// not a capitalized identifier, or if `T::NAME` is already registered.
    pub fn enum_type<T: EnumType>(&mut self) -> &mut Self {
        let variants: Vec<&str> = T::VARIANTS.iter().map(|&(name, _)| name).collect();
        let decl = format!("pub enum {} {{ {} }}", T::NAME, variants.join(", "));
        self.add_type(T::MODULE, T::NAME, |r| r.add_enum_type::<T>(), decl)
    }

    fn add_type(&mut self, module: &str, name: &'static str, register: fn(&mut Registry), decl: String) -> &mut Self {
        assert_eq!(module, self.name, "type `{name}` belongs to module `{module}`");
        crate::vm::value::check_type_name(name);
        assert!(!self.types.iter().any(|t| t.name == name), "type `{name}` is registered twice in host module `{}`", self.name);
        self.types.push(HostType { name, register, decl });
        self
    }

    fn derive_decl(&mut self, name: &str, params: fn(&mut TypeContext) -> Vec<String>, ret: fn(&mut TypeContext) -> String) -> Decl {
        let mut cx = TypeContext { module: self.name.clone(), imports: BTreeSet::new(), used_types: Vec::new() };
        let decl = Decl { name: name.into(), params: params(&mut cx), names: None, ret: ret(&mut cx) };
        self.imports.extend(cx.imports);
        self.used_types.extend(cx.used_types);
        decl
    }

    /// Registers a synchronous function; its Sand signature is derived from
    /// the Rust types (name the parameters with [`NativeModule::params`]).
    /// Arguments of type `&str` and `&[u8]` borrow the script's values
    /// instead of copying them.
    ///
    /// # Panics
    ///
    /// If a function of this name is already registered.
    pub fn function<M, F: HostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        let decl = self.derive_decl(name, F::param_types, F::ret_type);
        self.add(name, Some(decl), f.into_host());
        self
    }

    /// Registers an async function: the calling task is suspended until the future completes.
    pub fn async_function<M, F: AsyncHostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        let decl = self.derive_decl(name, F::param_types, F::ret_type);
        self.add(name, Some(decl), f.into_host());
        self
    }

    /// Registers a function guarded by a capability that `guard` computes
    /// from the arguments (given by reference). The Sand return type is
    /// `Result<R, Error>`; a denial yields a `permission_denied` error.
    pub fn guarded_function<M, G, F: GuardedFunction<M, G>>(&mut self, name: &str, guard: G, f: F) -> &mut Self {
        let decl = self.derive_decl(name, F::param_types, F::ret_type);
        self.add(name, Some(decl), f.into_host(guard));
        self
    }

    /// Names the parameters of the function registered last (shown by
    /// `help`, and usable as named arguments); by default they are `arg1`,
    /// `arg2`, ...
    ///
    /// # Panics
    ///
    /// If the last function registered has no derived signature, or the
    /// number of names differs from its number of parameters.
    pub fn params(&mut self, names: &[&str]) -> &mut Self {
        let last = self.functions.last().map(|(n, _)| n.as_str());
        let Some(decl) = self.decls.last_mut().filter(|d| Some(d.name.as_str()) == last) else {
            panic!(
                "`params` specifies the parameters of the function registered last by `function`, `async_function` or `guarded_function`"
            )
        };
        assert_eq!(names.len(), decl.params.len(), "function `{}` has {} parameters", decl.name, decl.params.len());
        decl.names = Some(names.iter().map(|n| n.to_string()).collect());
        self
    }

    /// Registers a function with direct access to the VM. Its signature must be
    /// declared in [`NativeModule::source`] with `native fn`.
    pub fn raw_function(&mut self, name: &str, f: impl Fn(&mut Vm, &[Value]) -> NResult + Send + Sync + 'static) -> &mut Self {
        self.add(name, None, Arc::new(f));
        self
    }

    /// Registers the native method of a trait impl declared in
    /// [`NativeModule::source`]: `"Conn.close"` for `impl Close for Conn {
    /// native fn close(self) }`. `f` receives `self` and the other arguments.
    pub fn raw_method(&mut self, name: &str, f: impl Fn(&mut Vm, &[Value]) -> NResult + Send + Sync + 'static) -> &mut Self {
        // Trait methods are passed the impl's dictionary first.
        self.add(name, None, Arc::new(move |vm: &mut Vm, a: &[Value]| f(vm, a.get(1..).unwrap_or_default())));
        self
    }

    /// Adds Sand declarations (types, helper functions, `native fn`
    /// signatures). A `native fn` declared here takes precedence over the
    /// signature derived for a function of the same name.
    pub fn source(&mut self, src: &str) -> &mut Self {
        self.source.push_str(src);
        self.source.push('\n');
        self
    }

    /// Documents the function or type `name` (shown by `help`). Declarations
    /// in [`NativeModule::source`] take their doc comments from the source.
    pub fn doc(&mut self, name: &str, text: &str) -> &mut Self {
        self.docs.insert(name.to_string(), text.to_string());
        self
    }

    pub(crate) fn register_types(&self, r: &mut Registry) {
        for t in &self.types {
            (t.register)(r);
        }
    }

    pub(crate) fn register(&self, r: &mut Registry) {
        for (name, id) in &self.used_types {
            let registered =
                r.by_name.values.get(name.as_str()).map(|v| v.type_id).or_else(|| r.by_name.enums.get(name.as_str()).map(|e| e.type_id));
            match registered {
                Some(t) => assert!(t == *id, "type `{name}` is registered for another Rust type"),
                None => panic!("type `{name}` is not registered (see `NativeModule::value_type` and `NativeModule::enum_type`)"),
            }
        }
        let (mut fns, mut types, mut imports) = (HashSet::new(), HashSet::new(), HashSet::new());
        for item in crate::syntax::parse_module(&self.source, 0).module.items {
            match item.kind {
                ItemKind::Fn(f) if f.is_native => fns.insert(f.name.name),
                ItemKind::NativeType(t) => types.insert(t.name.name),
                ItemKind::Enum(e) => types.insert(e.name.name),
                ItemKind::Import(Import { source: ImportSource::Name(m), alias: None, names: None }) => imports.insert(m.name),
                _ => false,
            };
        }
        let mut src: String = self.imports.iter().filter(|m| !imports.contains(**m)).map(|m| format!("import {m}\n")).collect();
        src.push_str(&self.source);
        let doc = |src: &mut String, name: &str| {
            for line in self.docs.get(name).into_iter().flat_map(|d| d.lines()) {
                src.push_str(&format!("/// {line}\n"));
            }
        };
        for t in self.types.iter().filter(|t| !types.contains(t.name)) {
            doc(&mut src, t.name);
            src.push_str(&t.decl);
            src.push('\n');
        }
        for decl in self.decls.iter().filter(|d| !fns.contains(&d.name)) {
            doc(&mut src, &decl.name);
            src.push_str(&decl.render());
            src.push('\n');
        }
        r.source(self.name.clone(), ModKind::Host, src);
        for (name, f) in &self.functions {
            r.add(format!("{}.{}", self.name, name), NativeImpl::Host(f.clone()));
        }
    }
}
