//! The embedding API: `Engine`, `Config`, errors and module loading.

use crate::check::{Checker, ModKind, ParsedModule};
use crate::diag::{Diag, Diagnostic, SourceMap, Span};
use crate::native::NativeModule;
use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter, normalize_path};
use crate::stdlib;
use crate::syntax::lexer::{StrPart, Tok, lex};
use crate::vm::bytecode::{HelpEntry, Program};
use crate::vm::sched::{IoShared, IoTable, RunEnd};
use crate::vm::{Host, NativeEntry, OutputSink, Rng, SharedSink, StdinSource, TraceEntry, Trap, Vm, entropy_seed};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Per-engine resource limits.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_memory: Option<usize>,
    pub max_instructions: Option<u64>,
    pub max_cpu_time: Option<Duration>,
    pub max_wall_time: Option<Duration>,
    pub max_call_depth: usize,
    pub max_tasks: usize,
    pub max_open_resources: usize,
    pub max_output_bytes: Option<u64>,
    pub max_http_body: usize,
    pub max_source_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_memory: Some(512 << 20),
            max_instructions: None,
            max_cpu_time: None,
            max_wall_time: None,
            max_call_depth: 10_000,
            max_tasks: 10_000,
            max_open_resources: 1024,
            max_output_bytes: None,
            max_http_body: 64 << 20,
            max_source_bytes: 16 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    Memory,
    Instructions,
    CpuTime,
    WallTime,
    Output,
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LimitKind::Memory => "memory limit exceeded",
            LimitKind::Instructions => "instruction limit exceeded",
            LimitKind::CpuTime => "cpu time limit exceeded",
            LimitKind::WallTime => "time limit exceeded",
            LimitKind::Output => "output limit exceeded",
        })
    }
}

pub type TraceFrame = TraceEntry;

#[derive(Debug, Clone)]
pub enum Error {
    Compile(Vec<Diagnostic>),
    Runtime { message: String, trace: Vec<TraceFrame> },
    Limit(LimitKind),
    Interrupted,
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Compile(ds) => {
                for (i, d) in ds.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{d}")?;
                }
                Ok(())
            }
            Error::Runtime { message, trace } => {
                write!(f, "panic: {message}")?;
                for t in trace {
                    write!(f, "\n    at {} ({}:{})", t.function, t.file, t.line)?;
                }
                Ok(())
            }
            Error::Limit(k) => write!(f, "{k}"),
            Error::Interrupted => write!(f, "interrupted"),
            Error::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub exit_code: i32,
}

/// An in-memory output sink, e.g. to capture a script's standard output.
#[derive(Clone, Default)]
pub struct MemorySink(Arc<Mutex<Vec<u8>>>);

impl MemorySink {
    pub fn new() -> MemorySink {
        MemorySink::default()
    }
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }
    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl OutputSink for MemorySink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(data);
        Ok(())
    }
}

struct StdoutSink;
impl OutputSink for StdoutSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut o = std::io::stdout().lock();
        o.write_all(data)?;
        o.flush()
    }
}

struct StderrSink;
impl OutputSink for StderrSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        std::io::stderr().lock().write_all(data)
    }
}

