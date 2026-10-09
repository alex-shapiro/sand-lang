//! The bytecode interpreter loop.

use super::compare::{values_eq, values_eq_metered, values_le, values_le_metered, values_lt, values_lt_metered, with_keyref_metered};
use super::display;
use super::meter::{MemoryLimit, code_fuel, ctl_message, loop_fuel, message_ctl};
use super::*;
use crate::LimitKind;

/// Why metered work stopped. Must fit in a register.
#[derive(Clone, Copy)]
enum Stop {
    Interrupted,
    Limit(LimitKind),
}

impl From<Ctl> for Stop {
    fn from(c: Ctl) -> Stop {
        match c {
            Ctl::Limit(k) => Stop::Limit(k),
            _ => Stop::Interrupted,
        }
    }
}

impl From<Stop> for Ctl {
    fn from(s: Stop) -> Ctl {
        match s {
            Stop::Limit(k) => Ctl::Limit(k),
            Stop::Interrupted => Ctl::Interrupted,
        }
    }
}

/// Why a fiber stopped running.
pub enum Exit {
    /// The fiber's outermost function returned.
    Done(Value),
    /// Out of fuel: preempted.
    Preempt,
    /// Waiting on a scheduler event (already registered).
    Blocked,
    Async(BoxFuture),
    Prompt(Capability),
    /// `exit(code)` or a top-level `?` failure.
    Terminate(i32),
    /// `task.yield_now()`.
    YieldNow,
    /// The running task was cancelled.
    Cancelled,
}

#[inline(always)]
fn floor_div(a: i64, b: i64) -> Option<i64> {
    let q = a.checked_div(b)?;
    if (a % b != 0) && ((a < 0) != (b < 0)) { Some(q - 1) } else { Some(q) }
}

#[inline(always)]
fn floor_rem(a: i64, b: i64) -> i64 {
    let r = a.wrapping_rem(b);
    if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r }
}

pub fn int_div(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        return Err("division by zero");
    }
    floor_div(a, b).ok_or("integer overflow")
}

pub fn int_rem(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        return Err("division by zero");
    }
    Ok(floor_rem(a, b))
}

pub fn int_pow(a: i64, b: i64) -> Result<i64, &'static str> {
    if b < 0 {
        return Err("negative exponent");
    }
    if b > u32::MAX as i64 {
        return match a {
            0 | 1 => Ok(a),
            -1 => Ok(if b % 2 == 0 { 1 } else { -1 }),
            _ => Err("integer overflow"),
        };
    }
    a.checked_pow(b as u32).ok_or("integer overflow")
}

/// The payload of an inline variant as field `idx` (kept out of line so the
/// interpreter's struct field fast path stays small).
#[cold]
#[inline(never)]
fn inline_field(v: Value, idx: u16) -> Option<Value> {
    match v.as_inline_variant() {
        Some((_, p)) if idx == 0 => Some(p),
        _ => None,
    }
}

#[cold]
#[inline(never)]
fn index_error(i: i64, len: usize) -> String {
    format!("index out of bounds: index {i}, length {len}")
}

#[cold]
#[inline(never)]
fn uninit_global(prog: &Program, g: u32) -> String {
    format!("global used before initialization: `{}`", prog.global_names[g as usize])
}

/// A range object as (first, last, done): the loop state of an inclusive
/// range loop visiting the same integers.
#[inline(never)]
fn unpack_range(o: &Obj) -> Option<(i64, i64, bool)> {
    let Obj::Range(a, b, inclusive) = *o else { return None };
    Some(match (inclusive, b.checked_sub(1)) {
        (true, _) => (a, b, false),
        (false, Some(last)) => (a, last, false),
        // `a..i64::MIN` is empty.
        (false, None) => (a, b, true),
    })
}

/// The interpreter's fuel counter, kept in a local and written back to the
/// caller's counter when the interpreter returns.
struct FuelGuard<'a> {
    left: i64,
    out: &'a mut i64,
}

impl Drop for FuelGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        *self.out = self.left;
    }
}

pub fn float_rem(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) { r + b } else { r }
}

fn shift_amount(b: i64) -> Result<u32, &'static str> {
    if (0..64).contains(&b) { Ok(b as u32) } else { Err("shift out of range") }
}

impl Vm {
    /// Generic arithmetic on runtime values.
    #[inline(never)]
    pub fn arith(&mut self, op: u8, a: Value, b: Value) -> Result<Value, String> {
        use Value::*;
        Ok(match (op, a, b) {
            (0, Int(x), Int(y)) => Int(x.checked_add(y).ok_or("integer overflow")?),
            (1, Int(x), Int(y)) => Int(x.checked_sub(y).ok_or("integer overflow")?),
            (2, Int(x), Int(y)) => Int(x.checked_mul(y).ok_or("integer overflow")?),
            (3, Int(x), Int(y)) => Int(int_div(x, y)?),
            (4, Int(x), Int(y)) => Int(int_rem(x, y)?),
            (5, Int(x), Int(y)) => Int(int_pow(x, y)?),
            (0, Float(x), Float(y)) => Float(x + y),
            (1, Float(x), Float(y)) => Float(x - y),
            (2, Float(x), Float(y)) => Float(x * y),
            (3, Float(x), Float(y)) => Float(x / y),
            (4, Float(x), Float(y)) => Float(float_rem(x, y)),
            (5, Float(x), Float(y)) => Float(x.powf(y)),
            (0, Dur(x), Dur(y)) => Dur(x.checked_add(y).ok_or("duration overflow")?),
            (1, Dur(x), Dur(y)) => Dur(x.checked_sub(y).ok_or("duration overflow")?),
            (2, Dur(x), Int(y)) | (2, Int(y), Dur(x)) => Dur(x.checked_mul(y).ok_or("duration overflow")?),
            (3, Dur(x), Int(y)) => {
                if y == 0 {
                    return Err("division by zero".into());
                }
                Dur(x.checked_div(y).ok_or("duration overflow")?)
            }
            (3, Dur(x), Dur(y)) => Float(x as f64 / y as f64),
            (0, Obj(_), Obj(_)) => {
                let (oa, ob) = (self.heap.get(a), self.heap.get(b));
                match (oa, ob) {
                    (super::value::Obj::Str(x), super::value::Obj::Str(y)) => {
                        if self.heap.would_exceed(x.len() + y.len()) {
                            return Err("\u{0}memory".into());
                        }
                        self.meter.charge_bytes(x.len() + y.len()).map_err(ctl_message)?;
                        let mut s = String::with_capacity(x.len() + y.len());
                        s.push_str(x);
                        s.push_str(y);
                        self.heap.new_str(s)
                    }
                    (super::value::Obj::Bytes(x), super::value::Obj::Bytes(y)) => {
                        if self.heap.would_exceed(x.len() + y.len()) {
                            return Err("\u{0}memory".into());
                        }
                        self.meter.charge_bytes(x.len() + y.len()).map_err(ctl_message)?;
                        let mut v = x.to_vec();
                        v.extend_from_slice(y);
                        self.heap.new_bytes(v)
                    }
                    (super::value::Obj::List(x), super::value::Obj::List(y)) => {
                        if self.heap.would_exceed((x.len() + y.len()) * 16) {
                            return Err("\u{0}memory".into());
                        }
                        self.meter.charge((x.len() + y.len()) as u64).map_err(ctl_message)?;
                        let mut v = Vec::with_capacity(x.len() + y.len());
                        v.extend_from_slice(x);
                        v.extend_from_slice(y);
                        self.heap.new_list(v)
                    }
                    _ => return Err("unsupported operands for `+`".into()),
                }
            }
            _ => return Err(format!("unsupported operands for arithmetic: {a:?}, {b:?}")),
        })
    }

    /// The trap for an error of the interpreter's own operations, whose
    /// `Result<_, String>` APIs encode limits and interrupts in the message
    /// (`meter::ctl_message`). Never use it for messages that may contain
    /// text chosen by the script: see [`Vm::panic_trap`].
    #[cold]
    #[inline(never)]
    fn trap(&mut self, fiber: &mut Fiber, ip: usize, msg: String) -> Trap {
        match message_ctl(&msg) {
            Some(Ctl::Limit(k)) => self.limit_trap(fiber, ip, k),
            Some(_) => Trap::Interrupted,
            None => self.panic_trap(fiber, ip, msg),
        }
    }

    /// `Op::MakeClosure`: a closure of `proto` (`p`) capturing from the
    /// registers `regs` and the running closure `clo`.
    #[inline(never)]
    fn make_closure(&mut self, p: &Proto, proto: u32, regs: &[Value], clo: u32) -> Value {
        let caps: Box<[Value]> = p
            .captures
            .iter()
            .map(|c| match *c {
                CaptureSrc::Reg(u16::MAX) => Value::Unit,
                CaptureSrc::Reg(r) => regs[r as usize],
                CaptureSrc::Capture(i) => self.heap.fields(Value::Obj(clo))[i as usize],
            })
            .collect();
        self.heap.alloc(Obj::Closure(proto, caps))
    }

