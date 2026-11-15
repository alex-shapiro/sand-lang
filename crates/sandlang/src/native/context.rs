//! VM access for the future of an async native call.

use super::convert::HostError;
use super::func::NativeReturn;
use crate::policy::Capability;
use crate::vm::bytecode::MAX_CALLBACK_ARGS;
use crate::vm::value::VmValue;
use crate::vm::{Fail, SharedVm, Vm, WaitKind};
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
/// ```
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::func::Later;
/// # use sandlang::native::ty::{Param, VmVal};
/// # use sandlang::vm::{Fail, WaitKind};
/// let mut module = NativeModule::new("calls");
/// module.source("pub native fn twice<T>(f: fn(T) -> T, x: T) -> T");
/// module.function("twice", |f: VmVal<fn(Param<0>) -> Param<0>>, x: Param<0>| -> Result<Later<Param<0>, Fail>, Fail> {
///     Ok(Later::new(WaitKind::Io, |cx| {
///         let (f, x) = (cx.root(f.raw()), cx.root(x.raw()));
///         async move {
///             let once = cx.call(f, &[x]).await;
///             let twice = cx.call(f, &[once]).await;
///             Ok(Param::from_raw(cx.value(twice)))
///         }
///     }))
/// });
/// ```
#[derive(Clone)]
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
    /// The request that the last poll made, with its id.
    pub request: Option<(u64, VmRequest)>,
    /// The number of requests that the call made.
    pub requests_made: u64,
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

pub(crate) type NativeFuture = Pin<Box<dyn Future<Output = Result<VmValue, Fail>> + Send>>;

/// An async native call that the scheduler has not started yet.
pub(crate) struct NativeStart {
    pub future: NativeFuture,
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
        let cx = NativeContext { call: Arc::default() };
        let call = cx.call.clone();
        let output = build(cx.clone());
        let future = Box::pin(async move {
            let r = output.await;
            cx.with_vm(|vm| vm.unchecked(|vm| r.into_call_result(vm)))
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
        let mut vm = vm.as_deref().and_then(Mutex::try_lock).expect("`NativeContext::with_vm` runs only while the engine polls the call");
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
    /// The callee takes at most 255 arguments.
    pub async fn call(&self, callee: RootedValue, args: &[RootedValue]) -> RootedValue {
        assert!(args.len() <= MAX_CALLBACK_ARGS, "a callback takes at most {MAX_CALLBACK_ARGS} arguments");
        let request = {
            let call = self.call.lock();
            let value = |r: RootedValue| call.roots[r.0 as usize];
            VmRequest::Call { callee: value(callee), args: args.iter().copied().map(value).collect() }
        };
        self.request(request).await;
        self.call.lock().reply.take().expect("the scheduler returns the callee's result")
    }

    /// Returns a memory-limit error if `extra` additional heap bytes exceed
    /// the limit after garbage collection. The async form of
    /// [`Vm::check_memory`]: it does not restart the native call.
    pub async fn check_memory(&self, extra: usize) -> Result<(), Fail> {
        if self.with_vm(|vm| vm.check_memory(extra)).is_ok() {
            return Ok(());
        }
        self.request(VmRequest::Collect).await;
        self.with_vm(|vm| vm.check_memory(extra))
    }

    /// Checks `capability` like [`Vm::require`], and asks the user if the
    /// policy requires a prompt. Returns a `"permission_denied"` error if the
    /// policy or the user denies it.
    pub async fn require(&self, capability: &Capability) -> Result<(), HostError<Fail>> {
        loop {
            let permit = self.with_vm(|vm| {
                let permit = vm.permit(capability);
                if permit.is_ok() {
                    vm.forget_once_answers();
                }
                permit
            });
            match permit {
                Ok(allowed) => {
                    return if allowed {
                        Ok(())
                    } else {
                        Err(HostError::new("permission_denied", format!("permission denied: {capability}")))
                    };
                }
                Err(_) => self.request(VmRequest::Prompt(capability.clone())).await,
            }
        }
    }

    /// Stores `request` for the scheduler and waits until the scheduler polls
    /// the future again.
    fn request(&self, request: VmRequest) -> PendingRequest<'_> {
        PendingRequest { cx: self, request: Some(request), id: None }
    }
}

/// A VM request that the scheduler handles before it polls the future again.
/// Dropping it before the scheduler takes it, for example in a `select!`
/// branch that loses, withdraws it.
struct PendingRequest<'a> {
    cx: &'a NativeContext,
    request: Option<VmRequest>,
    /// The id of the stored request.
    id: Option<u64>,
}

impl Future for PendingRequest<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        let Some(request) = self.request.take() else {
            self.id = None;
            return Poll::Ready(());
        };
        let mut call = self.cx.call.lock();
        assert!(call.lent_vm.is_some(), "`NativeContext` requests run only while the engine polls the call");
        assert!(call.request.is_none(), "a native call made two VM requests in one poll");
        call.requests_made += 1;
        let id = call.requests_made;
        call.request = Some((id, request));
        drop(call);
        self.id = Some(id);
        Poll::Pending
    }
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        let mut call = self.cx.call.lock();
        if self.id.is_some() && call.request.as_ref().map(|(id, _)| *id) == self.id {
            call.request = None;
        }
    }
}
