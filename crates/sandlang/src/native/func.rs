//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::context::{AsyncNativeCall, CallContext, NativeContext};
use super::convert::{Conversion, FromVmValue, HostError, IntoVmValue, ok_vm_value};
use super::ty::{SandType, Type};
use crate::vm::value::VmValue;
use crate::vm::{ContextNativeFn, Ctl, Fail, Fiber, NativeFn, Vm, WaitKind};
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
/// [`NativeModule::function`](super::NativeModule::function) selects from four
/// kinds of Rust function by signature:
///
/// ```
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::context::CallContext;
/// # use sandlang::native::ty::VmVal;
/// # use sandlang::vm::{Fail, Vm};
/// let mut module = NativeModule::new("text");
/// module.function("len", |text: &str| text.len() as i64);
/// module.function("reserve", |vm: &mut Vm, text: String| -> Result<i64, Fail> {
///     vm.check_memory(text.len())?;
///     Ok(text.len() as i64)
/// });
/// module.function("call", |context: &mut CallContext, sand_fn: VmVal<fn() -> i64>| -> Result<VmVal<i64>, Fail> {
///     Ok(VmVal::from_raw(context.call(sand_fn.raw(), &[])?))
/// });
/// module.function("fetch", |url: String| async move { url.len() as i64 });
/// ```
///
/// - **Synchronous.** Each parameter implements [`NativeParam`]. Parameters can
///   borrow `&str`, `&[u8]`, or `&T` from the VM heap. The return type
///   implements [`NativeReturn<Ctl>`].
/// - **Synchronous with `&mut Vm` first.** Each parameter after `&mut Vm` is an
///   owned [`NativeParam`] type. The return type implements
///   [`NativeReturn<Ctl>`]. Check permissions and limits before any side
///   effect. The engine reruns the function after a permission prompt or
///   limit-triggered garbage collection. [`Later`] suspends the calling task on
///   a future. That future can await host events or Sand callbacks.
/// - **Synchronous with `&mut CallContext` first.** The function can call Sand
///   functions on the calling task through [`CallContext::call`]. Each
///   parameter after `&mut CallContext` is an owned [`NativeParam`] type. The
///   return type implements [`NativeReturn<Fail>`]. The engine runs the
///   function once per call. [`CallContext::step`] retries limit checks in
///   place after garbage collection.
/// - **Asynchronous.** The function returns a future. The engine suspends only
///   the calling Sand task. Each parameter is an owned `Send` [`NativeParam`]
///   type. The future and its output are `Send`. The output implements
///   [`NativeReturn<Fail>`].
///
/// The compiler rejects borrowed parameters in `&mut Vm` and `&mut CallContext`
/// functions:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::vm::Vm;
/// let mut module = NativeModule::new("text");
/// module.function("len", |_: &mut Vm, s: &str| s.len() as i64);
/// ```
///
/// Garbage collection can free unrooted heap objects during a wait. The
/// compiler therefore rejects borrowed parameters and VM handles in an async
/// function:
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
/// A future makes scheduler requests through [`NativeContext`]. Its output
/// error type is therefore [`Fail`] or [`HostError<Fail>`]:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::convert::HostError;
/// let mut module = NativeModule::new("text");
/// module.function("fetch", || async { Err::<i64, HostError>(HostError::new("io", "broken pipe")) });
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a native function",
    note = "each parameter must implement `NativeParam`",
    note = "place `&mut Vm` or `&mut CallContext` first",
    note = "parameters after `&mut Vm` or `&mut CallContext` must be owned types",
    note = "plain synchronous and `&mut Vm` functions suspend the calling task with `Result<Later<T, E>, E>`",
    note = "an async function requires `Send` parameters, future, and output",
    note = "for `HostError` results, plain synchronous and `&mut Vm` functions use `HostError`",
    note = "for `HostError` results, `&mut CallContext` functions and futures use `HostError<Fail>`",
    note = "a function takes at most 12 parameters"
)]
pub trait NativeFunction<Kind>: Send + Sync + 'static {
    /// Returns the Sand parameter and return types.
    fn signature() -> Signature;
    #[doc(hidden)]
    fn into_native_fn(self) -> BoxedNativeFn;
}

