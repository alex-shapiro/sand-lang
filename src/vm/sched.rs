//! The task scheduler: runs fibers round-robin and drives pending I/O.

use super::interp::Exit;
use super::*;
use crate::engine::LimitKind;
use crate::platform::{ClockFuture, Instant};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// State shared with wakers and interrupt handles.
#[derive(Default)]
pub struct IoShared {
    /// The slots of the operations woken since the table last polled them.
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
}

/// Wakes the operation in a slot of an [`IoTable`]; a stale wake-up only
/// polls the slot's current operation once more.
struct SlotWaker {
    slot: u32,
    shared: Arc<IoShared>,
}

impl Wake for SlotWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.shared.ready.lock().push(self.slot);
        self.shared.wake();
    }
}

/// A pending I/O operation: the future of a native call of task `fid`,
/// which waits for it as wait generation `wgen`.
struct IoOp {
    fid: u32,
    wgen: u64,
    suspension: Suspension,
}

/// The pending I/O operations of a VM, in slots (an operation's id is its
/// slot); each slot keeps its waker for the operations it holds.
#[derive(Default)]
pub struct IoTable {
    ops: Vec<Option<IoOp>>,
    wakers: Vec<Waker>,
    free: Vec<u32>,
    pending: usize,
    pub shared: Arc<IoShared>,
}

impl IoTable {
    pub fn with_shared(shared: Arc<IoShared>) -> IoTable {
        IoTable { shared, ..IoTable::default() }
    }

    /// Polls the future of `suspension` once and registers it for task `fid`
    /// if still pending. Returns the slot ID, or the finisher if ready
    /// immediately.
    pub fn add(&mut self, fid: u32, wgen: u64, mut suspension: Suspension) -> Result<u64, Finisher> {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                let s = self.ops.len() as u32;
                self.ops.push(None);
                self.wakers.push(Waker::from(Arc::new(SlotWaker { slot: s, shared: self.shared.clone() })));
                s
            }
        };
        if let Poll::Ready(fin) = suspension.future.as_mut().poll(&mut Context::from_waker(&self.wakers[slot as usize])) {
            self.free.push(slot);
            return Err(fin);
        }
        self.ops[slot as usize] = Some(IoOp { fid, wgen, suspension });
        self.pending += 1;
        Ok(u64::from(slot))
    }

    pub fn cancel(&mut self, id: u64) {
        if let Some(op) = self.ops.get_mut(id as usize)
            && op.take().is_some()
        {
            self.free.push(id as u32);
            self.pending -= 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending == 0
    }

    /// Returns `true` if any pending operation is waiting for the user.
    pub fn awaits_user(&self) -> bool {
        self.ops.iter().flatten().any(|op| op.suspension.wait_kind == WaitKind::User)
    }

    /// Cancels every pending operation.
    pub fn clear(&mut self) {
        for id in 0..self.ops.len() {
            self.cancel(id as u64);
        }
    }

    /// Polls the operations that were woken. Returns those completed.
    pub fn poll_ready(&mut self, task_waker: Option<&Waker>) -> Vec<(u32, u64, Finisher)> {
        if let Some(w) = task_waker {
            self.shared.set_waker(w);
        }
        let slots = std::mem::take(&mut *self.shared.ready.lock());
        let mut done = Vec::new();
        for slot in slots {
            let Some(Some(op)) = self.ops.get_mut(slot as usize) else { continue };
            if let Poll::Ready(fin) = op.suspension.future.as_mut().poll(&mut Context::from_waker(&self.wakers[slot as usize])) {
                let IoOp { fid, wgen, .. } = self.ops[slot as usize].take().expect("polled");
                self.free.push(slot);
                self.pending -= 1;
                done.push((fid, wgen, fin));
            }
        }
        done
    }
}

/// Slices run between clock readings at most.
const CLOCK_SLICES: u32 = 32;

/// How often a running engine polls its I/O and lets other tasks of the
/// host's executor run.
const YIELD_EVERY: Duration = Duration::from_millis(2);

/// Why [`Vm::drive`] returned.
pub(crate) enum Stop {
    /// The main task finished with this value.
    Main(Value),
    /// Idle state when all tasks are blocked and all I/O is complete.
    Idle,
    /// Suspended state awaiting permission prompt.
    Prompt(Capability),
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

