//! Conversion between VM values and the arguments and results of native functions.

use super::ty::{Never, Num, Param, SandType, VmVal};
use crate::types::traits;
use crate::value::{DecodeError, NativeValue, Value, qualified_name, qualified_name_module};
use crate::vm::meter::Scratch;
use crate::vm::value::{Obj, ValueType, VmValue};
use crate::vm::{Ctl, Fail, Vm};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;

/// Converts a [`VmValue`] argument into a parameter value for a
/// [`NativeFunction`](super::func::NativeFunction).
///
/// In a synchronous function without `&mut Vm`, [`FromVmValue::Item`] can
/// borrow `&str`, `&[u8]`, or `&T` from the VM heap. Owned types allocate a
/// Rust copy.
///
/// A Sand value can contain shared references to one heap object. Converting a
/// value into an owned Rust type copies each reachable reference.
/// Implementations must charge fuel to `vm` and record allocated bytes in
/// [`Conversion`] so that instruction and memory limits bound shared-value
/// expansion.
pub trait FromVmValue: SandType {
    type Item<'a>;

    /// Converts `v` into [`Self::Item`]. Charges fuel to `vm` and records
    /// temporary Rust allocations in `conv`.
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail>;
}

/// Tracks temporary Rust memory allocated while converting arguments for a
/// native call and enforces the VM memory limit.
pub struct Conversion {
    bytes: usize,
    scratch: Scratch,
    depth: usize,
}

const MAX_STRUCT_DEPTH: usize = 256;

pub(crate) fn enter_struct(depth: usize) -> Result<usize, Fail> {
    if depth < MAX_STRUCT_DEPTH { Ok(depth + 1) } else { Err(Fail::runtime_error("limit", "value too deeply nested to convert")) }
}

impl Conversion {
    pub(crate) fn new() -> Conversion {
        Conversion { bytes: 0, scratch: Scratch::new(), depth: 0 }
    }

    pub(crate) fn enter_struct<R>(&mut self, f: impl FnOnce(&mut Conversion) -> Result<R, Fail>) -> Result<R, Fail> {
        self.depth = enter_struct(self.depth)?;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Records `bytes` of temporary Rust allocation and checks the VM memory limit.
    pub fn alloc(&mut self, vm: &Vm, bytes: usize) -> Result<(), Fail> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.scratch.check(&vm.heap, self.bytes)
    }
}

/// Converts a Rust return value into a [`VmValue`].
///
/// Implementations charge fuel and check the VM memory limit before allocating
/// on the heap.
pub trait IntoVmValue: SandType {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail>;
}

/// Error returned by a native function whose Sand return type is `Result<T>`.
///
/// Construct a recoverable script error with [`HostError::new`], or propagate a
/// [`Fail`], a [`Ctl`], or a `std::io::Error` with `?`.
///
/// `C` is the control signal that aborts or suspends the call. A
/// `HostError<Fail>` cannot hold a scheduler request (`Ctl`):
///
/// ```compile_fail,E0277
/// # use sandlang::native::convert::HostError;
/// # use sandlang::vm::{Ctl, Fail};
/// fn finish(ctl: Ctl) -> Result<(), HostError<Fail>> {
///     Err(ctl)?
/// }
/// ```
///
/// Make a helper generic over `C` to share it between a native function and a
/// finisher:
///
/// ```
/// # use sandlang::native::convert::HostError;
/// # use sandlang::vm::{Fail, Vm};
/// fn parse<C: From<Fail>>(vm: &Vm, text: &str) -> Result<i64, HostError<C>> {
///     vm.charge_bytes(text.len())?;
///     text.parse().map_err(|_| HostError::new("parse", "not a number"))
/// }
/// ```
#[must_use]
pub struct HostError<C = Ctl>(Box<HostErrorKind<C>>);

enum HostErrorKind<C> {
    Error {
        kind: String,
        message: String,
    },
    /// An error that the engine already made as the Sand `Err` value `err`.
    /// It holds the `kind` and `message` of `err` in case `err` is no longer
    /// live.
    Made {
        err: VmValue,
        kind: String,
        message: String,
    },
    Abort(C),
}

