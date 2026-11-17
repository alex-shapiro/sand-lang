//! Round-robin fiber scheduler for bytecode slices and async native calls.

use super::interp::FiberStop;
use super::*;
use crate::engine::LimitKind;
use crate::native::context::VmRequest;
use crate::platform::{ClockFuture, Instant};
use parking_lot::MutexGuard;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// Host-event wake queue and wait counters for an engine or session.
#[derive(Default)]
pub struct HostWakeState {
    /// Fibers that a [`FiberWaker`] queued since the last `Vm::wake_host_waits` pass.
    woken_fibers: Mutex<Vec<u32>>,
    /// Active host-event wait count for each [`WaitKind`].
    host_wait_counts: [AtomicUsize; 2],
    waker: Mutex<Option<Waker>>,
}

impl HostWakeState {
    /// Wakes the host task driving the engine.
    pub fn wake(&self) {
        if let Some(w) = self.waker.lock().as_ref() {
            w.wake_by_ref();
        }
    }

    /// Stores `waker` to wake the engine task on host events and interrupts.
    pub(crate) fn set_waker(&self, waker: &Waker) {
        let mut slot = self.waker.lock();
        if !slot.as_ref().is_some_and(|current| current.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }
}

/// Waker for a fiber's active host-event wait.
///
/// On a stale wake-up, the scheduler polls the fiber's active native call at
/// most once.
pub(crate) struct FiberWaker {
    fiber_id: u32,
    /// True if `HostWakeState::woken_fibers` contains `fiber_id`.
    is_queued: AtomicBool,
    wake_state: Arc<HostWakeState>,
}

impl Wake for FiberWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.is_queued.swap(true, Ordering::Relaxed) {
            self.wake_state.woken_fibers.lock().push(self.fiber_id);
            self.wake_state.wake();
        }
    }
}

/// Drop guard that counts an active host-event wait in [`HostWakeState`].
struct HostWaitGuard {
    kind: WaitKind,
    wake_state: Arc<HostWakeState>,
}

impl HostWaitGuard {
    fn new(kind: WaitKind, wake_state: Arc<HostWakeState>) -> HostWaitGuard {
        wake_state.host_wait_counts[kind as usize].fetch_add(1, Ordering::Relaxed);
        HostWaitGuard { kind, wake_state }
    }
}

impl Drop for HostWaitGuard {
    fn drop(&mut self) {
        self.wake_state.host_wait_counts[self.kind as usize].fetch_sub(1, Ordering::Relaxed);
    }
}

/// Active async native call on a [`Fiber`].
pub(crate) struct ActiveNativeCall {
    call: AsyncNativeCall,
    /// Bytecode frame count at the start of this call. `frames[frame_depth - 1]` is the caller.
    frame_depth: usize,
    /// Fiber stack slot where the scheduler writes this call's return value.
    result_slot: usize,
    /// Bytes charged to `Heap::fiber_bytes` for the future and its root table.
    pub tracked_bytes: usize,
    /// Guard for an active host-event wait.
    host_wait: Option<HostWaitGuard>,
}

impl ActiveNativeCall {
    /// Appends this call's rooted values to `roots`.
    pub fn trace(&self, roots: &mut Vec<VmValue>) {
        roots.extend_from_slice(&self.call.state.lock().roots);
    }
}

impl Fiber {
    /// Returns the innermost async native call if its caller is the top bytecode frame.
    fn top_native_call(&self) -> Option<&ActiveNativeCall> {
        self.native_calls.last().filter(|active_call| active_call.frame_depth == self.frames.len())
    }
}

/// Maximum number of Sand callbacks that synchronous native calls nest on the
/// Rust stack. Nested callbacks use at most half of a 512 KiB thread stack.
pub(crate) const MAX_NESTED_CALLBACKS: usize = (256 << 10) / NESTED_CALLBACK_STACK_BYTES;

/// Rust stack bytes for each nested callback, measured in a release build.
/// The test profile uses 2,320 bytes.
const NESTED_CALLBACK_STACK_BYTES: usize = 2368;

/// The program and native functions that [`Vm::interp`] runs.
pub(crate) type Code = (Arc<Program>, Arc<Vec<NativeEntry>>);

/// Maximum interpreter slices between clock readings.
const CLOCK_SLICES: u32 = 32;

/// Slice-loop time budget before yielding to the host executor.
const YIELD_EVERY: Duration = Duration::from_millis(2);

