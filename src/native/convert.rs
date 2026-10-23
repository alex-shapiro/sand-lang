//! Value conversion between Sand and Rust.

use super::func::{Later, NativeError};
use super::ty::{Never, Num, Param, Reflect, SandType, Val, bound::ParamBounds};
use crate::vm::meter::Scratch;
use crate::vm::value::{Obj, Value, ValueType};
use crate::vm::{Ctl, Vm};
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
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl>;
}

/// Tracks temporary Rust memory allocated while converting arguments for a
/// native call and enforces the VM memory limit.
pub struct Conversion {
    bytes: usize,
    scratch: Scratch,
    depth: usize,
}

const MAX_STRUCT_DEPTH: usize = 256;

pub(crate) fn enter_struct(depth: usize) -> Result<usize, Ctl> {
    if depth < MAX_STRUCT_DEPTH { Ok(depth + 1) } else { Err(Ctl::fail("limit", "value too deeply nested to convert")) }
}

impl Conversion {
    pub(crate) fn new() -> Conversion {
        Conversion { bytes: 0, scratch: Scratch::new(), depth: 0 }
    }

    pub(crate) fn enter_struct<R>(&mut self, f: impl FnOnce(&mut Conversion) -> Result<R, Ctl>) -> Result<R, Ctl> {
        self.depth = enter_struct(self.depth)?;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Records `bytes` of temporary Rust allocation and checks the VM memory limit.
    pub fn alloc(&mut self, vm: &Vm, bytes: usize) -> Result<(), Ctl> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.scratch.check(&vm.heap, self.bytes)
    }
}

/// Converts a Rust return value into a Sand [`Value`], charging fuel and
/// reserving heap memory.
pub trait IntoValue: SandType {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl>;
}

/// Error returned by a native function whose Sand return type is `Result<T>`.
///
/// Construct a recoverable script error with [`HostError::new`], or propagate a
/// VM control signal ([`Ctl`]) or `std::io::Error` with `?`.
pub struct HostError(Box<HostErrorKind>);

enum HostErrorKind {
    Error { kind: String, message: String },
    Abort(Ctl),
}

impl HostError {
    /// Creates a recoverable Sand `Error` with the given `kind` (such as
    /// `"not_found"`, `"invalid"`, or `"permission_denied"`; see SPEC §5.4) and
    /// `message`.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> HostError {
        HostError(Box::new(HostErrorKind::Error { kind: kind.into(), message: message.into() }))
    }
}

impl From<Ctl> for HostError {
    fn from(c: Ctl) -> HostError {
        HostError(Box::new(HostErrorKind::Abort(c)))
    }
}

impl From<std::io::Error> for HostError {
    fn from(e: std::io::Error) -> HostError {
        HostError::new(crate::stdlib::io_kind(&e), e.to_string())
    }
}

impl std::fmt::Debug for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            HostErrorKind::Error { kind, message } => write!(f, "HostError({kind}: {message})"),
            HostErrorKind::Abort(c) => write!(f, "HostError({c:?})"),
        }
    }
}

/// Converts a Rust error into a [`HostError`] for native functions returning
/// `Result<T, E>`.
pub trait IntoHostError {
    fn into_host_error(self) -> HostError;
}

impl IntoHostError for HostError {
    fn into_host_error(self) -> HostError {
        self
    }
}

impl IntoHostError for std::io::Error {
    fn into_host_error(self) -> HostError {
        self.into()
    }
}

impl IntoHostError for String {
    fn into_host_error(self) -> HostError {
        HostError::new("error", self)
    }
}

impl IntoHostError for &str {
    fn into_host_error(self) -> HostError {
        HostError::new("error", self)
    }
}

#[cold]
#[doc(hidden)]
pub fn mismatch<S: SandType + ?Sized>() -> Ctl {
    Ctl::internal(format!("a native function expected a value of type `{}`", S::sand_type()))
}

