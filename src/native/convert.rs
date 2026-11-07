//! Value conversion between Sand and Rust.

use super::ty::{Never, Num, Param, Reflect, SandType, Val};
use super::{Constant, RustVariant};
use crate::types::ConstVal;
use crate::types::traits;
use crate::vm::meter::Scratch;
use crate::vm::value::{Obj, Value, ValueType};
use crate::vm::{AfterEffect, Ctl, Fail, Vm};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;

/// Converts a Sand [`Value`] into a Rust argument for a native function.
///
/// For functions registered with [`NativeModule::function`](super::NativeModule::function),
/// [`FromValue::Item`] can borrow directly from the VM heap (`&str`, `&[u8]`,
/// `&T`). Owned types (`String`, `Vec<T>`, ...) allocate a Rust copy.
///
/// Because a Sand value can contain shared references to the same heap object,
/// converting a value into owned Rust types copies each reachable reference.
/// Implementations must charge fuel to `vm` and record allocated bytes in
/// [`Conversion`] so that instruction and memory limits bound exponential
/// expansion of shared values.
pub trait FromValue: SandType {
    type Item<'a>;

    /// Converts `v` into [`Self::Item`], charging fuel to `vm` and recording
    /// temporary Rust allocations in `conv`.
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail>;
}

/// Tracks temporary Rust memory allocated while converting arguments for a
/// native call and enforces the VM memory limit.
pub struct Conversion {
    bytes: usize,
    scratch: Scratch,
    depth: Depth,
}

/// How many structs and enum variants deep a conversion between Sand and
/// Rust is. A conversion fails at a limit, so that a deep value cannot
/// overflow the stack.
#[derive(Clone, Copy, Debug, Default)]
pub struct Depth(usize);

impl Depth {
    const MAX: usize = 256;

    /// The depth inside one more struct or enum variant.
    pub(crate) fn enter(self) -> Result<Depth, Fail> {
        if self.0 < Depth::MAX { Ok(Depth(self.0 + 1)) } else { Err(Fail::new("limit", "value too deeply nested to convert")) }
    }
}

impl Conversion {
    pub(crate) fn new() -> Conversion {
        Conversion { bytes: 0, scratch: Scratch::new(), depth: Depth::default() }
    }

    pub(crate) fn enter_struct<R>(&mut self, f: impl FnOnce(&mut Conversion) -> Result<R, Fail>) -> Result<R, Fail> {
        let outer = self.depth;
        self.depth = outer.enter()?;
        let r = f(self);
        self.depth = outer;
        r
    }

    /// Records `bytes` of temporary Rust allocation and checks the VM memory limit.
    pub fn alloc(&mut self, vm: &Vm, bytes: usize) -> Result<(), Fail> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.scratch.check(&vm.heap, self.bytes)
    }
}

/// Converts a Rust return value into a Sand [`Value`], charging fuel.
///
/// The conversion follows the native's effect, so it allocates through an
/// [`AfterEffect`], which does not check the memory limit: the engine
/// enforces the limit once it roots the value.
pub trait IntoValue: SandType {
    /// Converts `self`, which is nested `depth` structs and enum variants
    /// deep in the value that a native returns. Give `depth` to the
    /// conversion of each part of `self`.
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail>;