    #[cold]
    #[inline(never)]
    fn key_error(&self, k: Value) -> String {
        format!("key not found: {}", self.debug(k).unwrap_or_default())
    }

    /// `Op::TryTop`: a top-level `?` met `Err(e)` (`res`).
    #[cold]
    #[inline(never)]
    fn try_top(&mut self, fiber: &mut Fiber, ip: usize, prog: &Program, res: Value) -> Result<Exit, Trap> {
        let e = self.heap.field(res, 0).unwrap_or(Value::Unit);
        let text = match display::to_display_metered(prog, &self.heap, &self.meter, e) {
            Ok(t) => t,
            Err(c) => return Err(self.ctl_trap(fiber, ip, c)),
        };
        if self.session {
            return Err(self.panic_trap(fiber, ip, text));
        }
        let msg = format!("error: {text}\n");
        match self.write_stream(2, msg.as_bytes()) {
            Ok(()) => {}
            Err(Ctl::Limit(k)) => return Err(Trap::Limit(k)),
            Err(_) => {}
        }
        Ok(Exit::Terminate(1))
    }

    /// `Op::Panic`: a panic with constant message `msg`.
    #[cold]
    #[inline(never)]
    fn const_panic(&mut self, fiber: &mut Fiber, ip: usize, prog: &Program, msg: u32) -> Trap {
        let m = match &prog.consts[msg as usize] {
            Const::Str(s) => s.clone(),
            _ => "panic".into(),
        };
        self.panic_trap(fiber, ip, m)
    }

    /// The trap for an operand of the wrong type (a compiler bug).
    #[cold]
    #[inline(never)]
    fn operand_trap(&mut self, fiber: &mut Fiber, ip: usize, v: Value, want: &str) -> Trap {
        fiber.frames.last_mut().unwrap().ip = ip as u32;
        self.trap(fiber, ip, format!("internal error: expected {want}, found {v:?}"))
    }

    /// The trap for a panic with message `msg`, taken literally: panics
    /// raised by natives (whose limit errors are `Ctl::Limit` values, see
    /// `meter::ctl_from_message`) or with text chosen by the script, which
    /// therefore cannot pass for a limit error.
    #[cold]
    #[inline(never)]
    fn panic_trap(&mut self, fiber: &mut Fiber, ip: usize, msg: String) -> Trap {
        Trap::Panic { message: msg, trace: self.trace_of(fiber, Some(ip as u32)) }
    }

    /// The trap for a failed metered operation.
    #[cold]
    #[inline(never)]
    fn ctl_trap(&mut self, fiber: &mut Fiber, ip: usize, c: Ctl) -> Trap {
        match c {
            Ctl::Panic(m) => self.trap(fiber, ip, m),
            Ctl::Limit(k) => self.limit_trap(fiber, ip, k),
            Ctl::Interrupted => Trap::Interrupted,
            other => self.trap(fiber, ip, ctl_message(other)),
        }
    }

    /// A limit hit by the instruction before `ip`. Memory checks count
    /// garbage that has not been collected yet, so for the memory limit the
    /// frame is rewound to that instruction and the scheduler collects and
    /// runs it once more (the operations that check memory have no side
    /// effects before the check).
    #[cold]
    fn limit_trap(&mut self, fiber: &mut Fiber, ip: usize, k: LimitKind) -> Trap {
        if k == LimitKind::Memory
            && let Some(fr) = fiber.frames.last_mut()
        {
            fr.ip = ip.saturating_sub(1) as u32;
            self.gc_retry = true;
        }
        Trap::Limit(k)
    }

