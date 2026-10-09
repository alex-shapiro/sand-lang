//! The task scheduler: runs fibers round-robin and drives pending I/O.

use super::interp::Exit;
use super::*;
use crate::LimitKind;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

/// State shared with wakers and interrupt handles.
#[derive(Default)]
pub struct IoShared {
    ready: Mutex<Vec<u64>>,
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

struct IdWaker {
    id: u64,
    shared: Arc<IoShared>,
}

impl Wake for IdWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.shared.ready.lock().unwrap_or_else(|e| e.into_inner()).push(self.id);
        self.shared.wake();
    }
}

#[derive(Default)]
pub struct IoTable {
    next_id: u64,
    futures: HashMap<u64, (u32, u64, BoxFuture)>,
    pub shared: Arc<IoShared>,
}

impl IoTable {
    pub fn with_shared(shared: Arc<IoShared>) -> IoTable {
        IoTable { next_id: 0, futures: HashMap::new(), shared }
    }

    pub fn add(&mut self, fid: u32, wgen: u64, fut: BoxFuture) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.futures.insert(id, (fid, wgen, fut));
        self.shared.ready.lock().unwrap_or_else(|e| e.into_inner()).push(id);
        id
    }

    pub fn cancel(&mut self, id: u64) {
        self.futures.remove(&id);
    }

    pub fn is_empty(&self) -> bool {
        self.futures.is_empty()
    }

    /// Cancels every pending operation.
    pub fn clear(&mut self) {
        self.futures.clear();
    }

    /// Polls futures that were woken. Returns completed operations.
    pub fn poll_ready(&mut self, task_waker: Option<&Waker>) -> Vec<(u32, u64, Finisher)> {
        if let Some(w) = task_waker {
            self.shared.set_waker(w);
        }
        let ids: Vec<u64> = std::mem::take(&mut *self.shared.ready.lock().unwrap_or_else(|e| e.into_inner()));
        let mut done = Vec::new();
        for id in ids {
            let Some(entry) = self.futures.get_mut(&id) else { continue };
            let waker = Waker::from(Arc::new(IdWaker { id, shared: self.shared.clone() }));
            let mut cx = Context::from_waker(&waker);
            if let Poll::Ready(fin) = entry.2.as_mut().poll(&mut cx) {
                let (fid, wgen, _) = self.futures.remove(&id).unwrap();
                done.push((fid, wgen, fin));
            }
        }
        done
    }
}

/// Slices run between clock readings at most (see `Vm::run_main`).
const CLOCK_SLICES: u32 = 32;

/// Result of running the program's main task.
pub enum RunEnd {
    Finished(Value),
    Exit(i32),
}

impl Vm {
    fn check_limits(&self, start: Instant, paused: Duration, cpu: Duration) -> Result<(), Trap> {
        if self.host.interrupt.load(Ordering::Relaxed) {
            return Err(Trap::Interrupted);
        }
        let l = &self.host.limits;
        if let Some(max) = l.max_instructions
            && self.fuel_used > max
        {
            return Err(Trap::Limit(LimitKind::Instructions));
        }
        if let Some(max) = l.max_cpu_time
            && cpu > max
        {
            return Err(Trap::Limit(LimitKind::CpuTime));
        }
        if let Some(max) = l.max_wall_time
            && start.elapsed().saturating_sub(paused) > max
        {
            return Err(Trap::Limit(LimitKind::WallTime));
        }
        Ok(())
    }

    fn apply_completions(&mut self, done: Vec<(u32, u64, Finisher)>) -> Result<(), Trap> {
        // Finishers charge the meter for building their values between
        // slices: count that fuel now (the next slice resets the meter).
        let spent = self.meter.spent();
        let r = self.apply_finishers(done);
        self.fuel_used += self.meter.spent().saturating_sub(spent);
        r
    }

