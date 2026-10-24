//! The abstract syntax tree.

use crate::diag::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub items: Vec<Item>,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Import(Import),
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Extend(ExtendDecl),
    Trait(TraitDecl),
    Impl(ImplDecl),
    TypeAlias(TypeAlias),
    Test(TestDecl),
    Stmt(Stmt),
}

#[derive(Clone, Debug)]
pub enum ImportSource {
    /// `import json` — a standard or host module name.
    Name(Ident),
    /// `import "./x.sand"` — a script path, with the span of the literal.
    Path(String, Span),
}

#[derive(Clone, Debug)]
pub struct Import {
    pub source: ImportSource,
    pub alias: Option<Ident>,
    /// `from X import a, b`
    pub names: Option<Vec<Ident>>,
}

#[derive(Clone, Debug)]
pub struct GenericParam {
    pub name: Ident,
    /// The traits that the type must implement (`Ord`, `Describe`,
    /// `geo.Shape`, `Into<str>`), written like named types.
    pub traits: Vec<TypeExpr>,
    pub reflect: bool,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: Ident,
    /// `None` only for `self`.
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct WherePredicate {
    pub name: Ident,
    pub traits: Vec<TypeExpr>,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub where_clause: Vec<WherePredicate>,
    pub body: Option<Block>,
    pub is_pub: bool,
    pub is_native: bool,
    pub doc: Option<String>,
    pub span: Span,
}

impl FnDecl {
    pub fn has_self(&self) -> bool {
        self.params.first().map(|p| p.name.name == "self" && p.ty.is_none()).unwrap_or(false)
    }
}

#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub name: Ident,
    pub ty: TypeExpr,
    pub default: Option<Expr>,
    pub mutable: bool,
    pub is_pub: bool,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<FieldDecl>,
    pub methods: Vec<FnDecl>,
    pub is_pub: bool,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VariantField {
    pub name: Option<Ident>,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug)]
pub struct VariantDecl {
    pub name: Ident,
    pub fields: Vec<VariantField>,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub variants: Vec<VariantDecl>,
    pub methods: Vec<FnDecl>,
    pub is_pub: bool,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExtendDecl {
    pub generics: Vec<GenericParam>,
    pub target: TypeExpr,
    pub methods: Vec<FnDecl>,
    pub doc: Option<String>,
}

/// `trait Name<T>: Super + Eq { fn required(self) -> T  fn provided(self) { ... } }`
#[derive(Clone, Debug)]
pub struct TraitDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub supers: Vec<TypeExpr>,
    /// Required methods have no body; default methods do.
    pub methods: Vec<FnDecl>,
    pub is_pub: bool,
    pub doc: Option<String>,
}

/// `impl<T: B> Trait<Args> for Type where T: C { methods }`
#[derive(Clone, Debug)]
pub struct ImplDecl {
    pub generics: Vec<GenericParam>,
    /// The trait, written like a named type.
    pub trait_ref: TypeExpr,
    pub target: TypeExpr,
    pub where_clause: Vec<WherePredicate>,
    pub methods: Vec<FnDecl>,
}

#[derive(Clone, Debug)]
pub struct TypeAlias {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub ty: TypeExpr,
    pub is_pub: bool,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TestDecl {
    pub name: String,
    pub name_span: Span,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeExprKind {
    /// `int`, `Point`, `json.Json`, `Result<T, E>`, `Self`
    Named {
        path: Vec<Ident>,
        args: Vec<TypeExpr>,
    },
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>, Box<TypeExpr>),
    Tuple(Vec<TypeExpr>),
    Option(Box<TypeExpr>),
    Fn(Vec<TypeExpr>, Option<Box<TypeExpr>>),
    Unit,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Let {
        pattern: Pattern,
        ty: Option<TypeExpr>,
        value: Expr,
        else_block: Option<Block>,
        mutable: bool,
        is_pub: bool,
        doc: Option<String>,
    },
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    Expr(Expr),
    Fn(FnDecl),
    /// `defer expr`: runs `expr` when the enclosing block exits.
    Defer(Expr),
    /// `use name = value` (or `use _ = value`): binds `value`, whose type
    /// implements `Close`, and closes it when the enclosing block exits.
    Use {
        /// The name bound (`_` binds none).
        name: Ident,
        ty: Option<TypeExpr>,
        value: Expr,
    },
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
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
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn text(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            Pow => "**",
            BitAnd => "&",
            BitOr => "|",
            BitXor => "^",
            Shl => "<<",
            Shr => ">>",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            And => "and",
            Or => "or",
        }
    }
    pub fn is_comparison(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Clone, Debug)]
pub enum StrSeg {
    Text(String),
    Expr(Box<Expr>, Option<FormatSpecAst>),
}

