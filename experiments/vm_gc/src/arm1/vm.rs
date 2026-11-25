//! The Arm 1 interpreter, callbacks, fibers, and host API.

use super::heap::{Heap, HostObject, Obj, SLOT_BYTES, Value};
use crate::shared::isa::{Const, Op, Program};
use crate::shared::suite::{Arg, HostValue};
use crate::shared::{Fail, Limits, Result, ShortStr, check_stack, stack_address};
use std::future::Future;
use std::mem::size_of;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

/// The argument registers of a native call.
#[derive(Clone, Copy, Debug)]
pub struct Args {
    base: usize,
    pub len: usize,
}

/// Continues an async native call after its future completes.
pub type AsyncResume = Box<dyn FnOnce(&mut Vm) -> Result<Value> + Send>;
/// The future of an async native call.
pub type AsyncCall = Pin<Box<dyn Future<Output = AsyncResume> + Send>>;
pub type SyncFn = Box<dyn Fn(&mut Vm, Args) -> Result<Value> + Send + Sync>;
pub type AsyncFn = Box<dyn Fn(&mut Vm, Args) -> Result<AsyncCall> + Send + Sync>;

pub enum Native {
    Sync(SyncFn),
    Async(AsyncFn),
}

/// A named group of native functions.
#[derive(Default)]
pub struct NativeModule {
    functions: Vec<(String, Native)>,
}

impl NativeModule {
    pub fn function(&mut self, name: &str, f: impl Fn(&mut Vm, Args) -> Result<Value> + Send + Sync + 'static) -> &mut Self {
        self.functions.push((name.to_string(), Native::Sync(Box::new(f))));
        self
    }

    pub fn async_function(&mut self, name: &str, f: impl Fn(&mut Vm, Args) -> Result<AsyncCall> + Send + Sync + 'static) -> &mut Self {
        self.functions.push((name.to_string(), Native::Async(Box::new(f))));
        self
    }
}

/// A non-LIFO root that an async native holds across `.await`.
#[derive(Debug)]
pub struct AsyncRoot(u32);

#[derive(Clone, Copy)]
struct Frame {
    func: u16,
    base: u32,
    pc: u32,
    /// Absolute stack index of the caller's destination register.
    ret: u32,
    closure: Value,
}

enum FiberState {
    Ready,
    Waiting { call: AsyncCall, dst: usize },
    Done(Result<HostValue>),
}

struct Fiber {
    stack: Vec<Value>,
    frames: Vec<Frame>,
    state: FiberState,
}

/// How a run of the interpreter ended.
enum Exit {
    Return(Value),
    /// An async native call stored its future in `Vm::awaiting`.
    Await,
}

/// Spare stack capacity that callbacks and frames may use before growth
/// reserves memory, so that unrooted arguments are on the stack before a
/// collection can run.
const STACK_SLACK: usize = 256;
const FRAME_SLACK: usize = 16;
const ROOT_SLACK: usize = 16;
const MAX_FRAMES: usize = 100_000;

pub struct Vm {
    pub heap: Heap,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// Values that native code holds across allocations and callbacks.
    roots: Vec<Value>,
    async_roots: Vec<Option<Value>>,
    free_async_roots: Vec<u32>,
    fibers: Vec<Fiber>,
    /// The future of the async native call that the current fiber waits on.
    awaiting: Option<(AsyncCall, usize)>,
    globals: Vec<Value>,
    consts: Vec<Value>,
    pub program: Arc<Program>,
    natives: Arc<[Native]>,
    pub fuel: u64,
    /// Rust stack address at the outermost interpreter entry.
    stack_base: usize,
    /// Nesting depth of synchronous calls into the interpreter.
    sync_depth: u32,
}

#[cold]
#[inline(never)]
pub fn type_error(what: &str) -> Fail {
    Fail::Error(format!("type error: expected {what}"))
}

impl Vm {
    pub fn new(program: Arc<Program>, limits: Limits, modules: Vec<NativeModule>) -> Result<Vm> {
        let mut available: Vec<(String, Native)> = modules.into_iter().flat_map(|m| m.functions).collect();
        let mut natives = Vec::new();
        for name in &program.natives {
            let index = available.iter().position(|(n, _)| n == name).ok_or_else(|| Fail::error(format!("unknown native `{name}`")))?;
            natives.push(available.swap_remove(index).1);
        }
        let mut vm = Vm {
            heap: Heap::new(limits.max_memory, limits.gc_min_threshold),
            stack: Vec::new(),
            frames: Vec::new(),
            roots: Vec::new(),
            async_roots: Vec::new(),
            free_async_roots: Vec::new(),
            fibers: Vec::new(),
            awaiting: None,
            globals: Vec::new(),
            consts: Vec::new(),
            program: program.clone(),
            natives: natives.into(),
            fuel: limits.fuel,
            stack_base: stack_address(),
            sync_depth: 0,
        };
        vm.reserve(STACK_SLACK * size_of::<Value>() + FRAME_SLACK * size_of::<Frame>() + ROOT_SLACK * size_of::<Value>())?;
        vm.stack.reserve_exact(STACK_SLACK);
        vm.frames.reserve_exact(FRAME_SLACK);
        vm.roots.reserve_exact(ROOT_SLACK);
        vm.reserve((program.globals.len() + program.consts.len()) * size_of::<Value>())?;
        vm.globals = vec![Value::Unit; program.globals.len()];
        for c in &program.consts {
            let v = match c {
                Const::Int(i) => Value::Int(*i),
                Const::Float(f) => Value::Float(*f),
                Const::Str(s) => vm.new_str(s)?,
            };
            vm.consts.push(v);
        }
        Ok(vm)
    }

