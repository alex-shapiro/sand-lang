//! Tail-call-threaded opcode dispatch using nightly `become` and
//! `extern "rust-preserve-none"` over packed 64-bit bytecode words.

use super::*;
use crate::vm::bytecode::{TAG_JNOT_SOME, TAG_JSOME_BACK};

/// Interpreter state passed by pointer in `x20` into every tail-call handler.
pub(super) struct DispatchCtx<'a> {
    pub(super) vm: &'a mut Vm,
    pub(super) fiber: &'a mut Fiber,
    pub(super) prog: &'a Program,
    pub(super) packed: &'a [Box<[u64]>],
    pub(super) natives: &'a [NativeEntry],
    pub(super) exit: Option<DispatchExit>,
}

/// Outcome stored in `DispatchCtx::exit` when the tail-call chain returns to `Vm::interp`.
pub(super) enum DispatchExit {
    Done(VmValue, i64),
    SafePoint { resume_ip: usize, fuel: i64 },
    SlowOp { ip: usize, fuel: i64 },
    FailStop { ctl: Fail, ip: usize, fuel: i64 },
    LimitMemory { ip: usize, fuel: i64 },
    NativeSlow { result: Result<VmValue, Ctl>, ip: usize, fuel_at_call: i64 },
}

type Handler = for<'a, 'b, 'c> extern "rust-preserve-none" fn(
    ctx: &'b mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &'c mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
);

/// Enters the tail-call dispatch chain at `(code, ip, base, fuel)`.
#[inline(always)]
pub(super) fn enter<'a>(ctx: &mut DispatchCtx<'a>, code: &'a [u64], ip: usize, stack: &mut [VmValue], base: usize, fuel: i64) {
    let op = code[ip];
    HANDLERS[(op & 0xff) as usize](ctx, code, ip + 1, stack, base, fuel, op);
}

#[cold]
#[inline(never)]
fn exit_with(ctx: &mut DispatchCtx<'_>, exit: DispatchExit) {
    ctx.exit = Some(exit);
}

#[inline(always)]
const fn op_a(op: u64) -> usize {
    ((op >> 16) & 0xffff) as usize
}
#[inline(always)]
const fn op_b(op: u64) -> usize {
    ((op >> 32) & 0xffff) as usize
}
#[inline(always)]
const fn op_b_i64(op: u64) -> i64 {
    ((op >> 32) as i16) as i64
}
#[inline(always)]
const fn op_c(op: u64) -> usize {
    (op >> 48) as usize
}
#[inline(always)]
const fn op_c_i16(op: u64) -> i16 {
    ((op as i64) >> 48) as i16
}
#[inline(always)]
const fn op_c_i64(op: u64) -> i64 {
    (op as i64) >> 48
}
#[inline(always)]
const fn op_imm_u32(op: u64) -> u32 {
    (op >> 32) as u32
}
#[inline(always)]
const fn op_imm_i32(op: u64) -> i32 {
    ((op as i64) >> 32) as i32
}
#[inline(always)]
const fn op_imm_i64(op: u64) -> i64 {
    (op as i64) >> 32
}

macro_rules! dispatch {
    ($ctx:expr, $code:expr, $ip:expr, $stack:expr, $base:expr, $fuel:expr) => {{
        let next_ip: usize = $ip;
        let next_op = $code[next_ip];
        become HANDLERS[(next_op & 0xff) as usize]($ctx, $code, next_ip + 1, $stack, $base, $fuel, next_op)
    }};
}

macro_rules! r {
    ($stack:expr, $base:expr, $reg:expr) => {
        $stack[$base + $reg]
    };
}

macro_rules! int {
    ($ctx:expr, $stack:expr, $base:expr, $reg:expr, $ip:expr, $fuel:expr) => {
        match r!($stack, $base, $reg) {
            VmValue::Int(i) => i,
            _ => return exit_with($ctx, DispatchExit::SlowOp { ip: $ip - 1, fuel: $fuel }),
        }
    };
}

macro_rules! float {
    ($ctx:expr, $stack:expr, $base:expr, $reg:expr, $ip:expr, $fuel:expr) => {
        match r!($stack, $base, $reg) {
            VmValue::Float(f) => f,
            _ => return exit_with($ctx, DispatchExit::SlowOp { ip: $ip - 1, fuel: $fuel }),
        }
    };
}

macro_rules! jump_ip {
    ($ip:expr, $off:expr) => {
        ($ip as isize + $off as isize) as usize
    };
}

macro_rules! back_edge {
    ($ctx:expr, $code:expr, $ip:expr, $off:expr, $stack:expr, $base:expr, $fuel:expr) => {{
        let off: i32 = $off as i32;
        let target = jump_ip!($ip, off);
        let fuel = $fuel - back_edge_fuel(off);
        if fuel <= 0 || $ctx.vm.heap.needs_gc() {
            return exit_with($ctx, DispatchExit::SafePoint { resume_ip: target, fuel });
        }
        dispatch!($ctx, $code, target, $stack, $base, fuel)
    }};
}

macro_rules! retryable_op {
    ($ctx:expr, $body:expr) => {{
        let fuel_mark = $ctx.vm.begin_retryable_op();
        let result = $body;
        $ctx.vm.end_retryable_op(fuel_mark, result)
    }};
}

macro_rules! do_return {
    ($ctx:expr, $stack:expr, $fuel:expr, $val:expr) => {{
        let v: VmValue = $val;
        let ret = $ctx.fiber.frames.pop().map_or(0, |f| f.ret as usize);
        match $ctx.fiber.frames.last() {
            None => exit_with($ctx, DispatchExit::Done(v, $fuel)),
            Some(caller) => {
                let new_code: &'a [u64] = &$ctx.packed[caller.proto as usize];
                let new_ip = caller.ip as usize;
                let new_base = caller.base as usize;
                $stack[ret] = v;
                dispatch!($ctx, new_code, new_ip, $stack, new_base, $fuel)
            }
        }
    }};
}

