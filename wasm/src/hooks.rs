//! The JavaScript callbacks of an engine: output and permission prompts.
//!
//! The engine wants `Send` output sinks and prompters, and JavaScript values
//! are not `Send`. WebAssembly here is single threaded, so the callbacks
//! live in a thread-local table and the sinks and prompters hold their key.

use js_sys::Function;
use sandlang::policy::format_host_port;
use sandlang::{Capability, OutputSink, PromptAnswer, Prompter};
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
    pending: std::sync::Mutex<Vec<u8>>,
}

const DELIVERY_BYTES: usize = 8 << 10;

impl JsOutput {
    pub fn new(id: u32, stream: &'static str) -> JsOutput {
        JsOutput { id, stream, pending: Default::default() }
    }

    /// Delivers the pending text (keeping an unfinished UTF-8 sequence).
    fn deliver(&self, pending: &mut Vec<u8>) {
        let complete = match std::str::from_utf8(pending) {
            Ok(_) => pending.len(),
            // Invalid bytes are replaced; an incomplete sequence at the end waits.
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => pending.len(),
        };
        if complete == 0 {
            return;
        }
        let rest = pending.split_off(complete);
        let text = String::from_utf8_lossy(&std::mem::replace(pending, rest)).into_owned();
        if let Some(f) = callback(self.id, |h| h.on_output.as_ref()) {
            let _ = f.call2(&JsValue::NULL, &JsValue::from_str(self.stream), &JsValue::from_str(&text));
        }
    }
}

impl OutputSink for JsOutput {
    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.extend_from_slice(data);
        if self.stream == "stderr" || pending.len() >= DELIVERY_BYTES {
            self.deliver(&mut pending);
        }
        Ok(())
    }

    fn flush(&self) -> std::io::Result<()> {
        self.deliver(&mut self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(())
    }
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
            // A `PromptRequest`: the kind (`read`, `net`, ...), the target (path,
            // host, ...; empty for standard streams), the request in words, and
            // the policy rules that answer it from now on ("always" and "never").
            let (kind, target) = describe(&cap);
            let rule = |allow| sandlang::policy::rule_for(&cap, allow).map_or(JsValue::UNDEFINED, JsValue::from);
            let request = crate::object(&[
                ("kind", kind.into()),
                ("target", target.into()),
                ("description", cap.to_string().into()),
                ("allowRule", rule(true)),
                ("denyRule", rule(false)),
            ]);
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
        Capability::Net { host, port, .. } => ("net", format_host_port(host, *port)),
        Capability::Http { .. } => ("http", cap.to_string().split_once(' ').map_or_else(String::new, |(_, url)| url.to_string())),
        Capability::NetAddr { addr, .. } => ("net", addr.to_string()),
        Capability::NetListen(addr) => ("listen", addr.to_string()),
        Capability::Env(n) => ("env", n.clone()),
        Capability::Stdin => ("stdin", String::new()),
        Capability::TimeZone => ("timezone", String::new()),
        Capability::Stdout => ("stdout", String::new()),
        Capability::Stderr => ("stderr", String::new()),
        Capability::Custom { name, detail } => ("custom", format!("{name} {detail}")),
    }
}
