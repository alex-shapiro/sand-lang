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

use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter};
use crate::{LimitKind, Limits};
use bytecode::*;
use heap::{Heap, Staging};
use meter::Meter;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use value::*;

pub type NResult = Result<Value, Ctl>;
pub type NativeFn = fn(&mut Vm, &[Value]) -> NResult;
pub type HostFn = Arc<dyn Fn(&mut Vm, &[Value]) -> NResult + Send + Sync>;
pub type Finisher = Box<dyn FnOnce(&mut Vm) -> NResult + Send>;
pub type BoxFuture = Pin<Box<dyn Future<Output = Finisher> + Send>>;

#[derive(Clone)]
pub enum NativeImpl {
    Builtin(NativeFn),
    /// A native implementing a trait method: called without the impl's
    /// dictionary, which the method receives first (`self` comes next).
    Method(NativeFn),
    Host(HostFn),
}

#[derive(Clone)]
pub struct NativeEntry {
    pub name: std::borrow::Cow<'static, str>,
    pub imp: NativeImpl,
}

/// A non-local outcome of a native call or a VM helper: an error, a limit, a
/// request to the scheduler, ...
///
/// One pointer, so that `Result<Value, Ctl>` is 16 bytes like a `Value`
/// (see `interp`). [`Vm::block`] and
/// [`Vm::yield_now`] reuse a spare box instead of allocating.
pub struct Ctl(Box<CtlKind>);

const _: () = assert!(std::mem::size_of::<Ctl>() == std::mem::size_of::<usize>());
const _: () = assert!(std::mem::size_of::<Result<Value, Ctl>>() == 16);
const _: () = assert!(std::mem::size_of::<Result<Option<Value>, Ctl>>() == 16);

/// Category of asynchronous wait for a suspended task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    Io,
    User,
}

/// What a [`Ctl`] is.
pub enum CtlKind {
    /// The script failed with an error, `kind: message`.
    Error(String),
    /// An invariant of the engine broke: a bug, which ends a session.
    Internal(String),
    Limit(LimitKind),
    Exit(i32),
    /// Ask the user; the call is retried afterwards.
    NeedPermission(Capability),
    /// Suspend the task until the future completes.
    Async(BoxFuture, WaitKind),
    /// The task waits for the scheduler event that [`Vm::block`]
    /// registered; the call returns what wakes it.
    Blocked,
    /// Let other tasks run; the call returns `()`.
    Yield,
    /// The host interrupted the engine (see `Meter::check`).
    Interrupted,
    /// Collect garbage and call the native again (once): what it needs (an
    /// open-resource slot, I/O buffer space) may be held by garbage. See
    /// [`Vm::resource_slot`].
    Collect,
}

impl Ctl {
    /// The script fails with an error of kind `kind`.
    #[cold]
    pub fn fail(kind: &str, msg: impl std::fmt::Display) -> Ctl {
        Ctl(Box::new(CtlKind::Error(format!("{kind}: {msg}"))))
    }

    /// An internal error: an invariant of the engine broke.
    #[cold]
    pub fn internal(msg: impl Into<String>) -> Ctl {
        Ctl(Box::new(CtlKind::Internal(msg.into())))
    }

    /// A limit was exceeded.
    #[cold]
    pub fn limit(k: LimitKind) -> Ctl {
        Ctl(Box::new(CtlKind::Limit(k)))
    }

    /// The host interrupted the engine.
    #[cold]
    pub fn interrupted() -> Ctl {
        Ctl(Box::new(CtlKind::Interrupted))
    }

    /// `exit(code)`.
    pub fn exit(code: i32) -> Ctl {
        Ctl(Box::new(CtlKind::Exit(code)))
    }

    /// Suspend the task until `fut` completes.
    pub fn suspend(fut: BoxFuture) -> Ctl {
        Ctl(Box::new(CtlKind::Async(fut, WaitKind::Io)))
    }

    /// Suspends the task until `fut` completes in response to user input.
    /// The engine pauses the wall-time limit while all runnable tasks are idle
    /// and a task is waiting for the user.
    pub fn suspend_for_user(fut: BoxFuture) -> Ctl {
        Ctl(Box::new(CtlKind::Async(fut, WaitKind::User)))
    }

    /// Ask the user for `cap`.
    pub fn need_permission(cap: Capability) -> Ctl {
        Ctl(Box::new(CtlKind::NeedPermission(cap)))
    }

    pub fn kind(&self) -> &CtlKind {
        &self.0
    }

    pub fn into_kind(self) -> CtlKind {
        *self.0
    }

