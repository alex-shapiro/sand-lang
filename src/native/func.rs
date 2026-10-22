//! Rust functions usable as native functions, with their Sand signatures.

use super::convert::{Conversion, FromValue, HostError, IntoValue};
use super::ty::Type;
use crate::vm::value::Value;
use crate::vm::{BoxFuture, Ctl, Finisher, NResult, NativeFn, Vm};
use std::future::Future;
use std::marker::PhantomData;

/// The Sand signature of a native function, derived from its Rust types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl Signature {
    /// Whether the native `name` of `module` is a method that takes `self`:
    /// `name` is `Type.method`, and its first parameter has type `Type`.
    pub(crate) fn takes_self(&self, module: &str, name: &str) -> bool {
        let Some((ty, _)) = name.split_once('.') else { return false };
        matches!(self.params.first(), Some(Type::Named(n)) if n.module == module && n.name == ty && n.args.is_empty())
    }
}

/// A synchronous Rust function without access to the VM: `Fn(A, B, ...) -> R`
/// where the arguments implement [`FromValue`] and the result [`IntoValue`].
/// Arguments of type `&str` and `&[u8]` borrow the script's values. A result
/// of type `Result<T, Ctl>` gives the Sand type `T`; `Err` fails the call.
pub trait HostFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// A synchronous Rust function with access to the VM:
/// `Fn(&mut Vm, A, B, ...) -> R`. Its arguments are owned:
/// heap values that it does not copy are [`Val`](super::Val) handles.
pub trait VmFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// A Rust function returning a future, usable as an async native function.
/// Its arguments must be owned (the future outlives the call).
pub trait AsyncHostFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// The result of a native function that is ready when a future completes:
/// the calling task waits, then the VM finishes the call with the
/// `Result<T, E>` that the future's closure makes. A native returns it as
/// `Result<Later<T, E>, E>`: with `E = Ctl` its Sand result is `T`, with
/// `E = HostError` it is `Result<T>`.
pub struct Later<T, E>(Wait, PhantomData<fn() -> (T, E)>);

enum Wait {
    Ready(Finisher),
    Future(BoxFuture),
}

/// The error type of a native function's result: [`Ctl`] (an abort; the
/// Sand result is `T`) or [`HostError`] (the Sand result is `Result<T>`).
pub trait NativeError: Send + Sized + 'static + sealed::Sealed {
    /// The Sand result type for a success of type `t`.
    fn result_type(t: Type) -> Type;
    /// Converts `r` into the Sand result.
    fn finish<T: IntoValue>(r: Result<T, Self>, vm: &mut Vm) -> NResult;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::vm::Ctl {}
    impl Sealed for super::HostError {}
}

impl NativeError for Ctl {
    fn result_type(t: Type) -> Type {
        t
    }
    fn finish<T: IntoValue>(r: Result<T, Ctl>, vm: &mut Vm) -> NResult {
        r.into_value(vm)
    }
}

impl NativeError for HostError {
    fn result_type(t: Type) -> Type {
        Type::result(t, Type::error())
    }
    fn finish<T: IntoValue>(r: Result<T, HostError>, vm: &mut Vm) -> NResult {
        r.into_value(vm)
    }
}

impl<T: IntoValue + Send + 'static, E: NativeError> Later<T, E> {
    /// The result that `fut`'s closure makes.
    pub fn new<F>(fut: impl Future<Output = F> + Send + 'static) -> Later<T, E>
    where
        F: FnOnce(&mut Vm) -> Result<T, E> + Send + 'static,
    {
        let fut = async move {
            let finish = fut.await;
            Box::new(move |vm: &mut Vm| E::finish(finish(vm), vm)) as Finisher
        };
        Later(Wait::Future(Box::pin(fut)), PhantomData)
    }

    /// The result `r`, without waiting.
    pub fn ready(r: Result<T, E>) -> Later<T, E> {
        Later(Wait::Ready(Box::new(move |vm: &mut Vm| E::finish(r, vm))), PhantomData)
    }
}

impl<T, E> Later<T, E> {
    /// Finishes the call: now, or by suspending the calling task.
    pub(crate) fn finish(self, vm: &mut Vm) -> NResult {
        match self.0 {
            Wait::Ready(f) => f(vm),
            Wait::Future(fut) => Err(Ctl::suspend(fut)),
        }
    }
}

/// The arguments of a native that takes `n` of them. A call of a trait
/// method passes the impl's dictionaries first, which a native does not take.
#[inline(always)]
fn own_args(args: &[Value], n: usize) -> &[Value] {
    args.get(args.len().wrapping_sub(n)..).unwrap_or_default()
}

/// The error for too few arguments, possible only if the engine has a bug.
#[cold]
fn missing() -> Ctl {
    Ctl::internal("native function called with too few arguments")
}

macro_rules! fn_impls {
    ($($a:ident),*) => {
        fn_impls!(@impl [$($a),*] <[&str]>::len(&[$(stringify!($a)),*]));
    };
    (@impl [$($a:ident),*] $n:expr) => {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> HostFunction<($($a,)*)> for F
        where
            F: Fn($($a),*) -> R + for<'a> Fn($($a::Item<'a>),*) -> R + Send + Sync + 'static,
            R: IntoValue,
            $($a: FromValue + 'static,)*
        {
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self) -> Box<NativeFn> {
                // Calls through the higher-ranked signature (the arguments
                // borrow from the heap).
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Box::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let r = {
                        let &[$($a),*] = own_args(args, $n) else { return Err(missing()) };
                        let mut conv = Conversion::new();
                        $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                        call(&self, $($a),*)
                    };
                    r.into_value(vm)
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> VmFunction<(Vm, $($a,)*)> for F
        where
            F: Fn(&mut Vm, $($a),*) -> R + Send + Sync + 'static,
            R: IntoValue,
            $($a: for<'a> FromValue<Item<'a> = $a> + 'static,)*
        {
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let &[$($a),*] = own_args(args, $n) else { return Err(missing()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    self(vm, $($a),*).into_value(vm)
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
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let &[$($a),*] = own_args(args, $n) else { return Err(missing()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    let fut = self($($a),*);
                    Err(Ctl::suspend(Box::pin(async move {
                        let r = fut.await;
                        Box::new(move |vm: &mut Vm| r.into_value(vm)) as Finisher
                    })))
                })
            }
        }
    };
}
fn_impls!();
fn_impls!(A);
fn_impls!(A, B);
fn_impls!(A, B, C);
fn_impls!(A, B, C, D);
fn_impls!(A, B, C, D, E);
fn_impls!(A, B, C, D, E, G1);
fn_impls!(A, B, C, D, E, G1, H);
fn_impls!(A, B, C, D, E, G1, H, I);
fn_impls!(A, B, C, D, E, G1, H, I, J);
fn_impls!(A, B, C, D, E, G1, H, I, J, K);
fn_impls!(A, B, C, D, E, G1, H, I, J, K, L);
fn_impls!(A, B, C, D, E, G1, H, I, J, K, L, M);