impl<C> HostError<C> {
    /// Creates a recoverable Sand `Error` with the given `kind` (such as
    /// `"not_found"`, `"invalid"`, or `"permission_denied"`; see SPEC §5.4) and
    /// `message`.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> HostError<C> {
        HostError(Box::new(HostErrorKind::Error { kind: kind.into(), message: message.into() }))
    }

    /// Returns the error of the Sand `Err` value `err`, which holds an
    /// `Error` with `kind` and `message`. The native function returns `err`
    /// itself, so that the engine knows the error as one it made.
    pub(crate) fn made(err: VmValue, kind: String, message: String) -> HostError<C> {
        HostError(Box::new(HostErrorKind::Made { err, kind, message }))
    }
}

impl From<Ctl> for HostError<Ctl> {
    fn from(c: Ctl) -> HostError<Ctl> {
        HostError(Box::new(HostErrorKind::Abort(c)))
    }
}

impl<C: From<Fail>> From<Fail> for HostError<C> {
    fn from(fail: Fail) -> HostError<C> {
        HostError(Box::new(HostErrorKind::Abort(fail.into())))
    }
}

impl<C: From<Fail>> HostError<C> {
    /// Converts a recoverable error into a Sand `Err` value, or returns the
    /// control signal that stops the call.
    pub(crate) fn into_sand_error(self, vm: &mut Vm) -> Result<VmValue, C> {
        match *self.0 {
            HostErrorKind::Made { err, kind, message } if holds_error(vm, err, &kind, &message) => Ok(err),
            HostErrorKind::Error { kind, message } | HostErrorKind::Made { kind, message, .. } => {
                let e = crate::stdlib::core::Error { kind, message }.into_vm_value(vm)?;
                Ok(vm.err(e))
            }
            HostErrorKind::Abort(c) => Err(c),
        }
    }
}

/// Returns `true` if `err` is a Sand `Err` value whose `Error` has `kind` and
/// `message`.
fn holds_error(vm: &Vm, err: VmValue, kind: &str, message: &str) -> bool {
    let has_slot = |v: VmValue| !matches!(v, VmValue::Obj(_)) || vm.heap.has_slot(v);
    if !has_slot(err) || vm.heap.variant_id(err) != vm.prog.ids.err {
        return false;
    }
    let Some(e) = vm.heap.field(err, 0).filter(|&e| has_slot(e)) else { return false };
    let is_str = |v: VmValue, text: &str| matches!(vm.heap.get(v), Obj::Str(s) if s.as_str() == text);
    match vm.heap.get(e) {
        Obj::Struct(_, fields) => matches!(**fields, [m, k] if has_slot(m) && has_slot(k) && is_str(m, message) && is_str(k, kind)),
        _ => false,
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
            HostErrorKind::Error { kind, message } | HostErrorKind::Made { kind, message, .. } => write!(f, "HostError({kind}: {message})"),
            HostErrorKind::Abort(c) => write!(f, "HostError({c:?})"),
        }
    }
}

/// Converts a Rust error into a [`HostError<Fail>`] for native functions returning
/// `Result<T, E>`.
pub trait IntoHostError {
    fn into_host_error(self) -> HostError<Fail>;
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
    ($($t:ty: $p:pat => $e:expr, $v:ident => $into:expr),* $(,)?) => {$(
        impl FromVmValue for $t {
            type Item<'a> = $t;
            fn from_vm_value(_: &Vm, v: VmValue, _: &mut Conversion) -> Result<$t, Fail> {
                match v {
                    $p => Ok($e),
                    _ => Err(mismatch::<$t>()),
                }
            }
        }
        impl IntoVmValue for $t {
            fn into_vm_value(self, _: &mut Vm) -> Result<VmValue, Fail> {
                let $v = self;
                Ok($into)
            }
        }
    )*};
}

scalar! {
    i64: VmValue::Int(i) => i, x => VmValue::Int(x),
    f64: VmValue::Float(f) => f, x => VmValue::Float(x),
    bool: VmValue::Bool(b) => b, x => VmValue::Bool(x),
}

