//! REPL sessions: inputs checked incrementally and run, exactly once, in a
//! persistent VM.
//!
//! A session is one script module whose items arrive one input at a time.
//! Each input is parsed, checked in the context of the previous inputs
//! ([`Checker::check_cell`]), compiled by appending to the session's program
//! ([`compile_more`]), and run in the session's VM, whose heap, globals and
//! tasks persist. A session only appends to the checker's tables and the
//! program, so nothing a live value refers to is ever renumbered.

use crate::check::budget::CompileBudget;
use crate::check::{Checker, ModKind, ParsedModule};
use crate::compile::{CompileInput, Incremental, ProgramMark, compile_more};
use crate::diag::{Diag, FileId, Span};
use crate::engine::{Discovery, Engine, Error, InterruptHandle, Registry, TestReport, TestResult, on_compile_stack};
use crate::modules::{GraphMark, ModuleGraph, NativeInfo, Snapshot, budget_error, native_info};
use crate::platform::Instant;
use crate::policy::{Capability, PromptAnswer};
use crate::syntax::ast::{ItemKind, StmtKind};
use crate::types::{FnId, ModId, TypeId};
use crate::unwind::CatchUnwind;
use crate::vm::bytecode::Program;
use crate::vm::sched::{RunEnd, Stop};
use crate::vm::value::Value;
use crate::vm::{CtlKind, Trap, Vm};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::Poll;

/// An interactive session: a sequence of inputs (statements, expressions and
/// declarations, like the contents of a module) checked and run one at a
/// time, each in the context of the previous ones. Created by
/// [`Engine::session`]; uses the engine's configuration (policy, prompter,
/// limits, streams and modules).
///
/// ```no_run
/// # async fn demo() -> Result<(), sandlang::Error> {
/// use sandlang::{Config, Engine};
/// let mut session = Engine::new(Config::new()).session();
/// session.eval("var count = 0\nfn bump() -> int { count += 1\n count }").await?;
/// assert_eq!(session.eval("bump()").await?, Some("1".to_string()));
/// assert_eq!(session.eval("bump() + 10").await?, Some("12".to_string()));
/// # Ok(()) }
/// ```
///
/// * Every input runs exactly once. Top-level `let`/`var` bindings become
///   globals that later inputs can read (and, for `var`, assign); later
///   definitions of a name shadow earlier ones for later inputs, while
///   functions and values created earlier keep referring to the definitions
///   they were checked against.
/// * An input that fails to compile has no effect. An input that fails with an
///   error (or exceeds a limit, is interrupted or calls `exit`) keeps its declarations
///   and the effects that happened before the failure, including global
///   assignments; its remaining globals stay uninitialized. All tasks are
///   then cancelled.
/// * Tasks spawned by an input keep running, alongside later inputs, and
///   between inputs while the front end calls [`Session::run_tasks_until`].
/// * The instruction, CPU time, wall time and output limits apply to each
///   input separately; the memory limit applies to the session's heap.
/// * After an internal error (a bug in the engine or in a host native,
///   including a caught Rust panic), the VM may be inconsistent, so the
///   session is reset (as by [`Session::reset`]): the call fails with
///   [`Error::Internal`], whose message ends with [`RESET_NOTE`], and
///   [`Session::was_reset`] returns true.
pub struct Session {
    /// Shares the configuration, natives and "always" answers of the engine
    /// that created the session, with its own interrupt flag.
    engine: Engine,
    root: PathBuf,
    cwd: PathBuf,
    state: Option<Box<State>>,
    inputs: u32,
    exit_code: Option<i32>,
    /// Whether the last call reset the session after an internal error.
    was_reset: bool,
    /// The wall-time limit of each input (see [`Session::set_wall_time`]).
    wall_time: Option<std::time::Duration>,
}

/// The suffix of the error message of a call that reset the session.
pub const RESET_NOTE: &str = " (the session was reset)";

