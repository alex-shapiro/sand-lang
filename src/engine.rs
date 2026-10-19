//! The embedding API: `Engine`, `Config`, errors and module loading.

use crate::check::budget::CompileBudget;
use crate::check::{ModKind, NativeIds};
use crate::compile::{CompileInput, compile_more};
use crate::diag::{Diag, Diagnostic, FileId, Span};
use crate::files::{Fs, MemFs};
use crate::modules::{ImportRef, ModuleGraph, Snapshot, Target, budget_error, native_info};
use crate::native::NativeModule;
use crate::platform::{Clock, Instant, SystemClock};
use crate::policy::{Capability, Decision, Policy, PromptAnswer, Prompter};
use crate::session::Session;
use crate::stdlib;
use crate::types::{FnId, ModId};
use crate::vm::bytecode::Program;
use crate::vm::heap::GcStats;
use crate::vm::sched::{IoShared, IoTable, RunEnd};
use crate::vm::{Always, Host, NativeEntry, OutputSink, Rng, SharedSink, StdinSource, TraceEntry, Trap, Vm, entropy_seed};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
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
    /// Total size of the main file and imported scripts, not counting the
    /// standard library.
    pub max_source_bytes: usize,
    /// Work allowed for type checking, in compile fuel units; `None` means
    /// unlimited. Compilation also counts against `max_wall_time`.
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
    /// The script failed with an error; `message` is `kind: message`.
    Runtime {
        message: String,
        trace: Vec<TraceFrame>,
    },
    /// An invariant of the engine broke: a bug in Sand or in a host module.
    Internal {
        message: String,
        trace: Vec<TraceFrame>,
    },
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
                // A message of kind `error` already starts with the word.
                let message = message.strip_prefix("error: ").unwrap_or(message);
                write!(f, "error: {message}")?;
                write_trace(f, trace)
            }
            Error::Internal { message, trace } => {
                write!(f, "internal error: {message}")?;
                write_trace(f, trace)
            }
            Error::Limit(k) => write!(f, "{k}"),
            Error::Interrupted => write!(f, "interrupted"),
            Error::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

fn write_trace(f: &mut fmt::Formatter<'_>, trace: &[TraceFrame]) -> fmt::Result {
    for t in trace {
        write!(f, "\n    at {} ({}:{})", t.function, t.file, t.line)?;
    }
    Ok(())
}

impl Error {
    /// The exit status of the `sand` tools for the error: 1 for a runtime
    /// error, 2 for compile and I/O errors, 3 for a limit, 70 for an internal
    /// error, 130 if interrupted.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Runtime { .. } => 1,
            Error::Internal { .. } => 70,
            Error::Compile(_) | Error::Io(_) => 2,
            Error::Limit(_) => 3,
            Error::Interrupted => 130,
        }
    }

    /// The error as tools report it, ending with a line break: diagnostics
    /// with an excerpt of their source line, else the message.
    pub fn render(&self) -> String {
        match self {
            Error::Compile(diags) => diags.iter().map(Diagnostic::render).collect(),
            e => format!("{e}\n"),
        }
    }
}

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
    fn data(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.data()).into_owned()
    }
    pub fn bytes(&self) -> Vec<u8> {
        self.data().clone()
    }
    pub fn clear(&self) {
        self.data().clear();
    }
}

impl OutputSink for MemorySink {
    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        self.data().extend_from_slice(data);
        Ok(())
    }
}

