//! Enforces execution limits and cancellation for the SandLang VM.
//!
//! Checking system clocks and atomic flags on every bytecode instruction is
//! expensive. [`Meter`] amortizes checks through a single fuel counter that
//! enforces instruction, CPU-time, and wall-clock limits.
//!
//! ## How Fuel Works
//!
//! - At the start of a fiber's turn, the scheduler grants it a fuel slice.
//! - The interpreter keeps the active fiber's slice fuel in a local counter.
//!   It deducts fuel at calls and loop iterations. Before a native call, the
//!   interpreter commits its bytecode spending. After the call, it reloads its
//!   counter.
//! - Native functions and runtime helpers charge fuel through a shared meter.
//! - Once a slice is exhausted, the meter checks the interrupt flag, instruction
//!   budget, and CPU and wall-clock deadlines every of often. It returns an
//!   interrupt or limit failure if a limit is exceeded.
//!
//! ## Cost Model
//!
//! A fuel unit corresponds to a simple bytecode loop iteration. Native
//! functions and runtime helpers must charge fuel in proportion to their work
//! so they cannot bypass execution limits.
//!
//! - [`Meter::charge`] charges a unit for each logical item processed, such as
//!   a container element, traversal node, or match.
//! - [`Meter::charge_bytes`] charges a unit per [`BYTES_PER_FUEL_UNIT`] bytes
//!   for bulk memory work such as copying, comparing, or scanning.
//! - [`Meter::charge_slow_bytes`] charges a unit per
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
use super::heap::Heap;
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

    /// Returns the current fuel position.
    fn fuel_mark(&self) -> i64;

    /// Restores fuel to `mark` so the VM does not charge for aborted retries or
    /// discarded probes.
    fn refund_to(&self, mark: i64);
}

