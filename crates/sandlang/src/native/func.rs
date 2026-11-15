//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::context::{NativeContext, NativeStart};
use super::convert::{Conversion, FromVmValue, HostError, IntoVmValue, ok_vm_value};
use super::ty::{SandType, Type};
use crate::vm::value::VmValue;
use crate::vm::{Ctl, Fail, NativeFn, Vm, WaitKind};
use std::future::Future;

/// Sand parameter and return types derived from a Rust function signature.
#[derive(Clone, Debug)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl Signature {
    /// Builds a signature from the [`NativeParam::SCRIPT_PARAM_TYPE`] of each
    /// parameter and the return type.
    fn new<const N: usize>(script_param_types: [Option<fn() -> Type>; N], ret: Type) -> Signature {
        Signature { params: script_param_types.into_iter().flatten().map(|sand_type| sand_type()).collect(), ret }
    }

    /// Returns `true` if `name` is `"Type.method"` and the first parameter has
    /// type `module.Type`.
    pub(crate) fn takes_self(&self, module: &str, name: &str) -> bool {
        let Some((ty, _)) = name.split_once('.') else { return false };
        matches!(self.params.first(), Some(Type::Named(n)) if n.module == module && n.name == ty && n.args.is_empty())
    }
}

/// Rust function that [`NativeModule::function`](super::NativeModule::function) registers.
///
/// [`NativeModule::function`](super::NativeModule::function) accepts three
/// kinds of Rust function, selected by the function signature:
///
/// ```
/// # use sandlang::native::NativeModule;
/// # use sandlang::vm::{Fail, Vm};
/// let mut module = NativeModule::new("text");
/// module.function("len", |s: &str| s.len() as i64);
/// module.function("reserve", |vm: &mut Vm, s: String| -> Result<i64, Fail> {
///     vm.check_memory(s.len())?;
///     Ok(s.len() as i64)
/// });
/// module.function("fetch", |url: String| async move { url.len() as i64 });
/// ```
///
/// - **Synchronous.** Each parameter implements [`NativeParam`] and can borrow
///   `&str`, `&[u8]`, or `&T` from the VM heap. The return type implements
///   [`NativeReturn<Ctl>`].
/// - **Synchronous with `&mut Vm` first.** Each parameter after `&mut Vm` is an
///   owned [`NativeParam`] type, such as `String` or
///   [`VmVal`](super::ty::VmVal). The return type implements
///   [`NativeReturn<Ctl>`]. Check permissions and limits before any side
///   effect. The engine runs the function again from the start after a
///   permission prompt or a garbage collection during a memory or resource
///   check. Return a [`Later`] to wait for a future, which can also call Sand
///   functions.
/// - **Asynchronous.** A function that returns a future suspends only the
///   calling Sand task. Each parameter is an owned `Send` [`NativeParam`] type.
///   The future is `Send`. The future output is `Send` and implements
///   [`NativeReturn<Fail>`].
///
/// A function that takes `&mut Vm` cannot have a borrowed parameter:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::vm::Vm;
/// let mut module = NativeModule::new("text");
/// module.function("len", |_: &mut Vm, s: &str| s.len() as i64);
/// ```
///
/// An async function cannot have a borrowed parameter. It also cannot take or
/// return a VM handle, because garbage collection can free the heap object
/// during the wait:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// let mut module = NativeModule::new("text");
/// module.function("fetch", |url: &str| async move { 0 });
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::ty::VmVal;
/// let mut module = NativeModule::new("text");
/// module.function("fetch", |url: VmVal<String>| async move { 0 });
/// ```
///
/// A future makes scheduler requests through a [`NativeContext`], so its
/// output cannot hold a [`Ctl`]:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::convert::HostError;
/// let mut module = NativeModule::new("text");
/// module.function("fetch", || async { Err::<i64, HostError>(HostError::new("io", "broken pipe")) });
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a native function",
    note = "each parameter must implement `NativeParam`, and the return type must implement `NativeReturn<Ctl>`",
    note = "`&mut Vm` must be the first parameter, each parameter after it must be an owned type, and the function must return a `Later` to wait",
    note = "an async function must have `Send` parameters, future, and output, and cannot hold a VM handle such as `VmVal` across a wait",
    note = "a synchronous function returns `HostError`, and a future returns `HostError<Fail>`",
    note = "a function takes at most 12 parameters"
)]
pub trait NativeFunction<Kind>: Send + Sync + 'static {
    /// Returns the Sand parameter and return types.
    fn signature() -> Signature;
    #[doc(hidden)]
    fn into_native_fn(self) -> Box<NativeFn>;
}

