//! Enforces execution limits and cancellation.
//!
//! Checking system clocks and atomic flags on every bytecode instruction is
//! expensive. [`VmMeter`] amortizes checks through a single fuel counter that
//! enforces instruction, CPU-time, and wall-clock limits.
//!
//! ## How Fuel Works
//!
//! - [`VmMeter`] counts total fuel spent upward in `total_fuel`.
//! - At the start of a fiber turn, the scheduler sets `slice_end_fuel` and
//!   `next_check_fuel` to `total_fuel + SLICE_FUEL`, capped at
//!   `max_instructions`.
//! - The interpreter keeps a local countdown (`next_check_fuel - total_fuel`)
//!   in a register and deducts fuel at calls and loop jumps. Before a native
//!   call or at the end of a slice, it commits spent bytecode fuel back into
//!   `total_fuel`. After a native call, it reloads its countdown.
//! - If `total_fuel` exceeds `next_check_fuel`, the meter checks the interrupt
//!   flag, instruction limit, and CPU and wall-clock deadlines, then advances
//!   `next_check_fuel` by [`LIMIT_CHECK_FUEL_INTERVAL`].
//! - Because `slice_end_fuel` is tracked separately from `next_check_fuel`,
//!   synchronous callbacks can keep running past the slice limit while the
//!   outer fiber still preempts after the native call returns.
//!
//! ## Cost Model
//!
//! A fuel unit corresponds to a simple bytecode loop iteration. Native
//! functions and runtime helpers must charge fuel in proportion to their work
//! so they cannot bypass execution limits.
//!
//! - [`VmMeter::charge`] charges a unit for each logical item processed, such as
//!   a container element, traversal node, or match.
//! - [`VmMeter::charge_bytes`] charges a unit per [`BYTES_PER_FUEL_UNIT`] bytes
//!   for bulk memory work such as copying, comparing, or scanning.
//! - [`VmMeter::charge_slow_bytes`] charges a unit per
//!   [`SLOW_BYTES_PER_FUEL_UNIT`] bytes for per-byte algorithms such as
//!   hashing, regex search, and encoding.
//!
//! Charge the estimated cost of an uninterruptible operation before starting
//! it. The operation then stops immediately if the remaining budget is too
//! small. If you cannot estimate cost in advance, meter the operation by
//! elapsed time. See `stdlib/regex.rs` for an example.
//!
//! Shared substructure can cause formatting, encoding, or comparison to expand
//! a small heap graph exponentially. Charge fuel as you traverse nodes or emit
//! output rather than by the input's heap size.

use super::Fail;
use crate::engine::LimitKind;
use crate::platform::{Clock, Deadline, Instant};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Trait for metering fuel.
pub trait Meter {
    /// Charges fuel for runtime work.
    fn charge(&self, units: u64) -> Result<(), Fail>;

    /// Charges fuel for bulk memory work.
    #[inline]
    fn charge_bytes(&self, byte_count: usize) -> Result<(), Fail> {
        self.charge((byte_count / BYTES_PER_FUEL_UNIT) as u64)
    }
}

/// Tracks fuel and enforces limits inside the VM.
pub struct VmMeter {
    /// Fuel spent in the current run.
    spent_fuel: Cell<u64>,
    /// Fuel at which the next limit check runs.
    next_check_fuel: Cell<u64>,
    /// Fuel at which the current fiber slice ends.
    slice_fuel: Cell<u64>,
    /// Starting countdown handed to the interpreter at the last sync boundary.
    interpreter_fuel_baseline: Cell<i64>,
    max_instructions: Option<u64>,
    cpu_deadline: Cell<Option<Duration>>,
    wall_deadline: Cell<Option<Instant>>,
    interrupt_flag: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
}

