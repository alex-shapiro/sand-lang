//! Register-based bytecode.
//!
//! Every instruction is 8 bytes. Registers are frame-relative `u16` slots.
//! Jump offsets are relative to the instruction *after* the jump.

use super::value::TypeDesc;
use crate::hir::FormatSpec;
use std::sync::Arc;

pub type Reg = u16;

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
    /// Loads global `g` into `dst` before a loop that uses it; jumps if the
    /// global is not initialized yet (to an unhoisted copy of the loop).
    HoistGlobal {
        dst: Reg,
        g: u16,
        off: i16,
    },
    LoadCapture {
        dst: Reg,
        idx: u16,
    },
    LoadCaptureCell {
        dst: Reg,
        idx: u16,
    },
    StoreCaptureCell {
        idx: u16,
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
    /// Jump if the value is not `Some(_)
    JNotSome {
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

    // Calls.
    /// Callee at `base`, arguments at `base+1..`; result written to `dst`.
    Call {
        base: Reg,
        argc: u16,
        dst: Reg,
    },
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
    TailCall {
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
    RetUnit,

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
    /// Repeat count check: panics if negative.
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
    /// Range value `src` as the registers of an inclusive range loop
    /// (`ForRangeIncl`): `base` = start, `base+1` = last, `base+2` = done.
    UnpackRange {
        base: Reg,
        src: Reg,
    },
    /// Snapshot of map entries (as tuples) or set elements as a list.
    IterSnapshot {
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

    // Unfused forms used in very large functions (offsets that do not fit i16).
    IsVariant {
        dst: Reg,
        src: Reg,
        variant: u16,
    },
    ListLenEq {
        dst: Reg,
        src: Reg,
        n: u16,
    },
    ListLenGe {
        dst: Reg,
        src: Reg,
        n: u16,
    },
    /// Like `ForList` but writes whether an element was produced to `base+3`.
    ForListNext {
        elem: Reg,
        base: Reg,
    },
    ForRangeNext {
        var: Reg,
        base: Reg,
    },
    ForRangeInclNext {
        var: Reg,
        base: Reg,
    },
    Len {
        dst: Reg,
        src: Reg,
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
    /// `spec` indexes `Program::specs`.
    Format {
        dst: Reg,
        src: Reg,
        spec: u16,
    },
    Concat {
        dst: Reg,
        start: Reg,
        count: u16,
    },

    // Misc.
    Spawn {
        dst: Reg,
        closure: Reg,
    },
    /// Top-level `?` on an `Err`: report and exit with code 1.
    TryTop {
        src: Reg,
    },
    Panic {
        msg: u32,
    },
}

const _: () = assert!(std::mem::size_of::<Op>() == 8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSrc {
    Reg(Reg),
    Capture(u16),
}

#[derive(Clone, Debug)]
pub struct Proto {
    pub name: String,
    pub file: u32,
    pub nparams: u16,
    pub nregs: u16,
    pub code: Vec<Op>,
    pub lines: Vec<u32>,
    pub captures: Vec<CaptureSrc>,
    /// For each capture: whether it is a cell.
    pub capture_cells: Vec<bool>,
    pub doc: Option<String>,
    pub signature: String,
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
    Spec(FormatSpec),
    TypeDesc(Arc<TypeDesc>),
    /// An immutable tuple of earlier constants (constant trait dictionaries).
    Tuple(Vec<u32>),
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

#[derive(Clone, Debug)]
pub struct HelpEntry {
    pub doc: Option<String>,
    pub signature: String,
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
    /// Test setup (module initialization) entry.
    pub files: Vec<String>,
    /// Help text for natives by id.
    pub native_help: Vec<HelpEntry>,
    pub module_help: Vec<(String, String)>,
    pub native_argc: Vec<u16>,
    pub specs: Vec<FormatSpec>,
    /// Names of all standard and host modules available to `import`.
    pub available_modules: Vec<String>,
    /// Summaries of modules not compiled into this program (for `help.module`).
    pub help_index: Option<Arc<crate::engine::ModuleHelp>>,
    /// Native id of `task.spawn`-style helpers used by the compiler.
    pub recv_native: Option<u32>,
}