    /// Converts `self` as a whole value.
    fn into_value(self, fx: &mut AfterEffect<'_>) -> Result<Value, Fail>
    where
        Self: Sized,
    {
        self.into_value_at(fx, Depth::default())
    }

    /// Converts `self` into a compile-time Sand [`Constant`], or returns `None`
    /// if `self` cannot be represented as a constant.
    ///
    /// Scalars (`i64`, `f64`, `bool`, `()`, `String`, `Vec<u8>`,
    /// [`jiff::SignedDuration`]) and collections, options,
    /// [`SandEnum`](crate::SandEnum)s, and [`SandStruct`](crate::SandStruct)s
    /// composed of constants can be converted. Field defaults declared with
    /// `#[sand(default)]` require a constant value.
    fn to_const(&self) -> Option<Constant> {
        None
    }
}

/// Error returned by a native function whose Sand return type is `Result<T>`.
///
/// Construct a recoverable script error with [`HostError::new`], or propagate
/// a [`Ctl`] or a [`Fail`] (`C`) or a `std::io::Error` with `?`. A native
/// returns a `HostError`, which can hold a request to the scheduler, such as
/// a prompt for a permission. The code that can make no request, such as the
/// finisher of a [`Later`](super::func::Later) and the body of a journaled function, returns a
/// `HostError<Fail>`, which can only hold a failure:
///
/// ```compile_fail,E0277
/// # use sandlang::native::convert::HostError;
/// # use sandlang::vm::{Ctl, Fail};
/// fn finish(c: Ctl) -> Result<i64, HostError<Fail>> {
///     Err(c)?
/// }
/// ```
pub struct HostError<C = Ctl>(Box<HostErrorKind<C>>);

enum HostErrorKind<C> {
    Error { kind: String, message: String },
    Abort(C),
}

impl<C> HostError<C> {
    /// Creates a recoverable Sand `Error` with the given `kind` (such as
    /// `"not_found"`, `"invalid"`, or `"permission_denied"`; see SPEC §5.4) and
    /// `message`.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> HostError<C> {
        HostError(Box::new(HostErrorKind::Error { kind: kind.into(), message: message.into() }))
    }

    /// The Sand `Error` value of a recoverable error, or the control that
    /// the error ends the call with.
    pub(crate) fn into_sand(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Result<Value, C>, Fail> {
        match *self.0 {
            HostErrorKind::Error { kind, message } => {
                let e = crate::stdlib::core::Error { kind, message }.into_value_at(fx, depth)?;
                Ok(Ok(fx.err(e)))
            }
            HostErrorKind::Abort(c) => Ok(Err(c)),
        }
    }
}

impl From<Ctl> for HostError<Ctl> {
    fn from(c: Ctl) -> HostError<Ctl> {
        HostError(Box::new(HostErrorKind::Abort(c)))
    }
}

impl<C: From<Fail>> From<Fail> for HostError<C> {
    fn from(f: Fail) -> HostError<C> {
        HostError(Box::new(HostErrorKind::Abort(f.into())))
    }
}

impl<C> From<crate::stdlib::core::Error> for HostError<C> {
    fn from(e: crate::stdlib::core::Error) -> HostError<C> {
        HostError::new(e.kind, e.message)
    }
}

impl<C> From<std::io::Error> for HostError<C> {
    fn from(e: std::io::Error) -> HostError<C> {
        HostError::new(crate::stdlib::io_kind(&e), e.to_string())
    }
}

impl<C: std::fmt::Debug> std::fmt::Debug for HostError<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            HostErrorKind::Error { kind, message } => write!(f, "HostError({kind}: {message})"),
            HostErrorKind::Abort(c) => write!(f, "HostError({c:?})"),
        }
    }
}

/// Converts a Rust error into a [`HostError`] for native functions returning
/// `Result<T, E>`. The error can hold no request to the scheduler.
pub trait IntoHostError {
    fn into_host_error(self) -> HostError<Fail>;
}

impl IntoHostError for HostError<Fail> {
    fn into_host_error(self) -> HostError<Fail> {
        self
    }
}

impl IntoHostError for std::io::Error {
    fn into_host_error(self) -> HostError<Fail> {
        self.into()
    }
}

impl IntoHostError for String {
    fn into_host_error(self) -> HostError<Fail> {
        HostError::new("error", self)
    }
}

impl IntoHostError for &str {
    fn into_host_error(self) -> HostError<Fail> {
        HostError::new("error", self)
    }
}

#[cold]
#[doc(hidden)]
pub fn mismatch<S: SandType + ?Sized>() -> Fail {
    Fail::internal(format!("a native function expected a value of type `{}`", S::sand_type()))
}

macro_rules! scalar {
    ($($t:ty: $p:pat => $e:expr, $v:ident => $into:expr, $c:path),* $(,)?) => {$(
        impl FromValue for $t {
            type Item<'a> = $t;
            fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<$t, Fail> {
                match v {
                    $p => Ok($e),
                    _ => Err(mismatch::<$t>()),
                }
            }
        }
        impl IntoValue for $t {
            fn into_value_at(self, _: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
                let $v = self;
                Ok($into)
            }
            fn to_const(&self) -> Option<Constant> {
                Some(Constant($c(*self)))
            }
        }
    )*};
}

