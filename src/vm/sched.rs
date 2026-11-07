//! The task scheduler: runs fibers round-robin and drives pending I/O.

use super::interp::Exit;
use super::journal::{Due, Effects, Parking, Wait, WaitWork};
use super::*;
use crate::engine::LimitKind;
use crate::journal::{ParkedCall, Suspension};
use crate::native::func::Park;
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
    parking: Parking,
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

    /// Adds the operation `fut` of task `fid` (waiting as `wgen`), polling
    /// it once: returns its id, or its result if it is done already.
    pub(crate) fn add(&mut self, fid: u32, wgen: u64, mut fut: BoxFuture, parking: Parking) -> Result<u64, Finisher> {
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
        self.ops[slot as usize] = Some(IoOp { fid, wgen, fut, parking });
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

    /// The host calls that parked and wait.
    fn parked_calls(&self) -> Vec<ParkedCall> {
        self.ops.iter().flatten().filter_map(|op| op.parking.parked_call()).collect()
    }

    /// How long the run may be suspended while every pending operation
    /// waits: until the earliest time that one of them gives, if any. `None`
    /// if one of them does not let the run be suspended.
    fn park(&self) -> Option<Park> {
        self.ops.iter().flatten().try_fold(Park::UntilResumed, |park, op| {
            Some(match (park, op.parking.park()?) {
                (Park::Until(a), Park::Until(b)) => Park::Until(a.min(b)),
                (Park::Until(t), _) | (_, Park::Until(t)) => Park::Until(t),
                _ => Park::UntilResumed,
            })
        })
    }

    /// Cancels every pending operation.
    pub fn clear(&mut self) {
        for (slot, op) in self.ops.iter_mut().enumerate() {
            if op.take().is_some() {
                self.free.push(slot as u32);
            }
        }
        self.pending = 0;
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

/// Result of running the program's main task. `S` is how the run can
/// suspend ([`Suspends`]).
pub enum RunEnd<S> {
    Finished(Value),
    Exit(i32),
    /// The journaled run suspended.
    Suspended(S),
}

/// How a run can suspend when every task waits: [`Suspension`] for a run
/// that can have a journal (SPEC §16.7), and [`Infallible`](std::convert::Infallible)
/// for one that has none, such as a test or a session, so that its end has
/// no suspension to handle.
pub trait Suspends: Sized {
    /// The suspension of `vm` if it suspends now.
    #[doc(hidden)]
    fn now(vm: &Vm) -> Option<Self>;
}

impl Suspends for Suspension {
    fn now(vm: &Vm) -> Option<Suspension> {
        vm.suspension()
    }
}

impl Suspends for std::convert::Infallible {
    fn now(_: &Vm) -> Option<std::convert::Infallible> {
        None
    }
}

/// What [`Vm::drive`] runs the tasks for, and how it stops: [`Main`] runs
/// them until the main task finishes, and [`Idle`] and [`EndTasks`] until
/// nothing can run. Each has its own stop type, so a caller handles only
/// the stops that its purpose can produce.
pub(crate) trait Until {
    type Stop;
    /// The stop when a task calls `exit`.
    fn exit(code: i32) -> Self::Stop;
    /// The stop when task `fid` finishes with `v`, or `v` back if `fid` is
    /// not the main task.
    fn finished(&self, fid: u32, v: Value) -> Result<Self::Stop, Value>;
    /// The stop when a task needs a permission prompt, or `cap` back if
    /// the engine prompts at once.
    fn prompt(&self, cap: Capability) -> Result<Self::Stop, Capability>;
    /// The stop when nothing can run, or the fiber id of the main task,
    /// which then waits forever.
    fn idle(&self) -> Result<Self::Stop, u32>;
    /// The stop before the engine waits for pending I/O, if any.
    fn before_wait(&self, vm: &Vm) -> Option<Self::Stop>;
}

/// Run until the main task, the fiber with this id, finishes. `S` is how
/// the run can suspend.
pub(crate) struct Main<S>(pub u32, pub std::marker::PhantomData<S>);

impl<S: Suspends> Until for Main<S> {
    type Stop = RunEnd<S>;
    fn exit(code: i32) -> RunEnd<S> {
        RunEnd::Exit(code)
    }
    fn finished(&self, fid: u32, v: Value) -> Result<RunEnd<S>, Value> {
        if fid == self.0 { Ok(RunEnd::Finished(v)) } else { Err(v) }
    }
    fn prompt(&self, cap: Capability) -> Result<RunEnd<S>, Capability> {
        Err(cap)
    }
    fn idle(&self) -> Result<RunEnd<S>, u32> {
        Err(self.0)
    }
    fn before_wait(&self, vm: &Vm) -> Option<RunEnd<S>> {
        S::now(vm).map(RunEnd::Suspended)
    }
}

/// Why a run without a main task stopped.
pub(crate) enum IdleStop {
    /// Nothing can run.
    Idle,
    /// A task called `exit`.
    Exit(i32),
    /// A task waits for a permission prompt.
    Prompt(Capability),
}

/// Run until nothing can run, waiting for pending I/O.
pub(crate) struct Idle;

/// Run until nothing can run without waiting for I/O: the tasks left at
/// the end of a run unwind ([`Vm::end_tasks`]).
pub(crate) struct EndTasks;

macro_rules! idle_until {
    ($($t:ty: $wait:expr),*) => {$(
        impl Until for $t {
            type Stop = IdleStop;
            fn exit(code: i32) -> IdleStop {
                IdleStop::Exit(code)
            }
            fn finished(&self, _: u32, v: Value) -> Result<IdleStop, Value> {
                Err(v)
            }
            fn prompt(&self, cap: Capability) -> Result<IdleStop, Capability> {
                Ok(IdleStop::Prompt(cap))
            }
            fn idle(&self) -> Result<IdleStop, u32> {
                Ok(IdleStop::Idle)
            }
            fn before_wait(&self, _: &Vm) -> Option<IdleStop> {
                $wait
            }
        }
    )*};
}
idle_until!(Idle: None, EndTasks: Some(IdleStop::Idle));

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
    fn apply_completions(&mut self, done: Vec<(u32, u64, Finisher)>, timer: &RunTimer) -> Result<(), Ending> {
        if done.is_empty() {
            return Ok(());
        }
        self.meter.start_slice(timer.cpu_deadline(self.host.limits.max_cpu_time), self.deadline);
        for (fid, wgen, fin) in done {
            if !wait_is_live(&self.fibers, (fid, wgen)) {
                continue;
            }
            self.local_done(fid);
            // A Rust panic in the finisher (a bug) becomes an internal error.
            let r = match crate::unwind::catch(|| self.unchecked(fin)) {
                Ok(r) => r,
                Err(msg) => {
                    self.broken = true;
                    // The panic skipped the restore of the scope, and no scope is around it.
                    self.heap.unchecked = false;
                    Err(Ctl::internal(msg))
                }
            };
            match r {
                Ok(v) => {
                    self.deliver(fid, wgen, v).map_err(Trap::Diverged)?;
                    self.enforce_limit(None).map_err(Trap::Limit)?;
                }
                Err(c) => return Err(self.op_ending(fid, c)),
            }
        }
        Ok(())
    }

    /// How the failure `c` of an I/O operation of fiber `fid` ends the run.
    fn op_ending(&mut self, fid: u32, c: Ctl) -> Ending {
        self.ending_of(c, |vm| match vm.fibers.get(fid as usize) {
            Some(Some(f)) => vm.trace_of(f, None),
            _ => vec![],
        })
    }

    /// Waits for the journal to make its entries durable. The wait does not
    /// count against the wall-time limit.
    pub(crate) async fn flush_journal(&mut self) -> Result<(), Trap> {
        let started = self.host.clock.now();
        let Some(mut fut) = self.journal_state().and_then(|mut j| j.flush()) else { return Ok(()) };
        let (interrupt, shared) = (self.host.interrupt.clone(), self.io.shared.clone());
        let r = std::future::poll_fn(|cx| {
            shared.set_waker(cx.waker());
            if interrupt.load(Ordering::Relaxed) {
                return Poll::Ready(None);
            }
            fut.as_mut().poll(cx).map(Some)
        })
        .await;
        if let Some(d) = &mut self.deadline {
            *d += self.host.clock.now().saturating_duration_since(started);
        }
        match r {
            None => Err(Trap::Interrupted),
            Some(Err(e)) => Err(Trap::Abort(e)),
            Some(Ok(())) => Ok(()),
        }
    }

    /// Delivers the completions in `done`, or holds them while replay is not
    /// over (see [`Vm::hold_in_replay`]).
    fn complete(&mut self, done: Vec<(u32, u64, Finisher)>, timer: &RunTimer) -> Result<(), Ending> {
        let now = self.hold_in_replay(done);
        self.apply_completions(now, timer)
    }

    /// Lets fiber `fid` (`f`) wait for `wait`.
    async fn start_wait(&mut self, fid: u32, mut f: Box<Fiber>, wait: Wait, timer: &RunTimer) -> Result<(), Ending> {
        let g = self.next_gen();
        f.wgen = g;
        f.state = FiberState::Waiting;
        f.jop = wait.op;
        self.fibers[fid as usize] = Some(f);
        let (fut, effects) = match wait.work {
            WaitWork::Replay(build) => {
                if let (Some(op), Some(mut j)) = (wait.op, self.journal_state()) {
                    j.replayed.insert(op, (fid, g, super::journal::Replaying::Recorded(build)));
                }
                return Ok(());
            }
            WaitWork::Run { fut, effects } => (fut, effects),
        };
        // The journal holds the operation's `Start` entry. Making it durable
        // with the inputs before it gives the effect the same sequence
        // number in a resumed run, whatever completions that run delivers
        // first.
        if effects == Effects::External && self.must_flush() {
            self.flush_journal().await?;
        }
        let added = self.io.add(fid, g, fut, wait.parking);
        match added {
            Ok(io) => {
                if let Some(f) = self.fibers[fid as usize].as_mut() {
                    f.io = Some(io);
                }
                if wait.op.is_none() {
                    self.local_pending(fid, g).map_err(|c| self.op_ending(fid, c))?;
                }
                Ok(())
            }
            Err(fin) => self.complete(vec![(fid, g, fin)], timer),
        }
    }

    /// Delivers the completions that replay holds for this point, and once
    /// replay is over, those held back while it was not. Waits for a local
    /// operation that completes at this point. Returns whether it delivered
    /// any.
    async fn journal_checkpoint(&mut self, timer: &RunTimer) -> Result<bool, Ending> {
        let mut any = false;
        // One at a time: delivering one can cancel the task of another.
        loop {
            match self.next_due().map_err(|c| self.ending_of(c, |_| vec![]))? {
                Due::Nothing => break,
                Due::Ready(fid, wgen, fin) => {
                    any = true;
                    self.apply_completions(vec![(fid, wgen, fin)], timer)?;
                }
                Due::Waiting => {
                    let done = self.wait_for_io(timer).await;
                    self.complete(done, timer)?;
                    self.check_limits(timer)?;
                }
            }
        }
        if !self.replaying() {
            let held = self.journal_state().map(|mut j| std::mem::take(&mut j.held)).unwrap_or_default();
            any |= !held.is_empty();
            self.apply_completions(held, timer)?;
        }
        Ok(any)
    }

    /// Waits until an I/O operation completes, the host interrupts the
    /// run, or the wall-time limit is reached. Returns the completions.
    async fn wait_for_io(&mut self, timer: &RunTimer) -> Vec<(u32, u64, Finisher)> {
        let blocked = self.host.stdin_feed.as_ref().map(|f| f.blocked());
        let mut sleep = self.deadline.map(|d| timer.sleep(d.saturating_duration_since(timer.now())));
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
        done
    }

    /// Whether a journaled run must be suspended now that nothing can run:
    /// if every pending operation may be, and none is due within the
    /// configured threshold.
    fn suspension(&self) -> Option<Suspension> {
        let j = self.journal.as_ref()?.state();
        let after = j.suspend_after?;
        if j.replaying() || !j.held.is_empty() {
            return None;
        }
        match self.io.park()? {
            Park::Until(t) if t.duration_since(j.wall(&*self.host.clock)).unwrap_or_default() <= after => None,
            Park::Until(t) => Some(Suspension { wake_at: Some(t), parked: self.io.parked_calls() }),
            Park::UntilResumed => Some(Suspension { wake_at: None, parked: self.io.parked_calls() }),
        }
    }

    /// Runs `proto` (a zero-argument function) as the main task until it
    /// finishes, running the other tasks meanwhile, then returns (other
    /// tasks are left as they are). The wall-time limit is `self.deadline`.
    /// Buffered standard output is flushed at the end.
    pub async fn run_main<S: Suspends>(&mut self, proto: u32) -> Result<RunEnd<S>, Trap> {
        let closure = self.heap.alloc(Obj::Closure(proto, Box::new([])));
        self.run_call(closure, &[]).await
    }

    /// Like [`Vm::run_main`], with `closure` called with `args` as the main
    /// task. A closure with captures takes no arguments: its captures fill
    /// the first registers.
    pub(crate) async fn run_call<S: Suspends>(&mut self, closure: Value, args: &[Value]) -> Result<RunEnd<S>, Trap> {
        let r = match self.start_main(closure, args) {
            Ok(fid) => self.drive(Main::<S>(fid, std::marker::PhantomData)).await,
            Err(t) => Err(t),
        };
        let drained = self.drain_output();
        match drained {
            Err(c) if r.is_ok() => self.ending_of(c, |_| vec![]).into_run_end(),
            _ => r,
        }
    }

    fn start_main(&mut self, closure: Value, args: &[Value]) -> Result<u32, Trap> {
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: 0, state: TaskState::Running, result: Value::Unit })));
        let fid = self.spawn_fiber(closure, task).map_err(|c| match c.as_limit() {
            Some(k) => Trap::Limit(k),
            None => Trap::Internal { message: "cannot start main task".into(), trace: vec![] },
        })?;
        if let Some(f) = self.fibers[fid as usize].as_mut() {
            f.stack[..args.len()].copy_from_slice(args);
        }
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.fiber = fid;
        }
        self.main_fiber = fid;
        self.live_tasks += 1;
        Ok(fid)
    }

    /// Runs the ready tasks and pending I/O as `until` says. Without a main
    /// task, also stops when a task needs a permission prompt
    /// ([`IdleStop::Prompt`]). The future may be dropped where it waits:
    /// every task is then in the scheduler's tables.
    pub(crate) async fn drive<U: Until>(&mut self, until: U) -> Result<U::Stop, Trap> {
        match self.run_tasks(&until).await {
            Ok(stop) => Ok(stop),
            Err(Ending::Exit(code)) => Ok(U::exit(code)),
            Err(Ending::Trap(t)) => Err(t),
        }
    }

    /// The loop of [`Vm::drive`], which ends with a stop or how the run ends.
    async fn run_tasks<U: Until>(&mut self, until: &U) -> Result<U::Stop, Ending> {
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
                self.count_step();
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
                let exit = r?;
                if !matches!(exit, Exit::Collect) {
                    gc_retried = None;
                }
                match exit {
                    Exit::Collect => {
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
                    Exit::Done(v) => {
                        self.fibers[fid as usize] = Some(f);
                        match until.finished(fid, v) {
                            Ok(stop) => return Ok(stop),
                            Err(v) => self.finish_task(fid, v).map_err(Trap::Diverged)?,
                        }
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
                    Exit::Wait(w) => self.start_wait(fid, f, *w, &timer).await?,
                    Exit::Prompt(cap) => {
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_front(fid);
                        let cap = match until.prompt(cap) {
                            Ok(stop) => return Ok(stop),
                            Err(cap) => cap,
                        };
                        // Write the script's output before showing the prompt.
                        self.drain_output().map_err(|c| self.ending_of(c, |_| vec![]))?;
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
                                    None => return Err(Trap::Interrupted.into()),
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
                    Exit::Cancelled => {
                        // The task cancelled itself.
                        self.end_cancelled(fid, f);
                        if self.fibers[fid as usize].is_some() {
                            self.ready.push_back(fid);
                        } else if let Ok(stop) = until.finished(fid, Value::Unit) {
                            return Ok(stop);
                        }
                    }
                    Exit::Exited(code) => return Ok(U::exit(code)),
                    Exit::Unwound => {
                        self.free_fibers.push(fid);
                        self.live_tasks = self.live_tasks.saturating_sub(1);
                        self.recycle_fiber(f);
                        if let Ok(stop) = until.finished(fid, Value::Unit) {
                            return Ok(stop);
                        }
                    }
                }
                self.check_limits(&timer)?;
                self.journal_checkpoint(&timer).await?;
                if timer.now.saturating_duration_since(timer.last_yield) > YIELD_EVERY {
                    let done = self.io.poll_ready(None);
                    self.complete(done, &timer)?;
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
            if self.journal_checkpoint(&timer).await? {
                self.check_limits(&timer)?;
                continue;
            }
            if self.replaying() {
                if let Ok(stop) = until.idle() {
                    return Ok(stop);
                }
                return Err(self
                    .blocked_in_replay()
                    .map_or_else(|| Trap::Internal { message: "replay stalled".into(), trace: vec![] }, Trap::Diverged)
                    .into());
            }
            if self.io.is_empty() {
                let main = match until.idle() {
                    Ok(stop) => return Ok(stop),
                    Err(main) => main,
                };
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
            if let Some(stop) = until.before_wait(self) {
                return Ok(stop);
            }
            timer.read();
            let done = self.wait_for_io(&timer).await;
            timer.resume();
            timer.last_yield = timer.now;
            self.apply_completions(done, &timer)?;
            self.check_limits(&timer)?;
        }
    }
}