    // ---------------------------------------------------------------------
    // Memory
    // ---------------------------------------------------------------------

    /// Reserves `bytes` of tracked memory. May collect garbage.
    #[inline(always)]
    pub fn reserve(&mut self, bytes: usize) -> Result<()> {
        let used = self.heap.tracker.used();
        if bytes <= self.heap.next_check.saturating_sub(used) {
            self.heap.tracker.add_mut(bytes);
            return Ok(());
        }
        self.reserve_slow(bytes)
    }

    #[cold]
    #[inline(never)]
    fn reserve_slow(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.heap.tracker.limit() {
            return Err(Fail::OutOfMemory);
        }
        if self.heap.policy.should_collect(self.heap.tracker.used().saturating_add(bytes)) {
            self.collect();
        }
        if !self.heap.tracker.fits(bytes) {
            self.collect();
            if !self.heap.tracker.fits(bytes) {
                return Err(Fail::OutOfMemory);
            }
        }
        self.heap.tracker.add_mut(bytes);
        Ok(())
    }

    /// Returns `bytes` of tracked memory.
    #[inline]
    pub fn release(&mut self, bytes: usize) {
        self.heap.tracker.sub_mut(bytes);
    }

    /// Reserves a slot and `bytes` of tracked memory for a new object. May
    /// collect garbage.
    #[inline(always)]
    pub fn reserve_object(&mut self, bytes: usize) -> Result<()> {
        let growth = self.heap.slot_growth();
        if growth > 0 {
            self.reserve(growth * SLOT_BYTES)?;
            self.heap.grow_slots(growth);
        }
        self.reserve(bytes)
    }

    /// Stores `obj`, whose size the caller reserved with [`Vm::reserve_object`].
    #[inline(always)]
    pub fn insert(&mut self, obj: Obj) -> Value {
        self.heap.insert(obj)
    }

    /// Collects garbage now.
    pub fn collect(&mut self) {
        let Vm { heap, stack, frames, roots, async_roots, fibers, globals, consts, program, .. } = self;
        heap.collect(|mark| {
            mark.values(live_registers(program, stack, frames));
            frames.iter().for_each(|f| mark.value(f.closure));
            for fiber in fibers.iter() {
                mark.values(live_registers(program, &fiber.stack, &fiber.frames));
                fiber.frames.iter().for_each(|f| mark.value(f.closure));
            }
            mark.values(roots);
            async_roots.iter().flatten().for_each(|v| mark.value(*v));
            mark.values(globals);
            mark.values(consts);
        });
        let live = heap.tracker.used();
        heap.policy.collected(live);
        heap.update_next_check();
    }

    /// Pauses or resumes automatic collections.
    pub fn pause_gc(&mut self, paused: bool) {
        self.heap.policy.paused = paused;
        self.heap.update_next_check();
    }

    #[inline(always)]
    fn ensure_stack(&mut self, top: usize) -> Result<()> {
        if top + STACK_SLACK > self.stack.capacity() {
            self.grow_stack(top)?;
        }
        Ok(())
    }

    #[cold]
    fn grow_stack(&mut self, top: usize) -> Result<()> {
        let capacity = (top + STACK_SLACK).max(self.stack.capacity() * 2);
        self.reserve((capacity - self.stack.capacity()) * size_of::<Value>())?;
        self.stack.reserve_exact(capacity - self.stack.len());
        Ok(())
    }

    #[inline(always)]
    fn push_frame(&mut self, frame: Frame) -> Result<()> {
        self.frames.push(frame);
        if self.frames.len() + FRAME_SLACK > self.frames.capacity() {
            self.grow_frames()?;
        }
        Ok(())
    }

