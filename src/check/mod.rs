//! Name resolution and type checking: AST to typed HIR.

mod expr;
mod pat;
mod unify;

use crate::diag::{Diag, FileId, SourceMap, Span};
use crate::hir::*;
use crate::syntax::ast::{self, ItemKind, TypeExpr, TypeExprKind};
use crate::types::*;
use std::collections::{HashMap, HashSet};

pub use pat::Witness;

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
}

pub struct ModSummary {
    pub name: String,
    pub kind: ModKind,
    pub summary: String,
}

pub type NativeLookup<'a> = &'a dyn Fn(&str) -> Option<u32>;

pub(crate) struct LocalData {
    pub ty: Type,
    pub scheme: Vec<ParamId>,
    pub used: bool,
    pub assigned: bool,
    pub is_param: bool,
    pub decl_span: Span,
}

pub(crate) struct LoopCtx {
    pub break_ty: Option<Type>,
}

pub(crate) struct FnCtx {
    pub ret: Type,
    pub kind: CtxKind,
    pub loops: Vec<LoopCtx>,
    pub lambda: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtxKind {
    Function,
    Lambda,
    TopLevel { main: bool },
}

#[derive(Default)]
pub(crate) struct FnState {
    pub locals: Vec<LocalInfo>,
    pub data: Vec<LocalData>,
    pub scopes: Vec<Vec<(String, LocalId)>>,
    pub ctx: Vec<FnCtx>,
    pub next_lambda: u32,
    pub obligations: Vec<(Type, Bound, Span)>,
    pub reflect_checks: Vec<(Type, Span)>,
    pub infer_checks: Vec<(Type, Span)>,
    /// Inside a top-level statement of a module (lets become globals).
    pub top_level: bool,
    pub generics: Vec<(String, ParamId)>,
    pub self_ty: Option<Type>,
}

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
        }
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        if self.diags.len() < MAX_ERRORS {
            self.diags.push(Diag::error(span, msg));
        }
    }

    pub fn printer(&self) -> TypePrinter<'_> {
        TypePrinter { defs: &self.types, params: &self.params, wk: self.wk }
    }

    pub fn show(&self, t: &Type) -> String {
        let z = self.zonk(t);
        self.printer().show(&z)
    }

    pub fn new_param(&mut self, name: &str, bounds: Vec<Bound>, reflect: bool) -> ParamId {
        self.params.push(ParamInfo { name: name.to_string(), bounds, reflect });
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
        let modules: Vec<ParsedModule> = modules;
        for (i, m) in modules.iter().enumerate() {
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
            self.cur_mod = i as ModId;
            self.check_module(m);
            if i as ModId == self.prelude.unwrap_or(u32::MAX) {
                self.find_well_known();
            }
        }
        let errors: Vec<Diag> = self.diags.iter().filter(|d| d.severity == crate::diag::Severity::Error).cloned().collect();
        if !errors.is_empty() {
            return Err(errors);
        }
        let module_inits = self.mods.iter().filter_map(|m| m.init).collect();
        self.lint.main_imports = self.mods[self.main_module as usize].import_spans.clone();
        let summaries = self.module_summaries();
        Ok(CheckedProgram {
            fns: self.fns,
            bodies: self.bodies,
            types: self.types,
            params: self.params,
            wk: self.wk,
            variants: self.variants,
            globals: self.globals,
            module_inits,
            modules: summaries,
            tests: self.tests,
            lint: self.lint,
            main_module: self.main_module,
        })
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
        if let Some((existing, _, _)) = self.mods[m].scope.get(name) {
            // Re-binding a global with `let` shadows; anything else is a duplicate.
            let shadowable = matches!((existing, &def), (Def::Global(_), Def::Global(_)));
            if !shadowable {
                self.error(span, format!("`{name}` is already defined"));
                return;
            }
        }
        self.mods[m].scope.insert(name.to_string(), (def, is_pub, span));
    }

    fn check_module(&mut self, pm: &ParsedModule) {
        let mi = self.cur_mod;
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
        // 2. Declare types.
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
        // Resolve aliases eagerly so errors are reported once.
        for i in 0..self.aliases.len() {
            if self.aliases[i].ty.is_none() && self.aliases[i].decl.as_ref().map(|d| d.0) == Some(mi) {
                self.resolve_alias(i, Span::default());
            }
        }
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
                    });
                    let decl = self.fns[id as usize].decl.clone().unwrap();
                    self.check_fn_body(id, &decl);
                }
            }
        }
        // 8. Globals must have fully inferred types.
        for g in 0..self.globals.len() {
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

    fn declare_generics(&mut self, gs: &[ast::GenericParam]) -> Vec<ParamId> {
        let mut out = Vec::new();
        for g in gs {
            let bounds = self.parse_bounds(&g.bounds);
            out.push(self.new_param(&g.name.name, bounds, g.reflect));
        }
        out
    }

    fn parse_bounds(&mut self, bs: &[ast::Ident]) -> Vec<Bound> {
        let mut out = Vec::new();
        for b in bs {
            match Bound::parse(&b.name) {
                Some(x) => out.push(x),
                None => self.error(b.span, format!("unknown bound `{}`", b.name)),
            }
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
        let t = self.resolve_type_in(&te, &scope, None);
        self.cur_mod = saved;
        self.aliases[i].resolving = false;
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
        for g in &f.generics {
            let bounds = self.parse_bounds(&g.bounds);
            let p = self.new_param(&g.name.name, bounds, g.reflect);
            scope.push((g.name.name.clone(), p));
            generics.push(p);
        }
        for w in &f.where_clause {
            let bounds = self.parse_bounds(&w.bounds);
            match scope.iter().rev().find(|(n, _)| *n == w.name.name) {
                Some((_, p)) => {
                    let p = *p as usize;
                    self.params[p].bounds.extend(bounds);
                }
                None => self.error(w.name.span, format!("unknown type parameter `{}`", w.name.name)),
            }
        }
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
            let np = self.new_param(&info.name, info.bounds.clone(), false);
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
        let mut gen_names = Vec::new();
        for g in &x.generics {
            gen_names.push((g.name.name.clone(), self.parse_bounds(&g.bounds)));
        }
        // Resolve the target once to find its constructor.
        let probe_scope: Vec<(String, ParamId)> = gen_names.iter().map(|(n, b)| (n.clone(), self.new_param(n, b.clone(), false))).collect();
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
            let scope: Vec<(String, ParamId)> = gen_names.iter().map(|(n, b)| (n.clone(), self.new_param(n, b.clone(), false))).collect();
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
        self.fs.scopes.push(Vec::new());
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
        let mut param_locals = Vec::new();
        let decl_params: Vec<(String, Span)> = match decl {
            FnDeclRef::Fn(_, f) => f.params.iter().map(|p| (p.name.name.clone(), p.name.span)).collect(),
            _ => fi.params.iter().map(|p| (p.name.clone(), fi.span)).collect(),
        };
        for (p, (name, pspan)) in fi.params.iter().zip(decl_params) {
            let l = self.declare_local(&name, p.ty.clone(), false, pspan, true);
            param_locals.push(l);
        }
        self.fs.ctx.push(FnCtx { ret: fi.ret.clone(), kind: CtxKind::Function, loops: vec![], lambda: 0 });
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
        self.fs.ctx.push(FnCtx { ret: Type::Unit, kind: CtxKind::TopLevel { main }, loops: vec![], lambda: 0 });
        let mut stmts = Vec::new();
        for item in &pm.ast.items {
            if let ItemKind::Stmt(s) = &item.kind {
                self.fs.top_level = true;
                let h = self.check_stmt(s, false);
                self.fs.top_level = false;
                stmts.extend(h);
            }
        }
        let body = HExpr { kind: HKind::Block(stmts), ty: Type::Unit, span: pm.ast.span };
        let name = self.fns[id as usize].name.clone();
        let hfn = self.finish_fn(name, vec![], body, pm.ast.span);
        self.bodies[id as usize] = Some(hfn);
    }

    /// Final checks for a function: bounds, inference completeness; zonks the body.
    fn finish_fn(&mut self, name: String, params: Vec<LocalId>, mut body: HExpr, span: Span) -> HFn {
        let obligations = std::mem::take(&mut self.fs.obligations);
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
            self.fs.scopes.last_mut().unwrap().push((name.to_string(), id));
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
        for scope in self.fs.scopes.iter().rev() {
            for (n, id) in scope.iter().rev() {
                if n == name {
                    return Some(*id);
                }
            }
        }
        None
    }

    pub(crate) fn push_scope(&mut self) {
        self.fs.scopes.push(Vec::new());
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
    match t {
        Type::Var(v) => match &subst[*v as usize] {
            Some(t) => zonk_with(subst, t),
            None => t.clone(),
        },
        Type::List(t) => Type::List(Box::new(zonk_with(subst, t))),
        Type::Set(t) => Type::Set(Box::new(zonk_with(subst, t))),
        Type::Map(k, v) => Type::Map(Box::new(zonk_with(subst, k)), Box::new(zonk_with(subst, v))),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| zonk_with(subst, t)).collect()),
        Type::Named(d, ts) => Type::Named(*d, ts.iter().map(|t| zonk_with(subst, t)).collect()),
        Type::Fn(ps, r) => Type::Fn(ps.iter().map(|t| zonk_with(subst, t)).collect(), Box::new(zonk_with(subst, r))),
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
            ast::StmtKind::Expr(e) => expr(e, out),
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
            K::Block(b) | K::Loop(b) => block(b, out),
            K::While { cond: c, body } => {
                cond(c, out);
                block(body, out);
            }
            K::For { iter, body, .. } => {
                expr(iter, out);
                block(body, out);
            }
            K::Lambda { body, .. } => expr(body, out),
            K::Return(Some(x)) | K::Break(Some(x)) => expr(x, out),
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
            ItemKind::Test(t) => block(&t.body, &mut out),
            _ => {}
        }
    }
    out
}
