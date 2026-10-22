//! Native module registration and Rust-to-Sand type bindings.

mod convert;
mod func;
mod ty;

pub use convert::{Conversion, FromValue, HostError, IntoHostError, IntoValue};
pub use func::{AsyncHostFunction, HostFunction, Later, NativeError, Signature, VmFunction};
pub use ty::{Channel, Named, Never, Param, Range, Reflect, SandType, Task, Type, Val};

pub(crate) use convert::mismatch;

use crate::check::ModKind;
use crate::engine::Registry;
use crate::syntax::lexer::{is_ident, keyword};
use crate::types::ValueTypeInfo;
use crate::vm::heap::Heap;
use crate::vm::value::{ChannelObj, MapObj, Obj, SetObj, TaskObj, TypeDesc, Value, ValueType};
use crate::vm::{Ctl, HostFn, NativeFn, Vm};
use std::any::{Any, TypeId};
use std::sync::Arc;

/// A unit-only Rust enum that maps to a Sand `enum` with matching variants.
///
/// Derive this trait with [`#[derive(SandEnum)]`](crate::SandEnum) and register
/// the type with [`NativeModule::enum_type`]:
///
/// ```
/// /// A two-dimensional shape.
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// pub enum Shape {
///     Circle,
///     Square,
/// }
/// ```
pub trait EnumType: SandType + Copy + 'static {
    /// Name of the Sand module that owns the enum (`#[sand(module = "...")]`).
    const MODULE: &'static str;
    /// Sand enum name (defaults to the Rust identifier, or `#[sand(name = "...")]`).
    const NAME: &'static str;
    /// Doc comment attached to the enum declaration.
    const DOC: &'static str;
    /// Variants in declaration order.
    const VARIANTS: &'static [EnumVariant<Self>];
    /// Returns the zero-based index of `self` in [`Self::VARIANTS`].
    fn index(self) -> usize;
}

/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape { Circle(u32) }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape<T> { Circle, Other(T) }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape {}
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// enum Shape { Circle }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo")]
/// enum Shape { #[sand(name = "Round")] Circle }
/// ```
///
/// ```compile_fail
/// #[derive(Clone, Copy, sandlang::SandEnum)]
/// #[sand(module = "geo", name = "shape")]
/// enum Shape { Circle }
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "db")]
/// struct Conn;
/// // Missing: `impl sandlang::native::NativeObject for Conn {}`.
/// ```
///
/// ```compile_fail
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "db")]
/// struct Conn<T>(T);
/// impl<T: Send + 'static> sandlang::native::NativeObject for Conn<T> {}
/// ```
#[cfg(doctest)]
struct DeriveErrors;

/// Metadata for a single variant of an [`EnumType`].
pub struct EnumVariant<T> {
    pub name: &'static str,
    pub doc: &'static str,
    pub value: T,
}

/// Memory and resource accounting for a heap-allocated [`ObjectType`].
pub trait NativeObject: Any + Send {
    /// Approximate heap memory used by `self`, in bytes.
    fn size(&self) -> usize {
        64
    }

    /// Returns `true` if `self` holds an open host resource (such as a file or
    /// socket with an `iomem::ResourceSlot` counted against
    /// `max_open_resources`). Allocating resource objects paces garbage
    /// collection so unreachable handles are closed promptly.
    fn is_resource(&self) -> bool {
        false
    }
}

/// A Rust type exposed to scripts as an opaque reference type.
///
/// Derive this trait with [`#[derive(SandObject)]`](crate::SandObject),
/// implement [`NativeObject`], and register the type with
/// [`NativeModule::object_type`]:
///
/// ```
/// /// A shared atomic counter.
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "stats")]
/// pub struct Counter(std::sync::atomic::AtomicI64);
///
/// impl sandlang::native::NativeObject for Counter {}
/// ```
///
/// Functions registered with [`NativeModule::function`] borrow an object as
/// `&T` and return a new object by value as `T`. Functions registered with
/// [`NativeModule::vm_function`] accept a [`Val<T>`] handle.
pub trait ObjectType: NativeObject + SandType + Sized {
    /// Name of the Sand module that owns the type (`#[sand(module = "...")]`).
    const MODULE: &'static str;
    /// Sand type name (defaults to the Rust identifier, or `#[sand(name = "...")]`).
    const NAME: &'static str;
    /// Doc comment attached to the type declaration.
    const DOC: &'static str;
    /// `true` if the type is exported (`pub`), or `false` if marked `#[sand(private)]`.
    const PUBLIC: bool;
}

/// Type-erased heap representation of an [`ObjectType`].
pub trait DynObject: Any + Send + sealed::Sealed {
    fn type_name(&self) -> &'static str;
    fn size(&self) -> usize;
    fn is_resource(&self) -> bool;
}