    /// The limit, if this is one.
    pub fn as_limit(&self) -> Option<LimitKind> {
        match *self.0 {
            CtlKind::Limit(k) => Some(k),
            _ => None,
        }
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

impl std::fmt::Debug for CtlKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlKind::Error(m) => write!(f, "Error({m})"),
            CtlKind::Internal(m) => write!(f, "Internal({m})"),
            CtlKind::Limit(k) => write!(f, "Limit({k:?})"),
            CtlKind::Exit(c) => write!(f, "Exit({c})"),
            CtlKind::NeedPermission(c) => write!(f, "NeedPermission({c})"),
            CtlKind::Async(_, wait) => write!(f, "Async({wait:?})"),
            CtlKind::Blocked => write!(f, "Blocked"),
            CtlKind::Yield => write!(f, "Yield"),
            CtlKind::Interrupted => write!(f, "Interrupted"),
            CtlKind::Collect => write!(f, "Collect"),
        }
    }
}

impl From<&str> for Ctl {
    /// An error whose message `msg` is `kind: message`.
    fn from(msg: &str) -> Ctl {
        Ctl(Box::new(CtlKind::Error(msg.to_string())))
    }
}

/// Fails with an error of kind `kind`.
pub fn fail<T>(kind: &str, msg: impl std::fmt::Display) -> Result<T, Ctl> {
    Err(Ctl::fail(kind, msg))
}

/// Fails with an internal error.
pub fn internal<T>(msg: impl Into<String>) -> Result<T, Ctl> {
    Err(Ctl::internal(msg))
}

