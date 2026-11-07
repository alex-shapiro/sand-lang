//! The embedding API: `Engine`, `Config`, errors and module loading.

use crate::check::ModKind;
use crate::check::budget::{CompileBudget, Stop};
use crate::compile::{CompileInput, compile_more};
use crate::diag::{Diag, Diagnostic, FileId, Span};
use crate::files::Fs;
use crate::files::mem::MemFs;
use crate::modules::{ImportRef, ModuleGraph, Snapshot, Target, budget_error, native_info};
use crate::native::convert::{Conversion, FromValue};
use crate::native::func::Signature;
use crate::native::ty::Type;
use crate::native::{BoundType, NativeDecl, NativeModule};
use crate::platform::{Clock, Instant, SystemClock};
use crate::policy::rules::{PathPattern, RulePolicy};
use crate::policy::{Capability, CustomPolicy, Decision, Policy, PromptAnswer, Prompter};
use crate::session::Session;
use crate::stdlib;
use crate::types::{FnId, ModId};
use crate::vm::bytecode::Program;
use crate::vm::heap::GcStats;
use crate::vm::journal::Divergence;
use crate::vm::sched::{IoShared, IoTable, RunEnd};
use crate::vm::value::{Obj, TypeDesc, Value};
use crate::vm::{Always, Host, HostFn, NativeEntry, OutputSink, Rng, SharedSink, StdinSource, TraceEntry, Trap, Vm, entropy_seed};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
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

/// Why an operation of an [`Engine`] failed.
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
    /// Argument `index` of [`Engine::call`] does not decode to the type of its parameter.
    /// `kind` and `message` are those of the error of `json.decode`.
    Argument {
        index: usize,
        kind: String,
        message: String,
    },
    /// The run does something other than what its journal records at entry
    /// `seq` (SPEC §16.7).
    Diverged {
        seq: u64,
        message: String,
    },
    /// The host ended the run: a host function called
    /// [`Ctl::abort`](crate::vm::Ctl::abort), or [`Journal::flush`](crate::journal::Journal::flush)
    /// failed.
    Aborted(HostAbort),
}

/// Why the host ended a run ([`Error::Aborted`]): the host's own error, which the host gets
/// back with [`HostAbort::downcast_ref`].
#[derive(Clone)]
pub struct HostAbort(Arc<dyn std::error::Error + Send + Sync>);

impl HostAbort {
    pub fn new(e: impl std::error::Error + Send + Sync + 'static) -> HostAbort {
        HostAbort(Arc::new(e))
    }

    /// A reason with only a message.
    pub fn message(m: impl Into<String>) -> HostAbort {
        HostAbort::new(Message(m.into()))
    }

    /// The host's error, if it has type `T`.
    pub fn downcast_ref<T: std::error::Error + 'static>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl fmt::Display for HostAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for HostAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HostAbort({})", self.0)
    }
}

#[derive(Debug)]
struct Message(String);

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Message {}

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
            Error::Argument { index, kind, message } => write!(f, "argument {index} is not valid: {kind}: {message}"),
            Error::Diverged { seq, message } => write!(f, "the run diverged from its journal at entry {seq}: {message}"),
            Error::Aborted(e) => write!(f, "the host ended the run: {e}"),
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
    /// error, 2 for compile, I/O and argument errors, 3 for a limit, 70 for an
    /// internal error or a run that the host ended, 74 for a divergence from
    /// the journal, 130 if interrupted.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Runtime { .. } => 1,
            Error::Internal { .. } | Error::Aborted(_) => 70,
            Error::Diverged { .. } => 74,
            Error::Compile(_) | Error::Io(_) | Error::Argument { .. } => 2,
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

/// How a run that did not fail ended. A script ([`Engine::run_program`]) has an
/// `Outcome` with no return value; a call ([`Engine::call`]) has an
/// `Outcome<Returned>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<R = std::convert::Infallible> {
    /// The called function returned.
    Returned(R),
    /// The run ended with this exit code: 0 when a script ends, or the code
    /// that the run passed to `exit`.
    Exited(i32),
    /// The engine suspended the journaled run because every task waits
    /// (SPEC §16.7). To resume it, the host starts the run again with the
    /// same journal, and for a call, the same arguments.
    Suspended(crate::journal::Suspension),
}

/// What a function that [`Engine::call`] called returned. A value converts to [`Json`] as
/// `json.encode` encodes it.
///
/// [`Json`]: stdlib::json::Json
#[derive(Debug, Clone, PartialEq)]
pub enum Returned {
    /// The value, or the payload of `Ok` for a function that returns a `Result`.
    Value(stdlib::json::Json),
    /// The payload of `Err` for a function that returns `Result<T>`, whose error type is `Error`.
    Failed(stdlib::core::Error),
    /// The payload of `Err` for a function that returns `Result<T, E>` with an error type `E`
    /// other than `Error`.
    Err(stdlib::json::Json),
}

