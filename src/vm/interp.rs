//! The bytecode interpreter loop.
//!
//! Its speed depends on how LLVM allocates registers across the loop: when
//! too many values stay live, the hottest ones (`code`, `ip`) are spilled
//! and every instruction reloads them, which unrelated edits can trigger.
//! So:
//!
//! - The loop carries only `code`, `ip`, `base`, `regs` and the fuel;
//!   anything else is read from memory where needed.
//! - Back edges leave the `match` with their offset and share one safe point.
//! - Loops, allocations, formatting and error paths go into `#[inline(never)]`
//!   or `#[cold]` functions, not into the arms.
//! - Results stay 16 bytes (`Ctl` is one pointer), and a helper returns a
//!   `Value` whole (`Ok(*v)`), not through an `Option<Value>`: larger or
//!   split results go through memory and stall the caller's load.
//!
//! Measure changes with `cargo bench` and by looking for stack loads and
//! stores around the dispatch jump in the disassembly of `Vm::interp`.

use super::compare::{
    find_entry, find_key, hash_key, map_get_int, map_slot, map_slot_tuple, set_has_int, set_slot, values_eq, values_eq_metered, values_le,
    values_le_metered, values_lt, values_lt_metered,
};
use super::display;
use super::meter::{MemoryLimit, Scratch, code_fuel, loop_fuel};
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
        match c.as_limit() {
            Some(k) => Stop::Limit(k),
            None => Stop::Interrupted,
        }
    }
}

impl From<Stop> for Ctl {
    fn from(s: Stop) -> Ctl {
        match s {
            Stop::Limit(k) => Ctl::limit(k),
            Stop::Interrupted => Ctl::interrupted(),
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
    /// A cancelled task ran its pending deferred code.
    Unwound,
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
        return Err("invalid: division by zero");
    }
    floor_div(a, b).ok_or("overflow: integer overflow")
}

pub fn int_rem(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        return Err("invalid: division by zero");
    }
    Ok(floor_rem(a, b))
}

pub fn int_pow(a: i64, b: i64) -> Result<i64, &'static str> {
    if b < 0 {
        return Err("invalid: negative exponent");
    }
    if b > u32::MAX as i64 {
        return match a {
            0 | 1 => Ok(a),
            -1 => Ok(if b % 2 == 0 { 1 } else { -1 }),
            _ => Err("overflow: integer overflow"),
        };
    }
    a.checked_pow(b as u32).ok_or("overflow: integer overflow")
}

/// A lookup in the dense values of a map (`MapObj::vals`).
enum Dense<'h> {
    Hit(&'h Value),
    Miss,
    /// The map has no dense values, or the key is not an int.
    Unknown,
}

/// The int key `k` in map `m`, if it has dense values.
#[inline(always)]
fn dense_get<'h>(heap: &'h Heap, m: &Value, k: &Value) -> Dense<'h> {
    let (&Value::Obj(h), &Value::Int(i)) = (m, k) else { return Dense::Unknown };
    let Obj::Map(mo) = heap.obj(h) else { return Dense::Unknown };
    if mo.vals.is_empty() {
        return Dense::Unknown;
    }
    match usize::try_from(i).ok().and_then(|i| mo.vals.get(i)) {
        Some(Value::Uninit) | None => Dense::Miss,
        Some(v) => Dense::Hit(v),
    }
}

/// Copies the captures of closure `h` to the start of `regs`.
#[inline]
fn load_captures(heap: &Heap, h: u32, regs: &mut [Value]) {
    if let Obj::Closure(_, caps) = heap.obj(h) {
        copy_values(caps, regs);
    }
}

/// Moves the `n` registers from `src` to the start of `regs` (a tail call's
/// arguments), inline for the usual few.
#[inline(always)]
fn shift_args(regs: &mut [Value], src: usize, n: usize) {
    match n {
        0 => {}
        1 => regs[0] = regs[src],
        2 => {
            regs[0] = regs[src];
            regs[1] = regs[src + 1];
        }
        _ => regs.copy_within(src..src + n, 0),
    }
}

/// Copies `src` to the start of `dst`, inline for the usual few values
/// (`copy_from_slice` calls `memmove`).
#[inline(always)]
fn copy_values(src: &[Value], dst: &mut [Value]) {
    match *src {
        [] => {}
        [a] => dst[0] = a,
        [a, b] => {
            dst[0] = a;
            dst[1] = b;
        }
        [a, b, c] => {
            dst[0] = a;
            dst[1] = b;
            dst[2] = c;
        }
        _ => dst[..src.len()].copy_from_slice(src),
    }
}

/// The payload of an inline variant as field `idx` (kept out of line so the
/// interpreter's struct field fast path stays small).
#[cold]
#[inline(never)]
fn inline_field(v: &Value, idx: u16) -> Value {
    match v.as_inline_variant() {
        Some((_, p)) if idx == 0 => p,
        _ => Value::Uninit,
    }
}

#[cold]
#[inline(never)]
fn index_error(i: i64, len: usize) -> String {
    format!("bounds: index out of bounds: index {i}, length {len}")
}

#[cold]
#[inline(never)]
fn uninit_global(prog: &Program, g: u32) -> String {
    format!("uninitialized: global used before initialization: `{}`", prog.global_names[g as usize])
}

/// `a == b` where it needs no fuel: the same object, or two strings one of
/// which is shorter than a fuel unit's worth of bytes. `None` otherwise.
///
/// Takes references: matching copies of 16-byte values makes LLVM spill them
/// to the stack to read their tags.
#[inline(always)]
fn quick_eq(heap: &Heap, a: &Value, b: &Value) -> Option<bool> {
    let (&Value::Obj(x), &Value::Obj(y)) = (a, b) else { return None };
    if x == y {
        return Some(true);
    }
    match (heap.obj(x), heap.obj(y)) {
        (Obj::Str(p), Obj::Str(q)) if p.len().min(q.len()) < super::meter::BYTES_PER_UNIT => Some(short_eq(p.as_bytes(), q.as_bytes())),
        _ => None,
    }
}

/// Equality of short byte strings, comparing the commonest (one byte)
/// inline rather than calling `memcmp`.
#[inline(always)]
fn short_eq(p: &[u8], q: &[u8]) -> bool {
    match (p, q) {
        ([a], [b]) => a == b,
        _ => p == q,
    }
}

/// `Op::Payload`: the field of a one-field variant, inline or boxed.
#[inline(never)]
fn payload(heap: &Heap, v: Value) -> Option<Value> {
    match v.as_inline_variant() {
        Some((_, p)) => Some(p),
        None => heap.fields(v).first().copied(),
    }
}

/// The byte of a string of one byte, or -1 (`Op::SwitchChar`).
#[inline(always)]
fn char_code(heap: &Heap, v: &Value) -> i64 {
    match *v {
        Value::Obj(h) => match heap.obj(h) {
            Obj::Str(s) if s.len() == 1 => s.as_bytes()[0] as i64,
            _ => -1,
        },
        _ => -1,
    }
}

/// The target of a switch instruction for value `v`.
#[inline(never)]
fn switch_target(prog: &Program, table: u32, v: i64) -> usize {
    prog.switches[table as usize].target(v)
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

/// The exact `nanos * k`, truncated toward zero; `None` if `k` is not finite or the product overflows.
pub fn dur_scale(nanos: i64, k: f64) -> Option<i64> {
    if !k.is_finite() {
        return None;
    }
    let (mantissa, shift) = float_parts(k);
    let magnitude = u128::from(nanos.unsigned_abs()) * mantissa;
    let scaled = if magnitude == 0 {
        0
    } else if shift >= 0 {
        if shift >= 64 || magnitude > (1u128 << 63) >> shift {
            return None;
        }
        magnitude << shift
    } else {
        magnitude.checked_shr(shift.unsigned_abs()).unwrap_or(0)
    };
    signed_nanos(scaled, (nanos < 0) != k.is_sign_negative())
}

/// The exact `nanos / k`, truncated toward zero; `None` if `k` is NaN or the quotient overflows.
/// The caller rejects a zero `k`.
pub fn dur_div(nanos: i64, k: f64) -> Option<i64> {
    if k.is_nan() {
        return None;
    }
    if k.is_infinite() || nanos == 0 {
        return Some(0);
    }
    let (mantissa, shift) = float_parts(k);
    let n = u128::from(nanos.unsigned_abs());
    let quotient = if shift > 64 {
        // |k| >= 2^64 > |nanos|.
        0
    } else if shift >= 0 {
        n / (mantissa << shift)
    } else {
        // n * 2^t / mantissa, by long division in two steps so that each shift fits in u128.
        let t = shift.unsigned_abs();
        let (head, tail) = (t.min(64), t.saturating_sub(64));
        let (q, r) = ((n << head) / mantissa, (n << head) % mantissa);
        if tail == 0 {
            q
        } else if tail > 63 || q > (1u128 << 63) >> tail {
            return None;
        } else {
            (q << tail) + (r << tail) / mantissa
        }
    };
    signed_nanos(quotient, (nanos < 0) != k.is_sign_negative())
}

/// `(mantissa, shift)` with `|k| == mantissa * 2^shift` exactly, for a finite `k`.
fn float_parts(k: f64) -> (u128, i32) {
    let bits = k.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = (bits & ((1 << 52) - 1)) as u128;
    if exp == 0 { (frac, -1074) } else { (frac | 1 << 52, exp - 1075) }
}

fn signed_nanos(magnitude: u128, negative: bool) -> Option<i64> {
    i64::try_from(if negative { -(magnitude as i128) } else { magnitude as i128 }).ok()
}

fn shift_amount(b: i64) -> Result<u32, &'static str> {
    if (0..64).contains(&b) { Ok(b as u32) } else { Err("invalid: shift out of range") }
}

