//! Rust representations of Sand types.

use crate::vm::value::Value;
use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;

/// A Sand type derived from a Rust type through [`SandType`].
#[derive(Clone, Debug)]
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
    /// Type parameter at index `0..` (ordered with enclosing `extend` or `impl`
    /// type parameters before the function's own type parameters) and its
    /// required [`Bounds`].
    Param(u8, Bounds),
    /// Runtime type description for the `reflect` type parameter at index `0..`.
    Reflect(u8),
    /// Evidence that the type parameter at index `0..` is `Num` (see [`Num`]).
    Num(u8),
    /// Nominal type declared in a module.
    Named(Named),
}

/// A nominal Sand type and its type arguments.
#[derive(Clone, Debug)]
pub struct Named {
    pub module: &'static str,
    pub name: &'static str,
    pub args: Vec<Type>,
    pub rust: Option<TypeId>,
}

impl Type {
    /// Constructs a reference to the Sand type `module.name<args>`.
    pub fn named(module: &'static str, name: &'static str, args: Vec<Type>) -> Type {
        Type::Named(Named { module, name, args, rust: None })
    }

    /// Constructs a reference to the Sand type bound to the Rust type `T`,
    /// with the type arguments `args`.
    pub fn bound<T: super::NamedType>(args: Vec<Type>) -> Type {
        Type::Named(Named { module: T::MODULE, name: T::NAME, args, rust: Some(T::rust_type()) })
    }

    pub fn option(t: Type) -> Type {
        Type::named("prelude", "Option", vec![t])
    }

    pub fn result(t: Type, e: Type) -> Type {
        Type::named("prelude", "Result", vec![t, e])
    }

    pub fn error() -> Type {
        <crate::stdlib::Error as SandType>::sand_type()
    }

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

    pub(crate) fn render(&self, module: &str) -> String {
        self.render_with(module, &|i| format!("<type parameter {i}>"))
    }

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
            Type::Param(i, _) => out.push_str(&param(*i)),
            Type::Reflect(i) => out.push_str(&format!("<description of {}>", param(*i))),
            Type::Num(i) => out.push_str(&format!("<Num evidence of {}>", param(*i))),
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

/// Maps a Rust type to its corresponding Sand [`Type`].
pub trait SandType {
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

impl<T: SandType> SandType for Box<T> {
    fn sand_type() -> Type {
        T::sand_type()
    }
}

impl<T: SandType> SandType for Option<T> {
    fn sand_type() -> Type {
        Type::option(T::sand_type())
    }
}

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

impl<K: SandType, V: SandType> SandType for indexmap::IndexMap<K, V> {
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

macro_rules! fn_type {
    ($($n:ident),*) => {
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

/// Uninhabited return type for native functions that never return (Sand `never`).
#[derive(Clone, Copy, Debug)]
pub enum Never {}

/// Built-in trait bounds (`Eq`, `Ord`, `Hash`, `Num`, `Encode`) required on a
/// generic type parameter of a native function. `Ord`, `Hash`, and `Num` imply
/// `Eq`, and `Num` implies `Ord` and `Encode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bounds {
    pub eq: bool,
    pub ord: bool,
    pub hash: bool,
    pub num: bool,
    pub encode: bool,
}

impl Bounds {
    pub const NONE: Bounds = Bounds { eq: false, ord: false, hash: false, num: false, encode: false };

    #[must_use]
    pub const fn union(self, other: Bounds) -> Bounds {
        Bounds {
            eq: self.eq || other.eq,
            ord: self.ord || other.ord,
            hash: self.hash || other.hash,
            num: self.num || other.num,
            encode: self.encode || other.encode,
        }
    }

    /// Returns the bound names required by `self`, omitting the bounds that
    /// the others imply.
    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.eq && !self.ord && !self.hash && !self.num {
            out.push("Eq");
        }
        if self.ord && !self.num {
            out.push("Ord");
        }
        if self.hash {
            out.push("Hash");
        }
        if self.num {
            out.push("Num");
        }
        if self.encode && !self.num {
            out.push("Encode");
        }
        out
    }
}

/// Marker types that specify required type parameter bounds on [`Param`] (for
/// example, `Param<0, bound::Hash>` or `Param<0, (bound::Ord, bound::Hash)>`).
pub mod bound {
    use super::Bounds;

    /// Built-in trait bounds required on a [`Param`](super::Param).
    pub trait ParamBounds {
        const BOUNDS: Bounds;
    }

    impl ParamBounds for () {
        const BOUNDS: Bounds = Bounds::NONE;
    }

