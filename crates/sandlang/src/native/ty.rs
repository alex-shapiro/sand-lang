//! Rust representations of Sand types.

use crate::types::{Builtin, Builtins, traits};
use crate::value::Value;
use crate::vm::value::VmValue;
use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

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
    /// type parameters before the function's own type parameters) and the
    /// built-in traits that the Rust function requires of it.
    Param(u8, Builtins),
    /// Dictionary of a built-in trait for the type parameter at index `0..`
    /// (see [`TraitDict`]).
    Dict(u8, Builtin),
    /// An argument of the type parameter at index `0..`, which must implement
    /// `Encode`, as a host [`Value`](crate::value::Value) (see [`Encoded`]).
    Encoded(u8),
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

    /// Constructs a reference to the Sand type bound to `T` with type arguments `args`.
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
        <crate::stdlib::core::Error as SandType>::sand_type()
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
            Type::Dict(i, b) => out.push_str(&format!("<{} dictionary of {}>", b.name(), param(*i))),
            Type::Encoded(i) => out.push_str(&param(*i)),
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

impl<T: SandType, E: crate::native::convert::IntoHostError> SandType for Result<T, E> {
    fn sand_type() -> Type {
        Type::result(T::sand_type(), Type::error())
    }
}

impl<T: SandType, E: crate::native::func::NativeError> SandType for Result<crate::native::func::Later<T, E>, E>
where
    Result<T, E>: SandType,
{
    fn sand_type() -> Type {
        <Result<T, E>>::sand_type()
    }
}

impl<T: SandType> SandType for Result<T, crate::vm::Ctl> {
    fn sand_type() -> Type {
        T::sand_type()
    }
}

impl<T: SandType> SandType for Result<T, crate::vm::Fail> {
    fn sand_type() -> Type {
        T::sand_type()
    }
}

impl<T: SandType, C> SandType for Result<T, crate::native::convert::HostError<C>> {
    fn sand_type() -> Type {
        Type::result(T::sand_type(), Type::error())
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

/// Makes a VM handle `!Send` and `!Sync`. A handle is valid only during the
/// native call that received or allocated it.
struct UnrootedVmHandle(#[allow(dead_code)] *const ());

/// Uninhabited return type for native functions that never return (Sand `never`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Never {}

/// Value of the `N`-th generic type parameter of a `native fn`.
///
/// `B` lists the built-in [`traits`] that the Rust function requires of the
/// type parameter. See [`Type::Param`].
///
/// For `native fn first<T>(xs: [T]) -> T?`, the Rust function takes
/// `VmVal<Vec<Param<0>>>` and returns `Option<Param<0>>`. If the Rust function
/// uses `Param<0, traits::Hash>`, the `native fn` declaration must declare
/// `T: Hash`.
pub struct Param<const N: u8, B: traits::Set = ()>(VmValue, PhantomData<fn() -> B>, PhantomData<UnrootedVmHandle>);

impl<const N: u8, B: traits::Set> Param<N, B> {
    pub fn raw(self) -> VmValue {
        self.0
    }

    /// Wraps `v`, which must have the type of the `N`-th type parameter.
    pub fn from_raw(v: VmValue) -> Self {
        Param(v, PhantomData, PhantomData)
    }
}

impl<const N: u8, B: traits::Set> Clone for Param<N, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<const N: u8, B: traits::Set> Copy for Param<N, B> {}

impl<const N: u8, B: traits::Set> fmt::Debug for Param<N, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Param({:?})", self.0)
    }
}

impl<const N: u8, B: traits::Set> SandType for Param<N, B> {
    fn sand_type() -> Type {
        Type::Param(N, B::TRAITS)
    }
}

/// Runtime dictionary of the built-in trait `B` for the `N`-th generic type
/// parameter of a `native fn`.
///
/// A Rust function can accept `TraitDict<N, B>` if the `native fn`
/// declaration requires `B` of the type parameter, directly or through a
/// supertrait, and `B` declares methods. Place `TraitDict` parameters in
/// ascending order of `N`, before regular value parameters.
pub struct TraitDict<const N: u8, B: traits::One>(VmValue, PhantomData<fn() -> B>, PhantomData<UnrootedVmHandle>);

/// Runtime dictionary of `Num` for the `N`-th generic type parameter (`int`
/// or `float`).
pub type Num<const N: u8> = TraitDict<N, traits::Num>;

impl<const N: u8, B: traits::One> TraitDict<N, B> {
    pub fn raw(self) -> VmValue {
        self.0
    }

    pub(crate) fn from_raw(v: VmValue) -> Self {
        TraitDict(v, PhantomData, PhantomData)
    }
}

impl<const N: u8, B: traits::One> Clone for TraitDict<N, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<const N: u8, B: traits::One> Copy for TraitDict<N, B> {}

impl<const N: u8, B: traits::One> fmt::Debug for TraitDict<N, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraitDict<{N}, {}>({:?})", B::BUILTIN.name(), self.0)
    }
}

