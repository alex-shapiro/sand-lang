//! The typed intermediate representation produced by the checker and consumed
//! by the bytecode compiler. Names are resolved, overloads and methods are
//! bound, and syntactic sugar is removed.

use crate::diag::Span;
use crate::types::{FnId, GlobalId, ImplId, Symbol, TraitId, TraitRef, TypeDefId, TypeId};

pub type LocalId = u32;

#[derive(Clone, Debug)]
pub struct LocalInfo {
    pub name: Symbol,
    pub mutable: bool,
    /// The lambda (0 = the enclosing function itself) that declares the local.
    pub owner: u32,
    /// Stored in a heap cell because a closure captures it and it is mutable
    /// (or it is a local function that may refer to itself).
    pub cell: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HExpr {
    pub kind: HKind,
    pub ty: TypeId,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HUnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IterKind {
    Range,
    List,
    /// The characters of a string (`s.chars()`, without building the list).
    Chars,
    Map,
    Set,
    Channel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqKind {
    List,
    Str,
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TryKind {
    Option,
    Result,
    TopLevelResult,
    TopLevelOption,
}

#[derive(Clone, Debug)]
pub enum HArg {
    Expr(HExpr),
    /// A default argument computed by `helper` from the arguments at `inputs`.
    Default {
        helper: FnId,
        inputs: Vec<usize>,
        ty: TypeId,
    },
}

#[derive(Clone, Debug)]
pub enum Place {
    Local(LocalId),
    Global(GlobalId),
    Field { obj: Box<HExpr>, index: u32 },
    Index { obj: Box<HExpr>, index: Box<HExpr>, map: bool },
}

#[derive(Clone, Debug)]
pub struct FormatSpec {
    pub fill: char,
    pub align: Option<char>,
    pub sign: bool,
    pub zero: bool,
    pub width: Option<usize>,
    pub precision: Option<usize>,
    pub kind: Option<char>,
}

#[derive(Clone, Debug)]
pub enum InterpPart {
    Text(String),
    Expr(Box<HExpr>, Option<FormatSpec>),
}

#[derive(Clone, Debug)]
pub struct HArm {
    pub pat: HPat,
    pub guard: Option<HExpr>,
    pub body: HExpr,
}

/// The evidence an [`HKind::Evidence`] placeholder stands for: `ty: tr`.
#[derive(Clone, Debug)]
pub struct EvidenceReq {
    pub ty: TypeId,
    pub tr: TraitRef,
    /// Set for the dictionaries of a function value whose parameter types
    /// mention `ty`: if `ty` remains unknown, no argument of that type can
    /// exist, so the function is never called and a placeholder suffices.
    pub lenient: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct HLambda {
    pub id: u32,
    pub name: String,
    pub params: Vec<LocalId>,
    pub body: HExpr,
    /// If this lambda is a local function bound to a cell local, that local.
    pub self_local: Option<LocalId>,
}

#[derive(Clone, Debug)]
pub enum HKind {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Dur(i64),
    Str(String),
    Bytes(Vec<u8>),
    Local(LocalId),
    Global(GlobalId),
    /// A script function used as a value.
    FnValue(FnId),
    /// A variant constructor used as a function value.
    VariantFn {
        rt_id: u32,
        arity: usize,
    },
    /// A payload-less variant.
    Tag(u32),
    Let {
        pat: HPat,
        init: Box<HExpr>,
        else_: Option<Box<HExpr>>,
    },
    /// Declares cell locals for a group of local functions, then assigns them.
    LocalFns {
        locals: Vec<LocalId>,
        lambdas: Vec<HExpr>,
    },
    Assign {
        place: Place,
        op: Option<ArithOp>,
        op_ty: TypeId,
        value: Box<HExpr>,
    },
    Block(Vec<HExpr>),
    If {
        cond: Box<HExpr>,
        then: Box<HExpr>,
        else_: Option<Box<HExpr>>,
    },
    IfLet {
        pat: HPat,
        value: Box<HExpr>,
        then: Box<HExpr>,
        else_: Option<Box<HExpr>>,
    },
    Match {
        scrut: Box<HExpr>,
        arms: Vec<HArm>,
    },
    Loop(Box<HExpr>),
    While {
        cond: Box<HExpr>,
        body: Box<HExpr>,
    },
    WhileLet {
        pat: HPat,
        value: Box<HExpr>,
        body: Box<HExpr>,
    },
    For {
        pat: HPat,
        iter: Box<HExpr>,
        kind: IterKind,
        body: Box<HExpr>,
    },
    /// `for` over a literal range, without materialising it.
    ForRange {
        pat: HPat,
        start: Box<HExpr>,
        end: Box<HExpr>,
        inclusive: bool,
        body: Box<HExpr>,
    },
    /// Leaves the loop `depth` loops out from the innermost (0) enclosing one.
    Break {
        value: Option<Box<HExpr>>,
        depth: u32,
    },
    Continue {
        depth: u32,
    },
    Return(Option<Box<HExpr>>),
    Call {
        func: FnId,
        args: Vec<HArg>,
    },
    CallNative {
        native: u32,
        args: Vec<HArg>,
    },
    /// Runtime description for a `reflect` type argument `ty`. If `ty`
    /// contains type parameters, `args` provides their descriptions in the
    /// order returned by `TypeArena::params_of`.
    TypeDesc {
        ty: TypeId,
        args: Vec<HExpr>,
    },
    CallValue {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
    },
    /// Calls method `slot` of trait `trait_id` through the dictionary passed
    /// as the first argument.
    CallTrait {
        trait_id: TraitId,
        slot: u32,
        args: Vec<HArg>,
        /// The impl the dictionary comes from, when known statically (the
        /// compiler then calls its method directly).
        impl_id: Option<ImplId>,
    },
    /// Evidence that a type implements a trait, needed by a call: the index
    /// of an [`EvidenceReq`] of the function being checked. Replaced by a
    /// dictionary expression (a hidden parameter, a projection of one, or a
    /// [`HKind::Dict`]) when the function has been checked.
    Evidence(u32),
    /// The dictionary of an impl: the trait's methods, followed by `args`
    /// (supertrait dictionaries, then the dictionaries for the impl's bounds).
    Dict {
        impl_id: ImplId,
        args: Vec<HExpr>,
    },
    MakeVariant {
        rt_id: u32,
        args: Vec<HExpr>,
    },
    MakeStruct {
        def: TypeDefId,
        fields: Vec<HExpr>,
    },
    Tuple(Vec<HExpr>),
    List(Vec<HExpr>),
    Map(Vec<(HExpr, HExpr)>),
    Repeat {
        elem: Box<HExpr>,
        count: Box<HExpr>,
    },
    /// Appends to a list local (comprehensions).
    PushLocal {
        list: LocalId,
        value: Box<HExpr>,
    },
    InsertLocal {
        map: LocalId,
        key: Box<HExpr>,
        value: Box<HExpr>,
    },
    Field {
        obj: Box<HExpr>,
        index: u32,
    },
    Index {
        obj: Box<HExpr>,
        index: Box<HExpr>,
        map: bool,
    },
    Slice {
        obj: Box<HExpr>,
        start: Option<Box<HExpr>>,
        end: Option<Box<HExpr>>,
        inclusive: bool,
        kind: SeqKind,
    },
    Unary {
        op: HUnOp,
        e: Box<HExpr>,
    },
    Arith {
        op: ArithOp,
        l: Box<HExpr>,
        r: Box<HExpr>,
    },
    Cmp {
        op: CmpOp,
        l: Box<HExpr>,
        r: Box<HExpr>,
    },
    And(Box<HExpr>, Box<HExpr>),
    Or(Box<HExpr>, Box<HExpr>),
    Lambda(Box<HLambda>),
    Try {
        e: Box<HExpr>,
        kind: TryKind,
    },
    Coalesce {
        l: Box<HExpr>,
        r: Box<HExpr>,
        result: bool,
    },
    Interp(Vec<InterpPart>),
    Range {
        start: Box<HExpr>,
        end: Box<HExpr>,
        inclusive: bool,
    },
    Spawn(Box<HExpr>),
    /// `defer e` (a statement of a block): `e` runs when the block exits.
    Defer(Box<HExpr>),
    /// The body of function `func` inlined at a call (the expression's
    /// span); the body keeps the spans of the function's own source.
    Inlined {
        func: FnId,
        body: Box<HExpr>,
    },
    /// Universal methods.
    ToStr(Box<HExpr>),
    Debug(Box<HExpr>),
    Error,
}

#[derive(Clone, Debug)]
pub struct HPat {
    pub kind: HPatKind,
    pub ty: TypeId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum HPatKind {
    Wildcard,
    Bind(LocalId),
    Int(i64),
    Str(String),
    Bool(bool),
    Unit,
    Range(i64, i64),
    Tuple(Vec<HPat>),
    /// Matches a variant by runtime id; `args` match the payload.
    Variant {
        rt_id: u32,
        args: Vec<HPat>,
    },
    Struct {
        fields: Vec<(u32, HPat)>,
    },
    List {
        before: Vec<HPat>,
        rest: Option<Option<LocalId>>,
        after: Vec<HPat>,
    },
    Or(Vec<HPat>),
}

/// A part of an expression or pattern that a pass may visit or rewrite.
pub enum Part<'a> {
    Expr(&'a mut HExpr),
    Pat(&'a mut HPat),
    Type(&'a mut TypeId),
}

impl HExpr {
    /// Calls `f` on each direct part of the expression: its subexpressions
    /// (including lambda bodies, the expressions of places and arguments),
    /// patterns, and types other than `self.ty`.
    pub fn for_each_part(&mut self, f: &mut impl FnMut(Part<'_>)) {
        let mut e = |x: &mut HExpr| f(Part::Expr(x));
        match &mut self.kind {
            HKind::Let { pat, init, else_ } => {
                f(Part::Pat(pat));
                f(Part::Expr(init));
                if let Some(x) = else_ {
                    f(Part::Expr(x));
                }
            }
            HKind::LocalFns { lambdas, .. } => lambdas.iter_mut().for_each(e),
            HKind::Assign { place, op_ty, value, .. } => {
                f(Part::Type(op_ty));
                match place {
                    Place::Field { obj, .. } => f(Part::Expr(obj)),
                    Place::Index { obj, index, .. } => {
                        f(Part::Expr(obj));
                        f(Part::Expr(index));
                    }
                    Place::Local(_) | Place::Global(_) => {}
                }
                f(Part::Expr(value));
            }
            HKind::Block(xs)
            | HKind::Tuple(xs)
            | HKind::List(xs)
            | HKind::MakeVariant { args: xs, .. }
            | HKind::MakeStruct { fields: xs, .. }
            | HKind::Dict { args: xs, .. } => xs.iter_mut().for_each(e),
            HKind::If { cond, then, else_ } => {
                e(cond);
                e(then);
                if let Some(x) = else_ {
                    e(x);
                }
            }
            HKind::IfLet { pat, value, then, else_ } => {
                f(Part::Pat(pat));
                f(Part::Expr(value));
                f(Part::Expr(then));
                if let Some(x) = else_ {
                    f(Part::Expr(x));
                }
            }
            HKind::Match { scrut, arms } => {
                f(Part::Expr(scrut));
                for a in arms {
                    f(Part::Pat(&mut a.pat));
                    if let Some(g) = &mut a.guard {
                        f(Part::Expr(g));
                    }
                    f(Part::Expr(&mut a.body));
                }
            }
            HKind::Loop(b) => e(b),
            HKind::While { cond, body } => {
                e(cond);
                e(body);
            }
            HKind::WhileLet { pat, value: a, body: b } | HKind::For { pat, iter: a, body: b, .. } => {
                f(Part::Pat(pat));
                f(Part::Expr(a));
                f(Part::Expr(b));
            }
            HKind::ForRange { pat, start, end, body, .. } => {
                f(Part::Pat(pat));
                f(Part::Expr(start));
                f(Part::Expr(end));
                f(Part::Expr(body));
            }
            HKind::Break { value: x, .. } | HKind::Return(x) => {
                if let Some(x) = x {
                    e(x);
                }
            }
            HKind::Call { args, .. } | HKind::CallTrait { args, .. } | HKind::CallNative { args, .. } => args_parts(args, f),
            HKind::TypeDesc { ty, args } => {
                args.iter_mut().for_each(e);
                f(Part::Type(ty));
            }
            HKind::CallValue { callee, args } => {
                e(callee);
                args.iter_mut().for_each(e);
            }
            HKind::Map(kvs) => {
                for (k, v) in kvs {
                    e(k);
                    e(v);
                }
            }
            HKind::Repeat { elem: a, count: b } | HKind::InsertLocal { key: a, value: b, .. } | HKind::Index { obj: a, index: b, .. } => {
                e(a);
                e(b);
            }
            HKind::PushLocal { value: x, .. } | HKind::Field { obj: x, .. } => e(x),
            HKind::Slice { obj, start, end, .. } => {
                e(obj);
                if let Some(x) = start {
                    e(x);
                }
                if let Some(x) = end {
                    e(x);
                }
            }
            HKind::Unary { e: x, .. }
            | HKind::ToStr(x)
            | HKind::Debug(x)
            | HKind::Spawn(x)
            | HKind::Defer(x)
            | HKind::Try { e: x, .. }
            | HKind::Inlined { body: x, .. } => e(x),
            HKind::Arith { l, r, .. }
            | HKind::Cmp { l, r, .. }
            | HKind::And(l, r)
            | HKind::Or(l, r)
            | HKind::Coalesce { l, r, .. }
            | HKind::Range { start: l, end: r, .. } => {
                e(l);
                e(r);
            }
            HKind::Lambda(l) => e(&mut l.body),
            HKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        e(x);
                    }
                }
            }
            HKind::Unit
            | HKind::Bool(_)
            | HKind::Int(_)
            | HKind::Float(_)
            | HKind::Dur(_)
            | HKind::Str(_)
            | HKind::Bytes(_)
            | HKind::Local(_)
            | HKind::Global(_)
            | HKind::FnValue(_)
            | HKind::VariantFn { .. }
            | HKind::Tag(_)
            | HKind::Continue { .. }
            | HKind::Evidence(_)
            | HKind::Error => {}
        }
    }

    /// Pre-order traversal of every expression (including inside lambdas,
    /// places and arguments) that may replace nodes.
    pub fn visit_mut(&mut self, f: &mut impl FnMut(&mut HExpr)) {
        f(self);
        self.for_each_part(&mut |p| {
            if let Part::Expr(x) = p {
                x.visit_mut(f)
            }
        });
    }
}

fn args_parts(args: &mut [HArg], f: &mut impl FnMut(Part<'_>)) {
    for a in args {
        match a {
            HArg::Expr(x) => f(Part::Expr(x)),
            HArg::Default { ty, .. } => f(Part::Type(ty)),
        }
    }
}

impl HPat {
    pub fn is_irrefutable_bind(&self) -> Option<LocalId> {
        match self.kind {
            HPatKind::Bind(l) => Some(l),
            _ => None,
        }
    }

    pub fn for_each_subpattern(&mut self, f: &mut impl FnMut(&mut HPat)) {
        match &mut self.kind {
            HPatKind::Tuple(ps) | HPatKind::Or(ps) | HPatKind::Variant { args: ps, .. } => ps.iter_mut().for_each(f),
            HPatKind::Struct { fields } => fields.iter_mut().for_each(|(_, p)| f(p)),
            HPatKind::List { before, after, .. } => before.iter_mut().chain(after).for_each(f),
            HPatKind::Wildcard
            | HPatKind::Bind(_)
            | HPatKind::Int(_)
            | HPatKind::Str(_)
            | HPatKind::Bool(_)
            | HPatKind::Unit
            | HPatKind::Range(..) => {}
        }
    }
}

/// A checked function body.
#[derive(Clone, Debug)]
pub struct HFn {
    pub name: String,
    pub params: Vec<LocalId>,
    pub body: HExpr,
    pub locals: Vec<LocalInfo>,
    pub span: Span,
    /// The functions that `help` and `members` calls in the body name, which
    /// the `unused-function` lint counts as used.
    pub doc_refs: Vec<FnId>,
}
