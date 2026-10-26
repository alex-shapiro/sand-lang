//! Name resolution and type checking: AST to typed HIR.

pub mod budget;
mod derive;
mod docs;
mod expr;
mod fuse;
pub(crate) mod help;
mod pat;
pub mod persist;
mod resources;
mod traits;
mod unify;
mod verify;

use crate::diag::{Diag, FileId, SourceFile, Span};
use crate::engine::NativeDecls;
use crate::hir::*;
use crate::native::{BoundKind, Constant, EnumVariant, NativeDecl, RustVariant, Signature, StructField, Type};
use crate::syntax::ast::{self, ItemKind, TypeExpr, TypeExprKind};
use crate::types::*;
use crate::vm::value::{FxBuild, FxHashMap};
use budget::{CompileBudget, MAX_TYPE_SIZE};
use persist::{PMap, PVec};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub use pat::Witness;
pub use traits::{ImplDef, TraitDef, TraitMethod};
pub(crate) use verify::verify;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModKind {
    Prelude,
    Std,
    Host,
    Script { main: bool },
}

pub struct ParsedModule {
    pub name: String,
    pub file: FileId,
    pub source: Arc<SourceFile>,
    pub ast: ast::Module,
    /// The module's comments (for the linter: only the main module's).
    pub comments: Vec<crate::syntax::lexer::Comment>,
    pub kind: ModKind,
    /// For each `import` item (by item index): the target module index.
    pub imports: HashMap<usize, ModId>,
}

#[derive(Clone, Copy, Debug)]
pub enum Def {
    Fn(FnId),
    Type(TyRef),
    Global(GlobalId),
    Module(ModId),
    /// An enum variant usable unqualified (`Some`, `None`, `Ok`, `Err`).
    Variant(TypeDefId, u32),
    Trait(TraitId),
}

#[derive(Clone, Copy, Debug)]
pub enum TyRef {
    Prim(TypeId),
    /// Built-in generic collection (`List`, `Map`, or `Set`).
    Coll(Ctor),
    Def(TypeDefId),
    Alias(usize),
}

/// A name in a module's scope.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    pub def: Def,
    pub is_pub: bool,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ctor {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Duration,
    Unit,
    Range,
    List,
    Map,
    Set,
    Tuple,
    Fn,
    Def(TypeDefId),
}

impl Ctor {
    /// The built-in type constructors that `help` and the documentation describe.
    pub const BUILTIN: &[Ctor] =
        &[Ctor::Bool, Ctor::Int, Ctor::Float, Ctor::Str, Ctor::Bytes, Ctor::Duration, Ctor::Range, Ctor::List, Ctor::Map, Ctor::Set];

