//! Native modules: Rust functions and types that Sand scripts use, with Sand
//! types derived from the Rust types.

mod convert;
mod func;
mod ty;

pub use convert::{Conversion, FromValue, HostError, IntoHostError, IntoValue};
pub use func::{AsyncHostFunction, GuardedFunction, HostFunction, Later, NativeError, Signature, VmFunction};
pub use ty::{Channel, Named, Never, Param, Range, Reflect, SandType, Task, Type, Val};

pub(crate) use convert::mismatch;

use crate::check::ModKind;
use crate::engine::Registry;
use crate::syntax::lexer::{is_ident, keyword};
use crate::types::ValueTypeInfo;
use crate::vm::heap::Heap;
use crate::vm::value::{Obj, TypeDesc, Value, ValueType};
use crate::vm::{Ctl, HostFn, NativeFn, Vm};
use std::any::{Any, TypeId};
use std::sync::Arc;

/// A unit-only Rust enum that maps to a Sand `enum` of the same variants.
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

/// A variant descriptor for an [`EnumType`].
pub struct EnumVariant<T> {
    pub name: &'static str,
    pub doc: &'static str,
    pub value: T,
}

/// Memory accounting for a Rust object that a script holds (see [`ObjectType`]).
pub trait NativeObject: Any + Send {
    /// Approximate memory used, in bytes.
    fn size(&self) -> usize {
        64
    }

    /// Whether the object holds an open resource (a file or a socket, with
    /// its `iomem::ResourceSlot`, which counts it against
    /// `max_open_resources`). Allocating such objects paces garbage
    /// collection.
    fn is_resource(&self) -> bool {
        false
    }
}

/// A Rust type whose values scripts hold as opaque objects of a Sand type,
/// such as a socket or a compiled regex. Objects have reference semantics
/// and no Sand operations except the module's functions.
///
/// Derive this trait with [`#[derive(SandObject)]`](crate::SandObject),
/// implement [`NativeObject`], and register the type with
/// [`NativeModule::object_type`]:
///
/// ```
/// /// A counter that functions of `stats` update.
/// #[derive(sandlang::SandObject)]
/// #[sand(module = "stats")]
/// pub struct Counter(std::sync::atomic::AtomicI64);
///
/// impl sandlang::native::NativeObject for Counter {}
/// ```
///
/// Host functions take an object as `&T` and return a new one as `T`.
/// Functions with access to the VM take a [`Val<T>`] handle.
pub trait ObjectType: NativeObject + SandType + Sized {
    /// Name of the Sand module that owns the type (`#[sand(module = "...")]`).
    const MODULE: &'static str;
    /// Sand type name (defaults to the Rust identifier, or `#[sand(name = "...")]`).
    const NAME: &'static str;
    /// Doc comment attached to the type declaration.
    const DOC: &'static str;
    /// Whether other modules can name the type (`false` with `#[sand(private)]`).
    const PUBLIC: bool;
}

/// An object on the heap, of an [`ObjectType`].
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
    /// The object.
    pub fn get(self, heap: &Heap) -> Result<&T, Ctl> {
        heap.native::<T>(self.raw()).ok_or_else(mismatch::<T>)
    }

    /// The object, to change it.
    pub fn get_mut(self, heap: &mut Heap) -> Result<&mut T, Ctl> {
        heap.native_mut::<T>(self.raw()).ok_or_else(mismatch::<T>)
    }
}

impl<const N: u8> Reflect<N> {
    /// The type description.
    pub fn desc(self, heap: &Heap) -> Result<&Arc<TypeDesc>, Ctl> {
        match heap.get(self.raw()) {
            Obj::TypeDesc(d) => Ok(d),
            _ => Err(Ctl::internal("a native function expected a type description")),
        }
    }
}

impl Val<String> {
    /// The string.
    pub fn str(self, heap: &Heap) -> &str {
        heap.str(self.raw())
    }
}

impl Val<Vec<u8>> {
    /// The bytes.
    pub fn bytes(self, heap: &Heap) -> &[u8] {
        heap.bytes_of(self.raw())
    }
}

