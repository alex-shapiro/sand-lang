//! VM access and Sand calls for native functions: [`CallContext`] for a
//! synchronous native function, and [`NativeContext`] for the future of an
//! async native function.

use super::func::{NativeError, NativeReturn};
use super::ty::{Num, Param};
use crate::policy::Capability;
use crate::vm::bytecode::MAX_CALLBACK_ARGS;
use crate::vm::sched::ProgramCode;
use crate::vm::value::{Obj, VmValue};
use crate::vm::{Ctl, CtlKind, Fail, Fiber, SharedVm, Vm, WaitKind};
use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// VM and callback handle for a [`Later::new`](super::func::Later::new) future.
///
/// [`Later::new`](super::func::Later::new) passes a `NativeContext` to the
/// closure that builds the future. The engine runs that future on the calling
/// Sand task. During a wait, the scheduler can run other tasks. The garbage
/// collector can free any unrooted heap value during a wait.
///
/// [`NativeContext::call`] and [`NativeContext::step`] borrow `&mut self`. A
/// future can therefore await a single VM request at a time:
///
/// ```compile_fail,E0499
/// # use sandlang::native::context::{NativeContext, RootedValue};
/// async fn run_both(context: &mut NativeContext, sand_fn: RootedValue) {
///     let first = context.call(sand_fn, &[]);
///     let second = context.call(sand_fn, &[]);
///     first.await;
///     second.await;
/// }
/// ```
///
/// ```
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::func::Later;
/// # use sandlang::native::ty::{Param, VmVal};
/// # use sandlang::vm::{Fail, WaitKind};
/// let mut module = NativeModule::new("calls");
/// module.source("pub native fn twice<T>(f: fn(T) -> T, x: T) -> T");
/// module.function("twice", |sand_fn: VmVal<fn(Param<0>) -> Param<0>>, input: Param<0>| -> Result<Later<Param<0>, Fail>, Fail> {
///     Ok(Later::new(WaitKind::Io, |mut context| {
///         let (sand_fn, input) = (context.root(sand_fn.raw()), context.root(input.raw()));
///         async move {
///             let once = context.call(sand_fn, &[input]).await;
///             let twice = context.call(sand_fn, &[once]).await;
///             Ok(Param::from_raw(context.value(twice)))
///         }
///     }))
/// });
/// ```
pub struct NativeContext {
    state: Arc<Mutex<NativeCallState>>,
}

/// Handle to a heap value that a [`CallContext`] or a [`NativeContext`] keeps
/// alive until the native call returns.
///
/// Pass this handle to `value` on the context that created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootedValue(u32);

/// State shared between a [`NativeContext`] and the scheduler.
#[derive(Default)]
pub(crate) struct NativeCallState {
    /// Shared VM handle that the scheduler lends during each poll.
    pub lent_vm: Option<SharedVm>,
    /// Garbage-collection roots for [`RootedValue`] handles.
    pub roots: Vec<VmValue>,
    /// Pending scheduler request from the future.
    pub request: Option<VmRequest>,
    /// Rooted return value from the most recent [`VmRequest::Call`].
    pub callback_result: Option<RootedValue>,
}

impl NativeCallState {
    /// Roots `value` in this call. Returns its handle.
    pub fn root(&mut self, value: VmValue) -> RootedValue {
        self.roots.push(value);
        RootedValue((self.roots.len() - 1) as u32)
    }
}

/// Scheduler action requested by a native future before its next poll.
pub(crate) enum VmRequest {
    /// Call `callee` with `args` on the calling task.
    Call { callee: VmValue, args: Vec<VmValue> },
    /// Collect all garbage.
    Collect,
    /// Prompt the user for a capability.
    Prompt(Capability),
}

/// Pending async native call for a Sand task.
pub(crate) struct AsyncNativeCall {
    pub future: Pin<Box<dyn Future<Output = Result<VmValue, Fail>> + Send>>,
    pub state: Arc<Mutex<NativeCallState>>,
    pub wait_kind: WaitKind,
}

impl AsyncNativeCall {
    /// Builds an async native call that converts `build`'s future output into a
    /// [`VmValue`].
    pub fn new<F, R>(wait_kind: WaitKind, build: impl FnOnce(NativeContext) -> F) -> AsyncNativeCall
    where
        F: Future<Output = R> + Send + 'static,
        R: NativeReturn<Fail> + 'static,
    {
        let state: Arc<Mutex<NativeCallState>> = Arc::default();
        let output_future = build(NativeContext { state: state.clone() });
        let converter_context = NativeContext { state: state.clone() };
        let future = Box::pin(async move {
            let output = output_future.await;
            converter_context.with_vm(|vm| vm.unchecked(|vm| output.into_call_result(vm)))
        });
        AsyncNativeCall { future, state, wait_kind }
    }
}