    pub fn native_prefix(self, defs: &PVec<TypeDef>, syms: &Symbols) -> String {
        match self {
            Ctor::Int => "int".into(),
            Ctor::Float => "float".into(),
            Ctor::Bool => "bool".into(),
            Ctor::Str => "str".into(),
            Ctor::Bytes => "bytes".into(),
            Ctor::Duration => "duration".into(),
            Ctor::Unit => "unit".into(),
            Ctor::Range => "Range".into(),
            Ctor::List => "List".into(),
            Ctor::Map => "Map".into(),
            Ctor::Set => "Set".into(),
            Ctor::Tuple => "Tuple".into(),
            Ctor::Fn => "Fn".into(),
            Ctor::Def(d) => syms.str(defs[d as usize].name).to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Alias {
    pub name: Symbol,
    pub generics: Vec<ParamId>,
    pub ty: Option<TypeId>,
    /// The declaration, until it is resolved.
    pub decl: Option<(ModId, Arc<ast::TypeExpr>)>,
    pub resolving: bool,
}

#[derive(Clone, Debug)]
pub struct ParamSig {
    pub name: Symbol,
    pub ty: TypeId,
    pub default: Option<DefaultArg>,
}

#[derive(Clone, Debug)]
pub enum DefaultArg {
    /// Inlined constant expression (closed: literals, empty collections, `None`).
    Inline(Arc<ast::Expr>),
    /// Computed by a helper function from the preceding parameters.
    Helper(FnId),
}

#[derive(Clone, Debug)]
pub enum FnKind {
    Script,
    Native(u32),
    Init,
    Test(Symbol),
    /// Computes the default value of a parameter or field.
    DefaultHelper,
    /// The signature of method `slot` of a trait; calls dispatch through a dictionary.
    TraitMethod(TraitId, u32),
    /// A prelude function whose calls the checker evaluates (`help`, `members`).
    Intrinsic(Intrinsic),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intrinsic {
    Help,
    Members,
}

#[derive(Clone, Debug)]
pub struct FnInfo {
    pub name: Symbol,
    pub module: ModId,
    pub generics: Vec<ParamId>,
    /// Number of leading entries in `generics` inherited from an enclosing
    /// type, `extend` block, trait, or `impl`.
    pub outer: usize,
    pub params: Vec<ParamSig>,
    pub ret: TypeId,
    pub has_self: bool,
    pub kind: FnKind,
    pub doc: Option<Arc<str>>,
    pub span: Span,
    pub is_pub: bool,
    pub owner: Option<Ctor>,
    pub self_ty: Option<TypeId>,
    pub decl: Option<FnDeclRef>,
    /// Hidden dictionary parameters `(type, trait)` passed before declared
    /// parameters. For trait methods, default methods, and `impl` methods, the
    /// first entry is the dictionary for `Self`.
    pub dicts: Vec<(TypeId, TraitRef)>,
    pub impl_of: Option<ImplId>,
    pub trait_of: Option<TraitId>,
}

impl FnInfo {
    /// Whether this function is the `init` of its type.
    pub(crate) fn is_init(&self, syms: &Symbols) -> bool {
        self.owner.is_some() && !self.has_self && syms.str(self.name) == "init"
    }

    /// The name of this function in function values and stack traces: the
    /// name of its type if it is an `init`, which `T(...)` calls.
    pub(crate) fn runtime_name(&self, types: &PVec<TypeDef>, syms: &Symbols) -> String {
        match self.owner {
            Some(c) if self.is_init(syms) => c.native_prefix(types, syms),
            _ => syms.str(self.name).to_string(),
        }
    }
}

#[derive(Default)]
struct ConstFoldState {
    cache: FxHashMap<FnId, Option<ConstVal>>,
    depth: usize,
    depth_exceeded: bool,
}

enum BoundMembers {
    Struct(&'static [StructField], Arc<[Option<Constant>]>),
    Enum(&'static [EnumVariant]),
}

/// Location of a function body in the module AST, or a synthetic body kind.
#[derive(Clone, Copy, Debug)]
pub enum FnDeclRef {
    Fn(DeclPath),
    ParamDefault(DeclPath, u32),
    FieldDefault { item: u32, field: u32 },
    BoundDefault(u32),
    Init,
    Test { item: u32 },
}

/// A function or method declared by the index of an item in a module.
#[derive(Clone, Copy, Debug)]
pub struct DeclPath {
    pub item: u32,
    pub method: u32,
}

impl DeclPath {
    pub const NOT_METHOD: u32 = u32::MAX;

    pub fn get(self, m: &ast::Module) -> &ast::FnDecl {
        let i = self.method as usize;
        match &m.items[self.item as usize].kind {
            ItemKind::Fn(f) => f,
            ItemKind::Struct(s) => &s.methods[i],
            ItemKind::Enum(e) => &e.methods[i],
            ItemKind::Extend(x) => &x.methods[i],
            ItemKind::Trait(t) => &t.methods[i],
            ItemKind::Impl(x) => &x.methods[i],
            _ => unreachable!("not a function item"),
        }
    }
}

/// Restrictions on the syntax of a function declaration (trait and impl
/// methods; their callers report what is ignored).
#[derive(Clone, Copy)]
pub(crate) struct DeclRules<'s> {
    /// `where` predicates on these type parameters are ignored.
    pub skip_where: &'s [(Symbol, ParamId)],
    /// Which parameter defaults are kept.
    pub defaults: DefaultRule,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultRule {
    All,
    /// Only closed constants (`is_closed_const`).
    Const,
    None,
}

impl DeclRules<'_> {
    pub(crate) const PLAIN: DeclRules<'static> = DeclRules { skip_where: &[], defaults: DefaultRule::All };
}

#[derive(Clone, Debug)]
pub struct GlobalInfo {
    pub name: Symbol,
    pub module: ModId,
    pub ty: TypeId,
    pub mutable: bool,
    pub is_pub: bool,
    pub span: Span,
    pub doc: Option<Arc<str>>,
}

#[derive(Clone)]
pub struct ModInfo {
    pub name: Symbol,
    pub file: FileId,
    pub kind: ModKind,
    pub scope: PMap<Symbol, Binding>,
    pub init: Option<FnId>,
    pub doc: Option<Arc<str>>,
    /// Names referenced (for unused-import lint): module aliases and from-imports.
    pub import_spans: Vec<(Symbol, Span)>,
    /// The names that imports bind to modules.
    pub module_imports: Vec<(ModId, Symbol)>,
}

/// Facts gathered during checking for the linter.
#[derive(Default, Debug, Clone)]
pub struct LintFacts {
    /// Import names of the main module and their spans.
    pub main_imports: Vec<(String, Span)>,
    pub unused_results: Vec<Span>,
    /// `argument-order`: a positional argument or sub-pattern, and its message.
    pub argument_order: Vec<(Span, String)>,
    /// The resource lints (`unclosed-resource`, `resource-field`,
    /// `resource-escapes`; see `resources`): span, code and message.
    pub resources: Vec<(Span, &'static str, String)>,
    pub used_names: HashSet<(ModId, String)>,
    pub locals: Vec<LocalFact>,
}

#[derive(Debug, Clone)]
pub struct LocalFact {
    pub span: Span,
    pub name: String,
    pub used: bool,
    pub mutable: bool,
    pub assigned: bool,
    pub ty: String,
    pub is_param: bool,
    pub global: bool,
    /// Identifies the function the binding belongs to (for shadowing checks).
    pub func: u32,
    /// For a binding whose name starts with an upper-case letter and whose
    /// type is an enum: the enum's variants without a payload, qualified
    /// (`State.Empty`), which a misspelled variant pattern may have meant.
    pub variants: Vec<String>,
}

/// The checked bodies of the functions checked since the bodies were last
/// released (see [`Checker::release_bodies`]), by function id.
#[derive(Clone, Default)]
pub struct Bodies {
    /// The id of the first function of `items`.
    first: usize,
    items: Vec<Option<HFn>>,
}

impl Bodies {
    pub fn get(&self, f: FnId) -> Option<&HFn> {
        self.items.get((f as usize).checked_sub(self.first)?)?.as_ref()
    }

    fn set(&mut self, f: FnId, body: HFn) {
        self.items[f as usize - self.first] = Some(body);
    }

    fn is_checked(&self, f: FnId) -> bool {
        self.get(f).is_some()
    }

    /// The bodies of functions `from` and later.
    pub fn from(&self, from: FnId) -> impl Iterator<Item = (FnId, &HFn)> + '_ {
        let skip = (from as usize).saturating_sub(self.first);
        self.items.iter().enumerate().skip(skip).filter_map(|(i, b)| Some(((self.first + i) as FnId, b.as_ref()?)))
    }
}

pub struct CheckedProgram {
    pub arena: TypeArena,
    pub syms: Symbols,
    pub fns: PVec<FnInfo>,
    pub bodies: Bodies,
    pub types: PVec<TypeDef>,
    pub params: PVec<ParamInfo>,
    pub wk: WellKnown,
    /// Runtime variant id -> (enum def, index within enum).
    pub variants: PVec<(TypeDefId, u32)>,
    /// Maps each registered Rust enum `TypeId` to the runtime ID of its first variant.
    pub enum_bindings: PVec<(std::any::TypeId, u32)>,
    /// Maps each registered Rust struct `TypeId` to its type definition.
    pub struct_bindings: PVec<(std::any::TypeId, TypeDefId)>,
    pub globals: PVec<GlobalInfo>,
    pub module_inits: Vec<FnId>,
    pub tests: Vec<(String, FnId)>,
    pub lint: LintFacts,
    pub main_module: ModId,
    pub traits: PVec<TraitDef>,
    pub impls: PVec<ImplDef>,
}

impl CheckedProgram {
    pub fn printer(&self) -> TypePrinter<'_> {
        TypePrinter::new(&self.arena, &self.types, &self.params, &self.syms, self.wk)
    }

    pub fn name(&self, s: Symbol) -> &str {
        self.syms.str(s)
    }

    /// Returns the Rust `TypeId` bound to type definition `d`, if any.
    pub fn rust_type(&self, d: TypeDefId) -> Option<std::any::TypeId> {
        rust_type(&self.types[d as usize], &self.enum_bindings, &self.struct_bindings, d)
    }
}

fn rust_type(
    def: &TypeDef,
    enum_bindings: &PVec<(std::any::TypeId, u32)>,
    struct_bindings: &PVec<(std::any::TypeId, TypeDefId)>,
    d: TypeDefId,
) -> Option<std::any::TypeId> {
    match &def.kind {
        TypeDefKind::Opaque(Opaque::Value(v)) => Some(v.type_id),
        TypeDefKind::Opaque(Opaque::Object(t)) => Some(*t),
        TypeDefKind::Struct { .. } => struct_bindings.iter_from(0).find(|(_, def)| *def == d).map(|(t, _)| *t),
        TypeDefKind::Enum { variants } => {
            let first = variants.first()?.rt_id;
            enum_bindings.iter_from(0).find(|(_, v)| *v == first).map(|(t, _)| *t)
        }
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct LocalData {
    pub ty: TypeId,
    pub scheme: Vec<ParamId>,
    pub used: bool,
    pub assigned: bool,
    pub is_param: bool,
    pub decl_span: Span,
}

#[derive(Clone)]
pub(crate) struct LoopCtx {
    /// The type of `break` values (`loop` only).
    pub break_ty: Option<TypeId>,
    pub label: Option<String>,
    /// The loop of a comprehension clause, which `break` and `continue` skip
    /// (it still counts for their depth).
    pub hidden: bool,
    /// Whether a `break` targets the loop.
    pub broke: bool,
}

#[derive(Clone)]
pub(crate) struct FnCtx {
    pub ret: TypeId,
    pub kind: CtxKind,
    pub loops: Vec<LoopCtx>,
    pub lambda: u32,
    /// Inside a deferred expression: the number of loops around it, which
    /// `break` and `continue` may not target (nor may `return` or `?` be used).
    pub in_defer: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtxKind {
    Function,
    Lambda,
    TopLevel { main: bool },
    Test,
}

/// The local names in scope in a function: a stack of scopes, also indexed
/// by name. A name bound again in the same scope replaces its binding there.
#[derive(Default, Clone)]
pub(crate) struct Scopes {
    /// Per scope, innermost last: name -> local.
    scopes: Vec<FxHashMap<Symbol, LocalId>>,
    /// Per name: the scopes binding it (indices into `scopes`), innermost last.
    by_name: FxHashMap<Symbol, Vec<u32>>,
}

impl Scopes {
    pub(crate) fn push(&mut self) {
        self.scopes.push(FxHashMap::default());
    }

    pub(crate) fn pop(&mut self) {
        for name in self.scopes.pop().unwrap_or_default().into_keys() {
            self.unbind(name);
        }
    }

    /// Binds `name` to `id` in the innermost scope.
    pub(crate) fn declare(&mut self, name: Symbol, id: LocalId) {
        let depth = self.scopes.len() as u32 - 1;
        if self.scopes.last_mut().unwrap().insert(name, id).is_none() {
            self.by_name.entry(name).or_default().push(depth);
        }
    }

    pub(crate) fn find(&self, name: Symbol) -> Option<LocalId> {
        let depth = *self.by_name.get(&name)?.last()?;
        self.scopes[depth as usize].get(&name).copied()
    }

    /// Removes the binding of `name` from the innermost scope, if any.
    pub(crate) fn remove(&mut self, name: Symbol) -> Option<LocalId> {
        let id = self.scopes.last_mut().unwrap().remove(&name)?;
        self.unbind(name);
        Some(id)
    }

    /// Drops the innermost entry of `name`'s list of scopes.
    fn unbind(&mut self, name: Symbol) {
        if let Some(depths) = self.by_name.get_mut(&name) {
            depths.pop();
            if depths.is_empty() {
                self.by_name.remove(&name);
            }
        }
    }
}

#[derive(Default, Clone)]
pub(crate) struct FnState {
    pub locals: Vec<LocalInfo>,
    pub data: Vec<LocalData>,
    pub scopes: Scopes,
    pub ctx: Vec<FnCtx>,
    pub next_lambda: u32,
    /// For each lambda (by id, from 1): the number of locals when it was
    /// started, so that its own locals are found without a scan of all
    /// (see `fuse.rs`, which re-owns the locals of inlined lambdas).
    pub lambda_first_local: Vec<LocalId>,
    /// Deferred trait bounds on types that contained unresolved inference
    /// variables.
    pub obligations: Vec<(TypeId, TraitRef, Span)>,
    /// Deferred `+` operand checks on types that contained unresolved inference
    /// variables.
    pub plus_checks: Vec<(TypeId, Span)>,
    /// Indicates that the checker is checking a struct field default, which
    /// does not receive trait dictionaries.
    pub field_default: bool,
    /// Inside the `init` of this type constructor, where `T(...)` builds a
    /// struct field by field.
    pub init_of: Option<Ctor>,
    /// `(type, span, call_kind)` triples for `reflect` type arguments, checked
    /// after type inference completes.
    pub reflect_checks: Vec<(TypeId, Span, &'static str)>,
    /// Maps each `reflect` type parameter of the current function to the hidden
    /// local that holds its runtime description.
    pub reflect_locals: Vec<(ParamId, LocalId)>,
    pub infer_checks: Vec<(TypeId, Span)>,
    /// Inside a top-level statement of a module (lets become globals).
    pub top_level: bool,
    pub generics: Vec<(Symbol, ParamId)>,
    pub self_ty: Option<TypeId>,
    /// In-scope trait dictionaries `(type, trait, dict_expr)` available in the
    /// current function.
    pub evidence: Vec<(TypeId, TraitRef, HExpr)>,
    /// Pending [`HKind::Evidence`] placeholders in the current function body.
    pub holes: Vec<EvidenceReq>,
    /// The inference variables made before the function, and those of them
    /// bound while checking it (see `release_vars`).
    pub var_mark: VarId,
    pub old_bound: Vec<VarId>,
    /// See [`HFn::doc_refs`].
    pub doc_refs: Vec<FnId>,
    /// The number of errors when the function was started.
    pub error_mark: usize,
}

/// The checker. Its tables are persistent (`persist`), so a copy is cheap:
/// the standard library's checker is copied by every compilation that
/// extends it, and a REPL session checks each input on a copy.
#[derive(Clone)]
pub struct Checker {
    /// The sources of the files checked, for diagnostics and `help`.
    sources: PMap<FileId, Arc<SourceFile>>,
    natives: Arc<NativeDecls>,
    pub arena: TypeArena,
    pub syms: Symbols,
    pub mods: PVec<ModInfo>,
    pub types: PVec<TypeDef>,
    pub aliases: PVec<Alias>,
    pub params: PVec<ParamInfo>,
    pub fns: PVec<FnInfo>,
    pub bodies: Bodies,
    pub globals: PVec<GlobalInfo>,
    pub methods: PMap<(Ctor, Symbol), Arc<[FnId]>>,
    /// The doc comments of `extend` blocks, which document built-in types.
    pub ctor_docs: PMap<Ctor, Arc<str>>,
    /// The modules scripts can import by name, with the summaries of their
    /// docs (for `help()`).
    pub module_index: Arc<[(String, String)]>,
    /// Maps each public standard-library function name to the module that defines it.
    pub module_exports: Arc<HashMap<String, String>>,
    pub variants: PVec<(TypeDefId, u32)>,
    /// Maps each registered Rust enum `TypeId` to the runtime ID of its first variant.
    pub enum_bindings: PVec<(std::any::TypeId, u32)>,
    /// Maps each registered Rust struct `TypeId` to its type definition.
    pub struct_bindings: PVec<(std::any::TypeId, TypeDefId)>,
    /// The bindings of the inference variables of the module being checked
    /// (cleared when it is done: its types are all resolved by then).
    pub subst: Vec<Option<TypeId>>,
    /// Derived implementations of the built-in traits for each type
    /// definition (`derive.rs`).
    pub(crate) derived: PVec<[derive::Derived; Builtin::ALL.len()]>,
    pub diags: Vec<Diag>,
    pub wk: WellKnown,
    pub prelude: Option<ModId>,
    pub task_mod: Option<ModId>,
    pub cur_mod: ModId,
    /// The first type definition of the module or session input being checked.
    pub(crate) first_type: usize,
    pub(crate) fs: FnState,
    pub lint: LintFacts,
    pub tests: Vec<(String, FnId)>,
    pub main_module: ModId,
    pub(crate) fs_unused: Vec<(TypeId, Span)>,
    /// The index in `lint.locals` of the fact of each global (when linting).
    pub(crate) global_facts: HashMap<GlobalId, usize>,
    /// Whether lint facts (`lint`) are gathered (for the main module only).
    pub lint_facts: bool,
    pub(crate) lint_func: u32,
    /// Names referenced from functions of the module being checked.
    pub(crate) fn_names: HashSet<String, FxBuild>,
    pub traits: PVec<TraitDef>,
    pub impls: PVec<ImplDef>,
    pub(crate) impls_by: PMap<(TraitId, Ctor, u32), Arc<[ImplId]>>,
    pub(crate) impls_by_ctor: PMap<(Ctor, u32), Arc<[ImplId]>>,
    /// Records variable bindings made by `unify` so they can be undone.
    pub(crate) unify_log: Option<Vec<VarId>>,
    /// Bounds the work of the check in progress (see `budget`).
    pub budget: Arc<CompileBudget>,
    /// The module of a REPL session, whose inputs are checked one at a time
    /// ([`Checker::check_cell`]).
    pub(crate) session: Option<ModId>,
    /// Whether the final expression statement of the session input being
    /// checked is its value, and then (after checking) the value's type.
    pub(crate) want_cell_value: bool,
    pub(crate) cell_value: Option<TypeId>,
    /// Whether the session input being checked has a top-level `return` of a value.
    pub(crate) cell_returns: bool,
    /// Set while checking an expression for `Session::type_of`.
    pub(crate) type_query: Option<TypeQuery>,
    pub(crate) builtins: [Option<TraitId>; Builtin::ALL.len()],
    /// The prelude's `Iterator` trait and `Iter` struct, which `for` loops use.
    pub(crate) iter_trait: Option<TraitId>,
    /// The prelude's `Close` trait, which `use` bindings call.
    pub(crate) close_trait: Option<TraitId>,
    /// What the parameters of analysed functions may do (see `resources`).
    pub(crate) flows: PMap<FnId, resources::ParamFlow>,
    pub(crate) iter_struct: Option<TypeDefId>,
    /// Prelude functions taking part in iterator fusion (found on first use).
    pub(crate) fuse_fns: Option<Arc<fuse::FuseFns>>,
    pub(crate) optimize: bool,
}

const MAX_ERRORS: usize = 50;

impl Default for Checker {
    fn default() -> Checker {
        Checker {
            sources: PMap::new(),
            natives: Arc::default(),
            arena: TypeArena::new(),
            syms: Symbols::new(),
            mods: PVec::new(),
            types: PVec::new(),
            aliases: PVec::new(),
            params: PVec::new(),
            fns: PVec::new(),
            bodies: Bodies::default(),
            globals: PVec::new(),
            methods: PMap::new(),
            ctor_docs: PMap::new(),
            module_index: Arc::from([]),
            module_exports: Arc::default(),
            variants: PVec::new(),
            enum_bindings: PVec::new(),
            struct_bindings: PVec::new(),
            subst: Vec::new(),
            derived: PVec::new(),
            diags: Vec::new(),
            wk: WellKnown::default(),
            prelude: None,
            task_mod: None,
            cur_mod: 0,
            first_type: 0,
            fs: FnState::default(),
            lint: LintFacts::default(),
            tests: Vec::new(),
            main_module: 0,
            fs_unused: Vec::new(),
            global_facts: HashMap::new(),
            lint_facts: true,
            lint_func: 0,
            fn_names: HashSet::default(),
            traits: PVec::new(),
            impls: PVec::new(),
            impls_by: PMap::new(),
            impls_by_ctor: PMap::new(),
            unify_log: None,
            session: None,
            want_cell_value: false,
            cell_value: None,
            cell_returns: false,
            type_query: None,
            budget: Arc::new(CompileBudget::default()),
            builtins: [None; Builtin::ALL.len()],
            iter_trait: None,
            close_trait: None,
            flows: PMap::new(),
            iter_struct: None,
            fuse_fns: None,
            optimize: true,
        }
    }
}

impl Checker {
    // ------------------------------------------------------------------
    // Types and names

    #[inline]
    pub fn ty(&self, t: TypeId) -> Ty {
        self.arena.get(t)
    }

    /// The node of type `t` with inference variables resolved at the top.
    #[inline]
    pub(crate) fn rty(&self, t: TypeId) -> Ty {
        self.arena.get(self.resolve(t))
    }

    #[inline]
    pub fn name(&self, s: Symbol) -> &str {
        self.syms.str(s)
    }

    pub(crate) fn sym(&mut self, s: &str) -> Symbol {
        self.syms.intern(s)
    }

    pub(crate) fn list_ty(&mut self, e: TypeId) -> TypeId {
        self.arena.list_of(e)
    }

    pub(crate) fn map_ty(&mut self, k: TypeId, v: TypeId) -> TypeId {
        self.arena.map_of(k, v)
    }

    pub(crate) fn tuple_ty(&mut self, ts: &[TypeId]) -> TypeId {
        self.arena.tuple(ts)
    }

    pub(crate) fn fn_ty(&mut self, ps: &[TypeId], r: TypeId) -> TypeId {
        self.arena.func(ps, r)
    }

    pub(crate) fn named_ty(&mut self, d: TypeDefId, args: &[TypeId]) -> TypeId {
        self.arena.named(d, args)
    }

    pub(crate) fn param_ty(&mut self, p: ParamId) -> TypeId {
        self.arena.param(p)
    }

    /// The elements of a type list (copied, to be used while checking).
    pub(crate) fn elems(&self, l: TyList) -> Vec<TypeId> {
        self.arena.list(l).to_vec()
    }

    /// The type `def<P1, ...>` of a definition applied to the type parameters `ps`.
    pub(crate) fn def_self_ty(&mut self, def: TypeDefId, ps: &[ParamId]) -> TypeId {
        let args: Vec<TypeId> = ps.iter().map(|&p| self.arena.param(p)).collect();
        self.arena.named(def, &args)
    }

    /// Whether `t` has more than `MAX_TYPE_SIZE` nodes once inference
    /// variables are resolved. Costs at most `MAX_TYPE_SIZE` steps (charged
    /// to the budget), even for types with exponentially large expansions.
    pub(crate) fn type_too_large(&self, t: TypeId) -> bool {
        fn walk(c: &Checker, t: TypeId, n: &mut usize) -> bool {
            if !c.arena.has_vars(t) {
                // Its size is known.
                *n += c.arena.size(t) as usize;
                return *n > MAX_TYPE_SIZE;
            }
            *n += 1;
            if *n > MAX_TYPE_SIZE {
                return true;
            }
            match c.ty(t) {
                Ty::Var(v) => match c.subst.get(v as usize) {
                    Some(Some(b)) => {
                        *n -= 1;
                        walk(c, *b, n)
                    }
                    _ => false,
                },
                Ty::List(a) | Ty::Set(a) => walk(c, a, n),
                Ty::Map(a, b) => walk(c, a, n) || walk(c, b, n),
                Ty::Tuple(l) | Ty::Named(_, l) => c.arena.list(l).iter().any(|&x| walk(c, x, n)),
                Ty::Fn(l, r) => c.arena.list(l).iter().any(|&x| walk(c, x, n)) || walk(c, r, n),
                _ => false,
            }
        }
        let mut n = 0;
        let large = walk(self, t, &mut n);
        self.budget.charge(n.min(MAX_TYPE_SIZE + 1) as u64);
        large
    }

    /// Replaces a type that is too large with `TypeId::ERROR`, reporting it.
    pub(crate) fn limit_type_size(&mut self, t: &mut TypeId, span: Span) {
        if *t != TypeId::ERROR && self.type_too_large(*t) {
            self.error(span, format!("type too large: types may have at most {MAX_TYPE_SIZE} components"));
            *t = TypeId::ERROR;
        }
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        let d = Diag::error(span, msg);
        // The same obligation can be reported through several paths.
        let dup = self.diags.iter().any(|x| x.span == d.span && x.message == d.message);
        if self.diags.len() < MAX_ERRORS && !dup {
            self.diags.push(d);
        }
    }

    /// Reports a missing type unless an earlier error in the function can be its cause.
    pub(crate) fn infer_error(&mut self, span: Span, msg: impl Into<String>) {
        if self.diags.len() == self.fs.error_mark {
            self.error(span, msg);
        }
    }

    /// Prints types, following the bindings of inference variables.
    pub fn printer(&self) -> TypePrinter<'_> {
        TypePrinter { subst: &self.subst, ..TypePrinter::new(&self.arena, &self.types, &self.params, &self.syms, self.wk) }
    }

    pub fn show(&self, t: TypeId) -> String {
        self.printer().show(t)
    }

    /// Visits the nodes of `t` written out as a tree, following the
    /// bindings of inference variables.
    pub(crate) fn visit(&self, t: TypeId, f: &mut impl FnMut(Ty)) {
        let ty = self.rty(t);
        f(ty);
        match ty {
            Ty::List(e) | Ty::Set(e) => self.visit(e, f),
            Ty::Map(k, v) => {
                self.visit(k, f);
                self.visit(v, f)
            }
            Ty::Tuple(l) | Ty::Named(_, l) => self.arena.list(l).iter().for_each(|&x| self.visit(x, f)),
            Ty::Fn(l, r) => {
                self.arena.list(l).iter().for_each(|&x| self.visit(x, f));
                self.visit(r, f)
            }
            _ => {}
        }
    }

    /// Shows the types of a type mismatch. Distinct types that would look
    /// the same (`expected Point, found Point`, e.g. after a session input
    /// defined `Point` again) are told apart by where they are defined.
    pub fn show_pair(&self, a: TypeId, b: TypeId) -> (String, String) {
        // Names of the type definitions mentioned, with their ids.
        let mut seen: Vec<TypeDefId> = Vec::new();
        for t in [a, b] {
            self.visit(t, &mut |t| {
                if let Ty::Named(d, _) = t
                    && !seen.contains(&d)
                {
                    seen.push(d);
                }
            });
        }
        let name = |d: TypeDefId| self.types[d as usize].name;
        let clashing: Vec<TypeDefId> = seen.iter().copied().filter(|&d| seen.iter().any(|&e| e != d && name(e) == name(d))).collect();
        let p = self.printer();
        if clashing.is_empty() {
            return (p.show(a), p.show(b));
        }
        let mut tags: Vec<(TypeDefId, String)> = clashing.iter().map(|&d| (d, self.def_origin(d, false))).collect();
        if tags.iter().any(|(d, t)| tags.iter().any(|(e, u)| e != d && u == t && name(*e) == name(*d))) {
            tags = clashing.iter().map(|&d| (d, self.def_origin(d, true))).collect();
        }
        let tag = |d: TypeDefId| tags.iter().find(|(e, _)| *e == d).map(|(_, t)| t.clone());
        (p.show_tagged(a, &tag), p.show_tagged(b, &tag))
    }

    /// Where type `d` is defined, for diagnostics: "defined in input 3" (of a
    /// session) or "defined in module geo"; `exact` adds the position.
    fn def_origin(&self, d: TypeDefId, exact: bool) -> String {
        let def = &self.types[d as usize];
        let file = self.sources.get(&def.span.file);
        let name = file.map_or("", |f| f.name.as_str());
        let place = match name.strip_prefix("<input ").and_then(|f| f.strip_suffix('>')) {
            Some(n) => format!("input {n}"),
            None if Some(def.module) == self.prelude => "the prelude".to_string(),
            None => format!("module {}", self.name(def.module_name)),
        };
        if exact && let Some(f) = file {
            let (line, col) = f.line_col(def.span.lo);
            format!("defined in {place}, at {name}:{line}:{col}")
        } else {
            format!("defined in {place}")
        }
    }

    /// Shows the type of a session input's value (`Session::type_of`). A
    /// function type whose inference variables are left open is shown as a
    /// generic one, named after the type parameters they instantiate (if
    /// recorded in `type_query`): `fn<T>(T) -> T`, `fn<T: Ord>([T]) -> [T]`.
    pub(crate) fn show_value_type(&self, t: TypeId) -> String {
        let mut vars: Vec<VarId> = Vec::new();
        self.visit(t, &mut |t| {
            if let Ty::Var(v) = t
                && !vars.contains(&v)
            {
                vars.push(v);
            }
        });
        if !matches!(self.rty(t), Ty::Fn(..)) || vars.is_empty() {
            return self.show(t);
        }
        // What variable `o` stands for now.
        let now = |o: VarId| match self.subst.get(o as usize) {
            Some(Some(b)) => self.rty(*b),
            _ => Ty::Var(o),
        };
        let origin = |v: VarId| {
            let q = self.type_query.as_ref()?;
            q.origins.iter().find(|(o, _)| now(*o) == Ty::Var(v)).map(|&(_, p)| p)
        };
        // Names: those of the parameters first, then T, U, ... for the others.
        let mut names: Vec<Option<String>> = vec![None; vars.len()];
        let mut used: Vec<String> = Vec::new();
        let take = |base: &str, used: &mut Vec<String>| {
            let mut name = base.to_string();
            let mut k = 2;
            while used.contains(&name) {
                name = format!("{base}{k}");
                k += 1;
            }
            used.push(name.clone());
            name
        };
        for (i, &v) in vars.iter().enumerate() {
            if let Some(p) = origin(v) {
                names[i] = Some(take(self.name(self.params[p as usize].name), &mut used));
            }
        }
        for name in names.iter_mut().filter(|n| n.is_none()) {
            let base = ["T", "U", "V", "W"].into_iter().find(|n| !used.iter().any(|u| u == n)).unwrap_or("T");
            *name = Some(take(base, &mut used));
        }
        let mut var_names: Vec<(VarId, String)> = Vec::new();
        let mut generics = Vec::new();
        for (&v, name) in vars.iter().zip(names) {
            let name = name.unwrap_or_default();
            let declared = origin(v).map(|p| &self.params[p as usize]);
            let required = self.type_query.iter().flat_map(|q| &q.traits).filter(|(t, _)| self.rty(*t) == Ty::Var(v)).map(|(_, tr)| *tr);
            let traits: Vec<TraitRef> = declared.into_iter().flat_map(|p| p.traits.iter().copied()).chain(required).collect();
            let shown: Vec<String> = self.minimal_traits(&traits).iter().map(|tr| self.show_trait(tr)).collect();
            generics.push(if shown.is_empty() { name.clone() } else { format!("{name}: {}", shown.join(" + ")) });
            var_names.push((v, name));
        }
        let shown = TypePrinter { var_names: &var_names, ..self.printer() }.show(t);
        format!("fn<{}>{}", generics.join(", "), shown.strip_prefix("fn").unwrap_or(&shown))
    }

    pub fn new_param(&mut self, name: Symbol, reflect: bool) -> ParamId {
        self.params.push(ParamInfo { name, traits: Vec::new(), reflect });
        (self.params.len() - 1) as ParamId
    }

    pub fn option_of(&mut self, t: TypeId) -> TypeId {
        self.arena.named(self.wk.option, &[t])
    }
    pub fn result_of(&mut self, t: TypeId, e: TypeId) -> TypeId {
        self.arena.named(self.wk.result, &[t, e])
    }
    pub fn error_ty(&mut self) -> TypeId {
        self.arena.named(self.wk.error, &[])
    }

    // ------------------------------------------------------------------
    // Program

    /// The import names of the main module and their spans (for the linter).
    pub fn main_imports(&self) -> Vec<(String, Span)> {
        self.mods[self.main_module as usize].import_spans.iter().map(|&(n, sp)| (self.name(n).to_string(), sp)).collect()
    }

    fn errors(&self) -> Vec<Diag> {
        self.diags.iter().filter(|d| d.severity == crate::diag::Severity::Error).cloned().collect()
    }

    /// Checks `modules`, in dependency order, after the modules checked so
    /// far: a module's [`ModId`] is its position in the combined list.
    fn add_modules(&mut self, modules: &[ParsedModule]) {
        let base = self.mods.len();
        for (i, m) in modules.iter().enumerate() {
            let i = base + i;
            self.sources.insert(m.file, m.source.clone());
            let name = self.sym(&m.name);
            self.mods.push(ModInfo {
                name,
                file: m.file,
                kind: m.kind,
                scope: PMap::new(),
                init: None,
                doc: m.ast.doc.as_deref().map(Arc::from),
                import_spans: Vec::new(),
                module_imports: Vec::new(),
            });
            if m.kind == ModKind::Prelude {
                self.prelude = Some(i as ModId);
            }
            if m.kind == ModKind::Std && m.name == "task" {
                self.task_mod = Some(i as ModId);
            }
            if m.kind == (ModKind::Script { main: true }) {
                self.main_module = i as ModId;
            }
        }
        for (i, m) in modules.iter().enumerate() {
            if !self.budget.check() {
                break;
            }
            let i = (base + i) as ModId;
            self.cur_mod = i;
            self.check_module(m);
        }
    }

    /// Moves the checked program out of the checker (see [`Checker::restore_checked`]).
    pub(crate) fn take_checked(&mut self) -> CheckedProgram {
        let module_inits = self.mods.iter().filter_map(|m| m.init).collect();
        CheckedProgram {
            arena: self.arena.clone(),
            syms: self.syms.clone(),
            fns: self.fns.clone(),
            bodies: std::mem::replace(&mut self.bodies, Bodies { first: self.fns.len(), items: Vec::new() }),
            types: self.types.clone(),
            params: self.params.clone(),
            wk: self.wk,
            variants: self.variants.clone(),
            enum_bindings: self.enum_bindings.clone(),
            struct_bindings: self.struct_bindings.clone(),
            globals: self.globals.clone(),
            module_inits,
            tests: std::mem::take(&mut self.tests),
            lint: std::mem::take(&mut self.lint),
            main_module: self.main_module,
            traits: self.traits.clone(),
            impls: self.impls.clone(),
        }
    }

    /// Puts back what [`Checker::take_checked`] moved out, so that checking
    /// can resume (the tables were shared, not moved).
    pub(crate) fn restore_checked(&mut self, cp: CheckedProgram) {
        self.bodies = cp.bodies;
        self.tests = cp.tests;
        self.lint = cp.lint;
    }

    // ------------------------------------------------------------------
    // REPL sessions
    //
    // A session is one script module whose items arrive one input ("cell") at
    // a time: `check_cell` checks each input as more items of that module,
    // after the modules it imports for the first time. Each input is checked
    // on a copy, so a rejected input leaves no trace.

    /// Starts a session: checks `modules` (on a checker that has checked the
    /// standard library, an empty module, `session`, which will receive the
    /// inputs).
    pub(crate) fn begin_session(&mut self, modules: &[ParsedModule], session: ModId) -> Result<(), Vec<Diag>> {
        self.session = Some(session);
        self.lint_facts = false;
        self.check_modules(modules, &Default::default(), &Default::default())
    }

    /// Checks `modules` (in dependency order, numbered from
    /// `self.mods.len()`) after the modules checked so far, keeping the
    /// checker's state: the standard library is checked once and extended
    /// by each program.
    pub(crate) fn check_modules(
        &mut self,
        modules: &[ParsedModule],
        natives: &Arc<NativeDecls>,
        budget: &Arc<CompileBudget>,
    ) -> Result<(), Vec<Diag>> {
        self.natives = natives.clone();
        self.budget = budget.clone();
        self.add_modules(modules);
        let errors = self.errors();
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    /// Checks one session input, `cell`, after `modules` (the modules it
    /// imports for the first time, in dependency order, numbered from
    /// `self.mods.len()`). Its names are added to the session module, where
    /// they shadow those of earlier inputs. With `want_value`, a final
    /// expression statement is the value of the input; its type is then left
    /// in [`Checker::cell_value`].
    pub(crate) fn check_cell(
        &mut self,
        modules: &[ParsedModule],
        cell: &ParsedModule,
        want_value: bool,
        natives: &Arc<NativeDecls>,
        budget: &Arc<CompileBudget>,
    ) -> Result<(), Vec<Diag>> {
        let session = self.session.expect("not a session");
        self.natives = natives.clone();
        self.budget = budget.clone();
        self.diags.clear();
        self.add_modules(modules);
        self.sources.insert(cell.file, cell.source.clone());
        self.cur_mod = session;
        let m = &mut self.mods[session as usize];
        m.file = cell.file;
        m.import_spans.clear();
        self.want_cell_value = want_value;
        self.cell_value = None;
        self.cell_returns = false;
        self.check_module(cell);
        self.want_cell_value = false;
        let errors = self.errors();
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    pub(crate) fn in_session(&self) -> bool {
        self.session == Some(self.cur_mod)
    }

    /// Whether `span` is in an earlier input of the session whose input is
    /// being checked; the definitions of earlier inputs may be shadowed.
    fn defined_by_earlier_input(&self, span: Span) -> bool {
        self.in_session() && span.file != self.mods[self.cur_mod as usize].file
    }

    /// Drops the bodies of the functions checked so far (and the location
    /// of their syntax), which a session no longer needs once they are
    /// compiled.
    pub(crate) fn release_bodies(&mut self) {
        let ids: Vec<FnId> = self.bodies.from(0).map(|(f, _)| f).collect();
        for f in ids {
            self.fns[f as usize].decl = None;
        }
        self.bodies = Bodies { first: self.fns.len(), items: Vec::new() };
    }

    fn find_well_known(&mut self) {
        let p = self.prelude.unwrap();
        let get = |c: &Checker, n: &str| match c.syms.get(n).and_then(|s| c.mods[p as usize].scope.get(&s)) {
            Some(Binding { def: Def::Type(TyRef::Def(d)), .. }) => *d,
            _ => panic!("prelude is missing `{n}`"),
        };
        self.wk.option = get(self, "Option");
        self.wk.result = get(self, "Result");
        self.wk.error = get(self, "Error");
        self.wk.task = get(self, "Task");
        self.wk.channel = get(self, "Channel");
    }

    pub fn fn_signature(&self, f: FnId) -> String {
        let fi = &self.fns[f as usize];
        let params: Vec<(&str, TypeId, bool)> = fi.params.iter().map(|p| (self.name(p.name), p.ty, p.default.is_some())).collect();
        self.printer().signature(self.name(fi.name), &params, fi.ret)
    }

    // ------------------------------------------------------------------
    // Modules

    fn define(&mut self, name: Symbol, def: Def, is_pub: bool, span: Span) {
        let m = self.cur_mod as usize;
        if let Some(existing) = self.mods[m].scope.get(&name) {
            // Re-binding a global with `let` shadows, as does any definition of
            // a later session input; anything else is a duplicate.
            let shadowable =
                matches!((existing.def, def), (Def::Global(_), Def::Global(_))) || self.defined_by_earlier_input(existing.span);
            if !shadowable {
                self.error(span, format!("`{}` is already defined", self.name(name)));
                return;
            }
        }
        self.mods[m].scope.insert(name, Binding { def, is_pub, span });
    }

    fn define_ident(&mut self, id: &ast::Ident, def: Def, is_pub: bool) {
        let name = self.sym(&id.name);
        self.define(name, def, is_pub, id.span);
    }

    fn check_module(&mut self, pm: &ParsedModule) {
        let mi = self.cur_mod;
        let first_global = self.globals.len();
        let error_mark = self.diags.len();
        let first_fn = self.fns.len();
        let first_type = self.types.len();
        self.first_type = first_type;
        let items = &pm.ast.items;
        // 1. Imports.
        for (idx, item) in items.iter().enumerate() {
            if let ItemKind::Import(imp) = &item.kind {
                let Some(&target) = pm.imports.get(&idx) else {
                    continue;
                };
                self.bind_import(imp, target, item.span);
            }
        }
        // 2. Declare traits, then types.
        let mut trait_ids = Vec::new();
        for item in items {
            if let ItemKind::Trait(t) = &item.kind {
                let id = self.declare_trait(t);
                // Keep the prelude built-in trait in scope if a user module redefines its name.
                if Builtin::parse(&t.name.name).is_none() || pm.kind == ModKind::Prelude {
                    self.define_ident(&t.name, Def::Trait(id), t.is_pub);
                }
                trait_ids.push(id);
            }
        }
        let bound_members = if self.allows_native() { self.declare_bound_types() } else { Vec::new() };
        let mut struct_ids = Vec::new();
        // The traits of type parameters may take types declared later as arguments.
        let mut type_params: Vec<(&[ast::GenericParam], Vec<ParamId>)> = Vec::new();
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let generics = self.declare_param_names(&s.generics);
                    type_params.push((&s.generics, generics.clone()));
                    let kind = TypeDefKind::Struct { fields: vec![] };
                    let id = self.new_typedef(&s.name, generics, kind, s.is_pub, s.doc.as_deref());
                    self.define_ident(&s.name, Def::Type(TyRef::Def(id)), s.is_pub);
                    struct_ids.push(id);
                }
                ItemKind::Enum(e) => {
                    let generics = self.declare_param_names(&e.generics);
                    type_params.push((&e.generics, generics.clone()));
                    let kind = TypeDefKind::Enum { variants: vec![] };
                    let id = self.new_typedef(&e.name, generics, kind, e.is_pub, e.doc.as_deref());
                    self.define_ident(&e.name, Def::Type(TyRef::Def(id)), e.is_pub);
                    struct_ids.push(id);
                }
                ItemKind::TypeAlias(a) => {
                    for g in a.generics.iter().filter(|g| !g.traits.is_empty()) {
                        self.error(
                            g.name.span,
                            "the type parameters of a type alias cannot require traits; require them on the functions that use the alias",
                        );
                    }
                    let generics = self.declare_param_names(&a.generics);
                    let name = self.sym(&a.name.name);
                    self.aliases.push(Alias { name, generics, ty: None, decl: Some((mi, Arc::new(a.ty.clone()))), resolving: false });
                    let id = self.aliases.len() - 1;
                    self.define(name, Def::Type(TyRef::Alias(id)), a.is_pub, a.name.span);
                }
                _ => {}
            }
        }
        for (gs, ps) in type_params {
            let scope = self.generic_scope(&ps);
            self.resolve_param_traits(gs, &ps, &scope, None);
        }
        if pm.kind == ModKind::Prelude {
            self.find_well_known();
            if let Some(Def::Trait(t)) = self.scope_def(mi, "Iterator") {
                self.iter_trait = Some(t);
            }
            if let Some(Def::Trait(t)) = self.scope_def(mi, "Close") {
                self.close_trait = Some(t);
            }
            if let Some(Def::Type(TyRef::Def(d))) = self.scope_def(mi, "Iter") {
                self.iter_struct = Some(d);
            }
        }
        // Resolve aliases eagerly so errors are reported once.
        for i in 0..self.aliases.len() {
            if self.aliases[i].ty.is_none() && self.aliases[i].decl.as_ref().map(|d| d.0) == Some(mi) {
                self.resolve_alias(i, Span::default());
            }
        }
        // Supertraits, then trait method signatures.
        let trait_decls: Vec<(u32, &ast::TraitDecl)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, x)| if let ItemKind::Trait(t) = &x.kind { Some((i as u32, t)) } else { None })
            .collect();
        self.resolve_supertraits(&trait_ids, &trait_decls);
        // 3. Resolve struct fields and enum variants, infer implied member
        // bounds, and derive built-in traits. Well-formedness checks on member
        // types run after `impl` declarations are registered.
        let mut member_types: Vec<(TypeDefId, TypeId, Span)> = Vec::new();
        let mut bound_ids = Vec::new();
        for (id, members) in bound_members {
            self.resolve_bound_members(id, members);
            bound_ids.push(id);
        }
        let mut k = 0;
        for (idx, item) in items.iter().enumerate() {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let id = struct_ids[k];
                    k += 1;
                    self.fill_struct(id, s, idx as u32, &mut member_types);
                }
                ItemKind::Enum(e) => {
                    let id = struct_ids[k];
                    k += 1;
                    self.fill_enum(id, e, &mut member_types);
                }
                _ => {}
            }
        }
        self.infer_member_requirements(first_type);
        // `Decode` depends on which structs declare `init`, so it is derived
        // after the methods are declared.
        let early: Vec<Builtin> = Builtin::ALL.iter().copied().filter(|&b| b.is_derived() && b != Builtin::Decode).collect();
        self.derive_impls(first_type, &early);
        if pm.kind == ModKind::Prelude {
            for name in ["Option", "Result"] {
                if let Some(Def::Type(TyRef::Def(d))) = self.scope_def(mi, name) {
                    let span = self.types[d as usize].span;
                    let vs: Vec<Symbol> = self.types[d as usize].variants().iter().map(|v| v.name).collect();
                    for (i, vn) in vs.into_iter().enumerate() {
                        self.define(vn, Def::Variant(d, i as u32), true, span);
                    }
                }
            }
        }
        for (&id, &(idx, t)) in trait_ids.iter().zip(&trait_decls) {
            self.declare_trait_methods(id, t, idx);
        }
        // 4. Functions and methods.
        let mut k = 0;
        for (idx, item) in items.iter().enumerate() {
            let at = |method: usize| DeclPath { item: idx as u32, method: method as u32 };
            match &item.kind {
                ItemKind::Fn(f) => {
                    let id = self.declare_fn(f, at(DeclPath::NOT_METHOD as usize), DeclRules::PLAIN, None, &[], None);
                    self.define_ident(&f.name, Def::Fn(id), f.is_pub);
                }
                ItemKind::Struct(ast::StructDecl { methods, .. }) | ItemKind::Enum(ast::EnumDecl { methods, .. }) => {
                    let id = struct_ids[k];
                    k += 1;
                    for (j, m) in methods.iter().enumerate() {
                        self.declare_method(m, at(j), Ctor::Def(id), id);
                    }
                }
                ItemKind::Extend(x) => self.declare_extend(x, idx as u32),
                _ => {}
            }
        }
        self.derive_impls(first_type, &[Builtin::Decode]);
        // Impls, then the supertrait requirements of each.
        let mut impl_ids = Vec::new();
        for (idx, item) in items.iter().enumerate() {
            if let ItemKind::Impl(i) = &item.kind
                && let Some(id) = self.declare_impl(i, idx as u32)
            {
                impl_ids.push(id);
            }
        }
        for id in impl_ids {
            self.check_impl_supertraits(id);
        }
        for (d, t, span) in member_types {
            self.check_member_type(d, t, span, "");
        }
        for d in bound_ids {
            self.check_bound_member_types(d);
        }
        if self.allows_native() {
            self.declare_undeclared_natives(first_fn);
            // A native module can register an `init` without declaring it.
            self.derive_impls(first_type, &[Builtin::Decode]);
        }
        // 5. Top-level code.
        let mod_name = self.name(self.mods[mi as usize].name).to_string();
        let init_name = self.sym(&format!("<{mod_name}>"));
        let init = self.add_fn(FnInfo {
            name: init_name,
            module: mi,
            generics: vec![],
            outer: 0,
            params: vec![],
            ret: TypeId::UNIT,
            has_self: false,
            kind: FnKind::Init,
            doc: None,
            span: pm.ast.span,
            is_pub: false,
            owner: None,
            self_ty: None,
            decl: Some(FnDeclRef::Init),
            dicts: vec![],
            impl_of: None,
            trait_of: None,
        });
        self.mods[mi as usize].init = Some(init);
        self.check_init(init, pm);
        // 6. Function bodies (including default helpers, which may be added while checking).
        let mut i = first_fn;
        while i < self.fns.len() {
            if !self.bodies.is_checked(i as FnId)
                && !matches!(self.fns[i].kind, FnKind::Native(_) | FnKind::Init)
                && let Some(decl) = self.fns[i].decl
            {
                self.check_fn_body(i as FnId, decl, &pm.ast);
            }
            i += 1;
        }
        self.fold_field_defaults(first_fn);
        // 7. Tests (those of imported modules are type checked, not run).
        for (idx, item) in items.iter().enumerate() {
            if let ItemKind::Test(t) = &item.kind {
                let name = self.sym(&t.name);
                let id = self.add_fn(FnInfo {
                    name,
                    module: mi,
                    generics: vec![],
                    outer: 0,
                    params: vec![],
                    ret: TypeId::UNIT,
                    has_self: false,
                    kind: FnKind::Test(name),
                    doc: None,
                    span: t.name_span,
                    is_pub: false,
                    owner: None,
                    self_ty: None,
                    decl: Some(FnDeclRef::Test { item: idx as u32 }),
                    dicts: vec![],
                    impl_of: None,
                    trait_of: None,
                });
                self.check_fn_body(id, FnDeclRef::Test { item: idx as u32 }, &pm.ast);
                if pm.kind == (ModKind::Script { main: true }) {
                    self.tests.push((t.name.clone(), id));
                }
            }
        }
        // Where resources go (for the resource lints).
        self.resource_flows(first_fn, &pm.ast);
        // 8. Globals must have fully inferred types.
        for g in first_global..self.globals.len() {
            if self.globals[g].module == mi {
                let t = self.zonk(self.globals[g].ty);
                if self.arena.has_vars(t) {
                    if self.diags.len() == error_mark {
                        let span = self.globals[g].span;
                        self.error(span, format!("cannot infer type of `{}`; add a type annotation", self.name(self.globals[g].name)));
                    }
                    self.globals[g].ty = TypeId::ERROR;
                } else {
                    self.globals[g].ty = t;
                }
                if let Some(&i) = self.global_facts.get(&(g as GlobalId)) {
                    self.lint.locals[i].ty = self.show(self.globals[g].ty);
                }
            }
        }
        // The module's types are all resolved: its inference variables are
        // no longer needed (unless a type query shows them).
        if self.type_query.is_none() {
            if let Some(t) = self.cell_value {
                self.cell_value = Some(self.zonk(t));
            }
            self.subst.clear();
        }
    }

    /// What `name` is bound to in the scope of module `m`.
    fn scope_def(&self, m: ModId, name: &str) -> Option<Def> {
        let s = self.syms.get(name)?;
        self.mods[m as usize].scope.get(&s).map(|b| b.def)
    }

    fn allows_native(&self) -> bool {
        !matches!(self.mods[self.cur_mod as usize].kind, ModKind::Script { .. })
    }

    fn bind_import(&mut self, imp: &ast::Import, target: ModId, span: Span) {
        match &imp.names {
            None => {
                let name = match (&imp.alias, &imp.source) {
                    (Some(a), _) => self.sym(&a.name),
                    (None, ast::ImportSource::Name(n)) => self.sym(&n.name),
                    (None, ast::ImportSource::Path(_, _)) => self.mods[target as usize].name,
                };
                let nspan = imp.alias.as_ref().map(|a| a.span).unwrap_or(match &imp.source {
                    ast::ImportSource::Name(n) => n.span,
                    ast::ImportSource::Path(_, s) => *s,
                });
                self.define(name, Def::Module(target), false, nspan);
                let m = &mut self.mods[self.cur_mod as usize];
                m.module_imports.push((target, name));
                m.import_spans.push((name, span));
            }
            Some(names) => {
                for n in names {
                    let name = self.sym(&n.name);
                    let found = self.mods[target as usize].scope.get(&name).copied();
                    match found {
                        Some(Binding { def, is_pub: true, .. }) => {
                            self.define(name, def, false, n.span);
                            self.mods[self.cur_mod as usize].import_spans.push((name, n.span));
                        }
                        Some(_) => self.error(n.span, format!("`{}` is private", n.name)),
                        None => {
                            let m = self.name(self.mods[target as usize].name).to_string();
                            self.error(n.span, format!("module `{m}` has no public item `{}`", n.name))
                        }
                    }
                }
            }
        }
    }

    /// Declares type parameters `gs` and resolves their trait bounds against
    /// `outer` and the newly declared parameters.
    fn declare_params(&mut self, gs: &[ast::GenericParam], outer: &[(Symbol, ParamId)], self_ty: Option<TypeId>) -> Vec<ParamId> {
        let out = self.declare_param_names(gs);
        let mut scope = outer.to_vec();
        scope.extend(self.generic_scope(&out));
        self.resolve_param_traits(gs, &out, &scope, self_ty);
        out
    }

    fn declare_param_names(&mut self, gs: &[ast::GenericParam]) -> Vec<ParamId> {
        gs.iter()
            .map(|g| {
                let name = self.sym(&g.name.name);
                self.new_param(name, g.reflect)
            })
            .collect()
    }

    fn resolve_param_traits(&mut self, gs: &[ast::GenericParam], ps: &[ParamId], scope: &[(Symbol, ParamId)], self_ty: Option<TypeId>) {
        for (g, &p) in gs.iter().zip(ps) {
            let traits = self.resolve_traits(&g.traits, scope, self_ty);
            self.add_traits(p, traits);
        }
    }

    fn new_typedef(&mut self, name: &ast::Ident, generics: Vec<ParamId>, kind: TypeDefKind, is_pub: bool, doc: Option<&str>) -> TypeDefId {
        let module_name = self.mods[self.cur_mod as usize].name;
        let name_sym = self.sym(&name.name);
        self.types.push(TypeDef {
            name: name_sym,
            module: self.cur_mod,
            module_name,
            generics,
            kind,
            is_pub,
            doc: doc.map(Arc::from),
            span: name.span,
        });
        (self.types.len() - 1) as TypeDefId
    }

    pub fn add_fn(&mut self, f: FnInfo) -> FnId {
        self.fns.push(f);
        self.bodies.items.push(None);
        (self.fns.len() - 1) as FnId
    }

    fn generic_scope(&self, ps: &[ParamId]) -> Vec<(Symbol, ParamId)> {
        ps.iter().map(|&p| (self.params[p as usize].name, p)).collect()
    }

    fn fill_struct(&mut self, id: TypeDefId, s: &ast::StructDecl, item: u32, written: &mut Vec<(TypeDefId, TypeId, Span)>) {
        let gens = self.types[id as usize].generics.clone();
        let scope = self.generic_scope(&gens);
        let self_ty = self.def_self_ty(id, &gens);
        let mut fields: Vec<FieldDef> = Vec::new();
        for f in &s.fields {
            if fields.iter().any(|x| self.name(x.name) == f.name.name) {
                self.error(f.name.span, format!("field `{}` is defined twice", f.name.name));
                continue;
            }
            let ty = self.resolve_type_in(&f.ty, &scope, Some(self_ty));
            written.push((id, ty, f.ty.span));
            if s.is_pub && f.is_pub && self.allows_native() {
                self.check_public_types("field", &f.name, [ty]);
            }
            let name = self.sym(&f.name.name);
            fields.push(FieldDef {
                name,
                ty,
                mutable: f.mutable,
                is_pub: f.is_pub,
                default: None,
                const_default: None,
                doc: f.doc.as_deref().map(Arc::from),
            });
        }
        // Default helpers.
        for (i, f) in s.fields.iter().enumerate() {
            if let Some(d) = &f.default {
                if i >= fields.len() {
                    break;
                }
                let name = self.sym(&format!("{}.{}", s.name.name, f.name.name));
                let helper = self.add_fn(FnInfo {
                    name,
                    module: self.cur_mod,
                    generics: gens.clone(),
                    outer: gens.len(),
                    params: vec![],
                    ret: fields[i].ty,
                    has_self: false,
                    kind: FnKind::DefaultHelper,
                    doc: None,
                    span: d.span,
                    is_pub: false,
                    owner: None,
                    self_ty: Some(self_ty),
                    decl: Some(FnDeclRef::FieldDefault { item, field: i as u32 }),
                    dicts: vec![],
                    impl_of: None,
                    trait_of: None,
                });
                fields[i].default = Some(helper);
            }
        }
        self.types[id as usize].kind = TypeDefKind::Struct { fields };
    }

    /// Resolves the variants of enum `id` and appends their payload types to `written`.
    fn fill_enum(&mut self, id: TypeDefId, e: &ast::EnumDecl, written: &mut Vec<(TypeDefId, TypeId, Span)>) {
        let gens = self.types[id as usize].generics.clone();
        let scope = self.generic_scope(&gens);
        let self_ty = self.def_self_ty(id, &gens);
        let mut variants: Vec<VariantDef> = Vec::new();
        for (i, v) in e.variants.iter().enumerate() {
            if variants.iter().any(|x| self.name(x.name) == v.name.name) {
                self.error(v.name.span, format!("variant `{}` is defined twice", v.name.name));
                continue;
            }
            let fields: Vec<TypeId> = v.fields.iter().map(|f| self.resolve_type_in(&f.ty, &scope, Some(self_ty))).collect();
            written.extend(fields.iter().zip(&v.fields).map(|(&t, f)| (id, t, f.ty.span)));
            let names: Vec<Option<Symbol>> = v.fields.iter().map(|f| f.name.as_ref().map(|n| self.syms.intern(&n.name))).collect();
            if names.iter().any(Option::is_some) && names.iter().any(Option::is_none) {
                let msg = format!("the fields of variant `{}` must be all named or all positional", v.name.name);
                self.error(v.name.span, msg);
            }
            let field_names = if !names.is_empty() && names.iter().all(|n| n.is_some()) { names.into_iter().collect() } else { None };
            self.variants.push((id, i as u32));
            let rt_id = (self.variants.len() - 1) as u32;
            let name = self.sym(&v.name.name);
            variants.push(VariantDef { name, fields, field_names, rt_id, doc: v.doc.as_deref().map(Arc::from) });
        }
        // Keep runtime index consistent with position in `variants`.
        for (i, v) in variants.iter().enumerate() {
            self.variants[v.rt_id as usize] = (id, i as u32);
        }
        self.types[id as usize].kind = TypeDefKind::Enum { variants };
    }

    fn declare_bound_types(&mut self) -> Vec<(TypeDefId, BoundMembers)> {
        let natives = self.natives.clone();
        let module = self.name(self.mods[self.cur_mod as usize].name).to_string();
        let span = Span::new(self.mods[self.cur_mod as usize].file, 0, 0);
        let mut members = Vec::new();
        for t in natives.types.iter().filter(|t| t.module == module) {
            let ident = ast::Ident { name: t.name.to_string(), span };
            let doc = Some(t.doc).filter(|d| !d.is_empty());
            let params: &[&str] = match &t.kind {
                BoundKind::Builtin(params) | BoundKind::Enum { params, .. } | BoundKind::Struct { params, .. } => params,
                BoundKind::Value(_) | BoundKind::Object(_) => &[],
            };
            let ps = params.iter().map(|p| self.sym(p)).collect::<Vec<_>>();
            let generics = ps.into_iter().map(|p| self.new_param(p, false)).collect();
            let kind = match &t.kind {
                BoundKind::Value(v) => TypeDefKind::Opaque(Opaque::Value(*v)),
                BoundKind::Object(id) => TypeDefKind::Opaque(Opaque::Object(*id)),
                BoundKind::Builtin(_) => TypeDefKind::Opaque(Opaque::Builtin),
                BoundKind::Enum { .. } => TypeDefKind::Enum { variants: vec![] },
                BoundKind::Struct { .. } => TypeDefKind::Struct { fields: vec![] },
            };
            let id = self.new_typedef(&ident, generics, kind, t.public, doc);
            self.define_ident(&ident, Def::Type(TyRef::Def(id)), t.public);
            match &t.kind {
                BoundKind::Enum { rust, variants, .. } => {
                    let mut defs = Vec::new();
                    for (i, v) in variants.iter().enumerate() {
                        self.variants.push((id, i as u32));
                        let rt_id = (self.variants.len() - 1) as u32;
                        let doc = Some(v.doc).filter(|d| !d.is_empty()).map(Arc::from);
                        let names: Option<Vec<Symbol>> = v.fields.iter().map(|f| f.name.map(|n| self.sym(n))).collect();
                        let field_names = names.filter(|ns| !ns.is_empty());
                        defs.push(VariantDef { name: self.sym(v.name), fields: vec![], field_names, rt_id, doc });
                    }
                    if let Some(first) = defs.first() {
                        self.enum_bindings.push((*rust, first.rt_id));
                    }
                    self.types[id as usize].kind = TypeDefKind::Enum { variants: defs };
                    members.push((id, BoundMembers::Enum(variants)));
                }
                BoundKind::Struct { rust, fields, defaults, .. } => {
                    self.struct_bindings.push((*rust, id));
                    members.push((id, BoundMembers::Struct(fields, defaults.clone())));
                }
                _ => {}
            }
        }
        members
    }

    /// Resolves field or payload types for a Rust-bound type after all types in
    /// the module are declared.
    fn resolve_bound_members(&mut self, id: TypeDefId, members: BoundMembers) {
        let span = self.types[id as usize].span;
        let qual = format!("{}.{}", self.name(self.types[id as usize].module_name), self.name(self.types[id as usize].name));
        let generics = self.types[id as usize].generics.clone();
        let public = self.types[id as usize].is_pub;
        match members {
            BoundMembers::Struct(rust, defaults) => {
                let self_ty = self.def_self_ty(id, &generics);
                let mut fields = Vec::new();
                for (i, (f, default)) in rust.iter().zip(defaults.iter()).enumerate() {
                    let owner = format!("the Rust field `{}` of `{qual}`", f.name);
                    let ty = self.host_type(&(f.ty)(), &generics, &owner, span).unwrap_or(TypeId::ERROR);
                    if public && f.public {
                        self.check_public_types("field", &ast::Ident { name: f.name.to_string(), span }, [ty]);
                    }
                    let doc = Some(f.doc).filter(|d| !d.is_empty()).map(Arc::from);
                    let name = self.sym(f.name);
                    let const_default = default.as_ref().and_then(|c| self.resolve_bound_const(c)).map(Arc::new);
                    let helper = const_default.is_some().then(|| {
                        let helper_name = self.sym(&format!("{}.{}", self.name(self.types[id as usize].name), f.name));
                        self.add_fn(FnInfo {
                            name: helper_name,
                            module: self.cur_mod,
                            generics: generics.clone(),
                            outer: generics.len(),
                            params: vec![],
                            ret: ty,
                            has_self: false,
                            kind: FnKind::DefaultHelper,
                            doc: None,
                            span,
                            is_pub: false,
                            owner: None,
                            self_ty: Some(self_ty),
                            decl: Some(FnDeclRef::BoundDefault(i as u32)),
                            dicts: vec![],
                            impl_of: None,
                            trait_of: None,
                        })
                    });
                    fields.push(FieldDef { name, ty, mutable: f.var, is_pub: f.public, default: helper, const_default, doc });
                }
                self.types[id as usize].kind = TypeDefKind::Struct { fields };
            }
            BoundMembers::Enum(rust) => {
                for (i, v) in rust.iter().enumerate() {
                    let owner = format!("the Rust payload of variant `{}` of `{qual}`", v.name);
                    let tys: Vec<TypeId> =
                        v.fields.iter().map(|f| self.host_type(&(f.ty)(), &generics, &owner, span).unwrap_or(TypeId::ERROR)).collect();
                    if public {
                        self.check_public_types("variant", &ast::Ident { name: v.name.to_string(), span }, tys.iter().copied());
                    }
                    if let TypeDefKind::Enum { variants } = &mut self.types[id as usize].kind {
                        variants[i].fields = tys;
                    }
                }
            }
        }
    }

    /// Finds a struct reachable from `t` that has a field invisible to the current module.
    fn private_field_in(&self, t: TypeId) -> Option<(TypeDefId, Symbol)> {
        let mut seen = HashSet::new();
        let mut todo = vec![t];
        while let Some(t) = todo.pop() {
            let mut found = None;
            self.arena.any(t, &mut |ty| {
                if let Ty::Named(d, _) = ty
                    && seen.insert(d)
                {
                    if let Some(f) = self.hidden_field(d) {
                        found = Some((d, f.name));
                        return true;
                    }
                    let def = &self.types[d as usize];
                    todo.extend(def.fields().iter().map(|f| f.ty).chain(def.variants().iter().flat_map(|v| v.fields.iter().copied())));
                }
                false
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Resolves a Rust field default `c` into a [`ConstVal`].
    fn resolve_bound_const(&self, c: &Constant) -> Option<ConstVal> {
        let variant = |v: &RustVariant| match v {
            RustVariant::None => Some(self.none_rt()),
            RustVariant::Some => Some(self.some_rt()),
            RustVariant::Bound { rust, index } => {
                let first = self.enum_bindings.iter_from(0).find(|(t, _)| t == rust).map(|&(_, first)| first)?;
                Some(first + *index as u32)
            }
        };
        let def = |rust: &std::any::TypeId| self.struct_bindings.iter_from(0).find(|(t, _)| t == rust).map(|&(_, d)| d);
        c.0.resolve(&variant, &def)
    }

    /// Evaluates constant field defaults for structs declared in the current module.
    fn fold_field_defaults(&mut self, first_fn: usize) {
        let mut state = ConstFoldState::default();
        for f in first_fn..self.fns.len() {
            if let Some(FnDeclRef::FieldDefault { field, .. }) = self.fns[f].decl
                && let Some(Ty::Named(d, _)) = self.fns[f].self_ty.map(|t| self.arena.get(t))
            {
                let c = self.fold_default_helper(f as FnId, &mut state).map(Arc::new);
                if let TypeDefKind::Struct { fields } = &mut self.types[d as usize].kind {
                    fields[field as usize].const_default = c;
                }
            }
        }
    }

    /// Evaluates the field default helper `f` as a constant, if possible.
    fn fold_default_helper(&self, f: FnId, state: &mut ConstFoldState) -> Option<ConstVal> {
        let fi = &self.fns[f as usize];
        if !matches!(fi.kind, FnKind::DefaultHelper) {
            return None;
        }
        let Ty::Named(d, _) = self.arena.get(fi.self_ty?) else { return None };
        let field = self.types[d as usize].fields().iter().find(|x| x.default == Some(f))?;
        if let Some(c) = field.const_default.as_deref() {
            return self.charge_const(c).then(|| c.clone());
        }
        if fi.module != self.cur_mod {
            return None;
        }
        if let Some(c) = state.cache.get(&f) {
            return c.as_ref().filter(|c| self.charge_const(c)).cloned();
        }
        if state.depth == MAX_CONST_DEPTH {
            state.depth_exceeded = true;
            return None;
        }
        // Insert `None` before recursing to reject cyclic field defaults.
        state.cache.insert(f, None);
        let outer_exceeded = std::mem::replace(&mut state.depth_exceeded, false);
        state.depth += 1;
        let c = self.bodies.get(f).and_then(|b| self.fold_const_expr(&b.body, &mut Vec::new(), state)).filter(|c| self.charge_const(c));
        state.depth -= 1;
        // Do not cache `None` if a nested helper hit `MAX_CONST_DEPTH`, because
        // `f` may still fold within the depth limit when evaluated directly.
        match state.depth_exceeded {
            true => state.cache.remove(&f),
            false => state.cache.insert(f, c.clone()),
        };
        state.depth_exceeded |= outer_exceeded;
        c
    }

    /// Charges the compile budget for `c` and returns `false` if the budget is
    /// exhausted or `c` exceeds [`ConstVal::measure`] limits.
    fn charge_const(&self, c: &ConstVal) -> bool {
        c.measure().is_some_and(|(parts, bytes)| self.budget.charge(parts as u64 + bytes as u64 / budget::TEXT_BYTES_PER_UNIT))
    }

    /// Evaluates `h` as a compile-time constant, if possible.
    fn fold_const_expr(&self, h: &HExpr, env: &mut Vec<(LocalId, ConstVal)>, state: &mut ConstFoldState) -> Option<ConstVal> {
        if !self.budget.charge(1) {
            return None;
        }
        let mut all = |xs: &[HExpr]| xs.iter().map(|x| self.fold_const_expr(x, env, state)).collect::<Option<Vec<_>>>();
        Some(match &h.kind {
            HKind::Unit => ConstVal::Unit,
            HKind::Bool(b) => ConstVal::Bool(*b),
            HKind::Int(i) => ConstVal::Int(*i),
            HKind::Float(f) => ConstVal::Float(*f),
            HKind::Dur(d) => ConstVal::Dur(*d),
            HKind::Str(s) => ConstVal::Str(s.clone()),
            HKind::Bytes(b) => ConstVal::Bytes(b.clone()),
            HKind::Tuple(xs) => ConstVal::Tuple(all(xs)?),
            HKind::List(xs) => ConstVal::List(all(xs)?),
            HKind::Tag(id) => ConstVal::Variant(*id, Vec::new()),
            HKind::MakeVariant { rt_id, args } => ConstVal::Variant(*rt_id, all(args)?),
            HKind::MakeStruct { def, fields } => ConstVal::Struct(*def, all(fields)?),
            HKind::Map(kvs) => ConstVal::Map(
                kvs.iter()
                    .map(|(k, v)| Some((self.fold_const_expr(k, env, state)?, self.fold_const_expr(v, env, state)?)))
                    .collect::<Option<_>>()?,
            ),
            HKind::CallNative { native, args, .. } if Some(*native) == self.set_init() => match args.as_slice() {
                [HArg::Expr(xs)] => match self.fold_const_expr(xs, env, state)? {
                    ConstVal::List(xs) => ConstVal::Set(xs),
                    _ => return None,
                },
                _ => return None,
            },
            HKind::Range { start, end, inclusive } => {
                match (self.fold_const_expr(start, env, state)?, self.fold_const_expr(end, env, state)?) {
                    (ConstVal::Int(a), ConstVal::Int(b)) => ConstVal::Range(a, b, *inclusive),
                    _ => return None,
                }
            }
            HKind::Unary { op: HUnOp::Neg, e } => match self.fold_const_expr(e, env, state)? {
                ConstVal::Int(i) => ConstVal::Int(i.checked_neg()?),
                ConstVal::Float(f) => ConstVal::Float(-f),
                ConstVal::Dur(d) => ConstVal::Dur(d.checked_neg()?),
                _ => return None,
            },
            HKind::Local(l) => {
                let c = &env.iter().rev().find(|(x, _)| x == l)?.1;
                if !self.charge_const(c) {
                    return None;
                }
                c.clone()
            }
            HKind::Field { obj, index } => match self.fold_const_expr(obj, env, state)? {
                ConstVal::Struct(_, mut fields) if (*index as usize) < fields.len() => fields.swap_remove(*index as usize),
                _ => return None,
            },
            HKind::Call { func, args, .. } if args.is_empty() => self.fold_default_helper(*func, state)?,
            HKind::Block(stmts) => {
                let (last, lets) = stmts.split_last()?;
                let mark = env.len();
                for s in lets {
                    let HKind::Let { pat: HPat { kind: HPatKind::Bind(l), .. }, init, else_: None } = &s.kind else {
                        env.truncate(mark);
                        return None;
                    };
                    let Some(c) = self.fold_const_expr(init, env, state) else {
                        env.truncate(mark);
                        return None;
                    };
                    env.push((*l, c));
                }
                let c = self.fold_const_expr(last, env, state);
                env.truncate(mark);
                c?
            }
            _ => return None,
        })
    }

    fn rust_type(&self, d: TypeDefId) -> Option<std::any::TypeId> {
        rust_type(&self.types[d as usize], &self.enum_bindings, &self.struct_bindings, d)
    }

    /// The id of the prelude's native function `name`.
    pub(crate) fn prelude_native(&self, name: &str) -> Option<u32> {
        let p = self.prelude?;
        let Def::Fn(f) = self.mods[p as usize].scope.get(&self.syms.get(name)?)?.def else { return None };
        match self.fns[f as usize].kind {
            FnKind::Native(n) => Some(n),
            _ => None,
        }
    }

    /// The native `init` of `Set`, which builds a set from a list.
    pub(crate) fn set_init(&self) -> Option<u32> {
        match self.fns[self.init_method(Ctor::Set)? as usize].kind {
            FnKind::Native(n) => Some(n),
            _ => None,
        }
    }

    /// Declares registered host functions that do not have a `native fn` declaration in Sand source.
    fn declare_undeclared_natives(&mut self, first_fn: usize) {
        let span = Span::new(self.mods[self.cur_mod as usize].file, 0, 0);
        let declared: HashSet<u32> =
            self.fns.iter_from(first_fn).filter_map(|f| if let FnKind::Native(n) = f.kind { Some(n) } else { None }).collect();
        let module = self.name(self.mods[self.cur_mod as usize].name).to_string();
        let natives = self.natives.clone();
        let missing: Vec<(&str, &NativeDecl)> = natives
            .fns
            .iter()
            .filter_map(|d| Some((d.key.strip_prefix(module.as_str())?.strip_prefix('.')?, d)))
            .filter(|(_, d)| !declared.contains(&d.id))
            .collect();
        let host = self.mods[self.cur_mod as usize].kind == ModKind::Host;
        for (name, decl) in missing {
            if host {
                self.declare_derived_native(name, decl, span);
            } else {
                self.error(span, format!("native `{module}.{name}` is registered, but the module does not declare it with `native fn`"));
            }
        }
    }

    fn declare_derived_native(&mut self, name: &str, decl: &NativeDecl, span: Span) {
        let (mi, sig) = (self.cur_mod, &decl.sig);
        let module = self.name(self.mods[mi as usize].name).to_string();
        let key = format!("{module}.{name}");
        let (owner, fn_name) = match name.split_once('.') {
            Some((ty, method)) => match self.scope_def(mi, ty) {
                Some(Def::Type(TyRef::Def(d))) if self.types[d as usize].module == mi => {
                    if !self.types[d as usize].generics.is_empty() {
                        let msg = format!(
                            "native `{key}` is a method of the generic type `{ty}`, so the module must declare it with `native fn`"
                        );
                        self.error(span, msg);
                        return;
                    }
                    (Some(d), method)
                }
                _ => {
                    self.error(span, format!("native `{key}` is a method of `{ty}`, which is not a type of module `{module}`"));
                    return;
                }
            },
            None => (None, name),
        };
        let taken = match owner {
            Some(d) => self.methods_named(Ctor::Def(d), fn_name).and_then(|fs| fs.first().map(|&f| self.fns[f as usize].span)),
            None => self.syms.get(name).and_then(|s| self.mods[mi as usize].scope.get(&s)).map(|b| b.span),
        };
        if let Some(at) = taken {
            let msg =
                format!("`{fn_name}` is also the name of the registered native `{key}`; declare the native with `native fn {fn_name}`");
            self.error(at, msg);
            return;
        }
        let mut generic = false;
        for t in sig.params.iter().chain([&sig.ret]) {
            t.visit(&mut |t| generic |= matches!(t, Type::Param(..) | Type::Reflect(..) | Type::Num(_)));
        }
        if generic {
            let msg = format!("native `{key}` is generic, so the module must declare it with `native fn`");
            self.error(span, msg);
            return;
        }
        let has_self = sig.takes_self(&module, name);
        let host_owner = format!("the Rust function of native `{key}`");
        let mut params = Vec::new();
        for (i, t) in sig.params.iter().enumerate() {
            let ty = self.host_type(t, &[], &host_owner, span).unwrap_or(TypeId::ERROR);
            let name = match (i, has_self, &decl.names) {
                (0, true, _) => self.sym("self"),
                (_, _, Some(names)) => self.sym(&names[i - usize::from(has_self)]),
                (_, _, None) => self.sym(&format!("arg{}", i + 1 - usize::from(has_self))),
            };
            params.push(ParamSig { name, ty, default: None });
        }
        let ret = self.host_type(&sig.ret, &[], &host_owner, span).unwrap_or(TypeId::ERROR);
        let ident = ast::Ident { name: fn_name.to_string(), span };
        if owner.is_none_or(|d| self.types[d as usize].is_pub) {
            self.check_public_types("function", &ident, params.iter().map(|p| p.ty).chain([ret]));
        }
        let (name, self_ty) = (self.sym(fn_name), owner.map(|d| self.arena.named(d, &[])));
        let f = self.add_fn(FnInfo {
            name,
            module: mi,
            generics: vec![],
            outer: 0,
            params,
            ret,
            has_self,
            kind: FnKind::Native(decl.id),
            doc: decl.doc.clone(),
            span,
            is_pub: true,
            owner: owner.map(Ctor::Def),
            self_ty,
            decl: None,
            dicts: vec![],
            impl_of: None,
            trait_of: None,
        });
        match owner {
            Some(d) => self.register_method(Ctor::Def(d), &ident, f),
            None => self.define_ident(&ident, Def::Fn(f), true),
        }
    }

    /// Reports an error if a public item in a native module exposes a private type.
    fn check_public_types(&mut self, what: &str, item: &ast::Ident, tys: impl IntoIterator<Item = TypeId>) {
        let mi = self.cur_mod;
        let mut private = None;
        for t in tys {
            self.arena.any(t, &mut |ty| match ty {
                Ty::Named(d, _) if self.types[d as usize].module == mi && !self.types[d as usize].is_pub => {
                    private = Some(d);
                    true
                }
                _ => false,
            });
        }
        if let Some(d) = private {
            let ty = self.name(self.types[d as usize].name).to_string();
            self.error(item.span, format!("public {what} `{}` uses the private type `{ty}`; make `{ty}` public", item.name));
        }
    }

    /// Resolves a host [`Type`] from `owner` into a [`TypeId`].
    fn host_type(&mut self, t: &Type, generics: &[ParamId], owner: &str, span: Span) -> Option<TypeId> {
        Some(match t {
            Type::Int => TypeId::INT,
            Type::Float => TypeId::FLOAT,
            Type::Bool => TypeId::BOOL,
            Type::Str => TypeId::STR,
            Type::Bytes => TypeId::BYTES,
            Type::Duration => TypeId::DURATION,
            Type::Unit => TypeId::UNIT,
            Type::Never => TypeId::NEVER,
            Type::Range => TypeId::RANGE,
            Type::List(e) => {
                let e = self.host_type(e, generics, owner, span)?;
                self.arena.list_of(e)
            }
            Type::Set(e) => {
                let e = self.host_type(e, generics, owner, span)?;
                self.arena.set_of(e)
            }
            Type::Map(k, v) => {
                let (k, v) = (self.host_type(k, generics, owner, span)?, self.host_type(v, generics, owner, span)?);
                self.arena.map_of(k, v)
            }
            Type::Tuple(ts) => {
                let ts = ts.iter().map(|t| self.host_type(t, generics, owner, span)).collect::<Option<Vec<_>>>()?;
                self.arena.tuple(&ts)
            }
            Type::Fn(ps, r) => {
                let ps = ps.iter().map(|t| self.host_type(t, generics, owner, span)).collect::<Option<Vec<_>>>()?;
                let r = self.host_type(r, generics, owner, span)?;
                self.arena.func(&ps, r)
            }
            Type::Param(i, _) => match generics.get(*i as usize) {
                Some(&p) => self.param_ty(p),
                None => {
                    self.error(span, format!("{owner} uses type parameter {i}, which its declaration does not have"));
                    return None;
                }
            },
            Type::Reflect(..) => {
                self.error(span, format!("{owner} takes a type description that is not one of its first parameters"));
                return None;
            }
            Type::Num(_) => {
                self.error(span, format!("{owner} takes `Num` evidence that is not one of its first parameters"));
                return None;
            }
            Type::Named(n) => {
                let module = match n.module {
                    "prelude" => self.prelude,
                    m => self.module_named(m),
                };
                let def = match module.and_then(|m| self.scope_def(m, n.name)) {
                    Some(Def::Type(TyRef::Def(d))) if Some(self.types[d as usize].module) == module => d,
                    _ => {
                        let (m, t) = (n.module, n.name);
                        let msg = match module {
                            Some(_) => format!("{owner} uses `{m}.{t}`, which module `{m}` does not declare"),
                            None => {
                                format!("{owner} uses `{m}.{t}`, and module `{m}` is not added to the engine")
                            }
                        };
                        self.error(span, msg);
                        return None;
                    }
                };
                if module != Some(self.cur_mod) && !self.types[def as usize].is_pub {
                    self.error(span, format!("{owner} uses `{}.{}`, which is private", n.module, n.name));
                    return None;
                }
                if n.rust.is_some() && n.rust != self.rust_type(def) {
                    let msg = format!(
                        "{owner} uses a Rust type that is not the one bound to `{}.{}`; register it with its module",
                        n.module, n.name
                    );
                    self.error(span, msg);
                    return None;
                }
                let args = n.args.iter().map(|t| self.host_type(t, generics, owner, span)).collect::<Option<Vec<_>>>()?;
                if args.len() != self.types[def as usize].generics.len() {
                    self.error(span, format!("{owner} gives `{}.{}` the wrong number of type arguments", n.module, n.name));
                    return None;
                }
                self.arena.named(def, &args)
            }
        })
    }

    /// Verifies that a `native fn` declaration matches its registered Rust
    /// signature and declares each built-in trait that the Rust function
    /// requires. Returns
    /// the type parameter indices for which the Rust function accepts `Num`
    /// evidence.
    fn check_native_signature(
        &mut self,
        f: &ast::FnDecl,
        key: &str,
        sig: &Signature,
        generics: &[ParamId],
        params: &[ParamSig],
        ret: TypeId,
    ) -> Vec<u8> {
        let span = f.name.span;
        let owner = format!("the Rust function of native `{key}`");
        let reflect: Vec<Type> =
            generics.iter().enumerate().filter(|(_, p)| self.params[**p as usize].reflect).map(|(i, _)| Type::Reflect(i as u8)).collect();
        let num_trait = self.builtins[Builtin::Num as usize];
        let num: Vec<u8> =
            (0..generics.len() as u8).filter(|&i| num_trait.is_some_and(|n| self.param_has_trait(generics[i as usize], n))).collect();
        let lead_of = |t: &Type| match t {
            Type::Reflect(i) => Some((false, *i)),
            Type::Num(i) => Some((true, *i)),
            _ => None,
        };
        let n = reflect.len().min(sig.params.len());
        let (lead, rest) = sig.params.split_at(n);
        let k = rest.iter().take_while(|t| matches!(t, Type::Num(_))).count();
        let (evidence, rest) = rest.split_at(k);
        let taken: Vec<u8> = evidence.iter().filter_map(|t| if let Type::Num(i) = t { Some(*i) } else { None }).collect();
        let mut wanted = num.iter();
        let leads_match = lead.iter().map(lead_of).eq(reflect.iter().map(lead_of))
            && taken.iter().all(|i| wanted.any(|j| j == i))
            && !rest.iter().any(|t| lead_of(t).is_some());
        if leads_match {
            let mark = self.diags.len();
            let want: Option<Vec<TypeId>> = rest.iter().map(|t| self.host_type(t, generics, &owner, span)).collect();
            let want_ret = self.host_type(&sig.ret, generics, &owner, span);
            if self.diags.len() > mark {
                return taken;
            }
            let have: Vec<TypeId> = params.iter().map(|p| p.ty).collect();
            if want.as_ref() == Some(&have) && want_ret == Some(ret) {
                self.check_native_traits(f, sig, generics);
                return taken;
            }
        }
        let leads: Vec<Type> = reflect.into_iter().chain(num.iter().map(|&i| Type::Num(i))).collect();
        let module = self.name(self.mods[self.cur_mod as usize].name).to_string();
        let names: Vec<String> = generics.iter().map(|&p| self.name(self.params[p as usize].name).to_string()).collect();
        let show_rust =
            |t: &Type| t.render_with(&module, &|i| names.get(i as usize).cloned().unwrap_or_else(|| format!("<type parameter {i}>")));
        let mut declared: Vec<String> = leads.iter().map(show_rust).collect();
        declared.extend(params.iter().map(|p| self.show(p.ty)));
        let rust: Vec<String> = sig.params.iter().map(show_rust).collect();
        let msg = format!(
            "native function `{}` is declared `fn({}) -> {}`, but its Rust function is `fn({}) -> {}`",
            f.name.name,
            declared.join(", "),
            self.show(ret),
            rust.join(", "),
            show_rust(&sig.ret)
        );
        self.error(span, msg);
        Vec::new()
    }

    fn check_native_traits(&mut self, f: &ast::FnDecl, sig: &Signature, generics: &[ParamId]) {
        let mut required = vec![Builtins::NONE; generics.len()];
        for t in sig.params.iter().chain([&sig.ret]) {
            t.visit(&mut |t| {
                if let Type::Param(i, b) = t
                    && let Some(r) = required.get_mut(*i as usize)
                {
                    *r = r.union(*b);
                }
            });
        }
        for (&p, need) in generics.iter().zip(required) {
            let need: Vec<TraitRef> = need.iter().filter_map(|b| self.builtin_trait(b)).collect();
            let missing: Vec<TraitRef> = need.iter().copied().filter(|tr| !self.param_has_trait(p, tr.trait_id)).collect();
            if !missing.is_empty() {
                let show =
                    |c: &Checker, trs: &[TraitRef]| c.minimal_traits(trs).iter().map(|tr| c.show_trait(tr)).collect::<Vec<_>>().join(" + ");
                let name = self.name(self.params[p as usize].name).to_string();
                let msg = format!(
                    "native function `{}` must declare `{name}: {}`, as its Rust function requires `{name}: {}`",
                    f.name.name,
                    show(self, &missing),
                    show(self, &need)
                );
                self.error(f.name.span, msg);
            }
        }
    }

    fn resolve_alias(&mut self, i: usize, span: Span) -> TypeId {
        if let Some(t) = self.aliases[i].ty {
            return t;
        }
        if self.aliases[i].resolving {
            self.error(span, format!("type alias `{}` is recursive", self.name(self.aliases[i].name)));
            return TypeId::ERROR;
        }
        self.aliases[i].resolving = true;
        let (m, te) = self.aliases[i].decl.clone().unwrap();
        let saved = self.cur_mod;
        self.cur_mod = m;
        let gens = self.aliases[i].generics.clone();
        let scope = self.generic_scope(&gens);
        let mut t = self.resolve_type_in(&te, &scope, None);
        self.cur_mod = saved;
        self.limit_type_size(&mut t, te.span);
        let alias = &mut self.aliases[i];
        alias.resolving = false;
        alias.ty = Some(t);
        alias.decl = None;
        t
    }

    /// Resolves `name` in the current module scope, the prelude, built-in types,
    /// and (inside a `test` block) the `test` module.
    pub fn lookup_global(&mut self, name: &str) -> Option<Def> {
        let sym = self.syms.get(name);
        let cur_mod = self.cur_mod as usize;
        if let Some(sym) = sym
            && let Some(binding) = self.mods[cur_mod].scope.get(&sym)
        {
            let def = binding.def;
            self.note_name_used(name);
            return Some(def);
        }
        if name == "task"
            && let Some(task_mod) = self.task_mod
            && task_mod != self.cur_mod
        {
            return Some(Def::Module(task_mod));
        }
        if name == "test"
            && matches!(self.fs.ctx.first().map(|c| c.kind), Some(CtxKind::Test))
            && let Some(test_mod) = self.module_named("test")
        {
            return Some(Def::Module(test_mod));
        }
        if let (Some(sym), Some(prelude)) = (sym, self.prelude)
            && prelude != self.cur_mod
            && let Some(Binding { def, is_pub: true, .. }) = self.mods[prelude as usize].scope.get(&sym)
        {
            return Some(*def);
        }
        if let Some(ty) = builtin_type(name) {
            return Some(Def::Type(ty));
        }
        if let Some(sym) = sym
            && matches!(self.fs.ctx.first().map(|c| c.kind), Some(CtxKind::Test))
            && let Some(test_mod) = self.module_named("test")
            && let Some(Binding { def, is_pub: true, .. }) = self.mods[test_mod as usize].scope.get(&sym)
        {
            return Some(*def);
        }
        None
    }

    /// Looks up a public member of a module.
    pub fn module_member(&mut self, m: ModId, name: &str, span: Span) -> Option<Def> {
        let found = self.syms.get(name).and_then(|s| self.mods[m as usize].scope.get(&s).copied());
        match found {
            Some(Binding { def, is_pub: true, .. }) => Some(def),
            Some(_) => {
                self.error(span, format!("`{name}` is private"));
                None
            }
            None => {
                let mn = self.name(self.mods[m as usize].name).to_string();
                self.error(span, format!("module `{mn}` has no public item `{name}`"));
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // Types from syntax

    pub fn resolve_type_in(&mut self, te: &TypeExpr, generics: &[(Symbol, ParamId)], self_ty: Option<TypeId>) -> TypeId {
        match &te.kind {
            TypeExprKind::Unit => TypeId::UNIT,
            TypeExprKind::List(t) => {
                let e = self.resolve_type_in(t, generics, self_ty);
                self.list_ty(e)
            }
            TypeExprKind::Map(k, v) => {
                let k = self.resolve_type_in(k, generics, self_ty);
                let v = self.resolve_type_in(v, generics, self_ty);
                self.map_ty(k, v)
            }
            TypeExprKind::Tuple(ts) => {
                let ts: Vec<TypeId> = ts.iter().map(|t| self.resolve_type_in(t, generics, self_ty)).collect();
                self.tuple_ty(&ts)
            }
            TypeExprKind::Option(t) => {
                let inner = self.resolve_type_in(t, generics, self_ty);
                self.option_of(inner)
            }
            TypeExprKind::Fn(ps, r) => {
                let ps: Vec<TypeId> = ps.iter().map(|t| self.resolve_type_in(t, generics, self_ty)).collect();
                let r = r.as_ref().map(|r| self.resolve_type_in(r, generics, self_ty)).unwrap_or(TypeId::UNIT);
                self.fn_ty(&ps, r)
            }
            TypeExprKind::Named { path, args } => {
                let args: Vec<TypeId> = args.iter().map(|t| self.resolve_type_in(t, generics, self_ty)).collect();
                let first = &path[0];
                if path.len() == 1 {
                    if first.name == "Self" {
                        return match self_ty {
                            Some(t) => t,
                            None => {
                                self.error(first.span, "`Self` is only valid inside a type's methods");
                                TypeId::ERROR
                            }
                        };
                    }
                    if first.name == "never" && args.is_empty() {
                        return TypeId::NEVER;
                    }
                    let sym = self.syms.get(&first.name);
                    if let Some((_, p)) = generics.iter().rev().find(|(n, _)| Some(*n) == sym) {
                        if !args.is_empty() {
                            self.error(te.span, "type parameters do not take type arguments");
                        }
                        return self.param_ty(*p);
                    }
                    // A built-in type name shadowed by a value (e.g. `random.int`)
                    // still names the built-in type in type position.
                    let def = match self.lookup_global(&first.name) {
                        Some(Def::Type(r)) => Some(r),
                        Some(Def::Trait(_)) => {
                            self.trait_as_type_error(first);
                            return TypeId::ERROR;
                        }
                        _ => builtin_type(&first.name),
                    };
                    match def {
                        Some(r) => self.instantiate_tyref(r, args, te.span, &first.name),
                        None => {
                            self.error(first.span, format!("unknown type `{}`", first.name));
                            TypeId::ERROR
                        }
                    }
                } else if path.len() == 2 {
                    match self.lookup_global(&first.name) {
                        Some(Def::Module(m)) => match self.module_member(m, &path[1].name, path[1].span) {
                            Some(Def::Type(r)) => self.instantiate_tyref(r, args, te.span, &path[1].name),
                            Some(Def::Trait(_)) => {
                                self.trait_as_type_error(&path[1]);
                                TypeId::ERROR
                            }
                            Some(_) => {
                                self.error(path[1].span, format!("`{}` is not a type", path[1].name));
                                TypeId::ERROR
                            }
                            None => TypeId::ERROR,
                        },
                        _ => {
                            self.error(
                                first.span,
                                format!("unknown type `{}`", path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".")),
                            );
                            TypeId::ERROR
                        }
                    }
                } else {
                    self.error(te.span, "unknown type");
                    TypeId::ERROR
                }
            }
        }
    }

    pub(crate) fn coll_ty(&mut self, c: Ctor, args: &[TypeId]) -> TypeId {
        match c {
            Ctor::List => self.arena.list_of(args[0]),
            Ctor::Map => self.arena.map_of(args[0], args[1]),
            _ => self.arena.set_of(args[0]),
        }
    }

    fn instantiate_tyref(&mut self, r: TyRef, mut args: Vec<TypeId>, span: Span, name: &str) -> TypeId {
        match r {
            TyRef::Prim(t) => {
                if !args.is_empty() {
                    self.error(span, format!("`{name}` does not take type arguments"));
                }
                t
            }
            TyRef::Coll(c) => {
                let n = coll_arity(c);
                if args.len() != n {
                    let plural = if n == 1 { "" } else { "s" };
                    self.error(span, format!("expected {n} type argument{plural} for `{name}`, found {}", args.len()));
                    return TypeId::ERROR;
                }
                self.coll_ty(c, &args)
            }
            TyRef::Def(d) => {
                let n = self.types[d as usize].generics.len();
                if d == self.wk.result && args.len() == 1 && self.prelude.is_some() {
                    args.push(self.error_ty());
                }
                if args.len() != n {
                    let plural = if n == 1 { "" } else { "s" };
                    self.error(span, format!("expected {n} type argument{plural} for `{name}`, found {}", args.len()));
                    return TypeId::ERROR;
                }
                self.named_ty(d, &args)
            }
            TyRef::Alias(i) => {
                let body = self.resolve_alias(i, span);
                let gens = self.aliases[i].generics.clone();
                if args.len() != gens.len() {
                    self.error(span, format!("expected {} type arguments for `{name}`, found {}", gens.len(), args.len()));
                    return TypeId::ERROR;
                }
                self.arena.instantiate(body, &gens, &args)
            }
        }
    }

    // ------------------------------------------------------------------
    // Function declarations

    /// Declares function `f`, found at `at`. `outer` holds enclosing generic
    /// params (for methods), `self_ty` the receiver type.
    pub(crate) fn declare_fn(
        &mut self,
        f: &ast::FnDecl,
        at: DeclPath,
        rules: DeclRules,
        owner: Option<Ctor>,
        outer: &[(Symbol, ParamId)],
        self_ty: Option<TypeId>,
    ) -> FnId {
        if f.is_native && !self.allows_native() {
            self.error(f.name.span, "native functions can only be declared by native modules");
        }
        let mut scope: Vec<(Symbol, ParamId)> = outer.to_vec();
        let mut generics: Vec<ParamId> = outer.iter().map(|(_, p)| *p).collect();
        for p in self.declare_params(&f.generics, outer, self_ty) {
            scope.push((self.params[p as usize].name, p));
            generics.push(p);
        }
        let preds =
            f.where_clause.iter().filter(|w| !rules.skip_where.iter().any(|(n, _)| self.syms.str(*n) == w.name.name)).collect::<Vec<_>>();
        self.apply_where(preds, &scope, self_ty);
        let mut params = Vec::new();
        self.check_param_names(f.params.iter().map(|p| &p.name));
        for (i, p) in f.params.iter().enumerate() {
            let ty = match &p.ty {
                Some(t) => self.resolve_type_in(t, &scope, self_ty),
                None => match self_ty {
                    Some(t) if i == 0 => t,
                    _ => {
                        self.error(p.name.span, "`self` is only allowed as the first parameter of a method");
                        TypeId::ERROR
                    }
                },
            };
            let name = self.sym(&p.name.name);
            params.push(ParamSig { name, ty, default: None });
        }
        let ret = match &f.ret {
            Some(t) => self.resolve_type_in(t, &scope, self_ty),
            None => TypeId::UNIT,
        };
        let public = f.is_pub && !matches!(owner, Some(Ctor::Def(d)) if !self.types[d as usize].is_pub);
        if public && self.allows_native() {
            self.check_public_types("function", &f.name, params.iter().map(|p| p.ty).chain([ret]));
        }
        let intrinsic = match f.name.name.as_str() {
            "help" => Some(Intrinsic::Help),
            "members" => Some(Intrinsic::Members),
            _ => None,
        };
        let mut num_evidence = Vec::new();
        let kind = if let Some(i) = intrinsic.filter(|_| f.is_native && owner.is_none() && Some(self.cur_mod) == self.prelude) {
            FnKind::Intrinsic(i)
        } else if f.is_native {
            let module_name = self.name(self.mods[self.cur_mod as usize].name).to_string();
            let key = match owner.map(|c| c.native_prefix(&self.types, &self.syms)) {
                Some(t) => format!("{module_name}.{t}.{}", f.name.name),
                None => format!("{module_name}.{}", f.name.name),
            };
            match self.natives.get(&key).cloned() {
                Some(decl) => {
                    num_evidence = self.check_native_signature(f, &key, &decl.sig, &generics, &params, ret);
                    if decl.names.is_some() || decl.doc.is_some() {
                        let msg =
                            format!("native `{key}` has `params` or `doc`, which apply only to a native that the module does not declare");
                        self.error(f.name.span, msg);
                    }
                    FnKind::Native(decl.id)
                }
                None => {
                    self.error(f.name.span, format!("no native implementation registered for `{key}`"));
                    FnKind::Native(u32::MAX)
                }
            }
        } else {
            FnKind::Script
        };
        let mut dicts: Vec<(TypeId, TraitRef)> = Vec::new();
        for &p in &generics {
            let pt = self.param_ty(p);
            dicts.extend(self.params[p as usize].traits.iter().filter(|tr| self.has_dict(tr)).map(|&tr| (pt, tr)));
        }
        if f.is_native {
            if generics.iter().any(|&p| self.params[p as usize].traits.iter().any(|tr| self.traits[tr.trait_id as usize].builtin.is_none()))
            {
                self.error(f.name.span, "native functions can only require built-in traits");
            }
            dicts.clear();
            if let Some(num) = self.builtin_trait(Builtin::Num) {
                dicts = num_evidence.iter().map(|&i| (self.param_ty(generics[i as usize]), num)).collect();
            }
        }
        let name = self.sym(&f.name.name);
        let id = self.add_fn(FnInfo {
            name,
            module: self.cur_mod,
            generics: generics.clone(),
            outer: outer.len(),
            params,
            ret,
            has_self: f.has_self() && self_ty.is_some(),
            kind,
            doc: f.doc.as_deref().map(Arc::from),
            span: f.name.span,
            is_pub: f.is_pub,
            owner,
            self_ty,
            decl: if f.is_native { None } else { Some(FnDeclRef::Fn(at)) },
            dicts: dicts.clone(),
            impl_of: None,
            trait_of: None,
        });
        // Default argument helpers.
        for (i, p) in f.params.iter().enumerate() {
            let kept = |d: &&ast::Expr| match rules.defaults {
                DefaultRule::All => true,
                DefaultRule::Const => is_closed_const(d),
                DefaultRule::None => false,
            };
            if let Some(d) = p.default.as_ref().filter(kept) {
                let default = if is_closed_const(d) {
                    DefaultArg::Inline(Arc::new(d.clone()))
                } else {
                    let helper_params: Vec<ParamSig> = self.fns[id as usize].params[..i].to_vec();
                    let ret = self.fns[id as usize].params[i].ty;
                    let name = self.sym(&format!("{}.{}", f.name.name, p.name.name));
                    let helper = self.add_fn(FnInfo {
                        name,
                        module: self.cur_mod,
                        generics: generics.clone(),
                        outer: generics.len(),
                        params: helper_params,
                        ret,
                        has_self: false,
                        kind: FnKind::DefaultHelper,
                        doc: None,
                        span: d.span,
                        is_pub: false,
                        owner: None,
                        self_ty,
                        decl: Some(FnDeclRef::ParamDefault(at, i as u32)),
                        dicts: dicts.clone(),
                        impl_of: None,
                        trait_of: None,
                    });
                    DefaultArg::Helper(helper)
                };
                self.fns[id as usize].params[i].default = Some(default);
            }
        }
        id
    }

    fn declare_method(&mut self, m: &ast::FnDecl, at: DeclPath, ctor: Ctor, def: TypeDefId) -> FnId {
        // Clone the type's parameters per method so that method `where` clauses
        // do not mutate the type's parameter bounds.
        let orig = self.types[def as usize].generics.clone();
        let mut outer = Vec::new();
        for p in &orig {
            let name = self.params[*p as usize].name;
            outer.push((name, self.new_param(name, false)));
        }
        let ps: Vec<ParamId> = outer.iter().map(|(_, p)| *p).collect();
        let self_ty = self.def_self_ty(def, &ps);
        self.add_type_traits(self_ty, &ps);
        let id = self.declare_fn(m, at, DeclRules::PLAIN, Some(ctor), &outer, Some(self_ty));
        self.register_method(ctor, &m.name, id);
        id
    }

    /// Copies the trait bounds declared on `ty`'s type definition onto the
    /// corresponding type parameter arguments in `ps`.
    fn add_type_traits(&mut self, ty: TypeId, ps: &[ParamId]) {
        let Ty::Named(d, args) = self.ty(ty) else { return };
        let (gens, args) = (self.types[d as usize].generics.clone(), self.elems(args));
        let map = |q: ParamId| gens.iter().position(|&g| g == q).map(|i| args[i]);
        for (&g, &a) in gens.iter().zip(&args) {
            if let Ty::Param(p) = self.ty(a)
                && ps.contains(&p)
            {
                let traits = self.params[g as usize].traits.clone();
                let traits = traits.into_iter().map(|tr| tr.subst_params(&mut self.arena, &map)).collect();
                self.add_traits(p, traits);
            }
        }
    }

    fn register_method(&mut self, ctor: Ctor, name: &ast::Ident, id: FnId) {
        if name.name == "init" {
            self.check_init_decl(ctor, name, id);
        }
        let has_self = self.fns[id as usize].has_self;
        let module = self.fns[id as usize].module;
        let key = (ctor, self.syms.intern(&name.name));
        if self.in_session()
            && let Some(ids) = self.methods.get(&key)
        {
            // A method defined again by a later session input replaces the earlier one.
            let cur_file = self.mods[module as usize].file;
            let kept: Vec<FnId> = ids
                .iter()
                .copied()
                .filter(|&f| {
                    let other = &self.fns[f as usize];
                    !(other.has_self == has_self && other.module == module && other.span.file != cur_file)
                })
                .collect();
            if kept.len() != ids.len() {
                self.methods.insert(key, kept.into());
            }
        }
        let mut ids: Vec<FnId> = self.methods.get(&key).map(|v| v.to_vec()).unwrap_or_default();
        let dup = ids.iter().any(|&f| {
            let other = &self.fns[f as usize];
            other.has_self == has_self && other.module == module
        });
        if dup {
            self.error(name.span, format!("method `{}` is already defined", name.name));
        }
        ids.push(id);
        self.methods.insert(key, ids.into());
    }

    /// Checks that `init` method `id` of `ctor` constructs it: it is static,
    /// returns `Self` or `Result<Self, E>`, constructs every instance of the
    /// type, and comes with the type, not with a later session input.
    fn check_init_decl(&mut self, ctor: Ctor, name: &ast::Ident, id: FnId) {
        let f = &self.fns[id as usize];
        let (self_ty, ret) = (f.self_ty.unwrap_or(TypeId::ERROR), f.ret);
        if let Ctor::Def(d) = ctor
            && self.types[d as usize].is_enum()
        {
            self.error(name.span, "an enum cannot declare `init`; its variants construct it");
        } else if let Ctor::Def(d) = ctor
            && (d as usize) < self.first_type
        {
            // Code checked earlier constructs and decodes the type field by field.
            let ty = self.name(self.types[d as usize].name).to_string();
            self.error(name.span, format!("`init` must be declared in the same input as `{ty}`"));
        } else if f.has_self {
            self.error(name.span, "`init` cannot take `self`; it constructs a new value");
        } else if let Ctor::Def(d) = ctor
            && !self.is_generic_instance(d, self_ty)
        {
            let ty = self.ctor_name(ctor, 0);
            self.error(name.span, format!("`init` must construct every `{ty}`, so its type arguments cannot be fixed or bounded further"));
        } else {
            let fits = ret == self_ty
                || ret == TypeId::ERROR
                || matches!(self.arena.get(ret), Ty::Named(r, args) if r == self.wk.result && self.arena.elem(args, 0) == self_ty);
            if !fits {
                let ty = self.show(self_ty);
                self.error(name.span, format!("`init` must return `{ty}` or `Result<{ty}, E>`"));
            }
        }
    }

    /// Whether `t` is type definition `d` applied to distinct type parameters
    /// that have only the bounds that `d` declares for them.
    fn is_generic_instance(&mut self, d: TypeDefId, t: TypeId) -> bool {
        let Ty::Named(_, args) = self.ty(t) else { return false };
        let (gens, args) = (self.types[d as usize].generics.clone(), self.elems(args));
        let mut seen = Vec::new();
        for (&g, &a) in gens.iter().zip(&args) {
            let Ty::Param(p) = self.ty(a) else { return false };
            if seen.contains(&p) {
                return false;
            }
            seen.push(p);
            let map = |q: ParamId| gens.iter().position(|&x| x == q).map(|i| args[i]);
            let declared: Vec<TraitRef> =
                self.params[g as usize].traits.clone().into_iter().map(|tr| tr.subst_params(&mut self.arena, &map)).collect();
            if self.params[p as usize].traits.iter().any(|tr| !declared.contains(tr)) {
                return false;
            }
        }
        true
    }

    /// Records an `argument-order` lint if positional argument or
    /// sub-pattern `name` (at `span`) is given for parameter or field `names[i]`
    /// of `what` but is named like a different one of `names`.
    pub(crate) fn note_argument_order(&mut self, what: &str, names: &[&str], i: usize, name: &str, span: Span) {
        if !self.linting() || names.get(i) == Some(&name) || !names.contains(&name) {
            return;
        }
        let msg = format!(
            "`{name}` is given for `{}` of {what}, which also has `{name}`; write `{}: {name}` if this is intended",
            names[i], names[i]
        );
        self.lint.argument_order.push((span, msg));
    }

    pub(crate) fn visible(&self, module: ModId, is_pub: bool) -> bool {
        is_pub || module == self.cur_mod
    }

    pub(crate) fn hidden_field(&self, d: TypeDefId) -> Option<&FieldDef> {
        let def = &self.types[d as usize];
        def.fields().iter().find(|f| !self.visible(def.module, f.is_pub))
    }

    /// The methods named `name` of type constructor `c`.
    pub(crate) fn methods_named(&self, c: Ctor, name: &str) -> Option<Arc<[FnId]>> {
        let s = self.syms.get(name)?;
        self.methods.get(&(c, s)).cloned()
    }

    fn declare_extend(&mut self, x: &ast::ExtendDecl, item: u32) {
        // Resolve the target and bounds once so that unknown traits are reported even if the block is empty.
        let probe = self.declare_params(&x.generics, &[], None);
        let probe_scope = self.generic_scope(&probe);
        let target = self.resolve_type_in(&x.target, &probe_scope, None);
        let ctor = match ctor_of(self.ty(target)) {
            Some(c) => c,
            None => {
                if target != TypeId::ERROR {
                    self.error(x.target.span, "cannot extend this type");
                }
                return;
            }
        };
        if let Some(doc) = &x.doc
            && self.ctor_docs.get(&ctor).is_none()
        {
            self.ctor_docs.insert(ctor, Arc::from(doc.as_str()));
        }
        let builtin = !matches!(ctor, Ctor::Def(_));
        let foreign = match ctor {
            Ctor::Def(d) => self.types[d as usize].module != self.cur_mod,
            _ => true,
        };
        if foreign && !self.allows_native() {
            let what = if builtin { "built-in types" } else { "types defined in other modules" };
            self.error(x.target.span, format!("cannot extend {what}"));
            return;
        }
        for (j, m) in x.methods.iter().enumerate() {
            // Fresh parameters per method so that `where` clauses only affect that method.
            let params = self.declare_params(&x.generics, &[], None);
            let scope = self.generic_scope(&params);
            let self_ty = self.resolve_type_in(&x.target, &scope, None);
            self.add_type_traits(self_ty, &params);
            let at = DeclPath { item, method: j as u32 };
            let id = self.declare_fn(m, at, DeclRules::PLAIN, Some(ctor), &scope, Some(self_ty));
            self.register_method(ctor, &m.name, id);
        }
    }

    // ------------------------------------------------------------------
    // Bodies

    fn begin_fn(&mut self, generics: Vec<(Symbol, ParamId)>, self_ty: Option<TypeId>) {
        self.fs = FnState { var_mark: self.subst.len() as VarId, error_mark: self.diags.len(), ..FnState::default() };
        self.fs_unused.clear();
        self.fs.generics = generics;
        self.fs.self_ty = self_ty;
        self.fs.scopes.push();
    }

    /// Checks the body of function `id`, declared at `decl` in module `m`
    /// (the module being checked).
    fn check_fn_body(&mut self, id: FnId, decl: FnDeclRef, m: &ast::Module) {
        let fi = &self.fns[id as usize];
        let generics = self.generic_scope(&fi.generics);
        let (self_ty, ret, name) = (fi.self_ty, fi.ret, self.name(fi.name).to_string());
        // Parameters of trait and impl methods are dictated by the trait;
        // those of default helpers are synthetic.
        let synthetic = fi.impl_of.is_some()
            || fi.trait_of.is_some()
            || matches!(decl, FnDeclRef::ParamDefault(..) | FnDeclRef::FieldDefault { .. } | FnDeclRef::BoundDefault(_));
        self.begin_fn(generics, self_ty);
        self.fs.field_default = matches!(decl, FnDeclRef::FieldDefault { .. });
        let fi = &self.fns[id as usize];
        self.fs.init_of = fi.owner.filter(|_| fi.is_init(&self.syms));
        let mut param_locals = self.declare_reflect_params(id);
        param_locals.extend(self.declare_dict_params(id));
        let f = match decl {
            FnDeclRef::Fn(at) => Some(at.get(m)),
            _ => None,
        };
        for k in 0..self.fns[id as usize].params.len() {
            let p = &self.fns[id as usize].params[k];
            let ty = p.ty;
            let (pname, pspan) = match f {
                Some(f) => (self.sym(&f.params[k].name.name), f.params[k].name.span),
                None => (p.name, self.fns[id as usize].span),
            };
            let l = self.declare_local_sym(pname, ty, false, pspan, true);
            if synthetic {
                self.fs.data[l as usize].used = true;
            }
            param_locals.push(l);
        }
        let kind = if matches!(decl, FnDeclRef::Test { .. }) { CtxKind::Test } else { CtxKind::Function };
        self.fs.ctx.push(FnCtx { ret, kind, loops: vec![], lambda: 0, in_defer: None });
        let (body, span) = match decl {
            FnDeclRef::Fn(at) => {
                let f = at.get(m);
                let b = f.body.as_ref().unwrap();
                let want = ret != TypeId::UNIT;
                let h = self.check_block(b, if want { Some(ret) } else { None }, want);
                if want {
                    self.expect_type(ret, h.ty, self.block_tail_span(b));
                }
                (h, f.span)
            }
            FnDeclRef::ParamDefault(at, i) => {
                let e = at.get(m).params[i as usize].default.as_ref().unwrap();
                (self.check_expr(e, ret), e.span)
            }
            FnDeclRef::FieldDefault { item, field } => {
                let ItemKind::Struct(s) = &m.items[item as usize].kind else { unreachable!() };
                let e = s.fields[field as usize].default.as_ref().unwrap();
                (self.check_expr(e, ret), e.span)
            }
            FnDeclRef::BoundDefault(field) => {
                let span = self.fns[id as usize].span;
                let c = self_ty.and_then(|t| match self.arena.get(t) {
                    Ty::Named(d, _) => self.types[d as usize].fields()[field as usize].const_default.clone(),
                    _ => None,
                });
                let body = match c {
                    Some(c) => self.const_expr(&c, ret, span),
                    None => expr::err(span),
                };
                (body, span)
            }
            FnDeclRef::Test { item } => {
                let ItemKind::Test(t) = &m.items[item as usize].kind else { unreachable!() };
                (self.check_block(&t.body, None, false), t.body.span)
            }
            FnDeclRef::Init => unreachable!(),
        };
        let hfn = self.finish_fn(name, param_locals, body, span);
        self.bodies.set(id, hfn);
    }

    fn block_tail_span(&self, b: &ast::Block) -> Span {
        match b.stmts.last() {
            Some(s) => s.span,
            None => b.span,
        }
    }

    fn check_init(&mut self, id: FnId, pm: &ParsedModule) {
        self.fn_names = names_used_in_functions(&pm.ast);
        self.begin_fn(vec![], None);
        let main = pm.kind == (ModKind::Script { main: true });
        self.fs.ctx.push(FnCtx { ret: TypeId::UNIT, kind: CtxKind::TopLevel { main }, loops: vec![], lambda: 0, in_defer: None });
        let mut stmts = Vec::new();
        // The final expression statement of a session input may be its value.
        let value = match pm.ast.items.last() {
            Some(ast::Item { kind: ItemKind::Stmt(ast::Stmt { kind: ast::StmtKind::Expr(e), .. }), .. })
                if self.want_cell_value && self.in_session() =>
            {
                Some(e)
            }
            _ => None,
        };
        let mut ty = TypeId::UNIT;
        for (i, item) in pm.ast.items.iter().enumerate() {
            if let ItemKind::Stmt(s) = &item.kind {
                self.fs.top_level = true;
                match value {
                    Some(e) if i + 1 == pm.ast.items.len() => {
                        let h = self.infer(e, None);
                        ty = h.ty;
                        stmts.push(h);
                    }
                    _ => stmts.extend(self.check_stmt(s)),
                }
                self.fs.top_level = false;
            }
        }
        let body = HExpr { kind: HKind::Block(stmts), ty, span: pm.ast.span };
        let name = self.name(self.fns[id as usize].name).to_string();
        let hfn = self.finish_fn(name, vec![], body, pm.ast.span);
        if value.is_some() {
            self.cell_value = Some(hfn.body.ty);
        }
        self.bodies.set(id, hfn);
    }

    /// Whether lint facts are gathered for the module being checked: only
    /// those of the main module are used, and only by `sand lint`.
    pub(crate) fn linting(&self) -> bool {
        self.lint_facts && self.mods[self.cur_mod as usize].kind == (ModKind::Script { main: true })
    }

    /// Records that the module being checked uses `name` (for the linter).
    pub(crate) fn note_name_used(&mut self, name: &str) {
        if self.linting() {
            self.lint.used_names.insert((self.cur_mod, name.to_string()));
        }
    }

    /// Lint facts of the function just checked: its unused `Result`s (of
    /// the statements `unused`) and its locals.
    fn lint_fn_facts(&mut self, unused: Vec<(TypeId, Span)>) {
        for (t, sp) in unused {
            if let Ty::Named(d, _) = self.rty(t)
                && d == self.wk.result
            {
                self.lint.unused_results.push(sp);
            }
        }
        self.lint_func += 1;
        for (i, d) in self.fs.data.iter().enumerate() {
            let info = &self.fs.locals[i];
            let name = self.syms.str(info.name);
            if name.starts_with('<') {
                continue;
            }
            self.lint.locals.push(LocalFact {
                span: d.decl_span,
                name: name.to_string(),
                used: d.used,
                mutable: info.mutable,
                assigned: d.assigned,
                ty: self.printer().show(d.ty),
                is_param: d.is_param,
                global: false,
                func: self.lint_func,
                variants: if name.starts_with(|c: char| c.is_ascii_uppercase()) { self.payloadless_variants(d.ty) } else { Vec::new() },
            });
        }
    }

    /// The variants without a payload of `ty`, if it is an enum, qualified
    /// with its name (for the linter).
    fn payloadless_variants(&self, t: TypeId) -> Vec<String> {
        let Ty::Named(d, _) = self.rty(t) else { return Vec::new() };
        let def = &self.types[d as usize];
        match &def.kind {
            TypeDefKind::Enum { variants } => {
                variants.iter().filter(|v| v.fields.is_empty()).map(|v| format!("{}.{}", self.name(def.name), self.name(v.name))).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Final checks for a function: required traits, inference completeness; the
    /// final walk of the body.
    fn finish_fn(&mut self, name: String, params: Vec<LocalId>, mut body: HExpr, span: Span) -> HFn {
        // Trait evidence first: resolving it may determine types.
        let dicts = self.resolve_holes();
        let obligations = std::mem::take(&mut self.fs.obligations);
        if let Some(q) = &mut self.type_query {
            q.traits.extend(obligations.iter().map(|(t, tr, _)| (*t, *tr)));
        }
        for (t, tr, sp) in obligations {
            if let Err(m) = self.resolve_evidence(t, tr, 0)
                && !m.unknown
            {
                let msg = self.unmet_message(m, "");
                self.error(sp, msg);
            }
        }
        for (t, sp) in std::mem::take(&mut self.fs.plus_checks) {
            if !matches!(self.rty(t), Ty::Var(_)) {
                self.require_plus(t, sp);
            }
        }
        let checks = std::mem::take(&mut self.fs.infer_checks);
        for (t, sp) in checks {
            let z = self.zonk(t);
            if self.arena.has_vars(z) {
                self.infer_error(sp, "cannot infer type; add a type annotation");
            }
        }
        let reflect = std::mem::take(&mut self.fs.reflect_checks);
        for (t, sp, what) in reflect {
            let z = self.zonk(t);
            if self.reflect_local(z).is_some() {
                continue;
            }
            if self.arena.has_vars(z) {
                self.infer_error(sp, format!("cannot infer type for this {what}; add a type annotation"));
            } else if let Some(p) =
                self.arena.params_of(z).into_iter().find(|&p| !self.params[p as usize].reflect && self.reflect_param_local(p).is_none())
            {
                let msg = format!(
                    "this {what} needs a description of `{}` at run time, so `{}` must be a `reflect` type parameter of this function",
                    self.show(z),
                    self.name(self.params[p as usize].name)
                );
                self.error(sp, msg);
            } else if let Some((d, f)) = self.private_field_in(z) {
                let (ty, field) = (self.ctor_name(Ctor::Def(d), 0), self.name(f));
                self.error(sp, format!("this {what} cannot build `{ty}` here: its field `{field}` is private"));
            } else if let Some(m) = self.first_unmet_requirement(z) {
                let msg = format!("this {what} cannot build {}: {}", self.show(z), self.unmet_message(m, ""));
                self.error(sp, msg);
            }
        }
        for info in self.fs.locals.iter_mut().filter(|l| l.mutable) {
            info.cell = false;
        }
        let mut fin = unify::Finish { dicts, hoist: !self.fs.evidence.is_empty(), hoisted: Vec::new() };
        self.finish_expr(&mut body, &mut fin, 0, false);
        if !fin.hoisted.is_empty() {
            let (span, ty) = (body.span, body.ty);
            let mut stmts: Vec<HExpr> = fin
                .hoisted
                .into_iter()
                .map(|(l, init)| {
                    let pat = HPat { kind: HPatKind::Bind(l), ty: TypeId::UNIT, span };
                    HExpr { kind: HKind::Let { pat, init: Box::new(init), else_: None }, ty: TypeId::UNIT, span }
                })
                .collect();
            stmts.push(body);
            body = HExpr { kind: HKind::Block(stmts), ty, span };
        }
        let unused = std::mem::take(&mut self.fs_unused);
        if self.linting() {
            self.lint_fn_facts(unused);
        }
        let locals = std::mem::take(&mut self.fs.locals);
        let doc_refs = std::mem::take(&mut self.fs.doc_refs);
        self.release_vars();
        HFn { name, params, body, locals, span, doc_refs }
    }

    /// Drops the inference variables made while checking the function just
    /// finished: its body is zonked, so only older variables bound since
    /// (`FnState::old_bound`) may refer to them, and those are zonked too.
    /// Kept for module initializers (whose globals' types functions may
    /// still determine) and type queries.
    fn release_vars(&mut self) {
        let mark = self.fs.var_mark as usize;
        if self.type_query.is_some() || matches!(self.fs.ctx.first().map(|c| c.kind), Some(CtxKind::TopLevel { .. })) {
            return;
        }
        for x in std::mem::take(&mut self.fs.old_bound) {
            let xv = self.arena.var(x);
            let z = self.zonk(xv);
            if self.arena.has_vars(z) && self.arena.any(z, &mut |t| matches!(t, Ty::Var(v) if v as usize >= mark)) {
                return;
            }
            self.subst[x as usize] = if z == xv { None } else { Some(z) };
        }
        self.subst.truncate(mark);
    }

    // ------------------------------------------------------------------
    // Locals and scopes

    /// Reports parameters with the same name (other than `_`).
    pub(crate) fn check_param_names<'p>(&mut self, names: impl Iterator<Item = &'p ast::Ident>) {
        let mut seen = HashSet::new();
        for id in names {
            if id.name != "_" && !seen.insert(id.name.as_str()) {
                self.error(id.span, format!("parameter `{}` is declared twice", id.name));
            }
        }
    }

    pub(crate) fn declare_local(&mut self, name: &str, ty: TypeId, mutable: bool, span: Span, is_param: bool) -> LocalId {
        let name = self.sym(name);
        self.declare_local_sym(name, ty, mutable, span, is_param)
    }

    pub(crate) fn declare_local_sym(&mut self, name: Symbol, ty: TypeId, mutable: bool, span: Span, is_param: bool) -> LocalId {
        let owner = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
        self.fs.locals.push(LocalInfo { name, mutable, owner, cell: false, span });
        self.fs.data.push(LocalData { ty, scheme: vec![], used: false, assigned: false, is_param, decl_span: span });
        let id = (self.fs.locals.len() - 1) as LocalId;
        if name != Symbol::UNDERSCORE {
            self.fs.scopes.declare(name, id);
        }
        id
    }

    pub(crate) fn hidden_local(&mut self, ty: TypeId, span: Span) -> LocalId {
        let owner = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
        self.fs.locals.push(LocalInfo { name: Symbol::TMP, mutable: true, owner, cell: false, span });
        self.fs.data.push(LocalData { ty, scheme: vec![], used: true, assigned: true, is_param: false, decl_span: span });
        (self.fs.locals.len() - 1) as LocalId
    }

    pub(crate) fn find_local(&self, name: &str) -> Option<LocalId> {
        self.fs.scopes.find(self.syms.get(name)?)
    }

    pub(crate) fn push_scope(&mut self) {
        self.fs.scopes.push();
    }

    pub(crate) fn new_lambda(&mut self) -> u32 {
        self.fs.next_lambda += 1;
        self.fs.lambda_first_local.push(self.fs.locals.len() as LocalId);
        self.fs.next_lambda
    }
    pub(crate) fn pop_scope(&mut self) {
        self.fs.scopes.pop();
    }

    /// Records a use of a local (captures are found by `finish_expr`).
    pub(crate) fn use_local(&mut self, id: LocalId) {
        self.fs.data[id as usize].used = true;
    }
}

fn coll_arity(c: Ctor) -> usize {
    if c == Ctor::Map { 2 } else { 1 }
}

pub fn builtin_type(name: &str) -> Option<TyRef> {
    Some(match name {
        "int" => TyRef::Prim(TypeId::INT),
        "float" => TyRef::Prim(TypeId::FLOAT),
        "bool" => TyRef::Prim(TypeId::BOOL),
        "str" => TyRef::Prim(TypeId::STR),
        "bytes" => TyRef::Prim(TypeId::BYTES),
        "duration" => TyRef::Prim(TypeId::DURATION),
        "Range" => TyRef::Prim(TypeId::RANGE),
        "List" => TyRef::Coll(Ctor::List),
        "Map" => TyRef::Coll(Ctor::Map),
        "Set" => TyRef::Coll(Ctor::Set),
        _ => return None,
    })
}

pub fn ctor_of(t: Ty) -> Option<Ctor> {
    Some(match t {
        Ty::Int => Ctor::Int,
        Ty::Float => Ctor::Float,
        Ty::Bool => Ctor::Bool,
        Ty::Str => Ctor::Str,
        Ty::Bytes => Ctor::Bytes,
        Ty::Duration => Ctor::Duration,
        Ty::Unit => Ctor::Unit,
        Ty::Range => Ctor::Range,
        Ty::List(_) => Ctor::List,
        Ty::Map(..) => Ctor::Map,
        Ty::Set(_) => Ctor::Set,
        Ty::Tuple(_) => Ctor::Tuple,
        Ty::Fn(..) => Ctor::Fn,
        Ty::Named(d, _) => Ctor::Def(d),
        _ => return None,
    })
}

/// Default expressions that can be inlined at call sites (no names to resolve).
fn is_closed_const(e: &ast::Expr) -> bool {
    use ast::ExprKind as K;
    match &e.kind {
        K::Unit | K::Bool(_) | K::Int(_) | K::Float(_) | K::Duration(_) | K::Bytes(_) => true,
        K::Str(segs) => segs.iter().all(|s| matches!(s, ast::StrSeg::Text(_))),
        K::List(xs) => xs.is_empty(),
        K::Map(xs) => xs.is_empty(),
        K::Unary(ast::UnOp::Neg, inner) => {
            matches!(inner.kind, K::Int(_) | K::Float(_) | K::Duration(_))
        }
        _ => false,
    }
}

/// Names referenced (as identifiers or struct-literal shorthands) inside
/// function bodies, methods, tests and default expressions of a module.
/// Top-level bindings not in this set can live in the module initializer's
/// registers instead of global slots.
pub(crate) fn names_used_in_functions(m: &ast::Module) -> HashSet<String, FxBuild> {
    let mut out = HashSet::default();
    fn fn_decl(f: &ast::FnDecl, out: &mut HashSet<String, FxBuild>) {
        for p in &f.params {
            if let Some(d) = &p.default {
                expr(d, out);
            }
        }
        if let Some(b) = &f.body {
            block(b, out);
        }
    }
    fn block(b: &ast::Block, out: &mut HashSet<String, FxBuild>) {
        for s in &b.stmts {
            stmt(s, out);
        }
    }
    fn stmt(s: &ast::Stmt, out: &mut HashSet<String, FxBuild>) {
        match &s.kind {
            ast::StmtKind::Let { value, else_block, .. } => {
                expr(value, out);
                if let Some(b) = else_block {
                    block(b, out);
                }
            }
            ast::StmtKind::Assign { target, value, .. } => {
                expr(target, out);
                expr(value, out);
            }
            ast::StmtKind::Expr(e) | ast::StmtKind::Defer(e) | ast::StmtKind::Use { value: e, .. } => expr(e, out),
            ast::StmtKind::Fn(f) => fn_decl(f, out),
        }
    }
    fn cond(c: &ast::Cond, out: &mut HashSet<String, FxBuild>) {
        match c {
            ast::Cond::Expr(e) => expr(e, out),
            ast::Cond::Let(_, e) => expr(e, out),
        }
    }
    fn expr(e: &ast::Expr, out: &mut HashSet<String, FxBuild>) {
        use ast::ExprKind as K;
        match &e.kind {
            K::Ident(id) => {
                out.insert(id.name.clone());
            }
            K::Str(segs) => {
                for s in segs {
                    if let ast::StrSeg::Expr(x, _) = s {
                        expr(x, out);
                    }
                }
            }
            K::List(xs) | K::Tuple(xs) => xs.iter().for_each(|x| expr(x, out)),
            K::Repeat(a, b) | K::Coalesce(a, b) | K::Index(a, b) | K::Binary(_, a, b) => {
                expr(a, out);
                expr(b, out);
            }
            K::Map(kvs) => {
                for (k, v) in kvs {
                    expr(k, out);
                    expr(v, out);
                }
            }
            K::ListComp(x, cs) => {
                expr(x, out);
                clauses(cs, out);
            }
            K::MapComp(k, v, cs) => {
                expr(k, out);
                expr(v, out);
                clauses(cs, out);
            }
            K::Unary(_, x) | K::Paren(x) | K::Try(x) | K::Spawn(x) | K::Field(x, _) | K::TupleField(x, _, _) => expr(x, out),
            K::Range { start, end, .. } => {
                if let Some(x) = start {
                    expr(x, out);
                }
                if let Some(x) = end {
                    expr(x, out);
                }
            }
            K::Call { callee, args } => {
                expr(callee, out);
                args.iter().for_each(|a| expr(&a.value, out));
            }
            K::MethodCall { receiver, args, .. } => {
                expr(receiver, out);
                args.iter().for_each(|a| expr(&a.value, out));
            }
            K::If { cond: c, then, else_ } => {
                cond(c, out);
                block(then, out);
                if let Some(x) = else_ {
                    expr(x, out);
                }
            }
            K::Match { scrutinee, arms, .. } => {
                expr(scrutinee, out);
                for a in arms {
                    if let Some(g) = &a.guard {
                        expr(g, out);
                    }
                    expr(&a.body, out);
                }
            }
            K::Block(b) | K::Loop { body: b, .. } => block(b, out),
            K::While { cond: c, body, .. } => {
                cond(c, out);
                block(body, out);
            }
            K::For { iter, body, .. } => {
                expr(iter, out);
                block(body, out);
            }
            K::Lambda { body, .. } => expr(body, out),
            K::Return(Some(x)) | K::Break { value: Some(x), .. } => expr(x, out),
            _ => {}
        }
    }
    fn clauses(cs: &[ast::CompClause], out: &mut HashSet<String, FxBuild>) {
        for c in cs {
            match c {
                ast::CompClause::For(_, e) | ast::CompClause::If(e) => expr(e, out),
            }
        }
    }
    for item in &m.items {
        match &item.kind {
            ItemKind::Fn(f) => fn_decl(f, &mut out),
            ItemKind::Struct(s) => {
                s.methods.iter().for_each(|f| fn_decl(f, &mut out));
                s.fields.iter().filter_map(|f| f.default.as_ref()).for_each(|d| expr(d, &mut out));
            }
            ItemKind::Enum(e) => e.methods.iter().for_each(|f| fn_decl(f, &mut out)),
            ItemKind::Extend(x) => x.methods.iter().for_each(|f| fn_decl(f, &mut out)),
            ItemKind::Trait(t) => t.methods.iter().for_each(|f| fn_decl(f, &mut out)),
            ItemKind::Impl(i) => i.methods.iter().for_each(|f| fn_decl(f, &mut out)),
            ItemKind::Test(t) => block(&t.body, &mut out),
            _ => {}
        }
    }
    out
}

/// What `Session::type_of` needs to show a type with open inference
/// variables as a generic one.
#[derive(Clone, Default)]
pub(crate) struct TypeQuery {
    /// The variables made for the type parameters of instantiated functions.
    pub origins: Vec<(VarId, ParamId)>,
    /// The traits required of types not inferred yet.
    pub traits: Vec<(TypeId, TraitRef)>,
}