scalar! {
    i64: Value::Int(i) => i, x => Value::Int(x), ConstVal::Int,
    f64: Value::Float(f) => f, x => Value::Float(x), ConstVal::Float,
    bool: Value::Bool(b) => b, x => Value::Bool(x), ConstVal::Bool,
}

impl FromValue for () {
    type Item<'a> = ();
    fn from_value(_: &Vm, _: Value, _: &mut Conversion) -> Result<(), Fail> {
        Ok(())
    }
}
impl IntoValue for () {
    fn into_value_at(self, _: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        Ok(Value::Unit)
    }
    fn to_const(&self) -> Option<Constant> {
        Some(Constant(ConstVal::Unit))
    }
}

impl FromValue for jiff::SignedDuration {
    type Item<'a> = jiff::SignedDuration;
    fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Fail> {
        match v {
            Value::Dur(n) => Ok(jiff::SignedDuration::from_nanos(n)),
            _ => Err(mismatch::<Self>()),
        }
    }
}

impl IntoValue for jiff::SignedDuration {
    fn into_value_at(self, _: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        match i64::try_from(self.as_nanos()) {
            Ok(n) => Ok(Value::Dur(n)),
            Err(_) => Err(Fail::new("overflow", "the duration is out of range")),
        }
    }
    fn to_const(&self) -> Option<Constant> {
        i64::try_from(self.as_nanos()).ok().map(|d| Constant(ConstVal::Dur(d)))
    }
}

impl IntoValue for Never {
    fn into_value_at(self, _: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        match self {}
    }
}

impl FromValue for &str {
    type Item<'a> = &'a str;
    fn from_value<'a>(vm: &'a Vm, v: Value, _: &mut Conversion) -> Result<&'a str, Fail> {
        match vm.heap.get(v) {
            Obj::Str(s) => Ok(s),
            Obj::StrBuf(s) => Ok(s),
            _ => Err(mismatch::<String>()),
        }
    }
}
impl FromValue for String {
    type Item<'a> = String;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<String, Fail> {
        let s = <&str>::from_value(vm, v, conv)?;
        conv.alloc(vm, s.len())?;
        vm.charge_bytes(s.len())?;
        Ok(s.to_string())
    }
}
impl IntoValue for String {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        fx.charge_bytes(self.len())?;
        Ok(fx.heap_mut().new_str(self))
    }
    fn to_const(&self) -> Option<Constant> {
        Some(Constant(ConstVal::Str(self.clone())))
    }
}
impl IntoValue for &str {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        fx.charge_bytes(self.len())?;
        Ok(fx.heap_mut().new_str(self))
    }
}

impl FromValue for &[u8] {
    type Item<'a> = &'a [u8];
    fn from_value<'a>(vm: &'a Vm, v: Value, _: &mut Conversion) -> Result<&'a [u8], Fail> {
        match vm.heap.get(v) {
            Obj::Bytes(b) => Ok(b),
            _ => Err(mismatch::<Vec<u8>>()),
        }
    }
}
impl FromValue for Vec<u8> {
    type Item<'a> = Vec<u8>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Vec<u8>, Fail> {
        let b = <&[u8]>::from_value(vm, v, conv)?;
        conv.alloc(vm, b.len())?;
        vm.charge_bytes(b.len())?;
        Ok(b.to_vec())
    }
}
impl IntoValue for Vec<u8> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        fx.charge_bytes(self.len())?;
        Ok(fx.heap_mut().new_bytes(self))
    }
    fn to_const(&self) -> Option<Constant> {
        Some(Constant(ConstVal::Bytes(self.clone())))
    }
}
impl IntoValue for &[u8] {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
        fx.charge_bytes(self.len())?;
        Ok(fx.heap_mut().new_bytes(self))
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    type Item<'a> = Vec<T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        let Obj::List(items) = vm.heap.get(v) else { return Err(mismatch::<Self>()) };
        conv.alloc(vm, items.len().saturating_mul(std::mem::size_of::<T::Item<'a>>()))?;
        vm.charge(items.len() as u64)?;
        items.iter().map(|&x| T::from_value(vm, x, conv)).collect()
    }
}
impl<T: IntoValue> IntoValue for Vec<T> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        fx.charge(self.len() as u64)?;
        let mut items = Vec::with_capacity(self.len());
        for x in self {
            items.push(x.into_value_at(fx, depth)?);
        }
        Ok(fx.heap_mut().new_list(items))
    }
    fn to_const(&self) -> Option<Constant> {
        Some(Constant(ConstVal::List(super::collect_consts(self.iter().map(T::to_const))?)))
    }
}

