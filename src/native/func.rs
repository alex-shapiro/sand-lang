//! Traits and types for registering Rust functions in a [`NativeModule`](super::NativeModule).

use super::convert::{Conversion, Depth, FromValue, HostError, IntoValue};
use super::ty::{JournalArg, JournalData, SandType, Type};
use crate::vm::journal::Effects;
use crate::vm::value::Value;
use crate::vm::{AfterEffect, BoxFuture, Ctl, Fail, Finisher, NResult, NativeFn, Vm};
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
/// directly from the VM heap), and the return type implements [`VmReturn`].
#[diagnostic::on_unimplemented(
    note = "a function wrapped in `Journaled` or `Effectful` must take `JournalArg` types and return a `JournalData` type that fails only with `Fail`"
)]
pub trait HostFunction<Marker>: Send + Sync + 'static {
    /// The Rust parameter types, as a tuple.
    type Args;
    /// The Rust return type.
    type Ret: VmReturn;
    fn signature() -> Signature;
    /// Converts the function for registration under `key` (`module.name`).
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn>;
    /// Converts the function into the body of a journaled function
    /// ([`Journaled`] or [`Effectful`]) registered under `key`.
    #[doc(hidden)]
    fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn>
    where
        Self::Ret: VmReturn<Control = Fail>;
}

/// Synchronous function registered with [`NativeModule::vm_function`](super::NativeModule::vm_function).
///
/// Receives `&mut Vm` as its first argument, then optionally the [`Call`] or
/// the [`JournaledCall`] in progress. Remaining arguments must be owned
/// [`FromValue`] types or [`Val`](super::Val) handles.
#[diagnostic::on_unimplemented(
    note = "a function wrapped in `Journaled` or `Effectful` must take `JournalArg` types and return a `JournalData` type that fails only with `Fail`"
)]
pub trait VmFunction<Marker>: Send + Sync + 'static {
    /// The Rust parameter types after `&mut Vm` and the call parameter, as a tuple.
    type Args;
    /// The Rust return type.
    type Ret: VmReturn;
    /// The type of the call parameter after `&mut Vm`: [`Call`],
    /// [`JournaledCall`], or `()` if the function has no call parameter.
    type CallParam: CallParam;
    /// [`Parks`] if the function returns a [`Parked`] result, and
    /// [`NoPark`] otherwise.
    type Parking;
    fn signature() -> Signature;
    /// Converts the function for registration under `key` (`module.name`).
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn>;
    /// Converts the function into the body of a journaled function
    /// ([`Journaled`] or [`Effectful`]) registered under `key`.
    #[doc(hidden)]
    fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn>
    where
        Self::Ret: VmReturn<Control = Fail>;
}

/// The body of a journaled function ([`VmFunction::into_journaled`]). It
/// can only fail: a request to the scheduler, such as a yield, would let
/// the call end without a journal entry, so that replay diverges.
#[doc(hidden)]
pub type JournaledFn = dyn for<'c> Fn(&mut Vm, Call<'c>, &[Value]) -> Result<Answer, Fail> + Send + Sync;

/// What a native function gives back when it does not fail.
#[doc(hidden)]
pub enum Answer {
    Value(Value),
    /// The call waits for the future.
    Async(BoxFuture),
    /// The call waits for the future, and the engine may suspend the run
    /// while it waits, as the [`Park`] says. The tag goes to
    /// [`ParkedCall::tag`](crate::journal::ParkedCall::tag).
    Parked(BoxFuture, Park, String),
}

impl Answer {
    /// The result outside the journal. A parked call is then a plain wait:
    /// only a run with a journal suspends.
    pub(crate) fn into_result(self) -> NResult {
        match self {
            Answer::Value(v) => Ok(v),
            Answer::Async(fut) | Answer::Parked(fut, ..) => Err(Ctl::suspend(fut)),
        }
    }
}