/// Tracks fuel and enforces limits inside the VM.
pub struct VmMeter {
    /// Remaining fuel in the current slice. Negative if the slice is overspent.
    remaining_slice_fuel: Cell<i64>,
    /// Interpreter fuel counter recorded at the last sync boundary.
    interpreter_fuel_baseline: Cell<i64>,
    /// Threshold below which the next charge triggers a limit checkpoint.
    next_limit_check_fuel: Cell<i64>,
    /// Fuel granted at the start of the current slice.
    slice_initial_fuel: Cell<i64>,
    /// Total fuel spent across earlier slices in the current run.
    prior_slices_fuel: Cell<u64>,
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
            remaining_slice_fuel: Cell::new(0),
            interpreter_fuel_baseline: Cell::new(0),
            next_limit_check_fuel: Cell::new(0),
            slice_initial_fuel: Cell::new(0),
            prior_slices_fuel: Cell::new(0),
            max_instructions,
            cpu_deadline: Cell::new(None),
            wall_deadline: Cell::new(None),
            interrupt_flag,
            clock,
        }
    }

    /// Charges fuel for native or runtime work and enforces execution limits at
    /// periodic checkpoints.
    ///
    /// Returns immediately if remaining fuel is at or above the next
    /// checkpoint. Runs a full limit check if remaining fuel is below the
    /// checkpoint.
    #[inline]
    pub fn charge(&self, units: u64) -> Result<(), Fail> {
        let remaining = self.remaining_slice_fuel.get().saturating_sub(units.min(i64::MAX as u64) as i64);
        self.remaining_slice_fuel.set(remaining);
        if remaining < self.next_limit_check_fuel.get() { self.run_limit_checkpoint() } else { Ok(()) }
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

    /// Verifies immediately whether execution must stop due to cancellation,
    /// `max_instructions`, CPU time, or wall-clock time.
    pub fn check_limits(&self) -> Result<(), Fail> {
        if self.interrupt_flag.load(Ordering::Relaxed) {
            return Err(Fail::interrupted());
        }
        if let Some(max) = self.max_instructions
            && self.total_fuel_used() > max
        {
            return Err(Fail::limit(LimitKind::Instructions));
        }
        if self.cpu_deadline.get().is_some_and(|deadline| self.clock.cpu_time() >= deadline) {
            return Err(Fail::limit(LimitKind::CpuTime));
        }
        if self.wall_deadline.get().is_some_and(|deadline| self.clock.now() >= deadline) {
            return Err(Fail::limit(LimitKind::WallTime));
        }
        Ok(())
    }

    /// Returns the remaining fuel in the current slice.
    /// A negative value indicates an overspent slice.
    #[inline]
    pub fn remaining_slice_fuel(&self) -> i64 {
        self.remaining_slice_fuel.get()
    }

    /// Returns fuel spent in the current slice.
    pub fn slice_fuel_spent(&self) -> u64 {
        self.slice_initial_fuel.get().saturating_sub(self.remaining_slice_fuel.get()).max(0) as u64
    }

    /// Returns fuel spent across the current run.
    pub fn total_fuel_used(&self) -> u64 {
        self.prior_slices_fuel.get().saturating_add(self.slice_fuel_spent())
    }

    /// Begins a scheduling slice with a fuel allowance and time deadline.
    ///
    /// The scheduler calls this before each fiber turn. The fiber yields
    /// after `SLICE_FUEL` units or stops at `max_instructions`. Returns the
    /// starting fuel for the interpreter's local counter.
    pub(crate) fn start_slice(&self, cpu_deadline: Option<Duration>, wall_deadline: Option<Instant>) -> i64 {
        let used = self.total_fuel_used();
        let slice_fuel = match self.max_instructions {
            Some(max) => max.saturating_sub(used).min(super::SLICE_FUEL as u64) as i64,
            None => super::SLICE_FUEL,
        };
        self.prior_slices_fuel.set(used);
        self.slice_initial_fuel.set(slice_fuel);
        self.remaining_slice_fuel.set(slice_fuel);
        self.interpreter_fuel_baseline.set(slice_fuel);
        self.next_limit_check_fuel.set(0);
        self.cpu_deadline.set(cpu_deadline);
        self.wall_deadline.set(wall_deadline);
        slice_fuel
    }

    /// Commits bytecode fuel spent in the interpreter's local counter back into
    /// the meter.
    ///
    /// Call this before a native call or at the end of a slice so the meter
    /// holds the full run total before checking limits or running native code.
    /// Pass the interpreter's current `interpreter_fuel` counter.
    #[inline]
    pub(crate) fn flush_interpreter_fuel(&self, interpreter_fuel: i64) {
        let bytecode_spent = self.interpreter_fuel_baseline.get() - interpreter_fuel;
        self.remaining_slice_fuel.set(self.remaining_slice_fuel.get() - bytecode_spent);
    }

    /// Synchronizes the interpreter's local counter with fuel spent during a
    /// native call.
    ///
    /// Call this after a native call returns. The returned value becomes the
    /// interpreter's remaining slice fuel and the new sync baseline.
    #[inline]
    pub(crate) fn reload_interpreter_fuel(&self) -> i64 {
        let remaining = self.remaining_slice_fuel.get();
        self.interpreter_fuel_baseline.set(remaining);
        remaining
    }

    /// Restores remaining slice fuel so the VM does not charge twice for
    /// retried operations.
    #[inline]
    pub(crate) fn refund_to(&self, mark: i64) {
        self.remaining_slice_fuel.set(mark);
        self.next_limit_check_fuel.set(self.next_limit_check_fuel.get().max(mark.saturating_sub(LIMIT_CHECK_FUEL_INTERVAL)));
    }

    /// Clears accumulated run fuel and installs a fresh interrupt flag to begin
    /// a new session run.
    pub(crate) fn reset_for_run(&mut self, interrupt_flag: Arc<AtomicBool>) {
        self.prior_slices_fuel.set(0);
        self.slice_initial_fuel.set(self.remaining_slice_fuel.get());
        self.interrupt_flag = interrupt_flag;
    }

    /// Creates a [`WorkerMeter`] so work running off the VM thread can
    /// enforce execution limits independently.
    pub(crate) fn worker_meter(&self) -> WorkerMeter {
        WorkerMeter {
            interrupt_flag: self.interrupt_flag.clone(),
            abandoned_flag: Arc::new(AtomicBool::new(false)),
            wall_deadline: self.wall_deadline.get().map(|at| Deadline { at, clock: self.clock.clone() }),
            fuel_limit: self.max_instructions.map_or(u64::MAX, |max| max.saturating_sub(self.total_fuel_used())),
            fuel_used: 0,
            next_deadline_check_fuel: 0,
            stopped: false,
        }
    }

    /// Advances the limit checkpoint and verifies execution limits.
    #[cold]
    #[inline(never)]
    fn run_limit_checkpoint(&self) -> Result<(), Fail> {
        // Remaining slice fuel is below `instruction_limit_fuel` once total
        // fuel exceeds `max_instructions`. Clamping to this bound ensures that
        // the meter runs a limit check right at the budget boundary.
        let instruction_limit_fuel = match self.max_instructions {
            Some(max) => (i128::from(self.prior_slices_fuel.get()) + i128::from(self.slice_initial_fuel.get()) - i128::from(max))
                .clamp(i64::MIN.into(), i64::MAX.into()) as i64,
            None => i64::MIN,
        };
        self.next_limit_check_fuel
            .set(self.remaining_slice_fuel.get().saturating_sub(LIMIT_CHECK_FUEL_INTERVAL).max(instruction_limit_fuel));
        self.check_limits()
    }
}

impl Meter for VmMeter {
    #[inline]
    fn charge(&self, units: u64) -> Result<(), Fail> {
        VmMeter::charge(self, units)
    }

    #[inline]
    fn fuel_mark(&self) -> i64 {
        self.remaining_slice_fuel()
    }