impl<T: for<'a> FromValue<Item<'a> = T>> FromValue for Box<T> {
    type Item<'a> = Box<T>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Box<T>, Fail> {
        conv.alloc(vm, std::mem::size_of::<T>())?;
        T::from_value(vm, v, conv).map(Box::new)
    }
}
impl<T: IntoValue> IntoValue for Box<T> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        (*self).into_value_at(fx, depth)
    }
    fn to_const(&self) -> Option<Constant> {
        (**self).to_const()
    }
}

impl<T: FromValue> FromValue for Option<T> {
    type Item<'a> = Option<T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        vm.as_option(v).map(|x| T::from_value(vm, x, conv)).transpose()
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        Ok(match self {
            Some(x) => {
                let v = x.into_value_at(fx, depth)?;
                fx.some(v)
            }
            None => fx.none(),
        })
    }
    fn to_const(&self) -> Option<Constant> {
        Some(Constant(match self {
            Some(x) => ConstVal::Variant(RustVariant::Some, vec![x.to_const()?.0]),
            None => ConstVal::Variant(RustVariant::None, Vec::new()),
        }))
    }
}

impl<T: IntoValue, E: IntoHostError> IntoValue for Result<T, E> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        match self {
            Ok(x) => {
                let v = x.into_value_at(fx, depth)?;
                Ok(fx.ok(v))
            }
            Err(e) => e.into_host_error().into_sand(fx, depth)?,
        }
    }
}

impl<T: IntoValue> IntoValue for Result<T, Fail> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        self?.into_value_at(fx, depth)
    }
}

fn sort_cost(n: usize) -> u64 {
    (n as u64).saturating_mul(u64::from(usize::BITS - n.leading_zeros()))
}

fn read_map<'a, K, V, C>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<C, Fail>
where
    K: for<'k> FromValue<Item<'k> = K>,
    V: FromValue,
    C: FromIterator<(K, V::Item<'a>)>,
{
    let Obj::Map(m) = vm.heap.get(v) else { return Err(mismatch::<HashMap<K, V>>()) };
    conv.alloc(vm, m.live.saturating_mul(std::mem::size_of::<(K, V::Item<'a>)>()))?;
    vm.charge(m.live as u64)?;
    m.entries.iter().flatten().map(|&(k, x)| Ok((K::from_value(vm, k, conv)?, V::from_value(vm, x, conv)?))).collect()
}
fn write_map<K: IntoValue, V: IntoValue>(
    fx: &mut AfterEffect<'_>,
    depth: Depth,
    items: impl IntoIterator<Item = (K, V)>,
) -> Result<Value, Fail> {
    let m = fx.heap_mut().alloc(Obj::Map(Box::default()));
    for (k, v) in items {
        let k = k.into_value_at(fx, depth)?;
        let v = v.into_value_at(fx, depth)?;
        fx.map_insert(m, k, v)?;
    }
    Ok(m)
}
impl<K: for<'k> FromValue<Item<'k> = K> + Eq + Hash, V: FromValue> FromValue for HashMap<K, V> {
    type Item<'a> = HashMap<K, V::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
/// Sorts keys before insertion so the Sand map's iteration order is
/// deterministic.
impl<K: IntoValue + Ord, V: IntoValue> IntoValue for HashMap<K, V> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        let mut items: Vec<(K, V)> = self.into_iter().collect();
        fx.charge(sort_cost(items.len()))?;
        items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        write_map(fx, depth, items)
    }
    fn to_const(&self) -> Option<Constant> {
        let mut items: Vec<(&K, &V)> = self.iter().collect();
        items.sort_unstable_by(|a, b| a.0.cmp(b.0));
        map_const(items)
    }
}
impl<K: for<'k> FromValue<Item<'k> = K> + Ord, V: FromValue> FromValue for BTreeMap<K, V> {
    type Item<'a> = BTreeMap<K, V::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
impl<K: IntoValue, V: IntoValue> IntoValue for BTreeMap<K, V> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        write_map(fx, depth, self)
    }
    fn to_const(&self) -> Option<Constant> {
        map_const(self)
    }
}

impl<K: for<'k> FromValue<Item<'k> = K> + Eq + Hash, V: FromValue> FromValue for indexmap::IndexMap<K, V> {
    type Item<'a> = indexmap::IndexMap<K, V::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
impl<K: IntoValue, V: IntoValue> IntoValue for indexmap::IndexMap<K, V> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        write_map(fx, depth, self)
    }
    fn to_const(&self) -> Option<Constant> {
        map_const(self)
    }
}

fn map_const<'a, K: IntoValue + 'a, V: IntoValue + 'a>(entries: impl IntoIterator<Item = (&'a K, &'a V)>) -> Option<Constant> {
    let entries = entries.into_iter().map(|(k, v)| Some((k.to_const()?.0, v.to_const()?.0))).collect::<Option<_>>()?;
    Some(Constant(ConstVal::Map(entries)))
}

