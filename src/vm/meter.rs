//! Fuel metering: one counter for bytecode and for the work done outside the
//! bytecode loop (natives, and the interpreter's heavy generic operations:
//! deep comparison, display, concatenation, slicing, ...).
//!
//! The scheduler runs each task for a slice of fuel: at most `SLICE_FUEL`
//! units, and at most what is left of `max_instructions`. The interpreter
//! spends one unit per call, loop iteration and backward jump; natives
//! charge the meter ([`Meter::charge`]) for their work. The interpreter
//! keeps its count in a register, applies what it spent to the meter before
//! each native call ([`Meter::sync`]) and reloads it afterwards
//! ([`Meter::resync`]). So:
//!
//! * charging is a subtraction and a comparison, with no syscalls;
//! * native work counts against `max_instructions`, and a native that
//!   exhausts the slice preempts its task when it returns;
//! * once the slice is exhausted, the meter checks the interrupt flag, the
//!   instruction budget and the slice's CPU and wall-clock deadlines every
//!   `CHECK_INTERVAL` units, failing with `Ctl::interrupted()` or
//!   `Ctl::limit(kind)`. Neither a slice nor a check interval extends past
//!   the instruction budget, so natives stop exactly at the limit and
//!   bytecode within one unit of it;
//! * the interpreter's generic operations charge the meter before it has
//!   seen the bytecode fuel spent since the last sync, so they may overrun
//!   the limit by that much (less than a slice).
//!
//! The meter uses `Cell`s so that it can be charged through a shared
//! reference while the heap is borrowed (e.g. inside sort comparators).
//!
//! # Cost model
//!
//! One fuel unit is roughly the work of one simple bytecode loop iteration
//! (a few nanoseconds to a few tens of nanoseconds). Natives charge:
//!
//! * [`Meter::charge`]`(n)` — one unit per element visited, compared, hashed,
//!   copied or produced (list/map/set elements, comparison and display
//!   nodes, JSON values, regex matches);
//! * [`Meter::charge_bytes`]`(n)` — one unit per [`BYTES_PER_UNIT`] bytes
//!   scanned, copied or produced (strings, bytes, output buffers);
//! * [`Meter::charge_slow_bytes`]`(n)` — one unit per [`SLOW_BYTES_PER_UNIT`]
//!   bytes for byte-at-a-time algorithms (hash functions, regex search,
//!   base64, percent-encoding, case mapping).
//!
//! Operations that cannot be interrupted internally (a single regex search)
//! charge their estimated cost *before* running, so they fail fast when the
//! instruction budget cannot cover them.

use super::Ctl;
use super::heap::Heap;
use crate::LimitKind;
use crate::interrupt::Interrupt;
use crate::platform::{Clock, Deadline, Instant};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Bytes per fuel unit for memcpy-like work (copying, comparing, scanning).
pub const BYTES_PER_UNIT: usize = 64;

/// Bytes per fuel unit for byte-at-a-time work (hashing, regex, encoding).
pub const SLOW_BYTES_PER_UNIT: usize = 8;

/// Fuel units between limit checks once a slice's fuel is exhausted.
pub const CHECK_INTERVAL: i64 = 20_000;

/// Scratch-buffer growth between memory-limit checks.
pub const SCRATCH_STEP: usize = 64 << 10;

/// Instructions of straight-line bytecode per fuel unit, as a shift (64).
pub const CODE_UNIT_SHIFT: u32 = 6;

/// The extra fuel for running up to `n` instructions without passing a safe
/// point: one unit per 64.
///
/// Bytecode consumes fuel only at back edges and calls, and between those it
/// only moves forward, so it runs at most the instructions of the loop body
/// or function entered. Without this surcharge one unit could stand for
/// hundreds of thousands of instructions, making `max_instructions` that
/// inaccurate and delaying the limit and interrupt checks between slices by
/// seconds. So a back edge costs one unit plus one per 64 instructions it
/// jumps back over, entering a function (a call, a tail call, a task's
/// start) one plus one per 64 instructions of its code, and `MakeClosure`
/// one per 64 captures it copies. Typical loop bodies and functions are
/// shorter than 64 instructions and cost one unit.
#[inline(always)]
pub const fn code_fuel(n: usize) -> i64 {
    (n >> CODE_UNIT_SHIFT) as i64
}

/// The fuel of a back edge with offset `off < 0`: one unit per 64
/// instructions jumped back over, rounded up. This includes the edge's base
/// unit, so the back edge's path pays one shift for it.
#[inline(always)]
pub const fn loop_fuel(off: i32) -> i64 {
    -((off >> CODE_UNIT_SHIFT) as i64)
}