/// The return type of a native function: an [`IntoValue`] type, a
/// `Result` whose error can end the call with a [`Ctl`], or the deferred
/// result of a [`Later`] or [`Parked`].
pub trait VmReturn: SandType {
    /// [`Parks`] for a [`Parked`] result, and [`NoPark`] otherwise.
    type Parking;
    /// What the function ends the call with when it does not give a result:
    /// [`Ctl`] if it can make a request to the scheduler, and [`Fail`] if it
    /// can only fail. Only a function that can only fail can be journaled.
    type Control: Into<Ctl> + From<Fail>;
    /// The answer of the call, converted after the call's effect.
    #[doc(hidden)]
    fn answer(self, vm: &mut Vm) -> Result<Answer, Self::Control>;
}

impl<R: IntoValue> VmReturn for R {
    type Parking = NoPark;
    type Control = Fail;
    fn answer(self, vm: &mut Vm) -> Result<Answer, Fail> {
        self.into_value(&mut AfterEffect::new(vm)).map(Answer::Value)
    }
}

impl<T: IntoValue> VmReturn for Result<T, Ctl> {
    type Parking = NoPark;
    type Control = Ctl;
    fn answer(self, vm: &mut Vm) -> Result<Answer, Ctl> {
        Ok(Answer::Value(self?.into_value(&mut AfterEffect::new(vm))?))
    }
}

impl<T: IntoValue> VmReturn for Result<T, HostError> {
    type Parking = NoPark;
    type Control = Ctl;
    fn answer(self, vm: &mut Vm) -> Result<Answer, Ctl> {
        match self {
            Ok(x) => {
                let fx = &mut AfterEffect::new(vm);
                let v = x.into_value(fx)?;
                Ok(Answer::Value(fx.ok(v)))
            }
            Err(e) => e.answer(vm),
        }
    }
}

impl<T: SandType, E: NativeError> VmReturn for Result<Later<T, E>, E> {
    type Parking = NoPark;
    type Control = E::Control;
    fn answer(self, vm: &mut Vm) -> Result<Answer, E::Control> {
        match self {
            Ok(later) => Ok(later.answer(vm)?),
            Err(e) => e.answer(vm),
        }
    }
}

impl<T: SandType, E: NativeError> VmReturn for Result<Parked<T, E>, E> {
    type Parking = Parks;
    type Control = E::Control;
    fn answer(self, vm: &mut Vm) -> Result<Answer, E::Control> {
        match self {
            Ok(Parked { later: Later(Wait::Future(fut), _), park: Some((park, tag)) }) => Ok(Answer::Parked(fut, park, tag)),
            Ok(Parked { later, .. }) => Ok(later.answer(vm)?),
            Err(e) => e.answer(vm),
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
    /// Converts the function into the body of a journaled function
    /// ([`Journaled`] or [`Effectful`]) registered under `key`.
    #[doc(hidden)]
    fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn>;
}

/// One call of a native function, as the engine makes it. A
/// [`VmFunction`] gets it when it takes a `Call` parameter after `&mut Vm`,
/// and gives it to the [`Vm`] operations that need it, such as
/// [`Vm::reserve_io`] and [`Vm::resource_slot`].
///
/// A `Call` lives only as long as the native invocation, so a function
/// cannot keep one for a later call:
///
/// ```compile_fail,E0521
/// # use sandlang::native::func::Call;
/// # use sandlang::vm::Vm;
/// static KEPT: std::sync::Mutex<Option<Call<'static>>> = std::sync::Mutex::new(None);
/// let mut m = sandlang::native::NativeModule::new("m");
/// m.vm_function("keep", |_: &mut Vm, call: Call| *KEPT.lock().unwrap() = Some(call));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Call<'a> {
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
    pub(crate) invocation: PhantomData<&'a mut ()>,
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
///
/// Like a [`Call`], a `JournaledCall` lives only as long as the native
/// invocation. A function cannot keep one and give its sequence number to
/// the effect of a later call:
///
/// ```compile_fail,E0521
/// # use sandlang::native::func::{Journaled, JournaledCall};
/// # use sandlang::vm::Vm;
/// static KEPT: std::sync::Mutex<Option<JournaledCall<'static>>> = std::sync::Mutex::new(None);
/// let mut m = sandlang::native::NativeModule::new("ids");
/// m.vm_function("keep", Journaled(|_: &mut Vm, call: JournaledCall| *KEPT.lock().unwrap() = Some(call)));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct JournaledCall<'a> {
    seq: Option<u64>,
    invocation: PhantomData<&'a mut ()>,
}

impl JournaledCall<'_> {
    /// In a journaled run, the sequence number of the journal entry that
    /// records the call. It is the same in every execution of the run, also
    /// when a resumed run calls the function again, so a host can use it as
    /// the identity of the call's effect (SPEC §16.7). `None` in a run
    /// without a journal.
    pub fn seq(self) -> Option<u64> {
        self.seq
    }
}

