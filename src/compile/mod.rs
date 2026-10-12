//! Compiles checked HIR into register bytecode.

mod init_order;
mod inline;
mod reflect;

use crate::check::budget::CompileBudget;
use crate::check::persist::{PMap, PVec};
use crate::check::{CheckedProgram, FnKind, NativeIds};
use crate::diag::{Diag, SourceMap, Span};
use crate::hir::*;
use crate::types::{FnId, Ty, TypeArena, TypeDefId, TypeId};
use crate::vm::bytecode::*;
use crate::vm::value::FxHashMap;
use init_order::{InitOrder, Time};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What a trait method call with a known impl runs.
#[derive(Clone, Copy)]
enum Target {
    Proto(u32),
    /// A native implementing the method (it receives the impl's dictionary
    /// first, like the methods written in Sand).
    Native(u32),
}

pub struct CompileInput<'a> {
    pub checked: &'a CheckedProgram,
    pub sm: &'a SourceMap,
    pub natives: &'a NativeIds,
    pub native_names: Vec<String>,
    pub native_argc: Vec<u16>,
    /// Charged one unit per expression compiled, instruction emitted and
    /// node visited by an analysis. Once it is exhausted the compiler skips
    /// the rest of the code (callers check `budget.stopped()` and discard
    /// the result).
    pub budget: &'a CompileBudget,
    /// For a REPL session's input: which of the program's globals are
    /// initialized (see `init_order`).
    pub initialized: Option<Vec<bool>>,
}

struct Compiler<'a> {
    cp: &'a CheckedProgram,
    /// The program's types, which `reflect` extends with the instances it
    /// describes (a private copy: they are not needed after compiling).
    arena: TypeArena,
    budget: &'a CompileBudget,
    sm: &'a SourceMap,
    prog: &'a mut Program,
    fn_protos: PVec<Option<u32>>,
    /// The constants made so far, to share them.
    consts: ConstTable,
    variant_wrappers: PMap<u32, u32>,
    /// Constant trait dictionaries by their elements.
    dict_consts: PMap<Vec<u32>, u32>,
    /// Shared type descriptions of `reflect` type arguments.
    descs: reflect::DescCache,
    errors: Vec<Diag>,
    recv_native: Option<u32>,
    /// Natives compiled to dedicated instructions.
    inline: HashMap<u32, Inline>,
    /// The bodies of the functions to compile, with calls inlined.
    bodies: HashMap<FnId, &'a HFn>,
    order: InitOrder,
}

#[derive(Clone, Copy)]
enum Inline {
    Len,
    Push,
    ListGet,
    ListPop,
    MapGet,
    MapGetOr,
    MapHas,
    SetHas,
    SetAdd,
    ToFloat,
    /// `Op::StrClass` with this class.
    StrClass(u8),
}

#[derive(Default)]
struct LoopCx {
    breaks: Vec<usize>,
    /// `Jmp`s to the loop's next iteration (its test, for a rotated loop).
    continues: Vec<usize>,
    result: Option<Reg>,
    /// The number of defer scopes outside the loop: `break` and `continue`
    /// run the deferred expressions of the scopes above.
    defer_depth: usize,
}

struct FnState<'h> {
    code: Vec<Op>,
    lines: Vec<u32>,
    /// Branches whose targets are too far for their offset field, as
    /// (instruction, target): `assemble` routes them through branch islands.
    far: Vec<(usize, usize)>,
    locals: &'h [LocalInfo],
    /// The register of each local (`NONE`: not declared yet, or captured).
    local_reg: Vec<Reg>,
    /// The previous registers of the locals declared, to undo declarations
    /// (`reset_regs`).
    reg_log: Vec<(LocalId, Reg)>,
    /// The capture index of each local captured by the function (a lambda).
    next_reg: u32,
    max_reg: u32,
    loops: Vec<LoopCx>,
    line: u32,
    file: u32,
    /// Set once an error about the function's size has been reported.
    too_big: bool,
    /// Loop-invariant values loaded into registers before the enclosing
    /// outermost loop (see `hoisted_loop`).
    hoisted: FxHashMap<HoistKey<'h>, Reg>,
    /// Inside an outermost loop whose invariants were already hoisted.
    hoisting: bool,
    /// When the code being compiled can first run (see `init_order`).
    time: Time,
    /// Deferred expressions of the enclosing blocks (innermost last), compiled
    /// again at every exit edge that leaves their block, with their `pads`.
    defers: Vec<Vec<(&'h HExpr, usize)>>,
    /// The cleanup pad of each `defer`: a copy of its code run when the task
    /// is cancelled, and the pad of the defer pending before it.
    pads: Vec<(usize, Option<usize>)>,
    /// The innermost pending defer (in `pads`) and where code began to have
    /// it pending.
    pending: Option<usize>,
    pending_from: usize,
    /// Code ranges with a pending defer: (start, end, pad).
    cleanups: Vec<(usize, usize, usize)>,
    /// Instructions emitted for copies of deferred code (see `MAX_DEFER_OPS`).
    defer_ops: usize,
    /// The register `discard` gives to an expression whose value is unused:
    /// unit results are not materialized in it (see `unit_to`).
    void: Option<Reg>,
    /// What the compiler knows about the function's locals.
    facts: Rc<LocalFacts>,
    /// Failure paths of `?`, emitted after the function's code (so the
    /// success path falls through), with the branches to them and a line.
    stubs: Vec<(Stub, Vec<usize>, u32)>,
    /// The jump tables of the function's switches: their index in
    /// `Program::switches` and targets (instructions; the default last).
    switches: Vec<(u32, Vec<usize>)>,
    /// For a local function: the local naming it, whose calls in its own
    /// body are `CallSelf`.
    self_local: Option<LocalId>,
    /// The inlined call being compiled (see `Proto::sites`), 0 for none.
    site: u16,
    sites: Vec<InlineSite>,
}

const MAX_REGS: u32 = 60000;
/// No register (or capture index).
const NONE: u16 = u16::MAX;

impl FnState<'_> {
    fn reg(&self, l: LocalId) -> Option<Reg> {
        Some(self.local_reg[l as usize]).filter(|&r| r != NONE)
    }

    fn set_reg(&mut self, l: LocalId, r: Reg) {
        self.reg_log.push((l, self.local_reg[l as usize]));
        self.local_reg[l as usize] = r;
    }

    /// Undoes the declarations since `reg_log` had length `mark`.
    fn reset_regs(&mut self, mark: usize) {
        for (l, r) in self.reg_log.drain(mark..).rev() {
            self.local_reg[l as usize] = r;
        }
    }
}
/// The most instructions a function may contain for copies of deferred code.
const MAX_DEFER_OPS: usize = 1 << 20;

