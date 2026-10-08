//! The virtual machine: heap, fibers (tasks), scheduler and interpreter.

pub mod bytecode;
pub mod compare;
pub mod display;
pub mod heap;
mod interp;
pub mod sched;
pub mod value;

use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter};
use crate::{LimitKind, Limits};
use bytecode::*;
use heap::Heap;
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

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub proto: u32,
    pub ip: u32,
    pub base: u32,
    /// Absolute stack slot receiving the return value.
    pub ret: u32,
    pub closure: Value,
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
}

pub struct Vm {
    pub prog: Arc<Program>,
    pub natives: Arc<Vec<NativeEntry>>,
    pub heap: Heap,
    pub globals: Vec<Value>,
    pub consts: Vec<Value>,
    pub fibers: Vec<Option<Box<Fiber>>>,
    pub free_fibers: Vec<u32>,
    pub ready: VecDeque<u32>,
    pub current: u32,
    pub main_fiber: u32,
    pub live_tasks: usize,
    pub host: Host,
    pub io: sched::IoTable,
    pub gen_counter: u64,
    pub fuel_used: u64,
    pub cancel_current: bool,
    /// Extra GC roots (e.g. values held by the test runner).
    pub pinned: Vec<Value>,
    pub(crate) pending_race: Option<Vec<Value>>,
}

pub const SLICE_FUEL: i64 = 20_000;

impl Vm {
    pub fn new(prog: Arc<Program>, natives: Arc<Vec<NativeEntry>>, host: Host) -> Vm {
        let mut heap = Heap::new(host.limits.max_memory);
        let mut consts = Vec::with_capacity(prog.consts.len());
        for c in &prog.consts {
            let v = match c {
                Const::Int(i) => Value::Int(*i),
                Const::Float(f) => Value::Float(*f),
                Const::Dur(d) => Value::Dur(*d),
                Const::Str(s) => heap.new_str(s.as_str()),
                Const::Bytes(b) => heap.new_bytes(b.clone()),
                Const::Fn(p) => heap.alloc(Obj::Closure(*p, Box::new([]))),
                Const::Native(n) => heap.alloc(Obj::NativeFn(*n)),
                Const::Spec(_) => Value::Unit,
                Const::TypeDesc(t) => heap.alloc(Obj::TypeDesc(t.clone())),
            };
            consts.push(v);
        }
        let globals = vec![Value::Uninit; prog.nglobals];
        Vm {
            prog,
            natives,
            heap,
            globals,
            consts,
            fibers: Vec::new(),
            free_fibers: Vec::new(),
            ready: VecDeque::new(),
            current: 0,
            main_fiber: 0,
            live_tasks: 0,
            host,
            io: sched::IoTable::default(),
            gen_counter: 1,
            fuel_used: 0,
            cancel_current: false,
            pinned: Vec::new(),
            pending_race: None,
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

    pub fn some(&mut self, v: Value) -> Value {
        let id = self.prog.ids.some;
        self.heap.alloc(Obj::Variant(id, Box::new([v])))
    }

    pub fn none(&self) -> Value {
        Value::Tag(self.prog.ids.none)
    }

    pub fn ok(&mut self, v: Value) -> Value {
        let id = self.prog.ids.ok;
        self.heap.alloc(Obj::Variant(id, Box::new([v])))
    }

    pub fn err(&mut self, v: Value) -> Value {
        let id = self.prog.ids.err;
        self.heap.alloc(Obj::Variant(id, Box::new([v])))
    }

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
        if self.heap.variant_id(v) == self.prog.ids.some { Some(self.heap.fields(v)[0]) } else { None }
    }

    pub fn display(&self, v: Value) -> Result<String, Ctl> {
        display::to_display(&self.prog, &self.heap, v).map_err(Ctl::Panic)
    }

    pub fn debug(&self, v: Value) -> Result<String, Ctl> {
        display::to_debug(&self.prog, &self.heap, v).map_err(Ctl::Panic)
    }

    /// Fails with a memory limit error if `extra` bytes cannot be allocated.
    pub fn reserve(&self, extra: usize) -> Result<(), Ctl> {
        if self.heap.would_exceed(extra) { Err(Ctl::Limit(LimitKind::Memory)) } else { Ok(()) }
    }

    // ------------------------------------------------------------------
    // Policy

    /// Checks a capability: `Ok(true)` allowed, `Ok(false)` denied, or a
    /// request to prompt the user.
    pub fn permit(&mut self, cap: &Capability) -> Result<bool, Ctl> {
        if let Some(i) = self.host.grants_once.iter().position(|c| c == cap) {
            self.host.grants_once.remove(i);
            return Ok(true);
        }
        if let Some(i) = self.host.denies_once.iter().position(|c| c == cap) {
            self.host.denies_once.remove(i);
            return Ok(false);
        }
        if let Some(&d) = self.host.always.get(cap) {
            return Ok(d);
        }
        match self.host.policy.check(cap) {
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
        let sink = if which == 1 { self.host.stdout.clone() } else { self.host.stderr.clone() };
        let mut s = sink.lock().unwrap_or_else(|e| e.into_inner());
        let _ = s.write(data);
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
        let fiber = Box::new(Fiber {
            stack: vec![Value::Unit; nregs.max(1)],
            frames: vec![Frame { proto, ip: 0, base: 0, ret: 0, closure }],
            task,
            state: FiberState::Ready,
            wgen: 0,
            result_slot: 0,
            race: vec![],
            waiters: vec![],
            io: None,
            pending_panic: None,
        });
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

    /// Spawns a new task running `closure`; returns the task handle.
    pub fn spawn_task(&mut self, closure: Value) -> NResult {
        if self.live_tasks >= self.host.limits.max_tasks {
            return panic("task limit exceeded");
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
        for (w, g) in f.waiters {
            self.deliver(w, g, result);
        }
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

    pub fn collect_garbage(&mut self, current: Option<&Fiber>) {
        let mut roots: Vec<Value> = Vec::with_capacity(1024);
        roots.extend(self.globals.iter().copied());
        roots.extend(self.consts.iter().copied());
        roots.extend(self.pinned.iter().copied());
        let add_fiber = |f: &Fiber, roots: &mut Vec<Value>| {
            roots.extend(f.stack.iter().copied());
            roots.extend(f.frames.iter().map(|fr| fr.closure));
            roots.push(f.task);
            roots.extend(f.race.iter().copied());
        };
        for f in self.fibers.iter().flatten() {
            add_fiber(f, &mut roots);
        }
        if let Some(f) = current {
            add_fiber(f, &mut roots);
        }
        self.heap.collect(roots);
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
