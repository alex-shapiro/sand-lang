//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::convert::{Conversion, FromVmValue, HostError, IntoVmValue, ok_vm_value};
use super::ty::{SandType, Type};
use crate::vm::value::VmValue;
use crate::vm::{Ctl, Fail, NativeFn, Suspension, Vm, WaitKind};
use std::future::Future;

/// Sand parameter and return types derived from a Rust function signature.
#[derive(Clone, Debug)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl Signature {
    /// Builds a signature from the [`NativeParam::SCRIPT_ARG_TYPE`] of each
    /// parameter.
    fn of<const N: usize>(script_arg_types: [Option<fn() -> Type>; N], ret: Type) -> Signature {
        Signature { params: script_arg_types.into_iter().flatten().map(|sand_type| sand_type()).collect(), ret }
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
/// The parameters and the return type of the function select one of three
/// forms:
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
/// - **Parameters only.** Each parameter implements [`NativeParam`] and can
///   borrow `&str`, `&[u8]`, or `&T` from the VM heap. The return type
///   implements [`NativeReturn<Ctl>`].
/// - **`&mut Vm` first.** Each parameter after the `Vm` is an owned
///   [`NativeParam`] type, such as `String` or a [`VmVal`](super::ty::VmVal)
///   handle. The return type implements [`NativeReturn<Ctl>`]. The engine runs
///   the function again from the start after a permission prompt, or after a
///   garbage collection that a memory or resource check triggers. Check
///   permissions and limits before any side effect. To wait for a future,
///   return a [`Later`].
/// - **A future as the result.** Only the calling Sand task waits for the
///   future. Each parameter is an owned [`NativeParam`] type that is `Send`.
///   The output of the future is `Send` and implements [`NativeReturn<Fail>`].
///
/// A function that takes `&mut Vm` cannot borrow an argument from the VM heap:
///
/// ```compile_fail,E0277
/// # use sandlang::native::NativeModule;
/// # use sandlang::vm::Vm;
/// let mut module = NativeModule::new("text");
/// module.function("len", |_: &mut Vm, s: &str| s.len() as i64);
/// ```
///
/// A future cannot borrow an argument, and it cannot take or return a VM
/// handle. Garbage collection can free the heap object of a handle during the
/// wait:
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
/// A future cannot make a scheduler request, so its output cannot hold a
/// [`Ctl`]:
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
    note = "`&mut Vm` must be the first parameter, and each parameter after it must be an owned type",
    note = "a function that returns a future must take owned `Send` parameters, and the output of the future must be `Send` and implement `NativeReturn<Fail>`",
    note = "a function that takes `&mut Vm` cannot return a future: return a `Later` to wait",
    note = "a VM handle such as `VmVal` is not `Send`, because a handle cannot cross a wait",
    note = "a synchronous function returns `HostError`, and a future returns `HostError<Fail>`",
    note = "a function takes at most 12 parameters"
)]
pub trait NativeFunction<Form>: Send + Sync + 'static {
    /// Returns the Sand types of the script arguments and of the result.
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
#[cfg(doctest)]
struct FunctionFormErrors;

/// Parameter of a [`NativeFunction`].
///
/// Each [`FromVmValue`] type is a parameter that reads a script argument.
pub trait NativeParam: sealed::Param {
    /// Type that the function receives. A script argument can borrow from the
    /// VM heap for `'a`.
    type Item<'a>;

    /// Sand type of the script argument that the parameter reads, or `None` if
    /// the engine provides the value.
    #[doc(hidden)]
    const SCRIPT_ARG_TYPE: Option<fn() -> Type>;

    /// Reads the parameter. `script_args[next_arg]` is the first script
    /// argument that the parameters before this one did not read.
    #[doc(hidden)]
    fn from_call<'a>(vm: &'a Vm, script_args: &[VmValue], next_arg: usize, conv: &mut Conversion) -> Result<Self::Item<'a>, Fail>;
}

impl<T: FromVmValue> sealed::Param for T {}

impl<T: FromVmValue> NativeParam for T {
    type Item<'a> = T::Item<'a>;

    const SCRIPT_ARG_TYPE: Option<fn() -> Type> = Some(<T as SandType>::sand_type);

    #[inline(always)]
    fn from_call<'a>(vm: &'a Vm, script_args: &[VmValue], next_arg: usize, conv: &mut Conversion) -> Result<T::Item<'a>, Fail> {
        T::from_vm_value(vm, script_args[next_arg], conv)
    }
}

/// Returns the number of script arguments that the parameter `P` reads.
const fn script_arg_count<P: NativeParam>() -> usize {
    P::SCRIPT_ARG_TYPE.is_some() as usize
}

#[cold]
fn too_few_args() -> Fail {
    Fail::internal("native function called with too few arguments")
}

/// Forms of a [`NativeFunction`].
mod form {
    pub struct Params;
    pub struct VmFirst;
    pub struct Async;
}

