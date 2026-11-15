//! Task scheduler: runs fibers round-robin and drives async native calls.

use super::interp::FiberStop;
use super::*;
use crate::engine::LimitKind;
use crate::native::context::VmRequest;
use crate::platform::{ClockFuture, Instant};
use parking_lot::MutexGuard;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// State shared with host-event wakers and interrupt handles.
#[derive(Default)]
pub struct HostWakeState {
    /// Fiber IDs woken since the last `Vm::wake_host_waits` pass.
    woken_fibers: Mutex<Vec<u32>>,
    /// Count of fibers waiting on a host event, indexed by [`WaitKind`].
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
    /// True while `fiber_id` is queued in `HostWakeState::woken_fibers`.
    is_queued: AtomicBool,
    shared: Arc<HostWakeState>,
}

impl Wake for FiberWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.is_queued.swap(true, Ordering::Relaxed) {
            self.shared.woken_fibers.lock().push(self.fiber_id);
            self.shared.wake();
        }
    }
}

/// Drop guard that counts an active host-event wait in [`HostWakeState`].
struct HostWaitGuard {
    kind: WaitKind,
    shared: Arc<HostWakeState>,
}

impl HostWaitGuard {
    fn new(kind: WaitKind, shared: Arc<HostWakeState>) -> HostWaitGuard {
        shared.host_wait_counts[kind as usize].fetch_add(1, Ordering::Relaxed);
        HostWaitGuard { kind, shared }
    }
}

