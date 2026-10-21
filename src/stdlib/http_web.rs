//! Browser `fetch` implementation of the `http` module.
//!
//! Policy rules and the host request filter are evaluated before a request is
//! sent. Redirects are denied (`redirect: "manual"`) because the browser hides
//! redirect targets from policy checks. Response bodies are streamed into
//! memory-tracked buffers and aborted if they exceed `max_http_body`.
//!
//! Because JavaScript `fetch` objects are not `Send`, each request runs on the
//! local JavaScript event loop and sends its result back over a channel.
//! Dropping the receiver aborts the in-flight request.

use super::http_common::{BodyReader, MemoryBody, Url, alloc_body, filter_request, header_list, header_pairs, is_token};
use super::stream::IoFailure;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::iomem::Reservation;
use crate::vm::value::Value;
use crate::vm::{Ctl, Finisher, NResult, Vm};
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

pub fn register(r: &mut Registry) {
    r.source("http", ModKind::Std, concat!(include_str!("http_web.sand"), include_str!("http.sand")));
    super::http_common::register(r);
    r.add_builtin("http.send", send);
}

/// A request to send.
struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    max_body: usize,
}

/// A response: status, headers and body (reserved in the reservation).
type Response = (i64, Vec<(String, String)>, Vec<u8>, Reservation);

fn send(vm: &mut Vm, a: &[Value]) -> NResult {
    let method = vm.str(a[0]).to_string();
    let text = vm.str(a[1]).to_string();
    vm.charge_slow_bytes(method.len() + text.len())?;
    let url = match Url::parse(&text) {
        Ok(u) => u,
        Err(e) => return Err(Ctl::native_err(e.kind, e.message)),
    };
    if !is_token(&method) {
        return Err(Ctl::native_err("invalid", format!("invalid HTTP method {:?}", super::truncate(&method, 64))));
    }
    let mut headers = header_pairs(vm, a[2])?;
    let cap = url.capability(&method);
    if !vm.permit(&cap)? {
        return Err(Ctl::native_err("permission_denied", format!("permission denied: {cap}")));
    }
    if let Err(reason) = filter_request(vm, &url, &mut headers) {
        return Err(Ctl::native_err("permission_denied", reason));
    }
    // The request body is held, and the response buffered, outside the heap.
    let res = vm.reserve_io(vm.heap.bytes_of(a[3]).len())?;
    let body = vm.heap.bytes_of(a[3]).to_vec();
    vm.charge_bytes(body.len())?;
    let request = Request {
        method,
        url: format!("{}{}", url.origin(), url.target),
        headers,
        body,
        timeout: Duration::from_nanos(a[4].as_int().max(0) as u64),
        max_body: vm.host.limits.max_http_body,
    };
    let max_body = request.max_body;
    let (id, done) = start(request, res);
    Err(Ctl::suspend(Box::pin(async move {
        let _abort = AbortOnDrop(id);
        let result = done.await.unwrap_or_else(|_| Err(IoFailure::new("io", "the request was abandoned")));
        Box::new(move |vm: &mut Vm| -> NResult {
            let (status, headers, body, res) = result?;
            let headers = header_list(vm, headers)?;
            // The browser read the body: it stays reserved while it is read.
            let reader = BodyReader::new(Box::new(MemoryBody::new(body)), max_body, Some(res));
            let body = alloc_body(vm, reader);
            let tuple = vm.heap.new_tuple(&[Value::Int(status), headers, body]);
            Ok(vm.ok(tuple))
        }) as Finisher
    })))
}

thread_local! {
    /// The `AbortController`s of requests in flight, by id.
    static IN_FLIGHT: RefCell<HashMap<u64, JsValue>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Aborts request `.0` (if still in flight) when dropped.
struct AbortOnDrop(u64);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        abort(self.0);
    }
}

fn abort(id: u64) {
    if let Some(controller) = IN_FLIGHT.with(|m| m.borrow_mut().remove(&id)) {
        let _ = call(&controller, "abort", &[]);
    }
}

/// Starts `request` in a local task; the receiver gets its outcome.
fn start(request: Request, res: Reservation) -> (u64, tokio::sync::oneshot::Receiver<Result<Response, IoFailure>>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = NEXT_ID.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    wasm_bindgen_futures::spawn_local(async move {
        let result = exchange(id, &request, res).await;
        IN_FLIGHT.with(|m| m.borrow_mut().remove(&id));
        let _ = tx.send(result);
    });
    (id, rx)
}

