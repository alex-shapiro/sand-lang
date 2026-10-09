//! Compiles checked HIR into register bytecode.

mod reflect;

use crate::check::budget::CompileBudget;
use crate::check::{CheckedProgram, FnKind};
use crate::diag::{Diag, SourceMap, Span};
use crate::hir::*;
use crate::types::{FnId, Type};
use crate::vm::bytecode::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub struct CompileInput<'a> {
    pub checked: &'a CheckedProgram,
    pub sm: &'a SourceMap,
    pub native_lookup: &'a dyn Fn(&str) -> Option<u32>,
    pub native_help: Vec<HelpEntry>,
    pub native_argc: Vec<u16>,
    /// Charged one unit per expression compiled, instruction emitted and
    /// node visited by an analysis. Once it is exhausted the compiler skips
    /// the rest of the code (callers check `budget.stopped()` and discard
    /// the result).
    pub budget: &'a CompileBudget,
}

struct Compiler<'a> {
    cp: &'a CheckedProgram,
    budget: &'a CompileBudget,
    sm: &'a SourceMap,
    prog: &'a mut Program,
    fn_protos: Vec<Option<u32>>,
    str_consts: HashMap<String, u32>,
    int_consts: HashMap<i64, u32>,
    fn_consts: HashMap<u32, u32>,
    native_consts: HashMap<u32, u32>,
    variant_wrappers: HashMap<u32, u32>,
    /// Constant trait dictionaries by their elements.
    dict_consts: HashMap<Vec<u32>, u32>,
    /// Shared type descriptions of `reflect` type arguments.
    descs: reflect::DescCache,
    errors: Vec<Diag>,
    recv_native: Option<u32>,
    /// Natives compiled to dedicated instructions.
    inline: HashMap<u32, Inline>,
}

#[derive(Clone, Copy)]
enum Inline {
    Len,
    Push,
    ToFloat,
    /// `Op::StrClass` with this class.
    StrClass(u8),
}

#[derive(Default)]
struct LoopCx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    result: Option<Reg>,
    /// The number of defer scopes outside the loop: `break` and `continue`
    /// run the deferred expressions of the scopes above.
    defer_depth: usize,
}

struct FnState<'h> {
    code: Vec<Op>,
    lines: Vec<u32>,
    locals: &'h [LocalInfo],
    local_reg: HashMap<LocalId, Reg>,
    capture_idx: HashMap<LocalId, u16>,
    next_reg: u32,
    max_reg: u32,
    loops: Vec<LoopCx>,
    line: u32,
    file: u32,
    no_fused: bool,
    too_big: bool,
    in_tail_fn: bool,
    /// Loop-invariant values loaded into registers before the enclosing
    /// outermost loop (see `hoisted_loop`).
    hoisted: HashMap<HoistKey, Reg>,
    /// Inside an outermost loop whose invariants were already hoisted.
    hoisting: bool,
    /// Deferred expressions of the enclosing blocks (innermost last), compiled
    /// again at every exit edge that leaves their block.
    defers: Vec<Vec<HExpr>>,
    /// Instructions emitted for copies of deferred code (see `MAX_DEFER_OPS`).
    defer_ops: usize,
    /// The register `discard` gives to an expression whose value is unused:
    /// unit results are not materialised in it (see `unit_to`).
    void: Option<Reg>,
    /// String locals appended to in place (see `builder_locals`).
    builders: Rc<HashSet<LocalId>>,
}

const MAX_REGS: u32 = 60000;
/// The most instructions a function may contain for copies of deferred code.
const MAX_DEFER_OPS: usize = 1 << 20;

/// A loop-invariant value that can be loaded once before a loop.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum HoistKey {
    Int(i64),
    Float(u64),
    Str(String),
    Bytes(Vec<u8>),
    Dur(i64),
    Tag(u32),
    /// An immutable global (hoisted with a guard, see `hoisted_loop`).
    Global(u32),
}

/// At most this many values are hoisted per loop nest.
const MAX_HOISTED: usize = 24;
/// Hoisting is skipped when the function already uses this many registers.
const HOIST_REG_BUDGET: u32 = 1000;
/// Loops are duplicated for global hoisting only up to this many HIR nodes.
const MAX_VERSIONED_NODES: usize = 400;

pub fn compile(input: CompileInput) -> Result<Program, Vec<Diag>> {
    let mut prog = Program::default();
    let inits = input.checked.module_inits.clone();
    prog.entry = compile_more(input, &mut prog, &mut Incremental::default(), &inits)?;
    Ok(prog)
}

/// What the compiler remembers between the compilations of a REPL session,
/// each of which appends to the same [`Program`].
#[derive(Clone, Default)]
pub(crate) struct Incremental {
    fn_protos: Vec<Option<u32>>,
    str_consts: HashMap<String, u32>,
    int_consts: HashMap<i64, u32>,
    fn_consts: HashMap<u32, u32>,
    native_consts: HashMap<u32, u32>,
    variant_wrappers: HashMap<u32, u32>,
    dict_consts: HashMap<Vec<u32>, u32>,
    descs: reflect::DescCache,
    /// Number of tests compiled.
    tests: usize,
}

/// The sizes of a program's tables, to undo a failed [`compile_more`].
pub(crate) struct ProgramMark {
    protos: usize,
    consts: usize,
    types: usize,
    variants: usize,
    specs: usize,
    tests: usize,
    nglobals: usize,
    ids: RtIds,
}

impl ProgramMark {
    pub(crate) fn of(p: &Program) -> ProgramMark {
        ProgramMark {
            protos: p.protos.len(),
            consts: p.consts.len(),
            types: p.types.len(),
            variants: p.variants.len(),
            specs: p.specs.len(),
            tests: p.tests.len(),
            nglobals: p.nglobals,
            ids: p.ids,
        }
    }

    /// Truncates `p` to the marked sizes. (Help for natives of modules that
    /// were being added may remain, which is harmless.)
    pub(crate) fn rollback(&self, p: &mut Program) {
        p.protos.truncate(self.protos);
        p.consts.truncate(self.consts);
        p.types.truncate(self.types);
        p.variants.truncate(self.variants);
        p.specs.truncate(self.specs);
        p.tests.truncate(self.tests);
        p.nglobals = self.nglobals;
        p.global_names.truncate(self.nglobals);
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
pub(crate) fn compile_more(input: CompileInput, prog: &mut Program, inc: &mut Incremental, inits: &[FnId]) -> Result<u32, Vec<Diag>> {
    let cp = input.checked;
    let mut c = Compiler {
        cp,
        budget: input.budget,
        sm: input.sm,
        prog,
        fn_protos: std::mem::take(&mut inc.fn_protos),
        str_consts: std::mem::take(&mut inc.str_consts),
        int_consts: std::mem::take(&mut inc.int_consts),
        fn_consts: std::mem::take(&mut inc.fn_consts),
        native_consts: std::mem::take(&mut inc.native_consts),
        variant_wrappers: std::mem::take(&mut inc.variant_wrappers),
        dict_consts: std::mem::take(&mut inc.dict_consts),
        descs: std::mem::take(&mut inc.descs),
        errors: Vec::new(),
        recv_native: (input.native_lookup)("Channel.recv"),
        inline: HashMap::new(),
    };
    c.descs.start();
    let entry = c.compile_new(input, inits, &mut inc.tests);
    inc.fn_protos = c.fn_protos;
    inc.str_consts = c.str_consts;
    inc.int_consts = c.int_consts;
    inc.fn_consts = c.fn_consts;
    inc.native_consts = c.native_consts;
    inc.variant_wrappers = c.variant_wrappers;
    inc.dict_consts = c.dict_consts;
    inc.descs = c.descs;
    if c.errors.is_empty() { Ok(entry) } else { Err(c.errors) }
}

impl Compiler<'_> {
    fn compile_new(&mut self, input: CompileInput, inits: &[FnId], ntests: &mut usize) -> u32 {
        let cp = self.cp;
        for (name, op) in [
            ("List.len", Inline::Len),
            ("str.len", Inline::Len),
            ("bytes.len", Inline::Len),
            ("Map.len", Inline::Len),
            ("Set.len", Inline::Len),
            ("List.push", Inline::Push),
            ("int.to_float", Inline::ToFloat),
            ("str.is_digit", Inline::StrClass(0)),
            ("str.is_alpha", Inline::StrClass(1)),
            ("str.is_alnum", Inline::StrClass(2)),
            ("str.is_space", Inline::StrClass(3)),
            ("str.is_upper", Inline::StrClass(4)),
            ("str.is_lower", Inline::StrClass(5)),
        ] {
            if let Some(id) = (input.native_lookup)(name) {
                self.inline.insert(id, op);
            }
        }
        // Help for natives is only given when (new) natives were checked.
        if !input.native_help.is_empty() {
            self.prog.native_help = input.native_help;
            self.prog.native_argc = input.native_argc;
        }
        self.prog.recv_native = self.recv_native;
        self.prog.files = input.sm.files.iter().map(|f| f.name.clone()).collect();
        self.prog.nglobals = cp.globals.len();
        let known = self.prog.global_names.len();
        self.prog.global_names.extend(cp.globals[known..].iter().map(|g| g.name.clone()));
        for t in &cp.types[self.prog.types.len()..] {
            self.prog.types.push(StructRt {
                name: t.name.clone(),
                module: t.module_name.clone(),
                fields: t.fields().iter().map(|f| f.name.clone()).collect(),
                is_enum: t.is_enum(),
            });
        }
        for (d, i) in &cp.variants[self.prog.variants.len()..] {
            let v = &cp.types[*d as usize].variants()[*i as usize];
            self.prog.variants.push(VariantRt {
                def: *d,
                index: *i,
                name: v.name.clone(),
                field_names: v.field_names.clone(),
                arity: v.fields.len(),
            });
        }
        let wk = cp.wk;
        let vid = |d: u32, name: &str| cp.types[d as usize].variants().iter().find(|v| v.name == name).map(|v| v.rt_id).unwrap_or(0);
        self.prog.ids = RtIds {
            some: vid(wk.option, "Some"),
            none: vid(wk.option, "None"),
            ok: vid(wk.result, "Ok"),
            err: vid(wk.result, "Err"),
            error_def: wk.error,
            json: None,
        };
        if let Some(jd) = cp.types.iter().position(|t| t.name == "Json" && t.module_name == "json") {
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
        self.fn_protos.resize(cp.fns.len(), None);
        for (i, f) in cp.fns.iter().enumerate().skip(first) {
            if !matches!(f.kind, FnKind::Native(_)) && cp.bodies[i].is_some() {
                let idx = self.prog.protos.len() as u32;
                self.prog.protos.push(Proto {
                    name: f.name.clone(),
                    file: 0,
                    nparams: 0,
                    nregs: 0,
                    code: vec![],
                    lines: vec![],
                    captures: vec![],
                    capture_cells: vec![],
                    doc: None,
                    signature: String::new(),
                });
                self.fn_protos[i] = Some(idx);
            }
        }
        for i in first..cp.fns.len() {
            if let (Some(p), Some(body)) = (self.fn_protos[i], &cp.bodies[i]) {
                self.compile_fn(i as FnId, p, body);
            }
        }
        // Entry: run the module initialisers in order.
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
            capture_cells: vec![],
            doc: None,
            signature: String::new(),
        });
        for (name, f) in &cp.tests[*ntests..] {
            if let Some(p) = self.fn_protos[*f as usize] {
                self.prog.tests.push((name.clone(), p));
            }
        }
        *ntests = cp.tests.len();
        // Summaries are only given when (new) modules were checked.
        if !cp.modules.is_empty() {
            self.prog.module_help = cp
                .modules
                .iter()
                .filter(|m| !matches!(m.kind, crate::check::ModKind::Script { .. }))
                .map(|m| (m.name.clone(), m.summary.clone()))
                .collect();
        }
        entry
    }
}

