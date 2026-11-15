//! VM access for the future of an async native call.

use super::func::{NativeError, NativeReturn};
use crate::engine::LimitKind;
use crate::policy::Capability;
use crate::vm::bytecode::MAX_CALLBACK_ARGS;
use crate::vm::value::VmValue;
use crate::vm::{CtlKind, Fail, SharedVm, Vm, WaitKind};
use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Handle that lets the future of an async native call use the VM, call Sand
/// functions, and keep heap values alive across waits.
///
/// [`Later::new`](super::func::Later::new) passes a `NativeContext` to the
/// function that builds the future. The future runs on the calling Sand
/// task. While the future waits on a Sand call or on host I/O, the scheduler
/// runs other tasks, and garbage collection can free any heap value that the
/// call has not rooted.
///
/// The methods that make a VM request take `&mut self`, so a future waits on
/// at most one request at a time:
///
/// ```compile_fail,E0499
/// # use sandlang::native::context::{NativeContext, RootedValue};
/// async fn both(cx: &mut NativeContext, f: RootedValue) {
///     let first = cx.call(f, &[]);
///     let second = cx.call(f, &[]);
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
/// module.function("twice", |f: VmVal<fn(Param<0>) -> Param<0>>, x: Param<0>| -> Result<Later<Param<0>, Fail>, Fail> {
///     Ok(Later::new(WaitKind::Io, |mut cx| {
///         let (f, x) = (cx.root(f.raw()), cx.root(x.raw()));
///         async move {
///             let once = cx.call(f, &[x]).await;
///             let twice = cx.call(f, &[once]).await;
///             Ok(Param::from_raw(cx.value(twice)))
///         }
///     }))
/// });
/// ```
pub struct NativeContext {
    call: Arc<Mutex<CallState>>,
}

/// Heap value that a native call keeps alive across waits. Read it with
/// [`NativeContext::value`] on the context that rooted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootedValue(u32);

/// State that a native call shares with the scheduler.
#[derive(Default)]
pub(crate) struct CallState {
    /// The VM, while the scheduler polls the call's future.
    pub lent_vm: Option<SharedVm>,
    /// Garbage collection roots, indexed by [`RootedValue`].
    pub roots: Vec<VmValue>,
    /// The request that the last poll made.
    pub request: Option<VmRequest>,
    /// The result of the last [`VmRequest::Call`].
    pub reply: Option<RootedValue>,
}

impl CallState {
    /// Roots `v` and returns its handle.
    pub fn root(&mut self, v: VmValue) -> RootedValue {
        self.roots.push(v);
        RootedValue((self.roots.len() - 1) as u32)
    }
}

/// Work that a native call's future asks the scheduler to do before the next poll.
pub(crate) enum VmRequest {
    /// Run `callee` with `args` on the calling task.
    Call { callee: VmValue, args: Vec<VmValue> },
    /// Collect all garbage.
    Collect,
    /// Ask the user for permission.
    Prompt(Capability),
}

/// The future of an async native call, and the state it shares with the
/// scheduler.
pub(crate) struct NativeStart {
    pub future: Pin<Box<dyn Future<Output = Result<VmValue, Fail>> + Send>>,
    pub call: Arc<Mutex<CallState>>,
    pub wait_kind: WaitKind,
}

impl NativeStart {
    /// Builds the future of a native call with the context of the call. The
    /// future converts its output into the Sand result.
    pub fn new<F, R>(wait_kind: WaitKind, build: impl FnOnce(NativeContext) -> F) -> NativeStart
    where
        F: Future<Output = R> + Send + 'static,
        R: NativeReturn<Fail> + 'static,
    {
        let call: Arc<Mutex<CallState>> = Arc::default();
        let output = build(NativeContext { call: call.clone() });
        let converter = NativeContext { call: call.clone() };
        let future = Box::pin(async move {
            let r = output.await;
            converter.with_vm(|vm| vm.unchecked(|vm| r.into_call_result(vm)))
        });
        NativeStart { future, call, wait_kind }
    }
}

impl NativeContext {
    /// Runs `f` with the VM.
    ///
    /// A raw [`VmValue`] from the VM is valid until the next wait. Root it
    /// with [`NativeContext::root`] to keep it across a wait.
    ///
    /// # Panics
    ///
    /// Panics if the engine is not polling the call's future, for example
    /// after the call returns, during another call, or in a `Drop`
    /// implementation. Do not send the context to another thread.
    pub fn with_vm<R>(&self, f: impl FnOnce(&mut Vm) -> R) -> R {
        let vm = self.call.lock().lent_vm.clone();
        let mut vm = vm
            .as_deref()
            .and_then(Mutex::try_lock)
            .expect("`NativeContext::with_vm` runs only while the engine polls the call, and not inside another `with_vm`");
        f(&mut vm)
    }