impl NativeContext {
    /// Runs `f` with exclusive access to the [`Vm`].
    ///
    /// The garbage collector can invalidate any raw [`VmValue`] at the next
    /// wait. Pass a [`VmValue`] to [`NativeContext::root`] to keep it across a
    /// wait. Use [`NativeContext::step`] to allocate heap values or resources.
    /// If `f` reaches a memory or resource limit inside `step`, the engine
    /// collects garbage before running `f` again.
    ///
    /// # Panics
    ///
    /// Panics during a re-entrant `with_vm` call. Also panics outside an active
    /// poll of this call's future. Keep `NativeContext` on the thread that polls
    /// the future.
    pub fn with_vm<R>(&self, f: impl FnOnce(&mut Vm) -> R) -> R {
        let vm = self.state.lock().lent_vm.clone();
        let mut vm = vm
            .as_deref()
            .and_then(Mutex::try_lock)
            .expect("`NativeContext::with_vm` called outside a poll or inside an active `with_vm` closure");
        f(&mut vm)
    }

    /// Keeps `value` rooted until this native call returns. Returns its handle.
    pub fn root(&self, value: VmValue) -> RootedValue {
        self.state.lock().root(value)
    }

    /// Returns the [`VmValue`] for `rooted` from this call's root table.
    ///
    /// # Panics
    ///
    /// Panics after this native call returns. Also panics if `rooted` is out of
    /// bounds for this call's root table.
    pub fn value(&self, rooted: RootedValue) -> VmValue {
        self.state.lock().roots[rooted.0 as usize]
    }

    /// Calls the Sand function `callee` with `args` on the calling task.
    ///
    /// Returns the callback's return value as a [`RootedValue`]. During a
    /// `callee` wait, the scheduler can run other tasks. On task cancellation
    /// during `callee`, the engine runs `callee`'s `defer` blocks before
    /// dropping this native future. It then unwinds the caller. After the
    /// engine starts `callee`, it runs `callee` to completion before polling
    /// this future again.
    ///
    /// # Panics
    ///
    /// Panics if `args.len()` exceeds 255.
    pub async fn call(&mut self, callee: RootedValue, args: &[RootedValue]) -> RootedValue {
        assert!(args.len() <= MAX_CALLBACK_ARGS, "a callback takes at most {MAX_CALLBACK_ARGS} arguments");
        let request = {
            let state = self.state.lock();
            let read_root = |rooted: RootedValue| state.roots[rooted.0 as usize];
            VmRequest::Call { callee: read_root(callee), args: args.iter().copied().map(read_root).collect() }
        };
        self.wait_for_request(request).await;
        self.state.lock().callback_result.take().expect("the scheduler stores the callback result before polling again")
    }

    /// Runs `f` with `&mut Vm` under automatic retry for limit errors and
    /// permission requests.
    ///
    /// `f` must check limits and capabilities before any side effect. After a
    /// memory or resource limit error, the engine collects all garbage before
    /// running `f` again. After a permission request, the engine prompts the
    /// user before running `f` again.
    ///
    /// ```
    /// # use sandlang::native::context::NativeContext;
    /// # use sandlang::native::convert::HostError;
    /// # use sandlang::policy::Capability;
    /// # use sandlang::vm::Fail;
    /// async fn open(context: &mut NativeContext, capability: Capability) -> Result<(), HostError<Fail>> {
    ///     context.step(|vm| vm.require(&capability)).await?;
    ///     let _slot = context.step(|vm| vm.resource_slot()).await?;
    ///     Ok(())
    /// }
    /// ```
    pub async fn step<R, E: NativeError>(&mut self, mut f: impl FnMut(&mut Vm) -> Result<R, E>) -> Result<R, E::FutureError> {
        let mut collected = false;
        let result = loop {
            match self.with_vm(|vm| attempt_step(vm, collected, &mut f)) {
                StepAttempt::Done(result) => break result,
                StepAttempt::Collect => {
                    collected = true;
                    self.wait_for_request(VmRequest::Collect).await;
                }
                StepAttempt::Prompt(capability) => {
                    // Allow garbage collection again if the post-prompt retry hits a limit.
                    collected = false;
                    self.wait_for_request(VmRequest::Prompt(capability)).await;
                }
            }
        };
        self.with_vm(Vm::forget_once_answers);
        result
    }

    /// Builds a future that submits `request` to the scheduler.
    fn wait_for_request(&mut self, request: VmRequest) -> VmRequestFuture<'_> {
        VmRequestFuture { context: self, request: Some(request) }
    }
}