/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// let mut module = NativeModule::new("m");
/// module.function("f", |x: u32| 0);
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// let mut module = NativeModule::new("m");
/// module.function("f", || 0u32);
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::ty::VmVal;
/// # use sandlang::vm::value::VmValue;
/// let mut module = NativeModule::new("m");
/// module.function("f", || async { VmVal::<String>::from_raw(VmValue::Unit) });
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::ty::VmVal;
/// let mut module = NativeModule::new("m");
/// module.function("f", |items: Vec<VmVal<String>>| async move { 0 });
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::convert::HostError;
/// # use sandlang::vm::Fail;
/// let mut module = NativeModule::new("m");
/// module.function("f", || -> Result<i64, HostError<Fail>> { Ok(0) });
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::vm::Vm;
/// let mut module = NativeModule::new("m");
/// module.function("f", |x: i64, _: &mut Vm| x);
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::vm::Vm;
/// let mut module = NativeModule::new("m");
/// module.function("f", |_: &mut Vm| async { 0 });
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::ty::Param;
/// fn assert_send<T: Send>() {}
/// assert_send::<Param<0>>();
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::ty::Num;
/// fn assert_send<T: Send>() {}
/// assert_send::<Num<0>>();
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::ty::Reflect;
/// fn assert_send<T: Send>() {}
/// assert_send::<Reflect<0>>();
/// ```
///
/// ```compile_fail,E0277
/// # use sandlang::native::ty::VmVal;
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "db")]
/// struct Row(VmVal<String>);
/// impl sandlang::native::NativeObject for Row {}
/// ```
#[cfg(doctest)]
struct NativeFunctionErrors;

/// Parameter of a [`NativeFunction`].
///
/// Each [`FromVmValue`] type is a parameter that reads a script argument.
pub trait NativeParam: sealed::NativeParam {
    /// Value type passed to the Rust function. It can borrow from the VM heap
    /// for `'a`.
    type Item<'a>;

    /// Sand type of this parameter in [`Signature::params`], or `None` if the
    /// engine provides the value.
    #[doc(hidden)]
    const SCRIPT_PARAM_TYPE: Option<fn() -> Type>;

    /// Reads the parameter value from the call. `script_args[next_arg]` is the
    /// next unread script argument.
    #[doc(hidden)]
    fn from_call<'a>(vm: &'a Vm, script_args: &[VmValue], next_arg: usize, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail>;
}

impl<T: FromVmValue> sealed::NativeParam for T {}

impl<T: FromVmValue> NativeParam for T {
    type Item<'a> = T::Item<'a>;

    const SCRIPT_PARAM_TYPE: Option<fn() -> Type> = Some(<T as SandType>::sand_type);

    #[inline(always)]
    fn from_call<'a>(vm: &'a Vm, script_args: &[VmValue], next_arg: usize, conv: &mut Conversion) -> Result<T::Item<'a>, Fail> {
        T::from_vm_value(vm, script_args[next_arg], conv)
    }
}

/// Returns the number of script arguments that the parameter `P` reads.
const fn script_arg_count<P: NativeParam>() -> usize {
    P::SCRIPT_PARAM_TYPE.is_some() as usize
}

#[cold]
fn too_few_args() -> Fail {
    Fail::internal("native function called with too few arguments")
}

/// Synchronous [`NativeFunction`] without `&mut Vm`.
pub struct SyncNativeFn;

/// Synchronous [`NativeFunction`] with `&mut Vm` as its first parameter.
pub struct VmNativeFn;

/// Asynchronous [`NativeFunction`] that returns a future.
pub struct AsyncNativeFn;

