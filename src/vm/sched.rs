//! The task scheduler: runs fibers round-robin and drives pending I/O.

use super::interp::Exit;
use super::*;
use crate::LimitKind;
use crate::platform::{ClockFuture, Instant};
use std::sync::atomic::Ordering;
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
        if let Some(w) = self.waker.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            w.wake_by_ref();
        }
    }

    /// Registers the engine task's waker (woken by I/O and interrupts).
    fn set_waker(&self, w: &Waker) {
        let mut slot = self.waker.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.as_ref().is_some_and(|x| x.will_wake(w)) {
            *slot = Some(w.clone());
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
        self.shared.ready.lock().unwrap_or_else(|e| e.into_inner()).push(self.slot);
        self.shared.wake();
    }
}

/// A pending I/O operation: the future of a native call of task `fid`,
/// which waits for it as wait generation `wgen`.
struct IoOp {
    fid: u32,
    wgen: u64,
    fut: BoxFuture,
    wait: Wait,
}

/// The pending I/O operations of a VM, in slots (an operation's id is its
/// slot); each slot keeps its waker for the operations it holds.
#[derive(Default)]
pub struct IoTable {
    ops: Vec<Option<IoOp>>,
    wakers: Vec<Waker>,
    free: Vec<u32>,
    pending: usize,
    /// The pending operations that wait for the user.
    user_waits: usize,
    pub shared: Arc<IoShared>,
}

impl IoTable {
    pub fn with_shared(shared: Arc<IoShared>) -> IoTable {
        IoTable { shared, ..IoTable::default() }
    }

