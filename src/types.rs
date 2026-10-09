//! Types, type definitions and bounds.

use crate::diag::Span;
use std::fmt::Write as _;

pub type TypeDefId = u32;
pub type ParamId = u32;
pub type VarId = u32;
pub type FnId = u32;
pub type ModId = u32;
pub type GlobalId = u32;
pub type TraitId = u32;
pub type ImplId = u32;

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Duration,
    Unit,
    Never,
    Range,
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    Set(Box<Type>),
    Tuple(Vec<Type>),
    Fn(Vec<Type>, Box<Type>),
    Named(TypeDefId, Vec<Type>),
    /// A rigid type parameter of the function being checked.
    Param(ParamId),
    /// An inference variable.
    Var(VarId),
    /// Poison type after an error; unifies with everything.
    Error,
}

impl Type {
    pub fn list(t: Type) -> Type {
        Type::List(Box::new(t))
    }
    pub fn map(k: Type, v: Type) -> Type {
        Type::Map(Box::new(k), Box::new(v))
    }
    pub fn func(ps: Vec<Type>, r: Type) -> Type {
        Type::Fn(ps, Box::new(r))
    }
    pub fn is_never(&self) -> bool {
        matches!(self, Type::Never)
    }

    /// Visits every component type.
    pub fn walk(&self, f: &mut impl FnMut(&Type)) {
        f(self);
        match self {
            Type::List(t) | Type::Set(t) => t.walk(f),
            Type::Map(k, v) => {
                k.walk(f);
                v.walk(f)
            }
            Type::Tuple(ts) | Type::Named(_, ts) => ts.iter().for_each(|t| t.walk(f)),
            Type::Fn(ps, r) => {
                ps.iter().for_each(|t| t.walk(f));
                r.walk(f)
            }
            _ => {}
        }
    }

    pub fn has_vars(&self) -> bool {
        let mut found = false;
        self.walk(&mut |t| {
            if matches!(t, Type::Var(_)) {
                found = true
            }
        });
        found
    }

    /// Replaces type parameters according to `f`.
    pub fn subst_params(&self, f: &impl Fn(ParamId) -> Option<Type>) -> Type {
        match self {
            Type::Param(p) => f(*p).unwrap_or(Type::Param(*p)),
            Type::List(t) => Type::List(Box::new(t.subst_params(f))),
            Type::Set(t) => Type::Set(Box::new(t.subst_params(f))),
            Type::Map(k, v) => Type::Map(Box::new(k.subst_params(f)), Box::new(v.subst_params(f))),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| t.subst_params(f)).collect()),
            Type::Named(d, ts) => Type::Named(*d, ts.iter().map(|t| t.subst_params(f)).collect()),
            Type::Fn(ps, r) => Type::Fn(ps.iter().map(|t| t.subst_params(f)).collect(), Box::new(r.subst_params(f))),
            other => other.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bound {
    Eq,
    Ord,
    Hash,
    Num,
}

impl Bound {
    pub fn parse(s: &str) -> Option<Bound> {
        Some(match s {
            "Eq" => Bound::Eq,
            "Ord" => Bound::Ord,
            "Hash" => Bound::Hash,
            "Num" => Bound::Num,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Bound::Eq => "Eq",
            Bound::Ord => "Ord",
            Bound::Hash => "Hash",
            Bound::Num => "Num",
        }
    }
}

/// A user-defined trait applied to type arguments (`Into<str>`); the
/// implementing type is given separately.
#[derive(Clone, Debug, PartialEq)]
pub struct TraitRef {
    pub trait_id: TraitId,
    pub args: Vec<Type>,
}

impl TraitRef {
    pub fn subst_params(&self, f: &impl Fn(ParamId) -> Option<Type>) -> TraitRef {
        TraitRef { trait_id: self.trait_id, args: self.args.iter().map(|t| t.subst_params(f)).collect() }
    }
}

#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub name: String,
    /// Built-in bounds, including those implied by the supertraits of `traits`.
    pub bounds: Vec<Bound>,
    /// User-defined trait bounds.
    pub traits: Vec<TraitRef>,
    pub reflect: bool,
}

impl ParamInfo {
    pub fn has_bound(&self, b: Bound) -> bool {
        self.bounds.iter().any(|&x| {
            x == b || (b == Bound::Eq && matches!(x, Bound::Ord | Bound::Hash | Bound::Num)) || (b == Bound::Ord && x == Bound::Num)
        })
    }
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: Type,
    pub mutable: bool,
    /// Helper function computing the default value.
    pub default: Option<FnId>,
    /// The default as a constant, when it is one (used by `json.decode`).
    pub const_default: Option<ConstVal>,
    pub doc: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConstVal {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Dur(i64),
    EmptyList,
    EmptyMap,
    None,
}

#[derive(Clone, Debug)]
pub struct VariantDef {
    pub name: String,
    pub fields: Vec<Type>,
    pub field_names: Option<Vec<String>>,
    /// Program-wide runtime id of this variant.
    pub rt_id: u32,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub enum TypeDefKind {
    Struct { fields: Vec<FieldDef> },
    Enum { variants: Vec<VariantDef> },
    Opaque,
}

#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: String,
    pub module: ModId,
    pub module_name: String,
    pub generics: Vec<ParamId>,
    pub kind: TypeDefKind,
    pub is_pub: bool,
    pub doc: Option<String>,
    pub span: Span,
}

impl TypeDef {
    pub fn fields(&self) -> &[FieldDef] {
        match &self.kind {
            TypeDefKind::Struct { fields } => fields,
            _ => &[],
        }
    }
    pub fn variants(&self) -> &[VariantDef] {
        match &self.kind {
            TypeDefKind::Enum { variants } => variants,
            _ => &[],
        }
    }
    pub fn is_struct(&self) -> bool {
        matches!(self.kind, TypeDefKind::Struct { .. })
    }
    pub fn is_enum(&self) -> bool {
        matches!(self.kind, TypeDefKind::Enum { .. })
    }
}

/// The well-known type definitions every program has.
#[derive(Clone, Copy, Debug, Default)]
pub struct WellKnown {
    pub option: TypeDefId,
    pub result: TypeDefId,
    pub error: TypeDefId,
    pub task: TypeDefId,
    pub channel: TypeDefId,
    pub json: Option<TypeDefId>,
}

/// Formats types for diagnostics.
pub struct TypePrinter<'a> {
    pub defs: &'a [TypeDef],
    pub params: &'a [ParamInfo],
    pub wk: WellKnown,
}

impl TypePrinter<'_> {
    pub fn show(&self, t: &Type) -> String {
        self.show_tagged(t, &|_| None)
    }