/// Stopping policy for [`drive`]. Each policy defines its own
/// [`Stop`](Self::Stop) type for the stops it produces.
pub(crate) trait DriveUntil {
    type Stop;

    /// Returns a stop if finishing fiber `fid` with `result` ends the drive.
    fn stop_on_finish(&self, fid: u32, result: VmValue) -> Option<Self::Stop>;

    /// Returns a stop if a permission prompt for `capability` ends the drive.
    /// Returns `None` if the engine should prompt immediately.
    fn stop_on_prompt(&self, capability: &Capability) -> Option<Self::Stop>;

    /// Returns the outcome if all tasks are blocked and all I/O is complete.
    fn stop_on_all_blocked(&self, vm: &Vm) -> Result<Self::Stop, RunEnding>;

    /// Returns a stop if pending I/O ends the drive instead of waiting.
    fn stop_before_io_wait(&self) -> Option<Self::Stop>;
}

/// Drives tasks until the main fiber finishes.
pub(crate) struct UntilMainDone(u32);

impl DriveUntil for UntilMainDone {
    type Stop = VmValue;

    fn stop_on_finish(&self, fid: u32, result: VmValue) -> Option<VmValue> {
        (fid == self.0).then_some(result)
    }

    fn stop_on_prompt(&self, _capability: &Capability) -> Option<VmValue> {
        None
    }

    fn stop_on_all_blocked(&self, vm: &Vm) -> Result<VmValue, RunEnding> {
        let trace = match vm.fibers.get(self.0 as usize) {
            Some(Some(f)) => vm.trace_of(f, None),
            _ => vec![],
        };
        Err(Trap::Error { message: "deadlock: all tasks are blocked".into(), trace }.into())
    }

    fn stop_before_io_wait(&self) -> Option<VmValue> {
        None
    }
}

/// Why a drive with [`UntilIdle`] stopped.
pub(crate) enum IdleStop {
    /// All tasks are finished or blocked.
    Idle,
    /// A task needs a permission prompt.
    Prompt(Capability),
}

/// Drives tasks until all tasks are finished or blocked, or a task needs a
/// permission prompt.
pub(crate) struct UntilIdle {
    /// Whether the drive waits for pending I/O before stopping.
    pub waits_for_io: bool,
}

impl DriveUntil for UntilIdle {
    type Stop = IdleStop;

    fn stop_on_finish(&self, _fid: u32, _result: VmValue) -> Option<IdleStop> {
        None
    }

    fn stop_on_prompt(&self, capability: &Capability) -> Option<IdleStop> {
        Some(IdleStop::Prompt(capability.clone()))
    }

    fn stop_on_all_blocked(&self, _vm: &Vm) -> Result<IdleStop, RunEnding> {
        Ok(IdleStop::Idle)
    }

    fn stop_before_io_wait(&self) -> Option<IdleStop> {
        (!self.waits_for_io).then_some(IdleStop::Idle)
    }
}

/// Execution timer tracking elapsed CPU and clock time.
struct RunTimer {
    clock: Arc<dyn crate::platform::Clock>,
    cpu: Duration,
    cpu_mark: Duration,
    now: Instant,
    last_yield: Instant,
    unrecorded_slices: u32,
    unrecorded_fuel: u64,
}

impl RunTimer {
    fn start(clock: Arc<dyn crate::platform::Clock>) -> RunTimer {
        let now = clock.now();
        let cpu_mark = clock.cpu_time();
        RunTimer { clock, cpu: Duration::ZERO, cpu_mark, now, last_yield: now, unrecorded_slices: 0, unrecorded_fuel: 0 }
    }

    /// Updates CPU and monotonic times, and reports completed work.
    fn read(&mut self) {
        self.clock.work(self.unrecorded_fuel);
        let mark = self.clock.cpu_time();
        self.cpu += mark.saturating_sub(self.cpu_mark);
        self.cpu_mark = mark;
        self.now = self.clock.now();
        self.unrecorded_slices = 0;
        self.unrecorded_fuel = 0;
    }

    /// Resumes time tracking after an asynchronous wait.
    fn resume(&mut self) {
        self.cpu_mark = self.clock.cpu_time();
        self.now = self.clock.now();
        self.unrecorded_slices = 0;
        self.unrecorded_fuel = 0;
    }

