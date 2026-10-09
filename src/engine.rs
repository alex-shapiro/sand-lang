//! The embedding API: `Engine`, `Config`, errors and module loading.

use crate::check::budget::{CompileBudget, Stop};
use crate::check::{Checker, ModKind, ParsedModule};
use crate::diag::{Diag, Diagnostic, SourceMap, Span};
use crate::native::NativeModule;
use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter, normalize_path};
use crate::session::Session;
use crate::stdlib;
use crate::syntax::lexer::{StrPart, Tok, lex};
use crate::vm::bytecode::{HelpEntry, Program};
use crate::vm::heap::GcStats;
use crate::vm::sched::{IoShared, IoTable, RunEnd};
use crate::vm::{Host, NativeEntry, OutputSink, Rng, SharedSink, StdinSource, TraceEntry, Trap, Vm, entropy_seed};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Total size of the program's source files (the main file and every
    /// imported script; the standard library is not counted).
    pub max_source_bytes: usize,
    /// Work allowed for type checking, in compile fuel units (about one per
    /// expression checked, unification step, type node and pattern row; see
    /// `check::budget`). `None` means unlimited. Compilation is also bounded
    /// by `max_wall_time` (counted from the start of compilation) and can be
    /// stopped with an `InterruptHandle`.
    pub max_compile_fuel: Option<u64>,
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
            max_compile_fuel: Some(crate::check::budget::DEFAULT_COMPILE_FUEL),
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

const STDOUT_BUFFER: usize = 8 << 10;

struct StdoutBuffer {
    data: Vec<u8>,
    tty: Option<bool>,
}

static STDOUT: Mutex<StdoutBuffer> = Mutex::new(StdoutBuffer { data: Vec::new(), tty: None });

fn flush_stdout(b: &mut StdoutBuffer) -> std::io::Result<()> {
    use std::io::Write;
    if b.data.is_empty() {
        return Ok(());
    }
    let mut o = std::io::stdout().lock();
    let r = o.write_all(&b.data).and_then(|()| o.flush());
    b.data.clear();
    r
}