/// A started session.
struct State {
    snapshot: Arc<Snapshot>,
    graph: ModuleGraph,
    checker: Checker,
    compiler: Incremental,
    vm: Vm,
    /// The entry that initializes the standard modules, until it has run.
    pending: Option<u32>,
    /// Set while an input's imports are being loaded or it is running, so
    /// that the work of an `eval` future dropped there can be undone.
    loading: Option<GraphMark>,
    running: bool,
    /// A permission prompt a task needed while the front end waited for
    /// input (see [`Session::run_tasks_until`]), to ask once it has it.
    deferred_prompt: Option<Capability>,
}

/// Name of the session module (for qualified names of its functions).
const SESSION_MODULE: &str = "repl";

impl Session {
    pub(crate) fn new(engine: Engine) -> Session {
        let config = engine.config();
        let fs = &config.fs;
        let root = config.module_root.clone().or_else(|| config.cwd.clone()).unwrap_or_else(|| fs.default_dir());
        let cwd = config.cwd.clone().unwrap_or_else(|| root.clone());
        let root = fs.resolve(&fs.absolute(&root));
        let wall_time = config.limits.max_wall_time;
        Session { engine, root, cwd, state: None, inputs: 0, exit_code: None, was_reset: false, wall_time }
    }

    /// Checks, compiles and runs one input. Returns the debug form of its
    /// value: that of its final expression statement or of a top-level
    /// `return`, unless the input ends with `;` or the value is `()`.
    ///
    /// A failed top-level `?` fails the input with the error. If the input calls `exit(code)`, the result is `Ok(None)`
    /// and [`Session::exit_code`] returns the code.
    pub async fn eval(&mut self, source: &str) -> Result<Option<String>, Error> {
        self.eval_named(None, source).await
    }

    /// Like [`Session::eval`], naming the input `name` in diagnostics and
    /// traces (e.g. `main.sand` for a program that a session continues),
    /// instead of `<input N>`.
    pub async fn eval_as(&mut self, name: &str, source: &str) -> Result<Option<String>, Error> {
        self.eval_named(Some(name), source).await
    }

    async fn eval_named(&mut self, name: Option<&str>, source: &str) -> Result<Option<String>, Error> {
        self.was_reset = false;
        let r = CatchUnwind::new(self.eval_inner(name, source)).await;
        self.settle(r)
    }

    async fn eval_inner(&mut self, name: Option<&str>, source: &str) -> Result<Option<String>, Error> {
        self.exit_code = None;
        self.engine.clear_interrupt();
        self.start().await?;
        self.inputs += 1;
        let name = name.map_or_else(|| format!("<input {}>", self.inputs), str::to_string);
        let want_value = !source.trim_end().ends_with(';');
        // The wall-time limit counts from here, for compiling and running.
        let deadline = self.deadline();
        let (entry, value, returns, deadline) = self.compile_input(&name, source, want_value, deadline).await?;
        let show = want_value && (returns || value.is_some_and(|t| !matches!(t, TypeId::UNIT | TypeId::NEVER)));
        self.run(entry, show, deadline).await
    }

    /// The type of an expression, checked in the context of the session's
    /// inputs without running it (or changing the session).
    pub fn type_of(&mut self, expr: &str) -> Result<String, Error> {
        self.was_reset = false;
        let r = crate::unwind::catch(|| self.type_of_inner(expr));
        self.settle(r)
    }