/// Something that can be charged for work: the engine's [`Meter`], or
/// [`Unmetered`] for callers outside an engine run.
pub trait Budget {
    fn charge(&self, units: u64) -> Result<(), Ctl>;

    #[inline]
    fn charge_bytes(&self, n: usize) -> Result<(), Ctl> {
        self.charge((n / BYTES_PER_UNIT) as u64)
    }
}

/// A budget that never runs out.
pub struct Unmetered;

impl Budget for Unmetered {
    #[inline(always)]
    fn charge(&self, _: u64) -> Result<(), Ctl> {
        Ok(())
    }
}

/// The fuel counter of an engine.
pub struct Meter {
    /// Fuel left in the current slice; negative once overspent.
    fuel: Cell<i64>,
    /// The interpreter's count of the slice's fuel as of its last
    /// [`Meter::sync`] or [`Meter::resync`].
    synced: Cell<i64>,
    /// The next limit check happens when `fuel` drops below this.
    check_at: Cell<i64>,
    /// The fuel the current slice started with.
    slice: Cell<i64>,
    /// Fuel used by the run before the current slice.
    base: Cell<u64>,
    max_instructions: Option<u64>,
    cpu_deadline: Cell<Option<Duration>>,
    wall_deadline: Cell<Option<Instant>>,
    interrupt: Arc<Interrupt>,
    clock: Arc<dyn Clock>,
}

impl Meter {
    pub(crate) fn new(max_instructions: Option<u64>, interrupt: Arc<Interrupt>, clock: Arc<dyn Clock>) -> Meter {
        Meter {
            fuel: Cell::new(0),
            synced: Cell::new(0),
            check_at: Cell::new(0),
            slice: Cell::new(0),
            base: Cell::new(0),
            max_instructions,
            cpu_deadline: Cell::new(None),
            wall_deadline: Cell::new(None),
            interrupt,
            clock,
        }
    }

    /// Allocates execution fuel for the next slice and sets execution deadlines.
    pub(crate) fn start_slice(&self, cpu_deadline: Option<Duration>, wall_deadline: Option<Instant>) -> i64 {
        let used = self.used();
        let fuel = match self.max_instructions {
            Some(max) => max.saturating_sub(used).min(super::SLICE_FUEL as u64) as i64,
            None => super::SLICE_FUEL,
        };
        self.base.set(used);
        self.slice.set(fuel);
        self.fuel.set(fuel);
        self.synced.set(fuel);
        self.check_at.set(0);
        self.cpu_deadline.set(cpu_deadline);
        self.wall_deadline.set(wall_deadline);
        fuel
    }

    /// Applies the bytecode fuel the interpreter spent since its last sync,
    /// given the fuel it has `left` (before it calls a native, and when it
    /// returns).
    #[inline]
    pub(crate) fn sync(&self, left: i64) {
        self.fuel.set(self.fuel.get() - (self.synced.get() - left));
    }

    /// The fuel left for the interpreter (after it called a native).
    #[inline]
    pub(crate) fn resync(&self) -> i64 {
        let f = self.fuel.get();
        self.synced.set(f);
        f
    }

    /// Fuel left in the current slice (negative if overspent).
    #[inline]
    pub fn fuel(&self) -> i64 {
        self.fuel.get()
    }

    /// Fuel spent in the current slice.
    pub fn spent(&self) -> u64 {
        self.slice.get().saturating_sub(self.fuel.get()).max(0) as u64
    }

    /// Fuel used by the run so far (the interpreter's up to its last sync).
    pub fn used(&self) -> u64 {
        self.base.get().saturating_add(self.spent())
    }

    /// Counts the fuel used from zero again (a new input of a REPL session).
    pub(crate) fn reset(&self) {
        self.base.set(0);
        self.slice.set(self.fuel.get());
    }

    /// Charges `units` of fuel. Cheap; checks limits only at checkpoints.
    #[inline]
    pub fn charge(&self, units: u64) -> Result<(), Ctl> {
        let f = self.fuel.get().saturating_sub(units.min(i64::MAX as u64) as i64);
        self.fuel.set(f);
        if f < self.check_at.get() { self.checkpoint() } else { Ok(()) }
    }

    /// Charges for `n` bytes of memcpy-like work.
    #[inline]
    pub fn charge_bytes(&self, n: usize) -> Result<(), Ctl> {
        self.charge((n / BYTES_PER_UNIT) as u64)
    }

