//! What the `http` clients (over TCP natively, over `fetch` on the web)
//! share: URLs (built on the `uri` module's parser), redirect resolution
//! and header syntax.

use super::stream::IoFailure;
use super::uri;
use crate::vm::iomem::Reservation;
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, Vm};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ----------------------------------------------------------------------
// URLs

/// An absolute `http` or `https` URL (parsed as a URI by `uri::parse`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Url {
    pub scheme: String,
    /// Lower-cased host, without brackets for IPv6 literals.
    pub host: String,
    pub port: u16,
    /// The path, without `.` and `..` segments; never empty.
    pub path: String,
    /// Path and query (the request target).
    pub target: String,
}

pub(crate) fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

impl Url {
    pub(crate) fn parse(text: &str) -> Result<Url, IoFailure> {
        let parts = uri::parse(text, true).map_err(|e| IoFailure::new("parse", e))?;
        Url::from_parts(text, parts)
    }

    fn from_parts(text: &str, parts: uri::Parts) -> Result<Url, IoFailure> {
        let fail = |why: &str| IoFailure::new("parse", format!("invalid URL {:?}: {why}", super::truncate(text, 200)));
        let scheme = parts.scheme.unwrap_or_default();
        let Some(default) = default_port(&scheme) else {
            return Err(IoFailure::new(
                "invalid",
                format!("unsupported URL scheme {:?} (expected http or https)", super::truncate(&scheme, 64)),
            ));
        };
        // The browser does TLS on the web.
        if scheme == "https" && !cfg!(any(feature = "tls", web)) {
            return Err(IoFailure::new("invalid", "https is not supported: TLS support is not enabled in this build"));
        }
        // Credentials in URLs are not supported; they are ignored.
        let Some(authority) = parts.authority.filter(|a| !a.host.is_empty()) else { return Err(fail("missing host")) };
        let port = match authority.port {
            Some(p) => u16::try_from(p).map_err(|_| fail("invalid port"))?,
            None => default,
        };
        let path = without_dots(if parts.path.is_empty() { "/" } else { &parts.path });
        let target = match parts.query {
            Some(q) => format!("{path}?{q}"),
            None => path.clone(),
        };
        Ok(Url { scheme, host: authority.host.to_ascii_lowercase(), port, path, target })
    }

    /// The capability to send a `method` request to this URL.
    pub(crate) fn capability(&self, method: &str) -> crate::policy::Capability {
        let (scheme, host, port, path) = (self.scheme.clone(), self.host.clone(), self.port, self.path.clone());
        crate::policy::Capability::Http { method: method.to_string(), scheme, host, port, path }
    }

    /// `host[:port]` as sent in the `Host` header (the port only if non-default).
    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if Some(self.port) == default_port(&self.scheme) { host } else { format!("{host}:{}", self.port) }
    }

    pub(crate) fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority())
    }
}

/// `path` without `.` and `..` segments, also where they are spelled with
/// `%2e` (as browsers resolve them): the path the policy checks is the one
/// servers see.
fn without_dots(path: &str) -> String {
    let dots = |seg: &str| {
        let s = seg.to_ascii_lowercase().replace("%2e", ".");
        matches!(s.as_str(), "." | "..").then_some(s)
    };
    let spelled: Vec<String> = path.split('/').map(|seg| dots(seg).unwrap_or_else(|| seg.to_string())).collect();
    let resolved = uri::remove_dot_segments(&spelled.join("/"));
    if resolved.starts_with('/') { resolved } else { format!("/{resolved}") }
}

/// Resolves a `Location` header value against `base` (RFC 3986 §5.2,
/// `uri::resolve`), without the fragment.
pub(crate) fn resolve(base: &str, location: &str) -> Result<String, IoFailure> {
    let parse = |text: &str, absolute: bool| uri::parse(text, absolute).map_err(|e| IoFailure::new("parse", e));
    let mut target = uri::resolve(&parse(base, true)?, parse(location.trim(), false)?);
    target.fragment = None;
    let text = target.to_string();
    Url::from_parts(&text, target)?;
    Ok(text)
}

/// Registers the natives every client shares.
pub(crate) fn register(r: &mut crate::engine::Registry) {
    register_body(r);
    r.add_builtin("http.resolve", |vm, a| {
        vm.charge_slow_bytes(vm.str(a[0]).len().saturating_add(vm.str(a[1]).len()))?;
        let url = resolve(vm.str(a[0]), vm.str(a[1]))?;
        vm.reserve(url.len())?;
        let s = vm.new_str(url);
        Ok(vm.ok(s))
    });
    r.add_builtin("http.same_origin", |vm, a| {
        vm.charge_slow_bytes(vm.str(a[0]).len().saturating_add(vm.str(a[1]).len()))?;
        let same = match (Url::parse(vm.str(a[0])), Url::parse(vm.str(a[1]))) {
            (Ok(x), Ok(y)) => x.origin() == y.origin(),
            _ => false,
        };
        Ok(crate::vm::value::Value::Bool(same))
    });
}