    /// Adds `fuel` to the unrecorded batch. Reads the clock once the slice count or fuel reaches its threshold.
    fn ran(&mut self, fuel: u64) {
        self.unrecorded_slices += 1;
        self.unrecorded_fuel += fuel;
        if self.unrecorded_slices >= CLOCK_SLICES || self.unrecorded_fuel >= SLICE_FUEL as u64 {
            self.read();
        }
    }

    /// Returns the CPU time deadline for `max`.
    fn cpu_deadline(&self, max: Option<Duration>) -> Option<Duration> {
        max.map(|m| self.cpu_mark.saturating_add(m.saturating_sub(self.cpu)))
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }

    fn yield_now(&self) -> ClockFuture {
        self.clock.yield_now()
    }

    fn sleep(&self, duration: Duration) -> ClockFuture {
        self.clock.sleep(duration)
    }
}

impl Drop for RunTimer {
    fn drop(&mut self) {
        self.clock.work(self.unrecorded_fuel);
    }
}

impl Vm {
    /// Excludes the time elapsed since `wait_start` from `max_wall_time` by advancing `self.deadline`.
    fn exclude_wall_time(&mut self, wait_start: Instant, timer: &RunTimer) {
        if let Some(deadline) = &mut self.deadline {
            *deadline += timer.now().saturating_duration_since(wait_start);
        }
    }

    fn check_limits(&self, timer: &RunTimer) -> Result<(), Trap> {
        if self.host.interrupt.is_set() {
            return Err(Trap::Interrupted);
        }
        let l = &self.host.limits;
        if let Some(max) = l.max_instructions
            && self.meter.used() > max
        {
            return Err(Trap::Limit(LimitKind::Instructions));
        }
        if let Some(max) = l.max_cpu_time
            && timer.cpu > max
        {
            return Err(Trap::Limit(LimitKind::CpuTime));
        }
        if self.deadline.is_some_and(|d| timer.now() >= d) {
            return Err(Trap::Limit(LimitKind::WallTime));
        }
        Ok(())
    }

