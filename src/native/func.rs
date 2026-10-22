//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::convert::{Conversion, FromValue, HostError, IntoValue};
use super::ty::Type;
use crate::vm::value::Value;
use crate::vm::{BoxFuture, Ctl, Finisher, NResult, NativeFn, Vm};
use std::future::Future;
use std::marker::PhantomData;

/// Sand parameter and return types derived from a Rust function signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl Signature {
    /// Returns `true` if `name` is `"Type.method"` and the first parameter has
    /// type `module.Type`.
    pub(crate) fn takes_self(&self, module: &str, name: &str) -> bool {
        let Some((ty, _)) = name.split_once('.') else { return false };
        matches!(self.params.first(), Some(Type::Named(n)) if n.module == module && n.name == ty && n.args.is_empty())
    }
}

/// Synchronous function registered with [`NativeModule::function`](super::NativeModule::function).
///
/// Parameters implement [`FromValue`] (borrowing `&str`, `&[u8]`, and `&T`
/// directly from the VM heap), and the return type implements [`IntoValue`].
pub trait HostFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// Synchronous function registered with [`NativeModule::vm_function`](super::NativeModule::vm_function).
///
/// Receives `&mut Vm` as its first argument; remaining arguments must be owned
/// [`FromValue`] types or [`Val`](super::Val) handles.
pub trait VmFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// Asynchronous function registered with [`NativeModule::async_function`](super::NativeModule::async_function).
///
/// Arguments must be owned [`FromValue`] types so the returned future can
/// outlive the initial call.
pub trait AsyncHostFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// Deferred result returned by a [`VmFunction`] that may suspend the calling
/// Sand task on a future.
///
/// Return `Result<Later<T, E>, E>` from the function: if `E` is [`Ctl`], the
/// Sand return type is `T`; if `E` is [`HostError`], the Sand return type is
/// `Result<T, Error>`.
pub struct Later<T, E>(Wait, PhantomData<fn() -> (T, E)>);

enum Wait {
    Ready(Finisher),
    Future(BoxFuture),
}

/// Error type for a [`Later`] result ([`Ctl`] or [`HostError`]).
pub trait NativeError: Send + Sized + 'static + sealed::Sealed {
    fn result_type(t: Type) -> Type;
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
    /// Suspends the calling task until `fut` resolves, and then runs the
    /// returned closure on the VM thread to produce the Sand result.
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

    /// Completes immediately with `r` without suspending the calling task.
    pub fn ready(r: Result<T, E>) -> Later<T, E> {
        Later(Wait::Ready(Box::new(move |vm: &mut Vm| E::finish(r, vm))), PhantomData)
    }
}

impl<T, E> Later<T, E> {
    pub(crate) fn finish(self, vm: &mut Vm) -> NResult {
        match self.0 {
            Wait::Ready(f) => f(vm),
            Wait::Future(fut) => Err(Ctl::suspend(fut)),
        }
    }
}

/// Drops any leading trait-dictionary arguments passed before the `n` function
/// arguments.
#[inline(always)]
fn strip_dict_args(args: &[Value], n: usize) -> &[Value] {
    args.get(args.len().wrapping_sub(n)..).unwrap_or_default()
}

#[cold]
fn too_few_args() -> Ctl {
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
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Box::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let r = {
                        let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                        let mut conv = Conversion::new();
                        $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                        call(&self, $($a),*)
                    };
                    vm.unchecked(|vm| r.into_value(vm))
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
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    let r = self(vm, $($a),*);
                    vm.unchecked(|vm| r.into_value(vm))
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
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
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
