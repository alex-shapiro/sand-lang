//! Sand for the web: the engine compiled to WebAssembly, driven from
//! JavaScript (with `wasm-bindgen`).
//!
//! The engine is built without operating-system access: scripts see an
//! in-memory file system (`MemFs`) the host fills and reads back, an
//! environment and arguments the host gives, and standard input the host
//! provides as text. Output and permission prompts go to JavaScript
//! callbacks. A policy file (see `sandlang::policy::PolicyFile`) decides
//! what scripts may do and sets their limits.
//!
//! An engine runs one script (or session input) at a time and runs on the
//! thread that calls it, so long computations block that thread: run
//! engines in Web Workers, one or more per worker, to keep a page
//! responsive and to run several at once. A run returns to the JavaScript
//! event loop only while it waits (for a timer or a prompt); a worker that
//! must stop a busy engine at once can be terminated.
//!
//! ```js
//! import init, { Engine } from "./pkg/sandlang_wasm.js";
//! await init();
//! const engine = new Engine(
//!   { policy: "allow read /data\nlimit time 5s", files: { "/data/x.txt": "hi" } },
//!   { onOutput: (stream, text) => console.log(stream, text) },
//! );
//! const result = await engine.run('import fs\nprint(fs.read_text("/data/x.txt"))');
//! ```
#![forbid(unsafe_code)]

mod hooks;

use hooks::{Hooks, JsOutput, JsPrompter};
use js_sys::{Array, Object, Reflect};
use sandlang::lint::LintWarning;
use sandlang::policy::{PolicyError, PolicyFile};
use sandlang::{Config, Decision, Diagnostic, Error, MemFs, Severity};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(typescript_custom_section)]
const TYPES: &str = r#"
/** Options of `new Engine(options, hooks)`; every field is optional. */
export interface EngineOptions {
  /** A policy file: rules (`allow read /data`) and limits (`limit time 5s`). Default: deny everything but output. */
  policy?: string;
  /** Files of the in-memory file system, by absolute path. */
  files?: Record<string, string | Uint8Array>;
  /** The environment scripts may read (as the policy allows). */
  env?: Record<string, string>;
  /** Arguments (`env.args()`). */
  args?: string[];
  /** Standard input (as the policy allows). */
  stdin?: string;
  /** Seeds the random number generator, for reproducible runs. */
  seed?: number;
  /** Bytes the file system may hold (default 64 MiB). */
  fileSystemCapacity?: number;
  /** The working directory and module root (default `/`). */
  cwd?: string;
}

/** Callbacks of `new Engine(options, hooks)`. */
export interface EngineHooks {
  /** Receives output as it is produced. */
  onOutput?: (stream: "stdout" | "stderr", text: string) => void;
  /** Answers a permission request: one the policy does not decide. Without it, such requests are denied. */
  onPrompt?: (request: PromptRequest) => PromptAnswer | Promise<PromptAnswer>;
}

export interface PromptRequest {
  kind: "read" | "write" | "import" | "net" | "http" | "listen" | "env" | "stdin" | "stdout" | "stderr" | "custom";
  target: string;
  /** e.g. "read /data/a.txt" */
  description: string;
  /** The policy rule that allows it from now on, e.g. "allow read /data/a.txt" (what "always" means as a rule). */
  allowRule?: string;
  /** The policy rule that denies it from now on (what "never" means as a rule). */
  denyRule?: string;
}

/** "once" allows this request; "always" allows it for the engine's lifetime; "deny" and "never" likewise deny. */
export type PromptAnswer = "once" | "always" | "deny" | "never";

export interface Diagnostic {
  severity: "error" | "warning";
  code: string;
  message: string;
  file: string;
  /** 1-based; 0 when the diagnostic has no position. */
  line: number;
  column: number;
  width: number;
  notes: string[];
  /** The diagnostic as the command line prints it, with a source excerpt. */
  rendered: string;
}

