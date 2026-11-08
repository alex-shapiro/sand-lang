//! The embedding API: `Engine`, `Config`, errors and module loading.

use crate::check::ModKind;
use crate::check::budget::CompileBudget;
use crate::compile::{CompileInput, compile_more};
use crate::diag::{Diag, Diagnostic, FileId, Span};
use crate::files::Fs;
use crate::files::mem::MemFs;
use crate::modules::{ImportRef, ModuleGraph, Snapshot, Target, budget_error, native_info};
use crate::native::func::Signature;
use crate::native::ty::Type;
use crate::native::{BoundType, NativeDecl, NativeModule};
use crate::platform::{Clock, Instant, SystemClock};
use crate::policy::rules::{PathPattern, RulePolicy};
use crate::policy::{Capability, CustomPolicy, Decision, Policy, PromptAnswer, Prompter};
use crate::stdin_feed::{StdinChannel, StdinFeed};
use crate::stdlib;
use crate::stdlib::log::{LogLevel, LogSink, NullLogSink};
use crate::stdlib::time::TimeZone;
use crate::types::{FnId, ModId};
use crate::vm::bytecode::Program;
use crate::vm::heap::GcStats;
use crate::vm::sched::{IoShared, RunEnd};
use crate::vm::{Always, Host, HostFn, NativeEntry, OutputSink, Rng, SharedSink, TraceFrame, Trap, Vm, WaitKind, entropy_seed};
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, Weak};
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

impl Error {
    fn compiler_bug(message: &str) -> Error {
        Error::Internal { message: format!("compiler: {message}"), trace: Vec::new() }
    }
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
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().clone()
    }
    pub fn clear(&self) {
        self.0.lock().clear();
    }
}

impl OutputSink for MemorySink {
    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        self.0.lock().extend_from_slice(data);
        Ok(())
    }
}