macro_rules! scalar {
    ($($t:ty: $p:pat => $e:expr, $v:ident => $into:expr),* $(,)?) => {$(
        impl FromValue for $t {
            type Item<'a> = $t;
            fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<$t, Ctl> {
                match v {
                    $p => Ok($e),
                    _ => Err(mismatch::<$t>()),
                }
            }
        }
        impl IntoValue for $t {
            fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
                let $v = self;
                Ok($into)
            }
        }
    )*};
}

scalar! {
    i64: Value::Int(i) => i, x => Value::Int(x),
    f64: Value::Float(f) => f, x => Value::Float(x),
    bool: Value::Bool(b) => b, x => Value::Bool(x),
}

impl FromValue for () {
    type Item<'a> = ();
    fn from_value(_: &Vm, _: Value, _: &mut Conversion) -> Result<(), Ctl> {
        Ok(())
    }
}
impl IntoValue for () {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        Ok(Value::Unit)
    }
}

impl FromValue for jiff::SignedDuration {
    type Item<'a> = jiff::SignedDuration;
    fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Ctl> {
        match v {
            Value::Dur(n) => Ok(jiff::SignedDuration::from_nanos(n)),
            _ => Err(mismatch::<Self>()),
        }
    }
}

impl IntoValue for jiff::SignedDuration {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        match i64::try_from(self.as_nanos()) {
            Ok(n) => Ok(Value::Dur(n)),
            Err(_) => Err(Ctl::fail("overflow", "the duration is out of range")),
        }
    }
}

impl IntoValue for Never {
    fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
        match self {}
    }
}

impl FromValue for &str {
    type Item<'a> = &'a str;
    fn from_value<'a>(vm: &'a Vm, v: Value, _: &mut Conversion) -> Result<&'a str, Ctl> {
        match vm.heap.get(v) {
            Obj::Str(s) => Ok(s),
            Obj::StrBuf(s) => Ok(s),
            _ => Err(mismatch::<String>()),
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
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_str(self))
    }
}
impl IntoValue for &str {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_str(self))
    }
}

impl FromValue for &[u8] {
    type Item<'a> = &'a [u8];
    fn from_value<'a>(vm: &'a Vm, v: Value, _: &mut Conversion) -> Result<&'a [u8], Ctl> {
        match vm.heap.get(v) {
            Obj::Bytes(b) => Ok(b),
            _ => Err(mismatch::<Vec<u8>>()),
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
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_bytes(self))
    }
}
impl IntoValue for &[u8] {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_bytes(self))
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    type Item<'a> = Vec<T::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        let Obj::List(items) = vm.heap.get(v) else { return Err(mismatch::<Self>()) };
        conv.alloc(vm, items.len().saturating_mul(std::mem::size_of::<T::Item<'a>>()))?;
        vm.charge(items.len() as u64)?;
        items.iter().map(|&x| T::from_value(vm, x, conv)).collect()
    }
}
impl<T: IntoValue> IntoValue for Vec<T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        vm.check_memory(self.len().saturating_mul(std::mem::size_of::<Value>()))?;
        vm.charge(self.len() as u64)?;
        let mut items = Vec::with_capacity(self.len());
        for x in self {
            items.push(x.into_value(vm)?);
        }
        Ok(vm.heap.new_list(items))
    }
}

impl<T: for<'a> FromValue<Item<'a> = T>> FromValue for Box<T> {
    type Item<'a> = Box<T>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Box<T>, Ctl> {
        conv.alloc(vm, std::mem::size_of::<T>())?;
        T::from_value(vm, v, conv).map(Box::new)
    }
}
impl<T: IntoValue> IntoValue for Box<T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        (*self).into_value(vm)
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

