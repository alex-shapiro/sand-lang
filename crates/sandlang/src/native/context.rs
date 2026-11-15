//! VM access for the future of an async native call.

use super::func::{NativeError, NativeReturn};
use crate::policy::Capability;
use crate::vm::bytecode::MAX_CALLBACK_ARGS;
use crate::vm::value::VmValue;
use crate::vm::{CtlKind, Fail, SharedVm, Vm, WaitKind};
use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Context for accessing the VM, rooting heap values, and calling Sand
/// functions inside [`Later::new`](super::func::Later::new).
///
/// [`Later::new`](super::func::Later::new) passes a `NativeContext` to the
/// closure that builds the future. The engine runs that future on the calling
/// Sand task. While the future awaits, the scheduler can run other tasks. The
/// garbage collector can free any unrooted heap value during a wait.
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

/// Handle to a heap value rooted in a [`NativeContext`] across waits.
///
/// Pass this handle to [`NativeContext::value`] on the context that created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootedValue(u32);

/// State shared between a [`NativeContext`] and the scheduler.
#[derive(Default)]
pub(crate) struct NativeCallState {
    /// Shared VM handle lent while the scheduler polls the call's future.
    pub lent_vm: Option<SharedVm>,
    /// Garbage-collection roots indexed by [`RootedValue`].
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

/// Boxed future and shared state for an async native call.
pub(crate) struct AsyncNativeCall {
    pub future: Pin<Box<dyn Future<Output = Result<VmValue, Fail>> + Send>>,
    pub state: Arc<Mutex<NativeCallState>>,
    pub wait_kind: WaitKind,
}

impl AsyncNativeCall {
    /// Wraps the future from `build` in an async native call that converts its
    /// output on the VM.
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
    /// collects garbage and runs `f` again.
    ///
    /// # Panics
    ///
    /// Panics if called outside a poll of this call's future or inside an
    /// active `with_vm` closure. Keep `NativeContext` on the thread that polls
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
    /// Panics if `rooted` is out of bounds, including after the engine finishes
    /// this native call.
    pub fn value(&self, rooted: RootedValue) -> VmValue {
        self.state.lock().roots[rooted.0 as usize]
    }

    /// Calls the Sand function `callee` with `args` on the calling task.
    ///
    /// Returns the callback's return value as a [`RootedValue`]. While `callee`
    /// waits, the scheduler can run other tasks. If the calling task is
    /// canceled during `callee`, the engine runs `callee`'s `defer` blocks,
    /// drops this native future, and unwinds the caller. After the engine
    /// starts `callee`, it runs `callee` to completion before polling this
    /// future again.
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

    /// Runs `f` with `&mut Vm` and retries after garbage collection or
    /// permission prompts.
    ///
    /// `f` must check limits and capabilities before any side effect. After a
    /// memory or resource limit error, the engine collects all garbage and runs
    /// `f` again. After a permission prompt, the engine asks the user and runs
    /// `f` again.
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
            let step_result = self.with_vm(|vm| {
                vm.collected = collected;
                let step_result = f(vm);
                vm.collected = false;
                step_result
            });
            let ctl = match step_result.map_err(E::into_ctl) {
                Ok(value) => break Ok(value),
                Err(Err(error)) => break Err(error),
                Err(Ok(ctl)) => ctl,
            };
            if !collected && ctl.can_retry_after_collection() {
                collected = true;
                self.wait_for_request(VmRequest::Collect).await;
                continue;
            }
            match ctl.into_kind() {
                CtlKind::NeedPermission(capability) => {
                    // Allow garbage collection again if the post-prompt retry hits a limit.
                    collected = false;
                    self.wait_for_request(VmRequest::Prompt(capability)).await;
                }
                CtlKind::Fail(fail) => break Err(fail.into()),
                other => break Err(Fail::internal(format!("unsupported scheduler request in `NativeContext::step`: {other:?}")).into()),
            }
        };
        self.with_vm(Vm::forget_once_answers);
        result
    }

    /// Submits `request` to the scheduler and waits for the next poll.
    fn wait_for_request(&mut self, request: VmRequest) -> VmRequestFuture<'_> {
        VmRequestFuture { context: self, request: Some(request) }
    }
}

/// Future that submits a [`VmRequest`] and waits for the next scheduler poll.
///
/// If a losing `select!` branch drops this future before the scheduler takes
/// the request, [`Drop`] clears the pending request.
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
        // Combinators such as `FuturesUnordered` skip a child until the child wakes its waker.
        // Mark this child ready for the scheduler's next poll of the outer future.
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