struct NullSink;
impl OutputSink for NullSink {
    fn write(&self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

enum StdinConfig {
    None,
    Bytes(Vec<u8>),
    /// Host-supplied input channel shared across every VM of the engine.
    Host(Arc<StdinChannel>),
}

/// Engine configuration (builder style).
pub struct Config {
    pub(crate) policy: Policy,
    pub(crate) prompter: Option<Arc<dyn Prompter>>,
    pub(crate) limits: Limits,
    host_modules: HostModules,
    pub(crate) stdout: SharedSink,
    pub(crate) stderr: SharedSink,
    log_sink: Option<Arc<dyn LogSink>>,
    log_level: Option<LogLevel>,
    stdin: StdinConfig,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) module_root: Option<PathBuf>,
    pub(crate) library: Option<PathBuf>,
    pub(crate) args: Vec<String>,
    pub(crate) env: Option<Vec<(String, String)>>,
    pub(crate) seed: Option<u64>,
    pub(crate) gc_stress: bool,
    pub(crate) retry_stress: bool,
    pub(crate) optimize: bool,
    pub(crate) dump_bytecode: bool,
    pub(crate) exit_after_run: bool,
    #[cfg(feature = "os")]
    pub(crate) tls: crate::stdlib::net::TlsSettings,
    pub(crate) fs: Fs,
    pub(crate) request_filter: Option<RequestFilter>,
    pub(crate) clock: Arc<dyn Clock>,
    #[cfg(feature = "os")]
    pub(crate) network: Option<Arc<dyn crate::network::Network>>,
    pub(crate) time_zone: Option<TimeZone>,
    interrupt: Option<InterruptHandle>,
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
            policy: Policy::Rules(Arc::new(RulePolicy::standard())),
            prompter: None,
            limits: Limits::default(),
            host_modules: HostModules::default(),
            stdout: Arc::new(crate::stdio::StdoutSink::default()),
            stderr: Arc::new(crate::stdio::StderrSink::default()),
            log_sink: None,
            log_level: Some(LogLevel::Info),
            stdin: StdinConfig::None,
            cwd: None,
            module_root: None,
            library: None,
            args: Vec::new(),
            env: None,
            seed: None,
            gc_stress: false,
            retry_stress: false,
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
            time_zone: None,
            interrupt: None,
        }
    }

    /// Sets the [`InterruptHandle`] used to stop active calls on the engine or
    /// session built from this configuration.
    pub fn interrupt(mut self, handle: InterruptHandle) -> Self {
        self.interrupt = Some(handle);
        self
    }

    /// Sets the clock and timer provider for the engine.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sets the network provider for host resolution, outgoing TCP
    /// connections, and TCP listeners.
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
    /// Sets the rule policy, replacing any custom policy. Defaults to [`RulePolicy::standard`].
    pub fn policy(mut self, p: RulePolicy) -> Self {
        self.policy = Policy::Rules(Arc::new(p));
        self
    }
    /// Sets a host-defined policy callback. Replaces any rule policy.
    pub fn custom_policy(mut self, p: impl CustomPolicy + 'static) -> Self {
        self.policy = Policy::Custom(Arc::new(p));
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
    /// Sets the host modules that scripts can import. By default, scripts
    /// can import only the standard library.
    pub fn host_modules(mut self, host_modules: HostModules) -> Self {
        self.host_modules = host_modules;
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
    /// Routes `log` records to `sink` instead of standard error. Records count
    /// against `max_output_bytes`, but do not require [`Capability::Stderr`].
    pub fn log_sink(mut self, sink: impl LogSink + 'static) -> Self {
        self.log_sink = Some(Arc::new(sink));
        self
    }
    /// Sets the initial minimum script log level. Passing `None` disables
    /// script logging. Defaults to [`LogLevel::Info`].
    pub fn log_level(mut self, level: impl Into<Option<LogLevel>>) -> Self {
        self.log_level = level.into();
        self
    }
    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = StdinConfig::Bytes(data);
        self
    }
    /// Configures standard input from user actions through the returned
    /// [`StdinFeed`]. Idle time while a script waits for
    /// input does not count toward `max_wall_time`.
    pub fn stdin_feed(self) -> (Self, StdinFeed) {
        self.stdin_host_feed(WaitKind::User)
    }

    /// Configures programmatic standard input, such as a pipe, through the
    /// returned [`StdinFeed`]. Time spent waiting for input
    /// counts toward `max_wall_time`.
    pub fn stdin_io_feed(self) -> (Self, StdinFeed) {
        self.stdin_host_feed(WaitKind::Io)
    }

    fn stdin_host_feed(mut self, wait_kind: WaitKind) -> (Self, StdinFeed) {
        let (feed, channel) = StdinFeed::new(wait_kind);
        self.stdin = StdinConfig::Host(channel);
        (self, feed)
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
    /// Sets the fallback directory for `import <name>`. If `<name>` does not
    /// match a standard or host module, the engine loads `<dir>/<name>.sand`.
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
    /// Sets the time zone returned by `time.local_zone()` if the policy allows
    /// [`Capability::TimeZone`]. Defaults to `UTC`.
    pub fn time_zone(mut self, zone: TimeZone) -> Self {
        self.time_zone = Some(zone);
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
    /// Testing aid: forces the first memory or resource check in each
    /// `vm_function` call to fail, triggering garbage collection and re-running
    /// the function. Use this option to verify that native functions check
    /// memory and resource limits before performing side effects.
    pub fn retry_stress(mut self, on: bool) -> Self {
        self.retry_stress = on;
        self
    }
    /// Enables or disables compiler optimizations for scripts. Host modules
    /// are always compiled with optimizations enabled.
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

/// A thread-safe handle for stopping an engine or session.
/// Configured with [`Config::interrupt`].
///
/// If configured, the engine checks for an interrupt flag at regular checkpoints:
///
/// - During compilation, at fixed intervals
/// - During execution, at meter checkpoints
/// - During waits for I/O, timers, and permission prompts
///
/// If an interrupt is received at a checkpoint, the engine or session cancels
/// all ongoing compilation and VM tasks without running `defer` blocks.
/// The engine or session can be called again safely in the future, and a
/// session's global state remains safe.
///
/// An interrupted engine returns [`Error::Interrupted`].
/// An interrupted REPL session cancels all ongoing tasks,
/// but `run_tasks_until` continues to wait for host input.
#[derive(Clone, Default)]
pub struct InterruptHandle {
    targets: Arc<Mutex<InterruptTargets>>,
}

/// Active and recently finished calls registered with an [`InterruptHandle`].
#[derive(Default)]
struct InterruptTargets {
    calls: Vec<InterruptTarget>,
    /// Call count threshold at which `attach` prunes finished calls.
    cleanup_len: usize,
}

/// Weak references to a call's interrupt flag and I/O waker.
struct InterruptTarget {
    flag: Weak<AtomicBool>,
    io: Weak<IoShared>,
}

/// Interrupt flag and I/O waker for an active engine or session call.
#[derive(Clone)]
pub(crate) struct CallInterrupt {
    pub flag: Arc<AtomicBool>,
    pub io: Arc<IoShared>,
}

impl CallInterrupt {
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Awaits `future`, or returns `None` if the call is interrupted.
    pub async fn unless_interrupted<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| {
            self.io.set_waker(cx.waker());
            if self.is_set() {
                return std::task::Poll::Ready(None);
            }
            future.as_mut().poll(cx).map(Some)
        })
        .await
    }
}

impl InterruptHandle {
    /// Creates an interrupt handle.
    pub fn new() -> InterruptHandle {
        InterruptHandle::default()
    }

    /// Stops and returns from the engine or session.
    pub fn interrupt(&self) {
        let calls: Vec<_> =
            self.targets.lock().calls.iter().filter_map(|target| Some((target.flag.upgrade()?, target.io.upgrade()?))).collect();
        for (flag, io) in calls {
            flag.store(true, Ordering::SeqCst);
            io.wake();
        }
    }

    fn attach(&self, call: &CallInterrupt) {
        let mut targets = self.targets.lock();
        if targets.calls.len() >= targets.cleanup_len {
            targets.calls.retain(|target| target.flag.strong_count() > 0);
            targets.cleanup_len = 2 * targets.calls.len().max(4);
        }
        targets.calls.push(InterruptTarget { flag: Arc::downgrade(&call.flag), io: Arc::downgrade(&call.io) });
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

/// Compiled host modules that scripts can import.
///
/// Pass a `HostModules` to [`Config::host_modules`] to expose native modules
/// to scripts. Clones share the compiled modules across engines.
/// [`HostModules::default`] contains only the standard library.
#[derive(Clone)]
pub struct HostModules(Arc<Registry>);

impl Default for HostModules {
    fn default() -> Self {
        HostModules(STD.clone())
    }
}

impl HostModules {
    /// Type-checks and compiles `modules`. Returns [`Error::Compile`] if a
    /// module's Sand source is invalid or does not match its Rust functions.
    ///
    /// # Panics
    ///
    /// Panics if two modules share a name or bind the same Rust type.
    pub fn new(modules: impl IntoIterator<Item = NativeModule>) -> Result<HostModules, Error> {
        let modules: Vec<NativeModule> = modules.into_iter().collect();
        for (i, m) in modules.iter().enumerate() {
            assert!(!modules[..i].iter().any(|x| x.name() == m.name()), "host module `{}` is added twice", m.name());
        }
        let registry = Registry::for_modules(&modules);
        if registry.base.is_some() {
            registry.snapshot()?;
        }
        Ok(HostModules(registry))
    }
}

impl fmt::Debug for HostModules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.0.sources.iter().filter(|(_, src)| src.kind == ModKind::Host).map(|(name, _)| name);
        f.debug_list().entries(names).finish()
    }
}

/// A compiled script and its imported modules.
///
/// [`Engine::compile_source`] and [`Engine::compile_file`] produce a
/// `CompiledProgram`. [`Engine::run_program`] executes it on any engine.
/// Clones share the compiled bytecode.
#[derive(Clone)]
pub struct CompiledProgram {
    program: Arc<Program>,
    host_modules: HostModules,
}

impl fmt::Debug for CompiledProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledProgram").field("host_modules", &self.host_modules).finish_non_exhaustive()
    }
}

