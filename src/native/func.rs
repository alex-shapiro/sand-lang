//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::convert::{Conversion, FromValue, HostError, IntoValue};
use super::ty::{JournalArg, JournalData, SandType, Type};
use crate::vm::value::Value;
use crate::vm::{BoxFuture, Ctl, Finisher, HostFn, NResult, NativeFn, Vm};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

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
/// Parameters implement [`FromValue`] (borrowing `&str`, `&[u8]`, and `&T`
/// directly from the VM heap), and the return type implements [`IntoValue`].
#[diagnostic::on_unimplemented(
    note = "a function wrapped in `Journaled` or `Effectful` must take `JournalArg` types and return a `JournalData` type"
)]
pub trait HostFunction<Marker>: Send + Sync + 'static {
    /// The Rust parameter types, as a tuple.
    type Args;
    /// The Rust return type.
    type Ret;
    fn signature() -> Signature;
    /// Converts the function for registration under `key` (`module.name`).
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn>;
}

/// Synchronous function registered with [`NativeModule::vm_function`](super::NativeModule::vm_function).
///
/// Receives `&mut Vm` as its first argument, then optionally the [`Call`] or
/// the [`JournaledCall`] in progress. Remaining arguments must be owned
/// [`FromValue`] types or [`Val`](super::Val) handles.
#[diagnostic::on_unimplemented(
    note = "a function wrapped in `Journaled` or `Effectful` must take `JournalArg` types and return a `JournalData` type"
)]
pub trait VmFunction<Marker>: Send + Sync + 'static {
    /// The Rust parameter types after `&mut Vm` and the call parameter, as a tuple.
    type Args;
    /// The Rust return type.
    type Ret;
    /// The type of the call parameter after `&mut Vm`: [`Call`],
    /// [`JournaledCall`], or `()` if the function has no call parameter.
    type CallParam: CallParam;
    /// [`Parks`] if the function returns a [`Parked`] result, and
    /// [`NoPark`] otherwise.
    type Parking;
    fn signature() -> Signature;
    /// Converts the function for registration under `key` (`module.name`).
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn>;
}

/// The return type of a [`VmFunction`]: an [`IntoValue`] type, or the
/// [`Parked`] result of a journaled function.
pub trait VmReturn: SandType {
    /// [`Parks`] for a [`Parked`] result, and [`NoPark`] otherwise.
    type Parking;
    #[doc(hidden)]
    fn into_result(self, vm: &mut Vm) -> NResult;
}

impl<R: IntoValue> VmReturn for R {
    type Parking = NoPark;
    fn into_result(self, vm: &mut Vm) -> NResult {
        self.into_value(vm)
    }
}

impl<T: SandType, E: NativeError> VmReturn for Result<Parked<T, E>, E> {
    type Parking = Parks;
    fn into_result(self, vm: &mut Vm) -> NResult {
        match self {
            Ok(Parked { later: Later(Wait::Future(fut), _), park: Some((park, tag)) }) => {
                Err(Ctl::from(crate::vm::CtlKind::Parked(fut, park, tag)))
            }
            Ok(Parked { later, .. }) => later.finish(vm),
            Err(e) => E::finish(Err::<(), E>(e), vm),
        }
    }
}

/// Marks a [`VmFunction`] that returns a [`Parked`] result.
pub struct Parks;

/// Marks a [`VmFunction`] that does not return a [`Parked`] result.
pub struct NoPark;

/// The [`VmFunction::Parking`] of a function outside [`Journaled`] and
/// [`Effectful`]: only a journaled call can park.
#[diagnostic::on_unimplemented(message = "a function that returns `Parked` must be wrapped in `Journaled` or `Effectful`")]
pub trait LocalParking {}

impl LocalParking for NoPark {}

/// Asynchronous function registered with [`NativeModule::async_function`](super::NativeModule::async_function).
///
/// Arguments must be owned [`FromValue`] types so the returned future can
/// outlive the initial call.
#[diagnostic::on_unimplemented(
    note = "a function wrapped in `Journaled` or `Effectful` must take `JournalArg` types and return a `JournalData` type"
)]
pub trait AsyncHostFunction<Marker>: Send + Sync + 'static {
    /// The Rust parameter types, as a tuple.
    type Args;
    /// The output type of the future.
    type Ret;
    fn signature() -> Signature;
    /// Converts the function for registration under `key` (`module.name`).
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn>;
}

/// One call of a native function, as the engine makes it. A
/// [`VmFunction`] gets it when it takes a `Call` parameter after `&mut Vm`,
/// and gives it to the [`Vm`] operations that need it, such as
/// [`Vm::reserve_io`] and [`Vm::resource_slot`].
#[derive(Clone, Copy, Debug)]
pub struct Call {
    /// The function and instruction pointer of the script frame that calls
    /// the native.
    pub(crate) site: (u32, u32),
    /// Whether the engine calls the native again after the collection that
    /// a limit check of the first attempt asked for. A limit check that
    /// fails in this attempt is final.
    pub(crate) retry: bool,
    /// The sequence number of the journal entry that records the call, for
    /// a journaled call in a journaled run (see [`JournaledCall::seq`]).
    pub(crate) seq: Option<u64>,
}

