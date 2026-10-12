//! Modules and their compilation.
//!
//! The standard library is checked and compiled once per [`Registry`] (and
//! so once per process for engines without host modules): a [`Snapshot`]
//! holds its sources, the checker's state after checking it, the compiler's
//! tables and the compiled program. A compilation (of a program, or of a
//! REPL session's input) starts from a copy of the snapshot, loads the
//! script modules it imports into a [`ModuleGraph`], checks and compiles
//! only them, and initializes only the standard modules it imports.
//!
//! Imports are found by the lexer (`scan_imports`) on the caller's stack
//! while modules load, which may wait for prompts. Each module is then parsed
//! once on the compiler's stack, and its import declarations are matched to
//! the loaded modules by the spans of their names.

use crate::check::budget::{CompileBudget, Stop};
use crate::check::{Checker, ModKind, ParsedModule};
use crate::compile::{CompileInput, Incremental, ProgramMark, compile_more};
use crate::diag::{Diag, FileId, SourceMap, Span};
use crate::engine::{Error, Registry};
use crate::syntax::ast::{self, ImportSource, ItemKind};
use crate::syntax::lexer::{Tok, lex};
use crate::types::{FnId, ModId};
use crate::vm::bytecode::Program;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The modules of a registry, checked and compiled. Immutable once built:
/// every compilation works on its own copy of the parts it extends.
pub(crate) struct Snapshot {
    pub sm: SourceMap,
    /// The modules checked, by module id.
    pub mods: Vec<SnapModule>,
    /// Module name -> module id.
    pub by_name: HashMap<String, ModId>,
    pub checker: Checker,
    pub compiler: Incremental,
    /// The compiled modules; its entry initializes the prelude and `task`.
    pub program: Program,
    /// The sizes of `program`'s tables, to which a program compiled on it
    /// is truncated for reuse (see `Engine::recycle`).
    pub mark: ProgramMark,
}

#[derive(Clone)]
pub(crate) struct SnapModule {
    /// The modules it imports.
    pub deps: Vec<ModId>,
    /// Its initializer.
    pub init: Option<FnId>,
}

/// A module of a registry being loaded into a snapshot.
struct Pending {
    name: String,
    kind: ModKind,
    file: FileId,
    ast: ast::Module,
    /// Import item index -> module name.
    imports: Vec<(usize, String)>,
}

impl Pending {
    /// The module for the checker, with its imports resolved by `by_name`
    /// (which has them all), and the ids of the modules it imports.
    fn resolve(self, by_name: &HashMap<String, ModId>, sm: &SourceMap) -> (ParsedModule, Vec<ModId>) {
        let imports: HashMap<usize, ModId> = self.imports.iter().map(|(item, dep)| (*item, by_name[dep])).collect();
        let mut deps: Vec<ModId> = imports.values().copied().collect();
        deps.sort_unstable();
        deps.dedup();
        let source = sm.file(self.file).clone();
        (ParsedModule { name: self.name, file: self.file, source, ast: self.ast, comments: Vec::new(), kind: self.kind, imports }, deps)
    }
}