/// Return type of a registered function.
///
/// `C` is the control signal that stops the call. Code that runs before a wait
/// uses [`Ctl`]. Code that runs after a wait uses [`Fail`].
pub trait NativeReturn<C>: SandType {
    #[doc(hidden)]
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, C>;
}

impl<R: IntoVmValue, C: From<Fail>> NativeReturn<C> for R {
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, C> {
        Ok(self.into_vm_value(vm)?)
    }
}

impl<T: IntoVmValue> NativeReturn<Ctl> for Result<T, Ctl> {
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        Ok(self?.into_vm_value(vm)?)
    }
}

impl<T: IntoVmValue, C: From<Fail>> NativeReturn<C> for Result<T, HostError<C>> {
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, C> {
        match self {
            Ok(x) => Ok(ok_vm_value(x, vm)?),
            Err(e) => e.into_sand_error(vm),
        }
    }
}

impl<T: IntoVmValue, E: NativeError> NativeReturn<Ctl> for Result<Later<T, E>, E>
where
    Result<T, E>: SandType,
{
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        match self {
            Ok(Later(Completion::Now(r))) => Ok(E::future_result(r, vm)?),
            Ok(Later(Completion::After(start))) => Err(Ctl::native_call(start)),
            Err(e) => e.stop_call(vm),
        }
    }
}

/// Deferred result of a [`NativeFunction`] that suspends the calling Sand task
/// on a future.
///
/// Return `Result<Later<T, E>, E>` from the function. If `E` is [`Ctl`] or
/// [`Fail`], the Sand return type is `T`. If `E` is [`HostError`], the Sand
/// return type is `Result<T, Error>`.
#[must_use]
pub struct Later<T, E: NativeError>(Completion<T, E>);

/// Execution mode of a [`Later`] value.
enum Completion<T, E: NativeError> {
    /// Returns the result without suspending the calling task.
    Now(Result<T, E::FutureError>),
    /// Suspends the calling task until the future is resolved.
    After(NativeStart),
}

/// Error type for a [`Later`] result ([`Ctl`], [`Fail`], or [`HostError`]).
pub trait NativeError: Send + Sized + 'static + sealed::NativeError {
    /// Error type of the future output. It excludes scheduler requests, which
    /// the future makes through its [`NativeContext`].
    type FutureError: 'static;
    #[doc(hidden)]
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl>;
    #[doc(hidden)]
    fn future_result<T: IntoVmValue>(r: Result<T, Self::FutureError>, vm: &mut Vm) -> Result<VmValue, Fail>;
}

mod sealed {
    pub trait NativeError {}
    impl NativeError for crate::vm::Ctl {}
    impl NativeError for crate::vm::Fail {}
    impl NativeError for super::HostError {}

    pub trait NativeParam {}
}

impl NativeError for Ctl {
    type FutureError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self)
    }
    fn future_result<T: IntoVmValue>(r: Result<T, Fail>, vm: &mut Vm) -> Result<VmValue, Fail> {
        r?.into_vm_value(vm)
    }
}

impl NativeError for Fail {
    type FutureError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self.into())
    }
    fn future_result<T: IntoVmValue>(r: Result<T, Fail>, vm: &mut Vm) -> Result<VmValue, Fail> {
        r?.into_vm_value(vm)
    }
}

impl NativeError for HostError {
    type FutureError = HostError<Fail>;
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        self.into_sand_error(vm)
    }
    fn future_result<T: IntoVmValue>(r: Result<T, HostError<Fail>>, vm: &mut Vm) -> Result<VmValue, Fail> {
        r.into_call_result(vm)
    }
}

