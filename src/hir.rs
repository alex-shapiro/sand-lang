//! The typed intermediate representation produced by the checker and consumed
//! by the bytecode compiler. Names are resolved, overloads and methods are
//! bound, and syntactic sugar is removed.

use crate::diag::Span;
use crate::types::{FnId, GlobalId, Type, TypeDefId};

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
    Break(Option<Box<HExpr>>),
    Continue,
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