    fn start_main(&mut self, proto: u32) -> Result<u32, RunEnding> {
        let closure = self.heap.alloc(Obj::Closure(proto, Box::new([])));
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: 0, state: TaskState::Running, result: VmValue::Unit })));
        let fid = self.spawn_fiber(closure, task).map_err(|fail| self.run_ending_of(fail, |_| vec![]))?;
        if let Obj::Task(task_obj) = self.heap.get_mut(task) {
            task_obj.fiber = fid;
        }
        self.main_fiber = fid;
        self.live_tasks += 1;
        Ok(fid)
    }

    /// Runs `fiber` until it stops or spends `fuel`.
    ///
    /// On a memory-limit error, `run_slice` collects all garbage and restores
    /// the instruction's fuel. It then retries the instruction in the same
    /// slice.
    fn run_slice(&mut self, code: &Code, fiber: &mut Fiber, fuel: &mut i64) -> Result<FiberStop, RunEnding> {
        let mut last_retry_site = None;
        loop {
            let entry_fuel = *fuel;
            let stop = self.interp(&code.0, &code.1, fiber, fuel);
            let Ok(FiberStop::CollectAndRetry(frame)) = stop else { return stop };
            let tick = super::interp::call_tick(self.prog.protos[frame.proto as usize].code[frame.ip as usize]);
            // Reaching this instruction again crosses a back edge or call that spends fuel.
            // Equal fuel here means the retry itself exceeded `max_memory`.
            let retry_site = (fiber.frames.len(), frame.ip);
            if last_retry_site == Some(retry_site) && entry_fuel - *fuel <= tick {
                return Err(Trap::Limit(LimitKind::Memory).into());
            }
            last_retry_site = Some(retry_site);
            *fuel += tick;
            self.collect_all_garbage(Some(fiber));
            self.instruction_retried = self.retry_stress;
            if let Err(fail) = self.meter.check() {
                return Err(self.run_ending_of(fail, |vm| vm.trace_of(fiber, None)));
            }
        }
    }

    /// Pushes `call` onto `fiber.native_calls`.
    fn push_native_call(&mut self, fiber: &mut Fiber, call: AsyncNativeCall) {
        let old_capacity = fiber.native_calls.capacity();
        // `run_fiber_turn` calls `poll_native_call` next to compute `tracked_bytes`.
        fiber.native_calls.push(ActiveNativeCall {
            call,
            frame_depth: fiber.frames.len(),
            result_slot: fiber.result_slot,
            tracked_bytes: 0,
            host_wait: None,
        });
        self.heap.fiber_bytes += (fiber.native_calls.capacity() - old_capacity) * std::mem::size_of::<ActiveNativeCall>();
    }

    /// Pops the innermost async native call from `fiber`. Clears its root table.
    fn pop_native_call(&mut self, fiber: &mut Fiber) {
        if let Some(active_call) = fiber.native_calls.pop() {
            active_call.call.state.lock().roots = Vec::new();
            self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(active_call.tracked_bytes);
        }
    }

    /// Drops async native calls started by `fiber.frames[from_frame..]` from innermost to outermost.
    pub(crate) fn drop_native_calls(&mut self, fiber: &mut Fiber, from_frame: usize) {
        while fiber.native_calls.last().is_some_and(|active_call| active_call.frame_depth > from_frame) {
            self.pop_native_call(fiber);
        }
    }

    /// Pushes a [`CALLBACK_PROTO`] frame on `fiber` to call `callee` with `args`.
    fn begin_callback(&mut self, fiber: &mut Fiber, base: usize, callee: VmValue, args: &[VmValue]) -> Result<(), RunEnding> {
        if fiber.frames.len() >= self.host.limits.max_call_depth {
            return Err(self.stack_overflow(fiber));
        }
        let need = base + 1 + MAX_CALLBACK_ARGS;
        let grow = |vm: &mut Vm, fiber: &mut Fiber| -> Result<(), Fail> {
            if fiber.stack.len() < need {
                fiber.grow_stack(need, &mut vm.heap)?;
            }
            if fiber.frames.len() == fiber.frames.capacity() {
                fiber.grow_frames(&mut vm.heap)?;
            }
            Ok(())
        };
        let mut grown = grow(self, fiber);
        if grown.as_ref().is_err_and(|fail| fail.as_limit() == Some(LimitKind::Memory)) {
            self.collect_all_garbage(Some(fiber));
            grown = grow(self, fiber);
        }
        if let Err(fail) = grown {
            return Err(self.run_ending_of(fail, |vm| vm.trace_of(fiber, None)));
        }
        fiber.stack[base] = callee;
        // A loop with bounds checks copies a few arguments faster than `memcpy`.
        for (i, &arg) in args.iter().enumerate() {
            fiber.stack[base + 1 + i] = arg;
        }
        fiber.frames.push(Frame {
            proto: CALLBACK_PROTO,
            ip: Proto::callback_entry(args.len()),
            base: base as u32,
            ret: base as u32,
            closure: NO_CLOSURE,
        });
        Ok(())
    }

    /// Runs the Sand function `callee` with `args` on `fiber` until it returns,
    /// for a synchronous native call. `base` is the first free slot above the
    /// registers of the native's caller.
    ///
    /// The callee cannot suspend. If it waits, prompts for a permission, or
    /// cancels its own task, the run ends with an error. On every outcome,
    /// `fiber` keeps the frames that it had before the call.
    pub(crate) fn run_callback(
        &mut self,
        code: &Code,
        fiber: &mut Fiber,
        base: usize,
        callee: VmValue,
        args: &[VmValue],
    ) -> Result<VmValue, Fail> {
        if self.nested_callbacks >= MAX_NESTED_CALLBACKS {
            return Err(self.stack_overflow(fiber).into());
        }
        let frame_count = fiber.frames.len();
        // The native calls of the callee replace the state of the active native call.
        let (collected, caller_site) = (std::mem::take(&mut self.collected), self.caller_site);
        self.nested_callbacks += 1;
        let result = match self.begin_callback(fiber, base, callee, args) {
            Ok(()) => self.finish_callback(code, fiber),
            Err(ending) => Err(ending.into()),
        };
        self.nested_callbacks -= 1;
        (self.collected, self.caller_site) = (collected, caller_site);
        fiber.frames.truncate(frame_count);
        result
    }

    /// Runs `fiber` until the callback from [`Vm::begin_callback`] returns.
    fn finish_callback(&mut self, code: &Code, fiber: &mut Fiber) -> Result<VmValue, Fail> {
        let mut fuel = self.meter.resync();
        let result = loop {
            let stop = match self.run_slice(code, fiber, &mut fuel) {
                Ok(stop) => stop,
                Err(ending) => break Err(ending.into()),
            };
            let (action, trace_ip) = match stop {
                FiberStop::CallbackReturn(value) => break Ok(value),
                FiberStop::Preempt => {
                    self.meter.sync(fuel);
                    fuel = self.meter.next_slice();
                    match self.meter.check() {
                        Ok(()) => continue,
                        Err(fail) => break Err(self.run_ending_of(fail, |vm| vm.trace_of(fiber, None)).into()),
                    }
                }
                FiberStop::YieldNow => continue,
                FiberStop::Blocked | FiberStop::AsyncNative(_) => ("wait", None),
                // A prompt rewinds the frame to the call that needs the permission.
                FiberStop::Prompt(_) => ("prompt for a permission", fiber.frames.last().map(|frame| frame.ip + 1)),
                FiberStop::Cancelled => ("cancel the running task", None),
                FiberStop::Done(_) | FiberStop::Unwound | FiberStop::CollectAndRetry(_) => {
                    unreachable!("the callback frame stays below the callee, and `run_slice` retries collections")
                }
            };
            break Err(self.synchronous_call_error(fiber, action, trace_ip).into());
        };
        self.meter.sync(fuel);
        result
    }

    /// Returns the error for a synchronous native call that tries to `action`.
    pub(crate) fn synchronous_call_error(&self, fiber: &Fiber, action: &str, trace_ip: Option<u32>) -> RunEnding {
        Trap::Error { message: format!("invalid: cannot {action} inside a synchronous native call"), trace: self.trace_of(fiber, trace_ip) }
            .into()
    }

    /// Returns the error for a call that exceeds a call depth limit.
    fn stack_overflow(&self, fiber: &Fiber) -> RunEnding {
        Trap::Error { message: "limit: stack overflow".into(), trace: self.trace_of(fiber, None) }.into()
    }

    /// Pops the callback trampoline frame on `fiber`. Roots `result` for the waiting native call.
    fn end_callback(&mut self, fiber: &mut Fiber, result: VmValue) {
        fiber.frames.pop();
        fiber.stopped_in_native_call = true;
        let active_call = fiber.native_calls.last().expect("active native call for callback");
        let mut state = active_call.call.state.lock();
        let rooted = state.root(result);
        state.callback_result = Some(rooted);
    }

    /// Returns the host-event [`Waker`] for `fiber_id`.
    fn fiber_waker(&mut self, fiber_id: u32) -> Waker {
        while self.fiber_wakers.len() <= fiber_id as usize {
            let next_id = self.fiber_wakers.len() as u32;
            self.fiber_wakers.push(Arc::new(FiberWaker {
                fiber_id: next_id,
                is_queued: AtomicBool::new(false),
                wake_state: self.host.interrupt.wake_state.clone(),
            }));
        }
        Waker::from(self.fiber_wakers[fiber_id as usize].clone())
    }

    /// Moves woken host-waiting fibers into the ready queue. Returns `true` if it queued any fiber.
    fn wake_host_waits(&mut self) -> bool {
        let mut woke_any = false;
        for fiber_id in std::mem::take(&mut *self.host.interrupt.wake_state.woken_fibers.lock()) {
            self.fiber_wakers[fiber_id as usize].is_queued.store(false, Ordering::Relaxed);
            if let Some(Some(fiber)) = self.fibers.get_mut(fiber_id as usize)
                && let Some(active_call) = fiber.native_calls.last_mut()
                && active_call.host_wait.take().is_some()
            {
                fiber.state = FiberState::Ready;
                self.ready.push_back(fiber_id);
                woke_any = true;
            }
        }
        woke_any
    }

    /// Returns the active host-event [`WaitKind`], if any.
    ///
    /// Prefers [`WaitKind::User`] over [`WaitKind::Io`] if tasks wait on both.
    pub(crate) fn host_wait_kind(&self) -> Option<WaitKind> {
        let counts = &self.host.interrupt.wake_state.host_wait_counts;
        [WaitKind::User, WaitKind::Io].into_iter().find(|&kind| counts[kind as usize].load(Ordering::Relaxed) > 0)
    }
}

