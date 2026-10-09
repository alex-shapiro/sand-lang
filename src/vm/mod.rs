//! The virtual machine: heap, fibers (tasks), scheduler and interpreter.

pub mod bytecode;
pub mod compare;
pub mod display;
pub mod heap;
mod interp;
pub mod iomem;
pub mod meter;
pub mod sched;
pub mod value;

use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter};
use crate::{LimitKind, Limits};
use bytecode::*;
use heap::Heap;
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
    Host(HostFn),
}

#[derive(Clone)]
pub struct NativeEntry {
    pub name: String,
    pub imp: NativeImpl,
}

/// Non-local outcomes of a native call.
pub enum Ctl {
    Panic(String),
    Limit(LimitKind),
    Exit(i32),
    /// Ask the user; the call is retried afterwards.
    NeedPermission(Capability),
    /// Suspend the task until the future completes.
    Async(BoxFuture),
    /// Suspend the task on a scheduler event.
    Block(BlockOn),
    /// Let other tasks run; the call returns `()`.
    Yield,
    /// The host interrupted the engine (see `Meter::check`).
    Interrupted,
}

impl std::fmt::Debug for Ctl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ctl::Panic(m) => write!(f, "Panic({m})"),
            Ctl::Limit(k) => write!(f, "Limit({k:?})"),
            Ctl::Exit(c) => write!(f, "Exit({c})"),
            Ctl::NeedPermission(c) => write!(f, "NeedPermission({c})"),
            Ctl::Async(_) => write!(f, "Async"),
            Ctl::Block(_) => write!(f, "Block"),
            Ctl::Yield => write!(f, "Yield"),
            Ctl::Interrupted => write!(f, "Interrupted"),
        }
    }
}

pub fn panic<T>(msg: impl Into<String>) -> Result<T, Ctl> {
    Err(Ctl::Panic(msg.into()))
}

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
    /// A panic to raise when the fiber resumes.
    pub pending_panic: Option<String>,
}

/// A runtime error with its stack trace.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub function: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug)]
pub enum Trap {
    Panic { message: String, trace: Vec<TraceEntry> },
    Limit(LimitKind),
    Interrupted,
    Exit(i32),
}

/// Standard input source for an engine.
pub enum StdinSource {
    None,
    Bytes(std::io::Cursor<Vec<u8>>),
    Inherit(tokio::io::BufReader<tokio::io::Stdin>),
}

/// Output sink for standard streams and logs.
pub trait OutputSink: Send {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()>;
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    /// Called before the script reads standard input, so that a prompt it
    /// wrote is visible. A sink buffering a non-interactive stream may
    /// leave its buffer alone (as C's stdio does).
    fn flush_before_input(&mut self) -> std::io::Result<()> {
        self.flush()
    }
}

pub type SharedSink = Arc<Mutex<dyn OutputSink>>;

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
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.finish()
}

/// Engine services available to natives.
pub struct Host {
    pub policy: Policy,
    pub prompter: Option<Arc<dyn Prompter>>,
    pub grants_once: Vec<Capability>,
    pub denies_once: Vec<Capability>,
    pub always: HashMap<Capability, bool>,
    pub stdout: SharedSink,
    pub stderr: SharedSink,
    pub stdin: Arc<tokio::sync::Mutex<StdinSource>>,
    pub output_bytes: u64,
    pub limits: Limits,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub env: Option<Vec<(String, String)>>,
    pub rng: Rng,
    pub log_level: u8,
    pub interrupt: Arc<AtomicBool>,
    pub stream_decisions: HashMap<u8, bool>,
    pub(crate) tls: crate::stdlib::TlsSettings,
    /// Recently compiled regexes: (pattern hash, heap handle), see
    /// `stdlib::regex`. The handles are not GC roots; entries are checked
    /// against the object found at the handle.
    pub(crate) regex_cache: Vec<(u64, u32)>,
}