struct NullSink;
impl OutputSink for NullSink {
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
enum StdinConfig {
    None,
    Bytes(Vec<u8>),
    Inherit,
}

/// Engine configuration (builder style).
#[derive(Clone)]
pub struct Config {
    pub(crate) policy: Policy,
    pub(crate) prompter: Option<Arc<dyn Prompter>>,
    pub(crate) limits: Limits,
    pub(crate) modules: Vec<NativeModule>,
    pub(crate) stdout: SharedSink,
    pub(crate) stderr: SharedSink,
    stdin: StdinConfig,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) module_root: Option<PathBuf>,
    pub(crate) args: Vec<String>,
    pub(crate) env: Option<Vec<(String, String)>>,
    pub(crate) seed: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

impl Config {
    pub fn new() -> Config {
        Config {
            policy: Policy::deny_all(),
            prompter: None,
            limits: Limits::default(),
            modules: Vec::new(),
            stdout: Arc::new(Mutex::new(StdoutSink)),
            stderr: Arc::new(Mutex::new(StderrSink)),
            stdin: StdinConfig::None,
            cwd: None,
            module_root: None,
            args: Vec::new(),
            env: None,
            seed: None,
        }
    }
    pub fn policy(mut self, p: Policy) -> Self {
        self.policy = p;
        self
    }
    pub fn prompter(mut self, p: impl Prompter + 'static) -> Self {
        self.prompter = Some(Arc::new(p));
        self
    }
    pub fn limits(mut self, l: Limits) -> Self {
        self.limits = l;
        self
    }
    pub fn module(mut self, m: NativeModule) -> Self {
        self.modules.push(m);
        self
    }
    pub fn stdout(mut self, s: impl OutputSink + 'static) -> Self {
        self.stdout = Arc::new(Mutex::new(s));
        self
    }
    pub fn stderr(mut self, s: impl OutputSink + 'static) -> Self {
        self.stderr = Arc::new(Mutex::new(s));
        self
    }
    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = StdinConfig::Bytes(data);
        self
    }
    /// Uses the host process's standard input.
    pub fn inherit_stdin(mut self) -> Self {
        self.stdin = StdinConfig::Inherit;
        self
    }
    pub fn cwd(mut self, p: impl AsRef<Path>) -> Self {
        self.cwd = Some(p.as_ref().to_path_buf());
        self
    }
    pub fn module_root(mut self, p: impl AsRef<Path>) -> Self {
        self.module_root = Some(p.as_ref().to_path_buf());
        self
    }
    pub fn args(mut self, a: Vec<String>) -> Self {
        self.args = a;
        self
    }
    /// A virtual environment (instead of the host process environment).
    pub fn env(mut self, vars: Vec<(String, String)>) -> Self {
        self.env = Some(vars);
        self
    }
    pub fn seed(mut self, s: u64) -> Self {
        self.seed = Some(s);
        self
    }
}

/// Stops a running engine from another thread.
#[derive(Clone)]
pub struct InterruptHandle {
    flag: Arc<AtomicBool>,
    shared: Arc<IoShared>,
}

impl InterruptHandle {
    pub fn interrupt(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.shared.wake();
    }
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TestReport {
    pub results: Vec<TestResult>,
}

impl TestReport {
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }
    pub fn failed(&self) -> usize {
        self.results.len() - self.passed()
    }
    pub fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.results {
            if r.passed {
                out.push_str(&format!("ok {}\n", r.name));
            } else {
                out.push_str(&format!("FAIL {}: {}\n", r.name, r.message.as_deref().unwrap_or("")));
            }
        }
        out.push_str(&format!("{} passed, {} failed\n", self.passed(), self.failed()));
        out
    }
}

/// The native function registry shared by compilation and execution.
pub(crate) struct Registry {
    pub entries: Vec<NativeEntry>,
    pub by_name: HashMap<String, u32>,
    /// Module name -> Sand source.
    pub sources: BTreeMap<String, (ModKind, String)>,
}

impl Registry {
    fn build(config: &Config) -> Registry {
        let mut r = Registry { entries: Vec::new(), by_name: HashMap::new(), sources: BTreeMap::new() };
        stdlib::register(&mut r);
        for m in &config.modules {
            m.register(&mut r);
        }
        r
    }

    pub fn add(&mut self, name: &str, imp: crate::vm::NativeImpl) {
        let id = self.entries.len() as u32;
        self.entries.push(NativeEntry { name: name.to_string(), imp });
        self.by_name.insert(name.to_string(), id);
    }

    pub fn add_builtin(&mut self, name: &str, f: crate::vm::NativeFn) {
        self.add(name, crate::vm::NativeImpl::Builtin(f));
    }

    pub fn source(&mut self, module: &str, kind: ModKind, src: &str) {
        self.sources.insert(module.to_string(), (kind, src.to_string()));
    }
}

pub struct Engine {
    config: Config,
    registry: Arc<Registry>,
    natives: Arc<Vec<NativeEntry>>,
    interrupt: Arc<AtomicBool>,
    shared: Arc<IoShared>,
}

struct LoadedModule {
    name: String,
    file: u32,
    kind: ModKind,
    /// Import item index -> module key.
    imports: Vec<(usize, String)>,
    dir: Option<PathBuf>,
}

/// A compiled program with its sources (for diagnostics).
pub(crate) struct Compiled {
    pub program: Arc<Program>,
    pub warnings: Vec<Diagnostic>,
    /// Checker facts about the main module, for the linter.
    pub facts: Option<ResolvedFacts>,
}