fn read_set<T: for<'k> FromValue<Item<'k> = T>, C: FromIterator<T>>(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<C, Fail> {
    let Obj::Set(s) = vm.heap.get(v) else { return Err(mismatch::<HashSet<T>>()) };
    conv.alloc(vm, s.live.saturating_mul(std::mem::size_of::<T>()))?;
    vm.charge(s.live as u64)?;
    s.entries.iter().flatten().map(|&x| T::from_value(vm, x, conv)).collect()
}
fn write_set<T: IntoValue>(fx: &mut AfterEffect<'_>, depth: Depth, items: impl IntoIterator<Item = T>) -> Result<Value, Fail> {
    let s = fx.heap_mut().alloc(Obj::Set(Box::default()));
    for x in items {
        let x = x.into_value_at(fx, depth)?;
        fx.set_add(s, x)?;
    }
    Ok(s)
}
impl<T: for<'k> FromValue<Item<'k> = T> + Eq + Hash> FromValue for HashSet<T> {
    type Item<'a> = HashSet<T>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Self, Fail> {
        read_set(vm, v, conv)
    }
}
/// Sorts elements before insertion so the Sand set's iteration order is
/// deterministic.
impl<T: IntoValue + Ord> IntoValue for HashSet<T> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        let mut items: Vec<T> = self.into_iter().collect();
        fx.charge(sort_cost(items.len()))?;
        items.sort_unstable();
        write_set(fx, depth, items)
    }
    fn to_const(&self) -> Option<Constant> {
        let mut items: Vec<&T> = self.iter().collect();
        items.sort_unstable();
        Some(Constant(ConstVal::Set(super::collect_consts(items.into_iter().map(T::to_const))?)))
    }
}
impl<T: for<'k> FromValue<Item<'k> = T> + Ord> FromValue for BTreeSet<T> {
    type Item<'a> = BTreeSet<T>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Self, Fail> {
        read_set(vm, v, conv)
    }
}
impl<T: IntoValue> IntoValue for BTreeSet<T> {
    fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
        write_set(fx, depth, self)
    }
    fn to_const(&self) -> Option<Constant> {
        Some(Constant(ConstVal::Set(super::collect_consts(self.iter().map(T::to_const))?)))
    }
}