    macro_rules! markers {
        ($($(#[$doc:meta])* $name:ident: $field:ident),* $(,)?) => {$(
            $(#[$doc])*
            pub struct $name;
            impl ParamBounds for $name {
                const BOUNDS: Bounds = Bounds { $field: true, ..Bounds::NONE };
            }
        )*};
    }

    markers! {
        /// Requires `Eq` (`==` and `!=`).
        Eq: eq,
        /// Requires `Ord` (`<`, `<=`, `>`, `>=`).
        Ord: ord,
        /// Requires `Hash` (map keys and set elements).
        Hash: hash,
        /// Requires `Num` (`int` or `float`). A native function that needs the
        /// `Num` evidence takes it as [`Num`](super::super::Num).
        Num: num,
        /// Requires `Encode` (no functions, tasks, channels, or native objects).
        Encode: encode,
    }

    impl<A: ParamBounds, B: ParamBounds> ParamBounds for (A, B) {
        const BOUNDS: Bounds = A::BOUNDS.union(B::BOUNDS);
    }
}

/// Value of the `N`-th generic type parameter of a `native fn` (see
/// [`Type::Param`]), with required trait bounds `B` (see [`bound`]).
///
/// For `native fn first<T>(xs: [T]) -> T?`, the Rust function takes
/// `Val<Vec<Param<0>>>` and returns `Option<Param<0>>`. If the Rust function
/// requires a trait bound on `T` (such as `Param<0, bound::Hash>`), the
/// `native fn` declaration must declare that bound on `T`.
pub struct Param<const N: u8, B: bound::ParamBounds = ()>(Value, PhantomData<fn() -> B>);

impl<const N: u8, B: bound::ParamBounds> Param<N, B> {
    pub fn raw(self) -> Value {
        self.0
    }

    /// Wraps `v`, which must have the type of the `N`-th type parameter.
    pub fn from_raw(v: Value) -> Self {
        Param(v, PhantomData)
    }
}

impl<const N: u8, B: bound::ParamBounds> Clone for Param<N, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<const N: u8, B: bound::ParamBounds> Copy for Param<N, B> {}

impl<const N: u8, B: bound::ParamBounds> fmt::Debug for Param<N, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Param({:?})", self.0)
    }
}

impl<const N: u8, B: bound::ParamBounds> SandType for Param<N, B> {
    fn sand_type() -> Type {
        Type::Param(N, B::BOUNDS)
    }
}

/// Evidence that the `N`-th generic type parameter of a `native fn` is `Num`
/// (`int` or `float`), declared as `T: Num`.
///
/// A Rust function takes one `Num<N>` for each type parameter that its
/// `native fn` declaration bounds by `Num`, after its [`Reflect`] parameters
/// and before the other parameters.
pub struct Num<const N: u8>(Value);

impl<const N: u8> Num<N> {
    pub fn raw(self) -> Value {
        self.0
    }

    pub(crate) fn from_raw(v: Value) -> Self {
        Num(v)
    }
}

impl<const N: u8> Clone for Num<N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<const N: u8> Copy for Num<N> {}

impl<const N: u8> fmt::Debug for Num<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Num({:?})", self.0)
    }
}

impl<const N: u8> SandType for Num<N> {
    fn sand_type() -> Type {
        Type::Num(N)
    }
}

/// Runtime type description for the `N`-th `reflect` type parameter of a
/// `native fn` (see [`Type::Reflect`]).
#[derive(Clone, Copy)]
pub struct Reflect<const N: u8>(Value);

impl<const N: u8> Reflect<N> {
    pub fn raw(self) -> Value {
        self.0
    }

    pub(crate) fn from_raw(v: Value) -> Self {
        Reflect(v)
    }
}

impl<const N: u8> fmt::Debug for Reflect<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Reflect({:?})", self.0)
    }
}

impl<const N: u8> SandType for Reflect<N> {
    fn sand_type() -> Type {
        Type::Reflect(N)
    }
}

/// Uncopied handle to a Sand [`Value`] with the Sand type of `T`.
///
/// Functions registered with [`NativeModule::vm_function`](super::NativeModule::vm_function)
/// can accept and return `Val<T>` to avoid copying heap values (for example,
/// `Val<String>` for `str`, `Val<Vec<Param<0>>>` for `[T]`, or
/// `Val<fn() -> Option<String>>` for `fn() -> str?`).
///
/// Because the garbage collector does not trace `Val` handles stored in Rust
/// structs, a `Val` is valid only for the duration of the synchronous native
/// call that received or allocated it. Never store a `Val` inside a
/// [`NativeObject`](super::NativeObject) or across an async suspension point.
pub struct Val<T: ?Sized>(Value, PhantomData<fn() -> T>);

impl<T: ?Sized> Val<T> {
    pub fn raw(self) -> Value {
        self.0
    }

    /// Wraps `v`, which must have the Sand type of `T`.
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

/// Marker type representing the prelude's `Task<T>` in [`Val<Task<T>>`](Val).
pub struct Task<T>(PhantomData<T>);

impl<T: SandType> SandType for Task<T> {
    fn sand_type() -> Type {
        Type::named("prelude", "Task", vec![T::sand_type()])
    }
}

/// Marker type representing the prelude's `Channel<T>` in [`Val<Channel<T>>`](Val).
pub struct Channel<T>(PhantomData<T>);

impl<T: SandType> SandType for Channel<T> {
    fn sand_type() -> Type {
        Type::named("prelude", "Channel", vec![T::sand_type()])
    }
}

/// Marker type representing the prelude's `Range` in [`Val<Range>`](Val).
pub struct Range;

impl SandType for Range {
    fn sand_type() -> Type {
        Type::Range
    }
}
