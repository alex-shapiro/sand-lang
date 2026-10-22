//! Sand types described by Rust types.

use crate::vm::value::{Value, ValueType};
use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;

/// A Sand type, as a Rust type describes it (see [`SandType`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
    Set(Box<Type>),
    Map(Box<Type>, Box<Type>),
    Tuple(Vec<Type>),
    Fn(Vec<Type>, Box<Type>),
    /// The type parameter at this index in the native function's type
    /// parameters: those of its `extend` block or impl, then its own.
    Param(u8),
    /// The runtime description of the type parameter at this index, which
    /// the declaration marks `reflect`. Only leading parameters have it.
    Reflect(u8),
    /// A type that a module declares.
    Named(Named),
}

/// A type that a module declares, with its type arguments.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Named {
    pub module: &'static str,
    pub name: &'static str,
    pub args: Vec<Type>,
    /// The Rust type that the module binds to the Sand type, if any.
    pub rust: Option<TypeId>,
}

impl Type {
    /// The type `name` that `module` declares in Sand source.
    pub fn named(module: &'static str, name: &'static str, args: Vec<Type>) -> Type {
        Type::Named(Named { module, name, args, rust: None })
    }

    /// The Sand type of the Rust type `T`, which `module` binds to `name`.
    pub fn bound<T: 'static>(module: &'static str, name: &'static str) -> Type {
        Type::Named(Named { module, name, args: Vec::new(), rust: Some(TypeId::of::<T>()) })
    }

    pub fn option(t: Type) -> Type {
        Type::named("prelude", "Option", vec![t])
    }

    pub fn result(t: Type, e: Type) -> Type {
        Type::named("prelude", "Result", vec![t, e])
    }

    /// The prelude's `Error`.
    pub fn error() -> Type {
        Type::named("prelude", "Error", Vec::new())
    }

    /// Calls `f` on this type and each type in it.
    pub(crate) fn visit(&self, f: &mut impl FnMut(&Type)) {
        f(self);
        match self {
            Type::List(t) | Type::Set(t) => t.visit(f),
            Type::Map(k, v) => {
                k.visit(f);
                v.visit(f);
            }
            Type::Tuple(ts) => ts.iter().for_each(|t| t.visit(f)),
            Type::Fn(ps, r) => {
                ps.iter().for_each(|t| t.visit(f));
                r.visit(f);
            }
            Type::Named(n) => n.args.iter().for_each(|t| t.visit(f)),
            _ => {}
        }
    }

    /// The Sand syntax of the type, with names of `module` and the prelude
    /// unqualified.
    pub(crate) fn render(&self, module: &str) -> String {
        self.render_with(module, &|i| format!("<type parameter {i}>"))
    }

    /// As [`Type::render`], with `param` naming the type parameters.
    pub(crate) fn render_with(&self, module: &str, param: &dyn Fn(u8) -> String) -> String {
        let mut out = String::new();
        self.write(module, param, &mut out);
        out
    }

    fn write(&self, module: &str, param: &dyn Fn(u8) -> String, out: &mut String) {
        let list = |ts: &[Type], out: &mut String| {
            for (i, t) in ts.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                t.write(module, param, out);
            }
        };
        match self {
            Type::Int => out.push_str("int"),
            Type::Float => out.push_str("float"),
            Type::Bool => out.push_str("bool"),
            Type::Str => out.push_str("str"),
            Type::Bytes => out.push_str("bytes"),
            Type::Duration => out.push_str("duration"),
            Type::Unit => out.push_str("()"),
            Type::Never => out.push_str("never"),
            Type::Range => out.push_str("Range"),
            Type::List(t) => {
                out.push('[');
                t.write(module, param, out);
                out.push(']');
            }
            Type::Set(t) => {
                out.push_str("Set<");
                t.write(module, param, out);
                out.push('>');
            }
            Type::Map(k, v) => {
                out.push('[');
                k.write(module, param, out);
                out.push_str(": ");
                v.write(module, param, out);
                out.push(']');
            }
            Type::Tuple(ts) => {
                out.push('(');
                list(ts, out);
                out.push(')');
            }
            Type::Fn(ps, r) => {
                out.push_str("fn(");
                list(ps, out);
                out.push_str(") -> ");
                r.write(module, param, out);
            }
            Type::Param(i) => out.push_str(&param(*i)),
            Type::Reflect(i) => out.push_str(&format!("<description of {}>", param(*i))),
            Type::Named(n) if n.module == "prelude" && n.name == "Option" && n.args.len() == 1 => {
                let fn_type = matches!(n.args[0], Type::Fn(..));
                out.push_str(if fn_type { "(" } else { "" });
                n.args[0].write(module, param, out);
                out.push_str(if fn_type { ")?" } else { "?" });
            }
            Type::Named(n) => {
                if n.module != module && n.module != "prelude" {
                    out.push_str(n.module);
                    out.push('.');
                }
                out.push_str(n.name);
                if !n.args.is_empty() {
                    out.push('<');
                    list(&n.args, out);
                    out.push('>');
                }
            }
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(""))
    }
}