// ----------------------------------------------------------------------
// Message syntax

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub(crate) fn valid_header(name: &str, value: &str) -> bool {
    is_token(name) && !value.contains(['\r', '\n', '\0'])
}

/// Converts a `[(str, str)]` list into validated header pairs. Fails with
/// kind `"invalid"` for an invalid header.
pub(crate) fn header_pairs(vm: &Vm, list: Value) -> Result<Vec<(String, String)>, Ctl> {
    let items = vm.heap.list(list);
    vm.charge(items.len() as u64)?;
    let mut pairs = Vec::with_capacity(items.len());
    let mut size = 0usize;
    for &item in items {
        let [k, v] = vm.heap.fields(item) else { return Err(Ctl::internal("a header is not a pair")) };
        let (k, v) = (vm.str(*k), vm.str(*v));
        size = size.saturating_add(k.len() + v.len() + 64);
        vm.charge_bytes(k.len() + v.len())?;
        vm.reserve(size)?;
        if !valid_header(k, v) {
            return Err(Ctl::native_err("invalid", format!("invalid header {:?}: {:?}", super::truncate(k, 64), super::truncate(v, 64))));
        }
        pairs.push((k.to_string(), v.to_string()));
    }
    Ok(pairs)
}

/// Builds a `[(str, str)]` list from header pairs.
pub(crate) fn header_list(vm: &mut Vm, headers: Vec<(String, String)>) -> NResult {
    let size = headers.iter().map(|(k, v)| k.len() + v.len() + 160).fold(0usize, usize::saturating_add);
    vm.reserve(size)?;
    vm.charge(headers.len() as u64)?;
    let items = headers
        .into_iter()
        .map(|(k, v)| {
            let (k, v) = (vm.new_str(k), vm.new_str(v));
            vm.heap.new_tuple(&[k, v])
        })
        .collect();
    Ok(vm.heap.new_list(items))
}

// ----------------------------------------------------------------------
// Bodies

/// The bytes of a body that are available, once some are.
pub(crate) type Filled<'a> = Pin<Box<dyn Future<Output = Result<&'a [u8], IoFailure>> + Send + 'a>>;

/// The bytes of a message body as they arrive (any framing decoded).
pub(crate) trait BodyStream: Send {
    /// Waits for body bytes: the bytes available (empty at the end).
    fn fill(&mut self) -> Filled<'_>;
    /// Consumes `n` of the bytes `fill` returned.
    fn consume(&mut self, n: usize);
    /// Whether every byte of the body has been consumed.
    fn at_end(&self) -> bool;
    /// Whether the stream owns an open connection.
    fn owns_conn(&self) -> bool {
        false
    }
    /// Called after each read of the body: releases what the stream holds
    /// only while it is read (a connection's lock).
    fn release(&mut self) {}
    /// Called when the body ends, read to its end if `complete`: a
    /// connection the stream owns goes back to its pool, or is closed.
    fn end(&mut self, _complete: bool) {}
}

/// A body received whole (on the web, where the browser reads it).
pub(crate) struct MemoryBody {
    data: Vec<u8>,
    at: usize,
}

impl MemoryBody {
    pub fn new(data: Vec<u8>) -> MemoryBody {
        MemoryBody { data, at: 0 }
    }
}

impl BodyStream for MemoryBody {
    fn fill(&mut self) -> Filled<'_> {
        Box::pin(std::future::ready(Ok(&self.data[self.at..])))
    }
    fn consume(&mut self, n: usize) {
        self.at += n;
    }
    fn at_end(&self) -> bool {
        self.at == self.data.len()
    }
}

/// Reading a body: its stream, and what reading it holds.
pub(crate) struct BodyReader {
    /// `None` once the body has ended (read to its end, or failed).
    stream: Option<Box<dyn BodyStream>>,
    /// Whether the body was read to its end.
    complete: bool,
    /// The most `read_all` reads (`max_http_body`).
    max_all: usize,
    /// Memory reserved for the stream's buffers (a memory body's data),
    /// released with it.
    _held: Option<Reservation>,
}

