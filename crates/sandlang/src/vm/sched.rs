//! The task scheduler: runs fibers round-robin and drives pending I/O.

use super::interp::FiberStop;
use super::*;
use crate::engine::LimitKind;
use crate::native::context::{CallState, NativeFuture, VmRequest};
use crate::platform::{ClockFuture, Instant};
use parking_lot::MutexGuard;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// State shared with wakers and interrupt handles.
#[derive(Default)]
pub struct IoShared {
    /// The fibers woken since the scheduler last made them ready.
    ready: Mutex<Vec<u32>>,
    waker: Mutex<Option<Waker>>,
}

impl IoShared {
    /// Wakes the engine task (e.g. after an interrupt).
    pub fn wake(&self) {
        if let Some(w) = self.waker.lock().as_ref() {
            w.wake_by_ref();
        }
    }

    /// Stores `waker` so that I/O completions and interrupts wake the caller's
    /// task.
    pub(crate) fn set_waker(&self, waker: &Waker) {
        let mut slot = self.waker.lock();
        if !slot.as_ref().is_some_and(|current| current.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }

    fn has_woken(&self) -> bool {
        !self.ready.lock().is_empty()
    }
}

/// Wakes fiber `fid`, whose innermost native call waits on a host event. A
/// stale wake-up only polls the fiber's current native call once more.
struct FiberWaker {
    fid: u32,
    shared: Arc<IoShared>,
}

impl Wake for FiberWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.shared.ready.lock().push(self.fid);
        self.shared.wake();
    }
}

/// The host-event wakers of a VM's fibers, by fiber id, and the number of
/// fibers that wait on a host event.
#[derive(Default)]
pub struct IoTable {
    wakers: Vec<Waker>,
    waiting: usize,
    pub shared: Arc<IoShared>,
}

impl IoTable {
    pub fn with_shared(shared: Arc<IoShared>) -> IoTable {
        IoTable { shared, ..IoTable::default() }
    }

    /// The waker of fiber `fid`.
    fn waker(&mut self, fid: u32) -> Waker {
        while self.wakers.len() <= fid as usize {
            let fid = self.wakers.len() as u32;
            self.wakers.push(Waker::from(Arc::new(FiberWaker { fid, shared: self.shared.clone() })));
        }
        self.wakers[fid as usize].clone()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting == 0
    }

    /// Forgets every wait, after the VM drops all fibers.
    pub fn clear(&mut self) {
        self.waiting = 0;
    }
}

/// An async native call in progress on a fiber.
pub(crate) struct NativeCall {
    /// `None` while the scheduler polls it.
    future: Option<NativeFuture>,
    call: Arc<CallState>,
    wait_kind: WaitKind,
    /// The number of frames below the call. `frames[depth - 1]` made it.
    depth: usize,
    /// Absolute stack slot receiving the call's result.
    result_slot: usize,
    /// The waker of the fiber, which the future receives.
    waker: Waker,
    future_bytes: usize,
    /// Memory counted in `Heap::fiber_bytes`: the future and its roots.
    bytes: usize,
}

impl NativeCall {
    pub fn footprint(&self) -> usize {
        self.bytes
    }

    /// Adds the call's roots to `roots`.
    pub fn trace(&self, roots: &mut Vec<VmValue>) {
        roots.extend_from_slice(&self.call.exchange.lock().roots);
    }
}

/// Slices run between clock readings at most.
const CLOCK_SLICES: u32 = 32;

/// How often a running engine polls its I/O and lets other tasks of the
/// host's executor run.
const YIELD_EVERY: Duration = Duration::from_millis(2);