macro_rules! enter_frame {
    ($ctx:expr, $ip:expr, $stack:expr, $fuel:expr, $proto:expr, $new_base:expr, $ret:expr, $closure:expr, $load_caps:expr) => {{
        let proto: u32 = $proto;
        let nb: usize = $new_base;
        let p = &$ctx.prog.protos[proto as usize];
        let need = nb + p.nregs as usize;
        if $fuel <= 1 || $ctx.vm.heap.needs_gc() {
            return exit_with($ctx, DispatchExit::SafePoint { resume_ip: $ip - 1, fuel: $fuel - 1 });
        }
        if $ctx.fiber.frames.len() >= $ctx.vm.host.limits.max_call_depth
            || $stack.len() < need
            || $ctx.fiber.frames.len() == $ctx.fiber.frames.capacity()
        {
            return exit_with($ctx, DispatchExit::SlowOp { ip: $ip - 1, fuel: $fuel });
        }
        let mut fuel = $fuel - 1;
        $ctx.fiber.frames.last_mut().unwrap().ip = $ip as u32;
        $ctx.fiber.frames.push(Frame { proto, ip: 0, base: nb as u32, ret: $ret as u32, closure: $closure });
        let new_code: &'a [u64] = &$ctx.packed[proto as usize];
        if new_code.len() >= 64 {
            fuel -= code_fuel_surcharge(new_code.len());
        }
        if $load_caps {
            load_captures(&$ctx.vm.heap, $closure, &mut $stack[nb + p.nparams as usize..]);
        }
        dispatch!($ctx, new_code, 0, $stack, nb, fuel)
    }};
}

static HANDLERS: [Handler; 256] = {
    let mut t: [Handler; 256] = [op_slow; 256];
    t[0] = op_nop;
    t[1] = op_move;
    t[2] = op_load_unit;
    t[3] = op_load_bool;
    t[4] = op_load_int;
    t[5] = op_load_const;
    t[6] = op_load_tag;
    t[7] = op_load_global;
    t[8] = op_store_global;
    t[9] = op_new_cell;
    t[10] = op_load_cell;
    t[11] = op_store_cell;
    t[12] = op_make_closure;
    t[13] = op_add_i;
    t[14] = op_sub_i;
    t[15] = op_mul_i;
    t[16] = op_div_i;
    t[17] = op_rem_i;
    t[18] = op_pow_i;
    t[19] = op_add_ik;
    t[20] = op_mul_ik;
    t[21] = op_div_ik;
    t[22] = op_rem_ik;
    t[23] = op_neg_i;
    t[24] = op_bit_and;
    t[25] = op_bit_or;
    t[26] = op_bit_xor;
    t[27] = op_shl;
    t[28] = op_shr;
    t[29] = op_bit_not;
    t[30] = op_add_f;
    t[31] = op_sub_f;
    t[32] = op_mul_f;
    t[33] = op_div_f;
    t[34] = op_rem_f;
    t[35] = op_pow_f;
    t[36] = op_neg_f;
    t[37] = op_arith_gen;
    t[38] = op_arith_gen;
    t[39] = op_arith_gen;
    t[40] = op_arith_gen;
    t[41] = op_arith_gen;
    t[42] = op_arith_gen;
    t[43] = op_neg;
    t[44] = op_not;
    t[45] = op_eq_i;
    t[46] = op_ne_i;
    t[47] = op_lt_i;
    t[48] = op_le_i;
    t[49] = op_eq_ik;
    t[50] = op_ne_ik;
    t[51] = op_lt_ik;
    t[52] = op_le_ik;
    t[53] = op_gt_ik;
    t[54] = op_ge_ik;
    t[55] = op_eq_f;
    t[56] = op_ne_f;
    t[57] = op_lt_f;
    t[58] = op_le_f;
    t[59] = op_eq;
    t[60] = op_ne;
    t[63] = op_jmp;
    t[64] = op_loop;
    t[65] = op_jmp_if;
    t[66] = op_jmp_if_not;
    t[67] = op_jnlt_i;
    t[68] = op_jnle_i;
    t[69] = op_jneq_i;
    t[70] = op_jnne_i;
    t[71] = op_jnlt_f;
    t[72] = op_jnle_f;
    t[73] = op_jneq;
    t[74] = op_jnne;
    t[75] = op_jneq_k;
    t[76] = op_jnne_k;
    t[77] = op_jnlt_ik;
    t[78] = op_jnle_ik;
    t[79] = op_jngt_ik;
    t[80] = op_jnge_ik;
    t[81] = op_jneq_ik;
    t[82] = op_jnne_ik;
    t[83] = op_jnot_some;
    t[84] = op_jnot_ok;
    t[85] = op_jnot_variant;
    t[86] = op_switch_variant;
    t[87] = op_switch_int;
    t[88] = op_switch_char;
    t[89] = op_jmp_if_back;
    t[90] = op_jmp_if_not_back;
    t[91] = op_jnlt_i_back;
    t[92] = op_jnle_i_back;
    t[93] = op_jneq_i_back;
    t[94] = op_jnne_i_back;
    t[95] = op_jnlt_ik_back;
    t[96] = op_jnle_ik_back;
    t[97] = op_jngt_ik_back;
    t[98] = op_jnge_ik_back;
    t[99] = op_jneq_ik_back;
    t[100] = op_jnne_ik_back;
    t[101] = op_jsome_back;
    t[102] = op_call_r;
    t[103] = op_call_direct;
    t[104] = op_call_direct_to;
    t[105] = op_call_self;
    t[106] = op_call_native;
    t[107] = op_call_native_to;
    t[108] = op_tail_call;
    t[109] = op_tail_call_direct;
    t[110] = op_ret;
    t[111] = op_ret_variant;
    t[112] = op_ret_unit;
    t[115] = op_new_list;
    t[116] = op_new_map;
    t[117] = op_new_tuple;
    t[118] = op_new_struct;
    t[119] = op_new_variant;
    t[120] = op_new_range;
    t[121] = op_new_range_incl;
    t[122] = op_get_field;
    t[123] = op_payload;
    t[124] = op_set_field;
    t[125] = op_index;
    t[126] = op_index_map;
    t[127] = op_set_index;
    t[128] = op_set_index_map;
    t[130] = op_list_push;
    t[132] = op_for_list;
    t[133] = op_for_range;
    t[134] = op_for_range_incl;
    t[135] = op_for_set;
    t[136] = op_for_map;
    t[137] = op_for_chars;
    t[138] = op_for_list_back;
    t[139] = op_for_range_back;
    t[140] = op_for_range_incl_back;
    t[141] = op_for_set_back;
    t[142] = op_for_map_back;
    t[143] = op_for_chars_back;
    t[144] = op_unpack_range;
    t[145] = op_new_iter;
    t[146] = op_jlist_len;
    t[147] = op_jlist_min_len;
    t[148] = op_list_elem;
    t[150] = op_len;
    t[151] = op_list_get;
    t[152] = op_list_get_or;
    t[153] = op_list_pop;
    t[154] = op_map_get;
    t[157] = op_map_get_or;
    t[158] = op_map_has;
    t[159] = op_set_has;
    t[160] = op_set_add;
    t[161] = op_int_to_float;
    t[162] = op_append_str;
    t[163] = op_freeze_str;
    t[166] = op_to_str;
    t
};

