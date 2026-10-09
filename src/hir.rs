//! The typed intermediate representation produced by the checker and consumed
//! by the bytecode compiler. Names are resolved, overloads and methods are
//! bound, and syntactic sugar is removed.

use crate::diag::Span;
use crate::types::{FnId, GlobalId, ImplId, TraitId, TraitRef, Type, TypeDefId};

pub type LocalId = u32;

#[derive(Clone, Debug)]
pub struct LocalInfo {
    pub name: String,
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
    pub ty: Type,
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
    /// `?` on a `Result` at the top level of the main module.
    TopLevel,
}

#[derive(Clone, Debug)]
pub enum HArg {
    Expr(HExpr),
    /// A default argument computed by `helper` from the arguments at `inputs`.
    Default {
        helper: FnId,
        inputs: Vec<usize>,
        ty: Type,
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
    pub ty: Type,
    pub tr: TraitRef,
    /// Set for the dictionaries of a function value whose parameter types
    /// mention `ty`: if `ty` remains unknown, no argument of that type can
    /// exist, so the function is never called and a placeholder suffices.
    pub lenient: bool,
}

#[derive(Clone, Debug)]
pub struct HLambda {
    pub id: u32,
    pub name: String,
    pub params: Vec<LocalId>,
    /// Destructuring for parameters (lambdas take plain names, so usually empty).
    pub body: HExpr,
    pub doc: Option<String>,
    pub signature: String,
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
        op_ty: Type,
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
        reflect: Vec<Type>,
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
    /// Evidence that a type implements a trait, needed by a call. Replaced by
    /// a dictionary expression (a hidden parameter, a projection of one, or a
    /// [`HKind::Dict`]) when the enclosing function has been checked.
    Evidence(Box<EvidenceReq>),
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
    /// Universal methods.
    ToStr(Box<HExpr>),
    Debug(Box<HExpr>),
    Error,
}

#[derive(Clone, Debug)]
pub struct HPat {
    pub kind: HPatKind,
    pub ty: Type,
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

impl HExpr {
    /// Pre-order traversal of every expression (including inside lambdas,
    /// places and arguments) that may replace nodes.
    pub fn visit_mut(&mut self, f: &mut impl FnMut(&mut HExpr)) {
        f(self);
        match &mut self.kind {
            HKind::Let { init, else_, .. } => {
                init.visit_mut(f);
                if let Some(x) = else_ {
                    x.visit_mut(f);
                }
            }
            HKind::LocalFns { lambdas, .. } => lambdas.iter_mut().for_each(|x| x.visit_mut(f)),
            HKind::Assign { place, value, .. } => {
                match place {
                    Place::Field { obj, .. } => obj.visit_mut(f),
                    Place::Index { obj, index, .. } => {
                        obj.visit_mut(f);
                        index.visit_mut(f);
                    }
                    _ => {}
                }
                value.visit_mut(f);
            }
            HKind::Block(xs)
            | HKind::Tuple(xs)
            | HKind::List(xs)
            | HKind::MakeVariant { args: xs, .. }
            | HKind::MakeStruct { fields: xs, .. }
            | HKind::Dict { args: xs, .. } => xs.iter_mut().for_each(|x| x.visit_mut(f)),
            HKind::If { cond, then, else_ } => {
                cond.visit_mut(f);
                then.visit_mut(f);
                if let Some(x) = else_ {
                    x.visit_mut(f);
                }
            }
            HKind::IfLet { value, then, else_, .. } => {
                value.visit_mut(f);
                then.visit_mut(f);
                if let Some(x) = else_ {
                    x.visit_mut(f);
                }
            }
            HKind::Match { scrut, arms } => {
                scrut.visit_mut(f);
                for a in arms {
                    if let Some(g) = &mut a.guard {
                        g.visit_mut(f);
                    }
                    a.body.visit_mut(f);
                }
            }
            HKind::Loop(b) => b.visit_mut(f),
            HKind::While { cond: a, body: b } | HKind::WhileLet { value: a, body: b, .. } | HKind::For { iter: a, body: b, .. } => {
                a.visit_mut(f);
                b.visit_mut(f);
            }
            HKind::ForRange { start, end, body, .. } => {
                start.visit_mut(f);
                end.visit_mut(f);
                body.visit_mut(f);
            }
            HKind::Break { value: x, .. } | HKind::Return(x) => {
                if let Some(x) = x {
                    x.visit_mut(f);
                }
            }
            HKind::Call { args, .. } | HKind::CallNative { args, .. } | HKind::CallTrait { args, .. } => {
                for a in args {
                    if let HArg::Expr(x) = a {
                        x.visit_mut(f);
                    }
                }
            }
            HKind::CallValue { callee, args } => {
                callee.visit_mut(f);
                args.iter_mut().for_each(|x| x.visit_mut(f));
            }
            HKind::Map(kvs) => {
                for (k, v) in kvs {
                    k.visit_mut(f);
                    v.visit_mut(f);
                }
            }
            HKind::Repeat { elem: a, count: b } | HKind::InsertLocal { key: a, value: b, .. } | HKind::Index { obj: a, index: b, .. } => {
                a.visit_mut(f);
                b.visit_mut(f);
            }
            HKind::PushLocal { value: x, .. } | HKind::Field { obj: x, .. } => x.visit_mut(f),
            HKind::Slice { obj, start, end, .. } => {
                obj.visit_mut(f);
                if let Some(x) = start {
                    x.visit_mut(f);
                }
                if let Some(x) = end {
                    x.visit_mut(f);
                }
            }
            HKind::Unary { e, .. } | HKind::ToStr(e) | HKind::Debug(e) | HKind::Spawn(e) | HKind::Defer(e) | HKind::Try { e, .. } => {
                e.visit_mut(f)
            }
            HKind::Arith { l, r, .. }
            | HKind::Cmp { l, r, .. }
            | HKind::And(l, r)
            | HKind::Or(l, r)
            | HKind::Coalesce { l, r, .. }
            | HKind::Range { start: l, end: r, .. } => {
                l.visit_mut(f);
                r.visit_mut(f);
            }
            HKind::Lambda(l) => l.body.visit_mut(f),
            HKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(x, _) = p {
                        x.visit_mut(f);
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
}

impl HPat {
    pub fn is_irrefutable_bind(&self) -> Option<LocalId> {
        match self.kind {
            HPatKind::Bind(l) => Some(l),
            _ => None,
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
}