/// The type of the call parameter of a [`VmFunction`]: `()`, [`Call`], or
/// [`JournaledCall`].
pub trait CallParam: sealed::Sealed {}

impl CallParam for () {}
impl CallParam for Call<'static> {}
impl CallParam for JournaledCall<'static> {}

impl<'a> JournaledCall<'a> {
    fn of(call: Call<'a>) -> JournaledCall<'a> {
        JournaledCall { seq: call.seq, invocation: PhantomData }
    }
}

/// A call parameter that a function outside [`Journaled`] and
/// [`Effectful`] can take.
#[diagnostic::on_unimplemented(message = "a function that takes `JournaledCall` must be wrapped in `Journaled` or `Effectful`")]
pub trait LocalCallParam: CallParam {}

impl LocalCallParam for () {}
impl LocalCallParam for Call<'static> {}

/// A call parameter that a function in [`Journaled`] or [`Effectful`] can
/// take. A [`Call`] is not one: it lets a function read journal inputs.
#[diagnostic::on_unimplemented(message = "a function wrapped in `Journaled` or `Effectful` must take `JournaledCall`, not `Call`")]
pub trait JournaledCallParam: CallParam {}

impl JournaledCallParam for () {}
impl JournaledCallParam for JournaledCall<'static> {}

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
/// A function that can make a request to the scheduler does not compile: a
/// journaled function can only fail ([`VmReturn::Control`]). A yield, for
/// example, would end the call without a journal entry, and replay would then
/// find the entry of a later input where it expects the call's result:
///
/// ```compile_fail,E0277
/// # use sandlang::native::func::Journaled;
/// # use sandlang::vm::{Ctl, Vm};
/// let mut m = sandlang::native::NativeModule::new("ids");
/// m.vm_function("next", Journaled(|vm: &mut Vm| -> Result<i64, Ctl> { Err(vm.yield_now()) }));
/// ```
///
/// The function fails with a [`Fail`] or a `HostError<Fail>` in its place:
///
/// ```
/// # use sandlang::native::convert::HostError;
/// # use sandlang::native::func::Journaled;
/// # use sandlang::vm::{Fail, Vm};
/// let mut m = sandlang::native::NativeModule::new("ids");
/// m.vm_function("next", Journaled(|vm: &mut Vm| -> Result<i64, Fail> { vm.charge(1).map(|()| 1) }));
/// m.vm_function("parse", Journaled(|_: &mut Vm, s: String| -> Result<i64, HostError<Fail>> {
///     s.parse().map_err(|_| HostError::new("invalid", "not a number"))
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
    F::Ret: JournalData + VmReturn<Control = Fail>,
{
    type Args = F::Args;
    type Ret = F::Ret;
    fn signature() -> Signature {
        F::signature()
    }
    fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
        journaled(self.0.into_journaled(key), key, Effects::Pure)
    }
    fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn> {
        self.0.into_journaled(key)
    }
}

macro_rules! journaled_impls {
    ($($wrapper:ident: $effects:expr $(, $ret:path)?);*) => {$(
        impl<M, F: VmFunction<M, CallParam: JournaledCallParam>> VmFunction<M> for $wrapper<F>
        where
            F::Args: JournalArgs,
            F::Ret: JournalData + VmReturn<Control = Fail> $(+ $ret)?,
        {
            type Args = F::Args;
            type Ret = F::Ret;
            type CallParam = ();
            type Parking = NoPark;
            fn signature() -> Signature {
                F::signature()
            }
            fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
                journaled(self.0.into_journaled(key), key, $effects)
            }
            fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn> {
                self.0.into_journaled(key)
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
                journaled(self.0.into_journaled(key), key, $effects)
            }
            fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn> {
                self.0.into_journaled(key)
            }
        }
    )*};
}
journaled_impls!(Journaled: Effects::Pure; Effectful: Effects::External, Deferred);