impl<T: IntoValue, E: IntoHostError> IntoValue for Result<T, E> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        match self {
            Ok(x) => {
                let v = x.into_value(vm)?;
                Ok(vm.ok(v))
            }
            Err(e) => match *e.into_host_error().0 {
                HostErrorKind::Error { kind, message } => {
                    let e = crate::stdlib::Error { kind, message }.into_value(vm)?;
                    Ok(vm.err(e))
                }
                HostErrorKind::Abort(c) => Err(c),
            },
        }
    }
}

impl<T: SandType, E: NativeError> IntoValue for Result<Later<T, E>, E> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        match self {
            Ok(later) => later.finish(vm),
            Err(e) => E::finish(Err::<(), E>(e), vm),
        }
    }
}

impl<T: IntoValue> IntoValue for Result<T, Ctl> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        self?.into_value(vm)
    }
}

fn sort_cost(n: usize) -> u64 {
    (n as u64).saturating_mul(u64::from(usize::BITS - n.leading_zeros()))
}

fn read_map<'a, K, V, C>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<C, Ctl>
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
fn write_map<K: IntoValue, V: IntoValue>(vm: &mut Vm, len: usize, items: impl IntoIterator<Item = (K, V)>) -> Result<Value, Ctl> {
    vm.check_memory(len.saturating_mul(4 * std::mem::size_of::<Value>()))?;
    let m = vm.heap.alloc(Obj::Map(Box::default()));
    for (k, v) in items {
        let k = k.into_value(vm)?;
        let v = v.into_value(vm)?;
        vm.map_insert(m, k, v)?;
    }
    Ok(m)
}
impl<K: for<'k> FromValue<Item<'k> = K> + Eq + Hash, V: FromValue> FromValue for HashMap<K, V> {
    type Item<'a> = HashMap<K, V::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
/// Sorts keys before insertion so the Sand map's iteration order is
/// deterministic.
impl<K: IntoValue + Ord, V: IntoValue> IntoValue for HashMap<K, V> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        let mut items: Vec<(K, V)> = self.into_iter().collect();
        vm.charge(sort_cost(items.len()))?;
        items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        write_map(vm, items.len(), items)
    }
}
impl<K: for<'k> FromValue<Item<'k> = K> + Ord, V: FromValue> FromValue for BTreeMap<K, V> {
    type Item<'a> = BTreeMap<K, V::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
impl<K: IntoValue, V: IntoValue> IntoValue for BTreeMap<K, V> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        write_map(vm, self.len(), self)
    }
}

impl<K: for<'k> FromValue<Item<'k> = K> + Eq + Hash, V: FromValue> FromValue for indexmap::IndexMap<K, V> {
    type Item<'a> = indexmap::IndexMap<K, V::Item<'a>>;
    fn from_value<'a>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Self::Item<'a>, Ctl> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
/// Keeps the insertion order of the entries.
impl<K: IntoValue, V: IntoValue> IntoValue for indexmap::IndexMap<K, V> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        write_map(vm, self.len(), self)
    }
}

fn read_set<T: for<'k> FromValue<Item<'k> = T>, C: FromIterator<T>>(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<C, Ctl> {
    let Obj::Set(s) = vm.heap.get(v) else { return Err(mismatch::<HashSet<T>>()) };
    conv.alloc(vm, s.live.saturating_mul(std::mem::size_of::<T>()))?;
    vm.charge(s.live as u64)?;
    s.entries.iter().flatten().map(|&x| T::from_value(vm, x, conv)).collect()
}
fn write_set<T: IntoValue>(vm: &mut Vm, len: usize, items: impl IntoIterator<Item = T>) -> Result<Value, Ctl> {
    vm.check_memory(len.saturating_mul(3 * std::mem::size_of::<Value>()))?;
    let s = vm.heap.alloc(Obj::Set(Box::default()));
    for x in items {
        let x = x.into_value(vm)?;
        vm.set_add(s, x)?;
    }
    Ok(s)
}
impl<T: for<'k> FromValue<Item<'k> = T> + Eq + Hash> FromValue for HashSet<T> {
    type Item<'a> = HashSet<T>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Self, Ctl> {
        read_set(vm, v, conv)
    }
}
/// Sorts elements before insertion so the Sand set's iteration order is
/// deterministic.
impl<T: IntoValue + Ord> IntoValue for HashSet<T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        let mut items: Vec<T> = self.into_iter().collect();
        vm.charge(sort_cost(items.len()))?;
        items.sort_unstable();
        write_set(vm, items.len(), items)
    }
}
impl<T: for<'k> FromValue<Item<'k> = T> + Ord> FromValue for BTreeSet<T> {
    type Item<'a> = BTreeSet<T>;
    fn from_value(vm: &Vm, v: Value, conv: &mut Conversion) -> Result<Self, Ctl> {
        read_set(vm, v, conv)
    }
}
impl<T: IntoValue> IntoValue for BTreeSet<T> {
    fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
        write_set(vm, self.len(), self)
    }
}

