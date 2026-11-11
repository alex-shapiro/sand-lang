//! The virtual machine: heap, fibers (tasks), scheduler and interpreter.

pub mod bytecode;
pub mod compare;
pub mod display;
pub mod heap;
mod interp;
pub mod iomem;
mod iter;
pub mod meter;
pub mod sched;
pub mod value;

use crate::engine::{LimitKind, Limits};
use crate::native::convert::HostError;
use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter};
use bytecode::*;
use heap::{Heap, Staging};
use meter::Meter;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use value::*;

pub type NativeFn = dyn Fn(&mut Vm, &[VmValue]) -> Result<VmValue, Ctl> + Send + Sync;

#[derive(Clone)]
pub enum HostFn {
    Static(&'static NativeFn),
    Shared(Arc<NativeFn>),
}

impl HostFn {
    #[inline(always)]
    pub fn call(&self, vm: &mut Vm, args: &[VmValue]) -> Result<VmValue, Ctl> {
        match self {
            HostFn::Static(f) => f(vm, args),
            HostFn::Shared(f) => f(vm, args),
        }
    }
}
pub(crate) type Finisher = Box<dyn FnOnce(&mut Vm) -> Result<VmValue, Fail> + Send>;
pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = Finisher> + Send>>;

#[derive(Clone)]
pub struct NativeEntry {
    pub name: std::borrow::Cow<'static, str>,
    pub f: HostFn,
}

/// Control signal from a native call that does not return a value. A `Ctl` is a
/// [`Fail`] or a scheduler request.
///
/// Convert a [`Fail`] into a `Ctl` with `?` or [`From`]. Only `Vm` methods such
/// as [`Vm::yield_now`] and [`Vm::permit`] construct scheduler requests:
///
/// ```compile_fail,E0603
/// let _ = sandlang::vm::CtlKind::Yield;
/// ```
#[must_use]
pub struct Ctl(Box<CtlKind>);

/// Terminal failure that ends a run: a runtime error, a limit error, an
/// interrupt, an exit code, or an internal error.
///
/// Code that cannot make a scheduler request returns a `Fail` instead of a
/// [`Ctl`]. `?` converts a `Fail` into a `Ctl`, and does not convert a `Ctl`
/// into a `Fail`.
#[must_use]
pub struct Fail(Box<FailKind>);

// Keep `Ctl` and `Fail` pointer-sized so a native result is 16 bytes like a
// `VmValue` (see `interp`).
const _: () = assert!(std::mem::size_of::<Ctl>() == std::mem::size_of::<usize>());
const _: () = assert!(std::mem::size_of::<Result<VmValue, Ctl>>() == 16);
const _: () = assert!(std::mem::size_of::<Result<Option<VmValue>, Ctl>>() == 16);
const _: () = assert!(std::mem::size_of::<Result<VmValue, Fail>>() == 16);
const _: () = assert!(std::mem::size_of::<Result<Option<VmValue>, Fail>>() == 16);

/// Classification of a task wait. Determines whether idle wait time counts
/// toward `max_wall_time`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    /// I/O or timer wait. Idle time counts toward `max_wall_time`.
    Io,
    /// User-action wait. Idle time does not count toward `max_wall_time`.
    User,
}

/// Suspended native call: the pending future and its [`WaitKind`].
pub(crate) struct Suspension {
    pub(crate) future: BoxFuture,
    pub(crate) wait_kind: WaitKind,
}

/// Variant of a [`Ctl`].
pub(crate) enum CtlKind {
    Fail(Fail),
    /// Prompt the user for the capability, and then retry the call.
    NeedPermission(Capability),
    /// Suspend the task until the future completes.
    Async(Suspension),
    /// Wait for the scheduler event that [`Vm::block`] registered as
    /// generation `wait_gen`. The call returns the value that wakes the task.
    Blocked {
        wait_gen: u64,
    },
    /// Wait for a `task.race` event registered as generation `wait_gen`. On
    /// wake-up, the engine cancels the remaining `tasks`.
    Racing {
        wait_gen: u64,
        tasks: Vec<VmValue>,
        contenders: u32,
    },
    /// Let other tasks run. The call returns `()`.
    Yield,
    /// Collect garbage and retry the native call once. Garbage may hold an
    /// open-resource slot or I/O buffer space that the call needs. See
    /// [`Vm::resource_slot`].
    Collect,
    /// The native call cancelled the running task. See [`Vm::cancel_task`].
    Cancelled,
}

/// Variant of a [`Fail`].
pub(crate) enum FailKind {
    /// Uncaught runtime error (`kind: message`) that terminates the run (see [`Fail::runtime_error`]).
    RuntimeError(String),
    /// Broken engine invariant that ends the session.
    Internal(String),
    Limit(LimitKind),
    /// Host interrupt (see `Meter::check`).
    Interrupted,
    Exit(i32),
}

impl Fail {
    /// Aborts the run with an uncaught runtime error of `kind`. For recoverable `Result` errors, return [`HostError`] instead.
    #[cold]
    pub fn runtime_error(kind: &str, msg: impl std::fmt::Display) -> Fail {
        Fail(Box::new(FailKind::RuntimeError(format!("{kind}: {msg}"))))
    }

    /// Reports a broken engine invariant and ends the session. For checks in a
    /// host function, use [`Fail::runtime_error`].
    #[cold]
    pub fn internal(msg: impl Into<String>) -> Fail {
        Fail(Box::new(FailKind::Internal(msg.into())))
    }

    /// Reports that the run exceeded limit `k`. If a native call returns
    /// [`LimitKind::Memory`], the engine collects garbage and retries the call
    /// once.
    #[cold]
    pub fn limit(k: LimitKind) -> Fail {
        Fail(Box::new(FailKind::Limit(k)))
    }

    /// Reports that the host interrupted the engine.
    #[cold]
    pub fn interrupted() -> Fail {
        Fail(Box::new(FailKind::Interrupted))
    }

    /// Ends the run with exit code `code`, like the built-in `exit` function.
    pub fn exit(code: i32) -> Fail {
        Fail(Box::new(FailKind::Exit(code)))
    }

    /// Returns the exceeded limit if `self` is a limit error.
    pub fn as_limit(&self) -> Option<LimitKind> {
        match *self.0 {
            FailKind::Limit(k) => Some(k),
            _ => None,
        }
    }

    pub(crate) fn into_kind(self) -> FailKind {
        *self.0
    }
}

impl Ctl {
    /// Suspends the calling task on `suspension`.
    pub(crate) fn suspend(suspension: Suspension) -> Ctl {
        Ctl(Box::new(CtlKind::Async(suspension)))
    }

    /// Prompts the user for `cap`.
    pub(crate) fn need_permission(cap: Capability) -> Ctl {
        Ctl(Box::new(CtlKind::NeedPermission(cap)))
    }

    pub(crate) fn kind(&self) -> &CtlKind {
        &self.0
    }

    pub(crate) fn into_kind(self) -> CtlKind {
        *self.0
    }
}

impl From<Fail> for Ctl {
    #[cold]
    #[inline(never)]
    fn from(fail: Fail) -> Ctl {
        Ctl(Box::new(CtlKind::Fail(fail)))
    }
}

impl From<CtlKind> for Ctl {
    fn from(k: CtlKind) -> Ctl {
        Ctl(Box::new(k))
    }
}

impl std::fmt::Debug for Ctl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind().fmt(f)
    }
}

impl std::fmt::Debug for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            FailKind::RuntimeError(m) => write!(f, "RuntimeError({m})"),
            FailKind::Internal(m) => write!(f, "Internal({m})"),
            FailKind::Limit(k) => write!(f, "Limit({k:?})"),
            FailKind::Interrupted => write!(f, "Interrupted"),
            FailKind::Exit(c) => write!(f, "Exit({c})"),
        }
    }
}

impl std::fmt::Debug for CtlKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlKind::Fail(fail) => fail.fmt(f),
            CtlKind::NeedPermission(c) => write!(f, "NeedPermission({c})"),
            CtlKind::Async(s) => write!(f, "Async({:?})", s.wait_kind),
            CtlKind::Blocked { wait_gen } => write!(f, "Blocked({wait_gen})"),
            CtlKind::Racing { wait_gen, .. } => write!(f, "Racing({wait_gen})"),
            CtlKind::Yield => write!(f, "Yield"),
            CtlKind::Collect => write!(f, "Collect"),
            CtlKind::Cancelled => write!(f, "Cancelled"),
        }
    }
}

