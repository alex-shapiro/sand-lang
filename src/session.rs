//! REPL sessions: inputs checked incrementally and run, exactly once, in a
//! persistent VM.
//!
//! A session is one script module whose items arrive one input at a time.
//! Each input is parsed, checked in the context of the previous inputs
//! ([`Checker::check_cell`]), compiled by appending to the session's program
//! ([`compile_more`]), and run in the session's VM, whose heap, globals and
//! tasks persist. See `docs/INTERNALS.md` ("REPL sessions").

use crate::check::budget::CompileBudget;
use crate::check::{Checker, ModKind, ParsedModule};
use crate::compile::{CompileInput, Incremental, ProgramMark, compile_more};
use crate::diag::{Diag, Span};
use crate::engine::{
    Engine, Error, GraphMark, InterruptHandle, LoadedModule, ModuleGraph, TestReport, TestResult, budget_error, native_info, parse_in_order,
};
use crate::policy::{Capability, PromptFuture, Prompter};
use crate::syntax::ast::{ItemKind, StmtKind};
use crate::syntax::lexer::{Tok, lex};
use crate::types::{FnId, ModId, Type};
use crate::unwind::CatchUnwind;
use crate::vm::bytecode::{Op, Program, Proto};
use crate::vm::sched::RunEnd;
use crate::vm::value::Value;
use crate::vm::{Ctl, Finisher, NResult, NativeEntry, NativeImpl, Trap, Vm};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// * An input that fails to compile has no effect. An input that panics (or
///   exceeds a limit, is interrupted or calls `exit`) keeps its declarations
///   and the effects that happened before the failure, including global
///   assignments; its remaining globals stay uninitialised. All tasks are
///   then cancelled.
/// * Tasks spawned by an input keep running, alongside later inputs, and
///   between inputs while the front end calls [`Session::run_tasks_until`].
/// * The instruction, CPU time, wall time and output limits apply to each
///   input separately; the memory limit applies to the session's heap.
/// * After a Rust panic (a bug in the engine or in a host native) is caught
///   while the session's code runs, the VM may be inconsistent, so the
///   session is reset (as by [`Session::reset`]): the call fails with an
///   `internal error` whose message ends with [`RESET_NOTE`], and
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
}

/// The end of the message of the error reported when a session was reset
/// after an internal error (a caught Rust panic).
pub const RESET_NOTE: &str = " (the session was reset)";

/// A started session.
struct State {
    graph: ModuleGraph,
    /// Module index in `graph` -> module id of the checker.
    mod_ids: HashMap<usize, ModId>,
    checker: Checker<'static>,
    compiler: Incremental,
    vm: Vm,
    /// The entry that initialises the standard modules, until it has run.
    pending: Option<u32>,
    /// Set while an input's imports are being loaded or it is running, so
    /// that the work of an `eval` future dropped there can be undone.
    loading: Option<GraphMark>,
    running: bool,
    /// The entry that waits for the gate in `idle_slot`: the main task of
    /// [`Session::run_tasks_until`].
    idle_proto: u32,
    idle_slot: Arc<Mutex<Option<Arc<Gate>>>>,
    /// The gate of a [`Session::run_tasks_until`] in progress (or dropped).
    idle: Option<Arc<Gate>>,
}

/// Opened once: lets the main task of an idle run finish, and deferred
/// prompts be asked.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// The prompter while the front end waits for input: prompts wait until it
/// has its input, so that they do not compete with it for the terminal.
struct DeferredPrompter {
    inner: Arc<dyn Prompter>,
    gate: Arc<Gate>,
}

impl Prompter for DeferredPrompter {
    fn prompt(&self, cap: Capability) -> PromptFuture {
        let (inner, gate) = (self.inner.clone(), self.gate.clone());
        Box::pin(async move {
            gate.wait().await;
            inner.prompt(cap).await
        })
    }
}

/// Name of the session module (for qualified names of its functions).
const SESSION_MODULE: &str = "repl";