/// Lint facts with spans resolved to (line, column) in the main file.
#[derive(Debug, Default)]
pub(crate) struct ResolvedFacts {
    pub unused_results: Vec<(usize, usize)>,
    pub locals: Vec<(usize, usize, crate::check::LocalFact)>,
    pub imports: Vec<(usize, usize, String)>,
    pub used_names: std::collections::HashSet<String>,
}

const COMPILE_STACK: usize = 256 << 20;

impl Engine {
    pub fn new(config: Config) -> Engine {
        let registry = Registry::build(&config);
        let natives = Arc::new(registry.entries.clone());
        Engine {
            config,
            registry: Arc::new(registry),
            natives,
            interrupt: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(IoShared::default()),
        }
    }

    /// Compiles a file and returns checker facts for the linter.
    pub(crate) async fn lint_facts(&self, path: &Path) -> Result<ResolvedFacts, Error> {
        let (name, source, root) = self.read_main(path)?;
        let c = self.compile(&name, &source, root, Some(path)).await?;
        Ok(c.facts.unwrap_or_default())
    }

    /// Compiles source text and returns checker facts for the linter.
    pub(crate) async fn lint_facts_source(&self, name: &str, source: &str) -> Result<ResolvedFacts, Error> {
        let root = std::env::current_dir().unwrap_or_default();
        let c = self.compile(name, source, root, None).await?;
        Ok(c.facts.unwrap_or_default())
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle { flag: self.interrupt.clone(), shared: self.shared.clone() }
    }

    /// Type checks a source without running it; returns warnings.
    pub fn check_source(&mut self, name: &str, source: &str) -> Result<Vec<Diagnostic>, Error> {
        let rt = tokio::runtime::Builder::new_current_thread().build().map_err(|e| Error::Io(e.to_string()))?;
        let root = std::env::current_dir().unwrap_or_default();
        let c = rt.block_on(self.compile(name, source, root, None))?;
        Ok(c.warnings)
    }

    /// Type checks a file (and its imports) without running it; returns warnings.
    pub fn check_file(&mut self, path: impl AsRef<Path>) -> Result<Vec<Diagnostic>, Error> {
        let rt = tokio::runtime::Builder::new_current_thread().build().map_err(|e| Error::Io(e.to_string()))?;
        let (name, source, root) = self.read_main(path.as_ref())?;
        let c = rt.block_on(self.compile(&name, &source, root, Some(path.as_ref())))?;
        Ok(c.warnings)
    }