    /// Runs `fiber` until it finishes, blocks, or exhausts `fuel`.
    pub(crate) fn interp(&mut self, fiber: &mut Fiber, fuel_out: &mut i64) -> Result<Exit, Trap> {
        if let Some(msg) = fiber.pending_panic.take() {
            let ip = fiber.frames.last().map(|f| f.ip).unwrap_or(0) as usize;
            return Err(self.panic_trap(fiber, ip, msg));
        }
        let prog = self.prog.clone();
        let natives = self.natives.clone();
        // As few values as possible stay live across the loop, so that the
        // register allocator keeps `code`, `ip`, `regs` and fuel in registers.
        let fr = *fiber.frames.last().unwrap();
        let protos: &[Proto] = &prog.protos;
        let mut code: &[Op] = &protos[fr.proto as usize].code;
        let mut ip = fr.ip as usize;
        let mut base = fr.base as usize;
        // The remaining fuel lives in a local (written back on every exit).
        let mut fuel = FuelGuard { left: *fuel_out, out: fuel_out };
        // The current frame's register window. Re-derived (`reload!`) after
        // anything that may move or resize the stack.
        let mut regs: &mut [Value] = &mut fiber.stack[base..];

        macro_rules! r {
            ($x:expr) => {
                regs[$x as usize]
            };
        }
        macro_rules! reload {
            () => {
                regs = &mut fiber.stack[base..]
            };
        }
        macro_rules! save {
            () => {
                fiber.frames.last_mut().unwrap().ip = ip as u32;
            };
        }
        macro_rules! fail {
            ($msg:expr) => {{
                save!();
                return Err(self.trap(fiber, ip, String::from($msg)));
            }};
        }
        // A panic whose message may hold text chosen by the script.
        macro_rules! fail_literal {
            ($msg:expr) => {{
                save!();
                return Err(self.panic_trap(fiber, ip, String::from($msg)));
            }};
        }
        macro_rules! fail_ctl {
            ($c:expr) => {{
                save!();
                return Err(self.ctl_trap(fiber, ip, $c));
            }};
        }
        macro_rules! int {
            ($x:expr) => {
                match r!($x) {
                    Value::Int(i) => i,
                    other => return Err(self.operand_trap(fiber, ip, other, "int")),
                }
            };
        }
        macro_rules! float {
            ($x:expr) => {
                match r!($x) {
                    Value::Float(f) => f,
                    other => return Err(self.operand_trap(fiber, ip, other, "float")),
                }
            };
        }
        macro_rules! jump {
            ($off:expr) => {
                ip = (ip as isize + $off as isize) as usize
            };
        }
        // A safe point: consumes fuel, collects garbage, or preempts.
        // The fiber resumes at `$resume`.
        macro_rules! safe_point {
            ($resume:expr, $cost:expr) => {
                fuel.left -= $cost;
                if fuel.left <= 0 || self.heap.wants_gc() {
                    if let Some(exit) = self.safe_point(fiber, fuel.left, $resume) {
                        return exit;
                    }
                    reload!();
                }
            };
        }
        // A safe point of a call instruction: on preemption the call is
        // re-executed when the fiber resumes.
        macro_rules! tick_call {
            () => {
                safe_point!(ip - 1, 1)
            };
        }
        // Calls native `n` with `argc` arguments at absolute slot `start`,
        // writing the result to absolute slot `slot`.
        macro_rules! native_call {
            ($n:expr, $start:expr, $argc:expr, $slot:expr) => {{
                self.meter.sync_bytecode(fuel.left);
                match self.call_native(&natives, fiber, $n, $start, $argc, $slot, ip) {
                    Ok(None) => reload!(),
                    Ok(Some(exit)) => return Ok(exit),
                    Err(t) => return Err(t),
                }
            }};
        }
        // The heap handle of the running closure.
        macro_rules! closure {
            () => {
                fiber.frames.last().map_or(NO_CLOSURE, |f| f.closure)
            };
        }
        // Enters a new frame for a script function.
        macro_rules! enter {
            ($proto:expr, $new_base:expr, $ret:expr, $closure:expr) => {{
                if fiber.frames.len() >= self.host.limits.max_call_depth {
                    fail!("stack overflow");
                }
                save!();
                let p = &protos[$proto as usize];
                let nb = $new_base;
                let need = nb + p.nregs as usize;
                if fiber.stack.len() < need
                    && let Err(m) = fiber.grow_stack(need, &mut self.heap)
                {
                    fail!(m);
                }
                if fiber.frames.len() == fiber.frames.capacity()
                    && let Err(m) = fiber.grow_frames(&mut self.heap)
                {
                    fail!(m);
                }
                fiber.frames.push(Frame { proto: $proto, ip: 0, base: nb as u32, ret: $ret as u32, closure: $closure });
                code = &p.code;
                // Long functions cost more (`meter::code_fuel`).
                if code.len() >= 64 {
                    fuel.left -= code_fuel(code.len());
                }
                ip = 0;
                base = nb;
                reload!();
            }};
        }
        // Returns to the caller. The callee's registers are left in place:
        // they are dead, and the GC only scans each fiber's live window.
        macro_rules! do_return {
            ($v:expr) => {{
                let v = $v;
                let ret = fiber.frames.pop().map_or(0, |f| f.ret as usize);
                match fiber.frames.last() {
                    None => {
                        fiber.stack.truncate(0);
                        return Ok(Exit::Done(v));
                    }
                    Some(caller) => {
                        code = &protos[caller.proto as usize].code;
                        ip = caller.ip as usize;
                        base = caller.base as usize;
                        fiber.stack[ret] = v;
                        reload!();
                    }
                }
            }};
        }

        loop {
            let op = code[ip];
            ip += 1;
            match op {
                Op::Nop => {}
                Op::Move { dst, src } => r!(dst) = r!(src),
                Op::LoadUnit { dst } => r!(dst) = Value::Unit,
                Op::LoadBool { dst, v } => r!(dst) = Value::Bool(v),
                Op::LoadInt { dst, v } => r!(dst) = Value::Int(v as i64),
                Op::LoadConst { dst, k } => r!(dst) = self.consts[k as usize],
                Op::LoadTag { dst, tag } => r!(dst) = Value::Tag(tag),
                Op::LoadGlobal { dst, g } => {
                    let v = self.globals[g as usize];
                    if let Value::Uninit = v {
                        fail!(uninit_global(&prog, g));
                    }
                    r!(dst) = v;
                }
                Op::StoreGlobal { g, src } => self.globals[g as usize] = r!(src),
                Op::HoistGlobal { dst, g, off } => {
                    let v = self.globals[g as usize];
                    r!(dst) = v;
                    if let Value::Uninit = v {
                        jump!(off);
                    }
                }
                Op::LoadCapture { dst, idx } => {
                    r!(dst) = self.heap.fields(Value::Obj(closure!()))[idx as usize];
                }
                Op::LoadCaptureCell { dst, idx } => {
                    let cell = self.heap.fields(Value::Obj(closure!()))[idx as usize];
                    r!(dst) = match self.heap.get(cell) {
                        Obj::Cell(v) => *v,
                        _ => Value::Unit,
                    };
                }
                Op::StoreCaptureCell { idx, src } => {
                    let cell = self.heap.fields(Value::Obj(closure!()))[idx as usize];
                    let v = r!(src);
                    if let Obj::Cell(c) = self.heap.get_mut_storing(cell, &[v]) {
                        *c = v;
                    }
                }
                Op::NewCell { dst, src } => {
                    let v = r!(src);
                    r!(dst) = self.heap.alloc(Obj::Cell(v));
                }
                Op::LoadCell { dst, cell } => {
                    let c = r!(cell);
                    r!(dst) = match self.heap.get(c) {
                        Obj::Cell(v) => *v,
                        _ => Value::Unit,
                    };
                }
                Op::StoreCell { cell, src } => {
                    let c = r!(cell);
                    let v = r!(src);
                    if let Value::Obj(_) = c
                        && let Obj::Cell(x) = self.heap.get_mut_storing(c, &[v])
                    {
                        *x = v;
                    }
                }
                Op::MakeClosure { dst, proto } => {
                    let clo = closure!();
                    let p = &protos[proto as usize];
                    fuel.left -= code_fuel(p.captures.len());
                    r!(dst) = self.make_closure(p, proto, regs, clo);
                }

                // ---------------- integer arithmetic
                Op::AddI { dst, a, b } => match int!(a).checked_add(int!(b)) {
                    Some(v) => r!(dst) = Value::Int(v),
                    None => fail!("integer overflow"),
                },
                Op::SubI { dst, a, b } => match int!(a).checked_sub(int!(b)) {
                    Some(v) => r!(dst) = Value::Int(v),
                    None => fail!("integer overflow"),
                },
                Op::MulI { dst, a, b } => match int!(a).checked_mul(int!(b)) {
                    Some(v) => r!(dst) = Value::Int(v),
                    None => fail!("integer overflow"),
                },
                Op::DivI { dst, a, b } => match int_div(int!(a), int!(b)) {
                    Ok(v) => r!(dst) = Value::Int(v),
                    Err(m) => fail!(m),
                },
                Op::RemI { dst, a, b } => match int_rem(int!(a), int!(b)) {
                    Ok(v) => r!(dst) = Value::Int(v),
                    Err(m) => fail!(m),
                },
                Op::PowI { dst, a, b } => match int_pow(int!(a), int!(b)) {
                    Ok(v) => r!(dst) = Value::Int(v),
                    Err(m) => fail!(m),
                },
                Op::AddIK { dst, a, k } => match int!(a).checked_add(k as i64) {
                    Some(v) => r!(dst) = Value::Int(v),
                    None => fail!("integer overflow"),
                },
                Op::NegI { dst, a } => match int!(a).checked_neg() {
                    Some(v) => r!(dst) = Value::Int(v),
                    None => fail!("integer overflow"),
                },
                Op::BitAnd { dst, a, b } => r!(dst) = Value::Int(int!(a) & int!(b)),
                Op::BitOr { dst, a, b } => r!(dst) = Value::Int(int!(a) | int!(b)),
                Op::BitXor { dst, a, b } => r!(dst) = Value::Int(int!(a) ^ int!(b)),
                Op::Shl { dst, a, b } => match shift_amount(int!(b)) {
                    Ok(s) => r!(dst) = Value::Int(int!(a).wrapping_shl(s)),
                    Err(m) => fail!(m),
                },
                Op::Shr { dst, a, b } => match shift_amount(int!(b)) {
                    Ok(s) => r!(dst) = Value::Int(int!(a) >> s),
                    Err(m) => fail!(m),
                },
                Op::BitNot { dst, a } => r!(dst) = Value::Int(!int!(a)),

                // ---------------- float arithmetic
                Op::AddF { dst, a, b } => r!(dst) = Value::Float(float!(a) + float!(b)),
                Op::SubF { dst, a, b } => r!(dst) = Value::Float(float!(a) - float!(b)),
                Op::MulF { dst, a, b } => r!(dst) = Value::Float(float!(a) * float!(b)),
                Op::DivF { dst, a, b } => r!(dst) = Value::Float(float!(a) / float!(b)),
                Op::RemF { dst, a, b } => r!(dst) = Value::Float(float_rem(float!(a), float!(b))),
                Op::PowF { dst, a, b } => r!(dst) = Value::Float(float!(a).powf(float!(b))),
                Op::NegF { dst, a } => r!(dst) = Value::Float(-float!(a)),

                // ---------------- generic arithmetic
                Op::Add { dst, a, b }
                | Op::Sub { dst, a, b }
                | Op::Mul { dst, a, b }
                | Op::Div { dst, a, b }
                | Op::Rem { dst, a, b }
                | Op::Pow { dst, a, b } => {
                    let code_op = match op {
                        Op::Add { .. } => 0,
                        Op::Sub { .. } => 1,
                        Op::Mul { .. } => 2,
                        Op::Div { .. } => 3,
                        Op::Rem { .. } => 4,
                        _ => 5,
                    };
                    let (x, y) = (r!(a), r!(b));
                    match self.arith(code_op, x, y) {
                        Ok(v) => r!(dst) = v,
                        Err(m) => fail!(m),
                    }
                }
                Op::Neg { dst, a } => {
                    r!(dst) = match r!(a) {
                        Value::Int(i) => match i.checked_neg() {
                            Some(v) => Value::Int(v),
                            None => fail!("integer overflow"),
                        },
                        Value::Float(f) => Value::Float(-f),
                        Value::Dur(d) => match d.checked_neg() {
                            Some(v) => Value::Dur(v),
                            None => fail!("duration overflow"),
                        },
                        other => fail!(format!("cannot negate {other:?}")),
                    }
                }
                Op::Not { dst, a } => r!(dst) = Value::Bool(!r!(a).as_bool()),

                // ---------------- comparisons
                Op::EqI { dst, a, b } => r!(dst) = Value::Bool(int!(a) == int!(b)),
                Op::NeI { dst, a, b } => r!(dst) = Value::Bool(int!(a) != int!(b)),
                Op::LtI { dst, a, b } => r!(dst) = Value::Bool(int!(a) < int!(b)),
                Op::LeI { dst, a, b } => r!(dst) = Value::Bool(int!(a) <= int!(b)),
                Op::EqF { dst, a, b } => r!(dst) = Value::Bool(float!(a) == float!(b)),
                Op::NeF { dst, a, b } => r!(dst) = Value::Bool(float!(a) != float!(b)),
                Op::LtF { dst, a, b } => r!(dst) = Value::Bool(float!(a) < float!(b)),
                Op::LeF { dst, a, b } => r!(dst) = Value::Bool(float!(a) <= float!(b)),
                // Composite values are compared with metering: deep
                // comparisons of large values cost fuel and can be interrupted.
                // (Like natives, heavy operations charge the meter, not the
                // interpreter's own fuel, which stays in a register.)
                Op::Eq { dst, a, b } | Op::Ne { dst, a, b } | Op::Lt { dst, a, b } | Op::Le { dst, a, b } => {
                    match self.op_compare(&prog, op, r!(a), r!(b)) {
                        Ok(v) => r!(dst) = Value::Bool(v),
                        Err(c) => fail_ctl!(c),
                    }
                }

                // ---------------- control flow
                Op::Jmp { off } => jump!(off),
                Op::Loop { off } => {
                    jump!(off);
                    // Long loops cost more (`meter::loop_fuel`).
                    safe_point!(ip, loop_fuel(off));
                }
                Op::JmpIf { cond, off } => {
                    if r!(cond).as_bool() {
                        jump!(off)
                    }
                }
                Op::JmpIfNot { cond, off } => {
                    if !r!(cond).as_bool() {
                        jump!(off)
                    }
                }
                Op::JNltI { a, b, off } => {
                    if int!(a) >= int!(b) {
                        jump!(off)
                    }
                }
                Op::JNleI { a, b, off } => {
                    if int!(a) > int!(b) {
                        jump!(off)
                    }
                }
                Op::JNeqI { a, b, off } => {
                    if int!(a) != int!(b) {
                        jump!(off)
                    }
                }
                Op::JNneI { a, b, off } => {
                    if int!(a) == int!(b) {
                        jump!(off)
                    }
                }
                Op::JNltF { a, b, off } => {
                    if float!(a).partial_cmp(&float!(b)) != Some(std::cmp::Ordering::Less) {
                        jump!(off)
                    }
                }
                Op::JNleF { a, b, off } => {
                    if !matches!(float!(a).partial_cmp(&float!(b)), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)) {
                        jump!(off)
                    }
                }
                Op::JNotSome { src, dst, off } => match self.heap.payload_if(r!(src), prog.ids.some) {
                    Some(p) => r!(dst) = p,
                    None => jump!(off),
                },
                Op::JNotVariant { src, variant, off } => {
                    if self.heap.variant_id(r!(src)) != variant as u32 {
                        jump!(off)
                    }
                }
                Op::IsVariant { dst, src, variant } => {
                    r!(dst) = Value::Bool(self.heap.variant_id(r!(src)) == variant as u32);
                }

                // ---------------- calls
                // (`tick_call!` reloads `regs` after a collection; `enter!` reloads again.)
                #[allow(unused_assignments)]
                Op::CallDirect { base: b, proto } => {
                    tick_call!();
                    let nb = base + b as usize;
                    enter!(proto, nb, nb, NO_CLOSURE);
                }
                Op::Call { base: b, argc, dst } => {
                    tick_call!();
                    let callee = r!(b);
                    match self.heap.get(callee) {
                        Obj::Closure(p, _) => {
                            let p = *p;
                            let Value::Obj(h) = callee else { unreachable!() };
                            let nb = base + b as usize + 1;
                            enter!(p, nb, base + dst as usize, h);
                        }
                        Obj::NativeFn(n) => {
                            let n = *n;
                            let start = base + b as usize + 1;
                            let slot = base + dst as usize;
                            native_call!(n, start, argc as usize, slot);
                        }
                        Obj::IterFn(_) => r!(dst) = self.iter_step(callee),
                        _ => fail!("internal error: call of a non-function"),
                    }
                }
                Op::CallR { callee: c, base: b, dst, argc } => {
                    tick_call!();
                    let callee = r!(c);
                    match self.heap.get(callee) {
                        Obj::Closure(p, _) => {
                            let p = *p;
                            let Value::Obj(h) = callee else { unreachable!() };
                            let nb = base + b as usize;
                            enter!(p, nb, base + dst as usize, h);
                        }
                        Obj::IterFn(_) => {
                            let next = self.iter_next(callee);
                            r!(dst) = self.option(next);
                            if let Some(&Op::JNotSome { src, dst: d, off }) = code.get(ip)
                                && src == dst
                            {
                                ip += 1;
                                match next {
                                    Some(x) => r!(d) = x,
                                    None => jump!(off),
                                }
                            }
                        }
                        Obj::NativeFn(n) => {
                            let n = *n;
                            native_call!(n, base + b as usize, argc as usize, base + dst as usize);
                        }
                        _ => fail!("internal error: call of a non-function"),
                    }
                }
                // (`tick_call!` reloads `regs` after a collection; `enter!` reloads again.)
                #[allow(unused_assignments)]
                Op::CallDirectTo { base: b, dst, proto } => {
                    tick_call!();
                    let nb = base + b as usize;
                    enter!(proto as u32, nb, base + dst as usize, NO_CLOSURE);
                }
                Op::CallNative { base: b, native } => {
                    let start = base + b as usize;
                    let argc = prog.native_argc.get(native as usize).copied().unwrap_or(0) as usize;
                    native_call!(native, start, argc, start);
                }
                Op::CallNativeTo { base: b, dst, native } => {
                    let start = base + b as usize;
                    let argc = prog.native_argc.get(native as usize).copied().unwrap_or(0) as usize;
                    native_call!(native as u32, start, argc, base + dst as usize);
                }
                // (`tick_call!` reloads `regs` after a collection; the stack is reloaded again below.)
                #[allow(unused_assignments)]
                Op::TailCallDirect { base: b, proto } => {
                    tick_call!();
                    let p = &protos[proto as usize];
                    let n = p.nparams as usize;
                    let src = b as usize;
                    // Grow first: the call is retried after a memory limit error.
                    let need = base + p.nregs as usize;
                    if fiber.stack.len() < need
                        && let Err(m) = fiber.grow_stack(need, &mut self.heap)
                    {
                        fail!(m);
                    }
                    reload!();
                    regs.copy_within(src..src + n, 0);
                    let top = fiber.frames.last_mut().unwrap();
                    top.proto = proto;
                    top.closure = NO_CLOSURE;
                    code = &p.code;
                    fuel.left -= code_fuel(code.len());
                    ip = 0;
                }
                Op::TailCall { base: b, argc } => {
                    tick_call!();
                    let callee = r!(b);
                    match self.heap.get(callee) {
                        Obj::Closure(p, _) => {
                            let proto = *p;
                            let pr = &protos[proto as usize];
                            let n = argc as usize;
                            let src = b as usize + 1;
                            // Grow first: the call is retried after a memory limit error.
                            let need = base + pr.nregs as usize;
                            if fiber.stack.len() < need
                                && let Err(m) = fiber.grow_stack(need, &mut self.heap)
                            {
                                fail!(m);
                            }
                            reload!();
                            regs.copy_within(src..src + n, 0);
                            let Value::Obj(h) = callee else { unreachable!() };
                            let top = fiber.frames.last_mut().unwrap();
                            top.proto = proto;
                            top.closure = h;
                            code = &pr.code;
                            fuel.left -= code_fuel(code.len());
                            ip = 0;
                        }
                        Obj::NativeFn(n) => {
                            // Not a real tail call: the compiler emits `Ret base` next.
                            let n = *n;
                            let start = base + b as usize + 1;
                            let slot = base + b as usize;
                            native_call!(n, start, argc as usize, slot);
                        }
                        Obj::IterFn(_) => r!(b) = self.iter_step(callee),
                        _ => fail!("internal error: call of a non-function"),
                    }
                }
                Op::Ret { src } => {
                    let v = r!(src);
                    do_return!(v);
                }
                Op::RetUnit => do_return!(Value::Unit),

                // ---------------- data structures
                Op::NewList { dst, start, count } => {
                    let s = start as usize;
                    let items = regs[s..s + count as usize].to_vec();
                    r!(dst) = self.heap.new_list(items);
                }
                Op::NewTuple { dst, start, count } => {
                    let s = start as usize;
                    let items: Box<[Value]> = regs[s..s + count as usize].into();
                    r!(dst) = self.heap.alloc(Obj::Tuple(items));
                }
                Op::NewStruct { dst, start, def } => {
                    let n = prog.types[def as usize].fields.len();
                    let s = start as usize;
                    let items: Box<[Value]> = regs[s..s + n].into();
                    r!(dst) = self.heap.alloc(Obj::Struct(def as u32, items));
                }
                Op::NewVariant { dst, start, variant } => {
                    let n = prog.variants[variant as usize].arity;
                    let s = start as usize;
                    r!(dst) = self.heap.new_variant(variant as u32, &regs[s..s + n]);
                }
                Op::NewMap { dst, start, count } => {
                    let m = self.heap.alloc(Obj::Map(Box::default()));
                    let s = start as usize;
                    for i in 0..count as usize {
                        let k = regs[s + 2 * i];
                        let v = regs[s + 2 * i + 1];
                        if let Err(e) = self.map_insert(m, k, v) {
                            fail!(e);
                        }
                    }
                    r!(dst) = m;
                }
                Op::NewRange { dst, a, b } => {
                    let (x, y) = (int!(a), int!(b));
                    r!(dst) = self.heap.alloc(Obj::Range(x, y, false));
                }
                Op::NewRangeIncl { dst, a, b } => {
                    let (x, y) = (int!(a), int!(b));
                    r!(dst) = self.heap.alloc(Obj::Range(x, y, true));
                }
                Op::GetField { dst, obj, idx } => {
                    let o = r!(obj);
                    r!(dst) = match self.heap.fields(o).get(idx as usize) {
                        Some(v) => *v,
                        // Inline variants have no field slice.
                        None => match inline_field(o, idx) {
                            Some(v) => v,
                            None => fail!(format!("internal error: field {idx} of {o:?} {:?}", self.heap.get(o))),
                        },
                    };
                }
                Op::SetField { obj, idx, src } => {
                    let o = r!(obj);
                    let v = r!(src);
                    match self.heap.get_mut_storing(o, &[v]) {
                        Obj::Struct(_, f) => f[idx as usize] = v,
                        _ => fail!("internal error: set field on non-struct"),
                    }
                }
                Op::Index { dst, obj, idx } => {
                    let o = r!(obj);
                    let i = int!(idx);
                    let l = self.heap.list(o);
                    if i < 0 || i as usize >= l.len() {
                        fail!(index_error(i, l.len()));
                    }
                    r!(dst) = l[i as usize];
                }
                Op::IndexMap { dst, obj, key } => {
                    let m = r!(obj);
                    let k = r!(key);
                    match self.map_get(m, k) {
                        Ok(Some(v)) => r!(dst) = v,
                        Ok(None) => fail!(self.key_error(k)),
                        Err(e) => fail!(e),
                    }
                }
                Op::SetIndex { obj, idx, src } => {
                    let o = r!(obj);
                    let i = int!(idx);
                    let v = r!(src);
                    let Some(l) = self.heap.list_mut_storing(o, i as usize, v) else { fail!("internal error: set index on non-list") };
                    if i < 0 || i as usize >= l.len() {
                        let n = l.len();
                        fail!(index_error(i, n));
                    }
                    l[i as usize] = v;
                }
                Op::SetIndexMap { obj, key, src } => {
                    let (m, k, v) = (r!(obj), r!(key), r!(src));
                    if let Err(e) = self.map_insert(m, k, v) {
                        fail!(e);
                    }
                }
                Op::Slice { dst, base: b, flags } => {
                    let o = r!(b);
                    let start = if flags & 1 != 0 { Some(int!(b + 1)) } else { None };
                    let end = if flags & 2 != 0 { Some(int!(b + 2)) } else { None };
                    match self.slice(o, start, end, flags & 4 != 0, flags) {
                        Ok(v) => r!(dst) = v,
                        Err(e) => fail!(e),
                    }
                }
                Op::ListPush { list, src } => {
                    let (l, v) = (r!(list), r!(src));
                    self.heap.list_push(l, v);
                }
                Op::CheckRepeat { src } => {
                    let n = int!(src);
                    if n < 0 {
                        fail!(format!("negative repeat count: {n}"));
                    }
                    if self.heap.would_exceed((n as usize).saturating_mul(16)) {
                        save!();
                        return Err(Trap::Limit(LimitKind::Memory));
                    }
                }
                Op::Len { dst, src } => {
                    let o = r!(src);
                    let n = match self.heap.get(o) {
                        Obj::List(l) => l.len(),
                        Obj::Str(s) => s.len(),
                        Obj::Bytes(b) => b.len(),
                        Obj::Map(m) => m.live,
                        Obj::Set(s) => s.live,
                        _ => 0,
                    };
                    r!(dst) = Value::Int(n as i64);
                }
                Op::IntToFloat { dst, src } => r!(dst) = Value::Float(int!(src) as f64),
                Op::AppendStr { local, src } => {
                    let add = r!(src);
                    if let Err(c) = self.meter.charge_bytes(self.heap.append_copies(r!(local), add)) {
                        fail_ctl!(c);
                    }
                    match self.heap.append_str(r!(local), add) {
                        Ok(v) => r!(local) = v,
                        Err(k) => fail_ctl!(Ctl::Limit(k)),
                    }
                }
                Op::FreezeStr { reg } => self.heap.freeze_str(r!(reg)),
                Op::JStrClass { src, class, off } => match self.str_class(r!(src), class & 0x7f) {
                    Ok(b) => {
                        if b == (class & 0x80 != 0) {
                            jump!(off)
                        }
                    }
                    Err(c) => fail_ctl!(c.into()),
                },
                Op::StrClass { dst, src, class } => match self.str_class(r!(src), class) {
                    Ok(b) => r!(dst) = Value::Bool(b),
                    Err(c) => fail_ctl!(c.into()),
                },
                Op::ForList { elem, base: b, off } => {
                    let i = int!(b + 1);
                    let n = int!(b + 2);
                    let l = self.heap.list(r!(b));
                    if i < n && (i as usize) < l.len() {
                        let v = l[i as usize];
                        r!(elem) = v;
                        r!(b + 1) = Value::Int(i + 1);
                    } else {
                        jump!(off);
                    }
                }
                Op::ForListNext { elem, base: b } => {
                    let i = int!(b + 1);
                    let n = int!(b + 2);
                    let l = self.heap.list(r!(b));
                    if i < n && (i as usize) < l.len() {
                        let v = l[i as usize];
                        r!(elem) = v;
                        r!(b + 1) = Value::Int(i + 1);
                        r!(b + 3) = Value::Bool(true);
                    } else {
                        r!(b + 3) = Value::Bool(false);
                    }
                }
                Op::ForRange { var, base: b, off } => {
                    let i = int!(b);
                    if i < int!(b + 1) {
                        r!(var) = Value::Int(i);
                        r!(b) = Value::Int(i + 1);
                    } else {
                        jump!(off);
                    }
                }
                Op::ForRangeNext { var, base: b } => {
                    let i = int!(b);
                    if i < int!(b + 1) {
                        r!(var) = Value::Int(i);
                        r!(b) = Value::Int(i + 1);
                        r!(b + 3) = Value::Bool(true);
                    } else {
                        r!(b + 3) = Value::Bool(false);
                    }
                }
                Op::ForRangeIncl { var, base: b, off } => {
                    let i = int!(b);
                    if !r!(b + 2).as_bool() && i <= int!(b + 1) {
                        r!(var) = Value::Int(i);
                        if i == int!(b + 1) {
                            r!(b + 2) = Value::Bool(true);
                        } else {
                            r!(b) = Value::Int(i + 1);
                        }
                    } else {
                        jump!(off);
                    }
                }
                Op::ForRangeInclNext { var, base: b } => {
                    let i = int!(b);
                    if !r!(b + 2).as_bool() && i <= int!(b + 1) {
                        r!(var) = Value::Int(i);
                        if i == int!(b + 1) {
                            r!(b + 2) = Value::Bool(true);
                        } else {
                            r!(b) = Value::Int(i + 1);
                        }
                        r!(b + 3) = Value::Bool(true);
                    } else {
                        r!(b + 3) = Value::Bool(false);
                    }
                }
                Op::UnpackRange { base: b, src } => match unpack_range(self.heap.get(r!(src))) {
                    Some((first, last, done)) => {
                        r!(b) = Value::Int(first);
                        r!(b + 1) = Value::Int(last);
                        r!(b + 2) = Value::Bool(done);
                    }
                    None => fail!("internal error: not a range"),
                },
                Op::IterSnapshot { dst, src } => {
                    let o = r!(src);
                    match self.snapshot(o) {
                        Ok(v) => r!(dst) = v,
                        Err(e) => fail!(e),
                    }
                }
                Op::JListLen { src, n, off } => {
                    if self.heap.list(r!(src)).len() != n as usize {
                        jump!(off)
                    }
                }
                Op::JListMinLen { src, n, off } => {
                    if self.heap.list(r!(src)).len() < n as usize {
                        jump!(off)
                    }
                }
                Op::ListLenEq { dst, src, n } => r!(dst) = Value::Bool(self.heap.list(r!(src)).len() == n as usize),
                Op::ListLenGe { dst, src, n } => r!(dst) = Value::Bool(self.heap.list(r!(src)).len() >= n as usize),
                Op::ListElem { dst, list, idx } => {
                    let l = self.heap.list(r!(list));
                    let i = if idx < 0 { l.len() as isize + idx as isize } else { idx as isize };
                    r!(dst) = l.get(i as usize).copied().unwrap_or(Value::Unit);
                }
                Op::ListRest { dst, list, before, after } => {
                    let l = self.heap.list(r!(list));
                    let end = l.len().saturating_sub(after as usize);
                    let items = l.get(before as usize..end.max(before as usize)).map(|s| s.to_vec()).unwrap_or_default();
                    if let Err(c) = self.meter.charge(items.len() as u64) {
                        fail_ctl!(c);
                    }
                    r!(dst) = self.heap.new_list(items);
                }

                // ---------------- strings
                Op::ToStr { dst, src } => {
                    let v = r!(src);
                    if let Value::Obj(_) = v
                        && let Obj::Str(_) = self.heap.get(v)
                    {
                        r!(dst) = v;
                        continue;
                    }
                    match self.op_display(&prog, v, None, false) {
                        Ok(v) => r!(dst) = v,
                        Err(c) => fail_ctl!(c),
                    }
                }
                Op::Debug { dst, src } => match self.op_display(&prog, r!(src), None, true) {
                    Ok(v) => r!(dst) = v,
                    Err(c) => fail_ctl!(c),
                },
                Op::Format { dst, src, spec } => match self.op_display(&prog, r!(src), Some(spec), false) {
                    Ok(v) => r!(dst) = v,
                    Err(c) => fail_ctl!(c),
                },
                Op::Concat { dst, start, count } => {
                    let s = start as usize;
                    match self.op_concat(&regs[s..s + count as usize]) {
                        Ok(v) => r!(dst) = v,
                        Err(c) => fail_ctl!(c),
                    }
                }

                // ---------------- misc
                Op::Spawn { dst, closure } => {
                    let c = r!(closure);
                    match self.spawn_task(c) {
                        Ok(t) => r!(dst) = t,
                        Err(Ctl::Panic(m)) => fail_literal!(m),
                        Err(c) => fail_ctl!(c),
                    }
                }
                Op::TryTop { src } => {
                    save!();
                    let e = r!(src);
                    return self.try_top(fiber, ip, &prog, e);
                }
                Op::Panic { msg } => {
                    save!();
                    return Err(self.const_panic(fiber, ip, &prog, msg));
                }
            }
        }
    }

    /// The slow path of a safe point: collects garbage if wanted, then
    /// preempts if out of fuel. Returns how to leave the interpreter, if it
    /// must; the fiber resumes at `resume_ip`.
    #[cold]
    #[inline(never)]
    fn safe_point(&mut self, fiber: &mut Fiber, fuel_left: i64, resume_ip: usize) -> Option<Result<Exit, Trap>> {
        fiber.frames.last_mut().unwrap().ip = resume_ip as u32;
        if self.heap.wants_gc() {
            self.collect_garbage(Some(fiber));
            if self.heap.over_limit() {
                return Some(Err(Trap::Limit(LimitKind::Memory)));
            }
        }
        if fuel_left <= 0 {
            return Some(Ok(Exit::Preempt));
        }
        None
    }

    /// Calls a native; `Ok(None)` to continue, `Ok(Some(exit))` to suspend.
    /// Arguments are passed straight from the fiber's stack (the running
    /// fiber is not in `self.fibers`, so natives cannot observe or move it),
    /// and `natives` is the interpreter's own reference to the table, so no
    /// per-call copies or reference counting are needed. Natives charge the
    /// meter for their work; a native that exhausts the slice's native fuel
    /// preempts the task once it returns.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn call_native(
        &mut self,
        natives: &[NativeEntry],
        fiber: &mut Fiber,
        n: u32,
        start: usize,
        argc: usize,
        slot: usize,
        ip: usize,
    ) -> Result<Option<Exit>, Trap> {
        fiber.frames.last_mut().unwrap().ip = ip as u32;
        let mut r = Self::invoke_native(self, natives, n, &fiber.stack[start..start + argc]);
        if r.is_memory_limit() {
            // Natives check memory against an estimate that includes garbage
            // they cannot collect (their locals are not roots). Collect and
            // retry once; natives reserve memory before they mutate anything.
            self.collect_all_garbage(Some(fiber));
            r = Self::invoke_native(self, natives, n, &fiber.stack[start..start + argc]);
        }
        if !matches!(r, Err(Ctl::NeedPermission(_))) {
            self.forget_once_answers();
        }
        let r = match r {
            Ok(v) => {
                fiber.stack[slot] = v;
                if self.cancel_current {
                    self.cancel_current = false;
                    return Ok(Some(Exit::Cancelled));
                }
                if self.meter.fuel() <= 0 {
                    // Natives used up the slice: let the scheduler check
                    // limits and run other tasks.
                    return Ok(Some(Exit::Preempt));
                }
                return Ok(None);
            }
            Err(c) => c,
        };
        match r {
            Ctl::Panic(m) => Err(self.panic_trap(fiber, ip, m)),
            // Already retried after a collection.
            Ctl::Limit(k) => Err(Trap::Limit(k)),
            Ctl::Interrupted => Err(Trap::Interrupted),
            Ctl::Exit(c) => Ok(Some(Exit::Terminate(c))),
            Ctl::NeedPermission(cap) => {
                // Retry the call instruction after the prompt.
                fiber.frames.last_mut().unwrap().ip = (ip - 1) as u32;
                Ok(Some(Exit::Prompt(cap)))
            }
            Ctl::Yield => {
                fiber.stack[slot] = Value::Unit;
                Ok(Some(Exit::YieldNow))
            }
            Ctl::Async(fut) => {
                fiber.result_slot = slot;
                Ok(Some(Exit::Async(fut)))
            }
            Ctl::Block(b) => {
                fiber.result_slot = slot;
                let g = self.next_gen();
                fiber.wgen = g;
                let me = self.current;
                match self.block_on(me, g, b) {
                    Ok(Some(v)) => {
                        // Satisfied immediately.
                        fiber.stack[slot] = v;
                        Ok(None)
                    }
                    Ok(None) => {
                        if let Some(r) = self.pending_race.take() {
                            fiber.race = r;
                        }
                        Ok(Some(Exit::Blocked))
                    }
                    Err(m) => Err(self.trap(fiber, ip, m)),
                }
            }
        }
    }

    /// `Op::StrClass`: the character-class tests of strings.
    #[inline(never)]
    fn str_class(&self, v: Value, class: u8) -> Result<bool, Stop> {
        let t = self.heap.str(v);
        if let &[b] = t.as_bytes()
            && b.is_ascii()
        {
            // One ASCII character (the common case)
            return Ok(match class {
                0 => b.is_ascii_digit(),
                1 => b.is_ascii_alphabetic(),
                2 => b.is_ascii_alphanumeric(),
                3 => (b as char).is_whitespace(),
                4 => b.is_ascii_uppercase(),
                _ => b.is_ascii_lowercase(),
            });
        }
        self.meter.charge_bytes(t.len()).map_err(Stop::from)?;
        let f: fn(char) -> bool = match class {
            0 => |c| c.is_ascii_digit(),
            1 => char::is_alphabetic,
            2 => char::is_alphanumeric,
            3 => char::is_whitespace,
            4 => char::is_uppercase,
            _ => char::is_lowercase,
        };
        Ok(!t.is_empty() && t.chars().all(f))
    }

    /// `==`, `!=`, `<` and `<=` on generic values. Out of line, like the other
    /// heavy operations, to keep the dispatch loop small; composite values
    /// are compared with metering (deep comparisons cost fuel and can be
    /// interrupted).
    #[inline(never)]
    fn op_compare(&mut self, prog: &Program, op: Op, x: Value, y: Value) -> Result<bool, Ctl> {
        // Anything but two plain scalars may be arbitrarily large (inline
        // variants such as `Some(x)` hold heap values too).
        let scalar = |v: Value| matches!(v, Value::Unit | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Dur(_) | Value::Tag(_));
        let composite = !(scalar(x) && scalar(y));
        Ok(match op {
            Op::Eq { .. } | Op::Ne { .. } => {
                let eq = if composite { values_eq_metered(&self.heap, &self.meter, x, y)? } else { values_eq(&self.heap, x, y) };
                eq == matches!(op, Op::Eq { .. })
            }
            Op::Lt { .. } if composite => values_lt_metered(prog, &self.heap, &self.meter, x, y)?,
            Op::Lt { .. } => values_lt(prog, &self.heap, x, y),
            _ if composite => values_le_metered(prog, &self.heap, &self.meter, x, y)?,
            _ => values_le(prog, &self.heap, x, y),
        })
    }

    /// `ToStr` (display), `Debug` and `Format` of a value, as a new string.
    #[inline(never)]
    fn op_display(&mut self, prog: &Program, v: Value, spec: Option<u16>, debug: bool) -> Result<Value, Ctl> {
        let s = match spec {
            Some(i) => display::format_value_metered(prog, &self.heap, &self.meter, v, &prog.specs[i as usize])?,
            None if debug => display::to_debug_metered(prog, &self.heap, &self.meter, v)?,
            None => display::to_display_metered(prog, &self.heap, &self.meter, v)?,
        };
        Ok(self.heap.new_str(s))
    }

    /// Concatenates the strings in `parts` (string interpolation).
    #[inline(never)]
    fn op_concat(&mut self, parts: &[Value]) -> Result<Value, Ctl> {
        let total: usize = parts.iter().map(|p| self.heap.str(*p).len()).sum();
        if self.heap.would_exceed(total) {
            return Err(Ctl::Limit(LimitKind::Memory));
        }
        self.meter.charge_bytes(total)?;
        let mut out = String::with_capacity(total);
        for p in parts {
            out.push_str(self.heap.str(*p));
        }
        Ok(self.heap.new_str(out))
    }

    /// Calls native `n`. (A Rust panic in it is caught by the scheduler
    /// around the whole slice, see `crate::unwind`.)
    #[inline(always)]
    fn invoke_native(&mut self, natives: &[NativeEntry], n: u32, args: &[Value]) -> NResult {
        let entry = &natives[n as usize];
        match &entry.imp {
            NativeImpl::Builtin(f) => f(self, args),
            NativeImpl::Host(f) => f(self, args),
        }
    }

    /// Registers a blocking wait for fiber `me`; returns a value if it can be
    /// satisfied immediately.
    fn block_on(&mut self, me: u32, g: u64, b: BlockOn) -> Result<Option<Value>, String> {
        match b {
            BlockOn::Join(task) => {
                let (fid, state, result) = match self.heap.get(task) {
                    Obj::Task(t) => (t.fiber, t.state, t.result),
                    _ => return Err("internal error: not a task".into()),
                };
                match state {
                    TaskState::Done => Ok(Some(result)),
                    TaskState::Cancelled => Err("task cancelled".into()),
                    TaskState::Running => {
                        if fid == me {
                            return Err("deadlock: a task cannot wait for itself".into());
                        }
                        self.add_waiter(fid, (me, g))?;
                        Ok(None)
                    }
                }
            }
            BlockOn::Race(tasks) => {
                if tasks.is_empty() {
                    return Err("task.race called with an empty list".into());
                }
                // One waiter per task, and the list of tasks to cancel.
                self.meter.charge(tasks.len() as u64).map_err(ctl_message)?;
                if self.heap.would_exceed(tasks.len() * (std::mem::size_of::<Value>() + std::mem::size_of::<(u32, u64)>())) {
                    return Err(ctl_message(Ctl::Limit(LimitKind::Memory)));
                }
                for &t in &tasks {
                    if let Obj::Task(to) = self.heap.get(t)
                        && to.state == TaskState::Done
                    {
                        let r = to.result;
                        for &o in &tasks {
                            if o != t {
                                self.cancel_task(o);
                            }
                        }
                        return Ok(Some(r));
                    }
                }
                for &t in &tasks {
                    let fid = match self.heap.get(t) {
                        Obj::Task(to) if to.state == TaskState::Running => to.fiber,
                        _ => continue,
                    };
                    self.add_waiter(fid, (me, g))?;
                }
                self.heap.fiber_bytes += tasks.capacity() * std::mem::size_of::<Value>();
                self.pending_race = Some(tasks);
                Ok(None)
            }
            BlockOn::Recv(ch) => {
                let Obj::Channel(c) = self.heap.get_mut(ch) else { return Err("internal error: not a channel".into()) };
                if let Some(v) = c.queue.pop_front() {
                    // Move a blocked sender's value into the queue.
                    let woken = c.senders.pop_front().map(|(sf, sg, sv)| {
                        c.queue.push_back(sv);
                        (sf, sg)
                    });
                    let some = self.some(v);
                    if let Some((sf, sg)) = woken
                        && !self.deliver(sf, sg, Value::Bool(true))
                    {
                        // The sender went away; its value stays queued.
                    }
                    return Ok(Some(some));
                }
                if let Some((sf, sg, sv)) = c.senders.pop_front() {
                    let some = self.some(sv);
                    self.deliver(sf, sg, Value::Bool(true));
                    return Ok(Some(some));
                }
                if c.closed {
                    return Ok(Some(self.none()));
                }
                if c.receivers.len() >= c.receivers_prune_at.max(MIN_PRUNE) {
                    // Drop the waits of cancelled tasks.
                    self.meter.charge(c.receivers.len() as u64).map_err(ctl_message)?;
                    let fibers = &self.fibers;
                    c.receivers.retain(|&w| wait_is_live(fibers, w));
                    c.receivers_prune_at = 2 * c.receivers.len();
                }
                let before = c.receivers.capacity();
                c.receivers.push_back((me, g));
                let grown = c.receivers.capacity().saturating_sub(before);
                self.heap.bytes += grown * std::mem::size_of::<(u32, u64)>();
                Ok(None)
            }
            BlockOn::Send(ch, v) => {
                let Obj::Channel(c) = self.heap.get_mut(ch) else { return Err("internal error: not a channel".into()) };
                if c.closed {
                    return Ok(Some(Value::Bool(false)));
                }
                while let Some((rf, rg)) = c.receivers.pop_front() {
                    if wait_is_live(&self.fibers, (rf, rg)) {
                        let some = self.some(v);
                        self.deliver(rf, rg, some);
                        return Ok(Some(Value::Bool(true)));
                    }
                }
                let Obj::Channel(c) = self.heap.get(ch) else { unreachable!() };
                if c.capacity == 0 || c.queue.len() < c.capacity {
                    // Count the queue's growth before it happens (it doubles).
                    let (len, cap) = (c.queue.len(), c.queue.capacity());
                    let more = if len == cap { cap.max(4) } else { 0 };
                    if more > 0 && self.heap.would_exceed(more * std::mem::size_of::<Value>()) {
                        return Err(ctl_message(Ctl::Limit(LimitKind::Memory)));
                    }
                    let Obj::Channel(c) = self.heap.get_mut_storing(ch, &[v]) else { unreachable!() };
                    c.queue.reserve_exact(more);
                    c.queue.push_back(v);
                    let grown = c.queue.capacity() - cap;
                    self.heap.bytes += grown * std::mem::size_of::<Value>();
                    return Ok(Some(Value::Bool(true)));
                }
                let Obj::Channel(c) = self.heap.get_mut(ch) else { unreachable!() };
                let before = c.senders.capacity();
                c.senders.push_back((me, g, v));
                let grown = c.senders.capacity().saturating_sub(before);
                self.heap.bytes += grown * std::mem::size_of::<(u32, u64, Value)>();
                Ok(None)
            }
        }
    }

    /// Registers `waiter` to be woken when the task of fiber `target`
    /// finishes, first dropping stale waits (of cancelled tasks) if the list
    /// has grown enough.
    fn add_waiter(&mut self, target: u32, waiter: (u32, u64)) -> Result<(), String> {
        let Some(Some(f)) = self.fibers.get_mut(target as usize) else { return Ok(()) };
        let prune = f.waiters.len() >= f.waiters_prune_at.max(MIN_PRUNE);
        if prune {
            self.meter.charge(f.waiters.len() as u64).map_err(ctl_message)?;
        }
        let Some(Some(f)) = self.fibers.get_mut(target as usize) else { return Ok(()) };
        let mut list = std::mem::take(&mut f.waiters);
        let mut prune_at = f.waiters_prune_at;
        if prune {
            // The waiting fiber is running (not in `fibers`): its current
            // wait, registered by a `race` on the same task, is live.
            list.retain(|&w| w == waiter || wait_is_live(&self.fibers, w));
            prune_at = 2 * list.len();
        }
        let before = list.capacity();
        list.push(waiter);
        self.heap.fiber_bytes += list.capacity().saturating_sub(before) * std::mem::size_of::<(u32, u64)>();
        if let Some(Some(f)) = self.fibers.get_mut(target as usize) {
            f.waiters = list;
            f.waiters_prune_at = prune_at;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Maps, slices and snapshots

    #[inline(never)]
    pub fn map_get(&self, m: Value, k: Value) -> Result<Option<Value>, String> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err("internal error: not a map".into()) };
        let slot = with_keyref_metered(&self.heap, &self.meter, k, |kr| mo.index.get(kr))?;
        Ok(slot.and_then(|s| mo.entries[s].map(|(_, v)| v)))
    }

    #[inline(never)]
    pub fn map_insert(&mut self, m: Value, k: Value, v: Value) -> Result<Option<Value>, String> {
        // Existing key: overwrite in place.
        let slot = {
            let Obj::Map(mo) = self.heap.get(m) else { return Err("internal error: not a map".into()) };
            with_keyref_metered(&self.heap, &self.meter, k, |kr| mo.index.get(kr))?
        };
        if let Some(s) = slot {
            let Obj::Map(mo) = self.heap.entries_mut_storing(m, s, false, &[k, v]) else { unreachable!() };
            let old = mo.entries[s].map(|(_, ov)| ov);
            mo.entries[s] = Some((k, v));
            return Ok(old);
        }
        let (owned, key_bytes) = self.owned_key(k)?;
        let end = match self.heap.get(m) {
            Obj::Map(mo) => mo.entries.len(),
            _ => 0,
        };
        let Obj::Map(mo) = self.heap.entries_mut_storing(m, end, true, &[k, v]) else { unreachable!() };
        let s = mo.entries.len();
        mo.entries.push(Some((k, v)));
        mo.live += 1;
        mo.key_bytes += key_bytes;
        insert_owned(&mut mo.index, owned, s);
        self.heap.bytes += 2 * std::mem::size_of::<Value>() + INDEX_ENTRY_SIZE + key_bytes;
        Ok(None)
    }

    pub fn map_remove(&mut self, m: Value, k: Value) -> Result<Option<Value>, String> {
        let (owned, key_bytes) = self.owned_key(k)?;
        let Obj::Map(mo) = self.heap.get_mut(m) else { return Err("internal error: not a map".into()) };
        let slot = remove_owned(&mut mo.index, &owned);
        let Some(s) = slot else { return Ok(None) };
        let old = mo.entries[s].take().map(|(_, v)| v);
        mo.live -= 1;
        mo.key_bytes = mo.key_bytes.saturating_sub(key_bytes);
        // Compact when more than half the slots are tombstones.
        if mo.entries.len() > 32 && mo.live * 2 < mo.entries.len() {
            let mut remap = vec![usize::MAX; mo.entries.len()];
            let mut fresh = Vec::with_capacity(mo.live);
            for (i, e) in mo.entries.iter().enumerate() {
                if let Some(e) = e {
                    remap[i] = fresh.len();
                    fresh.push(Some(*e));
                }
            }
            mo.entries = fresh;
            mo.index.renumber(|i| remap[i]);
        }
        Ok(old)
    }

    /// An owned key usable for insertion/removal, and the bytes it owns.
    pub fn owned_key(&self, k: Value) -> Result<(OwnedKey, usize), String> {
        let other = |k| super::compare::hkey_sized(&self.heap, &self.meter, k).map(|(h, n)| (OwnedKey::Other(h), n));
        match k {
            Value::Int(i) => Ok((OwnedKey::Int(i), 0)),
            Value::Obj(_) => match self.heap.get(k) {
                Obj::Str(s) => {
                    self.meter.charge_bytes(s.len()).map_err(ctl_message)?;
                    Ok((OwnedKey::Str(s.clone()), s.len()))
                }
                _ => other(k),
            },
            _ => other(k),
        }
    }

    pub fn set_add(&mut self, s: Value, k: Value) -> Result<bool, String> {
        let exists = {
            let Obj::Set(so) = self.heap.get(s) else { return Err("internal error: not a set".into()) };
            with_keyref_metered(&self.heap, &self.meter, k, |kr| so.index.get(kr))?.is_some()
        };
        if exists {
            return Ok(false);
        }
        let (owned, key_bytes) = self.owned_key(k)?;
        let end = match self.heap.get(s) {
            Obj::Set(so) => so.entries.len(),
            _ => 0,
        };
        let Obj::Set(so) = self.heap.entries_mut_storing(s, end, true, &[k]) else { unreachable!() };
        let slot = so.entries.len();
        so.entries.push(Some(k));
        so.live += 1;
        so.key_bytes += key_bytes;
        insert_owned(&mut so.index, owned, slot);
        self.heap.bytes += std::mem::size_of::<Value>() + INDEX_ENTRY_SIZE + key_bytes;
        Ok(true)
    }

    pub fn set_contains(&self, s: Value, k: Value) -> Result<bool, String> {
        let Obj::Set(so) = self.heap.get(s) else { return Err("internal error: not a set".into()) };
        Ok(with_keyref_metered(&self.heap, &self.meter, k, |kr| so.index.get(kr))?.is_some())
    }

    pub fn set_remove(&mut self, s: Value, k: Value) -> Result<bool, String> {
        let (owned, key_bytes) = self.owned_key(k)?;
        let Obj::Set(so) = self.heap.get_mut(s) else { return Err("internal error: not a set".into()) };
        let Some(slot) = remove_owned(&mut so.index, &owned) else { return Ok(false) };
        so.entries[slot] = None;
        so.live -= 1;
        so.key_bytes = so.key_bytes.saturating_sub(key_bytes);
        if so.entries.len() > 32 && so.live * 2 < so.entries.len() {
            let mut remap = vec![usize::MAX; so.entries.len()];
            let mut fresh = Vec::with_capacity(so.live);
            for (i, e) in so.entries.iter().enumerate() {
                if let Some(e) = e {
                    remap[i] = fresh.len();
                    fresh.push(Some(*e));
                }
            }
            so.entries = fresh;
            so.index.renumber(|i| remap[i]);
        }
        Ok(true)
    }

    #[inline(never)]
    fn snapshot(&mut self, o: Value) -> Result<Value, String> {
        let n = match self.heap.get(o) {
            Obj::List(l) => l.len(),
            Obj::Set(s) => s.entries.len(),
            Obj::Map(m) => m.entries.len(),
            Obj::Range(a, b, incl) => ((*b as i128) - (*a as i128) + i128::from(*incl)).clamp(0, i64::MAX as i128) as usize,
            _ => 0,
        };
        if self.heap.would_exceed(n.saturating_mul(16)) {
            return Err("\u{0}memory".into());
        }
        self.meter.charge(n as u64).map_err(ctl_message)?;
        let items: Vec<Value> = match self.heap.get(o) {
            Obj::List(l) => l.clone(),
            Obj::Set(s) => s.entries.iter().flatten().copied().collect(),
            Obj::Map(m) => {
                let pairs: Vec<(Value, Value)> = m.entries.iter().flatten().copied().collect();
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    out.push(self.heap.alloc(Obj::Tuple(Box::new([k, v]))));
                }
                out
            }
            Obj::Range(a, b, incl) => {
                let (a, b, incl) = (*a, *b, *incl);
                let end = if incl { b.saturating_add(1) } else { b };
                let n = (end - a).max(0) as usize;
                if self.heap.would_exceed(n * 16) {
                    return Err("\u{0}memory".into());
                }
                (a..end).map(Value::Int).collect()
            }
            _ => return Err("internal error: not iterable".into()),
        };
        Ok(self.heap.new_list(items))
    }

    #[inline(never)]
    fn slice(&mut self, o: Value, start: Option<i64>, end: Option<i64>, inclusive: bool, flags: u16) -> Result<Value, String> {
        let len = match self.heap.get(o) {
            Obj::List(l) => l.len(),
            Obj::Str(s) => s.len(),
            Obj::Bytes(b) => b.len(),
            _ => return Err("internal error: slice of non-sequence".into()),
        } as i64;
        let a = start.unwrap_or(0);
        let mut b = end.unwrap_or(len);
        if inclusive {
            b = b.checked_add(1).ok_or("slice end out of range")?;
        }
        if a < 0 || b > len || a > b {
            return Err(format!("slice {a}..{b} out of range for length {len}"));
        }
        let (a, b) = (a as usize, b as usize);
        let list = matches!(self.heap.get(o), Obj::List(_));
        let cost = if list { (b - a) as u64 } else { ((b - a) / super::meter::BYTES_PER_UNIT) as u64 };
        if self.heap.would_exceed(if list { (b - a) * std::mem::size_of::<Value>() } else { b - a }) {
            return Err(ctl_message(Ctl::Limit(LimitKind::Memory)));
        }
        self.meter.charge(cost).map_err(ctl_message)?;
        Ok(match self.heap.get(o) {
            Obj::List(l) => {
                let v = l[a..b].to_vec();
                self.heap.new_list(v)
            }
            Obj::Str(s) => {
                if !s.is_char_boundary(a) || !s.is_char_boundary(b) {
                    return Err(format!("slice {a}..{b} is not on a character boundary"));
                }
                let t = s[a..b].to_string();
                self.heap.new_str(t)
            }
            Obj::Bytes(bs) => {
                let v = bs[a..b].to_vec();
                self.heap.new_bytes(v)
            }
            _ => {
                let _ = flags;
                unreachable!()
            }
        })
    }
}