/// Native functions, bound types, and module sources for the standard library
/// and optional host modules.
pub(crate) struct Registry {
    pub natives: Arc<Vec<NativeEntry>>,
    pub decls: Arc<NativeDecls>,
    pub sources: BTreeMap<Cow<'static, str>, Source>,
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

#[derive(Clone)]
pub(crate) struct Source {
    pub kind: ModKind,
    pub text: Cow<'static, str>,
    /// Other modules referenced by this module's native function signatures.
    pub deps: Vec<&'static str>,
}

/// Registered native functions and Rust-bound types.
#[derive(Clone, Debug, Default)]
pub(crate) struct NativeDecls {
    pub fns: Vec<NativeDecl>,
    pub ids: HashMap<Cow<'static, str>, u32>,
    pub types: Vec<BoundType>,
}

impl NativeDecls {
    pub(crate) fn id(&self, key: &str) -> Option<u32> {
        self.ids.get(key).copied()
    }

    pub(crate) fn get(&self, key: &str) -> Option<&NativeDecl> {
        self.id(key).map(|id| &self.fns[id as usize])
    }
}

/// At most this many partial snapshots are made: then the full one is.
const MAX_PARTIAL_SNAPSHOTS: usize = 16;

/// The standard library's registry. Engines without host modules share it,
/// and so its snapshot, which is immutable: compilations copy what they
/// extend.
static STD: LazyLock<Arc<Registry>> = LazyLock::new(|| {
    let mut r = Registry::empty();
    let modules = stdlib::modules();
    for m in &modules {
        m.register_types(&mut r);
    }
    for m in &modules {
        m.register(&mut r);
    }
    Arc::new(r)
});

impl Registry {
    fn empty() -> Registry {
        Registry {
            natives: Arc::default(),
            decls: Arc::default(),
            sources: BTreeMap::new(),
            base: None,
            snapshot: OnceLock::new(),
            core: OnceLock::new(),
            partial: Mutex::default(),
        }
    }

    fn for_modules(modules: &[NativeModule]) -> Arc<Registry> {
        let std = STD.clone();
        if modules.is_empty() {
            return std;
        }
        let mut r = Registry { natives: std.natives.clone(), decls: std.decls.clone(), sources: std.sources.clone(), ..Registry::empty() };
        for m in modules {
            m.register_types(&mut r);
        }
        for m in modules {
            m.register(&mut r);
        }
        r.base = Some(std);
        Arc::new(r)
    }

    pub fn add_native(&mut self, key: String, f: HostFn, sig: Signature, names: Option<Arc<[String]>>, doc: Option<Arc<str>>) {
        assert!(!self.decls.ids.contains_key(key.as_str()), "native `{key}` registered twice");
        let id = self.natives.len() as u32;
        Arc::make_mut(&mut self.natives).push(NativeEntry { name: key.clone().into(), f });
        let decls = Arc::make_mut(&mut self.decls);
        let key: Cow<'static, str> = key.into();
        decls.ids.insert(key.clone(), id);
        decls.fns.push(NativeDecl { id, key, sig: Arc::new(sig), names, doc });
    }

    pub(crate) fn add_bound_type(&mut self, t: BoundType) {
        let qual = format!("{}.{}", t.module, t.name);
        let decls = Arc::make_mut(&mut self.decls);
        if let Some(rust) = t.kind.rust_type()
            && let Some(other) = decls.types.iter().find(|b| b.kind.rust_type() == Some(rust))
        {
            panic!("Rust type of `{qual}` is bound to `{}.{}` already", other.module, other.name);
        }
        assert!(!decls.types.iter().any(|b| b.module == t.module && b.name == t.name), "type `{qual}` registered twice");
        decls.types.push(t);
    }

    pub fn source(&mut self, module: impl Into<Cow<'static, str>>, kind: ModKind, src: impl Into<Cow<'static, str>>) {
        let module = module.into();
        let prefix = format!("{module}.");
        let mut deps: Vec<&'static str> = Vec::new();
        let sigs = self.decls.fns.iter().filter(|d| d.key.starts_with(&prefix)).flat_map(|d| d.sig.params.iter().chain([&d.sig.ret]));
        let fields = self.decls.types.iter().filter(|t| t.module == module).flat_map(|t| t.kind.member_types());
        for t in sigs.cloned().chain(fields) {
            t.visit(&mut |t| {
                if let Type::Named(n) = t
                    && n.module != module
                    && n.module != "prelude"
                    && !deps.contains(&n.module)
                {
                    deps.push(n.module);
                }
            });
        }
        deps.sort_unstable();
        self.sources.insert(module, Source { kind, text: src.into(), deps });
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
        let mut partial = self.partial.lock();
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
    config: Config,
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
    /// Host modules linked into `program`.
    pub host_modules: HostModules,
    /// Lint warnings for the main module.
    pub lints: Option<Vec<crate::lint::LintWarning>>,
    /// Wall-time deadline for the run. If the run compiled the program, the
    /// deadline starts at compilation and excludes import prompts.
    pub deadline: Option<Instant>,
    /// Base snapshot used to recycle `program` if the program does not define
    /// native modules.
    pub snapshot: Option<Arc<Snapshot>>,
}

impl Compiled {
    fn into_program(self) -> CompiledProgram {
        CompiledProgram { program: self.program, host_modules: self.host_modules }
    }
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
    crate::platform::on_deep_stack(f).unwrap_or_else(|p| Err(Error::compiler_bug(&crate::unwind::panic_text(&*p))))
}

impl Engine {
    /// Creates an engine with `config`.
    pub fn new(config: Config) -> Engine {
        let origin = config.clock.now();
        Engine { origin, config, always: Arc::default(), gc_stats: GcStats::default(), root: OnceLock::new(), spare: Mutex::default() }
    }

    /// Returns the sorted names of all registered standard and host modules.
    pub fn module_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.registry().sources.keys().filter(|n| *n != "prelude").map(|n| n.to_string()).collect();
        names.sort_unstable();
        names
    }