    pub async fn run_source(&mut self, name: &str, source: &str) -> Result<crate::Outcome, Error> {
        let root = self
            .config
            .module_root
            .clone()
            .or_else(|| self.config.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let compiled = self.compile(name, source, root.clone(), None).await?;
        self.execute(compiled.program, root).await
    }

    pub async fn run_file(&mut self, path: impl AsRef<Path>) -> Result<crate::Outcome, Error> {
        let (name, source, root) = self.read_main(path.as_ref())?;
        let compiled = self.compile(&name, &source, root.clone(), Some(path.as_ref())).await?;
        let cwd = self.config.cwd.clone().unwrap_or(root);
        self.execute(compiled.program, cwd).await
    }

    pub async fn run_tests(&mut self, path: impl AsRef<Path>) -> Result<TestReport, Error> {
        let (name, source, root) = self.read_main(path.as_ref())?;
        let compiled = self.compile(&name, &source, root.clone(), Some(path.as_ref())).await?;
        let cwd = self.config.cwd.clone().unwrap_or(root);
        let prog = compiled.program;
        let mut report = TestReport::default();
        for (tname, proto) in prog.tests.clone() {
            let mut vm = self.new_vm(prog.clone(), cwd.clone(), true);
            let init = vm.run_main(prog.entry).await;
            let result = match init {
                Ok(RunEnd::Finished(_)) => {
                    let mut vm2 = vm;
                    vm2.ready.clear();
                    match vm2.run_main(proto).await {
                        Ok(RunEnd::Finished(_)) | Ok(RunEnd::Exit(0)) => Ok(()),
                        Ok(RunEnd::Exit(c)) => Err(format!("exited with code {c}")),
                        Err(t) => Err(trap_message(t)),
                    }
                }
                Ok(RunEnd::Exit(c)) => Err(format!("module exited with code {c}")),
                Err(t) => Err(trap_message(t)),
            };
            report.results.push(TestResult { name: tname, passed: result.is_ok(), message: result.err() });
        }
        Ok(report)
    }

    fn read_main(&self, path: &Path) -> Result<(String, String, PathBuf), Error> {
        let source = std::fs::read_to_string(path).map_err(|e| Error::Io(format!("cannot read {}: {e}", path.display())))?;
        let abs = normalize_path(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
        let root = self
            .config
            .module_root
            .clone()
            .map(|r| normalize_path(&r))
            .unwrap_or_else(|| abs.parent().map(|p| p.to_path_buf()).unwrap_or_default());
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "main.sand".into());
        Ok((name, source, root))
    }

    fn new_vm(&self, prog: Arc<Program>, cwd: PathBuf, quiet: bool) -> Vm {
        let c = &self.config;
        let stdin = match &c.stdin {
            StdinConfig::None => StdinSource::None,
            StdinConfig::Bytes(b) => StdinSource::Bytes(std::io::Cursor::new(b.clone())),
            StdinConfig::Inherit => StdinSource::Inherit(tokio::io::BufReader::new(tokio::io::stdin())),
        };
        let (stdout, stderr): (SharedSink, SharedSink) =
            if quiet { (Arc::new(Mutex::new(NullSink)), Arc::new(Mutex::new(NullSink))) } else { (c.stdout.clone(), c.stderr.clone()) };
        let host = Host {
            policy: c.policy.clone(),
            prompter: c.prompter.clone(),
            grants_once: Vec::new(),
            denies_once: Vec::new(),
            always: HashMap::new(),
            stdout,
            stderr,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            output_bytes: 0,
            limits: c.limits.clone(),
            cwd,
            args: c.args.clone(),
            env: c.env.clone(),
            rng: Rng::new(c.seed.unwrap_or_else(entropy_seed)),
            log_level: 1,
            interrupt: self.interrupt.clone(),
            stream_decisions: HashMap::new(),
        };
        let mut vm = Vm::new(prog, self.natives.clone(), host);
        vm.io = IoTable::with_shared(self.shared.clone());
        vm
    }

    async fn execute(&mut self, prog: Arc<Program>, cwd: PathBuf) -> Result<crate::Outcome, Error> {
        if std::env::var_os("SAND_DUMP_BYTECODE").is_some() {
            for (i, p) in prog.protos.iter().enumerate() {
                if prog.files.get(p.file as usize).is_some_and(|f| !f.starts_with('<')) {
                    eprintln!("proto {i} {} (params {}, regs {})", p.name, p.nparams, p.nregs);
                    for (ip, op) in p.code.iter().enumerate() {
                        eprintln!("  {ip:4} [{:3}] {op:?}", p.lines.get(ip).copied().unwrap_or(0));
                    }
                }
            }
        }
        self.interrupt.store(false, Ordering::SeqCst);
        let mut vm = self.new_vm(prog.clone(), cwd, false);
        let r = vm.run_main(prog.entry).await;
        if let Ok(mut s) = self.config.stdout.lock() {
            let _ = s.flush();
        }
        match r {
            Ok(RunEnd::Finished(_)) => Ok(crate::Outcome { exit_code: 0 }),
            Ok(RunEnd::Exit(c)) => Ok(crate::Outcome { exit_code: c }),
            Err(Trap::Exit(c)) => Ok(crate::Outcome { exit_code: c }),
            Err(Trap::Panic { message, trace }) => Err(Error::Runtime { message, trace }),
            Err(Trap::Limit(k)) => Err(Error::Limit(k)),
            Err(Trap::Interrupted) => Err(Error::Interrupted),
        }
    }

    // ------------------------------------------------------------------
    // Loading and compiling

    async fn check_import(&self, path: &Path) -> bool {
        let cap = Capability::Import(path.to_path_buf());
        match self.config.policy.check(&cap) {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Prompt => match &self.config.prompter {
                Some(p) => matches!(p.prompt(cap).await, PromptAnswer::AllowOnce | PromptAnswer::AllowAlways),
                None => false,
            },
        }
    }

    pub(crate) async fn compile(
        &self,
        main_name: &str,
        main_source: &str,
        root: PathBuf,
        main_path: Option<&Path>,
    ) -> Result<Compiled, Error> {
        let mut sm = SourceMap::default();
        let mut errors: Vec<Diag> = Vec::new();
        if main_source.len() > self.config.limits.max_source_bytes {
            let f = sm.add(main_name, "");
            errors.push(Diag::error(Span::new(f, 0, 0), "source too large"));
            return Err(Error::Compile(errors.iter().map(|d| sm.resolve(d)).collect()));
        }
        let main_dir = main_path
            .and_then(|p| std::path::absolute(p).ok())
            .and_then(|p| p.parent().map(normalize_path))
            .unwrap_or_else(|| root.clone());
        let root = normalize_path(&root);

        // Discover modules depth-first, in dependency order.
        let mut modules: Vec<LoadedModule> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut order: Vec<usize> = Vec::new();

        enum State {
            Visiting,
            Done,
        }
        let mut state: HashMap<String, State> = HashMap::new();

        // Built-in modules: prelude and task.
        let prelude_src = self.registry.sources.get("prelude").map(|(_, s)| s.clone()).unwrap_or_default();
        let f = sm.add("<prelude>", prelude_src.as_str());
        modules.push(LoadedModule { name: "prelude".into(), file: f, kind: ModKind::Prelude, imports: vec![], dir: None });
        index.insert("std:prelude".into(), 0);
        order.push(0);
        state.insert("std:prelude".into(), State::Done);

        // Iterative DFS stack: (module index, next import position).
        let main_file = sm.add(main_name, main_source);
        let main_key = format!(
            "file:{}",
            main_path
                .map(|p| normalize_path(&std::path::absolute(p).unwrap_or_default()).display().to_string())
                .unwrap_or_else(|| main_name.to_string())
        );
        let main_mod_name = Path::new(main_name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "main".into());
        modules.push(LoadedModule {
            name: main_mod_name,
            file: main_file,
            kind: ModKind::Script { main: true },
            imports: vec![],
            dir: Some(main_dir),
        });
        index.insert(main_key.clone(), 1);

        // Every module implicitly depends on `task`, so it is loaded first.
        self.load_std_tree("task", &mut sm, &mut modules, &mut index, &mut order, &mut errors);
        // DFS stack: (module index, its imports, next import to visit).
        let mut stack: Vec<ImportFrame> = Vec::new();
        state.insert(main_key.clone(), State::Visiting);
        let imports = scan_imports(&sm, main_file);
        stack.push((1, imports, 0));
        let mut path_stack: Vec<String> = vec![main_key.clone()];
        while let Some((mi, imports, pos)) = stack.pop() {
            if pos >= imports.len() {
                order.push(mi);
                let key = path_stack.pop().unwrap();
                state.insert(key, State::Done);
                continue;
            }
            let (item_idx, imp) = imports[pos].clone();
            stack.push((mi, imports, pos + 1));
            match imp {
                ImportRef::Name(name, span) => {
                    if self.registry.sources.contains_key(&name) && name != "prelude" {
                        let key = self.load_std_tree(&name, &mut sm, &mut modules, &mut index, &mut order, &mut errors);
                        modules[mi].imports.push((item_idx, key));
                    } else {
                        errors.push(Diag::error(span, format!("unknown module `{name}`")));
                    }
                }
                ImportRef::Path(p, span) => {
                    let dir = modules[mi].dir.clone().unwrap_or_else(|| root.clone());
                    let mut target = dir.join(&p);
                    if target.extension().is_none() {
                        target.set_extension("sand");
                    }
                    let target = normalize_path(&target);
                    if !target.starts_with(&root) {
                        errors.push(Diag::error(span, format!("cannot import `{p}`: the path is outside the module root")));
                        continue;
                    }
                    let key = format!("file:{}", target.display());
                    match state.get(&key) {
                        Some(State::Done) => {
                            modules[mi].imports.push((item_idx, key));
                            continue;
                        }
                        Some(State::Visiting) => {
                            let cycle: Vec<String> = path_stack.iter().map(|k| k.trim_start_matches("file:").to_string()).collect();
                            errors.push(Diag::error(span, format!("import cycle: {} -> {}", cycle.join(" -> "), target.display())));
                            continue;
                        }
                        None => {}
                    }
                    if !self.check_import(&target).await {
                        errors.push(Diag::error(span, format!("cannot import `{p}`: permission denied")));
                        continue;
                    }
                    let text = match std::fs::read_to_string(&target) {
                        Ok(t) => t,
                        Err(e) => {
                            errors.push(Diag::error(span, format!("cannot load module `{p}`: {e}")));
                            continue;
                        }
                    };
                    if text.len() > self.config.limits.max_source_bytes {
                        errors.push(Diag::error(span, format!("cannot load module `{p}`: source too large")));
                        continue;
                    }
                    let display =
                        target.strip_prefix(&root).map(|r| r.display().to_string()).unwrap_or_else(|_| target.display().to_string());
                    let f = sm.add(display, text.as_str());
                    let name = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    let idx = modules.len();
                    modules.push(LoadedModule {
                        name,
                        file: f,
                        kind: ModKind::Script { main: false },
                        imports: vec![],
                        dir: target.parent().map(|d| d.to_path_buf()),
                    });
                    index.insert(key.clone(), idx);
                    modules[mi].imports.push((item_idx, key.clone()));
                    state.insert(key.clone(), State::Visiting);
                    path_stack.push(key);
                    let imports = scan_imports(&sm, f);
                    stack.push((idx, imports, 0));
                }
            }
        }
        if !errors.is_empty() {
            return Err(Error::Compile(errors.iter().map(|d| sm.resolve(d)).collect()));
        }
        let registry = self.registry.clone();

        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(COMPILE_STACK)
                .spawn_scoped(s, || compile_modules(&sm, &modules, &index, &order, &registry))
                .map_err(|e| Error::Io(e.to_string()))?
                .join()
                .map_err(|_| Error::Io("internal compiler error".into()))?
        })
    }

    /// Loads a standard (or host) module and its dependencies; returns its key.
    fn load_std_tree(
        &self,
        name: &str,
        sm: &mut SourceMap,
        modules: &mut Vec<LoadedModule>,
        index: &mut HashMap<String, usize>,
        order: &mut Vec<usize>,
        errors: &mut Vec<Diag>,
    ) -> String {
        let key = format!("std:{name}");
        if index.contains_key(&key) {
            return key;
        }
        if name == "help" {
            // `help.module` can describe any module, so all of them are compiled.
            let others: Vec<String> = self.registry.sources.keys().filter(|k| !matches!(k.as_str(), "prelude" | "help")).cloned().collect();
            for other in others {
                self.load_std_tree(&other, sm, modules, index, order, errors);
            }
        }
        let Some((kind, src)) = self.registry.sources.get(name).cloned() else {
            return key;
        };
        let f = sm.add(format!("<{name}>"), src.as_str());
        let idx = modules.len();
        modules.push(LoadedModule { name: name.to_string(), file: f, kind, imports: vec![], dir: None });
        index.insert(key.clone(), idx);
        for (item, imp) in scan_imports(sm, f) {
            match imp {
                ImportRef::Name(dep, span) => {
                    if self.registry.sources.contains_key(&dep) {
                        let k = self.load_std_tree(&dep, sm, modules, index, order, errors);
                        modules[idx].imports.push((item, k));
                    } else {
                        errors.push(Diag::error(span, format!("unknown module `{dep}`")));
                    }
                }
                ImportRef::Path(_, span) => errors.push(Diag::error(span, "native modules cannot import script files")),
            }
        }
        order.push(idx);
        key
    }
}