async fn exchange(id: u64, request: &Request, mut res: Reservation) -> Result<Response, IoFailure> {
    let url = &request.url;
    let failed = |e: JsValue| IoFailure::new("io", format!("request to {url} failed: {}", describe(&e)));
    let global = js_sys::global();
    let controller = construct(&global, "AbortController").map_err(failed)?;
    IN_FLIGHT.with(|m| m.borrow_mut().insert(id, controller.clone()));
    // The timeout aborts the request (`timed_out` tells it apart).
    let timed_out = Rc::new(Cell::new(false));
    {
        let (timed_out, controller) = (timed_out.clone(), controller.clone());
        let fire = Closure::once_into_js(move || {
            timed_out.set(true);
            let _ = call(&controller, "abort", &[]);
        });
        let ms = (request.timeout.as_secs_f64() * 1000.0).min(2_147_483_647.0);
        let _ = call(&global, "setTimeout", &[fire, JsValue::from_f64(ms)]);
    }
    let failed = |e: JsValue| {
        if timed_out.get() { IoFailure::new("timeout", format!("request to {url} timed out")) } else { failed(e) }
    };
    let init = Object::new();
    let headers = Object::new();
    for (name, value) in &request.headers {
        set(&headers, name, &JsValue::from_str(value));
    }
    set(&init, "method", &JsValue::from_str(&request.method));
    set(&init, "headers", &headers);
    if !request.body.is_empty() {
        set(&init, "body", &Uint8Array::from(request.body.as_slice()));
    }
    set(&init, "redirect", &JsValue::from_str("manual"));
    set(&init, "credentials", &JsValue::from_str("omit"));
    set(&init, "cache", &JsValue::from_str("no-store"));
    set(&init, "signal", &Reflect::get(&controller, &"signal".into()).unwrap_or(JsValue::UNDEFINED));
    let promise = call(&global, "fetch", &[JsValue::from_str(url), init.into()]).map_err(failed)?;
    // The browser does not say why (its message is "Failed to fetch" or
    // "Load failed"): usually CORS, the site not letting pages of other
    // sites read its answers.
    let response = JsFuture::from(Promise::from(promise)).await.map_err(|e| {
        let f = failed(e);
        if f.kind != "io" {
            return f;
        }
        let host = url.split("//").nth(1).and_then(|r| r.split(['/', '?']).next()).unwrap_or(url);
        IoFailure::new(
            "io",
            format!(
                "the browser blocked the request to {url}: {host} does not let pages of other sites read its answers (CORS), or it cannot be reached"
            ),
        )
    })?;
    let status = get(&response, "status").as_f64().unwrap_or(0.0) as i64;
    // Browsers hide manual redirects (an "opaqueredirect" response); other
    // hosts (Node.js) show them. Either way they are not followed.
    if get(&response, "type").as_string().as_deref() == Some("opaqueredirect") || [301, 302, 303, 307, 308].contains(&status) {
        return Err(IoFailure::new("http", format!("{url} redirects elsewhere, and redirects are not followed in the browser")));
    }
    let mut headers = Vec::new();
    let entries = call(&get(&response, "headers"), "entries", &[]).map_err(failed)?;
    if let Ok(Some(iter)) = js_sys::try_iter(&entries) {
        for entry in iter.flatten() {
            let pair = Array::from(&entry);
            headers.push((pair.get(0).as_string().unwrap_or_default().to_ascii_lowercase(), pair.get(1).as_string().unwrap_or_default()));
            res.grow(64).map_err(IoFailure::from)?;
        }
    }
    let mut data = Vec::new();
    let body = get(&response, "body");
    if !body.is_null() && !body.is_undefined() {
        let reader = call(&body, "getReader", &[]).map_err(failed)?;
        loop {
            let chunk = JsFuture::from(Promise::from(call(&reader, "read", &[]).map_err(failed)?)).await.map_err(failed)?;
            if get(&chunk, "done").as_bool().unwrap_or(true) {
                break;
            }
            let bytes = Uint8Array::from(get(&chunk, "value"));
            let n = bytes.length() as usize;
            if data.len().saturating_add(n) > request.max_body {
                abort(id);
                return Err(IoFailure::new("limit", format!("response body larger than {} bytes (max_http_body)", request.max_body)));
            }
            res.reserve_buf(&mut data, n).map_err(IoFailure::from)?;
            let start = data.len();
            data.resize(start + n, 0);
            bytes.copy_to(&mut data[start..]);
        }
    }
    Ok((status, headers, data, res))
}

fn get(obj: &JsValue, key: &str) -> JsValue {
    Reflect::get(obj, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn set(obj: &JsValue, key: &str, value: &JsValue) {
    let _ = Reflect::set(obj, &JsValue::from_str(key), value);
}

/// Calls the method `name` of `obj`.
fn call(obj: &JsValue, name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let f: Function = get(obj, name).dyn_into().map_err(|_| JsValue::from_str(&format!("no function `{name}`")))?;
    f.apply(obj, &args.iter().collect::<Array>())
}

/// `new global[name]()`.
fn construct(global: &JsValue, name: &str) -> Result<JsValue, JsValue> {
    let ctor: Function = get(global, name).dyn_into().map_err(|_| JsValue::from_str(&format!("no `{name}`")))?;
    Reflect::construct(&ctor, &Array::new())
}

/// The message of a JavaScript error.
fn describe(e: &JsValue) -> String {
    get(e, "message").as_string().or_else(|| e.as_string()).unwrap_or_else(|| "unknown error".into())
}