pub enum OwnedKey {
    Int(i64),
    Str(Box<str>),
    Other(HKey),
}

fn insert_owned(index: &mut KeyIndex, k: OwnedKey, slot: usize) {
    if let KeyIndex::Empty = index {
        *index = match &k {
            OwnedKey::Int(_) => KeyIndex::Int(FxHashMap::default()),
            OwnedKey::Str(_) => KeyIndex::Str(FxHashMap::default()),
            OwnedKey::Other(_) => KeyIndex::Other(FxHashMap::default()),
        };
    }
    match (index, k) {
        (KeyIndex::Int(m), OwnedKey::Int(i)) => {
            m.insert(i, slot);
        }
        (KeyIndex::Str(m), OwnedKey::Str(s)) => {
            m.insert(s, slot);
        }
        (KeyIndex::Other(m), OwnedKey::Other(h)) => {
            m.insert(h, slot);
        }
        (KeyIndex::Other(m), OwnedKey::Int(i)) => {
            m.insert(HKey::Int(i), slot);
        }
        (KeyIndex::Other(m), OwnedKey::Str(s)) => {
            m.insert(HKey::Str(s), slot);
        }
        _ => {}
    }
}

fn remove_owned(index: &mut KeyIndex, k: &OwnedKey) -> Option<usize> {
    match (index, k) {
        (KeyIndex::Int(m), OwnedKey::Int(i)) => m.remove(i),
        (KeyIndex::Str(m), OwnedKey::Str(s)) => m.remove(s),
        (KeyIndex::Other(m), OwnedKey::Other(h)) => m.remove(h),
        (KeyIndex::Other(m), OwnedKey::Int(i)) => m.remove(&HKey::Int(*i)),
        (KeyIndex::Other(m), OwnedKey::Str(s)) => m.remove(&HKey::Str(s.clone())),
        _ => None,
    }
}