impl Drop for HostWaitGuard {
    fn drop(&mut self) {
        self.shared.host_wait_counts[self.kind as usize].fetch_sub(1, Ordering::Relaxed);
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
    /// Returns the innermost async native call if every frame above its caller has returned.
    fn top_native_call(&self) -> Option<&ActiveNativeCall> {
        self.native_calls.last().filter(|active_call| active_call.frame_depth == self.frames.len())
    }
}

/// Maximum interpreter slices between clock readings.
const CLOCK_SLICES: u32 = 32;

/// Interval at which a running engine polls host events and yields to the host executor.
const YIELD_EVERY: Duration = Duration::from_millis(2);

/// Stopping policy for [`drive`]. Each policy defines its own
/// [`Stop`](Self::Stop) type so callers handle only the stops that can occur.
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
    /// If an instruction hits a memory limit, `run_slice` collects all garbage,
    /// restores the instruction's fuel, and retries the instruction in the same
    /// slice.
    fn run_slice(&mut self, fiber: &mut Fiber, fuel: &mut i64) -> Result<FiberStop, RunEnding> {
        let mut last_retry_site = None;
        loop {
            let entry_fuel = *fuel;
            // Catch panics once per slice and report them as internal errors
            // instead of unwinding into the host.
            let stop = match crate::unwind::catch(|| self.interp(fiber, fuel)) {
                Ok(stop) => stop,
                Err(msg) => {
                    self.broken = true;
                    return Err(Trap::Internal { message: msg, trace: self.trace_of(fiber, None) }.into());
                }
            };
            let Ok(FiberStop::CollectAndRetry(frame)) = stop else { return stop };
            let tick = super::interp::call_tick(self.prog.protos[frame.proto as usize].code[frame.ip as usize]);
            // Reaching the same instruction again crosses a back edge or a
            // call and spends interpreter fuel. If the instruction fails again
            // at the same fuel, the retry itself exceeded `max_memory`.
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

    /// Pops the innermost async native call from `fiber` and clears its roots.
    fn pop_native_call(&mut self, fiber: &mut Fiber) {
        if let Some(active_call) = fiber.native_calls.pop() {
            active_call.call.state.lock().roots = Vec::new();
            self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(active_call.tracked_bytes);
        }
    }

    /// Drops async native calls started at or above `frame_depth`, from innermost to outermost.
    pub(crate) fn drop_native_calls(&mut self, fiber: &mut Fiber, frame_depth: usize) {
        while fiber.native_calls.last().is_some_and(|active_call| active_call.frame_depth > frame_depth) {
            self.pop_native_call(fiber);
        }
    }

    /// Pushes a [`CALLBACK_PROTO`] frame on `fiber` to call `callee` with `args`.
    fn begin_callback(&mut self, fiber: &mut Fiber, callee: VmValue, args: &[VmValue]) -> Result<(), RunEnding> {
        let base = fiber.live_top(&self.prog);
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
        fiber.stack[base + 1..][..args.len()].copy_from_slice(args);
        fiber.frames.push(Frame {
            proto: CALLBACK_PROTO,
            ip: Proto::callback_entry(args.len()),
            base: base as u32,
            ret: base as u32,
            closure: NO_CLOSURE,
        });
        Ok(())
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
                shared: self.host.interrupt.wake_state.clone(),
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

    /// Returns the active host-event [`WaitKind`].
    ///
    /// Prefers [`WaitKind::User`] if any task waits for user input. Returns
    /// `None` if every task is runnable or blocked on VM state.
    pub(crate) fn host_wait_kind(&self) -> Option<WaitKind> {
        let counts = &self.host.interrupt.wake_state.host_wait_counts;
        [WaitKind::User, WaitKind::Io].into_iter().find(|&kind| counts[kind as usize].load(Ordering::Relaxed) > 0)
    }
}

/// Advances `fiber` until it stops or exhausts `fuel`.
///
/// Alternates between bytecode slices and polls of the top native call.
fn run_fiber_turn(vm: &mut MutexGuard<'_, Vm>, shared: &SharedVm, fiber: &mut Fiber, fuel: &mut i64) -> Result<FiberStop, RunEnding> {
    loop {
        if fiber.top_native_call().is_some() {
            vm.meter.sync(*fuel);
            let polled = poll_native_call(vm, shared, fiber);
            *fuel = vm.meter.resync();
            if let Some(stop) = polled? {
                return Ok(stop);
            }
            if *fuel <= 0 {
                return Ok(FiberStop::Preempt);
            }
            continue;
        }
        match vm.run_slice(fiber, fuel)? {
            FiberStop::AsyncNative(call) => vm.push_native_call(fiber, call),
            FiberStop::CallbackReturn(result) => vm.end_callback(fiber, result),
            stop => return Ok(stop),
        }
    }
}

/// Polls the top async native call of `fiber` while unlocking the VM. Handles
/// each [`VmRequest`] until the call returns `Ready` or `fiber` yields to the
/// scheduler.
fn poll_native_call(vm: &mut MutexGuard<'_, Vm>, shared: &SharedVm, fiber: &mut Fiber) -> Result<Option<FiberStop>, RunEnding> {
    let mut collected = false;
    loop {
        let fiber_id = vm.current;
        let waker = vm.fiber_waker(fiber_id);
        let active_call = fiber.native_calls.last_mut().expect("a native call is on top");
        active_call.call.state.lock().lent_vm = Some(shared.clone());
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
                vm.begin_callback(fiber, callee, &args)?;
                return Ok(None);
            }
            Some(VmRequest::Collect) => {
                collected = true;
                vm.collect_all_garbage(Some(fiber));
            }
            Some(VmRequest::Prompt(capability)) => return Ok(Some(FiberStop::Prompt(capability))),
        }
    }
}

/// Reason [`run_ready`] returned control to [`drive`].
enum Pause<S> {
    Stop(S),
    Prompt(Capability),
    Yield,
    /// Ready queue is empty.
    Idle,
}

/// Runs tasks from `vm.ready` until the `until` policy stops, a task requests
/// a prompt, or the engine yields.
fn run_ready<U: DriveUntil>(
    vm: &mut MutexGuard<'_, Vm>,
    shared: &SharedVm,
    until: &U,
    timer: &mut RunTimer,
) -> Result<Pause<U::Stop>, RunEnding> {
    while let Some(fiber_id) = vm.ready.pop_front() {
        let Some(mut fiber) = vm.fibers.get_mut(fiber_id as usize).and_then(|slot| slot.take()) else { continue };
        if fiber.state != FiberState::Ready {
            vm.fibers[fiber_id as usize] = Some(fiber);
            continue;
        }
        vm.current = fiber_id;
        // Pass deadlines to the meter so long native calls can check time limits.
        let mut fuel = vm.meter.start_slice(timer.cpu_deadline(vm.host.limits.max_cpu_time), vm.deadline);
        let turn_result = run_fiber_turn(vm, shared, &mut fiber, &mut fuel);
        vm.meter.sync(fuel);
        timer.ran(vm.meter.spent());
        // Publish heap usage so background I/O readers enforce `max_memory`.
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
                if let Some(stop) = until.stop_on_finish(fiber_id, value) {
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
                return Ok(match until.stop_on_prompt(&cap) {
                    Some(stop) => Pause::Stop(stop),
                    None => Pause::Prompt(cap),
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
pub(crate) async fn run_main(shared: &SharedVm, proto: u32) -> Result<VmValue, RunEnding> {
    let started = shared.lock().start_main(proto);
    let main_result = match started {
        Ok(fid) => drive(shared, UntilMainDone(fid)).await,
        Err(ending) => Err(ending),
    };
    shared.lock().drain_output_after(main_result)
}

/// Cancels all remaining tasks after `run_main`. Drives their pending `defer`
/// blocks until every task finishes unwinding or blocks.
pub(crate) async fn end_tasks(shared: &SharedVm) -> Result<(), RunEnding> {
    {
        let mut vm = shared.lock();
        vm.release_main();
        let tasks: Vec<VmValue> = vm.fibers.iter().flatten().map(|f| f.task).collect();
        for task in tasks {
            vm.cancel_other_task(task);
        }
        if vm.ready.is_empty() {
            return Ok(());
        }
    }
    let drive_result = drive(shared, UntilIdle { waits_for_io: false }).await;
    shared.lock().drain_output_after(drive_result).map(|_| ())
}

/// Runs ready tasks and awaits host events until the `until` policy stops.
/// Releases the VM lock only while polling async native calls or awaiting in
/// `drive`. If the caller drops this future at an `.await` point, `Vm` retains
/// all tasks in its scheduler tables.
pub(crate) async fn drive<U: DriveUntil>(shared: &SharedVm, until: U) -> Result<U::Stop, RunEnding> {
    let mut timer = RunTimer::start(shared.lock().host.clock.clone());
    loop {
        let pause = run_ready(&mut shared.lock(), shared, &until, &mut timer)?;
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
                    let mut vm = shared.lock();
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
                let mut vm = shared.lock();
                vm.exclude_wall_time(asked, &timer);
                timer.resume();
                vm.apply_answer(cap, answer);
                vm.check_limits(&timer)?;
            }
            Pause::Idle => {
                let wait = {
                    let mut vm = shared.lock();
                    vm.flush_stdout();
                    let Some(wait_kind) = vm.host_wait_kind() else {
                        return until.stop_on_all_blocked(&vm);
                    };
                    if vm.wake_host_waits() {
                        vm.check_limits(&timer)?;
                        continue;
                    }
                    if let Some(stop) = until.stop_before_io_wait() {
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
                let mut vm = shared.lock();
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