/// A Rust type with a Sand equivalent.
pub trait SandType {
    /// The Sand type of `Self`.
    fn sand_type() -> Type;
}

macro_rules! sand_type {
    ($($t:ty => $v:expr),* $(,)?) => {$(
        impl SandType for $t {
            fn sand_type() -> Type {
                $v
            }
        }
    )*};
}

sand_type! {
    i64 => Type::Int,
    f64 => Type::Float,
    bool => Type::Bool,
    String => Type::Str,
    &str => Type::Str,
    Vec<u8> => Type::Bytes,
    &[u8] => Type::Bytes,
    () => Type::Unit,
    Never => Type::Never,
    jiff::SignedDuration => Type::Duration,
}

impl<T: SandType> SandType for Vec<T> {
    fn sand_type() -> Type {
        Type::List(Box::new(T::sand_type()))
    }
}

impl<T: SandType> SandType for Option<T> {
    fn sand_type() -> Type {
        Type::option(T::sand_type())
    }
}

/// `Result<T, Error>`: errors of any Rust type become the prelude's `Error`
/// (see [`IntoHostError`](crate::native::IntoHostError)).
impl<T: SandType, E: crate::native::IntoHostError> SandType for Result<T, E> {
    fn sand_type() -> Type {
        Type::result(T::sand_type(), Type::error())
    }
}

impl<T: SandType, E: crate::native::NativeError> SandType for Result<crate::native::Later<T, E>, E> {
    fn sand_type() -> Type {
        E::result_type(T::sand_type())
    }
}

/// `T`: a native that returns `Err(ctl)` fails the call with `ctl` (a
/// runtime error, a limit, or a suspension).
impl<T: SandType> SandType for Result<T, crate::vm::Ctl> {
    fn sand_type() -> Type {
        T::sand_type()
    }
}

impl<K: SandType, V: SandType> SandType for HashMap<K, V> {
    fn sand_type() -> Type {
        Type::Map(Box::new(K::sand_type()), Box::new(V::sand_type()))
    }
}

impl<K: SandType, V: SandType> SandType for BTreeMap<K, V> {
    fn sand_type() -> Type {
        Type::Map(Box::new(K::sand_type()), Box::new(V::sand_type()))
    }
}

impl<T: SandType> SandType for HashSet<T> {
    fn sand_type() -> Type {
        Type::Set(Box::new(T::sand_type()))
    }
}

impl<T: SandType> SandType for BTreeSet<T> {
    fn sand_type() -> Type {
        Type::Set(Box::new(T::sand_type()))
    }
}

macro_rules! tuple_type {
    ($($n:ident),+) => {
        impl<$($n: SandType),+> SandType for ($($n,)+) {
            fn sand_type() -> Type {
                Type::Tuple(vec![$($n::sand_type()),+])
            }
        }
    };
}
tuple_type!(A, B);
tuple_type!(A, B, C);
tuple_type!(A, B, C, D);
tuple_type!(A, B, C, D, E);
tuple_type!(A, B, C, D, E, F);
tuple_type!(A, B, C, D, E, F, G);
tuple_type!(A, B, C, D, E, F, G, H);

