//! Parts that every arm shares so that measurements stay fair: the
//! instruction set, assembler, programs, format codecs, byte tracker, GC
//! trigger policy, fuel, and limits.

pub mod asm;
pub mod bench;
pub mod format;
pub mod isa;
pub mod json;
pub mod programs;
pub mod sexpr;
pub mod suite;

use std::sync::atomic::{AtomicUsize, Ordering};

/// Why a run or call stopped with an error.
#[derive(Clone, Debug, PartialEq)]
pub enum Fail {
    /// A runtime error raised by bytecode or a native function.
    Error(String),
    OutOfFuel,
    OutOfMemory,
    StackOverflow,
}

impl Fail {
    pub fn error(message: impl Into<String>) -> Fail {
        Fail::Error(message.into())
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Error(m) => write!(f, "error: {m}"),
            Fail::OutOfFuel => write!(f, "out of fuel"),
            Fail::OutOfMemory => write!(f, "out of memory"),
            Fail::StackOverflow => write!(f, "stack overflow"),
        }
    }
}

pub type Result<T, E = Fail> = std::result::Result<T, E>;

/// Hard limits and GC trigger settings of one engine.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Hard cap on tracked bytes.
    pub max_memory: usize,
    /// Total fuel of the engine. Fuel never refills.
    pub fuel: u64,
    /// Smallest tracked byte count that triggers an automatic collection.
    pub gc_min_threshold: usize,
    /// Collects garbage at every allocation, so that tests catch values
    /// that native code forgot to root.
    pub gc_stress: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_memory: 1 << 32, fuel: u64::MAX, gc_min_threshold: 1 << 20, gc_stress: false }
    }
}

/// Fuel units charged per input byte that a native loop scans.
pub const BYTES_PER_FUEL: usize = 16;

/// Fuel for work over `len` bytes of text.
#[inline(always)]
pub fn text_fuel(len: usize) -> u64 {
    (len / BYTES_PER_FUEL) as u64
}

/// Fuel for comparing two strings. Strings of different lengths compare in
/// constant time.
#[inline]
pub fn compare_fuel(a: &str, b: &str) -> u64 {
    if a.len() == b.len() { text_fuel(a.len()) } else { 0 }
}

/// Charges `units` against `fuel`.
#[inline(always)]
pub fn charge(fuel: &mut u64, units: u64) -> Result<()> {
    match fuel.checked_sub(units) {
        Some(rest) => {
            *fuel = rest;
            Ok(())
        }
        None => {
            *fuel = 0;
            Err(Fail::OutOfFuel)
        }
    }
}

/// Rust stack budget in bytes for nested synchronous callbacks.
pub const MAX_CALLBACK_STACK_BYTES: usize = 256 << 10;

/// Returns an address on the current Rust stack.
#[inline(always)]
pub fn stack_address() -> usize {
    let marker = 0u8;
    std::ptr::from_ref(std::hint::black_box(&marker)).addr()
}

/// Fails if the Rust stack grew more than [`MAX_CALLBACK_STACK_BYTES`] below `base`.
#[inline]
pub fn check_stack(base: usize) -> Result<()> {
    if base.saturating_sub(stack_address()) > MAX_CALLBACK_STACK_BYTES { Err(Fail::StackOverflow) } else { Ok(()) }
}

/// The strict byte counter that every arm checks before each allocation.
///
/// Arms 1 and 2 update it through `&mut self`. Arm 3 shares it with string
/// handles that release bytes on drop, so it updates it atomically.
#[derive(Debug)]
pub struct Tracker {
    used: AtomicUsize,
    limit: usize,
}

impl Tracker {
    pub fn new(limit: usize) -> Tracker {
        Tracker { used: AtomicUsize::new(0), limit }
    }

    #[inline]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Returns whether `bytes` more fit under the limit.
    #[inline]
    pub fn fits(&self, bytes: usize) -> bool {
        self.used().checked_add(bytes).is_some_and(|total| total <= self.limit)
    }

    #[inline]
    pub fn add_mut(&mut self, bytes: usize) {
        *self.used.get_mut() += bytes;
    }

    #[inline]
    pub fn sub_mut(&mut self, bytes: usize) {
        *self.used.get_mut() -= bytes;
    }

    #[inline]
    pub fn add(&self, bytes: usize) {
        self.used.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn sub(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// The shared collection trigger: collect when tracked bytes reach the
/// larger of twice the last post-collection live set and a minimum threshold.
#[derive(Clone, Copy, Debug)]
pub struct GcPolicy {
    min_threshold: usize,
    next_threshold: usize,
    /// Skips automatic collections. Memory-limit collections still run.
    pub paused: bool,
    /// Triggers a collection at every allocation.
    stress: bool,
    pub collections: u64,
}

impl GcPolicy {
    pub fn new(limits: &Limits) -> GcPolicy {
        let min_threshold = limits.gc_min_threshold;
        GcPolicy { min_threshold, next_threshold: min_threshold, paused: false, stress: limits.gc_stress, collections: 0 }
    }

    /// Returns whether an allocation that brings tracked bytes to `used` triggers a collection.
    #[inline]
    pub fn should_collect(&self, used: usize) -> bool {
        self.stress || used > self.threshold()
    }

    /// Tracked byte count above which an allocation triggers a collection.
    #[inline]
    pub fn threshold(&self) -> usize {
        if self.stress {
            0
        } else if self.paused {
            usize::MAX
        } else {
            self.next_threshold
        }
    }

    /// Records a finished collection that left `live` tracked bytes.
    pub fn collected(&mut self, live: usize) {
        self.collections += 1;
        self.next_threshold = live.saturating_mul(2).max(self.min_threshold);
    }
}

/// Inline capacity of a short string.
pub const SHORT_STR_MAX: usize = 14;

/// A string of at most [`SHORT_STR_MAX`] bytes stored inside a value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ShortStr {
    len: u8,
    bytes: [u8; SHORT_STR_MAX],
}

impl ShortStr {
    #[inline]
    pub fn new(s: &str) -> Option<ShortStr> {
        if s.len() > SHORT_STR_MAX {
            return None;
        }
        let mut bytes = [0; SHORT_STR_MAX];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Some(ShortStr { len: s.len() as u8, bytes })
    }

    /// Returns `a` followed by `b`, if the result fits inline.
    #[inline]
    pub fn concat(a: &str, b: &str) -> Option<ShortStr> {
        let len = a.len() + b.len();
        if len > SHORT_STR_MAX {
            return None;
        }
        let mut bytes = [0; SHORT_STR_MAX];
        bytes[..a.len()].copy_from_slice(a.as_bytes());
        bytes[a.len()..len].copy_from_slice(b.as_bytes());
        Some(ShortStr { len: len as u8, bytes })
    }

    /// Returns `a` followed by `b`, if the result fits inline. Joining two
    /// valid UTF-8 strings needs no validation.
    #[inline]
    pub fn concat_short(a: ShortStr, b: ShortStr) -> Option<ShortStr> {
        let (a_len, len) = (a.len as usize, a.len as usize + b.len as usize);
        if len > SHORT_STR_MAX {
            return None;
        }
        let mut bytes = a.bytes;
        bytes[a_len..len].copy_from_slice(&b.bytes[..b.len as usize]);
        Some(ShortStr { len: len as u8, bytes })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // Bytes come from a `&str` cut at its end, so they are valid UTF-8.
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or_default()
    }
}

impl std::fmt::Debug for ShortStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// Appends the decimal form of `i` to `out`.
pub fn push_int(out: &mut String, i: i64) {
    out.push_str(itoa::Buffer::new().format(i));
}