/// Future that yields a [`VmRequest`] to the scheduler for a single poll.
///
/// If the caller drops this future before the scheduler takes the request,
/// [`Drop`] clears the pending request.
struct VmRequestFuture<'a> {
    context: &'a mut NativeContext,
    request: Option<VmRequest>,
}

impl Future for VmRequestFuture<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, task_context: &mut Context<'_>) -> Poll<()> {
        let Some(request) = self.request.take() else {
            return Poll::Ready(());
        };
        let mut state = self.context.state.lock();
        assert!(state.lent_vm.is_some(), "poll `NativeContext` requests only while the engine polls this call");
        state.request = Some(request);
        drop(state);
        // Wake the task waker for outer combinators that track child readiness.
        task_context.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Drop for VmRequestFuture<'_> {
    fn drop(&mut self) {
        // `&mut NativeContext` guarantees a single active `VmRequestFuture` per call.
        self.context.state.lock().request = None;
    }
}

/// Outcome of a single run of the closure of a `step` call.
enum StepAttempt<R, E> {
    Done(Result<R, E>),
    /// Collect garbage and run the closure again.
    Collect,
    /// Prompt the user for the capability and run the closure again.
    Prompt(Capability),
}

/// Runs the closure `f` of a `step` call once. Set `collected` after a
/// garbage collection for the previous run.
fn attempt_step<R, E: NativeError>(
    vm: &mut Vm,
    collected: bool,
    f: &mut impl FnMut(&mut Vm) -> Result<R, E>,
) -> StepAttempt<R, E::FutureError> {
    vm.collected = collected;
    let result = f(vm);
    vm.collected = false;
    let ctl = match result.map_err(E::into_ctl) {
        Ok(value) => return StepAttempt::Done(Ok(value)),
        Err(Err(error)) => return StepAttempt::Done(Err(error)),
        Err(Ok(ctl)) => ctl,
    };
    if !collected && ctl.can_retry_after_collection() {
        return StepAttempt::Collect;
    }
    match ctl.into_kind() {
        CtlKind::NeedPermission(capability) => StepAttempt::Prompt(capability),
        CtlKind::Fail(fail) => StepAttempt::Done(Err(fail.into())),
        other => StepAttempt::Done(Err(Fail::internal(format!("unsupported scheduler request in a native `step`: {other:?}")).into())),
    }
}

/// VM and callback handle for a synchronous native function.
///
/// [`NativeModule::function`](super::NativeModule::function) passes a
/// `CallContext` to a function that takes `&mut CallContext` as its first
/// parameter. [`CallContext::call`] runs a Sand function on the Rust stack,
/// on the calling task. The engine runs such a native function only once. It
/// does not run it again after a permission prompt or a garbage collection.
///
/// ```
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::context::CallContext;
/// # use sandlang::native::ty::{Param, VmVal};
/// # use sandlang::vm::Fail;
/// let mut module = NativeModule::new("calls");
/// module.source("pub native fn twice<T>(f: fn(T) -> T, x: T) -> T");
/// module.function("twice", |context: &mut CallContext, sand_fn: VmVal<fn(Param<0>) -> Param<0>>, input: Param<0>| -> Result<Param<0>, Fail> {
///     let once = context.call(sand_fn.raw(), &[input.raw()])?;
///     Ok(Param::from_raw(context.call(sand_fn.raw(), &[once])?))
/// });
/// ```
pub struct CallContext<'a> {
    vm: &'a mut Vm,
    fiber: &'a mut Fiber,
    /// Length of `fiber.native_roots` at the start of the call.
    roots_start: usize,
    /// The VM code and the first stack slot of each callback frame. The first
    /// callback sets them, because they stay the same during the call.
    callbacks: Option<(ProgramCode, usize)>,
}

