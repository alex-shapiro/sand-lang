//! The bytecode interpreter loop.

use super::compare::{values_eq, values_le, values_lt, with_keyref};
use super::display;
use super::*;
use crate::LimitKind;

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

const MAX_NATIVE_ARGS: usize = 16;

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

pub fn float_rem(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) { r + b } else { r }
}

fn shift_amount(b: i64) -> Result<u32, &'static str> {
    if (0..64).contains(&b) { Ok(b as u32) } else { Err("shift out of range") }
}

impl Vm {
    /// Generic arithmetic on runtime values.
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
                Dur(x / y)
            }
            (3, Dur(x), Dur(y)) => Float(x as f64 / y as f64),
            (0, Obj(_), Obj(_)) => {
                let (oa, ob) = (self.heap.get(a), self.heap.get(b));
                match (oa, ob) {
                    (super::value::Obj::Str(x), super::value::Obj::Str(y)) => {
                        let mut s = String::with_capacity(x.len() + y.len());
                        if self.heap.would_exceed(x.len() + y.len()) {
                            return Err("\u{0}memory".into());
                        }
                        s.push_str(x);
                        s.push_str(y);
                        self.heap.new_str(s)
                    }
                    (super::value::Obj::Bytes(x), super::value::Obj::Bytes(y)) => {
                        if self.heap.would_exceed(x.len() + y.len()) {
                            return Err("\u{0}memory".into());
                        }
                        let mut v = x.to_vec();
                        v.extend_from_slice(y);
                        self.heap.new_bytes(v)
                    }
                    (super::value::Obj::List(x), super::value::Obj::List(y)) => {
                        if self.heap.would_exceed((x.len() + y.len()) * 16) {
                            return Err("\u{0}memory".into());
                        }
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

    fn trap(&self, fiber: &Fiber, ip: usize, msg: String) -> Trap {
        if let Some(rest) = msg.strip_prefix('\u{0}')
            && rest == "memory"
        {
            return Trap::Limit(LimitKind::Memory);
        }
        Trap::Panic { message: msg, trace: self.trace_of(fiber, Some(ip as u32)) }
    }

    /// Runs `fiber` until it finishes, blocks, or exhausts `fuel`.
    pub(crate) fn interp(&mut self, fiber: &mut Fiber, fuel: &mut i64) -> Result<Exit, Trap> {
        if let Some(msg) = fiber.pending_panic.take() {
            let ip = fiber.frames.last().map(|f| f.ip).unwrap_or(0) as usize;
            return Err(self.trap(fiber, ip, msg));
        }
        let prog = self.prog.clone();
        let mut fr = *fiber.frames.last().unwrap();
        let mut code: &[Op] = &prog.protos[fr.proto as usize].code;
        let mut ip = fr.ip as usize;
        let mut base = fr.base as usize;

        macro_rules! r {
            ($x:expr) => {
                fiber.stack[base + $x as usize]
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
        macro_rules! int {
            ($x:expr) => {
                match r!($x) {
                    Value::Int(i) => i,
                    other => fail!(format!("internal error: expected int, found {other:?}")),
                }
            };
        }
        macro_rules! float {
            ($x:expr) => {
                match r!($x) {
                    Value::Float(f) => f,
                    other => fail!(format!("internal error: expected float, found {other:?}")),
                }
            };
        }
        macro_rules! jump {
            ($off:expr) => {
                ip = (ip as isize + $off as isize) as usize
            };
        }
        macro_rules! tick {
            () => {
                *fuel -= 1;
                if *fuel <= 0 || self.heap.wants_gc() {
                    save!();
                    if self.heap.wants_gc() {
                        self.collect_garbage(Some(fiber));
                        if self.heap.over_limit() {
                            return Err(Trap::Limit(LimitKind::Memory));
                        }
                    }
                    if *fuel <= 0 {
                        return Ok(Exit::Preempt);
                    }
                }
            };
        }
        // Like `tick!` but for call instructions: on preemption the call is
        // re-executed when the fiber resumes.
        macro_rules! tick_call {
            () => {
                *fuel -= 1;
                if *fuel <= 0 || self.heap.wants_gc() {
                    if self.heap.wants_gc() {
                        save!();
                        self.collect_garbage(Some(fiber));
                        if self.heap.over_limit() {
                            return Err(Trap::Limit(LimitKind::Memory));
                        }
                    }
                    if *fuel <= 0 {
                        fiber.frames.last_mut().unwrap().ip = (ip - 1) as u32;
                        return Ok(Exit::Preempt);
                    }
                }
            };
        }
        // Enters a new frame for a script function.
        macro_rules! enter {
            ($proto:expr, $new_base:expr, $ret:expr, $closure:expr) => {{
                if fiber.frames.len() >= self.host.limits.max_call_depth {
                    fail!("stack overflow");
                }
                save!();
                let p = &prog.protos[$proto as usize];
                let nb = $new_base;
                let need = nb + p.nregs as usize;
                if fiber.stack.len() < need {
                    if need > 64 * 1024 * 1024 {
                        fail!("stack overflow");
                    }
                    fiber.stack.resize(need, Value::Unit);
                }
                fr = Frame { proto: $proto, ip: 0, base: nb as u32, ret: $ret as u32, closure: $closure };
                fiber.frames.push(fr);
                code = &p.code;
                ip = 0;
                base = nb;
            }};
        }
        macro_rules! do_return {
            ($v:expr) => {{
                let v = $v;
                let done = fiber.frames.pop().unwrap();
                match fiber.frames.last() {
                    None => {
                        fiber.stack.truncate(0);
                        return Ok(Exit::Done(v));
                    }
                    Some(caller) => {
                        fr = *caller;
                        let cp = &prog.protos[fr.proto as usize];
                        code = &cp.code;
                        ip = fr.ip as usize;
                        base = fr.base as usize;
                        fiber.stack[done.ret as usize] = v;
                        let top = base + cp.nregs as usize;
                        if fiber.stack.len() > top + 256 {
                            fiber.stack.truncate(top);
                        }
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
                        fail!(format!("global used before initialization: `{}`", prog.global_names[g as usize]));
                    }
                    r!(dst) = v;
                }
                Op::StoreGlobal { g, src } => self.globals[g as usize] = r!(src),
                Op::LoadCapture { dst, idx } => {
                    r!(dst) = self.heap.fields(fr.closure)[idx as usize];
                }
                Op::LoadCaptureCell { dst, idx } => {
                    let cell = self.heap.fields(fr.closure)[idx as usize];
                    r!(dst) = match self.heap.get(cell) {
                        Obj::Cell(v) => *v,
                        _ => Value::Unit,
                    };
                }
                Op::StoreCaptureCell { idx, src } => {
                    let cell = self.heap.fields(fr.closure)[idx as usize];
                    let v = r!(src);
                    if let Obj::Cell(c) = self.heap.get_mut(cell) {
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
                        && let Obj::Cell(x) = self.heap.get_mut(c)
                    {
                        *x = v;
                    }
                }
                Op::MakeClosure { dst, proto } => {
                    let p = &prog.protos[proto as usize];
                    let mut caps = Vec::with_capacity(p.captures.len());
                    for c in &p.captures {
                        caps.push(match *c {
                            CaptureSrc::Reg(u16::MAX) => Value::Unit,
                            CaptureSrc::Reg(rr) => r!(rr),
                            CaptureSrc::Capture(i) => self.heap.fields(fr.closure)[i as usize],
                        });
                    }
                    r!(dst) = self.heap.alloc(Obj::Closure(proto, caps.into_boxed_slice()));
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
                Op::Eq { dst, a, b } => r!(dst) = Value::Bool(values_eq(&self.heap, r!(a), r!(b))),
                Op::Ne { dst, a, b } => r!(dst) = Value::Bool(!values_eq(&self.heap, r!(a), r!(b))),
                Op::Lt { dst, a, b } => r!(dst) = Value::Bool(values_lt(&prog, &self.heap, r!(a), r!(b))),
                Op::Le { dst, a, b } => r!(dst) = Value::Bool(values_le(&prog, &self.heap, r!(a), r!(b))),

                // ---------------- control flow
                Op::Jmp { off } => jump!(off),
                Op::Loop { off } => {
                    jump!(off);
                    tick!();
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
                Op::JNotVariant { src, variant, off } => {
                    if self.heap.variant_id(r!(src)) != variant as u32 {
                        jump!(off)
                    }
                }
                Op::IsVariant { dst, src, variant } => {
                    r!(dst) = Value::Bool(self.heap.variant_id(r!(src)) == variant as u32);
                }

                // ---------------- calls
                Op::CallDirect { base: b, proto } => {
                    tick_call!();
                    let nb = base + b as usize;
                    enter!(proto, nb, nb, Value::Unit);
                }
                Op::Call { base: b, argc } => {
                    tick_call!();
                    let callee = r!(b);
                    match self.heap.get(callee) {
                        Obj::Closure(p, _) => {
                            let p = *p;
                            let nb = base + b as usize + 1;
                            enter!(p, nb, nb - 1, callee);
                        }
                        Obj::NativeFn(n) => {
                            let n = *n;
                            let start = base + b as usize + 1;
                            let slot = base + b as usize;
                            match self.call_native(fiber, n, start, argc as usize, slot, ip) {
                                Ok(None) => {}
                                Ok(Some(exit)) => return Ok(exit),
                                Err(t) => return Err(t),
                            }
                        }
                        _ => fail!("internal error: call of a non-function"),
                    }
                }
                Op::CallNative { base: b, native } => {
                    let start = base + b as usize;
                    let argc = prog.native_argc.get(native as usize).copied().unwrap_or(0) as usize;
                    match self.call_native(fiber, native, start, argc, start, ip) {
                        Ok(None) => {}
                        Ok(Some(exit)) => return Ok(exit),
                        Err(t) => return Err(t),
                    }
                }
                Op::TailCallDirect { base: b, proto } => {
                    tick_call!();
                    let p = &prog.protos[proto as usize];
                    let n = p.nparams as usize;
                    let src = base + b as usize;
                    fiber.stack.copy_within(src..src + n, base);
                    let need = base + p.nregs as usize;
                    if fiber.stack.len() < need {
                        fiber.stack.resize(need, Value::Unit);
                    }
                    let top = fiber.frames.last_mut().unwrap();
                    top.proto = proto;
                    top.closure = Value::Unit;
                    fr = *top;
                    code = &p.code;
                    ip = 0;
                }
                Op::TailCall { base: b, argc } => {
                    tick_call!();
                    let callee = r!(b);
                    match self.heap.get(callee) {
                        Obj::Closure(p, _) => {
                            let proto = *p;
                            let pr = &prog.protos[proto as usize];
                            let n = argc as usize;
                            let src = base + b as usize + 1;
                            fiber.stack.copy_within(src..src + n, base);
                            let need = base + pr.nregs as usize;
                            if fiber.stack.len() < need {
                                fiber.stack.resize(need, Value::Unit);
                            }
                            let top = fiber.frames.last_mut().unwrap();
                            top.proto = proto;
                            top.closure = callee;
                            fr = *top;
                            code = &pr.code;
                            ip = 0;
                        }
                        Obj::NativeFn(n) => {
                            // Not a real tail call: the compiler emits `Ret base` next.
                            let n = *n;
                            let start = base + b as usize + 1;
                            let slot = base + b as usize;
                            match self.call_native(fiber, n, start, argc as usize, slot, ip) {
                                Ok(None) => {}
                                Ok(Some(exit)) => return Ok(exit),
                                Err(t) => return Err(t),
                            }
                        }
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
                    let s = base + start as usize;
                    let items = fiber.stack[s..s + count as usize].to_vec();
                    r!(dst) = self.heap.new_list(items);
                }
                Op::NewTuple { dst, start, count } => {
                    let s = base + start as usize;
                    let items: Box<[Value]> = fiber.stack[s..s + count as usize].into();
                    r!(dst) = self.heap.alloc(Obj::Tuple(items));
                }
                Op::NewStruct { dst, start, def } => {
                    let n = prog.types[def as usize].fields.len();
                    let s = base + start as usize;
                    let items: Box<[Value]> = fiber.stack[s..s + n].into();
                    r!(dst) = self.heap.alloc(Obj::Struct(def as u32, items));
                }
                Op::NewVariant { dst, start, variant } => {
                    let n = prog.variants[variant as usize].arity;
                    if n == 0 {
                        r!(dst) = Value::Tag(variant as u32);
                    } else {
                        let s = base + start as usize;
                        let items: Box<[Value]> = fiber.stack[s..s + n].into();
                        r!(dst) = self.heap.alloc(Obj::Variant(variant as u32, items));
                    }
                }
                Op::NewMap { dst, start, count } => {
                    let m = self.heap.alloc(Obj::Map(Box::default()));
                    let s = base + start as usize;
                    for i in 0..count as usize {
                        let k = fiber.stack[s + 2 * i];
                        let v = fiber.stack[s + 2 * i + 1];
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
                        None => fail!(format!("internal error: field {idx} of {:?}", self.heap.get(o))),
                    };
                }
                Op::SetField { obj, idx, src } => {
                    let o = r!(obj);
                    let v = r!(src);
                    match self.heap.get_mut(o) {
                        Obj::Struct(_, f) => f[idx as usize] = v,
                        _ => fail!("internal error: set field on non-struct"),
                    }
                }
                Op::Index { dst, obj, idx } => {
                    let o = r!(obj);
                    let i = int!(idx);
                    let l = self.heap.list(o);
                    if i < 0 || i as usize >= l.len() {
                        fail!(format!("index out of bounds: index {i}, length {}", l.len()));
                    }
                    r!(dst) = l[i as usize];
                }
                Op::IndexMap { dst, obj, key } => {
                    let m = r!(obj);
                    let k = r!(key);
                    match self.map_get(m, k) {
                        Ok(Some(v)) => r!(dst) = v,
                        Ok(None) => {
                            let ks = self.debug(k).unwrap_or_default();
                            fail!(format!("key not found: {ks}"))
                        }
                        Err(e) => fail!(e),
                    }
                }
                Op::SetIndex { obj, idx, src } => {
                    let o = r!(obj);
                    let i = int!(idx);
                    let v = r!(src);
                    let l = self.heap.list_mut(o);
                    if i < 0 || i as usize >= l.len() {
                        let n = l.len();
                        fail!(format!("index out of bounds: index {i}, length {n}"));
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
                    match display::to_display(&prog, &self.heap, v) {
                        Ok(s) => r!(dst) = self.heap.new_str(s),
                        Err(e) => fail!(e),
                    }
                }
                Op::Debug { dst, src } => match display::to_debug(&prog, &self.heap, r!(src)) {
                    Ok(s) => r!(dst) = self.heap.new_str(s),
                    Err(e) => fail!(e),
                },
                Op::Format { dst, src, spec } => {
                    let s = &prog.specs[spec as usize];
                    match display::format_value(&prog, &self.heap, r!(src), s) {
                        Ok(s) => r!(dst) = self.heap.new_str(s),
                        Err(e) => fail!(e),
                    }
                }
                Op::Concat { dst, start, count } => {
                    let s = base + start as usize;
                    let mut total = 0;
                    for i in 0..count as usize {
                        total += self.heap.str(fiber.stack[s + i]).len();
                    }
                    if self.heap.would_exceed(total) {
                        save!();
                        return Err(Trap::Limit(LimitKind::Memory));
                    }
                    let mut out = String::with_capacity(total);
                    for i in 0..count as usize {
                        out.push_str(self.heap.str(fiber.stack[s + i]));
                    }
                    r!(dst) = self.heap.new_str(out);
                }

                // ---------------- misc
                Op::Spawn { dst, closure } => {
                    let c = r!(closure);
                    match self.spawn_task(c) {
                        Ok(t) => r!(dst) = t,
                        Err(Ctl::Panic(m)) => fail!(m),
                        Err(_) => fail!("spawn failed"),
                    }
                }
                Op::TryTop { src } => {
                    save!();
                    let e = self.heap.fields(r!(src)).first().copied().unwrap_or(Value::Unit);
                    let text = match display::to_display(&prog, &self.heap, e) {
                        Ok(t) => t,
                        Err(m) => fail!(m),
                    };
                    let msg = format!("error: {text}\n");
                    match self.write_stream(2, msg.as_bytes()) {
                        Ok(()) => {}
                        Err(Ctl::Limit(k)) => return Err(Trap::Limit(k)),
                        Err(_) => {}
                    }
                    return Ok(Exit::Terminate(1));
                }
                Op::Panic { msg } => {
                    let m = match &prog.consts[msg as usize] {
                        Const::Str(s) => s.clone(),
                        _ => "panic".into(),
                    };
                    fail!(m);
                }
            }
        }
    }

    /// Calls a native; `Ok(None)` to continue, `Ok(Some(exit))` to suspend.
    fn call_native(&mut self, fiber: &mut Fiber, n: u32, start: usize, argc: usize, slot: usize, ip: usize) -> Result<Option<Exit>, Trap> {
        let mut buf = [Value::Unit; MAX_NATIVE_ARGS];
        let argc = argc.min(MAX_NATIVE_ARGS);
        buf[..argc].copy_from_slice(&fiber.stack[start..start + argc]);
        fiber.frames.last_mut().unwrap().ip = ip as u32;
        let r = match &self.natives[n as usize].imp {
            NativeImpl::Builtin(f) => {
                let f = *f;
                f(self, &buf[..argc])
            }
            NativeImpl::Host(f) => {
                let f = f.clone();
                f(self, &buf[..argc])
            }
        };
        let r = match r {
            Ok(v) => {
                fiber.stack[slot] = v;
                if self.cancel_current {
                    self.cancel_current = false;
                    return Ok(Some(Exit::Cancelled));
                }
                return Ok(None);
            }
            Err(c) => c,
        };
        match r {
            Ctl::Panic(m) => Err(self.trap(fiber, ip, m)),
            Ctl::Limit(k) => Err(Trap::Limit(k)),
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
                        if let Some(Some(f)) = self.fibers.get_mut(fid as usize) {
                            f.waiters.push((me, g));
                        }
                        Ok(None)
                    }
                }
            }
            BlockOn::Race(tasks) => {
                if tasks.is_empty() {
                    return Err("task.race called with an empty list".into());
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
                    if let Some(Some(f)) = self.fibers.get_mut(fid as usize) {
                        f.waiters.push((me, g));
                    }
                }
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
                c.receivers.push_back((me, g));
                Ok(None)
            }
            BlockOn::Send(ch, v) => {
                let Obj::Channel(c) = self.heap.get_mut(ch) else { return Err("internal error: not a channel".into()) };
                if c.closed {
                    return Ok(Some(Value::Bool(false)));
                }
                // Hand directly to a waiting receiver.
                let receivers: Vec<(u32, u64)> = c.receivers.drain(..).collect();
                let mut rest = receivers.into_iter();
                for (rf, rg) in rest.by_ref() {
                    let alive = matches!(self.fibers.get(rf as usize), Some(Some(f)) if f.state == FiberState::Waiting && f.wgen == rg);
                    if alive {
                        let some = self.some(v);
                        self.deliver(rf, rg, some);
                        if let Obj::Channel(c) = self.heap.get_mut(ch) {
                            c.receivers.extend(rest);
                        }
                        return Ok(Some(Value::Bool(true)));
                    }
                }
                let Obj::Channel(c) = self.heap.get_mut(ch) else { unreachable!() };
                if c.capacity == 0 || c.queue.len() < c.capacity {
                    c.queue.push_back(v);
                    let before = c.queue.capacity();
                    let _ = before;
                    self.heap.bytes += std::mem::size_of::<Value>();
                    return Ok(Some(Value::Bool(true)));
                }
                c.senders.push_back((me, g, v));
                Ok(None)
            }
        }
    }

    // ------------------------------------------------------------------
    // Maps, slices and snapshots

    pub fn map_get(&self, m: Value, k: Value) -> Result<Option<Value>, String> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err("internal error: not a map".into()) };
        let slot = with_keyref(&self.heap, k, |kr| mo.index.get(kr))?;
        Ok(slot.and_then(|s| mo.entries[s].map(|(_, v)| v)))
    }

    pub fn map_insert(&mut self, m: Value, k: Value, v: Value) -> Result<Option<Value>, String> {
        // Existing key: overwrite in place.
        let slot = {
            let Obj::Map(mo) = self.heap.get(m) else { return Err("internal error: not a map".into()) };
            with_keyref(&self.heap, k, |kr| mo.index.get(kr))?
        };
        if let Some(s) = slot {
            let Obj::Map(mo) = self.heap.get_mut(m) else { unreachable!() };
            let old = mo.entries[s].map(|(_, ov)| ov);
            mo.entries[s] = Some((k, v));
            return Ok(old);
        }
        let owned = self.owned_key(k)?;
        let Obj::Map(mo) = self.heap.get_mut(m) else { unreachable!() };
        let s = mo.entries.len();
        mo.entries.push(Some((k, v)));
        mo.live += 1;
        insert_owned(&mut mo.index, owned, s);
        self.heap.bytes += 2 * std::mem::size_of::<Value>() + 48;
        Ok(None)
    }

    pub fn map_remove(&mut self, m: Value, k: Value) -> Result<Option<Value>, String> {
        let owned = self.owned_key(k)?;
        let Obj::Map(mo) = self.heap.get_mut(m) else { return Err("internal error: not a map".into()) };
        let slot = remove_owned(&mut mo.index, &owned);
        let Some(s) = slot else { return Ok(None) };
        let old = mo.entries[s].take().map(|(_, v)| v);
        mo.live -= 1;
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

    /// An owned key usable for insertion/removal.
    pub fn owned_key(&self, k: Value) -> Result<OwnedKey, String> {
        Ok(match k {
            Value::Int(i) => OwnedKey::Int(i),
            Value::Obj(_) => match self.heap.get(k) {
                Obj::Str(s) => OwnedKey::Str(s.clone()),
                _ => OwnedKey::Other(super::compare::hkey(&self.heap, k)?),
            },
            other => OwnedKey::Other(super::compare::hkey(&self.heap, other)?),
        })
    }

    pub fn set_add(&mut self, s: Value, k: Value) -> Result<bool, String> {
        let exists = {
            let Obj::Set(so) = self.heap.get(s) else { return Err("internal error: not a set".into()) };
            with_keyref(&self.heap, k, |kr| so.index.get(kr))?.is_some()
        };
        if exists {
            return Ok(false);
        }
        let owned = self.owned_key(k)?;
        let Obj::Set(so) = self.heap.get_mut(s) else { unreachable!() };
        let slot = so.entries.len();
        so.entries.push(Some(k));
        so.live += 1;
        insert_owned(&mut so.index, owned, slot);
        self.heap.bytes += std::mem::size_of::<Value>() + 48;
        Ok(true)
    }

    pub fn set_contains(&self, s: Value, k: Value) -> Result<bool, String> {
        let Obj::Set(so) = self.heap.get(s) else { return Err("internal error: not a set".into()) };
        Ok(with_keyref(&self.heap, k, |kr| so.index.get(kr))?.is_some())
    }

    pub fn set_remove(&mut self, s: Value, k: Value) -> Result<bool, String> {
        let owned = self.owned_key(k)?;
        let Obj::Set(so) = self.heap.get_mut(s) else { return Err("internal error: not a set".into()) };
        let Some(slot) = remove_owned(&mut so.index, &owned) else { return Ok(false) };
        so.entries[slot] = None;
        so.live -= 1;
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

    fn snapshot(&mut self, o: Value) -> Result<Value, String> {
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
