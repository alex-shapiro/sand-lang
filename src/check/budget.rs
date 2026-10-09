//! The compile budget: bounds the work of type checking so that a hostile
//! program cannot stall the host before any runtime limit applies.
//!
//! Type checking is near-linear for ordinary programs, but some inputs are
//! superlinear: exhaustiveness checking (Maranget's usefulness algorithm) is
//! exponential in the worst case, and inferred or aliased types can double in
//! size at each step (`let b = (a, a)`), which makes every later clone or
//! zonk of them exponential. Three mechanisms bound this work:
//!
//! * a global fuel counter (`Limits::max_compile_fuel`), charged one unit per
//!   expression checked, unification step, type node visited or produced,
//!   and pattern row examined;
//! * local limits with precise diagnostics: types may have at most
//!   [`MAX_TYPE_SIZE`] nodes (`type too large`), and the exhaustiveness check
//!   of one `match` may do at most [`MAX_PATTERN_WORK`] units of work
//!   (`pattern too complex to check for exhaustiveness`);
//! * a deadline (`Limits::max_wall_time`, counted from the start of
//!   compilation) and the engine's interrupt flag, polled every
//!   [`POLL_INTERVAL`] units.
//!
//! Counters can be charged from `&self` methods. They are relaxed atomics
//! read and written with plain loads and stores (one thread charges a
//! budget at a time), so that a checker holding one stays `Send`, as REPL
//! sessions keep their checker between inputs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

/// A `Cell<u64>`-like counter that is `Sync`.
#[derive(Default)]
struct Counter(AtomicU64);

impl Counter {
    fn new(v: u64) -> Counter {
        Counter(AtomicU64::new(v))
    }
    #[inline]
    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    #[inline]
    fn set(&self, v: u64) {
        self.0.store(v, Ordering::Relaxed)
    }
}

/// `Option<Stop>`, as a `Sync` cell.
#[derive(Default)]
struct StopCell(AtomicU8);

impl StopCell {
    #[inline]
    fn get(&self) -> Option<Stop> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            1 => Some(Stop::Fuel),
            2 => Some(Stop::Time),
            _ => Some(Stop::Interrupted),
        }
    }
    fn set(&self, s: Option<Stop>) {
        let v = match s {
            None => 0,
            Some(Stop::Fuel) => 1,
            Some(Stop::Time) => 2,
            Some(Stop::Interrupted) => 3,
        };
        self.0.store(v, Ordering::Relaxed)
    }
}

/// Maximum number of nodes in a type (after resolving inference variables).
pub const MAX_TYPE_SIZE: usize = 10_000;

/// Maximum work (pattern rows examined) for checking one `match`.
pub const MAX_PATTERN_WORK: u64 = 5_000_000;

/// Fuel units between deadline/interrupt checks.
pub const POLL_INTERVAL: u64 = 1 << 16;

/// Default for `Limits::max_compile_fuel`. Ordinary programs need less than
/// one unit per byte of source (the standard library and the examples take
/// a few thousand units each; a 1,000-arm `match` on an enum about a
/// million). Exhausting it takes well under a second and bounds the
/// compiler's memory to roughly a gigabyte.
pub const DEFAULT_COMPILE_FUEL: u64 = 10_000_000;

/// Fuel per byte of source in the default budget of the formatter and the
/// linter ([`tool_fuel`]).
pub const TOOL_FUEL_PER_BYTE: u64 = 64;

/// The default budget for formatting or linting `len` bytes of source when
/// they run on their own (`sand fmt`, `sand lint`): [`TOOL_FUEL_PER_BYTE`]
/// units per byte (ordinary code needs one or two), and at least
/// [`DEFAULT_COMPILE_FUEL`]. This keeps their time and the formatter's
/// output linear in the size of the source, whatever its shape.
pub fn tool_fuel(len: usize) -> u64 {
    DEFAULT_COMPILE_FUEL.max(TOOL_FUEL_PER_BYTE.saturating_mul(len as u64))
}

/// Why compilation was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// `max_compile_fuel` was exhausted.
    Fuel,
    /// The compile deadline passed.
    Time,
    /// The host interrupted the engine.
    Interrupted,
}

