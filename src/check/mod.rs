//! Name resolution and type checking: AST to typed HIR.

pub mod budget;
mod expr;
mod fuse;
mod pat;
mod traits;
mod unify;

use crate::diag::{Diag, FileId, SourceMap, Span};
use crate::hir::*;
use crate::syntax::ast::{self, ItemKind, TypeExpr, TypeExprKind};
use crate::types::*;
use budget::{CompileBudget, MAX_TYPE_SIZE};
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
    pub ast: ast::Module,
    pub kind: ModKind,
    /// For each `import` item (by item index): the target module index.
    pub imports: HashMap<usize, ModId>,
}

#[derive(Clone, Debug)]
pub enum Def {
    Fn(FnId),
    Type(TyRef),
    Global(GlobalId),
    Module(ModId),
    /// An enum variant usable unqualified (`Some`, `None`, `Ok`, `Err`).
    Variant(TypeDefId, u32),
    Trait(TraitId),
}

#[derive(Clone, Debug)]
pub enum TyRef {
    Prim(Type),
    Set,
    Def(TypeDefId),
    Alias(usize),
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
    pub fn native_prefix(self, defs: &[TypeDef]) -> String {
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
            Ctor::Def(d) => defs[d as usize].name.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Alias {
    pub name: String,
    pub generics: Vec<ParamId>,
    pub ty: Option<Type>,
    pub decl: Option<(ModId, ast::TypeExpr)>,
    pub resolving: bool,
}

#[derive(Clone, Debug)]
pub struct ParamSig {
    pub name: String,
    pub ty: Type,
    pub default: Option<DefaultArg>,
}

#[derive(Clone, Debug)]
pub enum DefaultArg {
    /// Inlined constant expression (closed: literals, empty collections, `None`).
    Inline(ast::Expr),
    /// Computed by a helper function from the preceding parameters.
    Helper(FnId),
}

#[derive(Clone, Debug)]
pub enum FnKind {
    Script,
    Native(u32),
    Init,
    Test(String),
    /// Computes the default value of a parameter or field.
    DefaultHelper,
    /// The signature of method `slot` of a trait; calls dispatch through a dictionary.
    TraitMethod(TraitId, u32),
}

#[derive(Clone, Debug)]
pub struct FnInfo {
    pub name: String,
    pub qual_name: String,
    pub module: ModId,
    pub generics: Vec<ParamId>,
    pub params: Vec<ParamSig>,
    pub ret: Type,
    pub has_self: bool,
    pub kind: FnKind,
    pub doc: Option<String>,
    pub span: Span,
    pub is_pub: bool,
    pub owner: Option<Ctor>,
    /// Type parameter for `Self`, if the function is a method.
    pub self_ty: Option<Type>,
    /// AST for checking the body (script functions and helpers).
    pub decl: Option<FnDeclRef>,
    /// Hidden dictionary parameters, passed before the declared parameters:
    /// evidence that each type implements each trait. For trait methods
    /// (signatures, default methods and impl methods) the first is the
    /// dictionary of the implementing type.
    pub dicts: Vec<(Type, TraitRef)>,
    /// The impl this method belongs to.
    pub impl_of: Option<ImplId>,
    /// The trait whose signature or default method this is.
    pub trait_of: Option<TraitId>,
}

#[derive(Clone, Debug)]
pub enum FnDeclRef {
    Fn(ModId, ast::FnDecl),
    /// Default helper: expression, number of leading params.
    Default(ModId, ast::Expr),
    Init(ModId),
    Test(ModId, ast::Block),
}

#[derive(Clone, Debug)]
pub struct GlobalInfo {
    pub name: String,
    pub module: ModId,
    pub ty: Type,
    pub mutable: bool,
    pub is_pub: bool,
    pub span: Span,
}

#[derive(Clone)]
pub struct ModInfo {
    pub name: String,
    pub file: FileId,
    pub kind: ModKind,
    pub scope: HashMap<String, (Def, bool, Span)>,
    pub init: Option<FnId>,
    pub doc: Option<String>,
    /// Names referenced (for unused-import lint): module aliases and from-imports.
    pub import_spans: Vec<(String, Span)>,
}

/// Facts gathered during checking for the linter.
#[derive(Default, Debug, Clone)]
pub struct LintFacts {
    /// Import names of the main module and their spans.
    pub main_imports: Vec<(String, Span)>,
    pub unused_results: Vec<Span>,
    pub used_names: HashSet<(ModId, String)>,
    /// (declaration span, name, used, mutable, reassigned, type display)
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
}

pub struct CheckedProgram {
    pub fns: Vec<FnInfo>,
    pub bodies: Vec<Option<HFn>>,
    pub types: Vec<TypeDef>,
    pub params: Vec<ParamInfo>,
    pub wk: WellKnown,
    /// Runtime variant id -> (enum def, index within enum).
    pub variants: Vec<(TypeDefId, u32)>,
    pub globals: Vec<GlobalInfo>,
    pub module_inits: Vec<FnId>,
    pub modules: Vec<ModSummary>,
    pub tests: Vec<(String, FnId)>,
    pub lint: LintFacts,
    pub main_module: ModId,
    pub traits: Vec<TraitDef>,
    pub impls: Vec<ImplDef>,
}

pub struct ModSummary {
    pub name: String,
    pub kind: ModKind,
    pub summary: String,
}

pub type NativeLookup<'a> = &'a (dyn Fn(&str) -> Option<u32> + Sync);

#[derive(Clone)]
pub(crate) struct LocalData {
    pub ty: Type,
    pub scheme: Vec<ParamId>,
    pub used: bool,
    pub assigned: bool,
    pub is_param: bool,
    pub decl_span: Span,
}

#[derive(Clone)]
pub(crate) struct LoopCtx {
    /// The type of `break` values (`loop` only).
    pub break_ty: Option<Type>,
    pub label: Option<String>,
    /// The loop of a comprehension clause, which `break` and `continue` skip
    /// (it still counts for their depth).
    pub hidden: bool,
}

#[derive(Clone)]
pub(crate) struct FnCtx {
    pub ret: Type,
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
}

/// The local names in scope in a function: a stack of scopes, indexed by
/// name so that a lookup does not scan every binding in scope (that made
/// functions with many locals quadratic to check). A name bound again in
/// the same scope replaces its binding there: the older one could never be
/// seen again.
#[derive(Default, Clone)]
pub(crate) struct Scopes {
    /// Per scope, innermost last: name -> local.
    scopes: Vec<HashMap<String, LocalId>>,
    /// Per name: the scopes binding it (indices into `scopes`), innermost last.
    by_name: HashMap<String, Vec<u32>>,
}

impl Scopes {
    pub(crate) fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop(&mut self) {
        for name in self.scopes.pop().unwrap_or_default().into_keys() {
            self.unbind(&name);
        }
    }

    /// Binds `name` to `id` in the innermost scope.
    pub(crate) fn declare(&mut self, name: &str, id: LocalId) {
        let depth = self.scopes.len() as u32 - 1;
        if self.scopes.last_mut().unwrap().insert(name.to_string(), id).is_none() {
            self.by_name.entry(name.to_string()).or_default().push(depth);
        }
    }

    pub(crate) fn find(&self, name: &str) -> Option<LocalId> {
        let depth = *self.by_name.get(name)?.last()?;
        self.scopes[depth as usize].get(name).copied()
    }

    /// Removes the binding of `name` from the innermost scope, if any.
    pub(crate) fn remove(&mut self, name: &str) -> Option<LocalId> {
        let id = self.scopes.last_mut().unwrap().remove(name)?;
        self.unbind(name);
        Some(id)
    }

    /// Drops the innermost entry of `name`'s list of scopes.
    fn unbind(&mut self, name: &str) {
        if let Some(depths) = self.by_name.get_mut(name) {
            depths.pop();
            if depths.is_empty() {
                self.by_name.remove(name);
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
    pub obligations: Vec<(Type, Bound, Span)>,
    pub reflect_checks: Vec<(Type, Span)>,
    pub infer_checks: Vec<(Type, Span)>,
    /// Inside a top-level statement of a module (lets become globals).
    pub top_level: bool,
    pub generics: Vec<(String, ParamId)>,
    pub self_ty: Option<Type>,
    /// Trait evidence available in the function: (type, trait, dictionary expression).
    pub evidence: Vec<(Type, TraitRef, HExpr)>,
    /// Whether the body contains [`HKind::Evidence`] placeholders.
    pub holes: bool,
}

#[derive(Clone)]
pub struct Checker<'a> {
    pub sm: &'a SourceMap,
    pub natives: NativeLookup<'a>,
    pub mods: Vec<ModInfo>,
    pub types: Vec<TypeDef>,
    pub aliases: Vec<Alias>,
    pub params: Vec<ParamInfo>,
    pub fns: Vec<FnInfo>,
    pub bodies: Vec<Option<HFn>>,
    pub globals: Vec<GlobalInfo>,
    pub methods: HashMap<(Ctor, String), Vec<FnId>>,
    pub variants: Vec<(TypeDefId, u32)>,
    pub subst: Vec<Option<Type>>,
    pub var_bounds: Vec<Vec<(Bound, Span)>>,
    pub diags: Vec<Diag>,
    pub wk: WellKnown,
    pub prelude: Option<ModId>,
    pub task_mod: Option<ModId>,
    pub cur_mod: ModId,
    pub(crate) fs: FnState,
    pub lint: LintFacts,
    pub tests: Vec<(String, FnId)>,
    pub main_module: ModId,
    pub(crate) fs_unused: Vec<(Type, Span)>,
    pub(crate) global_facts: Vec<(GlobalId, usize)>,
    pub(crate) loop_breaks: Vec<bool>,
    pub(crate) lint_func: u32,
    /// Names referenced from functions of the module being checked.
    pub(crate) fn_names: HashSet<String>,
    pub traits: Vec<TraitDef>,
    pub impls: Vec<ImplDef>,
    /// Impls by (trait, type constructor, tuple arity).
    pub(crate) impls_by: HashMap<(TraitId, Ctor, u32), Vec<ImplId>>,
    /// Impls by (type constructor, tuple arity).
    pub(crate) impls_by_ctor: HashMap<(Ctor, u32), Vec<ImplId>>,
    /// Records variable bindings made by `unify` so they can be undone.
    pub(crate) unify_log: Option<Vec<VarId>>,
    /// Bounds the work of checking (see `budget`).
    pub budget: Arc<CompileBudget>,
    /// The module of a REPL session, whose inputs are checked one at a time
    /// ([`Checker::check_cell`]).
    pub(crate) session: Option<ModId>,
    /// Whether the final expression statement of the session input being
    /// checked is its value, and then (after checking) the value's type.
    pub(crate) want_cell_value: bool,
    pub(crate) cell_value: Option<Type>,
    /// Set while checking an expression for `Session::type_of`.
    pub(crate) type_query: Option<TypeQuery>,
    /// The prelude's private trait `Num` (static `zero` and `one`): the
    /// evidence that a type satisfies the built-in bound `Num`.
    pub(crate) num_trait: Option<TraitId>,
    /// The prelude's `Iterator` trait and `Iter` struct, which `for` loops use.
    pub(crate) iter_trait: Option<TraitId>,
    pub(crate) iter_struct: Option<TypeDefId>,
    /// Prelude functions taking part in iterator fusion (found on first use).
    pub(crate) fuse_fns: Option<fuse::FuseFns>,
}

const MAX_ERRORS: usize = 50;

impl<'a> Checker<'a> {
    pub fn new(sm: &'a SourceMap, natives: NativeLookup<'a>) -> Checker<'a> {
        Checker {
            sm,
            natives,
            mods: Vec::new(),
            types: Vec::new(),
            aliases: Vec::new(),
            params: Vec::new(),
            fns: Vec::new(),
            bodies: Vec::new(),
            globals: Vec::new(),
            methods: HashMap::new(),
            variants: Vec::new(),
            subst: Vec::new(),
            var_bounds: Vec::new(),
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
            global_facts: Vec::new(),
            loop_breaks: Vec::new(),
            lint_func: 0,
            fn_names: HashSet::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            impls_by: HashMap::new(),
            impls_by_ctor: HashMap::new(),
            unify_log: None,
            session: None,
            want_cell_value: false,
            cell_value: None,
            type_query: None,
            budget: Arc::new(CompileBudget::default()),
            num_trait: None,
            iter_trait: None,
            iter_struct: None,
            fuse_fns: None,
        }
    }

    /// Whether `t` has more than `MAX_TYPE_SIZE` nodes once inference
    /// variables are resolved. Costs at most `MAX_TYPE_SIZE` steps (charged
    /// to the budget), even for types with exponentially large expansions.
    pub(crate) fn type_too_large(&self, t: &Type) -> bool {
        fn walk(c: &Checker, t: &Type, n: &mut usize) -> bool {
            *n += 1;
            if *n > MAX_TYPE_SIZE {
                return true;
            }
            match t {
                Type::Var(v) => match &c.subst[*v as usize] {
                    Some(b) => {
                        *n -= 1;
                        walk(c, b, n)
                    }
                    None => false,
                },
                Type::List(a) | Type::Set(a) => walk(c, a, n),
                Type::Map(a, b) => walk(c, a, n) || walk(c, b, n),
                Type::Tuple(ts) | Type::Named(_, ts) => ts.iter().any(|x| walk(c, x, n)),
                Type::Fn(ps, r) => ps.iter().any(|x| walk(c, x, n)) || walk(c, r, n),
                _ => false,
            }
        }
        let mut n = 0;
        let large = walk(self, t, &mut n);
        self.budget.charge(n as u64);
        large
    }

    /// Replaces a type that is too large with `Type::Error`, reporting it.
    pub(crate) fn limit_type_size(&mut self, t: &mut Type, span: Span) {
        if !matches!(t, Type::Error) && self.type_too_large(t) {
            self.error(span, format!("type too large: types may have at most {MAX_TYPE_SIZE} components"));
            *t = Type::Error;
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

    pub fn printer(&self) -> TypePrinter<'_> {
        TypePrinter { defs: &self.types, params: &self.params, wk: self.wk }
    }

    pub fn show(&self, t: &Type) -> String {
        let z = self.zonk(t);
        self.printer().show(&z)
    }

    /// Shows the types of a type mismatch. Distinct types that would look
    /// the same (`expected Point, found Point`, e.g. after a session input
    /// defined `Point` again) are told apart by where they are defined.
    pub fn show_pair(&self, a: &Type, b: &Type) -> (String, String) {
        let (za, zb) = (self.zonk(a), self.zonk(b));
        // Names of the type definitions mentioned, with their ids.
        let mut seen: Vec<TypeDefId> = Vec::new();
        for t in [&za, &zb] {
            t.walk(&mut |t| {
                if let Type::Named(d, _) = t
                    && !seen.contains(d)
                {
                    seen.push(*d);
                }
            });
        }
        let name = |d: TypeDefId| self.types[d as usize].name.as_str();
        let clashing: Vec<TypeDefId> = seen.iter().copied().filter(|&d| seen.iter().any(|&e| e != d && name(e) == name(d))).collect();
        if clashing.is_empty() {
            let p = self.printer();
            return (p.show(&za), p.show(&zb));
        }
        let mut tags: Vec<(TypeDefId, String)> = clashing.iter().map(|&d| (d, self.def_origin(d, false))).collect();
        if tags.iter().any(|(d, t)| tags.iter().any(|(e, u)| e != d && u == t && name(*e) == name(*d))) {
            tags = clashing.iter().map(|&d| (d, self.def_origin(d, true))).collect();
        }
        let tag = |d: TypeDefId| tags.iter().find(|(e, _)| *e == d).map(|(_, t)| t.clone());
        let p = self.printer();
        (p.show_tagged(&za, &tag), p.show_tagged(&zb, &tag))
    }

    /// Where type `d` is defined, for diagnostics: "defined in input 3" (of a
    /// session) or "defined in module geo"; `exact` adds the position.
    fn def_origin(&self, d: TypeDefId, exact: bool) -> String {
        let def = &self.types[d as usize];
        let file = self.sm.files.get(def.span.file as usize);
        let name = file.map_or("", |f| f.name.as_str());
        let place = match name.strip_prefix("<input ").and_then(|f| f.strip_suffix('>')) {
            Some(n) => format!("input {n}"),
            None if Some(def.module) == self.prelude => "the prelude".to_string(),
            None => format!("module {}", def.module_name),
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
    pub(crate) fn show_value_type(&self, t: &Type) -> String {
        let z = self.zonk(t);
        if !matches!(z, Type::Fn(..)) || !z.has_vars() {
            return self.printer().show(&z);
        }
        let mut vars: Vec<VarId> = Vec::new();
        z.walk(&mut |t| {
            if let Type::Var(v) = t
                && !vars.contains(v)
            {
                vars.push(*v);
            }
        });
        let origin = |v: VarId| {
            let q = self.type_query.as_ref()?;
            q.origins.iter().find(|(o, _)| self.zonk(&Type::Var(*o)) == Type::Var(v)).map(|&(_, p)| p)
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
                names[i] = Some(take(&self.params[p as usize].name, &mut used));
            }
        }
        for name in names.iter_mut().filter(|n| n.is_none()) {
            let base = ["T", "U", "V", "W"].into_iter().find(|n| !used.iter().any(|u| u == n)).unwrap_or("T");
            *name = Some(take(base, &mut used));
        }
        let mut params = self.params.clone();
        let mut map: HashMap<VarId, Type> = HashMap::new();
        let mut generics = Vec::new();
        for (&v, name) in vars.iter().zip(names) {
            let name = name.unwrap_or_default();
            let mut bounds: Vec<String> = Vec::new();
            let declared = origin(v).map(|p| &self.params[p as usize]);
            let required = self.type_query.iter().flat_map(|q| &q.bounds).filter(|(t, _)| self.zonk(t) == Type::Var(v)).map(|(_, b)| *b);
            let builtin = required.chain(declared.into_iter().flat_map(|p| p.bounds.clone()));
            for b in builtin {
                if !bounds.iter().any(|x| x == b.name()) {
                    bounds.push(b.name().to_string());
                }
            }
            bounds.extend(declared.into_iter().flat_map(|p| p.traits.iter().map(|tr| self.show_trait(tr))));
            generics.push(if bounds.is_empty() { name.clone() } else { format!("{name}: {}", bounds.join(" + ")) });
            params.push(ParamInfo { name, bounds: Vec::new(), traits: Vec::new(), reflect: false });
            map.insert(v, Type::Param((params.len() - 1) as ParamId));
        }
        let shown = TypePrinter { defs: &self.types, params: &params, wk: self.wk }.show(&replace_vars(&z, &map));
        format!("fn<{}>{}", generics.join(", "), shown.strip_prefix("fn").unwrap_or(&shown))
    }

    pub fn new_param(&mut self, name: &str, bounds: Vec<Bound>, reflect: bool) -> ParamId {
        self.params.push(ParamInfo { name: name.to_string(), bounds, traits: Vec::new(), reflect });
        (self.params.len() - 1) as ParamId
    }

    pub fn option_of(&self, t: Type) -> Type {
        Type::Named(self.wk.option, vec![t])
    }
    pub fn result_of(&self, t: Type, e: Type) -> Type {
        Type::Named(self.wk.result, vec![t, e])
    }
    pub fn error_ty(&self) -> Type {
        Type::Named(self.wk.error, vec![])
    }

    // ------------------------------------------------------------------
    // Program

    pub fn check_program(mut self, modules: Vec<ParsedModule>) -> Result<CheckedProgram, Vec<Diag>> {
        self.add_modules(modules);
        let errors = self.errors();
        if !errors.is_empty() {
            return Err(errors);
        }
        self.lint.main_imports = self.mods[self.main_module as usize].import_spans.clone();
        Ok(self.take_checked(true))
    }

    fn errors(&self) -> Vec<Diag> {
        self.diags.iter().filter(|d| d.severity == crate::diag::Severity::Error).cloned().collect()
    }

    /// Checks `modules`, in dependency order, after the modules checked so
    /// far: a module's [`ModId`] is its position in the combined list.
    fn add_modules(&mut self, modules: Vec<ParsedModule>) {
        let base = self.mods.len();
        for (i, m) in modules.iter().enumerate() {
            let i = base + i;
            self.mods.push(ModInfo {
                name: m.name.clone(),
                file: m.file,
                kind: m.kind,
                scope: HashMap::new(),
                init: None,
                doc: m.ast.doc.clone(),
                import_spans: Vec::new(),
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
    pub(crate) fn take_checked(&mut self, summaries: bool) -> CheckedProgram {
        let module_inits = self.mods.iter().filter_map(|m| m.init).collect();
        let modules = if summaries { self.module_summaries() } else { Vec::new() };
        CheckedProgram {
            fns: std::mem::take(&mut self.fns),
            bodies: std::mem::take(&mut self.bodies),
            types: std::mem::take(&mut self.types),
            params: std::mem::take(&mut self.params),
            wk: self.wk,
            variants: std::mem::take(&mut self.variants),
            globals: std::mem::take(&mut self.globals),
            module_inits,
            modules,
            tests: std::mem::take(&mut self.tests),
            lint: std::mem::take(&mut self.lint),
            main_module: self.main_module,
            traits: std::mem::take(&mut self.traits),
            impls: std::mem::take(&mut self.impls),
        }
    }

    /// Puts back a program taken with [`Checker::take_checked`], so that
    /// checking can resume.
    pub(crate) fn restore_checked(&mut self, cp: CheckedProgram) {
        self.fns = cp.fns;
        self.bodies = cp.bodies;
        self.types = cp.types;
        self.params = cp.params;
        self.variants = cp.variants;
        self.globals = cp.globals;
        self.tests = cp.tests;
        self.lint = cp.lint;
        self.traits = cp.traits;
        self.impls = cp.impls;
    }

    // ------------------------------------------------------------------
    // REPL sessions
    //
    // A session is one script module whose items arrive one input ("cell") at
    // a time: `check_cell` checks each input as more items of that module,
    // after the modules it imports for the first time. Between inputs the
    // checker is stored detached from its source map (`detach`, `attach`);
    // each input is checked on a clone, so a rejected input leaves no trace.

    /// Starts a session: checks `modules` (the prelude, `task` and an empty
    /// module, `session`, which will receive the inputs).
    pub(crate) fn begin_session(&mut self, modules: Vec<ParsedModule>, session: ModId) -> Result<(), Vec<Diag>> {
        self.session = Some(session);
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
    pub(crate) fn check_cell(&mut self, modules: Vec<ParsedModule>, cell: &ParsedModule, want_value: bool) -> Result<(), Vec<Diag>> {
        let session = self.session.expect("not a session");
        self.diags.clear();
        // Lint facts are not needed (and would only accumulate).
        self.lint = LintFacts::default();
        self.global_facts.clear();
        self.add_modules(modules);
        self.cur_mod = session;
        let m = &mut self.mods[session as usize];
        m.file = cell.file;
        m.import_spans.clear();
        self.want_cell_value = want_value;
        self.cell_value = None;
        self.check_module(cell);
        self.want_cell_value = false;
        let errors = self.errors();
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    /// Whether the module being checked is a session module.
    pub(crate) fn in_session(&self) -> bool {
        self.session == Some(self.cur_mod)
    }

    /// Whether `span` is in an earlier input of the session whose input is
    /// being checked; the definitions of earlier inputs may be shadowed.
    fn defined_by_earlier_input(&self, span: Span) -> bool {
        self.in_session() && span.file != self.mods[self.cur_mod as usize].file
    }

    /// Drops the syntax and bodies of the functions checked so far, which a
    /// session no longer needs once they are compiled.
    pub(crate) fn release_bodies(&mut self) {
        for (f, b) in self.fns.iter_mut().zip(&mut self.bodies) {
            if b.take().is_some() {
                f.decl = None;
            }
        }
    }

    /// The checker without its borrows, to be kept between the inputs of a
    /// session and resumed with [`Checker::attach`].
    pub(crate) fn detach(self) -> Checker<'static> {
        static NO_SOURCES: SourceMap = SourceMap { files: Vec::new() };
        fn no_natives(_: &str) -> Option<u32> {
            None
        }
        let Checker {
            sm: _,
            natives: _,
            mods,
            types,
            aliases,
            params,
            fns,
            bodies,
            globals,
            methods,
            variants,
            subst,
            var_bounds,
            diags,
            wk,
            prelude,
            task_mod,
            cur_mod,
            fs,
            lint,
            tests,
            main_module,
            fs_unused,
            global_facts,
            loop_breaks,
            lint_func,
            fn_names,
            traits,
            impls,
            impls_by,
            impls_by_ctor,
            unify_log,
            session,
            want_cell_value,
            cell_value,
            type_query: _,
            budget: _,
            num_trait,
            iter_trait,
            iter_struct,
            fuse_fns,
        } = self;
        Checker {
            sm: &NO_SOURCES,
            natives: &no_natives,
            mods,
            types,
            aliases,
            params,
            fns,
            bodies,
            globals,
            methods,
            variants,
            subst,
            var_bounds,
            diags,
            wk,
            prelude,
            task_mod,
            cur_mod,
            fs,
            lint,
            tests,
            main_module,
            fs_unused,
            global_facts,
            loop_breaks,
            lint_func,
            fn_names,
            traits,
            impls,
            impls_by,
            impls_by_ctor,
            unify_log,
            session,
            want_cell_value,
            cell_value,
            type_query: None,
            // Each input gets its own budget (see `attach`).
            budget: Arc::new(CompileBudget::default()),
            num_trait,
            iter_trait,
            iter_struct,
            fuse_fns,
        }
    }

    /// Resumes a detached checker with the session's sources and natives.
    pub(crate) fn attach(
        detached: Checker<'static>,
        sm: &'a SourceMap,
        natives: NativeLookup<'a>,
        budget: Arc<CompileBudget>,
    ) -> Checker<'a> {
        let mut c: Checker<'a> = detached;
        c.sm = sm;
        c.natives = natives;
        c.budget = budget;
        c
    }

    fn find_well_known(&mut self) {
        let p = self.prelude.unwrap();
        let get = |c: &Checker, n: &str| match c.mods[p as usize].scope.get(n) {
            Some((Def::Type(TyRef::Def(d)), _, _)) => *d,
            _ => panic!("prelude is missing `{n}`"),
        };
        self.wk.option = get(self, "Option");
        self.wk.result = get(self, "Result");
        self.wk.error = get(self, "Error");
        self.wk.task = get(self, "Task");
        self.wk.channel = get(self, "Channel");
    }

    fn module_summaries(&self) -> Vec<ModSummary> {
        let mut out = Vec::new();
        for (mi, m) in self.mods.iter().enumerate() {
            let mut lines = Vec::new();
            if let Some(d) = &m.doc {
                lines.push(d.lines().next().unwrap_or("").to_string());
                lines.push(String::new());
            }
            let mut names: Vec<(&String, &(Def, bool, Span))> = m.scope.iter().filter(|(_, (_, p, _))| *p).collect();
            names.sort_by_key(|(_, (_, _, s))| s.lo);
            for (name, (def, _, _)) in names {
                match def {
                    Def::Fn(f) if self.fns[*f as usize].module == mi as ModId => {
                        let fi = &self.fns[*f as usize];
                        lines.push(self.fn_signature(*f));
                        if let Some(d) = &fi.doc {
                            lines.push(format!("    {}", d.lines().next().unwrap_or("")));
                        }
                    }
                    Def::Type(TyRef::Def(d)) if self.types[*d as usize].module == mi as ModId => {
                        let td = &self.types[*d as usize];
                        let kw = match td.kind {
                            TypeDefKind::Struct { .. } => "struct",
                            TypeDefKind::Enum { .. } => "enum",
                            TypeDefKind::Opaque => "type",
                        };
                        lines.push(format!("{kw} {name}"));
                        if let Some(d) = &td.doc {
                            lines.push(format!("    {}", d.lines().next().unwrap_or("")));
                        }
                        // Methods of the type.
                        let mut ms: Vec<(&(Ctor, String), &Vec<FnId>)> =
                            self.methods.iter().filter(|((c, _), _)| *c == Ctor::Def(*d)).collect();
                        ms.sort_by_key(|((_, n), _)| n.clone());
                        for (_, fids) in ms {
                            for f in fids {
                                lines.push(format!("    {}", self.fn_signature(*f)));
                            }
                        }
                    }
                    Def::Global(g) if self.globals[*g as usize].module == mi as ModId => {
                        let gi = &self.globals[*g as usize];
                        lines.push(format!("let {name}: {}", self.show(&gi.ty)));
                    }
                    Def::Trait(t) if self.traits[*t as usize].module == mi as ModId => {
                        let td = &self.traits[*t as usize];
                        lines.push(format!("trait {name}"));
                        if let Some(d) = &td.doc {
                            lines.push(format!("    {}", d.lines().next().unwrap_or("")));
                        }
                        for m in &td.methods {
                            lines.push(format!("    {}", self.fn_signature(m.sig)));
                        }
                    }
                    _ => {}
                }
            }
            out.push(ModSummary { name: m.name.clone(), kind: m.kind, summary: lines.join("\n") });
        }
        out
    }

    pub fn fn_signature(&self, f: FnId) -> String {
        let fi = &self.fns[f as usize];
        let params: Vec<(String, Type, bool)> = fi.params.iter().map(|p| (p.name.clone(), self.zonk(&p.ty), p.default.is_some())).collect();
        self.printer().signature(&fi.name, &params, &self.zonk(&fi.ret))
    }

    // ------------------------------------------------------------------
    // Modules

    fn define(&mut self, name: &str, def: Def, is_pub: bool, span: Span) {
        let m = self.cur_mod as usize;
        if let Some((existing, _, existing_span)) = self.mods[m].scope.get(name) {
            // Re-binding a global with `let` shadows, as does any definition of
            // a later session input; anything else is a duplicate.
            let shadowable = matches!((existing, &def), (Def::Global(_), Def::Global(_))) || self.defined_by_earlier_input(*existing_span);
            if !shadowable {
                self.error(span, format!("`{name}` is already defined"));
                return;
            }
        }
        self.mods[m].scope.insert(name.to_string(), (def, is_pub, span));
    }

    fn check_module(&mut self, pm: &ParsedModule) {
        let mi = self.cur_mod;
        let first_global = self.globals.len();
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
                self.define(&t.name.name, Def::Trait(id), t.is_pub, t.name.span);
                trait_ids.push(id);
            }
        }
        let mut struct_ids = Vec::new();
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let generics = self.declare_generics(&s.generics);
                    let id = self.new_typedef(
                        &s.name.name,
                        generics,
                        TypeDefKind::Struct { fields: vec![] },
                        s.is_pub,
                        s.doc.clone(),
                        s.name.span,
                    );
                    self.define(&s.name.name, Def::Type(TyRef::Def(id)), s.is_pub, s.name.span);
                    struct_ids.push(id);
                }
                ItemKind::Enum(e) => {
                    let generics = self.declare_generics(&e.generics);
                    let id = self.new_typedef(
                        &e.name.name,
                        generics,
                        TypeDefKind::Enum { variants: vec![] },
                        e.is_pub,
                        e.doc.clone(),
                        e.name.span,
                    );
                    self.define(&e.name.name, Def::Type(TyRef::Def(id)), e.is_pub, e.name.span);
                    struct_ids.push(id);
                }
                ItemKind::NativeType(n) => {
                    if !self.allows_native() {
                        self.error(n.name.span, "native types can only be declared by native modules");
                    }
                    let generics = self.declare_generics(&n.generics);
                    let id = self.new_typedef(&n.name.name, generics, TypeDefKind::Opaque, n.is_pub, n.doc.clone(), n.name.span);
                    self.define(&n.name.name, Def::Type(TyRef::Def(id)), n.is_pub, n.name.span);
                }
                ItemKind::TypeAlias(a) => {
                    let generics = self.declare_generics(&a.generics);
                    self.aliases.push(Alias {
                        name: a.name.name.clone(),
                        generics,
                        ty: None,
                        decl: Some((mi, a.ty.clone())),
                        resolving: false,
                    });
                    let id = self.aliases.len() - 1;
                    self.define(&a.name.name, Def::Type(TyRef::Alias(id)), a.is_pub, a.name.span);
                }
                _ => {}
            }
        }
        if pm.kind == ModKind::Prelude {
            let scope = &self.mods[mi as usize].scope;
            if let Some((Def::Trait(t), _, _)) = scope.get("Iterator") {
                self.iter_trait = Some(*t);
            }
            if let Some((Def::Type(TyRef::Def(d)), _, _)) = scope.get("Iter") {
                self.iter_struct = Some(*d);
            }
        }
        // Resolve aliases eagerly so errors are reported once.
        for i in 0..self.aliases.len() {
            if self.aliases[i].ty.is_none() && self.aliases[i].decl.as_ref().map(|d| d.0) == Some(mi) {
                self.resolve_alias(i, Span::default());
            }
        }
        // Supertraits, then trait method signatures.
        let trait_decls: Vec<&ast::TraitDecl> =
            items.iter().filter_map(|i| if let ItemKind::Trait(t) = &i.kind { Some(t) } else { None }).collect();
        self.resolve_supertraits(&trait_ids, &trait_decls);
        // 3. Fields and variants.
        let mut k = 0;
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let id = struct_ids[k];
                    k += 1;
                    self.fill_struct(id, s);
                }
                ItemKind::Enum(e) => {
                    let id = struct_ids[k];
                    k += 1;
                    self.fill_enum(id, e);
                }
                _ => {}
            }
        }
        if pm.kind == ModKind::Prelude {
            for name in ["Option", "Result"] {
                if let Some((Def::Type(TyRef::Def(d)), _, _)) = self.mods[mi as usize].scope.get(name).cloned() {
                    let vs: Vec<(String, Span)> =
                        self.types[d as usize].variants().iter().map(|v| (v.name.clone(), self.types[d as usize].span)).collect();
                    for (i, (vn, sp)) in vs.into_iter().enumerate() {
                        self.define(&vn, Def::Variant(d, i as u32), true, sp);
                    }
                }
            }
        }
        for (&id, t) in trait_ids.iter().zip(&trait_decls) {
            self.declare_trait_methods(id, t);
        }
        // 4. Functions and methods.
        let mut fn_items: Vec<FnId> = Vec::new();
        let mut k = 0;
        for item in items {
            match &item.kind {
                ItemKind::Fn(f) => {
                    let id = self.declare_fn(f, None, &[], None);
                    self.define(&f.name.name, Def::Fn(id), f.is_pub, f.name.span);
                    fn_items.push(id);
                }
                ItemKind::Struct(s) => {
                    let id = struct_ids[k];
                    k += 1;
                    for m in &s.methods {
                        let f = self.declare_method(m, Ctor::Def(id), id);
                        fn_items.push(f);
                    }
                }
                ItemKind::Enum(e) => {
                    let id = struct_ids[k];
                    k += 1;
                    for m in &e.methods {
                        let f = self.declare_method(m, Ctor::Def(id), id);
                        fn_items.push(f);
                    }
                }
                ItemKind::Extend(x) => {
                    for f in self.declare_extend(x) {
                        fn_items.push(f);
                    }
                }
                _ => {}
            }
        }
        // Impls, then the supertrait requirements of each.
        let mut impl_ids = Vec::new();
        for item in items {
            if let ItemKind::Impl(i) = &item.kind
                && let Some(id) = self.declare_impl(i)
            {
                impl_ids.push(id);
            }
        }
        for id in impl_ids {
            self.check_impl_supertraits(id);
        }
        // 5. Top-level code.
        let init = self.add_fn(FnInfo {
            name: format!("<{}>", self.mods[mi as usize].name),
            qual_name: format!("{}.<init>", self.mods[mi as usize].name),
            module: mi,
            generics: vec![],
            params: vec![],
            ret: Type::Unit,
            has_self: false,
            kind: FnKind::Init,
            doc: None,
            span: pm.ast.span,
            is_pub: false,
            owner: None,
            self_ty: None,
            decl: Some(FnDeclRef::Init(mi)),
            dicts: vec![],
            impl_of: None,
            trait_of: None,
        });
        self.mods[mi as usize].init = Some(init);
        self.check_init(init, pm);
        // 6. Function bodies (including default helpers, which may be added while checking).
        let mut i = 0;
        while i < self.fns.len() {
            if self.fns[i].module == mi
                && self.bodies[i].is_none()
                && !matches!(self.fns[i].kind, FnKind::Native(_) | FnKind::Init)
                && let Some(decl) = self.fns[i].decl.clone()
            {
                self.check_fn_body(i as FnId, &decl);
            }
            i += 1;
        }
        // 7. Tests.
        if pm.kind == (ModKind::Script { main: true }) {
            for item in items {
                if let ItemKind::Test(t) = &item.kind {
                    let id = self.add_fn(FnInfo {
                        name: t.name.clone(),
                        qual_name: format!("test {}", t.name),
                        module: mi,
                        generics: vec![],
                        params: vec![],
                        ret: Type::Unit,
                        has_self: false,
                        kind: FnKind::Test(t.name.clone()),
                        doc: None,
                        span: t.name_span,
                        is_pub: false,
                        owner: None,
                        self_ty: None,
                        decl: Some(FnDeclRef::Test(mi, t.body.clone())),
                        dicts: vec![],
                        impl_of: None,
                        trait_of: None,
                    });
                    let decl = self.fns[id as usize].decl.clone().unwrap();
                    self.check_fn_body(id, &decl);
                    self.tests.push((t.name.clone(), id));
                }
            }
        } else {
            // Tests in imported modules are still type checked.
            for item in items {
                if let ItemKind::Test(t) = &item.kind {
                    let id = self.add_fn(FnInfo {
                        name: t.name.clone(),
                        qual_name: format!("test {}", t.name),
                        module: mi,
                        generics: vec![],
                        params: vec![],
                        ret: Type::Unit,
                        has_self: false,
                        kind: FnKind::Test(t.name.clone()),
                        doc: None,
                        span: t.name_span,
                        is_pub: false,
                        owner: None,
                        self_ty: None,
                        decl: Some(FnDeclRef::Test(mi, t.body.clone())),
                        dicts: vec![],
                        impl_of: None,
                        trait_of: None,
                    });
                    let decl = self.fns[id as usize].decl.clone().unwrap();
                    self.check_fn_body(id, &decl);
                }
            }
        }
        // 8. Globals must have fully inferred types.
        for g in first_global..self.globals.len() {
            if self.globals[g].module == mi {
                let t = self.zonk(&self.globals[g].ty);
                if t.has_vars() {
                    let span = self.globals[g].span;
                    self.error(span, format!("cannot infer type of `{}`; add a type annotation", self.globals[g].name));
                    self.globals[g].ty = Type::Error;
                } else {
                    self.globals[g].ty = t;
                }
                let shown = self.show(&self.globals[g].ty);
                if let Some((_, idx)) = self.global_facts.iter().find(|(x, _)| *x as usize == g) {
                    let idx = *idx;
                    self.lint.locals[idx].ty = shown;
                }
            }
        }
        let _ = fn_items;
    }

    fn allows_native(&self) -> bool {
        !matches!(self.mods[self.cur_mod as usize].kind, ModKind::Script { .. })
    }

    fn bind_import(&mut self, imp: &ast::Import, target: ModId, span: Span) {
        match &imp.names {
            None => {
                let name = match (&imp.alias, &imp.source) {
                    (Some(a), _) => a.name.clone(),
                    (None, ast::ImportSource::Name(n)) => n.name.clone(),
                    (None, ast::ImportSource::Path(_, _)) => self.mods[target as usize].name.clone(),
                };
                let nspan = imp.alias.as_ref().map(|a| a.span).unwrap_or(match &imp.source {
                    ast::ImportSource::Name(n) => n.span,
                    ast::ImportSource::Path(_, s) => *s,
                });
                self.define(&name, Def::Module(target), false, nspan);
                self.mods[self.cur_mod as usize].import_spans.push((name, span));
            }
            Some(names) => {
                for n in names {
                    let found = self.mods[target as usize].scope.get(&n.name).cloned();
                    match found {
                        Some((def, true, _)) => {
                            self.define(&n.name, def, false, n.span);
                            self.mods[self.cur_mod as usize].import_spans.push((n.name.clone(), n.span));
                        }
                        Some((_, false, _)) => self.error(n.span, format!("`{}` is private", n.name)),
                        None => {
                            let m = self.mods[target as usize].name.clone();
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
    fn declare_params(&mut self, gs: &[ast::GenericParam], outer: &[(String, ParamId)], self_ty: Option<&Type>) -> Vec<ParamId> {
        let mut scope = outer.to_vec();
        let mut out = Vec::new();
        for g in gs {
            let p = self.new_param(&g.name.name, vec![], g.reflect);
            scope.push((g.name.name.clone(), p));
            out.push(p);
        }
        for (g, &p) in gs.iter().zip(&out) {
            let (bounds, traits) = self.resolve_bounds(&g.bounds, &scope, self_ty);
            self.add_bounds(p, bounds, traits);
        }
        out
    }

    fn new_typedef(
        &mut self,
        name: &str,
        generics: Vec<ParamId>,
        kind: TypeDefKind,
        is_pub: bool,
        doc: Option<String>,
        span: Span,
    ) -> TypeDefId {
        let module_name = self.mods[self.cur_mod as usize].name.clone();
        self.types.push(TypeDef { name: name.to_string(), module: self.cur_mod, module_name, generics, kind, is_pub, doc, span });
        (self.types.len() - 1) as TypeDefId
    }

    pub fn add_fn(&mut self, f: FnInfo) -> FnId {
        self.fns.push(f);
        self.bodies.push(None);
        (self.fns.len() - 1) as FnId
    }

    fn generic_scope_of(&self, def: TypeDefId) -> Vec<(String, ParamId)> {
        self.types[def as usize].generics.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect()
    }

    fn fill_struct(&mut self, id: TypeDefId, s: &ast::StructDecl) {
        let scope = self.generic_scope_of(id);
        let self_ty = Type::Named(id, self.types[id as usize].generics.iter().map(|&p| Type::Param(p)).collect());
        let mut fields: Vec<FieldDef> = Vec::new();
        for f in &s.fields {
            if fields.iter().any(|x| x.name == f.name.name) {
                self.error(f.name.span, format!("field `{}` is defined twice", f.name.name));
                continue;
            }
            let ty = self.resolve_type_in(&f.ty, &scope, Some(&self_ty));
            fields.push(FieldDef {
                name: f.name.name.clone(),
                ty,
                mutable: f.mutable,
                default: None,
                const_default: None,
                doc: f.doc.clone(),
            });
        }
        // Default helpers.
        for (i, f) in s.fields.iter().enumerate() {
            if let Some(d) = &f.default {
                if i >= fields.len() {
                    break;
                }
                let const_default = const_of(d);
                let helper = self.add_fn(FnInfo {
                    name: format!("{}.{}", s.name.name, f.name.name),
                    qual_name: format!("{}.{}.default", s.name.name, f.name.name),
                    module: self.cur_mod,
                    generics: self.types[id as usize].generics.clone(),
                    params: vec![],
                    ret: fields[i].ty.clone(),
                    has_self: false,
                    kind: FnKind::DefaultHelper,
                    doc: None,
                    span: d.span,
                    is_pub: false,
                    owner: None,
                    self_ty: Some(self_ty.clone()),
                    decl: Some(FnDeclRef::Default(self.cur_mod, d.clone())),
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
        let scope = self.generic_scope_of(id);
        let self_ty = Type::Named(id, self.types[id as usize].generics.iter().map(|&p| Type::Param(p)).collect());
        let mut variants: Vec<VariantDef> = Vec::new();
        for (i, v) in e.variants.iter().enumerate() {
            if variants.iter().any(|x| x.name == v.name.name) {
                self.error(v.name.span, format!("variant `{}` is defined twice", v.name.name));
                continue;
            }
            let fields: Vec<Type> = v.fields.iter().map(|f| self.resolve_type_in(&f.ty, &scope, Some(&self_ty))).collect();
            let names: Vec<Option<String>> = v.fields.iter().map(|f| f.name.as_ref().map(|n| n.name.clone())).collect();
            let field_names = if !names.is_empty() && names.iter().all(|n| n.is_some()) {
                Some(names.into_iter().map(|n| n.unwrap()).collect())
            } else {
                None
            };
            self.variants.push((id, i as u32));
            let rt_id = (self.variants.len() - 1) as u32;
            variants.push(VariantDef { name: v.name.name.clone(), fields, field_names, rt_id, doc: v.doc.clone() });
        }
        // Keep runtime index consistent with position in `variants`.
        for (i, v) in variants.iter().enumerate() {
            self.variants[v.rt_id as usize] = (id, i as u32);
        }
        self.types[id as usize].kind = TypeDefKind::Enum { variants };
    }

    fn resolve_alias(&mut self, i: usize, span: Span) -> Type {
        if let Some(t) = &self.aliases[i].ty {
            return t.clone();
        }
        if self.aliases[i].resolving {
            self.error(span, format!("type alias `{}` is recursive", self.aliases[i].name));
            return Type::Error;
        }
        self.aliases[i].resolving = true;
        let (m, te) = self.aliases[i].decl.clone().unwrap();
        let saved = self.cur_mod;
        self.cur_mod = m;
        let scope: Vec<(String, ParamId)> = self.aliases[i].generics.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect();
        let mut t = self.resolve_type_in(&te, &scope, None);
        self.cur_mod = saved;
        self.aliases[i].resolving = false;
        self.limit_type_size(&mut t, te.span);
        self.aliases[i].ty = Some(t.clone());
        t
    }

    /// Looks up a name in the current module scope, then the prelude.
    pub fn lookup_global(&mut self, name: &str) -> Option<Def> {
        let m = self.cur_mod as usize;
        if let Some((d, _, _)) = self.mods[m].scope.get(name) {
            let d = d.clone();
            self.lint.used_names.insert((self.cur_mod, name.to_string()));
            return Some(d);
        }
        if name == "task"
            && let Some(t) = self.task_mod
            && t != self.cur_mod
        {
            return Some(Def::Module(t));
        }
        if let Some(p) = self.prelude
            && p != self.cur_mod
            && let Some((d, true, _)) = self.mods[p as usize].scope.get(name)
        {
            return Some(d.clone());
        }
        builtin_type(name).map(Def::Type)
    }

    /// Looks up a public member of a module.
    pub fn module_member(&mut self, m: ModId, name: &str, span: Span) -> Option<Def> {
        match self.mods[m as usize].scope.get(name) {
            Some((d, true, _)) => Some(d.clone()),
            Some((_, false, _)) => {
                self.error(span, format!("`{name}` is private"));
                None
            }
            None => {
                let mn = self.mods[m as usize].name.clone();
                self.error(span, format!("module `{mn}` has no public item `{name}`"));
                None
            }
        }
    }

    // ------------------------------------------------------------------
    // Types from syntax

    pub fn resolve_type_in(&mut self, te: &TypeExpr, generics: &[(String, ParamId)], self_ty: Option<&Type>) -> Type {
        match &te.kind {
            TypeExprKind::Unit => Type::Unit,
            TypeExprKind::List(t) => Type::list(self.resolve_type_in(t, generics, self_ty)),
            TypeExprKind::Map(k, v) => Type::map(self.resolve_type_in(k, generics, self_ty), self.resolve_type_in(v, generics, self_ty)),
            TypeExprKind::Tuple(ts) => Type::Tuple(ts.iter().map(|t| self.resolve_type_in(t, generics, self_ty)).collect()),
            TypeExprKind::Option(t) => {
                let inner = self.resolve_type_in(t, generics, self_ty);
                self.option_of(inner)
            }
            TypeExprKind::Fn(ps, r) => {
                let ps = ps.iter().map(|t| self.resolve_type_in(t, generics, self_ty)).collect();
                let r = r.as_ref().map(|r| self.resolve_type_in(r, generics, self_ty)).unwrap_or(Type::Unit);
                Type::func(ps, r)
            }
            TypeExprKind::Named { path, args } => {
                let args: Vec<Type> = args.iter().map(|t| self.resolve_type_in(t, generics, self_ty)).collect();
                let first = &path[0];
                if path.len() == 1 {
                    if first.name == "Self" {
                        return match self_ty {
                            Some(t) => t.clone(),
                            None => {
                                self.error(first.span, "`Self` is only valid inside a type's methods");
                                Type::Error
                            }
                        };
                    }
                    if first.name == "never" && args.is_empty() {
                        return Type::Never;
                    }
                    if let Some((_, p)) = generics.iter().rev().find(|(n, _)| *n == first.name) {
                        if !args.is_empty() {
                            self.error(te.span, "type parameters do not take type arguments");
                        }
                        return Type::Param(*p);
                    }
                    // A built-in type name shadowed by a value (e.g. `random.int`)
                    // still names the built-in type in type position.
                    let def = match self.lookup_global(&first.name) {
                        Some(Def::Type(r)) => Some(r),
                        Some(Def::Trait(_)) => {
                            self.trait_as_type_error(first);
                            return Type::Error;
                        }
                        _ => builtin_type(&first.name),
                    };
                    match def {
                        Some(r) => self.instantiate_tyref(r, args, te.span, &first.name),
                        None => {
                            self.error(first.span, format!("unknown type `{}`", first.name));
                            Type::Error
                        }
                    }
                } else if path.len() == 2 {
                    match self.lookup_global(&first.name) {
                        Some(Def::Module(m)) => match self.module_member(m, &path[1].name, path[1].span) {
                            Some(Def::Type(r)) => self.instantiate_tyref(r, args, te.span, &path[1].name),
                            Some(Def::Trait(_)) => {
                                self.trait_as_type_error(&path[1]);
                                Type::Error
                            }
                            Some(_) => {
                                self.error(path[1].span, format!("`{}` is not a type", path[1].name));
                                Type::Error
                            }
                            None => Type::Error,
                        },
                        _ => {
                            self.error(
                                first.span,
                                format!("unknown type `{}`", path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".")),
                            );
                            Type::Error
                        }
                    }
                } else {
                    self.error(te.span, "unknown type");
                    Type::Error
                }
            }
        }
    }

    fn instantiate_tyref(&mut self, r: TyRef, mut args: Vec<Type>, span: Span, name: &str) -> Type {
        match r {
            TyRef::Prim(t) => {
                if !args.is_empty() {
                    self.error(span, format!("`{name}` does not take type arguments"));
                }
                t
            }
            TyRef::Set => {
                if args.len() != 1 {
                    self.error(span, format!("expected 1 type argument for `Set`, found {}", args.len()));
                    return Type::Error;
                }
                Type::Set(Box::new(args.pop().unwrap()))
            }
            TyRef::Def(d) => {
                let n = self.types[d as usize].generics.len();
                if d == self.wk.result && args.len() == 1 && self.prelude.is_some() {
                    args.push(self.error_ty());
                }
                if args.len() != n {
                    let plural = if n == 1 { "" } else { "s" };
                    self.error(span, format!("expected {n} type argument{plural} for `{name}`, found {}", args.len()));
                    return Type::Error;
                }
                Type::Named(d, args)
            }
            TyRef::Alias(i) => {
                let body = self.resolve_alias(i, span);
                let gens = self.aliases[i].generics.clone();
                if args.len() != gens.len() {
                    self.error(span, format!("expected {} type arguments for `{name}`, found {}", gens.len(), args.len()));
                    return Type::Error;
                }
                body.subst_params(&|p| gens.iter().position(|&g| g == p).map(|i| args[i].clone()))
            }
        }
    }

    // ------------------------------------------------------------------
    // Function declarations

    /// Declares a function. `outer` holds enclosing generic params (for methods),
    /// `self_ty` the receiver type.
    fn declare_fn(&mut self, f: &ast::FnDecl, owner: Option<Ctor>, outer: &[(String, ParamId)], self_ty: Option<Type>) -> FnId {
        if f.is_native && !self.allows_native() {
            self.error(f.name.span, "native functions can only be declared by native modules");
        }
        let mut scope: Vec<(String, ParamId)> = outer.to_vec();
        let mut generics: Vec<ParamId> = outer.iter().map(|(_, p)| *p).collect();
        for p in self.declare_params(&f.generics, outer, self_ty.as_ref()) {
            scope.push((self.params[p as usize].name.clone(), p));
            generics.push(p);
        }
        self.apply_where(&f.where_clause, &scope, self_ty.as_ref());
        let mut params = Vec::new();
        let mut seen = HashSet::new();
        for (i, p) in f.params.iter().enumerate() {
            if !seen.insert(p.name.name.clone()) {
                self.error(p.name.span, format!("parameter `{}` is declared twice", p.name.name));
            }
            let ty = match &p.ty {
                Some(t) => self.resolve_type_in(t, &scope, self_ty.as_ref()),
                None => {
                    if i == 0 && self_ty.is_some() {
                        self_ty.clone().unwrap()
                    } else {
                        self.error(p.name.span, "`self` is only allowed as the first parameter of a method");
                        Type::Error
                    }
                }
            };
            params.push(ParamSig { name: p.name.name.clone(), ty, default: None });
        }
        let ret = match &f.ret {
            Some(t) => self.resolve_type_in(t, &scope, self_ty.as_ref()),
            None => Type::Unit,
        };
        let module_name = self.mods[self.cur_mod as usize].name.clone();
        let type_prefix = owner.map(|c| c.native_prefix(&self.types));
        let qual_name = match (&type_prefix, self.mods[self.cur_mod as usize].kind) {
            (Some(t), ModKind::Prelude) => format!("{t}.{}", f.name.name),
            (Some(t), _) => format!("{module_name}.{t}.{}", f.name.name),
            (None, ModKind::Prelude) => f.name.name.clone(),
            (None, _) => format!("{module_name}.{}", f.name.name),
        };
        let kind = if f.is_native {
            match (self.natives)(&qual_name) {
                Some(n) => FnKind::Native(n),
                None => {
                    self.error(f.name.span, format!("no native implementation registered for `{qual_name}`"));
                    FnKind::Native(u32::MAX)
                }
            }
        } else {
            FnKind::Script
        };
        // One hidden dictionary per (type parameter, trait bound).
        let dicts: Vec<(Type, TraitRef)> =
            generics.iter().flat_map(|&p| self.params[p as usize].traits.iter().map(move |tr| (Type::Param(p), tr.clone()))).collect();
        if f.is_native && !dicts.is_empty() {
            self.error(f.name.span, "native functions cannot have trait bounds");
        }
        let id = self.add_fn(FnInfo {
            name: f.name.name.clone(),
            qual_name,
            module: self.cur_mod,
            generics: generics.clone(),
            params,
            ret,
            has_self: f.has_self() && self_ty.is_some(),
            kind,
            doc: f.doc.clone(),
            span: f.name.span,
            is_pub: f.is_pub,
            owner,
            self_ty: self_ty.clone(),
            decl: if f.is_native { None } else { Some(FnDeclRef::Fn(self.cur_mod, f.clone())) },
            dicts: dicts.clone(),
            impl_of: None,
            trait_of: None,
        });
        // Default argument helpers.
        for (i, p) in f.params.iter().enumerate() {
            if let Some(d) = &p.default {
                let default = if is_closed_const(d) {
                    DefaultArg::Inline(d.clone())
                } else {
                    let helper_params: Vec<ParamSig> = self.fns[id as usize].params[..i].to_vec();
                    let ret = self.fns[id as usize].params[i].ty.clone();
                    let helper = self.add_fn(FnInfo {
                        name: format!("{}.{}", f.name.name, p.name.name),
                        qual_name: format!("{}.default.{}", self.fns[id as usize].qual_name, p.name.name),
                        module: self.cur_mod,
                        generics: generics.clone(),
                        params: helper_params,
                        ret,
                        has_self: false,
                        kind: FnKind::DefaultHelper,
                        doc: None,
                        span: d.span,
                        is_pub: false,
                        owner: None,
                        self_ty: self_ty.clone(),
                        decl: Some(FnDeclRef::Default(self.cur_mod, d.clone())),
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

    fn declare_method(&mut self, m: &ast::FnDecl, ctor: Ctor, def: TypeDefId) -> FnId {
        // Each method gets its own copy of the type's parameters so that
        // `where` clauses only affect that method.
        let orig = self.types[def as usize].generics.clone();
        let mut outer = Vec::new();
        for p in &orig {
            let info = self.params[*p as usize].clone();
            let np = self.new_param(&info.name, vec![], false);
            self.add_bounds(np, info.bounds, vec![]);
            outer.push((info.name, np));
        }
        let self_ty = Type::Named(def, outer.iter().map(|(_, p)| Type::Param(*p)).collect());
        let id = self.declare_fn(m, Some(ctor), &outer, Some(self_ty));
        self.register_method(ctor, &m.name, id);
        id
    }

    fn register_method(&mut self, ctor: Ctor, name: &ast::Ident, id: FnId) {
        let has_self = self.fns[id as usize].has_self;
        let module = self.fns[id as usize].module;
        if self.in_session() {
            // A method defined again by a later session input replaces the earlier one.
            let fns = &self.fns;
            let cur_file = self.mods[module as usize].file;
            if let Some(ids) = self.methods.get_mut(&(ctor, name.name.clone())) {
                ids.retain(|&f| {
                    let other = &fns[f as usize];
                    !(other.has_self == has_self && other.module == module && other.span.file != cur_file)
                });
            }
        }
        let existing = self.methods.get(&(ctor, name.name.clone())).cloned().unwrap_or_default();
        let dup = existing.iter().any(|&f| {
            let other = &self.fns[f as usize];
            other.has_self == has_self && other.module == module
        });
        if dup {
            self.error(name.span, format!("method `{}` is already defined", name.name));
        }
        self.methods.entry((ctor, name.name.clone())).or_default().push(id);
    }

    fn declare_extend(&mut self, x: &ast::ExtendDecl) -> Vec<FnId> {
        let mut out = Vec::new();
        // Resolve the target once to find its constructor.
        // (This also reports invalid bounds once, even if there are no methods.)
        let probe = self.declare_params(&x.generics, &[], None);
        let probe_scope: Vec<(String, ParamId)> = probe.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect();
        let target = self.resolve_type_in(&x.target, &probe_scope, None);
        let ctor = match ctor_of(&target) {
            Some(c) => c,
            None => {
                if target != Type::Error {
                    self.error(x.target.span, "cannot extend this type");
                }
                return out;
            }
        };
        let builtin = !matches!(ctor, Ctor::Def(_));
        let foreign = match ctor {
            Ctor::Def(d) => self.types[d as usize].module != self.cur_mod,
            _ => true,
        };
        if foreign && !self.allows_native() {
            let what = if builtin { "built-in types" } else { "types defined in other modules" };
            self.error(x.target.span, format!("cannot extend {what}"));
            return out;
        }
        for m in &x.methods {
            // Fresh parameters per method so that `where` clauses only affect that method.
            let params = self.declare_params(&x.generics, &[], None);
            let scope: Vec<(String, ParamId)> = params.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect();
            let self_ty = self.resolve_type_in(&x.target, &scope, None);
            let id = self.declare_fn(m, Some(ctor), &scope, Some(self_ty));
            self.register_method(ctor, &m.name, id);
            out.push(id);
        }
        out
    }

    // ------------------------------------------------------------------
    // Bodies

    fn begin_fn(&mut self, generics: Vec<(String, ParamId)>, self_ty: Option<Type>) {
        self.fs = FnState::default();
        self.fs_unused.clear();
        self.loop_breaks.clear();
        self.fs.generics = generics;
        self.fs.self_ty = self_ty;
        self.fs.scopes.push();
    }

    fn check_fn_body(&mut self, id: FnId, decl: &FnDeclRef) {
        let fi = self.fns[id as usize].clone();
        let generics: Vec<(String, ParamId)> = fi.generics.iter().map(|&p| (self.params[p as usize].name.clone(), p)).collect();
        self.begin_fn(generics, fi.self_ty.clone());
        let saved_mod = self.cur_mod;
        let (m, span) = match decl {
            FnDeclRef::Fn(m, f) => (*m, f.span),
            FnDeclRef::Default(m, e) => (*m, e.span),
            FnDeclRef::Init(m) => (*m, fi.span),
            FnDeclRef::Test(m, b) => (*m, b.span),
        };
        self.cur_mod = m;
        let mut param_locals = self.declare_dict_params(&fi);
        let decl_params: Vec<(String, Span)> = match decl {
            FnDeclRef::Fn(_, f) => f.params.iter().map(|p| (p.name.name.clone(), p.name.span)).collect(),
            _ => fi.params.iter().map(|p| (p.name.clone(), fi.span)).collect(),
        };
        for (p, (name, pspan)) in fi.params.iter().zip(decl_params) {
            let l = self.declare_local(&name, p.ty.clone(), false, pspan, true);
            // Parameters of trait and impl methods are dictated by the trait;
            // those of default helpers are synthetic.
            if fi.impl_of.is_some() || fi.trait_of.is_some() || matches!(decl, FnDeclRef::Default(..)) {
                self.fs.data[l as usize].used = true;
            }
            param_locals.push(l);
        }
        self.fs.ctx.push(FnCtx { ret: fi.ret.clone(), kind: CtxKind::Function, loops: vec![], lambda: 0, in_defer: None });
        let body = match decl {
            FnDeclRef::Fn(_, f) => {
                let b = f.body.as_ref().unwrap();
                let want = fi.ret != Type::Unit;
                let h = self.check_block(b, if want { Some(&fi.ret) } else { None }, want);
                if want {
                    self.expect_type(&fi.ret, &h.ty, self.block_tail_span(b));
                }
                h
            }
            FnDeclRef::Default(_, e) => self.check_expr(e, &fi.ret),
            FnDeclRef::Test(_, b) => self.check_block(b, None, false),
            FnDeclRef::Init(_) => unreachable!(),
        };
        let hfn = self.finish_fn(fi.name.clone(), param_locals, body, span);
        self.bodies[id as usize] = Some(hfn);
        self.cur_mod = saved_mod;
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
        self.fs.ctx.push(FnCtx { ret: Type::Unit, kind: CtxKind::TopLevel { main }, loops: vec![], lambda: 0, in_defer: None });
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
        let mut ty = Type::Unit;
        for (i, item) in pm.ast.items.iter().enumerate() {
            if let ItemKind::Stmt(s) = &item.kind {
                self.fs.top_level = true;
                match value {
                    Some(e) if i + 1 == pm.ast.items.len() => {
                        let h = self.infer(e, None);
                        ty = h.ty.clone();
                        stmts.push(h);
                    }
                    _ => stmts.extend(self.check_stmt(s, false)),
                }
                self.fs.top_level = false;
            }
        }
        let body = HExpr { kind: HKind::Block(stmts), ty, span: pm.ast.span };
        let name = self.fns[id as usize].name.clone();
        let hfn = self.finish_fn(name, vec![], body, pm.ast.span);
        if value.is_some() {
            self.cell_value = Some(hfn.body.ty.clone());
        }
        self.bodies[id as usize] = Some(hfn);
    }

    /// Final checks for a function: bounds, inference completeness; zonks the body.
    fn finish_fn(&mut self, name: String, params: Vec<LocalId>, mut body: HExpr, span: Span) -> HFn {
        // Trait evidence first: resolving it may determine types.
        self.resolve_holes(&mut body);
        let obligations = std::mem::take(&mut self.fs.obligations);
        if let Some(q) = &mut self.type_query {
            q.bounds.extend(obligations.iter().map(|(t, b, _)| (t.clone(), *b)));
        }
        for (t, b, sp) in obligations {
            self.check_bound(&t, b, sp, true);
        }
        let checks = std::mem::take(&mut self.fs.infer_checks);
        for (t, sp) in checks {
            if self.zonk(&t).has_vars() {
                self.error(sp, "cannot infer type; add a type annotation");
            }
        }
        let reflect = std::mem::take(&mut self.fs.reflect_checks);
        for (t, sp) in reflect {
            let z = self.zonk(&t);
            let mut bad = z.has_vars();
            z.walk(&mut |t| {
                if matches!(t, Type::Param(_)) {
                    bad = true
                }
            });
            if bad {
                self.error(sp, "cannot infer type for this call; add a type annotation");
            }
        }
        self.zonk_expr(&mut body);
        for (t, sp) in std::mem::take(&mut self.fs_unused) {
            if let Type::Named(d, _) = self.zonk(&t)
                && d == self.wk.result
            {
                self.lint.unused_results.push(sp);
            }
        }
        // Lint facts for locals.
        self.lint_func += 1;
        for (i, d) in self.fs.data.iter().enumerate() {
            let info = &self.fs.locals[i];
            if info.name.starts_with('<') {
                continue;
            }
            self.lint.locals.push(LocalFact {
                span: d.decl_span,
                name: info.name.clone(),
                used: d.used,
                mutable: info.mutable,
                assigned: d.assigned,
                ty: TypePrinter { defs: &self.types, params: &self.params, wk: self.wk }.show(&zonk_with(&self.subst, &d.ty)),
                is_param: d.is_param,
                global: false,
                func: self.lint_func,
            });
        }
        let locals = std::mem::take(&mut self.fs.locals);
        HFn { name, params, body, locals, span }
    }

    // ------------------------------------------------------------------
    // Locals and scopes

    pub(crate) fn declare_local(&mut self, name: &str, ty: Type, mutable: bool, span: Span, is_param: bool) -> LocalId {
        let owner = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
        self.fs.locals.push(LocalInfo { name: name.to_string(), mutable, owner, cell: false, span });
        self.fs.data.push(LocalData { ty, scheme: vec![], used: false, assigned: false, is_param, decl_span: span });
        let id = (self.fs.locals.len() - 1) as LocalId;
        if name != "_" {
            self.fs.scopes.declare(name, id);
        }
        id
    }

    pub(crate) fn hidden_local(&mut self, ty: Type, span: Span) -> LocalId {
        let owner = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
        self.fs.locals.push(LocalInfo { name: "<tmp>".into(), mutable: true, owner, cell: false, span });
        self.fs.data.push(LocalData { ty, scheme: vec![], used: true, assigned: true, is_param: false, decl_span: span });
        (self.fs.locals.len() - 1) as LocalId
    }

    pub(crate) fn find_local(&self, name: &str) -> Option<LocalId> {
        self.fs.scopes.find(name)
    }

    pub(crate) fn push_scope(&mut self) {
        self.fs.scopes.push();
    }

    /// A new lambda id for the function being checked.
    pub(crate) fn new_lambda(&mut self) -> u32 {
        self.fs.next_lambda += 1;
        self.fs.lambda_first_local.push(self.fs.locals.len() as LocalId);
        self.fs.next_lambda
    }
    pub(crate) fn pop_scope(&mut self) {
        self.fs.scopes.pop();
    }

    /// Records a use of a local, marking captures.
    pub(crate) fn use_local(&mut self, id: LocalId) {
        self.fs.data[id as usize].used = true;
        let cur = self.fs.ctx.last().map(|c| c.lambda).unwrap_or(0);
        let info = &mut self.fs.locals[id as usize];
        if info.owner != cur && info.mutable {
            info.cell = true;
        }
    }
}

pub fn builtin_type(name: &str) -> Option<TyRef> {
    Some(match name {
        "int" => TyRef::Prim(Type::Int),
        "float" => TyRef::Prim(Type::Float),
        "bool" => TyRef::Prim(Type::Bool),
        "str" => TyRef::Prim(Type::Str),
        "bytes" => TyRef::Prim(Type::Bytes),
        "duration" => TyRef::Prim(Type::Duration),
        "Range" => TyRef::Prim(Type::Range),
        "Set" => TyRef::Set,
        _ => return None,
    })
}

pub fn ctor_of(t: &Type) -> Option<Ctor> {
    Some(match t {
        Type::Int => Ctor::Int,
        Type::Float => Ctor::Float,
        Type::Bool => Ctor::Bool,
        Type::Str => Ctor::Str,
        Type::Bytes => Ctor::Bytes,
        Type::Duration => Ctor::Duration,
        Type::Unit => Ctor::Unit,
        Type::Range => Ctor::Range,
        Type::List(_) => Ctor::List,
        Type::Map(..) => Ctor::Map,
        Type::Set(_) => Ctor::Set,
        Type::Tuple(_) => Ctor::Tuple,
        Type::Fn(..) => Ctor::Fn,
        Type::Named(d, _) => Ctor::Def(*d),
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

pub(crate) fn zonk_with(subst: &[Option<Type>], t: &Type) -> Type {
    zonk_budget(subst, t, &CompileBudget::default())
}

/// Resolves inference variables in `t`, charging `budget` one unit per node
/// produced; once the budget is exhausted the rest of the type is `Error`.
pub(crate) fn zonk_budget(subst: &[Option<Type>], t: &Type, budget: &CompileBudget) -> Type {
    if !budget.charge(1) {
        return Type::Error;
    }
    let z = |t: &Type| zonk_budget(subst, t, budget);
    match t {
        Type::Var(v) => match &subst[*v as usize] {
            Some(t) => z(t),
            None => t.clone(),
        },
        Type::List(t) => Type::List(Box::new(z(t))),
        Type::Set(t) => Type::Set(Box::new(z(t))),
        Type::Map(k, v) => Type::Map(Box::new(z(k)), Box::new(z(v))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(z).collect()),
        Type::Named(d, ts) => Type::Named(*d, ts.iter().map(z).collect()),
        Type::Fn(ps, r) => Type::Fn(ps.iter().map(z).collect(), Box::new(z(r))),
        other => other.clone(),
    }
}

/// Names referenced (as identifiers or struct-literal shorthands) inside
/// function bodies, methods, tests and default expressions of a module.
/// Top-level bindings not in this set can live in the module initialiser's
/// registers instead of global slots.
pub(crate) fn names_used_in_functions(m: &ast::Module) -> HashSet<String> {
    let mut out = HashSet::new();
    fn fn_decl(f: &ast::FnDecl, out: &mut HashSet<String>) {
        for p in &f.params {
            if let Some(d) = &p.default {
                expr(d, out);
            }
        }
        if let Some(b) = &f.body {
            block(b, out);
        }
    }
    fn block(b: &ast::Block, out: &mut HashSet<String>) {
        for s in &b.stmts {
            stmt(s, out);
        }
    }
    fn stmt(s: &ast::Stmt, out: &mut HashSet<String>) {
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
            ast::StmtKind::Expr(e) | ast::StmtKind::Defer(e) => expr(e, out),
            ast::StmtKind::Fn(f) => fn_decl(f, out),
        }
    }
    fn cond(c: &ast::Cond, out: &mut HashSet<String>) {
        match c {
            ast::Cond::Expr(e) => expr(e, out),
            ast::Cond::Let(_, e) => expr(e, out),
        }
    }
    fn expr(e: &ast::Expr, out: &mut HashSet<String>) {
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
    fn clauses(cs: &[ast::CompClause], out: &mut HashSet<String>) {
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
    pub bounds: Vec<(Type, Bound)>,
}

/// `t` with the inference variables in `map` replaced.
fn replace_vars(t: &Type, map: &HashMap<VarId, Type>) -> Type {
    let r = |t: &Type| replace_vars(t, map);
    match t {
        Type::Var(v) => map.get(v).cloned().unwrap_or(Type::Var(*v)),
        Type::List(t) => Type::List(Box::new(r(t))),
        Type::Set(t) => Type::Set(Box::new(r(t))),
        Type::Map(k, v) => Type::Map(Box::new(r(k)), Box::new(r(v))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(r).collect()),
        Type::Named(d, ts) => Type::Named(*d, ts.iter().map(r).collect()),
        Type::Fn(ps, ret) => Type::Fn(ps.iter().map(r).collect(), Box::new(r(ret))),
        other => other.clone(),
    }
}