struct NullSink;
impl OutputSink for NullSink {
    fn write(&self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
enum StdinConfig {
    None,
    Bytes(Vec<u8>),
    #[cfg(feature = "os")]
    Inherit,
    Feed(crate::StdinFeed),
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
    pub(crate) library: Option<PathBuf>,
    pub(crate) args: Vec<String>,
    pub(crate) env: Option<Vec<(String, String)>>,
    pub(crate) seed: Option<u64>,
    pub(crate) gc_stress: bool,
    pub(crate) optimize: bool,
    pub(crate) dump_bytecode: bool,
    pub(crate) exit_after_run: bool,
    #[cfg(feature = "os")]
    pub(crate) tls: crate::stdlib::TlsSettings,
    pub(crate) fs: Fs,
    pub(crate) request_filter: Option<RequestFilter>,
    pub(crate) clock: Arc<dyn Clock>,
    #[cfg(feature = "os")]
    pub(crate) network: Option<Arc<dyn crate::network::Network>>,
}

/// HTTP request destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestTarget {
    /// `http` or `https`.
    pub scheme: String,
    /// The lowercased host name or an IP address.
    pub host: String,
    pub port: u16,
}

/// Rewrites outgoing HTTP request headers or rejects the request with a reason.
pub type RequestFilter = Arc<dyn Fn(&RequestTarget, &mut Vec<(String, String)>) -> Result<(), String> + Send + Sync>;

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
            stdout: Arc::new(crate::stdio::StdoutSink::default()),
            stderr: Arc::new(crate::stdio::StderrSink::default()),
            stdin: StdinConfig::None,
            cwd: None,
            module_root: None,
            library: None,
            args: Vec::new(),
            env: None,
            seed: None,
            gc_stress: false,
            optimize: true,
            dump_bytecode: false,
            exit_after_run: false,
            #[cfg(feature = "os")]
            tls: Default::default(),
            fs: Fs::default(),
            request_filter: None,
            clock: Arc::new(SystemClock),
            #[cfg(feature = "os")]
            network: None,
        }
    }

    /// Sets the clock and timer provider for the engine.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sets the network provider for host resolution and outgoing TCP connections.
    #[cfg(feature = "os")]
    pub fn network(mut self, network: Arc<dyn crate::network::Network>) -> Self {
        self.network = Some(network);
        self
    }

    /// Filters the headers of every HTTP request.
    pub fn request_filter(
        mut self,
        filter: impl Fn(&RequestTarget, &mut Vec<(String, String)>) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.request_filter = Some(Arc::new(filter));
        self
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
    /// Adds a host module.
    ///
    /// # Panics
    ///
    /// If a module of the same name was added already.
    pub fn module(mut self, m: NativeModule) -> Self {
        assert!(!self.modules.iter().any(|x| x.name() == m.name()), "host module `{}` is added twice", m.name());
        self.modules.push(m);
        self
    }
    pub fn stdout(mut self, s: impl OutputSink + 'static) -> Self {
        self.stdout = Arc::new(s);
        self
    }
    pub fn stderr(mut self, s: impl OutputSink + 'static) -> Self {
        self.stderr = Arc::new(s);
        self
    }
    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = StdinConfig::Bytes(data);
        self
    }
    /// Uses the host process's standard input.
    #[cfg(feature = "os")]
    pub fn inherit_stdin(mut self) -> Self {
        self.stdin = StdinConfig::Inherit;
        self
    }
    /// Scripts read the input the host sends through `feed`.
    pub fn stdin_feed(mut self, feed: &crate::StdinFeed) -> Self {
        self.stdin = StdinConfig::Feed(feed.clone());
        self
    }
    /// Scripts, their imports and the main file of [`Engine::run_file`] use
    /// `fs` instead of the host's file system, with [`Config::cwd`] and
    /// [`Config::module_root`] as paths in `fs` (by default its root).
    /// Without the `os` feature, engines have an empty memory file system.
    pub fn memory_fs(mut self, fs: MemFs) -> Self {
        self.fs = Fs::Memory(fs);
        self
    }
    /// The directory relative paths of the script start from (by default
    /// the module root).
    pub fn cwd(mut self, p: impl AsRef<Path>) -> Self {
        self.cwd = Some(p.as_ref().to_path_buf());
        self
    }
    pub fn module_root(mut self, p: impl AsRef<Path>) -> Self {
        self.module_root = Some(p.as_ref().to_path_buf());
        self
    }
    /// Sets the library: a directory of script modules that `import name`
    /// loads (as `<dir>/name.sand`) when no standard or host module has that name.
    pub fn library(mut self, dir: impl AsRef<Path>) -> Self {
        self.library = Some(dir.as_ref().to_path_buf());
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
    /// Very slow.
    #[doc(hidden)]
    pub fn gc_stress(mut self, on: bool) -> Self {
        self.gc_stress = on;
        self
    }
    /// Enables or disables loop fusion, loop specialization, inlining, and
    /// loop-invariant hoisting.
    #[doc(hidden)]
    pub fn optimize(mut self, on: bool) -> Self {
        self.optimize = on;
        self
    }
    /// Debugging aid: print the bytecode of the program's own functions to
    /// standard error before running it.
    #[doc(hidden)]
    pub fn dump_bytecode(mut self, on: bool) -> Self {
        self.dump_bytecode = on;
        self
    }
    /// The process exits right after `run_file`/`run_source` returns.
    #[doc(hidden)]
    pub fn exit_after_run(mut self, on: bool) -> Self {
        self.exit_after_run = on;
        self
    }
    /// Trusts additional root certificates (DER-encoded X.509) for TLS
    /// connections (`https`, `net.connect_tls`), besides the bundled web PKI
    /// roots. An invalid certificate makes TLS connections fail. Without the
    /// `tls` feature this has no effect.
    #[cfg(feature = "os")]
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

/// The natives and module sources that engines compile against: the
/// standard library's, followed by those of a configuration's host modules.
pub(crate) struct Registry {
    /// Natives by id.
    pub natives: Arc<Vec<NativeEntry>>,
    pub by_name: Arc<NativeIds>,
    /// Module name -> kind and Sand source.
    pub sources: BTreeMap<Cow<'static, str>, (ModKind, Cow<'static, str>)>,
    /// The standard library's registry, whose snapshot this one's extends.
    base: Option<Arc<Registry>>,
    /// The modules checked and compiled, made on first use.
    snapshot: OnceLock<Result<Arc<Snapshot>, Error>>,
    /// The prelude and `task` checked and compiled, which the standard
    /// registry's `snapshot` extends.
    core: OnceLock<Result<Arc<Snapshot>, Error>>,
    /// The core extended with some standard modules, by their names
    /// (sorted), until the full snapshot is made.
    partial: Mutex<HashMap<Vec<String>, Arc<Snapshot>>>,
}

/// At most this many partial snapshots are made: then the full one is.
const MAX_PARTIAL_SNAPSHOTS: usize = 16;

/// The standard library's registry. Engines without host modules share it,
/// and so its snapshot, which is immutable: compilations copy what they
/// extend.
static STD: LazyLock<Arc<Registry>> = LazyLock::new(|| {
    let mut r = Registry {
        natives: Arc::default(),
        by_name: Arc::default(),
        sources: BTreeMap::new(),
        base: None,
        snapshot: OnceLock::new(),
        core: OnceLock::new(),
        partial: Mutex::default(),
    };
    stdlib::register(&mut r);
    Arc::new(r)
});

impl Registry {
    fn for_config(config: &Config) -> Arc<Registry> {
        let std = STD.clone();
        if config.modules.is_empty() {
            return std;
        }
        let mut r = Registry {
            natives: std.natives.clone(),
            by_name: std.by_name.clone(),
            sources: std.sources.clone(),
            base: None,
            snapshot: OnceLock::new(),
            core: OnceLock::new(),
            partial: Mutex::default(),
        };
        for m in &config.modules {
            m.register(&mut r);
        }
        // A host module replacing a standard one needs a snapshot of its own.
        if !r.sources.iter().any(|(name, (kind, _))| *kind == ModKind::Host && std.sources.contains_key(name)) {
            r.base = Some(std);
        }
        Arc::new(r)
    }

    pub fn add(&mut self, name: impl Into<Cow<'static, str>>, imp: crate::vm::NativeImpl) {
        let name = name.into();
        let id = self.natives.len() as u32;
        Arc::make_mut(&mut self.natives).push(NativeEntry { name: name.clone(), imp });
        Arc::make_mut(&mut self.by_name).insert(name, id);
    }

    pub fn add_builtin(&mut self, name: impl Into<Cow<'static, str>>, f: crate::vm::NativeFn) {
        self.add(name, crate::vm::NativeImpl::Builtin(f));
    }

    /// Registers the native implementing a trait method in an impl
    /// (`impl Close for File { native fn close(self) }` is `fs.File.close`):
    /// `f` receives `self` and the other arguments, without the impl's
    /// dictionary that trait methods are passed first.
    pub fn add_method(&mut self, name: impl Into<Cow<'static, str>>, f: crate::vm::NativeFn) {
        self.add(name, crate::vm::NativeImpl::Method(f));
    }

    pub fn source(&mut self, module: impl Into<Cow<'static, str>>, kind: ModKind, src: impl Into<Cow<'static, str>>) {
        self.sources.insert(module.into(), (kind, src.into()));
    }

    /// The registry's modules, checked and compiled (on first use).
    pub(crate) fn snapshot(self: &Arc<Self>) -> Result<Arc<Snapshot>, Error> {
        if let Some(s) = self.snapshot.get() {
            return s.clone();
        }
        let r = self.clone();
        on_compile_stack(move || r.build_snapshot())
    }