    fn apply_finishers(&mut self, done: Vec<(u32, u64, Finisher)>) -> Result<(), Trap> {
        for (fid, wgen, fin) in done {
            let still_waiting = matches!(self.fibers.get(fid as usize), Some(Some(f)) if f.state == FiberState::Waiting && f.wgen == wgen);
            if !still_waiting {
                continue;
            }
            // A Rust panic in the finisher (a bug) becomes a script panic.
            let r = match crate::unwind::catch(|| fin(self)) {
                Ok(r) => r,
                Err(msg) => {
                    self.caught_panic = true;
                    Err(Ctl::Panic(format!("internal error: {msg}")))
                }
            };
            match r {
                Ok(v) => {
                    self.deliver(fid, wgen, v);
                }
                Err(Ctl::Panic(m)) => {
                    let trace = match &self.fibers[fid as usize] {
                        Some(f) => self.trace_of(f, None),
                        None => vec![],
                    };
                    return Err(Trap::Panic { message: m, trace });
                }
                Err(Ctl::Limit(k)) => return Err(Trap::Limit(k)),
                Err(Ctl::Interrupted) => return Err(Trap::Interrupted),
                Err(Ctl::Exit(c)) => return Err(Trap::Exit(c)),
                Err(other) => {
                    return Err(Trap::Panic {
                        message: format!("internal error: unexpected {other:?} from an I/O operation"),
                        trace: vec![],
                    });
                }
            }
        }
        Ok(())
    }

    /// Runs `proto` (a zero-argument function) as the main task until it
    /// finishes, then returns. Other tasks are abandoned. Buffered standard
    /// output is flushed at the end.
    pub async fn run_main(&mut self, proto: u32) -> Result<RunEnd, Trap> {
        let r = self.run_main_inner(proto).await;
        self.flush_stdout();
        r
    }