    fn type_of_inner(&mut self, expr: &str) -> Result<String, Error> {
        self.engine.clear_interrupt();
        let registry = self.engine.registry().clone();
        let budget = self.engine.compile_budget(self.deadline());
        let st = self.state()?;
        let mark = st.graph.mark();
        let file = st.graph.sm.add("<type>", expr);
        let (sm, mut checker, expr) = (st.graph.sm.clone(), st.checker.clone(), expr.to_owned());
        st.graph.rollback(&mark);
        on_compile_stack(move || {
            let resolve = |ds: &[Diag]| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect());
            let res = crate::syntax::parse_module(&expr, file);
            let mut errors = res.errors;
            let is_expr = matches!(res.module.items.as_slice(), [item] if matches!(&item.kind, ItemKind::Stmt(s) if matches!(s.kind, StmtKind::Expr(_))));
            if errors.is_empty() && !is_expr {
                errors.push(Diag::error(Span::new(file, 0, expr.len() as u32), "expected an expression"));
            }
            if !errors.is_empty() {
                return Err(resolve(&errors));
            }
            let cell = ParsedModule {
                name: SESSION_MODULE.into(),
                file,
                source: sm.file(file).clone(),
                ast: res.module,
                comments: Vec::new(),
                kind: ModKind::Script { main: true },
                imports: HashMap::new(),
            };
            checker.type_query = Some(Default::default());
            let checked = checker.check_cell(&[], &cell, true, &registry.by_name, &budget);
            if let Some(stop) = budget.stopped() {
                return Err(budget_error(stop, &sm, file));
            }
            checked.map_err(|ds| resolve(&ds))?;
            Ok(checker.cell_value.map(|t| checker.show_value_type(t)).unwrap_or_else(|| "()".into()))
        })
    }

    /// Runs every `test` block entered so far (the latest of those with the
    /// same name), each as the main task of the session's VM, against the
    /// current state; their effects persist.
    pub async fn run_tests(&mut self) -> Result<TestReport, Error> {
        self.was_reset = false;
        let r = CatchUnwind::new(self.run_tests_inner()).await;
        self.settle(r)
    }

    async fn run_tests_inner(&mut self) -> Result<TestReport, Error> {
        self.start().await?;
        let mut tests: Vec<(String, u32)> = Vec::new();
        for (name, proto) in &self.state()?.vm.prog.tests {
            match tests.iter_mut().find(|(n, _)| n == name) {
                Some(t) => t.1 = *proto,
                None => tests.push((name.clone(), *proto)),
            }
        }
        let mut report = TestReport::default();
        for (name, proto) in tests {
            self.exit_code = None;
            let deadline = self.deadline();
            let message = match self.run(proto, false, deadline).await {
                Ok(_) => match self.exit_code {
                    Some(0) | None => None,
                    Some(c) => Some(format!("exited with code {c}")),
                },
                Err(Error::Interrupted) => return Err(Error::Interrupted),
                Err(Error::Runtime { message, .. }) => Some(message),
                Err(e) => Some(e.to_string()),
            };
            report.results.push(TestResult { name, passed: message.is_none(), message });
            if self.poisoned() {
                // No more tests on a VM that may be inconsistent.
                if let Some(TestResult { message: Some(m), .. }) = report.results.last_mut() {
                    m.push_str(RESET_NOTE);
                }
                break;
            }
        }
        self.exit_code = None;
        Ok(report)
    }

    /// Sets the wall-time limit.
    pub fn set_wall_time(&mut self, limit: Option<std::time::Duration>) {
        self.wall_time = limit;
    }

    /// Get the deadline, if a wall-time limit is set.
    fn deadline(&self) -> Option<Instant> {
        self.wall_time.and_then(|t| self.engine.config().clock.now().checked_add(t))
    }

    /// The code passed to `exit` by the last input, if it called `exit`.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Forgets all inputs: the next input starts a fresh session. "Always"
    /// answers to permission prompts are kept (they belong to the engine).
    pub fn reset(&mut self) {
        self.state = None;
        self.inputs = 0;
        self.exit_code = None;
    }

    /// Whether the last call of [`Session::eval`], [`Session::run_tests`],
    /// [`Session::type_of`] or [`Session::run_tasks_until`] reset the session
    /// because of an internal error.
    pub fn was_reset(&self) -> bool {
        self.was_reset
    }

    /// Runs the tasks that earlier inputs left running until `until`
    /// completes, and returns its output. A front end calls it with a future
    /// that resolves to the next input, so that tasks run between inputs.
    ///
    /// Instruction, CPU time and output limits apply to each call as to
    /// an input; the wall-time limit does not apply (waiting for input may
    /// take any time). Permission prompts wait until `until` has completed,
    /// so that they do not compete with the front end for its input; no
    /// script code runs while one waits. A front end that reads the
    /// process's standard input should pass it on through a
    /// [`crate::StdinFeed`].
    ///
    /// If a task fails with an error, exceeds a limit or is interrupted, every task is
    /// cancelled (as after a failed input), the error is passed to `on_error`
    /// right away, and the call goes on waiting for `until`. If a task calls
    /// `exit`, the call returns `None` at once ([`Session::exit_code`] has the
    /// code) and drops `until`.
    pub async fn run_tasks_until<F: Future>(&mut self, until: F, mut on_error: impl FnMut(Error)) -> Option<F::Output> {
        self.was_reset = false;
        let mut until = std::pin::pin!(until);
        let runnable = self.state.as_mut().is_some_and(|st| {
            st.recover();
            !st.vm.ready.is_empty() || !st.vm.io.is_empty()
        });
        if !runnable {
            return Some(until.await);
        }
        // Run the tasks until `until` completes (the run is then dropped,
        // where it waits) or they stop.
        let stopped = {
            let mut run = std::pin::pin!(CatchUnwind::new(self.run_idle()));
            std::future::poll_fn(|cx| {
                if let Poll::Ready(v) = until.as_mut().poll(cx) {
                    return Poll::Ready(Err(v));
                }
                run.as_mut().poll(cx).map(Ok)
            })
            .await
        };
        let output = match stopped {
            Err(v) => {
                if let Some(st) = self.state.as_ref() {
                    st.vm.flush_stdout();
                }
                v
            }
            Ok(r) => {
                match self.settle(r) {
                    Ok(false) => {}
                    Ok(true) => return None,
                    Err(e) => on_error(e),
                }
                until.await
            }
        };
        // A prompt a task needed meanwhile is asked now; the task retries
        // its call when it runs again.
        if let Some(st) = self.state.as_mut()
            && let Some(cap) = st.deferred_prompt.take()
        {
            let answer = match self.engine.config().prompter.clone() {
                Some(p) => p.prompt(cap.clone()).await,
                None => PromptAnswer::DenyOnce,
            };
            st.vm.apply_answer(cap, answer);
        }
        Some(output)
    }

    /// Cancels every task that earlier inputs left running, as a failed input
    /// does. Returns how many there were.
    pub fn cancel_tasks(&mut self) -> usize {
        let Some(st) = self.state.as_mut() else { return 0 };
        st.recover();
        let n = st.vm.live_tasks;
        st.vm.abandon_tasks();
        st.deferred_prompt = None;
        n
    }

    /// Whether an internal error broke the session's VM.
    fn poisoned(&self) -> bool {
        self.state.as_ref().is_some_and(|st| st.vm.broken)
    }

    /// The result of a call, given `r` (`Err` if a Rust panic was caught).
    /// After an internal error, resets the session and says so in the error.
    fn settle<T>(&mut self, r: Result<Result<T, Error>, String>) -> Result<T, Error> {
        let r = match r {
            Ok(r) if !self.poisoned() => return r,
            Ok(Err(Error::Internal { message, trace })) => Err(Error::Internal { message: format!("{message}{RESET_NOTE}"), trace }),
            Ok(r) => r,
            Err(msg) => Err(Error::Internal { message: format!("{msg}{RESET_NOTE}"), trace: vec![] }),
        };
        self.reset();
        self.was_reset = true;
        r
    }

    /// A handle that interrupts the input being run, from any thread. The
    /// input fails with [`Error::Interrupted`]; the session remains usable.
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.engine.interrupt_handle()
    }

    /// Whether `source` is a complete input, as far as the lexer can tell: a
    /// line-based front end keeps reading while it is not (brackets or a
    /// string or comment left open, a final binary operator or a trailing doc
    /// comment).
    pub fn is_complete(source: &str) -> bool {
        crate::syntax::lexer::is_complete(source)
    }

    // ------------------------------------------------------------------

    /// The session's state, created on first use: the registry's snapshot
    /// and a VM for its program.
    fn state(&mut self) -> Result<&mut State, Error> {
        if self.state.is_none() {
            self.state = Some(Box::new(self.init()?));
        }
        Ok(self.state.as_mut().unwrap())
    }

    /// Makes sure the session is started and its standard modules initialized.
    async fn start(&mut self) -> Result<(), Error> {
        if let Some(entry) = self.state()?.pending.take() {
            let deadline = self.deadline();
            self.run(entry, false, deadline).await?;
        }
        Ok(())
    }

    fn init(&self) -> Result<State, Error> {
        let registry = self.engine.registry();
        let snapshot = registry.snapshot()?;
        let mut graph = ModuleGraph::new(&snapshot);
        // The module the inputs are added to; its own source is empty.
        let file = graph.sm.add("<repl>", "");
        let mut checker = snapshot.checker.clone();
        checker.optimize = self.engine.config().optimize;
        let module = ParsedModule {
            name: SESSION_MODULE.into(),
            file,
            source: graph.sm.file(file).clone(),
            ast: crate::syntax::parse_module("", file).module,
            comments: Vec::new(),
            kind: ModKind::Script { main: true },
            imports: HashMap::new(),
        };
        let session = checker.mods.len() as ModId;
        checker.begin_session(&[module], session).map_err(|ds| Error::Compile(ds.iter().map(|d| graph.sm.resolve(d)).collect()))?;
        // The standard library's program; its entry initializes the prelude and `task`.
        let prog = snapshot.program.clone();
        let pending = Some(prog.entry);
        let vm = self.engine.new_vm(Arc::new(prog), self.cwd.clone(), false);
        let compiler = snapshot.compiler.clone();
        Ok(State { snapshot, graph, checker, compiler, vm, pending, loading: None, running: false, deferred_prompt: None })
    }

    /// Loads the modules `source` imports for the first time, then checks and
    /// compiles them and the input. Returns the entry that initializes them
    /// and runs the input, the type of the input's value (if `want_value` and
    /// it has one), and whether the input returns a value with a top-level
    /// `return`. On error, the session is unchanged.
    /// The wall-time limit passes at `deadline`, and for the input's run at
    /// the deadline returned (later by the time spent on prompts).
    async fn compile_input(
        &mut self,
        name: &str,
        source: &str,
        want_value: bool,
        deadline: Option<Instant>,
    ) -> Result<(u32, Option<TypeId>, bool, Option<Instant>), Error> {
        let max_source = self.engine.config().limits.max_source_bytes;
        let Session { engine, root, state, .. } = self;
        let registry = engine.registry();
        let st = state.as_mut().expect("session not started");
        if let Some(mark) = st.loading.take() {
            // An earlier `eval` was dropped while loading imports.
            st.graph.rollback(&mark);
        }
        let mark = st.graph.mark();
        let file = st.graph.sm.add(name, source);
        if source.len() > max_source {
            let e = Error::Compile(vec![st.graph.sm.resolve(&Diag::error(Span::new(file, 0, 0), "source too large"))]);
            st.graph.rollback(&mark);
            return Err(e);
        }
        let (cell, imports) = st.graph.add(SESSION_MODULE.into(), file, ModKind::Script { main: true }, Some(root.clone()));
        let mut errors = Vec::new();
        st.loading = Some(mark);
        let mut prompted = std::time::Duration::ZERO;
        let discovery = Discovery { snap: &st.snapshot, root, deadline, prompt: true };
        let stop = engine.discover(&discovery, &mut st.graph, cell, imports, format!("input:{name}"), &mut errors, &mut prompted).await;
        st.loading = None;
        if let Some(e) = stop {
            st.graph.rollback(&mark);
            return Err(e);
        }
        if !errors.is_empty() {
            let e = Error::Compile(errors.iter().map(|d| st.graph.sm.resolve(d)).collect());
            st.graph.rollback(&mark);
            return Err(e);
        }
        let new_mods: Vec<usize> = st.graph.order[mark.order..].iter().copied().filter(|&mi| mi != cell).collect();
        let deadline = deadline.map(|d| d + prompted);
        // Compilation appends to the program; a failure truncates it again.
        let prog_mark = ProgramMark::of(&st.vm.prog);
        let mut job = CellJob {
            natives: st.graph.has_natives(&new_mods),
            initialized: st.vm.initialized_globals(),
            new_std: st.graph.std[mark.std..].to_vec(),
            graph: std::mem::take(&mut st.graph),
            prog: std::mem::take(&mut st.vm.prog),
            checker: st.checker.clone(),
            compiler: st.compiler.clone(),
            snapshot: st.snapshot.clone(),
            registry: registry.clone(),
            budget: engine.compile_budget(deadline),
            optimize: engine.config().optimize,
            new_mods,
            cell,
            file,
            want_value,
        };
        // The graph and program come back with the result. If the compiler
        // panics they are lost, and the panic resets the session.
        let (job, result) = crate::platform::on_deep_stack(move || {
            let r = job.run();
            (job, r)
        })
        .unwrap_or_else(|p| std::panic::resume_unwind(p));
        st.graph = job.graph;
        st.vm.prog = job.prog;
        match result {
            Ok((entry, value, returns)) => {
                st.checker = job.checker;
                st.compiler = job.compiler;
                st.vm.load_program();
                Ok((entry, value, returns, deadline))
            }
            Err(e) => {
                st.graph.rollback(&mark);
                // A shared program was not changed.
                if let Some(prog) = Arc::get_mut(&mut st.vm.prog) {
                    prog_mark.rollback(prog);
                }
                Err(e)
            }
        }
    }

    /// Runs `proto` as the main task of the session's VM, with the
    /// wall-time limit passing at `deadline`. Returns the debug form of its
    /// result if `show`. On failure, cancels all tasks.
    async fn run(&mut self, proto: u32, show: bool, deadline: Option<Instant>) -> Result<Option<String>, Error> {
        let Session { engine, state, exit_code, .. } = self;
        let st = state.as_mut().expect("session not started");
        st.recover();
        let vm = &mut st.vm;
        engine.clear_interrupt();
        // Limits other than memory apply to each input.
        vm.meter.reset();
        vm.host.output_bytes = 0;
        vm.deadline = deadline;
        st.running = true;
        let r = vm.run_main(proto).await;
        st.running = false;
        let Some(v) = conclude(vm, r, exit_code)? else { return Ok(None) };
        let shown = if show && v != Value::Unit { Some(vm.debug(v)) } else { None };
        vm.release_main();
        match shown {
            None => Ok(None),
            Some(Ok(s)) => Ok(Some(s)),
            Some(Err(c)) => match c.into_kind() {
                CtlKind::Limit(k) => Err(Error::Limit(k)),
                CtlKind::RuntimeError(message) => Err(Error::Runtime { message, trace: vec![] }),
                other => Err(Error::Internal { message: format!("cannot show the value: {other:?}"), trace: vec![] }),
            },
        }
    }

    /// Runs the session's tasks (see [`Session::run_tasks_until`]) until
    /// none can run or one needs a permission prompt, which is deferred.
    /// Returns whether a task called `exit`.
    async fn run_idle(&mut self) -> Result<bool, Error> {
        let Session { engine, state, exit_code, .. } = self;
        let st = state.as_mut().expect("session not started");
        let vm = &mut st.vm;
        engine.clear_interrupt();
        vm.meter.reset();
        vm.host.output_bytes = 0;
        // Waiting for input may take any time.
        vm.deadline = None;
        let r = vm.drive(None).await;
        let r = r.and_then(|stop| vm.drain_output().map(|()| stop).map_err(Trap::from_ctl));
        let r = match r {
            Ok(Stop::Prompt(cap)) => {
                st.deferred_prompt = Some(cap);
                return Ok(false);
            }
            Ok(Stop::Idle | Stop::Main(_)) => return Ok(false),
            Ok(Stop::Exit(c)) => Ok(RunEnd::Exit(c)),
            Err(t) => Err(t),
        };
        Ok(conclude(vm, r, exit_code)?.is_none())
    }
}