    #[inline]
    fn refund_to(&self, mark: i64) {
        VmMeter::refund_to(self, mark);
    }
}

/// A [`FuelBudget`] that ignores all charges.
pub struct UnlimitedMeter;

impl Meter for UnlimitedMeter {
    #[inline(always)]
    fn charge(&self, _: u64) -> Result<(), Fail> {
        Ok(())
    }

    #[inline(always)]
    fn fuel_mark(&self) -> i64 {
        0
    }

    #[inline(always)]
    fn refund_to(&self, _: i64) {}
}

/// Prevents growing Rust-side temporary buffers from bypassing the VM memory
/// limit.
///
/// Native functions and runtime helpers build temporary strings and vectors in
/// Rust memory outside the SandLang heap. Checking the heap limit on every byte
/// appended is too expensive. This tracker amortizes checks to every
/// [`SCRATCH_CHECK_BYTES`] bytes of growth.
pub struct ScratchMemoryTracker {
    next_check_bytes: usize,
}

impl ScratchMemoryTracker {
    /// Creates a tracker that runs its first memory check at
    /// [`SCRATCH_CHECK_BYTES`] bytes.
    pub fn new() -> ScratchMemoryTracker {
        ScratchMemoryTracker { next_check_bytes: SCRATCH_CHECK_BYTES }
    }

    /// Verifies that `bytes` of temporary Rust buffer space fits within the VM
    /// memory limit.
    ///
    /// Skips the heap check until `bytes` reaches the next threshold, unless
    /// retry stress testing forces a check.
    #[inline]
    pub fn check_memory(&mut self, heap: &Heap, bytes: usize) -> Result<(), Fail> {
        if bytes < self.next_check_bytes && !heap.fail_next_check.get() {
            return Ok(());
        }
        self.next_check_bytes = bytes.saturating_add(SCRATCH_CHECK_BYTES);
        heap.check_memory(bytes)
    }
}

impl Default for ScratchMemoryTracker {
    fn default() -> Self {
        ScratchMemoryTracker::new()
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

/// Buffer growth in bytes between memory-limit checks in
/// [`ScratchMemoryTracker`].
pub const SCRATCH_CHECK_BYTES: usize = 64 << 10;

/// Bit shift for straight-line bytecode instructions per fuel unit: `1 << 6`,
/// or 64 instructions.
pub const INSTRUCTIONS_PER_FUEL_SHIFT: u32 = 6;

/// Enforces fuel, deadline, and cancellation limits for blocking work offloaded
/// to a worker thread.
///
/// [`Meter`] is bound to the VM thread, so offloaded jobs charge this instead.
/// A charge returns [`WorkerStopped`] if the engine is interrupted, the clock
/// passes the wall-clock deadline, the worker exceeds the starting instruction
/// budget, or the VM stops waiting. After the job finishes, the VM charges its
/// [`Meter`] for [`WorkerMeter::fuel_used`].
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
        assert_eq!((meter.remaining_slice_fuel(), meter.total_fuel_used()), (SLICE_FUEL - 50, 50));
        assert!(meter.charge(SLICE_FUEL as u64).is_ok());
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
        assert_eq!(meter.reload_interpreter_fuel(), -1);
        let meter = VmMeter::new(Some(1_000_000), Arc::new(AtomicBool::new(false)), system_clock());
        meter.start_slice(None, None);
        assert!(meter.charge(SLICE_FUEL as u64 + 1).is_ok());
        assert_eq!(meter.next_limit_check_fuel.get(), -1 - LIMIT_CHECK_FUEL_INTERVAL);
        assert!(meter.charge(1_000_000 - SLICE_FUEL as u64 - 1).is_ok(), "1,000,000 units used");
        assert!(matches!(meter.charge(1).map_err(Fail::into_kind), Err(FailKind::Limit(LimitKind::Instructions))));
        let mut meter = VmMeter::new(Some(100), Arc::new(AtomicBool::new(false)), system_clock());
        assert!(meter.charge(60).is_ok());
        assert_eq!(meter.start_slice(None, None), 40);
        meter.reset_for_run(Arc::new(AtomicBool::new(false)));
        assert_eq!((meter.total_fuel_used(), meter.start_slice(None, None)), (0, 100));
    }

    #[test]
    fn refund_to_restores_checkpoint_interval() {
        let interrupt_flag = Arc::new(AtomicBool::new(false));
        let meter = VmMeter::new(None, interrupt_flag.clone(), system_clock());
        meter.start_slice(None, None);
        let mark = meter.remaining_slice_fuel();
        assert!(meter.charge(SLICE_FUEL as u64 + 1).is_ok(), "checkpoint advances next_limit_check_fuel");
        meter.refund_to(mark);
        interrupt_flag.store(true, Ordering::SeqCst);
        assert!(matches!(meter.charge(SLICE_FUEL as u64 + 1).map_err(Fail::into_kind), Err(FailKind::Interrupted)));
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
