//! Register-based bytecode.
//!
//! Every instruction is 8 bytes. Registers are frame-relative `u16` slots.
//! Jump offsets are relative to the instruction *after* the jump.

use super::value::TypeDesc;
use crate::hir::FormatSpec;
use std::sync::Arc;

pub type Reg = u16;

/// No register (see `Proto::captures`).
pub const NO_REG: Reg = Reg::MAX;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Nop,
    Move {
        dst: Reg,
        src: Reg,
    },
    LoadUnit {
        dst: Reg,
    },
    LoadBool {
        dst: Reg,
        v: bool,
    },
    LoadInt {
        dst: Reg,
        v: i32,
    },
    LoadConst {
        dst: Reg,
        k: u32,
    },
    LoadTag {
        dst: Reg,
        tag: u32,
    },
    LoadGlobal {
        dst: Reg,
        g: u32,
    },
    StoreGlobal {
        g: u32,
        src: Reg,
    },
    NewCell {
        dst: Reg,
        src: Reg,
    },
    LoadCell {
        dst: Reg,
        cell: Reg,
    },
    StoreCell {
        cell: Reg,
        src: Reg,
    },
    MakeClosure {
        dst: Reg,
        proto: u32,
    },

    // Integer arithmetic (checked).
    AddI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    SubI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    MulI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    DivI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    RemI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    PowI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    AddIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    MulIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    /// `a / k` for a constant `k` other than 0.
    DivIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    /// `a % k` for a constant `k` other than 0.
    RemIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    NegI {
        dst: Reg,
        a: Reg,
    },
    BitAnd {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    BitOr {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    BitXor {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Shl {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Shr {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    BitNot {
        dst: Reg,
        a: Reg,
    },
    // Float arithmetic.
    AddF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    SubF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    MulF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    DivF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    RemF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    PowF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    NegF {
        dst: Reg,
        a: Reg,
    },
    // Generic arithmetic (dispatch on runtime values: numbers, durations, concatenation).
    Add {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Sub {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Mul {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Div {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Rem {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Pow {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Neg {
        dst: Reg,
        a: Reg,
    },
    Not {
        dst: Reg,
        a: Reg,
    },

    // Comparisons producing bool.
    EqI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    NeI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    LtI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    LeI {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// `a == k` for a constant `k` (and so on for the next ones).
    EqIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    NeIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    LtIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    LeIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    GtIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    GeIK {
        dst: Reg,
        a: Reg,
        k: i16,
    },
    EqF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    NeF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    LtF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    LeF {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Eq {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Ne {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Lt {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Le {
        dst: Reg,
        a: Reg,
        b: Reg,
    },

    // Control flow.
    Jmp {
        off: i32,
    },
    Loop {
        off: i32,
    },
    JmpIf {
        cond: Reg,
        off: i32,
    },
    JmpIfNot {
        cond: Reg,
        off: i32,
    },
    /// Fused compare-and-branch: jump if NOT (a op b).
    JNltI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNleI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNeqI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNneI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNltF {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNleF {
        a: Reg,
        b: Reg,
        off: i16,
    },
    /// Generic equality (strings, ...) and branch: jump if NOT (a == b).
    JNeq {
        a: Reg,
        b: Reg,
        off: i16,
    },
    /// Jump if NOT (a != b).
    JNne {
        a: Reg,
        b: Reg,
        off: i16,
    },
    /// `JNeq` with constant `k` (a string, ...) as `b`.
    JNeqK {
        a: Reg,
        k: u16,
        off: i16,
    },
    /// `JNne` with constant `k` as `b`.
    JNneK {
        a: Reg,
        k: u16,
        off: i16,
    },
    /// Compare with an immediate and branch: jump if NOT (a op k).
    JNltIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNleIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNgtIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNgeIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNeqIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNneIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    /// Jump if the value is not `Some(_)`; otherwise its payload goes to `dst`.
    JNotSome {
        src: Reg,
        dst: Reg,
        off: i16,
    },
    /// Jump if the value is not `Ok(_)`; otherwise its payload goes to `dst`.
    JNotOk {
        src: Reg,
        dst: Reg,
        off: i16,
    },
    /// Jump if the value is not the given variant.
    JNotVariant {
        src: Reg,
        variant: u16,
        off: i16,
    },
    /// Jump to `Program::switches[table]`'s target for the runtime
    /// variant id of `src` (`match` on an enum).
    SwitchVariant {
        src: Reg,
        table: u32,
    },
    /// Jump to `Program::switches[table]`'s target for the integer `src`.
    SwitchInt {
        src: Reg,
        table: u32,
    },
    /// Jump to `Program::switches[table]`'s target for the byte of `src`, a
    /// string of one byte; its default target for any other string.
    SwitchChar {
        src: Reg,
        table: u32,
    },

    // Backward branches, which end rotated loops: each is the forward
    // instruction of the same name without `Back`, but its (negative)
    // offset jumps back, and taking it is a safe point charging the loop's
    // fuel like `Loop`.
    JmpIfBack {
        cond: Reg,
        off: i32,
    },
    JmpIfNotBack {
        cond: Reg,
        off: i32,
    },
    JNltIBack {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNleIBack {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNeqIBack {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNneIBack {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JNltIKBack {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNleIKBack {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNgtIKBack {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNgeIKBack {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNeqIKBack {
        a: Reg,
        k: i16,
        off: i16,
    },
    JNneIKBack {
        a: Reg,
        k: i16,
        off: i16,
    },
    /// Jump back if the value is `Some(_)`, with its payload in `dst`.
    JSomeBack {
        src: Reg,
        dst: Reg,
        off: i16,
    },

    // Calls.
    /// Calls the function value in register `callee` (anywhere) with the
    /// `argc` arguments at `base..`; result written to `dst`. A closure's
    /// frame starts at `base`.
    CallR {
        callee: Reg,
        base: Reg,
        dst: Reg,
        argc: u8,
    },
    /// Arguments at `base..`; result written to `base`.
    CallDirect {
        base: Reg,
        proto: u32,
    },
    /// Like `CallDirect` (for the first 65536 protos), writing the result
    /// to `dst`.
    CallDirectTo {
        base: Reg,
        dst: Reg,
        proto: u16,
    },
    /// Calls the running closure (a local function calling itself) with the
    /// arguments at `base..`; result written to `dst`.
    CallSelf {
        base: Reg,
        dst: Reg,
    },
    /// Arguments at `base..`; result written to `base`.
    CallNative {
        base: Reg,
        native: u32,
    },
    /// Like `CallNative` (for the first 65536 natives), writing the result
    /// to `dst`. The arguments may be any registers, e.g. a local's.
    CallNativeTo {
        base: Reg,
        dst: Reg,
        native: u16,
    },
    /// Tail call of the function value in register `callee` with the
    /// `argc` arguments at `base..`. (A native is called as by `CallR`,
    /// writing its result to `base`: the compiler returns it next.)
    TailCall {
        callee: Reg,
        base: Reg,
        argc: u16,
    },
    TailCallDirect {
        base: Reg,
        proto: u32,
    },
    Ret {
        src: Reg,
    },
    /// Returns the variant `variant` with the one payload `src`.
    RetVariant {
        src: Reg,
        variant: u16,
    },
    RetUnit,
    /// Ends a cleanup pad: leaves the frame and continues unwinding a
    /// cancelled task (`Proto::cleanups`).
    Unwind,

    // Data structures.
    NewList {
        dst: Reg,
        start: Reg,
        count: u16,
    },
    NewMap {
        dst: Reg,
        start: Reg,
        count: u16,
    },
    NewTuple {
        dst: Reg,
        start: Reg,
        count: u16,
    },
    NewStruct {
        dst: Reg,
        start: Reg,
        def: u16,
    },
    NewVariant {
        dst: Reg,
        start: Reg,
        variant: u16,
    },
    NewRange {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    NewRangeIncl {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    GetField {
        dst: Reg,
        obj: Reg,
        idx: u16,
    },
    /// The field of a one-field variant.
    Payload {
        dst: Reg,
        src: Reg,
    },
    SetField {
        obj: Reg,
        idx: u16,
        src: Reg,
    },
    Index {
        dst: Reg,
        obj: Reg,
        idx: Reg,
    },
    IndexMap {
        dst: Reg,
        obj: Reg,
        key: Reg,
    },
    SetIndex {
        obj: Reg,
        idx: Reg,
        src: Reg,
    },
    SetIndexMap {
        obj: Reg,
        key: Reg,
        src: Reg,
    },
    /// `base` = object, `base+1` = start, `base+2` = end; flags: 1 = has start,
    /// 2 = has end, 4 = inclusive, 8 = str, 16 = bytes.
    Slice {
        dst: Reg,
        base: Reg,
        flags: u16,
    },
    ListPush {
        list: Reg,
        src: Reg,
    },
    /// Repeat count check: fails if negative.
    CheckRepeat {
        src: Reg,
    },
    /// `base` = list, `base+1` = index, `base+2` = length snapshot.
    /// If index < min(len, current length): elem = list[index], index += 1;
    /// otherwise jump.
    ForList {
        elem: Reg,
        base: Reg,
        off: i16,
    },
    /// `base` = counter, `base+1` = end (exclusive).
    ForRange {
        var: Reg,
        base: Reg,
        off: i16,
    },
    /// `base` = counter, `base+1` = end (inclusive), `base+2` = done flag.
    ForRangeIncl {
        var: Reg,
        base: Reg,
        off: i16,
    },
    /// `base` = the iterator of a set (`NewIter`): `elem` = its next
    /// element if there is one; otherwise jump.
    ForSet {
        elem: Reg,
        base: Reg,
        off: i16,
    },
    /// `base` = the iterator of a map (`NewIter`): if there is an entry
    /// left, `key` = its key and `key+1` = its value; otherwise jump.
    ForMap {
        key: Reg,
        base: Reg,
        off: i16,
    },
    /// `base` = a string, `base+1` = a byte offset in it: if there is a
    /// character there, `elem` = that character (a string) and the offset
    /// moves past it; otherwise jump.
    ForChars {
        elem: Reg,
        base: Reg,
        off: i16,
    },
    /// The steps of rotated loops (at their end): like `ForList`,
    /// `ForRange` and `ForRangeIncl`, but jumping back to the loop's body
    /// when there is an element (a safe point, charging the loop's fuel).
    ForListBack {
        elem: Reg,
        base: Reg,
        off: i16,
    },
    ForRangeBack {
        var: Reg,
        base: Reg,
        off: i16,
    },
    ForRangeInclBack {
        var: Reg,
        base: Reg,
        off: i16,
    },
    ForSetBack {
        elem: Reg,
        base: Reg,
        off: i16,
    },
    ForMapBack {
        key: Reg,
        base: Reg,
        off: i16,
    },
    ForCharsBack {
        elem: Reg,
        base: Reg,
        off: i16,
    },
    /// Range value `src` as the registers of an inclusive range loop
    /// (`ForRangeIncl`): `base` = start, `base+1` = last, `base+2` = done.
    UnpackRange {
        base: Reg,
        src: Reg,
    },
    /// The iterator of map or set `src` (`Vm::container_iter`): it goes on
    /// with the entries `src` has now, whatever happens to `src` meanwhile.
    NewIter {
        dst: Reg,
        src: Reg,
    },
    /// List pattern length test: jump if `len != n` (exact) or `len < n`.
    JListLen {
        src: Reg,
        n: u16,
        off: i16,
    },
    JListMinLen {
        src: Reg,
        n: u16,
        off: i16,
    },
    /// Element at `idx` (negative counts from the end).
    ListElem {
        dst: Reg,
        list: Reg,
        idx: i16,
    },
    /// Sub-list without the first `before` and last `after` elements.
    ListRest {
        dst: Reg,
        list: Reg,
        before: u8,
        after: u8,
    },

    Len {
        dst: Reg,
        src: Reg,
    },
    /// `list.get(idx)`: the element as an option.
    ListGet {
        dst: Reg,
        list: Reg,
        idx: Reg,
    },
    /// `list.get(idx) ?? default`, with `default` in `dst` beforehand:
    /// `dst` = the element, if there is one.
    ListGetOr {
        dst: Reg,
        list: Reg,
        idx: Reg,
    },
    /// `list.pop()`.
    ListPop {
        dst: Reg,
        list: Reg,
    },
    /// `map.get(key)`.
    MapGet {
        dst: Reg,
        map: Reg,
        key: Reg,
    },
    /// `map.get((a, b, ...))`: the tuple key's elements are the `n`
    /// registers from `start` (no tuple is allocated).
    MapGetTuple {
        dst: Reg,
        map: Reg,
        start: Reg,
        n: u8,
    },
    /// `map.contains((a, b, ...))`, as `MapGetTuple`.
    MapHasTuple {
        dst: Reg,
        map: Reg,
        start: Reg,
        n: u8,
    },
    /// `map.get_or(key, default)` and `map.get(key) ?? default`, with
    /// `default` in `dst` beforehand: `dst` = the value of `key`, if any.
    MapGetOr {
        dst: Reg,
        map: Reg,
        key: Reg,
    },
    /// `map.contains(key)`.
    MapHas {
        dst: Reg,
        map: Reg,
        key: Reg,
    },
    /// `set.contains(key)`.
    SetHas {
        dst: Reg,
        set: Reg,
        key: Reg,
    },
    /// `set.add(key)`.
    SetAdd {
        dst: Reg,
        set: Reg,
        key: Reg,
    },
    IntToFloat {
        dst: Reg,
        src: Reg,
    },
    /// `local += src` for a string local that only this register holds
    /// (see `compile::builder_locals`): appends in place to the local's
    /// private buffer (`Obj::StrBuf`), making one first if needed.
    AppendStr {
        local: Reg,
        src: Reg,
    },
    /// Before any other read of such a local: turns its buffer, if it has
    /// one, into an ordinary string (in place: the handle stays the same).
    FreezeStr {
        reg: Reg,
    },
    /// Jump unless string `src` is of class `class & 0x7f` (as `StrClass`);
    /// with bit 7 of `class` set, jump if it is.
    JStrClass {
        src: Reg,
        class: u8,
        off: i16,
    },
    /// Whether string `src` is non-empty and every character is of `class`
    /// (0 digit, 1 alphabetic, 2 alphanumeric, 3 whitespace, 4 uppercase,
    /// 5 lowercase): `str.is_digit` ... `str.is_lower`.
    StrClass {
        dst: Reg,
        src: Reg,
        class: u8,
    },

    // Strings.
    ToStr {
        dst: Reg,
        src: Reg,
    },
    Debug {
        dst: Reg,
        src: Reg,
    },
    /// String interpolation: the template `Program::templates[tmpl]`
    /// with the values of its registers, as a new string.
    Interp {
        dst: Reg,
        tmpl: u32,
    },
    /// `local += "..."` (an interpolation) for a string builder local (see
    /// `AppendStr`): formats the template and appends it in place.
    AppendInterp {
        local: Reg,
        tmpl: u32,
    },

    // Misc.
    Spawn {
        dst: Reg,
        closure: Reg,
    },
    /// Top-level `?` on an `Err`: fail with the error.
    TryTop {
        src: Reg,
    },
    /// An internal error with the constant message `msg`.
    Internal {
        msg: u32,
    },
}

const _: () = assert!(std::mem::size_of::<Op>() == 8);

impl Op {
    /// The offset of a branch instruction (relative to the next one).
    pub fn branch_off(&self) -> Option<i32> {
        match *self {
            Op::Jmp { off }
            | Op::Loop { off }
            | Op::JmpIf { off, .. }
            | Op::JmpIfNot { off, .. }
            | Op::JmpIfBack { off, .. }
            | Op::JmpIfNotBack { off, .. } => Some(off),
            Op::JNltI { off, .. }
            | Op::JNleI { off, .. }
            | Op::JNeqI { off, .. }
            | Op::JNneI { off, .. }
            | Op::JNltF { off, .. }
            | Op::JNleF { off, .. }
            | Op::JNeq { off, .. }
            | Op::JNne { off, .. }
            | Op::JNeqK { off, .. }
            | Op::JNneK { off, .. }
            | Op::JNltIK { off, .. }
            | Op::JNleIK { off, .. }
            | Op::JNgtIK { off, .. }
            | Op::JNgeIK { off, .. }
            | Op::JNeqIK { off, .. }
            | Op::JNneIK { off, .. }
            | Op::JNotSome { off, .. }
            | Op::JNotOk { off, .. }
            | Op::JNotVariant { off, .. }
            | Op::JStrClass { off, .. }
            | Op::JListLen { off, .. }
            | Op::JListMinLen { off, .. }
            | Op::ForList { off, .. }
            | Op::ForRange { off, .. }
            | Op::ForRangeIncl { off, .. }
            | Op::ForSet { off, .. }
            | Op::ForMap { off, .. }
            | Op::ForChars { off, .. }
            | Op::JNltIBack { off, .. }
            | Op::JNleIBack { off, .. }
            | Op::JNeqIBack { off, .. }
            | Op::JNneIBack { off, .. }
            | Op::JNltIKBack { off, .. }
            | Op::JNleIKBack { off, .. }
            | Op::JNgtIKBack { off, .. }
            | Op::JNgeIKBack { off, .. }
            | Op::JNeqIKBack { off, .. }
            | Op::JNneIKBack { off, .. }
            | Op::JSomeBack { off, .. }
            | Op::ForListBack { off, .. }
            | Op::ForRangeBack { off, .. }
            | Op::ForRangeInclBack { off, .. }
            | Op::ForSetBack { off, .. }
            | Op::ForMapBack { off, .. }
            | Op::ForCharsBack { off, .. } => Some(off as i32),
            _ => None,
        }
    }

    /// Sets the offset of a branch instruction; false if it does not fit
    /// the instruction's offset field (which is then left unchanged).
    pub fn set_branch_off(&mut self, to: i64) -> bool {
        match self {
            Op::Jmp { off }
            | Op::Loop { off }
            | Op::JmpIf { off, .. }
            | Op::JmpIfNot { off, .. }
            | Op::JmpIfBack { off, .. }
            | Op::JmpIfNotBack { off, .. } => match i32::try_from(to) {
                Ok(o) => *off = o,
                Err(_) => return false,
            },
            Op::JNltI { off, .. }
            | Op::JNleI { off, .. }
            | Op::JNeqI { off, .. }
            | Op::JNneI { off, .. }
            | Op::JNltF { off, .. }
            | Op::JNleF { off, .. }
            | Op::JNeq { off, .. }
            | Op::JNne { off, .. }
            | Op::JNeqK { off, .. }
            | Op::JNneK { off, .. }
            | Op::JNltIK { off, .. }
            | Op::JNleIK { off, .. }
            | Op::JNgtIK { off, .. }
            | Op::JNgeIK { off, .. }
            | Op::JNeqIK { off, .. }
            | Op::JNneIK { off, .. }
            | Op::JNotSome { off, .. }
            | Op::JNotOk { off, .. }
            | Op::JNotVariant { off, .. }
            | Op::JStrClass { off, .. }
            | Op::JListLen { off, .. }
            | Op::JListMinLen { off, .. }
            | Op::ForList { off, .. }
            | Op::ForRange { off, .. }
            | Op::ForRangeIncl { off, .. }
            | Op::ForSet { off, .. }
            | Op::ForMap { off, .. }
            | Op::ForChars { off, .. }
            | Op::JNltIBack { off, .. }
            | Op::JNleIBack { off, .. }
            | Op::JNeqIBack { off, .. }
            | Op::JNneIBack { off, .. }
            | Op::JNltIKBack { off, .. }
            | Op::JNleIKBack { off, .. }
            | Op::JNgtIKBack { off, .. }
            | Op::JNgeIKBack { off, .. }
            | Op::JNeqIKBack { off, .. }
            | Op::JNneIKBack { off, .. }
            | Op::JSomeBack { off, .. }
            | Op::ForListBack { off, .. }
            | Op::ForRangeBack { off, .. }
            | Op::ForRangeInclBack { off, .. }
            | Op::ForSetBack { off, .. }
            | Op::ForMapBack { off, .. }
            | Op::ForCharsBack { off, .. } => match i16::try_from(to) {
                Ok(o) => *off = o,
                Err(_) => return false,
            },
            _ => panic!("not a branch: {self:?}"),
        }
        true
    }

    /// The backward form of a branch (`Loop` for `Jmp`, see also
    /// `JmpIfBack`), if it has one.
    pub fn backward(self) -> Option<Op> {
        Some(match self {
            Op::Jmp { off } => Op::Loop { off },
            Op::JmpIf { cond, off } => Op::JmpIfBack { cond, off },
            Op::JmpIfNot { cond, off } => Op::JmpIfNotBack { cond, off },
            Op::JNltI { a, b, off } => Op::JNltIBack { a, b, off },
            Op::JNleI { a, b, off } => Op::JNleIBack { a, b, off },
            Op::JNeqI { a, b, off } => Op::JNeqIBack { a, b, off },
            Op::JNneI { a, b, off } => Op::JNneIBack { a, b, off },
            Op::JNltIK { a, k, off } => Op::JNltIKBack { a, k, off },
            Op::JNleIK { a, k, off } => Op::JNleIKBack { a, k, off },
            Op::JNgtIK { a, k, off } => Op::JNgtIKBack { a, k, off },
            Op::JNgeIK { a, k, off } => Op::JNgeIKBack { a, k, off },
            Op::JNeqIK { a, k, off } => Op::JNeqIKBack { a, k, off },
            Op::JNneIK { a, k, off } => Op::JNneIKBack { a, k, off },
            _ => return None,
        })
    }

    /// Whether taking the branch is a safe point charging the loop's fuel.
    pub fn is_back_edge(&self) -> bool {
        matches!(
            self,
            Op::Loop { .. }
                | Op::JmpIfBack { .. }
                | Op::JmpIfNotBack { .. }
                | Op::JNltIBack { .. }
                | Op::JNleIBack { .. }
                | Op::JNeqIBack { .. }
                | Op::JNneIBack { .. }
                | Op::JNltIKBack { .. }
                | Op::JNleIKBack { .. }
                | Op::JNgtIKBack { .. }
                | Op::JNgeIKBack { .. }
                | Op::JNeqIKBack { .. }
                | Op::JNneIKBack { .. }
                | Op::JSomeBack { .. }
                | Op::ForListBack { .. }
                | Op::ForRangeBack { .. }
                | Op::ForRangeInclBack { .. }
                | Op::ForSetBack { .. }
                | Op::ForMapBack { .. }
                | Op::ForCharsBack { .. }
        )
    }

    /// Whether execution never continues with the next instruction.
    pub fn is_unconditional(&self) -> bool {
        matches!(
            self,
            Op::Jmp { .. }
                | Op::Loop { .. }
                | Op::Ret { .. }
                | Op::RetVariant { .. }
                | Op::RetUnit
                | Op::Unwind
                | Op::TailCallDirect { .. }
                | Op::Internal { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Proto {
    pub name: String,
    pub file: u32,
    pub nparams: u16,
    pub nregs: u16,
    pub code: Vec<Op>,
    pub lines: Vec<u32>,
    /// The registers whose values a closure of this proto captures, from
    /// the frame that makes it (`NO_REG`: unit, for a local not declared
    /// yet). Calling the closure copies them to the registers following the
    /// parameters.
    pub captures: Vec<Reg>,
    /// Sorted, disjoint code ranges inside which deferred code is pending,
    /// with the cleanup pad that runs it when the task is cancelled.
    pub cleanups: Vec<Cleanup>,
    /// For each instruction, the inlined call it belongs to: 0 for none, `i`
    /// for `inlined[i - 1]`. Empty if nothing was inlined. The instruction's
    /// line (`lines`) is then in the inlined function's file.
    pub sites: Vec<u16>,
    pub inlined: Vec<InlineSite>,
}

/// A function body inlined into a proto (see `Proto::sites`).
#[derive(Clone, Debug)]
pub struct InlineSite {
    pub name: String,
    pub file: u32,
    /// The line of the call, in the file of the enclosing site (`parent`,
    /// 0 for the proto itself).
    pub line: u32,
    pub parent: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Cleanup {
    pub start: u32,
    pub end: u32,
    pub pad: u32,
}

impl Proto {
    /// The cleanup pad for a frame stopped at instruction `pos`.
    pub fn cleanup_at(&self, pos: usize) -> Option<usize> {
        let i = self.cleanups.partition_point(|c| c.start as usize <= pos).checked_sub(1)?;
        let c = self.cleanups[i];
        (pos < c.end as usize).then_some(c.pad as usize)
    }
}

#[derive(Clone, Debug)]
pub enum Const {
    Int(i64),
    Float(f64),
    Dur(i64),
    Str(String),
    Bytes(Vec<u8>),
    /// A script function (closure without captures).
    Fn(u32),
    Native(u32),
    TypeDesc(Arc<TypeDesc>),
    /// An immutable tuple of earlier constants (constant trait dictionaries).
    Tuple(Vec<u32>),
}

/// The jump table of a `SwitchVariant`, `SwitchInt` or `SwitchChar`: instruction
/// `targets[v - first]` for value `v`, `default` for the others.
#[derive(Clone, Debug, Default)]
pub struct Switch {
    pub first: i64,
    pub targets: Vec<u32>,
    pub default: u32,
}

impl Switch {
    #[inline]
    pub fn target(&self, v: i64) -> usize {
        match self.targets.get(v.wrapping_sub(self.first) as u64 as usize) {
            Some(&t) => t as usize,
            None => self.default as usize,
        }
    }
}

/// A string interpolation: its pieces, in order.
#[derive(Clone, Debug, Default)]
pub struct Template {
    pub pieces: Vec<Piece>,
    /// The total length of the text pieces.
    pub text_len: usize,
}

#[derive(Clone, Debug)]
pub enum Piece {
    Text(String),
    /// The display form of the value in a register.
    Value(Reg),
    /// The value in a register, formatted by a format specification.
    Format(Reg, FormatSpec),
}

#[derive(Clone, Debug)]
pub struct StructRt {
    pub name: String,
    pub module: String,
    pub fields: Vec<String>,
    pub is_enum: bool,
}

#[derive(Clone, Debug)]
pub struct VariantRt {
    pub def: u32,
    pub index: u32,
    pub name: String,
    pub field_names: Option<Vec<String>>,
    pub arity: usize,
}

/// Well-known runtime ids.
#[derive(Clone, Copy, Debug, Default)]
pub struct RtIds {
    pub some: u32,
    pub none: u32,
    pub ok: u32,
    pub err: u32,
    pub error_def: u32,
    pub json: Option<JsonIds>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct JsonIds {
    pub def: u32,
    pub null: u32,
    pub bool_: u32,
    pub num: u32,
    pub str_: u32,
    pub arr: u32,
    pub obj: u32,
}

/// A compiled program, shared by `Arc`. It is immutable once built, except
/// that a REPL session appends to it between inputs (never renumbering).
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub protos: Vec<Proto>,
    pub consts: Vec<Const>,
    pub nglobals: usize,
    pub global_names: Vec<String>,
    pub types: Vec<StructRt>,
    pub variants: Vec<VariantRt>,
    pub ids: RtIds,
    pub entry: u32,
    pub tests: Vec<(String, u32)>,
    pub files: Vec<String>,
    /// The names of the natives, by id.
    pub native_names: Arc<[String]>,
    pub native_argc: Arc<[u16]>,
    /// The templates of string interpolations (`Op::Interp`).
    pub templates: Vec<Template>,
    /// The jump tables of `SwitchVariant`, `SwitchInt` and `SwitchChar`.
    pub switches: Vec<Switch>,
}