impl VmMeter {
    /// Creates a meter.
    pub fn new(max_instructions: Option<u64>, interrupt_flag: Arc<AtomicBool>, clock: Arc<dyn Clock>) -> VmMeter {
        VmMeter {
            spent_fuel: Cell::new(0),
            next_check_fuel: Cell::new(0),
            slice_fuel: Cell::new(0),
            interpreter_fuel_baseline: Cell::new(0),
            max_instructions,
            cpu_deadline: Cell::new(None),
            wall_deadline: Cell::new(None),
            interrupt_flag,
            clock,
        }
    }

    /// Charges fuel for work and enforces execution limits.
    #[inline]
    pub fn charge(&self, units: u64) -> Result<(), Fail> {
        let used = self.spent_fuel.get().saturating_add(units);
        self.spent_fuel.set(used);
        if used > self.next_check_fuel.get() { self.advance_limit_check() } else { Ok(()) }
    }

    /// Charges fuel for bulk memory work like copying, comparing, or scanning.
    #[inline]
    pub fn charge_bytes(&self, byte_count: usize) -> Result<(), Fail> {
        self.charge((byte_count / BYTES_PER_FUEL_UNIT) as u64)
    }

    /// Charges fuel for per-byte work like hashing, regex search, or encoding.
    #[inline]
    pub fn charge_slow_bytes(&self, byte_count: usize) -> Result<(), Fail> {
        self.charge((byte_count / SLOW_BYTES_PER_FUEL_UNIT) as u64)
    }

    /// Fails if execution must stop due to cancellation,
    /// `max_instructions`, CPU time, or wall-clock time.
    pub fn check_limits(&self) -> Result<(), Fail> {
        if self.interrupt_flag.load(Ordering::Relaxed) {
            return Err(Fail::interrupted());
        }
        if let Some(max) = self.max_instructions
            && self.spent_fuel.get() > max
        {
            return Err(Fail::limit(LimitKind::Instructions));
        }
        if let Some(deadline) = self.cpu_deadline.get()
            && deadline <= self.clock.cpu_time()
        {
            return Err(Fail::limit(LimitKind::CpuTime));
        }
        if let Some(deadline) = self.wall_deadline.get()
            && deadline <= self.clock.now()
        {
            return Err(Fail::limit(LimitKind::WallTime));
        }
        Ok(())
    }

    /// Returns `true` if the current fiber slice has spent its fuel allowance.
    #[inline]
    pub fn is_slice_exhausted(&self) -> bool {
        self.spent_fuel.get() >= self.slice_fuel.get()
    }

    /// Returns fuel spent across the current run.
    #[inline]
    pub fn total_fuel_used(&self) -> u64 {
        self.spent_fuel.get()
    }

    /// Begins a scheduling slice with a fuel allowance and time deadline.
    ///
    /// The scheduler calls this before each fiber turn. The fiber yields
    /// after `SLICE_FUEL` units or stops at `max_instructions`. Returns the
    /// starting fuel for the interpreter's local counter.
    pub(crate) fn start_slice(&self, cpu_deadline: Option<Duration>, wall_deadline: Option<Instant>) -> i64 {
        let end = self.cap_target(self.spent_fuel.get().saturating_add(super::SLICE_FUEL as u64));
        self.slice_fuel.set(end);
        self.next_check_fuel.set(end);
        self.cpu_deadline.set(cpu_deadline);
        self.wall_deadline.set(wall_deadline);
        self.reload_interpreter_fuel()
    }

    /// Checks limits and sets the next check window.
    pub(crate) fn refill_after_limit_check(&self) -> Result<i64, Fail> {
        self.advance_limit_check()?;
        Ok(self.reload_interpreter_fuel())
    }

    /// Commits bytecode fuel spent in the interpreter's local counter back into
    /// the meter.
    ///
    /// Call this before a native call or at the end of a slice so the meter
    /// holds the full run total before checking limits or running native code.
    /// Pass the interpreter's current `interpreter_fuel` counter.
    #[inline]
    pub(crate) fn flush_interpreter_fuel(&self, interpreter_fuel: i64) {
        let spent = self.interpreter_fuel_baseline.get().saturating_sub(interpreter_fuel).max(0) as u64;
        self.spent_fuel.set(self.spent_fuel.get().saturating_add(spent));
    }

