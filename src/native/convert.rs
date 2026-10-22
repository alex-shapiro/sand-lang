//! Conversion of values between Sand and Rust.

use super::func::{Later, NativeError};
use super::ty::{Never, Param, Reflect, SandType, Val};
use crate::vm::meter::Scratch;
use crate::vm::value::{Obj, Value, ValueType};
use crate::vm::{Ctl, Vm};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;

/// Conversion of an argument from a Sand value.
///
/// The checker guarantees that each argument has the Sand type of its Rust
/// type, so a conversion fails only if the engine has a bug (with an internal
/// error). The converted [`FromValue::Item`] may borrow from the heap for the
/// duration of a call that has no access to the VM (`&str`, `&[u8]`). Owned
/// types (`String`, `Vec<T>`, ...) copy the value.
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
    pub(crate) fn new() -> Conversion {
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

/// The error of a native function whose Sand result is `Result<T>`: an
/// `Error { kind, message }` that the script receives, or an abort (`Ctl`,
/// converted with `?`) that ends the call. Any other error type `E: Display`
/// in a `Result<T, E>` becomes an error of kind `"error"`.
pub struct HostError(Box<Raised>);

enum Raised {
    Error { kind: String, message: String },
    Abort(Ctl),
}

impl HostError {
    /// An error of kind `kind`; the standard kinds are listed in SPEC §13
    /// (`not_found`, `invalid`, `permission_denied`, ...).
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> HostError {
        HostError(Box::new(Raised::Error { kind: kind.into(), message: message.into() }))
    }
}

impl From<Ctl> for HostError {
    fn from(c: Ctl) -> HostError {
        HostError(Box::new(Raised::Abort(c)))
    }
}

impl std::fmt::Debug for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            Raised::Error { kind, message } => write!(f, "HostError({kind}: {message})"),
            Raised::Abort(c) => write!(f, "HostError({c:?})"),
        }
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

/// The error for a value that does not have the type its declaration gives.
#[cold]
pub(crate) fn mismatch<S: SandType + ?Sized>() -> Ctl {
    Ctl::internal(format!("a native function expected a value of type `{}`", S::sand_type()))
}

/// Scalars convert without allocating.
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

/// Fails with kind `"overflow"` for a duration outside the range of Sand
/// durations (about ±292 years).
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
        vm.reserve(self.len().saturating_mul(std::mem::size_of::<Value>()))?;
        vm.charge(self.len() as u64)?;
        let mut items = Vec::with_capacity(self.len());
        for x in self {
            items.push(x.into_value(vm)?);
        }
        Ok(vm.heap.new_list(items))
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
                Raised::Error { kind, message } => {
                    vm.reserve(kind.len() + message.len())?;
                    Ok(vm.err_value(&kind, &message))
                }
                Raised::Abort(c) => Err(c),
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

/// The entries of a `[str: T]` map.
fn read_map<'a, T: FromValue>(vm: &'a Vm, v: Value, conv: &mut Conversion) -> Result<Vec<(String, T::Item<'a>)>, Ctl> {
    let Obj::Map(m) = vm.heap.get(v) else { return Err(mismatch::<HashMap<String, T>>()) };
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
    ($len:literal: $($n:ident $i:tt),+) => {
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
                vm.reserve(items.len() * std::mem::size_of::<Value>())?;
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

impl<T: ValueType> FromValue for T {
    type Item<'a> = T;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<T, Ctl> {
        vm.heap.value::<T>(v).ok_or_else(mismatch::<T>)
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

/// Handles pass through unconverted.
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
    [const N: u8] Param<N>,
}

impl<const N: u8> FromValue for Reflect<N> {
    type Item<'a> = Self;
    fn from_value(vm: &Vm, v: Value, _: &mut Conversion) -> Result<Self, Ctl> {
        let r = Reflect::from_raw(v);
        r.desc(&vm.heap)?;
        Ok(r)
    }
}