/// Return type of a registered function.
///
/// `C` is the control signal that stops the call. Functions that run before any
/// wait use [`Ctl`], and code that runs after a wait uses [`Fail`].
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
            Ok(Later(Completion::Now(r))) => Ok(E::finish_call(r, vm)?),
            Ok(Later(Completion::After(suspension))) => Err(Ctl::suspend(suspension)),
            Err(e) => e.stop_call(vm),
        }
    }
}

/// Deferred result of a [`NativeFunction`] that can suspend the calling Sand
/// task on a future.
///
/// Return `Result<Later<T, E>, E>` from the function. If `E` is [`Ctl`] or
/// [`Fail`], the Sand return type is `T`. If `E` is [`HostError`], the Sand
/// return type is `Result<T, Error>`.
#[must_use]
pub struct Later<T, E: NativeError>(Completion<T, E>);

/// Execution mode of a [`Later`] value.
enum Completion<T, E: NativeError> {
    /// Returns the result without suspending the calling task.
    Now(Result<T, E::FinishError>),
    /// Suspends the calling task until the future is resolved.
    After(Suspension),
}

/// Error type for a [`Later`] result ([`Ctl`], [`Fail`], or [`HostError`]).
pub trait NativeError: Send + Sized + 'static + sealed::Error {
    /// Error type of the finisher closure. It excludes scheduler requests
    /// because the closure runs after the wait.
    type FinishError: Send + 'static;
    #[doc(hidden)]
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl>;
    #[doc(hidden)]
    fn finish_call<T: IntoVmValue>(r: Result<T, Self::FinishError>, vm: &mut Vm) -> Result<VmValue, Fail>;
}

mod sealed {
    pub trait Error {}
    impl Error for crate::vm::Ctl {}
    impl Error for crate::vm::Fail {}
    impl Error for super::HostError {}

    pub trait Param {}
}

impl NativeError for Ctl {
    type FinishError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self)
    }
    fn finish_call<T: IntoVmValue>(r: Result<T, Fail>, vm: &mut Vm) -> Result<VmValue, Fail> {
        r?.into_vm_value(vm)
    }
}

impl NativeError for Fail {
    type FinishError = Fail;
    fn stop_call(self, _: &mut Vm) -> Result<VmValue, Ctl> {
        Err(self.into())
    }
    fn finish_call<T: IntoVmValue>(r: Result<T, Fail>, vm: &mut Vm) -> Result<VmValue, Fail> {
        r?.into_vm_value(vm)
    }
}

impl NativeError for HostError {
    type FinishError = HostError<Fail>;
    fn stop_call(self, vm: &mut Vm) -> Result<VmValue, Ctl> {
        self.into_sand_error(vm)
    }
    fn finish_call<T: IntoVmValue>(r: Result<T, HostError<Fail>>, vm: &mut Vm) -> Result<VmValue, Fail> {
        r.into_call_result(vm)
    }
}

impl<T: IntoVmValue + 'static, E: NativeError> Later<T, E> {
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
    /// `fut` and the closure cannot hold a VM handle, because garbage
    /// collection can free the heap object of the handle during the wait. The
    /// closure can allocate and return a new handle.
    ///
    /// ```compile_fail,E0277
    /// # use sandlang::native::func::Later;
    /// # use sandlang::native::ty::VmVal;
    /// # use sandlang::vm::{Fail, Vm, WaitKind};
    /// fn later(text: VmVal<String>) -> Later<i64, Fail> {
    ///     Later::new(WaitKind::Io, async move { move |vm: &mut Vm| Ok(text.str(&vm.heap).len() as i64) })
    /// }
    /// ```
    ///
    /// Copy the data that the closure needs before the wait:
    ///
    /// ```
    /// # use sandlang::native::func::Later;
    /// # use sandlang::native::ty::VmVal;
    /// # use sandlang::vm::{Fail, Vm, WaitKind};
    /// fn later(vm: &Vm, text: VmVal<String>) -> Later<i64, Fail> {
    ///     let len = text.str(&vm.heap).len() as i64;
    ///     Later::new(WaitKind::Io, async move { move |_: &mut Vm| Ok(len) })
    /// }
    /// ```
    ///
    /// The compiler does not check a raw [`VmValue`]. Do not hold a raw
    /// [`VmValue`] in `fut` or in the closure.
    pub fn new<F>(wait_kind: WaitKind, fut: impl Future<Output = F> + Send + 'static) -> Later<T, E>
    where
        F: FnOnce(&mut Vm) -> Result<T, E::FinishError> + Send + 'static,
    {
        Later(Completion::After(Suspension::new(wait_kind, async move {
            let finish = fut.await;
            move |vm: &mut Vm| E::finish_call(finish(vm), vm)
        })))
    }

    /// Completes immediately with `r` without suspending the calling task.
    pub fn ready(r: Result<T, E::FinishError>) -> Later<T, E> {
        Later(Completion::Now(r))
    }
}

/// Returns the last `n` of `args`, which are the script arguments that the
/// parameters read. The Sand compiler can pass trait dictionaries that no
/// parameter reads before them.
#[inline(always)]
fn script_args(args: &[VmValue], n: usize) -> Result<&[VmValue], Fail> {
    match args.get(args.len().wrapping_sub(n)..) {
        Some(script_args) => Ok(script_args),
        None => Err(too_few_args()),
    }
}