/// Wraps `inner`, registered under `key`, so that its calls go through the
/// journal.
fn journaled(inner: Box<JournaledFn>, key: &Arc<str>, effects: Effects) -> Box<NativeFn> {
    let key = key.clone();
    Box::new(move |vm: &mut Vm, call: Call<'_>, args: &[Value]| crate::vm::journal::journaled_call(vm, call, &*inner, &key, effects, args))
}

/// Error type for a [`Later`] result: [`Ctl`] or [`HostError`] for a
/// function that can make a request to the scheduler, and [`Fail`] or
/// `HostError<Fail>` for one that can only fail, such as a journaled one.
pub trait NativeError: Send + Sized + 'static + sealed::Sealed {
    /// The error of the closure that finishes the call: this error without
    /// its requests, since the closure runs after the effect.
    type Finish: Send + 'static;
    /// What the error ends the call with ([`VmReturn::Control`]).
    type Control: Into<Ctl> + From<Fail>;
    fn result_type(t: Type) -> Type;
    /// The Sand result of the finished call.
    #[doc(hidden)]
    fn finish<T: IntoValue>(r: Result<T, Self::Finish>, fx: &mut AfterEffect<'_>) -> Result<Value, Fail>;
    /// The answer of a call that gives the error before it waits.
    #[doc(hidden)]
    fn answer(self, vm: &mut Vm) -> Result<Answer, Self::Control>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::vm::Ctl {}
    impl Sealed for crate::vm::Fail {}
    impl<C> Sealed for super::HostError<C> {}
    impl Sealed for () {}
    impl Sealed for super::Call<'static> {}
    impl Sealed for super::JournaledCall<'static> {}
}

impl NativeError for Ctl {
    type Finish = Fail;
    type Control = Ctl;
    fn result_type(t: Type) -> Type {
        t
    }
    fn finish<T: IntoValue>(r: Result<T, Fail>, fx: &mut AfterEffect<'_>) -> Result<Value, Fail> {
        r.into_value(fx)
    }
    fn answer(self, _: &mut Vm) -> Result<Answer, Ctl> {
        Err(self)
    }
}

impl NativeError for Fail {
    type Finish = Fail;
    type Control = Fail;
    fn result_type(t: Type) -> Type {
        t
    }
    fn finish<T: IntoValue>(r: Result<T, Fail>, fx: &mut AfterEffect<'_>) -> Result<Value, Fail> {
        r.into_value(fx)
    }
    fn answer(self, _: &mut Vm) -> Result<Answer, Fail> {
        Err(self)
    }
}

impl<C: Send + 'static> NativeError for HostError<C>
where
    C: Into<Ctl> + From<Fail>,
{
    type Finish = HostError<Fail>;
    type Control = C;
    fn result_type(t: Type) -> Type {
        Type::result(t, Type::error())
    }
    fn finish<T: IntoValue>(r: Result<T, HostError<Fail>>, fx: &mut AfterEffect<'_>) -> Result<Value, Fail> {
        r.into_value(fx)
    }
    fn answer(self, vm: &mut Vm) -> Result<Answer, C> {
        self.into_sand(&mut AfterEffect::new(vm), Depth::default())?.map(Answer::Value)
    }
}

/// Deferred result returned by a [`VmFunction`] that may suspend the calling
/// Sand task on a future.
///
/// Return `Result<Later<T, E>, E>` from the function: if `E` is [`Ctl`] or
/// [`Fail`], the Sand return type is `T`; if `E` is a [`HostError`], the Sand
/// return type is `Result<T, Error>`. The closure that finishes the call
/// returns `Result<T, E::Finish>` ([`NativeError::Finish`]).
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

impl<T: IntoValue + Send + 'static, E: NativeError> Later<T, E> {
    /// Suspends the calling task until `fut` is resolved, and then executes the
    /// returned closure on the VM thread to produce the Sand result. The
    /// closure runs after the effect, so it sees the VM as an
    /// [`AfterEffect`], and it can only fail: its error is `E::Finish`. A
    /// closure that asks for a permission does not compile:
    ///
    /// ```compile_fail,E0271
    /// # use sandlang::native::func::Later;
    /// # use sandlang::policy::Capability;
    /// # use sandlang::vm::{AfterEffect, Ctl};
    /// let _: Later<i64, Ctl> = Later::new(async { |_: &mut AfterEffect| -> Result<i64, Ctl> { Err(Ctl::need_permission(Capability::Stdin)) } });
    /// ```
    ///
    /// Do not capture [`Value`]s in `fut`; the garbage collector does not trace
    /// pending futures while a task is suspended.
    pub fn new<F>(fut: impl Future<Output = F> + Send + 'static) -> Later<T, E>
    where
        F: FnOnce(&mut AfterEffect<'_>) -> Result<T, E::Finish> + Send + 'static,
    {
        let fut = async move {
            let finish = fut.await;
            Box::new(move |fx: &mut AfterEffect<'_>| E::finish(finish(fx), fx)) as Finisher
        };
        Later(Wait::Future(Box::pin(fut)), PhantomData)
    }

    /// Completes immediately with `r` without suspending the calling task.
    pub fn ready(r: Result<T, E::Finish>) -> Later<T, E> {
        Later(Wait::Ready(Box::new(move |fx: &mut AfterEffect<'_>| E::finish(r, fx))), PhantomData)
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
    /// # use sandlang::vm::{AfterEffect, Fail, Vm};
    /// let mut m = sandlang::native::NativeModule::new("signals");
    /// m.vm_function("wait", |_: &mut Vm| -> Result<Parked<i64, HostError<Fail>>, HostError<Fail>> {
    ///     Ok(Later::new(async { |_: &mut AfterEffect| Ok::<_, HostError<Fail>>(1) }).parked(Park::UntilResumed, "go"))
    /// });
    /// ```
    pub fn parked(self, park: Park, tag: impl Into<String>) -> Parked<T, E> {
        Parked { later: self, park: Some((park, tag.into())) }
    }

    /// The answer of the call: its result if it is ready, or else a wait for
    /// the future.
    fn answer(self, vm: &mut Vm) -> Result<Answer, Fail> {
        match self.0 {
            Wait::Ready(f) => f(&mut AfterEffect::new(vm)).map(Answer::Value),
            Wait::Future(fut) => Ok(Answer::Async(fut)),
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

/// Converts the arguments of a [`HostFunction`] and calls it.
trait Answers<Marker> {
    type Ret: VmReturn;
    fn answer(&self, vm: &mut Vm, args: &[Value]) -> Result<Answer, <Self::Ret as VmReturn>::Control>;
}

#[cold]
fn too_few_args() -> Fail {
    Fail::internal("native function called with too few arguments")
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
            R: VmReturn<Parking = NoPark>,
            $($a: FromValue + 'static,)*
        {
            type Args = ($($a,)*);
            type Ret = R;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, _: &Arc<str>) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, _: Call<'_>, args: &[Value]| -> NResult {
                    Answers::<($($a,)*)>::answer(&self, vm, args).map_err(Into::<Ctl>::into)?.into_result()
                })
            }
            fn into_journaled(self, _: &Arc<str>) -> Box<JournaledFn>
            where
                R: VmReturn<Control = Fail>,
            {
                Box::new(move |vm: &mut Vm, _: Call<'_>, args: &[Value]| -> Result<Answer, Fail> { Answers::<($($a,)*)>::answer(&self, vm, args) })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> Answers<($($a,)*)> for F
        where
            F: Fn($($a),*) -> R + for<'a> Fn($($a::Item<'a>),*) -> R + Send + Sync + 'static,
            R: VmReturn<Parking = NoPark>,
            $($a: FromValue + 'static,)*
        {
            type Ret = R;
            fn answer(&self, vm: &mut Vm, args: &[Value]) -> Result<Answer, R::Control> {
                fn call<R, $($a,)*>(f: &impl Fn($($a),*) -> R, $($a: $a,)*) -> R {
                    f($($a),*)
                }
                let r = {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args().into()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    call(self, $($a),*)
                };
                r.answer(vm)
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
                VmFunction::<(WithCall<Call<'static>>, $($a,)*)>::into_host(move |vm: &mut Vm, _: Call<'_>, $($a: $a),*| self(vm, $($a),*), key)
            }
            fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn>
            where
                R: VmReturn<Control = Fail>,
            {
                VmFunction::<(WithCall<Call<'static>>, $($a,)*)>::into_journaled(move |vm: &mut Vm, _: Call<'_>, $($a: $a),*| self(vm, $($a),*), key)
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> VmFunction<(WithCall<JournaledCall<'static>>, $($a,)*)> for F
        where
            F: for<'c> Fn(&mut Vm, JournaledCall<'c>, $($a),*) -> R + Send + Sync + 'static,
            R: VmReturn,
            $($a: for<'a> FromValue<Item<'a> = $a> + 'static,)*
        {
            type Args = ($($a,)*);
            type Ret = R;
            type CallParam = JournaledCall<'static>;
            type Parking = R::Parking;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
                VmFunction::<(WithCall<Call<'static>>, $($a,)*)>::into_host(
                    move |vm: &mut Vm, call: Call<'_>, $($a: $a),*| self(vm, JournaledCall::of(call), $($a),*),
                    key,
                )
            }
            fn into_journaled(self, key: &Arc<str>) -> Box<JournaledFn>
            where
                R: VmReturn<Control = Fail>,
            {
                VmFunction::<(WithCall<Call<'static>>, $($a,)*)>::into_journaled(
                    move |vm: &mut Vm, call: Call<'_>, $($a: $a),*| self(vm, JournaledCall::of(call), $($a),*),
                    key,
                )
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut, clippy::unused_unit, clippy::too_many_arguments)]
        impl<F, R, $($a,)*> VmFunction<(WithCall<Call<'static>>, $($a,)*)> for F
        where
            F: for<'c> Fn(&mut Vm, Call<'c>, $($a),*) -> R + Send + Sync + 'static,
            R: VmReturn,
            $($a: for<'a> FromValue<Item<'a> = $a> + 'static,)*
        {
            type Args = ($($a,)*);
            type Ret = R;
            type CallParam = Call<'static>;
            type Parking = R::Parking;
            fn signature() -> Signature {
                Signature { params: vec![$($a::sand_type()),*], ret: R::sand_type() }
            }
            fn into_host(self, _: &Arc<str>) -> Box<NativeFn> {
                Box::new(move |vm: &mut Vm, call: Call<'_>, args: &[Value]| -> NResult {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args().into()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    vm.arm_retry_stress(call);
                    self(vm, call, $($a),*).answer(vm).map_err(Into::<Ctl>::into)?.into_result()
                })
            }
            fn into_journaled(self, _: &Arc<str>) -> Box<JournaledFn>
            where
                R: VmReturn<Control = Fail>,
            {
                Box::new(move |vm: &mut Vm, call: Call<'_>, args: &[Value]| -> Result<Answer, Fail> {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    vm.arm_retry_stress(call);
                    self(vm, call, $($a),*).answer(vm)
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
            fn into_host(self, key: &Arc<str>) -> Box<NativeFn> {
                let body = AsyncHostFunction::into_journaled(self, key);
                Box::new(move |vm: &mut Vm, call: Call<'_>, args: &[Value]| -> NResult { body(vm, call, args)?.into_result() })
            }
            fn into_journaled(self, _: &Arc<str>) -> Box<JournaledFn> {
                Box::new(move |vm: &mut Vm, _: Call<'_>, args: &[Value]| -> Result<Answer, Fail> {
                    let &[$($a),*] = strip_dict_args(args, $n) else { return Err(too_few_args()) };
                    let mut conv = Conversion::new();
                    $(let $a = $a::from_value(vm, $a, &mut conv)?;)*
                    let fut = self($($a),*);
                    Ok(Answer::Async(Box::pin(async move {
                        let r = fut.await;
                        Box::new(move |fx: &mut AfterEffect<'_>| r.into_value(fx)) as Finisher
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