    fn build_snapshot(&self) -> Result<Arc<Snapshot>, Error> {
        let build = || {
            let base = match &self.base {
                Some(b) => b.build_snapshot()?,
                None => self.build_core()?,
            };
            Snapshot::build(self, Some(&base), None).map(Arc::new)
        };
        self.snapshot.get_or_init(build).clone()
    }

    /// The full snapshot if it is made already (always, for a registry with
    /// host modules: their programs need it).
    pub(crate) fn ready(self: &Arc<Self>) -> Option<Arc<Snapshot>> {
        if self.base.is_some() {
            return self.snapshot().ok();
        }
        self.snapshot.get().and_then(|s| s.as_ref().ok().cloned())
    }

    /// A snapshot holding the standard modules `names` (and the core): the
    /// core extended with them, made once per set of names, or the full
    /// snapshot (if made already, or after `MAX_PARTIAL_SNAPSHOTS` sets).
    pub(crate) fn extended(self: &Arc<Self>, names: &[&str]) -> Result<Arc<Snapshot>, Error> {
        if let Some(full) = self.ready() {
            return Ok(full);
        }
        let mut key: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        key.sort();
        key.dedup();
        let mut partial = self.partial.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = partial.get(&key) {
            return Ok(s.clone());
        }
        if partial.len() >= MAX_PARTIAL_SNAPSHOTS {
            drop(partial);
            return self.snapshot();
        }
        let (r, only) = (self.clone(), key.clone());
        let s = on_compile_stack(move || {
            let core = r.build_core()?;
            let only: Vec<&str> = only.iter().map(String::as_str).collect();
            Snapshot::build(&r, Some(&core), Some(&only)).map(Arc::new)
        })?;
        partial.insert(key, s.clone());
        Ok(s)
    }

    /// The prelude and `task` checked and compiled, on which compilations
    /// that import few standard modules start (see [`Registry::extended`]).
    pub(crate) fn core(self: &Arc<Self>) -> Result<Arc<Snapshot>, Error> {
        if let Some(b) = &self.base {
            return b.core();
        }
        if let Some(s) = self.core.get() {
            return s.clone();
        }
        let r = self.clone();
        on_compile_stack(move || r.build_core())
    }

    fn build_core(&self) -> Result<Arc<Snapshot>, Error> {
        if let Some(b) = &self.base {
            return b.build_core();
        }
        let build = || Snapshot::build(self, None, Some(&["prelude", "task"])).map(Arc::new);
        self.core.get_or_init(build).clone()
    }
}

pub struct Engine {
    config: Arc<Config>,
    registry: Arc<Registry>,
    interrupt: Arc<AtomicBool>,
    shared: Arc<IoShared>,
    /// "Always" prompt answers, remembered for the lifetime of the engine
    /// (SPEC 14.3): shared by import checks and every run (and VM).
    always: Always,
    gc_stats: GcStats,
    /// The module root of source text, and its normal form (see
    /// [`Engine::source_root`]).
    root: OnceLock<(PathBuf, PathBuf)>,
    /// A copy of the snapshot's program, to compile the next program on
    /// (see [`Engine::recycle`]).
    spare: Mutex<Option<(Arc<Snapshot>, Program)>>,
    /// Initial monotonic clock reading.
    origin: Instant,
}

/// A compiled program.
pub(crate) struct Compiled {
    pub program: Arc<Program>,
    /// The main module's lint warnings (if asked for).
    pub lints: Option<Vec<crate::lint::LintWarning>>,
    /// When the run's wall-time limit passes, counted from the start of
    /// compilation, excluding import prompts.
    pub deadline: Option<Instant>,
    /// The snapshot the program extends, if it only adds to its tables (no
    /// host module had to be compiled), so that it can be recycled.
    pub snapshot: Option<Arc<Snapshot>>,
}

/// How to compile a program.
#[derive(Clone, Copy)]
pub(crate) struct CompileOptions {
    /// Whether imports the policy would prompt for are prompted for
    /// (otherwise they are denied).
    pub prompt: bool,
    /// Whether to lint the main module.
    pub lint: bool,
}

/// Runs `f` on a stack deep enough for checking and compiling
/// (`platform::on_deep_stack`). A panic is an internal compiler error.
pub(crate) fn on_compile_stack<R: Send + 'static>(f: impl FnOnce() -> Result<R, Error> + Send + 'static) -> Result<R, Error> {
    crate::platform::on_deep_stack(f)
        .unwrap_or_else(|p| Err(Error::Io(format!("internal compiler error: {}", crate::unwind::panic_text(&*p)))))
}

impl Engine {
    pub fn new(config: Config) -> Engine {
        let registry = Registry::for_config(&config);
        let origin = config.clock.now();
        Engine {
            origin,
            config: Arc::new(config),
            registry,
            interrupt: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(IoShared::default()),
            always: Arc::default(),
            gc_stats: GcStats::default(),
            root: OnceLock::new(),
            spare: Mutex::default(),
        }
    }