    /// Returns documentation for the prelude, standard library modules, and
    /// host modules, sorted by kind and then by module name.
    pub fn docs(&self) -> Result<Vec<crate::docs::ModuleDoc>, Error> {
        use crate::check::ModKind;
        use crate::docs::ModuleKind;
        let snapshot = self.registry().snapshot()?;
        let mut checker = snapshot.checker.clone();
        let mut mods: Vec<(ModuleKind, &str, ModId)> = snapshot
            .by_name
            .iter()
            .filter_map(|(name, &m)| {
                let kind = match checker.mods[m as usize].kind {
                    ModKind::Prelude => ModuleKind::Prelude,
                    ModKind::Std => ModuleKind::Std,
                    ModKind::Host => ModuleKind::Host,
                    ModKind::Script { .. } => return None,
                };
                Some((kind, name.as_str(), m))
            })
            .collect();
        mods.sort_by_key(|&(kind, name, _)| (kind, name));
        Ok(mods.into_iter().map(|(kind, _, m)| checker.module_doc(m, kind)).collect())
    }

    /// Garbage collector statistics of the most recent run.
    pub fn gc_stats(&self) -> GcStats {
        self.gc_stats
    }

    /// Type checks `source` as [`Engine::check_source`] does, and lints it.
    ///
    /// Returns [`Error::Compile`] on compile errors, or [`Error::Io`] if
    /// linting exceeds its fuel or wall-time budget.
    pub fn lint_source(&self, name: &str, source: &str) -> Result<Vec<crate::lint::LintWarning>, Error> {
        let call = self.begin_call();
        let compiled = self.compile_now(&call, name, source, &self.source_root().1, None, true)?;
        self.recycle(compiled.program, compiled.snapshot);
        Ok(compiled.lints.unwrap_or_default())
    }

    /// Type checks and lints the file at `path` and its imports, as
    /// [`Engine::lint_source`] does.
    pub fn lint_file(&self, path: impl AsRef<Path>) -> Result<Vec<crate::lint::LintWarning>, Error> {
        let call = self.begin_call();
        let (name, source, root) = self.read_main(path.as_ref())?;
        let compiled = self.compile_now(&call, &name, &source, &root, Some(path.as_ref()), true)?;
        self.recycle(compiled.program, compiled.snapshot);
        Ok(compiled.lints.unwrap_or_default())
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn host_modules(&self) -> &HostModules {
        &self.config.host_modules
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.config.host_modules.0
    }

    /// Creates a [`CallInterrupt`] for a new call and registers it with the
    /// configured [`InterruptHandle`].
    pub(crate) fn begin_call(&self) -> CallInterrupt {
        self.begin_call_with_io(Arc::default())
    }

    /// Creates a [`CallInterrupt`] that shares `io` with an existing VM and
    /// registers it with the configured [`InterruptHandle`].
    pub(crate) fn begin_call_with_io(&self, io: Arc<IoShared>) -> CallInterrupt {
        let call = CallInterrupt { flag: Arc::default(), io };
        if let Some(handle) = &self.config.interrupt {
            handle.attach(&call);
        }
        call
    }

    /// Returns the instant at which a call starting now reaches
    /// `Limits::max_wall_time`.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.config.limits.max_wall_time.and_then(|t| self.config.clock.now().checked_add(t))
    }

    /// Creates a [`CompileBudget`] for `call` with `deadline`.
    pub(crate) fn compile_budget(&self, deadline: Option<Instant>, call: &CallInterrupt) -> Arc<CompileBudget> {
        let limits = &self.config.limits;
        let deadline = deadline.map(|at| crate::platform::Deadline { at, clock: self.config.clock.clone() });
        Arc::new(CompileBudget::new(limits.max_compile_fuel, deadline, Some(call.flag.clone())))
    }

    /// Type checks `source` without running it. Denies imports that require a
    /// permission prompt.
    pub fn check_source(&self, name: &str, source: &str) -> Result<(), Error> {
        let call = self.begin_call();
        let compiled = self.compile_now(&call, name, source, &self.source_root().1, None, false)?;
        self.recycle(compiled.program, compiled.snapshot);
        Ok(())
    }

    /// Type checks the file at `path` and its imports, as
    /// [`Engine::check_source`] does.
    pub fn check_file(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let call = self.begin_call();
        let (name, source, root) = self.read_main(path.as_ref())?;
        let compiled = self.compile_now(&call, &name, &source, &root, Some(path.as_ref()), false)?;
        self.recycle(compiled.program, compiled.snapshot);
        Ok(())
    }

    /// Saves `prog` as a spare base program if `snapshot` is present and
    /// `prog` has a single reference. Rolling `prog` back to `snapshot` is
    /// cheaper than cloning the snapshot program for the next compilation.
    fn recycle(&self, prog: Arc<Program>, snapshot: Option<Arc<Snapshot>>) {
        let (Ok(mut prog), Some(snap)) = (Arc::try_unwrap(prog), snapshot) else { return };
        snap.mark.rollback(&mut prog);
        *self.spare.lock() = Some((snap, prog));
    }

    /// Compiles `source` synchronously with import prompts disabled.
    fn compile_now(
        &self,
        call: &CallInterrupt,
        name: &str,
        source: &str,
        root: &Path,
        path: Option<&Path>,
        lint: bool,
    ) -> Result<Compiled, Error> {
        let options = CompileOptions { prompt: false, lint };
        let compiling = std::pin::pin!(self.compile_with(call, name, source, root, path, options));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match crate::unwind::catch(|| compiling.poll(&mut cx)) {
            Ok(std::task::Poll::Ready(r)) => r,
            Ok(std::task::Poll::Pending) => Err(Error::compiler_bug("compilation waited")),
            Err(msg) => Err(Error::compiler_bug(&msg)),
        }
    }

    /// Returns the unnormalized and normalized module root for source text.
    ///
    /// Uses [`Config::module_root`], [`Config::cwd`], or
    /// [`Engine::default_root`] in that order.
    pub(crate) fn source_root(&self) -> &(PathBuf, PathBuf) {
        self.root.get_or_init(|| {
            let root = self.config.module_root.clone().or_else(|| self.config.cwd.clone()).unwrap_or_else(|| self.default_root());
            let fs = &self.config.fs;
            let normalized_root = fs.resolve(&fs.absolute(&root));
            (root, normalized_root)
        })
    }

    pub async fn run_source(&mut self, name: &str, source: &str) -> Result<Outcome, Error> {
        crate::unwind::guard(self.run_source_inner(name, source)).await
    }