/// Boxed native function returned by [`NativeFunction::into_native_fn`].
#[doc(hidden)]
pub enum BoxedNativeFn {
    Plain(Box<NativeFn>),
    Context(Box<ContextNativeFn>),
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

/// Plain synchronous [`NativeFunction`] that takes only [`NativeParam`] parameters.
pub struct SyncNativeFn;

/// Synchronous [`NativeFunction`] with `&mut Vm` as its first parameter.
pub struct VmNativeFn;

/// Synchronous [`NativeFunction`] with `&mut CallContext` as its first parameter.
pub struct CallContextNativeFn;

/// Asynchronous [`NativeFunction`] that returns a future.
pub struct AsyncNativeFn;

/// Return type of a registered function.
///
/// `C` is the control signal that stops the call. Plain synchronous and
/// `&mut Vm` functions use [`Ctl`]. [`CallContext`] functions and futures use
/// [`Fail`].
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
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    fn into_call_result(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        match self {
            Ok(Later(Completion::Ready(result))) => Ok(result.into_call_result(vm)?),
            Ok(Later(Completion::Async(call))) => Err(Ctl::async_native(call)),
            Err(e) => e.stop_call(vm),
        }
    }
}

/// Immediate result or pending future returned by a [`NativeFunction`].
///
/// Return `Result<Later<T, E>, E>` from the function. If `E` is [`Ctl`] or
/// [`Fail`], the Sand return type is `T`. If `E` is [`HostError`], the Sand
/// return type is `Result<T, Error>`.
#[must_use]
pub struct Later<T, E: NativeError>(Completion<T, E>);

/// Execution mode of a [`Later`] value.
enum Completion<T, E: NativeError> {
    /// Returns the result immediately on the calling task.
    Ready(Result<T, E::FutureError>),
    /// Suspends the calling task on an async native call.
    Async(AsyncNativeCall),
}

/// Error type for a [`Later`] result: [`Ctl`], [`Fail`], or [`HostError`].
pub trait NativeError: Send + Sized + 'static + sealed::NativeError {
    /// Error type of the future output: [`Fail`] or [`HostError<Fail>`].
    ///
    /// The future submits scheduler requests through [`NativeContext`].
    type FutureError: From<Fail> + 'static;
    #[doc(hidden)]
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl>;
    /// Splits `self` into a [`Ctl`] signal for `step` or a final error.
    #[doc(hidden)]
    fn into_ctl(self) -> Result<Ctl, Self::FutureError>;
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
    fn into_ctl(self) -> Result<Ctl, Fail> {
        Ok(self)
    }
}

impl NativeError for Fail {
    type FutureError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self.into())
    }
    fn into_ctl(self) -> Result<Ctl, Fail> {
        Ok(self.into())
    }
}

impl NativeError for HostError {
    type FutureError = HostError<Fail>;
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        self.into_sand_error(vm)
    }
    fn into_ctl(self) -> Result<Ctl, HostError<Fail>> {
        self.into_abort()
    }
}