impl Snapshot {
    /// Checks and compiles the modules of `registry` (with `only`, just those
    /// and their imports) that `base`, the standard library's snapshot, does
    /// not hold. Every standard module must check; a host module that does
    /// not, or imports one that does not, is left out for the programs that
    /// import it to compile and report.
    pub(crate) fn build(registry: &Registry, base: Option<&Snapshot>, only: Option<&[&str]>) -> Result<Snapshot, Error> {
        let (mut sm, mut by_name, mut mods) = match base {
            Some(b) => (b.sm.clone(), b.by_name.clone(), b.mods.clone()),
            None => (SourceMap::default(), HashMap::new(), Vec::new()),
        };
        // Load and parse the new modules, dependencies first.
        let mut pending: Vec<Pending> = Vec::new();
        let mut errors = Vec::new();
        let all = registry.sources.keys().map(|k| k.as_ref());
        for name in ["prelude", "task"].into_iter().chain(all).filter(|n| only.is_none_or(|o| o.contains(n))) {
            load_registered(registry, name, &by_name, &mut sm, &mut pending, &mut errors, &mut Vec::new());
        }
        let resolve = |sm: &SourceMap, ds: &[Diag]| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect());
        if !errors.is_empty() {
            return Err(resolve(&sm, &errors));
        }
        let mut checker = base.map(|b| b.checker.clone()).unwrap_or_default();
        checker.lint_facts = false;
        // The standard modules at once (they import only each other).
        let (std, hosts): (Vec<Pending>, Vec<Pending>) = pending.into_iter().partition(|p| p.kind != ModKind::Host);
        for (k, p) in std.iter().enumerate() {
            by_name.insert(p.name.clone(), (checker.mods.len() + k) as ModId);
        }
        let mut parsed = Vec::new();
        for p in std {
            let (m, deps) = p.resolve(&by_name, &sm);
            mods.push(SnapModule { deps, init: None });
            parsed.push(m);
        }
        let budget = Arc::new(CompileBudget::default());
        checker.check_modules(&parsed, &registry.by_name, &budget).map_err(|ds| resolve(&sm, &ds))?;
        // Host modules one at a time, each on a copy of the checker.
        for p in hosts {
            if p.imports.iter().any(|(_, dep)| !by_name.contains_key(dep)) {
                // It imports a module that was left out.
                continue;
            }
            let (id, name) = (checker.mods.len() as ModId, p.name.clone());
            let (m, deps) = p.resolve(&by_name, &sm);
            let mut attempt = checker.clone();
            if attempt.check_modules(std::slice::from_ref(&m), &registry.by_name, &budget).is_ok() {
                checker = attempt;
                by_name.insert(name, id);
                mods.push(SnapModule { deps, init: None });
            }
        }
        for (m, info) in mods.iter_mut().zip(checker.mods.iter()) {
            m.init = info.init;
        }
        checker.module_index = registry
            .sources
            .iter()
            .filter(|(name, (kind, _))| *name != "prelude" && (*kind != ModKind::Host || by_name.contains_key(name.as_ref())))
            .map(|(name, (_, src))| (name.to_string(), crate::check::help::module_summary(src)))
            .collect();
        let mut program = base.map(|b| b.program.clone()).unwrap_or_default();
        let mut compiler = base.map(|b| b.compiler.clone()).unwrap_or_default();
        let cp = checker.take_checked();
        let (native_names, native_argc) = native_info(&cp, registry);
        let input = CompileInput {
            checked: &cp,
            sm: &sm,
            natives: &registry.by_name,
            native_names,
            native_argc,
            budget: &budget,
            initialized: None,
        };
        let inits: Vec<FnId> = ["prelude", "task"].iter().filter_map(|n| mods[by_name[*n] as usize].init).collect();
        program.entry = compile_more(input, &mut program, &mut compiler, &inits).map_err(|ds| resolve(&sm, &ds))?;
        checker.restore_checked(cp);
        checker.release_bodies();
        // Facts about the library are of no use to its users.
        checker.diags.clear();
        checker.lint = Default::default();
        checker.global_facts.clear();
        let mark = ProgramMark::of(&program);
        Ok(Snapshot { sm, mods, by_name, checker, compiler, program, mark })
    }

    /// The id of the module `name`, if it is in the snapshot.
    pub(crate) fn module(&self, name: &str) -> Option<ModId> {
        self.by_name.get(name).copied()
    }
}

/// Loads the registered module `name` and, first, those it imports (that
/// are not in `loaded` or `pending` yet). Returns whether it was loaded.
fn load_registered(
    registry: &Registry,
    name: &str,
    loaded: &HashMap<String, ModId>,
    sm: &mut SourceMap,
    pending: &mut Vec<Pending>,
    errors: &mut Vec<Diag>,
    path: &mut Vec<String>,
) -> bool {
    if loaded.contains_key(name) || pending.iter().any(|p| p.name == name) {
        return true;
    }
    let Some((kind, src)) = registry.sources.get(name) else { return false };
    if path.iter().any(|p| p == name) {
        // An import cycle: the module cannot be checked.
        return false;
    }
    let file = sm.add(format!("<{name}>"), src.as_ref());
    let res = crate::syntax::parse_module(src, file);
    let mut ok = res.errors.is_empty();
    if !ok && *kind != ModKind::Host {
        errors.extend(res.errors);
    }
    let mut imports = Vec::new();
    path.push(name.to_string());
    for (item, imp) in import_refs(&res.module) {
        match imp {
            ImportRef::Name(dep, span) => {
                if load_registered(registry, &dep, loaded, sm, pending, errors, path) {
                    imports.push((item, dep));
                } else {
                    ok = false;
                    if *kind != ModKind::Host {
                        errors.push(Diag::error(span, format!("unknown module `{dep}`")));
                    }
                }
            }
            ImportRef::Help(..) => unreachable!("not an import declaration"),
            ImportRef::Path(_, span) => {
                ok = false;
                if *kind != ModKind::Host {
                    errors.push(Diag::error(span, "native modules cannot import script files"));
                }
            }
        }
    }
    path.pop();
    if ok || *kind != ModKind::Host {
        pending.push(Pending { name: name.to_string(), kind: *kind, file, ast: res.module, imports });
    }
    ok || *kind != ModKind::Host
}