impl FromVmValue for () {
    type Item<'a> = ();
    fn from_vm_value(_: &Vm, _: VmValue, _: &mut Conversion) -> Result<(), Fail> {
        Ok(())
    }
}
impl IntoVmValue for () {
    fn into_vm_value(self, _: &mut Vm) -> Result<VmValue, Fail> {
        Ok(VmValue::Unit)
    }
}

impl FromVmValue for jiff::SignedDuration {
    type Item<'a> = jiff::SignedDuration;
    fn from_vm_value(_: &Vm, v: VmValue, _: &mut Conversion) -> Result<Self, Fail> {
        match v {
            VmValue::Dur(n) => Ok(jiff::SignedDuration::from_nanos(n)),
            _ => Err(mismatch::<Self>()),
        }
    }
}

impl IntoVmValue for jiff::SignedDuration {
    fn into_vm_value(self, _: &mut Vm) -> Result<VmValue, Fail> {
        match i64::try_from(self.as_nanos()) {
            Ok(n) => Ok(VmValue::Dur(n)),
            Err(_) => Err(Fail::runtime_error("overflow", "the duration is out of range")),
        }
    }
}

impl IntoVmValue for Never {
    fn into_vm_value(self, _: &mut Vm) -> Result<VmValue, Fail> {
        match self {}
    }
}

impl FromVmValue for &str {
    type Item<'a> = &'a str;
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, _: &mut Conversion) -> Result<&'a str, Fail> {
        match vm.heap.get(v) {
            Obj::Str(s) => Ok(s),
            Obj::StrBuf(s) => Ok(s),
            _ => Err(mismatch::<String>()),
        }
    }
}
impl FromVmValue for String {
    type Item<'a> = String;
    fn from_vm_value(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<String, Fail> {
        let s = <&str>::from_vm_value(vm, v, conv)?;
        conv.alloc(vm, s.len())?;
        vm.charge_bytes(s.len())?;
        Ok(s.to_string())
    }
}
impl IntoVmValue for String {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_str(self))
    }
}
impl IntoVmValue for &str {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_str(self))
    }
}

impl FromVmValue for &[u8] {
    type Item<'a> = &'a [u8];
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, _: &mut Conversion) -> Result<&'a [u8], Fail> {
        match vm.heap.get(v) {
            Obj::Bytes(b) => Ok(b),
            _ => Err(mismatch::<Vec<u8>>()),
        }
    }
}
impl FromVmValue for Vec<u8> {
    type Item<'a> = Vec<u8>;
    fn from_vm_value(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<Vec<u8>, Fail> {
        let b = <&[u8]>::from_vm_value(vm, v, conv)?;
        conv.alloc(vm, b.len())?;
        vm.charge_bytes(b.len())?;
        Ok(b.to_vec())
    }
}
impl IntoVmValue for Vec<u8> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_bytes(self))
    }
}
impl IntoVmValue for &[u8] {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        vm.check_memory(self.len())?;
        vm.charge_bytes(self.len())?;
        Ok(vm.heap.new_bytes(self))
    }
}

impl<T: FromVmValue> FromVmValue for Vec<T> {
    type Item<'a> = Vec<T::Item<'a>>;
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        let Obj::List(items) = vm.heap.get(v) else { return Err(mismatch::<Self>()) };
        conv.alloc(vm, items.len().saturating_mul(std::mem::size_of::<T::Item<'a>>()))?;
        vm.charge(items.len() as u64)?;
        items.iter().map(|&x| T::from_vm_value(vm, x, conv)).collect()
    }
}
impl<T: IntoVmValue> IntoVmValue for Vec<T> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        vm.check_memory(self.len().saturating_mul(std::mem::size_of::<VmValue>()))?;
        vm.charge(self.len() as u64)?;
        let mut items = Vec::with_capacity(self.len());
        for x in self {
            items.push(x.into_vm_value(vm)?);
        }
        Ok(vm.heap.new_list(items))
    }
}