/// The [`Call`] of a function registered as [`Journaled`] or [`Effectful`].
/// It gives no [`Call`], so the function cannot read journal inputs such as
/// [`Vm::wall_time`]: replay does not call the function, and thus would not
/// read them. Only a function in one of these wrappers can take it:
///
/// ```compile_fail
/// # use sandlang::native::func::JournaledCall;
/// # use sandlang::vm::Vm;
/// let mut m = sandlang::native::NativeModule::new("ids");
/// m.vm_function("next", |_: &mut Vm, call: JournaledCall| call.seq().unwrap_or(0) as i64);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct JournaledCall(Call);

impl JournaledCall {
    /// In a journaled run, the sequence number of the journal entry that
    /// records the call. It is the same in every execution of the run, also
    /// when a resumed run calls the function again, so a host can use it as
    /// the identity of the call's effect (SPEC §16.7). `None` in a run
    /// without a journal.
    pub fn seq(self) -> Option<u64> {
        self.0.seq
    }
}

/// The type of the call parameter of a [`VmFunction`].
pub trait CallParam: Copy + sealed::Sealed {
    #[doc(hidden)]
    fn from_call(call: Call) -> Self;
}

impl CallParam for () {
    fn from_call(_: Call) {}
}

impl CallParam for Call {
    fn from_call(call: Call) -> Call {
        call
    }
}

impl CallParam for JournaledCall {
    fn from_call(call: Call) -> JournaledCall {
        JournaledCall(call)
    }
}

/// A call parameter that a function outside [`Journaled`] and
/// [`Effectful`] can take.
#[diagnostic::on_unimplemented(message = "a function that takes `JournaledCall` must be wrapped in `Journaled` or `Effectful`")]
pub trait LocalCallParam: CallParam {}

impl LocalCallParam for () {}
impl LocalCallParam for Call {}

/// A call parameter that a function in [`Journaled`] or [`Effectful`] can
/// take. A [`Call`] is not one: it lets a function read journal inputs.
#[diagnostic::on_unimplemented(message = "a function wrapped in `Journaled` or `Effectful` must take `JournaledCall`, not `Call`")]
pub trait JournaledCallParam: CallParam {}

impl JournaledCallParam for () {}
impl JournaledCallParam for JournaledCall {}

/// Marks the [`VmFunction`] implementation for functions that take a call
/// parameter of type `C`.
#[doc(hidden)]
pub struct WithCall<C>(PhantomData<C>);

/// A tuple of [`JournalArg`] types: the parameters of a journaled function.
pub trait JournalArgs {}

/// Registers host function `F` as journaled (SPEC §16.7): in a run with a
/// journal, the engine records the function's result, and replay returns
/// the recorded result without a call. Wrap the function in it when you
/// register it with [`NativeModule::function`](super::NativeModule::function),
/// [`NativeModule::vm_function`](super::NativeModule::vm_function), or
/// [`NativeModule::async_function`](super::NativeModule::async_function).
/// A function that is not journaled is local: replay calls it again, so it
/// must give the same result in every execution.
///
/// The journal encodes the arguments, which identify the call, and the
/// result. Thus the parameters must be [`JournalArg`] types and the result
/// a [`JournalData`] type:
///
/// ```
/// use sandlang::native::func::Journaled;
/// let mut m = sandlang::native::NativeModule::new("ids");
/// m.function("next", Journaled(|prefix: String| format!("{prefix}-1")));
/// ```
///
/// A function that returns a bound object does not compile:
///
/// ```compile_fail
/// # use sandlang::native::func::Journaled;
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "files")]
/// struct File;
/// impl sandlang::native::NativeObject for File {}
///
/// let mut m = sandlang::native::NativeModule::new("files");
/// m.bind::<File>();
/// m.function("open", Journaled(|| File));
/// ```
///
/// A function that returns a value of a bound value type does not compile:
///
/// ```compile_fail
/// # use sandlang::native::func::Journaled;
/// let mut m = sandlang::native::NativeModule::new("clock");
/// m.function("now", Journaled(|| jiff::Timestamp::UNIX_EPOCH));
/// ```
///
/// A function that takes a bound object does not compile:
///
/// ```compile_fail
/// # use sandlang::native::func::Journaled;
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "files")]
/// struct File(u32);
/// impl sandlang::native::NativeObject for File {}
///
/// let mut m = sandlang::native::NativeModule::new("files");
/// m.bind::<File>();
/// m.function("read", Journaled(|f: &File| i64::from(f.0)));
/// ```
///
/// A function that takes a [`Call`] does not compile. The function would
/// be able to read journal inputs, such as [`Vm::wall_time`], which replay
/// does not read because it does not call the function:
///
/// ```compile_fail,E0277
/// # use sandlang::native::func::{Call, Journaled};
/// # use sandlang::vm::{Ctl, Vm};
/// let mut m = sandlang::native::NativeModule::new("clock");
/// m.vm_function("now", Journaled(|vm: &mut Vm, call: Call| -> Result<i64, Ctl> {
///     Ok(vm.wall_time(call)?.duration_since(sandlang::platform::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
/// }));
/// ```
///
/// A function that takes a function does not compile:
///
/// ```compile_fail
/// # use sandlang::native::func::Journaled;
/// # use sandlang::native::ty::Val;
/// # use sandlang::vm::Vm;
/// let mut m = sandlang::native::NativeModule::new("calls");
/// m.vm_function("apply", Journaled(|_: &mut Vm, f: Val<fn() -> i64>| 0i64));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Journaled<F>(pub F);