    /// Charges for `n` bytes of byte-at-a-time work.
    #[inline]
    pub fn charge_slow_bytes(&self, n: usize) -> Result<(), Ctl> {
        self.charge((n / SLOW_BYTES_PER_UNIT) as u64)
    }

    #[cold]
    #[inline(never)]
    fn checkpoint(&self) -> Result<(), Ctl> {
        // `fuel` falls below `limit_at` exactly when the fuel used exceeds
        // the instruction budget: the next check is no later than that.
        let limit_at = match self.max_instructions {
            Some(max) => (i128::from(self.base.get()) + i128::from(self.slice.get()) - i128::from(max))
                .clamp(i64::MIN.into(), i64::MAX.into()) as i64,
            None => i64::MIN,
        };
        self.check_at.set(self.fuel.get().saturating_sub(CHECK_INTERVAL).max(limit_at));
        self.check()
    }

    /// Checks the interrupt flag, the instruction budget and the time
    /// limits now.
    pub fn check(&self) -> Result<(), Ctl> {
        if self.interrupt.is_set() {
            return Err(Ctl::interrupted());
        }
        if let Some(max) = self.max_instructions
            && self.used() > max
        {
            return Err(Ctl::limit(LimitKind::Instructions));
        }
        if self.cpu_deadline.get().is_some_and(|d| self.clock.cpu_time() >= d) {
            return Err(Ctl::limit(LimitKind::CpuTime));
        }
        if self.wall_deadline.get().is_some_and(|d| self.clock.now() >= d) {
            return Err(Ctl::limit(LimitKind::WallTime));
        }
        Ok(())
    }
}

impl Budget for Meter {
    #[inline]
    fn charge(&self, units: u64) -> Result<(), Ctl> {
        Meter::charge(self, units)
    }
}

impl Meter {
    /// A new allowance for work about to run off the VM thread.
    pub(crate) fn allowance(&self) -> Allowance {
        Allowance {
            interrupt: self.interrupt.clone(),
            abandoned: Arc::new(AtomicBool::new(false)),
            deadline: self.wall_deadline.get().map(|at| Deadline { at, clock: self.clock.clone() }),
            left: self.max_instructions.map_or(u64::MAX, |max| max.saturating_sub(self.used())),
            used: 0,
            next_check: 0,
            stopped: false,
        }
    }
}

/// Fuel for work done off the VM thread, such as walking a directory tree
/// on a worker. The meter cannot see that work while it runs, so the work
/// charges its allowance instead. A charge fails once the engine is
/// interrupted, the wall-time deadline passes, the instructions left when
/// the work started are spent, or the VM stops waiting for it; the work
/// must then stop. Afterwards the VM charges the meter for what was used.
pub(crate) struct Allowance {
    interrupt: Arc<Interrupt>,
    abandoned: Arc<AtomicBool>,
    deadline: Option<Deadline>,
    left: u64,
    used: u64,
    next_check: u64,
    stopped: bool,
}

/// The error of a failed [`Allowance`] charge: the work must stop.
#[derive(Debug)]
pub(crate) struct Stopped;

impl Allowance {
    /// Charges `units` of fuel.
    pub fn charge(&mut self, units: u64) -> Result<(), Stopped> {
        self.used = self.used.saturating_add(units);
        self.stopped |= self.used > self.left || self.interrupt.is_set() || self.abandoned.load(Ordering::Relaxed);
        if !self.stopped && self.used >= self.next_check {
            self.next_check = self.used.saturating_add(CHECK_INTERVAL as u64 / 16);
            self.stopped = self.deadline.as_ref().is_some_and(Deadline::passed);
        }
        if self.stopped { Err(Stopped) } else { Ok(()) }
    }

    /// Whether a charge has failed.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Charges for `n` bytes of memcpy-like work.
    pub fn charge_bytes(&mut self, n: usize) -> Result<(), Stopped> {
        self.charge((n / BYTES_PER_UNIT) as u64)
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    /// The flag that makes further charges fail. The VM sets it when it
    /// stops waiting for the work.
    #[cfg(feature = "os")]
    pub fn abandon_flag(&self) -> Arc<AtomicBool> {
        self.abandoned.clone()
    }
}

/// Tracks a growing Rust-side buffer (output string, scratch vector) against
/// the memory limit, checking every [`SCRATCH_STEP`] bytes of growth.
pub struct Scratch {
    next: usize,
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch::new()
    }
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch { next: SCRATCH_STEP }
    }

    /// Fails with a memory limit error if `len` bytes of scratch space would
    /// exceed the limit (checked only when `len` has grown enough).
    #[inline]
    pub fn check(&mut self, heap: &Heap, len: usize) -> Result<(), Ctl> {
        if len < self.next {
            return Ok(());
        }
        self.next = len.saturating_add(SCRATCH_STEP);
        if heap.would_exceed(len) { Err(Ctl::limit(LimitKind::Memory)) } else { Ok(()) }
    }
}