impl<R> Outcome<R> {
    /// The exit code of a run that called `exit` or of a script that ended.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Outcome::Exited(c) => Some(*c),
            Outcome::Returned(_) | Outcome::Suspended(_) => None,
        }
    }
}

/// A compiled program, ready to run on any engine (see [`Engine::compile_file`]). It holds the
/// host modules that it was compiled against and runs with their natives.
#[derive(Clone)]
pub struct CompiledProgram {
    program: Arc<Program>,
    host: HostModules,
    cwd: PathBuf,
}

/// Host modules, checked and compiled with the standard library (see [`Config::host_modules`]).
/// A clone is cheap, and every engine whose configuration holds a clone compiles against the
/// same compiled modules. The default holds only the standard library.
#[derive(Clone)]
pub struct HostModules(Arc<Registry>);

impl HostModules {
    /// Checks and compiles `modules`. Fails with [`Error::Compile`] if the Sand source of a
    /// module does not match its natives.
    ///
    /// # Panics
    ///
    /// If two modules have the same name.
    pub fn new(modules: impl IntoIterator<Item = NativeModule>) -> Result<HostModules, Error> {
        HostModules::compile(&modules.into_iter().collect::<Vec<_>>(), true)
    }

    /// Checks and compiles `modules`, with the optimizations of [`Config::optimize`] on or off.
    pub(crate) fn compile(modules: &[NativeModule], optimize: bool) -> Result<HostModules, Error> {
        for (i, m) in modules.iter().enumerate() {
            assert!(!modules[..i].iter().any(|x| x.name() == m.name()), "host module `{}` is added twice", m.name());
        }
        let registry = Registry::for_modules(modules, optimize);
        if registry.base.is_some() {
            registry.snapshot()?;
        }
        Ok(HostModules(registry))
    }
}

impl Default for HostModules {
    fn default() -> Self {
        HostModules(STD.clone())
    }
}

impl fmt::Debug for HostModules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostModules").finish_non_exhaustive()
    }
}

impl fmt::Debug for CompiledProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledProgram").field("cwd", &self.cwd).finish_non_exhaustive()
    }
}

impl CompiledProgram {
    /// The function `name` of the main module, public or private, for [`Engine::call`]. A host
    /// can call a top-level function without type parameters whose types the engine can
    /// describe. For any other name, fails with the reason.
    pub fn function(&self, name: &str) -> Result<Function, FunctionError> {
        match self.program.functions.iter().find(|(n, _)| n == name) {
            Some((_, Ok((proto, signature)))) => Ok(Function { program: self.clone(), proto: *proto, signature: signature.clone() }),
            Some((_, Err(e))) => Err(e.clone()),
            None => Err(FunctionError::NotFound),
        }
    }
}

/// Why [`CompiledProgram::function`] gives no function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionError {
    /// The main module has no top-level function of this name.
    NotFound,
    /// The function has type parameters.
    Generic,
    /// The type of parameter `param`, or the return type if `param` is `None`, is too large to
    /// describe.
    TypeTooLarge { param: Option<String> },
}

impl fmt::Display for FunctionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FunctionError::NotFound => f.write_str("the main module has no top-level function of this name"),
            FunctionError::Generic => f.write_str("the function has type parameters"),
            FunctionError::TypeTooLarge { param: Some(p) } => write!(f, "the type of parameter `{p}` is too large to describe"),
            FunctionError::TypeTooLarge { param: None } => f.write_str("the return type is too large to describe"),
        }
    }
}

impl std::error::Error for FunctionError {}

/// A function of a compiled program's main module (see [`CompiledProgram::function`]).
#[derive(Clone, Debug)]
pub struct Function {
    program: CompiledProgram,
    proto: u32,
    signature: FunctionSignature,
}

impl Function {
    pub fn signature(&self) -> &FunctionSignature {
        &self.signature
    }
}

/// The name, parameters and return type of a function. It shows as the function's
/// declaration, such as `fn main(order: Order) -> Result<Receipt>`.
#[derive(Clone, Debug)]
pub struct FunctionSignature {
    pub name: String,
    /// The name and type of each parameter.
    pub params: Vec<(String, Arc<TypeDesc>)>,
    pub returns: Returns,
}

/// The return type of a function.
#[derive(Clone, Debug)]
pub enum Returns {
    /// A type other than `Result`.
    Value(Arc<TypeDesc>),
    /// `Result<ok, err>`.
    Result { ok: Arc<TypeDesc>, err: Arc<TypeDesc> },
}