    /// Synchronizes the interpreter's local counter with fuel remaining until
    /// `next_check_fuel`.
    ///
    /// Call this before entering or resuming the interpreter. The returned
    /// value becomes the interpreter's local counter and the new sync baseline.
    #[inline]
    pub(crate) fn reload_interpreter_fuel(&self) -> i64 {
        let remaining = self.next_check_fuel.get().saturating_sub(self.spent_fuel.get()).min(i64::MAX as u64) as i64;
        self.interpreter_fuel_baseline.set(remaining);
        remaining
    }

    /// Clears accumulated run fuel and installs a fresh interrupt flag to begin
    /// a new session run.
    pub(crate) fn reset_for_run(&mut self, interrupt_flag: Arc<AtomicBool>) {
        self.spent_fuel.set(0);
        self.next_check_fuel.set(0);
        self.slice_fuel.set(0);
        self.interpreter_fuel_baseline.set(0);
        self.interrupt_flag = interrupt_flag;
    }

    /// Creates a [`WorkerMeter`] so work running off the VM thread can
    /// enforce execution limits independently.
    pub(crate) fn worker_meter(&self) -> WorkerMeter {
        WorkerMeter {
            interrupt_flag: self.interrupt_flag.clone(),
            abandoned_flag: Arc::new(AtomicBool::new(false)),
            wall_deadline: self.wall_deadline.get().map(|at| Deadline { at, clock: self.clock.clone() }),
            fuel_limit: self.max_instructions.map_or(u64::MAX, |max| max.saturating_sub(self.spent_fuel.get())),
            fuel_used: 0,
            next_deadline_check_fuel: 0,
            stopped: false,
        }
    }

    /// Caps `target` at `max_instructions`, if set.
    #[inline]
    fn cap_target(&self, target: u64) -> u64 {
        self.max_instructions.map_or(target, |max| target.min(max))
    }

    /// Checks execution limits and advances `next_check_fuel` by one interval.
    #[cold]
    #[inline(never)]
    fn advance_limit_check(&self) -> Result<(), Fail> {
        self.check_limits()?;
        self.next_check_fuel.set(self.cap_target(self.spent_fuel.get().saturating_add(LIMIT_CHECK_FUEL_INTERVAL as u64)));
        Ok(())
    }
}

impl Meter for VmMeter {
    #[inline]
    fn charge(&self, units: u64) -> Result<(), Fail> {
        VmMeter::charge(self, units)
    }
}

/// A [`Meter`] that ignores all charges.
pub struct UnlimitedMeter;

impl Meter for UnlimitedMeter {
    #[inline(always)]
    fn charge(&self, _: u64) -> Result<(), Fail> {
        Ok(())
    }
}

/// Computes extra fuel needed to execute bytecode between safe points.
///
/// The interpreter deducts base fuel at calls and loop back edges.
/// This surcharge is proportional to block length. Callers add it to the base
/// cost of entering a function, starting a task, or copying a closure capture.
/// Blocks shorter than 64 instructions add zero surcharge.
#[inline(always)]
pub const fn code_fuel_surcharge(instructions: usize) -> i64 {
    (instructions >> INSTRUCTIONS_PER_FUEL_SHIFT) as i64
}

/// Computes the total fuel cost of a loop back edge so long loop bodies cannot
/// bypass execution limits.
///
/// Pass the negative jump offset of the backward branch. The returned cost
/// includes the base unit for the back edge plus an extra unit for each 64
/// instructions jumped over.
#[inline(always)]
pub const fn back_edge_fuel(back_offset: i32) -> i64 {
    -((back_offset >> INSTRUCTIONS_PER_FUEL_SHIFT) as i64)
}