mod sealed {
    pub trait Sealed {}
    impl<T: super::ObjectType> Sealed for T {}
}

impl<T: ObjectType> DynObject for T {
    fn type_name(&self) -> &'static str {
        T::NAME
    }
    fn size(&self) -> usize {
        NativeObject::size(self)
    }
    fn is_resource(&self) -> bool {
        NativeObject::is_resource(self)
    }
}

impl<T: ObjectType> Val<T> {
    pub fn get(self, heap: &Heap) -> Result<&T, Ctl> {
        heap.native::<T>(self.raw()).ok_or_else(mismatch::<T>)
    }

    pub fn get_mut(self, heap: &mut Heap) -> Result<&mut T, Ctl> {
        heap.native_mut::<T>(self.raw()).ok_or_else(mismatch::<T>)
    }
}

impl<const N: u8> Reflect<N> {
    pub fn desc(self, heap: &Heap) -> Result<&Arc<TypeDesc>, Ctl> {
        match heap.get(self.raw()) {
            Obj::TypeDesc(d) => Ok(d),
            _ => Err(Ctl::internal("a native function expected a type description")),
        }
    }
}

impl Val<String> {
    pub fn str(self, heap: &Heap) -> &str {
        heap.str(self.raw())
    }
}

impl Val<Vec<u8>> {
    pub fn bytes(self, heap: &Heap) -> &[u8] {
        heap.bytes_of(self.raw())
    }
}

impl<T: SandType> Val<Vec<T>> {
    pub fn items(self, heap: &Heap) -> &[Value] {
        heap.list(self.raw())
    }
}

impl<K: SandType, V: SandType> Val<std::collections::HashMap<K, V>> {
    pub(crate) fn map(self, heap: &Heap) -> Result<&MapObj, Ctl> {
        match heap.get(self.raw()) {
            Obj::Map(m) => Ok(m),
            _ => Err(mismatch::<std::collections::HashMap<K, V>>()),
        }
    }
}

impl<T: SandType> Val<std::collections::HashSet<T>> {
    pub(crate) fn set(self, heap: &Heap) -> Result<&SetObj, Ctl> {
        match heap.get(self.raw()) {
            Obj::Set(s) => Ok(s),
            _ => Err(mismatch::<std::collections::HashSet<T>>()),
        }
    }
}

impl Val<Range> {
    /// Returns the `(start, end, inclusive)` bounds of the range.
    pub(crate) fn bounds(self, heap: &Heap) -> Result<(i64, i64, bool), Ctl> {
        match *heap.get(self.raw()) {
            Obj::Range(a, b, inclusive) => Ok((a, b, inclusive)),
            _ => Err(mismatch::<Range>()),
        }
    }
}

impl<T: SandType> Val<Channel<T>> {
    pub(crate) fn channel(self, heap: &Heap) -> Result<&ChannelObj, Ctl> {
        match heap.get(self.raw()) {
            Obj::Channel(c) => Ok(c),
            _ => Err(mismatch::<Channel<T>>()),
        }
    }

    pub(crate) fn channel_mut(self, heap: &mut Heap) -> Result<&mut ChannelObj, Ctl> {
        heap.channel_mut(self.raw(), None).ok_or_else(mismatch::<Channel<T>>)
    }
}

impl<T: SandType> Val<Task<T>> {
    pub(crate) fn task(self, heap: &Heap) -> Result<&TaskObj, Ctl> {
        match heap.get(self.raw()) {
            Obj::Task(t) => Ok(t),
            _ => Err(mismatch::<Task<T>>()),
        }
    }
}

impl Vm {
    /// Converts a Sand enum value `v` into `T`.
    pub fn enum_value<T: EnumType>(&self, v: Value) -> Result<T, Ctl> {
        let index = match v {
            Value::Tag(id) => self.prog.enum_types.get(&TypeId::of::<T>()).and_then(|&first| id.checked_sub(first)),
            _ => None,
        };
        match index.and_then(|i| T::VARIANTS.get(i as usize)) {
            Some(v) => Ok(v.value),
            None => Err(mismatch::<T>()),
        }
    }

    /// Converts `x` into a Sand enum `Value`. Returns an internal error if the
    /// module that owns `T` is not loaded in the current program.
    pub fn new_enum<T: EnumType>(&self, x: T) -> Result<Value, Ctl> {
        match self.prog.enum_types.get(&TypeId::of::<T>()) {
            Some(&first) => Ok(Value::Tag(first + x.index() as u32)),
            None => Err(Ctl::internal(format!("enum {}.{} is not loaded", T::MODULE, T::NAME))),
        }
    }