macro_rules! tuple_impls {
    ($len:literal: $($n:ident $i:tt),+) => {
        impl<$($n: SandType),+> SandType for ($($n,)+) {
            fn sand_type() -> crate::native::Type {
                crate::native::Type::Tuple(vec![$($n::sand_type()),+])
            }
        }
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
            fn into_value(self, vm: &mut Vm) -> Result<Value, Ctl> {
                let items = [$(self.$i.into_value(vm)?),+];
                vm.check_memory(items.len() * std::mem::size_of::<Value>())?;
                Ok(vm.heap.new_tuple(&items))
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
        impl $crate::native::SandType for $t {
            fn sand_type() -> $crate::native::Type {
                $crate::native::Type::bound::<Self>(::std::vec::Vec::new())
            }
        }
        impl $crate::native::FromValue for $t {
            type Item<'a> = Self;
            fn from_value(
                vm: &$crate::vm::Vm,
                v: $crate::Value,
                _: &mut $crate::native::Conversion,
            ) -> ::std::result::Result<Self, $crate::vm::Ctl> {
                $crate::native::value_from(vm, v)
            }
        }
        impl $crate::native::IntoValue for $t {
            fn into_value(self, vm: &mut $crate::vm::Vm) -> ::std::result::Result<$crate::Value, $crate::vm::Ctl> {
                $crate::native::value_into(self, vm)
            }
        }
    };
}

#[doc(hidden)]
pub fn value_from<T: ValueType>(vm: &Vm, v: Value) -> Result<T, Ctl> {
    vm.heap.value::<T>(v).ok_or_else(mismatch::<T>)
}

#[doc(hidden)]
pub fn value_into<T: ValueType>(x: T, vm: &mut Vm) -> Result<Value, Ctl> {
    if T::PACKING.is_none() {
        vm.check_memory(ValueType::size(&x))?;
    }
    Ok(vm.new_value(x))
}

macro_rules! handle {
    ($([$($g:tt)*] $t:ty),* $(,)?) => {$(
        impl<$($g)*> FromValue for $t {
            type Item<'a> = Self;
            fn from_value(_: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Ctl> {
                Ok(Self::from_raw(v))
            }
        }
        impl<$($g)*> IntoValue for $t {
            fn into_value(self, _: &mut Vm) -> Result<Value, Ctl> {
                Ok(self.raw())
            }
        }
    )*};
}
handle! {
    [T: SandType + ?Sized] Val<T>,
    [const N: u8, B: ParamBounds] Param<N, B>,
}

impl<const N: u8> FromValue for Num<N> {
    type Item<'a> = Self;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Ctl> {
        match vm.heap.get(v) {
            Obj::Tuple(_) => Ok(Num::from_raw(v)),
            _ => Err(mismatch::<Self>()),
        }
    }
}

impl<const N: u8> FromValue for Reflect<N> {
    type Item<'a> = Self;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Ctl> {
        let r = Reflect::from_raw(v);
        r.desc(&vm.heap)?;
        Ok(r)
    }
}