impl<const N: u8, B: traits::One> SandType for TraitDict<N, B> {
    fn sand_type() -> Type {
        Type::Dict(N, B::BUILTIN)
    }
}

/// An argument of the `N`-th generic type parameter of a `native fn`,
/// encoded as a host [`Value`](crate::value::Value) before the call.
///
/// The `native fn` declaration must require `T: Encode`. The engine encodes
/// the argument with the `Encode` impls of Sand code, so a synchronous
/// Rust function can read the encoded value:
///
/// ```
/// # use sandlang::native::NativeModule;
/// # use sandlang::native::ty::Encoded;
/// # use sandlang::value::Value;
/// let mut module = NativeModule::new("audit");
/// module.source("pub native fn record<T: Encode>(event: T) -> int");
/// module.function("record", |event: Encoded<0>| match event.value() {
///     Value::Struct(event) => event.fields.len() as i64,
///     _ => 0,
/// });
/// ```
pub struct Encoded<const N: u8>(Arc<Value>);

impl<const N: u8> Encoded<N> {
    /// Returns the encoded argument.
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// Returns the encoded argument, which the engine copies if it retries
    /// the call.
    pub fn into_value(self) -> Value {
        Arc::unwrap_or_clone(self.0)
    }
}

impl<const N: u8> fmt::Debug for Encoded<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Encoded({:?})", self.0)
    }
}

impl<const N: u8> SandType for Encoded<N> {
    fn sand_type() -> Type {
        Type::Encoded(N)
    }
}

/// The encoded argument of an [`Encoded`] parameter, which the engine passes
/// to the Rust function.
#[derive(crate::SandObject)]
#[sand(module = "prelude", private)]
pub(crate) struct EncodedArg {
    pub(crate) value: Arc<Value>,
    /// The memory of `value`, which the memory limit counts.
    pub(crate) _reservation: crate::vm::iomem::Reservation,
}

impl super::NativeObject for EncodedArg {}

impl<const N: u8> crate::native::convert::FromVmValue for Encoded<N> {
    type Item<'a> = Self;
    fn from_vm_value(vm: &crate::vm::Vm, v: VmValue, _: &mut crate::native::convert::Conversion) -> Result<Self, crate::vm::Fail> {
        Ok(Encoded(VmVal::<EncodedArg>::from_raw(v).get(&vm.heap)?.value.clone()))
    }
}

/// Uncopied [`VmValue`] with the Sand type of `T`.
///
/// A [`NativeFunction`](super::func::NativeFunction) that takes `&mut Vm` can
/// accept and return `VmVal<T>` to pass heap handles directly.
///
/// A `VmVal` is valid only until the call returns or suspends. Garbage
/// collection during a wait can free an unrooted heap object. `VmVal`,
/// [`Param`] and [`TraitDict`] are therefore `!Send`. The compiler
/// rejects these VM handles in three places:
///
/// - a [`NativeObject`](super::NativeObject)
/// - the future of an async native function
/// - the future of [`Later::new`](super::func::Later::new)
///
/// Root a value with [`NativeContext::root`](super::context::NativeContext::root)
/// to keep it across a wait.
///
/// ```compile_fail,E0277
/// # use sandlang::native::ty::VmVal;
/// fn assert_send<T: Send>() {}
/// assert_send::<VmVal<String>>();
/// ```
///
/// [`VmVal::raw`] returns the underlying `Send` [`VmValue`]. Keep only
/// [`RootedValue`](super::context::RootedValue) handles across waits.
pub struct VmVal<T: ?Sized>(VmValue, PhantomData<fn() -> T>, PhantomData<UnrootedVmHandle>);

impl<T: ?Sized> VmVal<T> {
    pub fn raw(self) -> VmValue {
        self.0
    }

    /// Wraps `v`, which must have the Sand type of `T`.
    pub fn from_raw(v: VmValue) -> Self {
        VmVal(v, PhantomData, PhantomData)
    }
}

impl<T: ?Sized> Clone for VmVal<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for VmVal<T> {}

impl<T: ?Sized> fmt::Debug for VmVal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VmVal({:?})", self.0)
    }
}

impl<T: SandType + ?Sized> SandType for VmVal<T> {
    fn sand_type() -> Type {
        T::sand_type()
    }
}

/// Marker type representing the prelude's `Task<T>` in [`VmVal<Task<T>>`](VmVal).
pub struct Task<T>(PhantomData<T>);

impl<T: SandType> SandType for Task<T> {
    fn sand_type() -> Type {
        Type::named("prelude", "Task", vec![T::sand_type()])
    }
}

/// Marker type representing the prelude's `Channel<T>` in [`VmVal<Channel<T>>`](VmVal).
pub struct Channel<T>(PhantomData<T>);

impl<T: SandType> SandType for Channel<T> {
    fn sand_type() -> Type {
        Type::named("prelude", "Channel", vec![T::sand_type()])
    }
}

/// Marker type representing the prelude's `Range` in [`VmVal<Range>`](VmVal).
pub struct Range;

impl SandType for Range {
    fn sand_type() -> Type {
        Type::Range
    }
}