    /// Allocates `x` on the heap after reserving `x.size()` bytes against the
    /// memory limit.
    pub fn new_object<T: ObjectType>(&mut self, x: T) -> Result<Val<T>, Ctl> {
        self.reserve(NativeObject::size(&x))?;
        Ok(Val::from_raw(self.heap.alloc(Obj::Native(Box::new(x)))))
    }
}

/// A Rust-backed type registered on a [`NativeModule`].
#[derive(Clone, Debug)]
pub(crate) struct BoundType {
    pub module: &'static str,
    pub name: &'static str,
    pub doc: &'static str,
    pub public: bool,
    pub kind: BoundKind,
}

#[derive(Clone, Debug)]
pub(crate) enum BoundKind {
    Value(ValueTypeInfo),
    /// An [`EnumType`] with the `(name, doc)` pair of each variant in declaration order.
    Enum(TypeId, Arc<[(&'static str, &'static str)]>),
    Object(TypeId),
    /// A VM-implemented generic type (`Task<T>` or `Channel<T>`), with its type parameter names.
    Builtin(&'static [&'static str]),
}

impl BoundKind {
    pub(crate) fn rust_type(&self) -> Option<TypeId> {
        match self {
            BoundKind::Value(v) => Some(v.type_id),
            BoundKind::Enum(t, _) => Some(*t),
            BoundKind::Object(t) => Some(*t),
            BoundKind::Builtin(_) => None,
        }
    }
}

/// Checker metadata for a registered native function.
#[derive(Clone, Debug)]
pub(crate) struct NativeDecl {
    pub id: u32,
    pub sig: Arc<Signature>,
    pub names: Option<Arc<[String]>>,
    pub doc: Option<Arc<str>>,
}

#[derive(Clone)]
struct FnDef {
    name: String,
    sig: Signature,
    f: HostFn,
    names: Option<Arc<[String]>>,
    doc: Option<Arc<str>>,
}

/// A module of Rust functions and types that scripts can import with
/// `import <name>`.
///
/// The engine infers the Sand signature of each registered function from its
/// Rust parameter and return types. Functions that are not declared in
/// [`NativeModule::source`] are exported as `pub native fn` with the parameter
/// names configured by [`NativeModule::params`] (or `arg1`, `arg2`, ... by
/// default). Registering a function as `"Type.name"` defines a method on
/// `Type`; the method takes `self` if its first parameter has type `Type`. Use
/// `native fn` declarations in [`NativeModule::source`] to specify default
/// parameter values or generic type parameters. [`Engine::new`](crate::Engine::new)
/// verifies all declarations against the registered Rust signatures.
#[derive(Clone)]
pub struct NativeModule {
    name: String,
    kind: ModKind,
    source: String,
    functions: Vec<FnDef>,
    types: Vec<BoundType>,
}

impl NativeModule {
    /// Creates a host native module imported as `name`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a valid identifier or conflicts with the prelude
    /// or a standard library module.
    pub fn new(name: &str) -> NativeModule {
        assert!(is_ident(name) && keyword(name).is_none(), "a host module cannot be named `{name}`: it is not an identifier");
        assert!(!crate::stdlib::STD_MODULES.contains(&name), "a host module cannot be named `{name}`: it is the name of a standard module");
        NativeModule::of_kind(name, ModKind::Host)
    }

    pub(crate) fn std(name: &str, kind: ModKind, source: &str) -> NativeModule {
        let mut m = NativeModule::of_kind(name, kind);
        m.source(source);
        m
    }

    fn of_kind(name: &str, kind: ModKind) -> NativeModule {
        NativeModule { name: name.to_string(), kind, source: String::new(), functions: Vec::new(), types: Vec::new() }
    }