/// Bytes charged as a single fuel unit for bulk memory work such as copying,
/// comparing, and scanning.
pub const BYTES_PER_FUEL_UNIT: usize = 64;

/// Bytes charged as a single fuel unit for per-byte work such as hashing,
/// regex search, and encoding.
pub const SLOW_BYTES_PER_FUEL_UNIT: usize = 8;

/// Fuel units spent between limit checks after the current slice is exhausted.
pub const LIMIT_CHECK_FUEL_INTERVAL: i64 = 20_000;

/// Bit shift for straight-line bytecode instructions per fuel unit: `1 << 6`,
/// or 64 instructions.
pub const INSTRUCTIONS_PER_FUEL_SHIFT: u32 = 6;

/// Enforces fuel, deadline, and cancellation limits for blocking work offloaded
/// to a worker thread.
///
/// [`VmMeter`] is bound to the VM thread, so offloaded jobs use this instead.
/// A charge returns [`WorkerStopped`] if the engine is interrupted, the clock
/// passes the wall-clock deadline, the worker exceeds the starting instruction
/// budget, or the VM stops waiting. After the job finishes, the VM charges its
/// [`VmMeter`] for [`WorkerMeter::fuel_used`].
pub(crate) struct WorkerMeter {
    interrupt_flag: Arc<AtomicBool>,
    abandoned_flag: Arc<AtomicBool>,
    wall_deadline: Option<Deadline>,
    fuel_limit: u64,
    fuel_used: u64,
    next_deadline_check_fuel: u64,
    stopped: bool,
}

/// Signal that a worker hit an execution limit or cancellation and
/// must stop immediately.
#[derive(Debug)]
pub(crate) struct WorkerStopped;

impl WorkerMeter {
    /// Charges fuel for work.
    /// Returns [`WorkerStopped`] if the worker must abort.
    pub fn charge(&mut self, units: u64) -> Result<(), WorkerStopped> {
        self.fuel_used = self.fuel_used.saturating_add(units);
        self.stopped |=
            self.fuel_used > self.fuel_limit || self.interrupt_flag.load(Ordering::Relaxed) || self.abandoned_flag.load(Ordering::Relaxed);
        if !self.stopped && self.fuel_used >= self.next_deadline_check_fuel {
            self.next_deadline_check_fuel = self.fuel_used.saturating_add(LIMIT_CHECK_FUEL_INTERVAL as u64 / 16);
            self.stopped = self.wall_deadline.as_ref().is_some_and(Deadline::passed);
        }
        if self.stopped { Err(WorkerStopped) } else { Ok(()) }
    }

    /// Charges fuel for bulk memory work on a worker thread.
    pub fn charge_bytes(&mut self, byte_count: usize) -> Result<(), WorkerStopped> {
        self.charge((byte_count / BYTES_PER_FUEL_UNIT) as u64)
    }