    async fn run_source_inner(&mut self, name: &str, source: &str) -> Result<Outcome, Error> {
        let call = self.begin_call();
        let (compiled, cwd) = self.compile_main_source(&call, name, source).await?;
        self.execute(&call, compiled, cwd).await
    }

    pub async fn run_file(&mut self, path: impl AsRef<Path>) -> Result<Outcome, Error> {
        crate::unwind::guard(self.run_file_inner(path.as_ref())).await
    }

    async fn run_file_inner(&mut self, path: &Path) -> Result<Outcome, Error> {
        let call = self.begin_call();
        let (compiled, cwd) = self.compile_main_file(&call, path).await?;
        self.execute(&call, compiled, cwd).await
    }

    /// Compiles `source` and its imports into a [`CompiledProgram`].
    ///
    /// Resolves and checks imports under this engine's file system and policy,
    /// as [`Engine::run_source`] does.
    pub async fn compile_source(&self, name: &str, source: &str) -> Result<CompiledProgram, Error> {
        crate::unwind::guard(async {
            let call = self.begin_call();
            let (compiled, _) = self.compile_main_source(&call, name, source).await?;
            Ok(compiled.into_program())
        })
        .await
    }

    /// Compiles the script at `path` and its imports into a [`CompiledProgram`].
    ///
    /// Resolves and checks imports under this engine's file system and policy,
    /// as [`Engine::run_file`] does.
    pub async fn compile_file(&self, path: impl AsRef<Path>) -> Result<CompiledProgram, Error> {
        crate::unwind::guard(async {
            let call = self.begin_call();
            let (compiled, _) = self.compile_main_file(&call, path.as_ref()).await?;
            Ok(compiled.into_program())
        })
        .await
    }

    /// Executes `program` on this engine.
    ///
    /// The run uses the host modules from `program` and takes all other
    /// configuration from this engine. It selects the working directory in the
    /// same order as [`Engine::run_source`]: [`Config::cwd`],
    /// [`Config::module_root`], or the default directory of the file system.
    /// Each run starts with fresh globals. The engine measures resource limits
    /// from the start of the run.
    pub async fn run_program(&mut self, program: &CompiledProgram) -> Result<Outcome, Error> {
        let call = self.begin_call();
        let compiled = Compiled {
            program: program.program.clone(),
            host_modules: program.host_modules.clone(),
            lints: None,
            deadline: self.deadline(),
            snapshot: None,
        };
        let cwd = self.run_cwd(self.source_root().0.clone());
        crate::unwind::guard(self.execute(&call, compiled, cwd)).await
    }

    /// Compiles `source` as the main module for `call`. Returns the compiled
    /// program and the working directory for its run.
    async fn compile_main_source(&self, call: &CallInterrupt, name: &str, source: &str) -> Result<(Compiled, PathBuf), Error> {
        let (root, normalized_root) = self.source_root().clone();
        Ok((self.compile(call, name, source, &normalized_root, None).await?, self.run_cwd(root)))
    }

    /// Compiles the file at `path` as the main module for `call`. Returns the
    /// compiled program and the working directory for its run.
    async fn compile_main_file(&self, call: &CallInterrupt, path: &Path) -> Result<(Compiled, PathBuf), Error> {
        let (name, source, root) = self.read_main(path)?;
        Ok((self.compile(call, &name, &source, &root, Some(path)).await?, self.run_cwd(root)))
    }

    /// Returns the working directory for a run whose module root is `root`.
    pub(crate) fn run_cwd(&self, root: PathBuf) -> PathBuf {
        self.config.cwd.clone().unwrap_or(root)
    }

    /// Compiles the script at `path` and runs each of its `test` blocks on a
    /// fresh VM after the module initializer. Returns a [`TestReport`] with
    /// the outcome of each test.
    ///
    /// Runtime errors and limit violations are recorded as test failures in
    /// the report so that remaining tests still run. Calling
    /// [`InterruptHandle::interrupt`] stops the test run and returns
    /// [`Error::Interrupted`].
    pub async fn run_tests(&mut self, path: impl AsRef<Path>) -> Result<TestReport, Error> {
        crate::unwind::guard(self.run_tests_inner(path.as_ref())).await
    }

