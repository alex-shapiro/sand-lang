//! The JavaScript callbacks of an engine: output and permission prompts.
//!
//! The engine wants `Send` output sinks and prompters, and JavaScript values
//! are not `Send`. WebAssembly here is single threaded, so the callbacks
//! live in a thread-local table and the sinks and prompters hold their key.

use js_sys::Function;
use sandlang::{Capability, OutputSink, PromptAnswer, Prompter};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::HashMap;
use wasm_bindgen::prelude::*;

/// An engine's callbacks.
#[derive(Default)]
pub(crate) struct Hooks {
    /// `onOutput(stream, text)`, `stream` being `"stdout"` or `"stderr"`.
    pub on_output: Option<Function>,
    /// `onPrompt(request) -> answer | Promise<answer>`.
    pub on_prompt: Option<Function>,
}

thread_local! {
    static HOOKS: RefCell<HashMap<u32, Hooks>> = RefCell::new(HashMap::new());
    static NEXT_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

/// Registers `hooks`; returns their key.
pub(crate) fn register(hooks: Hooks) -> u32 {
    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id.wrapping_add(1).max(1));
        id
    });
    HOOKS.with(|h| h.borrow_mut().insert(id, hooks));
    id
}

pub(crate) fn unregister(id: u32) {
    HOOKS.with(|h| h.borrow_mut().remove(&id));
}

/// Writes `text` to the output callback of hooks `id`, as if `stream` had.
pub(crate) fn emit(id: u32, stream: &str, text: &str) {
    if let Some(f) = callback(id, |h| h.on_output.as_ref()) {
        let _ = f.call2(&JsValue::NULL, &JsValue::from_str(stream), &JsValue::from_str(text));
    }
}

fn callback(id: u32, which: impl Fn(&Hooks) -> Option<&Function>) -> Option<Function> {
    HOOKS.with(|h| h.borrow().get(&id).and_then(|h| which(h).cloned()))
}

/// Standard output or error, delivered to `onOutput` as text. Standard
/// output is buffered (calls into JavaScript cost, and a worker's messages
/// even more): it is delivered when 8 KiB have accumulated, before anything
/// is written to standard error, and whenever the engine flushes it, which
/// it does every few milliseconds while it runs, when it waits, and when
/// the run ends.
pub(crate) struct JsOutput {
    pub id: u32,
    pub stream: &'static str,
    /// Text not delivered yet, and the bytes of an unfinished UTF-8 sequence.
    pending: Vec<u8>,
}

const DELIVERY_BYTES: usize = 8 << 10;

impl JsOutput {
    pub fn new(id: u32, stream: &'static str) -> JsOutput {
        JsOutput { id, stream, pending: Vec::new() }
    }

    /// Delivers the pending text (keeping an unfinished UTF-8 sequence).
    fn deliver(&mut self) {
        let complete = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            // Invalid bytes are replaced; an incomplete sequence at the end waits.
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        if complete == 0 {
            return;
        }
        let rest = self.pending.split_off(complete);
        let text = String::from_utf8_lossy(&std::mem::replace(&mut self.pending, rest)).into_owned();
        if let Some(f) = callback(self.id, |h| h.on_output.as_ref()) {
            let _ = f.call2(&JsValue::NULL, &JsValue::from_str(self.stream), &JsValue::from_str(&text));
        }
    }
}

impl OutputSink for JsOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pending.extend_from_slice(data);
        if self.stream == "stderr" || self.pending.len() >= DELIVERY_BYTES {
            self.deliver();
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.deliver();
        Ok(())
    }
}

/// What `onPrompt` is asked about.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptRequest {
    /// `read`, `write`, `net`, `listen`, `env`, `stdin`, `stdout`, `stderr`,
    /// `import` or `custom`.
    kind: &'static str,
    /// The path, host, variable, ... (empty for standard streams).
    target: String,
    /// The whole request in words, e.g. "read /data/a.txt".
    description: String,
    /// The policy file rules that answer it from now on ("always" and
    /// "never"), e.g. `allow read /data/a.txt`, if a rule can.
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deny_rule: Option<String>,
}

/// Asks `onPrompt`, which answers `"once"`, `"always"`, `"deny"` or
/// `"never"` (or a promise of one); anything else, an exception or no
/// callback denies once.
pub(crate) struct JsPrompter {
    pub id: u32,
}

impl Prompter for JsPrompter {
    fn prompt(&self, cap: Capability) -> sandlang::policy::PromptFuture {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Some(f) = callback(self.id, |h| h.on_prompt.as_ref()) {
            let (kind, target) = describe(&cap);
            let request = PromptRequest {
                kind,
                target,
                description: cap.to_string(),
                allow_rule: sandlang::policy::rule_for(&cap, true),
                deny_rule: sandlang::policy::rule_for(&cap, false),
            };
            let request = serde_wasm_bindgen::to_value(&request).unwrap_or(JsValue::NULL);
            match f.call1(&JsValue::NULL, &request) {
                Ok(answer) => {
                    // The answer may be a promise: wait for it in a local task;
                    // the engine only waits for the (`Send`) receiver.
                    wasm_bindgen_futures::spawn_local(async move {
                        let answer = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&answer)).await;
                        let _ = tx.send(answer.ok().and_then(|a| a.as_string()).map_or(PromptAnswer::DenyOnce, |a| parse_answer(&a)));
                    });
                }
                Err(_) => drop(tx),
            }
        }
        Box::pin(async move { rx.await.unwrap_or(PromptAnswer::DenyOnce) })
    }
}

fn parse_answer(a: &str) -> PromptAnswer {
    match a {
        "once" | "allow" | "yes" => PromptAnswer::AllowOnce,
        "always" => PromptAnswer::AllowAlways,
        "never" => PromptAnswer::DenyAlways,
        _ => PromptAnswer::DenyOnce,
    }
}

fn describe(cap: &Capability) -> (&'static str, String) {
    match cap {
        Capability::FsRead(p) => ("read", p.display().to_string()),
        Capability::FsWrite(p) => ("write", p.display().to_string()),
        Capability::Import(p) => ("import", p.display().to_string()),
        Capability::Net { host, port } => ("net", format!("{host}:{port}")),
        Capability::NetAddr { addr, .. } => ("net", addr.to_string()),
        Capability::NetListen(a) => ("listen", a.clone()),
        Capability::Env(n) => ("env", n.clone()),
        Capability::Stdin => ("stdin", String::new()),
        Capability::Stdout => ("stdout", String::new()),
        Capability::Stderr => ("stderr", String::new()),
        Capability::Custom { name, detail } => ("custom", format!("{name} {detail}")),
    }
}
