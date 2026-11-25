//! The Arm 3 interpreter, callbacks, fibers, and host API.
//!
//! Values own their objects, so native code holds them across allocations,
//! callbacks, and `.await` without rooting.

use super::heap::Heap;
use super::value::*;
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

const MAX_FRAMES: usize = 100_000;

pub struct Vm {
    pub heap: Heap,
    stack: Vec<Value>,
    frames: Vec<Frame>,
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
            heap: Heap::new(&limits),
            stack: Vec::new(),
            frames: Vec::new(),
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
        vm.heap.reserve((program.globals.len() + program.consts.len()) * size_of::<Value>())?;
        vm.globals = vec![Value::Unit; program.globals.len()];
        for c in &program.consts {
            let v = match c {
                Const::Int(i) => Value::Int(*i),
                Const::Float(f) => Value::Float(*f),
                Const::Str(s) => vm.heap.new_str(s)?,
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
        self.heap.reserve(bytes)
    }

    #[inline]
    pub fn release(&mut self, bytes: usize) {
        self.heap.release(bytes);
    }

    /// Prunes dead containers and collects garbage cycles now.
    pub fn collect(&mut self) {
        self.heap.collect();
    }

    pub fn pause_gc(&mut self, paused: bool) {
        self.heap.policy.paused = paused;
        self.heap.update_next_check();
    }

    #[inline(always)]
    fn ensure_stack(&mut self, top: usize) -> Result<()> {
        if top > self.stack.capacity() {
            self.grow_stack(top)?;
        }
        Ok(())
    }

    #[cold]
    fn grow_stack(&mut self, top: usize) -> Result<()> {
        let capacity = top.max(self.stack.capacity() * 2).max(256);
        self.heap.reserve((capacity - self.stack.capacity()) * size_of::<Value>())?;
        self.stack.reserve_exact(capacity - self.stack.len());
        Ok(())
    }

    #[inline(always)]
    fn push_frame(&mut self, frame: Frame) -> Result<()> {
        if self.frames.len() == self.frames.capacity() {
            self.grow_frames()?;
        }
        self.frames.push(frame);
        Ok(())
    }

    #[cold]
    fn grow_frames(&mut self) -> Result<()> {
        if self.frames.len() > MAX_FRAMES {
            return Err(Fail::StackOverflow);
        }
        let capacity = (self.frames.capacity() * 2).max(16);
        self.heap.reserve((capacity - self.frames.capacity()) * size_of::<Frame>())?;
        self.frames.reserve_exact(capacity - self.frames.len());
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Host API
    // ---------------------------------------------------------------------

    #[inline]
    pub fn arg(&self, args: Args, i: usize) -> &Value {
        &self.stack[args.base + i]
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

    pub fn new_str(&mut self, s: &str) -> Result<Value> {
        self.heap.new_str(s)
    }

    pub fn new_list(&mut self, items: Vec<Value>) -> Result<Value> {
        let bytes = Heap::shared_size::<ListObj>(items.capacity() * size_of::<Value>());
        self.heap.reserve_container(bytes)?;
        Ok(self.heap.insert_list(items, bytes))
    }

    /// Creates an empty list with room for `capacity` items.
    pub fn new_list_with_capacity(&mut self, capacity: usize) -> Result<Value> {
        let bytes = Heap::shared_size::<ListObj>(capacity.saturating_mul(size_of::<Value>()));
        self.heap.reserve_container(bytes)?;
        Ok(self.heap.insert_list(Vec::with_capacity(capacity), bytes))
    }

    /// Appends `v` to `list`.
    pub fn list_push(&mut self, list: &Value, v: Value) -> Result<()> {
        match list {
            Value::List(l) => list_push(&mut self.heap, l, v),
            _ => Err(type_error("list")),
        }
    }

    pub fn new_host(&mut self, obj: HostBox) -> Result<Value> {
        let bytes = Heap::shared_size::<HostBox>(obj.size());
        self.heap.reserve_container(bytes)?;
        Ok(self.heap.insert_host(obj, bytes))
    }

    pub fn call(&mut self, callee: &Value, args: &[Value]) -> Result<Value> {
        match callee {
            Value::Closure(c) => self.call_function(c.data.func, callee.clone(), args),
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
        let base = self.top();
        self.sync_depth += 1;
        let result = self.enter(func, closure, base, regs, args).and_then(|_| self.execute(depth));
        self.sync_depth -= 1;
        self.frames.truncate(depth);
        // A failed call can leave deeper frames' registers behind.
        let end = if result.is_ok() { base + regs } else { self.stack.len() };
        release(&mut self.stack[base..end]);
        match result? {
            Exit::Return(v) => Ok(v),
            Exit::Await => unreachable!("async natives fail inside synchronous calls"),
        }
    }

    /// Pushes a frame and its arguments at `base`.
    fn enter(&mut self, func: u16, closure: Value, base: usize, regs: usize, args: &[Value]) -> Result<()> {
        self.ensure_stack(base + regs)?;
        self.push_frame(Frame { func, base: base as u32, pc: 0, ret: 0, closure })?;
        if self.stack.len() < base + regs {
            self.stack.resize(base + regs, Value::Unit);
        }
        for (slot, arg) in self.stack[base..].iter_mut().zip(args) {
            store(slot, arg.dup());
        }
        Ok(())
    }

    /// Returns the end of the current frame's registers. Registers at and
    /// above it hold no objects.
    fn top(&self) -> usize {
        self.frames.last().map_or(0, |f| f.base as usize + self.program.funcs[f.func as usize].regs as usize)
    }

    fn method_of(&self, receiver: &Value, method: u16) -> Result<u16> {
        let p = &self.program;
        let found = match receiver {
            Value::Tag(tag) => p.enum_method(p.tags[*tag as usize].enum_id, method),
            Value::Variant(o) => p.enum_method(p.tags[o.data.tag as usize].enum_id, method),
            Value::Struct(o) => p.struct_method(o.data.ty, method),
            Value::MutStruct(o) => p.struct_method(o.lock()?.ty, method),
            _ => None,
        };
        found.ok_or_else(|| Fail::error(format!("no method `{}`", p.methods[method as usize])))
    }

    // ---------------------------------------------------------------------
    // Interpreter
    // ---------------------------------------------------------------------

    /// Runs frames until the frame count drops to `entry_depth`.
    fn execute(&mut self, entry_depth: usize) -> Result<Exit> {
        let program = self.program.clone();
        let natives = self.natives.clone();
        'frame: loop {
            let frame = self.frames.last().expect("a frame");
            let (func, base, mut pc) = (frame.func, frame.base as usize, frame.pc as usize);
            let code = &program.funcs[func as usize].code[..];
            macro_rules! reg {
                ($r:expr) => {
                    self.stack[base + $r as usize]
                };
            }
            macro_rules! set {
                ($r:expr, $v:expr) => {{
                    let v = $v;
                    store(&mut reg!($r), v)
                }};
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
            macro_rules! save_pc {
                () => {
                    self.frames.last_mut().expect("a frame").pc = pc as u32
                };
            }
            /// Moves registers `base + b..` out of the argument area.
            macro_rules! take_args {
                ($b:expr, $count:expr) => {{
                    let start = base + $b as usize;
                    self.stack[start..start + $count as usize].iter_mut().map(std::mem::take).collect::<Vec<Value>>()
                }};
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
                    Op::Const { dst, idx } => set!(dst, self.consts[idx as usize].dup()),
                    Op::Int { dst, value } => set!(dst, Value::Int(value as i64)),
                    Op::Unit { dst } => set!(dst, Value::Unit),
                    Op::None { dst } => set!(dst, Value::None),
                    Op::Bool { dst, value } => set!(dst, Value::Bool(value)),
                    Op::Tag { dst, tag } => set!(dst, Value::Tag(tag)),
                    Op::Move { dst, src } => set!(dst, reg!(src).dup()),
                    Op::Add { dst, a, b } => set!(dst, Value::Int(int!(a).checked_add(int!(b)).ok_or_else(overflow)?)),
                    Op::Sub { dst, a, b } => set!(dst, Value::Int(int!(a).checked_sub(int!(b)).ok_or_else(overflow)?)),
                    Op::Mul { dst, a, b } => set!(dst, Value::Int(int!(a).checked_mul(int!(b)).ok_or_else(overflow)?)),
                    Op::Div { dst, a, b } => set!(dst, Value::Int(int!(a).checked_div(int!(b)).ok_or_else(div_error)?)),
                    Op::Rem { dst, a, b } => set!(dst, Value::Int(int!(a).checked_rem(int!(b)).ok_or_else(div_error)?)),
                    Op::FAdd { dst, a, b } => set!(dst, Value::Float(float!(a) + float!(b))),
                    Op::FSub { dst, a, b } => set!(dst, Value::Float(float!(a) - float!(b))),
                    Op::FMul { dst, a, b } => set!(dst, Value::Float(float!(a) * float!(b))),
                    Op::ToFloat { dst, src } => set!(dst, Value::Float(int!(src) as f64)),
                    Op::Lt { dst, a, b } => {
                        let r = match (&reg!(a), &reg!(b)) {
                            (Value::Int(x), Value::Int(y)) => x < y,
                            (Value::Float(x), Value::Float(y)) => x < y,
                            _ => return Err(type_error("numbers")),
                        };
                        set!(dst, Value::Bool(r))
                    }
                    Op::Le { dst, a, b } => {
                        let r = match (&reg!(a), &reg!(b)) {
                            (Value::Int(x), Value::Int(y)) => x <= y,
                            (Value::Float(x), Value::Float(y)) => x <= y,
                            _ => return Err(type_error("numbers")),
                        };
                        set!(dst, Value::Bool(r))
                    }
                    Op::Eq { dst, a, b } => set!(dst, Value::Bool(reg!(a).same(&reg!(b)))),
                    Op::Not { dst, src } => {
                        let r = match reg!(src) {
                            Value::Bool(b) => Value::Bool(!b),
                            _ => return Err(type_error("bool")),
                        };
                        set!(dst, r)
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
                        let Value::Closure(c) = &reg!(callee) else { return Err(type_error("closure")) };
                        let func = c.data.func;
                        let closure = reg!(callee).dup();
                        save_pc!();
                        self.call_frame(func, closure, base, b, argc, dst)?;
                        continue 'frame;
                    }
                    Op::CallMethod { dst, method, base: b, argc } => {
                        let func = self.method_of(&reg!(b), method)?;
                        save_pc!();
                        self.call_frame(func, Value::Unit, base, b, argc, dst)?;
                        continue 'frame;
                    }
                    Op::CallNative { dst, native, base: b, argc } => {
                        save_pc!();
                        let args = Args { base: base + b as usize, len: argc as usize };
                        match &natives[native as usize] {
                            Native::Sync(f) => set!(dst, f(self, args)?),
                            Native::Async(f) => {
                                if self.sync_depth > 0 {
                                    return Err(Fail::error("an async native cannot run inside a synchronous call"));
                                }
                                let call = f(self, args)?;
                                self.awaiting = Some((call, base + dst as usize));
                                return Ok(Exit::Await);
                            }
                        }
                    }
                    Op::Return { src } => {
                        let v = std::mem::take(&mut reg!(src));
                        let frame = self.frames.pop().expect("a frame");
                        if self.frames.len() == entry_depth {
                            return Ok(Exit::Return(v));
                        }
                        let callee_top = frame.base as usize + program.funcs[frame.func as usize].regs as usize;
                        release(&mut self.stack[frame.base as usize..callee_top]);
                        store(&mut self.stack[frame.ret as usize], v);
                        continue 'frame;
                    }
                    Op::Closure { dst, func, base: b, count } => {
                        let bytes = Heap::frozen_size::<ClosureObj>(count as usize);
                        self.heap.reserve_container(bytes)?;
                        let captures = take_args!(b, count).into_boxed_slice();
                        set!(dst, self.heap.insert_closure(func, captures, bytes));
                    }
                    Op::Capture { dst, idx } => {
                        let Value::Closure(c) = &self.frames.last().expect("a frame").closure else { return Err(type_error("closure")) };
                        set!(dst, c.data.captures[idx as usize].dup());
                    }
                    Op::NewCell { dst, src } => {
                        let bytes = Heap::shared_size::<CellObj>(0);
                        self.heap.reserve_container(bytes)?;
                        let v = reg!(src).dup();
                        set!(dst, self.heap.insert_cell(v, bytes));
                    }
                    Op::CellGet { dst, cell } => {
                        let Value::Cell(c) = &reg!(cell) else { return Err(type_error("cell")) };
                        let v = c.lock()?.0.clone();
                        set!(dst, v);
                    }
                    Op::CellSet { cell, src } => {
                        let v = reg!(src).dup();
                        let Value::Cell(c) = &reg!(cell) else { return Err(type_error("cell")) };
                        let old = std::mem::replace(&mut c.lock()?.0, v);
                        drop(old);
                    }
                    Op::Struct { dst, ty, base: b, count } => {
                        let mutable = program.structs[ty as usize].mutable;
                        let bytes = if mutable {
                            Heap::shared_size::<StructObj>(count as usize * size_of::<Value>())
                        } else {
                            Heap::frozen_size::<StructObj>(count as usize)
                        };
                        self.heap.reserve_container(bytes)?;
                        let fields = take_args!(b, count).into_boxed_slice();
                        set!(dst, self.heap.insert_struct(ty, fields, mutable, bytes));
                    }
                    Op::Field { dst, obj, idx } => {
                        let v = match &reg!(obj) {
                            Value::Struct(o) => o.data.fields[idx as usize].dup(),
                            Value::MutStruct(o) => o.lock()?.fields[idx as usize].clone(),
                            _ => return Err(type_error("struct")),
                        };
                        set!(dst, v);
                    }
                    Op::SetField { obj, idx, src } => {
                        let v = reg!(src).dup();
                        let Value::MutStruct(o) = &reg!(obj) else { return Err(type_error("struct")) };
                        let old = std::mem::replace(&mut o.lock()?.fields[idx as usize], v);
                        drop(old);
                    }
                    Op::Variant { dst, tag, base: b, count } => {
                        let bytes = Heap::frozen_size::<VariantObj>(count as usize);
                        self.heap.reserve_container(bytes)?;
                        let fields = take_args!(b, count).into_boxed_slice();
                        set!(dst, self.heap.insert_variant(tag, fields, bytes));
                    }
                    Op::VariantIndex { dst, src } => {
                        let tag = match &reg!(src) {
                            Value::Tag(t) => *t,
                            Value::Variant(o) => o.data.tag,
                            _ => return Err(type_error("variant")),
                        };
                        set!(dst, Value::Int(program.tags[tag as usize].index as i64));
                    }
                    Op::VariantField { dst, src, idx } => {
                        let Value::Variant(o) = &reg!(src) else { return Err(type_error("variant")) };
                        set!(dst, o.data.fields[idx as usize].dup());
                    }
                    Op::IsNone { dst, src } => set!(dst, Value::Bool(matches!(reg!(src), Value::None))),
                    Op::List { dst, base: b, count } => {
                        let bytes = Heap::shared_size::<ListObj>(count as usize * size_of::<Value>());
                        self.heap.reserve_container(bytes)?;
                        let items = take_args!(b, count);
                        set!(dst, self.heap.insert_list(items, bytes));
                    }
                    Op::ListWithCapacity { dst, cap } => {
                        let n = usize::try_from(int!(cap)).map_err(|_| type_error("capacity"))?;
                        set!(dst, self.new_list_with_capacity(n)?);
                    }
                    Op::Push { list, src } => {
                        let v = reg!(src).dup();
                        let Vm { stack, heap, .. } = self;
                        let Value::List(l) = &stack[base + list as usize] else { return Err(type_error("list")) };
                        list_push(heap, l, v)?;
                    }
                    Op::Pop { dst, list } => {
                        let Value::List(l) = &reg!(list) else { return Err(type_error("list")) };
                        let v = l.lock()?.0.pop().unwrap_or(Value::None);
                        set!(dst, v);
                    }
                    Op::Index { dst, list, idx } => {
                        let i = int!(idx);
                        let Value::List(l) = &reg!(list) else { return Err(type_error("list")) };
                        let v = usize::try_from(i).ok().and_then(|i| l.lock().ok()?.0.get(i).cloned()).ok_or_else(index_error)?;
                        set!(dst, v);
                    }
                    Op::SetIndex { list, idx, src } => {
                        let (i, v) = (int!(idx), reg!(src).dup());
                        let Value::List(l) = &reg!(list) else { return Err(type_error("list")) };
                        let mut items = l.lock()?;
                        let slot = usize::try_from(i).ok().and_then(|i| items.0.get_mut(i)).ok_or_else(index_error)?;
                        let old = std::mem::replace(slot, v);
                        drop(items);
                        drop(old);
                    }
                    Op::Len { dst, src } => {
                        let n = match &reg!(src) {
                            Value::Short(s) => s.len(),
                            Value::Str(s) => s.text.len(),
                            Value::List(l) => l.lock()?.0.len(),
                            Value::Map(m) => m.lock()?.0.len(),
                            _ => return Err(type_error("list, map, or str")),
                        };
                        set!(dst, Value::Int(n as i64));
                    }
                    Op::Map { dst } => {
                        let bytes = Heap::shared_size::<MapObj>(0);
                        self.heap.reserve_container(bytes)?;
                        set!(dst, self.heap.insert_map(Vec::new(), bytes));
                    }
                    Op::MapSet { map, key, value } => {
                        let (k, v) = (reg!(key).dup(), reg!(value).dup());
                        let Vm { stack, heap, .. } = self;
                        let Value::Map(m) = &stack[base + map as usize] else { return Err(type_error("map")) };
                        map_set(heap, m, k, v)?;
                    }
                    Op::MapGet { dst, map, key } => {
                        let Value::Map(m) = &reg!(map) else { return Err(type_error("map")) };
                        let key = &reg!(key);
                        let v = m.lock()?.0.iter().find(|(k, _)| k.same(key)).map_or(Value::None, |(_, v)| v.clone());
                        set!(dst, v);
                    }
                    Op::MapKey { dst, map, idx } | Op::MapValue { dst, map, idx } => {
                        let i = int!(idx);
                        let Value::Map(m) = &reg!(map) else { return Err(type_error("map")) };
                        let entries = m.lock()?;
                        let (k, v) = usize::try_from(i).ok().and_then(|i| entries.0.get(i)).ok_or_else(index_error)?;
                        let v = if matches!(op, Op::MapKey { .. }) { k.clone() } else { v.clone() };
                        drop(entries);
                        set!(dst, v);
                    }
                    Op::Concat { dst, a, b } => {
                        let Vm { stack, heap, .. } = self;
                        let v = concat(heap, &stack[base + a as usize], &stack[base + b as usize])?;
                        set!(dst, v);
                    }
                    Op::IntToStr { dst, src } => {
                        let i = int!(src);
                        set!(dst, self.heap.new_str(itoa::Buffer::new().format(i))?);
                    }
                    Op::Repeat { dst, s, n } => {
                        let count = usize::try_from(int!(n)).map_err(|_| type_error("count"))?;
                        let Vm { stack, heap, .. } = self;
                        let text = stack[base + s as usize].str().ok_or_else(|| type_error("str"))?;
                        let len = text.len().saturating_mul(count);
                        let v = if len <= crate::shared::SHORT_STR_MAX {
                            Value::Short(ShortStr::new(&text.repeat(count)).expect("short"))
                        } else {
                            heap.reserve(LongStr::size(len))?;
                            Value::Str(Arc::new(LongStr::new(text.repeat(count).into_boxed_str(), heap.tracker.clone())))
                        };
                        set!(dst, v);
                    }
                    Op::Global { dst, idx } => set!(dst, self.globals[idx as usize].dup()),
                    Op::SetGlobal { idx, src } => self.globals[idx as usize] = reg!(src).dup(),
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
        self.ensure_stack(base + regs)?;
        self.push_frame(Frame { func, base: base as u32, pc: 0, ret: (caller_base + dst as usize) as u32, closure })?;
        if self.stack.len() < base + regs {
            self.stack.resize(base + regs, Value::Unit);
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Fibers
    // ---------------------------------------------------------------------

    /// Adds a fiber that runs function `func` with `args`.
    pub fn spawn(&mut self, func: u16, args: Vec<Value>) -> Result<()> {
        let regs = self.program.funcs[func as usize].regs as usize;
        let capacity = regs.max(256);
        self.heap.reserve(capacity * size_of::<Value>() + 16 * size_of::<Frame>() + size_of::<Fiber>())?;
        let mut stack = Vec::with_capacity(capacity);
        stack.extend(args);
        stack.resize(regs, Value::Unit);
        let mut frames = Vec::with_capacity(16);
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
            Ok(Exit::Return(v)) => FiberState::Done(Ok(self.to_host(&v))),
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
                let state = match resume(self) {
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
            Arg::Str(s) => self.heap.new_str(s),
        }
    }

    pub fn to_host(&self, v: &Value) -> HostValue {
        match v {
            Value::Unit => HostValue::Unit,
            Value::None => HostValue::None,
            Value::Bool(b) => HostValue::Bool(*b),
            Value::Int(i) => HostValue::Int(*i),
            Value::Float(f) => HostValue::Float(*f),
            Value::Short(s) => HostValue::Str(s.as_str().to_string()),
            Value::Str(s) => HostValue::Str(s.text.to_string()),
            Value::Tag(t) => HostValue::Other(self.program.variant(*t).name.clone()),
            _ => HostValue::Other("object".into()),
        }
    }

    /// Calls the function named `name` and converts its result.
    pub fn call_named(&mut self, name: &str, args: &[Arg]) -> Result<HostValue> {
        let func = self.program.func(name).ok_or_else(|| Fail::error(format!("unknown function `{name}`")))?;
        self.stack_base = stack_address();
        let values = args.iter().map(|a| self.from_arg(a)).collect::<Result<Vec<_>>>()?;
        let v = self.call_function(func, Value::Unit, &values)?;
        Ok(self.to_host(&v))
    }
}

/// Drops the objects in `regs`, leaving immediates in place.
#[inline(always)]
fn release(regs: &mut [Value]) {
    for v in regs {
        if !v.is_immediate() {
            *v = Value::Unit;
        }
    }
}

/// Appends `v` to `list`, reserving tracked bytes when the buffer grows.
/// Holding the lock while a collection runs is safe: the collector treats
/// a locked container as a root.
#[inline]
pub fn list_push(heap: &mut Heap, list: &Shared<ListObj>, v: Value) -> Result<()> {
    let mut items = list.lock()?;
    let (len, capacity) = (items.0.len(), items.0.capacity());
    if len == capacity {
        let grown = (capacity * 2).max(4);
        let bytes = (grown - capacity) * size_of::<Value>();
        heap.reserve(bytes)?;
        heap.grew(list.slot, bytes);
        items.0.reserve_exact(grown - len);
    }
    items.0.push(v);
    Ok(())
}

#[inline(never)]
fn concat(heap: &mut Heap, a: &Value, b: &Value) -> Result<Value> {
    if let (Value::Short(x), Value::Short(y)) = (a, b)
        && let Some(short) = ShortStr::concat_short(*x, *y)
    {
        return Ok(Value::Short(short));
    }
    let (Some(x), Some(y)) = (a.str(), b.str()) else { return Err(type_error("str")) };
    if let Some(short) = ShortStr::concat(x, y) {
        return Ok(Value::Short(short));
    }
    heap.reserve(LongStr::size(x.len() + y.len()))?;
    let mut s = String::with_capacity(x.len() + y.len());
    s.push_str(x);
    s.push_str(y);
    Ok(Value::Str(Arc::new(LongStr::new(s.into_boxed_str(), heap.tracker.clone()))))
}

fn map_set(heap: &mut Heap, map: &Shared<MapObj>, key: Value, value: Value) -> Result<()> {
    let mut entries = map.lock()?;
    if let Some(entry) = entries.0.iter_mut().find(|(k, _)| k.same(&key)) {
        let old = std::mem::replace(&mut entry.1, value);
        drop(entries);
        drop(old);
        return Ok(());
    }
    let (len, capacity) = (entries.0.len(), entries.0.capacity());
    if len == capacity {
        let grown = (capacity * 2).max(2);
        let bytes = (grown - capacity) * size_of::<(Value, Value)>();
        heap.reserve(bytes)?;
        heap.grew(map.slot, bytes);
        entries.0.reserve_exact(grown - len);
    }
    entries.0.push((key, value));
    Ok(())
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