/// Registers host function `F` as journaled, like [`Journaled`], and as
/// having effects outside the run. The function starts its effect in the
/// future of the [`Later`] that it returns, or is asynchronous. The engine
/// records the call's start, and starts the future only once the journal
/// is durable ([`Journal::flush`](crate::journal::Journal::flush)). Thus a
/// resumed run that calls the function again gives it the same
/// [`JournaledCall::seq`]. A [`Later::ready`] result has no effect.
///
/// A function that returns its result at once does not compile:
///
/// ```compile_fail
/// # use sandlang::native::func::Effectful;
/// # use sandlang::vm::Vm;
/// let mut m = sandlang::native::NativeModule::new("payments");
/// m.vm_function("charge", Effectful(|_: &mut Vm, cents: i64| cents));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Effectful<F>(pub F);

/// The result of a [`VmFunction`] whose effect starts in a future: the
/// return type of a function in [`Effectful`].
#[diagnostic::on_unimplemented(
    message = "a function wrapped in `Effectful` must return `Result<Later<T, E>, E>` or `Result<Parked<T, E>, E>`",
    note = "start the effect in the future of the `Later`"
)]
pub trait Deferred {}

impl<T, E> Deferred for Result<Later<T, E>, E> {}

impl<T, E> Deferred for Result<Parked<T, E>, E> {}

impl<M, F: HostFunction<M>> HostFunction<M> for Journaled<F>
where
    F::Args: JournalArgs,
    F::Ret: JournalData,
{
    type Args = F::Args;
    type Ret = F::Ret;
    fn signature() -> Signature {
        F::signature()
    }
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
        journaled(self.0.into_host(key), key, false)
    }
}

macro_rules! journaled_impls {
    ($($wrapper:ident: $effectful:literal $(, $ret:path)?);*) => {$(
        impl<M, F: VmFunction<M, CallParam: JournaledCallParam>> VmFunction<M> for $wrapper<F>
        where
            F::Args: JournalArgs,
            F::Ret: JournalData $(+ $ret)?,
        {
            type Args = F::Args;
            type Ret = F::Ret;
            type CallParam = ();
            type Parking = NoPark;
            fn signature() -> Signature {
                F::signature()
            }
            fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
                journaled(self.0.into_host(key), key, $effectful)
            }
        }

        impl<M, F: AsyncHostFunction<M>> AsyncHostFunction<M> for $wrapper<F>
        where
            F::Args: JournalArgs,
            F::Ret: JournalData,
        {
            type Args = F::Args;
            type Ret = F::Ret;
            fn signature() -> Signature {
                F::signature()
            }
            fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
                journaled(self.0.into_host(key), key, $effectful)
            }
        }
    )*};
}
journaled_impls!(Journaled: false; Effectful: true, Deferred);

/// Wraps `inner`, registered under `key`, so that its calls go through the
/// journal.
fn journaled(inner: Box<NativeFn>, key: &Arc<str>, effectful: bool) -> Box<NativeFn> {
    let inner = HostFn::Shared(inner.into());
    let key = key.clone();
    Box::new(move |vm: &mut Vm, call: Call, args: &[Value]| crate::vm::journal::journaled_call(vm, call, &inner, &key, effectful, args))
}

/// Deferred result returned by a [`VmFunction`] that may suspend the calling
/// Sand task on a future.
///
/// Return `Result<Later<T, E>, E>` from the function: if `E` is [`Ctl`], the
/// Sand return type is `T`; if `E` is [`HostError`], the Sand return type is
/// `Result<T, Error>`.
pub struct Later<T, E>(Wait, PhantomData<fn() -> (T, E)>);