    /// Keeps `v` alive until the native call returns.
    pub fn root(&self, v: VmValue) -> RootedValue {
        self.call.lock().root(v)
    }

    /// Returns the rooted value `r`, which the call of this context rooted.
    ///
    /// # Panics
    ///
    /// Panics if the call has returned.
    pub fn value(&self, r: RootedValue) -> VmValue {
        self.call.lock().roots[r.0 as usize]
    }

    /// Calls the Sand function `callee` with `args` on the calling task, and
    /// returns its rooted result.
    ///
    /// The scheduler can suspend the callee on async I/O and run other tasks.
    /// If the task is cancelled, the engine drops the future during the call.
    /// The callee takes at most 255 arguments. A timer or `select!` branch
    /// around the call cannot interrupt the callee.
    pub async fn call(&mut self, callee: RootedValue, args: &[RootedValue]) -> RootedValue {
        assert!(args.len() <= MAX_CALLBACK_ARGS, "a callback takes at most {MAX_CALLBACK_ARGS} arguments");
        let request = {
            let call = self.call.lock();
            let value = |r: RootedValue| call.roots[r.0 as usize];
            VmRequest::Call { callee: value(callee), args: args.iter().copied().map(value).collect() }
        };
        self.request(request).await;
        self.call.lock().reply.take().expect("the scheduler returns the callee's result")
    }

    /// Runs `f` with the VM, like [`NativeContext::with_vm`], under the restart
    /// rule of a function that takes `&mut Vm`. If `f` fails on a memory or
    /// resource limit, the engine collects all garbage and runs `f` again. If
    /// `f` needs a permission prompt, the engine asks the user and runs `f`
    /// again. `f` must check limits and capabilities before any side effect.
    ///
    /// ```
    /// # use sandlang::native::context::NativeContext;
    /// # use sandlang::native::convert::HostError;
    /// # use sandlang::policy::Capability;
    /// # use sandlang::vm::Fail;
    /// async fn open(cx: &mut NativeContext, capability: Capability) -> Result<(), HostError<Fail>> {
    ///     cx.step(|vm| vm.require(&capability)).await?;
    ///     let _slot = cx.step(|vm| vm.resource_slot()).await?;
    ///     Ok(())
    /// }
    /// ```
    pub async fn step<R, E: NativeError>(&mut self, mut f: impl FnMut(&mut Vm) -> Result<R, E>) -> Result<R, E::FutureError> {
        let mut collected = false;
        let result = loop {
            let result = self.with_vm(|vm| {
                vm.collected = collected;
                let result = f(vm);
                vm.collected = false;
                result
            });
            let ctl = match result.map_err(E::into_ctl) {
                Ok(value) => break Ok(value),
                Err(Err(error)) => break Err(error),
                Err(Ok(ctl)) => ctl,
            };
            let memory_limit = matches!(ctl.kind(), CtlKind::Fail(fail) if fail.as_limit() == Some(LimitKind::Memory));
            if !collected && (memory_limit || matches!(ctl.kind(), CtlKind::Collect)) {
                collected = true;
                self.request(VmRequest::Collect).await;
                continue;
            }
            match ctl.into_kind() {
                CtlKind::NeedPermission(cap) => self.request(VmRequest::Prompt(cap)).await,
                CtlKind::Fail(fail) => break Err(fail.into()),
                other => break Err(Fail::internal(format!("a native step made the scheduler request {other:?}")).into()),
            }
        };
        self.with_vm(Vm::forget_once_answers);
        result
    }

    /// Stores `request` for the scheduler and waits until the scheduler polls
    /// the future again.
    fn request(&mut self, request: VmRequest) -> PendingRequest<'_> {
        PendingRequest { cx: self, request: Some(request), stored: false }
    }
}

/// A VM request that the scheduler handles before it polls the future again.
/// Dropping it before the scheduler takes it, for example in a `select!`
/// branch that loses, withdraws it.
struct PendingRequest<'a> {
    cx: &'a mut NativeContext,
    request: Option<VmRequest>,
    stored: bool,
}

impl Future for PendingRequest<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, w: &mut Context<'_>) -> Poll<()> {
        let Some(request) = self.request.take() else {
            self.stored = false;
            return Poll::Ready(());
        };
        let mut call = self.cx.call.lock();
        assert!(call.lent_vm.is_some(), "`NativeContext` requests run only while the engine polls the call");
        call.request = Some(request);
        drop(call);
        self.stored = true;
        // A combinator that polls its children with its own waker polls this
        // future again only after a wake.
        w.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        if self.stored {
            self.cx.call.lock().request = None;
        }
    }
}