impl BodyReader {
    pub fn new(stream: Box<dyn BodyStream>, max_all: usize, held: Option<Reservation>) -> BodyReader {
        BodyReader { stream: Some(stream), complete: false, max_all, _held: held }
    }

    /// An empty body.
    #[cfg_attr(not(feature = "os"), allow(dead_code))]
    pub fn empty(max_all: usize) -> BodyReader {
        BodyReader::new(Box::new(MemoryBody::new(Vec::new())), max_all, None)
    }

    /// Ends the body: the stream (and its connection) is released.
    fn end(&mut self, complete: bool) {
        if let Some(mut s) = self.stream.take() {
            s.end(complete);
            self.complete = complete;
        }
        self._held = None;
    }

    /// The bytes available (waiting for some); empty at the end.
    async fn fill(&mut self) -> Result<usize, IoFailure> {
        let Some(stream) = &mut self.stream else { return Ok(0) };
        match stream.fill().await {
            Ok([]) => {
                self.end(true);
                Ok(0)
            }
            Ok(b) => Ok(b.len()),
            Err(e) => {
                self.end(false);
                Err(e)
            }
        }
    }

    /// The bytes `fill` made available, consumed with `consume`.
    fn available(&mut self) -> Filled<'_> {
        match &mut self.stream {
            Some(s) => s.fill(),
            None => Box::pin(std::future::ready(Ok(&[][..]))),
        }
    }

    fn consume(&mut self, n: usize) {
        if let Some(s) = &mut self.stream {
            s.consume(n);
        }
    }

    /// Runs `op`, a read of the body. A body that fails is ended (its
    /// connection closed), unless the data read did not fit in memory
    /// (where `read` consumed nothing; `read_line` drops the partial line).
    /// A body whose last byte was read ends at once, releasing its
    /// connection.
    async fn reading<T>(&mut self, op: impl AsyncFnOnce(&mut Self) -> Result<T, IoFailure>) -> Result<T, IoFailure> {
        let r = op(self).await;
        match (&r, &mut self.stream) {
            (Err(e), _) if e.kind != "limit" => self.end(false),
            (_, Some(s)) if s.at_end() => self.end(true),
            (_, Some(s)) => s.release(),
            (_, None) => {}
        }
        r
    }

    /// Up to `max` bytes, once some are available; empty at the end.
    async fn read(&mut self, max: usize, res: &mut Reservation) -> Result<Vec<u8>, IoFailure> {
        let n = self.fill().await?.min(max);
        res.ensure(n)?;
        let data = self.available().await?[..n].to_vec();
        self.consume(n);
        Ok(data)
    }

    /// A line, without its `\n` or `\r\n`; `None` at the end.
    async fn read_line(&mut self, res: &mut Reservation) -> Result<Option<Vec<u8>>, IoFailure> {
        let mut line = Vec::new();
        loop {
            if self.fill().await? == 0 {
                return Ok((!line.is_empty()).then_some(line));
            }
            let buf = self.available().await?;
            let (take, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            res.reserve_buf(&mut line, take)?;
            line.extend_from_slice(&buf[..take]);
            self.consume(take);
            if done {
                super::stream::strip_eol(&mut line);
                return Ok(Some(line));
            }
        }
    }

    /// The rest of the body (at most `max_all` bytes).
    async fn read_all(&mut self, res: &mut Reservation) -> Result<Vec<u8>, IoFailure> {
        let mut data = Vec::new();
        loop {
            let n = self.fill().await?;
            if n == 0 {
                return Ok(data);
            }
            if data.len().saturating_add(n) > self.max_all {
                self.end(false);
                return Err(IoFailure::new("limit", format!("HTTP body larger than the limit of {} bytes", self.max_all)));
            }
            res.reserve_buf(&mut data, n)?;
            data.extend_from_slice(&self.available().await?[..n]);
            self.consume(n);
        }
    }
}

/// The `http.Body` type: a message body read as it arrives. A body that
/// owns its connection (a client's response) counts as an open resource
/// until it is closed.
pub(crate) struct Body {
    reader: Option<Arc<tokio::sync::Mutex<BodyReader>>>,
    closed: tokio::sync::watch::Sender<bool>,
    owns_conn: bool,
}

impl Body {
    pub fn new(reader: BodyReader) -> Body {
        let owns_conn = reader.stream.as_ref().is_some_and(|s| s.owns_conn());
        Body { reader: Some(Arc::new(tokio::sync::Mutex::new(reader))), closed: tokio::sync::watch::Sender::new(false), owns_conn }
    }