export interface SandError {
  kind: "compile" | "runtime" | "internal" | "limit" | "interrupted" | "io";
  message: string;
  /** How `sand run` exits for it: 1 runtime error, 2 compile error, 3 limit, 70 internal error, 130 interrupted. */
  exitCode: number;
  diagnostics: Diagnostic[];
  /** For runtime and internal errors: the stack, innermost first. */
  trace: { function: string; file: string; line: number }[];
}

export interface RunResult {
  ok: boolean;
  /** The script's exit code, or as `sand run` exits: 1 runtime error, 2 compile error, 3 limit, 70 internal error, 130 interrupted. */
  exitCode: number;
  error?: SandError;
  elapsedMs: number;
}

export interface CheckResult {
  ok: boolean;
  /** Errors, or warnings when `ok`. */
  diagnostics: Diagnostic[];
  /** Lint warnings when `ok` (from the same compilation). */
  lint: LintWarning[];
  error?: SandError;
}

export interface LintWarning { code: string; message: string; line: number; column: number; }

export interface EvalResult {
  /** The debug form of the input's value, if it has one. */
  value?: string;
  error?: SandError;
  /** Set when the input called `exit`. */
  exitCode?: number;
}

export interface FileEntry { path: string; data: Uint8Array; }
"#;

/// The fields of `EngineOptions` (unknown ones, typically misspelt, are denied).
const OPTION_KEYS: &[&str] = &["policy", "files", "env", "args", "stdin", "seed", "fileSystemCapacity", "cwd"];

/// The JavaScript callbacks of an engine and its sessions, unregistered when
/// the last of them is freed.
struct HookKey(u32);

impl Drop for HookKey {
    fn drop(&mut self) {
        hooks::unregister(self.0);
    }
}

/// A Sand engine: runs scripts against its in-memory file system under its
/// policy and limits.
#[wasm_bindgen]
pub struct Engine {
    /// Taken while a run uses it.
    engine: Rc<RefCell<Option<sandlang::Engine>>>,
    fs: MemFs,
    interrupt: sandlang::InterruptHandle,
    hooks: Rc<HookKey>,
}