/// Advances `fiber` until it stops or exhausts `fuel`.
///
/// Alternates between bytecode slices and polls of the top native call.
fn run_fiber_turn(
    vm: &mut MutexGuard<'_, Vm>,
    shared_vm: &SharedVm,
    code: &Code,
    fiber: &mut Fiber,
    fuel: &mut i64,
) -> Result<FiberStop, RunEnding> {
    loop {
        if fiber.top_native_call().is_some() {
            vm.meter.sync(*fuel);
            let polled = poll_native_call(vm, shared_vm, fiber);
            *fuel = vm.meter.resync();
            if let Some(stop) = polled? {
                return Ok(stop);
            }
            if *fuel <= 0 {
                return Ok(FiberStop::Preempt);
            }
            continue;
        }
        match vm.run_slice(code, fiber, fuel)? {
            FiberStop::AsyncNative(call) => vm.push_native_call(fiber, call),
            FiberStop::CallbackReturn(result) => vm.end_callback(fiber, result),
            stop => return Ok(stop),
        }
    }
}

/// Polls the top async native call of `fiber`.
///
/// Releases the VM lock during each poll. Handles collection requests in place.
/// Returns `Ok(None)` after `Poll::Ready` or a Sand callback request. Returns
/// `Ok(Some(stop))` on a host-event wait or a permission prompt.
fn poll_native_call(vm: &mut MutexGuard<'_, Vm>, shared_vm: &SharedVm, fiber: &mut Fiber) -> Result<Option<FiberStop>, RunEnding> {
    let mut collected = false;
    loop {
        let fiber_id = vm.current;
        let waker = vm.fiber_waker(fiber_id);
        let active_call = fiber.native_calls.last_mut().expect("a native call is on top");
        active_call.call.state.lock().lent_vm = Some(shared_vm.clone());
        vm.heap.fail_next_check.set(vm.retry_stress && !collected);
        let polled =
            MutexGuard::unlocked(vm, || crate::unwind::catch(|| active_call.call.future.as_mut().poll(&mut Context::from_waker(&waker))));
        vm.heap.fail_next_check.set(false);
        let (request, root_capacity) = {
            let mut state = active_call.call.state.lock();
            state.lent_vm = None;
            (state.request.take(), state.roots.capacity())
        };
        let polled = match polled {
            Ok(polled) => polled,
            Err(panic_message) => {
                let _ = crate::unwind::catch(|| vm.pop_native_call(fiber));
                vm.broken = true;
                return Err(Trap::Internal { message: panic_message, trace: vm.trace_of(fiber, None) }.into());
            }
        };
        if let Poll::Ready(result) = polled {
            let slot = active_call.result_slot;
            vm.pop_native_call(fiber);
            fiber.stack[slot] = result.map_err(|fail| vm.run_ending_of(fail, |vm| vm.trace_of(fiber, None)))?;
            vm.enforce_limit(Some(fiber)).map_err(Trap::Limit)?;
            return Ok(None);
        }
        let tracked_bytes = std::mem::size_of_val(&*active_call.call.future) + root_capacity * std::mem::size_of::<VmValue>();
        vm.heap.fiber_bytes = (vm.heap.fiber_bytes + tracked_bytes).saturating_sub(active_call.tracked_bytes);
        active_call.tracked_bytes = tracked_bytes;
        vm.enforce_limit(Some(fiber)).map_err(Trap::Limit)?;
        match request {
            None => {
                fiber.wait_gen = vm.next_gen();
                let active_call = fiber.native_calls.last_mut().expect("a native call is on top");
                active_call.host_wait = Some(HostWaitGuard::new(active_call.call.wait_kind, vm.host.interrupt.wake_state.clone()));
                return Ok(Some(FiberStop::Blocked));
            }
            Some(VmRequest::Call { callee, args }) => {
                let base = fiber.live_top(&vm.prog);
                vm.begin_callback(fiber, base, callee, &args)?;
                return Ok(None);
            }
            Some(VmRequest::Collect) => {
                collected = true;
                vm.collect_all_garbage(Some(fiber));
            }
            Some(VmRequest::Prompt(capability)) => return Ok(Some(FiberStop::Prompt(Box::new(capability)))),
        }
    }
}