    async fn run_main_inner(&mut self, proto: u32) -> Result<RunEnd, Trap> {
        let closure = self.heap.alloc(Obj::Closure(proto, Box::new([])));
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: 0, state: TaskState::Running, result: Value::Unit })));
        let fid = self.spawn_fiber(closure, task).map_err(|c| match c {
            Ctl::Limit(k) => Trap::Limit(k),
            _ => Trap::Panic { message: "internal error: cannot start main task".into(), trace: vec![] },
        })?;
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.fiber = fid;
        }
        self.main_fiber = fid;
        self.live_tasks += 1;
        let start = Instant::now();
        let mut paused = Duration::ZERO;
        let mut cpu = Duration::ZERO;
        // CPU time is measured from `now`, the last clock reading. Short
        // slices (tasks that finish or block quickly) do not read the clock
        // each: it is read after `CLOCK_SLICES` slices or `SLICE_FUEL` units
        // of fuel, and before and after anything that waits, so CPU time is
        // never more than about one slice behind.
        let mut now = Instant::now();
        let mut last_yield = now;
        let (mut unclocked_slices, mut unclocked_fuel) = (0u32, 0u64);
        // Where an instruction was retried after a memory limit error, until
        // the task makes progress.
        let mut gc_retried: Option<(u32, usize, u32)> = None;
        loop {
            while let Some(fid) = self.ready.pop_front() {
                let Some(mut f) = self.fibers.get_mut(fid as usize).and_then(|f| f.take()) else { continue };
                if f.state != FiberState::Ready {
                    self.fibers[fid as usize] = Some(f);
                    continue;
                }
                self.current = fid;
                // Near the instruction limit, the slice ends one unit past it.
                let l = &self.host.limits;
                let slice = l
                    .max_instructions
                    .map_or(SLICE_FUEL, |m| m.saturating_sub(self.fuel_used).saturating_add(1).min(SLICE_FUEL as u64) as i64);
                let mut fuel = slice;
                let t0 = now;
                // Deadlines let long natives check the time limits mid-call.
                let cpu_deadline = l.max_cpu_time.and_then(|m| t0.checked_add(m.saturating_sub(cpu)));
                let wall_deadline = l.max_wall_time.and_then(|m| start.checked_add(paused.saturating_add(m)));
                self.meter.start_slice(SLICE_FUEL, slice, self.fuel_used, cpu_deadline, wall_deadline);
                // A Rust panic in the interpreter or a native (a bug) ends
                // the run as a panic instead of unwinding into the host;
                // caught per slice, which costs nothing per native call.
                let r = match crate::unwind::catch(|| self.interp(&mut f, &mut fuel)) {
                    Ok(r) => r,
                    Err(msg) => {
                        self.caught_panic = true;
                        Err(Trap::Panic { message: format!("internal error: {msg}"), trace: self.trace_of(&f, None) })
                    }
                };
                let spent = (slice - fuel).max(0) as u64 + self.meter.spent();
                self.fuel_used += spent;
                unclocked_slices += 1;
                unclocked_fuel += spent;
                if unclocked_slices >= CLOCK_SLICES || unclocked_fuel >= SLICE_FUEL as u64 {
                    now = Instant::now();
                    cpu += now.saturating_duration_since(t0);
                    (unclocked_slices, unclocked_fuel) = (0, 0);
                }
                self.current = u32::MAX;
                let exit = match r {
                    Ok(e) => {
                        gc_retried = None;
                        e
                    }
                    Err(Trap::Limit(LimitKind::Memory)) if std::mem::take(&mut self.gc_retry) => {
                        // Memory checks count uncollected garbage: collect and
                        // run the failed instruction once more (unless that
                        // is what just failed).
                        let at = (fid, f.frames.len(), f.frames.last().map_or(0, |fr| fr.ip));
                        if gc_retried == Some(at) {
                            return Err(Trap::Limit(LimitKind::Memory));
                        }
                        gc_retried = Some(at);
                        self.collect_all_garbage(Some(&mut f));
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_front(fid);
                        continue;
                    }
                    Err(Trap::Exit(c)) => return Ok(RunEnd::Exit(c)),
                    Err(t) => return Err(t),
                };
                match exit {
                    Exit::Done(v) => {
                        if fid == self.main_fiber {
                            self.fibers[fid as usize] = Some(f);
                            return Ok(RunEnd::Finished(v));
                        }
                        self.fibers[fid as usize] = Some(f);
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
                    Exit::Async(fut) => {
                        let g = self.next_gen();
                        f.wgen = g;
                        f.state = FiberState::Waiting;
                        let id = self.io.add(fid, g, fut);
                        f.io = Some(id);
                        self.fibers[fid as usize] = Some(f);
                    }
                    Exit::Prompt(cap) => {
                        f.state = FiberState::Ready;
                        self.fibers[fid as usize] = Some(f);
                        self.ready.push_front(fid);
                        self.flush_stdout();
                        let prompter = self.host.prompter.clone();
                        let t = Instant::now();
                        cpu += t.saturating_duration_since(now);
                        (unclocked_slices, unclocked_fuel) = (0, 0);
                        let answer = match prompter {
                            Some(p) => {
                                // The host's interrupt stops the run even
                                // while a prompt is pending (the prompter may
                                // never answer); the prompt is dropped.
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
                        now = Instant::now();
                        paused += now.saturating_duration_since(t);
                        self.apply_answer(cap, answer);
                    }
                    Exit::Terminate(c) => return Ok(RunEnd::Exit(c)),
                    Exit::Cancelled => {
                        // The task cancelled itself.
                        let waiters = std::mem::take(&mut f.waiters);
                        self.free_fibers.push(fid);
                        self.live_tasks = self.live_tasks.saturating_sub(1);
                        for (w, g) in waiters {
                            self.deliver_panic(w, g, "task cancelled");
                        }
                        if fid == self.main_fiber {
                            return Ok(RunEnd::Finished(Value::Unit));
                        }
                    }
                }
                self.check_limits(start, paused, cpu)?;
                // Keep I/O flowing and let other host tasks run.
                if now.saturating_duration_since(last_yield) > Duration::from_millis(2) {
                    self.heap.io_memory.publish_heap(self.heap.used());
                    let done = self.io.poll_ready(None);
                    self.apply_completions(done)?;
                    cpu += Instant::now().saturating_duration_since(now);
                    tokio::task::yield_now().await;
                    now = Instant::now();
                    last_yield = now;
                    (unclocked_slices, unclocked_fuel) = (0, 0);
                }
            }
            // Nothing is ready: wait for I/O (with the output so far shown).
            self.flush_stdout();
            if self.io.is_empty() {
                let trace = match self.fibers.get(self.main_fiber as usize) {
                    Some(Some(f)) => self.trace_of(f, None),
                    _ => vec![],
                };
                return Err(Trap::Panic { message: "deadlock: all tasks are blocked".into(), trace });
            }
            cpu += Instant::now().saturating_duration_since(now);
            (unclocked_slices, unclocked_fuel) = (0, 0);
            let remaining = self.host.limits.max_wall_time.map(|m| m.saturating_sub(start.elapsed().saturating_sub(paused)));
            let mut sleep = remaining.map(|r| Box::pin(tokio::time::sleep(r)));
            let interrupt = self.host.interrupt.clone();
            // Pending I/O checks its buffers against the heap's current size.
            self.heap.io_memory.publish_heap(self.heap.used());
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
            self.apply_completions(done)?;
            self.check_limits(start, paused, cpu)?;
            now = Instant::now();
            last_yield = now;
        }
    }
}