/// What [`Vm::block`] waits for.
pub enum BlockOn {
    Join(Value),
    Race(Vec<Value>),
    Recv(Value),
    Send(Value, Value),
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
    pub fn closure_value(&self) -> Value {
        if self.closure == NO_CLOSURE { Value::Unit } else { Value::Obj(self.closure) }
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
pub(crate) fn wait_is_live(fibers: &[Option<Box<Fiber>>], (fid, wgen): (u32, u64)) -> bool {
    matches!(fibers.get(fid as usize), Some(Some(f)) if f.state == FiberState::Waiting && f.wgen == wgen)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FiberState {
    Ready,
    Waiting,
    Done,
}

pub struct Fiber {
    pub stack: Vec<Value>,
    pub frames: Vec<Frame>,
    pub task: Value,
    pub state: FiberState,
    /// Wait generation; waiters record it to detect stale wake-ups.
    pub wgen: u64,
    /// Absolute stack slot receiving the result of a blocking call.
    pub result_slot: usize,
    /// Tasks to cancel when a `race` completes.
    pub race: Vec<Value>,
    /// Fibers waiting for this one: (fiber, generation).
    pub waiters: Vec<(u32, u64)>,
    /// Stale `waiters` are pruned when it reaches this length.
    pub waiters_prune_at: usize,
    /// Pending IO operation id, if waiting on IO.
    pub io: Option<u64>,
    /// An error (`kind: message`) to raise when the fiber resumes.
    pub pending_error: Option<String>,
}

/// One frame of a runtime error's stack trace.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub function: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug)]
pub enum Trap {
    /// The script failed with an error, `kind: message`.
    Error {
        message: String,
        trace: Vec<TraceEntry>,
    },
    /// An invariant of the engine broke.
    Internal {
        message: String,
        trace: Vec<TraceEntry>,
    },
    Limit(LimitKind),
    Interrupted,
    Exit(i32),
}

impl Trap {
    /// The trap for an error from the meter: a limit or an interrupt.
    pub(crate) fn from_ctl(c: Ctl) -> Trap {
        match c.into_kind() {
            CtlKind::Limit(k) => Trap::Limit(k),
            CtlKind::Interrupted => Trap::Interrupted,
            other => Trap::Internal { message: format!("unexpected {other:?}"), trace: vec![] },
        }
    }
}

/// Standard input source for an engine.
pub enum StdinSource {
    None,
    Bytes(std::io::Cursor<Vec<u8>>),
    /// The host process's standard input.
    #[cfg(feature = "os")]
    Inherit(tokio::io::BufReader<tokio::io::Stdin>),
    /// Input the host sends ([`crate::StdinFeed`]).
    Fed(crate::feed::FedReader),
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
    if let Some(&d) = always.lock().unwrap_or_else(|e| e.into_inner()).get(cap) {
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
    pub policy: Policy,
    pub prompter: Option<Arc<dyn Prompter>>,
    pub grants_once: Vec<Capability>,
    pub denies_once: Vec<Capability>,
    /// "Always" answers to prompts, kept for the engine's lifetime.
    pub always: Always,
    pub stdout: SharedSink,
    pub stderr: SharedSink,
    pub stdin: Arc<tokio::sync::Mutex<StdinSource>>,
    /// The feed of a [`StdinSource::Fed`], told when the VM waits.
    pub(crate) stdin_feed: Option<Arc<crate::feed::Shared>>,
    pub output_bytes: u64,
    pub limits: Limits,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub env: Option<Vec<(String, String)>>,
    pub rng: Rng,
    pub log_level: u8,
    pub interrupt: Arc<AtomicBool>,
    /// Cached decisions for stdout (index 0) and stderr (1).
    pub stream_decisions: [Option<bool>; 2],
    #[cfg(feature = "os")]
    pub(crate) tls: crate::stdlib::TlsSettings,
    /// The run's idle `http` connections.
    #[cfg(feature = "os")]
    pub(crate) http_pool: Arc<crate::stdlib::HttpPool>,
    pub(crate) regex_cache: Vec<(u64, u32)>,
    /// The file system of the `fs` module.
    pub(crate) fs: crate::files::Fs,
    /// Rewrites or refuses outgoing HTTP requests (see [`crate::Config::request_filter`]).
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
    pub globals: Vec<Value>,
    pub consts: Vec<Value>,
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
    /// Set when the running instruction hit the memory limit and should be
    /// retried after a garbage collection (see `interp::limit_trap`).
    pub(crate) gc_retry: bool,
    /// Set while a native that asked for a collection (`CtlKind::Collect`)
    /// is called again after it.
    pub(crate) collected: bool,
    pub cancel_current: bool,
    /// Set while the tasks left at the end of a run unwind (`end_tasks`):
    /// `drive` stops instead of waiting for I/O.
    pub(crate) ending: bool,
    /// Extra GC roots (e.g. values held by the test runner).
    pub pinned: Vec<Value>,
    pub(crate) pending_race: Option<Vec<Value>>,
    /// The box of the last `Blocked` or `Yield` consumed (see `Vm::signal`).
    spare_ctl: Option<Box<CtlKind>>,
    /// The wait generation of the wait [`Vm::block`] registered.
    wait_gen: u64,
    /// The map handle and slot of the last lookup of a key other than an
    /// int, which a store to the same key reuses (see `Vm::map_set`).
    last_slot: std::cell::Cell<(u32, u32)>,
    /// Strings of one ASCII character, by byte (`Unit` until first
    /// made); shared, since strings are immutable. GC roots.
    ascii_strs: Vec<Value>,

    /// Set by an internal error (a bug), including a Rust panic caught in the
    /// interpreter, a native or an I/O finisher: the VM is memory safe but may
    /// be inconsistent, so a REPL session discards it.
    pub(crate) broken: bool,
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
    pub fn grow_stack(&mut self, need: usize, heap: &mut Heap) -> Result<(), Ctl> {
        if need > MAX_STACK_SLOTS {
            return Err(Ctl::fail("limit", "stack overflow"));
        }
        let n = need.max(self.stack.len() * 2).min(MAX_STACK_SLOTS);
        let before = self.stack.capacity();
        let extra = n.saturating_sub(before) * std::mem::size_of::<Value>();
        if heap.would_exceed(extra) {
            return Err(Ctl::limit(LimitKind::Memory));
        }
        self.stack.reserve_exact(n - self.stack.len());
        self.stack.resize(n, Value::Unit);
        heap.fiber_bytes += self.stack.capacity().saturating_sub(before) * std::mem::size_of::<Value>();
        Ok(())
    }

    /// Makes room for more frames (called when `frames` is full), counting
    /// the memory like [`Fiber::grow_stack`].
    #[cold]
    #[inline(never)]
    pub fn grow_frames(&mut self, heap: &mut Heap) -> Result<(), Ctl> {
        let before = self.frames.capacity();
        let more = before.max(8);
        if heap.would_exceed(more * std::mem::size_of::<Frame>()) {
            return Err(Ctl::limit(LimitKind::Memory));
        }
        self.frames.reserve_exact(more);
        heap.fiber_bytes += self.frames.capacity().saturating_sub(before) * std::mem::size_of::<Frame>();
        Ok(())
    }

    /// Memory used by the fiber, as counted in `Heap::fiber_bytes`.
    pub fn footprint(&self) -> usize {
        std::mem::size_of::<Fiber>()
            + self.stack.capacity() * std::mem::size_of::<Value>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
            + self.race.capacity() * std::mem::size_of::<Value>()
            + self.waiters.capacity() * std::mem::size_of::<(u32, u64)>()
    }
}

impl Vm {
    pub fn new(prog: Arc<Program>, natives: Arc<Vec<NativeEntry>>, host: Host) -> Vm {
        let heap = Heap::new(host.limits.max_memory, host.limits.max_open_resources);
        let consts = Vec::with_capacity(prog.consts.len());
        let globals = vec![Value::Uninit; prog.nglobals];
        let meter = Meter::new(host.limits.max_instructions, host.interrupt.clone(), host.clock.clone());
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
            io: sched::IoTable::default(),
            gen_counter: 1,
            meter,
            deadline: None,
            gc_retry: false,
            collected: false,
            cancel_current: false,
            ending: false,
            pinned: Vec::new(),
            pending_race: None,
            spare_ctl: None,
            wait_gen: 0,
            last_slot: std::cell::Cell::new((u32::MAX, 0)),
            ascii_strs: Vec::new(),
            broken: false,
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
                Const::Int(i) => Value::Int(*i),
                Const::Float(f) => Value::Float(*f),
                Const::Dur(d) => Value::Dur(*d),
                Const::Str(_) => unreachable!(),
                Const::Bytes(b) => heap.new_bytes(b.clone()),
                Const::Fn(p) => heap.alloc(Obj::Closure(*p, Box::new([]))),
                Const::Native(n) => heap.alloc(Obj::NativeFn(*n)),
                Const::TypeDesc(t) => heap.alloc(Obj::TypeDesc(t.clone())),
                Const::Tuple(items) => heap.new_tuple(&items.iter().map(|&i| self.consts[i as usize]).collect::<Vec<_>>()),
            };
            self.consts.push(v);
        }
        if self.globals.len() < self.prog.nglobals {
            self.globals.resize(self.prog.nglobals, Value::Uninit);
        }
    }

    /// Which globals are initialized, by id.
    pub(crate) fn initialized_globals(&self) -> Vec<bool> {
        self.globals.iter().map(|v| !matches!(v, Value::Uninit)).collect()
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
        self.cancel_current = false;
        self.pending_race = None;
        self.host.grants_once.clear();
        self.host.denies_once.clear();
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

    /// The dataless `kind` (`Blocked` or `Yield`) as a [`Ctl`], in the box
    /// of the last one consumed.
    #[inline]
    fn signal(&mut self, kind: CtlKind) -> Ctl {
        match self.spare_ctl.take() {
            Some(mut b) => {
                // A dataless kind: nothing to drop.
                std::mem::forget(std::mem::replace(&mut *b, kind));
                Ctl(b)
            }
            None => Ctl::from(kind),
        }
    }

    /// Keeps the box of a consumed `Blocked` or `Yield` for [`Vm::signal`].
    #[inline]
    pub(crate) fn recycle(&mut self, c: Ctl) {
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

    pub fn str(&self, v: Value) -> &str {
        self.heap.str(v)
    }

    pub fn new_str(&mut self, s: impl Into<compact_str::CompactString>) -> Value {
        self.heap.new_str(s)
    }

    /// [`Heap::staged`], also passing the shared strings of one ASCII
    /// character (indexed by the character; see `short_str`), for natives
    /// that split text into many short pieces.
    pub fn staged_strs<R>(&mut self, f: impl FnOnce(&Heap, &mut Staging<'_>, &[Value]) -> R) -> R {
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
    fn ascii_str(&mut self, b: u8) -> Value {
        match self.ascii_strs.get(b as usize) {
            Some(&v) => v,
            None => self.short_str(std::str::from_utf8(&[b]).unwrap_or("?")),
        }
    }

    /// A string equal to `s`, which may be shared: strings of one ASCII
    /// character (as `chars`, `char_at` and splitting produce in bulk) are
    /// made once per VM.
    pub fn short_str(&mut self, s: &str) -> Value {
        match s.as_bytes() {
            &[b] if b.is_ascii() => {
                self.ascii_table();
                self.ascii_strs[b as usize]
            }
            _ => self.heap.new_str(s),
        }
    }

    #[inline]
    pub fn some(&mut self, v: Value) -> Value {
        let id = self.prog.ids.some;
        match Value::variant1(id, v) {
            Some(inline) => inline,
            None => self.heap.new_variant(id, &[v]),
        }
    }

    pub fn none(&self) -> Value {
        Value::Tag(self.prog.ids.none)
    }

    pub fn ok(&mut self, v: Value) -> Value {
        let id = self.prog.ids.ok;
        self.heap.new_variant(id, &[v])
    }

    pub fn err(&mut self, v: Value) -> Value {
        let id = self.prog.ids.err;
        self.heap.new_variant(id, &[v])
    }

    pub fn new_value<T: value::ValueType>(&mut self, x: T) -> Value {
        self.heap.alloc(value::value_obj(x))
    }

    #[inline]
    pub fn option(&mut self, v: Option<Value>) -> Value {
        match v {
            Some(v) => self.some(v),
            None => self.none(),
        }
    }

    /// Builds an `Error { kind, message }` value.
    pub fn error_value(&mut self, kind: &str, message: &str) -> Value {
        let k = self.heap.new_str(kind);
        let m = self.heap.new_str(message);
        let def = self.prog.ids.error_def;
        self.heap.alloc(Obj::Struct(def, Box::new([k, m])))
    }

    /// `Err(Error { kind, message })`.
    pub fn err_value(&mut self, kind: &str, message: &str) -> Value {
        let e = self.error_value(kind, message);
        self.err(e)
    }

    /// Unpacks an Option: `Some(x)` → `Some(x)`.
    pub fn as_option(&self, v: Value) -> Option<Value> {
        if self.heap.variant_id(v) == self.prog.ids.some { self.heap.field(v, 0) } else { None }
    }

    /// Unpacks a Result: `Ok(x)` → `Ok(x)`, `Err(e)` → `Err(e)`.
    pub fn as_result(&self, v: Value) -> Result<Value, Value> {
        let payload = self.heap.field(v, 0).unwrap_or(Value::Unit);
        if self.heap.variant_id(v) == self.prog.ids.ok { Ok(payload) } else { Err(payload) }
    }

    /// Payload field `i` of an enum variant value, whatever its
    /// representation (`Unit` if there is no such field).
    pub fn variant_payload(&self, v: Value, i: usize) -> Value {
        self.heap.field(v, i).unwrap_or(Value::Unit)
    }

    /// The display form of `v`, charging the meter for the work.
    pub fn display(&self, v: Value) -> Result<String, Ctl> {
        display::to_display_metered(&self.prog, &self.heap, &self.meter, v)
    }

    /// The debug form of `v`, charging the meter for the work.
    pub fn debug(&self, v: Value) -> Result<String, Ctl> {
        display::to_debug_metered(&self.prog, &self.heap, &self.meter, v)
    }

    /// Fails with a memory limit error if `extra` bytes cannot be allocated.
    pub fn reserve(&self, extra: usize) -> Result<(), Ctl> {
        if self.heap.would_exceed(extra) { Err(Ctl::limit(LimitKind::Memory)) } else { Ok(()) }
    }

    /// Starts accounting for the buffers of an I/O operation that are not
    /// yet heap values (see [`iomem`]); the operation grows the reservation
    /// as its buffers grow and drops it before turning them into values.
    pub fn io_reservation(&self) -> iomem::Reservation {
        self.heap.io_memory.publish_heap(self.heap.used());
        iomem::Reservation::new(&self.heap.io_memory)
    }

    /// Like [`Vm::io_reservation`], reserving `bytes` at once. `Err` asks for
    /// a collection and a retry (propagate it with `?`); `Ok(Err(e))`: they
    /// still do not fit, and `e` is the `"limit"` error value to return.
    pub fn reserve_io(&mut self, bytes: usize) -> Result<Result<iomem::Reservation, Value>, Ctl> {
        let mut r = self.io_reservation();
        match r.ensure(bytes) {
            Ok(()) => Ok(Ok(r)),
            Err(_) if !self.collected => Err(CtlKind::Collect.into()),
            Err(e) => Ok(Err(self.err_value("limit", &format!("cannot buffer {bytes} bytes: {e}")))),
        }
    }

    /// A slot for a resource about to be opened, counted against
    /// `max_open_resources` until dropped. At the limit, `Err` asks for a
    /// collection (which closes dropped resources) and a retry: propagate it
    /// with `?` before any side effect. `Ok(Err(e))`: still at the limit, and
    /// `e` is the `"limit"` error value to return.
    pub fn resource_slot(&mut self) -> Result<Result<iomem::ResourceSlot, Value>, Ctl> {
        match iomem::ResourceSlot::try_new(&self.heap.io_memory) {
            Some(slot) => Ok(Ok(slot)),
            None if !self.collected => Err(CtlKind::Collect.into()),
            None => {
                let max = self.heap.io_memory.max_resources();
                Ok(Err(self.err_value("limit", &format!("too many open resources (limit {max})"))))
            }
        }
    }

    /// Like [`Vm::resource_slot`], also reserving `buffers` bytes for the
    /// fixed buffers of the resource (`what`, for the message: "a connection").
    pub fn resource_slot_with(&mut self, buffers: usize, what: &str) -> Result<Result<iomem::ResourceSlot, Value>, Ctl> {
        let mut slot = match self.resource_slot()? {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        self.heap.io_memory.publish_heap(self.heap.used());
        match slot.reserve_buffers(buffers) {
            Ok(()) => Ok(Ok(slot)),
            Err(_) if !self.collected => Err(CtlKind::Collect.into()),
            Err(e) => Ok(Err(self.err_value("limit", &format!("cannot open {what}: its buffers ({buffers} bytes) do not fit: {e}")))),
        }
    }

    // ------------------------------------------------------------------
    // Metering (see `meter` for the cost model)

    /// Charges `units` of fuel for native work: one unit per element
    /// visited, compared or produced. Fails if a limit is exceeded or the
    /// engine was interrupted; propagate the error with `?`.
    #[inline]
    pub fn charge(&self, units: u64) -> Result<(), Ctl> {
        self.meter.charge(units)
    }

    /// Charges for `n` bytes copied, compared or scanned.
    #[inline]
    pub fn charge_bytes(&self, n: usize) -> Result<(), Ctl> {
        self.meter.charge_bytes(n)
    }

    /// Charges for `n` bytes processed one at a time (hashing, encoding).
    #[inline]
    pub fn charge_slow_bytes(&self, n: usize) -> Result<(), Ctl> {
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
        !self.host.grants_once.is_empty()
            || !self.host.denies_once.is_empty()
            || !self.host.always.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
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
    pub(crate) fn offload<T: Send + 'static>(&self, job: impl FnOnce(&mut meter::Allowance) -> T + Send + 'static) -> Result<T, Ctl> {
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
                Ok(Err(panic)) => Err(Ctl::internal(panic)),
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
    fn stopped(&self) -> Ctl {
        self.meter.check().err().unwrap_or_else(Ctl::interrupted)
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
                self.host.always.lock().unwrap_or_else(|e| e.into_inner()).insert(cap, allow);
            }
        }
    }

    /// Starts writing the buffered standard output, without waiting for it.
    pub fn flush_stdout(&self) {
        let _ = self.host.stdout.flush();
    }

    /// Flushes standard output and error, and waits until they are written.
    /// Fails if the meter reports an interrupt or a limit first.
    pub fn drain_output(&self) -> Result<(), Ctl> {
        self.flush_stdout();
        for sink in [&self.host.stdout, &self.host.stderr] {
            self.wait_backlog(sink, 0)?;
        }
        Ok(())
    }

    /// Waits until `sink`'s backlog is at most `max` bytes. Fails if the
    /// meter reports an interrupt or a limit first.
    fn wait_backlog(&self, sink: &SharedSink, max: usize) -> Result<(), Ctl> {
        while !sink.wait_backlog(max, std::time::Duration::from_millis(5)) {
            self.meter.check()?;
        }
        Ok(())
    }

    /// Flushes standard output before reading standard input, if the sink
    /// wants to (see [`OutputSink::flush_before_input`]), and waits until it
    /// is written.
    pub fn flush_before_input(&self) -> Result<(), Ctl> {
        let _ = self.host.stdout.flush_before_input();
        self.wait_backlog(&self.host.stdout, 0)
    }

    /// Writes to stdout (1) or stderr (2), enforcing policy and output
    /// limits; a denial fails (as `print` does).
    pub fn write_stream(&mut self, which: u8, data: &[u8]) -> Result<(), Ctl> {
        self.admit_output(which, data.len())?;
        let _ = self.stream_sink(which).write(data);
        self.limit_backlog(which)
    }

    /// Writes string `s` (a heap value) to stdout (1) or stderr (2), like
    /// `write_stream`, without copying it out of the heap first.
    pub fn write_stream_str(&mut self, which: u8, s: Value) -> Result<(), Ctl> {
        self.admit_output(which, self.heap.str(s).len())?;
        let _ = self.stream_sink(which).write(self.heap.str(s).as_bytes());
        self.limit_backlog(which)
    }

    /// If the backlog of stream `which` exceeds [`MAX_OUTPUT_BACKLOG`], waits
    /// until half of it has been written.
    fn limit_backlog(&self, which: u8) -> Result<(), Ctl> {
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
    pub fn write_stream_value(&mut self, which: u8, v: Value) -> NResult {
        let len = match self.heap.get(v) {
            Obj::Str(s) => s.len(),
            _ => self.heap.bytes_of(v).len(),
        };
        if !self.output_allowed(which)? {
            let name = if which == 1 { "stdout" } else { "stderr" };
            return Ok(self.err_value("permission_denied", &format!("permission denied: {name}")));
        }
        self.count_output(which, len)?;
        let data = match self.heap.get(v) {
            Obj::Str(s) => s.as_bytes(),
            _ => self.heap.bytes_of(v),
        };
        match self.stream_sink(which).write(data) {
            Ok(()) => {
                self.limit_backlog(which)?;
                Ok(self.ok(Value::Unit))
            }
            Err(e) => Ok(self.err_value(crate::stdlib::io_kind(&e), &e.to_string())),
        }
    }

    /// Checks the policy and the output limit for writing `len` bytes to
    /// stream `which`, and counts them; a denial fails.
    fn admit_output(&mut self, which: u8, len: usize) -> Result<(), Ctl> {
        if !self.output_allowed(which)? {
            return fail("permission_denied", if which == 1 { "permission denied: stdout" } else { "permission denied: stderr" });
        }
        self.count_output(which, len)
    }

    /// Whether the policy allows writing to stream `which`.
    fn output_allowed(&mut self, which: u8) -> Result<bool, Ctl> {
        let slot = usize::from(which != 1);
        let allowed = match self.host.stream_decisions[slot] {
            Some(d) => d,
            None => {
                let cap = if which == 1 { Capability::Stdout } else { Capability::Stderr };
                let d = self.permit(&cap)?;
                // Cache static decisions for the hot path.
                if !self.host.always.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&cap)
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
    fn count_output(&mut self, which: u8, len: usize) -> Result<(), Ctl> {
        self.host.output_bytes += len as u64;
        if let Some(max) = self.host.limits.max_output_bytes
            && self.host.output_bytes > max
        {
            return Err(Ctl::limit(LimitKind::Output));
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
    pub fn spawn_fiber(&mut self, closure: Value, task: Value) -> Result<u32, Ctl> {
        let (proto, ncaps) = match self.heap.get(closure) {
            Obj::Closure(p, caps) => (*p, caps.len()),
            _ => return internal("spawn: not a closure"),
        };
        let nregs = self.prog.protos[proto as usize].nregs as usize;
        let Value::Obj(closure) = closure else { return internal("spawn: not a closure") };
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
                if self.heap.would_exceed(stack * std::mem::size_of::<Value>()) {
                    return Err(Ctl::limit(crate::LimitKind::Memory));
                }
                let mut frames = Vec::with_capacity(8);
                frames.push(frame);
                let fiber = Box::new(Fiber {
                    stack: vec![Value::Unit; stack],
                    frames,
                    task,
                    state: FiberState::Ready,
                    wgen: 0,
                    result_slot: 0,
                    race: vec![],
                    waiters: vec![],
                    waiters_prune_at: MIN_PRUNE,
                    io: None,
                    pending_error: None,
                });
                self.heap.fiber_bytes += fiber.footprint();
                fiber
            }
        };
        // A task's function takes no parameters: its captures come first.
        let mut fiber = fiber;
        if let Obj::Closure(_, caps) = self.heap.get(Value::Obj(closure)) {
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
        if self.spare_fibers.len() >= MAX_SPARE_FIBERS || !small || !f.race.is_empty() || f.waiters.capacity() > MIN_PRUNE {
            self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(f.footprint());
            return;
        }
        let slots = f.stack.capacity();
        f.stack.clear();
        f.stack.resize(slots, Value::Unit);
        f.frames.clear();
        f.task = Value::Unit;
        f.state = FiberState::Ready;
        f.wgen = 0;
        f.result_slot = 0;
        f.waiters.clear();
        f.waiters_prune_at = MIN_PRUNE;
        f.io = None;
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
    pub fn spawn_task(&mut self, closure: Value) -> NResult {
        if self.live_tasks >= self.host.limits.max_tasks {
            return fail("limit", "task limit exceeded");
        }
        if let Obj::Closure(p, _) = self.heap.get(closure) {
            let p = &self.prog.protos[*p as usize];
            self.meter.charge(meter::code_fuel(p.code.len() + p.nregs as usize) as u64)?;
        }
        let task = self.heap.alloc(Obj::Task(Box::new(TaskObj { fiber: u32::MAX, state: TaskState::Running, result: Value::Unit })));
        let fid = self.spawn_fiber(closure, task)?;
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.fiber = fid;
        }
        self.live_tasks += 1;
        Ok(task)
    }

    /// Delivers a value to a waiting fiber (if the wait is still current).
    pub fn deliver(&mut self, fid: u32, wgen: u64, v: Value) -> bool {
        let Some(Some(f)) = self.fibers.get_mut(fid as usize) else { return false };
        if f.state != FiberState::Waiting || f.wgen != wgen {
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
            self.cancel_task(t);
        }
        true
    }

    /// Wakes a waiting fiber with an error, `kind: message`.
    pub fn deliver_error(&mut self, fid: u32, wgen: u64, msg: &str) {
        if let Some(Some(f)) = self.fibers.get_mut(fid as usize)
            && f.state == FiberState::Waiting
            && f.wgen == wgen
        {
            f.pending_error = Some(msg.to_string());
            f.state = FiberState::Ready;
            self.ready.push_back(fid);
        }
    }

    /// Marks a task finished and wakes its waiters.
    pub fn finish_task(&mut self, fid: u32, result: Value) {
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

    /// Cancels a task (no-op if already finished).
    pub fn cancel_task(&mut self, task: Value) {
        let (fid, state) = match self.heap.get(task) {
            Obj::Task(t) => (t.fiber, t.state),
            _ => return,
        };
        if state != TaskState::Running {
            return;
        }
        if let Obj::Task(t) = self.heap.get_mut(task) {
            t.state = TaskState::Cancelled;
        }
        if fid == self.current && self.fibers.get(fid as usize).map(|f| f.is_none()).unwrap_or(false) {
            // Cancelling the running task: stop it after the current call.
            self.cancel_current = true;
            return;
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

    /// Ends the fiber of a cancelled task: wakes its waiters with an error,
    /// then resumes it at its pending deferred code (`unwind_to_pad`), or
    /// frees it if there is none.
    pub(crate) fn end_cancelled(&mut self, fid: u32, mut f: Box<Fiber>) {
        for (w, g) in std::mem::take(&mut f.waiters) {
            self.deliver_error(w, g, "cancelled: task cancelled");
        }
        // A fiber waiting in a call, or woken to fail in it, stopped before the call completed.
        let in_call = f.state == FiberState::Waiting || f.pending_error.is_some();
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

    /// Ends a run whose main task finished: cancels the other tasks and runs
    /// their pending deferred code until it finishes or would wait. Returns
    /// the exit code if deferred code calls `exit`.
    pub async fn end_tasks(&mut self) -> Result<Option<i32>, Trap> {
        self.release_main();
        let tasks: Vec<Value> = self.fibers.iter().flatten().map(|f| f.task).collect();
        for t in tasks {
            self.cancel_task(t);
        }
        if self.ready.is_empty() {
            return Ok(None);
        }
        self.ending = true;
        let r = self.drive(None).await;
        self.ending = false;
        let r = r.and_then(|stop| self.drain_output().map(|()| stop).map_err(Trap::from_ctl));
        match r {
            Ok(sched::Stop::Exit(c)) => Ok(Some(c)),
            Ok(_) => Ok(None),
            Err(t) => Err(t),
        }
    }

    // ------------------------------------------------------------------
    // Garbage collection

    pub fn collect_garbage(&mut self, current: Option<&mut Fiber>) {
        self.collect_garbage_with(current, false);
    }

    /// A major collection, freeing all unreachable objects; used before
    /// reporting a memory limit, since a minor collection leaves old garbage.
    /// Also closes idle `http` connections, which hold resource slots and
    /// buffers.
    pub fn collect_all_garbage(&mut self, current: Option<&mut Fiber>) {
        #[cfg(feature = "os")]
        self.host.http_pool.clear();
        self.collect_garbage_with(current, true);
    }

    /// GC roots: globals, constants, pinned values and the fibers' live stack
    /// windows, closures, tasks and race lists. Clears dead slots of stacks so
    /// that they never hold handles to objects freed later. Shrinks stacks left
    /// large by deep recursion and recounts `heap.fiber_bytes`.
    fn gc_roots(&mut self, current: Option<&mut Fiber>) -> Vec<Value> {
        let mut roots: Vec<Value> = Vec::with_capacity(1024);
        roots.extend(self.globals.iter().copied());
        roots.extend(self.consts.iter().copied());
        roots.extend(self.pinned.iter().copied());
        roots.extend(self.ascii_strs.iter().copied());
        let prog = &self.prog;
        let mut fiber_bytes = 0;
        let mut add_fiber = |f: &mut Fiber, roots: &mut Vec<Value>| {
            let top = f.live_top(prog);
            roots.extend(f.stack[..top].iter().copied());
            if f.stack.len() > 2 * top + 4096 {
                f.stack.truncate((top + top / 2).max(INITIAL_STACK_SLOTS));
                f.stack.shrink_to_fit();
            }
            f.stack[top..].fill(Value::Unit);
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

    pub fn trace_of(&self, fiber: &Fiber, ip: Option<u32>) -> Vec<TraceEntry> {
        let mut out = Vec::new();
        let n = fiber.frames.len();
        for (i, fr) in fiber.frames.iter().enumerate().rev() {
            let proto = &self.prog.protos[fr.proto as usize];
            let at = if i + 1 == n { ip.unwrap_or(fr.ip) } else { fr.ip };
            let idx = (at as usize).saturating_sub(1).min(proto.lines.len().saturating_sub(1));
            let mut line = proto.lines.get(idx).copied().unwrap_or(0) as usize;
            if proto.name == "<entry>" {
                continue;
            }
            let mut site = proto.sites.get(idx).copied().unwrap_or(0);
            while let Some(s) = (site as usize).checked_sub(1).and_then(|i| proto.inlined.get(i)) {
                out.push(TraceEntry {
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
            out.push(TraceEntry { function: name, file: self.prog.files.get(proto.file as usize).cloned().unwrap_or_default(), line });
        }
        out
    }
}