/// The outcome of a run of the session's VM: the main task's result, or
/// `None` if a task called `exit` (recorded in `exit_code`). On failure and
/// exit, cancels all tasks.
fn conclude(vm: &mut Vm, r: Result<RunEnd, Trap>, exit_code: &mut Option<i32>) -> Result<Option<Value>, Error> {
    let e = match r {
        Ok(RunEnd::Finished(v)) => return Ok(Some(v)),
        Ok(RunEnd::Exit(c)) | Err(Trap::Exit(c)) => {
            *exit_code = Some(c);
            vm.abandon_tasks();
            return Ok(None);
        }
        Err(Trap::Error { message, trace }) => Error::Runtime { message, trace },
        Err(Trap::Internal { message, trace }) => Error::Internal { message, trace },
        Err(Trap::Limit(k)) => Error::Limit(k),
        Err(Trap::Interrupted) => Error::Interrupted,
    };
    vm.abandon_tasks();
    if matches!(e, Error::Limit(_)) {
        vm.collect_garbage(None);
    }
    Err(e)
}

/// What compiling an input needs, owned so that it can move to the
/// compiler's thread.
struct CellJob {
    graph: ModuleGraph,
    prog: Arc<Program>,
    /// Copies of the session's, which the input extends.
    checker: Checker,
    compiler: Incremental,
    snapshot: Arc<Snapshot>,
    registry: Arc<Registry>,
    budget: Arc<CompileBudget>,
    /// The modules the input loaded (by index in `graph`), and the standard
    /// modules it uses for the first time.
    new_mods: Vec<usize>,
    new_std: Vec<ModId>,
    /// Whether a module of `new_mods` declares natives.
    natives: bool,
    /// Which globals of the program are initialized.
    initialized: Vec<bool>,
    /// The input's module (in `graph`) and file.
    cell: usize,
    file: FileId,
    want_value: bool,
    optimize: bool,
}