/// A loop-invariant value that can be loaded once before a loop.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HoistKey<'h> {
    Int(i64),
    Float(u64),
    Str(&'h str),
    Bytes(&'h [u8]),
    Dur(i64),
    Tag(u32),
    /// An immutable global, initialized wherever the loop runs.
    Global(u32),
    /// An immutable field of a struct or tuple in a local that is never
    /// assigned (`it.next`, the function of an `Iter`).
    Field(LocalId, u32),
    /// A method of a trait dictionary in a local that is never assigned
    /// (a call through a bound).
    Method(LocalId, u32),
}

/// The hoisting key of the method a dynamic trait call loads.
fn method_key<'h>(f: &FnState, args: &[HArg], slot: u32) -> Option<HoistKey<'h>> {
    let [HArg::Expr(HExpr { kind: HKind::Local(l), .. }), ..] = args else { return None };
    (!f.facts.assigned[*l as usize] && !f.locals[*l as usize].cell).then_some(HoistKey::Method(*l, slot))
}

/// The constants of a program by value (see `Compiler::const_idx`).
#[derive(Clone, Default)]
pub(crate) struct ConstTable {
    strs: PMap<String, u32>,
    bytes: PMap<Vec<u8>, u32>,
    scalars: PMap<ScalarConst, u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ScalarConst {
    Int(i64),
    /// By its bits (so `-0.0` and `0.0`, and NaNs, stay apart).
    Float(u64),
    Dur(i64),
    Fn(u32),
    Native(u32),
}

/// At most this many values are hoisted per loop nest.
const MAX_HOISTED: usize = 24;
/// While a function is compiled, each line entry also holds the inlined
/// call of the instruction above `LINE_BITS` (see `FnState::site`).
const LINE_BITS: u32 = 20;
const LINE_MASK: u32 = (1 << LINE_BITS) - 1;
const MAX_SITES: usize = (1 << (32 - LINE_BITS)) - 1;

/// A function's code as `FnState::finish` assembles it.
struct Finished {
    code: Vec<Op>,
    lines: Vec<u32>,
    cleanups: Vec<Cleanup>,
    sites: Vec<u16>,
    inlined: Vec<InlineSite>,
}

/// Hoisting is skipped when the function already uses this many registers.
const HOIST_REG_BUDGET: u32 = 1000;

/// What the compiler remembers between the compilations of a REPL session,
/// each of which appends to the same [`Program`] (and between the standard
/// library's snapshot and each program compiled on it). Its tables are
/// persistent, so copying it is cheap.
#[derive(Clone, Default)]
pub(crate) struct Incremental {
    fn_protos: PVec<Option<u32>>,
    /// The constants made so far, to share them.
    consts: ConstTable,
    variant_wrappers: PMap<u32, u32>,
    dict_consts: PMap<Vec<u32>, u32>,
    descs: reflect::DescCache,
    /// Number of tests compiled.
    tests: usize,
    /// The bodies of the functions whose calls are inlined.
    inline: PMap<FnId, std::sync::Arc<HFn>>,
}

/// The sizes of a program's tables, to undo a failed [`compile_more`].
pub(crate) struct ProgramMark {
    protos: usize,
    consts: usize,
    types: usize,
    variants: usize,
    templates: usize,
    switches: usize,
    tests: usize,
    nglobals: usize,
    files: usize,
    ids: RtIds,
}

impl ProgramMark {
    pub(crate) fn of(p: &Program) -> ProgramMark {
        ProgramMark {
            protos: p.protos.len(),
            consts: p.consts.len(),
            types: p.types.len(),
            variants: p.variants.len(),
            templates: p.templates.len(),
            switches: p.switches.len(),
            tests: p.tests.len(),
            nglobals: p.nglobals,
            files: p.files.len(),
            ids: p.ids,
        }
    }

    /// Truncates `p` to the marked sizes.
    pub(crate) fn rollback(&self, p: &mut Program) {
        p.protos.truncate(self.protos);
        p.consts.truncate(self.consts);
        p.types.truncate(self.types);
        p.variants.truncate(self.variants);
        p.templates.truncate(self.templates);
        p.switches.truncate(self.switches);
        p.tests.truncate(self.tests);
        p.nglobals = self.nglobals;
        p.global_names.truncate(self.nglobals);
        p.files.truncate(self.files);
        p.ids = self.ids;
    }
}

/// Compiles the parts of `input.checked` that are not yet in `prog` (all of
/// it, for a new program): the types, variants and globals added since, and
/// the functions with bodies numbered from `inc`'s count. Appends them to
/// `prog` without renumbering anything already there, so values of a running
/// VM stay valid. Returns a new entry proto that calls `inits` in order and
/// returns the result of the last. On error, `prog` may be partially
/// extended (see [`ProgramMark`]).
pub(crate) fn compile_more(mut input: CompileInput, prog: &mut Program, inc: &mut Incremental, inits: &[FnId]) -> Result<u32, Vec<Diag>> {
    let cp = input.checked;
    let first = inc.fn_protos.len();
    for (f, body) in cp.bodies.from(first as FnId) {
        if let Some(b) = inline::inlinable(cp, f, body) {
            inc.inline.insert(f, b);
        }
    }
    let inlined: Vec<Option<HFn>> =
        cp.bodies.from(first as FnId).map(|(_, b)| inline::inline_calls(b, &inc.inline, input.budget)).collect();
    let bodies: HashMap<FnId, &HFn> = cp.bodies.from(first as FnId).zip(&inlined).map(|((f, b), i)| (f, i.as_ref().unwrap_or(b))).collect();
    let order = InitOrder::new(cp, &bodies, inits, prog.global_names.len(), input.initialized.take(), input.budget);
    let mut c = Compiler {
        cp,
        arena: cp.arena.clone(),
        budget: input.budget,
        sm: input.sm,
        prog,
        fn_protos: std::mem::take(&mut inc.fn_protos),
        consts: std::mem::take(&mut inc.consts),
        variant_wrappers: std::mem::take(&mut inc.variant_wrappers),
        dict_consts: std::mem::take(&mut inc.dict_consts),
        descs: std::mem::take(&mut inc.descs),
        errors: Vec::new(),
        recv_native: input.natives.get("Channel.recv").copied(),
        inline: HashMap::new(),
        bodies,
        order,
    };
    c.descs.start();
    let entry = c.compile_new(input, inits, &mut inc.tests);
    inc.fn_protos = c.fn_protos;
    inc.consts = c.consts;
    inc.variant_wrappers = c.variant_wrappers;
    inc.dict_consts = c.dict_consts;
    inc.descs = c.descs;
    if c.errors.is_empty() { Ok(entry) } else { Err(c.errors) }
}

impl<'a> Compiler<'a> {
    fn compile_new(&mut self, input: CompileInput, inits: &[FnId], ntests: &mut usize) -> u32 {
        let cp = self.cp;
        for (name, op) in [
            ("List.len", Inline::Len),
            ("str.len", Inline::Len),
            ("bytes.len", Inline::Len),
            ("Map.len", Inline::Len),
            ("Set.len", Inline::Len),
            ("List.push", Inline::Push),
            ("List.get", Inline::ListGet),
            ("List.pop", Inline::ListPop),
            ("Map.get", Inline::MapGet),
            ("Map.get_or", Inline::MapGetOr),
            ("Map.contains", Inline::MapHas),
            ("Set.contains", Inline::SetHas),
            ("Set.add", Inline::SetAdd),
            ("int.to_float", Inline::ToFloat),
            ("str.is_digit", Inline::StrClass(0)),
            ("str.is_alpha", Inline::StrClass(1)),
            ("str.is_alnum", Inline::StrClass(2)),
            ("str.is_space", Inline::StrClass(3)),
            ("str.is_upper", Inline::StrClass(4)),
            ("str.is_lower", Inline::StrClass(5)),
        ] {
            if let Some(&id) = input.natives.get(name) {
                self.inline.insert(id, op);
            }
        }
        // Native names are only given when (new) natives were checked.
        if !input.native_names.is_empty() {
            self.prog.native_names = input.native_names.into();
            self.prog.native_argc = input.native_argc.into();
        }
        // The program's files are a prefix of the source map's (those of a
        // failed compilation are removed by `ProgramMark`): only add the new.
        let known = self.prog.files.len().min(input.sm.files.len());
        debug_assert!(known == 0 || self.prog.files[known - 1] == input.sm.files[known - 1].name);
        self.prog.files.truncate(known);
        self.prog.files.extend(input.sm.files[known..].iter().map(|f| f.name.clone()));
        self.prog.nglobals = cp.globals.len();
        let known = self.prog.global_names.len();
        let name = |s| cp.name(s).to_string();
        self.prog.global_names.extend(cp.globals.iter_from(known).map(|g| name(g.name)));
        for t in cp.types.iter_from(self.prog.types.len()) {
            self.prog.types.push(StructRt {
                name: name(t.name),
                module: name(t.module_name),
                fields: t.fields().iter().map(|f| name(f.name)).collect(),
                is_enum: t.is_enum(),
            });
        }
        for &(d, i) in cp.variants.iter_from(self.prog.variants.len()) {
            let v = &cp.types[d as usize].variants()[i as usize];
            self.prog.variants.push(VariantRt {
                def: d,
                index: i,
                name: name(v.name),
                field_names: v.field_names.as_ref().map(|ns| ns.iter().map(|&n| name(n)).collect()),
                arity: v.fields.len(),
            });
        }
        let wk = cp.wk;
        let vid = |d: u32, n: &str| {
            let s = cp.syms.get(n);
            cp.types[d as usize].variants().iter().find(|v| Some(v.name) == s).map(|v| v.rt_id).unwrap_or(0)
        };
        self.prog.ids = RtIds {
            some: vid(wk.option, "Some"),
            none: vid(wk.option, "None"),
            ok: vid(wk.result, "Ok"),
            err: vid(wk.result, "Err"),
            error_def: wk.error,
            json: None,
        };
        let (json, json_mod) = (cp.syms.get("Json"), cp.syms.get("json"));
        if let Some(jd) = cp.types.iter().position(|t| Some(t.name) == json && Some(t.module_name) == json_mod) {
            let jd = jd as u32;
            self.prog.ids.json = Some(JsonIds {
                def: jd,
                null: vid(jd, "Null"),
                bool_: vid(jd, "Bool"),
                num: vid(jd, "Num"),
                str_: vid(jd, "Str"),
                arr: vid(jd, "Arr"),
                obj: vid(jd, "Obj"),
            });
        }
        // Reserve protos for every new script function so calls can refer to them.
        let first = self.fn_protos.len();
        while self.fn_protos.len() < cp.fns.len() {
            self.fn_protos.push(None);
        }
        for (i, f) in cp.fns.iter_from(first).enumerate().map(|(k, f)| (first + k, f)) {
            if !matches!(f.kind, FnKind::Native(_)) && self.bodies.contains_key(&(i as FnId)) {
                let idx = self.prog.protos.len() as u32;
                self.prog.protos.push(Proto {
                    name: name(f.name),
                    file: 0,
                    nparams: 0,
                    nregs: 0,
                    code: vec![],
                    lines: vec![],
                    captures: vec![],
                    cleanups: Vec::new(),
                    sites: Vec::new(),
                    inlined: Vec::new(),
                });
                self.fn_protos[i] = Some(idx);
            }
        }
        for i in first..cp.fns.len() {
            if let (Some(p), Some(&body)) = (self.fn_protos[i], self.bodies.get(&(i as FnId))) {
                self.compile_fn(i as FnId, p, body);
            }
        }
        // Entry: run the module initializers in order.
        let mut code = Vec::new();
        for &init in inits {
            if let Some(p) = self.fn_protos[init as usize] {
                code.push(Op::CallDirect { base: 0, proto: p });
            }
        }
        code.push(Op::Ret { src: 0 });
        let n = code.len();
        let entry = self.prog.protos.len() as u32;
        self.prog.protos.push(Proto {
            name: "<entry>".into(),
            file: 0,
            nparams: 0,
            nregs: 1,
            code,
            lines: vec![0; n],
            captures: vec![],
            cleanups: Vec::new(),
            sites: Vec::new(),
            inlined: Vec::new(),
        });
        for (name, f) in &cp.tests[*ntests..] {
            if let Some(p) = self.fn_protos[*f as usize] {
                self.prog.tests.push((name.clone(), p));
            }
        }
        *ntests = cp.tests.len();
        entry
    }
}

fn int_fits_i32(v: i64) -> bool {
    v >= i32::MIN as i64 && v <= i32::MAX as i64
}

/// The value of an integer literal, also negated (`-1`, which the checker
/// leaves as a negation).
fn int_lit(e: &HExpr) -> Option<i64> {
    match &e.kind {
        HKind::Int(k) => Some(*k),
        HKind::Unary { op: HUnOp::Neg, e } => match e.kind {
            HKind::Int(k) => k.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

/// An integer literal that fits an instruction's immediate operand.
fn small_lit(e: &HExpr) -> Option<i16> {
    int_lit(e).and_then(|k| i16::try_from(k).ok())
}

/// The value of a float literal, also negated.
fn float_lit(e: &HExpr) -> Option<f64> {
    match &e.kind {
        HKind::Float(x) => Some(*x),
        HKind::Unary { op: HUnOp::Neg, e } => match e.kind {
            HKind::Float(x) => Some(-x),
            _ => None,
        },
        _ => None,
    }
}

/// The comparison that holds exactly when `op` does not (for integers).
fn negate_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Ge,
        CmpOp::Le => CmpOp::Gt,
        CmpOp::Gt => CmpOp::Le,
        CmpOp::Ge => CmpOp::Lt,
        CmpOp::Eq => CmpOp::Ne,
        CmpOp::Ne => CmpOp::Eq,
    }
}

/// `k op a` as `a op' k`.
fn mirror_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq | CmpOp::Ne => op,
    }
}

/// The branch taken when `a op k` is false.
fn jump_unless_k(op: CmpOp, a: Reg, k: i16) -> Op {
    match op {
        CmpOp::Lt => Op::JNltIK { a, k, off: 0 },
        CmpOp::Le => Op::JNleIK { a, k, off: 0 },
        CmpOp::Gt => Op::JNgtIK { a, k, off: 0 },
        CmpOp::Ge => Op::JNgeIK { a, k, off: 0 },
        CmpOp::Eq => Op::JNeqIK { a, k, off: 0 },
        CmpOp::Ne => Op::JNneIK { a, k, off: 0 },
    }
}

/// The step of a native loop over the registers at `base`.
#[derive(Clone, Copy)]
enum Step {
    /// `base` = list, `base+1` = index, `base+2` = length snapshot.
    List,
    /// `base` = counter, `base+1` = end (exclusive).
    Range,
    /// `base` = counter, `base+1` = end (inclusive), `base+2` = done flag.
    RangeIncl,
    /// `base` = the iterator of a map; the key goes to `var`, the value to
    /// `var+1`.
    Map,
    /// `base` = the iterator of a set.
    Set,
    /// `base` = string, `base+1` = byte offset.
    Chars,
}

impl Step {
    /// The step writing the next element to `var`: forward, jumping when
    /// there is none, or backward, jumping when there is one.
    fn op(self, var: Reg, base: Reg, back: bool) -> Op {
        match (self, back) {
            (Step::List, false) => Op::ForList { elem: var, base, off: 0 },
            (Step::List, true) => Op::ForListBack { elem: var, base, off: 0 },
            (Step::Range, false) => Op::ForRange { var, base, off: 0 },
            (Step::Range, true) => Op::ForRangeBack { var, base, off: 0 },
            (Step::RangeIncl, false) => Op::ForRangeIncl { var, base, off: 0 },
            (Step::RangeIncl, true) => Op::ForRangeInclBack { var, base, off: 0 },
            (Step::Map, false) => Op::ForMap { key: var, base, off: 0 },
            (Step::Map, true) => Op::ForMapBack { key: var, base, off: 0 },
            (Step::Set, false) => Op::ForSet { elem: var, base, off: 0 },
            (Step::Set, true) => Op::ForSetBack { elem: var, base, off: 0 },
            (Step::Chars, false) => Op::ForChars { elem: var, base, off: 0 },
            (Step::Chars, true) => Op::ForCharsBack { elem: var, base, off: 0 },
        }
    }
}

/// Whether a loop condition is small and simple enough to be compiled
/// twice (tested on entry and at the end of a rotated loop).
fn cheap_cond(cond: &HExpr) -> bool {
    let mut nodes = 0;
    let mut simple = true;
    walk(cond, &mut |e| {
        nodes += 1;
        simple &= matches!(
            e.kind,
            HKind::Local(_)
                | HKind::Global(_)
                | HKind::Int(_)
                | HKind::Float(_)
                | HKind::Bool(_)
                | HKind::Str(_)
                | HKind::Tag(_)
                | HKind::Cmp { .. }
                | HKind::And(..)
                | HKind::Or(..)
                | HKind::Unary { .. }
                | HKind::Arith { .. }
                | HKind::Field { .. }
                | HKind::Index { .. }
                | HKind::CallNative { .. }
        );
    });
    simple && nodes <= 16
}

/// A pattern-match scrutinee: a value in a register, or the elements of a
/// tuple literal, never built (see `Compiler::scrutinee`).
enum Scrut {
    Reg(Reg),
    Tuple(Vec<Reg>),
}

/// What the start of a pattern tests.
enum Head {
    /// Nothing: it matches any value.
    Any,
    /// The runtime variant id or integer is one of these.
    Values(Vec<i64>),
    /// Something else.
    Other,
}

fn head(p: &HPat) -> Head {
    match &p.kind {
        HPatKind::Wildcard | HPatKind::Bind(_) => Head::Any,
        HPatKind::Variant { rt_id, .. } => Head::Values(vec![*rt_id as i64]),
        HPatKind::Int(k) => Head::Values(vec![*k]),
        HPatKind::Str(t) if t.len() == 1 => Head::Values(vec![t.as_bytes()[0] as i64]),
        HPatKind::Or(alts) => {
            let mut all = Vec::new();
            for a in alts {
                match head(a) {
                    Head::Values(vs) => all.extend(vs),
                    h => return h,
                }
            }
            Head::Values(all)
        }
        _ => Head::Other,
    }
}

/// The `head` of the first element of a tuple pattern.
fn first_head(p: &HPat) -> Head {
    match &p.kind {
        HPatKind::Tuple(ps) => ps.first().map_or(Head::Any, head),
        HPatKind::Wildcard | HPatKind::Bind(_) => Head::Any,
        HPatKind::Or(alts) => {
            let mut all = Vec::new();
            for a in alts {
                match first_head(a) {
                    Head::Values(vs) => all.extend(vs),
                    h => return h,
                }
            }
            Head::Values(all)
        }
        _ => Head::Other,
    }
}

/// Whether the head of the first element of tuple pattern `p` is of kind `kind`.
fn first_is(p: &HPat, kind: SwitchKind) -> bool {
    match &p.kind {
        HPatKind::Tuple(ps) => ps.first().is_some_and(|q| head_is(q, kind)),
        HPatKind::Or(alts) => alts.iter().all(|a| first_is(a, kind)),
        _ => false,
    }
}

/// Whether the head of `p` (see `head`) is of kind `kind`.
fn head_is(p: &HPat, kind: SwitchKind) -> bool {
    match (&p.kind, kind) {
        (HPatKind::Int(_), SwitchKind::Int) | (HPatKind::Variant { .. }, SwitchKind::Variant) => true,
        (HPatKind::Str(t), SwitchKind::Char) => t.len() == 1,
        (HPatKind::Or(alts), _) => alts.iter().all(|a| head_is(a, kind)),
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SwitchKind {
    Int,
    Variant,
    /// Strings of one byte, switched on the byte (`Op::SwitchChar`).
    Char,
}

/// A `match` starting with a jump table (`Op::SwitchVariant`, `SwitchInt`,
/// `SwitchChar`) over the values `first..first + len`.
struct SwitchPlan {
    kind: SwitchKind,
    first: i64,
    len: usize,
    heads: Vec<Head>,
}

/// The out-of-line code a failed `?` jumps to (see `FnState::stubs`).
#[derive(Clone, Copy, PartialEq)]
enum Stub {
    /// Returns `None`.
    RetNone,
    /// Returns the register (an `Err`).
    Ret(Reg),
    /// A top-level `?` on an `Err`.
    TryTop(Reg),
}

impl<'a> Compiler<'a> {
    /// The index of constant `k`, shared with equal constants made before
    /// (but trait dictionaries and type descriptions have their own caches).
    fn const_idx(&mut self, k: Const) -> u32 {
        let next = self.prog.consts.len() as u32;
        let t = &mut self.consts;
        let i = match &k {
            Const::Str(s) => return self.str_const(s),
            Const::Bytes(b) => t.bytes.get_or_insert(b.clone(), next),
            Const::Int(v) => t.scalars.get_or_insert(ScalarConst::Int(*v), next),
            Const::Float(x) => t.scalars.get_or_insert(ScalarConst::Float(x.to_bits()), next),
            Const::Dur(d) => t.scalars.get_or_insert(ScalarConst::Dur(*d), next),
            Const::Fn(p) => t.scalars.get_or_insert(ScalarConst::Fn(*p), next),
            Const::Native(n) => t.scalars.get_or_insert(ScalarConst::Native(*n), next),
            Const::TypeDesc(_) | Const::Tuple(_) => next,
        };
        if i == next {
            self.prog.consts.push(k);
        }
        i
    }

    fn str_const(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.consts.strs.get(s) {
            return i;
        }
        let i = self.prog.consts.len() as u32;
        self.consts.strs.insert(s.to_string(), i);
        self.prog.consts.push(Const::Str(s.to_string()));
        i
    }

    fn line_of(&self, span: Span) -> u32 {
        if (span.file as usize) < self.sm.files.len() { self.sm.file(span.file).line(span.lo) as u32 } else { 0 }
    }

    fn compile_fn(&mut self, fid: FnId, proto_idx: u32, body: &'a HFn) {
        let fi = &self.cp.fns[fid as usize];
        let mut f = FnState {
            code: Vec::new(),
            lines: Vec::new(),
            far: Vec::new(),
            locals: &body.locals,
            local_reg: vec![NONE; body.locals.len()],
            reg_log: Vec::new(),
            next_reg: 0,
            max_reg: 0,
            loops: vec![],
            line: self.line_of(body.span),
            file: body.span.file,
            too_big: false,
            hoisted: FxHashMap::default(),
            hoisting: false,
            time: self.order.first_run(fid),
            defers: Vec::new(),
            pads: Vec::new(),
            pending: None,
            pending_from: 0,
            cleanups: Vec::new(),
            defer_ops: 0,
            void: None,
            facts: Rc::new(local_facts(&body.body, &body.locals, self.budget)),
            stubs: Vec::new(),
            switches: Vec::new(),
            self_local: None,
            site: 0,
            sites: Vec::new(),
        };
        for &p in &body.params {
            let r = self.alloc(&mut f);
            f.set_reg(p, r);
        }
        match (self.order.init_pos(fid), &body.body.kind) {
            (Some(pos), HKind::Block(items)) if !items.is_empty() && self.struct_literal(&f, items).is_none() => {
                self.ret_block(&mut f, items, Some(pos))
            }
            _ => self.ret_expr(&mut f, &body.body),
        }
        let nregs = f.max_reg.max(1) as u16;
        let Finished { code, lines, cleanups, sites, inlined } = self.finish(f);
        self.prog.protos[proto_idx as usize] = Proto {
            name: self.cp.name(fi.name).to_string(),
            file: body.span.file,
            nparams: body.params.len() as u16,
            nregs,
            code,
            lines,
            captures: vec![],
            cleanups,
            sites,
            inlined,
        };
    }

    // ------------------------------------------------------------------
    // Emission helpers

    fn emit(&mut self, f: &mut FnState<'a>, op: Op) -> usize {
        self.budget.charge(1);
        f.code.push(op);
        f.lines.push(f.line.min(LINE_MASK) | (f.site as u32) << LINE_BITS);
        f.code.len() - 1
    }

    fn alloc(&mut self, f: &mut FnState<'a>) -> Reg {
        let r = f.next_reg;
        f.next_reg += 1;
        if f.next_reg > f.max_reg {
            f.max_reg = f.next_reg;
        }
        if r >= MAX_REGS {
            if !f.too_big {
                self.errors.push(Diag::error(Span::new(f.file, 0, 0), "function needs too many registers"));
            }
            f.too_big = true;
            return (MAX_REGS - 1) as Reg;
        }
        r as Reg
    }

    fn alloc_n(&mut self, f: &mut FnState<'a>, n: usize) -> Reg {
        let base = f.next_reg;
        for _ in 0..n {
            self.alloc(f);
        }
        if n == 0 {
            // Ensure the base register exists.
            if f.next_reg + 1 > f.max_reg {
                f.max_reg = f.next_reg + 1;
            }
        }
        base.min(MAX_REGS - 1) as Reg
    }

    fn here(&self, f: &FnState<'a>) -> usize {
        f.code.len()
    }

    fn emit_jump(&mut self, f: &mut FnState<'a>) -> usize {
        self.emit(f, Op::Jmp { off: 0 })
    }

    /// Patches the jump at `at` to land on `target`. A target out of the
    /// range of the instruction's offset field is resolved by `assemble`.
    fn patch(&mut self, f: &mut FnState<'a>, at: usize, target: usize) {
        // A conditional branch back (ending a rotated loop) is a safe point.
        if target <= at
            && let Some(back) = f.code[at].backward()
        {
            f.code[at] = back;
        }
        if !f.code[at].set_branch_off(target as i64 - (at as i64 + 1)) {
            f.far.push((at, target));
        }
    }

    /// The function's final code, lines and cleanup ranges: its stubs
    /// emitted, assembled (`assemble`), and its jump tables completed.
    fn finish(&mut self, mut f: FnState<'a>) -> Finished {
        self.set_pending(&mut f, None);
        self.emit_stubs(&mut f);
        let mut tables: Vec<Vec<usize>> = f.switches.iter().map(|s| s.1.clone()).collect();
        tables.push(f.cleanups.iter().flat_map(|&(start, end, pad)| [start, end, f.pads[pad].0]).collect());
        let (code, lines) = assemble(f.code, f.lines, &f.far, &mut tables);
        let cleanups =
            tables.pop().unwrap().chunks(3).map(|c| Cleanup { start: c[0] as u32, end: c[1] as u32, pad: c[2] as u32 }).collect();
        for ((idx, _), targets) in f.switches.iter().zip(tables) {
            let sw = &mut self.prog.switches[*idx as usize];
            let (&default, targets) = targets.split_last().expect("a default target");
            sw.targets = targets.iter().map(|&t| t as u32).collect();
            sw.default = default as u32;
        }
        let sites = if f.sites.is_empty() { Vec::new() } else { lines.iter().map(|&l| (l >> LINE_BITS) as u16).collect() };
        let lines = lines.into_iter().map(|l| l & LINE_MASK).collect();
        Finished { code, lines, cleanups, sites, inlined: f.sites }
    }

    /// Compiles `body`, the body of function `func` inlined at the call `e`,
    /// with `k`, recording the inlined call for stack traces.
    fn inlined<R>(
        &mut self,
        f: &mut FnState<'a>,
        e: &HExpr,
        func: FnId,
        body: &HExpr,
        k: impl FnOnce(&mut Self, &mut FnState<'a>) -> R,
    ) -> R {
        self.set_line(f, e.span);
        if f.sites.len() >= MAX_SITES {
            return k(self, f);
        }
        let name = self.cp.name(self.cp.fns[func as usize].name).to_string();
        f.sites.push(InlineSite { name, file: body.span.file, line: f.line, parent: f.site });
        let saved = (f.site, f.file, f.line);
        f.site = f.sites.len() as u16;
        f.file = body.span.file;
        let r = k(self, f);
        (f.site, f.file, f.line) = saved;
        r
    }

    /// Makes `pad` the innermost pending defer from here on.
    fn set_pending(&mut self, f: &mut FnState<'a>, pad: Option<usize>) {
        if pad == f.pending {
            return;
        }
        let here = self.here(f);
        if let Some(p) = f.pending
            && f.pending_from < here
        {
            match f.cleanups.last_mut() {
                Some(last) if last.1 == f.pending_from && last.2 == p => last.1 = here,
                _ => f.cleanups.push((f.pending_from, here, p)),
            }
        }
        f.pending = pad;
        f.pending_from = here;
    }

    /// Makes the branch at `at` go to `stub` (emitted by `emit_stubs`).
    fn jump_to_stub(&mut self, f: &mut FnState<'a>, at: usize, stub: Stub) {
        match f.stubs.iter_mut().find(|s| s.0 == stub) {
            Some(s) => s.1.push(at),
            None => f.stubs.push((stub, vec![at], f.line)),
        }
    }

    fn emit_stubs(&mut self, f: &mut FnState<'a>) {
        for (stub, jumps, line) in std::mem::take(&mut f.stubs) {
            f.line = line;
            self.patch_list(f, jumps);
            match stub {
                Stub::RetNone => {
                    // Any register will do: the function returns.
                    self.emit(f, Op::LoadTag { dst: 0, tag: self.prog.ids.none });
                    self.emit(f, Op::Ret { src: 0 });
                }
                Stub::Ret(r) => {
                    self.emit(f, Op::Ret { src: r });
                }
                Stub::TryTop(r) => {
                    self.emit(f, Op::TryTop { src: r });
                }
            }
        }
    }

    fn patch_all(&mut self, f: &mut FnState<'a>, jumps: Vec<usize>, target: usize) {
        for j in jumps {
            self.patch(f, j, target);
        }
    }

    fn patch_here(&mut self, f: &mut FnState<'a>, at: usize) {
        let t = self.here(f);
        self.patch(f, at, t);
    }

    fn emit_loop_back(&mut self, f: &mut FnState<'a>, target: usize) {
        let at = self.emit(f, Op::Loop { off: 0 });
        self.patch(f, at, target);
    }

    fn mv(&mut self, f: &mut FnState<'a>, dst: Reg, src: Reg) {
        if dst != src {
            self.emit(f, Op::Move { dst, src });
        }
    }

    /// Calls script function `proto` with the arguments at `base..`,
    /// writing the result to `dst`.
    fn call_direct(&mut self, f: &mut FnState<'a>, base: Reg, proto: u32, dst: Reg) {
        match u16::try_from(proto) {
            Ok(proto) => {
                self.emit(f, Op::CallDirectTo { base, dst, proto });
            }
            Err(_) => {
                self.emit(f, Op::CallDirect { base, proto });
                self.mv(f, dst, base);
            }
        }
    }

    /// Calls `native` with the arguments at `base..`, writing the result to
    /// `dst`.
    fn call_native(&mut self, f: &mut FnState<'a>, base: Reg, native: u32, dst: Reg) {
        match u16::try_from(native) {
            Ok(native) => {
                self.emit(f, Op::CallNativeTo { base, dst, native });
            }
            Err(_) => {
                // The arguments may be locals' registers: call on copies.
                let argc = self.prog.native_argc.get(native as usize).copied().unwrap_or(0) as usize;
                let mark = f.next_reg;
                let t = self.alloc_n(f, argc.max(1));
                for i in 0..argc as Reg {
                    self.mv(f, t + i, base + i);
                }
                self.emit(f, Op::CallNative { base: t, native });
                self.mv(f, dst, t);
                f.next_reg = mark;
            }
        }
    }

    fn load_int(&mut self, f: &mut FnState<'a>, dst: Reg, v: i64) {
        if int_fits_i32(v) {
            self.emit(f, Op::LoadInt { dst, v: v as i32 });
        } else {
            let k = self.const_idx(Const::Int(v));
            self.emit(f, Op::LoadConst { dst, k });
        }
    }

    fn load_str(&mut self, f: &mut FnState<'a>, dst: Reg, s: &str) {
        let k = self.str_const(s);
        self.emit(f, Op::LoadConst { dst, k });
    }

    fn variant_u16(&mut self, f: &FnState<'a>, rt: u32) -> u16 {
        if rt > u16::MAX as u32 {
            self.errors.push(Diag::error(Span::new(f.file, 0, 0), "too many enum variants in program"));
            return 0;
        }
        rt as u16
    }

    // ------------------------------------------------------------------
    // Locals

    fn is_cell(&self, f: &FnState<'a>, l: LocalId) -> bool {
        f.locals[l as usize].cell
    }

    fn read_local(&mut self, f: &mut FnState<'a>, l: LocalId, dst: Reg) {
        if let Some(r) = f.reg(l) {
            if f.facts.builder[l as usize] {
                self.emit(f, Op::FreezeStr { reg: r });
            }
            if self.is_cell(f, l) {
                self.emit(f, Op::LoadCell { dst, cell: r });
            } else {
                self.mv(f, dst, r);
            }
        } else {
            // A local declared in an unreachable or not-yet-compiled branch.
            self.unit_to(f, dst);
        }
    }

    /// A register holding local `l`: its own, unless it is a cell or captured.
    fn local_operand(&mut self, f: &mut FnState<'a>, l: LocalId) -> Reg {
        match f.reg(l) {
            Some(r) if !self.is_cell(f, l) && !f.facts.builder[l as usize] => r,
            _ => {
                let r = self.alloc(f);
                self.read_local(f, l, r);
                r
            }
        }
    }

    fn write_local(&mut self, f: &mut FnState<'a>, l: LocalId, src: Reg) {
        if let Some(r) = f.reg(l) {
            if self.is_cell(f, l) {
                self.emit(f, Op::StoreCell { cell: r, src });
            } else {
                self.mv(f, r, src);
            }
        }
    }

    /// Declares local `l`, allocating its register.
    fn declare_local(&mut self, f: &mut FnState<'a>, l: LocalId) -> Reg {
        let r = self.alloc(f);
        f.set_reg(l, r);
        r
    }

    /// Initializes a declared local from `src` (wrapping in a cell if needed).
    fn init_local(&mut self, f: &mut FnState<'a>, l: LocalId, src: Reg) {
        let r = f.reg(l).unwrap();
        if self.is_cell(f, l) {
            self.emit(f, Op::NewCell { dst: r, src });
        } else {
            self.mv(f, r, src);
        }
    }

    // ------------------------------------------------------------------
    // Expressions

    /// Returns a register holding the value of `e`, avoiding a copy for plain locals.
    fn expr_reg(&mut self, f: &mut FnState<'a>, e: &'a HExpr) -> Reg {
        if let HKind::Local(l) = e.kind
            && let Some(r) = f.reg(l)
            && !self.is_cell(f, l)
        {
            if f.facts.builder[l as usize] {
                self.emit(f, Op::FreezeStr { reg: r });
            }
            return r;
        }
        if !f.hoisted.is_empty()
            && let Some(k) = self.hoist_key(f, e)
            && let Some(&r) = f.hoisted.get(&k)
        {
            return r;
        }
        let r = self.alloc(f);
        self.expr_to(f, e, r);
        r
    }

    /// `expr_reg` for an operand evaluated before the operands `later`: a
    /// local is read in place only if they cannot assign it (`x + { x = 5; x }`).
    fn operand(&mut self, f: &mut FnState<'a>, e: &'a HExpr, later: &[&'a HExpr]) -> Reg {
        if let HKind::Local(l) = e.kind
            && f.facts.assigned[l as usize]
            && later.iter().any(|x| assigns(x, l, self.budget))
        {
            let r = self.alloc(f);
            self.read_local(f, l, r);
            return r;
        }
        self.expr_reg(f, e)
    }

    /// Compiles `e` for its side effects only.
    fn discard(&mut self, f: &mut FnState<'a>, e: &'a HExpr) {
        match &e.kind {
            HKind::Let { pat, init, else_ } => {
                self.set_line(f, e.span);
                self.compile_let(f, pat, init, else_.as_deref());
                return;
            }
            HKind::Assign { place, op, op_ty, value } => {
                self.set_line(f, e.span);
                self.compile_assign(f, place, *op, *op_ty, value);
                return;
            }
            HKind::LocalFns { .. } => {
                // Declarations keep their registers until the enclosing block ends.
                let r = self.alloc(f);
                self.expr_to(f, e, r);
                return;
            }
            HKind::Unit | HKind::Local(_) | HKind::Bool(_) | HKind::Int(_) => return,
            HKind::Block(items) => {
                let (mark, pending) = (f.next_reg, f.pending);
                f.defers.push(Vec::new());
                for it in items {
                    self.discard(f, it);
                }
                if !e.ty.is_never() {
                    self.emit_scope_defers(f);
                }
                f.defers.pop();
                self.set_pending(f, pending);
                f.next_reg = mark;
                return;
            }
            HKind::Defer(x) => {
                if f.defers.is_empty() {
                    f.defers.push(Vec::new());
                }
                let pad = self.emit_pad(f, x);
                f.defers.last_mut().unwrap().push((x, pad));
                return;
            }
            _ => {}
        }
        let mark = f.next_reg;
        let r = self.alloc(f);
        let outer = f.void.replace(r);
        self.expr_to(f, e, r);
        f.void = outer;
        f.next_reg = mark;
    }

    /// Loads unit into `dst` as the value of an expression, unless the
    /// value is unused (`dst` is the register of a discarded expression).
    fn unit_to(&mut self, f: &mut FnState<'a>, dst: Reg) {
        if f.void != Some(dst) {
            self.emit(f, Op::LoadUnit { dst });
        }
    }

    fn set_line(&mut self, f: &mut FnState<'a>, span: Span) {
        if span.file == f.file && span.hi > 0 {
            f.line = self.line_of(span);
        }
    }

    /// Compiles the block `items` as the return value of the current
    /// function. If it is the body of the initializer at position `init`,
    /// each statement is compiled at its own time.
    fn ret_block(&mut self, f: &mut FnState<'a>, items: &'a [HExpr], init: Option<usize>) {
        let (mark, pending) = (f.next_reg, f.pending);
        f.defers.push(Vec::new());
        for (j, it) in items.iter().enumerate() {
            if let Some(pos) = init {
                f.time = init_order::at(pos, j);
            }
            if j + 1 < items.len() {
                self.discard(f, it);
            } else {
                self.ret_expr(f, it);
            }
        }
        f.defers.pop();
        self.set_pending(f, pending);
        f.next_reg = mark;
    }

    /// Compiles `e` as the return value of the current function (tail position).
    fn ret_expr(&mut self, f: &mut FnState<'a>, e: &'a HExpr) {
        self.set_line(f, e.span);
        // Pending deferred expressions run after the value is computed, so
        // calls cannot be tail calls.
        let tail = !defers_pending(f);
        match &e.kind {
            HKind::Block(items) if !items.is_empty() && self.struct_literal(f, items).is_none() => self.ret_block(f, items, None),
            HKind::Call { func, args } if tail && self.fn_protos[*func as usize].is_some() => {
                let proto = self.fn_protos[*func as usize].unwrap();
                let base = self.call_args(f, args, 0);
                self.set_line(f, e.span);
                self.emit(f, Op::TailCallDirect { base, proto });
            }
            HKind::CallValue { callee, args } if tail => {
                let argc = self.argc(f, args.len(), e.span);
                let callee = self.operand(f, callee, &args.iter().collect::<Vec<_>>());
                let base = self.alloc_n(f, args.len().max(1));
                for (i, a) in args.iter().enumerate() {
                    self.expr_to(f, a, base + i as Reg);
                }
                self.set_line(f, e.span);
                // (A native callee returns here: its result is in `base`.)
                self.emit(f, Op::TailCall { callee, base, argc: argc as u16 });
                self.emit(f, Op::Ret { src: base });
            }
            HKind::CallTrait { slot, args, impl_id, .. } if tail => match self.static_target(*impl_id, *slot) {
                Some(Target::Proto(proto)) => {
                    let base = self.call_args(f, args, 0);
                    self.set_line(f, e.span);
                    self.emit(f, Op::TailCallDirect { base, proto });
                }
                Some(Target::Native(native)) => {
                    let base = self.call_args(f, args, 0);
                    self.set_line(f, e.span);
                    self.call_native(f, base, native, base);
                    self.emit(f, Op::Ret { src: base });
                }
                None => {
                    let argc = self.argc(f, args.len(), e.span);
                    let base = self.call_args(f, args, 0);
                    let callee = self.method(f, args, base, *slot);
                    self.set_line(f, e.span);
                    self.emit(f, Op::TailCall { callee, base, argc: argc as u16 });
                    self.emit(f, Op::Ret { src: base });
                }
            },
            HKind::If { cond, then, else_: Some(els) } => {
                let mark = f.next_reg;
                let jf = self.cond_jump(f, cond);
                f.next_reg = mark;
                self.ret_expr(f, then);
                self.patch_list(f, jf);
                self.ret_expr(f, els);
            }
            // Every arm returns: calls in arms are tail calls (SPEC §10.5).
            HKind::IfLet { pat, value, then, else_: Some(els) } => {
                let mark = f.next_reg;
                self.declare_pattern_locals(f, pat);
                let v = self.scrutinee(f, value);
                let mut fails = Vec::new();
                self.match_scrut(f, pat, &v, &mut fails);
                self.ret_expr(f, then);
                self.patch_list(f, fails);
                self.ret_expr(f, els);
                f.next_reg = mark;
            }
            HKind::Match { scrut, arms } => self.compile_match(f, scrut, arms, None),
            HKind::Return(Some(v)) => self.ret_expr(f, v),
            HKind::MakeVariant { rt_id, args } if tail && args.len() == 1 => {
                let mark = f.next_reg;
                let src = self.expr_reg(f, &args[0]);
                let variant = self.variant_u16(f, *rt_id);
                self.emit(f, Op::RetVariant { src, variant });
                f.next_reg = mark;
            }
            _ => {
                let mark = f.next_reg;
                if matches!(e.ty, TypeId::UNIT) && !matches!(e.kind, HKind::Local(_) | HKind::Global(_)) {
                    self.discard(f, e);
                    self.emit_defers(f, 0);
                    self.emit(f, Op::RetUnit);
                } else if !defers_pending(f) {
                    let r = self.expr_reg(f, e);
                    self.emit(f, Op::Ret { src: r });
                } else {
                    // A fresh register: deferred code may assign the local returned.
                    let r = self.alloc(f);
                    self.expr_to(f, e, r);
                    self.emit_defers(f, 0);
                    self.emit(f, Op::Ret { src: r });
                }
                f.next_reg = mark;
            }
        }
    }

    /// Compiles the deferred expressions of the defer scopes `from..`,
    /// innermost scope first and each scope in reverse order (an exit edge
    /// leaving those blocks).
    fn emit_defers(&mut self, f: &mut FnState<'a>, from: usize) {
        let (line, pending) = (f.line, f.pending);
        'scopes: for i in (from..f.defers.len()).rev() {
            for j in (0..f.defers[i].len()).rev() {
                let (x, pad) = f.defers[i][j];
                self.set_pending(f, f.pads[pad].1);
                if !self.emit_defer_copy(f, x) {
                    break 'scopes;
                }
            }
        }
        self.set_pending(f, pending);
        f.line = line;
    }

    /// Compiles a copy of deferred expression `x`; false if the function
    /// has too much deferred code.
    fn emit_defer_copy(&mut self, f: &mut FnState<'a>, x: &'a HExpr) -> bool {
        // Copies are bounded so that many defers times many exits cannot make
        // compilation itself unbounded.
        if f.defer_ops > MAX_DEFER_OPS {
            let msg = "too much deferred code: a `defer` is compiled into every exit of its block; move the cleanup into a function";
            if !self.errors.iter().any(|d| d.span == x.span && d.message == msg) {
                self.errors.push(Diag::error(x.span, msg));
            }
            f.too_big = true;
            return false;
        }
        let (ops, protos) = (f.code.len(), self.prog.protos.len());
        // Locals declared by the deferred code get fresh registers at each
        // copy (the registers of an earlier copy may be in use here).
        let saved = f.reg_log.len();
        self.discard(f, x);
        f.reset_regs(saved);
        let lambda_ops: usize = self.prog.protos[protos..].iter().map(|p| p.code.len()).sum();
        f.defer_ops += f.code.len() - ops + lambda_ops;
        true
    }

    /// Emits the cleanup pad of `defer x` (skipped by normal control flow)
    /// and makes it pending: the pad runs `x`, then the pads of the defers
    /// pending before it, then leaves the frame (`Op::Unwind`).
    fn emit_pad(&mut self, f: &mut FnState<'a>, x: &'a HExpr) -> usize {
        let (line, parent) = (f.line, f.pending);
        let over = self.emit_jump(f);
        let at = self.here(f);
        self.emit_defer_copy(f, x);
        match parent {
            Some(p) => self.emit_loop_back(f, f.pads[p].0),
            None => {
                self.emit(f, Op::Unwind);
            }
        }
        f.line = line;
        self.patch_here(f, over);
        f.pads.push((at, parent));
        let pad = f.pads.len() - 1;
        self.set_pending(f, Some(pad));
        pad
    }

    /// Runs the deferred expressions of the innermost scope (its block ends normally).
    fn emit_scope_defers(&mut self, f: &mut FnState<'a>) {
        if f.defers.last().is_some_and(|d| !d.is_empty()) {
            let n = f.defers.len() - 1;
            self.emit_defers(f, n);
        }
    }

    fn patch_list(&mut self, f: &mut FnState<'a>, jumps: Vec<usize>) {
        for j in jumps {
            self.patch_here(f, j);
        }
    }

    /// Emits a conditional jump taken when `cond` is false; returns patch sites.
    fn cond_jump(&mut self, f: &mut FnState<'a>, cond: &'a HExpr) -> Vec<usize> {
        self.set_line(f, cond.span);
        if let Some(j) = self.cmp_k_jump(f, cond, false) {
            return j;
        }
        if let Some(j) = self.eq_jump(f, cond, false) {
            return j;
        }
        if let HKind::Cmp { op, l, r } = &cond.kind {
            let ty = l.ty;
            if matches!(ty, TypeId::INT) || matches!(ty, TypeId::FLOAT) {
                let a = self.operand(f, l, &[r]);
                let b = self.expr_reg(f, r);
                let int = matches!(ty, TypeId::INT);
                let op = match (op, int) {
                    (CmpOp::Lt, true) => Op::JNltI { a, b, off: 0 },
                    (CmpOp::Le, true) => Op::JNleI { a, b, off: 0 },
                    (CmpOp::Gt, true) => Op::JNltI { a: b, b: a, off: 0 },
                    (CmpOp::Ge, true) => Op::JNleI { a: b, b: a, off: 0 },
                    (CmpOp::Eq, true) => Op::JNeqI { a, b, off: 0 },
                    (CmpOp::Ne, true) => Op::JNneI { a, b, off: 0 },
                    (CmpOp::Lt, false) => Op::JNltF { a, b, off: 0 },
                    (CmpOp::Le, false) => Op::JNleF { a, b, off: 0 },
                    (CmpOp::Gt, false) => Op::JNltF { a: b, b: a, off: 0 },
                    (CmpOp::Ge, false) => Op::JNleF { a: b, b: a, off: 0 },
                    _ => {
                        let c = self.expr_reg(f, cond);
                        return vec![self.emit(f, Op::JmpIfNot { cond: c, off: 0 })];
                    }
                };
                return vec![self.emit(f, op)];
            }
        }
        if let Some(j) = self.str_class_jump(f, cond, false) {
            return j;
        }
        match &cond.kind {
            HKind::And(a, b) => {
                let mut j = self.cond_jump(f, a);
                j.extend(self.cond_jump(f, b));
                j
            }
            HKind::Or(a, b) => {
                // `a` true: go on (past the test of `b`).
                let ok = self.cond_jump_true(f, a);
                let fails = self.cond_jump(f, b);
                self.patch_list(f, ok);
                fails
            }
            HKind::Unary { op: HUnOp::Not, e } => self.cond_jump_true(f, e),
            HKind::Bool(true) => vec![],
            HKind::Bool(false) => vec![self.emit_jump(f)],
            HKind::Block(items) if cond_block(items) => self.block_jump(f, items, false),
            _ => {
                let c = self.expr_reg(f, cond);
                vec![self.emit(f, Op::JmpIfNot { cond: c, off: 0 })]
            }
        }
    }

    /// An integer comparison with a small constant as one compare-and-branch
    /// with an immediate: jumps when the comparison is `when`.
    fn cmp_k_jump(&mut self, f: &mut FnState<'a>, cond: &'a HExpr, when: bool) -> Option<Vec<usize>> {
        let HKind::Cmp { op, l, r } = &cond.kind else { return None };
        if !matches!(l.ty, TypeId::INT) {
            return None;
        }
        let (op, x, k) = match (small_lit(l), small_lit(r)) {
            (_, Some(k)) => (*op, l, k),
            (Some(k), None) => (mirror_cmp(*op), r, k),
            (None, None) => return None,
        };
        let a = self.expr_reg(f, x);
        let op = if when { negate_cmp(op) } else { op };
        Some(vec![self.emit(f, jump_unless_k(op, a, k))])
    }

    /// `==` or `!=` of values other than numbers (strings, ...) as one
    /// compare-and-branch: jumps when the comparison is `when`.
    fn eq_jump(&mut self, f: &mut FnState<'a>, cond: &'a HExpr, when: bool) -> Option<Vec<usize>> {
        let HKind::Cmp { op: op @ (CmpOp::Eq | CmpOp::Ne), l, r } = &cond.kind else { return None };
        if matches!(l.ty, TypeId::INT | TypeId::FLOAT) {
            return None;
        }
        let eq = (*op == CmpOp::Eq) != when;
        fn lit(x: &HExpr) -> Option<&str> {
            match &x.kind {
                HKind::Str(s) => Some(s),
                _ => None,
            }
        }
        if let Some((x, s)) = lit(r).map(|s| (l, s)).or_else(|| lit(l).map(|s| (r, s)))
            && let Ok(k) = u16::try_from(self.str_const(s))
        {
            let a = self.expr_reg(f, x);
            return Some(vec![self.emit(f, if eq { Op::JNeqK { a, k, off: 0 } } else { Op::JNneK { a, k, off: 0 } })]);
        }
        let a = self.operand(f, l, &[r]);
        let b = self.expr_reg(f, r);
        Some(vec![self.emit(f, if eq { Op::JNeq { a, b, off: 0 } } else { Op::JNne { a, b, off: 0 } })])
    }

    /// The condition `{ items }` (see `cond_block`): its statements, then a
    /// branch on its last expression taken when it is `when`.
    fn block_jump(&mut self, f: &mut FnState<'a>, items: &'a [HExpr], when: bool) -> Vec<usize> {
        f.defers.push(Vec::new());
        for it in &items[..items.len() - 1] {
            self.discard(f, it);
        }
        let last = items.last().unwrap();
        let j = if when { self.cond_jump_true(f, last) } else { self.cond_jump(f, last) };
        f.defers.pop();
        j
    }

    /// A character-class test (`c.is_alnum()`, an inline `StrClass`) as
    /// one compare-and-branch: jumps when the test is `when`.
    fn str_class_jump(&mut self, f: &mut FnState<'a>, cond: &'a HExpr, when: bool) -> Option<Vec<usize>> {
        let HKind::CallNative { native, args, .. } = &cond.kind else { return None };
        let (Some(&Inline::StrClass(class)), [HArg::Expr(x)]) = (self.inline.get(native), args.as_slice()) else {
            return None;
        };
        let mark = f.next_reg;
        let src = self.expr_reg(f, x);
        let j = self.emit(f, Op::JStrClass { src, class: if when { class | 0x80 } else { class }, off: 0 });
        f.next_reg = mark;
        Some(vec![j])
    }

    /// Like [`Self::cond_jump`], but the jumps are taken when `cond` is true.
    fn cond_jump_true(&mut self, f: &mut FnState<'a>, cond: &'a HExpr) -> Vec<usize> {
        self.set_line(f, cond.span);
        if let Some(j) = self.str_class_jump(f, cond, true) {
            return j;
        }
        if let Some(j) = self.cmp_k_jump(f, cond, true) {
            return j;
        }
        if let Some(j) = self.eq_jump(f, cond, true) {
            return j;
        }
        match &cond.kind {
            // Integer comparisons as the fused "jump if not" of the opposite
            // comparison (not floats: the opposite of `<` is not `>=` for NaN).
            HKind::Cmp { op, l, r } if matches!(l.ty, TypeId::INT) => {
                let a = self.operand(f, l, &[r]);
                let b = self.expr_reg(f, r);
                let op = match op {
                    CmpOp::Lt => Op::JNleI { a: b, b: a, off: 0 },
                    CmpOp::Le => Op::JNltI { a: b, b: a, off: 0 },
                    CmpOp::Gt => Op::JNleI { a, b, off: 0 },
                    CmpOp::Ge => Op::JNltI { a, b, off: 0 },
                    CmpOp::Eq => Op::JNneI { a, b, off: 0 },
                    CmpOp::Ne => Op::JNeqI { a, b, off: 0 },
                };
                vec![self.emit(f, op)]
            }
            HKind::Or(a, b) => {
                let mut j = self.cond_jump_true(f, a);
                j.extend(self.cond_jump_true(f, b));
                j
            }
            HKind::And(a, b) => {
                // `a` false: go on (past the test of `b`).
                let fails = self.cond_jump(f, a);
                let ok = self.cond_jump_true(f, b);
                self.patch_list(f, fails);
                ok
            }
            HKind::Unary { op: HUnOp::Not, e } => self.cond_jump(f, e),
            HKind::Bool(false) => vec![],
            HKind::Bool(true) => vec![self.emit_jump(f)],
            HKind::Block(items) if cond_block(items) => self.block_jump(f, items, true),
            _ => {
                let c = self.expr_reg(f, cond);
                vec![self.emit(f, Op::JmpIf { cond: c, off: 0 })]
            }
        }
    }

    /// Compiles arguments into consecutive registers starting at a fresh base
    /// (after `extra` leading slots); returns the base.
    fn call_args(&mut self, f: &mut FnState<'a>, args: &'a [HArg], extra: usize) -> Reg {
        let base = self.alloc_n(f, args.len() + extra);
        let base_args = base + extra as Reg;
        for (i, a) in args.iter().enumerate() {
            let dst = base_args + i as Reg;
            match a {
                HArg::Expr(e) => self.expr_to(f, e, dst),
                HArg::Default { helper, inputs, .. } => {
                    let mark = f.next_reg;
                    let t = self.alloc_n(f, inputs.len().max(1));
                    for (k, &inp) in inputs.iter().enumerate() {
                        self.mv(f, t + k as Reg, base_args + inp as Reg);
                    }
                    match self.fn_protos[*helper as usize] {
                        Some(p) => self.call_direct(f, t, p, dst),
                        None => {
                            self.emit(f, Op::LoadUnit { dst });
                        }
                    }
                    f.next_reg = mark;
                }
            }
        }
        base
    }

    fn expr_to(&mut self, f: &mut FnState<'a>, e: &'a HExpr, dst: Reg) {
        if !self.budget.charge(1) {
            return;
        }
        self.set_line(f, e.span);
        if !f.hoisting
            && matches!(e.kind, HKind::Loop(_) | HKind::While { .. } | HKind::WhileLet { .. } | HKind::ForRange { .. } | HKind::For { .. })
        {
            self.hoisted_loop(f, e, dst);
            return;
        }
        match &e.kind {
            HKind::Error | HKind::Evidence(_) => {
                let k = self.const_idx(Const::Str("internal error: unchecked expression".into()));
                self.emit(f, Op::Panic { msg: k });
            }
            HKind::Unit => {
                self.unit_to(f, dst);
            }
            HKind::Bool(b) => {
                self.emit(f, Op::LoadBool { dst, v: *b });
            }
            HKind::Int(i) => self.load_int(f, dst, *i),
            HKind::Float(x) => {
                let k = self.const_idx(Const::Float(*x));
                self.emit(f, Op::LoadConst { dst, k });
            }
            HKind::Dur(d) => {
                let k = self.const_idx(Const::Dur(*d));
                self.emit(f, Op::LoadConst { dst, k });
            }
            HKind::Str(s) => self.load_str(f, dst, s),
            HKind::Bytes(b) => {
                let k = self.const_idx(Const::Bytes(b.clone()));
                self.emit(f, Op::LoadConst { dst, k });
            }
            HKind::Local(l) => self.read_local(f, *l, dst),
            HKind::Global(g) => match f.hoisted.get(&HoistKey::Global(*g)) {
                Some(&r) => self.mv(f, dst, r),
                None => {
                    self.emit(f, Op::LoadGlobal { dst, g: *g });
                }
            },
            HKind::FnValue(fid) => {
                let k = match (&self.cp.fns[*fid as usize].kind, self.fn_protos[*fid as usize]) {
                    (FnKind::Native(n), _) => self.const_idx(Const::Native(*n)),
                    (_, Some(p)) => self.const_idx(Const::Fn(p)),
                    _ => self.const_idx(Const::Str("<missing fn>".into())),
                };
                self.emit(f, Op::LoadConst { dst, k });
            }
            HKind::VariantFn { rt_id, arity } => {
                let p = self.variant_wrapper(*rt_id, *arity, f.file);
                let k = self.const_idx(Const::Fn(p));
                self.emit(f, Op::LoadConst { dst, k });
            }
            HKind::Tag(t) => {
                self.emit(f, Op::LoadTag { dst, tag: *t });
            }
            HKind::Let { pat, init, else_ } => {
                self.compile_let(f, pat, init, else_.as_deref());
                self.unit_to(f, dst);
            }
            HKind::LocalFns { locals, lambdas } => {
                let unit = self.alloc(f);
                self.emit(f, Op::LoadUnit { dst: unit });
                let regs: Vec<Reg> = locals.iter().map(|&l| self.declare_local(f, l)).collect();
                for (&l, &r) in locals.iter().zip(&regs) {
                    let _ = l;
                    self.emit(f, Op::NewCell { dst: r, src: unit });
                }
                for (&r, lam) in regs.iter().zip(lambdas) {
                    let t = self.alloc(f);
                    self.expr_to(f, lam, t);
                    self.emit(f, Op::StoreCell { cell: r, src: t });
                    f.next_reg -= 1;
                }
                self.unit_to(f, dst);
            }
            HKind::Assign { place, op, op_ty, value } => {
                self.compile_assign(f, place, *op, *op_ty, value);
                self.unit_to(f, dst);
            }
            HKind::Block(items) if self.struct_literal(f, items).is_some() => {
                let (n, def, fields) = self.struct_literal(f, items).unwrap();
                let mark = f.next_reg;
                let start = self.alloc_n(f, fields.len());
                // The fields given, in source order, straight into their
                // registers; then the others (from `..base` or defaults).
                let mut done = vec![false; fields.len()];
                for it in &items[..n] {
                    let HKind::Let { pat, init, else_ } = &it.kind else { unreachable!() };
                    let HPatKind::Bind(l) = pat.kind else { unreachable!() };
                    match fields.iter().position(|x| matches!(x.kind, HKind::Local(m) if m == l)) {
                        Some(i) => {
                            self.expr_to(f, init, start + i as Reg);
                            done[i] = true;
                        }
                        // `..base`
                        None => self.compile_let(f, pat, init, else_.as_deref()),
                    }
                }
                for (i, x) in fields.iter().enumerate() {
                    if !done[i] {
                        self.expr_to(f, x, start + i as Reg);
                    }
                }
                let def = self.def_u16(f, def);
                self.emit(f, Op::NewStruct { dst, start, def });
                f.next_reg = mark;
            }
            HKind::Block(items) => {
                let mark = f.next_reg;
                if items.is_empty() {
                    self.unit_to(f, dst);
                } else {
                    let pending = f.pending;
                    f.defers.push(Vec::new());
                    for it in &items[..items.len() - 1] {
                        self.discard(f, it);
                    }
                    let last = items.last().unwrap();
                    if f.defers.last().is_some_and(|d| !d.is_empty()) || matches!(last.kind, HKind::Defer(_)) {
                        // The value is computed before the deferred code runs
                        // (which may assign `dst` if it is a local's register).
                        let t = self.alloc(f);
                        self.expr_to(f, last, t);
                        if !e.ty.is_never() {
                            self.emit_scope_defers(f);
                        }
                        self.mv(f, dst, t);
                    } else {
                        self.expr_to(f, last, dst);
                    }
                    f.defers.pop();
                    self.set_pending(f, pending);
                }
                f.next_reg = mark;
            }
            HKind::Defer(_) => {
                self.discard(f, e);
                self.unit_to(f, dst);
            }
            HKind::If { cond, then, else_: None } if f.void == Some(dst) && self.loop_exit(f, then).is_some() => {
                // `if c { continue }` / `if c { break }`: one branch to the target.
                let (i, cont) = self.loop_exit(f, then).unwrap();
                let mark = f.next_reg;
                let j = self.cond_jump_true(f, cond);
                f.next_reg = mark;
                let cx = &mut f.loops[i];
                if cont { cx.continues.extend(j) } else { cx.breaks.extend(j) }
            }
            HKind::If { cond, then, else_ } => {
                let mark = f.next_reg;
                let jf = self.cond_jump(f, cond);
                f.next_reg = mark;
                self.expr_to(f, then, dst);
                match else_ {
                    Some(els) => {
                        let jend = self.emit_jump(f);
                        self.patch_list(f, jf);
                        self.expr_to(f, els, dst);
                        self.patch_here(f, jend);
                    }
                    // A statement: nothing to do when the condition is false.
                    None if f.void == Some(dst) => self.patch_list(f, jf),
                    None => {
                        let jend = self.emit_jump(f);
                        self.patch_list(f, jf);
                        self.unit_to(f, dst);
                        self.patch_here(f, jend);
                    }
                }
            }
            HKind::IfLet { pat, value, then, else_ } => {
                let mark = f.next_reg;
                self.declare_pattern_locals(f, pat);
                let v = self.scrutinee(f, value);
                let mut fails = Vec::new();
                self.match_scrut(f, pat, &v, &mut fails);
                self.expr_to(f, then, dst);
                if else_.is_none() && f.void == Some(dst) {
                    // A statement: nothing to do when the pattern fails.
                    self.patch_list(f, fails);
                    f.next_reg = mark;
                    return;
                }
                let jend = self.emit_jump(f);
                self.patch_list(f, fails);
                match else_ {
                    Some(els) => self.expr_to(f, els, dst),
                    None => {
                        self.unit_to(f, dst);
                    }
                }
                self.patch_here(f, jend);
                f.next_reg = mark;
            }
            HKind::Match { scrut, arms } => self.compile_match(f, scrut, arms, Some(dst)),
            HKind::Loop(body) => {
                let mark = f.next_reg;
                let top = self.here(f);
                f.loops.push(LoopCx { result: Some(dst), defer_depth: f.defers.len(), ..Default::default() });
                self.discard(f, body);
                self.emit_loop_back(f, top);
                let cx = f.loops.pop().unwrap();
                self.patch_list(f, cx.breaks);
                self.patch_all(f, cx.continues, top);
                f.next_reg = mark;
            }
            // Loops are rotated: the test is at the end, branching back to
            // the body (one dispatch per iteration fewer than a test at the
            // top and a `Loop`). A cheap condition is also tested on entry;
            // otherwise entry jumps to the test.
            HKind::While { cond, body } => {
                let mark = f.next_reg;
                let cheap = cheap_cond(cond);
                let entry = if cheap { self.cond_jump(f, cond) } else { vec![self.emit_jump(f)] };
                f.next_reg = mark;
                let top = self.here(f);
                f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                self.discard(f, body);
                let cx = f.loops.pop().unwrap();
                self.patch_list(f, cx.continues);
                if !cheap {
                    self.patch_list(f, entry.clone());
                }
                self.set_line(f, cond.span);
                let back = self.cond_jump_true(f, cond);
                f.next_reg = mark;
                self.patch_all(f, back, top);
                if cheap {
                    self.patch_list(f, entry);
                }
                self.patch_list(f, cx.breaks);
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::WhileLet { pat, value, body } => {
                let mark = f.next_reg;
                self.declare_pattern_locals(f, pat);
                match &pat.kind {
                    // Rotated: `Some` is tested at the end (`JSomeBack`).
                    HPatKind::Variant { rt_id, args } if *rt_id == self.prog.ids.some && args.len() == 1 => {
                        let sub = &args[0];
                        let t = match sub.kind {
                            HPatKind::Bind(l) if !self.is_cell(f, l) && f.reg(l).is_some() => f.reg(l).unwrap(),
                            _ => self.alloc(f),
                        };
                        let entry = self.emit_jump(f);
                        let top = self.here(f);
                        let mut fails = Vec::new();
                        self.match_pattern(f, sub, t, &mut fails);
                        f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                        self.discard(f, body);
                        let cx = f.loops.pop().unwrap();
                        self.patch_list(f, cx.continues);
                        self.patch_here(f, entry);
                        let vmark = f.next_reg;
                        let v = self.expr_reg(f, value);
                        let back = self.emit(f, Op::JSomeBack { src: v, dst: t, off: 0 });
                        self.patch(f, back, top);
                        f.next_reg = vmark;
                        self.patch_list(f, fails);
                        self.patch_list(f, cx.breaks);
                    }
                    _ => {
                        let top = self.here(f);
                        let vmark = f.next_reg;
                        let v = self.expr_reg(f, value);
                        let mut fails = Vec::new();
                        self.match_pattern(f, pat, v, &mut fails);
                        f.next_reg = vmark;
                        f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                        self.discard(f, body);
                        self.emit_loop_back(f, top);
                        let cx = f.loops.pop().unwrap();
                        self.patch_list(f, fails);
                        self.patch_list(f, cx.breaks);
                        self.patch_all(f, cx.continues, top);
                    }
                }
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::ForRange { pat, start, end, inclusive, body } => {
                let mark = f.next_reg;
                let base = self.alloc_n(f, 4);
                self.expr_to(f, start, base);
                self.expr_to(f, end, base + 1);
                if *inclusive {
                    self.emit(f, Op::LoadBool { dst: base + 2, v: false });
                }
                self.for_loop(f, if *inclusive { Step::RangeIncl } else { Step::Range }, pat, body, base, dst);
                f.next_reg = mark;
            }
            HKind::For { pat, iter, kind, body } => {
                let mark = f.next_reg;
                if *kind == IterKind::Channel {
                    let ch = self.alloc(f);
                    self.expr_to(f, iter, ch);
                    self.declare_pattern_locals(f, pat);
                    let (elem, direct) = self.loop_var(f, pat);
                    // Rotated: receives at the end (`Channel.recv`, then `JSomeBack`).
                    let entry = self.emit_jump(f);
                    let top = self.here(f);
                    if !direct {
                        let mut fails = Vec::new();
                        self.match_pattern(f, pat, elem, &mut fails);
                        self.patch_list(f, fails);
                    }
                    f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                    self.discard(f, body);
                    let cx = f.loops.pop().unwrap();
                    self.patch_list(f, cx.continues);
                    self.patch_here(f, entry);
                    let t = self.alloc(f);
                    match self.recv_native {
                        Some(n) => self.call_native(f, ch, n, t),
                        None => {
                            self.emit(f, Op::LoadTag { dst: t, tag: self.prog.ids.none });
                        }
                    }
                    let back = self.emit(f, Op::JSomeBack { src: t, dst: elem, off: 0 });
                    self.patch(f, back, top);
                    self.patch_list(f, cx.breaks);
                    self.unit_to(f, dst);
                    f.next_reg = mark;
                    return;
                }
                if matches!(kind, IterKind::Map | IterKind::Set) {
                    // Over the entries as they are now, read in place.
                    let base = self.alloc(f);
                    let r = self.expr_reg(f, iter);
                    self.emit(f, Op::NewIter { dst: base, src: r });
                    let step = if *kind == IterKind::Map { Step::Map } else { Step::Set };
                    self.for_loop(f, step, pat, body, base, dst);
                    f.next_reg = mark;
                    return;
                }
                let base = self.alloc_n(f, 4);
                match kind {
                    IterKind::List => self.expr_to(f, iter, base),
                    IterKind::Chars => {
                        self.expr_to(f, iter, base);
                        self.emit(f, Op::LoadInt { dst: base + 1, v: 0 });
                        self.for_loop(f, Step::Chars, pat, body, base, dst);
                        f.next_reg = mark;
                        return;
                    }
                    IterKind::Range => {
                        // Loop over the bounds, without materialising the range.
                        let r = self.alloc(f);
                        self.expr_to(f, iter, r);
                        self.emit(f, Op::UnpackRange { base, src: r });
                        self.for_loop(f, Step::RangeIncl, pat, body, base, dst);
                        f.next_reg = mark;
                        return;
                    }
                    IterKind::Map | IterKind::Set | IterKind::Channel => unreachable!("handled above"),
                }
                self.emit(f, Op::LoadInt { dst: base + 1, v: 0 });
                self.emit(f, Op::Len { dst: base + 2, src: base });
                self.for_loop(f, Step::List, pat, body, base, dst);
                f.next_reg = mark;
            }
            HKind::Break { value: v, depth } => {
                let target = f.loops.len().checked_sub(1 + *depth as usize);
                let result = target.and_then(|i| f.loops[i].result);
                let from = target.map_or(f.defers.len(), |i| f.loops[i].defer_depth);
                let deferred = f.defers[from..].iter().any(|d| !d.is_empty());
                match (v, result) {
                    // The value is computed before the deferred code runs.
                    (Some(v), Some(r)) if deferred => {
                        let mark = f.next_reg;
                        let t = self.alloc(f);
                        self.expr_to(f, v, t);
                        self.emit_defers(f, from);
                        self.mv(f, r, t);
                        f.next_reg = mark;
                    }
                    (Some(v), Some(r)) => self.expr_to(f, v, r),
                    (None, Some(r)) => {
                        self.emit_defers(f, from);
                        self.emit(f, Op::LoadUnit { dst: r });
                    }
                    _ => self.emit_defers(f, from),
                }
                let j = self.emit_jump(f);
                if let Some(i) = target {
                    f.loops[i].breaks.push(j);
                }
            }
            HKind::Continue { depth } => {
                let target = f.loops.len().checked_sub(1 + *depth as usize);
                self.emit_defers(f, target.map_or(f.defers.len(), |i| f.loops[i].defer_depth));
                let j = self.emit_jump(f);
                if let Some(i) = target {
                    f.loops[i].continues.push(j);
                }
            }
            HKind::Return(v) => match v {
                Some(v) => {
                    let mark = f.next_reg;
                    self.ret_expr(f, v);
                    f.next_reg = mark;
                }
                None => {
                    self.emit_defers(f, 0);
                    self.emit(f, Op::RetUnit);
                }
            },
            HKind::Call { func, args } => {
                let mark = f.next_reg;
                let base = self.call_args(f, args, 0);
                self.set_line(f, e.span);
                match self.fn_protos[*func as usize] {
                    Some(p) => self.call_direct(f, base, p, dst),
                    None => self.unit_to(f, dst),
                }
                f.next_reg = mark;
            }
            HKind::CallNative { native, args, reflect } => {
                if let (Some(&op @ (Inline::MapGet | Inline::MapHas)), [HArg::Expr(m), HArg::Expr(k)]) =
                    (self.inline.get(native), args.as_slice())
                    && let HKind::Tuple(items) = &k.kind
                    && let Ok(n) = u8::try_from(items.len())
                {
                    let mark = f.next_reg;
                    let map = self.operand(f, m, &[k]);
                    let start = self.alloc_n(f, items.len());
                    for (i, a) in items.iter().enumerate() {
                        self.expr_to(f, a, start + i as Reg);
                    }
                    self.emit(
                        f,
                        match op {
                            Inline::MapGet => Op::MapGetTuple { dst, map, start, n },
                            _ => Op::MapHasTuple { dst, map, start, n },
                        },
                    );
                    f.next_reg = mark;
                    return;
                }
                if let (Some(&op), true) = (self.inline.get(native), args.iter().all(|a| matches!(a, HArg::Expr(_)))) {
                    let mark = f.next_reg;
                    let args: Vec<&HExpr> = args
                        .iter()
                        .map(|a| match a {
                            HArg::Expr(x) => x,
                            HArg::Default { .. } => unreachable!(),
                        })
                        .collect();
                    let regs: Vec<Reg> = (0..args.len()).map(|i| self.operand(f, args[i], &args[i + 1..])).collect();
                    match op {
                        Inline::Len => {
                            self.emit(f, Op::Len { dst, src: regs[0] });
                        }
                        Inline::ToFloat => {
                            self.emit(f, Op::IntToFloat { dst, src: regs[0] });
                        }
                        Inline::StrClass(class) => {
                            self.emit(f, Op::StrClass { dst, src: regs[0], class });
                        }
                        Inline::Push => {
                            self.emit(f, Op::ListPush { list: regs[0], src: regs[1] });
                            self.unit_to(f, dst)
                        }
                        Inline::ListGet => {
                            self.emit(f, Op::ListGet { dst, list: regs[0], idx: regs[1] });
                        }
                        Inline::ListPop => {
                            self.emit(f, Op::ListPop { dst, list: regs[0] });
                        }
                        Inline::MapGet => {
                            self.emit(f, Op::MapGet { dst, map: regs[0], key: regs[1] });
                        }
                        Inline::MapHas => {
                            self.emit(f, Op::MapHas { dst, map: regs[0], key: regs[1] });
                        }
                        Inline::MapGetOr => {
                            // The default goes to `dst` first, so it must not
                            // be the register of the map or key.
                            let out = if dst == regs[0] || dst == regs[1] { self.alloc(f) } else { dst };
                            self.mv(f, out, regs[2]);
                            self.emit(f, Op::MapGetOr { dst: out, map: regs[0], key: regs[1] });
                            self.mv(f, dst, out);
                        }
                        Inline::SetHas => {
                            self.emit(f, Op::SetHas { dst, set: regs[0], key: regs[1] });
                        }
                        Inline::SetAdd => {
                            self.emit(f, Op::SetAdd { dst, set: regs[0], key: regs[1] });
                        }
                    }
                    f.next_reg = mark;
                    return;
                }
                let mark = f.next_reg;
                if let ([HArg::Expr(x)], true) = (args.as_slice(), reflect.is_empty()) {
                    let r = self.expr_reg(f, x);
                    self.set_line(f, e.span);
                    self.call_native(f, r, *native, dst);
                    f.next_reg = mark;
                    return;
                }
                let base = self.call_args(f, args, reflect.len());
                for (i, t) in reflect.iter().enumerate() {
                    let prog = &mut *self.prog;
                    let add = |desc| {
                        prog.consts.push(Const::TypeDesc(desc));
                        (prog.consts.len() - 1) as u32
                    };
                    let Some(k) = self.descs.const_for(self.cp, &mut self.arena, *t, add) else {
                        self.errors.push(Diag::error(e.span, "type too large to describe at runtime"));
                        continue;
                    };
                    self.emit(f, Op::LoadConst { dst: base + i as Reg, k });
                }
                self.set_line(f, e.span);
                self.call_native(f, base, *native, dst);
                f.next_reg = mark;
            }
            HKind::CallValue { callee, args } if matches!(callee.kind, HKind::Local(l) if f.self_local == Some(l)) => {
                let mark = f.next_reg;
                self.argc(f, args.len(), e.span);
                let base = self.alloc_n(f, args.len().max(1));
                for (i, a) in args.iter().enumerate() {
                    self.expr_to(f, a, base + i as Reg);
                }
                self.set_line(f, e.span);
                self.emit(f, Op::CallSelf { base, dst });
                f.next_reg = mark;
            }
            HKind::CallValue { callee, args } => {
                let mark = f.next_reg;
                let argc = self.argc(f, args.len(), e.span);
                // The function is called where it is (a local's register, a
                // hoisted field, a temporary).
                let c = self.operand(f, callee, &args.iter().collect::<Vec<_>>());
                let base = self.alloc_n(f, args.len().max(1));
                for (i, a) in args.iter().enumerate() {
                    self.expr_to(f, a, base + i as Reg);
                }
                self.set_line(f, e.span);
                self.emit(f, Op::CallR { callee: c, base, dst, argc });
                f.next_reg = mark;
            }
            HKind::CallTrait { slot, args, impl_id, .. } => {
                let mark = f.next_reg;
                match self.static_target(*impl_id, *slot) {
                    // The impl is known: call its method directly.
                    Some(Target::Proto(proto)) => {
                        let base = self.call_args(f, args, 0);
                        self.set_line(f, e.span);
                        self.call_direct(f, base, proto, dst);
                    }
                    Some(Target::Native(native)) => {
                        let base = self.call_args(f, args, 0);
                        self.set_line(f, e.span);
                        self.call_native(f, base, native, dst);
                    }
                    // Otherwise fetch the method from the dictionary (the first argument).
                    None => {
                        let argc = self.argc(f, args.len(), e.span);
                        let base = self.call_args(f, args, 0);
                        let c = self.method(f, args, base, *slot);
                        self.set_line(f, e.span);
                        self.emit(f, Op::CallR { callee: c, base, dst, argc });
                    }
                }
                f.next_reg = mark;
            }
            HKind::Dict { impl_id, args } => {
                if let Some(k) = self.const_dict(e) {
                    self.emit(f, Op::LoadConst { dst, k });
                    return;
                }
                let mark = f.next_reg;
                let slots = &self.cp.impls[*impl_id as usize].slots;
                let n = slots.len() + args.len();
                let start = self.alloc_n(f, n);
                for (i, &fid) in slots.iter().enumerate() {
                    let k = self.fn_const(fid);
                    self.emit(f, Op::LoadConst { dst: start + i as Reg, k });
                }
                for (i, a) in args.iter().enumerate() {
                    self.expr_to(f, a, start + (slots.len() + i) as Reg);
                }
                self.emit(f, Op::NewTuple { dst, start, count: n as u16 });
                f.next_reg = mark;
            }
            HKind::MakeVariant { rt_id, args } => {
                let mark = f.next_reg;
                let start = match args.as_slice() {
                    // One payload: read in place (e.g. from a local's register).
                    [a] => self.expr_reg(f, a),
                    _ => {
                        let start = self.alloc_n(f, args.len());
                        for (i, a) in args.iter().enumerate() {
                            self.expr_to(f, a, start + i as Reg);
                        }
                        start
                    }
                };
                let variant = self.variant_u16(f, *rt_id);
                self.emit(f, Op::NewVariant { dst, start, variant });
                f.next_reg = mark;
            }
            HKind::MakeStruct { def, fields } => {
                let mark = f.next_reg;
                let start = self.alloc_n(f, fields.len());
                for (i, a) in fields.iter().enumerate() {
                    self.expr_to(f, a, start + i as Reg);
                }
                let def = self.def_u16(f, *def);
                self.emit(f, Op::NewStruct { dst, start, def });
                f.next_reg = mark;
            }
            HKind::Tuple(items) => {
                let mark = f.next_reg;
                let start = self.alloc_n(f, items.len());
                for (i, a) in items.iter().enumerate() {
                    self.expr_to(f, a, start + i as Reg);
                }
                self.emit(f, Op::NewTuple { dst, start, count: items.len() as u16 });
                f.next_reg = mark;
            }
            HKind::List(items) => {
                let mark = f.next_reg;
                if items.len() <= 64 {
                    let start = self.alloc_n(f, items.len());
                    for (i, a) in items.iter().enumerate() {
                        self.expr_to(f, a, start + i as Reg);
                    }
                    self.emit(f, Op::NewList { dst, start, count: items.len() as u16 });
                } else {
                    let l = self.alloc(f);
                    self.emit(f, Op::NewList { dst: l, start: l, count: 0 });
                    let t = self.alloc(f);
                    for a in items {
                        self.expr_to(f, a, t);
                        self.emit(f, Op::ListPush { list: l, src: t });
                    }
                    self.mv(f, dst, l);
                }
                f.next_reg = mark;
            }
            HKind::Map(pairs) => {
                let mark = f.next_reg;
                if pairs.len() <= 32 {
                    let start = self.alloc_n(f, pairs.len() * 2);
                    for (i, (k, v)) in pairs.iter().enumerate() {
                        self.expr_to(f, k, start + 2 * i as Reg);
                        self.expr_to(f, v, start + 2 * i as Reg + 1);
                    }
                    self.emit(f, Op::NewMap { dst, start, count: pairs.len() as u16 });
                } else {
                    let m = self.alloc(f);
                    self.emit(f, Op::NewMap { dst: m, start: m, count: 0 });
                    let k = self.alloc(f);
                    let v = self.alloc(f);
                    for (ke, ve) in pairs {
                        self.expr_to(f, ke, k);
                        self.expr_to(f, ve, v);
                        self.emit(f, Op::SetIndexMap { obj: m, key: k, src: v });
                    }
                    self.mv(f, dst, m);
                }
                f.next_reg = mark;
            }
            HKind::Repeat { elem, count } => {
                let mark = f.next_reg;
                let l = self.alloc(f);
                self.emit(f, Op::NewList { dst: l, start: l, count: 0 });
                let base = self.alloc_n(f, 4);
                self.emit(f, Op::LoadInt { dst: base, v: 0 });
                self.expr_to(f, count, base + 1);
                self.emit(f, Op::CheckRepeat { src: base + 1 });
                let var = self.alloc(f);
                let exit = self.emit(f, Step::Range.op(var, base, false));
                let top = self.here(f);
                let t = self.alloc(f);
                self.expr_to(f, elem, t);
                self.emit(f, Op::ListPush { list: l, src: t });
                let back = self.emit(f, Step::Range.op(var, base, true));
                self.patch(f, back, top);
                self.patch_here(f, exit);
                self.mv(f, dst, l);
                f.next_reg = mark;
            }
            HKind::PushLocal { list, value } => {
                let mark = f.next_reg;
                let l = self.local_operand(f, *list);
                let v = self.expr_reg(f, value);
                self.emit(f, Op::ListPush { list: l, src: v });
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::InsertLocal { map, key, value } => {
                let mark = f.next_reg;
                let m = self.local_operand(f, *map);
                let k = self.operand(f, key, &[value]);
                let v = self.expr_reg(f, value);
                self.emit(f, Op::SetIndexMap { obj: m, key: k, src: v });
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::Field { obj, index } => {
                let mark = f.next_reg;
                let o = self.expr_reg(f, obj);
                let idx = self.field_u16(f, *index);
                self.emit(f, Op::GetField { dst, obj: o, idx });
                f.next_reg = mark;
            }
            HKind::Index { obj, index, map } => {
                let mark = f.next_reg;
                let o = self.operand(f, obj, &[index]);
                let i = self.expr_reg(f, index);
                self.set_line(f, e.span);
                if *map {
                    self.emit(f, Op::IndexMap { dst, obj: o, key: i });
                } else {
                    self.emit(f, Op::Index { dst, obj: o, idx: i });
                }
                f.next_reg = mark;
            }
            HKind::Slice { obj, start, end, inclusive, kind } => {
                let mark = f.next_reg;
                let base = self.alloc_n(f, 3);
                self.expr_to(f, obj, base);
                let mut flags = 0u16;
                if let Some(s) = start {
                    self.expr_to(f, s, base + 1);
                    flags |= 1;
                }
                if let Some(en) = end {
                    self.expr_to(f, en, base + 2);
                    flags |= 2;
                }
                if *inclusive {
                    flags |= 4;
                }
                match kind {
                    SeqKind::Str => flags |= 8,
                    SeqKind::Bytes => flags |= 16,
                    SeqKind::List => {}
                }
                self.set_line(f, e.span);
                self.emit(f, Op::Slice { dst, base, flags });
                f.next_reg = mark;
            }
            HKind::Unary { op: HUnOp::Neg, .. } if int_lit(e).is_some() => self.load_int(f, dst, int_lit(e).unwrap()),
            HKind::Unary { op: HUnOp::Neg, .. } if float_lit(e).is_some() => {
                let k = self.const_idx(Const::Float(float_lit(e).unwrap()));
                self.emit(f, Op::LoadConst { dst, k });
            }
            HKind::Unary { op, e: inner } => {
                let mark = f.next_reg;
                let a = self.expr_reg(f, inner);
                let op = match op {
                    HUnOp::Not => Op::Not { dst, a },
                    HUnOp::BitNot => Op::BitNot { dst, a },
                    HUnOp::Neg => match inner.ty {
                        TypeId::INT => Op::NegI { dst, a },
                        TypeId::FLOAT => Op::NegF { dst, a },
                        _ => Op::Neg { dst, a },
                    },
                };
                self.set_line(f, e.span);
                self.emit(f, op);
                f.next_reg = mark;
            }
            HKind::Arith { op, l, r } => {
                let mark = f.next_reg;
                // `x + k`, `x - k`, `x * k`, `x / k`, `x % k`, `k + x`, `k * x` with
                // small integer constants.
                if matches!(l.ty, TypeId::INT)
                    && let Some((x, k)) = imm_operands(*op, l, r)
                    && let Some(imm) = imm_op(*op, k)
                {
                    let a = self.expr_reg(f, x);
                    self.set_line(f, e.span);
                    self.emit(f, imm(dst, a));
                    f.next_reg = mark;
                    return;
                }
                let a = self.operand(f, l, &[r]);
                let b = self.expr_reg(f, r);
                let op = arith_op(*op, l.ty, r.ty, dst, a, b);
                self.set_line(f, e.span);
                self.emit(f, op);
                f.next_reg = mark;
            }
            HKind::Cmp { op, l, r } => {
                let mark = f.next_reg;
                if matches!(l.ty, TypeId::INT)
                    && let Some((x, op, k)) = match (small_lit(l), small_lit(r)) {
                        (_, Some(k)) => Some((l, *op, k)),
                        (Some(k), None) => Some((r, mirror_cmp(*op), k)),
                        _ => None,
                    }
                {
                    let a = self.expr_reg(f, x);
                    self.emit(f, cmp_k(op, dst, a, k));
                    f.next_reg = mark;
                    return;
                }
                let a = self.operand(f, l, &[r]);
                let b = self.expr_reg(f, r);
                let op = cmp_op(*op, l.ty, dst, a, b);
                self.emit(f, op);
                f.next_reg = mark;
            }
            // `dst` is written only once the value is known: it may be the
            // register of a local that `b` reads (`x = y and x`).
            HKind::And(a, b) | HKind::Or(a, b) => {
                let and = matches!(e.kind, HKind::And(..));
                let mark = f.next_reg;
                let short = if and { self.cond_jump(f, a) } else { self.cond_jump_true(f, a) };
                f.next_reg = mark;
                self.expr_to(f, b, dst);
                if f.void == Some(dst) || short.is_empty() {
                    self.patch_list(f, short);
                } else {
                    let jend = self.emit_jump(f);
                    self.patch_list(f, short);
                    self.emit(f, Op::LoadBool { dst, v: !and });
                    self.patch_here(f, jend);
                }
            }
            HKind::Lambda(lam) => {
                let p = self.compile_lambda(f, lam);
                self.emit(f, Op::MakeClosure { dst, proto: p });
            }
            HKind::Try { e: inner, kind } => {
                let mark = f.next_reg;
                let test = |src: Reg| match kind {
                    TryKind::Option => Op::JNotSome { src, dst, off: 0 },
                    _ => Op::JNotOk { src, dst, off: 0 },
                };
                if defers_pending(f) {
                    // A fresh register: deferred code may assign the local tested.
                    let v = self.alloc(f);
                    self.expr_to(f, inner, v);
                    self.set_line(f, e.span);
                    let fail = self.emit(f, test(v));
                    let jend = self.emit_jump(f);
                    self.patch_here(f, fail);
                    self.emit_defers(f, 0);
                    match kind {
                        TryKind::Option => {
                            self.emit(f, Op::LoadTag { dst: v, tag: self.prog.ids.none });
                            self.emit(f, Op::Ret { src: v });
                        }
                        TryKind::Result => {
                            self.emit(f, Op::Ret { src: v });
                        }
                        TryKind::TopLevel => {
                            self.emit(f, Op::TryTop { src: v });
                        }
                    }
                    self.patch_here(f, jend);
                } else {
                    let v = self.expr_reg(f, inner);
                    self.set_line(f, e.span);
                    let fail = self.emit(f, test(v));
                    let stub = match kind {
                        TryKind::Option => Stub::RetNone,
                        TryKind::Result => Stub::Ret(v),
                        TryKind::TopLevel => Stub::TryTop(v),
                    };
                    self.jump_to_stub(f, fail, stub);
                }
                f.next_reg = mark;
            }
            // `m.get(k) ?? d` and `xs.get(i) ?? d` in one instruction when
            // `d` can be evaluated first (it is a constant or a plain local).
            HKind::Coalesce { l, r, result: false }
                if let HKind::CallNative { native, args, .. } = &l.kind
                    && let (Some(&op @ (Inline::MapGet | Inline::ListGet)), [HArg::Expr(obj), HArg::Expr(key)]) =
                        (self.inline.get(native), args.as_slice())
                    && (int_lit(r).is_some()
                        || float_lit(r).is_some()
                        || matches!(r.kind, HKind::Str(_) | HKind::Bool(_) | HKind::Unit | HKind::Tag(_))
                        || matches!(r.kind, HKind::Local(x) if !self.is_cell(f, x) && !f.facts.builder[x as usize])) =>
            {
                let mark = f.next_reg;
                let o = self.operand(f, obj, &[key]);
                let k = self.expr_reg(f, key);
                let out = if dst == o || dst == k { self.alloc(f) } else { dst };
                self.expr_to(f, r, out);
                self.emit(
                    f,
                    match op {
                        Inline::MapGet => Op::MapGetOr { dst: out, map: o, key: k },
                        _ => Op::ListGetOr { dst: out, list: o, idx: k },
                    },
                );
                self.mv(f, dst, out);
                f.next_reg = mark;
            }
            HKind::Coalesce { l, r, result } => {
                let mark = f.next_reg;
                let v = self.expr_reg(f, l);
                let other = if *result { Op::JNotOk { src: v, dst, off: 0 } } else { Op::JNotSome { src: v, dst, off: 0 } };
                let other = self.emit(f, other);
                let jend = self.emit_jump(f);
                self.patch_here(f, other);
                self.expr_to(f, r, dst);
                self.patch_here(f, jend);
                f.next_reg = mark;
            }
            HKind::Interp(parts) => {
                let mark = f.next_reg;
                let tmpl = self.template(f, parts);
                self.emit(f, Op::Interp { dst, tmpl });
                f.next_reg = mark;
            }
            HKind::Range { start, end, inclusive } => {
                let mark = f.next_reg;
                let a = self.operand(f, start, &[end]);
                let b = self.expr_reg(f, end);
                if *inclusive {
                    self.emit(f, Op::NewRangeIncl { dst, a, b });
                } else {
                    self.emit(f, Op::NewRange { dst, a, b });
                }
                f.next_reg = mark;
            }
            HKind::Spawn(lam) => {
                let mark = f.next_reg;
                let c = self.expr_reg(f, lam);
                self.set_line(f, e.span);
                self.emit(f, Op::Spawn { dst, closure: c });
                f.next_reg = mark;
            }
            HKind::Inlined { func, body } => self.inlined(f, e, *func, body, |s, f| s.expr_to(f, body, dst)),
            HKind::ToStr(x) => {
                let mark = f.next_reg;
                let v = self.expr_reg(f, x);
                self.emit(f, Op::ToStr { dst, src: v });
                f.next_reg = mark;
            }
            HKind::Debug(x) => {
                let mark = f.next_reg;
                let v = self.expr_reg(f, x);
                self.emit(f, Op::Debug { dst, src: v });
                f.next_reg = mark;
            }
        }
    }

    /// A struct literal as the checker lowers it: hidden locals bound to the
    /// fields given, in source order, then `MakeStruct` (see
    /// `check_struct_lit`). Returns the number of `let`s, the struct and
    /// its field values.
    fn struct_literal<'e>(&self, f: &FnState, items: &'e [HExpr]) -> Option<(usize, TypeDefId, &'e [HExpr])> {
        let (HKind::MakeStruct { def, fields }, lets) = (&items.last()?.kind, &items[..items.len() - 1]) else { return None };
        let hidden = |it: &HExpr| {
            matches!(&it.kind, HKind::Let { pat: HPat { kind: HPatKind::Bind(l), .. }, else_: None, .. }
                if f.locals[*l as usize].name == crate::types::Symbol::TMP && !f.locals[*l as usize].cell)
        };
        lets.iter().all(hidden).then_some((lets.len(), *def, fields))
    }

    /// A field (or dictionary slot) index as an instruction operand.
    fn field_u16(&mut self, f: &FnState<'a>, i: u32) -> u16 {
        u16::try_from(i).unwrap_or_else(|_| {
            self.errors.push(Diag::error(Span::new(f.file, 0, 0), "too many fields in a type"));
            0
        })
    }

    fn def_u16(&mut self, f: &FnState<'a>, def: TypeDefId) -> u16 {
        u16::try_from(def).unwrap_or_else(|_| {
            self.errors.push(Diag::error(Span::new(f.file, 0, 0), "too many types in program"));
            0
        })
    }

    /// The loop (index in `f.loops`) and kind (`true` for `continue`) of a
    /// `continue` or `break` without a value that is a plain jump (no
    /// deferred code to run, no result to set): `e` or a block of just it.
    fn loop_exit(&self, f: &FnState<'a>, e: &'a HExpr) -> Option<(usize, bool)> {
        let (depth, cont) = match &e.kind {
            HKind::Block(items) if items.len() == 1 => return self.loop_exit(f, &items[0]),
            HKind::Continue { depth } => (*depth, true),
            HKind::Break { value: None, depth } => (*depth, false),
            _ => return None,
        };
        let i = f.loops.len().checked_sub(1 + depth as usize)?;
        let plain = (cont || f.loops[i].result.is_none()) && !f.defers[f.loops[i].defer_depth..].iter().any(|d| !d.is_empty());
        plain.then_some((i, cont))
    }

    /// Evaluates the values of an interpolation (`Op::Interp`) into
    /// registers and returns its template.
    fn template(&mut self, f: &mut FnState<'a>, parts: &'a [InterpPart]) -> u32 {
        let mut t = Template::default();
        let text = |t: &mut Template, s: &str| {
            t.text_len += s.len();
            match t.pieces.last_mut() {
                Some(Piece::Text(prev)) => prev.push_str(s),
                _ => t.pieces.push(Piece::Text(s.to_string())),
            }
        };
        let exprs: Vec<&HExpr> = parts
            .iter()
            .filter_map(|p| match p {
                InterpPart::Expr(x, _) => Some(&**x),
                InterpPart::Text(_) => None,
            })
            .collect();
        let mut k = 0;
        for p in parts {
            // `exprs[k..]`: the values after this one.
            k += matches!(p, InterpPart::Expr(..)) as usize;
            match p {
                InterpPart::Text(s) => text(&mut t, s),
                InterpPart::Expr(x, None) if let HKind::Str(s) = &x.kind => text(&mut t, s),
                InterpPart::Expr(x, spec) => {
                    // Read when the instruction runs: no later value may assign it.
                    let r = self.operand(f, x, &exprs[k..]);
                    t.pieces.push(match spec {
                        Some(spec) => Piece::Format(r, spec.clone()),
                        None => Piece::Value(r),
                    });
                }
            }
        }
        self.prog.templates.push(t);
        (self.prog.templates.len() - 1) as u32
    }

    /// The argument count of a call of a function value (an instruction operand).
    fn argc(&mut self, f: &FnState<'a>, n: usize, span: Span) -> u8 {
        u8::try_from(n).unwrap_or_else(|_| {
            if !f.too_big {
                self.errors.push(Diag::error(span, "too many arguments in a call of a function value (at most 255)"));
            }
            0
        })
    }

    /// A register holding method `slot` of the dictionary of a trait call
    /// (its first argument, already in `base`): hoisted before the loop,
    /// or loaded now.
    fn method(&mut self, f: &mut FnState<'a>, args: &'a [HArg], base: Reg, slot: u32) -> Reg {
        if let Some(k) = method_key(f, args, slot)
            && let Some(&r) = f.hoisted.get(&k)
        {
            return r;
        }
        let c = self.alloc(f);
        let idx = self.field_u16(f, slot);
        self.emit(f, Op::GetField { dst: c, obj: base, idx });
        c
    }

    /// What a trait call runs when its dictionary comes from a known impl.
    fn static_target(&self, impl_id: Option<u32>, slot: u32) -> Option<Target> {
        let fid = *self.cp.impls[impl_id? as usize].slots.get(slot as usize)?;
        match self.cp.fns[fid as usize].kind {
            FnKind::Native(n) => Some(Target::Native(n)),
            _ => self.fn_protos[fid as usize].map(Target::Proto),
        }
    }

    fn fn_const(&mut self, fid: FnId) -> u32 {
        match (&self.cp.fns[fid as usize].kind, self.fn_protos[fid as usize]) {
            (FnKind::Native(n), _) => self.const_idx(Const::Native(*n)),
            (_, Some(p)) => self.const_idx(Const::Fn(p)),
            _ => self.const_idx(Const::Str("<missing fn>".into())),
        }
    }

    /// A dictionary that does not depend on the function's own dictionaries
    /// is built once, as a constant.
    fn const_dict(&mut self, e: &'a HExpr) -> Option<u32> {
        let HKind::Dict { impl_id, args } = &e.kind else { return None };
        let mut items = Vec::new();
        for a in args {
            items.push(self.const_dict(a)?);
        }
        let slots = self.cp.impls[*impl_id as usize].slots.clone();
        let mut all: Vec<u32> = slots.iter().map(|&fid| self.fn_const(fid)).collect();
        all.extend(items);
        if let Some(&k) = self.dict_consts.get(&all) {
            return Some(k);
        }
        let k = self.const_idx(Const::Tuple(all.clone()));
        self.dict_consts.insert(all, k);
        Some(k)
    }

    /// The register a loop binding `pat` writes each element to, and
    /// whether that is the register of the local `pat` binds.
    fn loop_var(&mut self, f: &mut FnState<'a>, pat: &'a HPat) -> (Reg, bool) {
        match pat.kind {
            HPatKind::Bind(l) if !self.is_cell(f, l) => (f.reg(l).unwrap(), true),
            _ => (self.alloc(f), false),
        }
    }

    /// A loop over the registers at `base` (see `Step`), rotated: `step`
    /// tests on entry, and its backward form at the end.
    fn for_loop(&mut self, f: &mut FnState<'a>, step: Step, pat: &'a HPat, body: &'a HExpr, base: Reg, dst: Reg) {
        // The patterns to match at the top of the body, against registers.
        let mut binds: Vec<(&HPat, Reg)> = Vec::new();
        // A map's key and value go to two registers: those of the locals
        // `(k, v)` binds, or a tuple when the pattern is not a pair.
        let mut pair = None;
        let var = match step {
            Step::Map => {
                let key = self.alloc_n(f, 2);
                match &pat.kind {
                    HPatKind::Tuple(ps) if ps.len() == 2 => {
                        for (p, r) in ps.iter().zip([key, key + 1]) {
                            match p.kind {
                                HPatKind::Bind(l) if !self.is_cell(f, l) => f.set_reg(l, r),
                                HPatKind::Wildcard => {}
                                _ => {
                                    self.declare_pattern_locals(f, p);
                                    binds.push((p, r));
                                }
                            }
                        }
                    }
                    _ => {
                        self.declare_pattern_locals(f, pat);
                        let t = self.alloc(f);
                        pair = Some(t);
                        binds.push((pat, t));
                    }
                }
                key
            }
            _ => {
                self.declare_pattern_locals(f, pat);
                let (var, direct) = self.loop_var(f, pat);
                if !direct {
                    binds.push((pat, var));
                }
                var
            }
        };
        let exit = self.emit(f, step.op(var, base, false));
        let top = self.here(f);
        if let Some(t) = pair {
            self.emit(f, Op::NewTuple { dst: t, start: var, count: 2 });
        }
        for (p, r) in binds {
            let mut fails = Vec::new();
            self.match_pattern(f, p, r, &mut fails);
            self.patch_list(f, fails);
        }
        f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
        self.discard(f, body);
        let cx = f.loops.pop().unwrap();
        self.patch_list(f, cx.continues);
        let back = self.emit(f, step.op(var, base, true));
        self.patch(f, back, top);
        self.patch_here(f, exit);
        self.patch_list(f, cx.breaks);
        self.unit_to(f, dst);
    }

    fn jump_not_variant(&mut self, f: &mut FnState<'a>, src: Reg, variant: u16) -> Vec<usize> {
        vec![self.emit(f, Op::JNotVariant { src, variant, off: 0 })]
    }

    fn variant_wrapper(&mut self, rt: u32, arity: usize, file: u32) -> u32 {
        if let Some(&p) = self.variant_wrappers.get(&rt) {
            return p;
        }
        if rt > u16::MAX as u32 || arity >= MAX_REGS as usize {
            self.errors.push(Diag::error(Span::new(file, 0, 0), "too many enum variants or fields in program"));
            return 0;
        }
        let name = self.prog.variants.get(rt as usize).map(|v| v.name.clone()).unwrap_or_default();
        let code = match arity {
            1 => vec![Op::RetVariant { src: 0, variant: rt as u16 }],
            _ => vec![Op::NewVariant { dst: arity as Reg, start: 0, variant: rt as u16 }, Op::Ret { src: arity as Reg }],
        };
        let p = self.prog.protos.len() as u32;
        self.prog.protos.push(Proto {
            name: name.clone(),
            file,
            nparams: arity as u16,
            nregs: arity as u16 + 1,
            lines: vec![0; code.len()],
            code,
            captures: vec![],
            cleanups: Vec::new(),
            sites: Vec::new(),
            inlined: Vec::new(),
        });
        self.variant_wrappers.insert(rt, p);
        p
    }

    // ------------------------------------------------------------------
    // Loop-invariant hoisting

    /// The hoisting key of a constant, or of an immutable global initialized
    /// wherever the code being compiled runs.
    fn hoist_key(&self, f: &FnState<'a>, e: &'a HExpr) -> Option<HoistKey<'a>> {
        if let Some(i) = int_lit(e) {
            return Some(HoistKey::Int(i));
        }
        if let Some(x) = float_lit(e) {
            return Some(HoistKey::Float(x.to_bits()));
        }
        Some(match &e.kind {
            HKind::Str(s) => HoistKey::Str(s),
            HKind::Bytes(b) => HoistKey::Bytes(b),
            HKind::Dur(d) => HoistKey::Dur(*d),
            HKind::Tag(t) => HoistKey::Tag(*t),
            HKind::Global(g) if self.cp.globals.get(*g as usize).is_some_and(|gi| !gi.mutable) && self.order.ready(self.cp, *g, f.time) => {
                HoistKey::Global(*g)
            }
            HKind::Field { obj, index } => {
                let HKind::Local(l) = obj.kind else { return None };
                let fixed = match self.arena.get(obj.ty) {
                    Ty::Tuple(_) => true,
                    Ty::Named(d, _) => {
                        let t = &self.cp.types[d as usize];
                        t.is_struct() && t.fields().get(*index as usize).is_some_and(|fd| !fd.mutable)
                    }
                    _ => false,
                };
                if !fixed || f.facts.assigned[l as usize] || f.locals[l as usize].cell {
                    return None;
                }
                HoistKey::Field(l, *index)
            }
            _ => return None,
        })
    }

    /// Hoistable values used in loop `e` outside nested lambdas, most
    /// frequent first, and the size of `e`.
    ///
    /// Constants only count where the compiler evaluates them with
    /// `expr_reg` (as instruction operands): elsewhere they are materialized
    /// into a fixed register anyway, so hoisting would not save anything.
    /// Immutable globals count everywhere, since a `Move` from a register is
    /// cheaper than a checked `LoadGlobal`.
    fn hoist_candidates(&self, f: &FnState<'a>, e: &'a HExpr) -> (Vec<HoistKey<'a>>, usize) {
        let mut nodes = 0;
        let mut found: Vec<(HoistKey<'a>, usize)> = Vec::new();
        let mut index: FxHashMap<HoistKey<'a>, usize> = FxHashMap::default();
        let mut count = |k: HoistKey<'a>| match index.get(&k) {
            Some(&i) => found[i].1 += 1,
            None => {
                index.insert(k.clone(), found.len());
                found.push((k, 1));
            }
        };
        walk_pruned(e, &mut |x| {
            nodes += 1;
            if let HKind::Lambda(_) = &x.kind {
                // A separate function with its own registers.
                return false;
            }
            if let Some(k @ HoistKey::Global(_)) = self.hoist_key(f, x) {
                count(k);
            }
            if let HKind::CallTrait { slot, args, impl_id, .. } = &x.kind
                && self.static_target(*impl_id, *slot).is_none()
                && let Some(k) = method_key(f, args, *slot)
            {
                count(k);
            }
            self.operands(x, &mut |o| {
                if let Some(k) = self.hoist_key(f, o)
                    && !matches!(k, HoistKey::Global(_))
                {
                    count(k);
                }
            });
            true
        });
        // Most frequent first; ties keep source order. Fields and methods
        // of locals declared in the loop cannot be loaded before it.
        found.retain(|(k, _)| match k {
            HoistKey::Field(l, _) | HoistKey::Method(l, _) => f.reg(*l).is_some(),
            _ => true,
        });
        found.sort_by_key(|e| std::cmp::Reverse(e.1));
        (found.into_iter().map(|(k, _)| k).collect(), nodes)
    }

    /// Calls `out` for the direct sub-expressions of `x` that the compiler
    /// evaluates with `expr_reg` (approximately; used for hoisting only).
    fn operands<'x>(&self, x: &'x HExpr, out: &mut impl FnMut(&'x HExpr)) {
        let small = |e: &HExpr| small_lit(e).is_some();
        match &x.kind {
            HKind::Arith { op, l, r } => match imm_operands(*op, l, r) {
                Some((x, k)) if matches!(l.ty, TypeId::INT) && imm_op(*op, k).is_some() => out(x),
                _ => {
                    out(l);
                    out(r);
                }
            },
            // Integer comparisons with small constants use an immediate.
            HKind::Cmp { l, r, .. } => {
                let int = matches!(l.ty, TypeId::INT);
                if !(int && small(l)) {
                    out(l);
                }
                if !(int && small(r)) {
                    out(r);
                }
            }
            HKind::Index { obj, index, .. } => {
                out(obj);
                out(index);
            }
            HKind::Range { start, end, .. } => {
                out(start);
                out(end);
            }
            HKind::InsertLocal { key, value, .. } => {
                out(key);
                out(value);
            }
            HKind::Unary { e, .. } | HKind::Field { obj: e, .. } | HKind::Try { e, .. } | HKind::ToStr(e) | HKind::Debug(e) => out(e),
            HKind::Coalesce { l, .. } => out(l),
            HKind::PushLocal { value, .. } => out(value),
            HKind::IfLet { value, .. } | HKind::WhileLet { value, .. } => out(value),
            HKind::Match { scrut, .. } => out(scrut),
            HKind::CallValue { callee, .. } => out(callee),
            HKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(e, _) = p {
                        out(e);
                    }
                }
            }
            HKind::CallNative { native, args, .. } if self.inline.contains_key(native) => {
                for a in args {
                    if let HArg::Expr(e) = a {
                        out(e);
                    }
                }
            }
            HKind::Assign { place, op, value, .. } => {
                match place {
                    Place::Field { obj, .. } => out(obj),
                    Place::Index { obj, index, .. } => {
                        out(obj);
                        out(index);
                    }
                    _ => {}
                }
                let local = matches!(place, Place::Local(_));
                let immediate = matches!(op, Some(ArithOp::Add | ArithOp::Sub)) && small(value);
                if (op.is_some() || !local) && !immediate {
                    out(value);
                }
            }
            HKind::If { cond, .. } | HKind::While { cond, .. } => self.cond_operands(cond, out),
            _ => {}
        }
    }

    /// Operands of a condition compiled by `cond_jump`.
    fn cond_operands<'x>(&self, cond: &'x HExpr, out: &mut impl FnMut(&'x HExpr)) {
        match &cond.kind {
            HKind::And(a, b) => {
                self.cond_operands(a, out);
                self.cond_operands(b, out);
            }
            HKind::Unary { op: HUnOp::Not, e } => out(e),
            HKind::Cmp { .. } | HKind::Bool(_) => {}
            _ => out(cond),
        }
    }

    /// Loads the hoisted value `k` into `dst`.
    fn load_hoisted(&mut self, f: &mut FnState<'a>, k: &HoistKey<'a>, dst: Reg) {
        match k {
            HoistKey::Int(i) => self.load_int(f, dst, *i),
            HoistKey::Str(s) => self.load_str(f, dst, s),
            HoistKey::Tag(t) => {
                self.emit(f, Op::LoadTag { dst, tag: *t });
            }
            HoistKey::Float(_) | HoistKey::Bytes(_) | HoistKey::Dur(_) => {
                let c = match k {
                    HoistKey::Float(bits) => Const::Float(f64::from_bits(*bits)),
                    HoistKey::Bytes(b) => Const::Bytes(b.to_vec()),
                    HoistKey::Dur(d) => Const::Dur(*d),
                    _ => unreachable!(),
                };
                let kidx = self.const_idx(c);
                self.emit(f, Op::LoadConst { dst, k: kidx });
            }
            HoistKey::Global(g) => {
                self.emit(f, Op::LoadGlobal { dst, g: *g });
            }
            HoistKey::Field(l, idx) | HoistKey::Method(l, idx) => {
                let obj = f.reg(*l).expect("a local declared before the loop");
                self.emit(f, Op::GetField { dst, obj, idx: *idx as u16 });
            }
        }
    }

    /// Compiles the outermost loop `e` with its loop-invariant constants,
    /// immutable globals and fields loaded into registers once, before it.
    fn hoisted_loop(&mut self, f: &mut FnState<'a>, e: &'a HExpr, dst: Reg) {
        let mark = f.next_reg;
        f.hoisting = true;
        let (cands, nodes) = self.hoist_candidates(f, e);
        if !self.budget.charge(nodes as u64) {
            f.hoisting = false;
            return;
        }
        if f.next_reg + cands.len().min(MAX_HOISTED) as u32 <= HOIST_REG_BUDGET {
            for k in cands.into_iter().take(MAX_HOISTED) {
                let r = self.alloc(f);
                self.load_hoisted(f, &k, r);
                f.hoisted.insert(k, r);
            }
        }
        self.expr_to(f, e, dst);
        f.hoisted.clear();
        f.hoisting = false;
        f.next_reg = mark;
    }

    // ------------------------------------------------------------------
    // Assignment

    fn compile_assign(&mut self, f: &mut FnState<'a>, place: &'a Place, op: Option<ArithOp>, op_ty: TypeId, value: &'a HExpr) {
        let mark = f.next_reg;
        match place {
            Place::Local(l) => match op {
                None => {
                    if let (Some(r), false) = (f.reg(*l), self.is_cell(f, *l)) {
                        self.expr_to(f, value, r);
                    } else {
                        let v = self.expr_reg(f, value);
                        self.write_local(f, *l, v);
                    }
                }
                Some(aop) => {
                    if let (Some(r), false) = (f.reg(*l), self.is_cell(f, *l)) {
                        if f.facts.builder[*l as usize] && aop == ArithOp::Add {
                            if let HKind::Interp(parts) = &value.kind {
                                let tmpl = self.template(f, parts);
                                self.emit(f, Op::AppendInterp { local: r, tmpl });
                            } else {
                                let v = self.expr_reg(f, value);
                                self.emit(f, Op::AppendStr { local: r, src: v });
                            }
                            f.next_reg = mark;
                            return;
                        }
                        // Operate on the local's register directly.
                        self.apply_op(f, aop, op_ty, value.ty, r, value);
                    } else {
                        let cur = self.alloc(f);
                        self.read_local(f, *l, cur);
                        self.apply_op(f, aop, op_ty, value.ty, cur, value);
                        self.write_local(f, *l, cur);
                    }
                }
            },
            Place::Global(g) => {
                let v = match op {
                    None => self.expr_reg(f, value),
                    Some(aop) => {
                        let cur = self.alloc(f);
                        self.emit(f, Op::LoadGlobal { dst: cur, g: *g });
                        self.apply_op(f, aop, op_ty, value.ty, cur, value);
                        cur
                    }
                };
                self.emit(f, Op::StoreGlobal { g: *g, src: v });
            }
            Place::Field { obj, index } => {
                let o = self.operand(f, obj, &[value]);
                let v = match op {
                    None => self.expr_reg(f, value),
                    Some(aop) => {
                        let cur = self.alloc(f);
                        let idx = self.field_u16(f, *index);
                        self.emit(f, Op::GetField { dst: cur, obj: o, idx });
                        self.apply_op(f, aop, op_ty, value.ty, cur, value);
                        cur
                    }
                };
                let idx = self.field_u16(f, *index);
                self.emit(f, Op::SetField { obj: o, idx, src: v });
            }
            Place::Index { obj, index, map } => {
                let o = self.operand(f, obj, &[index, value]);
                let i = self.operand(f, index, &[value]);
                let v = match op {
                    None => self.expr_reg(f, value),
                    Some(aop) => {
                        let cur = self.alloc(f);
                        if *map {
                            self.emit(f, Op::IndexMap { dst: cur, obj: o, key: i });
                        } else {
                            self.emit(f, Op::Index { dst: cur, obj: o, idx: i });
                        }
                        self.apply_op(f, aop, op_ty, value.ty, cur, value);
                        cur
                    }
                };
                if *map {
                    self.emit(f, Op::SetIndexMap { obj: o, key: i, src: v });
                } else {
                    self.emit(f, Op::SetIndex { obj: o, idx: i, src: v });
                }
            }
        }
        f.next_reg = mark;
    }

    /// `cur = cur <op> value`.
    fn apply_op(&mut self, f: &mut FnState<'a>, op: ArithOp, lt: TypeId, rt: TypeId, cur: Reg, value: &'a HExpr) {
        if matches!(lt, TypeId::INT)
            && matches!(op, ArithOp::Add | ArithOp::Sub)
            && let Some(k) = int_lit(value)
        {
            let k = if op == ArithOp::Sub { k.checked_neg() } else { Some(k) };
            if let Some(k) = k.filter(|k| *k >= i16::MIN as i64 && *k <= i16::MAX as i64) {
                self.emit(f, Op::AddIK { dst: cur, a: cur, k: k as i16 });
                return;
            }
        }
        let v = self.expr_reg(f, value);
        let o = arith_op(op, lt, rt, cur, cur, v);
        self.emit(f, o);
    }

    // ------------------------------------------------------------------
    // Patterns

    fn declare_pattern_locals(&mut self, f: &mut FnState<'a>, p: &'a HPat) {
        match &p.kind {
            HPatKind::Bind(l) => {
                if f.reg(*l).is_none() {
                    self.declare_local(f, *l);
                }
            }
            HPatKind::Tuple(ps) | HPatKind::Or(ps) => ps.iter().for_each(|x| self.declare_pattern_locals(f, x)),
            HPatKind::Variant { args, .. } => args.iter().for_each(|x| self.declare_pattern_locals(f, x)),
            HPatKind::Struct { fields } => fields.iter().for_each(|(_, x)| self.declare_pattern_locals(f, x)),
            HPatKind::List { before, rest, after } => {
                before.iter().for_each(|x| self.declare_pattern_locals(f, x));
                if let Some(Some(l)) = rest
                    && f.reg(*l).is_none()
                {
                    self.declare_local(f, *l);
                }
                after.iter().for_each(|x| self.declare_pattern_locals(f, x));
            }
            _ => {}
        }
    }

    /// Tests `v` against `p`, binding locals; jumps to `fails` on mismatch.
    fn match_pattern(&mut self, f: &mut FnState<'a>, p: &'a HPat, v: Reg, fails: &mut Vec<usize>) {
        match &p.kind {
            HPatKind::Wildcard => {}
            HPatKind::Bind(l) => self.init_local(f, *l, v),
            HPatKind::Int(i) => fails.push(self.jump_unless(f, CmpOp::Eq, v, *i)),
            HPatKind::Range(a, b) => {
                fails.push(self.jump_unless(f, CmpOp::Ge, v, *a));
                fails.push(self.jump_unless(f, CmpOp::Le, v, *b));
            }
            HPatKind::Str(s) => match u16::try_from(self.str_const(s)) {
                Ok(k) => fails.push(self.emit(f, Op::JNeqK { a: v, k, off: 0 })),
                Err(_) => {
                    let t = self.alloc(f);
                    self.load_str(f, t, s);
                    fails.push(self.emit(f, Op::JNeq { a: v, b: t, off: 0 }));
                }
            },
            HPatKind::Bool(b) => {
                if *b {
                    fails.push(self.emit(f, Op::JmpIfNot { cond: v, off: 0 }));
                } else {
                    fails.push(self.emit(f, Op::JmpIf { cond: v, off: 0 }));
                }
            }
            HPatKind::Unit => {}
            HPatKind::Tuple(ps) => {
                for (i, sp) in ps.iter().enumerate() {
                    if matches!(sp.kind, HPatKind::Wildcard) {
                        continue;
                    }
                    let t = self.field_temp(f, sp, v, i as u16);
                    self.match_pattern(f, sp, t, fails);
                }
            }
            HPatKind::Struct { fields } => {
                for (i, sp) in fields {
                    if matches!(sp.kind, HPatKind::Wildcard) {
                        continue;
                    }
                    let idx = self.field_u16(f, *i);
                    let t = self.field_temp(f, sp, v, idx);
                    self.match_pattern(f, sp, t, fails);
                }
            }
            HPatKind::Variant { rt_id, args }
                if *rt_id == self.prog.ids.some && !matches!(args[..], [HPat { kind: HPatKind::Wildcard, .. }]) =>
            {
                let sp = &args[0];
                let t = match sp.kind {
                    HPatKind::Bind(l) if !self.is_cell(f, l) && f.reg(l).is_some() => f.reg(l).unwrap(),
                    _ => self.alloc(f),
                };
                fails.push(self.emit(f, Op::JNotSome { src: v, dst: t, off: 0 }));
                self.match_pattern(f, sp, t, fails);
            }
            HPatKind::Variant { rt_id, args } => {
                let var = self.variant_u16(f, *rt_id);
                fails.extend(self.jump_not_variant(f, v, var));
                self.match_variant_args(f, args, v, fails);
            }
            HPatKind::List { before, rest, after } => {
                if before.len() > 255 || after.len() > 255 {
                    self.errors.push(Diag::error(p.span, "list pattern too long"));
                    return;
                }
                let n = (before.len() + after.len()) as u16;
                if rest.is_some() {
                    fails.push(self.emit(f, Op::JListMinLen { src: v, n, off: 0 }));
                } else {
                    fails.push(self.emit(f, Op::JListLen { src: v, n, off: 0 }));
                }
                for (i, sp) in before.iter().enumerate() {
                    if matches!(sp.kind, HPatKind::Wildcard) {
                        continue;
                    }
                    let t = self.alloc(f);
                    self.emit(f, Op::ListElem { dst: t, list: v, idx: i as i16 });
                    self.match_pattern(f, sp, t, fails);
                }
                for (i, sp) in after.iter().enumerate() {
                    if matches!(sp.kind, HPatKind::Wildcard) {
                        continue;
                    }
                    let t = self.alloc(f);
                    let idx = -((after.len() - i) as i16);
                    self.emit(f, Op::ListElem { dst: t, list: v, idx });
                    self.match_pattern(f, sp, t, fails);
                }
                if let Some(Some(l)) = rest {
                    let t = self.alloc(f);
                    self.emit(f, Op::ListRest { dst: t, list: v, before: before.len() as u8, after: after.len() as u8 });
                    self.init_local(f, *l, t);
                }
            }
            HPatKind::Or(alts) => {
                let mut oks = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    let mut alt_fails = Vec::new();
                    self.match_pattern(f, alt, v, &mut alt_fails);
                    if i + 1 < alts.len() {
                        oks.push(self.emit_jump(f));
                        self.patch_list(f, alt_fails);
                    } else {
                        fails.extend(alt_fails);
                    }
                }
                self.patch_list(f, oks);
            }
        }
    }

    /// A branch taken unless `v op k` (integers).
    fn jump_unless(&mut self, f: &mut FnState<'a>, op: CmpOp, v: Reg, k: i64) -> usize {
        if let Ok(k) = i16::try_from(k) {
            return self.emit(f, jump_unless_k(op, v, k));
        }
        let t = self.alloc(f);
        self.load_int(f, t, k);
        self.emit(
            f,
            match op {
                CmpOp::Ge => Op::JNleI { a: t, b: v, off: 0 },
                CmpOp::Le => Op::JNleI { a: v, b: t, off: 0 },
                _ => Op::JNeqI { a: v, b: t, off: 0 },
            },
        )
    }

    fn match_variant_args(&mut self, f: &mut FnState<'a>, args: &'a [HPat], v: Reg, fails: &mut Vec<usize>) {
        if let [sp] = args {
            // One payload: inline or not, `Payload` reads it.
            if !matches!(sp.kind, HPatKind::Wildcard) {
                let t = match sp.kind {
                    HPatKind::Bind(l) if !self.is_cell(f, l) && f.reg(l).is_some() => f.reg(l).unwrap(),
                    _ => self.alloc(f),
                };
                self.emit(f, Op::Payload { dst: t, src: v });
                self.match_pattern(f, sp, t, fails);
            }
            return;
        }
        for (i, sp) in args.iter().enumerate() {
            if matches!(sp.kind, HPatKind::Wildcard) {
                continue;
            }
            let t = self.field_temp(f, sp, v, i as u16);
            self.match_pattern(f, sp, t, fails);
        }
    }

    /// The scrutinee of a pattern match: a tuple literal is not built, its
    /// elements are matched where they are (a virtual tuple).
    fn scrutinee(&mut self, f: &mut FnState<'a>, e: &'a HExpr) -> Scrut {
        let HKind::Tuple(items) = &e.kind else { return Scrut::Reg(self.expr_reg(f, e)) };
        let regs = items
            .iter()
            .map(|x| match x.kind {
                // Arms run after all of the scrutinee is evaluated, and a
                // guard may assign a local: those are copied.
                HKind::Local(l) if !f.facts.assigned[l as usize] => self.expr_reg(f, x),
                _ => {
                    let r = self.alloc(f);
                    self.expr_to(f, x, r);
                    r
                }
            })
            .collect();
        Scrut::Tuple(regs)
    }

    /// `match_pattern` against a scrutinee.
    fn match_scrut(&mut self, f: &mut FnState<'a>, p: &'a HPat, s: &Scrut, fails: &mut Vec<usize>) {
        let regs = match s {
            Scrut::Reg(r) => return self.match_pattern(f, p, *r, fails),
            Scrut::Tuple(regs) => regs,
        };
        match &p.kind {
            HPatKind::Wildcard => {}
            HPatKind::Tuple(ps) => {
                for (sp, &r) in ps.iter().zip(regs) {
                    self.match_pattern(f, sp, r, fails);
                }
            }
            HPatKind::Or(alts) => {
                let mut oks = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    let mut alt_fails = Vec::new();
                    self.match_scrut(f, alt, s, &mut alt_fails);
                    if i + 1 < alts.len() {
                        oks.push(self.emit_jump(f));
                        self.patch_list(f, alt_fails);
                    } else {
                        fails.extend(alt_fails);
                    }
                }
                self.patch_list(f, oks);
            }
            // The tuple itself is needed (`t => ...`).
            _ => {
                let start = self.alloc_n(f, regs.len());
                for (i, &r) in regs.iter().enumerate() {
                    self.mv(f, start + i as Reg, r);
                }
                let t = self.alloc(f);
                self.emit(f, Op::NewTuple { dst: t, start, count: regs.len() as u16 });
                self.match_pattern(f, p, t, fails);
            }
        }
    }

    /// Whether a `match` on a value of type `ty` should start with a jump
    /// table: when it tests at least three variants or integers (not too
    /// sparse ones) and every arm starts with such a test or none.
    fn switch_plan(&self, ty: TypeId, arms: &'a [HArm], first: bool) -> Option<SwitchPlan> {
        // With `first`, the scrutinee is a tuple literal, switched on its
        // first element.
        let heads: Vec<Head> = arms.iter().map(|a| if first { first_head(&a.pat) } else { head(&a.pat) }).collect();
        let mut values: Vec<i64> = Vec::new();
        for h in &heads {
            match h {
                Head::Any => {}
                Head::Values(vs) => values.extend(vs),
                Head::Other => return None,
            }
        }
        values.sort_unstable();
        values.dedup();
        if values.len() < 3 {
            return None;
        }
        let (kind, lo, len) = match self.arena.get(ty) {
            t @ (Ty::Int | Ty::Str) => {
                let span = values.last()?.checked_sub(values[0])?.checked_add(1)?;
                // Bytes take at most 256 entries, however sparse.
                if matches!(t, Ty::Int) && (span > 4 * values.len() as i64 + 8 || span > 4096) {
                    return None;
                }
                (if matches!(t, Ty::Int) { SwitchKind::Int } else { SwitchKind::Char }, values[0], span as usize)
            }
            Ty::Named(d, _) if self.cp.types[d as usize].is_enum() => {
                let ids = self.cp.types[d as usize].variants().iter().map(|v| v.rt_id as i64);
                let (lo, hi) = (ids.clone().min()?, ids.max()?);
                (SwitchKind::Variant, lo, (hi - lo + 1) as usize)
            }
            _ => return None,
        };
        arms.iter()
            .zip(&heads)
            .all(|(a, h)| !matches!(h, Head::Values(_)) || if first { first_is(&a.pat, kind) } else { head_is(&a.pat, kind) })
            .then_some(SwitchPlan { kind, first: lo, len, heads })
    }

    /// Compiles `match`: into `dst`, or (`None`) returning each arm's value.
    ///
    /// The arms are tested in order; a jump table (`switch_plan`) first
    /// sends the scrutinee to the first arm that can match it, past that
    /// arm's test of the variant or integer. The last arm, unguarded, is
    /// not tested: the checker proved the match exhaustive.
    fn compile_match(&mut self, f: &mut FnState<'a>, scrut: &'a HExpr, arms: &'a [HArm], dst: Option<Reg>) {
        let mark = f.next_reg;
        let s = self.scrutinee(f, scrut);
        let plan = match (&s, &scrut.kind) {
            (Scrut::Reg(r), _) => self.switch_plan(scrut.ty, arms, false).map(|p| (p, *r)),
            (Scrut::Tuple(regs), HKind::Tuple(items)) => self.switch_plan(items[0].ty, arms, true).map(|p| (p, regs[0])),
            _ => None,
        };
        let switch = plan.as_ref().map(|(p, r)| {
            let table = self.prog.switches.len() as u32;
            self.prog.switches.push(Switch { first: p.first, ..Default::default() });
            let src = *r;
            let op = match p.kind {
                SwitchKind::Int => Op::SwitchInt { src, table },
                SwitchKind::Variant => Op::SwitchVariant { src, table },
                SwitchKind::Char => Op::SwitchChar { src, table },
            };
            self.emit(f, op);
            table
        });
        let mut entries = Vec::with_capacity(arms.len());
        let mut ends = Vec::new();
        let mut unmatched = Vec::new();
        for (i, arm) in arms.iter().enumerate() {
            let amark = f.next_reg;
            let start = self.here(f);
            self.declare_pattern_locals(f, &arm.pat);
            let mut fails = Vec::new();
            let last = i + 1 == arms.len() && arm.guard.is_none();
            let entry = match (&s, &arm.pat.kind) {
                // Test the variant or integer (unless exhaustiveness proves
                // it), then the rest: an entry point past the test.
                (Scrut::Reg(r), HPatKind::Variant { rt_id, args }) if *rt_id != self.prog.ids.some || last => {
                    if !last {
                        let var = self.variant_u16(f, *rt_id);
                        fails.extend(self.jump_not_variant(f, *r, var));
                    }
                    let entry = self.here(f);
                    self.match_variant_args(f, args, *r, &mut fails);
                    entry
                }
                (Scrut::Reg(r), HPatKind::Int(k)) => {
                    if !last {
                        fails.push(self.jump_unless(f, CmpOp::Eq, *r, *k));
                    }
                    self.here(f)
                }
                (Scrut::Reg(r), HPatKind::Str(_)) if switch.is_some() => {
                    if !last {
                        self.match_pattern(f, &arm.pat, *r, &mut fails);
                    }
                    self.here(f)
                }
                _ => {
                    self.match_scrut(f, &arm.pat, &s, &mut fails);
                    start
                }
            };
            entries.push(entry);
            if let Some(g) = &arm.guard {
                fails.extend(self.cond_jump(f, g));
            }
            match dst {
                Some(dst) => {
                    self.expr_to(f, &arm.body, dst);
                    ends.push(self.emit_jump(f));
                }
                None => self.ret_expr(f, &arm.body),
            }
            if i + 1 < arms.len() {
                self.patch_list(f, fails);
            } else {
                unmatched = fails;
            }
            f.next_reg = amark;
        }
        let nowhere = self.here(f);
        if !unmatched.is_empty() || arms.is_empty() || switch.is_some() {
            self.patch_list(f, unmatched);
            let k = self.const_idx(Const::Str("internal error: no match arm matched".into()));
            self.emit(f, Op::Panic { msg: k });
        }
        if let (Some(table), Some((plan, _))) = (switch, plan) {
            // Each value goes to the first arm that can match it, past the
            // test of an arm testing just this value (`entries`).
            let target = |v: Option<i64>| {
                let arm = plan.heads.iter().position(|h| match (h, v) {
                    (Head::Any, _) => true,
                    (Head::Values(vs), Some(v)) => vs.contains(&v),
                    _ => false,
                });
                arm.map_or(nowhere, |i| entries[i])
            };
            let mut targets: Vec<usize> = (0..plan.len).map(|i| target(Some(plan.first + i as i64))).collect();
            targets.push(target(None));
            f.switches.push((table, targets));
        }
        self.patch_list(f, ends);
        f.next_reg = mark;
    }

    /// Loads field `idx` of `v` into a register suitable for matching `sp`.
    fn field_temp(&mut self, f: &mut FnState<'a>, sp: &'a HPat, v: Reg, idx: u16) -> Reg {
        if let HPatKind::Bind(l) = sp.kind
            && let (Some(r), false) = (f.reg(l), self.is_cell(f, l))
        {
            self.emit(f, Op::GetField { dst: r, obj: v, idx });
            return r;
        }
        let t = self.alloc(f);
        self.emit(f, Op::GetField { dst: t, obj: v, idx });
        t
    }

    fn compile_let(&mut self, f: &mut FnState<'a>, pat: &'a HPat, init: &'a HExpr, else_: Option<&'a HExpr>) {
        if let HPatKind::Bind(l) = pat.kind
            && else_.is_none()
        {
            if f.facts.alias[l as usize]
                && let HKind::Local(x) = init.kind
                && let Some(r) = f.reg(x)
            {
                f.set_reg(l, r);
                return;
            }
            let r = self.declare_local(f, l);
            if self.is_cell(f, l) {
                let mark = f.next_reg;
                let t = self.expr_reg(f, init);
                self.emit(f, Op::NewCell { dst: r, src: t });
                f.next_reg = mark;
            } else {
                self.expr_to(f, init, r);
            }
            return;
        }
        self.declare_pattern_locals(f, pat);
        let mark = f.next_reg;
        let v = self.scrutinee(f, init);
        let mut fails = Vec::new();
        self.match_scrut(f, pat, &v, &mut fails);
        match else_ {
            Some(els) => {
                let jok = self.emit_jump(f);
                self.patch_list(f, fails);
                self.discard(f, els);
                self.patch_here(f, jok);
            }
            None => {
                if !fails.is_empty() {
                    let jok = self.emit_jump(f);
                    self.patch_list(f, fails);
                    let k = self.const_idx(Const::Str("internal error: refutable let".into()));
                    self.emit(f, Op::Panic { msg: k });
                    self.patch_here(f, jok);
                }
            }
        }
        f.next_reg = mark;
    }

    // ------------------------------------------------------------------
    // Lambdas

    fn compile_lambda(&mut self, f: &mut FnState<'a>, lam: &'a HLambda) -> u32 {
        // Free locals: referenced inside but declared outside the lambda.
        let facts = f.facts.clone();
        let free: &[LocalId] = facts.free.get(&lam.id).map_or(&[], |v| v);
        if free.len() > u16::MAX as usize {
            self.errors.push(Diag::error(lam.body.span, "a closure captures too many variables"));
        }
        // Declared in code not yet reached (e.g. a sibling local fn): unit.
        let captures: Vec<Reg> = free.iter().map(|&l| f.reg(l).unwrap_or(NO_REG)).collect();
        let mut sub = FnState {
            code: Vec::new(),
            lines: Vec::new(),
            far: Vec::new(),
            locals: f.locals,
            local_reg: vec![NONE; f.locals.len()],
            reg_log: Vec::new(),
            next_reg: 0,
            max_reg: 0,
            loops: vec![],
            line: f.line,
            file: f.file,
            too_big: false,
            hoisted: FxHashMap::default(),
            hoisting: false,
            time: f.time,
            defers: Vec::new(),
            pads: Vec::new(),
            pending: None,
            pending_from: 0,
            cleanups: Vec::new(),
            defer_ops: 0,
            void: None,
            facts: f.facts.clone(),
            stubs: Vec::new(),
            switches: Vec::new(),
            self_local: lam.self_local,
            site: 0,
            sites: Vec::new(),
        };
        // The parameters, then the captures (see `Proto::captures`).
        for &l in lam.params.iter().chain(free) {
            let r = self.alloc(&mut sub);
            sub.set_reg(l, r);
        }
        self.ret_expr(&mut sub, &lam.body);
        f.too_big |= sub.too_big;
        let p = self.prog.protos.len() as u32;
        let nregs = sub.max_reg.max(1) as u16;
        let Finished { code, lines, cleanups, sites, inlined } = self.finish(sub);
        self.prog.protos.push(Proto {
            name: lam.name.clone(),
            file: f.file,
            nparams: lam.params.len() as u16,
            nregs,
            code,
            lines,
            captures,
            cleanups,
            sites,
            inlined,
        });
        p
    }
}

/// Whether a block used as a condition can be compiled as its statements
/// followed by a branch on its last expression (an inlined filter lambda,
/// `{ let x = e; x > 2 }`): it defers nothing.
fn cond_block(items: &[HExpr]) -> bool {
    !items.is_empty() && !items.iter().any(|x| matches!(x.kind, HKind::Defer(_)))
}

fn defers_pending(f: &FnState) -> bool {
    f.defers.iter().any(|d| !d.is_empty())
}

/// The operands of `l op r` as `x op k` when one is an integer literal that
/// can be an immediate.
fn imm_operands<'e>(op: ArithOp, l: &'e HExpr, r: &'e HExpr) -> Option<(&'e HExpr, i64)> {
    match (int_lit(l), int_lit(r)) {
        (_, Some(k)) => Some((l, k)),
        (Some(k), None) if matches!(op, ArithOp::Add | ArithOp::Mul) => Some((r, k)),
        _ => None,
    }
}

/// The instruction for `a op k` with an immediate, if there is one for `op`
/// and `k` (division and remainder by 0 panic, through the general ones).
fn imm_op(op: ArithOp, k: i64) -> Option<impl Fn(Reg, Reg) -> Op> {
    let k = match op {
        ArithOp::Sub => k.checked_neg()?,
        ArithOp::Div | ArithOp::Rem if k == 0 => return None,
        _ => k,
    };
    let k = i16::try_from(k).ok()?;
    let make: fn(Reg, Reg, i16) -> Op = match op {
        ArithOp::Add | ArithOp::Sub => |dst, a, k| Op::AddIK { dst, a, k },
        ArithOp::Mul => |dst, a, k| Op::MulIK { dst, a, k },
        ArithOp::Div => |dst, a, k| Op::DivIK { dst, a, k },
        ArithOp::Rem => |dst, a, k| Op::RemIK { dst, a, k },
        _ => return None,
    };
    Some(move |dst, a| make(dst, a, k))
}

fn arith_op(op: ArithOp, lt: TypeId, rt: TypeId, dst: Reg, a: Reg, b: Reg) -> Op {
    let int = matches!(lt, TypeId::INT) && matches!(rt, TypeId::INT);
    let float = matches!(lt, TypeId::FLOAT) && matches!(rt, TypeId::FLOAT);
    use ArithOp::*;
    match op {
        Add if int => Op::AddI { dst, a, b },
        Sub if int => Op::SubI { dst, a, b },
        Mul if int => Op::MulI { dst, a, b },
        Div if int => Op::DivI { dst, a, b },
        Rem if int => Op::RemI { dst, a, b },
        Pow if int => Op::PowI { dst, a, b },
        Add if float => Op::AddF { dst, a, b },
        Sub if float => Op::SubF { dst, a, b },
        Mul if float => Op::MulF { dst, a, b },
        Div if float => Op::DivF { dst, a, b },
        Rem if float => Op::RemF { dst, a, b },
        Pow if float => Op::PowF { dst, a, b },
        Add => Op::Add { dst, a, b },
        Sub => Op::Sub { dst, a, b },
        Mul => Op::Mul { dst, a, b },
        Div => Op::Div { dst, a, b },
        Rem => Op::Rem { dst, a, b },
        Pow => Op::Pow { dst, a, b },
        BitAnd => Op::BitAnd { dst, a, b },
        BitOr => Op::BitOr { dst, a, b },
        BitXor => Op::BitXor { dst, a, b },
        Shl => Op::Shl { dst, a, b },
        Shr => Op::Shr { dst, a, b },
    }
}

/// `a op k` as a bool for integers.
fn cmp_k(op: CmpOp, dst: Reg, a: Reg, k: i16) -> Op {
    match op {
        CmpOp::Eq => Op::EqIK { dst, a, k },
        CmpOp::Ne => Op::NeIK { dst, a, k },
        CmpOp::Lt => Op::LtIK { dst, a, k },
        CmpOp::Le => Op::LeIK { dst, a, k },
        CmpOp::Gt => Op::GtIK { dst, a, k },
        CmpOp::Ge => Op::GeIK { dst, a, k },
    }
}

fn cmp_op(op: CmpOp, ty: TypeId, dst: Reg, a: Reg, b: Reg) -> Op {
    match ty {
        TypeId::INT => match op {
            CmpOp::Eq => Op::EqI { dst, a, b },
            CmpOp::Ne => Op::NeI { dst, a, b },
            CmpOp::Lt => Op::LtI { dst, a, b },
            CmpOp::Le => Op::LeI { dst, a, b },
            CmpOp::Gt => Op::LtI { dst, a: b, b: a },
            CmpOp::Ge => Op::LeI { dst, a: b, b: a },
        },
        TypeId::FLOAT => match op {
            CmpOp::Eq => Op::EqF { dst, a, b },
            CmpOp::Ne => Op::NeF { dst, a, b },
            CmpOp::Lt => Op::LtF { dst, a, b },
            CmpOp::Le => Op::LeF { dst, a, b },
            CmpOp::Gt => Op::LtF { dst, a: b, b: a },
            CmpOp::Ge => Op::LeF { dst, a: b, b: a },
        },
        _ => match op {
            CmpOp::Eq => Op::Eq { dst, a, b },
            CmpOp::Ne => Op::Ne { dst, a, b },
            CmpOp::Lt => Op::Lt { dst, a, b },
            CmpOp::Le => Op::Le { dst, a, b },
            CmpOp::Gt => Op::Lt { dst, a: b, b: a },
            CmpOp::Ge => Op::Le { dst, a: b, b: a },
        },
    }
}

/// Appends the locals `x` itself refers to (not its sub-expressions).
fn local_refs(x: &HExpr, out: &mut Vec<LocalId>) {
    match &x.kind {
        HKind::Local(l) => out.push(*l),
        HKind::Assign { place: Place::Local(l), .. } => out.push(*l),
        HKind::PushLocal { list, .. } => out.push(*list),
        HKind::InsertLocal { map, .. } => out.push(*map),
        HKind::LocalFns { locals, .. } => out.extend(locals.iter().copied()),
        _ => {}
    }
}

/// The farthest (in instructions) `assemble` places a branch island from its branch.
const ISLAND_REACH: usize = 30000;

/// The final assembly of a function's code: threads jumps (`thread_jumps`),
/// then routes each branch in `far` (instruction, target), whose target is
/// out of the range of its offset field, through a branch island: a `Jmp`
/// (or, backward, a `Loop`) to the target. An island goes into a gap
/// between the branch and its target that no code falls through to (after
/// an unconditional instruction) when there is one within reach, so it
/// costs nothing on other paths; otherwise right after the branch, behind
/// a jump over it.
fn assemble(mut code: Vec<Op>, mut lines: Vec<u32>, far: &[(usize, usize)], tables: &mut [Vec<usize>]) -> (Vec<Op>, Vec<u32>) {
    thread_jumps(&mut code);
    let mut far = far.to_vec();
    drop_null_jumps(&mut code, &mut lines, &mut far, tables);
    let far = &far[..];
    let n = code.len();
    // The target of every branch, by instruction.
    let mut targets: Vec<Option<usize>> =
        code.iter().enumerate().map(|(i, op)| op.branch_off().map(|o| (i as i64 + 1 + o as i64) as usize)).collect();
    for &(at, t) in far {
        targets[at] = Some(t);
    }
    // The branches needing islands, with whether their island must follow
    // them: those out of range, and conditional branches back without a
    // backward form (which must reach a safe point: a `Loop` island).
    let mut needs: Vec<(usize, bool)> = far.iter().map(|&(at, _)| (at, false)).collect();
    for (i, op) in code.iter().enumerate() {
        if targets[i].is_some_and(|t| t <= i) && !op.is_back_edge() && !far.iter().any(|&(at, _)| at == i) {
            needs.push((i, true));
        }
    }
    if needs.is_empty() {
        debug_assert!(check_back_edges(&code));
        return (code, lines);
    }
    // Positions (before an instruction, or `n`) no code falls through to.
    let gaps: Vec<usize> = (1..=n).filter(|&p| code[p - 1].is_unconditional()).collect();
    loop {
        // (position, index in `needs`), in code order.
        let mut islands: Vec<(usize, usize)> = needs
            .iter()
            .enumerate()
            .map(|(k, &(at, inline))| {
                let t = targets[at].unwrap();
                let gap = gaps.get(gaps.partition_point(|&p| p <= at)).copied().filter(|&p| !inline && p <= t && p - at <= ISLAND_REACH);
                (gap.unwrap_or(at + 1), k)
            })
            .collect();
        islands.sort_unstable();
        let mut out = Vec::with_capacity(n + 2 * islands.len());
        let mut out_lines = Vec::with_capacity(n + 2 * islands.len());
        let mut new_idx = vec![0; n + 1];
        let mut island_at = vec![0; needs.len()];
        // Jumps over inline islands, with the position they skip to.
        let mut skips = Vec::new();
        let mut next = islands.iter().peekable();
        for i in 0..=n {
            while let Some(&&(pos, k)) = next.peek()
                && pos == i
            {
                let (at, _) = needs[k];
                if gaps.binary_search(&pos).is_err() {
                    skips.push((out.len(), pos));
                    out.push(Op::Jmp { off: 0 });
                    out_lines.push(lines[at]);
                }
                island_at[k] = out.len();
                out.push(Op::Jmp { off: 0 });
                out_lines.push(lines[at]);
                next.next();
            }
            new_idx[i] = out.len();
            if i < n {
                out.push(code[i]);
                out_lines.push(lines[i]);
            }
        }
        for (j, pos) in skips {
            out[j] = Op::Jmp { off: (new_idx[pos] - j - 1) as i32 };
        }
        for (k, &(at, _)) in needs.iter().enumerate() {
            let (j, t) = (island_at[k], new_idx[targets[at].unwrap()]);
            let off = t as i64 - j as i64 - 1;
            out[j] = if off < 0 { Op::Loop { off: off as i32 } } else { Op::Jmp { off: off as i32 } };
        }
        // Re-target every branch; one that no longer fits (islands were
        // inserted in between) gets an island too, as does one too far from
        // its island in a gap.
        let mut need_of = vec![None; n];
        for (k, &(at, _)) in needs.iter().enumerate() {
            need_of[at] = Some(k);
        }
        let mut retry = false;
        for i in 0..n {
            let Some(t) = targets[i] else { continue };
            let k = need_of[i];
            let to = match k {
                Some(k) => island_at[k],
                None => new_idx[t],
            };
            let from = new_idx[i];
            if !out[from].set_branch_off(to as i64 - from as i64 - 1) {
                match k {
                    Some(k) => needs[k].1 = true,
                    None => needs.push((i, false)),
                }
                retry = true;
            }
        }
        if !retry {
            for t in tables.iter_mut().flatten() {
                *t = new_idx[*t];
            }
            debug_assert!(check_back_edges(&out));
            return (out, out_lines);
        }
    }
}

/// Jump threading: a `Jmp` to a `Jmp` goes to the final target, one to a
/// `Loop` becomes that `Loop` (or a `Jmp` to its target), one to a return
/// becomes the return, and a `Jmp` back becomes a `Loop` (a back edge).
/// Removes the jumps to the next instruction, moving every branch target,
/// far branch and table entry (positions) accordingly.
fn drop_null_jumps(code: &mut Vec<Op>, lines: &mut Vec<u32>, far: &mut [(usize, usize)], tables: &mut [Vec<usize>]) {
    let null = |op: &Op| matches!(op, Op::Jmp { off: 0 });
    if !code.iter().any(null) {
        return;
    }
    let n = code.len();
    // Each position's new position: removed jumps go where they jumped.
    let mut new_idx = Vec::with_capacity(n + 1);
    let mut k = 0;
    for op in code.iter() {
        new_idx.push(k);
        k += usize::from(!null(op));
    }
    new_idx.push(k);
    let mut out = Vec::with_capacity(k);
    let mut out_lines = Vec::with_capacity(k);
    for (i, &op) in code.iter().enumerate() {
        if null(&op) {
            continue;
        }
        let mut op = op;
        if let Some(off) = op.branch_off()
            && !far.iter().any(|&(at, _)| at == i)
        {
            let t = new_idx[(i as i64 + 1 + off as i64) as usize] as i64;
            // Branches only get shorter, so the offset fits.
            op.set_branch_off(t - out.len() as i64 - 1);
        }
        out.push(op);
        out_lines.push(lines[i]);
    }
    for (at, t) in far.iter_mut() {
        (*at, *t) = (new_idx[*at], new_idx[*t]);
    }
    for table in tables.iter_mut() {
        table.iter_mut().for_each(|p| *p = new_idx[*p]);
    }
    *code = out;
    *lines = out_lines;
}

fn thread_jumps(code: &mut [Op]) {
    for i in 0..code.len() {
        let Op::Jmp { off } = code[i] else { continue };
        let mut t = i as i64 + 1 + off as i64;
        // Follow chains of jumps (a few, in case of cycles).
        for _ in 0..8 {
            match code.get(t as usize) {
                Some(Op::Jmp { off }) => t = t + 1 + *off as i64,
                _ => break,
            }
        }
        code[i] = match code.get(t as usize) {
            Some(&Op::Loop { off }) if t + 1 + off as i64 <= i as i64 => Op::Loop { off: (t + 1 + off as i64 - (i as i64 + 1)) as i32 },
            Some(&Op::Loop { off }) => Op::Jmp { off: (t + 1 + off as i64 - (i as i64 + 1)) as i32 },
            Some(&op @ (Op::Ret { .. } | Op::RetVariant { .. } | Op::RetUnit)) => op,
            _ if t <= i as i64 => Op::Loop { off: (t - (i as i64 + 1)) as i32 },
            _ => Op::Jmp { off: (t - (i as i64 + 1)) as i32 },
        };
    }
}

/// Whether every branch back is a safe point charging fuel (see "Metering"
/// in docs/INTERNALS.md) and every `Loop` jumps back.
fn check_back_edges(code: &[Op]) -> bool {
    code.iter().all(|op| match op.branch_off() {
        Some(off) if off < 0 => op.is_back_edge(),
        Some(_) => !matches!(op, Op::Loop { .. }),
        None => true,
    })
}

/// Facts about the locals of a function body (indexed by `LocalId`).
struct LocalFacts {
    /// The string locals that `+=` appends to and no closure captures
    /// (cells): they are appended to in place. Such a local holds a private
    /// buffer (`Obj::StrBuf`) while it is being appended to; every other
    /// read of it first freezes the buffer into an ordinary string
    /// (`Op::FreezeStr`, in place), so no other register, object or closure
    /// ever sees a buffer, and appending cannot change a value seen elsewhere.
    builder: Vec<bool>,
    /// Locals that share the register of the local they are initialized
    /// from (`let v = x`, copy propagation): neither is a cell, `v` is
    /// never assigned, `x` is not assigned by the rest of the block (while
    /// `v` exists) and is not a string builder. Inlined lambdas bind their
    /// parameters this way.
    alias: Vec<bool>,
    /// Locals assigned somewhere (after their declaration).
    assigned: Vec<bool>,
    /// The free locals of each lambda (by `HLambda::id`): referenced in it
    /// but declared outside, in order of first reference.
    free: HashMap<u32, Vec<LocalId>>,
}

/// A lambda being visited by `lambda_free`: its id and the locals referenced in it.
struct FreeFrame {
    id: u32,
    refs: Vec<LocalId>,
    seen: HashSet<LocalId>,
}

/// Collects the free locals of the lambdas in `e` into `free` (one walk:
/// a lambda's free locals are those of its own references and of its
/// lambdas' free locals that it does not declare).
fn lambda_free(e: &HExpr, locals: &[LocalInfo], stack: &mut Vec<FreeFrame>, free: &mut HashMap<u32, Vec<LocalId>>, nodes: &mut u64) {
    walk_pruned(e, &mut |x| {
        *nodes += 1;
        if let HKind::Lambda(l) = &x.kind {
            stack.push(FreeFrame { id: l.id, refs: Vec::new(), seen: HashSet::new() });
            lambda_free(&l.body, locals, stack, free, nodes);
            let fr = stack.pop().unwrap();
            let list: Vec<LocalId> = fr.refs.into_iter().filter(|&r| locals.get(r as usize).is_some_and(|i| i.owner != fr.id)).collect();
            if let Some(parent) = stack.last_mut() {
                for &r in &list {
                    if parent.seen.insert(r) {
                        parent.refs.push(r);
                    }
                }
            }
            free.insert(l.id, list);
            return false;
        }
        if let Some(top) = stack.last_mut() {
            let mut refs = Vec::new();
            local_refs(x, &mut refs);
            for r in refs {
                if top.seen.insert(r) {
                    top.refs.push(r);
                }
            }
        }
        true
    });
}

fn assigns(e: &HExpr, l: LocalId, budget: &CompileBudget) -> bool {
    let mut found = false;
    let mut nodes = 0;
    walk(e, &mut |x| {
        nodes += 1;
        found |= matches!(x.kind, HKind::Assign { place: Place::Local(m), .. } if m == l);
    });
    budget.charge(nodes);
    found
}

/// Rest-of-block scans for `LocalFacts::alias` give up after this many nodes.
const MAX_ALIAS_SCAN: usize = 1000;

fn local_facts(body: &HExpr, locals: &[LocalInfo], budget: &CompileBudget) -> LocalFacts {
    let n = locals.len();
    let cell = |l: LocalId| locals.get(l as usize).is_none_or(|i| i.cell);
    let mut builder = vec![false; n];
    let mut assigned = vec![false; n];
    let mut nodes = 0u64;
    walk(body, &mut |e| {
        nodes += 1;
        if let HKind::Assign { place: Place::Local(l), op, op_ty, .. } = &e.kind {
            assigned[*l as usize] = true;
            if *op == Some(ArithOp::Add) && *op_ty == TypeId::STR && !cell(*l) {
                builder[*l as usize] = true;
            }
        }
    });
    let mut alias = vec![false; n];
    walk(body, &mut |e| {
        let HKind::Block(items) = &e.kind else { return };
        for (i, it) in items.iter().enumerate() {
            let HKind::Let { pat: HPat { kind: HPatKind::Bind(v), .. }, init, else_: None } = &it.kind else { continue };
            let HKind::Local(x) = init.kind else { continue };
            if cell(*v) || cell(x) || assigned[*v as usize] || builder[x as usize] {
                continue;
            }
            // Is `x` assigned by the rest of the block?
            let mut seen = 0;
            let mut clobbered = false;
            for later in &items[i + 1..] {
                walk(later, &mut |y| {
                    seen += 1;
                    clobbered |= matches!(y.kind, HKind::Assign { place: Place::Local(l), .. } if l == x);
                });
                if clobbered || seen > MAX_ALIAS_SCAN {
                    break;
                }
            }
            nodes += seen as u64;
            alias[*v as usize] = !clobbered && seen <= MAX_ALIAS_SCAN;
        }
    });
    let mut free = HashMap::new();
    lambda_free(body, locals, &mut Vec::new(), &mut free, &mut nodes);
    budget.charge(nodes);
    LocalFacts { builder, alias, assigned, free }
}

/// Pre-order traversal of every expression (including inside lambdas, places and args).
pub(crate) fn walk<'e>(e: &'e HExpr, f: &mut impl FnMut(&'e HExpr)) {
    walk_pruned(e, &mut |x| {
        f(x);
        true
    });
}

/// Like [`walk`], but skips the sub-expressions of the expressions for
/// which `f` returns false.
fn walk_pruned<'e>(e: &'e HExpr, f: &mut impl FnMut(&'e HExpr) -> bool) {
    if !f(e) {
        return;
    }
    match &e.kind {
        HKind::Let { init, else_, .. } => {
            walk_pruned(init, f);
            if let Some(x) = else_ {
                walk_pruned(x, f);
            }
        }
        HKind::LocalFns { lambdas, .. } => lambdas.iter().for_each(|x| walk_pruned(x, f)),
        HKind::Assign { place, value, .. } => {
            match place {
                Place::Field { obj, .. } => walk_pruned(obj, f),
                Place::Index { obj, index, .. } => {
                    walk_pruned(obj, f);
                    walk_pruned(index, f);
                }
                _ => {}
            }
            walk_pruned(value, f);
        }
        HKind::Block(xs)
        | HKind::Tuple(xs)
        | HKind::List(xs)
        | HKind::MakeVariant { args: xs, .. }
        | HKind::MakeStruct { fields: xs, .. } => xs.iter().for_each(|x| walk_pruned(x, f)),
        HKind::If { cond, then, else_ } => {
            walk_pruned(cond, f);
            walk_pruned(then, f);
            if let Some(x) = else_ {
                walk_pruned(x, f);
            }
        }
        HKind::IfLet { value, then, else_, .. } => {
            walk_pruned(value, f);
            walk_pruned(then, f);
            if let Some(x) = else_ {
                walk_pruned(x, f);
            }
        }
        HKind::Match { scrut, arms } => {
            walk_pruned(scrut, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_pruned(g, f);
                }
                walk_pruned(&a.body, f);
            }
        }
        HKind::Loop(b) => walk_pruned(b, f),
        HKind::While { cond, body } => {
            walk_pruned(cond, f);
            walk_pruned(body, f);
        }
        HKind::WhileLet { value, body, .. } => {
            walk_pruned(value, f);
            walk_pruned(body, f);
        }
        HKind::For { iter, body, .. } => {
            walk_pruned(iter, f);
            walk_pruned(body, f);
        }
        HKind::ForRange { start, end, body, .. } => {
            walk_pruned(start, f);
            walk_pruned(end, f);
            walk_pruned(body, f);
        }
        HKind::Break { value: x, .. } | HKind::Return(x) => {
            if let Some(x) = x {
                walk_pruned(x, f);
            }
        }
        HKind::Call { args, .. } | HKind::CallNative { args, .. } | HKind::CallTrait { args, .. } => {
            for a in args {
                if let HArg::Expr(x) = a {
                    walk_pruned(x, f);
                }
            }
        }
        HKind::Dict { args, .. } => args.iter().for_each(|x| walk_pruned(x, f)),
        HKind::CallValue { callee, args } => {
            walk_pruned(callee, f);
            args.iter().for_each(|x| walk_pruned(x, f));
        }
        HKind::Map(kvs) => {
            for (k, v) in kvs {
                walk_pruned(k, f);
                walk_pruned(v, f);
            }
        }
        HKind::Repeat { elem, count } => {
            walk_pruned(elem, f);
            walk_pruned(count, f);
        }
        HKind::PushLocal { value, .. } => walk_pruned(value, f),
        HKind::InsertLocal { key, value, .. } => {
            walk_pruned(key, f);
            walk_pruned(value, f);
        }
        HKind::Field { obj, .. } => walk_pruned(obj, f),
        HKind::Index { obj, index, .. } => {
            walk_pruned(obj, f);
            walk_pruned(index, f);
        }
        HKind::Slice { obj, start, end, .. } => {
            walk_pruned(obj, f);
            if let Some(x) = start {
                walk_pruned(x, f);
            }
            if let Some(x) = end {
                walk_pruned(x, f);
            }
        }
        HKind::Unary { e, .. } | HKind::ToStr(e) | HKind::Debug(e) | HKind::Spawn(e) | HKind::Defer(e) | HKind::Try { e, .. } => {
            walk_pruned(e, f)
        }
        HKind::Arith { l, r, .. } | HKind::Cmp { l, r, .. } | HKind::And(l, r) | HKind::Or(l, r) | HKind::Coalesce { l, r, .. } => {
            walk_pruned(l, f);
            walk_pruned(r, f);
        }
        HKind::Range { start, end, .. } => {
            walk_pruned(start, f);
            walk_pruned(end, f);
        }
        HKind::Lambda(l) => walk_pruned(&l.body, f),
        HKind::Inlined { body, .. } => walk_pruned(body, f),
        HKind::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    walk_pruned(x, f);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::bytecode::{Op, Program};
    use crate::{Config, Engine};
    use std::sync::Arc;

    fn compile(src: &str) -> Arc<Program> {
        let engine = Engine::new(Config::new());
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let root = std::env::current_dir().unwrap();
        match rt.block_on(engine.compile("main.sand", src, &root, None)) {
            Ok(c) => c.program,
            Err(e) => panic!("{e}"),
        }
    }

    /// The first loop (from its back edge's target to the back edge) of
    /// function `name`, and the code before it.
    fn first_loop<'p>(prog: &'p Program, name: &str) -> (&'p [Op], &'p [Op]) {
        let p = prog.protos.iter().find(|p| p.name == name).unwrap();
        let (at, off) =
            p.code.iter().enumerate().find_map(|(i, op)| op.is_back_edge().then(|| Some((i, op.branch_off()?))).flatten()).unwrap();
        let top = (at as i64 + 1 + off as i64) as usize;
        (&p.code[top..=at], &p.code[..top])
    }

    fn run(src: &str) -> String {
        let out = crate::MemorySink::new();
        let mut engine = Engine::new(Config::new().stdout(out.clone()));
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        if let Err(e) = rt.block_on(engine.run_source("main.sand", src)) {
            panic!("{e}");
        }
        out.contents()
    }

    #[test]
    fn branches_beyond_the_offset_range_go_through_islands() {
        // Bodies of about 40000 instructions: the loop exits, the `if` and
        // the match test jump farther than an `i16` offset reaches.
        let body = "            t += i % 3\n".repeat(20000);
        let src = format!(
            "fn big(n: int, o: int?) -> int {{\n    var t = 0\n    var i = 0\n    while i < n {{\n        if i % 2 == 0 {{\n{body}        }}\n        i += 1\n    }}\n    for i in 0..n {{\n        match o {{\n            Some(k) if k > i => {{\n{body}            }}\n            _ => {{ t += 1 }}\n        }}\n    }}\n    t\n}}\nprint(big(4, Some(3)))\nprint(big(3, None))\n"
        );
        assert_eq!(run(&src), "100001\n40003\n");
        let prog = compile(&src);
        let p = prog.protos.iter().find(|p| p.name == "big").unwrap();
        assert!(p.code.len() > 70000);
        // A jump table whose targets move when islands are inserted.
        let arm = "            t += i % 3\n".repeat(20000);
        let src = format!(
            "enum E {{ A, B(int), C, D }}\nfn f(e: E, i: int) -> int {{\n    var t = 0\n    match e {{\n        A => {{\n{arm}        }}\n        B(k) if k > 0 => {{\n{arm}        }}\n        C => {{ t = 7 }}\n        _ => {{ t = -1 }}\n    }}\n    t\n}}\nprint([E.A, E.B(1), E.B(0), E.C, E.D].map(e => f(e, 2)))\n"
        );
        assert_eq!(run(&src), "[40000, 40000, -1, 7, -1]\n");
    }

    fn loads(ops: &[Op]) -> usize {
        ops.iter().filter(|op| matches!(op, Op::LoadInt { .. } | Op::LoadConst { .. } | Op::LoadGlobal { .. } | Op::LoadTag { .. })).count()
    }

    #[test]
    fn loop_invariant_constants_and_immutable_globals_are_hoisted() {
        let prog = compile(
            "let K = 3\nlet S = \"ab\"\nfn f(n: int) -> int {\n    var t = 0\n    var i = 0\n    while i < 100000 {\n        t += K * i + i % 7 + S.len()\n        if t > 1000000 { t = t % 1000003 }\n        i += 1\n    }\n    t\n}\nprint(f(3))",
        );
        let (body, pre) = first_loop(&prog, "f");
        assert_eq!(loads(body), 0, "{body:#?}");
        assert_eq!(global_loads(pre), 2, "{pre:#?}");
    }

    fn global_loads(ops: &[Op]) -> usize {
        ops.iter().filter(|op| matches!(op, Op::LoadGlobal { .. })).count()
    }

    #[test]
    fn globals_are_hoisted_where_they_are_initialized() {
        // `f` first runs from the third statement, after `K` and before `L`.
        let src = "let K = 1\nfn f(n: int) -> int {\n    var t = 0\n    for i in 0..n {\n        print(i)\n        t += K + L\n    }\n    t\n}\nprint(f(0))\nlet L = 2\nprint(f(2))";
        let prog = compile(src);
        let (body, pre) = first_loop(&prog, "f");
        assert_eq!((global_loads(pre), global_loads(body)), (1, 1), "{pre:#?}\n{body:#?}");
        assert_eq!(run(src), "0\n0\n1\n6\n");
        // A top-level loop after the declaration (`K` is a global, as `g` reads it).
        let prog = compile("let K = 3\nfn g() -> int { K }\nvar t = 0\nfor i in 0..10 { t += K }\nprint(t)");
        let (body, pre) = first_loop(&prog, "<main>");
        assert_eq!((global_loads(pre), global_loads(body)), (1, 0), "{pre:#?}\n{body:#?}");
    }

    #[test]
    fn loops_with_lambdas_and_large_loops_hoist_globals() {
        let prog = compile(
            "let K = 2\nfn f(xs: [int]) -> int {\n    var t = 0\n    for x in xs {\n        let add = (y: int) => y + K\n        t += add(x) + K\n    }\n    t\n}\nprint(f([1, 2]))",
        );
        let (body, pre) = first_loop(&prog, "f");
        assert_eq!((global_loads(pre), global_loads(body)), (1, 0), "{pre:#?}\n{body:#?}");
        let big = "        t += i % 7 + K\n".repeat(100);
        let prog = compile(&format!(
            "let K = 2\nfn f(n: int) -> int {{\n    var t = 0\n    for i in 0..n {{\n{big}    }}\n    t\n}}\nprint(f(3))"
        ));
        let (body, pre) = first_loop(&prog, "f");
        assert_eq!((global_loads(pre), global_loads(body)), (1, 0));
    }

    #[test]
    fn mutable_globals_are_not_hoisted() {
        let prog = compile("var g = 1\nfn h(n: int) -> int {\n    var t = 0\n    for i in 0..n { t += g }\n    t\n}\nprint(h(3))");
        let (body, pre) = first_loop(&prog, "h");
        assert_eq!(body.iter().filter(|op| matches!(op, Op::LoadGlobal { .. })).count(), 1, "{body:#?}");
        assert_eq!(global_loads(pre), 0, "{pre:#?}");
    }
}