impl<T: for<'a> FromVmValue<Item<'a> = T>> FromVmValue for Box<T> {
    type Item<'a> = Box<T>;
    fn from_vm_value(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<Box<T>, Fail> {
        conv.alloc(vm, std::mem::size_of::<T>())?;
        T::from_vm_value(vm, v, conv).map(Box::new)
    }
}
impl<T: IntoVmValue> IntoVmValue for Box<T> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        (*self).into_vm_value(vm)
    }
}

impl<T: FromVmValue> FromVmValue for Option<T> {
    type Item<'a> = Option<T::Item<'a>>;
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        vm.as_option(v).map(|x| T::from_vm_value(vm, x, conv)).transpose()
    }
}
impl<T: IntoVmValue> IntoVmValue for Option<T> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        Ok(match self {
            Some(x) => {
                let v = x.into_vm_value(vm)?;
                vm.some(v)
            }
            None => vm.none(),
        })
    }
}

impl<T: IntoVmValue, E: IntoHostError> IntoVmValue for Result<T, E> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        match self {
            Ok(x) => ok_vm_value(x, vm),
            Err(e) => e.into_host_error().into_sand_error(vm),
        }
    }
}

impl<T: IntoVmValue> IntoVmValue for Result<T, Fail> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        self?.into_vm_value(vm)
    }
}

/// Converts `x` into a Sand `Ok` value.
pub(crate) fn ok_vm_value<T: IntoVmValue>(x: T, vm: &mut Vm) -> Result<VmValue, Fail> {
    let v = x.into_vm_value(vm)?;
    Ok(vm.ok(v))
}

fn sort_cost(n: usize) -> u64 {
    (n as u64).saturating_mul(u64::from(usize::BITS - n.leading_zeros()))
}

fn read_map<'a, K, V, C>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<C, Fail>
where
    K: for<'k> FromVmValue<Item<'k> = K>,
    V: FromVmValue,
    C: FromIterator<(K, V::Item<'a>)>,
{
    let Obj::Map(m) = vm.heap.get(v) else { return Err(mismatch::<HashMap<K, V>>()) };
    conv.alloc(vm, m.live.saturating_mul(std::mem::size_of::<(K, V::Item<'a>)>()))?;
    vm.charge(m.live as u64)?;
    m.entries.iter().flatten().map(|&(k, x)| Ok((K::from_vm_value(vm, k, conv)?, V::from_vm_value(vm, x, conv)?))).collect()
}
fn write_map<K: IntoVmValue, V: IntoVmValue>(vm: &mut Vm, len: usize, items: impl IntoIterator<Item = (K, V)>) -> Result<VmValue, Fail> {
    vm.check_memory(len.saturating_mul(4 * std::mem::size_of::<VmValue>()))?;
    let m = vm.heap.alloc(Obj::Map(Box::default()));
    for (k, v) in items {
        let k = k.into_vm_value(vm)?;
        let v = v.into_vm_value(vm)?;
        vm.map_insert(m, k, v)?;
    }
    Ok(m)
}
impl<K: for<'k> FromVmValue<Item<'k> = K> + Eq + Hash, V: FromVmValue> FromVmValue for HashMap<K, V> {
    type Item<'a> = HashMap<K, V::Item<'a>>;
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
/// Sorts keys before insertion so the Sand map's iteration order is
/// deterministic.
impl<K: IntoVmValue + Ord, V: IntoVmValue> IntoVmValue for HashMap<K, V> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        let mut items: Vec<(K, V)> = self.into_iter().collect();
        vm.charge(sort_cost(items.len()))?;
        items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        write_map(vm, items.len(), items)
    }
}
impl<K: for<'k> FromVmValue<Item<'k> = K> + Ord, V: FromVmValue> FromVmValue for BTreeMap<K, V> {
    type Item<'a> = BTreeMap<K, V::Item<'a>>;
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
impl<K: IntoVmValue, V: IntoVmValue> IntoVmValue for BTreeMap<K, V> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        write_map(vm, self.len(), self)
    }
}

impl<K: for<'k> FromVmValue<Item<'k> = K> + Eq + Hash, V: FromVmValue> FromVmValue for indexmap::IndexMap<K, V> {
    type Item<'a> = indexmap::IndexMap<K, V::Item<'a>>;
    fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
        read_map::<K, V, _>(vm, v, conv)
    }
}
impl<K: IntoVmValue, V: IntoVmValue> IntoVmValue for indexmap::IndexMap<K, V> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        write_map(vm, self.len(), self)
    }
}