    /// The names of the standard and host modules that scripts import by name.
    pub fn module_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.registry.sources.keys().filter(|n| *n != "prelude").map(|n| n.to_string()).collect();
        names.sort_unstable();
        names
    }

    /// Garbage collector statistics of the most recent run.
    pub fn gc_stats(&self) -> GcStats {
        self.gc_stats
    }

    /// Type checks source text as [`Engine::check_source`] does, and lints
    /// it. Compile errors are an [`Error::Compile`]; a lint that runs out of
    /// budget or time is an [`Error::Io`].
    pub fn lint_source(&self, name: &str, source: &str) -> Result<Vec<crate::lint::LintWarning>, Error> {
        let c = self.compile_now(name, source, &self.source_root().1, None, true)?;
        self.recycle(c.program, c.snapshot);
        Ok(c.lints.unwrap_or_default())
    }

    /// Lints a file and its imports, like [`Engine::lint_source`].
    pub fn lint_file(&self, path: impl AsRef<Path>) -> Result<Vec<crate::lint::LintWarning>, Error> {
        let (name, source, root) = self.read_main(path.as_ref())?;
        let c = self.compile_now(&name, &source, &root, Some(path.as_ref()), true)?;
        self.recycle(c.program, c.snapshot);
        Ok(c.lints.unwrap_or_default())
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
            interrupt: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(IoShared::default()),
            always: self.always.clone(),
            gc_stats: GcStats::default(),
            root: OnceLock::new(),
            spare: Mutex::default(),
            origin: self.origin,
        })
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Forgets interrupts of earlier runs; interrupts from now on stop the
    /// next compilation or run.
    pub(crate) fn clear_interrupt(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
    }

    /// The point at which a run starting now (with its compilation)
    /// exceeds the wall-time limit.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.config.limits.max_wall_time.and_then(|t| self.config.clock.now().checked_add(t))
    }

    /// A compile budget for a compilation that must end by `deadline`.
    pub(crate) fn compile_budget(&self, deadline: Option<Instant>) -> Arc<CompileBudget> {
        let limits = &self.config.limits;
        let deadline = deadline.map(|at| crate::platform::Deadline { at, clock: self.config.clock.clone() });
        Arc::new(CompileBudget::new(limits.max_compile_fuel, deadline, Some(self.interrupt.clone())))
    }

    /// Type checks a source without running it. Imports the policy would
    /// prompt for are denied.
    pub fn check_source(&self, name: &str, source: &str) -> Result<(), Error> {
        let c = self.compile_now(name, source, &self.source_root().1, None, false)?;
        self.recycle(c.program, c.snapshot);
        Ok(())
    }

    /// Type checks a file and its imports, like [`Engine::check_source`].
    pub fn check_file(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let (name, source, root) = self.read_main(path.as_ref())?;
        let c = self.compile_now(&name, &source, &root, Some(path.as_ref()), false)?;
        self.recycle(c.program, c.snapshot);
        Ok(())
    }

    /// Keeps `prog` (if `snapshot` is given and nothing else holds it),
    /// truncated to the snapshot's tables, to compile the next program on:
    /// copying the snapshot's program costs more than compiling a small one.
    fn recycle(&self, prog: Arc<Program>, snapshot: Option<Arc<Snapshot>>) {
        let (Ok(mut prog), Some(snap)) = (Arc::try_unwrap(prog), snapshot) else { return };
        snap.mark.rollback(&mut prog);
        *self.spare.lock().unwrap_or_else(|e| e.into_inner()) = Some((snap, prog));
    }

    /// Compiles synchronously: without prompts, it never waits.
    fn compile_now(&self, name: &str, source: &str, root: &Path, path: Option<&Path>, lint: bool) -> Result<Compiled, Error> {
        let options = CompileOptions { prompt: false, lint };
        let compiling = std::pin::pin!(self.compile_with(name, source, root, path, options));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match crate::unwind::catch(|| compiling.poll(&mut cx)) {
            Ok(std::task::Poll::Ready(r)) => r,
            Ok(std::task::Poll::Pending) => Err(Error::Io("internal compiler error: compilation waited".into())),
            Err(msg) => Err(Error::Io(format!("internal compiler error: {msg}"))),
        }
    }

    /// The module root of source text: the configured root or working
    /// directory, or [`Engine::default_root`] (as it is when first needed),
    /// and its normal form.
    fn source_root(&self) -> &(PathBuf, PathBuf) {
        self.root.get_or_init(|| {
            let root = self.config.module_root.clone().or_else(|| self.config.cwd.clone()).unwrap_or_else(|| self.default_root());
            let fs = &self.config.fs;
            let normal = fs.resolve(&fs.absolute(&root));
            (root, normal)
        })
    }

    pub async fn run_source(&mut self, name: &str, source: &str) -> Result<crate::Outcome, Error> {
        crate::unwind::guard(self.run_source_inner(name, source)).await
    }

    async fn run_source_inner(&mut self, name: &str, source: &str) -> Result<crate::Outcome, Error> {
        let (root, normal) = self.source_root().clone();
        let compiled = self.compile(name, source, &normal, None).await?;
        self.execute(compiled, root).await
    }

    pub async fn run_file(&mut self, path: impl AsRef<Path>) -> Result<crate::Outcome, Error> {
        crate::unwind::guard(self.run_file_inner(path.as_ref())).await
    }

    async fn run_file_inner(&mut self, path: &Path) -> Result<crate::Outcome, Error> {
        let (name, source, root) = self.read_main(path)?;
        let compiled = self.compile(&name, &source, &root, Some(path)).await?;
        let cwd = self.config.cwd.clone().unwrap_or(root);
        self.execute(compiled, cwd).await
    }

    pub async fn run_tests(&mut self, path: impl AsRef<Path>) -> Result<TestReport, Error> {
        crate::unwind::guard(self.run_tests_inner(path.as_ref())).await
    }

    async fn run_tests_inner(&mut self, path: &Path) -> Result<TestReport, Error> {
        let (name, source, root) = self.read_main(path)?;
        let compiled = self.compile(&name, &source, &root, Some(path)).await?;
        let cwd = self.config.cwd.clone().unwrap_or(root);
        let prog = compiled.program;
        // The wall-time limit bounds the whole test run.
        let mut deadline = compiled.deadline;
        let mut report = TestReport::default();
        for (tname, proto) in prog.tests.clone() {
            let mut vm = self.new_vm(prog.clone(), cwd.clone(), true);
            vm.deadline = deadline;
            let init = vm.run_main(prog.entry).await;
            let result = match init {
                Ok(RunEnd::Finished(_)) => {
                    vm.ready.clear();
                    let r = vm.run_main(proto).await;
                    match end_run(&mut vm, r).await {
                        Ok(RunEnd::Finished(_)) | Ok(RunEnd::Exit(0)) => Ok(()),
                        Ok(RunEnd::Exit(c)) => Err(format!("exited with code {c}")),
                        Err(t) => Err(trap_message(t)),
                    }
                }
                Ok(RunEnd::Exit(c)) => Err(format!("module exited with code {c}")),
                Err(t) => Err(trap_message(t)),
            };
            deadline = vm.deadline;
            report.results.push(TestResult { name: tname, passed: result.is_ok(), message: result.err() });
        }
        self.recycle(prog, compiled.snapshot);
        Ok(report)
    }

    /// The module root of source text run without a configured root or
    /// working directory: the process's working directory, or the root of a
    /// memory file system.
    pub(crate) fn default_root(&self) -> PathBuf {
        self.config.fs.default_dir()
    }

    fn read_main(&self, path: &Path) -> Result<(String, String, PathBuf), Error> {
        let fs = &self.config.fs;
        let source = fs.read_main(path).map_err(|e| Error::Io(format!("cannot read {}: {e}", path.display())))?;
        let abs = fs.resolve(&fs.absolute(path));
        let root = self
            .config
            .module_root
            .clone()
            .map(|r| fs.resolve(&fs.absolute(&r)))
            .unwrap_or_else(|| abs.parent().map(|p| p.to_path_buf()).unwrap_or_default());
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "main.sand".into());
        Ok((name, source, root))
    }

    pub(crate) fn new_vm(&self, prog: Arc<Program>, cwd: PathBuf, quiet: bool) -> Vm {
        let c = &self.config;
        let stdin = match &c.stdin {
            StdinConfig::None => StdinSource::None,
            StdinConfig::Bytes(b) => StdinSource::Bytes(std::io::Cursor::new(b.clone())),
            #[cfg(feature = "os")]
            StdinConfig::Inherit => StdinSource::Inherit(tokio::io::BufReader::new(tokio::io::stdin())),
            StdinConfig::Feed(feed) => StdinSource::Fed(crate::feed::FedReader::new(feed)),
        };
        let stdin_feed = match &c.stdin {
            StdinConfig::Feed(feed) => Some(feed.shared()),
            _ => None,
        };
        let (stdout, stderr): (SharedSink, SharedSink) =
            if quiet { (Arc::new(NullSink), Arc::new(NullSink)) } else { (c.stdout.clone(), c.stderr.clone()) };
        let host = Host {
            policy: c.policy.clone(),
            prompter: c.prompter.clone(),
            grants_once: Vec::new(),
            denies_once: Vec::new(),
            always: self.always.clone(),
            stdout,
            stderr,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            stdin_feed,
            output_bytes: 0,
            limits: c.limits.clone(),
            cwd,
            args: c.args.clone(),
            env: c.env.clone(),
            rng: Rng::new(c.seed.unwrap_or_else(entropy_seed)),
            log_level: 1,
            interrupt: self.interrupt.clone(),
            stream_decisions: [None; 2],
            #[cfg(feature = "os")]
            tls: c.tls.clone(),
            #[cfg(feature = "os")]
            http_pool: crate::stdlib::HttpPool::new(c.limits.max_open_resources, c.clock.clone()),
            fs: c.fs.clone(),
            request_filter: c.request_filter.clone(),
            regex_cache: Vec::new(),
            clock: c.clock.clone(),
            origin: self.origin,
            #[cfg(feature = "os")]
            network: c.network.clone().unwrap_or_else(|| Arc::new(crate::network::OsNetwork)),
        };
        let mut vm = Vm::new(prog, self.registry.natives.clone(), host);
        vm.io = IoTable::with_shared(self.shared.clone());
        if c.gc_stress {
            vm.heap.stress = true;
            vm.heap.next_gc = 0;
        }
        vm
    }

    async fn execute(&mut self, compiled: Compiled, cwd: PathBuf) -> Result<crate::Outcome, Error> {
        let prog = compiled.program;
        if self.config.dump_bytecode {
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
        vm.deadline = compiled.deadline;
        let r = vm.run_main(prog.entry).await;
        let r = end_run(&mut vm, r).await;
        let _ = self.config.stdout.flush();
        self.gc_stats = vm.heap.stats;
        if self.config.exit_after_run {
            std::mem::forget(vm);
        } else {
            drop(vm);
            self.recycle(prog, compiled.snapshot);
        }
        match r {
            Ok(RunEnd::Finished(_)) => Ok(crate::Outcome { exit_code: 0 }),
            Ok(RunEnd::Exit(c)) => Ok(crate::Outcome { exit_code: c }),
            Err(Trap::Exit(c)) => Ok(crate::Outcome { exit_code: c }),
            Err(Trap::Error { message, trace }) => Err(Error::Runtime { message, trace }),
            Err(Trap::Internal { message, trace }) => Err(Error::Internal { message, trace }),
            Err(Trap::Limit(k)) => Err(Error::Limit(k)),
            Err(Trap::Interrupted) => Err(Error::Interrupted),
        }
    }

    // ------------------------------------------------------------------
    // Loading and compiling

    /// Checks (and if `prompt`, prompts for) importing the script at the
    /// normalized `path`.
    async fn check_import(&self, path: &Path, prompt: bool) -> bool {
        let cap = Capability::Import(path.to_path_buf());
        if let Some(&d) = self.always.lock().unwrap_or_else(|e| e.into_inner()).get(&cap) {
            return d;
        }
        match self.config.policy.check_normalized_in(&cap, self.config.fs.space()) {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Prompt => match &self.config.prompter {
                Some(p) if prompt => {
                    let answer = p.prompt(cap.clone()).await;
                    let allowed = matches!(answer, PromptAnswer::AllowOnce | PromptAnswer::AllowAlways);
                    if matches!(answer, PromptAnswer::AllowAlways | PromptAnswer::DenyAlways) {
                        self.always.lock().unwrap_or_else(|e| e.into_inner()).insert(cap, allowed);
                    }
                    allowed
                }
                _ => false,
            },
        }
    }

    /// Loads, checks and compiles the program whose main module is
    /// `main_source` (the file `main_path`, if it is one), to run it.
    pub(crate) async fn compile(
        &self,
        main_name: &str,
        main_source: &str,
        root: &Path,
        main_path: Option<&Path>,
    ) -> Result<Compiled, Error> {
        self.compile_with(main_name, main_source, root, main_path, RUN).await
    }

    /// Like [`Engine::compile`], with `options`. `root` is the module root,
    /// in normal form (resolved by the engine's file system).
    pub(crate) async fn compile_with(
        &self,
        main_name: &str,
        main_source: &str,
        root: &Path,
        main_path: Option<&Path>,
        options: CompileOptions,
    ) -> Result<Compiled, Error> {
        self.clear_interrupt();
        // Unless the full snapshot exists, compile on the core extended with
        // just the standard modules the program imports (`Registry::extended`).
        let mut snap = match self.registry.ready() {
            Some(s) => s,
            None => self.registry.core()?,
        };
        let mut g = ModuleGraph::new(&snap);
        g.source_bytes = main_source.len();
        if main_source.len() > self.config.limits.max_source_bytes {
            let f = g.sm.add(main_name, "");
            return Err(Error::Compile(vec![g.sm.resolve(&Diag::error(Span::new(f, 0, 0), "source too large"))]));
        }
        // The main file's real directory (symlinks resolved), as in `read_main`.
        let fs = &self.config.fs;
        let main_dir =
            main_path.and_then(|p| fs.resolve(&fs.absolute(p)).parent().map(Path::to_path_buf)).unwrap_or_else(|| root.to_path_buf());
        let main_file = g.sm.add(main_name, main_source);
        let main_key = format!(
            "file:{}",
            main_path.map(|p| fs.resolve(&fs.absolute(p)).display().to_string()).unwrap_or_else(|| main_name.to_string())
        );
        let main_mod_name = Path::new(main_name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "main".into());
        let (main, imports) = g.add(main_mod_name, main_file, ModKind::Script { main: true }, Some(main_dir));
        g.index.insert(main_key.clone(), main);
        let mut errors: Vec<Diag> = Vec::new();
        let mut prompted = Duration::ZERO;
        let deadline = self.deadline();
        let discovery = Discovery { snap: &snap, root, deadline, prompt: options.prompt };
        if let Some(stop) = self.discover(&discovery, &mut g, main, imports, main_key, &mut errors, &mut prompted).await {
            return Err(stop);
        }
        if !errors.is_empty() {
            return Err(Error::Compile(errors.iter().map(|d| g.sm.resolve(d)).collect()));
        }
        let std: Vec<&str> = g.modules.iter().filter(|m| m.kind == ModKind::Std).map(|m| m.name.as_str()).collect();
        if !std.is_empty() {
            snap = self.registry.extended(&std)?;
            g = g.rebase(&snap);
        }
        let main_file = g.modules[main].file;
        // Time spent waiting for prompt answers does not count.
        let deadline = deadline.map(|d| d + prompted);
        let budget = self.compile_budget(deadline);
        let spare = match self.spare.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some((s, p)) if Arc::ptr_eq(&s, &snap) => Some(p),
            _ => None,
        };
        let natives = g.has_natives(&g.order);
        let (base, registry, optimize) = (snap.clone(), self.registry.clone(), self.config.optimize);
        let (program, lints) =
            on_compile_stack(move || compile_program(&base, &mut g, &registry, &budget, main_file, options.lint, optimize, spare))?;
        Ok(Compiled { program, lints, deadline, snapshot: (!natives).then_some(snap) })
    }

    /// Runs `job`, file system work for module discovery, on a worker
    /// thread (see `Vm::offload`). Returns an error if the engine is
    /// interrupted or the wall-time limit passes before the job finishes.
    /// `prompted` is the time spent on prompts, which extends the deadline.
    fn discovery_io<T: Send + 'static>(
        &self,
        d: &Discovery<'_>,
        prompted: Duration,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Error> {
        if self.config.fs.memory().is_some() {
            return Ok(job());
        }
        // Waits synchronously: discovery can run outside a runtime with a timer.
        #[cfg(feature = "os")]
        {
            let check = || {
                if self.interrupt.load(Ordering::Relaxed) {
                    return Err(Error::Interrupted);
                }
                if d.deadline.is_some_and(|t| self.config.clock.now() >= t + prompted) {
                    return Err(Error::Limit(LimitKind::WallTime));
                }
                Ok(())
            };
            crate::platform::offload(job).wait(check)?.map_err(|panic| Error::Internal { message: panic, trace: vec![] })
        }
        #[cfg(not(feature = "os"))]
        {
            let _ = (d, prompted);
            unreachable!("without the `os` feature, the file system is in memory")
        }
    }

    /// Loads, depth first, the modules `imports` of module `start` (keyed
    /// `key`) and their imports, checking script imports against the policy.
    /// Appends the new modules to `g.order` in dependency order, followed by
    /// `start`.
    ///
    /// This runs before the compile budget exists, so it is bounded on its
    /// own: overlong import paths are denied, discovery stops after
    /// [`MAX_DISCOVERY_ERRORS`] errors, and it returns the error to report
    /// if the engine is interrupted or the wall-time limit passes. Time
    /// spent waiting for prompt answers is added to `prompted`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn discover(
        &self,
        d: &Discovery<'_>,
        g: &mut ModuleGraph,
        start: usize,
        imports: Vec<ImportRef>,
        key: String,
        errors: &mut Vec<Diag>,
        prompted: &mut Duration,
    ) -> Option<Error> {
        // DFS stack: (module index, its imports, next import to visit), and
        // the keys of the modules on it (to detect cycles).
        let mut stack: Vec<ImportFrame> = vec![(start, imports, 0)];
        let mut path_stack: Vec<String> = vec![key];
        // Whether a module loaded by an earlier session input is stale, by module index.
        let mut stale: HashMap<usize, bool> = HashMap::new();
        while let Some((mi, imports, pos)) = stack.pop() {
            if self.interrupt.load(Ordering::Relaxed) {
                return Some(Error::Interrupted);
            }
            if d.deadline.is_some_and(|t| self.config.clock.now() >= t + *prompted) {
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
            let imp = imports[pos].clone();
            stack.push((mi, imports, pos + 1));
            let optional = matches!(imp, ImportRef::Help(..));
            let script = match imp {
                ImportRef::Name(name, span) | ImportRef::Help(name, span) => {
                    let registered = self.registry.sources.get(name.as_str()).filter(|_| name != "prelude");
                    if let Some(id) = d.snap.module(&name).filter(|_| name != "prelude") {
                        g.use_std(d.snap, id);
                        g.modules[mi].imports.push((span, Target::Snapshot(id)));
                        None
                    } else if let Some(&(kind, ref src)) = registered {
                        // A module that is not in the snapshot: a host module
                        // with errors, or a standard module not in the core.
                        let key = format!("std:{name}");
                        if path_stack.contains(&key) {
                            errors.push(Diag::error(span, format!("import cycle through module `{name}`")));
                            continue;
                        }
                        if let Some(&idx) = g.index.get(&key) {
                            g.modules[mi].imports.push((span, Target::Loaded(idx)));
                            continue;
                        }
                        let f = g.sm.add(format!("<{name}>"), src.as_ref());
                        let (idx, imports) = g.add(name, f, kind, None);
                        g.index.insert(key.clone(), idx);
                        g.modules[mi].imports.push((span, Target::Loaded(idx)));
                        path_stack.push(key);
                        stack.push((idx, imports, 0));
                        None
                    } else {
                        let library = match self.library_module(d, *prompted, &name) {
                            Ok(found) => found,
                            Err(e) => return Some(e),
                        };
                        if library.is_some() && !matches!(g.modules[mi].kind, ModKind::Script { .. }) {
                            errors.push(Diag::error(span, "native modules cannot import script files"));
                            continue;
                        }
                        match library {
                            Some(resolved) => Some((resolved, name, span, false)),
                            None if optional => continue,
                            None if crate::stdlib::OS_MODULES.contains(&name.as_str()) => {
                                errors.push(Diag::error(
                                    span,
                                    format!("module `{name}` is not available: this build of Sand has no operating-system access"),
                                ));
                                continue;
                            }
                            None => {
                                errors.push(Diag::error(span, format!("unknown module `{name}`")));
                                continue;
                            }
                        }
                    }
                }
                ImportRef::Path(_, span) if !matches!(g.modules[mi].kind, ModKind::Script { .. }) => {
                    errors.push(Diag::error(span, "native modules cannot import script files"));
                    None
                }
                ImportRef::Path(p, span) => {
                    if p.len() > crate::policy::MAX_PATH_BYTES {
                        errors.push(Diag::error(span, "cannot import: the path is too long"));
                        continue;
                    }
                    let dir = g.modules[mi].dir.clone().unwrap_or_else(|| d.root.to_path_buf());
                    let mut target = dir.join(&p);
                    if target.extension().is_none() {
                        target.set_extension("sand");
                    }
                    // Only scripts: a diagnostic would quote other files.
                    if target.extension().is_none_or(|e| e != "sand") {
                        errors.push(Diag::error(span, format!("cannot import `{p}`: script modules are `.sand` files")));
                        continue;
                    }
                    match self.discovery_io(d, *prompted, self.script_target(&target)) {
                        Ok(t) => Some((t, p, span, true)),
                        Err(e) => return Some(e),
                    }
                }
            };
            let Some((resolved, p, span, under_root)) = script else { continue };
            let target = resolved.path.clone();
            if under_root && !target.starts_with(d.root) {
                errors.push(Diag::error(span, format!("cannot import `{p}`: the path is outside the module root")));
                continue;
            }
            let key = format!("file:{}", target.display());
            if path_stack.contains(&key) {
                let cycle: Vec<String> = path_stack.iter().map(|k| k.trim_start_matches("file:").to_string()).collect();
                errors.push(Diag::error(span, format!("import cycle: {} -> {}", cycle.join(" -> "), target.display())));
                continue;
            }
            if let Some(&idx) = g.index.get(&key) {
                // A module from an earlier session input is loaded again if its source has changed since.
                match self.stale(d, *prompted, g, idx, start, &mut stale) {
                    Ok(false) => {
                        g.modules[mi].imports.push((span, Target::Loaded(idx)));
                        continue;
                    }
                    Ok(true) => {}
                    Err(e) => return Some(e),
                }
            }
            let asked = self.config.clock.now();
            let allowed = self.check_import(&target, d.prompt).await;
            *prompted += self.config.clock.now().saturating_duration_since(asked);
            if !allowed {
                errors.push(Diag::error(span, format!("cannot import `{p}`: permission denied")));
                // Compilation has failed: do not prompt for further imports.
                break;
            }
            // Read the checked file without following symlinks swapped in since.
            let (fs, max) = (self.config.fs.clone(), self.config.limits.max_source_bytes);
            let text = match self.discovery_io(d, *prompted, move || fs.read_source(resolved, max)) {
                Ok(Ok(t)) => t,
                Err(e) => return Some(e),
                Ok(Err(e)) => {
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
            let display = target.strip_prefix(d.root).map(|r| r.display().to_string()).unwrap_or_else(|_| target.display().to_string());
            let f = g.sm.add(display, text);
            let name = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let (idx, imports) = g.add(name, f, ModKind::Script { main: false }, target.parent().map(|d| d.to_path_buf()));
            g.modules[idx].path = Some(target);
            g.index.insert(key.clone(), idx);
            g.modules[mi].imports.push((span, Target::Loaded(idx)));
            path_stack.push(key);
            stack.push((idx, imports, 0));
        }
        None
    }

    /// A job that resolves the script path `target` to the file it names.
    fn script_target(&self, target: &Path) -> impl FnOnce() -> crate::files::Target + Send + 'static {
        let (fs, lexical) = (self.config.fs.clone(), crate::policy::lexical(target));
        move || {
            let mut t = fs.target(&lexical, crate::files::Mode::Follow);
            // Modules are identified by path, so use the name as stored on disk.
            t.fix_spelling();
            t
        }
    }

    /// The script that the import of module `name` loads from the library, if the library has it.
    fn library_module(&self, d: &Discovery<'_>, prompted: Duration, name: &str) -> Result<Option<crate::files::Target>, Error> {
        let Some(library) = &self.config.library else { return Ok(None) };
        if !crate::check::help::is_ident(name) {
            return Ok(None);
        }
        let resolve = self.script_target(&self.config.fs.absolute(library).join(format!("{name}.sand")));
        let fs = self.config.fs.clone();
        self.discovery_io(d, prompted, move || {
            let t = resolve();
            fs.stat(&t).ok().filter(|m| m.kind == crate::files::EntryKind::File).map(|_| t)
        })
    }

    /// Whether module `idx`, or a module it imports, was loaded before
    /// module `fresh` and its file has changed since.
    fn stale(
        &self,
        d: &Discovery<'_>,
        prompted: Duration,
        g: &ModuleGraph,
        idx: usize,
        fresh: usize,
        memo: &mut HashMap<usize, bool>,
    ) -> Result<bool, Error> {
        if idx >= fresh {
            return Ok(false);
        }
        if let Some(&known) = memo.get(&idx) {
            return Ok(known);
        }
        let m = &g.modules[idx];
        let Some(path) = m.path.clone() else { return Ok(false) };
        let (resolve, fs, max) = (self.script_target(&path), self.config.fs.clone(), self.config.limits.max_source_bytes);
        let current = self.discovery_io(d, prompted, move || fs.read_source(resolve(), max).ok())?;
        let mut changed = current.as_deref() != Some(g.sm.text(m.file));
        for &(_, target) in &m.imports {
            if changed {
                break;
            }
            if let Target::Loaded(dep) = target {
                changed = self.stale(d, prompted, g, dep, fresh, memo)?;
            }
        }
        memo.insert(idx, changed);
        Ok(changed)
    }
}

/// The run's compile options: imports are prompted for, no linting.
const RUN: CompileOptions = CompileOptions { prompt: true, lint: false };

/// What module discovery needs besides the graph.
pub(crate) struct Discovery<'a> {
    pub snap: &'a Snapshot,
    /// The module root: script imports must be below it.
    pub root: &'a Path,
    /// When the wall-time limit passes (plus the time spent on prompts).
    pub deadline: Option<Instant>,
    pub prompt: bool,
}