impl CellJob {
    /// Checks the new modules and the input, and compiles them by appending
    /// to `prog`. Returns the entry that initializes them and runs the input,
    /// the type of the input's value (if `want_value` and it has one), and
    /// whether the input has a top-level `return` of a value.
    fn run(&mut self) -> Result<(u32, Option<TypeId>, bool), Error> {
        let session = self.checker.session.unwrap_or_default();
        let first = self.checker.mods.len();
        let parsed = self.graph.parse(&self.new_mods, first as ModId).and_then(|modules| {
            let mut cell = self.graph.parse(&[self.cell], session)?;
            Ok((modules, cell.pop().expect("one module")))
        });
        let (checker, sm, budget) = (&mut self.checker, &self.graph.sm, &self.budget);
        let resolve = |ds: Vec<Diag>| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect());
        let (modules, cell_module) = parsed.map_err(resolve)?;
        let added = modules.len();
        let checked = checker.check_cell(&modules, &cell_module, self.want_value, &self.registry.by_name, budget);
        if let Some(stop) = budget.stopped() {
            return Err(budget_error(stop, sm, self.file));
        }
        checked.map_err(resolve)?;
        let value = checker.cell_value.take();
        let returns = checker.cell_returns;
        let std_inits = self.new_std.iter().map(|&id| self.snapshot.mods[id as usize].init);
        let inits = (first..first + added).chain([session as usize]).map(|m| checker.mods[m].init);
        let inits: Vec<FnId> = std_inits.chain(inits).flatten().collect();
        let cp = checker.take_checked();
        let registry = &*self.registry;
        let native_info = if self.natives { native_info(&cp, registry) } else { NativeInfo::default() };
        let initialized = Some(std::mem::take(&mut self.initialized));
        let input =
            CompileInput { checked: &cp, sm, natives: &registry.by_name, native_info, budget, initialized, optimize: self.optimize };
        let entry = compile_more(input, Arc::make_mut(&mut self.prog), &mut self.compiler, &inits);
        if let Some(stop) = budget.stopped() {
            return Err(budget_error(stop, sm, self.file));
        }
        let entry = entry.map_err(resolve)?;
        checker.restore_checked(cp);
        checker.release_bodies();
        Ok((entry, value, returns))
    }
}

impl State {
    /// Recovers from an `eval` whose future was dropped while its input
    /// ran: all tasks are cancelled. (A dropped `run_tasks_until` leaves the
    /// tasks where they wait.)
    fn recover(&mut self) {
        if std::mem::take(&mut self.running) {
            self.vm.abandon_tasks();
            self.deferred_prompt = None;
        }
    }
}