    /// Runs the finishers of completed I/O operations, which charge the
    /// meter like natives.
    fn apply_completions(&mut self, done: Vec<(u32, u64, Finisher)>, timer: &RunTimer) -> Result<(), RunEnding> {
        if done.is_empty() {
            return Ok(());
        }
        self.meter.start_slice(timer.cpu_deadline(self.host.limits.max_cpu_time), self.deadline);
        for (fid, wgen, fin) in done {
            if !wait_is_live(&self.fibers, (fid, wgen)) {
                continue;
            }
            // A Rust panic in the finisher (a bug) becomes an internal error.
            let r = match crate::unwind::catch(|| self.unchecked(fin)) {
                Ok(r) => r,
                Err(msg) => {
                    self.broken = true;
                    self.heap.unchecked = false;
                    Err(Ctl::internal(msg))
                }
            };
            match r {
                Ok(v) => {
                    self.deliver(fid, wgen, v);
                    self.enforce_limit(None).map_err(Trap::Limit)?;
                }
                Err(c) => {
                    return Err(self.run_ending_of(c.into_kind(), |vm| match &vm.fibers[fid as usize] {
                        Some(f) => vm.trace_of(f, None),
                        None => vec![],
                    }));
                }
            }
        }
        Ok(())
    }

    /// Runs `proto` (a zero-argument function) as the main task until it
    /// finishes, running the other tasks meanwhile, then returns (other
    /// tasks are left as they are). The wall-time limit is `self.deadline`.
    /// Buffered standard output is flushed at the end.
    pub async fn run_main(&mut self, proto: u32) -> Result<Value, RunEnding> {
        let r = match self.start_main(proto) {
            Ok(fid) => self.drive(Some(fid)).await.map(|stop| match stop {
                Stop::Main(v) => v,
                // Only runs without a main task stop this way.
                Stop::Idle | Stop::Prompt(_) => Value::Unit,
            }),
            Err(ending) => Err(ending),
        };
        self.drain_output_after(r)
    }