/// Stopping policy for [`Vm::drive`]. Each policy defines its own
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

    /// Records consumed fuel and updates times when thresholds are reached.
    fn ran(&mut self, fuel: u64) {
        self.unrecorded_slices += 1;
        self.unrecorded_fuel += fuel;
        if self.unrecorded_slices >= CLOCK_SLICES || self.unrecorded_fuel >= SLICE_FUEL as u64 {
            self.read();
        }
    }

    /// Returns the CPU time deadline for max.
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
    /// Advances the wall-time deadline by the time elapsed since `since` so
    /// that the wait does not count toward the limit.
    fn exclude_wall_time(&mut self, since: Instant, timer: &RunTimer) {
        if let Some(deadline) = &mut self.deadline {
            *deadline += timer.now().saturating_duration_since(since);
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

    /// Starts the async native call `start` on fiber `f`.
    fn begin_native(&mut self, shared: &SharedVm, f: &mut Fiber, start: NativeStart) {
        let _ = start.call.vm.set(Arc::downgrade(shared));
        let before = f.footprint();
        let future_bytes = std::mem::size_of_val(&*start.future);
        let waker = self.io.waker(self.current);
        f.natives.push(NativeCall {
            future: Some(start.future),
            call: start.call,
            wait_kind: start.wait_kind,
            depth: f.frames.len(),
            result_slot: f.result_slot,
            waker,
            future_bytes,
            bytes: future_bytes,
        });
        self.heap.fiber_bytes += f.footprint() - before;
    }

    /// Removes the innermost native call of `f` and drops its future.
    fn pop_native(&mut self, f: &mut Fiber) -> Option<NativeCall> {
        let before = f.footprint();
        let native = f.natives.pop()?;
        native.call.exchange.lock().roots = Vec::new();
        self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(before - f.footprint());
        Some(native)
    }

    /// Drops the native calls of `f` that frames at index `depth` and above
    /// made, innermost first.
    pub(crate) fn drop_natives(&mut self, f: &mut Fiber, depth: usize) {
        if f.host_wait.take().is_some() {
            self.io.waiting -= 1;
        }
        while f.natives.last().is_some_and(|native| native.depth > depth) {
            self.pop_native(f);
        }
    }

    /// Pushes a [`CALLBACK_PROTO`] frame that calls `callee` with `args` for
    /// the innermost native call of `f`.
    fn begin_callback(&mut self, f: &mut Fiber, callee: VmValue, args: &[VmValue]) -> Result<(), RunEnding> {
        let takes = match callee {
            VmValue::Obj(h) => match self.heap.obj(h) {
                Obj::Closure(proto, _) => Some(self.prog.protos[*proto as usize].nparams as usize),
                _ => None,
            },
            _ => None,
        };
        let misuse = match takes {
            Some(n) if n != args.len() => Some(format!("a native call passed {} arguments to a callback that takes {n}", args.len())),
            _ if args.len() > MAX_CALLBACK_ARGS => {
                Some(format!("a native call passed more than {MAX_CALLBACK_ARGS} arguments to a callback"))
            }
            _ => None,
        };
        if let Some(message) = misuse {
            return Err(self.run_ending_of(Fail::internal(message), |vm| vm.trace_of(f, None)));
        }
        let base = f.live_top(&self.prog);
        let need = base + 1 + MAX_CALLBACK_ARGS;
        let grow = |vm: &mut Vm, f: &mut Fiber| -> Result<(), Fail> {
            if f.stack.len() < need {
                f.grow_stack(need, &mut vm.heap)?;
            }
            if f.frames.len() == f.frames.capacity() {
                f.grow_frames(&mut vm.heap)?;
            }
            Ok(())
        };
        let mut grown = grow(self, f);
        if grown.as_ref().is_err_and(|fail| fail.as_limit() == Some(LimitKind::Memory)) {
            self.collect_all_garbage(Some(f));
            grown = grow(self, f);
        }
        if let Err(fail) = grown {
            return Err(self.run_ending_of(fail, |vm| vm.trace_of(f, None)));
        }
        f.stack[base] = callee;
        f.stack[base + 1..][..args.len()].copy_from_slice(args);
        f.frames.push(Frame { proto: CALLBACK_PROTO, ip: 2 * args.len() as u32, base: base as u32, ret: base as u32, closure: NO_CLOSURE });
        Ok(())
    }

    /// Ends the callback of the innermost native call of `f`, which returned `v`.
    fn end_callback(&mut self, f: &mut Fiber, v: VmValue) {
        f.frames.pop();
        f.stopped_in_native_call = true;
        let native = f.natives.last().expect("a native call made the callback");
        let mut exchange = native.call.exchange.lock();
        let reply = exchange.root(v);
        exchange.reply = Some(reply);
    }

    /// Makes the fibers whose host events fired ready. Returns whether any
    /// fiber became ready.
    fn wake_host_waits(&mut self) -> bool {
        let mut woke = false;
        for fid in std::mem::take(&mut *self.io.shared.ready.lock()) {
            if let Some(Some(f)) = self.fibers.get_mut(fid as usize)
                && f.host_wait.take().is_some()
            {
                self.io.waiting -= 1;
                f.state = FiberState::Ready;
                self.ready.push_back(fid);
                woke = true;
            }
        }
        woke
    }

    /// Returns `true` if any task waits for the user.
    fn awaits_user(&self) -> bool {
        self.fibers.iter().flatten().any(|f| f.host_wait == Some(WaitKind::User))
    }
}

type VmGuard<'a> = MutexGuard<'a, Vm>;

/// Runs `f` until it stops. Polls its innermost native call while that call
/// is on top of its frames, and runs its frames otherwise.
fn run_turn(vm: &mut VmGuard<'_>, shared: &SharedVm, f: &mut Fiber, fuel: &mut i64) -> Result<FiberStop, RunEnding> {
    loop {
        if f.natives.last().is_some_and(|native| native.depth == f.frames.len()) {
            vm.meter.sync(*fuel);
            let polled = poll_native(vm, f);
            *fuel = vm.meter.resync();
            if let Some(stop) = polled? {
                return Ok(stop);
            }
            if *fuel <= 0 {
                return Ok(FiberStop::Preempt);
            }
            continue;
        }
        match vm.run_slice(f, fuel)? {
            FiberStop::Native(start) => vm.begin_native(shared, f, start),
            FiberStop::NativeReturn(v) => vm.end_callback(f, v),
            stop => return Ok(stop),
        }
    }
}

/// Polls the innermost native call of `f` with the VM unlocked, and handles
/// its result or request. Returns a stop if `f` must stop running.
fn poll_native(vm: &mut VmGuard<'_>, f: &mut Fiber) -> Result<Option<FiberStop>, RunEnding> {
    let mut collected = false;
    loop {
        let native = f.natives.last_mut().expect("a native call is on top");
        let mut future = native.future.take().expect("the scheduler polls each call once at a time");
        let waker = &native.waker;
        // A panic in the future (a bug) becomes an internal error.
        let stress = vm.retry_stress && !collected;
        vm.heap.fail_next_check.set(stress);
        native.call.polled.store(true, Ordering::Relaxed);
        let polled = MutexGuard::unlocked(vm, || crate::unwind::catch(|| future.as_mut().poll(&mut Context::from_waker(waker))));
        native.call.polled.store(false, Ordering::Relaxed);
        vm.heap.fail_next_check.set(false);
        let (request, roots) = {
            let mut exchange = native.call.exchange.lock();
            (exchange.request.take(), exchange.roots.capacity())
        };
        let polled = match polled {
            Ok(polled) => polled,
            Err(msg) => {
                let _ = crate::unwind::catch(move || drop(future));
                vm.broken = true;
                return Err(Trap::Internal { message: msg, trace: vm.trace_of(f, None) }.into());
            }
        };
        let Poll::Pending = polled else {
            drop(future);
            let slot = native.result_slot;
            vm.pop_native(f);
            let result = match (polled, request) {
                (Poll::Ready(result), None) => result,
                _ => Err(Fail::internal("a native call returned during a VM request")),
            };
            f.stack[slot] = result.map_err(|fail| vm.run_ending_of(fail, |vm| vm.trace_of(f, None)))?;
            vm.enforce_limit(Some(f)).map_err(Trap::Limit)?;
            return Ok(None);
        };
        native.future = Some(future);
        let bytes = native.future_bytes + roots * std::mem::size_of::<VmValue>();
        vm.heap.fiber_bytes = (vm.heap.fiber_bytes + bytes).saturating_sub(native.bytes);
        native.bytes = bytes;
        let wait_kind = native.wait_kind;
        vm.enforce_limit(Some(f)).map_err(Trap::Limit)?;
        match request {
            None => {
                f.wait_gen = vm.next_gen();
                f.host_wait = Some(wait_kind);
                vm.io.waiting += 1;
                return Ok(Some(FiberStop::Blocked));
            }
            Some(VmRequest::Call { callee, args }) => {
                vm.begin_callback(f, callee, &args)?;
                return Ok(None);
            }
            Some(VmRequest::Collect) => {
                collected = true;
                vm.collect_all_garbage(Some(f));
            }
            Some(VmRequest::Prompt(cap)) => return Ok(Some(FiberStop::Prompt(cap))),
        }
    }
}

/// Why [`run_ready`] returned to [`drive`].
enum Pause<S> {
    Stop(S),
    Prompt(Capability),
    Yield,
    /// No task is ready.
    Idle,
}

/// Runs ready tasks until none is ready, a task needs a prompt, or the engine
/// must let other tasks of the host's executor run.
fn run_ready<U: DriveUntil>(vm: &mut VmGuard<'_>, shared: &SharedVm, until: &U, timer: &mut RunTimer) -> Result<Pause<U::Stop>, RunEnding> {
    while let Some(fid) = vm.ready.pop_front() {
        let Some(mut f) = vm.fibers.get_mut(fid as usize).and_then(|f| f.take()) else { continue };
        if f.state != FiberState::Ready {
            vm.fibers[fid as usize] = Some(f);
            continue;
        }
        vm.current = fid;
        // Deadlines let long natives check the time limits mid-call.
        let mut fuel = vm.meter.start_slice(timer.cpu_deadline(vm.host.limits.max_cpu_time), vm.deadline);
        let r = run_turn(vm, shared, &mut f, &mut fuel);
        vm.meter.sync(fuel);
        timer.ran(vm.meter.spent());
        // Pending I/O checks its buffers against the heap's size.
        let used = vm.heap.used();
        vm.heap.io_memory.publish_heap(used);
        vm.current = u32::MAX;
        let stop = match r {
            Ok(stop) => stop,
            Err(ending) => {
                vm.fibers[fid as usize] = Some(f);
                return Err(ending);
            }
        };
        match stop {
            FiberStop::CollectAndRetry(_) | FiberStop::Native(_) | FiberStop::NativeReturn(_) => {
                unreachable!("run_turn handles retries and native calls")
            }
            FiberStop::Done(v) => {
                vm.fibers[fid as usize] = Some(f);
                if let Some(stop) = until.stop_on_finish(fid, v) {
                    return Ok(Pause::Stop(stop));
                }
                vm.finish_task(fid, v);
            }
            FiberStop::Preempt | FiberStop::YieldNow => {
                f.state = FiberState::Ready;
                vm.fibers[fid as usize] = Some(f);
                vm.ready.push_back(fid);
            }
            FiberStop::Blocked => {
                f.state = FiberState::Waiting;
                vm.fibers[fid as usize] = Some(f);
            }
            FiberStop::Prompt(cap) => {
                f.state = FiberState::Ready;
                vm.fibers[fid as usize] = Some(f);
                vm.ready.push_front(fid);
                return Ok(match until.stop_on_prompt(&cap) {
                    Some(stop) => Pause::Stop(stop),
                    None => Pause::Prompt(cap),
                });
            }
            FiberStop::Cancelled => {
                // The task cancelled itself.
                vm.end_cancelled(fid, f);
                if vm.fibers[fid as usize].is_some() {
                    vm.ready.push_back(fid);
                }
            }
            FiberStop::Unwound => {
                vm.free_fibers.push(fid);
                vm.live_tasks = vm.live_tasks.saturating_sub(1);
                vm.recycle_fiber(f);
            }
        }
        vm.check_limits(timer)?;
        // Once the host task spends its tokio budget, tokio futures return
        // `Pending` until the task yields, so polling them would only repeat.
        if !tokio::task::coop::has_budget_remaining() || timer.now.saturating_duration_since(timer.last_yield) > YIELD_EVERY {
            vm.wake_host_waits();
            // On the web a computing engine keeps the thread (yielding does
            // not reach the event loop), so output reaches the host only when
            // flushed: flush it as it runs.
            #[cfg(web)]
            vm.flush_stdout();
            return Ok(Pause::Yield);
        }
    }
    Ok(Pause::Idle)
}

/// What [`drive`] waits for when no task is ready.
struct HostEventWait {
    idle: Option<crate::stdin_feed::StdinIdleGuard>,
    /// When the wait started, if a task waits for the user.
    pause_start: Option<Instant>,
    deadline: Option<Instant>,
    interrupt: crate::engine::CallInterrupt,
    io: Arc<IoShared>,
}

/// Runs `proto` (a zero-argument function) as the main task until it
/// finishes, running the other tasks meanwhile, then returns (other tasks are
/// left as they are). The wall-time limit is `Vm::deadline`. Buffered
/// standard output is flushed at the end.
pub(crate) async fn run_main(shared: &SharedVm, proto: u32) -> Result<VmValue, RunEnding> {
    let started = shared.lock().start_main(proto);
    let main_result = match started {
        Ok(fid) => drive(shared, UntilMainDone(fid)).await,
        Err(ending) => Err(ending),
    };
    shared.lock().drain_output_after(main_result)
}

/// Cancels remaining tasks after the main task finishes. Runs their pending
/// `defer` blocks until they finish or block.
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

/// Runs ready tasks and waits for host events until `until` stops the drive.
/// The VM is locked except while the drive awaits and while it polls native
/// calls. If the future is dropped at an `.await` point, all tasks remain in
/// the scheduler tables.
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
                    // Write the script's output before showing the prompt.
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
                    if vm.io.is_empty() {
                        return until.stop_on_all_blocked(&vm);
                    }
                    if vm.wake_host_waits() {
                        vm.check_limits(&timer)?;
                        continue;
                    }
                    if let Some(stop) = until.stop_before_io_wait() {
                        return Ok(stop);
                    }
                    timer.read();
                    HostEventWait {
                        idle: vm.host.stdin.clone().map(crate::stdin_feed::StdinIdleGuard::new),
                        pause_start: vm.awaits_user().then_some(timer.now),
                        deadline: vm.deadline,
                        interrupt: vm.host.interrupt.clone(),
                        io: vm.io.shared.clone(),
                    }
                };
                let mut sleep =
                    wait.deadline.filter(|_| wait.pause_start.is_none()).map(|d| timer.sleep(d.saturating_duration_since(timer.now())));
                std::future::poll_fn(|cx| {
                    wait.io.set_waker(cx.waker());
                    if wait.io.has_woken() || wait.interrupt.is_set() {
                        return Poll::Ready(());
                    }
                    if let Some(s) = sleep.as_mut()
                        && s.as_mut().poll(cx).is_ready()
                    {
                        return Poll::Ready(());
                    }
                    Poll::Pending
                })
                .await;
                drop(wait.idle);
                let mut vm = shared.lock();
                if let Some(start) = wait.pause_start {
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