    /// Returns `true` if a previous charge triggered a stop.
    /// The VM uses this flag to discard the offloaded result.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Returns total fuel charged on the worker thread.
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
    }

    /// Returns a shared flag that the VM sets to cancel the worker after the VM
    /// stops waiting for it.
    #[cfg(feature = "os")]
    pub fn abandon_flag(&self) -> Arc<AtomicBool> {
        self.abandoned_flag.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FailKind, SLICE_FUEL};
    use super::*;

    fn system_clock() -> Arc<dyn Clock> {
        Arc::new(crate::platform::SystemClock)
    }

    #[test]
    fn charges_are_free_until_the_slice_is_spent() {
        let meter = VmMeter::new(Some(100_000), Arc::new(AtomicBool::new(false)), system_clock());
        assert_eq!(meter.start_slice(None, None), SLICE_FUEL);
        assert!(meter.charge(50).is_ok());
        assert!(!meter.is_slice_exhausted());
        assert_eq!((meter.reload_interpreter_fuel(), meter.total_fuel_used()), (SLICE_FUEL - 50, 50));
        assert!(meter.charge(SLICE_FUEL as u64).is_ok());
        assert!(meter.is_slice_exhausted());
        assert_eq!(meter.total_fuel_used(), SLICE_FUEL as u64 + 50);
        assert!(matches!(meter.charge(100_000).map_err(Fail::into_kind), Err(FailKind::Limit(LimitKind::Instructions))));
    }

    #[test]
    fn the_instruction_limit_is_checked_exactly() {
        let meter = VmMeter::new(Some(1000), Arc::new(AtomicBool::new(false)), system_clock());
        assert_eq!(meter.start_slice(None, None), 1000);
        meter.flush_interpreter_fuel(700);
        assert_eq!(meter.total_fuel_used(), 300);
        assert!(meter.charge(700).is_ok(), "1000 units used");
        assert!(matches!(meter.charge(1).map_err(Fail::into_kind), Err(FailKind::Limit(LimitKind::Instructions))));
        assert_eq!(meter.reload_interpreter_fuel(), 0);
        let meter = VmMeter::new(Some(1_000_000), Arc::new(AtomicBool::new(false)), system_clock());
        meter.start_slice(None, None);
        assert!(meter.charge(SLICE_FUEL as u64 + 1).is_ok());
        assert_eq!(meter.next_check_fuel.get(), SLICE_FUEL as u64 + 1 + LIMIT_CHECK_FUEL_INTERVAL as u64);
        assert!(meter.charge(1_000_000 - SLICE_FUEL as u64 - 1).is_ok(), "1,000,000 units used");
        assert!(matches!(meter.charge(1).map_err(Fail::into_kind), Err(FailKind::Limit(LimitKind::Instructions))));
        let mut meter = VmMeter::new(Some(100), Arc::new(AtomicBool::new(false)), system_clock());
        assert!(meter.charge(60).is_ok());
        assert_eq!(meter.start_slice(None, None), 40);
        meter.reset_for_run(Arc::new(AtomicBool::new(false)));
        assert_eq!((meter.total_fuel_used(), meter.start_slice(None, None)), (0, 100));
    }

    #[test]
    fn refill_after_limit_check_preserves_slice_exhaustion() {
        let meter = VmMeter::new(None, Arc::new(AtomicBool::new(false)), system_clock());
        assert_eq!(meter.start_slice(None, None), SLICE_FUEL);
        meter.flush_interpreter_fuel(0);
        assert!(meter.is_slice_exhausted());
        let next = meter.refill_after_limit_check().unwrap();
        assert_eq!(next, LIMIT_CHECK_FUEL_INTERVAL);
        assert!(meter.is_slice_exhausted());
        meter.flush_interpreter_fuel(next - 3_000);
        assert_eq!(meter.total_fuel_used(), SLICE_FUEL as u64 + 3_000);
    }

    #[test]
    fn interrupt_is_seen_at_checkpoints() {
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        let meter = VmMeter::new(None, interrupt_flag.clone(), system_clock());
        meter.start_slice(None, None);
        interrupt_flag.store(true, Ordering::SeqCst);
        assert!(meter.charge(5).is_ok(), "limit check is skipped while slice fuel remains");
        assert!(matches!(meter.charge(SLICE_FUEL as u64).map_err(Fail::into_kind), Err(FailKind::Interrupted)));
    }

    #[test]
    fn deadlines() {
        let meter = VmMeter::new(None, Arc::new(AtomicBool::new(false)), system_clock());
        meter.start_slice(Some(system_clock().cpu_time()), None);
        assert!(matches!(meter.charge(SLICE_FUEL as u64 + 1).map_err(Fail::into_kind), Err(FailKind::Limit(LimitKind::CpuTime))));
        meter.start_slice(None, Some(Instant::now()));
        assert!(matches!(meter.charge(SLICE_FUEL as u64 + 1).map_err(Fail::into_kind), Err(FailKind::Limit(LimitKind::WallTime))));
    }
}