impl Vm {
    /// Generic arithmetic on runtime values.
    #[inline(never)]
    pub fn arith(&mut self, op: u8, a: Value, b: Value) -> Result<Value, Ctl> {
        use Value::*;
        Ok(match (op, a, b) {
            (0, Int(x), Int(y)) => Int(x.checked_add(y).ok_or_else(|| overflow("integer"))?),
            (1, Int(x), Int(y)) => Int(x.checked_sub(y).ok_or_else(|| overflow("integer"))?),
            (2, Int(x), Int(y)) => Int(x.checked_mul(y).ok_or_else(|| overflow("integer"))?),
            (3, Int(x), Int(y)) => Int(int_div(x, y).map_err(Ctl::from)?),
            (4, Int(x), Int(y)) => Int(int_rem(x, y).map_err(Ctl::from)?),
            (5, Int(x), Int(y)) => Int(int_pow(x, y).map_err(Ctl::from)?),
            (0, Float(x), Float(y)) => Float(x + y),
            (1, Float(x), Float(y)) => Float(x - y),
            (2, Float(x), Float(y)) => Float(x * y),
            (3, Float(x), Float(y)) => Float(x / y),
            (4, Float(x), Float(y)) => Float(float_rem(x, y)),
            (5, Float(x), Float(y)) => Float(x.powf(y)),
            (0, Dur(x), Dur(y)) => Dur(x.checked_add(y).ok_or_else(|| overflow("duration"))?),
            (1, Dur(x), Dur(y)) => Dur(x.checked_sub(y).ok_or_else(|| overflow("duration"))?),
            (2, Dur(x), Int(y)) | (2, Int(y), Dur(x)) => Dur(x.checked_mul(y).ok_or_else(|| overflow("duration"))?),
            (2, Dur(x), Float(y)) | (2, Float(y), Dur(x)) => Dur(dur_scale(x, y).ok_or_else(|| overflow("duration"))?),
            (3, Dur(x), Int(y)) => {
                if y == 0 {
                    return Err(Ctl::fail("invalid", "division by zero"));
                }
                Dur(x.checked_div(y).ok_or_else(|| overflow("duration"))?)
            }
            (3, Dur(x), Float(y)) => {
                if y == 0.0 {
                    return Err(Ctl::fail("invalid", "division by zero"));
                }
                Dur(dur_div(x, y).ok_or_else(|| overflow("duration"))?)
            }
            (3, Dur(x), Dur(y)) => Float(x as f64 / y as f64),
            (0, Obj(_), Obj(_)) => {
                let (oa, ob) = (self.heap.get(a), self.heap.get(b));
                match (oa, ob) {
                    (super::value::Obj::Str(x), super::value::Obj::Str(y)) => {
                        self.heap.check_memory(x.len() + y.len())?;
                        self.meter.charge_bytes(x.len() + y.len())?;
                        let mut s = String::with_capacity(x.len() + y.len());
                        s.push_str(x);
                        s.push_str(y);
                        self.heap.new_str(s)
                    }
                    (super::value::Obj::Bytes(x), super::value::Obj::Bytes(y)) => {
                        self.heap.check_memory(x.len() + y.len())?;
                        self.meter.charge_bytes(x.len() + y.len())?;
                        let mut v = x.to_vec();
                        v.extend_from_slice(y);
                        self.heap.new_bytes(v)
                    }
                    (super::value::Obj::List(x), super::value::Obj::List(y)) => {
                        self.heap.check_memory((x.len() + y.len()) * std::mem::size_of::<Value>())?;
                        self.meter.charge((x.len() + y.len()) as u64)?;
                        let mut v = Vec::with_capacity(x.len() + y.len());
                        v.extend_from_slice(x);
                        v.extend_from_slice(y);
                        self.heap.new_list(v)
                    }
                    _ => return Err(internal("unsupported operands for `+`")),
                }
            }
            _ => return Err(internal(format!("unsupported operands for arithmetic: {a:?}, {b:?}"))),
        })
    }

    /// `Op::NewTuple` of anything but an inline pair.
    #[inline(never)]
    fn new_tuple(&mut self, items: &[Value]) -> Value {
        let items = self.heap.boxed(items);
        self.heap.alloc(Obj::Tuple(items))
    }

    /// `Op::MakeClosure`: a closure of `proto` (`p`) capturing from the
    /// registers `regs`.
    #[inline(never)]
    fn make_closure(&mut self, p: &Proto, proto: u32, regs: &[Value]) -> Value {
        let capture = |&r: &Reg| if r == NO_REG { Value::Unit } else { regs[r as usize] };
        let caps: Box<[Value]> = match *p.captures {
            [a] => self.heap.boxed(&[capture(&a)]),
            [a, b] => self.heap.boxed(&[capture(&a), capture(&b)]),
            _ => p.captures.iter().map(capture).collect(),
        };
        self.heap.alloc(Obj::Closure(proto, caps))
    }

    #[cold]
    #[inline(never)]
    fn key_error(&self, k: Value) -> Ctl {
        Ctl::fail("not_found", format!("key not found: {}", self.debug(k).unwrap_or_default()))
    }

    #[cold]
    #[inline(never)]
    fn try_top(&mut self, fiber: &mut Fiber, ip: usize, prog: &Program, res: Value) -> Trap {
        // `Option::None` has 0 fields; `Result::Err(e)` stores `e` at field 0.
        let Some(e) = self.heap.field(res, 0) else {
            return self.ctl_trap(fiber, ip, Ctl::fail("none", "`?` found None"));
        };
        match display::to_display_metered(prog, &self.heap, &self.meter, e) {
            Ok(text) => self.error_trap(fiber, ip, text),
            Err(c) => self.ctl_trap(fiber, ip, c),
        }
    }

    /// `Op::Internal`: an internal error with constant message `msg`.
    #[cold]
    #[inline(never)]
    fn const_internal(&mut self, fiber: &mut Fiber, ip: usize, prog: &Program, msg: u32) -> Trap {
        let m = match &prog.consts[msg as usize] {
            Const::Str(s) => s.clone(),
            _ => "unknown".into(),
        };
        self.internal_trap(fiber, ip, m)
    }

    /// The trap for an operand of the wrong type (a compiler bug).
    #[cold]
    #[inline(never)]
    fn operand_trap(&mut self, fiber: &mut Fiber, ip: usize, v: Value, want: &str) -> Trap {
        fiber.frames.last_mut().unwrap().ip = ip as u32;
        self.internal_trap(fiber, ip, format!("expected {want}, found {v:?}"))
    }

    /// The trap for a `GetField` on a value without that field (a compiler bug).
    #[cold]
    #[inline(never)]
    fn field_trap(&mut self, fiber: &mut Fiber, ip: usize, v: Value, idx: u16) -> Trap {
        fiber.frames.last_mut().unwrap().ip = ip as u32;
        let msg = format!("field {idx} of {v:?} {:?}", self.heap.get(v));
        self.internal_trap(fiber, ip, msg)
    }

    /// The trap for an error whose message `msg` is `kind: message`.
    #[cold]
    #[inline(never)]
    fn error_trap(&mut self, fiber: &mut Fiber, ip: usize, msg: String) -> Trap {
        Trap::Error { message: msg, trace: self.trace_of(fiber, Some(ip as u32)) }
    }

    /// The trap for an internal error, which marks the engine broken.
    #[cold]
    #[inline(never)]
    fn internal_trap(&mut self, fiber: &mut Fiber, ip: usize, msg: String) -> Trap {
        self.broken = true;
        Trap::Internal { message: msg, trace: self.trace_of(fiber, Some(ip as u32)) }
    }

    /// The trap for a failed metered operation.
    #[cold]
    #[inline(never)]
    fn ctl_trap(&mut self, fiber: &mut Fiber, ip: usize, c: Ctl) -> Trap {
        match c.into_kind() {
            CtlKind::RuntimeError(m) => self.error_trap(fiber, ip, m),
            CtlKind::Internal(m) => self.internal_trap(fiber, ip, m),
            CtlKind::Limit(k) => self.limit_trap(fiber, ip, k),
            CtlKind::Interrupted => Trap::Interrupted,
            other => self.internal_trap(fiber, ip, format!("unexpected {other:?}")),
        }
    }