#[wasm_bindgen]
impl Engine {
    /// Creates an engine. Throws an `Error` for invalid options; for an
    /// invalid policy, its `policyErrors` property lists `{line, column, message}`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "EngineOptions")] options: JsValue,
        #[wasm_bindgen(unchecked_param_type = "EngineHooks")] hooks: JsValue,
    ) -> Result<Engine, JsValue> {
        check_keys(&options, OPTION_KEYS, "engine option")?;
        check_keys(&hooks, &["onOutput", "onPrompt"], "engine hook")?;
        let cwd = string_option(&options, "cwd")?.unwrap_or_else(|| "/".into());
        let policy = string_option(&options, "policy")?.unwrap_or_default();
        let policy = PolicyFile::parse(&policy, Path::new(&cwd), None).map_err(policy_error)?;
        let fs = match number_option(&options, "fileSystemCapacity")? {
            Some(c) if c >= 0.0 => MemFs::with_capacity(c as usize),
            Some(_) => return Err(error("invalid fileSystemCapacity")),
            None => MemFs::new(),
        };
        write_files(&fs, &options)?;
        let key =
            Rc::new(HookKey(hooks::register(Hooks { on_output: function(&hooks, "onOutput")?, on_prompt: function(&hooks, "onPrompt")? })));
        let mut config = Config::new()
            // What the policy does not decide is asked about (`onPrompt`).
            .policy(policy.policy_or(Decision::Prompt))
            .limits(policy.limits.clone())
            .memory_fs(fs.clone())
            .cwd(&cwd)
            .env(string_entries(&options, "env")?)
            .args(string_list(&options, "args")?)
            .stdout(JsOutput::new(key.0, "stdout"))
            .stderr(JsOutput::new(key.0, "stderr"));
        config = config.prompter(JsPrompter { id: key.0 });
        if let Some(stdin) = string_option(&options, "stdin")? {
            config = config.stdin_bytes(stdin.into_bytes());
        }
        if let Some(seed) = number_option(&options, "seed")? {
            config = config.seed(seed as u64);
        }
        let engine = sandlang::Engine::new(config);
        let interrupt = engine.interrupt_handle();
        Ok(Engine { engine: Rc::new(RefCell::new(Some(engine))), fs, interrupt, hooks: key })
    }

    /// Runs a script (`name` names it in messages; default `main.sand`).
    /// Resolves to a `RunResult` (it does not reject for errors of the
    /// script); rejects if the engine is already running.
    #[wasm_bindgen(unchecked_return_type = "Promise<RunResult>")]
    pub fn run(&self, source: String, name: Option<String>) -> js_sys::Promise {
        let slot = self.engine.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let name = name.unwrap_or_else(|| "main.sand".into());
            let mut engine = take(&slot)?;
            let started = sandlang::platform::Instant::now();
            let outcome = engine.run_source(&name, &source).await;
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            *slot.borrow_mut() = Some(engine);
            let (exit_code, error) = match outcome {
                Ok(o) => (o.exit_code, JsValue::UNDEFINED),
                Err(e) => (e.exit_code(), error_info(&e)),
            };
            Ok(object(&[
                ("ok", (exit_code == 0).into()),
                ("exitCode", exit_code.into()),
                ("error", error),
                ("elapsedMs", elapsed_ms.into()),
            ]))
        })
    }

    /// Type checks a script without running it (imports the policy would
    /// prompt for are denied) and, if it type checks, lints it.
    #[wasm_bindgen(unchecked_return_type = "CheckResult")]
    pub fn check(&self, source: &str, name: Option<String>) -> Result<JsValue, JsValue> {
        let name = name.unwrap_or_else(|| "main.sand".into());
        let engine = take(&self.engine)?;
        let r = engine.lint_source(&name, source);
        *self.engine.borrow_mut() = Some(engine);
        let (ok, diagnostics, lint, error) = match r {
            Ok(warnings) => (true, Array::new().into(), array(&warnings, lint_warning), JsValue::UNDEFINED),
            Err(Error::Compile(errors)) => (false, array(&errors, diagnostic), Array::new().into(), JsValue::UNDEFINED),
            Err(e) => (false, Array::new().into(), Array::new().into(), error_info(&e)),
        };
        Ok(object(&[("ok", ok.into()), ("diagnostics", diagnostics), ("lint", lint), ("error", error)]))
    }

    /// Lint warnings of a script that type checks; throws an `Error` with
    /// the first problem otherwise.
    #[wasm_bindgen(unchecked_return_type = "LintWarning[]")]
    pub fn lint(&self, source: &str, name: Option<String>) -> Result<JsValue, JsValue> {
        let name = name.unwrap_or_else(|| "main.sand".into());
        let engine = take(&self.engine)?;
        let r = engine.lint_source(&name, source);
        *self.engine.borrow_mut() = Some(engine);
        Ok(array(&r.map_err(|e| error(&e.to_string()))?, lint_warning))
    }

    /// Stops the current run (or session input) as soon as it waits or
    /// checks its limits; it fails with an `interrupted` error.
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// A session: inputs run one at a time, each in the context of the
    /// previous ones (like `sand repl`), with this engine's configuration.
    pub fn session(&self) -> Result<Session, JsValue> {
        let engine = self.engine.borrow();
        let engine = engine.as_ref().ok_or_else(|| error("the engine is running"))?;
        let session = engine.session();
        let interrupt = session.interrupt_handle();
        Ok(Session { session: Rc::new(RefCell::new(Some(session))), interrupt, hooks: self.hooks.clone(), stop_tasks: Rc::default() })
    }

    /// Every file of the file system, by path.
    #[wasm_bindgen(unchecked_return_type = "FileEntry[]")]
    pub fn files(&self) -> JsValue {
        file_entries(self.fs.files())
    }

    /// A number that grows whenever a file is written or moved (by a
    /// script or the host), for `changedFiles`.
    pub fn generation(&self) -> f64 {
        self.fs.generation() as f64
    }

    /// The files written or moved since the file system was at `generation`.
    #[wasm_bindgen(js_name = changedFiles, unchecked_return_type = "FileEntry[]")]
    pub fn changed_files(&self, generation: f64) -> JsValue {
        file_entries(self.fs.changed_files(generation as u64))
    }

    /// The contents of a file, or `undefined` if there is none.
    #[wasm_bindgen(js_name = readFile)]
    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        self.fs.read(path).ok()
    }

    /// Writes a file (text or bytes), creating its directories.
    #[wasm_bindgen(js_name = writeFile)]
    pub fn write_file(
        &self,
        path: &str,
        #[wasm_bindgen(unchecked_param_type = "string | Uint8Array")] data: JsValue,
    ) -> Result<(), JsValue> {
        let bytes = file_bytes(&data).ok_or_else(|| error("file contents must be a string or a Uint8Array"))?;
        self.fs.write(path, bytes).map_err(|e| error(&format!("{path}: {e}")))
    }

    /// Removes a file, or a directory with its contents.
    pub fn remove(&self, path: &str) -> Result<(), JsValue> {
        self.fs.remove(path).map_err(|e| error(&format!("{path}: {e}")))
    }
}

