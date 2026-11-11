//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::convert::{Conversion, FromVmValue, HostError, IntoVmValue, ok_vm_value};
use super::ty::{SandType, Type};
use crate::vm::value::VmValue;
use crate::vm::{Ctl, Fail, Finisher, NativeFn, Suspension, Vm, WaitKind};
use std::future::Future;
use std::marker::PhantomData;

/// Sand parameter and return types derived from a Rust function signature.
#[derive(Clone, Debug)]
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
/// Each parameter implements [`FromVmValue`] and can borrow `&str`, `&[u8]`, or
/// `&T` from the VM heap. The return type implements [`HostReturn<Ctl>`].
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a host function",
    note = "each parameter must implement `FromVmValue`, and the return type must implement `HostReturn<Ctl>`",
    note = "a synchronous function returns `HostError`, not `HostError<Fail>`"
)]
pub trait HostFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// Synchronous function registered with [`NativeModule::vm_function`](super::NativeModule::vm_function).
///
/// Receives `&mut Vm` as its first argument. Remaining arguments must be owned
/// [`FromVmValue`] types or [`VmVal`](super::VmVal) handles. The return type
/// implements [`HostReturn<Ctl>`].
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a `Vm` function",
    note = "the first parameter must be `&mut Vm`, each other parameter must be an owned `FromVmValue` type, and the return type must implement `HostReturn<Ctl>`",
    note = "a synchronous function returns `HostError`, not `HostError<Fail>`"
)]
pub trait VmFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// Asynchronous function registered with [`NativeModule::async_function`](super::NativeModule::async_function).
///
/// Arguments must be owned [`FromVmValue`] types so the returned future can
/// outlive the initial call. The output of the future implements
/// [`HostReturn<Fail>`] because the future cannot make a scheduler request:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::convert::HostError;
/// let mut m = NativeModule::new("m");
/// m.async_function("f", || async { Err::<i64, HostError>(HostError::new("io", "broken pipe")) });
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be an async host function",
    note = "each parameter must be an owned `FromVmValue` type, and the output of the future must implement `HostReturn<Fail>`",
    note = "a future returns `HostError<Fail>`, not `HostError`, because it cannot make a scheduler request"
)]
pub trait AsyncHostFunction<Marker>: Send + Sync + 'static {
    fn signature() -> Signature;
    fn into_host(self) -> Box<NativeFn>;
}

/// Return type of a registered function.
///
/// `C` is the control signal that stops the call. Functions that run before any
/// wait use [`Ctl`], and code that runs after a wait uses [`Fail`].
pub trait HostReturn<C>: SandType {
    #[doc(hidden)]
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, C>;
}

impl<R: IntoVmValue, C: From<Fail>> HostReturn<C> for R {
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, C> {
        Ok(self.into_vm_value(vm)?)
    }
}

impl<T: IntoVmValue> HostReturn<Ctl> for Result<T, Ctl> {
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        Ok(self?.into_vm_value(vm)?)
    }
}

impl<T: IntoVmValue, C: From<Fail>> HostReturn<C> for Result<T, HostError<C>> {
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, C> {
        match self {
            Ok(x) => Ok(ok_vm_value(x, vm)?),
            Err(e) => e.into_sand_error(vm),
        }
    }
}

impl<T: SandType, E: NativeError> HostReturn<Ctl> for Result<Later<T, E>, E>
where
    Result<T, E>: SandType,
{
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        match self {
            Ok(later) => later.start(vm),
            Err(e) => e.stop_call(vm),
        }
    }
}

/// Deferred result returned by a [`VmFunction`] that may suspend the calling
/// Sand task on a future.
///
/// Return `Result<Later<T, E>, E>` from the function. If `E` is [`Ctl`] or
/// [`Fail`], the Sand return type is `T`. If `E` is [`HostError`], the Sand
/// return type is `Result<T, Error>`.
#[must_use]
pub struct Later<T, E>(Completion, PhantomData<fn() -> (T, E)>);

/// Execution mode of a [`Later`] value.
enum Completion {
    /// Runs the finisher immediately without suspending the calling task.
    Now(Finisher),
    /// Suspends the calling task until the future is resolved.
    After(Suspension),
}

/// Error type for a [`Later`] result ([`Ctl`], [`Fail`], or [`HostError`]).
pub trait NativeError: Send + Sized + 'static + sealed::Sealed {
    /// Error type of the finisher closure. It excludes scheduler requests
    /// because the closure runs after the wait.
    type FinishError: Send + 'static;
    #[doc(hidden)]
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::vm::Ctl {}
    impl Sealed for crate::vm::Fail {}
    impl Sealed for super::HostError {}
}

