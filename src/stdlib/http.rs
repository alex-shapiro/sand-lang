//! The `http` module: an HTTP/1.1 client and server written directly on top
//! of tokio TCP streams (and rustls for `https`).
//!
//! Redirects are followed in Sand (`http.sand`) so that every hop goes
//! through `send`, which checks the policy before connecting.

use super::http_common::*;
use super::net::{self, TlsSettings, check_permission, finish, suspend};
use super::stream::{IoFailure, read_line_bytes, read_to_end_reserved, strip_eol};
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::{Capability, Policy};
use crate::vm::iomem::{IoMemory, Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value, u64_to_usize_saturating};
use crate::vm::{Ctl, NResult, Vm};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Maximum size of a request or response head (start line and headers).
const MAX_HEAD: usize = 64 << 10;
/// How long a server connection may take to deliver its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("sand/", env!("CARGO_PKG_VERSION"));

pub fn register(r: &mut Registry) {
    r.source("http", ModKind::Std, include_str!("http.sand"));
    r.add_builtin("http.send", send);
    r.add_builtin("http.resolve", |vm, a| {
        let resolved = resolve(vm.str(a[0]), vm.str(a[1]));
        Ok(match resolved {
            Ok(url) => {
                let s = vm.new_str(url);
                vm.ok(s)
            }
            Err(e) => vm.err_value(e.kind, &e.message),
        })
    });
    r.add_builtin("http.same_origin", |vm, a| {
        let same = match (Url::parse(vm.str(a[0])), Url::parse(vm.str(a[1]))) {
            (Ok(x), Ok(y)) => x.origin() == y.origin(),
            _ => false,
        };
        Ok(Value::Bool(same))
    });
    r.add_builtin("http.new_pending", |vm, _| {
        let pending = PendingRequests { shared: Arc::new(PendingShared::default()) };
        Ok(vm.heap.alloc(Obj::Native(Box::new(pending))))
    });
    r.add_builtin("http.next_request", next_request);
    r.add_builtin("http.close_pending", |vm, a| {
        let p = vm.heap.native_mut::<PendingRequests>(a[0]).ok_or_else(|| Ctl::Panic("not a request queue".into()))?;
        // The connections are dropped (closed) with the last reference to
        // the old state: now, or when a pending `next_request` ends.
        p.shared = Arc::new(PendingShared::default());
        Ok(Value::Unit)
    });
    r.add_builtin("http.encode_response", encode_response);
}