#[inline(never)]
extern "rust-preserve-none" fn op_slow<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    ip: usize,
    _stack: &mut [VmValue],
    _base: usize,
    fuel: i64,
    _op: u64,
) {
    exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel })
}

// ---------------- loads / moves / cells / closures

#[inline(never)]
extern "rust-preserve-none" fn op_nop<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    _op: u64,
) {
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_move<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = r!(stack, base, op_b(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_unit<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Unit;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_bool<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(op_b(op) != 0);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_int<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Int(op_imm_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_const<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = ctx.vm.consts[op_imm_u32(op) as usize];
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_tag<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Tag(op_imm_u32(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_global<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let v = ctx.vm.globals[op_imm_u32(op) as usize];
    if let VmValue::Uninit = v {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    }
    r!(stack, base, op_a(op)) = v;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_store_global<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    ctx.vm.globals[op_imm_u32(op) as usize] = r!(stack, base, op_a(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_cell<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let v = r!(stack, base, op_b(op));
    r!(stack, base, op_a(op)) = ctx.vm.heap.alloc(Obj::Cell(v));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_load_cell<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if let VmValue::Obj(h) = r!(stack, base, op_b(op))
        && let Obj::Cell(v) = ctx.vm.heap.obj(h)
    {
        r!(stack, base, op_a(op)) = *v;
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_store_cell<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    ctx.vm.heap.set_cell(r!(stack, base, op_a(op)), r!(stack, base, op_b(op)));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_make_closure<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let proto = op_imm_u32(op);
    let p = &ctx.prog.protos[proto as usize];
    let fuel = fuel - code_fuel_surcharge(p.captures.len());
    let c = ctx.vm.make_closure(p, proto, &stack[base..]);
    r!(stack, base, op_a(op)) = c;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- integer arithmetic

#[inline(never)]
extern "rust-preserve-none" fn op_add_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Some(v) = int!(ctx, stack, base, op_b(op), ip, fuel).checked_add(int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_sub_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Some(v) = int!(ctx, stack, base, op_b(op), ip, fuel).checked_sub(int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_mul_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Some(v) = int!(ctx, stack, base, op_b(op), ip, fuel).checked_mul(int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_div_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Ok(v) = int_div(int!(ctx, stack, base, op_b(op), ip, fuel), int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_rem_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Ok(v) = int_rem(int!(ctx, stack, base, op_b(op), ip, fuel), int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_pow_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Ok(v) = int_pow(int!(ctx, stack, base, op_b(op), ip, fuel), int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_add_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Some(v) = int!(ctx, stack, base, op_b(op), ip, fuel).checked_add(op_c_i64(op)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_mul_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Some(v) = int!(ctx, stack, base, op_b(op), ip, fuel).checked_mul(op_c_i64(op)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_div_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let k = op_c_i16(op);
    let x = int!(ctx, stack, base, op_b(op), ip, fuel);
    let v = if k > 0 && k & (k - 1) == 0 {
        x >> k.trailing_zeros()
    } else {
        let Some(v) = floor_div(x, k as i64) else {
            return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
        };
        v
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_rem_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let k = op_c_i16(op);
    let x = int!(ctx, stack, base, op_b(op), ip, fuel);
    let v = if k > 0 && k & (k - 1) == 0 { x & (k as i64 - 1) } else { floor_rem(x, k as i64) };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_neg_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Some(v) = int!(ctx, stack, base, op_b(op), ip, fuel).checked_neg() else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(v);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_bit_and<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Int(int!(ctx, stack, base, op_b(op), ip, fuel) & int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_bit_or<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Int(int!(ctx, stack, base, op_b(op), ip, fuel) | int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_bit_xor<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Int(int!(ctx, stack, base, op_b(op), ip, fuel) ^ int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_shl<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Ok(s) = shift_amount(int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(int!(ctx, stack, base, op_b(op), ip, fuel).wrapping_shl(s));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_shr<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let Ok(s) = shift_amount(int!(ctx, stack, base, op_c(op), ip, fuel)) else {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    };
    r!(stack, base, op_a(op)) = VmValue::Int(int!(ctx, stack, base, op_b(op), ip, fuel) >> s);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_bit_not<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Int(!int!(ctx, stack, base, op_b(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- float arithmetic

#[inline(never)]
extern "rust-preserve-none" fn op_add_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Float(float!(ctx, stack, base, op_b(op), ip, fuel) + float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_sub_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Float(float!(ctx, stack, base, op_b(op), ip, fuel) - float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_mul_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Float(float!(ctx, stack, base, op_b(op), ip, fuel) * float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_div_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Float(float!(ctx, stack, base, op_b(op), ip, fuel) / float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_rem_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) =
        VmValue::Float(float_rem(float!(ctx, stack, base, op_b(op), ip, fuel), float!(ctx, stack, base, op_c(op), ip, fuel)));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_pow_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) =
        VmValue::Float(float!(ctx, stack, base, op_b(op), ip, fuel).powf(float!(ctx, stack, base, op_c(op), ip, fuel)));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_neg_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Float(-float!(ctx, stack, base, op_b(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- generic arithmetic

#[inline(never)]
extern "rust-preserve-none" fn op_arith_gen<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let code_op = ((op & 0xff) as u8) - 37;
    let (x, y) = (r!(stack, base, op_b(op)), r!(stack, base, op_c(op)));
    match ctx.vm.arith(code_op, x, y) {
        Ok(v) => {
            r!(stack, base, op_a(op)) = v;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_neg<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = match r!(stack, base, op_b(op)) {
        VmValue::Int(i) => match i.checked_neg() {
            Some(v) => VmValue::Int(v),
            None => return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
        },
        VmValue::Float(f) => VmValue::Float(-f),
        VmValue::Dur(d) => match d.checked_neg() {
            Some(v) => VmValue::Dur(v),
            None => return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
        },
        _ => return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
    };
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_not<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(!r!(stack, base, op_b(op)).as_bool());
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- comparisons

#[inline(never)]
extern "rust-preserve-none" fn op_eq_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) == int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_ne_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) != int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_lt_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) < int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_le_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) <= int!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_eq_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) == op_c_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_ne_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) != op_c_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_lt_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) < op_c_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_le_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) <= op_c_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_gt_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) > op_c_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_ge_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(int!(ctx, stack, base, op_b(op), ip, fuel) >= op_c_i64(op));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_eq_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(float!(ctx, stack, base, op_b(op), ip, fuel) == float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_ne_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(float!(ctx, stack, base, op_b(op), ip, fuel) != float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_lt_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(float!(ctx, stack, base, op_b(op), ip, fuel) < float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_le_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Bool(float!(ctx, stack, base, op_b(op), ip, fuel) <= float!(ctx, stack, base, op_c(op), ip, fuel));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_eq<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if let Some(eq) = quick_eq(&ctx.vm.heap, &r!(stack, base, op_b(op)), &r!(stack, base, op_c(op))) {
        r!(stack, base, op_a(op)) = VmValue::Bool(eq);
        dispatch!(ctx, code, ip, stack, base, fuel)
    }
    exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel })
}

#[inline(never)]
extern "rust-preserve-none" fn op_ne<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if let Some(eq) = quick_eq(&ctx.vm.heap, &r!(stack, base, op_b(op)), &r!(stack, base, op_c(op))) {
        r!(stack, base, op_a(op)) = VmValue::Bool(!eq);
        dispatch!(ctx, code, ip, stack, base, fuel)
    }
    exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel })
}

// ---------------- control flow

#[inline(never)]
extern "rust-preserve-none" fn op_jmp<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = jump_ip!(ip, op_imm_i32(op));
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_loop<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    back_edge!(ctx, code, ip, op_imm_i32(op), stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jmp_if<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if r!(stack, base, op_a(op)).as_bool() { jump_ip!(ip, op_imm_i32(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jmp_if_not<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if !r!(stack, base, op_a(op)).as_bool() { jump_ip!(ip, op_imm_i32(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnlt_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) >= int!(ctx, stack, base, op_b(op), ip, fuel) {
        jump_ip!(ip, op_c_i16(op))
    } else {
        ip
    };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnle_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) > int!(ctx, stack, base, op_b(op), ip, fuel) {
        jump_ip!(ip, op_c_i16(op))
    } else {
        ip
    };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jneq_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) != int!(ctx, stack, base, op_b(op), ip, fuel) {
        jump_ip!(ip, op_c_i16(op))
    } else {
        ip
    };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnne_i<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) == int!(ctx, stack, base, op_b(op), ip, fuel) {
        jump_ip!(ip, op_c_i16(op))
    } else {
        ip
    };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnlt_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if float!(ctx, stack, base, op_a(op), ip, fuel).partial_cmp(&float!(ctx, stack, base, op_b(op), ip, fuel))
        != Some(std::cmp::Ordering::Less)
    {
        jump_ip!(ip, op_c_i16(op))
    } else {
        ip
    };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnle_f<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if !matches!(
        float!(ctx, stack, base, op_a(op), ip, fuel).partial_cmp(&float!(ctx, stack, base, op_b(op), ip, fuel)),
        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
    ) {
        jump_ip!(ip, op_c_i16(op))
    } else {
        ip
    };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jneq<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if let Some(eq) = quick_eq(&ctx.vm.heap, &r!(stack, base, op_a(op)), &r!(stack, base, op_b(op))) {
        let nip = if !eq { jump_ip!(ip, op_c_i16(op)) } else { ip };
        dispatch!(ctx, code, nip, stack, base, fuel)
    }
    exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel })
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnne<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if let Some(eq) = quick_eq(&ctx.vm.heap, &r!(stack, base, op_a(op)), &r!(stack, base, op_b(op))) {
        let nip = if eq { jump_ip!(ip, op_c_i16(op)) } else { ip };
        dispatch!(ctx, code, nip, stack, base, fuel)
    }
    exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel })
}

#[inline(never)]
extern "rust-preserve-none" fn op_jneq_k<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match ctx.vm.eq_const(&r!(stack, base, op_a(op)), op_b(op) as u16) {
        Ok(true) => dispatch!(ctx, code, ip, stack, base, fuel),
        Ok(false) => {
            let nip = jump_ip!(ip, op_c_i16(op));
            dispatch!(ctx, code, nip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnne_k<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match ctx.vm.eq_const(&r!(stack, base, op_a(op)), op_b(op) as u16) {
        Ok(true) => {
            let nip = jump_ip!(ip, op_c_i16(op));
            dispatch!(ctx, code, nip, stack, base, fuel)
        }
        Ok(false) => dispatch!(ctx, code, ip, stack, base, fuel),
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnlt_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) >= op_b_i64(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnle_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) > op_b_i64(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jngt_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) <= op_b_i64(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnge_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) < op_b_i64(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jneq_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) != op_b_i64(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnne_ik<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if int!(ctx, stack, base, op_a(op), ip, fuel) == op_b_i64(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnot_some<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match ctx.vm.heap.variant_payload_if(&r!(stack, base, op_a(op)), ctx.prog.ids.some) {
        VmValue::Uninit => {
            let nip = jump_ip!(ip, op_c_i16(op));
            dispatch!(ctx, code, nip, stack, base, fuel)
        }
        p => {
            r!(stack, base, op_b(op)) = p;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnot_ok<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match ctx.vm.heap.variant_payload_if(&r!(stack, base, op_a(op)), ctx.prog.ids.ok) {
        VmValue::Uninit => {
            let nip = jump_ip!(ip, op_c_i16(op));
            dispatch!(ctx, code, nip, stack, base, fuel)
        }
        p => {
            r!(stack, base, op_b(op)) = p;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnot_variant<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if ctx.vm.heap.variant_id(r!(stack, base, op_a(op))) != op_b(op) as u32 { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_switch_variant<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    _ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = switch_target(ctx.prog, op_imm_u32(op), ctx.vm.heap.variant_id(r!(stack, base, op_a(op))) as i64);
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_switch_int<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = switch_target(ctx.prog, op_imm_u32(op), int!(ctx, stack, base, op_a(op), ip, fuel));
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_switch_char<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    _ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = switch_target(ctx.prog, op_imm_u32(op), char_code(&ctx.vm.heap, &r!(stack, base, op_a(op))));
    dispatch!(ctx, code, nip, stack, base, fuel)
}

// ---------------- backward branches

#[inline(never)]
extern "rust-preserve-none" fn op_jmp_if_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if r!(stack, base, op_a(op)).as_bool() {
        back_edge!(ctx, code, ip, op_imm_i32(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jmp_if_not_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if !r!(stack, base, op_a(op)).as_bool() {
        back_edge!(ctx, code, ip, op_imm_i32(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnlt_i_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) >= int!(ctx, stack, base, op_b(op), ip, fuel) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnle_i_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) > int!(ctx, stack, base, op_b(op), ip, fuel) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jneq_i_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) != int!(ctx, stack, base, op_b(op), ip, fuel) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnne_i_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) == int!(ctx, stack, base, op_b(op), ip, fuel) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnlt_ik_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) >= op_b_i64(op) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnle_ik_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) > op_b_i64(op) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jngt_ik_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) <= op_b_i64(op) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnge_ik_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) < op_b_i64(op) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jneq_ik_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) != op_b_i64(op) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jnne_ik_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if int!(ctx, stack, base, op_a(op), ip, fuel) == op_b_i64(op) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jsome_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let p = ctx.vm.heap.variant_payload_if(&r!(stack, base, op_a(op)), ctx.prog.ids.some);
    if !matches!(p, VmValue::Uninit) {
        r!(stack, base, op_b(op)) = p;
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- calls and returns

#[inline(never)]
extern "rust-preserve-none" fn op_call_direct<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nb = base + op_a(op);
    enter_frame!(ctx, ip, stack, fuel, op_imm_u32(op), nb, nb, NO_CLOSURE, false)
}

#[inline(never)]
extern "rust-preserve-none" fn op_call_direct_to<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nb = base + op_a(op);
    enter_frame!(ctx, ip, stack, fuel, op_c(op) as u32, nb, base + op_b(op), NO_CLOSURE, false)
}

#[inline(never)]
extern "rust-preserve-none" fn op_call_self<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let fr = *ctx.fiber.frames.last().unwrap();
    let nb = base + op_a(op);
    enter_frame!(ctx, ip, stack, fuel, fr.proto, nb, base + op_b(op), fr.closure, true)
}

macro_rules! native_call_fast {
    ($ctx:expr, $code:expr, $ip:expr, $stack:expr, $base:expr, $fuel:expr, $n:expr, $start:expr, $slot:expr) => {{
        let n: u32 = $n;
        let start: usize = $start;
        let slot: usize = $slot;
        let argc = $ctx.prog.native_argc.get(n as usize).copied().unwrap_or(0) as usize;
        $ctx.vm.meter.flush_interpreter_fuel($fuel);
        let frame = $ctx.fiber.frames.last_mut().unwrap();
        frame.ip = $ip as u32;
        $ctx.vm.caller_site = (frame.proto, frame.ip);
        let fuel_at_call = $ctx.vm.meter.remaining_slice_fuel();
        let result = $ctx.natives[n as usize].f.call($ctx.vm, &$stack[start..start + argc]);
        $ctx.vm.heap.fail_next_check.set(false);
        if let Ok(v) = result
            && !$ctx.vm.heap.is_over_memory_limit()
            && $ctx.vm.meter.remaining_slice_fuel() > 0
        {
            $ctx.vm.forget_once_answers();
            $stack[slot] = v;
            let next_fuel = $ctx.vm.meter.reload_interpreter_fuel();
            dispatch!($ctx, $code, $ip, $stack, $base, next_fuel)
        } else {
            exit_with($ctx, DispatchExit::NativeSlow { result, ip: $ip, fuel_at_call })
        }
    }};
}

#[inline(never)]
extern "rust-preserve-none" fn op_call_native<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let start = base + op_a(op);
    native_call_fast!(ctx, code, ip, stack, base, fuel, op_imm_u32(op), start, start)
}

#[inline(never)]
extern "rust-preserve-none" fn op_call_native_to<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let start = base + op_a(op);
    native_call_fast!(ctx, code, ip, stack, base, fuel, op_c(op) as u32, start, base + op_b(op))
}

#[inline(never)]
extern "rust-preserve-none" fn op_call_r<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let callee = r!(stack, base, op_a(op));
    let b = op_b(op);
    let dst = op_c(op);
    match ctx.vm.heap.get(callee) {
        Obj::Closure(p, _) => {
            let p = *p;
            let VmValue::Obj(h) = callee else { unreachable!() };
            let nb = base + b;
            enter_frame!(ctx, ip, stack, fuel, p, nb, base + dst, h, true)
        }
        Obj::IterFn(_) => {
            if fuel <= 1 || ctx.vm.heap.needs_gc() {
                return exit_with(ctx, DispatchExit::SafePoint { resume_ip: ip - 1, fuel: fuel - 1 });
            }
            let fuel = fuel - 1;
            let next = ctx.vm.iter_next(callee);
            r!(stack, base, dst) = ctx.vm.option(next);
            if let Some(&peek) = code.get(ip) {
                let tag = (peek & 0xff) as u8;
                if tag == TAG_JNOT_SOME && op_a(peek) == dst {
                    let ip = ip + 1;
                    match next {
                        Some(x) => {
                            r!(stack, base, op_b(peek)) = x;
                            dispatch!(ctx, code, ip, stack, base, fuel)
                        }
                        None => {
                            let nip = jump_ip!(ip, op_c_i16(peek));
                            dispatch!(ctx, code, nip, stack, base, fuel)
                        }
                    }
                } else if tag == TAG_JSOME_BACK && op_a(peek) == dst {
                    let ip = ip + 1;
                    if let Some(x) = next {
                        r!(stack, base, op_b(peek)) = x;
                        back_edge!(ctx, code, ip, op_c_i16(peek), stack, base, fuel)
                    }
                    dispatch!(ctx, code, ip, stack, base, fuel)
                }
            }
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        _ => exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_tail_call_direct<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let proto = op_imm_u32(op);
    let p = &ctx.prog.protos[proto as usize];
    let need = base + p.nregs as usize;
    if fuel <= 1 || ctx.vm.heap.needs_gc() {
        return exit_with(ctx, DispatchExit::SafePoint { resume_ip: ip - 1, fuel: fuel - 1 });
    }
    if stack.len() < need {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    }
    shift_args(&mut stack[base..], op_a(op), p.nparams as usize);
    let top = ctx.fiber.frames.last_mut().unwrap();
    top.proto = proto;
    top.closure = NO_CLOSURE;
    let new_code: &'a [u64] = &ctx.packed[proto as usize];
    let fuel = fuel - 1 - code_fuel_surcharge(new_code.len());
    dispatch!(ctx, new_code, 0, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_tail_call<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let callee = r!(stack, base, op_a(op));
    let b = op_b(op);
    let argc = op_c(op);
    match ctx.vm.heap.get(callee) {
        Obj::Closure(p, _) => {
            let proto = *p;
            let pr = &ctx.prog.protos[proto as usize];
            let need = base + pr.nregs as usize;
            if fuel <= 1 || ctx.vm.heap.needs_gc() {
                return exit_with(ctx, DispatchExit::SafePoint { resume_ip: ip - 1, fuel: fuel - 1 });
            }
            if stack.len() < need {
                return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
            }
            shift_args(&mut stack[base..], b, argc);
            let VmValue::Obj(h) = callee else { unreachable!() };
            load_captures(&ctx.vm.heap, h, &mut stack[base + pr.nparams as usize..]);
            let top = ctx.fiber.frames.last_mut().unwrap();
            top.proto = proto;
            top.closure = h;
            let new_code: &'a [u64] = &ctx.packed[proto as usize];
            let fuel = fuel - 1 - code_fuel_surcharge(new_code.len());
            dispatch!(ctx, new_code, 0, stack, base, fuel)
        }
        Obj::IterFn(_) => {
            if fuel <= 1 || ctx.vm.heap.needs_gc() {
                return exit_with(ctx, DispatchExit::SafePoint { resume_ip: ip - 1, fuel: fuel - 1 });
            }
            r!(stack, base, b) = ctx.vm.iter_step(callee);
            dispatch!(ctx, code, ip, stack, base, fuel - 1)
        }
        _ => exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_ret<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    _ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let v = r!(stack, base, op_a(op));
    do_return!(ctx, stack, fuel, v)
}

#[inline(never)]
extern "rust-preserve-none" fn op_ret_variant<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    _ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let val = r!(stack, base, op_a(op));
    let variant = op_b(op) as u32;
    let v = match VmValue::variant1(variant, val) {
        Some(v) => v,
        None => ctx.vm.heap.new_variant(variant, &[val]),
    };
    do_return!(ctx, stack, fuel, v)
}

#[inline(never)]
extern "rust-preserve-none" fn op_ret_unit<'a>(
    ctx: &mut DispatchCtx<'a>,
    _code: &'a [u64],
    _ip: usize,
    stack: &mut [VmValue],
    _base: usize,
    fuel: i64,
    _op: u64,
) {
    do_return!(ctx, stack, fuel, VmValue::Unit)
}

// ---------------- data structures

#[inline(never)]
extern "rust-preserve-none" fn op_new_list<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let s = base + op_b(op);
    let list = ctx.vm.heap.new_list_from_slice(&stack[s..s + op_c(op)]);
    r!(stack, base, op_a(op)) = list;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_map<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let s = base + op_b(op);
    match retryable_op!(ctx, ctx.vm.op_new_map(&stack[s..s + 2 * op_c(op)])) {
        Ok(m) => {
            r!(stack, base, op_a(op)) = m;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_tuple<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let s = base + op_b(op);
    let count = op_c(op);
    r!(stack, base, op_a(op)) = match stack[s..s + count] {
        [VmValue::Int(a), VmValue::Int(b)] if let Ok(a) = i32::try_from(a) => VmValue::Pair(a, b),
        _ => ctx.vm.new_tuple(&stack[s..s + count]),
    };
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_struct<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let def = op_c(op);
    let n = ctx.prog.types[def].fields.len();
    let s = base + op_b(op);
    let obj = ctx.vm.heap.new_struct(def as u32, &stack[s..s + n]);
    r!(stack, base, op_a(op)) = obj;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_variant<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let variant = op_c(op);
    let n = ctx.prog.variants[variant].arity;
    let s = base + op_b(op);
    r!(stack, base, op_a(op)) = match VmValue::variant1(variant as u32, stack[s]) {
        Some(v) if n == 1 => v,
        _ => ctx.vm.heap.new_variant(variant as u32, &stack[s..s + n]),
    };
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_range<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let (x, y) = (int!(ctx, stack, base, op_b(op), ip, fuel), int!(ctx, stack, base, op_c(op), ip, fuel));
    r!(stack, base, op_a(op)) = ctx.vm.heap.alloc(Obj::Range(x, y, false));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_range_incl<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let (x, y) = (int!(ctx, stack, base, op_b(op), ip, fuel), int!(ctx, stack, base, op_c(op), ip, fuel));
    r!(stack, base, op_a(op)) = ctx.vm.heap.alloc(Obj::Range(x, y, true));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_get_field<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let idx = op_c(op);
    let val = match &r!(stack, base, op_b(op)) {
        &VmValue::Obj(h) => match ctx.vm.heap.obj(h) {
            Obj::Struct(_, f) | Obj::Tuple(f) | Obj::Variant(_, f) | Obj::Closure(_, f) if idx < f.len() => f[idx],
            _ => return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
        },
        &VmValue::Pair(a, b) => VmValue::Int(if idx == 0 { a as i64 } else { b }),
        v => match inline_field(v, idx as u16) {
            VmValue::Uninit => return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
            p => p,
        },
    };
    r!(stack, base, op_a(op)) = val;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_payload<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match payload(&ctx.vm.heap, r!(stack, base, op_b(op))) {
        Some(p) => {
            r!(stack, base, op_a(op)) = p;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        None => exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_set_field<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let o = r!(stack, base, op_a(op));
    let idx = op_b(op);
    let v = r!(stack, base, op_c(op));
    if !ctx.vm.heap.set_struct_field(o, idx, v) {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_index<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let o = r!(stack, base, op_b(op));
    let i = int!(ctx, stack, base, op_c(op), ip, fuel);
    let l = ctx.vm.heap.list(o);
    match to_index(i).and_then(|at| l.get(at)) {
        Some(&v) => {
            r!(stack, base, op_a(op)) = v;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        None => exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_index_map<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let obj = op_b(op);
    let key = op_c(op);
    match dense_get(&ctx.vm.heap, &r!(stack, base, obj), &r!(stack, base, key)) {
        Dense::Hit(v) => {
            r!(stack, base, op_a(op)) = *v;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        _ => match retryable_op!(ctx, ctx.vm.map_index(r!(stack, base, obj), r!(stack, base, key))) {
            Ok(v) => {
                r!(stack, base, op_a(op)) = v;
                dispatch!(ctx, code, ip, stack, base, fuel)
            }
            Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
        },
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_set_index<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let o = r!(stack, base, op_a(op));
    let i = int!(ctx, stack, base, op_b(op), ip, fuel);
    let v = r!(stack, base, op_c(op));
    let at = to_index(i).unwrap_or(usize::MAX);
    if !ctx.vm.heap.set_list_index(o, at, v) {
        return exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel });
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_set_index_map<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let (m, k, v) = (r!(stack, base, op_a(op)), r!(stack, base, op_b(op)), r!(stack, base, op_c(op)));
    if let Err(ctl) = retryable_op!(ctx, ctx.vm.map_set(m, k, v)) {
        return exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel });
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_list_push<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let (l, v) = (r!(stack, base, op_a(op)), r!(stack, base, op_b(op)));
    if ctx.vm.heap.list_push(l, v).is_err() {
        return exit_with(ctx, DispatchExit::LimitMemory { ip, fuel });
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- for loops

#[inline(never)]
extern "rust-preserve-none" fn op_for_list<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    let i = int!(ctx, stack, base, b + 1, ip, fuel);
    let n = int!(ctx, stack, base, b + 2, ip, fuel);
    let l = ctx.vm.heap.list(r!(stack, base, b));
    if i < n && (i as usize) < l.len() {
        let v = l[i as usize];
        r!(stack, base, op_a(op)) = v;
        r!(stack, base, b + 1) = VmValue::Int(i + 1);
        dispatch!(ctx, code, ip, stack, base, fuel)
    }
    let nip = jump_ip!(ip, op_c_i16(op));
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_range<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    let i = int!(ctx, stack, base, b, ip, fuel);
    if i < int!(ctx, stack, base, b + 1, ip, fuel) {
        r!(stack, base, op_a(op)) = VmValue::Int(i);
        r!(stack, base, b) = VmValue::Int(i + 1);
        dispatch!(ctx, code, ip, stack, base, fuel)
    }
    let nip = jump_ip!(ip, op_c_i16(op));
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_range_incl<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    let i = int!(ctx, stack, base, b, ip, fuel);
    let end = int!(ctx, stack, base, b + 1, ip, fuel);
    if !r!(stack, base, b + 2).as_bool() && i <= end {
        r!(stack, base, op_a(op)) = VmValue::Int(i);
        if i == end {
            r!(stack, base, b + 2) = VmValue::Bool(true);
        } else {
            r!(stack, base, b) = VmValue::Int(i + 1);
        }
        dispatch!(ctx, code, ip, stack, base, fuel)
    }
    let nip = jump_ip!(ip, op_c_i16(op));
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_set<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let it = r!(stack, base, op_b(op));
    if !ctx.vm.set_step(it, &mut r!(stack, base, op_a(op))) {
        let nip = jump_ip!(ip, op_c_i16(op));
        dispatch!(ctx, code, nip, stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_map<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let (it, k) = (r!(stack, base, op_b(op)), base + op_a(op));
    if !ctx.vm.map_step(it, &mut stack[k..k + 2]) {
        let nip = jump_ip!(ip, op_c_i16(op));
        dispatch!(ctx, code, nip, stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_chars<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    match ctx.vm.next_char(&r!(stack, base, b), &r!(stack, base, b + 1)) {
        Some((c, at)) => {
            r!(stack, base, op_a(op)) = c;
            r!(stack, base, b + 1) = VmValue::Int(at);
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        None => {
            let nip = jump_ip!(ip, op_c_i16(op));
            dispatch!(ctx, code, nip, stack, base, fuel)
        }
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_list_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    let i = int!(ctx, stack, base, b + 1, ip, fuel);
    let n = int!(ctx, stack, base, b + 2, ip, fuel);
    let l = ctx.vm.heap.list(r!(stack, base, b));
    if i < n && (i as usize) < l.len() {
        let v = l[i as usize];
        r!(stack, base, op_a(op)) = v;
        r!(stack, base, b + 1) = VmValue::Int(i + 1);
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_range_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    let i = int!(ctx, stack, base, b, ip, fuel);
    if i < int!(ctx, stack, base, b + 1, ip, fuel) {
        r!(stack, base, op_a(op)) = VmValue::Int(i);
        r!(stack, base, b) = VmValue::Int(i + 1);
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_range_incl_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    let i = int!(ctx, stack, base, b, ip, fuel);
    let end = int!(ctx, stack, base, b + 1, ip, fuel);
    if !r!(stack, base, b + 2).as_bool() && i <= end {
        r!(stack, base, op_a(op)) = VmValue::Int(i);
        if i == end {
            r!(stack, base, b + 2) = VmValue::Bool(true);
        } else {
            r!(stack, base, b) = VmValue::Int(i + 1);
        }
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_set_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let it = r!(stack, base, op_b(op));
    if ctx.vm.set_step(it, &mut r!(stack, base, op_a(op))) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_map_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let (it, k) = (r!(stack, base, op_b(op)), base + op_a(op));
    if ctx.vm.map_step(it, &mut stack[k..k + 2]) {
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_for_chars_back<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_b(op);
    if let Some((c, at)) = ctx.vm.next_char(&r!(stack, base, b), &r!(stack, base, b + 1)) {
        r!(stack, base, op_a(op)) = c;
        r!(stack, base, b + 1) = VmValue::Int(at);
        back_edge!(ctx, code, ip, op_c_i16(op), stack, base, fuel)
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

// ---------------- containers & helpers

#[inline(never)]
extern "rust-preserve-none" fn op_unpack_range<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let b = op_a(op);
    match unpack_range(ctx.vm.heap.get(r!(stack, base, op_b(op)))) {
        Some((first, last, done)) => {
            r!(stack, base, b) = VmValue::Int(first);
            r!(stack, base, b + 1) = VmValue::Int(last);
            r!(stack, base, b + 2) = VmValue::Bool(done);
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        None => exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_new_iter<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match ctx.vm.container_iter(r!(stack, base, op_b(op))) {
        Ok(v) => {
            r!(stack, base, op_a(op)) = v;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_jlist_len<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if ctx.vm.heap.list(r!(stack, base, op_a(op))).len() != op_b(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_jlist_min_len<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let nip = if ctx.vm.heap.list(r!(stack, base, op_a(op))).len() < op_b(op) { jump_ip!(ip, op_c_i16(op)) } else { ip };
    dispatch!(ctx, code, nip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_list_elem<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let l = ctx.vm.heap.list(r!(stack, base, op_b(op)));
    let idx = op_c_i16(op);
    let i = if idx < 0 { l.len() as isize + idx as isize } else { idx as isize };
    if let Some(v) = l.get(i as usize) {
        r!(stack, base, op_a(op)) = *v;
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_len<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let o = r!(stack, base, op_b(op));
    let n = match ctx.vm.heap.get(o) {
        Obj::List(l) => l.len(),
        Obj::Str(s) => s.len(),
        Obj::Bytes(b) => b.len(),
        Obj::Map(m) => m.live,
        Obj::Set(s) => s.live,
        _ => 0,
    };
    r!(stack, base, op_a(op)) = VmValue::Int(n as i64);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_list_get<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let opt = match to_index(int!(ctx, stack, base, op_c(op), ip, fuel)).and_then(|i| ctx.vm.heap.list(r!(stack, base, op_b(op))).get(i)) {
        Some(&v) => ctx.vm.some(v),
        None => ctx.vm.none(),
    };
    r!(stack, base, op_a(op)) = opt;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_list_get_or<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    if let Some(&v) = to_index(int!(ctx, stack, base, op_c(op), ip, fuel)).and_then(|i| ctx.vm.heap.list(r!(stack, base, op_b(op))).get(i))
    {
        r!(stack, base, op_a(op)) = v;
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_list_pop<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let opt = match ctx.vm.heap.list_mut_truncating(r!(stack, base, op_b(op))).and_then(Vec::pop) {
        Some(v) => ctx.vm.some(v),
        None => ctx.vm.none(),
    };
    r!(stack, base, op_a(op)) = opt;
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_map_get<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let dst = op_a(op);
    let map = op_b(op);
    let key = op_c(op);
    match dense_get(&ctx.vm.heap, &r!(stack, base, map), &r!(stack, base, key)) {
        Dense::Hit(v) => {
            let v = *v;
            r!(stack, base, dst) = ctx.vm.some(v);
            if let Some(&peek) = code.get(ip)
                && (peek & 0xff) as u8 == TAG_JNOT_SOME
                && op_a(peek) == dst
            {
                r!(stack, base, op_b(peek)) = v;
                dispatch!(ctx, code, ip + 1, stack, base, fuel)
            }
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Dense::Miss => {
            r!(stack, base, dst) = ctx.vm.none();
            if let Some(&peek) = code.get(ip)
                && (peek & 0xff) as u8 == TAG_JNOT_SOME
                && op_a(peek) == dst
            {
                let nip = jump_ip!(ip + 1, op_c_i16(peek));
                dispatch!(ctx, code, nip, stack, base, fuel)
            }
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Dense::Unknown => match retryable_op!(ctx, ctx.vm.map_get_option(r!(stack, base, map), r!(stack, base, key))) {
            Ok(v) => {
                r!(stack, base, dst) = v;
                dispatch!(ctx, code, ip, stack, base, fuel)
            }
            Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
        },
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_map_get_or<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let dst = op_a(op);
    let map = op_b(op);
    let key = op_c(op);
    match dense_get(&ctx.vm.heap, &r!(stack, base, map), &r!(stack, base, key)) {
        Dense::Hit(v) => r!(stack, base, dst) = *v,
        Dense::Miss => {}
        Dense::Unknown => {
            let (m, k) = (r!(stack, base, map), r!(stack, base, key));
            if let Err(ctl) = retryable_op!(ctx, ctx.vm.map_get_or(m, k, &mut r!(stack, base, dst))) {
                return exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel });
            }
        }
    }
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_map_has<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match retryable_op!(ctx, ctx.vm.map_has(r!(stack, base, op_b(op)), r!(stack, base, op_c(op)))) {
        Ok(b) => {
            r!(stack, base, op_a(op)) = VmValue::Bool(b);
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_set_has<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match retryable_op!(ctx, ctx.vm.set_contains(r!(stack, base, op_b(op)), r!(stack, base, op_c(op)))) {
        Ok(b) => {
            r!(stack, base, op_a(op)) = VmValue::Bool(b);
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_set_add<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    match retryable_op!(ctx, ctx.vm.set_add(r!(stack, base, op_b(op)), r!(stack, base, op_c(op)))) {
        Ok(b) => {
            r!(stack, base, op_a(op)) = VmValue::Bool(b);
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_int_to_float<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    r!(stack, base, op_a(op)) = VmValue::Float(int!(ctx, stack, base, op_b(op), ip, fuel) as f64);
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_append_str<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let local = op_a(op);
    let (cur, add) = (r!(stack, base, local), r!(stack, base, op_b(op)));
    match retryable_op!(ctx, ctx.vm.op_append_str(cur, add)) {
        Ok(v) => {
            r!(stack, base, local) = v;
            dispatch!(ctx, code, ip, stack, base, fuel)
        }
        Err(ctl) => exit_with(ctx, DispatchExit::FailStop { ctl, ip, fuel }),
    }
}

#[inline(never)]
extern "rust-preserve-none" fn op_freeze_str<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    ctx.vm.heap.freeze_str(r!(stack, base, op_a(op)));
    dispatch!(ctx, code, ip, stack, base, fuel)
}

#[inline(never)]
extern "rust-preserve-none" fn op_to_str<'a>(
    ctx: &mut DispatchCtx<'a>,
    code: &'a [u64],
    ip: usize,
    stack: &mut [VmValue],
    base: usize,
    fuel: i64,
    op: u64,
) {
    let v = r!(stack, base, op_b(op));
    if let VmValue::Obj(_) = v
        && let Obj::Str(_) = ctx.vm.heap.get(v)
    {
        r!(stack, base, op_a(op)) = v;
        dispatch!(ctx, code, ip, stack, base, fuel)
    }
    exit_with(ctx, DispatchExit::SlowOp { ip: ip - 1, fuel })
}