impl<T> Val<Vec<T>> {
    /// The elements of the list, which have the type that `T` describes.
    pub fn items(self, heap: &Heap) -> &[Value] {
        heap.list(self.raw())
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

    /// Moves `x` to the heap, failing if it does not fit in the memory limit.
    pub fn new_object<T: ObjectType>(&mut self, x: T) -> Result<Val<T>, Ctl> {
        self.reserve(NativeObject::size(&x))?;
        Ok(Val::from_raw(self.heap.alloc(Obj::Native(Box::new(x)))))
    }
}

/// The Rust type that a module binds to one of its Sand types. The checker
/// declares the Sand type from it.
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
    /// An [`EnumType`], with the `(name, doc)` of each variant in declaration order.
    Enum(TypeId, Arc<[(&'static str, &'static str)]>),
    Object(TypeId),
    /// A type that the VM implements (`Task<T>` and `Channel<T>` of the
    /// prelude), with the names of its type parameters.
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

/// A native function of a module, as the checker sees it.
#[derive(Clone, Debug)]
pub(crate) struct NativeDecl {
    /// The native's id in the registry.
    pub id: u32,
    pub sig: Arc<Signature>,
    /// Parameter names and doc comment for a host function that the module
    /// source does not declare: the checker declares it from its signature.
    pub names: Arc<[String]>,
    pub doc: Option<Arc<str>>,
}

/// A function registered by a [`NativeModule`].
#[derive(Clone)]
struct FnDef {
    name: String,
    sig: Signature,
    f: HostFn,
    names: Option<Vec<String>>,
}

/// A module of Rust functions and types, imported by scripts with
/// `import <name>`.
///
/// Each function's Sand signature is derived from its Rust types. Unless the
/// module's Sand source declares the function with `native fn` (to name its
/// parameters, give defaults, or make it a method), the module declares it
/// as `pub native fn` with the parameter names that
/// [`NativeModule::params`] sets (`arg1`, `arg2`, ... by default). The engine
/// checks each declaration against the Rust types when it is created.
#[derive(Clone)]
pub struct NativeModule {
    name: String,
    kind: ModKind,
    source: String,
    functions: Vec<FnDef>,
    types: Vec<BoundType>,
    docs: Vec<(String, String)>,
}

impl NativeModule {
    /// A module imported as `name`.
    ///
    /// # Panics
    ///
    /// If `name` is not an identifier, or is the name of a standard module
    /// (or of the prelude).
    pub fn new(name: &str) -> NativeModule {
        assert!(is_ident(name) && keyword(name).is_none(), "a host module cannot be named `{name}`: it is not an identifier");
        assert!(!crate::stdlib::STD_MODULES.contains(&name), "a host module cannot be named `{name}`: it is the name of a standard module");
        NativeModule::of_kind(name, ModKind::Host)
    }

    /// A standard module (or the prelude) with Sand source `source`. Its
    /// functions live as long as the process.
    pub(crate) fn std(name: &str, kind: ModKind, source: &str) -> NativeModule {
        let mut m = NativeModule::of_kind(name, kind);
        m.source(source);
        m
    }

    fn of_kind(name: &str, kind: ModKind) -> NativeModule {
        NativeModule { name: name.to_string(), kind, source: String::new(), functions: Vec::new(), types: Vec::new(), docs: Vec::new() }
    }

    /// The name scripts import the module by.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn add(&mut self, name: &str, sig: Signature, f: Box<NativeFn>) -> &mut Self {
        let parts: Vec<&str> = name.split('.').collect();
        let valid = parts.len() <= 2 && parts.iter().all(|p| is_ident(p)) && parts.last() != Some(&"self");
        assert!(valid, "function name `{name}` is not `name` or `Type.name`");
        assert!(!self.functions.iter().any(|d| d.name == name), "function `{name}` is registered twice in module `{}`", self.name);
        // The standard modules are built once per process.
        let f = if self.kind == ModKind::Host { HostFn::Shared(f.into()) } else { HostFn::Static(Box::leak(f)) };
        self.functions.push(FnDef { name: name.to_string(), sig, f, names: None });
        self
    }