/// An interactive session (see [`Engine::session`]).
#[wasm_bindgen]
pub struct Session {
    session: Rc<RefCell<Option<sandlang::Session>>>,
    interrupt: sandlang::InterruptHandle,
    hooks: Rc<HookKey>,
    /// Ends the `runTasks` in progress.
    stop_tasks: Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>,
}

#[wasm_bindgen]
impl Session {
    /// Runs one input: statements, expressions or declarations, named
    /// `name` in messages if given (e.g. `main.sand` for a whole program
    /// that later inputs continue), else `<input N>`. Resolves to an
    /// `EvalResult`; rejects if an input or `runTasks` is running.
    #[wasm_bindgen(unchecked_return_type = "Promise<EvalResult>")]
    pub fn eval(&self, source: String, name: Option<String>) -> js_sys::Promise {
        let slot = self.session.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let mut session = take(&slot)?;
            let r = match &name {
                Some(name) => session.eval_as(name, &source).await,
                None => session.eval(&source).await,
            };
            let exit_code = session.exit_code().map_or(JsValue::UNDEFINED, JsValue::from);
            *slot.borrow_mut() = Some(session);
            let (value, error) = match r {
                Ok(value) => (value.map_or(JsValue::UNDEFINED, JsValue::from), JsValue::UNDEFINED),
                Err(e) => (JsValue::UNDEFINED, error_info(&e)),
            };
            Ok(object(&[("value", value), ("error", error), ("exitCode", exit_code)]))
        })
    }

    /// Runs the tasks that earlier inputs left running (their output goes to
    /// `onOutput`) until `stopTasks` is called; call it, and wait for this
    /// promise, before the next `eval`. If a task fails, every task is
    /// cancelled and the error is written to standard error; the promise
    /// resolves to `{ exitCode }` if a task calls `exit`, else to `{}`.
    #[wasm_bindgen(js_name = runTasks, unchecked_return_type = "Promise<{ exitCode?: number }>")]
    pub fn run_tasks(&self) -> js_sys::Promise {
        let slot = self.session.clone();
        let hook = self.hooks.0;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tasks.borrow_mut() = Some(tx);
        wasm_bindgen_futures::future_to_promise(async move {
            let mut session = take(&slot)?;
            let on_error = |e: Error| {
                hooks::emit(hook, "stderr", &format!("{}(in a background task; all tasks were cancelled)\n", e.render()));
            };
            let stopped = session.run_tasks_until(rx, on_error).await;
            let exit_code = if stopped.is_none() { session.exit_code() } else { None };
            *slot.borrow_mut() = Some(session);
            Ok(object(&[("exitCode", exit_code.map_or(JsValue::UNDEFINED, JsValue::from))]))
        })
    }

    /// Ends the `runTasks` in progress (the tasks stay, paused, until the
    /// next input or `runTasks`).
    #[wasm_bindgen(js_name = stopTasks)]
    pub fn stop_tasks(&self) {
        if let Some(tx) = self.stop_tasks.borrow_mut().take() {
            let _ = tx.send(());
        }
    }

    /// Cancels the tasks left running; returns how many there were.
    #[wasm_bindgen(js_name = cancelTasks)]
    pub fn cancel_tasks(&self) -> Result<u32, JsValue> {
        let mut slot = self.session.borrow_mut();
        Ok(slot.as_mut().ok_or_else(|| error("an input is running"))?.cancel_tasks() as u32)
    }

    /// The type of an expression, without running it.
    #[wasm_bindgen(js_name = typeOf)]
    pub fn type_of(&self, expr: &str) -> Result<String, JsValue> {
        let mut session = take(&self.session)?;
        let r = session.type_of(expr);
        *self.session.borrow_mut() = Some(session);
        r.map_err(|e| error(e.render().trim_end()))
    }

    /// Forgets every input.
    pub fn reset(&self) -> Result<(), JsValue> {
        let mut slot = self.session.borrow_mut();
        slot.as_mut().ok_or_else(|| error("an input is running"))?.reset();
        Ok(())
    }

    /// Stops the running input (see [`Engine::interrupt`]).
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// Whether `source` is a complete input (no open brackets, strings or
    /// comments): a front end continues reading lines until it is.
    #[wasm_bindgen(js_name = isComplete)]
    pub fn is_complete(source: &str) -> bool {
        sandlang::Session::is_complete(source)
    }
}

