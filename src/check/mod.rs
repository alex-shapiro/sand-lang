//! Name resolution and type checking: AST to typed HIR.

pub mod budget;
mod docs;
mod expr;
mod fuse;
pub(crate) mod help;
mod pat;
pub mod persist;
mod resources;
mod traits;
mod unify;

use crate::diag::{Diag, FileId, SourceFile, Span};
use crate::hir::*;
use crate::syntax::ast::{self, ItemKind, TypeExpr, TypeExprKind};
use crate::types::*;
use crate::vm::value::{FxBuild, FxHashMap};
use budget::{CompileBudget, MAX_TYPE_SIZE};
use persist::{PMap, PVec};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub use pat::Witness;
pub use traits::{ImplDef, TraitDef, TraitMethod};

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
    /// How many of `generics` come first from the enclosing type, `extend`
    /// block, trait or impl.
    pub outer: usize,
    pub params: Vec<ParamSig>,
    pub ret: TypeId,
    pub has_self: bool,
    pub kind: FnKind,
    pub doc: Option<Arc<str>>,
    pub span: Span,
    pub is_pub: bool,
    pub owner: Option<Ctor>,
    /// Type parameter for `Self`, if the function is a method.
    pub self_ty: Option<TypeId>,
    /// Where the syntax of the body is (script functions and helpers).
    pub decl: Option<FnDeclRef>,
    /// Hidden dictionary parameters, passed before the declared parameters:
    /// evidence that each type implements each trait. For trait methods
    /// (signatures, default methods and impl methods) the first is the
    /// dictionary of the implementing type.
    pub dicts: Vec<(TypeId, TraitRef)>,
    /// The impl this method belongs to.
    pub impl_of: Option<ImplId>,
    /// The trait whose signature or default method this is.
    pub trait_of: Option<TraitId>,
}

/// Where the syntax of a function body is in the items of its module.
/// AST is borrowed while the module is checked.
#[derive(Clone, Copy, Debug)]
pub enum FnDeclRef {
    Fn(DeclPath),
    ParamDefault(DeclPath, u32),
    FieldDefault { item: u32, field: u32 },
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
}