fn trap_message(t: Trap) -> String {
    match t {
        Trap::Panic { message, .. } => message,
        Trap::Limit(k) => k.to_string(),
        Trap::Interrupted => "interrupted".into(),
        Trap::Exit(c) => format!("exited with code {c}"),
    }
}

type ImportFrame = (usize, Vec<(usize, ImportRef)>, usize);

#[derive(Clone, Debug)]
enum ImportRef {
    Name(String, Span),
    Path(String, Span),
}

/// Finds the import declarations of a module using the lexer only.
fn scan_imports(sm: &SourceMap, file: u32) -> Vec<(usize, ImportRef)> {
    let text = sm.text(file);
    let toks = lex(text, file).tokens;
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth = 0i32;
    let mut at_start = true;
    while i < toks.len() {
        match &toks[i].tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
            Tok::Import | Tok::From if depth == 0 && at_start => {
                if let Some(t) = toks.get(i + 1) {
                    let span = Span::new(file, t.lo, t.hi);
                    match &t.tok {
                        Tok::Ident => out.push(ImportRef::Name(text[t.lo as usize..t.hi as usize].to_string(), span)),
                        Tok::Str(parts) => {
                            if let [StrPart::Text(s)] = parts.as_slice() {
                                out.push(ImportRef::Path(s.clone(), span));
                            }
                        }
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
    out.into_iter().enumerate().collect()
}

fn compile_modules(
    sm: &SourceMap,
    modules: &[LoadedModule],
    index: &HashMap<String, usize>,
    order: &[usize],
    registry: &Registry,
) -> Result<Compiled, Error> {
    let mut parse_errors = Vec::new();
    let mut parsed: Vec<Option<ParsedModule>> = (0..modules.len()).map(|_| None).collect();
    // Map module index -> position in check order.
    let mut position = vec![0u32; modules.len()];
    for (pos, &mi) in order.iter().enumerate() {
        position[mi] = pos as u32;
    }
    for &mi in order {
        let m = &modules[mi];
        let res = crate::syntax::parse_module(sm.text(m.file), m.file);
        parse_errors.extend(res.errors);
        // Import items in source order; match them with scanned imports by order.
        let mut imports = HashMap::new();
        let mut k = 0;
        for (item_idx, item) in res.module.items.iter().enumerate() {
            if let crate::syntax::ast::ItemKind::Import(_) = item.kind {
                if let Some((_, key)) = m.imports.iter().find(|(scan_k, _)| *scan_k == k)
                    && let Some(&target) = index.get(key)
                {
                    imports.insert(item_idx, position[target]);
                }
                k += 1;
            }
        }
        parsed[mi] = Some(ParsedModule { name: m.name.clone(), file: m.file, ast: res.module, kind: m.kind, imports });
    }
    if !parse_errors.is_empty() {
        return Err(Error::Compile(parse_errors.iter().map(|d| sm.resolve(d)).collect()));
    }
    let ordered: Vec<ParsedModule> = order.iter().map(|&mi| parsed[mi].take().unwrap()).collect();
    let lookup = |name: &str| registry.by_name.get(name).copied();
    let checker = Checker::new(sm, &lookup);
    let checked = checker.check_program(ordered).map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
    let main_file = modules.iter().find(|m| m.kind == (ModKind::Script { main: true })).map(|m| m.file).unwrap_or(0);
    let mut facts = ResolvedFacts::default();
    {
        let f = sm.file(main_file);
        for sp in &checked.lint.unused_results {
            if sp.file == main_file {
                let (l, c) = f.line_col(sp.lo);
                facts.unused_results.push((l, c));
            }
        }
        for lf in &checked.lint.locals {
            if lf.span.file == main_file {
                let (l, c) = f.line_col(lf.span.lo);
                facts.locals.push((l, c, lf.clone()));
            }
        }
        for (name, sp) in &checked.lint.main_imports {
            let (l, c) = f.line_col(sp.lo);
            facts.imports.push((l, c, name.clone()));
        }
        facts.used_names = checked.lint.used_names.iter().filter(|(m, _)| *m == checked.main_module).map(|(_, n)| n.clone()).collect();
    }
    // Help entries for natives.
    let mut native_help: Vec<HelpEntry> = (0..registry.entries.len()).map(|_| HelpEntry { doc: None, signature: String::new() }).collect();
    let mut native_argc = vec![0u16; registry.entries.len()];
    for (i, f) in checked.fns.iter().enumerate() {
        if let crate::check::FnKind::Native(n) = f.kind
            && (n as usize) < native_help.len()
        {
            let printer = crate::types::TypePrinter { defs: &checked.types, params: &checked.params, wk: checked.wk };
            let params: Vec<(String, crate::types::Type, bool)> =
                f.params.iter().map(|p| (p.name.clone(), p.ty.clone(), p.default.is_some())).collect();
            native_help[n as usize] = HelpEntry { doc: f.doc.clone(), signature: printer.signature(&f.name, &params, &f.ret) };
            let reflect = f.generics.iter().filter(|g| checked.params[**g as usize].reflect).count();
            native_argc[n as usize] = (f.params.len() + reflect) as u16;
            let _ = i;
        }
    }
    let mut program =
        crate::compile::compile(crate::compile::CompileInput { checked: &checked, sm, native_lookup: &lookup, native_help, native_argc })
            .map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
    program.available_modules = registry.sources.keys().filter(|k| k.as_str() != "prelude").cloned().collect();
    Ok(Compiled { program: Arc::new(program), warnings: vec![], facts: Some(facts) })
}