    /// Returns the module name used in `import` declarations.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn add(&mut self, name: &str, sig: Signature, f: Box<NativeFn>) -> &mut Self {
        let parts: Vec<&str> = name.split('.').collect();
        let valid = parts.len() <= 2 && parts.iter().all(|p| is_ident(p)) && parts.last() != Some(&"self");
        assert!(valid, "function name `{name}` is not `name` or `Type.name`");
        assert!(!self.functions.iter().any(|d| d.name == name), "function `{name}` is registered twice in module `{}`", self.name);
        let f = if self.kind == ModKind::Host { HostFn::Shared(f.into()) } else { HostFn::Static(Box::leak(f)) };
        self.functions.push(FnDef { name: name.to_string(), sig, f, names: None, doc: None });
        self
    }

    /// Registers a synchronous function `f` under `name` (or `"Type.method"`
    /// for a method on a type in this module). Parameters of type `&str`,
    /// `&[u8]`, and `&T` (for an [`ObjectType`] `T`) borrow directly from the
    /// VM heap without copying.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not of the form `name` or `Type.name`, or if `name`
    /// is already registered in this module.
    pub fn function<M, F: HostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host())
    }

    /// Registers a synchronous function `f` that receives `&mut Vm` as its
    /// first parameter. Heap values that the function does not copy must be
    /// taken as [`Val`] handles.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`NativeModule::function`].
    pub fn vm_function<M, F: VmFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host())
    }

    /// Registers an asynchronous function `f` that suspends the calling Sand
    /// task until the returned future completes.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`NativeModule::function`].
    pub fn async_function<M, F: AsyncHostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host())
    }

    /// Sets the parameter names (excluding `self`) for the most recently
    /// registered function if it is not declared in [`NativeModule::source`].
    ///
    /// # Panics
    ///
    /// Panics if this module has not registered any function yet, if
    /// `names.len()` does not match the function's parameter count, or if any
    /// name is not a valid unique identifier.
    pub fn params(&mut self, names: &[&str]) -> &mut Self {
        let module = self.name.clone();
        let f = self.last("params");
        let n = f.sig.params.len() - usize::from(f.sig.takes_self(&module, &f.name));
        assert_eq!(names.len(), n, "function `{}` has {n} parameters", f.name);
        for (i, n) in names.iter().enumerate() {
            assert!(is_ident(n) && keyword(n).is_none(), "parameter name `{n}` of function `{}` is not an identifier", f.name);
            assert!(!names[..i].contains(n), "parameter name `{n}` of function `{}` is given twice", f.name);
        }
        f.names = Some(names.iter().map(|n| n.to_string()).collect());
        self
    }

    /// Sets the documentation comment for the most recently registered
    /// function if it is not declared in [`NativeModule::source`].
    ///
    /// # Panics
    ///
    /// Panics if this module has not registered any function yet.
    pub fn doc(&mut self, text: &str) -> &mut Self {
        self.last("doc").doc = Some(text.into());
        self
    }

    fn last(&mut self, method: &str) -> &mut FnDef {
        match self.functions.last_mut() {
            Some(f) => f,
            None => panic!("`{method}` applies to the function registered last, and no function is registered"),
        }
    }

    /// Registers `T` as a public opaque value type of this module (see
    /// [`ValueType`]).
    ///
    /// # Panics
    ///
    /// Panics if `T::MODULE` does not match [`NativeModule::name`], if
    /// `T::NAME` is not a capitalized identifier, or if `T::NAME` is already
    /// registered in this module.
    pub fn value_type<T: ValueType>(&mut self) -> &mut Self {
        let info = ValueTypeInfo { ord: T::ORDER.is_some(), parse: crate::vm::value::parse_value::<T>, type_id: TypeId::of::<T>() };
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: true, kind: BoundKind::Value(info) })
    }

    /// Registers `T` as a public enum type of this module (see [`EnumType`]).
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`NativeModule::value_type`].
    pub fn enum_type<T: EnumType>(&mut self) -> &mut Self {
        let kind = BoundKind::Enum(TypeId::of::<T>(), T::VARIANTS.iter().map(|v| (v.name, v.doc)).collect());
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: true, kind })
    }

    /// Registers `T` as an opaque object type of this module (see
    /// [`ObjectType`]).
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`NativeModule::value_type`].
    pub fn object_type<T: ObjectType>(&mut self) -> &mut Self {
        let kind = BoundKind::Object(TypeId::of::<T>());
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: T::PUBLIC, kind })
    }

    pub(crate) fn builtin_type(&mut self, name: &'static str, doc: &'static str, params: &'static [&'static str]) -> &mut Self {
        assert!(self.kind == ModKind::Prelude, "only the prelude has built-in types");
        self.add_type(BoundType { module: "prelude", name, doc, public: true, kind: BoundKind::Builtin(params) })
    }

    fn add_type(&mut self, t: BoundType) -> &mut Self {
        assert_eq!(t.module, self.name, "type `{}` belongs to module `{}`", t.name, t.module);
        crate::vm::value::check_type_name(t.name);
        assert!(!self.types.iter().any(|u| u.name == t.name), "type `{}` is registered twice in module `{}`", t.name, self.name);
        self.types.push(t);
        self
    }

    /// Appends Sand source code (types, helper functions, and `native fn`
    /// declarations for registered functions) to this module.
    pub fn source(&mut self, src: &str) -> &mut Self {
        self.source.push_str(src);
        self.source.push('\n');
        self
    }

    pub(crate) fn register_types(&self, r: &mut Registry) {
        for t in &self.types {
            r.add_bound_type(t.clone());
        }
    }

    pub(crate) fn register(&self, r: &mut Registry) {
        for f in &self.functions {
            r.add_native(format!("{}.{}", self.name, f.name), f.f.clone(), f.sig.clone(), f.names.clone(), f.doc.clone());
        }
        r.source(self.name.clone(), self.kind, self.source.clone());
    }
}