/// Reason [`run_ready`] returned control to [`drive`].
enum Pause<S> {
    Stop(S),
    Prompt(Capability),
    Yield,
    /// The ready queue is empty.
    Idle,
}

/// Runs ready fibers from `vm.ready` until a [`Pause`] condition stops the loop.
fn run_ready<U: DriveUntil>(
    vm: &mut MutexGuard<'_, Vm>,
    shared_vm: &SharedVm,
    stop_policy: &U,
    timer: &mut RunTimer,
) -> Result<Pause<U::Stop>, RunEnding> {
    let code = (vm.prog.clone(), vm.natives.clone());
    while let Some(fiber_id) = vm.ready.pop_front() {
        let Some(mut fiber) = vm.fibers.get_mut(fiber_id as usize).and_then(|slot| slot.take()) else { continue };
        if fiber.state != FiberState::Ready {
            vm.fibers[fiber_id as usize] = Some(fiber);
            continue;
        }
        vm.current = fiber_id;
        let used_before = vm.meter.used();
        // Set meter deadlines for time-limit checks inside long native calls.
        let mut fuel = vm.meter.start_slice(timer.cpu_deadline(vm.host.limits.max_cpu_time), vm.deadline);
        // Catch panics once per turn. Report each panic as an internal error.
        let turn_result = crate::unwind::catch(|| run_fiber_turn(vm, shared_vm, &code, &mut fiber, &mut fuel)).unwrap_or_else(|message| {
            vm.broken = true;
            Err(Trap::Internal { message, trace: vm.trace_of(&fiber, None) }.into())
        });
        vm.meter.sync(fuel);
        // Synchronous native calls can start more slices during the turn.
        timer.ran(vm.meter.used() - used_before);
        // Share current heap usage with background I/O readers for `max_memory` checks.
        let used = vm.heap.used();
        vm.heap.io_memory.publish_heap(used);
        vm.current = u32::MAX;
        let stop = match turn_result {
            Ok(stop) => stop,
            Err(ending) => {
                vm.fibers[fiber_id as usize] = Some(fiber);
                return Err(ending);
            }
        };
        match stop {
            FiberStop::CollectAndRetry(_) | FiberStop::AsyncNative(_) | FiberStop::CallbackReturn(_) => {
                unreachable!("run_fiber_turn handles retries and native calls")
            }
            FiberStop::Done(value) => {
                vm.fibers[fiber_id as usize] = Some(fiber);
                if let Some(stop) = stop_policy.stop_on_finish(fiber_id, value) {
                    return Ok(Pause::Stop(stop));
                }
                vm.finish_task(fiber_id, value);
            }
            FiberStop::Preempt | FiberStop::YieldNow => {
                fiber.state = FiberState::Ready;
                vm.fibers[fiber_id as usize] = Some(fiber);
                vm.ready.push_back(fiber_id);
            }
            FiberStop::Blocked => {
                fiber.state = FiberState::Waiting;
                vm.fibers[fiber_id as usize] = Some(fiber);
            }
            FiberStop::Prompt(cap) => {
                fiber.state = FiberState::Ready;
                vm.fibers[fiber_id as usize] = Some(fiber);
                vm.ready.push_front(fiber_id);
                return Ok(match stop_policy.stop_on_prompt(&cap) {
                    Some(stop) => Pause::Stop(stop),
                    None => Pause::Prompt(*cap),
                });
            }
            FiberStop::Cancelled => {
                vm.end_cancelled(fiber_id, fiber);
                if vm.fibers[fiber_id as usize].is_some() {
                    vm.ready.push_back(fiber_id);
                }
            }
            FiberStop::Unwound => {
                vm.free_fibers.push(fiber_id);
                vm.live_tasks = vm.live_tasks.saturating_sub(1);
                vm.recycle_fiber(fiber);
            }
        }
        vm.check_limits(timer)?;
        if !tokio::task::coop::has_budget_remaining() || timer.now.saturating_duration_since(timer.last_yield) > YIELD_EVERY {
            vm.wake_host_waits();
            // On WebAssembly, `yield_now` queues a microtask inside the current browser turn.
            // Flush standard output before yielding to deliver incremental output to the host.
            #[cfg(web)]
            vm.flush_stdout();
            return Ok(Pause::Yield);
        }
    }
    Ok(Pause::Idle)
}

