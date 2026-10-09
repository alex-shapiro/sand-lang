//! Fuel metering for work done outside the bytecode loop: natives and the
//! heavy generic operations of the interpreter (deep comparison, display,
//! concatenation, slicing, ...).
//!
//! The interpreter burns one unit of fuel per call, loop iteration and
//! backward jump; the scheduler runs each task for a slice of `SLICE_FUEL`
//! units and checks the engine's limits between slices. Work outside the
//! bytecode loop is charged with [`Meter::charge`] against a second budget of
//! `SLICE_FUEL` units per slice (kept apart from the interpreter's counter,
//! which stays in a register in the dispatch loop). The scheduler adds both
//! to the fuel used, so native work counts against `max_instructions`; a
//! native that exhausts the slice's native fuel preempts its task when it
//! returns; and long operations check limits *while they are running*:
//!
//! * charging is a subtraction and a comparison — no syscalls;
//! * once the slice's native fuel is exhausted, every further
//!   `CHECK_INTERVAL` units the meter checks the interrupt flag, the
//!   instruction budget and the CPU/wall-clock deadlines computed by the
//!   scheduler for this slice, and fails with `Ctl::Interrupted` or
//!   `Ctl::Limit(kind)`. (Mid-call instruction checks do not see the
//!   bytecode fuel of the current slice, so they may let a call run up to
//!   `SLICE_FUEL` units past `max_instructions`; the scheduler's check after
//!   the slice is exact.)
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
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Bytes per fuel unit for memcpy-like work (copying, comparing, scanning).
pub const BYTES_PER_UNIT: usize = 64;

/// Bytes per fuel unit for byte-at-a-time work (hashing, regex, encoding).
pub const SLOW_BYTES_PER_UNIT: usize = 8;

/// Fuel units between limit checks once a slice's fuel is exhausted.
pub const CHECK_INTERVAL: i64 = 20_000;

/// Scratch-buffer growth between memory-limit checks.
pub const SCRATCH_STEP: usize = 64 << 10;

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

/// The native fuel counter of an engine.
pub struct Meter {
    /// Native fuel left in the current slice; negative once overspent.
    fuel: Cell<i64>,
    /// The next limit check happens when `fuel` drops below this.
    check_at: Cell<i64>,
    /// Size of the current slice.
    slice: Cell<i64>,
    /// Fuel used before the current slice started.
    base: Cell<u64>,
    max_instructions: Option<u64>,
    cpu_deadline: Cell<Option<Instant>>,
    wall_deadline: Cell<Option<Instant>>,
    interrupt: Arc<AtomicBool>,
}

impl Meter {
    pub fn new(max_instructions: Option<u64>, interrupt: Arc<AtomicBool>) -> Meter {
        Meter {
            fuel: Cell::new(super::SLICE_FUEL),
            check_at: Cell::new(0),
            slice: Cell::new(super::SLICE_FUEL),
            base: Cell::new(0),
            max_instructions,
            cpu_deadline: Cell::new(None),
            wall_deadline: Cell::new(None),
            interrupt,
        }
    }

    /// Starts a scheduler slice with `fuel` units of native fuel. `used` is
    /// the fuel used so far; the deadlines are the instants at which the CPU
    /// and wall-clock limits are exceeded.
    pub(crate) fn start_slice(&self, fuel: i64, used: u64, cpu_deadline: Option<Instant>, wall_deadline: Option<Instant>) {
        self.fuel.set(fuel);
        self.slice.set(fuel);
        self.base.set(used);
        self.check_at.set(0);
        self.cpu_deadline.set(cpu_deadline);
        self.wall_deadline.set(wall_deadline);
    }

    /// Native fuel left in the current slice (negative if overspent).
    #[inline]
    pub fn fuel(&self) -> i64 {
        self.fuel.get()
    }

    /// Native fuel spent in the current slice.
    pub fn spent(&self) -> u64 {
        self.slice.get().saturating_sub(self.fuel.get()).max(0) as u64
    }

    /// Fuel used by the engine run so far: all slices before this one, plus
    /// the native fuel spent in this one.
    pub fn used(&self) -> u64 {
        self.base.get().saturating_add(self.spent())
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
        self.check_at.set(self.fuel.get().saturating_sub(CHECK_INTERVAL));
        self.check()
    }