/// An import declaration: of a standard or host module by name, or of a
/// script by path, with the span of the name or path. `Help` is a name that
/// starts the argument of `help` or `members`: the module of that name, if
/// there is one, is loaded (and checked) but not bound.
#[derive(Clone, Debug)]
pub(crate) enum ImportRef {
    Name(String, Span),
    Path(String, Span),
    Help(String, Span),
}

impl ImportRef {
    fn span(&self) -> Span {
        match self {
            ImportRef::Name(_, s) | ImportRef::Path(_, s) | ImportRef::Help(_, s) => *s,
        }
    }
}

/// The imports of a syntax tree, by item index.
fn import_refs(m: &ast::Module) -> Vec<(usize, ImportRef)> {
    let mut out = Vec::new();
    for (i, item) in m.items.iter().enumerate() {
        if let ItemKind::Import(imp) = &item.kind {
            let r = match &imp.source {
                ImportSource::Name(id) => ImportRef::Name(id.name.clone(), id.span),
                ImportSource::Path(p, span) => ImportRef::Path(p.clone(), *span),
            };
            out.push((i, r));
        }
    }
    out
}

/// Finds the import declarations of a module, and the modules named by
/// `help` and `members` (`help(json)`, `help("json.Json")`), with the lexer
/// only (which, unlike the parser, needs no deep stack).
pub(crate) fn scan_imports(text: &str, file: FileId) -> Vec<ImportRef> {
    let toks = lex(text, file).tokens;
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth = 0i32;
    let mut at_start = true;
    let word = |t: &crate::syntax::lexer::Token| &text[t.lo as usize..t.hi as usize];
    while i < toks.len() {
        if toks[i].tok == Tok::Ident
            && matches!(word(&toks[i]), "help" | "members")
            && toks.get(i + 1).is_some_and(|t| t.tok == Tok::LParen)
            && let Some(arg) = toks[i + 2..].iter().find(|t| t.tok != Tok::LParen)
        {
            let name = match &arg.tok {
                Tok::Ident => Some(word(arg)),
                Tok::Str(s) => s.split(['.', '<']).next().map(str::trim),
                _ => None,
            };
            if let Some(name) = name.filter(|n| !n.is_empty()) {
                out.push(ImportRef::Help(name.to_string(), Span::new(file, arg.lo, arg.hi)));
            }
        }
        match &toks[i].tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
            Tok::Import | Tok::From if depth == 0 && at_start => {
                if let Some(t) = toks.get(i + 1) {
                    let span = Span::new(file, t.lo, t.hi);
                    match &t.tok {
                        Tok::Ident => out.push(ImportRef::Name(text[t.lo as usize..t.hi as usize].to_string(), span)),
                        Tok::Str(s) => out.push(ImportRef::Path(s.clone(), span)),
                        _ => {}
                    }
                }
                // Skip the rest of the declaration.
                while i < toks.len() && !matches!(toks[i].tok, Tok::Semi | Tok::Eof) {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        at_start = depth == 0 && matches!(toks[i].tok, Tok::Semi);
        i += 1;
    }
    out
}

/// A module loaded by a compilation (or a REPL session): a script, or a
/// host module that is not in the snapshot.
pub(crate) struct LoadedModule {
    pub name: String,
    pub file: FileId,
    pub kind: ModKind,
    /// The directory relative imports start from (scripts).
    pub dir: Option<PathBuf>,
    /// The modules it imports, by the span of their name or path in its
    /// import declarations.
    pub imports: Vec<(Span, Target)>,
    /// The module's id, once it is checked.
    pub id: Option<ModId>,
}

/// The module an import refers to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Target {
    /// A module of the snapshot.
    Snapshot(ModId),
    /// A module of the graph, by index.
    Loaded(usize),
}

/// The modules of a compilation (or of a REPL session, which adds to them).
#[derive(Default)]
pub(crate) struct ModuleGraph {
    /// The snapshot's sources, then those loaded.
    pub sm: SourceMap,
    pub modules: Vec<LoadedModule>,
    /// Module key (`std:name` or `file:path`) -> index in `modules`.
    pub index: HashMap<String, usize>,
    /// Module indices in dependency (check) order.
    pub order: Vec<usize>,
    /// The snapshot modules used (imported by loaded modules, the prelude and
    /// `task`, and what they import), in dependency order: the program
    /// initializes them.
    pub std: Vec<ModId>,
    /// Bytes of script source loaded (main file and imports), limited by
    /// `max_source_bytes` in total.
    pub source_bytes: usize,
}

/// The sizes of a [`ModuleGraph`], to undo a failed addition.
#[derive(Clone, Copy)]
pub(crate) struct GraphMark {
    pub(crate) files: usize,
    pub(crate) modules: usize,
    pub(crate) order: usize,
    pub(crate) std: usize,
    source_bytes: usize,
}

impl ModuleGraph {
    /// A graph using the prelude and `task` of `snap`.
    pub(crate) fn new(snap: &Snapshot) -> ModuleGraph {
        let mut g = ModuleGraph {
            sm: snap.sm.clone(),
            modules: Vec::new(),
            index: HashMap::new(),
            order: Vec::new(),
            std: Vec::new(),
            source_bytes: 0,
        };
        for name in ["prelude", "task"] {
            if let Some(id) = snap.module(name) {
                g.use_std(snap, id);
            }
        }
        g
    }

    pub(crate) fn mark(&self) -> GraphMark {
        GraphMark {
            files: self.sm.files.len(),
            modules: self.modules.len(),
            order: self.order.len(),
            std: self.std.len(),
            source_bytes: self.source_bytes,
        }
    }

    pub(crate) fn rollback(&mut self, m: &GraphMark) {
        self.sm.files.truncate(m.files);
        self.modules.truncate(m.modules);
        self.order.truncate(m.order);
        self.std.truncate(m.std);
        self.source_bytes = m.source_bytes;
        self.index.retain(|_, i| *i < m.modules);
    }

    /// The graph on `snap`, which extends this graph's snapshot with the
    /// standard modules it loaded; the other modules get new file ids.
    pub(crate) fn rebase(self, snap: &Snapshot) -> ModuleGraph {
        let mut g = ModuleGraph::new(snap);
        g.source_bytes = self.source_bytes;
        // Old module index -> where the module is now.
        let mut index: Vec<Target> = Vec::with_capacity(self.modules.len());
        let mut files: HashMap<FileId, FileId> = HashMap::new();
        for m in &self.modules {
            if m.kind == ModKind::Std {
                let id = snap.module(&m.name).expect("the snapshot holds the module");
                g.use_std(snap, id);
                index.push(Target::Snapshot(id));
                continue;
            }
            let f = self.sm.file(m.file);
            let file = g.sm.add(f.name.clone(), f.text.clone());
            files.insert(m.file, file);
            index.push(Target::Loaded(g.modules.len()));
            g.modules.push(LoadedModule { name: m.name.clone(), file, kind: m.kind, dir: m.dir.clone(), imports: Vec::new(), id: None });
        }
        let target = |t: Target| match t {
            Target::Loaded(i) => index[i],
            t => t,
        };
        for (m, t) in self.modules.iter().zip(&index) {
            if let Target::Loaded(i) = t {
                g.modules[*i].imports = m.imports.iter().map(|&(span, t)| (Span { file: files[&span.file], ..span }, target(t))).collect();
            }
        }
        g.order = self
            .order
            .iter()
            .filter_map(|&mi| match index[mi] {
                Target::Loaded(i) => Some(i),
                Target::Snapshot(_) => None,
            })
            .collect();
        g.index = self
            .index
            .iter()
            .filter_map(|(k, &mi)| match index[mi] {
                Target::Loaded(i) => Some((k.clone(), i)),
                Target::Snapshot(_) => None,
            })
            .collect();
        g
    }

    /// Records that the snapshot module `id` (and what it imports) is used.
    pub(crate) fn use_std(&mut self, snap: &Snapshot, id: ModId) {
        if self.std.contains(&id) {
            return;
        }
        for &dep in &snap.mods[id as usize].deps {
            self.use_std(snap, dep);
        }
        self.std.push(id);
    }

    /// Adds the module `name`, whose source is `file`; returns its index
    /// and the imports to load.
    pub(crate) fn add(&mut self, name: String, file: FileId, kind: ModKind, dir: Option<PathBuf>) -> (usize, Vec<ImportRef>) {
        let imports = scan_imports(self.sm.text(file), file);
        self.modules.push(LoadedModule { name, file, kind, dir, imports: Vec::new(), id: None });
        (self.modules.len() - 1, imports)
    }

    /// Parses the modules `order` for the checker, which numbers them from
    /// `first`, or returns their syntax errors.
    pub(crate) fn parse(&mut self, order: &[usize], first: ModId) -> Result<Vec<ParsedModule>, Vec<Diag>> {
        for (k, &mi) in order.iter().enumerate() {
            self.modules[mi].id = Some(first + k as ModId);
        }
        let mut errors = Vec::new();
        let mut parsed = Vec::new();
        for &mi in order {
            let m = &self.modules[mi];
            let res = crate::syntax::parse_module(self.sm.text(m.file), m.file);
            errors.extend(res.errors);
            let mut imports = HashMap::new();
            for (item, r) in import_refs(&res.module) {
                let target = m.imports.iter().find(|(span, _)| *span == r.span()).map(|&(_, t)| t);
                let id = match target {
                    Some(Target::Snapshot(id)) => Some(id),
                    Some(Target::Loaded(i)) => self.modules[i].id,
                    None => None,
                };
                if let Some(id) = id {
                    imports.insert(item, id);
                }
            }
            let comments = if m.kind == (ModKind::Script { main: true }) { res.comments } else { Vec::new() };
            let source = self.sm.file(m.file).clone();
            parsed.push(ParsedModule { name: m.name.clone(), file: m.file, source, ast: res.module, comments, kind: m.kind, imports });
        }
        if errors.is_empty() { Ok(parsed) } else { Err(errors) }
    }

    /// Whether a module of `order` is a standard or host module (which
    /// declare natives).
    pub(crate) fn has_natives(&self, order: &[usize]) -> bool {
        order.iter().any(|&mi| !matches!(self.modules[mi].kind, ModKind::Script { .. }))
    }
}

/// The error for a compilation stopped by its budget.
pub(crate) fn budget_error(stop: Stop, sm: &SourceMap, main_file: u32) -> Error {
    match stop {
        Stop::Fuel => Error::Compile(vec![sm.resolve(&Diag::error(
            Span::new(main_file, 0, 0),
            "program too complex to compile: exceeded the compile budget (max_compile_fuel)",
        ))]),
        Stop::Time => Error::Limit(crate::LimitKind::WallTime),
        Stop::Interrupted => Error::Interrupted,
    }
}

/// Names and argument counts (by native id) of the natives declared by the
/// checked modules.
pub(crate) fn native_info(checked: &crate::check::CheckedProgram, registry: &Registry) -> (Vec<String>, Vec<u16>) {
    let n = registry.natives.len();
    let mut native_names = vec![String::new(); n];
    let mut native_argc = vec![0u16; n];
    for f in &checked.fns {
        if let crate::check::FnKind::Native(n) = f.kind
            && (n as usize) < native_names.len()
        {
            native_names[n as usize] = checked.name(f.name).to_string();
            let reflect = f.generics.iter().filter(|g| checked.params[**g as usize].reflect).count();
            // A native implementing a trait method also receives its impl's dictionary.
            native_argc[n as usize] = (f.dicts.len() + f.params.len() + reflect) as u16;
        }
    }
    (native_names, native_argc)
}