    /// Like `show`, adding `tag(d)` (if any) in parentheses after the name
    /// of each type definition `d`.
    pub fn show_tagged(&self, t: &Type, tag: &dyn Fn(TypeDefId) -> Option<String>) -> String {
        let mut s = String::new();
        Tagged { p: self, tag }.write(&mut s, t);
        s
    }
}

struct Tagged<'p, 'a> {
    p: &'p TypePrinter<'a>,
    tag: &'p dyn Fn(TypeDefId) -> Option<String>,
}

impl Tagged<'_, '_> {
    fn write(&self, s: &mut String, t: &Type) {
        let TypePrinter { defs, params, wk } = self.p;
        match t {
            Type::Int => s.push_str("int"),
            Type::Float => s.push_str("float"),
            Type::Bool => s.push_str("bool"),
            Type::Str => s.push_str("str"),
            Type::Bytes => s.push_str("bytes"),
            Type::Duration => s.push_str("duration"),
            Type::Unit => s.push_str("()"),
            Type::Never => s.push_str("never"),
            Type::Range => s.push_str("Range"),
            Type::Error => s.push_str("{error}"),
            Type::Var(_) => s.push('_'),
            Type::Param(p) => s.push_str(params.get(*p as usize).map(|p| p.name.as_str()).unwrap_or("?")),
            Type::List(t) => {
                s.push('[');
                self.write(s, t);
                s.push(']');
            }
            Type::Map(k, v) => {
                s.push('[');
                self.write(s, k);
                s.push_str(": ");
                self.write(s, v);
                s.push(']');
            }
            Type::Set(t) => {
                s.push_str("Set<");
                self.write(s, t);
                s.push('>');
            }
            Type::Tuple(ts) => {
                s.push('(');
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    self.write(s, t);
                }
                s.push(')');
            }
            Type::Fn(ps, r) => {
                s.push_str("fn(");
                for (i, t) in ps.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    self.write(s, t);
                }
                s.push(')');
                if **r != Type::Unit {
                    s.push_str(" -> ");
                    self.write(s, r);
                }
            }
            Type::Named(d, args) => {
                // (`Option<Point (defined in ...)>` rather than a `?` after the tag.)
                let tagged = matches!(&args[..], [Type::Named(e, _)] if (self.tag)(*e).is_some());
                if *d == wk.option && args.len() == 1 && !tagged {
                    let needs_parens = matches!(args[0], Type::Fn(..));
                    if needs_parens {
                        s.push('(');
                    }
                    self.write(s, &args[0]);
                    if needs_parens {
                        s.push(')');
                    }
                    s.push('?');
                    return;
                }
                let name = defs.get(*d as usize).map(|d| d.name.as_str()).unwrap_or("?");
                s.push_str(name);
                if !args.is_empty() {
                    s.push('<');
                    for (i, t) in args.iter().enumerate() {
                        if i > 0 {
                            s.push_str(", ");
                        }
                        self.write(s, t);
                    }
                    s.push('>');
                }
                if let Some(tag) = (self.tag)(*d) {
                    let _ = write!(s, " ({tag})");
                }
            }
        }
    }
}

impl TypePrinter<'_> {
    /// `fn name(a: int, b: str = ...) -> T`
    pub fn signature(&self, name: &str, params: &[(String, Type, bool)], ret: &Type) -> String {
        let mut s = format!("fn {name}(");
        for (i, (n, t, d)) in params.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            if n == "self" {
                s.push_str("self");
                continue;
            }
            let _ = write!(s, "{n}: {}", self.show(t));
            if *d {
                s.push_str(" = ...");
            }
        }
        s.push(')');
        if *ret != Type::Unit {
            let _ = write!(s, " -> {}", self.show(ret));
        }
        s
    }
}