    /// Checks the interrupt flag, the instruction budget and the time
    /// limits now.
    pub fn check(&self) -> Result<(), Ctl> {
        if self.interrupt.load(Ordering::Relaxed) {
            return Err(Ctl::Interrupted);
        }
        if let Some(max) = self.max_instructions
            && self.used() > max
        {
            return Err(Ctl::Limit(LimitKind::Instructions));
        }
        let (cpu, wall) = (self.cpu_deadline.get(), self.wall_deadline.get());
        if cpu.is_some() || wall.is_some() {
            let now = Instant::now();
            if cpu.is_some_and(|d| now >= d) {
                return Err(Ctl::Limit(LimitKind::CpuTime));
            }
            if wall.is_some_and(|d| now >= d) {
                return Err(Ctl::Limit(LimitKind::WallTime));
            }
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
        if heap.would_exceed(len) { Err(Ctl::Limit(LimitKind::Memory)) } else { Ok(()) }
    }
}

/// Results that may report the memory limit (directly or as an encoded
/// message). Memory checks use the heap's byte count, which includes garbage
/// not yet collected, so callers that can run the collector retry once.
pub(crate) trait MemoryLimit {
    fn is_memory_limit(&self) -> bool;
}

impl<T> MemoryLimit for Result<T, Ctl> {
    fn is_memory_limit(&self) -> bool {
        matches!(self, Err(Ctl::Limit(LimitKind::Memory)))
    }
}

impl<T> MemoryLimit for Result<T, String> {
    fn is_memory_limit(&self) -> bool {
        matches!(self, Err(m) if matches!(message_ctl(m), Some(Ctl::Limit(LimitKind::Memory))))
    }
}

/// Internal messages encoding a limit or interrupt through APIs that return
/// `Result<_, String>` (map and set helpers, slicing, ...). They start with a
/// NUL byte. Script panic messages may start with one too, so they are only
/// decoded where such an API's error is turned into a `Ctl`: in natives with
/// [`ctl_from_message`] (never `Ctl::Panic` directly), and in the
/// interpreter's own operations. A `Ctl::Panic` is always a panic.
pub(crate) fn ctl_message(c: Ctl) -> String {
    match c {
        Ctl::Panic(m) => m,
        Ctl::Limit(k) => format!("\u{0}{}", limit_code(k)),
        Ctl::Interrupted => "\u{0}interrupted".into(),
        other => format!("internal error: unexpected {other:?}"),
    }
}

fn limit_code(k: LimitKind) -> &'static str {
    match k {
        LimitKind::Memory => "memory",
        LimitKind::Instructions => "instructions",
        LimitKind::CpuTime => "cpu",
        LimitKind::WallTime => "wall",
        LimitKind::Output => "output",
    }
}

/// Converts the error of a `Result<_, String>` API (see [`ctl_message`])
/// into a `Ctl`: a limit, an interrupt or a panic.
pub fn ctl_from_message(msg: String) -> Ctl {
    message_ctl(&msg).unwrap_or(Ctl::Panic(msg))
}

/// Decodes a message produced by [`ctl_message`]: `Some(Ctl::Limit(..))` or
/// `Some(Ctl::Interrupted)`, `None` for ordinary panic messages.
pub(crate) fn message_ctl(msg: &str) -> Option<Ctl> {
    let code = msg.strip_prefix('\u{0}')?;
    Some(match code {
        "memory" => Ctl::Limit(LimitKind::Memory),
        "instructions" => Ctl::Limit(LimitKind::Instructions),
        "cpu" => Ctl::Limit(LimitKind::CpuTime),
        "wall" => Ctl::Limit(LimitKind::WallTime),
        "output" => Ctl::Limit(LimitKind::Output),
        "interrupted" => Ctl::Interrupted,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_are_free_until_the_slice_is_spent() {
        let m = Meter::new(Some(1000), Arc::new(AtomicBool::new(false)));
        m.start_slice(100, 0, None, None);
        assert!(m.charge(50).is_ok());
        assert_eq!(m.fuel(), 50);
        assert_eq!(m.used(), 50);
        assert!(m.charge(40).is_ok());
        // Overspending the slice triggers a check: 1040 units exceed the budget.
        assert!(matches!(m.charge(950), Err(Ctl::Limit(LimitKind::Instructions))));
    }

    #[test]
    fn interrupt_is_seen_at_checkpoints() {
        let flag = Arc::new(AtomicBool::new(false));
        let m = Meter::new(None, flag.clone());
        m.start_slice(10, 0, None, None);
        flag.store(true, Ordering::SeqCst);
        assert!(m.charge(5).is_ok(), "no check while fuel is left");
        assert!(matches!(m.charge(10), Err(Ctl::Interrupted)));
    }

    #[test]
    fn deadlines() {
        let m = Meter::new(None, Arc::new(AtomicBool::new(false)));
        m.start_slice(0, 0, Some(Instant::now()), None);
        assert!(matches!(m.charge(1), Err(Ctl::Limit(LimitKind::CpuTime))));
        m.start_slice(0, 0, None, Some(Instant::now()));
        assert!(matches!(m.charge(1), Err(Ctl::Limit(LimitKind::WallTime))));
    }

    #[test]
    fn messages_round_trip() {
        for c in [Ctl::Limit(LimitKind::Memory), Ctl::Limit(LimitKind::CpuTime), Ctl::Interrupted] {
            let text = format!("{c:?}");
            let back = message_ctl(&ctl_message(c)).unwrap();
            assert_eq!(format!("{back:?}"), text);
        }
        assert!(message_ctl("ordinary panic").is_none());
    }
}