    fn start_main(&mut self, proto: u32) -> Result<u32, RunEnding> {
        let closure = self.heap.alloc(Obj::Closure(proto, Box::new([])));
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: 0, state: TaskState::Running, result: Value::Unit })));
        let fid = self.spawn_fiber(closure, task).map_err(|c| self.run_ending_of(c.into_kind(), |_| vec![]))?;
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.fiber = fid;
        }
        self.main_fiber = fid;
        self.live_tasks += 1;
        Ok(fid)
    }

    /// Runs the ready tasks and pending I/O until the main task (fiber
    /// `main`) finishes or, without one, until nothing can run or a task
    /// needs a permission prompt ([`Stop::Prompt`]). The future may be
    /// dropped where it waits: every task is then in the scheduler's tables.
    pub(crate) async fn drive(&mut self, main: Option<u32>) -> Result<Stop, RunEnding> {
        let mut timer = RunTimer::start(self.host.clock.clone());
        // Where an instruction was retried after a memory limit error.
        let mut gc_retried: Option<(u32, usize, u32)> = None;
        loop {
            while let Some(fid) = self.ready.pop_front() {
                let Some(mut f) = self.fibers.get_mut(fid as usize).and_then(|f| f.take()) else { continue };
                if f.state != FiberState::Ready {
                    self.fibers[fid as usize] = Some(f);
                    continue;
                }
                self.current = fid;
                // Deadlines let long natives check the time limits mid-call.
                let mut fuel = self.meter.start_slice(timer.cpu_deadline(self.host.limits.max_cpu_time), self.deadline);
                let slice = fuel;
                // A Rust panic in the interpreter or a native (a bug) ends
                // the run as an internal error instead of unwinding into the
                // host; caught per slice, which costs nothing per native call.
                let r = match crate::unwind::catch(|| self.interp(&mut f, &mut fuel)) {
                    Ok(r) => r,
                    Err(msg) => {
                        self.broken = true;
                        Err(Trap::Internal { message: msg, trace: self.trace_of(&f, None) }.into())
                    }
                };
                self.meter.sync(fuel);
                timer.ran(self.meter.spent());
                // Pending I/O checks its buffers against the heap's size.
                self.heap.io_memory.publish_heap(self.heap.used());
                self.current = u32::MAX;
                let exit = match r {
                    Ok(e) => {
                        gc_retried = None;
                        e
                    }
                    Err(RunEnding::Trap(Trap::Limit(LimitKind::Memory))) if std::mem::take(&mut self.gc_retry) => {
                        // Memory checks count garbage that has not been
                        // collected yet, so collect and run the failed
                        // instruction again. If the retry itself failed, report
                        // the limit. A retry is told apart from a later run of
                        // the same instruction by the fuel spent: reaching it
                        // again takes a back edge or a call, which cost fuel.
                        let Some(&fr) = f.frames.last() else { return Err(Trap::Limit(LimitKind::Memory).into()) };
                        let at = (fid, f.frames.len(), fr.ip);
                        let op = self.prog.protos[fr.proto as usize].code[fr.ip as usize];
                        if gc_retried == Some(at) && slice - fuel <= super::interp::call_tick(op) {
                            return Err(Trap::Limit(LimitKind::Memory).into());
                        }
                        gc_retried = Some(at);
                        self.collect_all_garbage(Some(&mut f));
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_front(fid);
                        continue;
                    }
                    Err(ending) => return Err(ending),
                };
                match exit {
                    Exit::Done(v) => {
                        self.fibers[fid as usize] = Some(f);
                        if Some(fid) == main {
                            return Ok(Stop::Main(v));
                        }
                        self.finish_task(fid, v);
                    }
                    Exit::Preempt | Exit::YieldNow => {
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_back(fid);
                    }
                    Exit::Blocked => {
                        f.state = FiberState::Waiting;
                        self.fibers[fid as usize] = Some(f);
                    }
                    Exit::Async(suspension) => {
                        let g = self.next_gen();
                        f.wgen = g;
                        f.state = FiberState::Waiting;
                        let added = self.io.add(fid, g, suspension);
                        f.io = added.as_ref().ok().copied();
                        self.fibers[fid as usize] = Some(f);
                        if let Err(fin) = added {
                            self.apply_completions(vec![(fid, g, fin)], &timer)?;
                        }
                    }
                    Exit::Prompt(cap) => {
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_front(fid);
                        if main.is_none() {
                            return Ok(Stop::Prompt(cap));
                        }
                        // Write the script's output before showing the prompt.
                        self.drain_output()?;
                        timer.read();
                        let asked = timer.now;
                        let answer = match self.host.prompter.clone() {
                            Some(prompter) => {
                                let interrupt = self.host.interrupt.clone();
                                match interrupt.unless_interrupted(prompter.prompt(cap.clone())).await {
                                    Some(answer) => answer,
                                    None => return Err(Trap::Interrupted.into()),
                                }
                            }
                            None => PromptAnswer::DenyOnce,
                        };
                        self.exclude_wall_time(asked, &timer);
                        timer.resume();
                        self.apply_answer(cap, answer);
                    }
                    Exit::Cancelled => {
                        // The task cancelled itself.
                        self.end_cancelled(fid, f);
                        if self.fibers[fid as usize].is_some() {
                            self.ready.push_back(fid);
                        } else if Some(fid) == main {
                            return Ok(Stop::Main(Value::Unit));
                        }
                    }
                    Exit::Unwound => {
                        self.free_fibers.push(fid);
                        self.live_tasks = self.live_tasks.saturating_sub(1);
                        self.recycle_fiber(f);
                        if Some(fid) == main {
                            return Ok(Stop::Main(Value::Unit));
                        }
                    }
                }
                self.check_limits(&timer)?;
                if timer.now.saturating_duration_since(timer.last_yield) > YIELD_EVERY {
                    let done = self.io.poll_ready(None);
                    self.apply_completions(done, &timer)?;
                    // On the web a computing engine keeps the thread (yielding
                    // does not reach the event loop), so output reaches the
                    // host only when flushed: flush it as it runs.
                    #[cfg(web)]
                    self.flush_stdout();
                    timer.read();
                    timer.yield_now().await;
                    timer.resume();
                    timer.last_yield = timer.now;
                }
            }
            self.flush_stdout();
            if self.io.is_empty() {
                let Some(main) = main else { return Ok(Stop::Idle) };
                let trace = match self.fibers.get(main as usize) {
                    Some(Some(f)) => self.trace_of(f, None),
                    _ => vec![],
                };
                return Err(Trap::Error { message: "deadlock: all tasks are blocked".into(), trace }.into());
            }
            let done = self.io.poll_ready(None);
            if !done.is_empty() {
                self.apply_completions(done, &timer)?;
                self.check_limits(&timer)?;
                continue;
            }
            if self.ending {
                return Ok(Stop::Idle);
            }
            timer.read();
            let idle = self.host.stdin.clone().map(crate::stdin_feed::StdinIdleGuard::new);
            let pause_start = self.io.awaits_user().then_some(timer.now);
            let mut sleep = self.deadline.filter(|_| pause_start.is_none()).map(|d| timer.sleep(d.saturating_duration_since(timer.now())));
            let interrupt = self.host.interrupt.clone();
            let done = std::future::poll_fn(|cx| {
                let done = self.io.poll_ready(Some(cx.waker()));
                if !done.is_empty() {
                    return Poll::Ready(done);
                }
                if interrupt.is_set() {
                    return Poll::Ready(vec![]);
                }
                if let Some(s) = sleep.as_mut()
                    && s.as_mut().poll(cx).is_ready()
                {
                    return Poll::Ready(vec![]);
                }
                Poll::Pending
            })
            .await;
            drop(idle);
            if let Some(start) = pause_start {
                self.exclude_wall_time(start, &timer);
            }
            timer.resume();
            timer.last_yield = timer.now;
            self.apply_completions(done, &timer)?;
            self.check_limits(&timer)?;
        }
    }
}