fn int_fits_i32(v: i64) -> bool {
    v >= i32::MIN as i64 && v <= i32::MAX as i64
}

impl<'a> Compiler<'a> {
    fn const_idx(&mut self, k: Const) -> u32 {
        match &k {
            Const::Str(s) => {
                if let Some(&i) = self.str_consts.get(s) {
                    return i;
                }
                let i = self.prog.consts.len() as u32;
                self.str_consts.insert(s.clone(), i);
                self.prog.consts.push(k);
                i
            }
            Const::Int(v) => {
                if let Some(&i) = self.int_consts.get(v) {
                    return i;
                }
                let i = self.prog.consts.len() as u32;
                self.int_consts.insert(*v, i);
                self.prog.consts.push(k);
                i
            }
            Const::Fn(p) => {
                if let Some(&i) = self.fn_consts.get(p) {
                    return i;
                }
                let i = self.prog.consts.len() as u32;
                self.fn_consts.insert(*p, i);
                self.prog.consts.push(k);
                i
            }
            Const::Native(n) => {
                if let Some(&i) = self.native_consts.get(n) {
                    return i;
                }
                let i = self.prog.consts.len() as u32;
                self.native_consts.insert(*n, i);
                self.prog.consts.push(k);
                i
            }
            _ => {
                self.prog.consts.push(k);
                (self.prog.consts.len() - 1) as u32
            }
        }
    }

    fn line_of(&self, span: Span) -> u32 {
        if (span.file as usize) < self.sm.files.len() { self.sm.file(span.file).line_col(span.lo).0 as u32 } else { 0 }
    }

    fn compile_fn(&mut self, fid: FnId, proto_idx: u32, body: &HFn) {
        let fi = &self.cp.fns[fid as usize];
        let signature = self.signature_of(fid);
        for no_fused in [false, true] {
            let mut f = FnState {
                code: Vec::new(),
                lines: Vec::new(),
                locals: &body.locals,
                local_reg: HashMap::new(),
                capture_idx: HashMap::new(),
                next_reg: 0,
                max_reg: 0,
                loops: vec![],
                line: self.line_of(body.span),
                file: body.span.file,
                no_fused,
                too_big: false,
                in_tail_fn: true,
                hoisted: HashMap::new(),
                hoisting: false,
                defers: Vec::new(),
                defer_ops: 0,
                void: None,
                builders: Rc::new(builder_locals(&body.body, &body.locals)),
            };
            for &p in &body.params {
                let r = self.alloc(&mut f);
                f.local_reg.insert(p, r);
            }
            self.ret_expr(&mut f, &body.body);
            if f.too_big && !no_fused {
                continue;
            }
            let nparams = body.params.len() as u16;
            thread_jumps(&mut f.code);
            let proto = Proto {
                name: fi.name.clone(),
                file: body.span.file,
                nparams,
                nregs: f.max_reg.max(1) as u16,
                code: f.code,
                lines: f.lines,
                captures: vec![],
                capture_cells: vec![],
                doc: fi.doc.clone(),
                signature: signature.clone(),
            };
            self.prog.protos[proto_idx as usize] = proto;
            return;
        }
    }

    fn signature_of(&self, fid: FnId) -> String {
        let fi = &self.cp.fns[fid as usize];
        let printer = crate::types::TypePrinter { defs: &self.cp.types, params: &self.cp.params, wk: self.cp.wk };
        let params: Vec<(String, Type, bool)> = fi.params.iter().map(|p| (p.name.clone(), p.ty.clone(), p.default.is_some())).collect();
        printer.signature(&fi.name, &params, &fi.ret)
    }

    // ------------------------------------------------------------------
    // Emission helpers

    fn emit(&mut self, f: &mut FnState, op: Op) -> usize {
        self.budget.charge(1);
        f.code.push(op);
        f.lines.push(f.line);
        f.code.len() - 1
    }