/// Aborts execution with a runtime error of kind `kind` (see [`Fail::runtime_error`]).
pub fn fail<T>(kind: &str, msg: impl std::fmt::Display) -> Result<T, Fail> {
    Err(Fail::runtime_error(kind, msg))
}

/// Fails with an internal error.
pub fn internal<T>(msg: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::internal(msg))
}

/// Returns an internal error if a value of kind `what` (see [`Obj::kind_name`])
/// is used as `role` without implementing `trait_name`.
#[cold]
pub(crate) fn without_trait(what: &str, role: &str, trait_name: &str) -> Fail {
    Fail::internal(format!("a {what} is {role}, but its type does not implement `{trait_name}`"))
}

/// What [`Vm::block`] waits for.
pub enum BlockOn {
    Join(VmValue),
    Race(Vec<VmValue>),
    Recv(VmValue),
    Send(VmValue, VmValue),
}

/// A call frame (20 bytes).
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub proto: u32,
    pub ip: u32,
    pub base: u32,
    /// Absolute stack slot receiving the return value.
    pub ret: u32,
    /// Heap handle of the running closure, or `NO_CLOSURE` for direct calls.
    pub closure: u32,
}

/// `Frame::closure` of a direct (closure-less) call.
pub const NO_CLOSURE: u32 = u32::MAX;

impl Frame {
    /// The running closure as a value (`Unit` for direct calls).
    #[inline]
    pub fn closure_value(&self) -> VmValue {
        if self.closure == NO_CLOSURE { VmValue::Unit } else { VmValue::Obj(self.closure) }
    }
}

/// Maximum number of value slots in a fiber's stack.
pub const MAX_STACK_SLOTS: usize = 64 * 1024 * 1024;

/// Initial number of stack slots of a new fiber.
const INITIAL_STACK_SLOTS: usize = 64;

/// At most this many finished fibers are kept for reuse, and only those
/// whose stack and frame list are small (a few MB at most, counted against
/// the memory limit and freed before a memory limit error is reported).
const MAX_SPARE_FIBERS: usize = 4096;
const MAX_SPARE_STACK_SLOTS: usize = 1024;
const MAX_SPARE_FRAMES: usize = 64;

/// Lists of waiting fibers (task waiters, channel receivers) are pruned of
/// stale entries (cancelled waits) once they reach this length, and then
/// again at twice their length after pruning.
pub(crate) const MIN_PRUNE: usize = 16;

/// Whether the wait `(fiber, generation)` is still current.
pub(crate) fn wait_is_live(fibers: &[Option<Box<Fiber>>], (fid, wait_gen): (u32, u64)) -> bool {
    matches!(fibers.get(fid as usize), Some(Some(f)) if f.state == FiberState::Waiting && f.wait_gen == wait_gen)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FiberState {
    Ready,
    Waiting,
    Done,
}

pub struct Fiber {
    pub stack: Vec<VmValue>,
    pub frames: Vec<Frame>,
    pub task: VmValue,
    pub state: FiberState,
    /// Wait generation; waiters record it to detect stale wake-ups.
    pub wait_gen: u64,
    /// Absolute stack slot receiving the result of a blocking call.
    pub result_slot: usize,
    /// Tasks to cancel when a `race` finishes.
    pub race: Vec<VmValue>,
    /// Number of tasks in `race` that can still finish the race.
    pub race_contenders: u32,
    /// Fibers waiting for this one: (fiber, generation).
    pub waiters: Vec<(u32, u64)>,
    /// Stale `waiters` are pruned when it reaches this length.
    pub waiters_prune_at: usize,
    /// Pending IO operation id, if waiting on IO.
    pub io: Option<u64>,
    /// An error (`kind: message`) to raise when the fiber resumes.
    pub pending_error: Option<String>,
    /// Whether the fiber stopped in or just after a native call. Its
    /// instruction pointer is then one past that call.
    pub stopped_in_native_call: bool,
}

/// A stack trace frame from a runtime error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceFrame {
    pub function: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug)]
pub enum Trap {
    /// The script failed with an error, `kind: message`.
    Error {
        message: String,
        trace: Vec<TraceFrame>,
    },
    /// An invariant of the engine broke.
    Internal {
        message: String,
        trace: Vec<TraceFrame>,
    },
    Limit(LimitKind),
    Interrupted,
}

/// An early exit or trap that stops a VM run.
#[derive(Debug)]
pub enum RunEnding {
    /// A task exited with this status code.
    Exit(i32),
    Trap(Trap),
}

impl From<Trap> for RunEnding {
    fn from(trap: Trap) -> RunEnding {
        RunEnding::Trap(trap)
    }
}

/// Output sink for standard streams and logs. `write` must not block for
/// long, because an engine cannot interrupt it. A sink whose destination
/// can block should buffer the data and report the size of the buffer as
/// its backlog; the engine waits for the backlog to shrink instead.
pub trait OutputSink: Send + Sync {
    fn write(&self, data: &[u8]) -> std::io::Result<()>;
    /// Starts writing the buffered data.
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    /// Called before the script reads standard input.
    fn flush_before_input(&self) -> std::io::Result<()> {
        self.flush()
    }
    /// The bytes accepted but not yet written.
    fn backlog(&self) -> usize {
        0
    }
    /// Waits at most `timeout` for the backlog to drop to `max` bytes, and
    /// returns whether it did. With `max` 0, waits until everything this
    /// sink accepted and flushed has been written.
    fn wait_backlog(&self, max: usize, timeout: std::time::Duration) -> bool {
        let _ = (max, timeout);
        true
    }
}

/// When a sink's backlog exceeds this many bytes, the engine waits until
/// half of it has been written.
const MAX_OUTPUT_BACKLOG: usize = 1 << 20;

pub type SharedSink = Arc<dyn OutputSink>;

/// "Always" answers to permission prompts, shared by an engine's runs.
pub type Always = Arc<Mutex<HashMap<Capability, bool>>>;

/// Decides `cap` from the user's earlier answers to prompts, or else with
/// `check`. Returns `Err` if the user must be asked.
fn decide(
    grants_once: &[Capability],
    denies_once: &[Capability],
    always: &Always,
    prompts: bool,
    cap: &Capability,
    check: impl FnOnce() -> Decision,
) -> Result<bool, ()> {
    // "Once" answers given for the call being retried (see `forget_once_answers`).
    if grants_once.contains(cap) {
        return Ok(true);
    }
    if denies_once.contains(cap) {
        return Ok(false);
    }
    if let Some(&d) = always.lock().get(cap) {
        return Ok(d);
    }
    match check() {
        Decision::Allow => Ok(true),
        Decision::Deny => Ok(false),
        Decision::Prompt if prompts => Err(()),
        Decision::Prompt => Ok(false),
    }
}

/// A copy of a VM's permission state (see [`Vm::permits`]).
pub(crate) struct Permits {
    pub policy: Policy,
    space: crate::policy::PathSpace,
    grants_once: Vec<Capability>,
    denies_once: Vec<Capability>,
    always: Always,
    prompts: bool,
}

impl Permits {
    pub fn permit_path(&self, cap: &Capability, entry: bool) -> Result<bool, ()> {
        decide(&self.grants_once, &self.denies_once, &self.always, self.prompts, cap, || {
            if entry { self.policy.check_entry_normalized_in(cap, self.space) } else { self.policy.check_normalized_in(cap, self.space) }
        })
    }
}