/// Module discovery stops after this many errors (they are all reported).
const MAX_DISCOVERY_ERRORS: usize = 100;

/// Ends a run whose main task finished (`Vm::end_tasks`).
async fn end_run(vm: &mut Vm, r: Result<RunEnd, Trap>) -> Result<RunEnd, Trap> {
    match r {
        Ok(RunEnd::Finished(v)) => Ok(vm.end_tasks().await?.map_or(RunEnd::Finished(v), RunEnd::Exit)),
        r => r,
    }
}

fn trap_message(t: Trap) -> String {
    match t {
        Trap::Error { message, .. } => message,
        Trap::Internal { message, .. } => format!("internal error: {message}"),
        Trap::Limit(k) => k.to_string(),
        Trap::Interrupted => "interrupted".into(),
        Trap::Exit(c) => format!("exited with code {c}"),
    }
}

type ImportFrame = (usize, Vec<ImportRef>, usize);

/// Checks and compiles the modules of `g` (in `g.order`; the main module is
/// in `main_file`) on top of the snapshot `snap`.
#[allow(clippy::too_many_arguments)]
fn compile_program(
    snap: &Snapshot,
    g: &mut ModuleGraph,
    registry: &Registry,
    budget: &Arc<CompileBudget>,
    main_file: FileId,
    lint: bool,
    optimize: bool,
    spare: Option<Program>,
) -> Result<(Arc<Program>, Option<Vec<crate::lint::LintWarning>>), Error> {
    let order = g.order.clone();
    let modules =
        g.parse(&order, snap.checker.mods.len() as ModId).map_err(|ds| Error::Compile(ds.iter().map(|d| g.sm.resolve(d)).collect()))?;
    let mut checker = snap.checker.clone();
    checker.lint_facts = lint;
    checker.optimize = optimize;
    let checked = checker.check_modules(&modules, &registry.by_name, budget);
    let sm = &g.sm;
    if let Some(stop) = budget.stopped() {
        // Diagnostics after the budget ran out are not meaningful.
        return Err(budget_error(stop, sm, main_file));
    }
    checked.map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
    // The program initializes the standard modules it uses, then its own.
    let ids = order.iter().filter_map(|&mi| g.modules[mi].id);
    let inits: Vec<FnId> =
        g.std.iter().map(|&id| snap.mods[id as usize].init).chain(ids.map(|id| checker.mods[id as usize].init)).flatten().collect();
    if lint {
        checker.lint.main_imports = checker.main_imports();
    }
    // Only host modules declare natives.
    let host = g.has_natives(&order);
    let cp = checker.take_checked();
    let lints = match modules.iter().find(|m| m.file == main_file).filter(|_| lint) {
        Some(main) => Some(crate::lint::lint_module(main, &main.source, &cp, budget)?),
        None => None,
    };
    let (native_names, native_argc) = if host { native_info(&cp, registry) } else { (Vec::new(), Vec::new()) };
    let mut program = spare.unwrap_or_else(|| snap.program.clone());
    let input =
        CompileInput { checked: &cp, sm, natives: &registry.by_name, native_names, native_argc, budget, initialized: None, optimize };
    let entry = compile_more(input, &mut program, &mut snap.compiler.clone(), &inits);
    if let Some(stop) = budget.stopped() {
        return Err(budget_error(stop, sm, main_file));
    }
    program.entry = entry.map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
    Ok((Arc::new(program), lints))
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::check::FnKind;

    /// Every native the standard library registers is declared by a `native
    /// fn` (a mistyped `add_builtin` would never be called).
    #[test]
    fn every_std_native_is_declared() {
        let snapshot = STD.snapshot().expect("the standard library compiles");
        let declared: std::collections::HashSet<u32> =
            snapshot.checker.fns.iter().filter_map(|f| if let FnKind::Native(n) = f.kind { Some(n) } else { None }).collect();
        let undeclared: Vec<&str> =
            STD.natives.iter().enumerate().filter(|(id, _)| !declared.contains(&(*id as u32))).map(|(_, e)| &*e.name).collect();
        assert!(undeclared.is_empty(), "registered natives no `native fn` declares: {undeclared:?}");
    }

    /// `STD_MODULES` (the names host modules cannot take) lists the standard modules.
    #[test]
    fn std_module_names_are_listed() {
        for name in STD.sources.keys() {
            assert!(stdlib::STD_MODULES.contains(&&**name), "{name}");
        }
        for name in stdlib::STD_MODULES {
            assert!(STD.sources.contains_key(*name) || stdlib::OS_MODULES.contains(name), "{name}");
        }
    }
}