impl<T: IntoVmValue + 'static, E: NativeError> Later<T, E>
where
    Result<T, E::FutureError>: NativeReturn<Fail>,
{
    /// Suspends the calling task on the future returned by `build`.
    ///
    /// The engine passes the call's [`NativeContext`] to `build` before the
    /// first poll. Inside `build`, call [`NativeContext::root`] to root heap
    /// values before returning the future. Access the VM through
    /// [`NativeContext`] only inside the returned future. After the future
    /// returns `Ready`, the engine converts its output into the Sand return
    /// value.
    ///
    /// `wait_kind` classifies host I/O waits. Pass [`WaitKind::User`] if the
    /// task waits for user input. The engine excludes [`WaitKind::User`] wait
    /// time from `max_wall_time`.
    ///
    /// The garbage collector can free unrooted heap objects during a wait. The
    /// compiler therefore rejects VM handles inside the future:
    ///
    /// ```compile_fail,E0277
    /// # use sandlang::native::func::Later;
    /// # use sandlang::native::ty::VmVal;
    /// # use sandlang::vm::{Fail, WaitKind};
    /// fn later(text: VmVal<String>) -> Later<i64, Fail> {
    ///     Later::new(WaitKind::Io, |context| async move { Ok(context.with_vm(|vm| text.str(&vm.heap).len() as i64)) })
    /// }
    /// ```
    ///
    /// Root the value with [`NativeContext::root`] before building the future:
    ///
    /// ```
    /// # use sandlang::native::func::Later;
    /// # use sandlang::native::ty::VmVal;
    /// # use sandlang::vm::{Fail, WaitKind};
    /// fn later(text: VmVal<String>) -> Later<i64, Fail> {
    ///     Later::new(WaitKind::Io, |context| {
    ///         let text = context.root(text.raw());
    ///         async move { Ok(context.with_vm(|vm| vm.str(context.value(text)).len() as i64)) }
    ///     })
    /// }
    /// ```
    ///
    /// [`VmValue`] implements [`Send`]. The compiler therefore accepts a raw
    /// [`VmValue`] inside a future. Garbage collection during a wait can
    /// invalidate that [`VmValue`]. Keep only
    /// [`RootedValue`](super::context::RootedValue) handles across waits.
    pub fn new<F>(wait_kind: WaitKind, build: impl FnOnce(NativeContext) -> F) -> Later<T, E>
    where
        F: Future<Output = Result<T, E::FutureError>> + Send + 'static,
    {
        Later(Completion::Async(AsyncNativeCall::new(wait_kind, build)))
    }

    /// Returns `result` immediately from a [`Later`] function on the calling task.
    pub fn ready(result: Result<T, E::FutureError>) -> Later<T, E> {
        Later(Completion::Ready(result))
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
    ($vm:ident, $args:expr, $($param:ident),*) => {
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
            fn into_native_fn(self) -> BoxedNativeFn {
                fn call<R, $($param,)*>(f: &impl Fn($($param),*) -> R, $($param: $param,)*) -> R {
                    f($($param),*)
                }
                BoxedNativeFn::Plain(Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    let output = {
                        read_params!(vm, args, $($param),*);
                        call(&self, $($param),*)
                    };
                    vm.unchecked(|vm| output.into_call_result(vm))
                }))
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, clippy::unused_unit)]
        impl<F, R, $($param,)*> NativeFunction<(VmNativeFn, ($($param,)*))> for F
        where
            F: Fn(&mut Vm, $($param),*) -> R + Send + Sync + 'static,
            R: NativeReturn<Ctl>,
            $($param: for<'a> NativeParam<Item<'a> = $param> + 'static,)*
        {
            fn signature() -> Signature {
                Signature::new([$($param::SCRIPT_PARAM_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> BoxedNativeFn {
                BoxedNativeFn::Plain(Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    read_params!(vm, args, $($param),*);
                    vm.arm_retry_stress();
                    let output = self(vm, $($param),*);
                    vm.unchecked(|vm| output.into_call_result(vm))
                }))
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments)]
        impl<F, R, $($param,)*> NativeFunction<(CallContextNativeFn, ($($param,)*))> for F
        where
            F: Fn(&mut CallContext<'_>, $($param),*) -> R + Send + Sync + 'static,
            R: NativeReturn<Fail>,
            $($param: for<'a> NativeParam<Item<'a> = $param> + 'static,)*
        {
            fn signature() -> Signature {
                Signature::new([$($param::SCRIPT_PARAM_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> BoxedNativeFn {
                BoxedNativeFn::Context(Box::new(move |vm: &mut Vm, fiber: &mut Fiber, args: std::ops::Range<usize>| -> Result<VmValue, Ctl> {
                    read_params!(vm, &fiber.stack[args], $($param),*);
                    vm.arm_retry_stress();
                    let mut context = CallContext::new(vm, fiber);
                    let output = self(&mut context, $($param),*);
                    context.finish(output)
                }))
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
            fn into_native_fn(self) -> BoxedNativeFn {
                BoxedNativeFn::Plain(Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    read_params!(vm, args, $($param),*);
                    let future = self($($param),*);
                    Err(Ctl::async_native(AsyncNativeCall::new(WaitKind::Io, |_| future)))
                }))
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