    #[cold]
    fn grow_frames(&mut self) -> Result<()> {
        if self.frames.len() > MAX_FRAMES {
            return Err(Fail::StackOverflow);
        }
        let capacity = self.frames.capacity() * 2;
        self.reserve((capacity - self.frames.capacity()) * size_of::<Frame>())?;
        self.frames.reserve_exact(capacity - self.frames.len());
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Roots
    // ---------------------------------------------------------------------

    /// Returns the root stack length, to pass to [`Vm::truncate_roots`] later.
    #[inline]
    pub fn root_mark(&self) -> usize {
        self.roots.len()
    }

    /// Roots `v` until the root stack is truncated below it. Returns its index.
    #[inline]
    pub fn push_root(&mut self, v: Value) -> Result<usize> {
        self.roots.push(v);
        if self.roots.len() + ROOT_SLACK > self.roots.capacity() {
            self.grow_roots()?;
        }
        Ok(self.roots.len() - 1)
    }

    #[cold]
    fn grow_roots(&mut self) -> Result<()> {
        let capacity = self.roots.capacity() * 2;
        self.reserve((capacity - self.roots.capacity()) * size_of::<Value>())?;
        self.roots.reserve_exact(capacity - self.roots.len());
        Ok(())
    }

    #[inline]
    pub fn root(&self, index: usize) -> Value {
        self.roots[index]
    }

    #[inline]
    pub fn set_root(&mut self, index: usize, v: Value) {
        self.roots[index] = v;
    }

    #[inline]
    pub fn roots_from(&self, mark: usize) -> &[Value] {
        &self.roots[mark..]
    }

    #[inline]
    pub fn truncate_roots(&mut self, mark: usize) {
        self.roots.truncate(mark);
    }

    /// Roots `v` for an async native until [`Vm::take_async_root`].
    pub fn async_root(&mut self, v: Value) -> Result<AsyncRoot> {
        if let Some(i) = self.free_async_roots.pop() {
            self.async_roots[i as usize] = Some(v);
            return Ok(AsyncRoot(i));
        }
        let (len, capacity) = (self.async_roots.len(), self.async_roots.capacity());
        if len == capacity {
            let mark = self.push_root(v)?;
            let grown = (capacity * 2).max(4);
            self.reserve((grown - capacity) * (size_of::<Option<Value>>() + size_of::<u32>()))?;
            self.async_roots.reserve_exact(grown - len);
            self.free_async_roots.reserve_exact(grown);
            self.truncate_roots(mark);
        }
        self.async_roots.push(Some(v));
        Ok(AsyncRoot(len as u32))
    }

    pub fn take_async_root(&mut self, root: AsyncRoot) -> Value {
        self.free_async_roots.push(root.0);
        self.async_roots[root.0 as usize].take().unwrap_or(Value::Unit)
    }

    // ---------------------------------------------------------------------
    // Host API
    // ---------------------------------------------------------------------

    #[inline]
    pub fn arg(&self, args: Args, i: usize) -> Value {
        self.stack[args.base + i]
    }

    /// Fails if nested callbacks used up the Rust stack budget.
    #[inline]
    pub fn check_stack(&self) -> Result<()> {
        check_stack(self.stack_base)
    }

    #[inline]
    pub fn charge(&mut self, units: u64) -> Result<()> {
        crate::shared::charge(&mut self.fuel, units)
    }

    #[inline]
    pub fn str<'a>(&'a self, v: &'a Value) -> Result<&'a str> {
        self.heap.str(v).ok_or_else(|| type_error("str"))
    }

    pub fn new_str(&mut self, s: &str) -> Result<Value> {
        if let Some(short) = ShortStr::new(s) {
            return Ok(Value::Short(short));
        }
        self.reserve_object(s.len())?;
        Ok(self.insert(Obj::Str(s.into())))
    }

    /// Stores `s`, whose capacity the caller already reserved, as a string.
    pub fn new_str_from_reserved(&mut self, s: String) -> Result<Value> {
        let capacity = s.capacity();
        if let Some(short) = ShortStr::new(&s) {
            self.release(capacity);
            return Ok(Value::Short(short));
        }
        self.reserve_object(0)?;
        let boxed = s.into_boxed_str();
        self.release(capacity - boxed.len());
        Ok(self.insert(Obj::Str(boxed)))
    }

    pub fn new_list(&mut self, capacity: usize) -> Result<Value> {
        self.reserve_object(capacity.saturating_mul(size_of::<Value>()))?;
        Ok(self.insert(Obj::List(Vec::with_capacity(capacity))))
    }

    pub fn list_len(&self, list: Value) -> Result<usize> {
        match self.obj(list)? {
            Obj::List(items) => Ok(items.len()),
            _ => Err(type_error("list")),
        }
    }

    pub fn list_get(&self, list: Value, i: usize) -> Result<Option<Value>> {
        match self.obj(list)? {
            Obj::List(items) => Ok(items.get(i).copied()),
            _ => Err(type_error("list")),
        }
    }

    /// Appends `v` to `list`. The caller must root `v` and `list`.
    pub fn list_push(&mut self, list: Value, v: Value) -> Result<()> {
        let Value::Obj(h) = list else { return Err(type_error("list")) };
        let Obj::List(items) = self.heap.get(h) else { return Err(type_error("list")) };
        let (len, capacity) = (items.len(), items.capacity());
        if len == capacity {
            let grown = (capacity * 2).max(4);
            self.reserve((grown - capacity) * size_of::<Value>())?;
            let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
            items.reserve_exact(grown - len);
        }
        let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
        items.push(v);
        Ok(())
    }

    pub fn new_host(&mut self, obj: Box<dyn HostObject>) -> Result<Value> {
        self.reserve_object(obj.size())?;
        Ok(self.insert(Obj::Host(obj)))
    }

    pub fn host<T: HostObject>(&self, v: Value) -> Result<&T> {
        match self.obj(v)? {
            Obj::Host(h) => (&**h as &dyn std::any::Any).downcast_ref().ok_or_else(|| type_error("host object")),
            _ => Err(type_error("host object")),
        }
    }

    pub fn host_mut<T: HostObject>(&mut self, v: Value) -> Result<&mut T> {
        let Value::Obj(h) = v else { return Err(type_error("host object")) };
        match self.heap.get_mut(h) {
            Obj::Host(h) => (&mut **h as &mut dyn std::any::Any).downcast_mut().ok_or_else(|| type_error("host object")),
            _ => Err(type_error("host object")),
        }
    }

    #[inline]
    pub fn obj(&self, v: Value) -> Result<&Obj> {
        match v {
            Value::Obj(h) => Ok(self.heap.get(h)),
            _ => Err(type_error("object")),
        }
    }

    pub fn field(&self, v: Value, idx: usize) -> Result<Value> {
        match self.obj(v)? {
            Obj::Struct { fields, .. } => fields.get(idx).copied().ok_or_else(|| type_error("field")),
            _ => Err(type_error("struct")),
        }
    }

    /// Calls `callee` synchronously. The callee cannot wait on async natives.
    /// The result is unrooted.
    pub fn call(&mut self, callee: Value, args: &[Value]) -> Result<Value> {
        match self.obj(callee)? {
            Obj::Closure { func, .. } => self.call_function(*func, callee, args),
            _ => Err(type_error("closure")),
        }
    }

    /// Calls function `func` synchronously with `closure` as its closure.
    pub fn call_function(&mut self, func: u16, closure: Value, args: &[Value]) -> Result<Value> {
        check_stack(self.stack_base)?;
        let f = &self.program.funcs[func as usize];
        if f.params as usize != args.len() {
            return Err(Fail::error(format!("{} expects {} arguments", f.name, f.params)));
        }
        let regs = f.regs as usize;
        let depth = self.frames.len();
        let base = live_registers(&self.program, &self.stack, &self.frames).len();
        debug_assert!(args.len() <= STACK_SLACK);
        // The stack slack holds the arguments without a reallocation, so
        // they are on the stack before a collection can run.
        if self.stack.len() < base + args.len() {
            self.stack.resize(base + args.len(), Value::Unit);
        }
        self.stack[base..base + args.len()].copy_from_slice(args);
        self.sync_depth += 1;
        let result = self.enter(func, closure, base, regs).and_then(|_| self.execute(depth));
        self.sync_depth -= 1;
        self.frames.truncate(depth);
        match result? {
            Exit::Return(v) => Ok(v),
            Exit::Await => unreachable!("async natives fail inside synchronous calls"),
        }
    }

    /// Pushes a frame whose arguments are already at `base`.
    fn enter(&mut self, func: u16, closure: Value, base: usize, regs: usize) -> Result<()> {
        self.push_frame(Frame { func, base: base as u32, pc: 0, ret: 0, closure })?;
        self.ensure_stack(base + regs)?;
        if self.stack.len() < base + regs {
            self.stack.resize(base + regs, Value::Unit);
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Interpreter
    // ---------------------------------------------------------------------

    #[inline]
    fn values_eq(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Obj(x), Value::Obj(y)) if x == y => true,
            (Value::Obj(_), _) | (_, Value::Obj(_)) => matches!((self.heap.str(a), self.heap.str(b)), (Some(x), Some(y)) if x == y),
            _ => a == b,
        }
    }

    fn method_of(&self, receiver: Value, method: u16) -> Result<u16> {
        let p = &self.program;
        let found = match receiver {
            Value::Tag(tag) => p.enum_method(p.tags[tag as usize].enum_id, method),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Struct { ty, .. } => p.struct_method(*ty, method),
                Obj::Variant { tag, .. } => p.enum_method(p.tags[*tag as usize].enum_id, method),
                _ => None,
            },
            _ => None,
        };
        found.ok_or_else(|| Fail::error(format!("no method `{}`", p.methods[method as usize])))
    }