impl fmt::Display for FunctionSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn {}(", self.name)?;
        for (i, (name, ty)) in self.params.iter().enumerate() {
            let sep = if i > 0 { ", " } else { "" };
            write!(f, "{sep}{name}: {ty}")?;
        }
        f.write_str(")")?;
        match &self.returns {
            Returns::Value(t) if matches!(**t, TypeDesc::Unit) => Ok(()),
            Returns::Value(t) => write!(f, " -> {t}"),
            Returns::Result { ok, err } if err.is_error() => write!(f, " -> Result<{ok}>"),
            Returns::Result { ok, err } => write!(f, " -> Result<{ok}, {err}>"),
        }
    }
}

/// Resources that the most recent run of an engine used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunStats {
    /// Fuel units spent, as `max_instructions` counts them (SPEC §15.2).
    pub fuel: u64,
    /// The most memory in use at a memory check, as `max_memory` counts it.
    pub peak_memory: usize,
    /// Bytes written to standard output and standard error.
    pub output_bytes: u64,
    pub gc: GcStats,
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
    Feed(crate::feed::StdinFeed),
}

/// Engine configuration (builder style).
#[derive(Clone)]
pub struct Config {
    pub(crate) policy: Policy,
    pub(crate) prompter: Option<Arc<dyn Prompter>>,
    pub(crate) limits: Limits,
    host: HostModules,
    pub(crate) stdout: SharedSink,
    pub(crate) stderr: SharedSink,
    log_sink: Option<Arc<dyn crate::stdlib::log::LogSink>>,
    log_level: Option<crate::stdlib::log::LogLevel>,
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
    pub(crate) time_zone: Option<crate::stdlib::time::TimeZone>,
    context: Option<Arc<dyn std::any::Any + Send + Sync>>,
    durable: Option<crate::journal::Durable>,
    interrupt: InterruptHandle,
}