    /// Registers a synchronous function without access to the VM. Arguments
    /// of type `&str`, `&[u8]` and `&T` (for an [`ObjectType`] `T`) borrow
    /// the script's values instead of copying them.
    ///
    /// `name` is the function's name, or `Type.method` for a method that the
    /// module source declares in `extend Type` or an `impl` for `Type`.
    ///
    /// # Panics
    ///
    /// If `name` is not `name` or `Type.name`, or a function of this name
    /// is already registered.
    pub fn function<M, F: HostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host())
    }

    /// Registers a synchronous function with access to the VM, as
    /// [`NativeModule::function`] does. Its arguments are owned: it takes heap
    /// values that it does not copy as [`Val`] handles.
    pub fn vm_function<M, F: VmFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host())
    }

    /// Registers an async function: the calling task is suspended until the future completes.
    pub fn async_function<M, F: AsyncHostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host())
    }

    /// Registers a function guarded by a capability that `guard` computes
    /// from the arguments (given by reference). The Sand return type is
    /// `Result<R, Error>`; a denial yields a `permission_denied` error.
    pub fn guarded_function<M, G, F: GuardedFunction<M, G>>(&mut self, name: &str, guard: G, f: F) -> &mut Self {
        self.add(name, F::signature(), f.into_host(guard))
    }

    /// Names the parameters of the function registered last (shown by
    /// `help`, and usable as named arguments) if the module source does not
    /// declare it.
    ///
    /// # Panics
    ///
    /// If no function is registered, or the number of names differs from
    /// its number of parameters.
    pub fn params(&mut self, names: &[&str]) -> &mut Self {
        let Some(f) = self.functions.last_mut() else { panic!("`params` names the parameters of the function registered last") };
        assert_eq!(names.len(), f.sig.params.len(), "function `{}` has {} parameters", f.name, f.sig.params.len());
        f.names = Some(names.iter().map(|n| n.to_string()).collect());
        self
    }

    /// Registers `T` as a value type of this module (see [`ValueType`]); the
    /// module declares it as a public opaque type.
    ///
    /// # Panics
    ///
    /// If `T::MODULE` is not this module's name, if `T::NAME` is not a
    /// capitalized identifier, or if `T::NAME` is already registered.
    pub fn value_type<T: ValueType>(&mut self) -> &mut Self {
        let info = ValueTypeInfo { ord: T::ORDER.is_some(), parse: crate::vm::value::parse_value::<T>, type_id: TypeId::of::<T>() };
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: true, kind: BoundKind::Value(info) })
    }

    /// Registers `T` as an enum type of this module (see [`EnumType`]); the
    /// module declares the `pub enum` from `T`.
    ///
    /// # Panics
    ///
    /// As [`NativeModule::value_type`].
    pub fn enum_type<T: EnumType>(&mut self) -> &mut Self {
        let kind = BoundKind::Enum(TypeId::of::<T>(), T::VARIANTS.iter().map(|v| (v.name, v.doc)).collect());
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: true, kind })
    }

    /// Registers `T` as an object type of this module (see [`ObjectType`]);
    /// the module declares it as an opaque type.
    ///
    /// # Panics
    ///
    /// As [`NativeModule::value_type`].
    pub fn object_type<T: ObjectType>(&mut self) -> &mut Self {
        let kind = BoundKind::Object(TypeId::of::<T>());
        self.add_type(BoundType { module: T::MODULE, name: T::NAME, doc: T::DOC, public: T::PUBLIC, kind })
    }

    /// Registers a type that the VM implements.
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

    /// Appends Sand declarations to the module: types, functions, and
    /// `native fn` declarations of the registered functions.
    pub fn source(&mut self, src: &str) -> &mut Self {
        self.source.push_str(src);
        self.source.push('\n');
        self
    }

    /// Attaches a doc comment (shown by `help`) to a registered function
    /// that the module source does not declare.
    pub fn doc(&mut self, name: &str, text: &str) -> &mut Self {
        self.docs.retain(|(n, _)| n != name);
        self.docs.push((name.to_string(), text.to_string()));
        self
    }

    /// Adds the module's types to `r`, before any module's functions (whose
    /// signatures may use them).
    pub(crate) fn register_types(&self, r: &mut Registry) {
        for t in &self.types {
            r.add_bound_type(t.clone());
        }
    }

    /// Adds the module's functions and source to `r`.
    pub(crate) fn register(&self, r: &mut Registry) {
        for f in &self.functions {
            let doc = self.docs.iter().find(|(n, _)| *n == f.name).map(|(_, d)| Arc::from(d.as_str()));
            let names = f.names.clone().unwrap_or_else(|| (1..=f.sig.params.len()).map(|i| format!("arg{i}")).collect());
            r.add_native(format!("{}.{}", self.name, f.name), f.f.clone(), f.sig.clone(), names.into(), doc);
        }
        r.source(self.name.clone(), self.kind, self.source.clone());
    }
}