    async fn run_tests_inner(&mut self, path: &Path) -> Result<TestReport, Error> {
        let call = self.begin_call();
        let (compiled, cwd) = self.compile_main_file(&call, path).await?;
        let prog = compiled.program;
        // The wall-time limit bounds the whole test run.
        let mut deadline = compiled.deadline;
        let mut report = TestReport::default();
        for (tname, proto) in prog.tests.clone() {
            let mut vm = self.new_vm(prog.clone(), &compiled.host_modules, cwd.clone(), true, &call);
            vm.deadline = deadline;
            let init = vm.run_main(prog.entry).await;
            let result = match init {
                Ok(RunEnd::Finished(_)) => {
                    vm.ready.clear();
                    let r = vm.run_main(proto).await;
                    match end_run(&mut vm, r).await {
                        Ok(RunEnd::Finished(_)) | Ok(RunEnd::Exit(0)) => Ok(()),
                        Ok(RunEnd::Exit(c)) => Err(format!("exited with code {c}")),
                        Err(Trap::Interrupted) => return Err(Error::Interrupted),
                        Err(t) => Err(trap_message(t)),
                    }
                }
                Ok(RunEnd::Exit(c)) => Err(format!("module exited with code {c}")),
                Err(Trap::Interrupted) => return Err(Error::Interrupted),
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

    /// Creates a VM to execute `prog` with `host_modules` during `call`.
    pub(crate) fn new_vm(&self, prog: Arc<Program>, host_modules: &HostModules, cwd: PathBuf, quiet: bool, call: &CallInterrupt) -> Vm {
        let c = &self.config;
        let stdin = match &c.stdin {
            StdinConfig::None => None,
            StdinConfig::Bytes(b) => Some(StdinChannel::with_input(b.clone())),
            StdinConfig::Host(channel) => Some(channel.clone()),
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
            stdin,
            output_bytes: 0,
            limits: c.limits.clone(),
            cwd,
            args: c.args.clone(),
            env: c.env.clone(),
            rng: Rng::new(c.seed.unwrap_or_else(entropy_seed)),
            log_level: c.log_level,
            log_sink: if quiet { Some(Arc::new(NullLogSink)) } else { c.log_sink.clone() },
            interrupt: call.clone(),
            stream_decisions: [None; 2],
            #[cfg(feature = "os")]
            tls: c.tls.clone(),
            #[cfg(feature = "os")]
            http_pool: crate::stdlib::http::Pool::new(c.limits.max_open_resources, c.clock.clone()),
            fs: c.fs.clone(),
            request_filter: c.request_filter.clone(),
            regex_cache: Vec::new(),
            clock: c.clock.clone(),
            origin: self.origin,
            #[cfg(feature = "os")]
            network: c.network.clone().unwrap_or_else(|| Arc::new(crate::network::OsNetwork)),
            time_zone: c.time_zone.as_ref().map(|z| z.zone().clone()),
        };
        let mut vm = Vm::new(prog, host_modules.0.natives.clone(), host);
        if c.gc_stress {
            vm.heap.stress = true;
            vm.heap.next_gc = 0;
        }
        vm.retry_stress = c.retry_stress;
        vm
    }

    async fn execute(&mut self, call: &CallInterrupt, compiled: Compiled, cwd: PathBuf) -> Result<Outcome, Error> {
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
        let mut vm = self.new_vm(prog.clone(), &compiled.host_modules, cwd, false, call);
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
            Ok(RunEnd::Finished(_)) => Ok(Outcome { exit_code: 0 }),
            Ok(RunEnd::Exit(c)) => Ok(Outcome { exit_code: c }),
            Err(Trap::Exit(c)) => Ok(Outcome { exit_code: c }),
            Err(Trap::Error { message, trace }) => Err(Error::Runtime { message, trace }),
            Err(Trap::Internal { message, trace }) => Err(Error::Internal { message, trace }),
            Err(Trap::Limit(k)) => Err(Error::Limit(k)),
            Err(Trap::Interrupted) => Err(Error::Interrupted),
        }
    }

    // ------------------------------------------------------------------
    // Loading and compiling

    /// Checks `path` against the import policy and prompts if
    /// `discovery.prompt` is true.
    async fn check_import(&self, path: &Path, discovery: &Discovery<'_>) -> ImportDecision {
        let capability = Capability::Import(path.to_path_buf());
        let decided = |allowed| if allowed { ImportDecision::Allowed } else { ImportDecision::Denied };
        if let Some(&allowed) = self.always.lock().get(&capability) {
            return decided(allowed);
        }
        match self.config.policy.check_normalized_in(&capability, self.config.fs.space()) {
            Decision::Allow => ImportDecision::Allowed,
            Decision::Deny => ImportDecision::Denied,
            Decision::Prompt => match &self.config.prompter {
                Some(prompter) if discovery.prompt => {
                    let Some(answer) = discovery.interrupt.unless_interrupted(prompter.prompt(capability.clone())).await else {
                        return ImportDecision::Interrupted;
                    };
                    let allowed = matches!(answer, PromptAnswer::AllowOnce | PromptAnswer::AllowAlways);
                    if matches!(answer, PromptAnswer::AllowAlways | PromptAnswer::DenyAlways) {
                        self.always.lock().insert(capability, allowed);
                    }
                    decided(allowed)
                }
                Some(_) => ImportDecision::Unprompted("cannot prompt during check"),
                None => ImportDecision::Unprompted("a prompter is not configured"),
            },
        }
    }

    fn format_unprompted_import_reason(&self, path: &Path, why: &str) -> String {
        let Policy::Rules(rules) = &self.config.policy else { return format!("the policy requires a prompt, and {why}") };
        match rules.verdict(&Capability::Import(path.to_path_buf()), self.config.fs.space(), &[]) {
            Some((_, rule)) => format!("{} requires a prompt, and {why}", rules.describe_rule(rule)),
            None => match PathPattern::literal(path.parent().unwrap_or(path)) {
                Ok(dir) => {
                    let rule = RulePolicy::new().allow_import(dir).to_string();
                    format!("unmatched import requires a prompt, and {why}; add `{}` to allow it", rule.trim_end())
                }
                Err(_) => format!("unmatched import requires a prompt, and {why}"),
            },
        }
    }

    /// Loads, checks, and compiles `main_source` and its imports for `call`.
    /// `main_path` is `Some` if the main module was loaded from a file.
    pub(crate) async fn compile(
        &self,
        call: &CallInterrupt,
        main_name: &str,
        main_source: &str,
        root: &Path,
        main_path: Option<&Path>,
    ) -> Result<Compiled, Error> {
        self.compile_with(call, main_name, main_source, root, main_path, RUN).await
    }

    /// Loads, checks, and compiles `main_source` and its imports with
    /// `options`. `root` is the normalized module root.
    pub(crate) async fn compile_with(
        &self,
        call: &CallInterrupt,
        main_name: &str,
        main_source: &str,
        root: &Path,
        main_path: Option<&Path>,
        options: CompileOptions,
    ) -> Result<Compiled, Error> {
        // Unless the full snapshot exists, compile on the core extended with
        // just the standard modules the program imports (`Registry::extended`).
        let mut snap = match self.registry().ready() {
            Some(s) => s,
            None => self.registry().core()?,
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
        let discovery = Discovery { snap: &snap, root, deadline, prompt: options.prompt, interrupt: call };
        if let Some(stop) = self.discover(&discovery, &mut g, main, imports, main_key, &mut errors, &mut prompted).await {
            return Err(stop);
        }
        if !errors.is_empty() {
            return Err(Error::Compile(errors.iter().map(|d| g.sm.resolve(d)).collect()));
        }
        let std: Vec<&str> = g.modules.iter().filter(|m| m.kind == ModKind::Std).map(|m| m.name.as_str()).collect();
        if !std.is_empty() {
            snap = self.registry().extended(&std)?;
            g = g.rebase(&snap);
        }
        let main_file = g.modules[main].file;
        // Time spent waiting for prompt answers does not count.
        let deadline = deadline.map(|d| d + prompted);
        let budget = self.compile_budget(deadline, call);
        let spare = match self.spare.lock().take() {
            Some((s, p)) if Arc::ptr_eq(&s, &snap) => Some(p),
            _ => None,
        };
        let has_natives = g.has_natives(&g.order);
        let (base, registry, optimize) = (snap.clone(), self.registry().clone(), self.config.optimize);
        let (program, lints) =
            on_compile_stack(move || compile_program(&base, &mut g, &registry, &budget, main_file, options.lint, optimize, spare))?;
        let host_modules = self.config.host_modules.clone();
        Ok(Compiled { program, host_modules, lints, deadline, snapshot: (!has_natives).then_some(snap) })
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
                if d.interrupt.is_set() {
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
        let mut stale_cache: HashMap<usize, bool> = HashMap::new();
        while let Some((mi, imports, pos)) = stack.pop() {
            if d.interrupt.is_set() {
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
            let optional = matches!(imp, ImportRef::Implicit(..));
            let script_import = match imp {
                ImportRef::Name(name, span) | ImportRef::Implicit(name, span) => {
                    let registered = self.registry().sources.get(name.as_str()).filter(|_| name != "prelude");
                    if let Some(id) = d.snap.module(&name).filter(|_| name != "prelude") {
                        g.use_std(d.snap, id);
                        g.modules[mi].imports.push((span, Target::Snapshot(id)));
                        None
                    } else if let Some(Source { kind, text: src, deps }) = registered {
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
                        let (idx, mut imports) = g.add(name, f, *kind, None);
                        imports.extend(deps.iter().map(|d| ImportRef::Implicit(d.to_string(), Span::new(f, 0, 0))));
                        g.index.insert(key.clone(), idx);
                        g.modules[mi].imports.push((span, Target::Loaded(idx)));
                        path_stack.push(key);
                        stack.push((idx, imports, 0));
                        None
                    } else {
                        let library = match self.resolve_library_module(d, *prompted, &name) {
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
                            None => {
                                errors.push(Diag::error(span, crate::modules::unknown_module(&name)));
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
                    // Reject non-`.sand` paths before reading so diagnostics do not quote arbitrary files.
                    if target.extension().is_none_or(|e| e != "sand") {
                        errors.push(Diag::error(span, format!("cannot import `{p}`: script modules are `.sand` files")));
                        continue;
                    }
                    match self.discovery_io(d, *prompted, self.resolve_script_target(&target)) {
                        Ok(t) => Some((t, p, span, true)),
                        Err(e) => return Some(e),
                    }
                }
            };
            let Some((resolved, import_label, span, require_module_root)) = script_import else { continue };
            let target = resolved.path.clone();
            if require_module_root && !target.starts_with(d.root) {
                errors.push(Diag::error(span, format!("cannot import `{import_label}`: the path is outside the module root")));
                continue;
            }
            let key = format!("file:{}", target.display());
            if path_stack.contains(&key) {
                let cycle: Vec<String> = path_stack.iter().map(|k| k.trim_start_matches("file:").to_string()).collect();
                errors.push(Diag::error(span, format!("import cycle: {} -> {}", cycle.join(" -> "), target.display())));
                continue;
            }
            if let Some(&idx) = g.index.get(&key) {
                match self.is_module_stale(d, *prompted, g, idx, start, &mut stale_cache) {
                    Ok(false) => {
                        g.modules[mi].imports.push((span, Target::Loaded(idx)));
                        continue;
                    }
                    Ok(true) => {}
                    Err(e) => return Some(e),
                }
            }
            let asked = self.config.clock.now();
            let decision = self.check_import(&target, d).await;
            *prompted += self.config.clock.now().saturating_duration_since(asked);
            let hint = match decision {
                ImportDecision::Allowed => None,
                ImportDecision::Denied => Some(String::new()),
                ImportDecision::Unprompted(why) => Some(format!("; {}", self.format_unprompted_import_reason(&target, why))),
                ImportDecision::Interrupted => return Some(Error::Interrupted),
            };
            if let Some(hint) = hint {
                errors.push(Diag::error(span, format!("cannot import `{import_label}`: permission denied{hint}")));
                // Stop after a denied import to avoid prompting for remaining imports.
                break;
            }
            let (fs, max) = (self.config.fs.clone(), self.config.limits.max_source_bytes);
            let text = match self.discovery_io(d, *prompted, move || fs.read_source(resolved, max)) {
                Ok(Ok(t)) => t,
                Err(e) => return Some(e),
                Ok(Err(e)) => {
                    errors.push(Diag::error(span, format!("cannot load module `{import_label}`: {e}")));
                    continue;
                }
            };
            g.source_bytes = g.source_bytes.saturating_add(text.len());
            if g.source_bytes > self.config.limits.max_source_bytes {
                errors.push(Diag::error(
                    span,
                    format!("cannot load module `{import_label}`: source too large (the program's sources exceed the limit in total)"),
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

    fn resolve_script_target(&self, path: &Path) -> impl FnOnce() -> crate::files::Target + Send + 'static {
        let (fs, lexical) = (self.config.fs.clone(), crate::policy::lexical(path));
        move || {
            let mut t = fs.target(&lexical, crate::files::Mode::Follow);
            // Normalize path casing so that case-insensitive imports share a single module key.
            t.fix_spelling();
            t
        }
    }

    fn resolve_library_module(&self, d: &Discovery<'_>, prompted: Duration, name: &str) -> Result<Option<crate::files::Target>, Error> {
        let Some(library) = &self.config.library else { return Ok(None) };
        if !crate::syntax::lexer::is_ident(name) || stdlib::STD_MODULES.contains(&name) {
            return Ok(None);
        }
        let resolve = self.resolve_script_target(&self.config.fs.absolute(library).join(format!("{name}.sand")));
        let fs = self.config.fs.clone();
        self.discovery_io(d, prompted, move || {
            let t = resolve();
            fs.stat(&t).ok().filter(|m| m.kind == crate::files::EntryKind::File).map(|_| t)
        })
    }

    /// Returns true if module `idx` was loaded in an earlier compilation and its
    /// source file or a transitive dependency has changed.
    fn is_module_stale(
        &self,
        d: &Discovery<'_>,
        prompted: Duration,
        g: &ModuleGraph,
        idx: usize,
        first_new_module: usize,
        stale_cache: &mut HashMap<usize, bool>,
    ) -> Result<bool, Error> {
        if idx >= first_new_module {
            return Ok(false);
        }
        if let Some(&known) = stale_cache.get(&idx) {
            return Ok(known);
        }
        let m = &g.modules[idx];
        let Some(path) = m.path.clone() else { return Ok(false) };
        let (resolve, fs, max) = (self.resolve_script_target(&path), self.config.fs.clone(), self.config.limits.max_source_bytes);
        let current = self.discovery_io(d, prompted, move || fs.read_source(resolve(), max).ok())?;
        let mut changed = current.as_deref() != Some(g.sm.text(m.file));
        for &(_, target) in &m.imports {
            if changed {
                break;
            }
            if let Target::Loaded(dep) = target {
                changed = self.is_module_stale(d, prompted, g, dep, first_new_module, stale_cache)?;
            }
        }
        stale_cache.insert(idx, changed);
        Ok(changed)
    }
}

/// The run's compile options: imports are prompted for, no linting.
const RUN: CompileOptions = CompileOptions { prompt: true, lint: false };

/// Context for module discovery in [`Engine::discover`].
pub(crate) struct Discovery<'a> {
    pub snap: &'a Snapshot,
    /// Normalized module root. Script imports must resolve inside this path.
    pub root: &'a Path,
    /// Wall-time deadline before adding time spent on prompts.
    pub deadline: Option<Instant>,
    pub prompt: bool,
    /// Interrupt state for the active call.
    pub interrupt: &'a CallInterrupt,
}

/// Policy decision for a module import in [`Engine::check_import`].
enum ImportDecision {
    Allowed,
    Denied,
    /// The policy requires a prompt, which is unavailable for the given
    /// reason.
    Unprompted(&'static str),
    /// An interrupt stopped the call during the prompt wait.
    Interrupted,
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
    let checked = checker.check_modules(&modules, &registry.decls, budget);
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
    let host = g.has_natives(&order);
    let cp = checker.take_checked();
    let lints = match modules.iter().find(|m| m.file == main_file).filter(|_| lint) {
        Some(main) => Some(crate::lint::lint_module(main, &main.source, &cp, budget)?),
        None => None,
    };
    let native_info = if host { native_info(&cp, registry) } else { crate::modules::NativeInfo::default() };
    let mut program = spare.unwrap_or_else(|| snap.program.clone());
    let input = CompileInput { checked: &cp, sm, natives: &registry.decls, native_info, budget, initialized: None, optimize };
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

    #[test]
    fn every_std_native_is_declared() {
        let snapshot = STD.snapshot().expect("the standard library compiles");
        let declared: std::collections::HashSet<u32> =
            snapshot.checker.fns.iter().filter_map(|f| if let FnKind::Native(n) = f.kind { Some(n) } else { None }).collect();
        let undeclared: Vec<&str> =
            STD.natives.iter().enumerate().filter(|(id, _)| !declared.contains(&(*id as u32))).map(|(_, e)| &*e.name).collect();
        assert!(undeclared.is_empty(), "registered natives no `native fn` declares: {undeclared:?}");
    }

    #[test]
    fn host_module_source_is_compiled_with_optimizations() {
        let src = "fn double(x: int) -> int { x * 2 }\npub fn doubled_total(xs: [int]) -> int { xs.map(double).filter(x => x > 2).sum() }";
        let mut m = NativeModule::new("host");
        m.source(src);
        let snap = HostModules::new([m]).unwrap().0.snapshot().unwrap();
        let host = snap.program.protos.iter().find(|p| p.name == "doubled_total").expect("doubled_total is compiled").code.clone();
        let script = |optimize: bool| {
            let engine = Engine::new(Config::new().optimize(optimize));
            let compiled = engine.compile_now(&engine.begin_call(), "main.sand", src, Path::new("."), None, false).unwrap();
            let proto = compiled.program.protos.iter().find(|p| p.name == "doubled_total").expect("doubled_total is compiled");
            proto.code.clone()
        };
        let (fast, slow) = (script(true), script(false));
        assert_ne!(fast.len(), slow.len());
        assert_eq!(host.len(), fast.len());
    }

    #[test]
    fn engines_share_the_registry_of_their_host_modules() {
        let host_modules = HostModules::new([NativeModule::new("a")]).unwrap();
        let one = Engine::new(Config::new().host_modules(host_modules.clone()));
        let two = Engine::new(Config::new().seed(1).host_modules(host_modules.clone()));
        assert!(Arc::ptr_eq(one.registry(), two.registry()));
        assert!(Arc::ptr_eq(one.registry(), &host_modules.0));
        assert!(Arc::ptr_eq(Engine::new(Config::new()).registry(), &STD));
        assert_eq!(format!("{host_modules:?}"), r#"["a"]"#);
    }

    /// `STD_MODULES` lists every standard library module and the prelude.
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
    use std::sync::atomic::AtomicUsize;

    /// A module with two natives. `hang()` blocks a worker until `release`
    /// is set. `walk()` charges its allowance one unit at a time, counting
    /// each step in `progress`, until the allowance stops it.
    fn hang_module(release: Arc<AtomicBool>, progress: Arc<AtomicUsize>) -> NativeModule {
        let mut m = NativeModule::new("hung");
        m.source("pub native fn hang()\npub native fn walk()");
        let r = release.clone();
        m.vm_function("hang", move |vm: &mut Vm| {
            let r = r.clone();
            vm.offload(move |_| {
                while !r.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        });
        m.vm_function("walk", move |vm: &mut Vm| {
            let p = progress.clone();
            vm.offload(move |allowance| {
                while allowance.charge(1).is_ok() {
                    p.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_micros(100));
                }
            })
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
        let config = || Config::new().host_modules(HostModules::new([hang_module(release.clone(), progress.clone())]).unwrap());
        let mut engine = Engine::new(config().limits(limits));
        let r = rt().block_on(engine.run_source("main.sand", "import hung\nhung.hang()"));
        assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");

        let handle = InterruptHandle::new();
        let mut engine = Engine::new(config().interrupt(handle.clone()));
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