/// Installed when the module starts: a Rust panic (a bug) aborts the
/// WebAssembly module, so its message is logged and left in the global
/// `sandPanic` for the host to report.
#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| {
        let message = format!("internal error: {info}");
        let global = js_sys::global();
        let _ = js_sys::Reflect::set(&global, &"sandPanic".into(), &message.as_str().into());
        if let Ok(console) = js_sys::Reflect::get(&global, &"console".into())
            && let Ok(error) = js_sys::Reflect::get(&console, &"error".into())
            && let Ok(error) = error.dyn_into::<js_sys::Function>()
        {
            let _ = error.call1(&console, &message.as_str().into());
        }
    }));
}

/// Formats Sand source code; throws an `Error` if it does not parse.
#[wasm_bindgen]
pub fn format(source: &str) -> Result<String, JsValue> {
    sandlang::fmt::format_source(source).map_err(|e| error(&e))
}

/// Formats a policy file; throws an `Error` with `policyErrors` if it does
/// not split into words.
#[wasm_bindgen(js_name = formatPolicy)]
pub fn format_policy(policy: &str) -> Result<String, JsValue> {
    sandlang::policy::format_policy(policy).map_err(policy_error)
}

/// Lints a policy file whose relative paths start at `cwd` (default `/`);
/// throws an `Error` with `policyErrors` if it is invalid.
#[wasm_bindgen(js_name = lintPolicy, unchecked_return_type = "LintWarning[]")]
pub fn lint_policy(policy: &str, cwd: Option<String>) -> Result<JsValue, JsValue> {
    let cwd = cwd.unwrap_or_else(|| "/".into());
    let warnings = sandlang::policy::lint_policy(policy, Path::new(&cwd), None).map_err(policy_error)?;
    Ok(array(&warnings, lint_warning))
}

/// The syntax highlighting of `source`: `[start, end, kind]` triples of
/// UTF-16 offsets (as JavaScript strings count) and indices into
/// `highlightKinds()`, in order.
#[wasm_bindgen]
pub fn highlight(source: &str) -> Vec<u32> {
    // Byte offsets to UTF-16 offsets.
    let mut utf16 = Vec::with_capacity(source.len() + 1);
    let mut n = 0u32;
    for c in source.chars() {
        for _ in 0..c.len_utf8() {
            utf16.push(n);
        }
        n += c.len_utf16() as u32;
    }
    utf16.push(n);
    let mut out = Vec::new();
    for (lo, hi, kind) in sandlang::syntax::highlight::highlight(source) {
        out.extend([utf16[lo], utf16[hi], HIGHLIGHT_KINDS.iter().position(|k| *k == kind).unwrap_or(0) as u32]);
    }
    out
}

const HIGHLIGHT_KINDS: [sandlang::syntax::highlight::Highlight; 9] = {
    use sandlang::syntax::highlight::Highlight::*;
    [Keyword, Constant, Number, String, Interpolation, Comment, DocComment, Type, Call]
};