pub struct CompileBudget {
    fuel: Counter,
    next_poll: Counter,
    deadline: Option<Instant>,
    interrupt: Option<Arc<AtomicBool>>,
    stop: StopCell,
    /// Work done by the current exhaustiveness check.
    pattern_work: Counter,
}

impl Default for CompileBudget {
    fn default() -> Self {
        CompileBudget::new(None, None, None)
    }
}

impl CompileBudget {
    pub fn new(fuel: Option<u64>, deadline: Option<Instant>, interrupt: Option<Arc<AtomicBool>>) -> CompileBudget {
        let fuel = fuel.unwrap_or(u64::MAX);
        CompileBudget {
            fuel: Counter::new(fuel),
            next_poll: Counter::new(fuel.saturating_sub(POLL_INTERVAL)),
            deadline,
            interrupt,
            stop: StopCell::default(),
            pattern_work: Counter::default(),
        }
    }

    /// Charges `units`; returns `false` once the budget is exhausted (or
    /// compilation was stopped for another reason), after which callers
    /// should bail out cheaply.
    #[inline]
    pub fn charge(&self, units: u64) -> bool {
        if self.stop.get().is_some() {
            return false;
        }
        let Some(f) = self.fuel.get().checked_sub(units) else {
            self.stop.set(Some(Stop::Fuel));
            return false;
        };
        self.fuel.set(f);
        if f < self.next_poll.get() { self.poll() } else { true }
    }

    #[cold]
    fn poll(&self) -> bool {
        self.next_poll.set(self.fuel.get().saturating_sub(POLL_INTERVAL));
        if self.interrupt.as_ref().is_some_and(|i| i.load(Ordering::Relaxed)) {
            self.stop.set(Some(Stop::Interrupted));
            return false;
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            self.stop.set(Some(Stop::Time));
            return false;
        }
        true
    }

    /// Checks the deadline and interrupt flag now (between compiler phases).
    pub fn check(&self) -> bool {
        self.stop.get().is_none() && self.poll()
    }

    /// The fuel left (`u64::MAX` if unlimited and nothing was charged).
    pub fn remaining(&self) -> u64 {
        self.fuel.get()
    }

    /// Why compilation was stopped, if it was.
    pub fn stopped(&self) -> Option<Stop> {
        self.stop.get()
    }

    /// Starts the exhaustiveness check of one `match`.
    pub fn start_pattern_check(&self) {
        self.pattern_work.set(0);
    }

    /// Charges pattern work; `false` once the current check is too complex
    /// (or the global budget is exhausted).
    #[inline]
    pub fn pattern_step(&self, units: u64) -> bool {
        let w = self.pattern_work.get().saturating_add(units);
        self.pattern_work.set(w);
        w <= MAX_PATTERN_WORK && self.charge(units)
    }

    /// Whether the current exhaustiveness check ran out of work.
    pub fn pattern_too_complex(&self) -> bool {
        self.pattern_work.get() > MAX_PATTERN_WORK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuel_runs_out() {
        let b = CompileBudget::new(Some(10), None, None);
        assert!(b.charge(6));
        assert!(!b.charge(6));
        assert_eq!(b.stopped(), Some(Stop::Fuel));
        assert!(!b.charge(0), "stays stopped");
    }

    #[test]
    fn deadline_and_interrupt_are_polled() {
        let b = CompileBudget::new(None, Some(Instant::now()), None);
        assert!(!b.charge(POLL_INTERVAL + 1));
        assert_eq!(b.stopped(), Some(Stop::Time));
        let flag = Arc::new(AtomicBool::new(true));
        let b = CompileBudget::new(None, None, Some(flag));
        assert!(!b.check());
        assert_eq!(b.stopped(), Some(Stop::Interrupted));
    }

    #[test]
    fn pattern_work_is_per_match() {
        let b = CompileBudget::default();
        b.start_pattern_check();
        assert!(b.pattern_step(MAX_PATTERN_WORK));
        assert!(!b.pattern_step(1));
        assert!(b.pattern_too_complex());
        b.start_pattern_check();
        assert!(b.pattern_step(1));
        assert!(b.stopped().is_none(), "a complex match does not stop compilation");
    }
}