fn read_set<T: for<'k> FromVmValue<Item<'k> = T>, C: FromIterator<T>>(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<C, Fail> {
    let Obj::Set(s) = vm.heap.get(v) else { return Err(mismatch::<HashSet<T>>()) };
    conv.alloc(vm, s.live.saturating_mul(std::mem::size_of::<T>()))?;
    vm.charge(s.live as u64)?;
    s.entries.iter().flatten().map(|&x| T::from_vm_value(vm, x, conv)).collect()
}
fn write_set<T: IntoVmValue>(vm: &mut Vm, len: usize, items: impl IntoIterator<Item = T>) -> Result<VmValue, Fail> {
    vm.check_memory(len.saturating_mul(3 * std::mem::size_of::<VmValue>()))?;
    let s = vm.heap.alloc(Obj::Set(Box::default()));
    for x in items {
        let x = x.into_vm_value(vm)?;
        vm.set_add(s, x)?;
    }
    Ok(s)
}
impl<T: for<'k> FromVmValue<Item<'k> = T> + Eq + Hash> FromVmValue for HashSet<T> {
    type Item<'a> = HashSet<T>;
    fn from_vm_value(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<Self, Fail> {
        read_set(vm, v, conv)
    }
}
/// Sorts elements before insertion so the Sand set's iteration order is
/// deterministic.
impl<T: IntoVmValue + Ord> IntoVmValue for HashSet<T> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        let mut items: Vec<T> = self.into_iter().collect();
        vm.charge(sort_cost(items.len()))?;
        items.sort_unstable();
        write_set(vm, items.len(), items)
    }
}
impl<T: for<'k> FromVmValue<Item<'k> = T> + Ord> FromVmValue for BTreeSet<T> {
    type Item<'a> = BTreeSet<T>;
    fn from_vm_value(vm: &Vm, v: VmValue, conv: &mut Conversion) -> Result<Self, Fail> {
        read_set(vm, v, conv)
    }
}
impl<T: IntoVmValue> IntoVmValue for BTreeSet<T> {
    fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
        write_set(vm, self.len(), self)
    }
}