/// A small deterministic PRNG (xoshiro256**).
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut sm = seed;
        let mut next = || {
            sm = sm.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = sm;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        };
        Rng { s: [next(), next(), next(), next()] }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, n)` without modulo bias.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % n;
            }
        }
    }

    pub fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

pub fn entropy_seed() -> u64 {
    crate::platform::entropy()
}

/// Engine services available to natives.
pub struct Host {
    pub(crate) policy: Policy,
    pub prompter: Option<Arc<dyn Prompter>>,
    pub grants_once: Vec<Capability>,
    pub denies_once: Vec<Capability>,
    /// "Always" answers to prompts, kept for the engine's lifetime.
    pub always: Always,
    pub stdout: SharedSink,
    pub stderr: SharedSink,
    /// `None` if standard input is not configured.
    pub(crate) stdin: Option<Arc<crate::stdin_feed::StdinChannel>>,
    pub output_bytes: u64,
    pub limits: Limits,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub env: Option<Vec<(String, String)>>,
    pub rng: Rng,
    /// Minimum active script log level, or `None` if logging is disabled.
    pub log_level: Option<crate::stdlib::log::LogLevel>,
    pub(crate) log_sink: Option<Arc<dyn crate::stdlib::log::LogSink>>,
    /// Interrupt flag and I/O waker for the active call.
    pub(crate) interrupt: crate::engine::CallInterrupt,
    /// Cached decisions for stdout (index 0) and stderr (1).
    pub stream_decisions: [Option<bool>; 2],
    #[cfg(feature = "os")]
    pub(crate) tls: crate::stdlib::net::TlsSettings,
    /// The run's idle `http` connections.
    #[cfg(feature = "os")]
    pub(crate) http_pool: Arc<crate::stdlib::http::Pool>,
    pub(crate) regex_cache: Vec<Option<crate::stdlib::regex::RegexCacheEntry>>,
    /// The file system of the `fs` module.
    pub(crate) fs: crate::files::Fs,
    /// Rewrites or blocks outgoing HTTP requests. See [`crate::engine::Config::request_filter`].
    #[cfg_attr(not(any(feature = "os", web)), expect(dead_code, reason = "HTTP client is disabled in this build"))]
    pub(crate) request_filter: Option<crate::engine::RequestFilter>,
    /// Clock and timer provider for the engine.
    pub clock: Arc<dyn crate::platform::Clock>,
    /// Monotonic clock baseline for time measurements.
    pub origin: crate::platform::Instant,
    /// Network provider for outgoing connections.
    #[cfg(feature = "os")]
    pub(crate) network: Arc<dyn crate::network::Network>,
    pub(crate) time_zone: Option<jiff::tz::TimeZone>,
}

pub struct Vm {
    pub prog: Arc<Program>,
    pub natives: Arc<Vec<NativeEntry>>,
    pub heap: Heap,
    pub globals: Vec<VmValue>,
    pub consts: Vec<VmValue>,
    pub fibers: Vec<Option<Box<Fiber>>>,
    pub free_fibers: Vec<u32>,
    /// Finished fibers kept for reuse, boxed so they move without copying.
    #[allow(clippy::vec_box)]
    spare_fibers: Vec<Box<Fiber>>,
    pub ready: VecDeque<u32>,
    pub current: u32,
    pub main_fiber: u32,
    pub live_tasks: usize,
    pub host: Host,
    pub io: sched::IoTable,
    pub gen_counter: u64,
    /// Fuel accounting (see `meter`).
    pub meter: Meter,
    /// When the run exceeds the wall-time limit (moved on by the time
    /// spent waiting for prompt answers).
    pub(crate) deadline: Option<crate::platform::Instant>,
    /// Set while a native that asked for a collection (`CtlKind::Collect`)
    /// is called again after it.
    pub(crate) collected: bool,
    /// Function and instruction pointer of the active native call.
    caller_site: (u32, u32),
    /// Extra GC roots (e.g. values held by the test runner).
    pub pinned: Vec<VmValue>,
    /// The box of the last `Blocked` or `Yield` consumed (see `Vm::signal`).
    spare_ctl: Option<Box<CtlKind>>,
    /// The map handle and slot of the last lookup of a key other than an
    /// int, which a store to the same key reuses (see `Vm::map_set`).
    last_slot: std::cell::Cell<(u32, u32)>,
    /// Strings of one ASCII character, by byte (`Unit` until first
    /// made); shared, since strings are immutable. GC roots.
    ascii_strs: Vec<VmValue>,

    /// Set by an internal error (a bug), including a Rust panic caught in the
    /// interpreter, a native or an I/O finisher: the VM is memory safe but may
    /// be inconsistent, so a REPL session discards it.
    pub(crate) broken: bool,
    /// Testing aid that fails the first memory or resource check in each native call and retryable instruction. See [`crate::engine::Config::retry_stress`].
    pub(crate) retry_stress: bool,
    /// True while `retry_stress` runs an instruction a second time after garbage collection.
    pub(crate) instruction_retried: bool,
    /// Struct nesting depth during conversion to Sand (see [`struct_into_vm_value`](crate::native::struct_into_vm_value)).
    pub(crate) struct_depth: usize,
}

pub const SLICE_FUEL: i64 = 20_000;

impl Fiber {
    /// One past the highest stack slot used by the innermost frame. Slots at
    /// and above it are dead: they are not GC roots and may hold stale values.
    pub fn live_top(&self, prog: &Program) -> usize {
        match self.frames.last() {
            Some(fr) => (fr.base as usize + prog.protos[fr.proto as usize].nregs as usize).min(self.stack.len()),
            None => 0,
        }
    }

    /// Grows the stack (geometrically) to at least `need` slots, counting
    /// the memory in `heap.fiber_bytes`. Fails with `"stack overflow"` beyond
    /// `MAX_STACK_SLOTS`, or with the memory limit error.
    #[cold]
    #[inline(never)]
    pub fn grow_stack(&mut self, need: usize, heap: &mut Heap) -> Result<(), Fail> {
        if need > MAX_STACK_SLOTS {
            return Err(Fail::runtime_error("limit", "stack overflow"));
        }
        let n = need.max(self.stack.len() * 2).min(MAX_STACK_SLOTS);
        let before = self.stack.capacity();
        let extra = n.saturating_sub(before) * std::mem::size_of::<VmValue>();
        heap.check_memory(extra)?;
        self.stack.reserve_exact(n - self.stack.len());
        self.stack.resize(n, VmValue::Unit);
        heap.fiber_bytes += self.stack.capacity().saturating_sub(before) * std::mem::size_of::<VmValue>();
        Ok(())
    }

    /// Makes room for more frames (called when `frames` is full), counting
    /// the memory like [`Fiber::grow_stack`].
    #[cold]
    #[inline(never)]
    pub fn grow_frames(&mut self, heap: &mut Heap) -> Result<(), Fail> {
        let before = self.frames.capacity();
        let more = before.max(8);
        heap.check_memory(more * std::mem::size_of::<Frame>())?;
        self.frames.reserve_exact(more);
        heap.fiber_bytes += self.frames.capacity().saturating_sub(before) * std::mem::size_of::<Frame>();
        Ok(())
    }

    /// Memory used by the fiber, as counted in `Heap::fiber_bytes`.
    pub fn footprint(&self) -> usize {
        std::mem::size_of::<Fiber>()
            + self.stack.capacity() * std::mem::size_of::<VmValue>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
            + self.race.capacity() * std::mem::size_of::<VmValue>()
            + self.waiters.capacity() * std::mem::size_of::<(u32, u64)>()
    }
}

impl Vm {
    pub fn new(prog: Arc<Program>, natives: Arc<Vec<NativeEntry>>, host: Host) -> Vm {
        let heap = Heap::new(host.limits.max_memory, host.limits.max_open_resources);
        let consts = Vec::with_capacity(prog.consts.len());
        let globals = vec![VmValue::Uninit; prog.nglobals];
        let meter = Meter::new(host.limits.max_instructions, host.interrupt.flag.clone(), host.clock.clone());
        let io = sched::IoTable::with_shared(host.interrupt.io.clone());
        let mut vm = Vm {
            prog,
            natives,
            heap,
            globals,
            consts,
            fibers: Vec::new(),
            free_fibers: Vec::new(),
            spare_fibers: Vec::new(),
            ready: VecDeque::new(),
            current: 0,
            main_fiber: 0,
            live_tasks: 0,
            host,
            io,
            gen_counter: 1,
            meter,
            deadline: None,
            collected: false,
            caller_site: (0, 0),
            pinned: Vec::new(),
            spare_ctl: None,
            last_slot: std::cell::Cell::new((u32::MAX, 0)),
            ascii_strs: Vec::new(),
            broken: false,
            retry_stress: false,
            instruction_retried: false,
            struct_depth: 0,
        };
        vm.load_program();
        vm
    }

    /// Creates the values of the program's constants and the slots of its
    /// globals that do not exist yet: all of them for a new VM, or those a
    /// REPL session appended to the program.
    pub fn load_program(&mut self) {
        let prog = self.prog.clone();
        for i in self.consts.len()..prog.consts.len() {
            if let Const::Str(s) = &prog.consts[i] {
                let v = self.short_str(s);
                self.consts.push(v);
                continue;
            }
            let heap = &mut self.heap;
            let v = match &prog.consts[i] {
                Const::Int(i) => VmValue::Int(*i),
                Const::Float(f) => VmValue::Float(*f),
                Const::Dur(d) => VmValue::Dur(*d),
                Const::Str(_) => unreachable!(),
                Const::Bytes(b) => heap.new_bytes(b.clone()),
                Const::Fn(p) => heap.alloc(Obj::Closure(*p, Box::new([]))),
                Const::Native(n) => heap.alloc(Obj::NativeFn(*n)),
                Const::TypeDesc(t) => heap.alloc(Obj::TypeDesc(TypeDescObj::constant(t.clone()))),
                Const::Tuple(items) => heap.new_tuple(&items.iter().map(|&i| self.consts[i as usize]).collect::<Vec<_>>()),
            };
            self.consts.push(v);
        }
        if self.globals.len() < self.prog.nglobals {
            self.globals.resize(self.prog.nglobals, VmValue::Uninit);
        }
    }

    /// Which globals are initialized, by id.
    pub(crate) fn initialized_globals(&self) -> Vec<bool> {
        self.globals.iter().map(|v| !matches!(v, VmValue::Uninit)).collect()
    }

    /// Ends every task: after the main task of a REPL session input has
    /// failed, the session continues with a VM that runs nothing. Tasks that
    /// were running are marked cancelled; heap and globals are kept.
    pub fn abandon_tasks(&mut self) {
        for slot in &mut self.fibers {
            if let Some(f) = slot.take()
                && let Obj::Task(t) = self.heap.get_mut(f.task)
                && t.state == TaskState::Running
            {
                t.state = TaskState::Cancelled;
            }
        }
        self.fibers.clear();
        self.free_fibers.clear();
        self.ready.clear();
        self.io.clear();
        self.live_tasks = 0;
        self.host.grants_once.clear();
        self.host.denies_once.clear();
    }

    /// Resets per-run counters and attaches `call` for a new session run.
    /// `call` must share the VM's [`sched::IoShared`].
    pub(crate) fn begin_session_run(&mut self, call: &crate::engine::CallInterrupt) {
        self.meter.reset(call.flag.clone());
        self.host.output_bytes = 0;
        self.host.interrupt = call.clone();
    }

    /// Frees the fiber of the main task after it finished, leaving other
    /// tasks to run (REPL sessions).
    pub fn release_main(&mut self) {
        let fid = self.main_fiber;
        if let Some(f) = self.fibers.get_mut(fid as usize).and_then(Option::take) {
            if let Obj::Task(t) = self.heap.get_mut(f.task) {
                t.state = TaskState::Done;
            }
            self.free_fibers.push(fid);
            self.live_tasks = self.live_tasks.saturating_sub(1);
        }
    }

    /// Wraps `kind` in a [`Ctl`], reusing `spare_ctl` if available. `kind`
    /// must be `Blocked` or `Yield`.
    #[inline]
    fn signal(&mut self, kind: CtlKind) -> Ctl {
        match self.spare_ctl.take() {
            Some(mut b) => {
                std::mem::forget(std::mem::replace(&mut *b, kind));
                Ctl(b)
            }
            None => Ctl::from(kind),
        }
    }

    /// Keeps the box of a consumed `Blocked` or `Yield` for [`Vm::signal`].
    #[inline]
    pub(crate) fn recycle(&mut self, c: Ctl) {
        debug_assert!(matches!(c.kind(), CtlKind::Blocked { .. } | CtlKind::Yield));
        if self.spare_ctl.is_none() {
            self.spare_ctl = Some(c.0);
        }
    }

    /// Lets other tasks run (a native's result); the call returns `()`.
    pub fn yield_now(&mut self) -> Ctl {
        self.signal(CtlKind::Yield)
    }

    pub fn next_gen(&mut self) -> u64 {
        self.gen_counter += 1;
        self.gen_counter
    }

    // ------------------------------------------------------------------
    // Values

    pub fn str(&self, v: VmValue) -> &str {
        self.heap.str(v)
    }

    pub fn new_str(&mut self, s: impl Into<compact_str::CompactString>) -> VmValue {
        self.heap.new_str(s)
    }

    /// [`Heap::staged`], also passing the shared strings of one ASCII
    /// character (indexed by the character; see `short_str`), for natives
    /// that split text into many short pieces.
    pub fn staged_strs<R>(&mut self, f: impl FnOnce(&Heap, &mut Staging<'_>, &[VmValue]) -> R) -> R {
        self.ascii_table();
        let ascii = &self.ascii_strs;
        self.heap.staged(|heap, st| f(heap, st, ascii))
    }

    /// Makes the shared strings of one ASCII character (once per VM).
    fn ascii_table(&mut self) {
        if self.ascii_strs.is_empty() {
            self.ascii_strs = (0..128u8).map(|b| self.heap.new_str(char::from(b).to_string())).collect();
        }
    }

    /// The shared string of one ASCII character `b` (see `short_str`).
    #[inline]
    fn ascii_str(&mut self, b: u8) -> VmValue {
        match self.ascii_strs.get(b as usize) {
            Some(&v) => v,
            None => self.short_str(std::str::from_utf8(&[b]).unwrap_or("?")),
        }
    }

    /// A string equal to `s`, which may be shared: strings of one ASCII
    /// character (as `chars`, `char_at` and splitting produce in bulk) are
    /// made once per VM.
    pub fn short_str(&mut self, s: &str) -> VmValue {
        match s.as_bytes() {
            &[b] if b.is_ascii() => {
                self.ascii_table();
                self.ascii_strs[b as usize]
            }
            _ => self.heap.new_str(s),
        }
    }

    #[inline]
    pub fn some(&mut self, v: VmValue) -> VmValue {
        let id = self.prog.ids.some;
        match VmValue::variant1(id, v) {
            Some(inline) => inline,
            None => self.heap.new_variant(id, &[v]),
        }
    }

    pub fn none(&self) -> VmValue {
        VmValue::Tag(self.prog.ids.none)
    }

    pub fn ok(&mut self, v: VmValue) -> VmValue {
        let id = self.prog.ids.ok;
        self.heap.new_variant(id, &[v])
    }

    pub fn err(&mut self, v: VmValue) -> VmValue {
        let id = self.prog.ids.err;
        self.heap.new_variant(id, &[v])
    }

    pub fn new_value<T: value::ValueType>(&mut self, x: T) -> VmValue {
        self.heap.alloc(value::value_obj(x))
    }

    #[inline]
    pub fn option(&mut self, v: Option<VmValue>) -> VmValue {
        match v {
            Some(v) => self.some(v),
            None => self.none(),
        }
    }

    /// Unpacks an Option: `Some(x)` → `Some(x)`.
    pub fn as_option(&self, v: VmValue) -> Option<VmValue> {
        if self.heap.variant_id(v) == self.prog.ids.some { self.heap.field(v, 0) } else { None }
    }

    /// Unpacks a Result: `Ok(x)` → `Ok(x)`, `Err(e)` → `Err(e)`.
    pub fn as_result(&self, v: VmValue) -> Result<VmValue, VmValue> {
        let payload = self.heap.field(v, 0).unwrap_or(VmValue::Unit);
        if self.heap.variant_id(v) == self.prog.ids.ok { Ok(payload) } else { Err(payload) }
    }

    /// Payload field `i` of an enum variant value, whatever its
    /// representation (`Unit` if there is no such field).
    pub fn variant_payload(&self, v: VmValue, i: usize) -> VmValue {
        self.heap.field(v, i).unwrap_or(VmValue::Unit)
    }

    /// The display form of `v`, charging the meter for the work.
    pub fn display(&self, v: VmValue) -> Result<String, Fail> {
        display::to_display_metered(&self.prog, &self.heap, &self.meter, v)
    }

    /// The debug form of `v`, charging the meter for the work.
    pub fn debug(&self, v: VmValue) -> Result<String, Fail> {
        display::to_debug_metered(&self.prog, &self.heap, &self.meter, v)
    }

    /// If [`Config::retry_stress`](crate::engine::Config::retry_stress) is enabled and
    /// this call is not a retry, forces the next memory or resource check to fail.
    pub(crate) fn arm_retry_stress(&self) {
        self.heap.fail_next_check.set(self.retry_stress && !self.collected);
    }

    /// Returns a memory-limit error if `extra` additional heap bytes exceed the
    /// limit (see [`Heap::check_memory`](heap::Heap::check_memory)).
    pub fn check_memory(&self, extra: usize) -> Result<(), Fail> {
        self.heap.check_memory(extra)
    }

    /// Runs `f` with heap memory-limit checks disabled so post-effect result conversion cannot trigger a retry. Call [`Vm::enforce_limit`] after rooting the result.
    pub(crate) fn unchecked<R>(&mut self, f: impl FnOnce(&mut Vm) -> R) -> R {
        self.heap.unchecked = true;
        let r = f(self);
        self.heap.unchecked = false;
        r
    }

    /// Runs a full garbage collection if the heap exceeds the memory limit, and returns `Err(LimitKind::Memory)` if it still exceeds the limit. All live values must be rooted.
    pub(crate) fn enforce_limit(&mut self, current: Option<&mut Fiber>) -> Result<(), LimitKind> {
        if !self.heap.over_limit() {
            return Ok(());
        }
        self.collect_all_garbage(current);
        if self.heap.over_limit() { Err(LimitKind::Memory) } else { Ok(()) }
    }

    /// Starts accounting for the buffers of an I/O operation that are not
    /// yet heap values (see [`iomem`]); the operation grows the reservation
    /// as its buffers grow and drops it before turning them into values.
    pub fn io_reservation(&self) -> iomem::Reservation {
        self.heap.io_memory.publish_heap(self.heap.used());
        iomem::Reservation::new(&self.heap.io_memory)
    }

    /// Reserves `bytes` of off-heap I/O memory before a side effect. Triggers a
    /// garbage collection and retry if the limit is reached, then fails with
    /// kind `"limit"` if `bytes` still do not fit.
    pub fn reserve_io(&mut self, bytes: usize) -> Result<iomem::Reservation, HostError> {
        self.try_reserve_io(bytes)?.ok_or_else(|| HostError::new("limit", format!("cannot buffer {bytes} bytes: {}", iomem::MemoryLimit)))
    }

    /// Reserves `bytes` of off-heap I/O memory, or returns `Ok(None)` if `bytes`
    /// do not fit after garbage collection.
    pub fn try_reserve_io(&mut self, bytes: usize) -> Result<Option<iomem::Reservation>, Ctl> {
        let mut r = self.io_reservation();
        if self.heap.fail_next_check.replace(false) {
            return Err(CtlKind::Collect.into());
        }
        match r.ensure(bytes) {
            Ok(()) => Ok(Some(r)),
            Err(_) if !self.collected => Err(CtlKind::Collect.into()),
            Err(_) => Ok(None),
        }
    }

    /// Acquires an open-resource slot counted against `max_open_resources`
    /// before a side effect. Triggers a garbage collection and retry if the
    /// limit is reached, then fails with kind `"limit"` if the limit is still
    /// reached.
    pub fn resource_slot(&mut self) -> Result<iomem::ResourceSlot, HostError> {
        if self.heap.fail_next_check.replace(false) {
            return Err(Ctl::from(CtlKind::Collect).into());
        }
        match iomem::ResourceSlot::try_new(&self.heap.io_memory) {
            Some(slot) => Ok(slot),
            None if !self.collected => Err(Ctl::from(CtlKind::Collect).into()),
            None => Err(HostError::new("limit", format!("too many open resources (limit {})", self.heap.io_memory.max_resources()))),
        }
    }

    /// Acquires an open-resource slot and reserves `buffers` bytes for `what`
    /// (such as `"a connection"`).
    pub fn resource_slot_with(&mut self, buffers: usize, what: &str) -> Result<iomem::ResourceSlot, HostError> {
        let mut slot = self.resource_slot()?;
        self.heap.io_memory.publish_heap(self.heap.used());
        match slot.reserve_buffers(buffers) {
            Ok(()) => Ok(slot),
            Err(_) if !self.collected => Err(Ctl::from(CtlKind::Collect).into()),
            Err(e) => Err(HostError::new("limit", format!("cannot open {what}: its buffers ({buffers} bytes) do not fit: {e}"))),
        }
    }

    // ------------------------------------------------------------------
    // Metering (see `meter` for the cost model)

    /// Charges `units` of fuel for native work: one unit per element
    /// visited, compared or produced. Fails if a limit is exceeded or the
    /// engine was interrupted; propagate the error with `?`.
    #[inline]
    pub fn charge(&self, units: u64) -> Result<(), Fail> {
        self.meter.charge(units)
    }

    /// Charges for `n` bytes copied, compared or scanned.
    #[inline]
    pub fn charge_bytes(&self, n: usize) -> Result<(), Fail> {
        self.meter.charge_bytes(n)
    }

    /// Charges for `n` bytes processed one at a time (hashing, encoding).
    #[inline]
    pub fn charge_slow_bytes(&self, n: usize) -> Result<(), Fail> {
        self.meter.charge_slow_bytes(n)
    }

    // ------------------------------------------------------------------
    // Policy

    /// Checks a capability: `Ok(true)` allowed, `Ok(false)` denied, or a
    /// request to prompt the user. Paths (`FsRead`, `FsWrite`, `Import`) are
    /// paths of the engine's file system (the host's or a memory file
    /// system), normalized as it does.
    pub fn permit(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        let space = self.host.fs.space();
        self.permit_by(cap, |p, c| p.check_in(c, space))
    }

    /// Checks `cap` with [`Vm::permit`] and returns a `"permission_denied"` [`HostError`] if denied.
    pub fn require(&mut self, cap: &Capability) -> Result<(), HostError> {
        if self.permit(cap)? { Ok(()) } else { Err(HostError::new("permission_denied", format!("permission denied: {cap}"))) }
    }

    pub fn permit_normalized(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        self.permit_path(cap, false)
    }

    pub fn permit_entry(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        let space = self.host.fs.space();
        self.permit_by(cap, |p, c| p.check_entry_in(c, space))
    }

    pub(crate) fn permit_path(&mut self, cap: &Capability, entry: bool) -> Result<bool, Ctl> {
        let space = self.host.fs.space();
        self.permit_by(cap, |p, c| if entry { p.check_entry_normalized_in(c, space) } else { p.check_normalized_in(c, space) })
    }

    /// Whether prompt answers are remembered (for this call or for good):
    /// they apply to capabilities as the policy saw them when prompting.
    pub(crate) fn has_answers(&self) -> bool {
        !self.host.grants_once.is_empty() || !self.host.denies_once.is_empty() || !self.host.always.lock().is_empty()
    }

    fn permit_by(&mut self, cap: &Capability, check: impl FnOnce(&Policy, &Capability) -> Decision) -> Result<bool, Ctl> {
        let h = &self.host;
        decide(&h.grants_once, &h.denies_once, &h.always, h.prompter.is_some(), cap, || check(&h.policy, cap))
            .map_err(|()| Ctl::need_permission(cap.clone()))
    }

    /// A copy of the permission state, for deciding on normalized paths on
    /// another thread.
    pub(crate) fn permits(&self) -> Permits {
        let h = &self.host;
        Permits {
            policy: h.policy.clone(),
            space: h.fs.space(),
            grants_once: h.grants_once.clone(),
            denies_once: h.denies_once.clone(),
            always: h.always.clone(),
            prompts: h.prompter.is_some(),
        }
    }

    /// Runs `job`, which may block. With the host's file system, the job
    /// runs on a worker thread, and the VM stops waiting for it when the
    /// meter reports an interrupt or a limit. With a memory file system,
    /// the job runs on this thread. The job charges its
    /// [`meter::Allowance`], and the meter is then charged the same amount.
    /// If a charge to the allowance failed, the job's result is discarded
    /// and the meter's error is returned.
    pub(crate) fn offload<T: Send + 'static>(&self, job: impl FnOnce(&mut meter::Allowance) -> T + Send + 'static) -> Result<T, Fail> {
        let mut allowance = self.meter.allowance();
        let finish = |t: T, used: u64, stopped: bool| {
            self.meter.charge(used)?;
            if stopped { Err(self.stopped()) } else { Ok(t) }
        };
        if self.host.fs.memory().is_some() {
            let t = job(&mut allowance);
            return finish(t, allowance.used(), allowance.stopped());
        }
        #[cfg(feature = "os")]
        {
            let abandon = allowance.abandon_flag();
            let pending = crate::platform::offload(move || {
                let t = job(&mut allowance);
                (t, allowance.used(), allowance.stopped())
            });
            match pending.wait(|| self.meter.check()) {
                Ok(Ok((t, used, stopped))) => finish(t, used, stopped),
                Ok(Err(panic)) => Err(Fail::internal(panic)),
                Err(c) => {
                    abandon.store(true, std::sync::atomic::Ordering::Relaxed);
                    Err(c)
                }
            }
        }
        #[cfg(not(feature = "os"))]
        unreachable!("without the `os` feature, the file system is in memory")
    }

    /// The error to return when a charge to a [`meter::Allowance`] failed.
    fn stopped(&self) -> Fail {
        self.meter.check().err().unwrap_or_else(Fail::interrupted)
    }

    /// Forgets "once" answers. They hold while a native is retried after
    /// prompts (several, if it checks several capabilities like `fs.copy`)
    /// and are forgotten when it returns anything else.
    #[inline]
    pub(crate) fn forget_once_answers(&mut self) {
        if !self.host.grants_once.is_empty() || !self.host.denies_once.is_empty() {
            self.host.grants_once.clear();
            self.host.denies_once.clear();
        }
    }

    pub fn apply_answer(&mut self, cap: Capability, answer: PromptAnswer) {
        match answer {
            PromptAnswer::AllowOnce => self.host.grants_once.push(cap),
            PromptAnswer::DenyOnce => self.host.denies_once.push(cap),
            PromptAnswer::AllowAlways | PromptAnswer::DenyAlways => {
                let allow = answer == PromptAnswer::AllowAlways;
                self.host.always.lock().insert(cap, allow);
            }
        }
    }

    /// Starts writing the buffered standard output, without waiting for it.
    pub fn flush_stdout(&self) {
        let _ = self.host.stdout.flush();
    }

    /// Flushes standard output and error, and waits until they are written.
    /// Fails if the meter reports an interrupt or a limit first.
    pub(crate) fn drain_output(&mut self) -> Result<(), RunEnding> {
        self.flush_stdout();
        let drained = [&self.host.stdout, &self.host.stderr].into_iter().try_for_each(|sink| self.wait_backlog(sink, 0));
        drained.map_err(|fail| self.run_ending_of(fail, |_| vec![]))
    }

    /// Drains standard output and error after a scheduler run. A drain failure
    /// overrides an `Ok` result or [`RunEnding::Exit`], but preserves
    /// [`RunEnding::Trap`].
    pub(crate) fn drain_output_after<T>(&mut self, run_result: Result<T, RunEnding>) -> Result<T, RunEnding> {
        let drained = self.drain_output();
        if matches!(run_result, Err(RunEnding::Trap(_))) { run_result } else { drained.and(run_result) }
    }

    /// Waits until `sink`'s backlog is at most `max` bytes. Fails if the
    /// meter reports an interrupt or a limit first.
    fn wait_backlog(&self, sink: &SharedSink, max: usize) -> Result<(), Fail> {
        while !sink.wait_backlog(max, std::time::Duration::from_millis(5)) {
            self.meter.check()?;
        }
        Ok(())
    }

    /// Flushes standard output before reading standard input, if the sink
    /// wants to (see [`OutputSink::flush_before_input`]), and waits until it
    /// is written.
    pub fn flush_before_input(&self) -> Result<(), Fail> {
        let _ = self.host.stdout.flush_before_input();
        self.wait_backlog(&self.host.stdout, 0)
    }

    /// Writes to stdout (1) or stderr (2), enforcing policy and output
    /// limits; a denial fails (as `print` does).
    pub fn write_stream(&mut self, which: u8, data: &[u8]) -> Result<(), Ctl> {
        self.admit_output(which, data.len())?;
        Ok(self.write_counted(which, data)?)
    }

    pub(crate) fn write_counted(&mut self, which: u8, data: &[u8]) -> Result<(), Fail> {
        let _ = self.stream_sink(which).write(data);
        self.limit_backlog(which)
    }

    /// Writes string `s` (a heap value) to stdout (1) or stderr (2), like
    /// `write_stream`, without copying it out of the heap first.
    pub fn write_stream_str(&mut self, which: u8, s: VmValue) -> Result<(), Ctl> {
        self.admit_output(which, self.heap.str(s).len())?;
        let _ = self.stream_sink(which).write(self.heap.str(s).as_bytes());
        Ok(self.limit_backlog(which)?)
    }

    /// If the backlog of stream `which` exceeds [`MAX_OUTPUT_BACKLOG`], waits
    /// until half of it has been written.
    fn limit_backlog(&self, which: u8) -> Result<(), Fail> {
        let sink = self.stream_sink(which);
        if sink.backlog() > MAX_OUTPUT_BACKLOG {
            self.wait_backlog(sink, MAX_OUTPUT_BACKLOG / 2)?;
        }
        Ok(())
    }

    /// Writes the bytes or string `v` (a heap value) to stdout (1) or
    /// stderr (2) for `io.stdout` and `io.stderr`: a denial or a failed
    /// write is the `Err(Error)` value it returns (`Ok(Unit)` otherwise);
    /// exceeding the output limit stops the run.
    pub fn write_stream_value(&mut self, which: u8, v: VmValue) -> Result<(), HostError> {
        let len = match self.heap.get(v) {
            Obj::Str(s) => s.len(),
            _ => self.heap.bytes_of(v).len(),
        };
        if !self.output_allowed(which)? {
            let name = if which == 1 { "stdout" } else { "stderr" };
            return Err(HostError::new("permission_denied", format!("permission denied: {name}")));
        }
        self.count_output(which, len)?;
        let data = match self.heap.get(v) {
            Obj::Str(s) => s.as_bytes(),
            _ => self.heap.bytes_of(v),
        };
        match self.stream_sink(which).write(data) {
            Ok(()) => Ok(self.limit_backlog(which)?),
            Err(e) => Err(e.into()),
        }
    }

    /// Checks the policy and the output limit for writing `len` bytes to
    /// stream `which`, and counts them; a denial fails.
    fn admit_output(&mut self, which: u8, len: usize) -> Result<(), Ctl> {
        if !self.output_allowed(which)? {
            return Err(Fail::runtime_error(
                "permission_denied",
                if which == 1 { "permission denied: stdout" } else { "permission denied: stderr" },
            )
            .into());
        }
        Ok(self.count_output(which, len)?)
    }

    /// Whether the policy allows writing to stream `which`.
    pub(crate) fn output_allowed(&mut self, which: u8) -> Result<bool, Ctl> {
        let slot = usize::from(which != 1);
        let allowed = match self.host.stream_decisions[slot] {
            Some(d) => d,
            None => {
                let cap = if which == 1 { Capability::Stdout } else { Capability::Stderr };
                let d = self.permit(&cap)?;
                // Cache static decisions for the hot path.
                if !self.host.always.lock().contains_key(&cap)
                    && self.host.grants_once.is_empty()
                    && self.host.denies_once.is_empty()
                    && let Decision::Allow | Decision::Deny = self.host.policy.check(&cap)
                {
                    self.host.stream_decisions[slot] = Some(d);
                }
                d
            }
        };
        Ok(allowed)
    }

    /// Counts `len` bytes written to stream `which` against the output limit.
    pub(crate) fn count_output(&mut self, which: u8, len: usize) -> Result<(), Fail> {
        self.host.output_bytes += len as u64;
        if let Some(max) = self.host.limits.max_output_bytes
            && self.host.output_bytes > max
        {
            return Err(Fail::limit(LimitKind::Output));
        }
        if which != 1 {
            self.flush_stdout();
        }
        Ok(())
    }

    /// The sink of stream `which`.
    fn stream_sink(&self, which: u8) -> &SharedSink {
        if which == 1 { &self.host.stdout } else { &self.host.stderr }
    }

    // ------------------------------------------------------------------
    // Fibers

    /// Creates a fiber that calls `closure` with no arguments.
    pub fn spawn_fiber(&mut self, closure: VmValue, task: VmValue) -> Result<u32, Fail> {
        let (proto, ncaps) = match self.heap.get(closure) {
            Obj::Closure(p, caps) => (*p, caps.len()),
            _ => return internal("spawn: not a closure"),
        };
        let nregs = self.prog.protos[proto as usize].nregs as usize;
        let VmValue::Obj(closure) = closure else { return internal("spawn: not a closure") };
        let frame = Frame { proto, ip: 0, base: 0, ret: 0, closure };
        let fiber = match self.spare_fibers.pop() {
            // A recycled fiber is already counted in `fiber_bytes`.
            Some(mut f) if f.stack.len() >= nregs => {
                f.frames.push(frame);
                f.task = task;
                f
            }
            spare => {
                if let Some(f) = spare {
                    self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(f.footprint());
                }
                let stack = nregs.max(INITIAL_STACK_SLOTS);
                if self.heap.would_exceed(stack * std::mem::size_of::<VmValue>()) {
                    return Err(Fail::limit(LimitKind::Memory));
                }
                let mut frames = Vec::with_capacity(8);
                frames.push(frame);
                let fiber = Box::new(Fiber {
                    stack: vec![VmValue::Unit; stack],
                    frames,
                    task,
                    state: FiberState::Ready,
                    wait_gen: 0,
                    result_slot: 0,
                    race: vec![],
                    race_contenders: 0,
                    waiters: vec![],
                    waiters_prune_at: MIN_PRUNE,
                    io: None,
                    pending_error: None,
                    stopped_in_native_call: false,
                });
                self.heap.fiber_bytes += fiber.footprint();
                fiber
            }
        };
        // A task's function takes no parameters: its captures come first.
        let mut fiber = fiber;
        if let Obj::Closure(_, caps) = self.heap.get(VmValue::Obj(closure)) {
            fiber.stack[..ncaps].copy_from_slice(caps);
        }
        let id = match self.free_fibers.pop() {
            Some(id) => {
                self.fibers[id as usize] = Some(fiber);
                id
            }
            None => {
                self.fibers.push(Some(fiber));
                (self.fibers.len() - 1) as u32
            }
        };
        self.ready.push_back(id);
        Ok(id)
    }

    /// Keeps the allocations of a finished fiber for a later `spawn_fiber`,
    /// if it is small; otherwise frees it. Pooled fibers stay counted in
    /// `heap.fiber_bytes`. Their stacks are cleared: a reused fiber's live
    /// window is a GC root before the new task writes every slot of it, so
    /// it must not hold handles of objects freed since.
    fn recycle_fiber(&mut self, mut f: Box<Fiber>) {
        let small = f.stack.capacity() <= MAX_SPARE_STACK_SLOTS && f.frames.capacity() <= MAX_SPARE_FRAMES;
        if self.spare_fibers.len() >= MAX_SPARE_FIBERS || !small || f.waiters.capacity() > MIN_PRUNE {
            self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(f.footprint());
            return;
        }
        let slots = f.stack.capacity();
        f.stack.clear();
        f.stack.resize(slots, VmValue::Unit);
        f.frames.clear();
        f.task = VmValue::Unit;
        f.state = FiberState::Ready;
        f.wait_gen = 0;
        f.result_slot = 0;
        f.waiters.clear();
        f.waiters_prune_at = MIN_PRUNE;
        f.io = None;
        f.stopped_in_native_call = false;
        f.pending_error = None;
        self.spare_fibers.push(f);
    }

    /// Frees spare fibers before a memory limit error is reported.
    fn free_spare_fibers(&mut self) {
        let spare: usize = self.spare_fibers.iter().map(|f| f.footprint()).sum();
        self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(spare);
        self.spare_fibers = Vec::new();
    }

    /// Spawns a new task running `closure`; returns the task handle.
    pub fn spawn_task(&mut self, closure: VmValue) -> Result<VmValue, Fail> {
        if self.live_tasks >= self.host.limits.max_tasks {
            return fail("limit", "task limit exceeded");
        }
        if let Obj::Closure(p, _) = self.heap.get(closure) {
            let p = &self.prog.protos[*p as usize];
            self.meter.charge(meter::code_fuel(p.code.len() + p.nregs as usize) as u64)?;
        }
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: u32::MAX, state: TaskState::Running, result: VmValue::Unit })));
        let fid = self.spawn_fiber(closure, task)?;
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.fiber = fid;
        }
        self.live_tasks += 1;
        Ok(task)
    }

    /// Delivers a value to a waiting fiber (if the wait is still current).
    pub fn deliver(&mut self, fid: u32, wait_gen: u64, v: VmValue) -> bool {
        let Some(Some(f)) = self.fibers.get_mut(fid as usize) else { return false };
        if f.state != FiberState::Waiting || f.wait_gen != wait_gen {
            return false;
        }
        let slot = f.result_slot;
        if slot < f.stack.len() {
            f.stack[slot] = v;
        }
        f.state = FiberState::Ready;
        f.io = None;
        let race = std::mem::take(&mut f.race);
        self.ready.push_back(fid);
        for t in race {
            self.cancel_other_task(t);
        }
        true
    }

    /// Wakes a waiting fiber with an error, `kind: message`.
    pub fn deliver_error(&mut self, fid: u32, wait_gen: u64, msg: &str) {
        if let Some(Some(f)) = self.fibers.get_mut(fid as usize)
            && f.state == FiberState::Waiting
            && f.wait_gen == wait_gen
        {
            f.pending_error = Some(msg.to_string());
            f.state = FiberState::Ready;
            self.ready.push_back(fid);
        }
    }

    /// Marks a task finished and wakes its waiters.
    pub fn finish_task(&mut self, fid: u32, result: VmValue) {
        let Some(f) = self.fibers[fid as usize].take() else { return };
        let task = f.task;
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.state = TaskState::Done;
            t.result = result;
        }
        self.free_fibers.push(fid);
        self.live_tasks = self.live_tasks.saturating_sub(1);
        for &(w, g) in &f.waiters {
            self.deliver(w, g, result);
        }
        self.recycle_fiber(f);
    }

    /// Cancels a running `task`. If `task` is the caller, returns a [`Ctl`]
    /// that the native function must propagate so the engine ends the task.
    pub fn cancel_task(&mut self, task: VmValue) -> Result<(), Ctl> {
        if let Obj::Task(t) = self.heap.get_mut(task)
            && t.state == TaskState::Running
            && t.fiber == self.current
        {
            t.state = TaskState::Cancelled;
            return Err(Ctl::from(CtlKind::Cancelled));
        }
        self.cancel_other_task(task);
        Ok(())
    }

    /// Cancels a running `task` other than `self.current`.
    fn cancel_other_task(&mut self, task: VmValue) {
        let (fid, state) = match self.heap.get(task) {
            Obj::Task(t) => (t.fiber, t.state),
            _ => return,
        };
        if state != TaskState::Running {
            return;
        }
        debug_assert_ne!(fid, self.current, "use cancel_task for the running task");
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.state = TaskState::Cancelled;
        }
        if let Some(mut f) = self.fibers.get_mut(fid as usize).and_then(|f| f.take()) {
            if let Some(io) = f.io.take() {
                self.io.cancel(io);
            }
            let queued = f.state == FiberState::Ready;
            self.end_cancelled(fid, f);
            if !queued && self.fibers[fid as usize].is_some() {
                self.ready.push_back(fid);
            }
        }
    }

    /// Ends the fiber of a cancelled task. Clears its active race and wakes
    /// its waiters with an error. Resumes the fiber at its next cleanup pad,
    /// or frees the fiber if all cleanups have run.
    pub(crate) fn end_cancelled(&mut self, fid: u32, mut f: Box<Fiber>) {
        f.race = Vec::new();
        for (w, g) in std::mem::take(&mut f.waiters) {
            if !self.race_outlasts_cancelled_contender(w, g) {
                self.deliver_error(w, g, "cancelled: task cancelled");
            }
        }
        let in_call = f.stopped_in_native_call;
        if self.unwind_to_pad(&mut f, in_call) {
            f.pending_error = None;
            f.state = FiberState::Ready;
            self.fibers[fid as usize] = Some(f);
        } else {
            self.free_fibers.push(fid);
            self.live_tasks = self.live_tasks.saturating_sub(1);
            self.recycle_fiber(f);
        }
    }

    /// Records that a contender of the race wait `(fid, wait_gen)` was
    /// cancelled. Returns whether another contender can still finish the race.
    fn race_outlasts_cancelled_contender(&mut self, fid: u32, wait_gen: u64) -> bool {
        let Some(Some(racer)) = self.fibers.get_mut(fid as usize) else { return false };
        if racer.state != FiberState::Waiting || racer.wait_gen != wait_gen || racer.race.is_empty() {
            return false;
        }
        racer.race_contenders = racer.race_contenders.saturating_sub(1);
        racer.race_contenders > 0
    }

    /// Drops the frames of `f` above the innermost one with deferred code
    /// pending and resumes that frame at its cleanup pad; false if no frame
    /// has any. Frames below the top stopped in a call; so did the top one
    /// if `in_call`.
    pub(crate) fn unwind_to_pad(&self, f: &mut Fiber, in_call: bool) -> bool {
        let n = f.frames.len();
        for i in (0..n).rev() {
            let fr = &mut f.frames[i];
            let pos = if i + 1 < n || in_call { (fr.ip as usize).saturating_sub(1) } else { fr.ip as usize };
            if let Some(pad) = self.prog.protos[fr.proto as usize].cleanup_at(pos) {
                fr.ip = pad as u32;
                f.frames.truncate(i + 1);
                return true;
            }
        }
        false
    }

    /// Cancels remaining tasks after the main task finishes. Runs their
    /// pending `defer` blocks until they finish or block.
    pub async fn end_tasks(&mut self) -> Result<(), RunEnding> {
        self.release_main();
        let tasks: Vec<VmValue> = self.fibers.iter().flatten().map(|f| f.task).collect();
        for task in tasks {
            self.cancel_other_task(task);
        }
        if self.ready.is_empty() {
            return Ok(());
        }
        let drive_result = self.drive(sched::UntilIdle { waits_for_io: false }).await;
        self.drain_output_after(drive_result).map(|_| ())
    }

    /// Converts `fail` into a [`RunEnding`], using `trace` to build stack
    /// traces for errors. Marks the VM broken if `fail` is an internal error.
    pub(crate) fn run_ending_of(&mut self, fail: Fail, trace: impl FnOnce(&Vm) -> Vec<TraceFrame>) -> RunEnding {
        match fail.into_kind() {
            FailKind::Exit(code) => RunEnding::Exit(code),
            FailKind::RuntimeError(message) => Trap::Error { message, trace: trace(self) }.into(),
            FailKind::Limit(kind) => Trap::Limit(kind).into(),
            FailKind::Interrupted => Trap::Interrupted.into(),
            FailKind::Internal(message) => {
                self.broken = true;
                Trap::Internal { message, trace: trace(self) }.into()
            }
        }
    }

    // ------------------------------------------------------------------
    // Garbage collection

    pub fn collect_garbage(&mut self, current: Option<&mut Fiber>) {
        self.collect_garbage_with(current, false);
    }

    /// Runs a major garbage collection to free all unreachable objects before a
    /// memory limit check. Also clears idle HTTP connections and cached
    /// regexes.
    pub fn collect_all_garbage(&mut self, current: Option<&mut Fiber>) {
        #[cfg(feature = "os")]
        self.host.http_pool.clear();
        self.host.regex_cache.iter_mut().flatten().for_each(|entry| entry.regex = None);
        self.collect_garbage_with(current, true);
    }

    /// GC roots: globals, constants, pinned values and the fibers' live stack
    /// windows, closures, tasks and race lists. Clears dead slots of stacks so
    /// that they never hold handles to objects freed later. Shrinks stacks left
    /// large by deep recursion and recounts `heap.fiber_bytes`.
    fn gc_roots(&mut self, current: Option<&mut Fiber>) -> Vec<VmValue> {
        let mut roots: Vec<VmValue> = Vec::with_capacity(1024);
        roots.extend(self.globals.iter().copied());
        roots.extend(self.consts.iter().copied());
        roots.extend(self.pinned.iter().copied());
        roots.extend(self.ascii_strs.iter().copied());
        roots.extend(self.host.regex_cache.iter().flatten().filter_map(|entry| entry.regex));
        let prog = &self.prog;
        let mut fiber_bytes = 0;
        let mut add_fiber = |f: &mut Fiber, roots: &mut Vec<VmValue>| {
            let top = f.live_top(prog);
            roots.extend(f.stack[..top].iter().copied());
            if f.stack.len() > 2 * top + 4096 {
                f.stack.truncate((top + top / 2).max(INITIAL_STACK_SLOTS));
                f.stack.shrink_to_fit();
            }
            f.stack[top..].fill(VmValue::Unit);
            roots.extend(f.frames.iter().map(|fr| fr.closure_value()));
            roots.push(f.task);
            roots.extend(f.race.iter().copied());
            fiber_bytes += f.footprint();
        };
        for f in self.fibers.iter_mut().flatten() {
            add_fiber(f, &mut roots);
        }
        if let Some(f) = current {
            add_fiber(f, &mut roots);
        }
        fiber_bytes += self.spare_fibers.iter().map(|f| f.footprint()).sum::<usize>();
        self.heap.fiber_bytes = fiber_bytes;
        roots
    }

    fn collect_garbage_with(&mut self, mut current: Option<&mut Fiber>, force_major: bool) {
        let start = crate::platform::Instant::now();
        let mut full = force_major;
        if force_major {
            self.free_spare_fibers();
        } else if self.heap.cycle_active() {
            if self.heap.minor_due() {
                let roots = self.gc_roots(current.as_deref_mut());
                self.heap.collect(roots, false);
                self.heap.stats.record(start.elapsed(), false);
            }
            let step = crate::platform::Instant::now();
            if self.heap.cycle_step() {
                let roots = self.gc_roots(current.as_deref_mut());
                self.heap.finish_marking(roots);
            }
            self.heap.stats.record_step(step.elapsed());
        } else {
            let roots = self.gc_roots(current.as_deref_mut());
            // Major collection if the old generation is large enough; minor otherwise.
            let major = self.heap.wants_major() || (self.heap.stress && self.heap.stats.collections.is_multiple_of(8));
            if !major {
                self.heap.collect(roots, false);
                self.heap.stats.record(start.elapsed(), false);
            } else if self.heap.major_is_incremental() {
                self.heap.start_cycle(roots);
                self.heap.stats.record_step(start.elapsed());
            } else {
                full = true;
            }
        }
        // Old garbage may keep the heap over the limit.
        if !full && self.heap.over_limit() {
            self.free_spare_fibers();
            full = self.heap.over_limit();
        }
        if full {
            let start = crate::platform::Instant::now();
            let roots = self.gc_roots(current);
            self.heap.collect(roots, true);
            if self.heap.over_limit() {
                self.free_spare_fibers();
            }
            self.heap.stats.record(start.elapsed(), true);
        }
    }

    pub fn trace_of(&self, fiber: &Fiber, ip: Option<u32>) -> Vec<TraceFrame> {
        let mut out = Vec::new();
        let n = fiber.frames.len();
        for (i, fr) in fiber.frames.iter().enumerate().rev() {
            let at = if i + 1 == n { ip.unwrap_or(fr.ip) } else { fr.ip };
            self.trace_frame(fr.proto, at, &mut out);
        }
        out
    }

    /// Returns the innermost script frame that called the active native function.
    pub(crate) fn native_caller(&self) -> Option<TraceFrame> {
        let mut out = Vec::new();
        self.trace_frame(self.caller_site.0, self.caller_site.1, &mut out);
        out.into_iter().next()
    }

    /// Appends the stack trace entries for `proto` at instruction `at` to `out`,
    /// from innermost inlined call to outermost.
    fn trace_frame(&self, proto: u32, at: u32, out: &mut Vec<TraceFrame>) {
        let proto = &self.prog.protos[proto as usize];
        if proto.name == "<entry>" || proto.name == FILL_PROTO {
            return;
        }
        let idx = (at as usize).saturating_sub(1).min(proto.lines.len().saturating_sub(1));
        let mut line = proto.lines.get(idx).copied().unwrap_or(0) as usize;
        let mut site = proto.sites.get(idx).copied().unwrap_or(0);
        while let Some(s) = (site as usize).checked_sub(1).and_then(|i| proto.inlined.get(i)) {
            out.push(TraceFrame {
                function: s.name.clone(),
                file: self.prog.files.get(s.file as usize).cloned().unwrap_or_default(),
                line,
            });
            line = s.line as usize;
            site = s.parent;
        }
        let name = if proto.name.starts_with('<') && proto.name.ends_with('>') && proto.name != "<lambda>" && proto.name != "<spawn>" {
            "<top level>".to_string()
        } else {
            proto.name.clone()
        };
        out.push(TraceFrame { function: name, file: self.prog.files.get(proto.file as usize).cloned().unwrap_or_default(), line });
    }
}