/// HTTP request destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestTarget {
    /// `http` or `https`.
    pub scheme: String,
    /// The lowercased host name or an IP address.
    pub host: String,
    pub port: u16,
    /// In a journaled run, the sequence number of the journal entry that
    /// records the start of the request. It is the same in every execution
    /// that sends the request, so a filter can derive an idempotency key
    /// from it (SPEC §16.7).
    pub journal_seq: Option<u64>,
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
            host: HostModules::default(),
            stdout: Arc::new(crate::stdio::StdoutSink::default()),
            stderr: Arc::new(crate::stdio::StderrSink::default()),
            log_sink: None,
            log_level: Some(crate::stdlib::log::LogLevel::Info),
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
            context: None,
            durable: None,
            interrupt: InterruptHandle::new(),
        }
    }

    /// Lets `handle` stop the engines made from this configuration, and
    /// their sessions. Without this call, nothing can interrupt them.
    pub fn interrupt(mut self, handle: InterruptHandle) -> Self {
        self.interrupt = handle;
        self
    }

    /// A new engine's interrupt flag and I/O waker, which the interrupt
    /// handle of this configuration reaches.
    fn attach_interrupt(&self) -> (Arc<AtomicBool>, Arc<IoShared>) {
        let (flag, io) = (Arc::new(AtomicBool::new(false)), Arc::new(IoShared::default()));
        self.interrupt.attach(&flag, &io);
        (flag, io)
    }

    /// Makes each run durable (SPEC §16.7): the engine records the
    /// nondeterministic inputs of the run in the journal of `durable`, and
    /// replays the entries that it already holds. A journaled run never
    /// prompts for permissions and disables `net` sockets and HTTP servers.
    pub fn durable(mut self, durable: crate::journal::Durable) -> Self {
        self.durable = Some(durable);
        self
    }

    /// Attaches host data to each run of the engine. Native functions
    /// registered with [`NativeModule::vm_function`] read it with
    /// [`Vm::context`](crate::vm::Vm::context). Setting the context does not
    /// recompile host modules, so a host can share one set of compiled
    /// modules across engines that serve different callers.
    pub fn context(mut self, data: Arc<dyn std::any::Any + Send + Sync>) -> Self {
        self.context = Some(data);
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

    /// Filters the headers of every HTTP request that the engine sends. In
    /// a journaled run, replay sends no request and does not call the
    /// filter.
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
    /// Sets the host modules that scripts can import. By default, scripts can import only the
    /// standard library.
    pub fn host_modules(mut self, host: HostModules) -> Self {
        self.host = host;
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
    pub fn log_sink(mut self, sink: impl crate::stdlib::log::LogSink + 'static) -> Self {
        self.log_sink = Some(Arc::new(sink));
        self
    }
    /// Sets the initial minimum script log level (default [`LogLevel::Info`](crate::stdlib::log::LogLevel::Info)),
    /// or disables script logging if `level` is `None`.
    pub fn log_level(mut self, level: impl Into<Option<crate::stdlib::log::LogLevel>>) -> Self {
        self.log_level = level.into();
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
    pub fn stdin_feed(mut self, feed: &crate::feed::StdinFeed) -> Self {
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
    pub fn time_zone(mut self, zone: crate::stdlib::time::TimeZone) -> Self {
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
    /// Enables or disables loop fusion, loop specialization, inlining, and
    /// loop-invariant hoisting in the programs that the engine compiles. Host
    /// modules compile with the optimizations on.
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

/// Stops, from any thread, the engines and sessions made from the
/// configurations that hold this handle (see [`Config::interrupt`]).
#[derive(Clone, Default)]
pub struct InterruptHandle {
    engines: Arc<Mutex<Vec<Attached>>>,
}

/// The interrupt flag and the I/O waker of one engine that an
/// [`InterruptHandle`] stops.
struct Attached {
    flag: Weak<AtomicBool>,
    io: Weak<IoShared>,
}

impl InterruptHandle {
    pub fn new() -> InterruptHandle {
        InterruptHandle::default()
    }

    /// Stops the compilation or run in progress in each engine and session
    /// of this handle. The run fails with [`Error::Interrupted`]. An engine
    /// forgets an interrupt that comes before its next compilation or run.
    pub fn interrupt(&self) {
        for e in self.engines.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if let (Some(flag), Some(io)) = (e.flag.upgrade(), e.io.upgrade()) {
                flag.store(true, Ordering::SeqCst);
                io.wake();
            }
        }
    }

    fn attach(&self, flag: &Arc<AtomicBool>, io: &Arc<IoShared>) {
        let mut engines = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        engines.retain(|e| e.flag.strong_count() > 0);
        engines.push(Attached { flag: Arc::downgrade(flag), io: Arc::downgrade(io) });
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
    pub natives: Arc<Vec<NativeEntry>>,
    pub decls: Arc<NativeDecls>,
    pub sources: BTreeMap<Cow<'static, str>, Source>,
    pub optimize: bool,
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
            optimize: true,
            base: None,
            snapshot: OnceLock::new(),
            core: OnceLock::new(),
            partial: Mutex::default(),
        }
    }

    fn for_modules(modules: &[NativeModule], optimize: bool) -> Arc<Registry> {
        let std = STD.clone();
        if modules.is_empty() {
            return std;
        }
        let mut r = Registry {
            natives: std.natives.clone(),
            decls: std.decls.clone(),
            sources: std.sources.clone(),
            optimize,
            ..Registry::empty()
        };
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
    interrupt: Arc<AtomicBool>,
    shared: Arc<IoShared>,
    /// "Always" prompt answers, remembered for the lifetime of the engine
    /// (SPEC 14.3): shared by import checks and every run (and VM).
    always: Always,
    stats: RunStats,
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
    /// The natives that `program` calls.
    pub natives: Arc<Vec<NativeEntry>>,
    /// The main module's lint warnings (if asked for).
    pub lints: Option<Vec<crate::lint::LintWarning>>,
    /// When the run's wall-time limit passes, counted from the start of
    /// compilation, excluding import prompts.
    pub deadline: Option<Instant>,
    /// Base snapshot when no additional native modules were compiled, allowing `program` to be recycled.
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
    crate::platform::on_deep_stack(f).unwrap_or_else(|p| Err(Error::compiler_bug(&crate::unwind::panic_text(&*p))))
}

impl Engine {
    /// Creates an engine with `config`.
    pub fn new(config: Config) -> Engine {
        let origin = config.clock.now();
        let (interrupt, shared) = config.attach_interrupt();
        Engine {
            origin,
            config: Arc::new(config),
            interrupt,
            shared,
            always: Arc::default(),
            stats: RunStats::default(),
            root: OnceLock::new(),
            spare: Mutex::default(),
        }
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

    /// Resources used by the most recent run, including runs that failed or
    /// exceeded a limit. Compilation is not included.
    pub fn run_stats(&self) -> RunStats {
        self.stats
    }

    /// Type checks source text as [`Engine::check_source`] does, and lints
    /// it. Compile errors, and a lint that runs out of budget, are an
    /// [`Error::Compile`].
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

    /// Starts an interactive session ([`Session`]) with this engine's
    /// configuration. The session shares the engine's "always" answers to
    /// permission prompts and its [`InterruptHandle`].
    pub fn session(&self) -> Session {
        let (interrupt, shared) = self.config.attach_interrupt();
        Session::new(Engine {
            config: self.config.clone(),
            interrupt,
            shared,
            always: self.always.clone(),
            stats: RunStats::default(),
            root: OnceLock::new(),
            spare: Mutex::default(),
            origin: self.origin,
        })
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn registry(&self) -> &Arc<Registry> {
        &self.config.host.0
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
            Ok(std::task::Poll::Pending) => Err(Error::compiler_bug("compilation waited")),
            Err(msg) => Err(Error::compiler_bug(&msg)),
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

    pub async fn run_source(&mut self, name: &str, source: &str) -> Result<Outcome, Error> {
        crate::unwind::guard(self.run_source_inner(name, source)).await
    }

    async fn run_source_inner(&mut self, name: &str, source: &str) -> Result<Outcome, Error> {
        let (root, normal) = self.source_root().clone();
        let compiled = self.compile(name, source, &normal, None).await?;
        self.run_script(compiled, root).await
    }

    pub async fn run_file(&mut self, path: impl AsRef<Path>) -> Result<Outcome, Error> {
        crate::unwind::guard(self.run_file_inner(path.as_ref())).await
    }

    async fn run_file_inner(&mut self, path: &Path) -> Result<Outcome, Error> {
        let (compiled, cwd) = self.compile_main(path).await?;
        self.run_script(compiled, cwd).await
    }

    /// Compiles a file and its imports without running it. The returned program runs with
    /// [`Engine::run_program`] on any engine, with the host modules of this engine, so a host
    /// that runs the same code many times compiles it once.
    ///
    /// Import checks happen during compilation, under this engine's policy and file system.
    pub async fn compile_file(&mut self, path: impl AsRef<Path>) -> Result<CompiledProgram, Error> {
        crate::unwind::guard(self.compile_file_inner(path.as_ref())).await
    }

    async fn compile_file_inner(&mut self, path: &Path) -> Result<CompiledProgram, Error> {
        let (compiled, cwd) = self.compile_main(path).await?;
        Ok(CompiledProgram { program: compiled.program, host: self.config.host.clone(), cwd })
    }

    /// Compiles the file at `path` as the main module, and returns it with the directory that
    /// it runs in.
    async fn compile_main(&mut self, path: &Path) -> Result<(Compiled, PathBuf), Error> {
        let (name, source, root) = self.read_main(path)?;
        let compiled = self.compile(&name, &source, &root, Some(path)).await?;
        Ok((compiled, self.config.cwd.clone().unwrap_or(root)))
    }

    /// Runs a program from [`Engine::compile_file`] with the host modules that it was compiled
    /// against, and with the configuration of this engine otherwise. The wall-time limit counts
    /// from the start of the run.
    pub async fn run_program(&mut self, program: &CompiledProgram) -> Result<Outcome, Error> {
        let compiled = self.start_program(program);
        crate::unwind::guard(self.run_script(compiled, program.cwd.clone())).await
    }

    /// Runs the program of `function` as [`Engine::run_program`] does, but calls `function`
    /// after the module initializers instead of ending, and returns what it returns.
    ///
    /// Each argument is JSON text, which the engine decodes to the type of its parameter as
    /// `json.decode` does; an argument that does not decode fails the call with
    /// [`Error::Argument`]. A journaled run replays the same call, so the host gives the same
    /// arguments to each execution of the run. Fails with [`Error::Internal`] if the number of
    /// arguments is not the number of parameters.
    pub async fn call(&mut self, function: &Function, args: &[&str]) -> Result<Outcome<Returned>, Error> {
        let sig = function.signature();
        if args.len() != sig.params.len() {
            let message = format!("`{}` takes {} arguments, and the call gives {}", sig.name, sig.params.len(), args.len());
            return Err(Error::Internal { message, trace: Vec::new() });
        }
        let compiled = self.start_program(&function.program);
        let cwd = function.program.cwd.clone();
        crate::unwind::guard(self.call_compiled(compiled, cwd, function, args)).await
    }

    async fn call_compiled(
        &mut self,
        compiled: Compiled,
        cwd: PathBuf,
        function: &Function,
        args: &[&str],
    ) -> Result<Outcome<Returned>, Error> {
        let sig = function.signature();
        let r = self
            .execute(compiled, cwd, async |vm: &mut Vm| match call_function(vm, function, args).await? {
                Called::Returned(v) => {
                    vm.pinned.push(v);
                    let r = match vm.end_tasks().await {
                        Ok(Some(c)) => Ok(Ok(Outcome::Exited(c))),
                        Ok(None) => returned(vm, v, &sig.returns).map(|r| Ok(Outcome::Returned(r))).map_err(|c| vm.trap_of(c, |_| vec![])),
                        Err(t) => Err(t),
                    };
                    vm.pinned.pop();
                    r
                }
                Called::Exited(c) => Ok(Ok(Outcome::Exited(c))),
                Called::Suspended(s) => Ok(Ok(Outcome::Suspended(s))),
                Called::BadArgument { index, error } => Ok(Err(Error::Argument { index, kind: error.kind, message: error.message })),
            })
            .await;
        match r {
            Ok(r) => r,
            Err(Trap::Exit(c)) => Ok(Outcome::Exited(c)),
            Err(t) => Err(trap_error(t)),
        }
    }

    /// Prepares a run of `program` on this engine.
    fn start_program(&self, program: &CompiledProgram) -> Compiled {
        self.clear_interrupt();
        let natives = program.host.0.natives.clone();
        Compiled { program: program.program.clone(), natives, lints: None, deadline: self.deadline(), snapshot: None }
    }

    /// Runs `compiled` as a script: its module initializers, then the tasks that are left.
    async fn run_script(&mut self, compiled: Compiled, cwd: PathBuf) -> Result<Outcome, Error> {
        let entry = compiled.program.entry;
        let r = self
            .execute(compiled, cwd, async |vm: &mut Vm| {
                let r = vm.run_main(entry).await;
                end_run(vm, r).await
            })
            .await;
        match r {
            Ok(RunEnd::Finished(_)) => Ok(Outcome::Exited(0)),
            Ok(RunEnd::Exit(c)) | Err(Trap::Exit(c)) => Ok(Outcome::Exited(c)),
            Ok(RunEnd::Suspended(s)) => Ok(Outcome::Suspended(s)),
            Err(t) => Err(trap_error(t)),
        }
    }

    pub async fn run_tests(&mut self, path: impl AsRef<Path>) -> Result<TestReport, Error> {
        crate::unwind::guard(self.run_tests_inner(path.as_ref())).await
    }

    async fn run_tests_inner(&mut self, path: &Path) -> Result<TestReport, Error> {
        let (compiled, cwd) = self.compile_main(path).await?;
        let prog = compiled.program;
        // The wall-time limit bounds the whole test run.
        let mut deadline = compiled.deadline;
        let mut report = TestReport::default();
        for (tname, proto) in prog.tests.clone() {
            let mut vm = self.new_vm(prog.clone(), compiled.natives.clone(), cwd.clone(), true);
            vm.deadline = deadline;
            let init = vm.run_main(prog.entry).await;
            let result = match init {
                Ok(RunEnd::Finished(_)) => {
                    vm.ready.clear();
                    let r = vm.run_main(proto).await;
                    match end_run(&mut vm, r).await {
                        Ok(RunEnd::Finished(_)) | Ok(RunEnd::Exit(0)) => Ok(()),
                        Ok(RunEnd::Exit(c)) => Err(format!("exited with code {c}")),
                        Ok(RunEnd::Suspended(_)) => unreachable!("a test runs without a journal"),
                        Err(t) => Err(trap_message(t)),
                    }
                }
                Ok(RunEnd::Exit(c)) => Err(format!("module exited with code {c}")),
                Ok(RunEnd::Suspended(_)) => unreachable!("a test runs without a journal"),
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

    /// A VM that runs `prog`, which calls `natives`.
    pub(crate) fn new_vm(&self, prog: Arc<Program>, natives: Arc<Vec<NativeEntry>>, cwd: PathBuf, quiet: bool) -> Vm {
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
            log_level: c.log_level,
            log_sink: if quiet { Some(Arc::new(crate::stdlib::log::NullLogSink)) } else { c.log_sink.clone() },
            interrupt: self.interrupt.clone(),
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
            context: c.context.clone(),
        };
        let mut vm = Vm::new(prog, natives, host);
        vm.io = IoTable::with_shared(self.shared.clone());
        if c.gc_stress {
            vm.heap.stress = true;
            vm.heap.next_gc = 0;
        }
        vm.retry_stress = c.retry_stress;
        vm
    }

    /// Runs `run` on a new VM of `compiled`, with the configured journal, and records the
    /// run's statistics.
    async fn execute<T>(&mut self, compiled: Compiled, cwd: PathBuf, run: impl AsyncFnOnce(&mut Vm) -> Result<T, Trap>) -> Result<T, Trap> {
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
        let mut vm = self.new_vm(prog.clone(), compiled.natives, cwd, false);
        vm.deadline = compiled.deadline;
        let r = match self.attach_journal(&mut vm) {
            Ok(()) => {
                let r = run(&mut vm).await;
                match r {
                    Err(Trap::Diverged(_) | Trap::Interrupted | Trap::Abort(_)) => r,
                    r => match vm.flush_journal().await {
                        Ok(()) => r,
                        Err(t) => Err(t),
                    },
                }
            }
            Err(t) => Err(t),
        };
        let _ = self.config.stdout.flush();
        vm.heap.note_peak();
        self.stats = RunStats { fuel: vm.meter.used(), peak_memory: vm.heap.peak(), output_bytes: vm.host.output_bytes, gc: vm.heap.stats };
        if self.config.exit_after_run {
            std::mem::forget(vm);
        } else {
            drop(vm);
            self.recycle(prog, compiled.snapshot);
        }
        r
    }

    /// Attaches the configured journal to `vm`, the VM of a run. A
    /// journaled run never prompts and ignores "always" answers of earlier
    /// runs, since replay cannot repeat a prompt's answer.
    fn attach_journal(&self, vm: &mut Vm) -> Result<(), Trap> {
        let Some(durable) = &self.config.durable else { return Ok(()) };
        vm.host.prompter = None;
        vm.host.always = Always::default();
        let state = crate::vm::journal::start(durable.journal.clone(), self.config.clock.now(), durable.suspend_after, &self.config.limits)
            .map_err(Trap::Diverged)?;
        vm.attach_journal(state, self.config.seed.is_some()).map_err(Trap::Diverged)
    }

    // ------------------------------------------------------------------
    // Loading and compiling

    /// Checks `path` against the import policy and prompts if `prompt` is true.
    async fn check_import(&self, path: &Path, prompt: bool) -> Result<bool, &'static str> {
        let cap = Capability::Import(path.to_path_buf());
        if let Some(&d) = self.always.lock().unwrap_or_else(|e| e.into_inner()).get(&cap) {
            return Ok(d);
        }
        match self.config.policy.check_normalized_in(&cap, self.config.fs.space()) {
            Decision::Allow => Ok(true),
            Decision::Deny => Ok(false),
            Decision::Prompt => match &self.config.prompter {
                Some(p) if prompt => {
                    let answer = p.prompt(cap.clone()).await;
                    let allowed = matches!(answer, PromptAnswer::AllowOnce | PromptAnswer::AllowAlways);
                    if matches!(answer, PromptAnswer::AllowAlways | PromptAnswer::DenyAlways) {
                        self.always.lock().unwrap_or_else(|e| e.into_inner()).insert(cap, allowed);
                    }
                    Ok(allowed)
                }
                Some(_) => Err("cannot prompt during check"),
                None => Err("a prompter is not configured"),
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
        let discovery = Discovery { snap: &snap, root, deadline, prompt: options.prompt };
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
        let budget = self.compile_budget(deadline);
        let spare = match self.spare.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some((s, p)) if Arc::ptr_eq(&s, &snap) => Some(p),
            _ => None,
        };
        let natives = g.has_natives(&g.order);
        let (base, registry, optimize) = (snap.clone(), self.registry().clone(), self.config.optimize);
        let (program, lints) =
            on_compile_stack(move || compile_program(&base, &mut g, &registry, &budget, main_file, options.lint, optimize, spare))?;
        Ok(Compiled { program, natives: self.registry().natives.clone(), lints, deadline, snapshot: (!natives).then_some(snap) })
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
        let mut stale_cache: HashMap<usize, bool> = HashMap::new();
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
            let allowed = self.check_import(&target, d.prompt).await;
            *prompted += self.config.clock.now().saturating_duration_since(asked);
            if allowed != Ok(true) {
                let hint = allowed.err().map(|why| format!("; {}", self.format_unprompted_import_reason(&target, why))).unwrap_or_default();
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
        if !crate::syntax::lexer::is_ident(name) {
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

/// How [`call_function`] ended.
enum Called {
    Returned(Value),
    Exited(i32),
    Suspended(crate::journal::Suspension),
    BadArgument { index: usize, error: stdlib::core::Error },
}

/// Runs the module initializers of `vm`'s program, decodes `args` for the parameters of
/// `function`, and calls it. The tasks that the run started are left as they are.
async fn call_function(vm: &mut Vm, function: &Function, args: &[&str]) -> Result<Called, Trap> {
    let entry = vm.prog.entry;
    match vm.run_main(entry).await? {
        RunEnd::Finished(_) => {}
        RunEnd::Exit(c) => return Ok(Called::Exited(c)),
        RunEnd::Suspended(s) => return Ok(Called::Suspended(s)),
    }
    vm.release_main();
    let pinned = vm.pinned.len();
    let r = call_decoded(vm, function, args).await;
    vm.pinned.truncate(pinned);
    r
}

/// Decodes `args` and calls `function` with them. Keeps each argument in `vm.pinned`, so
/// that it lives while the functions that fill in its computed defaults run.
async fn call_decoded(vm: &mut Vm, function: &Function, args: &[&str]) -> Result<Called, Trap> {
    let first = vm.pinned.len();
    for (index, ((_, desc), arg)) in function.signature().params.iter().zip(args).enumerate() {
        let text = vm.new_str(*arg);
        match stdlib::json::decode(vm, desc, crate::native::ty::Val::from_raw(text)).map_err(|c| vm.trap_of(c, |_| vec![]))? {
            stdlib::json::Decoding::Done(v) => vm.pinned.push(v),
            stdlib::json::Decoding::Pending(v, fills) => {
                vm.pinned.extend([v, fills]);
                for fill in vm.heap.list(fills).to_vec() {
                    match vm.run_call(fill, &[]).await? {
                        RunEnd::Finished(_) => {}
                        RunEnd::Exit(c) => return Ok(Called::Exited(c)),
                        RunEnd::Suspended(s) => return Ok(Called::Suspended(s)),
                    }
                    vm.release_main();
                }
                vm.pinned.pop();
            }
            stdlib::json::Decoding::Failed(error) => return Ok(Called::BadArgument { index, error }),
        }
    }
    let args = vm.pinned[first..].to_vec();
    let closure = vm.heap.alloc(Obj::Closure(function.proto, Box::new([])));
    match vm.run_call(closure, &args).await? {
        RunEnd::Finished(v) => Ok(Called::Returned(v)),
        RunEnd::Exit(c) => Ok(Called::Exited(c)),
        RunEnd::Suspended(s) => Ok(Called::Suspended(s)),
    }
}

/// `v`, the value that a function that returns `returns` returned.
fn returned(vm: &Vm, v: Value, returns: &Returns) -> Result<Returned, crate::vm::Ctl> {
    match returns {
        Returns::Value(_) => Ok(Returned::Value(stdlib::json::to_json(vm, v)?)),
        Returns::Result { err, .. } => match vm.as_result(v) {
            Ok(v) => Ok(Returned::Value(stdlib::json::to_json(vm, v)?)),
            Err(e) if err.is_error() => Ok(Returned::Failed(stdlib::core::Error::from_value(vm, e, &mut Conversion::new())?)),
            Err(e) => Ok(Returned::Err(stdlib::json::to_json(vm, e)?)),
        },
    }
}

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
        Trap::Exit(c) => format!("exited with code {c}"),
        t => trap_error(t).to_string(),
    }
}

/// The error of a run that `t` ended, other than an exit.
pub(crate) fn trap_error(t: Trap) -> Error {
    match t {
        Trap::Error { message, trace } => Error::Runtime { message, trace },
        Trap::Internal { message, trace } => Error::Internal { message, trace },
        Trap::Limit(k) => Error::Limit(k),
        Trap::Interrupted => Error::Interrupted,
        Trap::Abort(a) => Error::Aborted(a),
        Trap::Diverged(Divergence { seq, message }) => Error::Diverged { seq, message },
        Trap::Exit(c) => Error::Internal { message: format!("an exit with code {c} where none is possible"), trace: Vec::new() },
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
        Some(main) => Some(crate::lint::lint_module(main, &main.source, &cp, budget).map_err(|stop| match stop {
            Stop::Fuel => Error::Compile(vec![
                sm.resolve(&Diag::error(Span::new(main_file, 0, 0), "source too complex to lint: exceeded the lint budget")),
            ]),
            stop => budget_error(stop, sm, main_file),
        })?),
        None => None,
    };
    let native_info = if host { native_info(&cp, registry) } else { crate::modules::NativeInfo::default() };
    let mut program = spare.unwrap_or_else(|| snap.program.clone());
    let main = Some(cp.main_module);
    let input = CompileInput { checked: &cp, sm, natives: &registry.decls, native_info, budget, initialized: None, optimize, main };
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
    fn host_module_source_follows_the_optimize_setting() {
        let src = "fn double(x: int) -> int { x * 2 }\npub fn doubled_total(xs: [int]) -> int { xs.map(double).filter(x => x > 2).sum() }";
        let code = |optimize: bool| {
            let mut m = NativeModule::new("host");
            m.source(src);
            let host = HostModules::compile(&[m], optimize).unwrap();
            let snap = host.0.snapshot().unwrap();
            let proto = snap.program.protos.iter().find(|p| p.name == "doubled_total").expect("doubled_total is compiled");
            proto.code.clone()
        };
        let script = |optimize: bool| {
            let engine = Engine::new(Config::new().optimize(optimize));
            let c = engine.compile_now("main.sand", src, Path::new("."), None, false).unwrap();
            let proto = c.program.protos.iter().find(|p| p.name == "doubled_total").expect("doubled_total is compiled");
            proto.code.clone()
        };
        let (fast, slow) = (code(true), code(false));
        assert_ne!(fast, slow);
        assert_eq!(fast.len(), script(true).len());
        assert_eq!(slow.len(), script(false).len());
    }

    #[test]
    fn configs_with_one_host_modules_share_its_registry() {
        let host = HostModules::new([NativeModule::new("a")]).unwrap();
        let one = Engine::new(Config::new().host_modules(host.clone()));
        let two = Engine::new(Config::new().seed(1).host_modules(host.clone()));
        assert!(Arc::ptr_eq(one.registry(), two.registry()));
        assert!(Arc::ptr_eq(one.registry(), &host.0));
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
        let host = HostModules::new([hang_module(release.clone(), progress.clone())]).unwrap();
        let config = Config::new().host_modules(host).limits(limits);
        let mut engine = Engine::new(config.clone());
        let r = rt().block_on(engine.run_source("main.sand", "import hung\nhung.hang()"));
        assert!(matches!(r, Err(Error::Limit(LimitKind::WallTime))), "{r:?}");

        let handle = InterruptHandle::new();
        let mut engine = Engine::new(config.limits(Limits::default()).interrupt(handle.clone()));
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