    /// Adds the operation `fut` of task `fid` (waiting as `wgen`), polling
    /// it once: returns its id, or its result if it is done already.
    pub fn add(&mut self, fid: u32, wgen: u64, mut fut: BoxFuture, wait: Wait) -> Result<u64, Finisher> {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                let s = self.ops.len() as u32;
                self.ops.push(None);
                self.wakers.push(Waker::from(Arc::new(SlotWaker { slot: s, shared: self.shared.clone() })));
                s
            }
        };
        if let Poll::Ready(fin) = fut.as_mut().poll(&mut Context::from_waker(&self.wakers[slot as usize])) {
            self.free.push(slot);
            return Err(fin);
        }
        self.ops[slot as usize] = Some(IoOp { fid, wgen, fut, wait });
        self.pending += 1;
        self.user_waits += usize::from(wait == Wait::User);
        Ok(u64::from(slot))
    }

    pub fn cancel(&mut self, id: u64) {
        if let Some(op) = self.ops.get_mut(id as usize).and_then(Option::take) {
            self.free.push(id as u32);
            self.forget(&op);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending == 0
    }

    /// Whether a pending operation waits for the user.
    pub fn awaits_user(&self) -> bool {
        self.user_waits > 0
    }

    /// Cancels every pending operation.
    pub fn clear(&mut self) {
        for (slot, op) in self.ops.iter_mut().enumerate() {
            if op.take().is_some() {
                self.free.push(slot as u32);
            }
        }
        self.pending = 0;
        self.user_waits = 0;
    }

    fn forget(&mut self, op: &IoOp) {
        self.pending -= 1;
        self.user_waits -= usize::from(op.wait == Wait::User);
    }

    /// Polls the operations that were woken. Returns those completed.
    pub fn poll_ready(&mut self, task_waker: Option<&Waker>) -> Vec<(u32, u64, Finisher)> {
        if let Some(w) = task_waker {
            self.shared.set_waker(w);
        }
        let slots = std::mem::take(&mut *self.shared.ready.lock().unwrap_or_else(|e| e.into_inner()));
        let mut done = Vec::new();
        for slot in slots {
            let Some(Some(op)) = self.ops.get_mut(slot as usize) else { continue };
            if let Poll::Ready(fin) = op.fut.as_mut().poll(&mut Context::from_waker(&self.wakers[slot as usize])) {
                let op = self.ops[slot as usize].take().expect("polled");
                self.free.push(slot);
                self.forget(&op);
                done.push((op.fid, op.wgen, fin));
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

/// Result of running the program's main task.
pub enum RunEnd {
    Finished(Value),
    Exit(i32),
}

/// Why [`Vm::drive`] returned.
pub(crate) enum Stop {
    /// The main task finished with this value.
    Main(Value),
    /// A task called `exit`.
    Exit(i32),
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
    fn check_limits(&self, timer: &RunTimer) -> Result<(), Trap> {
        if self.host.interrupt.load(Ordering::Relaxed) {
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
    fn apply_completions(&mut self, done: Vec<(u32, u64, Finisher)>, timer: &RunTimer) -> Result<(), Trap> {
        if done.is_empty() {
            return Ok(());
        }
        self.meter.start_slice(timer.cpu_deadline(self.host.limits.max_cpu_time), self.deadline);
        for (fid, wgen, fin) in done {
            if !wait_is_live(&self.fibers, (fid, wgen)) {
                continue;
            }
            // A Rust panic in the finisher (a bug) becomes an internal error.
            let r = match crate::unwind::catch(|| fin(self)) {
                Ok(r) => r,
                Err(msg) => {
                    self.broken = true;
                    Err(Ctl::internal(msg))
                }
            };
            match r {
                Ok(v) => {
                    self.deliver(fid, wgen, v);
                }
                Err(c) => {
                    let trace = match &self.fibers[fid as usize] {
                        Some(f) => self.trace_of(f, None),
                        None => vec![],
                    };
                    return Err(match c.into_kind() {
                        CtlKind::Error(message) => Trap::Error { message, trace },
                        CtlKind::Internal(message) => {
                            self.broken = true;
                            Trap::Internal { message, trace }
                        }
                        CtlKind::Limit(k) => Trap::Limit(k),
                        CtlKind::Interrupted => Trap::Interrupted,
                        CtlKind::Exit(c) => Trap::Exit(c),
                        other => {
                            self.broken = true;
                            Trap::Internal { message: format!("unexpected {other:?} from an I/O operation"), trace }
                        }
                    });
                }
            }
        }
        Ok(())
    }

    /// Runs `proto` (a zero-argument function) as the main task until it
    /// finishes, running the other tasks meanwhile, then returns (other
    /// tasks are left as they are). The wall-time limit is `self.deadline`.
    /// Buffered standard output is flushed at the end.
    pub async fn run_main(&mut self, proto: u32) -> Result<RunEnd, Trap> {
        let r = match self.start_main(proto) {
            Ok(fid) => match self.drive(Some(fid)).await {
                Ok(Stop::Exit(c)) => Ok(RunEnd::Exit(c)),
                Ok(Stop::Main(v)) => Ok(RunEnd::Finished(v)),
                // Only runs without a main task stop this way.
                Ok(Stop::Idle | Stop::Prompt(_)) => Ok(RunEnd::Finished(Value::Unit)),
                Err(t) => Err(t),
            },
            Err(t) => Err(t),
        };
        let drained = self.drain_output();
        match drained {
            Err(c) if r.is_ok() => Err(Trap::from_ctl(c)),
            _ => r,
        }
    }

    fn start_main(&mut self, proto: u32) -> Result<u32, Trap> {
        let closure = self.heap.alloc(Obj::Closure(proto, Box::new([])));
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: 0, state: TaskState::Running, result: Value::Unit })));
        let fid = self.spawn_fiber(closure, task).map_err(|c| match c.as_limit() {
            Some(k) => Trap::Limit(k),
            None => Trap::Internal { message: "cannot start main task".into(), trace: vec![] },
        })?;
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
    pub(crate) async fn drive(&mut self, main: Option<u32>) -> Result<Stop, Trap> {
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
                        Err(Trap::Internal { message: msg, trace: self.trace_of(&f, None) })
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
                    Err(Trap::Limit(LimitKind::Memory)) if std::mem::take(&mut self.gc_retry) => {
                        // Memory checks count garbage that has not been
                        // collected yet, so collect and run the failed
                        // instruction again. If the retry itself failed, report
                        // the limit. A retry is told apart from a later run of
                        // the same instruction by the fuel spent: reaching it
                        // again takes a back edge or a call, which cost fuel.
                        let Some(&fr) = f.frames.last() else { return Err(Trap::Limit(LimitKind::Memory)) };
                        let at = (fid, f.frames.len(), fr.ip);
                        let op = self.prog.protos[fr.proto as usize].code[fr.ip as usize];
                        if gc_retried == Some(at) && slice - fuel <= super::interp::call_tick(op) {
                            return Err(Trap::Limit(LimitKind::Memory));
                        }
                        gc_retried = Some(at);
                        self.collect_all_garbage(Some(&mut f));
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_front(fid);
                        continue;
                    }
                    Err(Trap::Exit(c)) => return Ok(Stop::Exit(c)),
                    Err(t) => return Err(t),
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
                    Exit::Async(fut, wait) => {
                        let g = self.next_gen();
                        f.wgen = g;
                        f.state = FiberState::Waiting;
                        let added = self.io.add(fid, g, fut, wait);
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
                        self.drain_output().map_err(Trap::from_ctl)?;
                        timer.read();
                        let asked = timer.now;
                        let answer = match self.host.prompter.clone() {
                            Some(p) => {
                                // A host interrupt stops the run while a prompt is pending.
                                let mut prompt = p.prompt(cap.clone());
                                let (interrupt, shared) = (self.host.interrupt.clone(), self.io.shared.clone());
                                let answer = std::future::poll_fn(|cx| {
                                    shared.set_waker(cx.waker());
                                    if interrupt.load(Ordering::Relaxed) {
                                        return Poll::Ready(None);
                                    }
                                    prompt.as_mut().poll(cx).map(Some)
                                })
                                .await;
                                match answer {
                                    Some(a) => a,
                                    None => return Err(Trap::Interrupted),
                                }
                            }
                            None => PromptAnswer::DenyOnce,
                        };
                        // Time spent waiting for the answer does not count.
                        if let Some(d) = &mut self.deadline {
                            *d += timer.now().saturating_duration_since(asked);
                        }
                        timer.resume();
                        self.apply_answer(cap, answer);
                    }
                    Exit::Terminate(c) => return Ok(Stop::Exit(c)),
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
                return Err(Trap::Error { message: "deadlock: all tasks are blocked".into(), trace });
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
            let blocked = self.host.stdin_feed.as_ref().map(|f| f.blocked());
            // Time spent waiting for the user does not count, as for prompts.
            let paused = self.io.awaits_user().then_some(timer.now);
            let mut sleep = self.deadline.filter(|_| paused.is_none()).map(|d| timer.sleep(d.saturating_duration_since(timer.now())));
            let interrupt = self.host.interrupt.clone();
            let done = std::future::poll_fn(|cx| {
                let done = self.io.poll_ready(Some(cx.waker()));
                if !done.is_empty() {
                    return Poll::Ready(done);
                }
                if interrupt.load(Ordering::Relaxed) {
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
            drop(blocked);
            if let (Some(asked), Some(d)) = (paused, &mut self.deadline) {
                *d += timer.now().saturating_duration_since(asked);
            }
            timer.resume();
            timer.last_yield = timer.now;
            self.apply_completions(done, &timer)?;
            self.check_limits(&timer)?;
        }
    }
}