    fn alloc(&mut self, f: &mut FnState) -> Reg {
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

    fn alloc_n(&mut self, f: &mut FnState, n: usize) -> Reg {
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

    fn here(&self, f: &FnState) -> usize {
        f.code.len()
    }

    fn emit_jump(&mut self, f: &mut FnState) -> usize {
        self.emit(f, Op::Jmp { off: 0 })
    }

    /// Patches the jump at `at` to land on `target`.
    fn patch(&mut self, f: &mut FnState, at: usize, target: usize) {
        let off = target as i64 - (at as i64 + 1);
        let fits16 = off >= i16::MIN as i64 && off <= i16::MAX as i64;
        let op = &mut f.code[at];
        match op {
            Op::Jmp { off: o } | Op::Loop { off: o } | Op::JmpIf { off: o, .. } | Op::JmpIfNot { off: o, .. } => *o = off as i32,
            Op::JNltI { off: o, .. }
            | Op::JNleI { off: o, .. }
            | Op::JNeqI { off: o, .. }
            | Op::JNneI { off: o, .. }
            | Op::JNltF { off: o, .. }
            | Op::JNleF { off: o, .. }
            | Op::JNotVariant { off: o, .. }
            | Op::JNotSome { off: o, .. }
            | Op::JStrClass { off: o, .. }
            | Op::HoistGlobal { off: o, .. }
            | Op::JListLen { off: o, .. }
            | Op::JListMinLen { off: o, .. }
            | Op::ForList { off: o, .. }
            | Op::ForRange { off: o, .. }
            | Op::ForRangeIncl { off: o, .. } => {
                if fits16 {
                    *o = off as i16;
                } else {
                    f.too_big = true;
                }
            }
            _ => unreachable!("patching a non-jump"),
        }
    }

    fn patch_here(&mut self, f: &mut FnState, at: usize) {
        let t = self.here(f);
        self.patch(f, at, t);
    }

    fn emit_loop_back(&mut self, f: &mut FnState, target: usize) {
        let at = self.emit(f, Op::Loop { off: 0 });
        self.patch(f, at, target);
    }

    fn mv(&mut self, f: &mut FnState, dst: Reg, src: Reg) {
        if dst != src {
            self.emit(f, Op::Move { dst, src });
        }
    }

    /// Calls script function `proto` with the arguments at `base..`,
    /// writing the result to `dst`.
    fn call_direct(&mut self, f: &mut FnState, base: Reg, proto: u32, dst: Reg) {
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
    fn call_native(&mut self, f: &mut FnState, base: Reg, native: u32, dst: Reg) {
        match u16::try_from(native) {
            Ok(native) => {
                self.emit(f, Op::CallNativeTo { base, dst, native });
            }
            Err(_) => {
                self.emit(f, Op::CallNative { base, native });
                self.mv(f, dst, base);
            }
        }
    }

    fn load_int(&mut self, f: &mut FnState, dst: Reg, v: i64) {
        if int_fits_i32(v) {
            self.emit(f, Op::LoadInt { dst, v: v as i32 });
        } else {
            let k = self.const_idx(Const::Int(v));
            self.emit(f, Op::LoadConst { dst, k });
        }
    }

    fn load_str(&mut self, f: &mut FnState, dst: Reg, s: &str) {
        let k = self.const_idx(Const::Str(s.to_string()));
        self.emit(f, Op::LoadConst { dst, k });
    }

    fn variant_u16(&mut self, f: &FnState, rt: u32) -> u16 {
        if rt > u16::MAX as u32 {
            self.errors.push(Diag::error(Span::new(f.file, 0, 0), "too many enum variants in program"));
            return 0;
        }
        rt as u16
    }

    // ------------------------------------------------------------------
    // Locals

    fn is_cell(&self, f: &FnState, l: LocalId) -> bool {
        f.locals[l as usize].cell
    }

    /// Reads local `l` into `dst`.
    fn read_local(&mut self, f: &mut FnState, l: LocalId, dst: Reg) {
        if let Some(&r) = f.local_reg.get(&l) {
            if f.builders.contains(&l) {
                self.emit(f, Op::FreezeStr { reg: r });
            }
            if self.is_cell(f, l) {
                self.emit(f, Op::LoadCell { dst, cell: r });
            } else {
                self.mv(f, dst, r);
            }
        } else if let Some(&i) = f.capture_idx.get(&l) {
            if self.is_cell(f, l) {
                self.emit(f, Op::LoadCaptureCell { dst, idx: i });
            } else {
                self.emit(f, Op::LoadCapture { dst, idx: i });
            }
        } else {
            // A local declared in an unreachable or not-yet-compiled branch.
            self.unit_to(f, dst);
        }
    }

    fn write_local(&mut self, f: &mut FnState, l: LocalId, src: Reg) {
        if let Some(&r) = f.local_reg.get(&l) {
            if self.is_cell(f, l) {
                self.emit(f, Op::StoreCell { cell: r, src });
            } else {
                self.mv(f, r, src);
            }
        } else if let Some(&i) = f.capture_idx.get(&l) {
            self.emit(f, Op::StoreCaptureCell { idx: i, src });
        }
    }

    /// Declares local `l` (allocating its register) and initialises it from `src`.
    fn declare_local(&mut self, f: &mut FnState, l: LocalId) -> Reg {
        let r = self.alloc(f);
        f.local_reg.insert(l, r);
        r
    }

    /// Initialises a declared local from `src` (wrapping in a cell if needed).
    fn init_local(&mut self, f: &mut FnState, l: LocalId, src: Reg) {
        let r = f.local_reg[&l];
        if self.is_cell(f, l) {
            self.emit(f, Op::NewCell { dst: r, src });
        } else {
            self.mv(f, r, src);
        }
    }

    // ------------------------------------------------------------------
    // Expressions

    /// Returns a register holding the value of `e`, avoiding a copy for plain locals.
    fn expr_reg(&mut self, f: &mut FnState, e: &HExpr) -> Reg {
        if let HKind::Local(l) = e.kind
            && let Some(&r) = f.local_reg.get(&l)
            && !self.is_cell(f, l)
        {
            if f.builders.contains(&l) {
                self.emit(f, Op::FreezeStr { reg: r });
            }
            return r;
        }
        if !f.hoisted.is_empty()
            && let Some(k) = self.hoist_key(e)
            && let Some(&r) = f.hoisted.get(&k)
        {
            return r;
        }
        let r = self.alloc(f);
        self.expr_to(f, e, r);
        r
    }

    /// Compiles `e` for its side effects only.
    fn discard(&mut self, f: &mut FnState, e: &HExpr) {
        match &e.kind {
            HKind::Let { pat, init, else_ } => {
                self.set_line(f, e.span);
                self.compile_let(f, pat, init, else_.as_deref());
                return;
            }
            HKind::Assign { place, op, op_ty, value } => {
                self.set_line(f, e.span);
                self.compile_assign(f, place, *op, op_ty, value);
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
                let mark = f.next_reg;
                f.defers.push(Vec::new());
                for it in items {
                    self.discard(f, it);
                }
                if !e.ty.is_never() {
                    self.emit_scope_defers(f);
                }
                f.defers.pop();
                f.next_reg = mark;
                return;
            }
            HKind::Defer(x) => {
                if f.defers.is_empty() {
                    f.defers.push(Vec::new());
                }
                f.defers.last_mut().unwrap().push((**x).clone());
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
    fn unit_to(&mut self, f: &mut FnState, dst: Reg) {
        if f.void != Some(dst) {
            self.emit(f, Op::LoadUnit { dst });
        }
    }

    fn set_line(&mut self, f: &mut FnState, span: Span) {
        if span.file == f.file && span.hi > 0 {
            f.line = self.line_of(span);
        }
    }

    /// Compiles `e` as the return value of the current function (tail position).
    fn ret_expr(&mut self, f: &mut FnState, e: &HExpr) {
        self.set_line(f, e.span);
        // Pending deferred expressions run after the value is computed, so
        // calls cannot be tail calls.
        let tail = !defers_pending(f);
        match &e.kind {
            HKind::Block(items) if !items.is_empty() => {
                let mark = f.next_reg;
                f.defers.push(Vec::new());
                for it in &items[..items.len() - 1] {
                    self.discard(f, it);
                }
                self.ret_expr(f, items.last().unwrap());
                f.defers.pop();
                f.next_reg = mark;
            }
            HKind::Call { func, args } if tail && self.fn_protos[*func as usize].is_some() => {
                let proto = self.fn_protos[*func as usize].unwrap();
                let base = self.call_args(f, args, 0);
                self.set_line(f, e.span);
                self.emit(f, Op::TailCallDirect { base, proto });
                self.emit(f, Op::Ret { src: base });
            }
            HKind::CallValue { callee, args } if tail => {
                let base = self.alloc_n(f, args.len() + 1);
                self.expr_to(f, callee, base);
                for (i, a) in args.iter().enumerate() {
                    self.expr_to(f, a, base + 1 + i as Reg);
                }
                self.set_line(f, e.span);
                self.emit(f, Op::TailCall { base, argc: args.len() as u16 });
                self.emit(f, Op::Ret { src: base });
            }
            HKind::CallTrait { slot, args, impl_id, .. } if tail => match self.static_target(*impl_id, *slot) {
                Some(proto) => {
                    let base = self.call_args(f, args, 0);
                    self.set_line(f, e.span);
                    self.emit(f, Op::TailCallDirect { base, proto });
                    self.emit(f, Op::Ret { src: base });
                }
                None => {
                    let base = self.call_args(f, args, 1);
                    self.emit(f, Op::GetField { dst: base, obj: base + 1, idx: *slot as u16 });
                    self.set_line(f, e.span);
                    self.emit(f, Op::TailCall { base, argc: args.len() as u16 });
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
            HKind::Return(Some(v)) => self.ret_expr(f, v),
            _ => {
                let mark = f.next_reg;
                if matches!(e.ty, Type::Unit) && !matches!(e.kind, HKind::Local(_) | HKind::Global(_)) {
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
    fn emit_defers(&mut self, f: &mut FnState, from: usize) {
        let line = f.line;
        for i in (from..f.defers.len()).rev() {
            for j in (0..f.defers[i].len()).rev() {
                // Copies are bounded so that many defers times many exits
                // cannot make compilation itself unbounded.
                if f.defer_ops > MAX_DEFER_OPS {
                    let span = f.defers[i][j].span;
                    let msg =
                        "too much deferred code: a `defer` is compiled into every exit of its block; move the cleanup into a function";
                    if !self.errors.iter().any(|d| d.span == span && d.message == msg) {
                        self.errors.push(Diag::error(span, msg));
                    }
                    f.too_big = true;
                    return;
                }
                let x = f.defers[i][j].clone();
                let (ops, protos) = (f.code.len(), self.prog.protos.len());
                // Locals declared by the deferred code get fresh registers at
                // each copy (the registers of an earlier copy may be in use here).
                let saved = f.local_reg.clone();
                self.discard(f, &x);
                f.local_reg = saved;
                let lambda_ops: usize = self.prog.protos[protos..].iter().map(|p| p.code.len()).sum();
                f.defer_ops += f.code.len() - ops + lambda_ops;
            }
        }
        f.line = line;
    }

    /// Runs the deferred expressions of the innermost scope (its block ends normally).
    fn emit_scope_defers(&mut self, f: &mut FnState) {
        if f.defers.last().is_some_and(|d| !d.is_empty()) {
            let n = f.defers.len() - 1;
            self.emit_defers(f, n);
        }
    }

    fn patch_list(&mut self, f: &mut FnState, jumps: Vec<usize>) {
        for j in jumps {
            self.patch_here(f, j);
        }
    }

    /// Emits a conditional jump taken when `cond` is false; returns patch sites.
    fn cond_jump(&mut self, f: &mut FnState, cond: &HExpr) -> Vec<usize> {
        self.set_line(f, cond.span);
        if !f.no_fused
            && let HKind::Cmp { op, l, r } = &cond.kind
        {
            let ty = &l.ty;
            if matches!(ty, Type::Int) || matches!(ty, Type::Float) {
                let a = self.expr_reg(f, l);
                let b = self.expr_reg(f, r);
                let int = matches!(ty, Type::Int);
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
            _ => {
                let c = self.expr_reg(f, cond);
                vec![self.emit(f, Op::JmpIfNot { cond: c, off: 0 })]
            }
        }
    }

    /// A character-class test (`c.is_alnum()`, an inline `StrClass`) as
    /// one compare-and-branch: jumps when the test is `when`.
    fn str_class_jump(&mut self, f: &mut FnState, cond: &HExpr, when: bool) -> Option<Vec<usize>> {
        let HKind::CallNative { native, args, .. } = &cond.kind else { return None };
        let (Some(&Inline::StrClass(class)), [HArg::Expr(x)], false) = (self.inline.get(native), args.as_slice(), f.no_fused) else {
            return None;
        };
        let mark = f.next_reg;
        let src = self.expr_reg(f, x);
        let j = self.emit(f, Op::JStrClass { src, class: if when { class | 0x80 } else { class }, off: 0 });
        f.next_reg = mark;
        Some(vec![j])
    }

    /// Like [`Self::cond_jump`], but the jumps are taken when `cond` is true.
    fn cond_jump_true(&mut self, f: &mut FnState, cond: &HExpr) -> Vec<usize> {
        self.set_line(f, cond.span);
        if let Some(j) = self.str_class_jump(f, cond, true) {
            return j;
        }
        match &cond.kind {
            // Integer comparisons as the fused "jump if not" of the opposite
            // comparison (not floats: the opposite of `<` is not `>=` for NaN).
            HKind::Cmp { op, l, r } if !f.no_fused && matches!(l.ty, Type::Int) => {
                let a = self.expr_reg(f, l);
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
            _ => {
                let c = self.expr_reg(f, cond);
                vec![self.emit(f, Op::JmpIf { cond: c, off: 0 })]
            }
        }
    }

    /// Compiles arguments into consecutive registers starting at a fresh base
    /// (after `extra` leading slots); returns the base.
    fn call_args(&mut self, f: &mut FnState, args: &[HArg], extra: usize) -> Reg {
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

    fn expr_to(&mut self, f: &mut FnState, e: &HExpr, dst: Reg) {
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
                self.compile_assign(f, place, *op, op_ty, value);
                self.unit_to(f, dst);
            }
            HKind::Block(items) => {
                let mark = f.next_reg;
                if items.is_empty() {
                    self.unit_to(f, dst);
                } else {
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
                }
                f.next_reg = mark;
            }
            HKind::Defer(_) => {
                self.discard(f, e);
                self.unit_to(f, dst);
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
                let v = self.expr_reg(f, value);
                let mut fails = Vec::new();
                self.match_pattern(f, pat, v, &mut fails);
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
            HKind::Match { scrut, arms } => {
                let mark = f.next_reg;
                let s = self.expr_reg(f, scrut);
                let mut ends = Vec::new();
                for arm in arms {
                    let amark = f.next_reg;
                    self.declare_pattern_locals(f, &arm.pat);
                    let mut fails = Vec::new();
                    self.match_pattern(f, &arm.pat, s, &mut fails);
                    if let Some(g) = &arm.guard {
                        fails.extend(self.cond_jump(f, g));
                    }
                    self.expr_to(f, &arm.body, dst);
                    ends.push(self.emit_jump(f));
                    self.patch_list(f, fails);
                    f.next_reg = amark;
                }
                let k = self.const_idx(Const::Str("internal error: no match arm matched".into()));
                self.emit(f, Op::Panic { msg: k });
                self.patch_list(f, ends);
                f.next_reg = mark;
            }
            HKind::Loop(body) => {
                let mark = f.next_reg;
                let top = self.here(f);
                f.loops.push(LoopCx { result: Some(dst), defer_depth: f.defers.len(), ..Default::default() });
                self.discard(f, body);
                self.emit_loop_back(f, top);
                let cx = f.loops.pop().unwrap();
                self.patch_list(f, cx.breaks);
                for c in cx.continues {
                    self.patch(f, c, top);
                }
                f.next_reg = mark;
            }
            HKind::While { cond, body } => {
                let mark = f.next_reg;
                let top = self.here(f);
                let jf = self.cond_jump(f, cond);
                f.next_reg = mark;
                f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                self.discard(f, body);
                self.emit_loop_back(f, top);
                let cx = f.loops.pop().unwrap();
                self.patch_list(f, jf);
                self.patch_list(f, cx.breaks);
                for c in cx.continues {
                    self.patch(f, c, top);
                }
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::WhileLet { pat, value, body } => {
                let mark = f.next_reg;
                self.declare_pattern_locals(f, pat);
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
                for c in cx.continues {
                    self.patch(f, c, top);
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
                self.range_loop(f, pat, body, base, *inclusive, dst);
                f.next_reg = mark;
            }
            HKind::For { pat, iter, kind, body } => {
                let mark = f.next_reg;
                if *kind == IterKind::Channel {
                    let ch = self.alloc(f);
                    self.expr_to(f, iter, ch);
                    self.declare_pattern_locals(f, pat);
                    let top = self.here(f);
                    let t = self.alloc(f);
                    self.mv(f, t, ch);
                    match self.recv_native {
                        Some(n) => {
                            self.emit(f, Op::CallNative { base: t, native: n });
                        }
                        None => {
                            self.emit(f, Op::LoadTag { dst: t, tag: self.prog.ids.none });
                        }
                    }
                    let some = self.variant_u16(f, self.prog.ids.some);
                    let exit = self.jump_not_variant(f, t, some);
                    let elem = self.alloc(f);
                    self.emit(f, Op::GetField { dst: elem, obj: t, idx: 0 });
                    let mut fails = Vec::new();
                    self.match_pattern(f, pat, elem, &mut fails);
                    self.patch_list(f, fails);
                    f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                    self.discard(f, body);
                    self.emit_loop_back(f, top);
                    let cx = f.loops.pop().unwrap();
                    self.patch_list(f, exit);
                    self.patch_list(f, cx.breaks);
                    for c in cx.continues {
                        self.patch(f, c, top);
                    }
                    self.unit_to(f, dst);
                    f.next_reg = mark;
                    return;
                }
                let base = self.alloc_n(f, 4);
                match kind {
                    IterKind::List => self.expr_to(f, iter, base),
                    IterKind::Range => {
                        // Loop over the bounds, without materialising the range.
                        let r = self.alloc(f);
                        self.expr_to(f, iter, r);
                        self.emit(f, Op::UnpackRange { base, src: r });
                        self.range_loop(f, pat, body, base, true, dst);
                        f.next_reg = mark;
                        return;
                    }
                    _ => {
                        let r = self.alloc(f);
                        self.expr_to(f, iter, r);
                        self.emit(f, Op::IterSnapshot { dst: base, src: r });
                    }
                }
                self.emit(f, Op::LoadInt { dst: base + 1, v: 0 });
                self.emit(f, Op::Len { dst: base + 2, src: base });
                self.declare_pattern_locals(f, pat);
                let (elem, direct) = match pat.kind {
                    HPatKind::Bind(l) if !self.is_cell(f, l) => (f.local_reg[&l], true),
                    _ => (self.alloc(f), false),
                };
                let top = self.here(f);
                let exit = self.for_next(f, 0, elem, base);
                if !direct {
                    let mut fails = Vec::new();
                    self.match_pattern(f, pat, elem, &mut fails);
                    self.patch_list(f, fails);
                }
                f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
                self.discard(f, body);
                self.emit_loop_back(f, top);
                let cx = f.loops.pop().unwrap();
                self.patch_list(f, exit);
                self.patch_list(f, cx.breaks);
                for c in cx.continues {
                    self.patch(f, c, top);
                }
                self.unit_to(f, dst);
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
                // A backward jump to the loop's start: like the loop's own
                // back edge, it consumes fuel (a loop that only continues
                // must still be preempted and stopped by the limits).
                let j = self.emit(f, Op::Loop { off: 0 });
                if let Some(i) = target {
                    f.loops[i].continues.push(j);
                }
            }
            HKind::Return(v) => match v {
                Some(v) => {
                    let mark = f.next_reg;
                    if f.in_tail_fn {
                        self.ret_expr(f, v);
                    } else {
                        let r = self.expr_reg(f, v);
                        self.emit(f, Op::Ret { src: r });
                    }
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
                if let (Some(&op), true) = (self.inline.get(native), args.iter().all(|a| matches!(a, HArg::Expr(_)))) {
                    let mark = f.next_reg;
                    let regs: Vec<Reg> = args
                        .iter()
                        .map(|a| match a {
                            HArg::Expr(x) => self.expr_reg(f, x),
                            HArg::Default { .. } => unreachable!(),
                        })
                        .collect();
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
                    }
                    f.next_reg = mark;
                    return;
                }
                let mark = f.next_reg;
                if let ([HArg::Expr(x)], true) = (args.as_slice(), reflect.is_empty()) {
                    // One argument: passed in place (e.g. a local's register).
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
                    let Some(k) = self.descs.const_for(self.cp, t, add) else {
                        self.errors.push(Diag::error(e.span, "type too large to describe at runtime"));
                        continue;
                    };
                    self.emit(f, Op::LoadConst { dst: base + i as Reg, k });
                }
                self.set_line(f, e.span);
                self.call_native(f, base, *native, dst);
                f.next_reg = mark;
            }
            HKind::CallValue { callee, args } => {
                let mark = f.next_reg;
                if let (HKind::Local(_), Ok(argc)) = (&callee.kind, u8::try_from(args.len())) {
                    // The function is in a register already: call it there.
                    let c = self.expr_reg(f, callee);
                    let base = self.alloc_n(f, args.len().max(1));
                    for (i, a) in args.iter().enumerate() {
                        self.expr_to(f, a, base + i as Reg);
                    }
                    self.set_line(f, e.span);
                    self.emit(f, Op::CallR { callee: c, base, dst, argc });
                    f.next_reg = mark;
                    return;
                }
                let base = self.alloc_n(f, args.len() + 1);
                self.expr_to(f, callee, base);
                for (i, a) in args.iter().enumerate() {
                    self.expr_to(f, a, base + 1 + i as Reg);
                }
                self.set_line(f, e.span);
                self.emit(f, Op::Call { base, argc: args.len() as u16, dst });
                f.next_reg = mark;
            }
            HKind::CallTrait { slot, args, impl_id, .. } => {
                let mark = f.next_reg;
                match self.static_target(*impl_id, *slot) {
                    // The impl is known: call its method directly.
                    Some(proto) => {
                        let base = self.call_args(f, args, 0);
                        self.set_line(f, e.span);
                        self.call_direct(f, base, proto, dst);
                    }
                    // Otherwise fetch the method from the dictionary (the first argument).
                    None => {
                        let base = self.call_args(f, args, 1);
                        self.emit(f, Op::GetField { dst: base, obj: base + 1, idx: *slot as u16 });
                        self.set_line(f, e.span);
                        self.emit(f, Op::Call { base, argc: args.len() as u16, dst });
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
                self.emit(f, Op::NewStruct { dst, start, def: *def as u16 });
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
                let top = self.here(f);
                let exit = self.for_next(f, 1, var, base);
                let t = self.alloc(f);
                self.expr_to(f, elem, t);
                self.emit(f, Op::ListPush { list: l, src: t });
                self.emit_loop_back(f, top);
                self.patch_list(f, exit);
                self.mv(f, dst, l);
                f.next_reg = mark;
            }
            HKind::PushLocal { list, value } => {
                let mark = f.next_reg;
                let l = self.alloc(f);
                self.read_local(f, *list, l);
                let v = self.expr_reg(f, value);
                self.emit(f, Op::ListPush { list: l, src: v });
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::InsertLocal { map, key, value } => {
                let mark = f.next_reg;
                let m = self.alloc(f);
                self.read_local(f, *map, m);
                let k = self.expr_reg(f, key);
                let v = self.expr_reg(f, value);
                self.emit(f, Op::SetIndexMap { obj: m, key: k, src: v });
                self.unit_to(f, dst);
                f.next_reg = mark;
            }
            HKind::Field { obj, index } => {
                let mark = f.next_reg;
                let o = self.expr_reg(f, obj);
                self.emit(f, Op::GetField { dst, obj: o, idx: *index as u16 });
                f.next_reg = mark;
            }
            HKind::Index { obj, index, map } => {
                let mark = f.next_reg;
                let o = self.expr_reg(f, obj);
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
            HKind::Unary { op, e: inner } => {
                let mark = f.next_reg;
                let a = self.expr_reg(f, inner);
                let op = match op {
                    HUnOp::Not => Op::Not { dst, a },
                    HUnOp::BitNot => Op::BitNot { dst, a },
                    HUnOp::Neg => match inner.ty {
                        Type::Int => Op::NegI { dst, a },
                        Type::Float => Op::NegF { dst, a },
                        _ => Op::Neg { dst, a },
                    },
                };
                self.set_line(f, e.span);
                self.emit(f, op);
                f.next_reg = mark;
            }
            HKind::Arith { op, l, r } => {
                let mark = f.next_reg;
                // `x + k` / `x - k` with small integer constants.
                if matches!(l.ty, Type::Int)
                    && matches!(op, ArithOp::Add | ArithOp::Sub)
                    && let HKind::Int(k) = r.kind
                {
                    let k = if *op == ArithOp::Sub { k.checked_neg() } else { Some(k) };
                    if let Some(k) = k.filter(|k| *k >= i16::MIN as i64 && *k <= i16::MAX as i64) {
                        let a = self.expr_reg(f, l);
                        self.set_line(f, e.span);
                        self.emit(f, Op::AddIK { dst, a, k: k as i16 });
                        f.next_reg = mark;
                        return;
                    }
                }
                let a = self.expr_reg(f, l);
                let b = self.expr_reg(f, r);
                let op = arith_op(*op, &l.ty, &r.ty, dst, a, b);
                self.set_line(f, e.span);
                self.emit(f, op);
                f.next_reg = mark;
            }
            HKind::Cmp { op, l, r } => {
                let mark = f.next_reg;
                let a = self.expr_reg(f, l);
                let b = self.expr_reg(f, r);
                let op = cmp_op(*op, &l.ty, dst, a, b);
                self.emit(f, op);
                f.next_reg = mark;
            }
            HKind::And(a, b) => {
                self.expr_to(f, a, dst);
                let j = self.emit(f, Op::JmpIfNot { cond: dst, off: 0 });
                self.expr_to(f, b, dst);
                self.patch_here(f, j);
            }
            HKind::Or(a, b) => {
                self.expr_to(f, a, dst);
                let j = self.emit(f, Op::JmpIf { cond: dst, off: 0 });
                self.expr_to(f, b, dst);
                self.patch_here(f, j);
            }
            HKind::Lambda(lam) => {
                let p = self.compile_lambda(f, lam, &e.ty);
                self.emit(f, Op::MakeClosure { dst, proto: p });
            }
            HKind::Try { e: inner, kind } => {
                let mark = f.next_reg;
                let v = if defers_pending(f) {
                    // A fresh register: deferred code may assign the local tested.
                    let t = self.alloc(f);
                    self.expr_to(f, inner, t);
                    t
                } else {
                    self.expr_reg(f, inner)
                };
                let (good, _) = match kind {
                    TryKind::Option => (self.prog.ids.some, self.prog.ids.none),
                    _ => (self.prog.ids.ok, self.prog.ids.err),
                };
                let good = self.variant_u16(f, good);
                let fail = self.jump_not_variant(f, v, good);
                self.emit(f, Op::GetField { dst, obj: v, idx: 0 });
                let jend = self.emit_jump(f);
                self.patch_list(f, fail);
                self.set_line(f, e.span);
                self.emit_defers(f, 0);
                match kind {
                    TryKind::Option => {
                        let t = self.alloc(f);
                        self.emit(f, Op::LoadTag { dst: t, tag: self.prog.ids.none });
                        self.emit(f, Op::Ret { src: t });
                    }
                    TryKind::Result => {
                        self.emit(f, Op::Ret { src: v });
                    }
                    TryKind::TopLevel => {
                        self.emit(f, Op::TryTop { src: v });
                    }
                }
                self.patch_here(f, jend);
                f.next_reg = mark;
            }
            HKind::Coalesce { l, r, result } => {
                let mark = f.next_reg;
                let v = self.expr_reg(f, l);
                let good = if *result { self.prog.ids.ok } else { self.prog.ids.some };
                let good = self.variant_u16(f, good);
                let other = self.jump_not_variant(f, v, good);
                self.emit(f, Op::GetField { dst, obj: v, idx: 0 });
                let jend = self.emit_jump(f);
                self.patch_list(f, other);
                self.expr_to(f, r, dst);
                self.patch_here(f, jend);
                f.next_reg = mark;
            }
            HKind::Interp(parts) => {
                let mark = f.next_reg;
                let start = self.alloc_n(f, parts.len());
                for (i, p) in parts.iter().enumerate() {
                    let r = start + i as Reg;
                    match p {
                        InterpPart::Text(t) => self.load_str(f, r, t),
                        InterpPart::Expr(x, spec) => {
                            let v = self.expr_reg(f, x);
                            match spec {
                                Some(s) => {
                                    let k = self.prog.specs.len();
                                    if k > u16::MAX as usize {
                                        self.errors.push(Diag::error(x.span, "too many format specifications in program"));
                                    }
                                    self.prog.specs.push(s.clone());
                                    self.emit(f, Op::Format { dst: r, src: v, spec: k as u16 });
                                }
                                None => {
                                    if matches!(x.ty, Type::Str) {
                                        self.mv(f, r, v);
                                    } else {
                                        self.emit(f, Op::ToStr { dst: r, src: v });
                                    }
                                }
                            }
                            f.next_reg = start as u32 + parts.len() as u32;
                        }
                    }
                }
                self.emit(f, Op::Concat { dst, start, count: parts.len() as u16 });
                f.next_reg = mark;
            }
            HKind::Range { start, end, inclusive } => {
                let mark = f.next_reg;
                let a = self.expr_reg(f, start);
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

    /// The proto of the method a trait call invokes, when its dictionary
    /// comes from a known impl.
    fn static_target(&self, impl_id: Option<u32>, slot: u32) -> Option<u32> {
        let fid = *self.cp.impls[impl_id? as usize].slots.get(slot as usize)?;
        self.fn_protos[fid as usize]
    }

    fn fn_const(&mut self, fid: FnId) -> u32 {
        match self.fn_protos[fid as usize] {
            Some(p) => self.const_idx(Const::Fn(p)),
            None => self.const_idx(Const::Str("<missing fn>".into())),
        }
    }

    /// A dictionary that does not depend on the function's own dictionaries
    /// is built once, as a constant.
    fn const_dict(&mut self, e: &HExpr) -> Option<u32> {
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

    /// Emits the per-iteration step of a fused loop; returns exit jump sites.
    /// kind: 0 = list, 1 = range, 2 = inclusive range.
    /// The loop of a range whose bounds are in `base` (start) and `base+1`
    /// (end; for an inclusive loop, `base+2` holds the done flag).
    fn range_loop(&mut self, f: &mut FnState, pat: &HPat, body: &HExpr, base: Reg, inclusive: bool, dst: Reg) {
        self.declare_pattern_locals(f, pat);
        let (var, direct) = match pat.kind {
            HPatKind::Bind(l) if !self.is_cell(f, l) => (f.local_reg[&l], true),
            _ => (self.alloc(f), false),
        };
        let top = self.here(f);
        let exit = self.for_next(f, if inclusive { 2 } else { 1 }, var, base);
        if !direct {
            let mut fails = Vec::new();
            self.match_pattern(f, pat, var, &mut fails);
            self.patch_list(f, fails);
        }
        f.loops.push(LoopCx { defer_depth: f.defers.len(), ..Default::default() });
        self.discard(f, body);
        self.emit_loop_back(f, top);
        let cx = f.loops.pop().unwrap();
        self.patch_list(f, exit);
        self.patch_list(f, cx.breaks);
        for c in cx.continues {
            self.patch(f, c, top);
        }
        self.unit_to(f, dst);
    }

    fn for_next(&mut self, f: &mut FnState, kind: u8, var: Reg, base: Reg) -> Vec<usize> {
        if !f.no_fused {
            let op = match kind {
                0 => Op::ForList { elem: var, base, off: 0 },
                1 => Op::ForRange { var, base, off: 0 },
                _ => Op::ForRangeIncl { var, base, off: 0 },
            };
            return vec![self.emit(f, op)];
        }
        let op = match kind {
            0 => Op::ForListNext { elem: var, base },
            1 => Op::ForRangeNext { var, base },
            _ => Op::ForRangeInclNext { var, base },
        };
        self.emit(f, op);
        vec![self.emit(f, Op::JmpIfNot { cond: base + 3, off: 0 })]
    }

    fn jump_not_variant(&mut self, f: &mut FnState, src: Reg, variant: u16) -> Vec<usize> {
        if !f.no_fused {
            return vec![self.emit(f, Op::JNotVariant { src, variant, off: 0 })];
        }
        let t = self.alloc(f);
        self.emit(f, Op::IsVariant { dst: t, src, variant });
        vec![self.emit(f, Op::JmpIfNot { cond: t, off: 0 })]
    }

    fn variant_wrapper(&mut self, rt: u32, arity: usize, file: u32) -> u32 {
        if let Some(&p) = self.variant_wrappers.get(&rt) {
            return p;
        }
        let name = self.prog.variants.get(rt as usize).map(|v| v.name.clone()).unwrap_or_default();
        let code = vec![Op::NewVariant { dst: arity as Reg, start: 0, variant: rt as u16 }, Op::Ret { src: arity as Reg }];
        let p = self.prog.protos.len() as u32;
        self.prog.protos.push(Proto {
            name: name.clone(),
            file,
            nparams: arity as u16,
            nregs: arity as u16 + 1,
            lines: vec![0; code.len()],
            code,
            captures: vec![],
            capture_cells: vec![],
            doc: None,
            signature: format!("fn {name}(...)"),
        });
        self.variant_wrappers.insert(rt, p);
        p
    }

    // ------------------------------------------------------------------
    // Loop-invariant hoisting

    /// The hoisting key of a constant or immutable-global expression.
    fn hoist_key(&self, e: &HExpr) -> Option<HoistKey> {
        Some(match &e.kind {
            HKind::Int(i) => HoistKey::Int(*i),
            HKind::Float(x) => HoistKey::Float(x.to_bits()),
            HKind::Str(s) => HoistKey::Str(s.clone()),
            HKind::Bytes(b) => HoistKey::Bytes(b.clone()),
            HKind::Dur(d) => HoistKey::Dur(*d),
            HKind::Tag(t) => HoistKey::Tag(*t),
            HKind::Global(g) if self.cp.globals.get(*g as usize).is_some_and(|gi| !gi.mutable) => HoistKey::Global(*g),
            _ => return None,
        })
    }

    /// Hoistable values used in loop `e` outside nested lambdas, most
    /// frequent first, plus whether `e` contains lambdas and its size.
    ///
    /// Constants only count where the compiler evaluates them with
    /// `expr_reg` (as instruction operands): elsewhere they are materialised
    /// into a fixed register anyway, so hoisting would not save anything.
    /// Immutable globals count everywhere, since a `Move` from a register is
    /// cheaper than a checked `LoadGlobal`.
    fn hoist_candidates(&self, e: &HExpr) -> (Vec<HoistKey>, bool, usize) {
        let mut in_lambda: HashSet<*const HExpr> = HashSet::new();
        let mut has_lambda = false;
        let mut nodes = 0;
        let mut found: Vec<(HoistKey, usize)> = Vec::new();
        let mut count = |k: HoistKey| match found.iter_mut().find(|(fk, _)| *fk == k) {
            Some(entry) => entry.1 += 1,
            None => found.push((k, 1)),
        };
        walk(e, &mut |x| {
            nodes += 1;
            if in_lambda.contains(&(x as *const HExpr)) {
                return;
            }
            if let HKind::Lambda(l) = &x.kind {
                // A separate function with its own registers.
                has_lambda = true;
                walk(&l.body, &mut |y| {
                    in_lambda.insert(y as *const HExpr);
                });
                return;
            }
            if let Some(k @ HoistKey::Global(_)) = self.hoist_key(x) {
                count(k);
            }
            self.operands(x, &mut |o| {
                if let Some(k) = self.hoist_key(o)
                    && !matches!(k, HoistKey::Global(_))
                {
                    count(k);
                }
            });
        });
        // Most frequent first; ties keep source order.
        found.sort_by_key(|e| std::cmp::Reverse(e.1));
        (found.into_iter().map(|(k, _)| k).collect(), has_lambda, nodes)
    }

    /// Calls `out` for the direct sub-expressions of `x` that the compiler
    /// evaluates with `expr_reg` (approximately; used for hoisting only).
    fn operands<'x>(&self, x: &'x HExpr, out: &mut impl FnMut(&'x HExpr)) {
        let small = |e: &HExpr| matches!(e.kind, HKind::Int(k) if k >= i16::MIN as i64 && k <= i16::MAX as i64);
        match &x.kind {
            HKind::Arith { op, l, r } => {
                out(l);
                // `x + k` / `x - k` use an immediate.
                if !(matches!(op, ArithOp::Add | ArithOp::Sub) && matches!(l.ty, Type::Int) && small(r)) {
                    out(r);
                }
            }
            HKind::Cmp { l, r, .. } => {
                out(l);
                out(r);
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

    /// Loads the constant `k` into `dst`.
    fn load_hoisted(&mut self, f: &mut FnState, k: &HoistKey, dst: Reg) {
        match k {
            HoistKey::Int(i) => self.load_int(f, dst, *i),
            HoistKey::Str(s) => self.load_str(f, dst, s),
            HoistKey::Tag(t) => {
                self.emit(f, Op::LoadTag { dst, tag: *t });
            }
            HoistKey::Float(_) | HoistKey::Bytes(_) | HoistKey::Dur(_) => {
                let c = match k {
                    HoistKey::Float(bits) => Const::Float(f64::from_bits(*bits)),
                    HoistKey::Bytes(b) => Const::Bytes(b.clone()),
                    HoistKey::Dur(d) => Const::Dur(*d),
                    _ => unreachable!(),
                };
                let kidx = self.const_idx(c);
                self.emit(f, Op::LoadConst { dst, k: kidx });
            }
            HoistKey::Global(g) => {
                self.emit(f, Op::LoadGlobal { dst, g: *g });
            }
        }
    }

    /// Compiles the outermost loop `e` with its loop-invariant constants and
    /// immutable globals loaded into registers once, before the loop.
    ///
    /// Constants are pure. An immutable global never changes once
    /// initialised, but reading it before its initialiser has run must
    /// panic at the point of use, so hoisted globals are loaded with
    /// `HoistGlobal`, which jumps to a second, unhoisted copy of the loop if
    /// any of them is still uninitialised. Loops containing lambdas (whose
    /// prototypes would be duplicated) or larger than `MAX_VERSIONED_NODES`
    /// only get their constants hoisted.
    fn hoisted_loop(&mut self, f: &mut FnState, e: &HExpr, dst: Reg) {
        let mark = f.next_reg;
        f.hoisting = true;
        let (cands, has_lambda, nodes) = self.hoist_candidates(e);
        if !self.budget.charge(nodes as u64) {
            f.hoisting = false;
            return;
        }
        let cands: Vec<HoistKey> = cands.into_iter().take(MAX_HOISTED).collect();
        if cands.is_empty() || f.next_reg + cands.len() as u32 > HOIST_REG_BUDGET {
            self.expr_to(f, e, dst);
            f.hoisting = false;
            return;
        }
        let version = !has_lambda && nodes <= MAX_VERSIONED_NODES && !f.no_fused;
        let mut globals = Vec::new();
        for k in &cands {
            match k {
                HoistKey::Global(g) => {
                    if version && *g <= u16::MAX as u32 {
                        globals.push((*g as u16, k.clone()));
                    }
                }
                _ => {
                    let r = self.alloc(f);
                    self.load_hoisted(f, k, r);
                    f.hoisted.insert(k.clone(), r);
                }
            }
        }
        if globals.is_empty() {
            self.expr_to(f, e, dst);
        } else {
            let mut guards = Vec::new();
            for (g, k) in &globals {
                let r = self.alloc(f);
                guards.push(self.emit(f, Op::HoistGlobal { dst: r, g: *g, off: 0 }));
                f.hoisted.insert(k.clone(), r);
            }
            let locals = f.local_reg.clone();
            self.expr_to(f, e, dst);
            let jend = self.emit_jump(f);
            // Some global was not initialised yet: run the loop without them.
            self.patch_list(f, guards);
            for (_, k) in &globals {
                f.hoisted.remove(k);
            }
            f.local_reg = locals;
            self.expr_to(f, e, dst);
            self.patch_here(f, jend);
        }
        f.hoisted.clear();
        f.hoisting = false;
        f.next_reg = mark;
    }

    // ------------------------------------------------------------------
    // Assignment

    fn compile_assign(&mut self, f: &mut FnState, place: &Place, op: Option<ArithOp>, op_ty: &Type, value: &HExpr) {
        let mark = f.next_reg;
        match place {
            Place::Local(l) => match op {
                None => {
                    if let (Some(&r), false) = (f.local_reg.get(l), self.is_cell(f, *l)) {
                        self.expr_to(f, value, r);
                    } else {
                        let v = self.expr_reg(f, value);
                        self.write_local(f, *l, v);
                    }
                }
                Some(aop) => {
                    if let (Some(&r), false) = (f.local_reg.get(l), self.is_cell(f, *l)) {
                        if f.builders.contains(l) && aop == ArithOp::Add {
                            let v = self.expr_reg(f, value);
                            self.emit(f, Op::AppendStr { local: r, src: v });
                            f.next_reg = mark;
                            return;
                        }
                        // Operate on the local's register directly.
                        self.apply_op(f, aop, op_ty, &value.ty, r, value);
                    } else {
                        let cur = self.alloc(f);
                        self.read_local(f, *l, cur);
                        self.apply_op(f, aop, op_ty, &value.ty, cur, value);
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
                        self.apply_op(f, aop, op_ty, &value.ty, cur, value);
                        cur
                    }
                };
                self.emit(f, Op::StoreGlobal { g: *g, src: v });
            }
            Place::Field { obj, index } => {
                let o = self.expr_reg(f, obj);
                let v = match op {
                    None => self.expr_reg(f, value),
                    Some(aop) => {
                        let cur = self.alloc(f);
                        self.emit(f, Op::GetField { dst: cur, obj: o, idx: *index as u16 });
                        self.apply_op(f, aop, op_ty, &value.ty, cur, value);
                        cur
                    }
                };
                self.emit(f, Op::SetField { obj: o, idx: *index as u16, src: v });
            }
            Place::Index { obj, index, map } => {
                let o = self.expr_reg(f, obj);
                let i = self.expr_reg(f, index);
                let v = match op {
                    None => self.expr_reg(f, value),
                    Some(aop) => {
                        let cur = self.alloc(f);
                        if *map {
                            self.emit(f, Op::IndexMap { dst: cur, obj: o, key: i });
                        } else {
                            self.emit(f, Op::Index { dst: cur, obj: o, idx: i });
                        }
                        self.apply_op(f, aop, op_ty, &value.ty, cur, value);
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
    fn apply_op(&mut self, f: &mut FnState, op: ArithOp, lt: &Type, rt: &Type, cur: Reg, value: &HExpr) {
        if matches!(lt, Type::Int)
            && matches!(op, ArithOp::Add | ArithOp::Sub)
            && let HKind::Int(k) = value.kind
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

    fn declare_pattern_locals(&mut self, f: &mut FnState, p: &HPat) {
        match &p.kind {
            HPatKind::Bind(l) => {
                if !f.local_reg.contains_key(l) {
                    self.declare_local(f, *l);
                }
            }
            HPatKind::Tuple(ps) | HPatKind::Or(ps) => ps.iter().for_each(|x| self.declare_pattern_locals(f, x)),
            HPatKind::Variant { args, .. } => args.iter().for_each(|x| self.declare_pattern_locals(f, x)),
            HPatKind::Struct { fields } => fields.iter().for_each(|(_, x)| self.declare_pattern_locals(f, x)),
            HPatKind::List { before, rest, after } => {
                before.iter().for_each(|x| self.declare_pattern_locals(f, x));
                if let Some(Some(l)) = rest
                    && !f.local_reg.contains_key(l)
                {
                    self.declare_local(f, *l);
                }
                after.iter().for_each(|x| self.declare_pattern_locals(f, x));
            }
            _ => {}
        }
    }

    /// Tests `v` against `p`, binding locals; jumps to `fails` on mismatch.
    fn match_pattern(&mut self, f: &mut FnState, p: &HPat, v: Reg, fails: &mut Vec<usize>) {
        match &p.kind {
            HPatKind::Wildcard => {}
            HPatKind::Bind(l) => self.init_local(f, *l, v),
            HPatKind::Int(i) => {
                let t = self.alloc(f);
                self.load_int(f, t, *i);
                if f.no_fused {
                    let c = self.alloc(f);
                    self.emit(f, Op::EqI { dst: c, a: v, b: t });
                    fails.push(self.emit(f, Op::JmpIfNot { cond: c, off: 0 }));
                } else {
                    fails.push(self.emit(f, Op::JNeqI { a: v, b: t, off: 0 }));
                }
            }
            HPatKind::Range(a, b) => {
                let t = self.alloc(f);
                let c = self.alloc(f);
                self.load_int(f, t, *a);
                self.emit(f, Op::LeI { dst: c, a: t, b: v });
                fails.push(self.emit(f, Op::JmpIfNot { cond: c, off: 0 }));
                self.load_int(f, t, *b);
                self.emit(f, Op::LeI { dst: c, a: v, b: t });
                fails.push(self.emit(f, Op::JmpIfNot { cond: c, off: 0 }));
            }
            HPatKind::Str(s) => {
                let t = self.alloc(f);
                self.load_str(f, t, s);
                self.emit(f, Op::Eq { dst: t, a: v, b: t });
                fails.push(self.emit(f, Op::JmpIfNot { cond: t, off: 0 }));
            }
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
                    let t = self.field_temp(f, sp, v, *i as u16);
                    self.match_pattern(f, sp, t, fails);
                }
            }
            HPatKind::Variant { rt_id, args }
                if *rt_id == self.prog.ids.some && !f.no_fused && !matches!(args[..], [HPat { kind: HPatKind::Wildcard, .. }]) =>
            {
                // `Some(p)`: test and load the payload in one instruction.
                let sp = &args[0];
                let t = match sp.kind {
                    HPatKind::Bind(l) if !self.is_cell(f, l) && f.local_reg.contains_key(&l) => f.local_reg[&l],
                    _ => self.alloc(f),
                };
                fails.push(self.emit(f, Op::JNotSome { src: v, dst: t, off: 0 }));
                self.match_pattern(f, sp, t, fails);
            }
            HPatKind::Variant { rt_id, args } => {
                let var = self.variant_u16(f, *rt_id);
                fails.extend(self.jump_not_variant(f, v, var));
                for (i, sp) in args.iter().enumerate() {
                    if matches!(sp.kind, HPatKind::Wildcard) {
                        continue;
                    }
                    let t = self.field_temp(f, sp, v, i as u16);
                    self.match_pattern(f, sp, t, fails);
                }
            }
            HPatKind::List { before, rest, after } => {
                let n = (before.len() + after.len()) as u16;
                if f.no_fused {
                    let c = self.alloc(f);
                    if rest.is_some() {
                        self.emit(f, Op::ListLenGe { dst: c, src: v, n });
                    } else {
                        self.emit(f, Op::ListLenEq { dst: c, src: v, n });
                    }
                    fails.push(self.emit(f, Op::JmpIfNot { cond: c, off: 0 }));
                } else if rest.is_some() {
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
                    if before.len() > 255 || after.len() > 255 {
                        self.errors.push(Diag::error(p.span, "list pattern too long"));
                    }
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

    /// Loads field `idx` of `v` into a register suitable for matching `sp`.
    fn field_temp(&mut self, f: &mut FnState, sp: &HPat, v: Reg, idx: u16) -> Reg {
        if let HPatKind::Bind(l) = sp.kind
            && let (Some(&r), false) = (f.local_reg.get(&l), self.is_cell(f, l))
        {
            self.emit(f, Op::GetField { dst: r, obj: v, idx });
            return r;
        }
        let t = self.alloc(f);
        self.emit(f, Op::GetField { dst: t, obj: v, idx });
        t
    }

    fn compile_let(&mut self, f: &mut FnState, pat: &HPat, init: &HExpr, else_: Option<&HExpr>) {
        if let HPatKind::Bind(l) = pat.kind
            && else_.is_none()
        {
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
        let v = self.expr_reg(f, init);
        let mut fails = Vec::new();
        self.match_pattern(f, pat, v, &mut fails);
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

    fn compile_lambda(&mut self, f: &mut FnState, lam: &HLambda, ty: &Type) -> u32 {
        // Free locals: referenced inside but declared outside the lambda.
        let mut inner_lambdas = HashSet::new();
        inner_lambdas.insert(lam.id);
        let mut refs = Vec::new();
        let mut nodes = 0;
        walk(&lam.body, &mut |x| {
            nodes += 1;
            if let HKind::Lambda(l) = &x.kind {
                inner_lambdas.insert(l.id);
            }
            local_refs(x, &mut refs);
        });
        // (Nested lambdas are walked again for each enclosing one.)
        self.budget.charge(nodes);
        let mut free: Vec<LocalId> = Vec::new();
        let mut seen = HashSet::new();
        for l in refs {
            let owner = f.locals[l as usize].owner;
            if !inner_lambdas.contains(&owner) && seen.insert(l) {
                free.push(l);
            }
        }
        let mut captures = Vec::new();
        let mut cells = Vec::new();
        for &l in &free {
            let src = if let Some(&r) = f.local_reg.get(&l) {
                CaptureSrc::Reg(r)
            } else if let Some(&i) = f.capture_idx.get(&l) {
                CaptureSrc::Capture(i)
            } else {
                // Declared in code not yet reached (e.g. a sibling local fn); capture unit.
                CaptureSrc::Reg(u16::MAX)
            };
            captures.push(src);
            cells.push(self.is_cell(f, l));
        }
        let mut sub = FnState {
            code: Vec::new(),
            lines: Vec::new(),
            locals: f.locals,
            local_reg: HashMap::new(),
            capture_idx: free.iter().enumerate().map(|(i, &l)| (l, i as u16)).collect(),
            next_reg: 0,
            max_reg: 0,
            loops: vec![],
            line: f.line,
            file: f.file,
            no_fused: f.no_fused,
            too_big: false,
            in_tail_fn: true,
            hoisted: HashMap::new(),
            hoisting: false,
            defers: Vec::new(),
            defer_ops: 0,
            void: None,
            builders: f.builders.clone(),
        };
        for &p in &lam.params {
            let r = self.alloc(&mut sub);
            sub.local_reg.insert(p, r);
        }
        // Captured values that cannot change (not cells) are loaded into
        // registers once, on entry, and used like the closure's own locals.
        for (i, &l) in free.iter().enumerate() {
            if !cells[i] {
                let r = self.alloc(&mut sub);
                self.emit(&mut sub, Op::LoadCapture { dst: r, idx: i as u16 });
                sub.local_reg.insert(l, r);
            }
        }
        self.ret_expr(&mut sub, &lam.body);
        if sub.too_big {
            f.too_big = true;
        }
        let signature = if lam.signature.is_empty() {
            let printer = crate::types::TypePrinter { defs: &self.cp.types, params: &self.cp.params, wk: self.cp.wk };
            printer.show(ty)
        } else {
            lam.signature.clone()
        };
        let p = self.prog.protos.len() as u32;
        thread_jumps(&mut sub.code);
        self.prog.protos.push(Proto {
            name: lam.name.clone(),
            file: f.file,
            nparams: lam.params.len() as u16,
            nregs: sub.max_reg.max(1) as u16,
            code: sub.code,
            lines: sub.lines,
            captures,
            capture_cells: cells,
            doc: lam.doc.clone(),
            signature,
        });
        p
    }
}

/// Whether a deferred expression is pending in the current function.
fn defers_pending(f: &FnState) -> bool {
    f.defers.iter().any(|d| !d.is_empty())
}

fn arith_op(op: ArithOp, lt: &Type, rt: &Type, dst: Reg, a: Reg, b: Reg) -> Op {
    let int = matches!(lt, Type::Int) && matches!(rt, Type::Int);
    let float = matches!(lt, Type::Float) && matches!(rt, Type::Float);
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

fn cmp_op(op: CmpOp, ty: &Type, dst: Reg, a: Reg, b: Reg) -> Op {
    match ty {
        Type::Int => match op {
            CmpOp::Eq => Op::EqI { dst, a, b },
            CmpOp::Ne => Op::NeI { dst, a, b },
            CmpOp::Lt => Op::LtI { dst, a, b },
            CmpOp::Le => Op::LeI { dst, a, b },
            CmpOp::Gt => Op::LtI { dst, a: b, b: a },
            CmpOp::Ge => Op::LeI { dst, a: b, b: a },
        },
        Type::Float => match op {
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

/// Pre-order traversal of every expression (including inside lambdas, places and args).
/// Jump threading: an unconditional jump to another jump, a backward jump
/// (`Loop`) or a return is replaced by its final target, so that e.g. the
/// end of an `if` branch inside a loop jumps straight back to the loop's
/// start. Only jumps are rewritten in place; no instruction moves.
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
            Some(&Op::Loop { off }) => Op::Loop { off: (t + 1 + off as i64 - (i as i64 + 1)) as i32 },
            Some(&op @ (Op::Ret { .. } | Op::RetUnit)) => op,
            // Every backward jump consumes fuel (see `Op::Loop`), so that no
            // loop escapes the limits.
            _ if t <= i as i64 => Op::Loop { off: (t - (i as i64 + 1)) as i32 },
            _ => Op::Jmp { off: (t - (i as i64 + 1)) as i32 },
        };
    }
    debug_assert!(
        code.iter().all(|op| !matches!(op, Op::JmpIf { off, .. } | Op::JmpIfNot { off, .. } if *off < 0)),
        "conditional backward jump (it would not consume fuel)"
    );
}

/// The string locals of a function body that `+=` appends to and no
/// closure captures (cells): they are appended to in place. Such a local
/// holds a private buffer (`Obj::StrBuf`) while it is being appended to;
/// every other read of it first freezes the buffer into an ordinary string
/// (`Op::FreezeStr`, in place), so no other register, object or closure
/// ever sees a buffer, and appending cannot change a value seen elsewhere.
fn builder_locals(body: &HExpr, locals: &[LocalInfo]) -> HashSet<LocalId> {
    let mut out = HashSet::new();
    walk(body, &mut |e| {
        if let HKind::Assign { place: Place::Local(l), op: Some(ArithOp::Add), op_ty: Type::Str, .. } = &e.kind
            && locals.get(*l as usize).is_some_and(|i| !i.cell)
        {
            out.insert(*l);
        }
    });
    out
}

pub(crate) fn walk(e: &HExpr, f: &mut impl FnMut(&HExpr)) {
    f(e);
    match &e.kind {
        HKind::Let { init, else_, .. } => {
            walk(init, f);
            if let Some(x) = else_ {
                walk(x, f);
            }
        }
        HKind::LocalFns { lambdas, .. } => lambdas.iter().for_each(|x| walk(x, f)),
        HKind::Assign { place, value, .. } => {
            match place {
                Place::Field { obj, .. } => walk(obj, f),
                Place::Index { obj, index, .. } => {
                    walk(obj, f);
                    walk(index, f);
                }
                _ => {}
            }
            walk(value, f);
        }
        HKind::Block(xs)
        | HKind::Tuple(xs)
        | HKind::List(xs)
        | HKind::MakeVariant { args: xs, .. }
        | HKind::MakeStruct { fields: xs, .. } => xs.iter().for_each(|x| walk(x, f)),
        HKind::If { cond, then, else_ } => {
            walk(cond, f);
            walk(then, f);
            if let Some(x) = else_ {
                walk(x, f);
            }
        }
        HKind::IfLet { value, then, else_, .. } => {
            walk(value, f);
            walk(then, f);
            if let Some(x) = else_ {
                walk(x, f);
            }
        }
        HKind::Match { scrut, arms } => {
            walk(scrut, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk(g, f);
                }
                walk(&a.body, f);
            }
        }
        HKind::Loop(b) => walk(b, f),
        HKind::While { cond, body } => {
            walk(cond, f);
            walk(body, f);
        }
        HKind::WhileLet { value, body, .. } => {
            walk(value, f);
            walk(body, f);
        }
        HKind::For { iter, body, .. } => {
            walk(iter, f);
            walk(body, f);
        }
        HKind::ForRange { start, end, body, .. } => {
            walk(start, f);
            walk(end, f);
            walk(body, f);
        }
        HKind::Break { value: x, .. } | HKind::Return(x) => {
            if let Some(x) = x {
                walk(x, f);
            }
        }
        HKind::Call { args, .. } | HKind::CallNative { args, .. } | HKind::CallTrait { args, .. } => {
            for a in args {
                if let HArg::Expr(x) = a {
                    walk(x, f);
                }
            }
        }
        HKind::Dict { args, .. } => args.iter().for_each(|x| walk(x, f)),
        HKind::CallValue { callee, args } => {
            walk(callee, f);
            args.iter().for_each(|x| walk(x, f));
        }
        HKind::Map(kvs) => {
            for (k, v) in kvs {
                walk(k, f);
                walk(v, f);
            }
        }
        HKind::Repeat { elem, count } => {
            walk(elem, f);
            walk(count, f);
        }
        HKind::PushLocal { value, .. } => walk(value, f),
        HKind::InsertLocal { key, value, .. } => {
            walk(key, f);
            walk(value, f);
        }
        HKind::Field { obj, .. } => walk(obj, f),
        HKind::Index { obj, index, .. } => {
            walk(obj, f);
            walk(index, f);
        }
        HKind::Slice { obj, start, end, .. } => {
            walk(obj, f);
            if let Some(x) = start {
                walk(x, f);
            }
            if let Some(x) = end {
                walk(x, f);
            }
        }
        HKind::Unary { e, .. } | HKind::ToStr(e) | HKind::Debug(e) | HKind::Spawn(e) | HKind::Defer(e) | HKind::Try { e, .. } => walk(e, f),
        HKind::Arith { l, r, .. } | HKind::Cmp { l, r, .. } | HKind::And(l, r) | HKind::Or(l, r) | HKind::Coalesce { l, r, .. } => {
            walk(l, f);
            walk(r, f);
        }
        HKind::Range { start, end, .. } => {
            walk(start, f);
            walk(end, f);
        }
        HKind::Lambda(l) => walk(&l.body, f),
        HKind::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    walk(x, f);
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
        match rt.block_on(engine.compile("main.sand", src, root, None)) {
            Ok(c) => c.program,
            Err(e) => panic!("{e}"),
        }
    }

    /// The first loop (from its back-edge target to the `Loop` instruction)
    /// of function `name`, and the code before it.
    fn first_loop<'p>(prog: &'p Program, name: &str) -> (&'p [Op], &'p [Op]) {
        let p = prog.protos.iter().find(|p| p.name == name).unwrap();
        let (at, off) =
            p.code.iter().enumerate().find_map(|(i, op)| if let Op::Loop { off } = op { Some((i, *off)) } else { None }).unwrap();
        let top = (at as i64 + 1 + off as i64) as usize;
        (&p.code[top..=at], &p.code[..top])
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
        assert_eq!(pre.iter().filter(|op| matches!(op, Op::HoistGlobal { .. })).count(), 2, "{pre:#?}");
    }

    #[test]
    fn mutable_globals_are_not_hoisted() {
        let prog = compile("var g = 1\nfn h(n: int) -> int {\n    var t = 0\n    for i in 0..n { t += g }\n    t\n}\nprint(h(3))");
        let (body, pre) = first_loop(&prog, "h");
        assert_eq!(body.iter().filter(|op| matches!(op, Op::LoadGlobal { .. })).count(), 1, "{body:#?}");
        assert!(!pre.iter().any(|op| matches!(op, Op::HoistGlobal { .. })), "{pre:#?}");
    }
}