impl<T: IntoVmValue + 'static, E: NativeError> Later<T, E> {
    /// Suspends the calling task on the future that `build` returns for the
    /// call's [`NativeContext`]. The engine converts the future output into
    /// the Sand result.
    ///
    /// `wait_kind` classifies waits on host I/O. Pass [`WaitKind::User`] if the
    /// task waits for a user action so that idle wait time does not count
    /// toward `max_wall_time`.
    ///
    /// The future cannot hold a VM handle, because garbage collection can free
    /// the heap object during a wait:
    ///
    /// ```compile_fail,E0277
    /// # use sandlang::native::func::Later;
    /// # use sandlang::native::ty::VmVal;
    /// # use sandlang::vm::{Fail, WaitKind};
    /// fn later(text: VmVal<String>) -> Later<i64, Fail> {
    ///     Later::new(WaitKind::Io, |cx| async move { Ok(cx.with_vm(|vm| text.str(&vm.heap).len() as i64)) })
    /// }
    /// ```
    ///
    /// Root the value with [`NativeContext::root`] before the wait instead:
    ///
    /// ```
    /// # use sandlang::native::func::Later;
    /// # use sandlang::native::ty::VmVal;
    /// # use sandlang::vm::{Fail, WaitKind};
    /// fn later(text: VmVal<String>) -> Later<i64, Fail> {
    ///     Later::new(WaitKind::Io, |cx| {
    ///         let text = cx.root(text.raw());
    ///         async move { Ok(cx.with_vm(|vm| vm.str(cx.value(text)).len() as i64)) }
    ///     })
    /// }
    /// ```
    ///
    /// The compiler does not check a raw [`VmValue`]. Do not hold a raw
    /// [`VmValue`] across a wait.
    pub fn new<F>(wait_kind: WaitKind, build: impl FnOnce(NativeContext) -> F) -> Later<T, E>
    where
        F: Future<Output = Result<T, E::FutureError>> + Send + 'static,
    {
        Later(Completion::After(NativeStart::new(wait_kind, |cx| {
            let future = build(cx.clone());
            async move {
                let r = future.await;
                cx.with_vm(|vm| vm.unchecked(|vm| E::future_result(r, vm)))
            }
        })))
    }

    /// Returns a [`Later`] that produces `r` without suspending the calling task.
    pub fn ready(r: Result<T, E::FutureError>) -> Later<T, E> {
        Later(Completion::Now(r))
    }
}

/// Returns the last `n` elements of `args`, which are the script arguments
/// that the parameters read. The Sand compiler can pass unused trait
/// dictionaries before them.
#[inline(always)]
fn script_args(args: &[VmValue], n: usize) -> Result<&[VmValue], Fail> {
    match args.get(args.len().wrapping_sub(n)..) {
        Some(script_args) => Ok(script_args),
        None => Err(too_few_args()),
    }
}

/// Reads each parameter `$param` of a native call into a variable named `$param`.
macro_rules! read_params {
    ($vm:ident, $args:ident, $($param:ident),*) => {
        let script_args = script_args($args, const { 0 $(+ script_arg_count::<$param>())* })?;
        let mut next_arg = 0;
        let mut conv = Conversion::new();
        $(
            let $param = $param::from_call($vm, script_args, next_arg, &mut conv)?;
            next_arg += const { script_arg_count::<$param>() };
        )*
    };
}