macro_rules! tuple_impls {
    ($len:literal: $($n:ident $i:tt),+) => {
        impl<$($n: SandType),+> SandType for ($($n,)+) {
            fn sand_type() -> crate::native::ty::Type {
                crate::native::ty::Type::Tuple(vec![$($n::sand_type()),+])
            }
        }
        impl<$($n: FromValue),+> FromValue for ($($n,)+) {
            type Item<'a> = ($($n::Item<'a>,)+);
            fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
                let pair;
                let f: &[Value] = match v {
                    Value::Pair(a, b) => {
                        pair = [Value::Int(a as i64), Value::Int(b)];
                        &pair
                    }
                    _ => match vm.heap.get(v) {
                        Obj::Tuple(f) => f,
                        _ => return Err(mismatch::<Self>()),
                    },
                };
                if f.len() != $len {
                    return Err(mismatch::<Self>());
                }
                Ok(($($n::from_value(vm, f[$i], conv)?,)+))
            }
        }
        impl<$($n: IntoValue),+> IntoValue for ($($n,)+) {
            fn into_value_at(self, fx: &mut AfterEffect<'_>, depth: Depth) -> Result<Value, Fail> {
                let items = [$(self.$i.into_value_at(fx, depth)?),+];
                Ok(fx.heap_mut().new_tuple(&items))
            }
            fn to_const(&self) -> Option<Constant> {
                Some(Constant(ConstVal::Tuple(vec![$(self.$i.to_const()?.0),+])))
            }
        }
    };
}
tuple_impls!(2: A 0, B 1);
tuple_impls!(3: A 0, B 1, C 2);
tuple_impls!(4: A 0, B 1, C 2, D 3);
tuple_impls!(5: A 0, B 1, C 2, D 3, E 4);
tuple_impls!(6: A 0, B 1, C 2, D 3, E 4, F 5);
tuple_impls!(7: A 0, B 1, C 2, D 3, E 4, F 5, G 6);
tuple_impls!(8: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);

#[doc(hidden)]
#[macro_export]
macro_rules! __value_type_impls {
    ($t:ty) => {
        impl $crate::native::ty::SandType for $t {
            fn sand_type() -> $crate::native::ty::Type {
                $crate::native::ty::Type::bound::<Self>(::std::vec::Vec::new())
            }
        }
        impl $crate::native::ty::JournalArg for $t {}
        impl $crate::native::convert::FromValue for $t {
            type Item<'a> = Self;
            fn from_value(
                vm: &$crate::vm::Vm,
                v: $crate::vm::value::Value,
                _: &mut $crate::native::convert::Conversion,
            ) -> ::std::result::Result<Self, $crate::vm::Fail> {
                $crate::native::convert::value_from(vm, v)
            }
        }
        impl $crate::native::convert::IntoValue for $t {
            fn into_value_at(
                self,
                fx: &mut $crate::vm::AfterEffect<'_>,
                _: $crate::native::convert::Depth,
            ) -> ::std::result::Result<$crate::vm::value::Value, $crate::vm::Fail> {
                ::std::result::Result::Ok(fx.new_value(self))
            }
        }
    };
}

#[doc(hidden)]
pub fn value_from<T: ValueType>(vm: &Vm, v: Value) -> Result<T, Fail> {
    vm.heap.value::<T>(v).ok_or_else(mismatch::<T>)
}

macro_rules! handle {
    ($([$($g:tt)*] $t:ty),* $(,)?) => {$(
        impl<$($g)*> FromValue for $t {
            type Item<'a> = Self;
            fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Fail> {
                Ok(Self::from_raw(v))
            }
        }
        impl<$($g)*> IntoValue for $t {
            fn into_value_at(self, _: &mut AfterEffect<'_>, _: Depth) -> Result<Value, Fail> {
                Ok(self.raw())
            }
        }
    )*};
}
handle! {
    [T: SandType + ?Sized] Val<T>,
    [const N: u8, B: traits::Set] Param<N, B>,
}

impl<const N: u8> FromValue for Num<N> {
    type Item<'a> = Self;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Fail> {
        match vm.heap.get(v) {
            Obj::Tuple(_) => Ok(Num::from_raw(v)),
            _ => Err(mismatch::<Self>()),
        }
    }
}

impl<const N: u8> FromValue for Reflect<N> {
    type Item<'a> = Self;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Fail> {
        let r = Reflect::from_raw(v);
        r.desc(&vm.heap)?;
        Ok(r)
    }
}