impl<'a> CallContext<'a> {
    pub(crate) fn new(vm: &'a mut Vm, fiber: &'a mut Fiber) -> CallContext<'a> {
        let roots_start = fiber.native_roots.len();
        CallContext { vm, fiber, roots_start, callbacks: None }
    }

    /// Returns the [`Vm`].
    ///
    /// The garbage collector can free any unrooted heap value during
    /// [`CallContext::call`] and [`CallContext::step`]. Pass a [`VmValue`] to
    /// [`CallContext::root`] to keep it across these calls. Check memory and
    /// resource limits inside [`CallContext::step`]: a limit error outside it
    /// ends the run, because the engine does not run the function again.
    pub fn vm(&mut self) -> &mut Vm {
        self.vm
    }

    /// Calls the Sand function `callee` with `args` on the calling task, and
    /// returns its result.
    ///
    /// `callee` runs until it returns. It cannot suspend: if it waits, prompts
    /// for a permission, or cancels its own task, the run ends with an error.
    /// Propagate the error with `?`. It holds the run ending of `callee`, with
    /// its stack trace.
    ///
    /// # Panics
    ///
    /// Panics if `args.len()` exceeds 255.
    #[inline]
    pub fn call(&mut self, callee: VmValue, args: &[VmValue]) -> Result<VmValue, Fail> {
        assert!(args.len() <= MAX_CALLBACK_ARGS, "a callback takes at most {MAX_CALLBACK_ARGS} arguments");
        let (vm, fiber) = (&mut *self.vm, &mut *self.fiber);
        let (code, base) = self.callbacks.get_or_insert_with(|| (vm.program_code(), fiber.live_top(&vm.prog)));
        vm.run_callback(code, fiber, *base, callee, args)
    }

    /// Keeps `value` alive until this native call returns. Returns its handle.
    pub fn root(&mut self, value: VmValue) -> RootedValue {
        let roots = &mut self.fiber.native_roots;
        let capacity = roots.capacity();
        roots.push(value);
        self.vm.heap.fiber_bytes += (roots.capacity() - capacity) * std::mem::size_of::<VmValue>();
        RootedValue((roots.len() - 1) as u32)
    }

    /// Returns the [`VmValue`] for `rooted`.
    ///
    /// # Panics
    ///
    /// Panics if `rooted` comes from a native call that returned.
    pub fn value(&self, rooted: RootedValue) -> VmValue {
        self.fiber.native_roots[rooted.0 as usize]
    }

    /// Runs `f` with `&mut Vm` under automatic retry for limit errors.
    ///
    /// `f` must check limits before any side effect. After a memory or
    /// resource limit error, the engine collects all garbage and runs `f`
    /// again. A permission request ends the run with an error, because the
    /// engine cannot prompt the user inside a synchronous native call.
    pub fn step<R, E: NativeError>(&mut self, mut f: impl FnMut(&mut Vm) -> Result<R, E>) -> Result<R, E::FutureError> {
        let mut collected = false;
        let result = loop {
            self.vm.heap.fail_next_check.set(self.vm.retry_stress && !collected);
            match attempt_step(self.vm, collected, &mut f) {
                StepAttempt::Done(result) => break result,
                StepAttempt::Collect => {
                    collected = true;
                    self.vm.collect_all_garbage(Some(self.fiber));
                }
                StepAttempt::Prompt(_) => {
                    break Err(Fail::from(self.vm.synchronous_call_error(self.fiber, "prompt for a permission", None)).into());
                }
            }
        };
        self.vm.heap.fail_next_check.set(false);
        self.vm.forget_once_answers();
        result
    }

    /// Converts the `output` of the native function into its Sand return value.
    ///
    /// Converts an error into a run ending, so that the engine does not run
    /// the native function again.
    pub(crate) fn finish(self, output: impl NativeReturn<Fail>) -> Result<VmValue, Ctl> {
        let CallContext { vm, fiber, roots_start, .. } = self;
        fiber.native_roots.truncate(roots_start);
        vm.unchecked(|vm| output.into_call_result(vm))
            .map_err(|fail| Fail::from(vm.run_ending_of(fail, |vm| vm.trace_of(fiber, None))).into())
    }
}

impl<const N: u8> Num<N> {
    /// Returns the additive identity (`0` or `0.0`) for type parameter `N`.
    pub fn zero(self, context: &mut CallContext) -> Result<Param<N>, Fail> {
        self.call_method(context, 0)
    }

    /// Returns the multiplicative identity (`1` or `1.0`) for type parameter `N`.
    pub fn one(self, context: &mut CallContext) -> Result<Param<N>, Fail> {
        self.call_method(context, 1)
    }

    /// Calls the native function in `slot` of the `Num` dictionary.
    fn call_method(self, context: &mut CallContext, slot: usize) -> Result<Param<N>, Fail> {
        let vm = &mut *context.vm;
        let method = match vm.heap.get(self.raw()) {
            Obj::Tuple(slots) => slots.get(slot).map(|&method| vm.heap.get(method)),
            _ => None,
        };
        let Some(&Obj::NativeFn(n)) = method else { return Err(Fail::internal("a native function expected `Num` evidence")) };
        let method = vm.natives[n as usize].f.clone();
        method.call(vm, context.fiber, 0).map(Param::from_raw).map_err(|ctl| Fail::internal(format!("`Num` evidence stopped with {ctl:?}")))
    }
}