pub struct Vm {
    pub prog: Arc<Program>,
    pub natives: Arc<Vec<NativeEntry>>,
    pub heap: Heap,
    pub globals: Vec<Value>,
    pub consts: Vec<Value>,
    pub fibers: Vec<Option<Box<Fiber>>>,
    pub free_fibers: Vec<u32>,
    /// Finished fibers kept for reuse by `spawn_fiber` (see `recycle_fiber`);
    /// boxed like those in `fibers`, so that they move without copying.
    #[allow(clippy::vec_box)]
    spare_fibers: Vec<Box<Fiber>>,
    pub ready: VecDeque<u32>,
    pub current: u32,
    pub main_fiber: u32,
    pub live_tasks: usize,
    pub host: Host,
    pub io: sched::IoTable,
    pub gen_counter: u64,
    pub fuel_used: u64,
    /// Fuel accounting for native work (see `meter`).
    pub meter: Meter,
    /// Set when the running instruction hit the memory limit and should be
    /// retried after a garbage collection (see `interp::limit_trap`).
    pub(crate) gc_retry: bool,
    pub cancel_current: bool,
    /// Extra GC roots (e.g. values held by the test runner).
    pub pinned: Vec<Value>,
    pub(crate) pending_race: Option<Vec<Value>>,
    /// Runs the inputs of a REPL session: a top-level `?` that meets an
    /// `Err` panics (aborting the input) instead of ending the program.
    pub session: bool,
    /// The strings of one ASCII character, by byte (`Unit` until first
    /// made); shared, since strings are immutable. GC roots.
    ascii_strs: Vec<Value>,