macro_rules! fn_type {
    ($($n:ident),*) => {
        /// A Sand function type, for [`Val`] handles to functions.
        impl<R: SandType, $($n: SandType),*> SandType for fn($($n),*) -> R {
            fn sand_type() -> Type {
                Type::Fn(vec![$($n::sand_type()),*], Box::new(R::sand_type()))
            }
        }
    };
}
fn_type!();
fn_type!(A);
fn_type!(A, B);
fn_type!(A, B, C);

impl<T: ValueType> SandType for T {
    fn sand_type() -> Type {
        Type::bound::<T>(T::MODULE, T::NAME)
    }
}

/// The type of a native function that does not return, such as one that
/// always fails. It has no values.
#[derive(Clone, Copy, Debug)]
pub enum Never {}

/// A value of the type parameter at index `N` of a generic native function
/// (see [`Type::Param`]).
///
/// For `native fn first<T>(xs: [T]) -> T?`, the Rust function takes a
/// `Val<Vec<Param<0>>>` and returns an `Option<Param<0>>`.
#[derive(Clone, Copy, Debug)]
pub struct Param<const N: u8>(Value);

impl<const N: u8> Param<N> {
    /// The value. The caller must keep the value's type unchanged.
    pub fn raw(self) -> Value {
        self.0
    }

    /// `v`, which must be a value of the type parameter.
    pub fn from_raw(v: Value) -> Self {
        Param(v)
    }
}

impl<const N: u8> SandType for Param<N> {
    fn sand_type() -> Type {
        Type::Param(N)
    }
}

/// The runtime description of the type parameter at index `N`, which the
/// declaration marks `reflect` (see [`Type::Reflect`]).
#[derive(Clone, Copy, Debug)]
pub struct Reflect<const N: u8>(Value);

impl<const N: u8> Reflect<N> {
    pub fn raw(self) -> Value {
        self.0
    }

    pub(crate) fn from_raw(v: Value) -> Self {
        Reflect(v)
    }
}

impl<const N: u8> SandType for Reflect<N> {
    fn sand_type() -> Type {
        Type::Reflect(N)
    }
}

/// A Sand value of the type that `T` describes, passed without conversion.
///
/// A native function that needs the VM takes its heap values as `Val`s:
/// `Val<String>` is a `str`, `Val<Vec<Param<0>>>` a list of the first type
/// parameter, `Val<fn() -> Option<String>>` a function.
pub struct Val<T: ?Sized>(Value, PhantomData<fn() -> T>);

impl<T: ?Sized> Val<T> {
    /// The value. The caller must keep the value's type unchanged.
    pub fn raw(self) -> Value {
        self.0
    }

    /// `v`, which must be a value of the type that `T` describes.
    pub fn from_raw(v: Value) -> Self {
        Val(v, PhantomData)
    }
}

impl<T: ?Sized> Clone for Val<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for Val<T> {}

impl<T: ?Sized> fmt::Debug for Val<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Val({:?})", self.0)
    }
}

impl<T: SandType + ?Sized> SandType for Val<T> {
    fn sand_type() -> Type {
        T::sand_type()
    }
}

/// The prelude's `Task<T>`, for [`Val`] handles.
pub struct Task<T>(PhantomData<T>);

impl<T: SandType> SandType for Task<T> {
    fn sand_type() -> Type {
        Type::named("prelude", "Task", vec![T::sand_type()])
    }
}

/// The prelude's `Channel<T>`, for [`Val`] handles.
pub struct Channel<T>(PhantomData<T>);

impl<T: SandType> SandType for Channel<T> {
    fn sand_type() -> Type {
        Type::named("prelude", "Channel", vec![T::sand_type()])
    }
}

/// The prelude's `Range`, for [`Val`] handles.
pub struct Range;

impl SandType for Range {
    fn sand_type() -> Type {
        Type::Range
    }
}