macro_rules! fn_impls {
    ($($param:ident),*) => {
        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($param,)*> NativeFunction<(SyncNativeFn, ($($param,)*))> for F
        where
            F: Fn($($param),*) -> R + for<'a> Fn($($param::Item<'a>),*) -> R + Send + Sync + 'static,
            R: NativeReturn<Ctl>,
            $($param: NativeParam + 'static,)*
        {
            fn signature() -> Signature {
                Signature::new([$($param::SCRIPT_PARAM_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> Box<NativeFn> {
                fn call<R, $($param,)*>(f: &impl Fn($($param),*) -> R, $($param: $param,)*) -> R {
                    f($($param),*)
                }
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    let r = {
                        read_params!(vm, args, $($param),*);
                        call(&self, $($param),*)
                    };
                    vm.unchecked(|vm| r.into_call_result(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($param,)*> NativeFunction<(VmNativeFn, ($($param,)*))> for F
        where
            F: Fn(&mut Vm, $($param),*) -> R + Send + Sync + 'static,
            R: NativeReturn<Ctl>,
            $($param: for<'a> NativeParam<Item<'a> = $param> + 'static,)*
        {
            fn signature() -> Signature {
                Signature::new([$($param::SCRIPT_PARAM_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    read_params!(vm, args, $($param),*);
                    vm.arm_retry_stress();
                    let r = self(vm, $($param),*);
                    vm.unchecked(|vm| r.into_call_result(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments)]
        impl<F, Fut, R, $($param,)*> NativeFunction<(AsyncNativeFn, ($($param,)*))> for F
        where
            F: Fn($($param),*) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: NativeReturn<Fail> + Send + 'static,
            $($param: for<'a> NativeParam<Item<'a> = $param> + Send + 'static,)*
        {
            fn signature() -> Signature {
                Signature::new([$($param::SCRIPT_PARAM_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    read_params!(vm, args, $($param),*);
                    let future = self($($param),*);
                    Err(Ctl::native_call(NativeStart::new(WaitKind::Io, |cx| async move {
                        let r = future.await;
                        cx.with_vm(|vm| vm.unchecked(|vm| r.into_call_result(vm)))
                    })))
                })
            }
        }
    };
}
fn_impls!();
fn_impls!(P1);
fn_impls!(P1, P2);
fn_impls!(P1, P2, P3);
fn_impls!(P1, P2, P3, P4);
fn_impls!(P1, P2, P3, P4, P5);
fn_impls!(P1, P2, P3, P4, P5, P6);
fn_impls!(P1, P2, P3, P4, P5, P6, P7);
fn_impls!(P1, P2, P3, P4, P5, P6, P7, P8);
fn_impls!(P1, P2, P3, P4, P5, P6, P7, P8, P9);
fn_impls!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
fn_impls!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
fn_impls!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Config, Engine, HostModules, MemorySink};
    use crate::native::NativeModule;
    use crate::native::ty::{Num, Param};

    /// Value of [`Config::retry_stress`], which the engine provides.
    struct RetryStress(bool);

    impl sealed::NativeParam for RetryStress {}

    impl NativeParam for RetryStress {
        type Item<'a> = RetryStress;

        const SCRIPT_PARAM_TYPE: Option<fn() -> Type> = None;

        fn from_call(vm: &Vm, _: &[VmValue], _: usize, _: &mut Conversion) -> Result<RetryStress, Fail> {
            Ok(RetryStress(vm.retry_stress))
        }
    }

    fn script_params<Kind, F: NativeFunction<Kind>>(_: &F) -> Vec<String> {
        F::signature().params.iter().map(Type::to_string).collect()
    }

    #[test]
    fn native_functions_take_engine_provided_params() {
        let sync_fn = |stress: RetryStress, s: &str| format!("{s} {}", stress.0);
        let vm_fn = |_: &mut Vm, n: i64, stress: RetryStress| format!("{n} {}", stress.0);
        let async_fn = |stress: RetryStress, n: i64| async move { format!("{n} {}", stress.0) };
        let generic_fn = |_: Num<0>, stress: RetryStress, x: Param<0>| (x, stress.0);
        assert_eq!(script_params(&sync_fn), ["str"]);
        assert_eq!(script_params(&vm_fn), ["int"]);
        assert_eq!(script_params(&async_fn), ["int"]);
        assert_eq!(script_params(&generic_fn).len(), 2);

        let mut module = NativeModule::new("provided");
        module.source("pub native fn generic_fn<T: Num>(x: T) -> (T, bool)");
        module.function("sync_fn", sync_fn).function("vm_fn", vm_fn).function("async_fn", async_fn).function("generic_fn", generic_fn);
        let src = "import provided\n\
                   print(provided.sync_fn(\"a\"))\nprint(provided.vm_fn(1))\nprint(provided.async_fn(2))\nprint(provided.generic_fn(3))";
        for stress in [false, true] {
            let out = MemorySink::new();
            let config = Config::new().retry_stress(stress).host_modules(HostModules::new([module.clone()]).unwrap()).stdout(out.clone());
            let run = async { Engine::new(config).run_source("main.sand", src).await };
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(run).unwrap();
            assert_eq!(out.contents(), format!("a {stress}\n1 {stress}\n2 {stress}\n({}, {stress})\n", 3));
        }
    }
}