impl NativeError for Ctl {
    type FinishError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self)
    }
}

impl NativeError for Fail {
    type FinishError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self.into())
    }
}

impl NativeError for HostError {
    type FinishError = HostError<Fail>;
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        self.into_sand_error(vm)
    }
}

impl<T: IntoVmValue + Send + 'static, E: NativeError> Later<T, E> {
    /// Suspends the calling task until `fut` is resolved, and then executes the
    /// returned closure on the VM thread to produce the Sand result.
    ///
    /// The closure returns [`NativeError::FinishError`], so it cannot make a
    /// scheduler request. It cannot write to standard output or standard error
    /// because a write can prompt for permission. Return the data to the script
    /// instead. A closure that returns a [`Ctl`] does not compile:
    ///
    /// ```compile_fail,E0271
    /// # use sandlang::native::func::Later;
    /// # use sandlang::vm::{Ctl, Vm, WaitKind};
    /// fn later(ctl: Ctl) -> Later<i64, Ctl> {
    ///     Later::new(WaitKind::Io, async { move |_: &mut Vm| -> Result<i64, Ctl> { Err(ctl) } })
    /// }
    /// ```
    ///
    /// `wait_kind` classifies the wait. Pass [`WaitKind::User`] if the task
    /// waits for a user action so that idle wait time does not count toward
    /// `max_wall_time`.
    ///
    /// Do not capture [`VmValue`] values in `fut`. The garbage collector does
    /// not trace pending futures while a task is suspended.
    pub fn new<F>(wait_kind: WaitKind, fut: impl Future<Output = F> + Send + 'static) -> Later<T, E>
    where
        F: FnOnce(&mut Vm) -> Result<T, E::FinishError> + Send + 'static,
        Result<T, E::FinishError>: HostReturn<Fail>,
    {
        let fut = async move {
            let finish = fut.await;
            Box::new(move |vm: &mut Vm| finish(vm).into_call_result(vm)) as Finisher
        };
        Later(Completion::After(Suspension { future: Box::pin(fut), wait_kind }), PhantomData)
    }

    /// Completes immediately with `r` without suspending the calling task.
    pub fn ready(r: Result<T, E::FinishError>) -> Later<T, E>
    where
        Result<T, E::FinishError>: HostReturn<Fail>,
    {
        Later(Completion::Now(Box::new(move |vm: &mut Vm| r.into_call_result(vm))), PhantomData)
    }
}

impl<T, E> Later<T, E> {
    fn start(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        match self.0 {
            Completion::Now(f) => Ok(f(vm)?),
            Completion::After(suspension) => Err(Ctl::suspend(suspension)),
        }
    }
}

/// Drops any leading trait-dictionary arguments passed before the `n` function
/// arguments.
#[inline(always)]
fn strip_dict_args(args: &[VmValue], n: usize) -> &[VmValue] {
    args.get(args.len().wrapping_sub(n)..).unwrap_or_default()
}

#[cold]
fn too_few_args() -> Ctl {
    Fail::internal("native function called with too few arguments").into()
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
            R: HostReturn<Ctl>,
            $($a: FromVmValue + 'static,)*
        {
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self) -> Box<NativeFn> {
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    let r = {
                        let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                        let mut conv = Conversion::new();
                        $(let $a = $a::from_vm_value(vm, $a, &mut conv)?;)*
                        call(&self, $($a),*)
                    };
                    vm.unchecked(|vm| r.into_call_result(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> VmFunction<(Vm, $($a,)*)> for F
        where
            F: Fn(&mut Vm, $($a),*) -> R + Send + Sync + 'static,
            R: HostReturn<Ctl>,
            $($a: for<'a> FromVmValue<Item<'a> = $a> + 'static,)*
        {
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_vm_value(vm, $a, &mut conv)?;)*
                    vm.arm_retry_stress();
                    let r = self(vm, $($a),*);
                    vm.unchecked(|vm| r.into_call_result(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut)]
        impl<F, Fut, R, $($a,)*> AsyncHostFunction<(Fut, $($a,)*)> for F
        where
            F: Fn($($a),*) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: HostReturn<Fail> + Send + 'static,
            $($a: for<'a> FromVmValue<Item<'a> = $a> + 'static,)*
        {
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_vm_value(vm, $a, &mut conv)?;)*
                    let fut = self($($a),*);
                    let future = Box::pin(async move {
                        let r = fut.await;
                        Box::new(move |vm: &mut Vm| r.into_call_result(vm)) as Finisher
                    });
                    Err(Ctl::suspend(Suspension { future, wait_kind: WaitKind::Io }))
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