/// The names of the kinds of `highlight` (e.g. for CSS classes).
#[wasm_bindgen(js_name = highlightKinds)]
pub fn highlight_kinds() -> Vec<String> {
    HIGHLIGHT_KINDS.iter().map(|k| k.name().to_string()).collect()
}

/// The version of Sand.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ----------------------------------------------------------------------
// Conversions

/// A JavaScript object with the given properties (`undefined` ones are left out).
fn object(properties: &[(&str, JsValue)]) -> JsValue {
    let o = Object::new();
    for (name, value) in properties {
        if !value.is_undefined() {
            let _ = Reflect::set(&o, &(*name).into(), value);
        }
    }
    o.into()
}

fn array<T>(items: &[T], f: impl Fn(&T) -> JsValue) -> JsValue {
    items.iter().map(f).collect::<Array>().into()
}

fn strings(items: &[String]) -> JsValue {
    array(items, |s| s.into())
}

/// `{path, data}` entries.
fn file_entries(files: Vec<(std::path::PathBuf, Vec<u8>)>) -> JsValue {
    array(&files, |(path, data)| {
        object(&[("path", path.display().to_string().into()), ("data", js_sys::Uint8Array::from(data.as_slice()).into())])
    })
}

/// A JavaScript `Error`.
fn error(message: &str) -> JsValue {
    js_sys::Error::new(message).into()
}

/// `options[name]`, unless it is missing (undefined or null).
fn option(options: &JsValue, name: &str) -> Result<Option<JsValue>, JsValue> {
    if options.is_undefined() || options.is_null() {
        return Ok(None);
    }
    let value = Reflect::get(options, &name.into())?;
    Ok((!value.is_undefined() && !value.is_null()).then_some(value))
}

fn string_option(options: &JsValue, name: &str) -> Result<Option<String>, JsValue> {
    option(options, name)?.map(|v| v.as_string().ok_or_else(|| error(&format!("`{name}` must be a string")))).transpose()
}

fn number_option(options: &JsValue, name: &str) -> Result<Option<f64>, JsValue> {
    option(options, name)?
        .map(|v| v.as_f64().filter(|n| n.is_finite()).ok_or_else(|| error(&format!("`{name}` must be a number"))))
        .transpose()
}

/// `options[name]`: an array of strings.
fn string_list(options: &JsValue, name: &str) -> Result<Vec<String>, JsValue> {
    let Some(list) = option(options, name)? else { return Ok(Vec::new()) };
    let bad = || error(&format!("`{name}` must be an array of strings"));
    let list = list.dyn_into::<Array>().map_err(|_| bad())?;
    list.iter().map(|v| v.as_string().ok_or_else(bad)).collect()
}

/// `options[name]`: an object whose values are strings.
fn string_entries(options: &JsValue, name: &str) -> Result<Vec<(String, String)>, JsValue> {
    let Some(obj) = option(options, name)? else { return Ok(Vec::new()) };
    let bad = || error(&format!("`{name}` must be an object of strings"));
    let obj = obj.dyn_into::<Object>().map_err(|_| bad())?;
    Object::entries(&obj)
        .iter()
        .map(|entry| {
            let pair = Array::from(&entry);
            Ok((pair.get(0).as_string().ok_or_else(bad)?, pair.get(1).as_string().ok_or_else(bad)?))
        })
        .collect()
}

fn policy_error(errors: Vec<PolicyError>) -> JsValue {
    let lines: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    let err = js_sys::Error::new(&format!("invalid policy\n{}", lines.join("\n")));
    let list = array(&errors, |e| {
        object(&[("line", (e.line as u32).into()), ("column", (e.column as u32).into()), ("message", e.message.as_str().into())])
    });
    let _ = Reflect::set(&err, &"policyErrors".into(), &list);
    err.into()
}

/// The engine out of its slot for a call; fails if a run has it.
fn take<T>(slot: &RefCell<Option<T>>) -> Result<T, JsValue> {
    slot.borrow_mut().take().ok_or_else(|| error("the engine is busy: wait for the current run to finish"))
}