    /// Runs frames until the frame count drops to `entry_depth`.
    fn execute(&mut self, entry_depth: usize) -> Result<Exit> {
        let program = self.program.clone();
        let natives = self.natives.clone();
        'frame: loop {
            let frame = *self.frames.last().expect("a frame");
            let code = &program.funcs[frame.func as usize].code[..];
            let base = frame.base as usize;
            let mut pc = frame.pc as usize;
            macro_rules! reg {
                ($r:expr) => {
                    self.stack[base + $r as usize]
                };
            }
            macro_rules! int {
                ($r:expr) => {
                    match reg!($r) {
                        Value::Int(i) => i,
                        _ => return Err(type_error("int")),
                    }
                };
            }
            macro_rules! float {
                ($r:expr) => {
                    match reg!($r) {
                        Value::Float(f) => f,
                        _ => return Err(type_error("float")),
                    }
                };
            }
            macro_rules! handle {
                ($r:expr) => {
                    match reg!($r) {
                        Value::Obj(h) => h,
                        _ => return Err(type_error("object")),
                    }
                };
            }
            macro_rules! save_pc {
                () => {
                    self.frames.last_mut().expect("a frame").pc = pc as u32
                };
            }
            loop {
                if self.fuel == 0 {
                    save_pc!();
                    return Err(Fail::OutOfFuel);
                }
                self.fuel -= 1;
                let op = code[pc];
                pc += 1;
                match op {
                    Op::Const { dst, idx } => reg!(dst) = self.consts[idx as usize],
                    Op::Int { dst, value } => reg!(dst) = Value::Int(value as i64),
                    Op::Unit { dst } => reg!(dst) = Value::Unit,
                    Op::None { dst } => reg!(dst) = Value::None,
                    Op::Bool { dst, value } => reg!(dst) = Value::Bool(value),
                    Op::Tag { dst, tag } => reg!(dst) = Value::Tag(tag),
                    Op::Move { dst, src } => reg!(dst) = reg!(src),
                    Op::Add { dst, a, b } => reg!(dst) = Value::Int(int!(a).checked_add(int!(b)).ok_or_else(overflow)?),
                    Op::Sub { dst, a, b } => reg!(dst) = Value::Int(int!(a).checked_sub(int!(b)).ok_or_else(overflow)?),
                    Op::Mul { dst, a, b } => reg!(dst) = Value::Int(int!(a).checked_mul(int!(b)).ok_or_else(overflow)?),
                    Op::Div { dst, a, b } => reg!(dst) = Value::Int(int!(a).checked_div(int!(b)).ok_or_else(div_error)?),
                    Op::Rem { dst, a, b } => reg!(dst) = Value::Int(int!(a).checked_rem(int!(b)).ok_or_else(div_error)?),
                    Op::FAdd { dst, a, b } => reg!(dst) = Value::Float(float!(a) + float!(b)),
                    Op::FSub { dst, a, b } => reg!(dst) = Value::Float(float!(a) - float!(b)),
                    Op::FMul { dst, a, b } => reg!(dst) = Value::Float(float!(a) * float!(b)),
                    Op::ToFloat { dst, src } => reg!(dst) = Value::Float(int!(src) as f64),
                    Op::Lt { dst, a, b } => {
                        reg!(dst) = Value::Bool(match (reg!(a), reg!(b)) {
                            (Value::Int(x), Value::Int(y)) => x < y,
                            (Value::Float(x), Value::Float(y)) => x < y,
                            _ => return Err(type_error("numbers")),
                        })
                    }
                    Op::Le { dst, a, b } => {
                        reg!(dst) = Value::Bool(match (reg!(a), reg!(b)) {
                            (Value::Int(x), Value::Int(y)) => x <= y,
                            (Value::Float(x), Value::Float(y)) => x <= y,
                            _ => return Err(type_error("numbers")),
                        })
                    }
                    Op::Eq { dst, a, b } => reg!(dst) = Value::Bool(self.values_eq(&reg!(a), &reg!(b))),
                    Op::Not { dst, src } => {
                        reg!(dst) = match reg!(src) {
                            Value::Bool(b) => Value::Bool(!b),
                            _ => return Err(type_error("bool")),
                        }
                    }
                    Op::Jump { target } => pc = target as usize,
                    Op::JumpIf { cond, target } => match reg!(cond) {
                        Value::Bool(true) => pc = target as usize,
                        Value::Bool(false) => {}
                        _ => return Err(type_error("bool")),
                    },
                    Op::JumpIfNot { cond, target } => match reg!(cond) {
                        Value::Bool(false) => pc = target as usize,
                        Value::Bool(true) => {}
                        _ => return Err(type_error("bool")),
                    },
                    Op::Call { dst, func, base: b, argc } => {
                        save_pc!();
                        self.call_frame(func, Value::Unit, base, b, argc, dst)?;
                        continue 'frame;
                    }
                    Op::CallClosure { dst, callee, base: b, argc } => {
                        let closure = reg!(callee);
                        let Obj::Closure { func, .. } = self.heap.get(handle!(callee)) else { return Err(type_error("closure")) };
                        let func = *func;
                        save_pc!();
                        self.call_frame(func, closure, base, b, argc, dst)?;
                        continue 'frame;
                    }
                    Op::CallMethod { dst, method, base: b, argc } => {
                        let func = self.method_of(reg!(b), method)?;
                        save_pc!();
                        self.call_frame(func, Value::Unit, base, b, argc, dst)?;
                        continue 'frame;
                    }
                    Op::CallNative { dst, native, base: b, argc } => {
                        save_pc!();
                        let args = Args { base: base + b as usize, len: argc as usize };
                        match &natives[native as usize] {
                            Native::Sync(f) => {
                                let mark = self.roots.len();
                                let result = f(self, args);
                                self.roots.truncate(mark);
                                reg!(dst) = result?;
                            }
                            Native::Async(f) => {
                                if self.sync_depth > 0 {
                                    return Err(Fail::error("an async native cannot run inside a synchronous call"));
                                }
                                let mark = self.roots.len();
                                let call = f(self, args);
                                self.roots.truncate(mark);
                                self.awaiting = Some((call?, base + dst as usize));
                                return Ok(Exit::Await);
                            }
                        }
                    }
                    Op::Return { src } => {
                        let v = reg!(src);
                        self.frames.pop();
                        if self.frames.len() == entry_depth {
                            return Ok(Exit::Return(v));
                        }
                        self.stack[frame.ret as usize] = v;
                        continue 'frame;
                    }
                    Op::Closure { dst, func, base: b, count } => {
                        let bytes = count as usize * size_of::<Value>();
                        self.reserve_object(bytes)?;
                        let start = base + b as usize;
                        let captures = self.stack[start..start + count as usize].into();
                        reg!(dst) = self.insert(Obj::Closure { func, captures });
                    }
                    Op::Capture { dst, idx } => {
                        let Value::Obj(h) = frame.closure else { return Err(type_error("closure")) };
                        let Obj::Closure { captures, .. } = self.heap.get(h) else { return Err(type_error("closure")) };
                        reg!(dst) = captures[idx as usize];
                    }
                    Op::NewCell { dst, src } => {
                        self.reserve_object(0)?;
                        let v = reg!(src);
                        reg!(dst) = self.insert(Obj::Cell(v));
                    }
                    Op::CellGet { dst, cell } => {
                        let Obj::Cell(v) = self.heap.get(handle!(cell)) else { return Err(type_error("cell")) };
                        reg!(dst) = *v;
                    }
                    Op::CellSet { cell, src } => {
                        let v = reg!(src);
                        let Obj::Cell(slot) = self.heap.get_mut(handle!(cell)) else { return Err(type_error("cell")) };
                        *slot = v;
                    }
                    Op::Struct { dst, ty, base: b, count } => {
                        self.reserve_object(count as usize * size_of::<Value>())?;
                        let start = base + b as usize;
                        let fields = self.stack[start..start + count as usize].into();
                        reg!(dst) = self.insert(Obj::Struct { ty, fields });
                    }
                    Op::Field { dst, obj, idx } => {
                        let Obj::Struct { fields, .. } = self.heap.get(handle!(obj)) else { return Err(type_error("struct")) };
                        reg!(dst) = fields[idx as usize];
                    }
                    Op::SetField { obj, idx, src } => {
                        let v = reg!(src);
                        let Obj::Struct { fields, .. } = self.heap.get_mut(handle!(obj)) else { return Err(type_error("struct")) };
                        fields[idx as usize] = v;
                    }
                    Op::Variant { dst, tag, base: b, count } => {
                        self.reserve_object(count as usize * size_of::<Value>())?;
                        let start = base + b as usize;
                        let fields = self.stack[start..start + count as usize].into();
                        reg!(dst) = self.insert(Obj::Variant { tag, fields });
                    }
                    Op::VariantIndex { dst, src } => {
                        let tag = match reg!(src) {
                            Value::Tag(t) => t,
                            Value::Obj(h) => match self.heap.get(h) {
                                Obj::Variant { tag, .. } => *tag,
                                _ => return Err(type_error("variant")),
                            },
                            _ => return Err(type_error("variant")),
                        };
                        reg!(dst) = Value::Int(program.tags[tag as usize].index as i64);
                    }
                    Op::VariantField { dst, src, idx } => {
                        let Obj::Variant { fields, .. } = self.heap.get(handle!(src)) else { return Err(type_error("variant")) };
                        reg!(dst) = fields[idx as usize];
                    }
                    Op::IsNone { dst, src } => reg!(dst) = Value::Bool(reg!(src) == Value::None),
                    Op::List { dst, base: b, count } => {
                        self.reserve_object(count as usize * size_of::<Value>())?;
                        let start = base + b as usize;
                        let items = self.stack[start..start + count as usize].to_vec();
                        reg!(dst) = self.insert(Obj::List(items));
                    }
                    Op::ListWithCapacity { dst, cap } => {
                        let n = usize::try_from(int!(cap)).map_err(|_| type_error("capacity"))?;
                        reg!(dst) = self.new_list(n)?;
                    }
                    Op::Push { list, src } => self.list_push(reg!(list), reg!(src))?,
                    Op::Pop { dst, list } => {
                        let Obj::List(items) = self.heap.get_mut(handle!(list)) else { return Err(type_error("list")) };
                        reg!(dst) = items.pop().unwrap_or(Value::None);
                    }
                    Op::Index { dst, list, idx } => {
                        let i = int!(idx);
                        let Obj::List(items) = self.heap.get(handle!(list)) else { return Err(type_error("list")) };
                        reg!(dst) = *usize::try_from(i).ok().and_then(|i| items.get(i)).ok_or_else(index_error)?;
                    }
                    Op::SetIndex { list, idx, src } => {
                        let (i, v) = (int!(idx), reg!(src));
                        let Obj::List(items) = self.heap.get_mut(handle!(list)) else { return Err(type_error("list")) };
                        *usize::try_from(i).ok().and_then(|i| items.get_mut(i)).ok_or_else(index_error)? = v;
                    }
                    Op::Len { dst, src } => {
                        let v = reg!(src);
                        let n = match v {
                            Value::Short(s) => s.as_str().len(),
                            Value::Obj(h) => match self.heap.get(h) {
                                Obj::Str(s) => s.len(),
                                Obj::List(items) => items.len(),
                                Obj::Map(entries) => entries.len(),
                                _ => return Err(type_error("list, map, or str")),
                            },
                            _ => return Err(type_error("list, map, or str")),
                        };
                        reg!(dst) = Value::Int(n as i64);
                    }
                    Op::Map { dst } => {
                        self.reserve_object(0)?;
                        reg!(dst) = self.insert(Obj::Map(Vec::new()));
                    }
                    Op::MapSet { map, key, value } => self.map_set(reg!(map), reg!(key), reg!(value))?,
                    Op::MapGet { dst, map, key } => {
                        let key = reg!(key);
                        let Obj::Map(entries) = self.heap.get(handle!(map)) else { return Err(type_error("map")) };
                        reg!(dst) = entries.iter().find(|(k, _)| self.values_eq(k, &key)).map_or(Value::None, |(_, v)| *v);
                    }
                    Op::MapKey { dst, map, idx } | Op::MapValue { dst, map, idx } => {
                        let i = int!(idx);
                        let Obj::Map(entries) = self.heap.get(handle!(map)) else { return Err(type_error("map")) };
                        let (k, v) = *usize::try_from(i).ok().and_then(|i| entries.get(i)).ok_or_else(index_error)?;
                        reg!(dst) = if matches!(op, Op::MapKey { .. }) { k } else { v };
                    }
                    Op::Concat { dst, a, b } => {
                        let (x, y) = (self.str(&reg!(a))?, self.str(&reg!(b))?);
                        let v = match ShortStr::concat(x, y) {
                            Some(short) => Value::Short(short),
                            None => {
                                let len = x.len() + y.len();
                                self.reserve_object(len)?;
                                let mut s = String::with_capacity(len);
                                s.push_str(self.str(&reg!(a))?);
                                s.push_str(self.str(&reg!(b))?);
                                self.insert(Obj::Str(s.into_boxed_str()))
                            }
                        };
                        reg!(dst) = v;
                    }
                    Op::IntToStr { dst, src } => {
                        let i = int!(src);
                        reg!(dst) = self.new_str(itoa::Buffer::new().format(i))?;
                    }
                    Op::Repeat { dst, s, n } => {
                        let count = usize::try_from(int!(n)).map_err(|_| type_error("count"))?;
                        let len = self.str(&reg!(s))?.len().saturating_mul(count);
                        if len <= crate::shared::SHORT_STR_MAX {
                            let text = self.str(&reg!(s))?.repeat(count);
                            reg!(dst) = Value::Short(ShortStr::new(&text).expect("short"));
                        } else {
                            self.reserve_object(len)?;
                            let text = self.str(&reg!(s))?.repeat(count);
                            reg!(dst) = self.insert(Obj::Str(text.into_boxed_str()));
                        }
                    }
                    Op::Global { dst, idx } => reg!(dst) = self.globals[idx as usize],
                    Op::SetGlobal { idx, src } => self.globals[idx as usize] = reg!(src),
                }
            }
        }
    }

    /// Pushes a frame for a call whose arguments are at `caller_base + b`.
    #[inline(always)]
    fn call_frame(&mut self, func: u16, closure: Value, caller_base: usize, b: u16, argc: u8, dst: u16) -> Result<()> {
        let f = &self.program.funcs[func as usize];
        if f.params != argc as u16 {
            return Err(Fail::error(format!("{} expects {} arguments", f.name, f.params)));
        }
        let regs = f.regs as usize;
        let base = caller_base + b as usize;
        self.push_frame(Frame { func, base: base as u32, pc: 0, ret: (caller_base + dst as usize) as u32, closure })?;
        self.ensure_stack(base + regs)?;
        if self.stack.len() < base + regs {
            self.stack.resize(base + regs, Value::Unit);
        }
        Ok(())
    }

    fn map_set(&mut self, map: Value, key: Value, value: Value) -> Result<()> {
        let Value::Obj(h) = map else { return Err(type_error("map")) };
        let Obj::Map(entries) = self.heap.get(h) else { return Err(type_error("map")) };
        if let Some(i) = entries.iter().position(|(k, _)| self.values_eq(k, &key)) {
            let Obj::Map(entries) = self.heap.get_mut(h) else { unreachable!() };
            entries[i].1 = value;
            return Ok(());
        }
        let (len, capacity) = (entries.len(), entries.capacity());
        if len == capacity {
            let grown = (capacity * 2).max(2);
            self.reserve((grown - capacity) * size_of::<(Value, Value)>())?;
            let Obj::Map(entries) = self.heap.get_mut(h) else { unreachable!() };
            entries.reserve_exact(grown - len);
        }
        let Obj::Map(entries) = self.heap.get_mut(h) else { unreachable!() };
        entries.push((key, value));
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Fibers
    // ---------------------------------------------------------------------

    /// Adds a fiber that runs function `func` with `args`.
    pub fn spawn(&mut self, func: u16, args: &[Value]) -> Result<()> {
        let regs = self.program.funcs[func as usize].regs as usize;
        let capacity = regs + STACK_SLACK;
        self.reserve(capacity * size_of::<Value>() + FRAME_SLACK * size_of::<Frame>() + size_of::<Fiber>())?;
        let mut stack = Vec::with_capacity(capacity);
        stack.extend_from_slice(args);
        stack.resize(regs, Value::Unit);
        let mut frames = Vec::with_capacity(FRAME_SLACK);
        frames.push(Frame { func, base: 0, pc: 0, ret: 0, closure: Value::Unit });
        self.fibers.push(Fiber { stack, frames, state: FiberState::Ready });
        Ok(())
    }

    fn swap_fiber(&mut self, i: usize) {
        std::mem::swap(&mut self.stack, &mut self.fibers[i].stack);
        std::mem::swap(&mut self.frames, &mut self.fibers[i].frames);
    }

    /// Runs fiber `i` until it finishes or waits.
    fn run_fiber(&mut self, i: usize) {
        self.swap_fiber(i);
        let state = match self.execute(0) {
            Ok(Exit::Return(v)) => FiberState::Done(Ok(self.to_host(v))),
            Ok(Exit::Await) => {
                let (call, dst) = self.awaiting.take().expect("an awaited call");
                FiberState::Waiting { call, dst }
            }
            Err(e) => FiberState::Done(Err(e)),
        };
        if matches!(state, FiberState::Done(_)) {
            self.frames.clear();
            self.stack.clear();
        }
        self.swap_fiber(i);
        self.fibers[i].state = state;
    }

    /// Runs every fiber to completion and returns their results in spawn order.
    pub async fn run_fibers(&mut self) -> Vec<Result<HostValue>> {
        self.stack_base = stack_address();
        loop {
            let mut ran = false;
            for i in 0..self.fibers.len() {
                if matches!(self.fibers[i].state, FiberState::Ready) {
                    self.run_fiber(i);
                    ran = true;
                }
            }
            if ran {
                continue;
            }
            if self.fibers.iter().all(|f| matches!(f.state, FiberState::Done(_))) {
                break;
            }
            let ready = std::future::poll_fn(|cx| {
                let mut ready = Vec::new();
                for (i, fiber) in self.fibers.iter_mut().enumerate() {
                    if let FiberState::Waiting { call, dst } = &mut fiber.state
                        && let Poll::Ready(resume) = call.as_mut().poll(cx)
                    {
                        ready.push((i, *dst, resume));
                    }
                }
                if ready.is_empty() { Poll::Pending } else { Poll::Ready(ready) }
            })
            .await;
            for (i, dst, resume) in ready {
                self.stack_base = stack_address();
                self.swap_fiber(i);
                let mark = self.roots.len();
                let result = resume(self);
                self.roots.truncate(mark);
                let state = match result {
                    Ok(v) => {
                        self.stack[dst] = v;
                        FiberState::Ready
                    }
                    Err(e) => FiberState::Done(Err(e)),
                };
                self.swap_fiber(i);
                self.fibers[i].state = state;
            }
        }
        let fibers = std::mem::take(&mut self.fibers);
        fibers
            .into_iter()
            .map(|f| match f.state {
                FiberState::Done(r) => r,
                _ => unreachable!(),
            })
            .collect()
    }

    // ---------------------------------------------------------------------
    // Conversions for the test harness
    // ---------------------------------------------------------------------

    pub fn from_arg(&mut self, arg: &Arg) -> Result<Value> {
        match arg {
            Arg::Int(i) => Ok(Value::Int(*i)),
            Arg::Str(s) => self.new_str(s),
        }
    }

    pub fn to_host(&self, v: Value) -> HostValue {
        match v {
            Value::Unit => HostValue::Unit,
            Value::None => HostValue::None,
            Value::Bool(b) => HostValue::Bool(b),
            Value::Int(i) => HostValue::Int(i),
            Value::Float(f) => HostValue::Float(f),
            Value::Short(s) => HostValue::Str(s.as_str().to_string()),
            Value::Tag(t) => HostValue::Other(self.program.variant(t).name.clone()),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => HostValue::Str(s.to_string()),
                _ => HostValue::Other("object".into()),
            },
        }
    }

    /// Calls the function named `name` and converts its result.
    pub fn call_named(&mut self, name: &str, args: &[Arg]) -> Result<HostValue> {
        let func = self.program.func(name).ok_or_else(|| Fail::error(format!("unknown function `{name}`")))?;
        self.stack_base = stack_address();
        let mark = self.roots.len();
        for a in args {
            let v = self.from_arg(a)?;
            self.push_root(v)?;
        }
        let values = self.roots[mark..].to_vec();
        let result = self.call_function(func, Value::Unit, &values);
        self.roots.truncate(mark);
        Ok(self.to_host(result?))
    }

    /// Live object count.
    pub fn live_objects(&self) -> usize {
        self.heap.live_objects
    }
}

/// Returns the registers of every frame in `frames`. Registers above them
/// may hold stale handles, so collections do not trace them. Registers of a
/// frame may hold stale handles from an earlier frame, which collections
/// keep alive until the frame overwrites them.
fn live_registers<'a>(program: &Program, stack: &'a [Value], frames: &[Frame]) -> &'a [Value] {
    let top = frames.last().map_or(0, |f| f.base as usize + program.funcs[f.func as usize].regs as usize);
    &stack[..top.min(stack.len())]
}

#[cold]
fn overflow() -> Fail {
    Fail::error("integer overflow")
}

#[cold]
fn div_error() -> Fail {
    Fail::error("division by zero")
}

#[cold]
fn index_error() -> Fail {
    Fail::error("index out of bounds")
}

/// A future that is pending once and then ready.
#[derive(Default)]
pub struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