#[derive(Clone, Debug)]
pub struct FormatSpecAst {
    pub text: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub enum CompClause {
    For(Pattern, Expr),
    If(Expr),
}

#[derive(Clone, Debug)]
pub enum Cond {
    Expr(Box<Expr>),
    Let(Box<Pattern>, Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct LambdaParam {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
}

#[derive(Clone, Debug)]
pub struct FieldInit {
    pub name: Ident,
    /// `None` for shorthand `Point { x }`.
    pub value: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Unit,
    Bool(bool),
    Int(i64),
    /// An integer literal outside i64 range; `true` if it is exactly 2^63.
    IntOverflow(bool),
    Float(f64),
    Duration(i64),
    Str(Vec<StrSeg>),
    Bytes(Vec<u8>),
    Ident(Ident),
    SelfValue,
    /// `Self` used as a value namespace (`Self.new()`).
    SelfType,
    List(Vec<Expr>),
    Repeat(Box<Expr>, Box<Expr>),
    Map(Vec<(Expr, Expr)>),
    ListComp(Box<Expr>, Vec<CompClause>),
    MapComp(Box<Expr>, Box<Expr>, Vec<CompClause>),
    Tuple(Vec<Expr>),
    Paren(Box<Expr>),
    StructLit {
        path: Vec<Ident>,
        fields: Vec<FieldInit>,
        base: Option<Box<Expr>>,
    },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Coalesce(Box<Expr>, Box<Expr>),
    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    MethodCall {
        receiver: Box<Expr>,
        method: Ident,
        args: Vec<Arg>,
    },
    Field(Box<Expr>, Ident),
    TupleField(Box<Expr>, u32, Span),
    Index(Box<Expr>, Box<Expr>),
    Try(Box<Expr>),
    If {
        cond: Cond,
        then: Block,
        else_: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Block(Block),
    /// Loops may be labeled (`@outer loop { ... }`) for `break @outer`.
    Loop {
        label: Option<Ident>,
        body: Block,
    },
    While {
        label: Option<Ident>,
        cond: Cond,
        body: Block,
    },
    For {
        label: Option<Ident>,
        pattern: Box<Pattern>,
        iter: Box<Expr>,
        body: Block,
    },
    Lambda {
        params: Vec<LambdaParam>,
        body: Box<Expr>,
        parens: bool,
    },
    Return(Option<Box<Expr>>),
    /// `break`, `break value`, `break @label`, `break @label value`.
    Break {
        label: Option<Ident>,
        value: Option<Box<Expr>>,
    },
    Continue(Option<Ident>),
    Spawn(Box<Expr>),
    /// A macro call: its source form for the formatter, and its expansion for the other passes.
    Macro(Box<MacroCall>),
    /// A standard module referenced by macro expansion.
    ModuleRef(String),
    /// Placeholder produced after a parse error.
    Error,
}

/// A data macro: the standard module that its expansion calls, and the paths in it that build each kind of value.
pub struct DataMacro {
    pub name: &'static str,
    pub module: &'static str,
    pub null: &'static [&'static str],
    pub list: &'static [&'static str],
    pub object: &'static [&'static str],
    pub value: &'static [&'static str],
}

/// The macros.
pub const MACROS: [DataMacro; 1] = [DataMacro {
    name: "json",
    module: "json",
    null: &["Json", "Null"],
    list: &["Json", "Arr"],
    object: &["Json", "Obj"],
    value: &["to_json"],
}];

/// A call `name!(arg)` of a data macro.
#[derive(Clone, Debug)]
pub struct MacroCall {
    pub name: Ident,
    pub arg: Data,
    pub expansion: Expr,
}

/// The argument of a data macro: JSON syntax whose values can be Sand expressions.
#[derive(Clone, Debug)]
pub enum Data {
    Null(Span),
    Value(Expr),
    List(Vec<Data>, Span),
    /// An object, written `{k: v}` or `[k: v]`.
    Object(Vec<(Expr, Data)>, Span),
}

impl Data {
    pub fn span(&self) -> Span {
        match self {
            Data::Null(s) | Data::List(_, s) | Data::Object(_, s) => *s,
            Data::Value(e) => e.span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PatLit {
    Int(i64),
    Str(String),
    Bool(bool),
    Unit,
}

#[derive(Clone, Debug)]
pub struct FieldPat {
    pub name: Ident,
    pub pattern: Option<Pattern>,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    Wildcard,
    /// A plain name: a binding, or a payload-less variant.
    Binding(Ident),
    Literal(PatLit),
    Range(i64, i64),
    Tuple(Vec<Pattern>),
    /// `Name(args)` or `Enum.Name(args)` or qualified `Enum.Name`.
    Variant {
        path: Vec<Ident>,
        args: Option<Vec<Pattern>>,
    },
    Struct {
        path: Vec<Ident>,
        fields: Vec<FieldPat>,
        rest: bool,
    },
    /// `[a, b, ..rest, z]`; `rest` is `Some(None)` for `..` and `Some(Some(name))` for `..name`.
    List {
        before: Vec<Pattern>,
        rest: Option<Option<Ident>>,
        after: Vec<Pattern>,
    },
    Or(Vec<Pattern>),
}

impl Pattern {
    /// Names bound by the pattern (in order of appearance).
    pub fn bindings(&self, out: &mut Vec<Ident>) {
        match &self.kind {
            PatternKind::Binding(id) => out.push(id.clone()),
            PatternKind::Tuple(ps) => ps.iter().for_each(|p| p.bindings(out)),
            PatternKind::Variant { args: Some(ps), .. } => ps.iter().for_each(|p| p.bindings(out)),
            PatternKind::Struct { fields, .. } => {
                for f in fields {
                    match &f.pattern {
                        Some(p) => p.bindings(out),
                        None => out.push(f.name.clone()),
                    }
                }
            }
            PatternKind::List { before, rest, after } => {
                before.iter().for_each(|p| p.bindings(out));
                if let Some(Some(r)) = rest {
                    out.push(r.clone());
                }
                after.iter().for_each(|p| p.bindings(out));
            }
            PatternKind::Or(ps) => {
                if let Some(p) = ps.first() {
                    p.bindings(out)
                }
            }
            _ => {}
        }
    }
}
