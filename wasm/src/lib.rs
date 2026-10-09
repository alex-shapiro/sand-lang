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
use sandlang::policy::{PolicyError, PolicyFile};
use sandlang::{Config, Decision, Diagnostic, Error, MemFs, Severity};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
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
  kind: "read" | "write" | "import" | "net" | "listen" | "env" | "stdin" | "stdout" | "stderr" | "custom";
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
  kind: "compile" | "runtime" | "limit" | "interrupted" | "io";
  message: string;
  diagnostics: Diagnostic[];
  /** For runtime errors: the stack, innermost first. */
  trace: { function: string; file: string; line: number }[];
}

export interface RunResult {
  ok: boolean;
  /** The script's exit code, or as `sand run` exits: 1 panic, 2 compile error, 3 limit, 130 interrupted. */
  exitCode: number;
  error?: SandError;
  elapsedMs: number;
}

export interface CheckResult {
  ok: boolean;
  /** Errors, or warnings when `ok`. */
  diagnostics: Diagnostic[];
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

/// The fields of [`Options`] (serde-wasm-bindgen reads only known fields,
/// so unknown ones, typically misspelt, are refused by `check_keys`).
const OPTION_KEYS: &[&str] = &["policy", "files", "env", "args", "stdin", "seed", "fileSystemCapacity", "cwd"];

/// Options of an engine (see `EngineOptions` above).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Options {
    policy: String,
    /// Read by `write_files` (strings or bytes).
    files: Option<serde::de::IgnoredAny>,
    env: BTreeMap<String, String>,
    args: Vec<String>,
    stdin: Option<String>,
    seed: Option<f64>,
    file_system_capacity: Option<f64>,
    cwd: Option<String>,
}

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
    files: Files,
    interrupt: sandlang::InterruptHandle,
    hooks: Rc<HookKey>,
}