/// Denies keys of the object `obj` other than `known`.
fn check_keys(obj: &JsValue, known: &[&str], what: &str) -> Result<(), JsValue> {
    let Some(obj) = obj.dyn_ref::<Object>() else {
        return if obj.is_undefined() || obj.is_null() { Ok(()) } else { Err(error(&format!("{what}s must be an object"))) };
    };
    for key in Object::keys(obj) {
        let key = key.as_string().unwrap_or_default();
        if !known.contains(&key.as_str()) {
            return Err(error(&format!("unknown {what} `{key}` (expected one of: {})", known.join(", "))));
        }
    }
    Ok(())
}

/// The function `obj[name]`, if any.
fn function(obj: &JsValue, name: &str) -> Result<Option<js_sys::Function>, JsValue> {
    option(obj, name)?.map(|f| f.dyn_into::<js_sys::Function>().map_err(|_| error(&format!("`{name}` must be a function")))).transpose()
}

/// Text or bytes from JavaScript.
fn file_bytes(data: &JsValue) -> Option<Vec<u8>> {
    if let Some(s) = data.as_string() {
        return Some(s.into_bytes());
    }
    data.dyn_ref::<js_sys::Uint8Array>().map(|a| a.to_vec())
}

/// Writes `options.files` into `fs`.
fn write_files(fs: &MemFs, options: &JsValue) -> Result<(), JsValue> {
    let Some(files) = option(options, "files")? else { return Ok(()) };
    let files = files.dyn_into::<Object>().map_err(|_| error("`files` must be an object"))?;
    for entry in Object::entries(&files) {
        let pair = Array::from(&entry);
        let path = pair.get(0).as_string().unwrap_or_default();
        let data = file_bytes(&pair.get(1)).ok_or_else(|| error(&format!("file {path}: contents must be a string or a Uint8Array")))?;
        fs.write(&path, data).map_err(|e| error(&format!("file {path}: {e}")))?;
    }
    Ok(())
}

/// The frames of a trace, innermost first.
fn frames(trace: &[sandlang::TraceFrame]) -> JsValue {
    array(trace, |t| object(&[("function", t.function.as_str().into()), ("file", t.file.as_str().into()), ("line", t.line.into())]))
}

/// A `SandError`.
fn error_info(e: &Error) -> JsValue {
    let (kind, diagnostics, trace) = match e {
        Error::Compile(ds) => ("compile", array(ds, diagnostic), Array::new().into()),
        Error::Runtime { trace, .. } => ("runtime", Array::new().into(), frames(trace)),
        Error::Internal { trace, .. } => ("internal", Array::new().into(), frames(trace)),
        Error::Limit(_) => ("limit", Array::new().into(), Array::new().into()),
        Error::Interrupted => ("interrupted", Array::new().into(), Array::new().into()),
        Error::Io(_) => ("io", Array::new().into(), Array::new().into()),
    };
    let message = match e {
        Error::Compile(_) => e.render(),
        Error::Runtime { message, .. } => format!("error: {}", message.strip_prefix("error: ").unwrap_or(message)),
        Error::Internal { message, .. } => format!("internal error: {message}"),
        e => e.to_string(),
    };
    object(&[
        ("kind", kind.into()),
        ("message", message.into()),
        ("exitCode", e.exit_code().into()),
        ("diagnostics", diagnostics),
        ("trace", trace),
    ])
}

/// A `Diagnostic`, with its rendering.
fn diagnostic(d: &Diagnostic) -> JsValue {
    let severity = match d.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    object(&[
        ("severity", severity.into()),
        ("code", d.code.as_str().into()),
        ("message", d.message.as_str().into()),
        ("file", d.file.as_str().into()),
        ("line", d.line.into()),
        ("column", d.column.into()),
        ("width", d.width.into()),
        ("notes", strings(&d.notes)),
        ("rendered", d.render().into()),
    ])
}

fn lint_warning(w: &LintWarning) -> JsValue {
    object(&[("code", w.code.into()), ("message", w.message.as_str().into()), ("line", w.line.into()), ("column", w.column.into())])
}
