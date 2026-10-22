//! Rust representations of Sand types.

use crate::vm::value::{TypeDesc, Value};
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
    /// Type parameter at index `0..`, ordered with any enclosing `extend` or
    /// `impl` type parameters before the function's own type parameters, and
    /// the bounds that the Rust function requires of it.
    Param(u8, Bounds),
    /// Runtime type description for the `reflect` type parameter at index
    /// `0..`, and the check of the types that the native accepts for it.
    Reflect(u8, fn(&TypeDesc) -> Result<(), String>),
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

    /// Constructs a reference to the Sand type `module.name` bound to the Rust
    /// type `T`.
    pub fn bound<T: 'static>(module: &'static str, name: &'static str) -> Type {
        Type::Named(Named { module, name, args: Vec::new(), rust: Some(TypeId::of::<T>()) })
    }

    pub fn option(t: Type) -> Type {
        Type::named("prelude", "Option", vec![t])
    }

    pub fn result(t: Type, e: Type) -> Type {
        Type::named("prelude", "Result", vec![t, e])
    }

    pub fn error() -> Type {
        Type::named("prelude", "Error", Vec::new())
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
            Type::Reflect(i, _) => out.push_str(&format!("<description of {}>", param(*i))),
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

/// The built-in bounds (`Eq`, `Ord`, `Hash`) that a native function requires
/// of a type parameter. `Ord` and `Hash` imply `Eq`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bounds {
    pub eq: bool,
    pub ord: bool,
    pub hash: bool,
}

impl Bounds {
    /// No bounds.
    pub const NONE: Bounds = Bounds { eq: false, ord: false, hash: false };

    /// The bounds in `self` or `other`.
    pub const fn union(self, other: Bounds) -> Bounds {
        Bounds { eq: self.eq || other.eq, ord: self.ord || other.ord, hash: self.hash || other.hash }
    }

    /// The names of the bounds, without `Eq` if another bound implies it.
    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.eq && !self.ord && !self.hash {
            out.push("Eq");
        }
        if self.ord {
            out.push("Ord");
        }
        if self.hash {
            out.push("Hash");
        }
        out
    }
}

/// Marker types for the bounds of [`Param`]: `Param<0, bound::Hash>`, or
/// `Param<0, (bound::Ord, bound::Hash)>` for several bounds.
pub mod bound {
    use super::Bounds;

    /// A set of built-in bounds: a marker type, or a tuple of marker types.
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
        /// Values can be compared with `==`.
        Eq: eq,
        /// Values can be ordered with `<`.
        Ord: ord,
        /// Values can be map keys and set elements.
        Hash: hash,
    }

    impl<A: ParamBounds, B: ParamBounds> ParamBounds for (A, B) {
        const BOUNDS: Bounds = A::BOUNDS.union(B::BOUNDS);
    }
}

/// Value of the `N`-th generic type parameter of a `native fn` (see
/// [`Type::Param`]), which the Rust function requires to satisfy the bounds
/// `B` (see [`bound`]).
///
/// For `native fn first<T>(xs: [T]) -> T?`, the Rust function accepts
/// `Val<Vec<Param<0>>>` and returns `Option<Param<0>>`. A Rust function that
/// hashes the values of `T` takes `Param<0, bound::Hash>` (in one place or
/// more), and the engine requires the `native fn` declaration to give `T` the
/// bound `Hash`.
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

/// Decides at compile time which types a native function accepts for a
/// `reflect` type parameter (see [`Reflect`]).
pub trait ReflectCheck {
    /// Returns the error message for a call site if the native cannot handle
    /// values of the type that `node` describes. The engine calls this function
    /// once for each distinct node of the description of the type argument.
    /// [`TypeDesc::name`] gives the Sand name of a type for the message.
    fn check(node: &TypeDesc) -> Result<(), String>;
}

/// Accepts every type.
impl ReflectCheck for () {
    fn check(_: &TypeDesc) -> Result<(), String> {
        Ok(())
    }
}

/// Runtime type description passed for the `N`-th `reflect` type parameter of
/// a `native fn` (see [`Type::Reflect`]). At each call site, the engine checks
/// the description with `C` and rejects the program if `C` rejects it.
pub struct Reflect<const N: u8, C: ReflectCheck = ()>(Value, PhantomData<fn() -> C>);

impl<const N: u8, C: ReflectCheck> Reflect<N, C> {
    pub fn raw(self) -> Value {
        self.0
    }

    pub(crate) fn from_raw(v: Value) -> Self {
        Reflect(v, PhantomData)
    }
}

impl<const N: u8, C: ReflectCheck> Clone for Reflect<N, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<const N: u8, C: ReflectCheck> Copy for Reflect<N, C> {}

impl<const N: u8, C: ReflectCheck> fmt::Debug for Reflect<N, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Reflect({:?})", self.0)
    }
}

impl<const N: u8, C: ReflectCheck> SandType for Reflect<N, C> {
    fn sand_type() -> Type {
        Type::Reflect(N, C::check)
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