#[cfg(all(test, feature = "os"))]
mod offload_tests {
    use super::*;
    use crate::native::NativeModule;
    use crate::vm::value::Value;
    use std::sync::atomic::AtomicUsize;

    /// A module with two natives. `hang()` blocks a worker until `release`
    /// is set. `walk()` charges its allowance one unit at a time, counting
    /// each step in `progress`, until the allowance stops it.
    fn hang_module(release: Arc<AtomicBool>, progress: Arc<AtomicUsize>) -> NativeModule {
        let mut m = NativeModule::new("hung");
        m.source("pub native fn hang()\npub native fn walk()");
        let r = release.clone();
        m.raw_function("hang", move |vm: &mut Vm, _: &[Value]| {
            let r = r.clone();
            vm.offload(move |_| {
                while !r.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })?;
            Ok(Value::Unit)
        });
        m.raw_function("walk", move |vm: &mut Vm, _: &[Value]| {
            let p = progress.clone();
            vm.offload(move |allowance| {
                while allowance.charge(1).is_ok() {
                    p.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_micros(100));
                }
            })?;
            Ok(Value::Unit)
        });
        m
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// If a run makes a system call that does not return, as on an
    /// unresponsive network file system, then the run ends at its time limit
    /// or when it is interrupted.
    #[test]
    fn hung_io_is_abandoned_at_limits_and_interrupts() {
        let (release, progress) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let limits = Limits { max_wall_time: Some(Duration::from_millis(100)), ..Limits::default() };
        let config = Config::new().module(hang_module(release.clone(), progress.clone())).limits(limits);
        let mut engine = Engine::new(config.clone());
        let r = rt().block_on(engine.run_source("main.sand", "import hung\nhung.hang()"));
        assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");

        let mut engine = Engine::new(config.limits(Limits::default()));
        let handle = engine.interrupt_handle();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            handle.interrupt();
        });
        let r = rt().block_on(engine.run_source("main.sand", "import hung\nhung.walk()"));
        assert!(matches!(r, Err(Error::Interrupted)), "{r:?} after {} steps", progress.load(Ordering::SeqCst));
        // The job on the worker no longer makes progress.
        std::thread::sleep(Duration::from_millis(20));
        let seen = progress.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(progress.load(Ordering::SeqCst), seen);
        release.store(true, Ordering::SeqCst);
    }
}