/// Reads `name: value` header lines up to the blank line ending the head,
/// after the start line. Names are lower-cased; repeated headers are joined
/// with `", "`. The lines are reserved in `res`.
async fn read_head<R: AsyncBufRead + Unpin>(r: &mut R, res: &mut Reservation) -> Result<(String, Vec<(String, String)>), IoFailure> {
    let mut budget = MAX_HEAD;
    let mut next_line = async |budget: &mut usize, res: &mut Reservation| -> Result<Option<String>, IoFailure> {
        let line = read_line_bytes(&mut *r, *budget, res).await.map_err(|e| match e.kind {
            "limit" => IoFailure::new("http", "message head too large"),
            _ => e,
        })?;
        let Some(mut line) = line else { return Ok(None) };
        *budget -= line.len();
        strip_eol(&mut line);
        String::from_utf8(line).map(Some).map_err(|_| IoFailure::new("http", "message head is not valid UTF-8"))
    };
    // Tolerate blank lines before the start line (RFC 9112 §2.2).
    let start = loop {
        match next_line(&mut budget, res).await? {
            None => return Err(net::closed()),
            Some(l) if l.is_empty() => continue,
            Some(l) => break l,
        }
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let Some(line) = next_line(&mut budget, res).await? else {
            return Err(IoFailure::new("http", "connection closed in the middle of the message head"));
        };
        if line.is_empty() {
            return Ok((start, headers));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(IoFailure::new("http", format!("malformed header line {line:?}")));
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        match headers.iter_mut().find(|(k, _)| *k == name) {
            Some((_, v)) => {
                v.push_str(", ");
                v.push_str(value);
            }
            None => headers.push((name, value.to_string())),
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn too_large(max: usize) -> IoFailure {
    IoFailure::new("limit", format!("HTTP body larger than the limit of {max} bytes"))
}

/// Reads a message body framed by `headers`. Without `Content-Length` or
/// chunked encoding the body extends to the end of the stream if
/// `read_to_close`, otherwise it is empty. The body is reserved in `res`.
async fn read_body<R: AsyncBufRead + Unpin>(
    r: &mut R,
    headers: &[(String, String)],
    max: usize,
    read_to_close: bool,
    res: &mut Reservation,
) -> Result<Vec<u8>, IoFailure> {
    let chunked = header(headers, "transfer-encoding")
        .is_some_and(|te| te.rsplit(',').next().is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked")));
    if chunked {
        return read_chunked(r, max, res).await;
    }
    if let Some(len) = header(headers, "content-length") {
        // Repeated identical values were joined with ", " by `read_head`.
        // Parsed as `u64` so that a length past `usize::MAX` (32-bit targets) is too large, not invalid.
        let mut values = len.split(',').map(|v| v.trim().parse::<u64>());
        let first = values.next().and_then(|v| v.ok());
        let Some(len) = first.filter(|&n| values.all(|v| v.ok() == Some(n))) else {
            return Err(IoFailure::new("http", format!("invalid content-length {len:?}")));
        };
        let len = u64_to_usize_saturating(len);
        if len > max {
            return Err(too_large(max));
        }
        // Reserved as it arrives: the peer's length alone does not claim memory.
        let mut body = Vec::new();
        read_to_end_reserved(&mut (&mut *r).take(len as u64), &mut body, res, len).await?;
        if body.len() < len {
            return Err(IoFailure::new("io", "connection closed in the middle of the message body"));
        }
        return Ok(body);
    }
    let mut body = Vec::new();
    if read_to_close {
        let cap = max.saturating_add(1);
        read_to_end_reserved(&mut r.take(u64::try_from(cap).unwrap_or(u64::MAX)), &mut body, res, cap).await?;
        if body.len() > max {
            return Err(too_large(max));
        }
    }
    Ok(body)
}

async fn read_chunked<R: AsyncBufRead + Unpin>(r: &mut R, max: usize, res: &mut Reservation) -> Result<Vec<u8>, IoFailure> {
    let truncated = || IoFailure::new("http", "connection closed in the middle of a chunked body");
    let mut body = Vec::new();
    loop {
        // Size and trailer lines are released as soon as they are parsed.
        let mut line = read_line_bytes(&mut *r, 1024, &mut res.sibling()).await?.ok_or_else(truncated)?;
        strip_eol(&mut line);
        let text = String::from_utf8_lossy(&line);
        let size = text.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size, 16).map_err(|_| IoFailure::new("http", format!("invalid chunk size {size:?}")))?;
        let size = u64_to_usize_saturating(size);
        if size == 0 {
            // Skip trailer fields up to the final blank line.
            while let Some(mut line) = read_line_bytes(&mut *r, MAX_HEAD, &mut res.sibling()).await? {
                strip_eol(&mut line);
                if line.is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        if body.len().saturating_add(size) > max {
            return Err(too_large(max));
        }
        let start = body.len();
        read_to_end_reserved(&mut (&mut *r).take(size as u64), &mut body, res, start + size).await?;
        if body.len() < start + size {
            return Err(truncated());
        }
        let mut end = read_line_bytes(&mut *r, 2, &mut res.sibling()).await?.ok_or_else(truncated)?;
        strip_eol(&mut end);
        if !end.is_empty() {
            return Err(IoFailure::new("http", "missing CRLF after a chunk"));
        }
    }
}

// ----------------------------------------------------------------------
// Client

struct RawResponse {
    status: i64,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn send(vm: &mut Vm, a: &[Value]) -> NResult {
    let method = vm.str(a[0]).to_string();
    let text = vm.str(a[1]).to_string();
    let url = match Url::parse(&text) {
        Ok(u) => u,
        Err(e) => return Ok(vm.err_value(e.kind, &e.message)),
    };
    if !is_token(&method) {
        return Ok(vm.err_value("invalid", &format!("invalid HTTP method {:?}", super::truncate(&method, 64))));
    }
    let headers = match header_pairs(vm, a[2]) {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    if let Some(denied) = check_permission(vm, Capability::Net { host: url.host.clone(), port: url.port })? {
        return Ok(denied);
    }
    // The request is held, and the response buffered, outside the heap.
    let mut res = match vm.reserve_io(vm.heap.bytes_of(a[3]).len()) {
        Ok(r) => r,
        Err(e) => return Ok(e),
    };
    let body = vm.heap.bytes_of(a[3]);
    let request = encode_request(&method, &url, &headers, body);
    if let Err(e) = res.ensure(request.len()) {
        return Ok(vm.err_value("limit", &format!("cannot buffer the request: {e}")));
    }
    // Request socket counts as an open resource while it is in use.
    // Request buffers count against the memory limit.
    let buffers = if url.scheme == "https" { net::CONN_BUFFER + net::TLS_BUFFERS } else { net::CONN_BUFFER };
    let slot = match net::conn_slot(vm, buffers) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let timeout = Duration::from_nanos(a[4].as_int().max(0) as u64);
    let head_only = method.eq_ignore_ascii_case("HEAD");
    let max_body = vm.host.limits.max_http_body;
    let policy = vm.host.policy.clone();
    let tls = vm.host.tls.clone();
    suspend(
        async move {
            let mut response_res = res.sibling();
            let exchange = fetch(&policy, &tls, &url, &request, head_only, max_body, &mut response_res);
            let r = match tokio::time::timeout(timeout, exchange).await {
                Ok(r) => r.map(|resp| (resp, response_res)),
                Err(_) => Err(IoFailure::new("timeout", format!("request to {text} timed out"))),
            };
            // The socket was closed when the exchange ended.
            drop(slot);
            r
        },
        |vm, (resp, res): (RawResponse, Reservation)| {
            drop(res);
            vm.reserve(resp.body.len())?;
            let headers = header_map(vm, resp.headers)?;
            let body = vm.heap.new_bytes(resp.body);
            Ok(vm.heap.new_tuple(vec![Value::Int(resp.status), headers, body]))
        },
    )
}

fn encode_request(method: &str, url: &Url, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut head = format!("{method} {} HTTP/1.1\r\n", url.target);
    if !has("host") {
        head += &format!("Host: {}\r\n", url.authority());
    }
    for (k, v) in headers {
        // Framing and connection management are ours.
        if !["content-length", "transfer-encoding", "connection"].iter().any(|h| k.eq_ignore_ascii_case(h)) {
            head += &format!("{k}: {v}\r\n");
        }
    }
    if !has("user-agent") {
        head += &format!("User-Agent: {USER_AGENT}\r\n");
    }
    if !body.is_empty() || ["POST", "PUT", "PATCH"].iter().any(|m| method.eq_ignore_ascii_case(m)) {
        head += &format!("Content-Length: {}\r\n", body.len());
    }
    head += "Connection: close\r\n\r\n";
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Connects to `url` (at an address the policy allows) and performs one exchange.
async fn fetch(
    policy: &Policy,
    tls: &TlsSettings,
    url: &Url,
    request: &[u8],
    head_only: bool,
    max_body: usize,
    res: &mut Reservation,
) -> Result<RawResponse, IoFailure> {
    // The overall timeout is applied by the caller.
    let tcp = net::connect_checked(policy, &url.host, url.port).await?;
    if url.scheme == "https" {
        exchange(net::tls::connect(tls, &url.host, tcp).await?, request, head_only, max_body, res).await
    } else {
        exchange(tcp, request, head_only, max_body, res).await
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    request: &[u8],
    head_only: bool,
    max_body: usize,
    res: &mut Reservation,
) -> Result<RawResponse, IoFailure> {
    let mut stream = BufReader::with_capacity(net::CONN_BUFFER, stream);
    stream.write_all(request).await?;
    stream.flush().await?;
    loop {
        // Interim responses are discarded with their reservation.
        let mut head_res = res.sibling();
        let (start, headers) = read_head(&mut stream, &mut head_res).await.map_err(|e| match e.kind {
            "closed" => IoFailure::new("io", "connection closed before a response was received"),
            _ => e,
        })?;
        let mut parts = start.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let status = parts.next().and_then(|s| s.parse::<i64>().ok()).filter(|s| (100..=999).contains(s));
        let Some(status) = status.filter(|_| version.starts_with("HTTP/")) else {
            return Err(IoFailure::new("http", format!("malformed status line {start:?}")));
        };
        // Skip interim responses such as `100 Continue`.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        let no_body = head_only || status < 200 || status == 204 || status == 304;
        let body = if no_body { vec![] } else { read_body(&mut stream, &headers, max_body, true, res).await? };
        res.absorb(head_res);
        return Ok(RawResponse { status, headers, body });
    }
}

// ----------------------------------------------------------------------
// Server

/// Connections whose requests one server reads at the same time.
const MAX_READING: usize = 64;

/// A request as read by the server: method, path, query, headers, body.
type RawRequest = (String, String, String, Vec<(String, String)>, Vec<u8>);

/// A request read completely, with its connection.
struct Received {
    request: RawRequest,
    conn: net::Conn,
    /// The request's buffers, until they become values.
    res: Reservation,
    /// Counts the connection until it becomes a `net.Conn` object.
    slot: ResourceSlot,
}

/// Reading the request of one accepted connection. If it does not deliver a
/// valid request, it is answered with 400 where possible and closed, and
/// its slot is returned for the next connection.
type Reading = Pin<Box<dyn Future<Output = Result<Received, ResourceSlot>> + Send>>;

#[derive(Default)]
struct PendingShared {
    /// Locked by the `next_request` call that drives the readings.
    reads: tokio::sync::Mutex<Vec<Reading>>,
    /// `reads.len()`, readable without the lock.
    count: AtomicUsize,
}

/// The `http.PendingRequests` type: connections accepted by a server whose
/// requests are still being read. Their sockets are counted against
/// `max_open_resources` by their [`ResourceSlot`]s.
struct PendingRequests {
    shared: Arc<PendingShared>,
}

impl NativeObject for PendingRequests {
    fn type_name(&self) -> &'static str {
        "PendingRequests"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `next_request(listener, pending)`: accepts connections and reads their
/// requests concurrently (each within `REQUEST_TIMEOUT` of being accepted)
/// until one is complete. Readings still in progress when it returns (or
/// when the calling task is cancelled) stay in `pending` and continue with
/// the next call.
fn next_request(vm: &mut Vm, a: &[Value]) -> NResult {
    let Some((socket, closed)) = net::listener_socket(vm, a[0])? else { return Ok(vm.err_value("closed", "listener closed")) };
    let shared = match vm.heap.native::<PendingRequests>(a[1]) {
        Some(p) => p.shared.clone(),
        None => return Err(Ctl::Panic("not a request queue".into())),
    };
    // Slots for the connections this call may accept: up to MAX_READING
    // read at once, and at most half of what the open-resource limit has
    // left, so that a waiting server does not starve other tasks' sockets.
    let reading = shared.count.load(Ordering::Relaxed);
    let max = vm.host.limits.max_open_resources;
    let available = max.saturating_sub(vm.heap.open_resources.saturating_add(vm.heap.io_memory.pending_resources()));
    let grant = MAX_READING.saturating_sub(reading).min(available.div_ceil(2));
    if grant == 0 && reading == 0 {
        return Ok(vm.err_value("limit", &format!("too many open resources (limit {max})")));
    }
    let mem = vm.heap.io_memory.clone();
    let mut slots: Vec<ResourceSlot> = (0..grant).map(|_| ResourceSlot::new(&mem)).collect();
    let max_body = vm.host.limits.max_http_body;
    let serve = async move {
        let mut reads = shared.reads.lock().await;
        std::future::poll_fn(|cx| {
            loop {
                let mut progress = false;
                if !slots.is_empty() && reads.len() < MAX_READING {
                    match socket.poll_accept(cx) {
                        Poll::Ready(Ok((stream, _))) => {
                            let slot = slots.pop().expect("a free slot");
                            reads.push(Box::pin(read_connection(stream, max_body, mem.clone(), slot)));
                            progress = true;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(IoFailure::from(e))),
                        Poll::Pending => {}
                    }
                }
                let mut i = 0;
                while i < reads.len() {
                    match reads[i].as_mut().poll(cx) {
                        Poll::Ready(done) => {
                            drop(reads.swap_remove(i));
                            shared.count.store(reads.len(), Ordering::Relaxed);
                            match done {
                                Ok(r) => return Poll::Ready(Ok(r)),
                                Err(slot) => slots.push(slot),
                            }
                            progress = true;
                        }
                        Poll::Pending => i += 1,
                    }
                }
                shared.count.store(reads.len(), Ordering::Relaxed);
                if !progress {
                    return Poll::Pending;
                }
            }
        })
        .await
    };
    Err(Ctl::Async(Box::pin(async move {
        finish(closed.guard(serve).await, |vm, received: Received| {
            let Received { request: (method, path, query, headers, body), conn, res, slot } = received;
            drop(res);
            vm.reserve(body.len())?;
            let fields = [method, path, query].map(|s| vm.new_str(s));
            let headers = header_map(vm, headers)?;
            let body = vm.heap.new_bytes(body);
            let conn = net::alloc_resource(vm, Box::new(conn), slot);
            Ok(vm.heap.new_tuple(vec![fields[0], fields[1], fields[2], headers, body, conn]))
        })
    })))
}

/// The fixed buffers of a server connection: the buffer its request is
/// read with, then also the `net.Conn`'s own (the first may still hold data
/// that followed the request).
const SERVER_CONN_BUFFERS: usize = 2 * net::CONN_BUFFER;

/// Reads the request of an accepted connection (see [`Reading`]).
async fn read_connection(stream: TcpStream, max_body: usize, mem: Arc<IoMemory>, mut slot: ResourceSlot) -> Result<Received, ResourceSlot> {
    let Ok(peer) = stream.peer_addr().map(|a| a.to_string()) else { return Err(slot) };
    let (r, mut w) = stream.into_split();
    if slot.reserve_buffers(SERVER_CONN_BUFFERS).is_err() {
        // The connection's buffers do not fit in the memory limit
        let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = tokio::time::timeout(Duration::from_secs(5), w.write_all(response.as_bytes())).await;
        return Err(slot);
    }
    let mut reader = BufReader::with_capacity(net::CONN_BUFFER, r);
    let mut res = Reservation::new(&mem);
    let read = read_request(&mut reader, &mut w, max_body, &mut res);
    let result = match tokio::time::timeout(REQUEST_TIMEOUT, read).await {
        Ok(r) => r,
        Err(_) => Err(IoFailure::new("timeout", "timed out reading the request")),
    };
    match result {
        Ok(request) => {
            let conn = net::Conn::new(Box::new(reader), Box::new(w), peer, SERVER_CONN_BUFFERS - net::CONN_BUFFER);
            Ok(Received { request, conn, res, slot })
        }
        Err(e) => {
            drop(res);
            if e.kind != "closed" {
                let body = format!("{}\n", e.message);
                let response = format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let send = async {
                    w.write_all(response.as_bytes()).await?;
                    w.flush().await
                };
                // Best effort: a client that does not read cannot hold the slot.
                let _ = tokio::time::timeout(Duration::from_secs(5), send).await;
            }
            // Closed before the slot can be reused.
            drop((reader, w));
            Err(slot)
        }
    }
}

/// Reads one request (head and body) from `reader`; `writer` receives a
/// `100 Continue` if the client asks for one.
async fn read_request<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    max_body: usize,
    res: &mut Reservation,
) -> Result<RawRequest, IoFailure> {
    let (start, headers) = read_head(&mut *reader, res).await?;
    let parts: Vec<&str> = start.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(IoFailure::new("http", format!("malformed request line {start:?}")));
    };
    if !is_token(method) || !version.starts_with("HTTP/1.") || target.is_empty() {
        return Err(IoFailure::new("http", format!("malformed request line {start:?}")));
    }
    // Absolute-form targets (`GET http://host/path`) carry the path after the authority.
    let target = match split_scheme(target).and_then(|(_, rest)| rest.strip_prefix("//")) {
        Some(rest) => rest.find(['/', '?']).map_or("/", |i| &rest[i..]),
        None => target,
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if header(&headers, "expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        writer.flush().await?;
    }
    let body = read_body(&mut *reader, &headers, max_body, false, res).await?;
    Ok((method.to_string(), path.to_string(), query.to_string(), headers, body))
}

fn reason_phrase(status: i64) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

fn encode_response(vm: &mut Vm, a: &[Value]) -> NResult {
    let status = a[0].as_int();
    if !(100..=999).contains(&status) {
        return Ok(vm.err_value("invalid", &format!("invalid HTTP status {status}")));
    }
    let headers = match header_pairs(vm, a[2]) {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    let body = vm.heap.bytes_of(a[1]);
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    for (k, v) in &headers {
        if !["content-length", "transfer-encoding", "connection"].iter().any(|h| k.eq_ignore_ascii_case(h)) {
            head += &format!("{k}: {v}\r\n");
        }
    }
    let has_body = status >= 200 && status != 204 && status != 304;
    if has_body {
        head += &format!("Content-Length: {}\r\n", body.len());
    }
    head += "Connection: close\r\n\r\n";
    let mut out = head.into_bytes();
    if has_body && a[3].as_bool() {
        out.extend_from_slice(body);
    }
    vm.reserve(out.len())?;
    let out = vm.heap.new_bytes(out);
    Ok(vm.ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Url::parse("HTTP://Example.com:8080/a/b?x=1#frag").unwrap();
        assert_eq!((u.scheme.as_str(), u.host.as_str(), u.port, u.target.as_str()), ("http", "example.com", 8080, "/a/b?x=1"));
        assert_eq!(u.authority(), "example.com:8080");
        let u = Url::parse("http://[::1]/").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.authority()), ("::1", 80, "[::1]".to_string()));
        assert_eq!(Url::parse("http://h?q").unwrap().target, "/?q");
        assert_eq!(Url::parse("not a url").unwrap_err().kind, "parse");
        assert_eq!(Url::parse("ftp://x/").unwrap_err().kind, "invalid");
        assert_eq!(Url::parse("http://h:99999/").unwrap_err().kind, "parse");
        assert_eq!(Url::parse("http:///x").unwrap_err().kind, "parse");
    }

    #[test]
    fn resolves_locations() {
        let base = "http://h:81/a/b/c?q=1";
        assert_eq!(resolve(base, "/x").unwrap(), "http://h:81/x");
        assert_eq!(resolve(base, "d").unwrap(), "http://h:81/a/b/d");
        assert_eq!(resolve(base, "../d?y").unwrap(), "http://h:81/a/d?y");
        assert_eq!(resolve(base, "?z").unwrap(), "http://h:81/a/b/c?z");
        assert_eq!(resolve(base, "").unwrap(), "http://h:81/a/b/c?q=1");
        assert_eq!(resolve(base, "//other/p").unwrap(), "http://other/p");
        #[cfg(feature = "tls")]
        assert_eq!(resolve(base, "https://o.example/x#f").unwrap(), "https://o.example/x");
        #[cfg(not(feature = "tls"))]
        assert_eq!(resolve(base, "https://o.example/x#f").unwrap_err().kind, "invalid");
        assert_eq!(resolve(base, "./").unwrap(), "http://h:81/a/b/");
    }
}