    /// Whether the body has been read to its end (false while a read is
    /// pending).
    #[cfg_attr(not(feature = "os"), allow(dead_code))]
    pub fn is_complete(&self) -> bool {
        let Some(reader) = &self.reader else { return false };
        reader.try_lock().is_ok_and(|r| r.complete || r.stream.as_ref().is_some_and(|s| s.at_end()))
    }
}

impl NativeObject for Body {
    fn type_name(&self) -> &'static str {
        "Body"
    }
    fn is_resource(&self) -> bool {
        self.owns_conn && self.reader.is_some()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// What a read of a body reads.
#[derive(Clone, Copy)]
enum BodyRead {
    Upto(usize),
    Line,
    All,
}

/// Reads from body `v` until the read completes or the body is closed.
fn read_body(vm: &mut Vm, v: Value, what: BodyRead) -> NResult {
    let body = vm.heap.native::<Body>(v).ok_or_else(|| Ctl::internal("not a body"))?;
    let Some(reader) = body.reader.clone() else { return Err(Ctl::native_err("closed", "body closed")) };
    let mut closed = body.closed.subscribe();
    let mut res = match what {
        BodyRead::Upto(n) => vm.reserve_io(n.min(64 << 10))?,
        _ => vm.io_reservation(),
    };
    let op = async move {
        if *closed.borrow() {
            return Err(IoFailure::new("closed", "body closed"));
        }
        let read = async {
            let mut r = reader.lock().await;
            let res = &mut res;
            r.reading(async move |r: &mut BodyReader| match what {
                BodyRead::Upto(n) => r.read(n, res).await.map(Some),
                BodyRead::Line => r.read_line(res).await,
                BodyRead::All => r.read_all(res).await.map(Some),
            })
            .await
        };
        let data = {
            let mut read = std::pin::pin!(read);
            let mut wait = std::pin::pin!(closed.wait_for(|c| *c));
            std::future::poll_fn(|cx| {
                if let std::task::Poll::Ready(r) = read.as_mut().poll(cx) {
                    return std::task::Poll::Ready(r);
                }
                match wait.as_mut().poll(cx) {
                    std::task::Poll::Ready(_) => std::task::Poll::Ready(Err(IoFailure::new("closed", "body closed"))),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            })
            .await?
        };
        Ok((data, res))
    };
    Err(Ctl::suspend(Box::pin(async move {
        let r = op.await;
        Box::new(move |vm: &mut Vm| -> NResult {
            let (data, res) = r?;
            drop(res);
            let v = match (what, data) {
                (BodyRead::Line, None) => vm.none(),
                (BodyRead::Line, Some(line)) => {
                    vm.reserve(line.len())?;
                    vm.charge_slow_bytes(line.len())?;
                    let Ok(text) = String::from_utf8(line) else { return Err(Ctl::native_err("invalid", "line is not valid UTF-8")) };
                    let s = vm.new_str(text);
                    vm.some(s)
                }
                (_, data) => {
                    let data = data.unwrap_or_default();
                    vm.reserve(data.len())?;
                    vm.charge_bytes(data.len())?;
                    vm.heap.new_bytes(data)
                }
            };
            Ok(vm.ok(v))
        }) as Finisher
    })))
}

pub(crate) fn alloc_body(vm: &mut Vm, reader: BodyReader) -> Value {
    vm.heap.alloc(Obj::Native(Box::new(Body::new(reader))))
}

pub(crate) fn register_body(r: &mut crate::engine::Registry) {
    r.add_method("http.Body.read", |vm, a| {
        let max = a[1].as_int();
        if max <= 0 {
            return Err(Ctl::native_err("invalid", "read: max must be positive"));
        }
        read_body(vm, a[0], BodyRead::Upto(crate::vm::value::to_usize_saturating(max)))
    });
    r.add_method("http.Body.read_line", |vm, a| read_body(vm, a[0], BodyRead::Line));
    r.add_method("http.Body.read_all", |vm, a| read_body(vm, a[0], BodyRead::All));
    r.add_method("http.Body.close", |vm, a| {
        let b = vm.heap.native_mut::<Body>(a[0]).ok_or_else(|| Ctl::internal("not a body"))?;
        b.closed.send_replace(true);
        // The stream and the connection go with the last reference to the
        // reader: now, or when a pending read ends.
        b.reader = None;
        Ok(Value::Unit)
    });
}

pub(crate) fn filter_request(vm: &Vm, url: &Url, headers: &mut Vec<(String, String)>) -> Result<(), String> {
    let Some(filter) = &vm.host.request_filter else { return Ok(()) };
    let target = crate::engine::RequestTarget { scheme: url.scheme.clone(), host: url.host.clone(), port: url.port };
    filter(&target, headers)
}