    /// A limit hit by the instruction before `ip`. Memory checks count
    /// garbage that has not been collected yet, so for the memory limit the
    /// frame is rewound to that instruction and the scheduler collects and
    /// runs it once more (the operations that check memory have no side
    /// effects before the check, other than charging [`call_tick`]).
    #[cold]
    #[inline(never)]
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
        if let Some(msg) = fiber.pending_error.take() {
            let ip = fiber.frames.last().map(|f| f.ip).unwrap_or(0) as usize;
            return Err(self.error_trap(fiber, ip, msg));
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
        // Fails with an error whose message is `kind: message`.
        macro_rules! fail {
            ($msg:expr) => {{
                save!();
                return Err(self.error_trap(fiber, ip, String::from($msg)));
            }};
        }
        macro_rules! internal {
            ($msg:expr) => {{
                save!();
                return Err(self.internal_trap(fiber, ip, String::from($msg)));
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
        // re-executed when the fiber resumes. It charges `call_tick`.
        macro_rules! tick_call {
            () => {
                safe_point!(ip - 1, 1)
            };
        }
        // Calls native `n` with `argc` arguments at absolute slot `start`,
        // writing the result to absolute slot `slot`.
        macro_rules! native_call {
            ($n:expr, $start:expr, $argc:expr, $slot:expr) => {{
                self.meter.sync(fuel.left);
                let r = self.call_native(&natives, fiber, $n, $start, $argc, $slot, ip);
                fuel.left = self.meter.resync();
                match r {
                    Ok(None) => reload!(),
                    Ok(Some(exit)) => return Ok(exit),
                    Err(t) => return Err(t),
                }
            }};
        }
        // Enters a new frame for a script function.
        macro_rules! enter {
            ($proto:expr, $new_base:expr, $ret:expr, $closure:expr) => {{
                if fiber.frames.len() >= self.host.limits.max_call_depth {
                    fail!("limit: stack overflow");
                }
                save!();
                let p = &protos[$proto as usize];
                let nb = $new_base;
                let need = nb + p.nregs as usize;
                if fiber.stack.len() < need
                    && let Err(c) = fiber.grow_stack(need, &mut self.heap)
                {
                    fail_ctl!(c);
                }
                if fiber.frames.len() == fiber.frames.capacity()
                    && let Err(c) = fiber.grow_frames(&mut self.heap)
                {
                    fail_ctl!(c);
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
        // Enters a new frame for closure `$closure` (a handle), copying its
        // captures to the registers after the parameters.
        macro_rules! enter_closure {
            ($proto:expr, $new_base:expr, $ret:expr, $closure:expr) => {{
                enter!($proto, $new_base, $ret, $closure);
                load_captures(&self.heap, $closure, &mut regs[protos[$proto as usize].nparams as usize..]);
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

        'dispatch: loop {
            let op = code[ip];
            ip += 1;
            // Back edges leave the `match` with their offset, to share one
            // safe point; every other instruction continues the loop.
            let back = 'back: {
                match op {
                    Op::Nop => {}
                    Op::Move { dst, src } => r!(dst) = r!(src),
                    Op::LoadUnit { dst } => r!(dst) = Value::Unit,
                    Op::LoadBool { dst, v } => r!(dst) = Value::Bool(v),
                    Op::LoadInt { dst, v } => r!(dst) = Value::Int(v as i64),
                    Op::LoadConst { dst, k } => r!(dst) = self.consts[k as usize],
                    Op::LoadTag { dst, tag } => r!(dst) = Value::Tag(tag),
                    Op::LoadGlobal { dst, g } => {
                        let v = &self.globals[g as usize];
                        if let Value::Uninit = v {
                            fail!(uninit_global(&prog, g));
                        }
                        r!(dst) = *v;
                    }
                    Op::StoreGlobal { g, src } => self.globals[g as usize] = r!(src),
                    Op::NewCell { dst, src } => {
                        let v = r!(src);
                        r!(dst) = self.heap.alloc(Obj::Cell(v));
                    }
                    // Each arm stores: a `Value` merged from a loaded one and
                    // a constant is built in pieces through the stack.
                    Op::LoadCell { dst, cell } => {
                        if let Value::Obj(h) = r!(cell)
                            && let Obj::Cell(v) = self.heap.obj(h)
                        {
                            r!(dst) = *v;
                        }
                    }
                    Op::StoreCell { cell, src } => self.heap.set_cell(r!(cell), r!(src)),
                    Op::MakeClosure { dst, proto } => {
                        let p = &protos[proto as usize];
                        fuel.left -= code_fuel(p.captures.len());
                        r!(dst) = self.make_closure(p, proto, regs);
                    }

                    // ---------------- integer arithmetic
                    Op::AddI { dst, a, b } => match int!(a).checked_add(int!(b)) {
                        Some(v) => r!(dst) = Value::Int(v),
                        None => fail!("overflow: integer overflow"),
                    },
                    Op::SubI { dst, a, b } => match int!(a).checked_sub(int!(b)) {
                        Some(v) => r!(dst) = Value::Int(v),
                        None => fail!("overflow: integer overflow"),
                    },
                    Op::MulI { dst, a, b } => match int!(a).checked_mul(int!(b)) {
                        Some(v) => r!(dst) = Value::Int(v),
                        None => fail!("overflow: integer overflow"),
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
                        None => fail!("overflow: integer overflow"),
                    },
                    Op::MulIK { dst, a, k } => match int!(a).checked_mul(k as i64) {
                        Some(v) => r!(dst) = Value::Int(v),
                        None => fail!("overflow: integer overflow"),
                    },
                    // Floor division and modulo by a positive power of two
                    // are a shift and a mask.
                    Op::DivIK { dst, a, k } if k > 0 && k & (k - 1) == 0 => r!(dst) = Value::Int(int!(a) >> k.trailing_zeros()),
                    Op::DivIK { dst, a, k } => match floor_div(int!(a), k as i64) {
                        Some(v) => r!(dst) = Value::Int(v),
                        None => fail!("overflow: integer overflow"),
                    },
                    Op::RemIK { dst, a, k } if k > 0 && k & (k - 1) == 0 => r!(dst) = Value::Int(int!(a) & (k as i64 - 1)),
                    Op::RemIK { dst, a, k } => r!(dst) = Value::Int(floor_rem(int!(a), k as i64)),
                    Op::NegI { dst, a } => match int!(a).checked_neg() {
                        Some(v) => r!(dst) = Value::Int(v),
                        None => fail!("overflow: integer overflow"),
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
                            Err(c) => fail_ctl!(c),
                        }
                    }
                    Op::Neg { dst, a } => {
                        r!(dst) = match r!(a) {
                            Value::Int(i) => match i.checked_neg() {
                                Some(v) => Value::Int(v),
                                None => fail!("overflow: integer overflow"),
                            },
                            Value::Float(f) => Value::Float(-f),
                            Value::Dur(d) => match d.checked_neg() {
                                Some(v) => Value::Dur(v),
                                None => fail!("overflow: duration overflow"),
                            },
                            other => internal!(format!("cannot negate {other:?}")),
                        }
                    }
                    Op::Not { dst, a } => r!(dst) = Value::Bool(!r!(a).as_bool()),

                    // ---------------- comparisons
                    Op::EqI { dst, a, b } => r!(dst) = Value::Bool(int!(a) == int!(b)),
                    Op::NeI { dst, a, b } => r!(dst) = Value::Bool(int!(a) != int!(b)),
                    Op::LtI { dst, a, b } => r!(dst) = Value::Bool(int!(a) < int!(b)),
                    Op::LeI { dst, a, b } => r!(dst) = Value::Bool(int!(a) <= int!(b)),
                    Op::EqIK { dst, a, k } => r!(dst) = Value::Bool(int!(a) == k as i64),
                    Op::NeIK { dst, a, k } => r!(dst) = Value::Bool(int!(a) != k as i64),
                    Op::LtIK { dst, a, k } => r!(dst) = Value::Bool(int!(a) < k as i64),
                    Op::LeIK { dst, a, k } => r!(dst) = Value::Bool(int!(a) <= k as i64),
                    Op::GtIK { dst, a, k } => r!(dst) = Value::Bool(int!(a) > k as i64),
                    Op::GeIK { dst, a, k } => r!(dst) = Value::Bool(int!(a) >= k as i64),
                    Op::EqF { dst, a, b } => r!(dst) = Value::Bool(float!(a) == float!(b)),
                    Op::NeF { dst, a, b } => r!(dst) = Value::Bool(float!(a) != float!(b)),
                    Op::LtF { dst, a, b } => r!(dst) = Value::Bool(float!(a) < float!(b)),
                    Op::LeF { dst, a, b } => r!(dst) = Value::Bool(float!(a) <= float!(b)),
                    Op::JNeq { a, b, off } if let Some(eq) = quick_eq(&self.heap, &r!(a), &r!(b)) => {
                        if !eq {
                            jump!(off)
                        }
                    }
                    Op::JNne { a, b, off } if let Some(eq) = quick_eq(&self.heap, &r!(a), &r!(b)) => {
                        if eq {
                            jump!(off)
                        }
                    }
                    Op::JNeqK { a, k, off } => match self.eq_const(&r!(a), k) {
                        Ok(true) => {}
                        Ok(false) => jump!(off),
                        Err(c) => fail_ctl!(c),
                    },
                    Op::JNneK { a, k, off } => match self.eq_const(&r!(a), k) {
                        Ok(true) => jump!(off),
                        Ok(false) => {}
                        Err(c) => fail_ctl!(c),
                    },
                    Op::Eq { dst, a, b } if let Some(eq) = quick_eq(&self.heap, &r!(a), &r!(b)) => r!(dst) = Value::Bool(eq),
                    Op::Ne { dst, a, b } if let Some(eq) = quick_eq(&self.heap, &r!(a), &r!(b)) => r!(dst) = Value::Bool(!eq),
                    // Metered and out of line (`op_compare`); the branches
                    // `JNeq` and `JNne` share the arm to keep one call site.
                    Op::Eq { .. } | Op::Ne { .. } | Op::Lt { .. } | Op::Le { .. } | Op::JNeq { .. } | Op::JNne { .. } => {
                        match self.op_compare(&prog, op, regs) {
                            Ok(off) => jump!(off),
                            Err(c) => fail_ctl!(c),
                        }
                    }

                    // ---------------- control flow
                    Op::Jmp { off } => jump!(off),
                    Op::Loop { off } => break 'back off,
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
                    Op::JNltIK { a, k, off } => {
                        if int!(a) >= k as i64 {
                            jump!(off)
                        }
                    }
                    Op::JNleIK { a, k, off } => {
                        if int!(a) > k as i64 {
                            jump!(off)
                        }
                    }
                    Op::JNgtIK { a, k, off } => {
                        if int!(a) <= k as i64 {
                            jump!(off)
                        }
                    }
                    Op::JNgeIK { a, k, off } => {
                        if int!(a) < k as i64 {
                            jump!(off)
                        }
                    }
                    Op::JNeqIK { a, k, off } => {
                        if int!(a) != k as i64 {
                            jump!(off)
                        }
                    }
                    Op::JNneIK { a, k, off } => {
                        if int!(a) == k as i64 {
                            jump!(off)
                        }
                    }
                    Op::JNotSome { src, dst, off } => match self.heap.payload_if(&r!(src), prog.ids.some) {
                        Value::Uninit => jump!(off),
                        p => r!(dst) = p,
                    },
                    Op::JNotOk { src, dst, off } => match self.heap.payload_if(&r!(src), prog.ids.ok) {
                        Value::Uninit => jump!(off),
                        p => r!(dst) = p,
                    },
                    Op::JmpIfBack { cond, off } => {
                        if r!(cond).as_bool() {
                            break 'back off;
                        }
                    }
                    Op::JmpIfNotBack { cond, off } => {
                        if !r!(cond).as_bool() {
                            break 'back off;
                        }
                    }
                    Op::JNltIBack { a, b, off } => {
                        if int!(a) >= int!(b) {
                            break 'back off as i32;
                        }
                    }
                    Op::JNleIBack { a, b, off } => {
                        if int!(a) > int!(b) {
                            break 'back off as i32;
                        }
                    }
                    Op::JNeqIBack { a, b, off } => {
                        if int!(a) != int!(b) {
                            break 'back off as i32;
                        }
                    }
                    Op::JNneIBack { a, b, off } => {
                        if int!(a) == int!(b) {
                            break 'back off as i32;
                        }
                    }
                    Op::JNltIKBack { a, k, off } => {
                        if int!(a) >= k as i64 {
                            break 'back off as i32;
                        }
                    }
                    Op::JNleIKBack { a, k, off } => {
                        if int!(a) > k as i64 {
                            break 'back off as i32;
                        }
                    }
                    Op::JNgtIKBack { a, k, off } => {
                        if int!(a) <= k as i64 {
                            break 'back off as i32;
                        }
                    }
                    Op::JNgeIKBack { a, k, off } => {
                        if int!(a) < k as i64 {
                            break 'back off as i32;
                        }
                    }
                    Op::JNeqIKBack { a, k, off } => {
                        if int!(a) != k as i64 {
                            break 'back off as i32;
                        }
                    }
                    Op::JNneIKBack { a, k, off } => {
                        if int!(a) == k as i64 {
                            break 'back off as i32;
                        }
                    }
                    Op::JSomeBack { src, dst, off } => {
                        let p = self.heap.payload_if(&r!(src), prog.ids.some);
                        if !matches!(p, Value::Uninit) {
                            r!(dst) = p;
                            break 'back off as i32;
                        }
                    }
                    Op::SwitchVariant { src, table } => ip = switch_target(&prog, table, self.heap.variant_id(r!(src)) as i64),
                    Op::SwitchInt { src, table } => ip = switch_target(&prog, table, int!(src)),
                    Op::SwitchChar { src, table } => ip = switch_target(&prog, table, char_code(&self.heap, &r!(src))),
                    Op::JNotVariant { src, variant, off } => {
                        if self.heap.variant_id(r!(src)) != variant as u32 {
                            jump!(off)
                        }
                    }

                    // ---------------- calls
                    // `tick_call!` may reload `regs`, which `enter!` overwrites unread.
                    #[allow(unused_assignments)]
                    Op::CallDirect { base: b, proto } => {
                        tick_call!();
                        let nb = base + b as usize;
                        enter!(proto, nb, nb, NO_CLOSURE);
                    }
                    Op::CallR { callee: c, base: b, dst, argc } => {
                        tick_call!();
                        let callee = r!(c);
                        match self.heap.get(callee) {
                            Obj::Closure(p, _) => {
                                let p = *p;
                                let Value::Obj(h) = callee else { unreachable!() };
                                let nb = base + b as usize;
                                enter_closure!(p, nb, base + dst as usize, h);
                            }
                            Obj::IterFn(_) => {
                                let next = self.iter_next(callee);
                                r!(dst) = self.option(next);
                                match code.get(ip) {
                                    Some(&Op::JNotSome { src, dst: d, off }) if src == dst => {
                                        ip += 1;
                                        match next {
                                            Some(x) => r!(d) = x,
                                            None => jump!(off),
                                        }
                                    }
                                    Some(&Op::JSomeBack { src, dst: d, off }) if src == dst => {
                                        ip += 1;
                                        if let Some(x) = next {
                                            r!(d) = x;
                                            break 'back off as i32;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Obj::NativeFn(n) => {
                                let n = *n;
                                native_call!(n, base + b as usize, argc as usize, base + dst as usize);
                            }
                            _ => internal!("call of a non-function"),
                        }
                    }
                    // `tick_call!` may reload `regs`, which `enter!` overwrites unread.
                    #[allow(unused_assignments)]
                    Op::CallDirectTo { base: b, dst, proto } => {
                        tick_call!();
                        let nb = base + b as usize;
                        enter!(proto as u32, nb, base + dst as usize, NO_CLOSURE);
                    }
                    // `tick_call!` may reload `regs`, which `enter!` overwrites unread.
                    #[allow(unused_assignments)]
                    Op::CallSelf { base: b, dst } => {
                        tick_call!();
                        let fr = *fiber.frames.last().unwrap();
                        let nb = base + b as usize;
                        enter_closure!(fr.proto, nb, base + dst as usize, fr.closure);
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
                    // `tick_call!` may reload `regs`, which is reloaded again below unread.
                    #[allow(unused_assignments)]
                    Op::TailCallDirect { base: b, proto } => {
                        tick_call!();
                        let p = &protos[proto as usize];
                        let n = p.nparams as usize;
                        let src = b as usize;
                        // Grow first: the call is retried after a memory limit error.
                        let need = base + p.nregs as usize;
                        if fiber.stack.len() < need
                            && let Err(c) = fiber.grow_stack(need, &mut self.heap)
                        {
                            fail_ctl!(c);
                        }
                        reload!();
                        shift_args(regs, src, n);
                        let top = fiber.frames.last_mut().unwrap();
                        top.proto = proto;
                        top.closure = NO_CLOSURE;
                        code = &p.code;
                        fuel.left -= code_fuel(code.len());
                        ip = 0;
                    }
                    Op::TailCall { callee: c, base: b, argc } => {
                        tick_call!();
                        let callee = r!(c);
                        match self.heap.get(callee) {
                            Obj::Closure(p, _) => {
                                let proto = *p;
                                let pr = &protos[proto as usize];
                                let n = argc as usize;
                                let src = b as usize;
                                // Grow first: the call is retried after a memory limit error.
                                let need = base + pr.nregs as usize;
                                if fiber.stack.len() < need
                                    && let Err(c) = fiber.grow_stack(need, &mut self.heap)
                                {
                                    fail_ctl!(c);
                                }
                                reload!();
                                shift_args(regs, src, n);
                                let Value::Obj(h) = callee else { unreachable!() };
                                load_captures(&self.heap, h, &mut regs[pr.nparams as usize..]);
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
                                let start = base + b as usize;
                                native_call!(n, start, argc as usize, start);
                            }
                            Obj::IterFn(_) => r!(b) = self.iter_step(callee),
                            _ => internal!("call of a non-function"),
                        }
                    }
                    Op::Ret { src } => {
                        let v = r!(src);
                        do_return!(v);
                    }
                    Op::RetVariant { src, variant } => {
                        let v = match Value::variant1(variant as u32, r!(src)) {
                            Some(v) => v,
                            None => self.heap.new_variant(variant as u32, &[r!(src)]),
                        };
                        do_return!(v);
                    }
                    Op::RetUnit => do_return!(Value::Unit),
                    Op::Unwind => {
                        fiber.frames.pop();
                        if !self.unwind_to_pad(fiber, true) {
                            return Ok(Exit::Unwound);
                        }
                        let fr = *fiber.frames.last().unwrap();
                        code = &protos[fr.proto as usize].code;
                        ip = fr.ip as usize;
                        base = fr.base as usize;
                        reload!();
                    }

                    // ---------------- data structures
                    Op::NewList { dst, start, count } => {
                        let s = start as usize;
                        let items = regs[s..s + count as usize].to_vec();
                        r!(dst) = self.heap.new_list(items);
                    }
                    Op::NewTuple { dst, start, count } => {
                        let s = start as usize;
                        r!(dst) = match regs[s..s + count as usize] {
                            [Value::Int(a), Value::Int(b)] if let Ok(a) = i32::try_from(a) => Value::Pair(a, b),
                            _ => self.new_tuple(&regs[s..s + count as usize]),
                        };
                    }
                    Op::NewStruct { dst, start, def } => {
                        let n = prog.types[def as usize].fields.len();
                        let s = start as usize;
                        let items = self.heap.boxed(&regs[s..s + n]);
                        r!(dst) = self.heap.alloc(Obj::Struct(def as u32, items));
                    }
                    Op::NewVariant { dst, start, variant } => {
                        let n = prog.variants[variant as usize].arity;
                        let s = start as usize;
                        r!(dst) = match Value::variant1(variant as u32, r!(s)) {
                            Some(v) if n == 1 => v,
                            _ => self.heap.new_variant(variant as u32, &regs[s..s + n]),
                        };
                    }
                    Op::NewMap { dst, start, count } => {
                        let m = self.heap.alloc(Obj::Map(Box::default()));
                        let s = start as usize;
                        for i in 0..count as usize {
                            let k = regs[s + 2 * i];
                            let v = regs[s + 2 * i + 1];
                            if let Err(c) = self.map_insert(m, k, v) {
                                fail_ctl!(c);
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
                    Op::GetField { dst, obj, idx } => match r!(obj) {
                        Value::Obj(h) => match self.heap.obj(h) {
                            Obj::Struct(_, f) | Obj::Tuple(f) | Obj::Variant(_, f) | Obj::Closure(_, f) if (idx as usize) < f.len() => {
                                r!(dst) = f[idx as usize];
                            }
                            _ => {
                                let v = r!(obj);
                                return Err(self.field_trap(fiber, ip, v, idx));
                            }
                        },
                        Value::Pair(a, b) => r!(dst) = Value::Int(if idx == 0 { a as i64 } else { b }),
                        v => match inline_field(&v, idx) {
                            Value::Uninit => return Err(self.field_trap(fiber, ip, v, idx)),
                            p => r!(dst) = p,
                        },
                    },
                    Op::Payload { dst, src } => {
                        let v = r!(src);
                        match payload(&self.heap, v) {
                            Some(p) => r!(dst) = p,
                            None => return Err(self.operand_trap(fiber, ip, v, "a variant with a payload")),
                        }
                    }
                    Op::SetField { obj, idx, src } => {
                        let o = r!(obj);
                        let v = r!(src);
                        match self.heap.get_mut_storing(o, &[v]) {
                            Obj::Struct(_, f) => f[idx as usize] = v,
                            _ => internal!("set field on non-struct"),
                        }
                    }
                    Op::Index { dst, obj, idx } => {
                        let o = r!(obj);
                        let i = int!(idx);
                        let l = self.heap.list(o);
                        match to_index(i).and_then(|at| l.get(at)) {
                            Some(&v) => r!(dst) = v,
                            None => fail!(index_error(i, l.len())),
                        }
                    }
                    Op::IndexMap { dst, obj, key } => match dense_get(&self.heap, &r!(obj), &r!(key)) {
                        Dense::Hit(v) => r!(dst) = *v,
                        _ => match self.map_index(r!(obj), r!(key)) {
                            Ok(v) => r!(dst) = v,
                            Err(c) => fail_ctl!(c),
                        },
                    },
                    Op::SetIndex { obj, idx, src } => {
                        let o = r!(obj);
                        let i = int!(idx);
                        let v = r!(src);
                        let at = to_index(i).unwrap_or(usize::MAX);
                        let Some(l) = self.heap.list_mut_storing(o, at, v) else { internal!("set index on non-list") };
                        match l.get_mut(at) {
                            Some(slot) => *slot = v,
                            None => {
                                let n = l.len();
                                fail!(index_error(i, n));
                            }
                        }
                    }
                    Op::SetIndexMap { obj, key, src } => {
                        let (m, k, v) = (r!(obj), r!(key), r!(src));
                        if let Err(c) = self.map_set(m, k, v) {
                            fail_ctl!(c);
                        }
                    }
                    Op::Slice { dst, base: b, flags } => {
                        let o = r!(b);
                        let start = if flags & 1 != 0 { Some(int!(b + 1)) } else { None };
                        let end = if flags & 2 != 0 { Some(int!(b + 2)) } else { None };
                        match self.slice(o, start, end, flags & 4 != 0) {
                            Ok(v) => r!(dst) = v,
                            Err(c) => fail_ctl!(c),
                        }
                    }
                    Op::ListPush { list, src } => {
                        let (l, v) = (r!(list), r!(src));
                        if self.heap.list_push(l, v).is_err() {
                            return Err(self.limit_trap(fiber, ip, LimitKind::Memory));
                        }
                    }
                    Op::CheckRepeat { src } => {
                        let n = int!(src);
                        if n < 0 {
                            fail!(format!("invalid: negative repeat count: {n}"));
                        }
                        if self.heap.would_exceed(to_usize_saturating(n).saturating_mul(std::mem::size_of::<Value>())) {
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
                    Op::ListGet { dst, list, idx } => {
                        let v = to_index(int!(idx)).and_then(|i| self.heap.list(r!(list)).get(i)).copied();
                        r!(dst) = self.option(v);
                    }
                    Op::ListGetOr { dst, list, idx } => {
                        if let Some(&v) = to_index(int!(idx)).and_then(|i| self.heap.list(r!(list)).get(i)) {
                            r!(dst) = v;
                        }
                    }
                    Op::ListPop { dst, list } => {
                        let v = self.heap.list_shrink(r!(list)).and_then(Vec::pop);
                        r!(dst) = self.option(v);
                    }
                    // `if let Some(x) = map.get(key)` tests the result right away.
                    Op::MapGet { dst, map, key } => match dense_get(&self.heap, &r!(map), &r!(key)) {
                        Dense::Hit(v) => {
                            let v = *v;
                            r!(dst) = self.some(v);
                            if let Some(&Op::JNotSome { src, dst: d, .. }) = code.get(ip)
                                && src == dst
                            {
                                ip += 1;
                                r!(d) = v;
                            }
                        }
                        Dense::Miss => {
                            r!(dst) = self.none();
                            if let Some(&Op::JNotSome { src, off, .. }) = code.get(ip)
                                && src == dst
                            {
                                ip += 1;
                                jump!(off);
                            }
                        }
                        Dense::Unknown => match self.map_get_option(r!(map), r!(key)) {
                            Ok(v) => r!(dst) = v,
                            Err(c) => fail_ctl!(c),
                        },
                    },
                    Op::MapGetTuple { dst, map, start, n } => {
                        let s = start as usize;
                        match self.map_get_tuple(r!(map), &regs[s..s + n as usize]) {
                            Ok(v) => r!(dst) = v,
                            Err(c) => fail_ctl!(c),
                        }
                    }
                    Op::MapHasTuple { dst, map, start, n } => {
                        let s = start as usize;
                        match self.map_has_tuple(r!(map), &regs[s..s + n as usize]) {
                            Ok(b) => r!(dst) = Value::Bool(b),
                            Err(c) => fail_ctl!(c),
                        }
                    }
                    Op::MapGetOr { dst, map, key } => match dense_get(&self.heap, &r!(map), &r!(key)) {
                        Dense::Hit(v) => r!(dst) = *v,
                        Dense::Miss => {}
                        Dense::Unknown => {
                            let (m, k) = (r!(map), r!(key));
                            if let Err(c) = self.map_get_or(m, k, &mut r!(dst)) {
                                fail_ctl!(c);
                            }
                        }
                    },
                    Op::MapHas { dst, map, key } => match self.map_has(r!(map), r!(key)) {
                        Ok(b) => r!(dst) = Value::Bool(b),
                        Err(c) => fail_ctl!(c),
                    },
                    Op::SetHas { dst, set, key } => match self.set_contains(r!(set), r!(key)) {
                        Ok(b) => r!(dst) = Value::Bool(b),
                        Err(c) => fail_ctl!(c),
                    },
                    Op::SetAdd { dst, set, key } => match self.set_add(r!(set), r!(key)) {
                        Ok(b) => r!(dst) = Value::Bool(b),
                        Err(c) => fail_ctl!(c),
                    },
                    Op::IntToFloat { dst, src } => r!(dst) = Value::Float(int!(src) as f64),
                    Op::AppendStr { local, src } => {
                        let add = r!(src);
                        if let Err(c) = self.meter.charge_bytes(self.heap.append_copies(r!(local), add)) {
                            fail_ctl!(c);
                        }
                        match self.heap.append_str(r!(local), add) {
                            Ok(v) => r!(local) = v,
                            Err(k) => fail_ctl!(Ctl::limit(k)),
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
                    Op::ForRange { var, base: b, off } => {
                        let i = int!(b);
                        if i < int!(b + 1) {
                            r!(var) = Value::Int(i);
                            r!(b) = Value::Int(i + 1);
                        } else {
                            jump!(off);
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
                    Op::ForListBack { elem, base: b, off } => {
                        let i = int!(b + 1);
                        let n = int!(b + 2);
                        let l = self.heap.list(r!(b));
                        if i < n && (i as usize) < l.len() {
                            let v = l[i as usize];
                            r!(elem) = v;
                            r!(b + 1) = Value::Int(i + 1);
                            break 'back off as i32;
                        }
                    }
                    Op::ForRangeBack { var, base: b, off } => {
                        let i = int!(b);
                        if i < int!(b + 1) {
                            r!(var) = Value::Int(i);
                            r!(b) = Value::Int(i + 1);
                            break 'back off as i32;
                        }
                    }
                    Op::ForRangeInclBack { var, base: b, off } => {
                        let i = int!(b);
                        if !r!(b + 2).as_bool() && i <= int!(b + 1) {
                            r!(var) = Value::Int(i);
                            if i == int!(b + 1) {
                                r!(b + 2) = Value::Bool(true);
                            } else {
                                r!(b) = Value::Int(i + 1);
                            }
                            break 'back off as i32;
                        }
                    }
                    Op::ForSet { elem, base: b, off } => {
                        let it = r!(b);
                        if !self.set_step(it, &mut r!(elem)) {
                            jump!(off);
                        }
                    }
                    Op::ForSetBack { elem, base: b, off } => {
                        let it = r!(b);
                        if self.set_step(it, &mut r!(elem)) {
                            break 'back off as i32;
                        }
                    }
                    Op::ForMap { key, base: b, off } => {
                        let (it, k) = (r!(b), key as usize);
                        if !self.map_step(it, &mut regs[k..k + 2]) {
                            jump!(off);
                        }
                    }
                    Op::ForMapBack { key, base: b, off } => {
                        let (it, k) = (r!(b), key as usize);
                        if self.map_step(it, &mut regs[k..k + 2]) {
                            break 'back off as i32;
                        }
                    }
                    Op::ForChars { elem, base: b, off } => match self.next_char(&r!(b), &r!(b + 1)) {
                        Some((c, at)) => {
                            r!(elem) = c;
                            r!(b + 1) = Value::Int(at);
                        }
                        None => jump!(off),
                    },
                    Op::ForCharsBack { elem, base: b, off } => {
                        if let Some((c, at)) = self.next_char(&r!(b), &r!(b + 1)) {
                            r!(elem) = c;
                            r!(b + 1) = Value::Int(at);
                            break 'back off as i32;
                        }
                    }
                    Op::UnpackRange { base: b, src } => match unpack_range(self.heap.get(r!(src))) {
                        Some((first, last, done)) => {
                            r!(b) = Value::Int(first);
                            r!(b + 1) = Value::Int(last);
                            r!(b + 2) = Value::Bool(done);
                        }
                        None => internal!("not a range"),
                    },
                    Op::NewIter { dst, src } => match self.container_iter(r!(src)) {
                        Ok(v) => r!(dst) = v,
                        Err(c) => fail_ctl!(c),
                    },
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
                    Op::ListElem { dst, list, idx } => {
                        let l = self.heap.list(r!(list));
                        let i = if idx < 0 { l.len() as isize + idx as isize } else { idx as isize };
                        if let Some(v) = l.get(i as usize) {
                            r!(dst) = *v;
                        }
                    }
                    Op::ListRest { dst, list, before, after } => match self.list_rest(r!(list), before as usize, after as usize) {
                        Ok(v) => r!(dst) = v,
                        Err(c) => fail_ctl!(c),
                    },

                    // ---------------- strings
                    Op::ToStr { dst, src } => {
                        let v = r!(src);
                        if let Value::Obj(_) = v
                            && let Obj::Str(_) = self.heap.get(v)
                        {
                            r!(dst) = v;
                            continue 'dispatch;
                        }
                        match self.op_display(&prog, v, false) {
                            Ok(v) => r!(dst) = v,
                            Err(c) => fail_ctl!(c),
                        }
                    }
                    Op::Debug { dst, src } => match self.op_display(&prog, r!(src), true) {
                        Ok(v) => r!(dst) = v,
                        Err(c) => fail_ctl!(c),
                    },
                    Op::Interp { dst, tmpl } => match self.op_interp(&prog, tmpl, regs) {
                        Ok(v) => r!(dst) = v,
                        Err(c) => fail_ctl!(c),
                    },
                    Op::AppendInterp { local, tmpl } => match self.op_append_interp(&prog, tmpl, local, regs) {
                        Ok(v) => r!(local) = v,
                        Err(c) => fail_ctl!(c),
                    },
                    Op::Describe { base, tmpl } => match self.op_describe(&prog, tmpl, &regs[base as usize..]) {
                        Ok(v) => r!(base) = v,
                        Err(c) => fail_ctl!(c),
                    },

                    // ---------------- misc
                    Op::Spawn { dst, closure } => {
                        let c = r!(closure);
                        match self.spawn_task(c) {
                            Ok(t) => r!(dst) = t,
                            Err(c) => fail_ctl!(c),
                        }
                    }
                    Op::TryTop { src } => {
                        save!();
                        let e = r!(src);
                        return Err(self.try_top(fiber, ip, &prog, e));
                    }
                    Op::Internal { msg } => {
                        save!();
                        return Err(self.const_internal(fiber, ip, &prog, msg));
                    }
                }
                continue 'dispatch;
            };
            jump!(back);
            // Long loops cost more (`meter::loop_fuel`).
            safe_point!(ip, loop_fuel(back));
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
    /// Arguments are borrowed from the fiber's stack: the running fiber is not
    /// in `self.fibers`, so natives cannot observe or move it. A native that
    /// exhausts the slice's fuel preempts the task once it returns.
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
        let frame = fiber.frames.last_mut().unwrap();
        frame.ip = ip as u32;
        self.caller_site = (frame.proto, frame.ip);
        let mut r = Self::invoke_native(self, natives, n, &fiber.stack[start..start + argc]);
        self.heap.fail_next_check.set(false);
        if r.is_memory_limit() || matches!(&r, Err(c) if matches!(c.kind(), CtlKind::Collect)) {
            // Native functions check memory and open-resource limits against
            // counts that include uncollected garbage. Run a full collection
            // and retry once.
            self.collect_all_garbage(Some(fiber));
            self.collected = true;
            r = Self::invoke_native(self, natives, n, &fiber.stack[start..start + argc]);
            self.collected = false;
        }
        if !matches!(&r, Err(c) if matches!(c.kind(), CtlKind::NeedPermission(_))) {
            self.forget_once_answers();
        }
        let r = match r {
            Ok(v) => {
                fiber.stack[slot] = v;
                self.enforce_limit(Some(fiber)).map_err(Trap::Limit)?;
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
        match r.kind() {
            CtlKind::Yield => {
                self.recycle(r);
                fiber.stack[slot] = Value::Unit;
                return Ok(Some(Exit::YieldNow));
            }
            CtlKind::Blocked => {
                self.recycle(r);
                fiber.result_slot = slot;
                fiber.wgen = self.wait_gen;
                if let Some(r) = self.pending_race.take() {
                    fiber.race = r;
                }
                return Ok(Some(Exit::Blocked));
            }
            _ => {}
        }
        match r.into_kind() {
            CtlKind::RuntimeError(m) => Err(self.error_trap(fiber, ip, m)),
            CtlKind::Internal(m) => Err(self.internal_trap(fiber, ip, m)),
            // Already retried after a collection.
            CtlKind::Limit(k) => Err(Trap::Limit(k)),
            CtlKind::Interrupted => Err(Trap::Interrupted),
            CtlKind::Exit(c) => Ok(Some(Exit::Terminate(c))),
            CtlKind::NeedPermission(cap) => {
                // Retry the call instruction after the prompt.
                fiber.frames.last_mut().unwrap().ip = (ip - 1) as u32;
                Ok(Some(Exit::Prompt(cap)))
            }
            CtlKind::Async(fut) => {
                fiber.result_slot = slot;
                Ok(Some(Exit::Async(fut)))
            }
            CtlKind::Yield | CtlKind::Blocked => unreachable!("handled above"),
            CtlKind::Collect => Err(self.internal_trap(fiber, ip, "a native asked for a collection twice".into())),
        }
    }

    /// `Op::StrClass`: the character-class tests of strings.
    #[inline(never)]
    fn str_class(&self, v: Value, class: u8) -> Result<bool, Stop> {
        let t = self.heap.str(v);
        if let &[b] = t.as_bytes()
            && b.is_ascii()
        {
            // One ASCII character: the common case.
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

    /// `==`, `!=`, `<` and `<=` on generic values: writes the destination
    /// register, or returns the offset of a taken `JNeq`/`JNne` branch
    /// (else 0). Composite values are compared with metering, so deep
    /// comparisons cost fuel and can be interrupted.
    #[inline(never)]
    fn op_compare(&mut self, prog: &Program, op: Op, regs: &mut [Value]) -> Result<i16, Ctl> {
        let (Op::Eq { a, b, .. }
        | Op::Ne { a, b, .. }
        | Op::Lt { a, b, .. }
        | Op::Le { a, b, .. }
        | Op::JNeq { a, b, .. }
        | Op::JNne { a, b, .. }) = op
        else {
            return Err(internal("not a comparison"));
        };
        let (x, y) = (regs[a as usize], regs[b as usize]);
        // Anything but two plain scalars may be arbitrarily large (inline
        // variants such as `Some(x)` hold heap values too).
        let scalar = |v: Value| matches!(v, Value::Unit | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Dur(_) | Value::Tag(_));
        let composite = !(scalar(x) && scalar(y));
        let v = match op {
            Op::Eq { .. } | Op::Ne { .. } | Op::JNeq { .. } | Op::JNne { .. } => {
                let eq = if composite { values_eq_metered(&self.heap, &self.meter, x, y)? } else { values_eq(&self.heap, x, y) };
                eq == matches!(op, Op::Eq { .. } | Op::JNeq { .. })
            }
            Op::Lt { .. } if composite => values_lt_metered(prog, &self.heap, &self.meter, x, y)?,
            Op::Lt { .. } => values_lt(prog, &self.heap, x, y),
            _ if composite => values_le_metered(prog, &self.heap, &self.meter, x, y)?,
            _ => values_le(prog, &self.heap, x, y),
        };
        Ok(match op {
            Op::Eq { dst, .. } | Op::Ne { dst, .. } | Op::Lt { dst, .. } | Op::Le { dst, .. } => {
                regs[dst as usize] = Value::Bool(v);
                0
            }
            Op::JNeq { off, .. } | Op::JNne { off, .. } if !v => off,
            _ => 0,
        })
    }

    /// `v == consts[k]`.
    #[inline(always)]
    fn eq_const(&self, v: &Value, k: u16) -> Result<bool, Ctl> {
        let c = &self.consts[k as usize];
        match quick_eq(&self.heap, v, c) {
            Some(eq) => Ok(eq),
            None => self.eq_slow(*v, *c),
        }
    }

    #[inline(never)]
    fn eq_slow(&self, a: Value, b: Value) -> Result<bool, Ctl> {
        values_eq_metered(&self.heap, &self.meter, a, b)
    }

    /// `ToStr` (display) and `Debug` of a value, as a new string.
    #[inline(never)]
    fn op_display(&mut self, prog: &Program, v: Value, debug: bool) -> Result<Value, Ctl> {
        let s = if debug {
            display::to_debug_metered(prog, &self.heap, &self.meter, v)?
        } else {
            display::to_display_metered(prog, &self.heap, &self.meter, v)?
        };
        Ok(self.heap.new_str(s))
    }

    /// The text of template `tmpl` with the values of the registers `regs`.
    fn render_template(&self, prog: &Program, tmpl: u32, regs: &[Value]) -> Result<String, Ctl> {
        let t = &prog.templates[tmpl as usize];
        self.meter.charge_bytes(t.text_len)?;
        let mut out = String::with_capacity(t.text_len + 8 * t.pieces.len());
        let mut scratch = Scratch::new();
        for p in &t.pieces {
            match p {
                Piece::Text(s) => out.push_str(s),
                Piece::Value(r) => display::write_display_metered(prog, &self.heap, &self.meter, regs[*r as usize], &mut out)?,
                Piece::Format(r, spec) => {
                    out.push_str(&display::format_value_metered(prog, &self.heap, &self.meter, regs[*r as usize], spec)?)
                }
            }
            scratch.check(&self.heap, out.len())?;
        }
        self.heap.check_memory(out.len())?;
        Ok(out)
    }

    /// `Op::Describe`: instantiates a type description template with the
    /// descriptions at the start of `args`.
    #[inline(never)]
    fn op_describe(&mut self, prog: &Program, tmpl: u32, args: &[Value]) -> Result<Value, Ctl> {
        let t = &prog.desc_templates[tmpl as usize];
        self.meter.charge(t.size() as u64)?;
        let args = &args[..t.arity()];
        let mut descs = Vec::with_capacity(args.len());
        for &a in args {
            match self.heap.get(a) {
                Obj::TypeDesc(d) => descs.push(d),
                _ => return Err(internal("`Op::Describe` expected type descriptions")),
            }
        }
        let d = Obj::TypeDesc(t.instantiate(&descs, args.into())?);
        self.check_memory(d.size())?;
        Ok(self.heap.alloc(d))
    }

    /// `Op::Interp`: string interpolation into one buffer.
    #[inline(never)]
    fn op_interp(&mut self, prog: &Program, tmpl: u32, regs: &[Value]) -> Result<Value, Ctl> {
        let s = self.render_template(prog, tmpl, regs)?;
        Ok(self.heap.new_str(s))
    }

    /// `Op::AppendInterp`: string interpolation appended to the builder
    /// local in register `local` (charged like `AppendStr`). Returns the
    /// local's new value.
    #[inline(never)]
    fn op_append_interp(&mut self, prog: &Program, tmpl: u32, local: Reg, regs: &[Value]) -> Result<Value, Ctl> {
        let s = self.render_template(prog, tmpl, regs)?;
        let cur = regs[local as usize];
        let copied = match self.heap.get(cur) {
            Obj::StrBuf(_) => s.len(),
            _ => self.heap.str(cur).len() + s.len(),
        };
        self.meter.charge_bytes(copied)?;
        self.heap.append_text(cur, &s).map_err(Ctl::limit)
    }

    /// Calls native `n`. A Rust panic in it is caught by the scheduler
    /// around the whole slice (`crate::unwind`).
    #[inline(always)]
    fn invoke_native(&mut self, natives: &[NativeEntry], n: u32, args: &[Value]) -> NResult {
        let entry = &natives[n as usize];
        entry.f.call(self, args)
    }

    /// Waits for `on` (a native's result): its value if it is there
    /// already, or else `Blocked`, having registered the running task's wait.
    pub fn block(&mut self, on: BlockOn) -> NResult {
        let g = self.next_gen();
        match self.block_on(self.current, g, on)? {
            Some(v) => Ok(v),
            None => {
                self.wait_gen = g;
                Err(self.signal(CtlKind::Blocked))
            }
        }
    }

    /// Registers a blocking wait for fiber `me`; returns a value if it can be
    /// satisfied immediately.
    fn block_on(&mut self, me: u32, g: u64, b: BlockOn) -> Result<Option<Value>, Ctl> {
        match b {
            BlockOn::Join(task) => {
                let (fid, state, result) = match self.heap.get(task) {
                    Obj::Task(t) => (t.fiber, t.state, t.result),
                    _ => return Err(internal("not a task")),
                };
                match state {
                    TaskState::Done => Ok(Some(result)),
                    TaskState::Cancelled => Err(Ctl::fail("cancelled", "task cancelled")),
                    TaskState::Running => {
                        if fid == me {
                            return Err(Ctl::fail("deadlock", "a task cannot wait for itself"));
                        }
                        self.add_waiter(fid, (me, g))?;
                        Ok(None)
                    }
                }
            }
            BlockOn::Race(tasks) => {
                if tasks.is_empty() {
                    return Err(Ctl::fail("invalid", "task.race called with an empty list"));
                }
                // One waiter per task, and the list of tasks to cancel.
                self.meter.charge(tasks.len() as u64)?;
                self.heap.check_memory(tasks.len() * (std::mem::size_of::<Value>() + std::mem::size_of::<(u32, u64)>()))?;
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
                let Some(c) = self.heap.channel_mut(ch, None) else { return Err(internal("not a channel")) };
                if let Some((v, woken)) = c.take() {
                    let some = self.some(v);
                    if let Some((sf, sg)) = woken {
                        // If the sender went away, its value stays queued.
                        self.deliver(sf, sg, Value::Bool(true));
                    }
                    return Ok(Some(some));
                }
                if c.closed {
                    return Ok(Some(self.none()));
                }
                if c.receivers.len() >= c.receivers_prune_at.max(MIN_PRUNE) {
                    // Drop the waits of cancelled tasks.
                    self.meter.charge(c.receivers.len() as u64)?;
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
                let Some(c) = self.heap.channel_mut(ch, None) else { return Err(internal("not a channel")) };
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
                if c.has_room() {
                    self.heap.channel_push(ch, v).map_err(Ctl::limit)?;
                    return Ok(Some(Value::Bool(true)));
                }
                let Some(c) = self.heap.channel_mut(ch, Some(v)) else { unreachable!() };
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
    fn add_waiter(&mut self, target: u32, waiter: (u32, u64)) -> Result<(), Ctl> {
        let Some(Some(f)) = self.fibers.get_mut(target as usize) else { return Ok(()) };
        let prune = f.waiters.len() >= f.waiters_prune_at.max(MIN_PRUNE);
        if prune {
            self.meter.charge(f.waiters.len() as u64)?;
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
    // Maps and slices

    /// The value of key `k` in map `m`, or `None`.
    #[inline]
    pub fn map_get(&self, m: Value, k: Value) -> Result<Option<Value>, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        match map_slot(&self.heap, &self.meter, mo, k)?.map(|s| &mo.entries[s]) {
            Some(Some((_, v))) => Ok(Some(*v)),
            _ => Ok(None),
        }
    }

    /// `Op::ForChars`: the character of string `s` at byte offset `at`, and
    /// the offset after it; `None` at the end.
    #[inline]
    fn next_char(&mut self, s: &Value, at: &Value) -> Option<(Value, i64)> {
        let (&Value::Obj(h), &Value::Int(i)) = (s, at) else { return None };
        let Obj::Str(t) = self.heap.obj(h) else { return None };
        let i = i as usize;
        let &b = t.as_bytes().get(i)?;
        if b.is_ascii() {
            return Some((self.ascii_str(b), i as i64 + 1));
        }
        self.next_char_slow(h, i)
    }

    #[inline(never)]
    fn next_char_slow(&mut self, h: u32, i: usize) -> Option<(Value, i64)> {
        let Obj::Str(t) = self.heap.obj(h) else { return None };
        let c = t.get(i..)?.chars().next()?;
        let end = i + c.len_utf8();
        let text = c.to_string();
        Some((self.heap.new_str(text), end as i64))
    }

    /// `m.get(k)`: the value as an option (built here rather than returned
    /// as an `Option<Value>`, for the reason given at `map_index`).
    #[inline(never)]
    fn map_get_option(&mut self, m: Value, k: Value) -> Result<Value, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        if let Value::Int(i) = k {
            let v = map_get_int(mo, i).copied();
            return Ok(self.option(v));
        }
        match self.map_lookup(m, mo, k)?.map(|s| &mo.entries[s]) {
            Some(&Some((_, v))) => Ok(self.some(v)),
            _ => Ok(self.none()),
        }
    }

    /// The slot of key `k` (not an int) in map `m`, remembered for a store
    /// to the same key right after (`m[k] = m.get_or(k, 0) + 1`).
    #[inline]
    fn map_lookup(&self, m: Value, mo: &MapObj, k: Value) -> Result<Option<usize>, Ctl> {
        let s = map_slot(&self.heap, &self.meter, mo, k)?;
        if let (Some(s), Value::Obj(h)) = (s, m) {
            self.last_slot.set((h, s as u32));
        }
        Ok(s)
    }

    /// The slot of `k` in map `m` if it is the slot last looked up: the
    /// entry there holds `k` itself.
    #[inline]
    fn last_slot(&self, m: Value, mo: &MapObj, k: Value) -> Option<usize> {
        let (h, s) = self.last_slot.get();
        let hit = matches!(m, Value::Obj(x) if x == h)
            && match (mo.entries.get(s as usize), k) {
                (Some(Some((Value::Obj(x), _))), Value::Obj(y)) => *x == y,
                (Some(Some((x, _))), _) => *x == k,
                _ => false,
            };
        hit.then_some(s as usize)
    }

    /// `Op::MapGetOr`: the value of `k` in map `m` to `out`, if there is one.
    #[inline(never)]
    fn map_get_or(&self, m: Value, k: Value, out: &mut Value) -> Result<(), Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        let v = match k {
            Value::Int(i) => map_get_int(mo, i),
            _ => self.map_lookup(m, mo, k)?.and_then(|s| mo.entries[s].as_ref()).map(|(_, v)| v),
        };
        if let Some(v) = v {
            *out = *v;
        }
        Ok(())
    }

    /// `Op::MapGetTuple`.
    #[inline(never)]
    fn map_get_tuple(&mut self, m: Value, items: &[Value]) -> Result<Value, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        match map_slot_tuple(&self.heap, &self.meter, mo, items)?.map(|s| &mo.entries[s]) {
            Some(&Some((_, v))) => Ok(self.some(v)),
            _ => Ok(self.none()),
        }
    }

    /// `Op::MapHasTuple`.
    #[inline(never)]
    fn map_has_tuple(&self, m: Value, items: &[Value]) -> Result<bool, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        Ok(map_slot_tuple(&self.heap, &self.meter, mo, items)?.is_some())
    }

    /// `m[k]`: the value, or an error if the key is missing. It copies the
    /// value out of the entry directly rather than through `map_get`'s
    /// `Option<Value>`, whose niche makes LLVM write the result piecewise and
    /// stalls the caller's 16-byte load on store forwarding.
    #[inline(never)]
    fn map_index(&self, m: Value, k: Value) -> Result<Value, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        if let Value::Int(i) = k {
            return match map_get_int(mo, i) {
                Some(v) => Ok(*v),
                None => Err(self.key_error(k)),
            };
        }
        match self.map_lookup(m, mo, k)?.map(|s| &mo.entries[s]) {
            Some(Some((_, v))) => Ok(*v),
            _ => Err(self.key_error(k)),
        }
    }

    /// Inserts or replaces `k` in map `m`; returns the old value.
    #[inline(never)]
    pub fn map_insert(&mut self, m: Value, k: Value, v: Value) -> Result<Option<Value>, Ctl> {
        self.map_store(m, k, v, true)
    }

    /// `m[k] = v`: `map_insert` without the old value.
    #[inline(never)]
    fn map_set(&mut self, m: Value, k: Value, v: Value) -> Result<(), Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        let found = match k {
            Value::Int(i) => mo.index.find_int(i, |s| matches!(mo.entries[s], Some((Value::Int(x), _)) if x == i)),
            _ if let Some(s) = self.last_slot(m, mo, k) => Some(s),
            _ => {
                let hash = hash_key(&self.heap, &self.meter, k)?;
                match find_entry(&self.heap, &self.meter, &mo.index, hash, k, &mo.entries, |e| e.0)? {
                    Some(s) => Some(s),
                    None => return self.map_push(m, k, v, hash),
                }
            }
        };
        let Some(s) = found else { return self.map_push(m, k, v, int_hash(k.as_int())) };
        self.unpin(m)?;
        // The key stays: it equals `k`.
        let Obj::Map(mo) = self.heap.entries_mut_storing(m, s, false, &[v]) else { unreachable!() };
        if let Some(e) = &mut mo.entries[s] {
            e.1 = v;
        }
        mo.set_dense(k, v);
        Ok(())
    }

    /// Inserts `k` with value `v` into map `m` unless it is there already;
    /// returns its existing value. One lookup, unlike `map_get` then `map_insert`.
    pub fn map_get_or_insert(&mut self, m: Value, k: Value, v: Value) -> Result<Option<Value>, Ctl> {
        self.map_store(m, k, v, false)
    }

    /// `map_insert` (`replace`) or `map_get_or_insert`.
    fn map_store(&mut self, m: Value, k: Value, v: Value, replace: bool) -> Result<Option<Value>, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        let hash = hash_key(&self.heap, &self.meter, k)?;
        let found = find_entry(&self.heap, &self.meter, &mo.index, hash, k, &mo.entries, |e| e.0)?;
        if let Some(s) = found {
            if !replace {
                return Ok(mo.entries[s].map(|(_, old)| old));
            }
            self.unpin(m)?;
            let Obj::Map(mo) = self.heap.entries_mut_storing(m, s, false, &[k, v]) else { unreachable!() };
            let old = mo.entries[s].replace((k, v)).map(|(_, old)| old);
            mo.set_dense(k, v);
            return Ok(old);
        }
        self.map_push(m, k, v, hash)?;
        Ok(None)
    }

    /// Appends the entry `k: v` to map `m`, which does not hold `k` (with
    /// hash `hash`).
    fn map_push(&mut self, m: Value, k: Value, v: Value, hash: u64) -> Result<(), Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        let end = mo.entries.len();
        if !slot_fits(end) {
            return Err(Ctl::fail("limit", "map too large"));
        }
        let new_cap = self.entries_capacity(end, mo.entries.capacity(), MAP_ENTRY_SIZE, mo.index_growth(k))?;
        self.unpin(m)?;
        let Obj::Map(mo) = self.heap.entries_mut_storing(m, end, true, &[k, v]) else { unreachable!() };
        let keys = mo.entries.iter().enumerate().filter_map(|(s, e)| e.map(|(k, _)| (s, k)));
        mo.index.densify(mo.live + 1, keys);
        let (cap, index) = (mo.entries.capacity(), mo.index_bytes());
        mo.entries.reserve_exact(new_cap - end);
        mo.entries.push(Some((k, v)));
        mo.live += 1;
        mo.index.insert(k, hash, end);
        mo.set_dense(k, v);
        if mo.entries.capacity() != cap || mo.index_bytes() != index {
            let grown = (mo.entries.capacity() - cap) * MAP_ENTRY_SIZE + mo.index_bytes();
            self.heap.bytes = (self.heap.bytes + grown).saturating_sub(index);
        }
        Ok(())
    }

    /// The capacity the entries of a map or set need for one more entry.
    /// The entries hold `len` of `cap` slots of `elem` bytes, and adding the
    /// entry allocates `index` bytes for the index. Fails with the memory
    /// limit, before anything changes, if the growth does not fit.
    #[inline]
    fn entries_capacity(&self, len: usize, cap: usize, elem: usize, index: usize) -> Result<usize, Ctl> {
        if len < cap && index == 0 {
            return Ok(cap);
        }
        self.entries_capacity_slow(len, cap, elem, index)
    }

    #[cold]
    #[inline(never)]
    fn entries_capacity_slow(&self, len: usize, cap: usize, elem: usize, index: usize) -> Result<usize, Ctl> {
        if len < cap {
            self.check_memory(index)?;
            return Ok(cap);
        }
        self.heap.grown_capacity(cap, len + 1, elem, index).map_err(Ctl::limit)
    }

    /// Removes `k` from map `m`; returns its value.
    pub fn map_remove(&mut self, m: Value, k: Value) -> Result<Option<Value>, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        if mo.live == 0 {
            return Ok(None);
        }
        let hash = hash_key(&self.heap, &self.meter, k)?;
        let Some(s) = find_key(&self.heap, &self.meter, &mo.index, hash, k, |i| mo.entries[i].map(|(k, _)| k))? else { return Ok(None) };
        self.unpin(m)?;
        let Obj::Map(mo) = self.heap.get(m) else { unreachable!() };
        let before = mo.entries.capacity() * MAP_ENTRY_SIZE + mo.index_bytes();
        // Compaction moves entries: the full write barrier.
        let Obj::Map(mo) = self.heap.get_mut(m) else { unreachable!() };
        mo.index.remove(k, hash, |i| i == s);
        let old = mo.entries[s].take().map(|(_, v)| v);
        mo.live -= 1;
        mo.set_dense(k, Value::Uninit);
        compact_entries(&mut mo.entries, mo.live, &mut mo.index);
        let after = mo.entries.capacity() * MAP_ENTRY_SIZE + mo.index_bytes();
        self.heap.account(before, after);
        Ok(old)
    }

    /// Adds `k` to set `s`; false if it was there already.
    pub fn set_add(&mut self, s: Value, k: Value) -> Result<bool, Ctl> {
        let Obj::Set(so) = self.heap.get(s) else { return Err(internal("not a set")) };
        let hash = hash_key(&self.heap, &self.meter, k)?;
        if find_entry(&self.heap, &self.meter, &so.index, hash, k, &so.entries, |&e| e)?.is_some() {
            return Ok(false);
        }
        let end = so.entries.len();
        if !slot_fits(end) {
            return Err(Ctl::fail("limit", "set too large"));
        }
        let new_cap = self.entries_capacity(end, so.entries.capacity(), SET_ENTRY_SIZE, so.index_growth(k))?;
        self.unpin(s)?;
        let Obj::Set(so) = self.heap.entries_mut_storing(s, end, true, &[k]) else { unreachable!() };
        let keys = so.entries.iter().enumerate().filter_map(|(s, e)| e.map(|k| (s, k)));
        so.index.densify(so.live + 1, keys);
        let (cap, index) = (so.entries.capacity(), so.index.bytes());
        so.entries.reserve_exact(new_cap - end);
        so.entries.push(Some(k));
        so.live += 1;
        so.index.insert(k, hash, end);
        if so.entries.capacity() != cap || so.index.bytes() != index {
            let grown = (so.entries.capacity() - cap) * SET_ENTRY_SIZE + so.index.bytes();
            self.heap.bytes = (self.heap.bytes + grown).saturating_sub(index);
        }
        Ok(true)
    }

    #[inline(never)]
    pub fn set_contains(&self, s: Value, k: Value) -> Result<bool, Ctl> {
        let Obj::Set(so) = self.heap.get(s) else { return Err(internal("not a set")) };
        if let Value::Int(i) = k {
            return Ok(set_has_int(so, i));
        }
        Ok(set_slot(&self.heap, &self.meter, so, k)?.is_some())
    }

    /// `m.contains(k)`.
    #[inline(never)]
    fn map_has(&self, m: Value, k: Value) -> Result<bool, Ctl> {
        let Obj::Map(mo) = self.heap.get(m) else { return Err(internal("not a map")) };
        if let Value::Int(i) = k {
            return Ok(map_get_int(mo, i).is_some());
        }
        Ok(self.map_lookup(m, mo, k)?.is_some())
    }

    /// Removes `k` from set `s`; false if it was not there.
    pub fn set_remove(&mut self, s: Value, k: Value) -> Result<bool, Ctl> {
        let Obj::Set(so) = self.heap.get(s) else { return Err(internal("not a set")) };
        if so.live == 0 {
            return Ok(false);
        }
        let hash = hash_key(&self.heap, &self.meter, k)?;
        let Some(slot) = find_key(&self.heap, &self.meter, &so.index, hash, k, |i| so.entries[i])? else { return Ok(false) };
        self.unpin(s)?;
        let Obj::Set(so) = self.heap.get(s) else { unreachable!() };
        let before = so.entries.capacity();
        let Obj::Set(so) = self.heap.get_mut(s) else { unreachable!() };
        so.index.remove(k, hash, |i| i == slot);
        so.entries[slot] = None;
        so.live -= 1;
        compact_entries(&mut so.entries, so.live, &mut so.index);
        let after = so.entries.capacity();
        self.heap.account(before * SET_ENTRY_SIZE, after * SET_ENTRY_SIZE);
        Ok(true)
    }

    /// `Op::ListRest`: a copy of `list` without its first `before` and last
    /// `after` elements.
    #[inline(never)]
    fn list_rest(&mut self, list: Value, before: usize, after: usize) -> Result<Value, Ctl> {
        let l = self.heap.list(list);
        let end = l.len().saturating_sub(after).max(before);
        let n = end.saturating_sub(before);
        self.check_memory(n * std::mem::size_of::<Value>())?;
        self.meter.charge(n as u64)?;
        let items = self.heap.list(list).get(before..end).map(<[Value]>::to_vec).unwrap_or_default();
        Ok(self.heap.new_list(items))
    }

    #[inline(never)]
    fn slice(&mut self, o: Value, start: Option<i64>, end: Option<i64>, inclusive: bool) -> Result<Value, Ctl> {
        let len = match self.heap.get(o) {
            Obj::List(l) => l.len(),
            Obj::Str(s) => s.len(),
            Obj::Bytes(b) => b.len(),
            _ => return Err(internal("slice of non-sequence")),
        } as i64;
        let a = start.unwrap_or(0);
        let mut b = end.unwrap_or(len);
        if inclusive {
            b = b.checked_add(1).ok_or_else(|| Ctl::fail("bounds", "slice end out of range"))?;
        }
        if a < 0 || b > len || a > b {
            return Err(Ctl::fail("bounds", format!("slice {a}..{b} out of range for length {len}")));
        }
        let (a, b) = (a as usize, b as usize);
        let list = matches!(self.heap.get(o), Obj::List(_));
        let cost = if list { (b - a) as u64 } else { ((b - a) / super::meter::BYTES_PER_UNIT) as u64 };
        self.heap.check_memory(if list { (b - a) * std::mem::size_of::<Value>() } else { b - a })?;
        self.meter.charge(cost)?;
        Ok(match self.heap.get(o) {
            Obj::List(l) => {
                let v = l[a..b].to_vec();
                self.heap.new_list(v)
            }
            Obj::Str(s) => {
                if !s.is_char_boundary(a) || !s.is_char_boundary(b) {
                    return Err(Ctl::fail("bounds", format!("slice {a}..{b} is not on a character boundary")));
                }
                let t = s[a..b].to_string();
                self.heap.new_str(t)
            }
            Obj::Bytes(bs) => {
                let v = bs[a..b].to_vec();
                self.heap.new_bytes(v)
            }
            _ => unreachable!(),
        })
    }
}

/// The fuel that instruction `op` spends before it can fail. Only calls
/// spend any, at their safe point (`tick_call!`).
pub(super) fn call_tick(op: Op) -> i64 {
    match op {
        Op::CallDirect { .. }
        | Op::CallR { .. }
        | Op::CallDirectTo { .. }
        | Op::CallSelf { .. }
        | Op::TailCallDirect { .. }
        | Op::TailCall { .. } => 1,
        _ => 0,
    }
}

/// An internal error: a compiler or runtime bug.
#[cold]
pub(crate) fn internal(msg: impl std::fmt::Display) -> Ctl {
    Ctl::internal(msg.to_string())
}

#[cold]
fn overflow(what: &str) -> Ctl {
    Ctl::fail("overflow", format!("{what} overflow"))
}