#[wasm_bindgen]
impl Engine {
    /// Creates an engine. Throws an `Error` for invalid options; for an
    /// invalid policy, its `policyErrors` property lists `{line, message}`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "EngineOptions")] options: JsValue,
        #[wasm_bindgen(unchecked_param_type = "EngineHooks")] hooks: JsValue,
    ) -> Result<Engine, JsValue> {
        let opts: Options = if options.is_undefined() || options.is_null() {
            Options::default()
        } else {
            check_keys(&options, OPTION_KEYS, "engine option")?;
            serde_wasm_bindgen::from_value(options.clone()).map_err(|e| error(&format!("invalid engine options: {e}")))?
        };
        check_keys(&hooks, &["onOutput", "onPrompt"], "engine hook")?;
        let cwd = opts.cwd.clone().unwrap_or_else(|| "/".into());
        let policy = PolicyFile::parse(&opts.policy, Path::new(&cwd)).map_err(policy_error)?;
        let fs = match opts.file_system_capacity {
            Some(c) if c.is_finite() && c >= 0.0 => MemFs::with_capacity(c as usize),
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
            .env(opts.env.into_iter().collect())
            .args(opts.args)
            .stdout(JsOutput::new(key.0, "stdout"))
            .stderr(JsOutput::new(key.0, "stderr"));
        config = config.prompter(JsPrompter { id: key.0 });
        if let Some(stdin) = opts.stdin {
            config = config.stdin_bytes(stdin.into_bytes());
        }
        if let Some(seed) = opts.seed {
            config = config.seed(seed as u64);
        }
        let engine = sandlang::Engine::new(config);
        let interrupt = engine.interrupt_handle();
        let files = Files { fs, root: Path::new("/").join(&cwd) };
        Ok(Engine { engine: Rc::new(RefCell::new(Some(engine))), files, interrupt, hooks: key })
    }

    /// Runs a script (`name` names it in messages; default `main.sand`).
    /// Resolves to a `RunResult` (it does not reject for errors of the
    /// script); rejects if the engine is already running.
    #[wasm_bindgen(unchecked_return_type = "Promise<RunResult>")]
    pub fn run(&self, source: String, name: Option<String>) -> js_sys::Promise {
        let slot = self.engine.clone();
        let files = self.files.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let name = name.unwrap_or_else(|| "main.sand".into());
            let mut engine = take(&slot)?;
            let started = sandlang::platform::Instant::now();
            let outcome = engine.run_source(&name, &source).await;
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            *slot.borrow_mut() = Some(engine);
            let result = match outcome {
                Ok(o) => RunResult { ok: o.exit_code == 0, exit_code: o.exit_code, error: None, elapsed_ms },
                Err(e) => {
                    let exit_code = exit_code(&e);
                    RunResult { ok: false, exit_code, error: Some(error_info(&e, &name, &source, &files)), elapsed_ms }
                }
            };
            to_js(&result)
        })
    }

    /// Type checks a script without running it (imports the policy would
    /// prompt for are refused).
    #[wasm_bindgen(unchecked_return_type = "CheckResult")]
    pub fn check(&self, source: &str, name: Option<String>) -> Result<JsValue, JsValue> {
        let name = name.unwrap_or_else(|| "main.sand".into());
        let mut engine = take(&self.engine)?;
        let r = engine.check_source(&name, source);
        *self.engine.borrow_mut() = Some(engine);
        let result = match r {
            Ok(warnings) => CheckResult { ok: true, diagnostics: diagnostics(&warnings, &name, source, &self.files), error: None },
            Err(Error::Compile(errors)) => {
                CheckResult { ok: false, diagnostics: diagnostics(&errors, &name, source, &self.files), error: None }
            }
            Err(e) => CheckResult { ok: false, diagnostics: vec![], error: Some(error_info(&e, &name, source, &self.files)) },
        };
        to_js(&result)
    }

    /// Lint warnings of a script that type checks; throws an `Error` with
    /// the first problem otherwise.
    #[wasm_bindgen(unchecked_return_type = "LintWarning[]")]
    pub fn lint(&self, source: &str, name: Option<String>) -> Result<JsValue, JsValue> {
        let name = name.unwrap_or_else(|| "main.sand".into());
        let engine = take(&self.engine)?;
        let r = sandlang::lint::lint_source_with(&engine, &name, source);
        *self.engine.borrow_mut() = Some(engine);
        let warnings: Vec<LintInfo> = r
            .map_err(|e| error(&e))?
            .into_iter()
            .map(|w| LintInfo { code: w.code, message: w.message, line: w.line, column: w.column })
            .collect();
        to_js(&warnings)
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
        Ok(Session {
            session: Rc::new(RefCell::new(Some(session))),
            files: self.files.clone(),
            interrupt,
            hooks: self.hooks.clone(),
            stop_tasks: Rc::default(),
        })
    }

    /// Every file of the file system, by path.
    #[wasm_bindgen(unchecked_return_type = "FileEntry[]")]
    pub fn files(&self) -> Result<JsValue, JsValue> {
        let files = js_sys::Array::new();
        for (path, data) in self.files.fs.files() {
            let entry = js_sys::Object::new();
            js_sys::Reflect::set(&entry, &"path".into(), &path.display().to_string().into())?;
            js_sys::Reflect::set(&entry, &"data".into(), &js_sys::Uint8Array::from(data.as_slice()))?;
            files.push(&entry);
        }
        Ok(files.into())
    }

    /// The contents of a file, or `undefined` if there is none.
    #[wasm_bindgen(js_name = readFile)]
    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        self.files.fs.read(path).ok()
    }

    /// Writes a file (text or bytes), creating its directories.
    #[wasm_bindgen(js_name = writeFile)]
    pub fn write_file(
        &self,
        path: &str,
        #[wasm_bindgen(unchecked_param_type = "string | Uint8Array")] data: JsValue,
    ) -> Result<(), JsValue> {
        let bytes = file_bytes(&data).ok_or_else(|| error("file contents must be a string or a Uint8Array"))?;
        self.files.fs.write(path, bytes).map_err(|e| error(&format!("{path}: {e}")))
    }

    /// Removes a file, or a directory with its contents.
    pub fn remove(&self, path: &str) -> Result<(), JsValue> {
        self.files.fs.remove(path).map_err(|e| error(&format!("{path}: {e}")))
    }
}