impl Session {
    pub(crate) fn new(engine: Engine) -> Session {
        let config = engine.config();
        let root = config.module_root.clone().or_else(|| config.cwd.clone()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let cwd = config.cwd.clone().unwrap_or_else(|| root.clone());
        Session { engine, root: crate::policy::normalize_path(&root), cwd, state: None, inputs: 0, exit_code: None, was_reset: false }
    }

    /// Checks, compiles and runs one input. Returns the debug form of its
    /// value: that of its final expression statement, unless the input ends
    /// with `;` or the value has type `()`.
    ///
    /// A failed top-level `?` aborts the input like a panic whose message is
    /// the error. If the input calls `exit(code)`, the result is `Ok(None)`
    /// and [`Session::exit_code`] returns the code.
    pub async fn eval(&mut self, source: &str) -> Result<Option<String>, Error> {
        self.was_reset = false;
        let r = CatchUnwind::new(self.eval_inner(source)).await;
        self.settle(r)
    }

    async fn eval_inner(&mut self, source: &str) -> Result<Option<String>, Error> {
        self.exit_code = None;
        self.engine.clear_interrupt();
        self.start().await?;
        self.inputs += 1;
        let name = format!("<input {}>", self.inputs);
        let want_value = !source.trim_end().ends_with(';');
        let (entry, value) = self.compile_input(&name, source, want_value).await?;
        let show = value.is_some_and(|t| !matches!(t, Type::Unit | Type::Never));
        self.run(entry, show).await
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
        let registry = self.engine.registry();
        let budget = self.engine.compile_budget();
        let st = self.state()?;
        let mark = st.graph.mark();
        let file = st.graph.sm.add("<type>", expr);
        let res = crate::syntax::parse_module(expr, file);
        let resolve = |sm: &crate::diag::SourceMap, ds: &[Diag]| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect());
        let mut errors = res.errors;
        let is_expr =
            matches!(res.module.items.as_slice(), [item] if matches!(&item.kind, ItemKind::Stmt(s) if matches!(s.kind, StmtKind::Expr(_))));
        if errors.is_empty() && !is_expr {
            errors.push(Diag::error(Span::new(file, 0, expr.len() as u32), "expected an expression"));
        }
        if !errors.is_empty() {
            let e = resolve(&st.graph.sm, &errors);
            st.graph.rollback(&mark);
            return Err(e);
        }
        let cell = ParsedModule {
            name: SESSION_MODULE.into(),
            file,
            ast: res.module,
            kind: ModKind::Script { main: true },
            imports: HashMap::new(),
        };
        let (graph, detached) = (&st.graph, &st.checker);
        let r = on_compile_stack(|| {
            let lookup = |name: &str| registry.by_name.get(name).copied();
            let mut checker = Checker::attach(detached.clone(), &graph.sm, &lookup, budget.clone());
            checker.type_query = Some(Default::default());
            let checked = checker.check_cell(Vec::new(), &cell, true);
            if let Some(stop) = budget.stopped() {
                return Err(budget_error(stop, &graph.sm, file));
            }
            checked.map_err(|ds| resolve(&graph.sm, &ds))?;
            Ok(checker.cell_value.as_ref().map(|t| checker.show_value_type(t)).unwrap_or_else(|| "()".into()))
        });
        st.graph.rollback(&mark);
        r
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
            let message = match self.run(proto, false).await {
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

    /// The code passed to `exit` by the last input, if it called `exit`.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Forgets all inputs: the next input starts a fresh session. "Always"
    /// answers to permission prompts are kept (they belong to the engine).
    pub fn reset(&mut self) {
        if let Some(st) = &self.state {
            // Answers given during a run that did not finish normally.
            self.engine.remember_always(&st.vm.host.always);
        }
        self.state = None;
        self.inputs = 0;
        self.exit_code = None;
    }

    /// Whether the last call of [`Session::eval`], [`Session::run_tests`],
    /// [`Session::type_of`] or [`Session::run_tasks_until`] reset the session
    /// because a Rust panic (an internal error) was caught while it ran.
    pub fn was_reset(&self) -> bool {
        self.was_reset
    }

    /// Runs the tasks that earlier inputs left running until `until`
    /// completes, and returns its output. A front end calls it while it waits
    /// for the next input (with a future that resolves to that input), so
    /// that background tasks keep running between inputs; when no task can
    /// make progress, it just awaits `until`.
    ///
    /// The instruction, CPU time and output limits apply to each call as to
    /// an input; the wall-time limit does not apply (waiting for input may
    /// take any time). Permission prompts wait until `until` has completed,
    /// so that they do not compete with the front end for its input; no
    /// script code runs while one waits. Likewise, reads of standard input
    /// wait (unless one was already in progress).
    ///
    /// If a task panics, exceeds a limit or is interrupted, every task is
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
        let gate = Arc::new(Gate::default());
        let mut output = None;
        let r = {
            let mut run = std::pin::pin!(CatchUnwind::new(self.run_idle(gate.clone())));
            std::future::poll_fn(|cx| {
                if output.is_none()
                    && let Poll::Ready(v) = until.as_mut().poll(cx)
                {
                    output = Some(v);
                    gate.open();
                }
                run.as_mut().poll(cx)
            })
            .await
        };
        match self.settle(r) {
            Ok(false) => {}
            Ok(true) => return None,
            Err(e) => on_error(e),
        }
        Some(match output {
            Some(v) => v,
            None => until.await,
        })
    }

    /// Cancels every task that earlier inputs left running, as a failed input
    /// does. Returns how many there were.
    pub fn cancel_tasks(&mut self) -> usize {
        let Some(st) = self.state.as_mut() else { return 0 };
        st.recover();
        let n = st.vm.live_tasks;
        st.vm.abandon_tasks();
        n
    }

    /// Whether a Rust panic was caught while the session's VM ran.
    fn poisoned(&self) -> bool {
        self.state.as_ref().is_some_and(|st| st.vm.caught_panic)
    }

    /// The result of a call, given `r` (`Err` if it panicked). After a caught
    /// panic, resets the session and says so in the error.
    fn settle<T>(&mut self, r: Result<Result<T, Error>, String>) -> Result<T, Error> {
        let r = match r {
            Ok(r) if !self.poisoned() => return r,
            Ok(Err(Error::Runtime { message, trace })) => Err(Error::Runtime { message: format!("{message}{RESET_NOTE}"), trace }),
            Ok(r) => r,
            Err(msg) => Err(Error::Runtime { message: format!("internal error: {msg}{RESET_NOTE}"), trace: vec![] }),
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
        let lexed = lex(source, 0);
        if lexed.errors.iter().any(|d| d.message.starts_with("unterminated")) {
            return false;
        }
        let mut depth = 0i32;
        let mut last = None;
        for t in &lexed.tokens {
            match t.tok {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
                Tok::Semi | Tok::Newline | Tok::Eof => continue,
                _ => {}
            }
            last = Some(&t.tok);
        }
        let continues = matches!(
            last,
            Some(
                Tok::Plus
                    | Tok::Minus
                    | Tok::Star
                    | Tok::Slash
                    | Tok::Percent
                    | Tok::StarStar
                    | Tok::Amp
                    | Tok::Pipe
                    | Tok::Caret
                    | Tok::Shl
                    | Tok::Shr
                    | Tok::EqEq
                    | Tok::NotEq
                    | Tok::Lt
                    | Tok::Le
                    | Tok::Gt
                    | Tok::Ge
                    | Tok::Eq
                    | Tok::PlusEq
                    | Tok::MinusEq
                    | Tok::StarEq
                    | Tok::SlashEq
                    | Tok::PercentEq
                    | Tok::FatArrow
                    | Tok::Arrow
                    | Tok::PipeGt
                    | Tok::QQ
                    | Tok::Dot
                    | Tok::Comma
                    | Tok::Colon
                    | Tok::And
                    | Tok::Or
                    | Tok::Not
                    | Tok::Else
                    | Tok::In
                    | Tok::As
            )
        );
        // A doc comment belongs to the item that follows.
        let doc = source.lines().rev().find(|l| !l.trim().is_empty()).is_some_and(|l| l.trim_start().starts_with("///"));
        depth <= 0 && !continues && !doc
    }

    // ------------------------------------------------------------------

    /// The session's state, created on first use: the prelude and `task`
    /// checked and compiled, and a VM for the program.
    fn state(&mut self) -> Result<&mut State, Error> {
        if self.state.is_none() {
            self.state = Some(Box::new(self.init()?));
        }
        Ok(self.state.as_mut().unwrap())
    }

    /// Makes sure the session is started and its standard modules initialised.
    async fn start(&mut self) -> Result<(), Error> {
        if let Some(entry) = self.state()?.pending.take() {
            self.run(entry, false).await?;
        }
        Ok(())
    }

    fn init(&self) -> Result<State, Error> {
        let registry = self.engine.registry();
        let mut g = ModuleGraph::default();
        let mut errors = Vec::new();
        g.load_prelude(&registry.sources);
        g.load_std_tree(&registry.sources, "task", &mut errors);
        // The module the inputs are added to; its own source is empty.
        let file = g.sm.add("<repl>", "");
        g.modules.push(LoadedModule {
            name: SESSION_MODULE.into(),
            file,
            kind: ModKind::Script { main: true },
            imports: vec![],
            dir: Some(self.root.clone()),
        });
        g.order.push(g.modules.len() - 1);
        if !errors.is_empty() {
            return Err(Error::Compile(errors.iter().map(|d| g.sm.resolve(d)).collect()));
        }
        let mod_ids: HashMap<usize, ModId> = g.order.iter().enumerate().map(|(pos, &mi)| (mi, pos as ModId)).collect();
        let parsed = parse_in_order(&g.sm, &g.modules, &g.index, &g.order, |mi| mod_ids.get(&mi).copied())?;
        let session = (g.order.len() - 1) as ModId;
        let graph = &g;
        let (checker, compiler, prog) = on_compile_stack(|| {
            let sm = &graph.sm;
            let lookup = |name: &str| registry.by_name.get(name).copied();
            let mut checker = Checker::new(sm, &lookup);
            checker.begin_session(parsed, session).map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
            let cp = checker.take_checked(true);
            let (native_help, native_argc) = native_info(&cp, &registry);
            let mut prog = Program::default();
            let mut compiler = Incremental::default();
            let budget = CompileBudget::default();
            let input = CompileInput { checked: &cp, sm, native_lookup: &lookup, native_help, native_argc, budget: &budget };
            prog.entry = compile_more(input, &mut prog, &mut compiler, &cp.module_inits)
                .map_err(|ds| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect()))?;
            prog.available_modules = registry.sources.keys().filter(|k| k.as_str() != "prelude").cloned().collect();
            prog.help_index = Some(registry.help.clone());
            checker.restore_checked(cp);
            checker.release_bodies();
            Ok((checker.detach(), compiler, prog))
        })?;
        let pending = Some(prog.entry);
        let mut prog = prog;
        // The main task of idle runs: waits for the gate in `idle_slot`.
        let idle_slot: Arc<Mutex<Option<Arc<Gate>>>> = Arc::default();
        let idle_native = registry.entries.len() as u32;
        let idle_proto = prog.protos.len() as u32;
        prog.protos.push(Proto {
            name: "<entry>".into(),
            file: 0,
            nparams: 0,
            nregs: 1,
            code: vec![Op::CallNative { base: 0, native: idle_native }, Op::Ret { src: 0 }],
            lines: vec![0, 0],
            captures: vec![],
            capture_cells: vec![],
            doc: None,
            signature: String::new(),
        });
        let mut vm = self.engine.new_vm(Arc::new(prog), self.cwd.clone(), false);
        vm.session = true;
        let slot = idle_slot.clone();
        let wait = move |_: &mut Vm, _: &[Value]| -> NResult {
            let gate = slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Err(Ctl::Async(Box::pin(async move {
                if let Some(gate) = gate {
                    gate.wait().await;
                }
                Box::new(|_: &mut Vm| -> NResult { Ok(Value::Unit) }) as Finisher
            })))
        };
        // The session's VM has one native more than the engine's (its name
        // cannot be declared by a `native fn`).
        let natives = Arc::make_mut(&mut vm.natives);
        debug_assert_eq!(natives.len() as u32, idle_native);
        natives.push(NativeEntry { name: "<session idle>".into(), imp: NativeImpl::Host(Arc::new(wait)) });
        Ok(State { graph: g, mod_ids, checker, compiler, vm, pending, loading: None, running: false, idle_proto, idle_slot, idle: None })
    }

    /// Loads the modules `source` imports for the first time, then checks and
    /// compiles them and the input. Returns the entry that initialises them
    /// and runs the input, and the type of the input's value (if
    /// `want_value` and it has one). On error, the session is unchanged.
    async fn compile_input(&mut self, name: &str, source: &str, want_value: bool) -> Result<(u32, Option<Type>), Error> {
        let registry = self.engine.registry();
        let max_source = self.engine.config().limits.max_source_bytes;
        let Session { engine, root, state, .. } = self;
        let st = state.as_mut().expect("session not started");
        if let Some(mark) = st.loading.take() {
            // An earlier `eval` was dropped while loading imports.
            st.rollback(&mark);
        }
        let mark = st.graph.mark();
        let file = st.graph.sm.add(name, source);
        if source.len() > max_source {
            let e = Error::Compile(vec![st.graph.sm.resolve(&Diag::error(Span::new(file, 0, 0), "source too large"))]);
            st.graph.rollback(&mark);
            return Err(e);
        }
        let cell = st.graph.modules.len();
        st.graph.modules.push(LoadedModule {
            name: SESSION_MODULE.into(),
            file,
            kind: ModKind::Script { main: true },
            imports: vec![],
            dir: Some(root.clone()),
        });
        let mut errors = Vec::new();
        st.loading = Some(mark);
        let stop = engine.discover(&mut st.graph, cell, format!("input:{name}"), root, &mut errors).await;
        st.loading = None;
        if let Some(e) = stop {
            st.rollback(&mark);
            return Err(e);
        }
        let new_mods: Vec<usize> = st.graph.order[mark.order..].iter().copied().filter(|&mi| mi != cell).collect();
        let first_id = st.checker.mods.len();
        for (k, &mi) in new_mods.iter().enumerate() {
            st.mod_ids.insert(mi, (first_id + k) as ModId);
        }
        let parsed = if errors.is_empty() {
            let mod_id = |mi: usize| st.mod_ids.get(&mi).copied();
            parse_in_order(&st.graph.sm, &st.graph.modules, &st.graph.index, &new_mods, mod_id).and_then(|ms| {
                let mut cell = parse_in_order(&st.graph.sm, &st.graph.modules, &st.graph.index, &[cell], mod_id)?;
                Ok((ms, cell.pop().unwrap()))
            })
        } else {
            Err(Error::Compile(errors.iter().map(|d| st.graph.sm.resolve(d)).collect()))
        };
        // Compilation appends to the program; a failure (even an internal
        // error) truncates it again.
        let prog_mark = ProgramMark::of(&st.vm.prog);
        let result = match parsed {
            Ok((modules, cell)) => {
                let (graph, detached, compiler, prog) = (&st.graph, &st.checker, &st.compiler, &mut st.vm.prog);
                let budget = engine.compile_budget();
                on_compile_stack(|| {
                    let sm = &graph.sm;
                    let resolve = |ds: Vec<Diag>| Error::Compile(ds.iter().map(|d| sm.resolve(d)).collect());
                    let lookup = |name: &str| registry.by_name.get(name).copied();
                    let mut checker = Checker::attach(detached.clone(), sm, &lookup, budget.clone());
                    let added = modules.len();
                    let checked = checker.check_cell(modules, &cell, want_value);
                    if let Some(stop) = budget.stopped() {
                        return Err(budget_error(stop, sm, file));
                    }
                    checked.map_err(resolve)?;
                    let value = checker.cell_value.take();
                    // Initialise the new modules, then run the input.
                    let session = checker.session.unwrap_or_default();
                    let inits: Vec<FnId> =
                        (first_id..first_id + added).chain([session as usize]).filter_map(|m| checker.mods[m].init).collect();
                    let cp = checker.take_checked(added > 0);
                    let (native_help, native_argc) = if added > 0 { native_info(&cp, &registry) } else { (Vec::new(), Vec::new()) };
                    let mut compiler = compiler.clone();
                    let input = CompileInput { checked: &cp, sm, native_lookup: &lookup, native_help, native_argc, budget: &budget };
                    let entry = compile_more(input, Arc::make_mut(prog), &mut compiler, &inits);
                    if let Some(stop) = budget.stopped() {
                        return Err(budget_error(stop, sm, file));
                    }
                    let entry = entry.map_err(resolve)?;
                    checker.restore_checked(cp);
                    checker.release_bodies();
                    Ok((checker.detach(), compiler, entry, value))
                })
            }
            Err(e) => Err(e),
        };
        match result {
            Ok((checker, compiler, entry, value)) => {
                st.checker = checker;
                st.compiler = compiler;
                st.vm.load_program();
                Ok((entry, value))
            }
            Err(e) => {
                st.rollback(&mark);
                // (Not shared if it was changed.)
                if let Some(prog) = Arc::get_mut(&mut st.vm.prog) {
                    prog_mark.rollback(prog);
                }
                Err(e)
            }
        }
    }

    /// Runs `proto` as the main task of the session's VM. Returns the debug
    /// form of its result if `show`. On failure, cancels all tasks.
    async fn run(&mut self, proto: u32, show: bool) -> Result<Option<String>, Error> {
        let Session { engine, state, exit_code, .. } = self;
        let st = state.as_mut().expect("session not started");
        st.recover();
        let vm = &mut st.vm;
        engine.prepare_run(vm);
        // Limits other than memory apply to each input.
        vm.fuel_used = 0;
        vm.host.output_bytes = 0;
        // (Changed by idle runs.)
        vm.host.limits.max_wall_time = engine.config().limits.max_wall_time;
        vm.host.prompter = engine.config().prompter.clone();
        st.running = true;
        let r = vm.run_main(proto).await;
        st.running = false;
        engine.finish_run(vm);
        let Some(v) = conclude(vm, r, exit_code)? else { return Ok(None) };
        let shown = if show { Some(vm.debug(v)) } else { None };
        vm.release_main();
        match shown {
            None => Ok(None),
            Some(Ok(s)) => Ok(Some(s)),
            Some(Err(Ctl::Limit(k))) => Err(Error::Limit(k)),
            Some(Err(Ctl::Panic(message))) => Err(Error::Runtime { message, trace: vec![] }),
            Some(Err(other)) => Err(Error::Runtime { message: format!("cannot show the value: {other:?}"), trace: vec![] }),
        }
    }

    /// Runs the session's tasks until `gate` opens (see
    /// [`Session::run_tasks_until`]). Returns whether a task called `exit`.
    async fn run_idle(&mut self, gate: Arc<Gate>) -> Result<bool, Error> {
        let Session { engine, state, exit_code, .. } = self;
        let st = state.as_mut().expect("session not started");
        let vm = &mut st.vm;
        engine.prepare_run(vm);
        vm.fuel_used = 0;
        vm.host.output_bytes = 0;
        // (Restored by the next `run`.)
        vm.host.limits.max_wall_time = None;
        vm.host.prompter =
            engine.config().prompter.clone().map(|inner| Arc::new(DeferredPrompter { inner, gate: gate.clone() }) as Arc<dyn Prompter>);
        // Reads of standard input wait until the front end has its input.
        let _stdin = vm.host.stdin.clone().try_lock_owned().ok();
        *st.idle_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(gate.clone());
        st.idle = Some(gate);
        let r = vm.run_main(st.idle_proto).await;
        st.idle = None;
        *st.idle_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        engine.finish_run(vm);
        if conclude(vm, r, exit_code)?.is_none() {
            return Ok(true);
        }
        vm.release_main();
        Ok(false)
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
        Err(Trap::Panic { message, trace }) => Error::Runtime { message, trace },
        Err(Trap::Limit(k)) => Error::Limit(k),
        Err(Trap::Interrupted) => Error::Interrupted,
    };
    vm.abandon_tasks();
    if matches!(e, Error::Limit(_)) {
        vm.collect_garbage(None);
    }
    Err(e)
}

impl State {
    /// Recovers from a call whose future was dropped: an `eval` while its
    /// input ran (all tasks are cancelled), or a `run_tasks_until` (its main
    /// task is let finish, as an ordinary task).
    fn recover(&mut self) {
        if std::mem::take(&mut self.running) {
            self.vm.abandon_tasks();
        }
        if let Some(gate) = self.idle.take() {
            gate.open();
            *self.idle_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// Forgets the modules loaded since `mark`.
    fn rollback(&mut self, mark: &GraphMark) {
        self.graph.rollback(mark);
        self.mod_ids.retain(|&mi, _| mi < mark.modules);
    }
}

/// Runs `f` on a thread with a stack big enough for checking and compiling.
fn on_compile_stack<R: Send>(f: impl FnOnce() -> Result<R, Error> + Send) -> Result<R, Error> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(crate::engine::COMPILE_STACK)
            .spawn_scoped(s, f)
            .map_err(|e| Error::Io(e.to_string()))?
            .join()
            .map_err(|p| Error::Io(format!("internal compiler error: {}", crate::unwind::panic_text(&*p))))?
    })
}