/// State captured before [`drive`] awaits a host event or deadline.
struct HostEventWait {
    idle_guard: Option<crate::stdin_feed::StdinIdleGuard>,
    user_wait_start: Option<Instant>,
    deadline: Option<Instant>,
    interrupt: crate::engine::CallInterrupt,
}

/// Runs the zero-argument function `proto` as the main task under `Vm::deadline`.
/// Interleaves spawned tasks until the main task returns. Leaves unfinished
/// spawned tasks in the scheduler. Flushes standard output before returning.
pub(crate) async fn run_main(shared_vm: &SharedVm, proto: u32) -> Result<VmValue, RunEnding> {
    let started = shared_vm.lock().start_main(proto);
    let main_result = match started {
        Ok(fid) => drive(shared_vm, UntilMainDone(fid)).await,
        Err(ending) => Err(ending),
    };
    shared_vm.lock().drain_output_after(main_result)
}

/// Cancels all remaining tasks after `run_main`. Drives their pending `defer`
/// blocks until every task finishes unwinding or blocks.
pub(crate) async fn end_tasks(shared_vm: &SharedVm) -> Result<(), RunEnding> {
    {
        let mut vm = shared_vm.lock();
        vm.release_main();
        let tasks: Vec<VmValue> = vm.fibers.iter().flatten().map(|f| f.task).collect();
        for task in tasks {
            vm.cancel_other_task(task);
        }
        if vm.ready.is_empty() {
            return Ok(());
        }
    }
    let drive_result = drive(shared_vm, UntilIdle { waits_for_io: false }).await;
    shared_vm.lock().drain_output_after(drive_result).map(|_| ())
}