/// Results that may report the memory limit. Memory checks use the heap's
/// byte count, which includes garbage not yet collected, so callers that can
/// run the collector retry once.
pub(crate) trait MemoryLimit {
    fn is_memory_limit(&self) -> bool;
}

impl<T> MemoryLimit for Result<T, Ctl> {
    fn is_memory_limit(&self) -> bool {
        matches!(self, Err(c) if c.as_limit() == Some(LimitKind::Memory))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CtlKind, SLICE_FUEL};
    use super::*;

    fn system() -> Arc<dyn Clock> {
        Arc::new(crate::platform::SystemClock)
    }

    #[test]
    fn charges_are_free_until_the_slice_is_spent() {
        let m = Meter::new(Some(100_000), Interrupt::new(None), system());
        assert_eq!(m.start_slice(None, None), SLICE_FUEL);
        assert!(m.charge(50).is_ok());
        assert_eq!((m.fuel(), m.used()), (SLICE_FUEL - 50, 50));
        // Overspending the slice triggers a check: the budget is not exceeded.
        assert!(m.charge(SLICE_FUEL as u64).is_ok());
        assert_eq!(m.used(), SLICE_FUEL as u64 + 50);
        assert!(matches!(m.charge(100_000).map_err(Ctl::into_kind), Err(CtlKind::Limit(LimitKind::Instructions))));
    }

    #[test]
    fn the_instruction_limit_is_checked_exactly() {
        let m = Meter::new(Some(1000), Interrupt::new(None), system());
        // The slice ends where the budget does.
        assert_eq!(m.start_slice(None, None), 1000);
        // The interpreter spends 300 units, then a native the rest.
        m.sync(700);
        assert_eq!(m.used(), 300);
        assert!(m.charge(700).is_ok(), "1000 units used");
        assert!(matches!(m.charge(1).map_err(Ctl::into_kind), Err(CtlKind::Limit(LimitKind::Instructions))));
        assert_eq!(m.resync(), -1);
        // Past the end of a slice, checks stay no later than the limit.
        let m = Meter::new(Some(1_000_000), Interrupt::new(None), system());
        m.start_slice(None, None);
        assert!(m.charge(SLICE_FUEL as u64 + 1).is_ok());
        assert_eq!(m.check_at.get(), -1 - CHECK_INTERVAL);
        assert!(m.charge(1_000_000 - SLICE_FUEL as u64 - 1).is_ok(), "1,000,000 units used");
        assert!(matches!(m.charge(1).map_err(Ctl::into_kind), Err(CtlKind::Limit(LimitKind::Instructions))));
        // Charges between slices count too.
        let m = Meter::new(Some(100), Interrupt::new(None), system());
        assert!(m.charge(60).is_ok());
        assert_eq!(m.start_slice(None, None), 40);
        m.reset();
        assert_eq!((m.used(), m.start_slice(None, None)), (0, 100));
    }

    #[test]
    fn interrupt_is_seen_at_checkpoints() {
        let flag = Interrupt::new(None);
        let m = Meter::new(None, flag.clone(), system());
        m.start_slice(None, None);
        flag.set();
        assert!(m.charge(5).is_ok(), "no check while fuel is left");
        assert!(matches!(m.charge(SLICE_FUEL as u64).map_err(Ctl::into_kind), Err(CtlKind::Interrupted)));
    }

    #[test]
    fn deadlines() {
        let m = Meter::new(None, Interrupt::new(None), system());
        m.start_slice(Some(system().cpu_time()), None);
        assert!(matches!(m.charge(SLICE_FUEL as u64 + 1).map_err(Ctl::into_kind), Err(CtlKind::Limit(LimitKind::CpuTime))));
        m.start_slice(None, Some(Instant::now()));
        assert!(matches!(m.charge(SLICE_FUEL as u64 + 1).map_err(Ctl::into_kind), Err(CtlKind::Limit(LimitKind::WallTime))));
    }
}