macro_rules! tuple_impls {
    ($len:literal: $($n:ident $i:tt),+) => {
        impl<$($n: SandType),+> SandType for ($($n,)+) {
            fn sand_type() -> crate::native::ty::Type {
                crate::native::ty::Type::Tuple(vec![$($n::sand_type()),+])
            }
        }
        impl<$($n: FromVmValue),+> FromVmValue for ($($n,)+) {
            type Item<'a> = ($($n::Item<'a>,)+);
            fn from_vm_value<'a>(vm: &'a Vm, v: VmValue, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail> {
                let pair;
                let f: &[VmValue] = match v {
                    VmValue::Pair(a, b) => {
                        pair = [VmValue::Int(a as i64), VmValue::Int(b)];
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
                Ok(($($n::from_vm_value(vm, f[$i], conv)?,)+))
            }
        }
        impl<$($n: IntoVmValue),+> IntoVmValue for ($($n,)+) {
            fn into_vm_value(self, vm: &mut Vm) -> Result<VmValue, Fail> {
                let items = [$(self.$i.into_vm_value(vm)?),+];
                vm.check_memory(items.len() * std::mem::size_of::<VmValue>())?;
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
        impl $crate::native::ty::SandType for $t {
            fn sand_type() -> $crate::native::ty::Type {
                $crate::native::ty::Type::bound::<Self>(::std::vec::Vec::new())
            }
        }
        impl $crate::native::convert::FromVmValue for $t {
            type Item<'a> = Self;
            fn from_vm_value(
                vm: &$crate::vm::Vm,
                v: $crate::vm::value::VmValue,
                _: &mut $crate::native::convert::Conversion,
            ) -> ::std::result::Result<Self, $crate::vm::Fail> {
                $crate::native::convert::value_type_from_vm_value(vm, v)
            }
        }
        impl $crate::native::convert::IntoVmValue for $t {
            fn into_vm_value(self, vm: &mut $crate::vm::Vm) -> ::std::result::Result<$crate::vm::value::VmValue, $crate::vm::Fail> {
                $crate::native::convert::value_type_into_vm_value(self, vm)
            }
        }
        impl $crate::value::Encode for $t {
            fn encode(&self) -> $crate::value::Value {
                static SAND_TYPE_NAME: $crate::native::convert::TypeNameCell = $crate::native::convert::TypeNameCell::new();
                $crate::native::convert::encode_value_type(&SAND_TYPE_NAME, self)
            }
        }
        impl $crate::value::Decode for $t {
            fn decode(value: &$crate::value::Value) -> ::std::result::Result<Self, $crate::value::DecodeError> {
                $crate::native::convert::decode_value_type(value)
            }
        }
    };
}

/// Cached qualified type name shared across values from a derived `Encode`
/// implementation.
#[doc(hidden)]
pub type TypeNameCell = std::sync::OnceLock<std::sync::Arc<str>>;

#[doc(hidden)]
pub fn encode_value_type<T: ValueType>(type_name: &TypeNameCell, x: &T) -> Value {
    let type_name = type_name.get_or_init(|| qualified_name(qualified_name_module(T::MODULE), T::NAME).into()).clone();
    Value::Native(Box::new(NativeValue { type_name, text: x.to_string() }))
}

#[doc(hidden)]
pub fn decode_value_type<T: ValueType>(value: &Value) -> Result<T, DecodeError> {
    let name = qualified_name(qualified_name_module(T::MODULE), T::NAME);
    let text = crate::value::decode_scalar(value, false, |n, in_key| n.native_text(&name, in_key))?;
    T::parse(text).map_err(DecodeError::new)
}

#[doc(hidden)]
pub fn value_type_from_vm_value<T: ValueType>(vm: &Vm, v: VmValue) -> Result<T, Fail> {
    vm.heap.native_value::<T>(v).ok_or_else(mismatch::<T>)
}

#[doc(hidden)]
pub fn value_type_into_vm_value<T: ValueType>(x: T, vm: &mut Vm) -> Result<VmValue, Fail> {
    if T::PACKING.is_none() {
        vm.check_memory(ValueType::size(&x))?;
    }
    Ok(vm.new_native_value(x))
}

macro_rules! handle {
    ($([$($g:tt)*] $t:ty),* $(,)?) => {$(
        impl<$($g)*> FromVmValue for $t {
            type Item<'a> = Self;
            fn from_vm_value(_: &Vm, v: VmValue, _: &mut Conversion) -> Result<Self, Fail> {
                Ok(Self::from_raw(v))
            }
        }
        impl<$($g)*> IntoVmValue for $t {
            fn into_vm_value(self, _: &mut Vm) -> Result<VmValue, Fail> {
                Ok(self.raw())
            }
        }
    )*};
}
handle! {
    [T: SandType + ?Sized] VmVal<T>,
    [const N: u8, B: traits::Set] Param<N, B>,
}

impl<const N: u8> FromVmValue for super::ty::Encoded<N> {
    type Item<'a> = Self;
    fn from_vm_value(vm: &Vm, v: VmValue, _: &mut Conversion) -> Result<Self, Fail> {
        match vm.heap.native::<crate::codec::Encoder>(v).and_then(crate::codec::Encoder::encoded_value) {
            Some(value) => Ok(Self::new(value.clone())),
            None => Err(mismatch::<Self>()),
        }
    }
}

impl<const N: u8> FromVmValue for Num<N> {
    type Item<'a> = Self;
    fn from_vm_value(vm: &Vm, v: VmValue, _: &mut Conversion) -> Result<Self, Fail> {
        match vm.heap.get(v) {
            Obj::Tuple(_) => Ok(Num::from_raw(v)),
            _ => Err(mismatch::<Self>()),
        }
    }
}