/// Drives ready tasks and async waits until `stop_policy` ends the drive.
///
/// Releases the VM lock only while polling async native calls or awaiting in
/// `drive`. Dropping this future at an `.await` point leaves all tasks in `Vm`.
pub(crate) async fn drive<U: DriveUntil>(shared_vm: &SharedVm, stop_policy: U) -> Result<U::Stop, RunEnding> {
    let mut timer = RunTimer::start(shared_vm.lock().host.clock.clone());
    loop {
        let pause = run_ready(&mut shared_vm.lock(), shared_vm, &stop_policy, &mut timer)?;
        match pause {
            Pause::Stop(stop) => return Ok(stop),
            Pause::Yield => {
                timer.read();
                if tokio::task::coop::has_budget_remaining() {
                    timer.yield_now().await;
                } else {
                    tokio::task::yield_now().await;
                }
                timer.resume();
                timer.last_yield = timer.now;
            }
            Pause::Prompt(cap) => {
                let (prompter, interrupt) = {
                    let mut vm = shared_vm.lock();
                    // Flush script output before prompting the user.
                    vm.drain_output()?;
                    (vm.host.prompter.clone(), vm.host.interrupt.clone())
                };
                timer.read();
                let asked = timer.now;
                let answer = match prompter {
                    Some(prompter) => interrupt.unless_interrupted(prompter.prompt(cap.clone())).await.ok_or(Trap::Interrupted)?,
                    None => PromptAnswer::DenyOnce,
                };
                let mut vm = shared_vm.lock();
                vm.exclude_wall_time(asked, &timer);
                timer.resume();
                vm.apply_answer(cap, answer);
                vm.check_limits(&timer)?;
            }
            Pause::Idle => {
                let wait = {
                    let mut vm = shared_vm.lock();
                    vm.flush_stdout();
                    let Some(wait_kind) = vm.host_wait_kind() else {
                        return stop_policy.stop_on_all_blocked(&vm);
                    };
                    if vm.wake_host_waits() {
                        vm.check_limits(&timer)?;
                        continue;
                    }
                    if let Some(stop) = stop_policy.stop_before_io_wait() {
                        return Ok(stop);
                    }
                    timer.read();
                    HostEventWait {
                        idle_guard: vm.host.stdin.clone().map(crate::stdin_feed::StdinIdleGuard::new),
                        user_wait_start: (wait_kind == WaitKind::User).then_some(timer.now),
                        deadline: vm.deadline,
                        interrupt: vm.host.interrupt.clone(),
                    }
                };
                let mut sleep =
                    wait.deadline.filter(|_| wait.user_wait_start.is_none()).map(|d| timer.sleep(d.saturating_duration_since(timer.now())));
                let host_event = std::future::poll_fn(|cx| {
                    let woken = !wait.interrupt.wake_state.woken_fibers.lock().is_empty();
                    let deadline_passed = sleep.as_mut().is_some_and(|s| s.as_mut().poll(cx).is_ready());
                    if woken || deadline_passed { Poll::Ready(()) } else { Poll::Pending }
                });
                wait.interrupt.unless_interrupted(host_event).await;
                drop(wait.idle_guard);
                let mut vm = shared_vm.lock();
                if let Some(start) = wait.user_wait_start {
                    vm.exclude_wall_time(start, &timer);
                }
                timer.resume();
                timer.last_yield = timer.now;
                vm.wake_host_waits();
                vm.check_limits(&timer)?;
            }
        }
    }
}