impl OutputSink for StdoutSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::IsTerminal;
        let mut b = STDOUT.lock().unwrap_or_else(|e| e.into_inner());
        let tty = *b.tty.get_or_insert_with(|| std::io::stdout().is_terminal());
        b.data.extend_from_slice(data);
        if b.data.len() >= STDOUT_BUFFER || (tty && data.contains(&b'\n')) {
            flush_stdout(&mut b)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        flush_stdout(&mut STDOUT.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn flush_before_input(&mut self) -> std::io::Result<()> {
        let mut b = STDOUT.lock().unwrap_or_else(|e| e.into_inner());
        // Like C's stdio: only a line-buffered (terminal) stream.
        if b.tty == Some(true) { flush_stdout(&mut b) } else { Ok(()) }
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
    pub(crate) gc_stress: bool,
    pub(crate) exit_after_run: bool,
    pub(crate) tls: crate::stdlib::TlsSettings,
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
            gc_stress: std::env::var_os("SAND_GC_STRESS").is_some(),
            exit_after_run: false,
            tls: Default::default(),
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
    /// Testing aid: run a garbage collection at every safe point (every
    /// loop iteration and call), alternating minor and major collections.
    /// Very slow. Also enabled by the `SAND_GC_STRESS` environment variable.
    #[doc(hidden)]
    pub fn gc_stress(mut self, on: bool) -> Self {
        self.gc_stress = on;
        self
    }
    /// The process exits right after `run_file`/`run_source` returns
    #[doc(hidden)]
    pub fn exit_after_run(mut self, on: bool) -> Self {
        self.exit_after_run = on;
        self
    }
    /// Trusts additional root certificates (DER-encoded X.509) for TLS
    /// connections (`https`, `net.connect_tls`), besides the bundled web PKI
    /// roots. An invalid certificate makes TLS connections fail. Without the
    /// `tls` feature this has no effect.
    pub fn tls_root_certificates(mut self, certs: Vec<Vec<u8>>) -> Self {
        self.tls = self.tls.with_roots(certs);
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
    /// Lazily computed `help.module` summaries.
    pub help: Arc<ModuleHelp>,
}

impl Registry {
    fn build(config: &Config) -> Registry {
        let mut r = Registry { entries: Vec::new(), by_name: HashMap::new(), sources: BTreeMap::new(), help: Arc::default() };
        stdlib::register(&mut r);
        for m in &config.modules {
            m.register(&mut r);
        }
        r.help = Arc::new(ModuleHelp { sources: r.sources.clone(), cache: Mutex::default() });
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
    /// "Always" prompt answers, remembered for the lifetime of the engine
    /// (SPEC 14.3): shared by import checks and every run.
    always: Arc<Mutex<HashMap<Capability, bool>>>,
    gc_stats: GcStats,
}

pub(crate) struct LoadedModule {
    pub name: String,
    pub file: u32,
    pub kind: ModKind,
    /// Import item index -> module key.
    pub imports: Vec<(usize, String)>,
    pub dir: Option<PathBuf>,
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
    /// Unused private functions and methods: (line, column, display name, is method).
    pub unused_fns: Vec<(usize, usize, String, bool)>,
}

pub(crate) const COMPILE_STACK: usize = 256 << 20;

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
            always: Arc::default(),
            gc_stats: GcStats::default(),
        }
    }

    /// Garbage collector statistics of the most recent run.
    pub fn gc_stats(&self) -> GcStats {
        self.gc_stats
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

    /// Starts an interactive session ([`Session`]) with this engine's
    /// configuration. The session shares the engine's "always" answers to
    /// permission prompts, but not its interrupt handle: use
    /// [`Session::interrupt_handle`].
    pub fn session(&self) -> Session {
        Session::new(Engine {
            config: self.config.clone(),
            registry: self.registry.clone(),
            natives: self.natives.clone(),
            interrupt: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(IoShared::default()),
            always: self.always.clone(),
            gc_stats: GcStats::default(),
        })
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    /// Forgets interrupts of earlier runs; interrupts from now on stop the
    /// next compilation or run.
    pub(crate) fn clear_interrupt(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
    }

    /// Prepares a VM that persists between runs (a session's) for a run.
    pub(crate) fn prepare_run(&self, vm: &mut Vm) {
        self.interrupt.store(false, Ordering::SeqCst);
        // "Always" answers given since (to imports, or by other runs).
        let always = self.always.lock().unwrap_or_else(|e| e.into_inner());
        vm.host.always.extend(always.iter().map(|(c, d)| (c.clone(), *d)));
    }

    /// Records a run's "always" answers and flushes its output.
    /// A compile budget for one compilation starting now (REPL inputs).
    pub(crate) fn compile_budget(&self) -> Arc<CompileBudget> {
        let limits = &self.config.limits;
        let deadline = limits.max_wall_time.and_then(|t| Instant::now().checked_add(t));
        Arc::new(CompileBudget::new(limits.max_compile_fuel, deadline, Some(self.interrupt.clone())))
    }

    pub(crate) fn finish_run(&self, vm: &Vm) {
        self.remember_always(&vm.host.always);
        if let Ok(mut s) = self.config.stdout.lock() {
            let _ = s.flush();
        }
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
        crate::unwind::guard(self.run_source_inner(name, source)).await
    }

    async fn run_source_inner(&mut self, name: &str, source: &str) -> Result<crate::Outcome, Error> {
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
        crate::unwind::guard(self.run_file_inner(path.as_ref())).await
    }

    async fn run_file_inner(&mut self, path: &Path) -> Result<crate::Outcome, Error> {
        let (name, source, root) = self.read_main(path)?;
        let compiled = self.compile(&name, &source, root.clone(), Some(path)).await?;
        let cwd = self.config.cwd.clone().unwrap_or(root);
        self.execute(compiled.program, cwd).await
    }

    pub async fn run_tests(&mut self, path: impl AsRef<Path>) -> Result<TestReport, Error> {
        crate::unwind::guard(self.run_tests_inner(path.as_ref())).await
    }

    async fn run_tests_inner(&mut self, path: &Path) -> Result<TestReport, Error> {
        let (name, source, root) = self.read_main(path)?;
        let compiled = self.compile(&name, &source, root.clone(), Some(path)).await?;
        let cwd = self.config.cwd.clone().unwrap_or(root);
        let prog = compiled.program;
        let mut report = TestReport::default();
        for (tname, proto) in prog.tests.clone() {
            let mut vm = self.new_vm(prog.clone(), cwd.clone(), true);
            let init = vm.run_main(prog.entry).await;
            let result = match init {
                Ok(RunEnd::Finished(_)) => {
                    vm.ready.clear();
                    match vm.run_main(proto).await {
                        Ok(RunEnd::Finished(_)) | Ok(RunEnd::Exit(0)) => Ok(()),
                        Ok(RunEnd::Exit(c)) => Err(format!("exited with code {c}")),
                        Err(t) => Err(trap_message(t)),
                    }
                }
                Ok(RunEnd::Exit(c)) => Err(format!("module exited with code {c}")),
                Err(t) => Err(trap_message(t)),
            };
            self.remember_always(&vm.host.always);
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

    pub(crate) fn new_vm(&self, prog: Arc<Program>, cwd: PathBuf, quiet: bool) -> Vm {
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
            always: self.always.lock().unwrap_or_else(|e| e.into_inner()).clone(),
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
            stream_decisions: [None; 2],
            tls: c.tls.clone(),
            regex_cache: Vec::new(),
        };
        let mut vm = Vm::new(prog, self.natives.clone(), host);
        vm.io = IoTable::with_shared(self.shared.clone());
        if c.gc_stress {
            vm.heap.stress = true;
            vm.heap.next_gc = 0;
        }
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
        let mut vm = self.new_vm(prog.clone(), cwd, false);
        let r = vm.run_main(prog.entry).await;
        self.finish_run(&vm);
        self.gc_stats = vm.heap.stats;
        if self.config.exit_after_run {
            std::mem::forget(vm);
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

    /// Records "always" answers given during a run.
    pub(crate) fn remember_always(&self, answers: &HashMap<Capability, bool>) {
        if !answers.is_empty() {
            self.always.lock().unwrap_or_else(|e| e.into_inner()).extend(answers.iter().map(|(c, d)| (c.clone(), *d)));
        }
    }

    /// Checks (and if need be, prompts for) importing the script at `path`.
    async fn check_import(&self, path: &Path) -> bool {
        let cap = Capability::Import(path.to_path_buf());
        if let Some(&d) = self.always.lock().unwrap_or_else(|e| e.into_inner()).get(&cap) {
            return d;
        }
        match self.config.policy.check(&cap) {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Prompt => match &self.config.prompter {
                Some(p) => {
                    let answer = p.prompt(cap.clone()).await;
                    let allowed = matches!(answer, PromptAnswer::AllowOnce | PromptAnswer::AllowAlways);
                    if matches!(answer, PromptAnswer::AllowAlways | PromptAnswer::DenyAlways) {
                        self.always.lock().unwrap_or_else(|e| e.into_inner()).insert(cap, allowed);
                    }
                    allowed
                }
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
        // A new run: forget interrupts of previous runs. Interrupts from now
        // on stop compilation or execution.
        self.interrupt.store(false, Ordering::SeqCst);
        let started = Instant::now();
        let mut g = ModuleGraph::default();
        let mut errors: Vec<Diag> = Vec::new();
        g.source_bytes = main_source.len();
        if main_source.len() > self.config.limits.max_source_bytes {
            let f = g.sm.add(main_name, "");
            errors.push(Diag::error(Span::new(f, 0, 0), "source too large"));
            return Err(Error::Compile(errors.iter().map(|d| g.sm.resolve(d)).collect()));
        }
        // The directory of the main file itself (a symlink to it resolved),
        // like the default module root (`read_main`), so that the main
        // file's imports resolve as they do next to the file.
        let main_dir = main_path
            .and_then(|p| std::path::absolute(p).ok())
            .and_then(|p| normalize_path(&p).parent().map(Path::to_path_buf))
            .unwrap_or_else(|| root.clone());
        let root = normalize_path(&root);

        // Discover modules depth-first, in dependency order.
        // Built-in modules: prelude and task.
        g.load_prelude(&self.registry.sources);

        let main_file = g.sm.add(main_name, main_source);
        let main_key = format!(
            "file:{}",
            main_path
                .map(|p| normalize_path(&std::path::absolute(p).unwrap_or_default()).display().to_string())
                .unwrap_or_else(|| main_name.to_string())
        );
        let main_mod_name = Path::new(main_name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "main".into());
        g.modules.push(LoadedModule {
            name: main_mod_name,
            file: main_file,
            kind: ModKind::Script { main: true },
            imports: vec![],
            dir: Some(main_dir),
        });
        g.index.insert(main_key.clone(), 1);

        // Every module implicitly depends on `task`, so it is loaded first.
        g.load_std_tree(&self.registry.sources, "task", &mut errors);
        if let Some(stop) = self.discover(&mut g, 1, main_key, &root, &mut errors).await {
            return Err(stop);
        }
        if !errors.is_empty() {
            return Err(Error::Compile(errors.iter().map(|d| g.sm.resolve(d)).collect()));
        }
        let registry = self.registry.clone();
        let limits = &self.config.limits;
        let budget = BudgetConfig {
            fuel: limits.max_compile_fuel,
            deadline: limits.max_wall_time.and_then(|t| started.checked_add(t)),
            interrupt: self.interrupt.clone(),
        };

        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(COMPILE_STACK)
                .spawn_scoped(s, || compile_modules(&g.sm, &g.modules, &g.index, &g.order, &registry, budget))
                .map_err(|e| Error::Io(e.to_string()))?
                .join()
                .map_err(|_| Error::Io("internal compiler error".into()))?
        })
    }

    /// Loads the modules imported by module `start` (whose key is `key`) and,
    /// depth first, their imports, checking script imports against the
    /// policy. Appends the new modules to `g.order` in dependency order,
    /// followed by `start`. Modules already in `g` are not loaded again.
    ///
    /// This runs before the compile budget exists, so it is bounded on its
    /// own: overlong import paths are refused, discovery stops after
    /// [`MAX_DISCOVERY_ERRORS`] errors, and it returns the error to report
    /// if the engine is interrupted or the wall-time limit passes.
    pub(crate) async fn discover(
        &self,
        g: &mut ModuleGraph,
        start: usize,
        key: String,
        root: &Path,
        errors: &mut Vec<Diag>,
    ) -> Option<Error> {
        let deadline = self.config.limits.max_wall_time.and_then(|t| Instant::now().checked_add(t));
        // DFS stack: (module index, its imports, next import to visit), and
        // the keys of the modules on it (to detect cycles).
        let mut stack: Vec<ImportFrame> = vec![(start, scan_imports(&g.sm, g.modules[start].file), 0)];
        let mut path_stack: Vec<String> = vec![key];
        while let Some((mi, imports, pos)) = stack.pop() {
            if self.interrupt.load(Ordering::Relaxed) {
                return Some(Error::Interrupted);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Some(Error::Limit(LimitKind::WallTime));
            }
            if errors.len() >= MAX_DISCOVERY_ERRORS {
                return None;
            }
            if pos >= imports.len() {
                g.order.push(mi);
                path_stack.pop();
                continue;
            }
            let (item_idx, imp) = imports[pos].clone();
            stack.push((mi, imports, pos + 1));
            match imp {
                ImportRef::Name(name, span) => {
                    if self.registry.sources.contains_key(&name) && name != "prelude" {
                        let key = g.load_std_tree(&self.registry.sources, &name, errors);
                        g.modules[mi].imports.push((item_idx, key));
                    } else {
                        errors.push(Diag::error(span, format!("unknown module `{name}`")));
                    }
                }
                ImportRef::Path(p, span) => {
                    if p.len() > crate::policy::MAX_PATH_BYTES {
                        errors.push(Diag::error(span, "cannot import: the path is too long"));
                        continue;
                    }
                    let dir = g.modules[mi].dir.clone().unwrap_or_else(|| root.to_path_buf());
                    let mut target = dir.join(&p);
                    if target.extension().is_none() {
                        target.set_extension("sand");
                    }
                    let target = normalize_path(&target);
                    if !target.starts_with(root) {
                        errors.push(Diag::error(span, format!("cannot import `{p}`: the path is outside the module root")));
                        continue;
                    }
                    let key = format!("file:{}", target.display());
                    if path_stack.contains(&key) {
                        let cycle: Vec<String> = path_stack.iter().map(|k| k.trim_start_matches("file:").to_string()).collect();
                        errors.push(Diag::error(span, format!("import cycle: {} -> {}", cycle.join(" -> "), target.display())));
                        continue;
                    }
                    if g.index.contains_key(&key) {
                        g.modules[mi].imports.push((item_idx, key));
                        continue;
                    }
                    if !self.check_import(&target).await {
                        errors.push(Diag::error(span, format!("cannot import `{p}`: permission denied")));
                        // Compilation has failed: do not prompt for further imports.
                        break;
                    }
                    // Read the checked file without following symlinks swapped in since.
                    let text = match crate::stdlib::read_source(&target, self.config.limits.max_source_bytes) {
                        Ok(t) => t,
                        Err(e) => {
                            errors.push(Diag::error(span, format!("cannot load module `{p}`: {e}")));
                            continue;
                        }
                    };
                    g.source_bytes = g.source_bytes.saturating_add(text.len());
                    if g.source_bytes > self.config.limits.max_source_bytes {
                        errors.push(Diag::error(
                            span,
                            format!("cannot load module `{p}`: source too large (the program's sources exceed the limit in total)"),
                        ));
                        continue;
                    }
                    let display =
                        target.strip_prefix(root).map(|r| r.display().to_string()).unwrap_or_else(|_| target.display().to_string());
                    let f = g.sm.add(display, text.as_str());
                    let name = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    let idx = g.modules.len();
                    g.modules.push(LoadedModule {
                        name,
                        file: f,
                        kind: ModKind::Script { main: false },
                        imports: vec![],
                        dir: target.parent().map(|d| d.to_path_buf()),
                    });
                    g.index.insert(key.clone(), idx);
                    g.modules[mi].imports.push((item_idx, key.clone()));
                    path_stack.push(key);
                    let imports = scan_imports(&g.sm, f);
                    stack.push((idx, imports, 0));
                }
            }
        }
        None
    }
}

/// Module discovery stops after this many errors (they are all reported).
const MAX_DISCOVERY_ERRORS: usize = 100;

/// The modules of a compilation (or of a REPL session, which adds to them).
#[derive(Default)]
pub(crate) struct ModuleGraph {
    pub sm: SourceMap,
    pub modules: Vec<LoadedModule>,
    /// Module key (`std:name` or `file:path`) -> index in `modules`.
    pub index: HashMap<String, usize>,
    /// Module indices in dependency (check) order.
    pub order: Vec<usize>,
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
    pub(crate) source_bytes: usize,
}

impl ModuleGraph {
    /// Adds the prelude as module 0.
    pub(crate) fn load_prelude(&mut self, sources: &BTreeMap<String, (ModKind, String)>) {
        let prelude_src = sources.get("prelude").map(|(_, s)| s.clone()).unwrap_or_default();
        let f = self.sm.add("<prelude>", prelude_src.as_str());
        self.modules.push(LoadedModule { name: "prelude".into(), file: f, kind: ModKind::Prelude, imports: vec![], dir: None });
        self.index.insert("std:prelude".into(), 0);
        self.order.push(0);
    }

    pub(crate) fn mark(&self) -> GraphMark {
        GraphMark { files: self.sm.files.len(), modules: self.modules.len(), order: self.order.len(), source_bytes: self.source_bytes }
    }

    pub(crate) fn rollback(&mut self, m: &GraphMark) {
        self.sm.files.truncate(m.files);
        self.modules.truncate(m.modules);
        self.order.truncate(m.order);
        self.source_bytes = m.source_bytes;
        self.index.retain(|_, i| *i < m.modules);
    }

    /// Loads a standard (or host) module and its dependencies; returns its key.
    pub(crate) fn load_std_tree(&mut self, sources: &BTreeMap<String, (ModKind, String)>, name: &str, errors: &mut Vec<Diag>) -> String {
        let key = format!("std:{name}");
        if self.index.contains_key(&key) {
            return key;
        }
        let Some((kind, src)) = sources.get(name) else {
            return key;
        };
        let f = self.sm.add(format!("<{name}>"), src.as_str());
        let idx = self.modules.len();
        self.modules.push(LoadedModule { name: name.to_string(), file: f, kind: *kind, imports: vec![], dir: None });
        self.index.insert(key.clone(), idx);
        for (item, imp) in scan_imports(&self.sm, f) {
            match imp {
                ImportRef::Name(dep, span) => {
                    if sources.contains_key(&dep) {
                        let k = self.load_std_tree(sources, &dep, errors);
                        self.modules[idx].imports.push((item, k));
                    } else {
                        errors.push(Diag::error(span, format!("unknown module `{dep}`")));
                    }
                }
                ImportRef::Path(_, span) => errors.push(Diag::error(span, "native modules cannot import script files")),
            }
        }
        self.order.push(idx);
        key
    }
}

/// Summaries of standard and host modules for `help.module`, computed on the
/// first request and cached for the lifetime of the engine, so that importing
/// `help` costs nothing extra.
#[derive(Default)]
pub struct ModuleHelp {
    sources: BTreeMap<String, (ModKind, String)>,
    cache: Mutex<HelpCache>,
}

#[derive(Default)]
struct HelpCache {
    /// Whether summarising all modules at once has been attempted.
    all_tried: bool,
    summaries: HashMap<String, Option<String>>,
}

impl fmt::Debug for ModuleHelp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleHelp").finish_non_exhaustive()
    }
}

impl ModuleHelp {
    /// The summary of module `name`, or `None` if there is no such module.
    pub(crate) fn summary(&self, name: &str) -> Option<String> {
        if name == "prelude" || !self.sources.contains_key(name) {
            return None;
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.all_tried {
            // One checker pass over every module costs about as much as one over
            // a single module (checking the prelude dominates).
            cache.all_tried = true;
            let names: Vec<&str> = self.sources.keys().map(String::as_str).collect();
            for (m, s) in self.summarise(&names).unwrap_or_default() {
                cache.summaries.insert(m, Some(s));
            }
        }
        if let Some(s) = cache.summaries.get(name) {
            return s.clone();
        }
        // Checking everything failed (e.g. a broken host module): just this one.
        let s = self.summarise(&[name]).and_then(|all| all.into_iter().find(|(m, _)| m == name).map(|(_, s)| s));
        cache.summaries.insert(name.to_string(), s.clone());
        s
    }

    /// Type checks the named modules (with their dependencies) and returns the
    /// summaries of all standard and host modules involved.
    fn summarise(&self, names: &[&str]) -> Option<Vec<(String, String)>> {
        let check = || {
            let mut g = ModuleGraph::default();
            let mut errors = Vec::new();
            for m in ["prelude", "task"].iter().chain(names) {
                g.load_std_tree(&self.sources, m, &mut errors);
            }
            if !errors.is_empty() {
                return None;
            }
            let parsed = parse_modules(&g.sm, &g.modules, &g.index, &g.order, &CompileBudget::default(), 0).ok()?;
            // Native implementations are irrelevant to summaries.
            let lookup = |_: &str| Some(0);
            let checked = Checker::new(&g.sm, &lookup).check_program(parsed).ok()?;
            Some(checked.modules.into_iter().filter(|m| !matches!(m.kind, ModKind::Script { .. })).map(|m| (m.name, m.summary)).collect())
        };
        std::thread::scope(|s| std::thread::Builder::new().stack_size(COMPILE_STACK).spawn_scoped(s, check).ok()?.join().ok()?)
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

/// Limits for compilation (see `check::budget`).
struct BudgetConfig {
    fuel: Option<u64>,
    deadline: Option<Instant>,
    interrupt: Arc<AtomicBool>,
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

/// Parses modules (in check order) and links their imports.
fn parse_modules(
    sm: &SourceMap,
    modules: &[LoadedModule],
    index: &HashMap<String, usize>,
    order: &[usize],
    budget: &CompileBudget,
    main_file: u32,
) -> Result<Vec<ParsedModule>, Error> {
    // Map module index -> position in check order.
    let mut position = vec![0u32; modules.len()];
    for (pos, &mi) in order.iter().enumerate() {
        position[mi] = pos as u32;
    }
    if !budget.check() {
        return Err(budget_error(budget.stopped().unwrap_or(Stop::Time), sm, main_file));
    }
    parse_in_order(sm, modules, index, order, |mi| Some(position[mi]))
}

/// Parses the modules `order` (indices into `modules`) and links their
/// imports to the ids the checker knows the imported modules by (`mod_id`).
pub(crate) fn parse_in_order(
    sm: &SourceMap,
    modules: &[LoadedModule],
    index: &HashMap<String, usize>,
    order: &[usize],
    mod_id: impl Fn(usize) -> Option<crate::types::ModId>,
) -> Result<Vec<ParsedModule>, Error> {
    let mut parse_errors = Vec::new();
    let mut parsed = Vec::new();
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
                    && let Some(id) = mod_id(target)
                {
                    imports.insert(item_idx, id);
                }
                k += 1;
            }
        }
        parsed.push(ParsedModule { name: m.name.clone(), file: m.file, ast: res.module, kind: m.kind, imports });
    }
    if !parse_errors.is_empty() {
        return Err(Error::Compile(parse_errors.iter().map(|d| sm.resolve(d)).collect()));
    }
    Ok(parsed)
}

fn compile_modules(
    sm: &SourceMap,
    modules: &[LoadedModule],
    index: &HashMap<String, usize>,
    order: &[usize],
    registry: &Registry,
    budget: BudgetConfig,
) -> Result<Compiled, Error> {
    let budget = Arc::new(CompileBudget::new(budget.fuel, budget.deadline, Some(budget.interrupt)));
    let main_file = modules.iter().find(|m| m.kind == (ModKind::Script { main: true })).map(|m| m.file).unwrap_or(0);
    let ordered = parse_modules(sm, modules, index, order, &budget, main_file)?;
    let lookup = |name: &str| registry.by_name.get(name).copied();
    let mut checker = Checker::new(sm, &lookup);
    checker.budget = budget.clone();
    let checked = checker.check_program(ordered);
    if let Some(stop) = budget.stopped() {
        // Diagnostics after the budget ran out are not meaningful.
        return Err(budget_error(stop, sm, main_file));
    }
    let checked = checked.map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
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
        for id in crate::lint::unused_functions(&checked) {
            let fi = &checked.fns[id as usize];
            if fi.span.file == main_file {
                let (l, c) = f.line_col(fi.span.lo);
                let name = match fi.owner {
                    Some(crate::check::Ctor::Def(d)) => format!("{}.{}", checked.types[d as usize].name, fi.name),
                    _ => fi.name.clone(),
                };
                facts.unused_fns.push((l, c, name, fi.owner.is_some()));
            }
        }
    }
    let (native_help, native_argc) = native_info(&checked, registry);
    let input = crate::compile::CompileInput { checked: &checked, sm, native_lookup: &lookup, native_help, native_argc, budget: &budget };
    let program = crate::compile::compile(input);
    if let Some(stop) = budget.stopped() {
        return Err(budget_error(stop, sm, main_file));
    }
    let mut program = program.map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
    program.available_modules = registry.sources.keys().filter(|k| k.as_str() != "prelude").cloned().collect();
    program.help_index = Some(registry.help.clone());
    Ok(Compiled { program: Arc::new(program), warnings: vec![], facts: Some(facts) })
}

/// Help entries and argument counts (by native id) of the natives declared
/// by the checked modules.
pub(crate) fn native_info(checked: &crate::check::CheckedProgram, registry: &Registry) -> (Vec<HelpEntry>, Vec<u16>) {
    let mut native_help: Vec<HelpEntry> = (0..registry.entries.len()).map(|_| HelpEntry { doc: None, signature: String::new() }).collect();
    let mut native_argc = vec![0u16; registry.entries.len()];
    for f in &checked.fns {
        if let crate::check::FnKind::Native(n) = f.kind
            && (n as usize) < native_help.len()
        {
            let printer = crate::types::TypePrinter { defs: &checked.types, params: &checked.params, wk: checked.wk };
            let params: Vec<(String, crate::types::Type, bool)> =
                f.params.iter().map(|p| (p.name.clone(), p.ty.clone(), p.default.is_some())).collect();
            native_help[n as usize] = HelpEntry { doc: f.doc.clone(), signature: printer.signature(&f.name, &params, &f.ret) };
            let reflect = f.generics.iter().filter(|g| checked.params[**g as usize].reflect).count();
            native_argc[n as usize] = (f.params.len() + reflect) as u16;
        }
    }
    (native_help, native_argc)
}