/// Registered native functions and value types, keyed by qualified name.
#[derive(Clone, Debug, Default)]
pub struct NativeIds {
    pub fns: HashMap<Cow<'static, str>, u32>,
    pub values: HashMap<Cow<'static, str>, ValueTypeInfo>,
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
    pub obligations: Vec<(TypeId, Bound, Span)>,
    pub reflect_checks: Vec<(TypeId, Span)>,
    pub infer_checks: Vec<(TypeId, Span)>,
    /// Inside a top-level statement of a module (lets become globals).
    pub top_level: bool,
    pub generics: Vec<(Symbol, ParamId)>,
    pub self_ty: Option<TypeId>,
    /// Trait evidence available in the function: (type, trait, dictionary expression).
    pub evidence: Vec<(TypeId, TraitRef, HExpr)>,
    /// The evidence needed by the [`HKind::Evidence`] placeholders of the body.
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
    /// The natives that the modules being checked can declare.
    natives: Arc<NativeIds>,
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
    /// The bindings of the inference variables of the module being checked
    /// (cleared when it is done: its types are all resolved by then).
    pub subst: Vec<Option<TypeId>>,
    /// For each type definition, what it needs to satisfy `Eq`, `Ord` and
    /// `Hash` (see `unify::Derived`).
    pub(crate) derived: PVec<[unify::Derived; 3]>,
    pub diags: Vec<Diag>,
    pub wk: WellKnown,
    pub prelude: Option<ModId>,
    pub task_mod: Option<ModId>,
    pub cur_mod: ModId,
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
    /// Impls by (trait, type constructor, tuple arity).
    pub(crate) impls_by: PMap<(TraitId, Ctor, u32), Arc<[ImplId]>>,
    /// Impls by (type constructor, tuple arity).
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
    /// The prelude's private trait `Num` (static `zero` and `one`): the
    /// evidence that a type satisfies the built-in bound `Num`.
    pub(crate) num_trait: Option<TraitId>,
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
            subst: Vec::new(),
            derived: PVec::new(),
            diags: Vec::new(),
            wk: WellKnown::default(),
            prelude: None,
            task_mod: None,
            cur_mod: 0,
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
            num_trait: None,
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
            let mut bounds: Vec<String> = Vec::new();
            let declared = origin(v).map(|p| &self.params[p as usize]);
            let required = self.type_query.iter().flat_map(|q| &q.bounds).filter(|(t, _)| self.rty(*t) == Ty::Var(v)).map(|(_, b)| *b);
            // (`Add` cannot be written: `+` is shown as allowed on any type.)
            let builtin = required.chain(declared.into_iter().flat_map(|p| p.bounds.clone())).filter(|&b| b != Bound::Add);
            for b in builtin {
                if !bounds.iter().any(|x| x == b.name()) {
                    bounds.push(b.name().to_string());
                }
            }
            bounds.extend(declared.into_iter().flat_map(|p| p.traits.iter().map(|tr| self.show_trait(tr))));
            generics.push(if bounds.is_empty() { name.clone() } else { format!("{name}: {}", bounds.join(" + ")) });
            var_names.push((v, name));
        }
        let shown = TypePrinter { var_names: &var_names, ..self.printer() }.show(t);
        format!("fn<{}>{}", generics.join(", "), shown.strip_prefix("fn").unwrap_or(&shown))
    }

    pub fn new_param(&mut self, name: Symbol, bounds: Vec<Bound>, reflect: bool) -> ParamId {
        self.params.push(ParamInfo { name, bounds, traits: Vec::new(), reflect });
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
            if i == self.prelude.unwrap_or(u32::MAX) {
                self.find_well_known();
            }
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
        natives: &Arc<NativeIds>,
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
        natives: &Arc<NativeIds>,
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
                self.define_ident(&t.name, Def::Trait(id), t.is_pub);
                trait_ids.push(id);
            }
        }
        let mut struct_ids = Vec::new();
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let generics = self.declare_generics(&s.generics);
                    let kind = TypeDefKind::Struct { fields: vec![] };
                    let id = self.new_typedef(&s.name, generics, kind, s.is_pub, s.doc.as_deref());
                    self.define_ident(&s.name, Def::Type(TyRef::Def(id)), s.is_pub);
                    struct_ids.push(id);
                }
                ItemKind::Enum(e) => {
                    let generics = self.declare_generics(&e.generics);
                    let kind = TypeDefKind::Enum { variants: vec![] };
                    let id = self.new_typedef(&e.name, generics, kind, e.is_pub, e.doc.as_deref());
                    self.define_ident(&e.name, Def::Type(TyRef::Def(id)), e.is_pub);
                    struct_ids.push(id);
                }
                ItemKind::NativeType(n) => {
                    if !self.allows_native() {
                        self.error(n.name.span, "native types can only be declared by native modules");
                    }
                    let generics = self.declare_generics(&n.generics);
                    let module = self.name(self.mods[self.cur_mod as usize].name);
                    let value = self.natives.values.get(format!("{module}.{}", n.name.name).as_str()).copied();
                    if value.is_some() && !generics.is_empty() {
                        self.error(n.name.span, "a native value type cannot have type parameters");
                    }
                    let id = self.new_typedef(&n.name, generics, TypeDefKind::Opaque(value), n.is_pub, n.doc.as_deref());
                    self.define_ident(&n.name, Def::Type(TyRef::Def(id)), n.is_pub);
                }
                ItemKind::TypeAlias(a) => {
                    let generics = self.declare_generics(&a.generics);
                    let name = self.sym(&a.name.name);
                    self.aliases.push(Alias { name, generics, ty: None, decl: Some((mi, Arc::new(a.ty.clone()))), resolving: false });
                    let id = self.aliases.len() - 1;
                    self.define(name, Def::Type(TyRef::Alias(id)), a.is_pub, a.name.span);
                }
                _ => {}
            }
        }
        if pm.kind == ModKind::Prelude {
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
        // 3. Fields and variants, and the built-in bounds they satisfy.
        let mut k = 0;
        for (idx, item) in items.iter().enumerate() {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let id = struct_ids[k];
                    k += 1;
                    self.fill_struct(id, s, idx as u32);
                }
                ItemKind::Enum(e) => {
                    let id = struct_ids[k];
                    k += 1;
                    self.fill_enum(id, e);
                }
                _ => {}
            }
        }
        self.derive_bounds(first_type);
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

    /// Type parameters of a type declaration (only built-in bounds are allowed).
    fn declare_generics(&mut self, gs: &[ast::GenericParam]) -> Vec<ParamId> {
        let out = self.declare_params(gs, &[], None);
        for (g, &p) in gs.iter().zip(&out) {
            // `Num` evidence is passed to the type's methods, not stored.
            let num = self.num_trait;
            self.params[p as usize].traits.retain(|t| Some(t.trait_id) != num);
            if !self.params[p as usize].traits.is_empty() {
                self.error(
                    g.name.span,
                    format!(
                        "trait bounds are not allowed on the type parameters of a type; add them to the functions or impls that need them (`where {}: ...`)",
                        g.name.name
                    ),
                );
                self.params[p as usize].traits.clear();
            }
        }
        out
    }

    /// Declares type parameters with their bounds; bounds may refer to any of
    /// the parameters and to `outer`.
    fn declare_params(&mut self, gs: &[ast::GenericParam], outer: &[(Symbol, ParamId)], self_ty: Option<TypeId>) -> Vec<ParamId> {
        let mut scope = outer.to_vec();
        let mut out = Vec::new();
        for g in gs {
            let name = self.sym(&g.name.name);
            let p = self.new_param(name, vec![], g.reflect);
            scope.push((name, p));
            out.push(p);
        }
        for (g, &p) in gs.iter().zip(&out) {
            let (bounds, traits) = self.resolve_bounds(&g.bounds, &scope, self_ty);
            self.add_bounds(p, bounds, traits);
        }
        out
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

    /// The names of type parameters `ps` (a scope for resolving types).
    fn generic_scope(&self, ps: &[ParamId]) -> Vec<(Symbol, ParamId)> {
        ps.iter().map(|&p| (self.params[p as usize].name, p)).collect()
    }

    fn fill_struct(&mut self, id: TypeDefId, s: &ast::StructDecl, item: u32) {
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
            let name = self.sym(&f.name.name);
            fields.push(FieldDef {
                name,
                ty,
                mutable: f.mutable,
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
                let const_default = const_of(d);
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
                fields[i].const_default = const_default;
            }
        }
        self.types[id as usize].kind = TypeDefKind::Struct { fields };
    }

    fn fill_enum(&mut self, id: TypeDefId, e: &ast::EnumDecl) {
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
            let names: Vec<Option<Symbol>> = v.fields.iter().map(|f| f.name.as_ref().map(|n| self.syms.intern(&n.name))).collect();
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
        let intrinsic = match f.name.name.as_str() {
            "help" => Some(Intrinsic::Help),
            "members" => Some(Intrinsic::Members),
            _ => None,
        };
        let kind = if let Some(i) = intrinsic.filter(|_| f.is_native && owner.is_none() && Some(self.cur_mod) == self.prelude) {
            FnKind::Intrinsic(i)
        } else if f.is_native {
            let module_name = self.name(self.mods[self.cur_mod as usize].name).to_string();
            let type_prefix = owner.map(|c| c.native_prefix(&self.types, &self.syms));
            let qual_name = match (&type_prefix, self.mods[self.cur_mod as usize].kind) {
                (Some(t), ModKind::Prelude) => format!("{t}.{}", f.name.name),
                (Some(t), _) => format!("{module_name}.{t}.{}", f.name.name),
                (None, ModKind::Prelude) => f.name.name.clone(),
                (None, _) => format!("{module_name}.{}", f.name.name),
            };
            match self.natives.fns.get(qual_name.as_str()) {
                Some(&n) => FnKind::Native(n),
                None => {
                    self.error(f.name.span, format!("no native implementation registered for `{qual_name}`"));
                    FnKind::Native(u32::MAX)
                }
            }
        } else {
            FnKind::Script
        };
        // One hidden dictionary per (type parameter, trait bound).
        let mut dicts: Vec<(TypeId, TraitRef)> = Vec::new();
        for &p in &generics {
            let pt = self.param_ty(p);
            dicts.extend(self.params[p as usize].traits.iter().map(|&tr| (pt, tr)));
        }
        if f.is_native && !dicts.is_empty() {
            self.error(f.name.span, "native functions cannot have trait bounds");
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
        // Each method gets its own copy of the type's parameters so that
        // `where` clauses only affect that method.
        let orig = self.types[def as usize].generics.clone();
        let mut outer = Vec::new();
        for p in &orig {
            let info = self.params[*p as usize].clone();
            let np = self.new_param(info.name, vec![], false);
            self.add_bounds(np, info.bounds, vec![]);
            outer.push((info.name, np));
        }
        let ps: Vec<ParamId> = outer.iter().map(|(_, p)| *p).collect();
        let self_ty = self.def_self_ty(def, &ps);
        let id = self.declare_fn(m, at, DeclRules::PLAIN, Some(ctor), &outer, Some(self_ty));
        self.register_method(ctor, &m.name, id);
        id
    }

    fn register_method(&mut self, ctor: Ctor, name: &ast::Ident, id: FnId) {
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

    /// The methods named `name` of type constructor `c`.
    pub(crate) fn methods_named(&self, c: Ctor, name: &str) -> Option<Arc<[FnId]>> {
        let s = self.syms.get(name)?;
        self.methods.get(&(c, s)).cloned()
    }

    fn declare_extend(&mut self, x: &ast::ExtendDecl, item: u32) {
        // Resolve the target once to find its constructor.
        // (This also reports invalid bounds once, even if there are no methods.)
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
        let synthetic =
            fi.impl_of.is_some() || fi.trait_of.is_some() || matches!(decl, FnDeclRef::ParamDefault(..) | FnDeclRef::FieldDefault { .. });
        self.begin_fn(generics, self_ty);
        let mut param_locals = self.declare_dict_params(id);
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

    /// Final checks for a function: bounds, inference completeness; the
    /// final walk of the body.
    fn finish_fn(&mut self, name: String, params: Vec<LocalId>, mut body: HExpr, span: Span) -> HFn {
        // Trait evidence first: resolving it may determine types.
        let dicts = self.resolve_holes();
        let obligations = std::mem::take(&mut self.fs.obligations);
        if let Some(q) = &mut self.type_query {
            q.bounds.extend(obligations.iter().map(|(t, b, _)| (*t, *b)));
        }
        for (t, b, sp) in obligations {
            if self.satisfies(t, b) == Some(false) {
                self.report_bound(t, b, sp);
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
        for (t, sp) in reflect {
            let z = self.zonk(t);
            if self.arena.has_vars(z) || self.arena.has_params(z) {
                self.infer_error(sp, "cannot infer type for this call; add a type annotation");
            }
        }
        // Cells are decided by the final walk.
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

fn const_of(e: &ast::Expr) -> Option<ConstVal> {
    use ast::ExprKind as K;
    Some(match &e.kind {
        K::Unit => ConstVal::Unit,
        K::Bool(b) => ConstVal::Bool(*b),
        K::Int(i) => ConstVal::Int(*i),
        K::Float(f) => ConstVal::Float(*f),
        K::Duration(d) => ConstVal::Dur(*d),
        K::Str(segs) => {
            let mut s = String::new();
            for seg in segs {
                match seg {
                    ast::StrSeg::Text(t) => s.push_str(t),
                    _ => return None,
                }
            }
            ConstVal::Str(s)
        }
        K::List(xs) if xs.is_empty() => ConstVal::EmptyList,
        K::Map(xs) if xs.is_empty() => ConstVal::EmptyMap,
        K::Ident(id) if id.name == "None" => ConstVal::None,
        K::Unary(ast::UnOp::Neg, inner) => match inner.kind {
            K::Int(i) => ConstVal::Int(-i),
            K::Float(f) => ConstVal::Float(-f),
            K::Duration(d) => ConstVal::Dur(-d),
            _ => return None,
        },
        _ => return None,
    })
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
            K::Repeat(a, b) | K::Pipe(a, b) | K::Coalesce(a, b) | K::Index(a, b) | K::Binary(_, a, b) => {
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
            K::StructLit { fields, base, .. } => {
                for f in fields {
                    match &f.value {
                        Some(v) => expr(v, out),
                        None => {
                            out.insert(f.name.name.clone());
                        }
                    }
                }
                if let Some(b) = base {
                    expr(b, out);
                }
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
    /// The built-in bounds required of types.
    pub bounds: Vec<(TypeId, Bound)>,
}