    /// Set when a Rust panic (a bug) was caught in the interpreter, a native
    /// or an I/O finisher: the VM is memory safe but may be inconsistent, so
    /// a REPL session discards it.
    pub(crate) caught_panic: bool,
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
    /// `MAX_STACK_SLOTS`, or with the encoded memory limit error
    /// (`meter::ctl_message`) if the memory limit does not allow it.
    #[cold]
    #[inline(never)]
    pub fn grow_stack(&mut self, need: usize, heap: &mut Heap) -> Result<(), String> {
        if need > MAX_STACK_SLOTS {
            return Err("stack overflow".into());
        }
        let n = need.max(self.stack.len() * 2).min(MAX_STACK_SLOTS);
        let before = self.stack.capacity();
        let extra = n.saturating_sub(before) * std::mem::size_of::<Value>();
        if heap.would_exceed(extra) {
            return Err(meter::ctl_message(Ctl::Limit(LimitKind::Memory)));
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
    pub fn grow_frames(&mut self, heap: &mut Heap) -> Result<(), String> {
        let before = self.frames.capacity();
        let more = before.max(8);
        if heap.would_exceed(more * std::mem::size_of::<Frame>()) {
            return Err(meter::ctl_message(Ctl::Limit(LimitKind::Memory)));
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
        let heap = Heap::new(host.limits.max_memory);
        let consts = Vec::with_capacity(prog.consts.len());
        let globals = vec![Value::Uninit; prog.nglobals];
        let meter = Meter::new(host.limits.max_instructions, host.interrupt.clone());
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
            fuel_used: 0,
            meter,
            gc_retry: false,
            cancel_current: false,
            pinned: Vec::new(),
            pending_race: None,
            session: false,
            ascii_strs: Vec::new(),
            caught_panic: false,
        };
        vm.load_program();
        vm
    }

    /// Creates the values of the program's constants and the slots of its
    /// globals that do not exist yet: all of them for a new VM, or those a
    /// REPL session appended to the program.
    pub fn load_program(&mut self) {
        for i in self.consts.len()..self.prog.consts.len() {
            let heap = &mut self.heap;
            let v = match &self.prog.consts[i] {
                Const::Int(i) => Value::Int(*i),
                Const::Float(f) => Value::Float(*f),
                Const::Dur(d) => Value::Dur(*d),
                Const::Str(s) => heap.new_str(s.as_str()),
                Const::Bytes(b) => heap.new_bytes(b.clone()),
                Const::Fn(p) => heap.alloc(Obj::Closure(*p, Box::new([]))),
                Const::Native(n) => heap.alloc(Obj::NativeFn(*n)),
                Const::Spec(_) => Value::Unit,
                Const::TypeDesc(t) => heap.alloc(Obj::TypeDesc(t.clone())),
                Const::Tuple(items) => heap.new_tuple(items.iter().map(|&i| self.consts[i as usize]).collect()),
            };
            self.consts.push(v);
        }
        if self.globals.len() < self.prog.nglobals {
            self.globals.resize(self.prog.nglobals, Value::Uninit);
        }
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

    pub fn next_gen(&mut self) -> u64 {
        self.gen_counter += 1;
        self.gen_counter
    }

    // ------------------------------------------------------------------
    // Values

    pub fn str(&self, v: Value) -> &str {
        self.heap.str(v)
    }

    pub fn new_str(&mut self, s: impl Into<Box<str>>) -> Value {
        self.heap.new_str(s)
    }

    /// A function value (`fn() -> T?`) producing the elements of `src`.
    pub fn iter_fn(&mut self, src: IterSrc) -> Value {
        self.heap.alloc(Obj::IterFn(src))
    }

    /// Calls the iterator function `it` (an [`Obj::IterFn`]): the next
    /// element, or `None`.
    #[inline(never)]
    pub(crate) fn iter_step(&mut self, it: Value) -> Value {
        let next = self.iter_next(it);
        self.option(next)
    }

    /// [`Vm::iter_step`] without wrapping the element in `Some`.
    #[inline(never)]
    pub(crate) fn iter_next(&mut self, it: Value) -> Option<Value> {
        let Obj::IterFn(src) = self.heap.get(it) else { return None };
        // The characters of a string, one ASCII byte at a time (the common
        // case): no decoding, and the state updated in place.
        if let IterSrc::Chars { s, i } = *src
            && let Some(&b) = self.heap.str(Value::Obj(s)).as_bytes().get(i as usize)
            && b.is_ascii()
        {
            if let Some(IterSrc::Chars { i, .. }) = self.heap.iter_src_mut(it) {
                *i += 1;
            }
            return Some(self.ascii_str(b));
        }
        let (next, state) = match *src {
            IterSrc::List { list, i, n } => match self.heap.list(Value::Obj(list)).get(i as usize) {
                Some(&x) if i < n => (Some(x), IterSrc::List { list, i: i + 1, n }),
                _ => (None, IterSrc::List { list, i: n, n }),
            },
            IterSrc::Range { i, end } if i < end => (Some(Value::Int(i)), IterSrc::Range { i: i + 1, end }),
            IterSrc::Range { .. } => return None,
            IterSrc::Chars { s, i } => {
                let c = self.heap.str(Value::Obj(s)).get(i as usize..).and_then(|t| t.chars().next())?;
                let state = IterSrc::Chars { s, i: i + c.len_utf8() as i64 };
                (Some(self.short_str(c.encode_utf8(&mut [0; 4]))), state)
            }
        };
        // The state holds no new object: no write barrier is needed.
        if let Some(src) = self.heap.iter_src_mut(it) {
            *src = state;
        }
        next
    }

    /// The shared string of one ASCII character `b` (see `short_str`).
    #[inline]
    fn ascii_str(&mut self, b: u8) -> Value {
        match self.ascii_strs.get(b as usize) {
            Some(&v @ Value::Obj(_)) => v,
            _ => self.short_str(std::str::from_utf8(&[b]).unwrap_or("?")),
        }
    }

    /// A string equal to `s`, which may be shared: strings of one ASCII
    /// character (as `chars`, `char_at` and splitting produce in bulk) are
    /// made once per VM.
    pub fn short_str(&mut self, s: &str) -> Value {
        let &[b] = s.as_bytes() else { return self.heap.new_str(s) };
        if !b.is_ascii() {
            return self.heap.new_str(s);
        }
        if self.ascii_strs.is_empty() {
            self.ascii_strs = vec![Value::Unit; 128];
        }
        match self.ascii_strs[b as usize] {
            Value::Unit => {
                let v = self.heap.new_str(s);
                self.ascii_strs[b as usize] = v;
                v
            }
            v => v,
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
        if self.heap.would_exceed(extra) { Err(Ctl::Limit(LimitKind::Memory)) } else { Ok(()) }
    }

    /// Starts accounting for the buffers of an I/O operation that are not
    /// yet heap values (see [`iomem`]); the operation grows the reservation
    /// as its buffers grow and drops it before turning them into values.
    pub fn io_reservation(&self) -> iomem::Reservation {
        self.heap.io_memory.publish_heap(self.heap.used());
        iomem::Reservation::new(&self.heap.io_memory)
    }

    /// Like [`Vm::io_reservation`], reserving `bytes` at once. Fails with the
    /// `Err(Error { kind: "limit" })` value to return if they do not fit.
    pub fn reserve_io(&mut self, bytes: usize) -> Result<iomem::Reservation, Value> {
        let mut r = self.io_reservation();
        match r.ensure(bytes) {
            Ok(()) => Ok(r),
            Err(e) => Err(self.err_value("limit", &format!("cannot buffer {bytes} bytes: {e}"))),
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
    /// request to prompt the user.
    pub fn permit(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        self.permit_by(cap, Policy::check)
    }

    /// Like [`Vm::permit`] for a file capability whose path is already
    /// normalised (see [`Policy::check_normalized`]).
    pub fn permit_normalized(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        self.permit_by(cap, Policy::check_normalized)
    }

    /// Like [`Vm::permit`] for an operation on a directory entry itself
    /// (see [`Policy::check_entry`]).
    pub fn permit_entry(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        self.permit_by(cap, Policy::check_entry)
    }

    fn permit_by(&mut self, cap: &Capability, check: fn(&Policy, &Capability) -> Decision) -> Result<bool, Ctl> {
        // "Once" answers given for the call being retried (see `forget_once_answers`).
        if self.host.grants_once.contains(cap) {
            return Ok(true);
        }
        if self.host.denies_once.contains(cap) {
            return Ok(false);
        }
        if let Some(&d) = self.host.always.get(cap) {
            return Ok(d);
        }
        match check(&self.host.policy, cap) {
            Decision::Allow => Ok(true),
            Decision::Deny => Ok(false),
            Decision::Prompt => {
                if self.host.prompter.is_some() {
                    Err(Ctl::NeedPermission(cap.clone()))
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// Forgets "once" answers. A native that needs permission is retried
    /// after each prompt, possibly several times when it checks several
    /// capabilities (`fs.copy`), so "once" answers stay valid for the retried
    /// call and are forgotten when it returns anything else than another
    /// permission request: they never apply to a later call.
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
            PromptAnswer::AllowAlways => {
                self.host.always.insert(cap, true);
            }
            PromptAnswer::DenyAlways => {
                self.host.always.insert(cap, false);
            }
        }
    }

    /// Flushes buffered standard output (see `Engine`'s stdout sink).
    pub fn flush_stdout(&self) {
        let _ = self.host.stdout.lock().unwrap_or_else(|e| e.into_inner()).flush();
    }

    /// Flushes standard output before reading standard input, if the sink
    /// wants to (see [`OutputSink::flush_before_input`]).
    pub fn flush_before_input(&self) {
        let _ = self.host.stdout.lock().unwrap_or_else(|e| e.into_inner()).flush_before_input();
    }

    /// Writes to stdout (1) or stderr (2), enforcing policy and output limits.
    pub fn write_stream(&mut self, which: u8, data: &[u8]) -> Result<(), Ctl> {
        let allowed = match self.host.stream_decisions.get(&which) {
            Some(&d) => d,
            None => {
                let cap = if which == 1 { Capability::Stdout } else { Capability::Stderr };
                let d = self.permit(&cap)?;
                // Cache static decisions for the hot path.
                if !self.host.always.contains_key(&cap)
                    && self.host.grants_once.is_empty()
                    && self.host.denies_once.is_empty()
                    && let Decision::Allow | Decision::Deny = self.host.policy.check(&cap)
                {
                    self.host.stream_decisions.insert(which, d);
                }
                d
            }
        };
        if !allowed {
            return panic(if which == 1 { "permission denied: stdout" } else { "permission denied: stderr" });
        }
        self.host.output_bytes += data.len() as u64;
        if let Some(max) = self.host.limits.max_output_bytes
            && self.host.output_bytes > max
        {
            return Err(Ctl::Limit(LimitKind::Output));
        }
        if which != 1 {
            // Standard error is unbuffered: earlier output comes first.
            self.flush_stdout();
        }
        let sink = if which == 1 { &self.host.stdout } else { &self.host.stderr };
        let _ = sink.lock().unwrap_or_else(|e| e.into_inner()).write(data);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Fibers

    /// Creates a fiber that calls `closure` with no arguments.
    pub fn spawn_fiber(&mut self, closure: Value, task: Value) -> Result<u32, Ctl> {
        let proto = match self.heap.get(closure) {
            Obj::Closure(p, _) => *p,
            _ => return panic("spawn: not a closure"),
        };
        let nregs = self.prog.protos[proto as usize].nregs as usize;
        let Value::Obj(closure) = closure else { return panic("spawn: not a closure") };
        let frame = Frame { proto, ip: 0, base: 0, ret: 0, closure };
        let fiber = match self.spare_fibers.pop() {
            // A recycled fiber is already counted in `fiber_bytes`, and its
            // stack is all `Unit` (see `recycle_fiber`).
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
                    return Err(Ctl::Limit(crate::LimitKind::Memory));
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
                    pending_panic: None,
                });
                self.heap.fiber_bytes += fiber.footprint();
                fiber
            }
        };
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
        // (The final return truncated the stack.)
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
        f.pending_panic = None;
        self.spare_fibers.push(f);
    }

    /// Frees the spare fibers (before a memory limit error is reported).
    fn free_spare_fibers(&mut self) {
        let spare: usize = self.spare_fibers.iter().map(|f| f.footprint()).sum();
        self.heap.fiber_bytes = self.heap.fiber_bytes.saturating_sub(spare);
        self.spare_fibers = Vec::new();
    }

    /// Spawns a new task running `closure`; returns the task handle.
    pub fn spawn_task(&mut self, closure: Value) -> NResult {
        if self.live_tasks >= self.host.limits.max_tasks {
            return panic("task limit exceeded");
        }
        // The task starts without a call instruction: charge for entering
        // its code (and setting up its registers) as a call would
        // (`meter::code_fuel`).
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

    /// Wakes a waiting fiber with a panic.
    pub fn deliver_panic(&mut self, fid: u32, wgen: u64, msg: &str) {
        if let Some(Some(f)) = self.fibers.get_mut(fid as usize)
            && f.state == FiberState::Waiting
            && f.wgen == wgen
        {
            f.pending_panic = Some(msg.to_string());
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
        if let Some(f) = self.fibers.get_mut(fid as usize).and_then(|f| f.take()) {
            if let Some(io) = f.io {
                self.io.cancel(io);
            }
            self.free_fibers.push(fid);
            self.live_tasks = self.live_tasks.saturating_sub(1);
            for (w, g) in f.waiters {
                self.deliver_panic(w, g, "task cancelled");
            }
        }
    }

    // ------------------------------------------------------------------
    // Garbage collection

    pub fn collect_garbage(&mut self, current: Option<&mut Fiber>) {
        self.collect_garbage_with(current, false);
    }

    /// A major collection, freeing all unreachable objects; used before
    /// reporting a memory limit, since a minor collection leaves old garbage.
    pub fn collect_all_garbage(&mut self, current: Option<&mut Fiber>) {
        self.collect_garbage_with(current, true);
    }

    /// The GC roots: globals, constants, pinned values and the fibers' live
    /// stack windows, closures, tasks and race lists. Also clears the dead
    /// slots of the stacks (so that they never hold handles to objects freed
    /// later), shrinks stacks left large by deep recursion and recounts
    /// `heap.fiber_bytes`.
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
        let start = std::time::Instant::now();
        let mut full = force_major;
        if force_major {
            self.free_spare_fibers();
        } else if self.heap.cycle_active() {
            // A step of an incremental major collection, after a minor
            // collection if one is due.
            if self.heap.minor_due() {
                let roots = self.gc_roots(current.as_deref_mut());
                self.heap.collect(roots, false);
                self.heap.stats.record(start.elapsed(), false);
            }
            let step = std::time::Instant::now();
            if self.heap.cycle_step() {
                let roots = self.gc_roots(current.as_deref_mut());
                self.heap.finish_marking(roots);
            }
            self.heap.stats.record_step(step.elapsed());
        } else {
            let roots = self.gc_roots(current.as_deref_mut());
            // Major collections when the old generation has grown enough
            // (and regularly under stress testing); minor ones otherwise.
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
        // Unreachable old objects may be what keeps us over the limit: only
        // a full collection decides.
        if !full && self.heap.over_limit() {
            self.free_spare_fibers();
            full = self.heap.over_limit();
        }
        if full {
            let start = std::time::Instant::now();
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
            let line = proto.lines.get(idx).copied().unwrap_or(0) as usize;
            if proto.name == "<entry>" {
                continue;
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