/// How long the engine may suspend a journaled run while a host call waits
/// (see [`Later::parked`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Park {
    /// The engine may suspend the run until the host resumes it, for
    /// example when a signal that the call waits for arrives.
    UntilResumed,
    /// The engine may suspend the run until this time, when the host
    /// resumes it, or earlier if the host resumes it earlier.
    Until(crate::platform::SystemTime),
}

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
    impl Sealed for () {}
    impl Sealed for super::Call {}
    impl Sealed for super::JournaledCall {}
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
    /// Suspends the calling task until `fut` is resolved, and then executes the
    /// returned closure on the VM thread to produce the Sand result.
    ///
    /// Do not capture [`Value`]s in `fut`; the garbage collector does not trace
    /// pending futures while a task is suspended.
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
    /// Lets the engine suspend a journaled run while the future is pending
    /// (SPEC §16.7): if every task waits, and no operation is due within
    /// the configured threshold, the run ends with
    /// [`Outcome::Suspended`](crate::engine::Outcome::Suspended), whose
    /// [`Suspension::parked`](crate::journal::Suspension::parked) lists the
    /// call with `tag`. The resumed run calls the function again. Without
    /// a journal, the future runs as usual.
    ///
    /// Only a function in [`Journaled`] or [`Effectful`] can return the
    /// result. A function that is not journaled does not compile:
    ///
    /// ```compile_fail
    /// # use sandlang::native::convert::HostError;
    /// # use sandlang::native::func::{Later, Park, Parked};
    /// # use sandlang::vm::Vm;
    /// let mut m = sandlang::native::NativeModule::new("signals");
    /// m.vm_function("wait", |_: &mut Vm| -> Result<Parked<i64, HostError>, HostError> {
    ///     Ok(Later::new(async { |_: &mut Vm| Ok::<_, HostError>(1) }).parked(Park::UntilResumed, "go"))
    /// });
    /// ```
    pub fn parked(self, park: Park, tag: impl Into<String>) -> Parked<T, E> {
        Parked { later: self, park: Some((park, tag.into())) }
    }

    pub(crate) fn finish(self, vm: &mut Vm) -> NResult {
        match self.0 {
            Wait::Ready(f) => f(vm),
            Wait::Future(fut) => Err(Ctl::suspend(fut)),
        }
    }
}

/// A [`Later`] whose pending future lets the engine suspend a journaled run
/// ([`Later::parked`]). Return `Result<Parked<T, E>, E>` from a function in
/// [`Journaled`] or [`Effectful`], as for a `Later`.
pub struct Parked<T, E> {
    later: Later<T, E>,
    /// How long the run may be suspended, and the tag of the call. `None`
    /// for a `Later` that does not park.
    park: Option<(Park, String)>,
}

/// A `Later` that does not park, for a journaled function that parks only
/// sometimes.
impl<T, E> From<Later<T, E>> for Parked<T, E> {
    fn from(later: Later<T, E>) -> Parked<T, E> {
        Parked { later, park: None }
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
        impl<$($a: JournalArg),*> JournalArgs for ($($a,)*) {}

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> HostFunction<($($a,)*)> for F
        where
            F: Fn($($a),*) -> R + for<'a> Fn($($a::Item<'a>),*) -> R + Send + Sync + 'static,
            R: IntoValue,
            $($a: FromValue + 'static,)*
        {
            type Args = ($($a,)*);
            type Ret = R;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, _: &Arc<str>) -> Box<NativeFn> {
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                Box::new(move |vm: &mut Vm, _: Call, args: &[Value]| -> NResult {
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
            R: VmReturn,
            $($a: for<'a> FromValue<Item<'a> = $a> + 'static,)*
        {
            type Args = ($($a,)*);
            type Ret = R;
            type CallParam = ();
            type Parking = R::Parking;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
                VmFunction::<(WithCall<()>, $($a,)*)>::into_host(move |vm: &mut Vm, _: (), $($a: $a),*| self(vm, $($a),*), key)
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, P, $($a,)*> VmFunction<(WithCall<P>, $($a,)*)> for F
        where
            F: Fn(&mut Vm, P, $($a),*) -> R + Send + Sync + 'static,
            R: VmReturn,
            P: CallParam,
            $($a: for<'a> FromValue<Item<'a> = $a> + 'static,)*
        {
            type Args = ($($a,)*);
            type Ret = R;
            type CallParam = P;
            type Parking = R::Parking;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, _: &Arc<str>) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, call: Call, args: &[Value]| -> NResult {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    vm.arm_retry_stress(call);
                    let r = self(vm, P::from_call(call), $($a),*);
                    vm.unchecked(|vm| r.into_result(vm))
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
            type Args = ($($a,)*);
            type Ret = R;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, _: &Arc<str>) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, _: Call, args: &[Value]| -> NResult {
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