/// An interactive session (see [`Engine::session`]).
#[wasm_bindgen]
pub struct Session {
    session: Rc<RefCell<Option<sandlang::Session>>>,
    files: Files,
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
        let files = self.files.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let mut session = take(&slot)?;
            let r = match &name {
                Some(name) => session.eval_as(name, &source).await,
                None => session.eval(&source).await,
            };
            let exit_code = session.exit_code();
            *slot.borrow_mut() = Some(session);
            let result = match r {
                Ok(value) => EvalResult { value, error: None, exit_code },
                Err(e) => EvalResult {
                    value: None,
                    error: Some(error_info(&e, name.as_deref().unwrap_or("<input>"), &source, &files)),
                    exit_code,
                },
            };
            to_js(&result)
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
        let files = self.files.clone();
        let hook = self.hooks.0;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tasks.borrow_mut() = Some(tx);
        wasm_bindgen_futures::future_to_promise(async move {
            let mut session = take(&slot)?;
            let on_error = |e: Error| {
                let message = error_info(&e, "<task>", "", &files).message;
                hooks::emit(hook, "stderr", &format!("{}\n(in a background task; all tasks were cancelled)\n", message.trim_end()));
            };
            let stopped = session.run_tasks_until(rx, on_error).await;
            let exit_code = if stopped.is_none() { session.exit_code() } else { None };
            *slot.borrow_mut() = Some(session);
            let result = js_sys::Object::new();
            if let Some(code) = exit_code {
                js_sys::Reflect::set(&result, &"exitCode".into(), &code.into())?;
            }
            Ok(result.into())
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
        r.map_err(|e| error(&error_info(&e, "<type>", expr, &self.files).message))
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunResult {
    ok: bool,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorInfo>,
    elapsed_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckResult {
    ok: bool,
    diagnostics: Vec<DiagnosticInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvalResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

#[derive(Serialize)]
struct ErrorInfo {
    kind: &'static str,
    message: String,
    diagnostics: Vec<DiagnosticInfo>,
    trace: Vec<Frame>,
}

#[derive(Serialize)]
struct Frame {
    function: String,
    file: String,
    line: usize,
}

#[derive(Serialize)]
struct DiagnosticInfo {
    severity: &'static str,
    code: String,
    message: String,
    file: String,
    line: usize,
    column: usize,
    width: usize,
    notes: Vec<String>,
    rendered: String,
}

#[derive(Serialize)]
struct LintInfo {
    code: &'static str,
    message: String,
    line: usize,
    column: usize,
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    value.serialize(&serde_wasm_bindgen::Serializer::json_compatible()).map_err(|e| error(&e.to_string()))
}

/// A JavaScript `Error`.
fn error(message: &str) -> JsValue {
    js_sys::Error::new(message).into()
}

fn policy_error(errors: Vec<PolicyError>) -> JsValue {
    let lines: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    let err = js_sys::Error::new(&format!("invalid policy\n{}", lines.join("\n")));
    let list = js_sys::Array::new();
    for e in &errors {
        let o = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&o, &"line".into(), &(e.line as u32).into());
        let _ = js_sys::Reflect::set(&o, &"message".into(), &e.message.as_str().into());
        list.push(&o);
    }
    let _ = js_sys::Reflect::set(&err, &"policyErrors".into(), &list);
    err.into()
}

/// The engine out of its slot for a call; fails if a run has it.
fn take<T>(slot: &RefCell<Option<T>>) -> Result<T, JsValue> {
    slot.borrow_mut().take().ok_or_else(|| error("the engine is busy: wait for the current run to finish"))
}

/// Refuses keys of the object `obj` other than `known`.
fn check_keys(obj: &JsValue, known: &[&str], what: &str) -> Result<(), JsValue> {
    let Some(obj) = obj.dyn_ref::<js_sys::Object>() else {
        return if obj.is_undefined() || obj.is_null() { Ok(()) } else { Err(error(&format!("{what}s must be an object"))) };
    };
    for key in js_sys::Object::keys(obj) {
        let key = key.as_string().unwrap_or_default();
        if !known.contains(&key.as_str()) {
            return Err(error(&format!("unknown {what} `{key}` (expected one of: {})", known.join(", "))));
        }
    }
    Ok(())
}

/// The function `obj[name]`, if any.
fn function(obj: &JsValue, name: &str) -> Result<Option<js_sys::Function>, JsValue> {
    if obj.is_undefined() || obj.is_null() {
        return Ok(None);
    }
    let f = js_sys::Reflect::get(obj, &name.into())?;
    if f.is_undefined() || f.is_null() {
        return Ok(None);
    }
    f.dyn_into::<js_sys::Function>().map(Some).map_err(|_| error(&format!("`{name}` must be a function")))
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
    if options.is_undefined() || options.is_null() {
        return Ok(());
    }
    let files = js_sys::Reflect::get(options, &"files".into())?;
    if files.is_undefined() || files.is_null() {
        return Ok(());
    }
    let files = files.dyn_into::<js_sys::Object>().map_err(|_| error("`files` must be an object"))?;
    for entry in js_sys::Object::entries(&files) {
        let pair = js_sys::Array::from(&entry);
        let path = pair.get(0).as_string().unwrap_or_default();
        let data = file_bytes(&pair.get(1)).ok_or_else(|| error(&format!("file {path}: contents must be a string or a Uint8Array")))?;
        fs.write(&path, data).map_err(|e| error(&format!("file {path}: {e}")))?;
    }
    Ok(())
}

/// The exit status `sand run` would have for `e`.
fn exit_code(e: &Error) -> i32 {
    match e {
        Error::Compile(_) | Error::Io(_) => 2,
        Error::Runtime { .. } => 1,
        Error::Limit(_) => 3,
        Error::Interrupted => 130,
    }
}

/// An engine's file system and module root, for diagnostics' excerpts.
#[derive(Clone)]
struct Files {
    fs: MemFs,
    root: std::path::PathBuf,
}

fn error_info(e: &Error, name: &str, source: &str, fs: &Files) -> ErrorInfo {
    let (kind, diagnostics, trace) = match e {
        Error::Compile(ds) => ("compile", diagnostics(ds, name, source, fs), vec![]),
        Error::Runtime { trace, .. } => {
            ("runtime", vec![], trace.iter().map(|t| Frame { function: t.function.clone(), file: t.file.clone(), line: t.line }).collect())
        }
        Error::Limit(_) => ("limit", vec![], vec![]),
        Error::Interrupted => ("interrupted", vec![], vec![]),
        Error::Io(_) => ("io", vec![], vec![]),
    };
    let message = match e {
        Error::Compile(_) => diagnostics.iter().map(|d| d.rendered.as_str()).collect(),
        Error::Runtime { message, .. } => format!("panic: {message}"),
        e => e.to_string(),
    };
    ErrorInfo { kind, message, diagnostics, trace }
}

/// Diagnostics with their rendering (a source excerpt from the main source,
/// or from the file system for imported modules).
fn diagnostics(ds: &[Diagnostic], name: &str, source: &str, files: &Files) -> Vec<DiagnosticInfo> {
    ds.iter()
        .map(|d| {
            let text = if d.file == name || d.file.starts_with("<input") || d.file == "<type>" {
                Some(source.to_string())
            } else {
                // Imported modules are named relative to the module root.
                files.fs.read(files.root.join(&d.file)).ok().and_then(|b| String::from_utf8(b).ok())
            };
            DiagnosticInfo {
                severity: match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                code: d.code.clone(),
                message: d.message.clone(),
                file: d.file.clone(),
                line: d.line,
                column: d.column,
                width: d.width,
                notes: d.notes.clone(),
                rendered: d.render(text.as_deref()),
            }
        })
        .collect()
}