/// Reads each parameter `$a` of a native call into a variable named `$a`.
macro_rules! read_params {
    ($vm:ident, $args:ident, $($a:ident),*) => {
        let script_args = script_args($args, const { 0 $(+ script_arg_count::<$a>())* })?;
        let mut next_arg = 0;
        let mut conv = Conversion::new();
        $(
            let $a = $a::from_call($vm, script_args, next_arg, &mut conv)?;
            next_arg += const { script_arg_count::<$a>() };
        )*
    };
}

macro_rules! fn_impls {
    ($($a:ident),*) => {
        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> NativeFunction<(form::Params, $($a,)*)> for F
        where
            F: Fn($($a),*) -> R + for<'a> Fn($($a::Item<'a>),*) -> R + Send + Sync + 'static,
            R: NativeReturn<Ctl>,
            $($a: NativeParam + 'static,)*
        {
            fn signature() -> Signature {
                Signature::of([$($a::SCRIPT_ARG_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> Box<NativeFn> {
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    let r = {
                        read_params!(vm, args, $($a),*);
                        call(&self, $($a),*)
                    };
                    vm.unchecked(|vm| r.into_call_result(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> NativeFunction<(form::VmFirst, $($a,)*)> for F
        where
            F: Fn(&mut Vm, $($a),*) -> R + Send + Sync + 'static,
            R: NativeReturn<Ctl>,
            $($a: for<'a> NativeParam<Item<'a> = $a> + 'static,)*
        {
            fn signature() -> Signature {
                Signature::of([$($a::SCRIPT_ARG_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    read_params!(vm, args, $($a),*);
                    vm.arm_retry_stress();
                    let r = self(vm, $($a),*);
                    vm.unchecked(|vm| r.into_call_result(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, unused_assignments)]
        impl<F, Fut, R, $($a,)*> NativeFunction<(form::Async, $($a,)*)> for F
        where
            F: Fn($($a),*) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: NativeReturn<Fail> + Send + 'static,
            $($a: for<'a> NativeParam<Item<'a> = $a> + Send + 'static,)*
        {
            fn signature() -> Signature {
                Signature::of([$($a::SCRIPT_ARG_TYPE),*], R::sand_type())
            }
            fn into_native_fn(self) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, args: &[VmValue]| -> Result<VmValue, Ctl> {
                    read_params!(vm, args, $($a),*);
                    let fut = self($($a),*);
                    Err(Ctl::suspend(Suspension::new(WaitKind::Io, async move {
                        let r = fut.await;
                        move |vm: &mut Vm| r.into_call_result(vm)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Config, Engine, HostModules, MemorySink};
    use crate::native::NativeModule;
    use crate::native::ty::{Num, Param};

    /// Value of [`Config::retry_stress`], which the engine provides.
    struct RetryStress(bool);

    impl sealed::Param for RetryStress {}

    impl NativeParam for RetryStress {
        type Item<'a> = RetryStress;

        const SCRIPT_ARG_TYPE: Option<fn() -> Type> = None;

        fn from_call(vm: &Vm, _: &[VmValue], _: usize, _: &mut Conversion) -> Result<RetryStress, Fail> {
            Ok(RetryStress(vm.retry_stress))
        }
    }

    fn script_params<Form, F: NativeFunction<Form>>(_: &F) -> Vec<String> {
        F::signature().params.iter().map(Type::to_string).collect()
    }

    #[test]
    fn each_form_takes_provided_values_among_script_arguments() {
        let params_only = |stress: RetryStress, s: &str| format!("{s} {}", stress.0);
        let vm_first = |_: &mut Vm, n: i64, stress: RetryStress| format!("{n} {}", stress.0);
        let future = |stress: RetryStress, n: i64| async move { format!("{n} {}", stress.0) };
        let generic = |_: Num<0>, stress: RetryStress, x: Param<0>| (x, stress.0);
        assert_eq!(script_params(&params_only), ["str"]);
        assert_eq!(script_params(&vm_first), ["int"]);
        assert_eq!(script_params(&future), ["int"]);
        assert_eq!(script_params(&generic).len(), 2);

        let mut module = NativeModule::new("provided");
        module.source("pub native fn generic<T: Num>(x: T) -> (T, bool)");
        module.function("params_only", params_only).function("vm_first", vm_first).function("future", future).function("generic", generic);
        let src = "import provided\n\
                   print(provided.params_only(\"a\"))\nprint(provided.vm_first(1))\nprint(provided.future(2))\nprint(provided.generic(3))";
        for stress in [false, true] {
            let out = MemorySink::new();
            let config = Config::new().retry_stress(stress).host_modules(HostModules::new([module.clone()]).unwrap()).stdout(out.clone());
            let run = async { Engine::new(config).run_source("main.sand", src).await };
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(run).unwrap();
            assert_eq!(out.contents(), format!("a {stress}\n1 {stress}\n2 {stress}\n({}, {stress})\n", 3));
        }
    }
}
