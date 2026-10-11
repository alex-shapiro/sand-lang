//! The `http` module: an HTTP/1.1 client and server written directly on top
//! of tokio TCP streams (and rustls for `https`).
//!
//! Redirects are followed in Sand (`http.sand`) so that every hop goes
//! through `send`, which checks the policy before connecting. Bodies are
//! read as scripts read them (`http.Body`, through [`Framed`]): a client's
//! `send` returns at the response's head, and a server's `accept` at the
//! request's head.

use super::http_common::*;
use super::net::{self, ConnHandle, TlsSettings, check_permission, finish, suspend};
use super::stream::{IoFailure, read_line_bytes, strip_eol};
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::{Capability, Policy};
use crate::vm::iomem::{IoMemory, Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, NResult, Vm};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Maximum size of a request or response head (start line and headers).
const MAX_HEAD: usize = 64 << 10;
/// How long a server connection may take to deliver its request's head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a server's body read waits for data.
const SERVER_BODY_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("sand/", env!("CARGO_PKG_VERSION"));

pub fn register(r: &mut Registry) {
    r.source("http", ModKind::Std, concat!(include_str!("http_os.sand"), include_str!("http.sand")));
    super::http_common::register(r);
    r.add_builtin("http.send", send);
    r.add_builtin("http.serve", serve);
    r.add_builtin("http.Server.addr", |vm, a| {
        let addr = server(vm, a[0])?.addr.clone();
        Ok(vm.new_str(addr))
    });
    r.add_method("http.Server.close", |vm, a| {
        vm.heap.native_mut::<Server>(a[0]).ok_or_else(|| Ctl::panic("not a server"))?.close();
        Ok(Value::Unit)
    });
    r.add_builtin("http.next_request", next_request);
    r.add_builtin("http.write_response", write_response);
    r.add_builtin("http.start_upload", start_upload);
    r.add_builtin("http.reply_body", reply_body);
    r.add_builtin("http.write_body", write_body);
    r.add_builtin("http.read_response", read_response);
}

/// Reads `name: value` header lines up to the blank line ending the head,
/// after the start line. Names are lower-cased; repeated headers are kept,
/// in order. The lines are reserved in `res`.
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
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
}

/// The values of header `name` (lower-case), in order.
fn values<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> {
    headers.iter().filter(move |(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// How a message's body is delimited (RFC 9112 §6.3). Without
/// `Content-Length` or chunked encoding a body extends to the end of the
/// stream if `read_to_close` (responses), otherwise it is empty (requests).
fn framing(headers: &[(String, String)], read_to_close: bool) -> Result<Framing, IoFailure> {
    let chunked = values(headers, "transfer-encoding")
        .flat_map(|v| v.split(','))
        .last()
        .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
    if chunked {
        return Ok(Framing::Chunked(Chunk::Size));
    }
    let mut lengths = values(headers, "content-length").flat_map(|v| v.split(',')).map(|v| v.trim().parse::<u64>()).peekable();
    if lengths.peek().is_some() {
        let first = lengths.next().and_then(|v| v.ok());
        // Repeated values must agree.
        return match first.filter(|&n| lengths.all(|v| v.ok() == Some(n))) {
            Some(n) => Ok(Framing::Length(n)),
            None => Err(IoFailure::new("http", "invalid content-length")),
        };
    }
    Ok(if read_to_close { Framing::UntilClose } else { Framing::Length(0) })
}

// ----------------------------------------------------------------------
// Bodies

/// What remains of a body.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Framing {
    /// This many bytes.
    Length(u64),
    Chunked(Chunk),
    /// Everything up to the end of the stream.
    UntilClose,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Chunk {
    /// Next is a chunk-size line.
    Size,
    /// This many bytes of the chunk remain.
    Data(u64),
    /// Next is the CRLF after a chunk's data.
    DataEnd,
}

/// A buffered stream that a client's response owns.
trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<S: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for S {}

/// Where a body's bytes come from.
enum Source {
    /// A client's connection (a response's body).
    Owned(BufReader<Box<dyn AsyncStream>>),
    /// A server's connection (a request's body): its halves, held weakly
    /// (closing the `net.Conn` closes the socket), and its reader, locked
    /// during each read of the body only (so the script may use the
    /// connection between them).
    Conn {
        reader: Weak<tokio::sync::Mutex<net::ReadHalf>>,
        writer: Weak<tokio::sync::Mutex<net::WriteHalf>>,
        closed: net::CloseSignal,
        guard: Option<tokio::sync::OwnedMutexGuard<net::ReadHalf>>,
    },
}

impl Source {
    /// The source of a request's body on the connection `handle`.
    fn conn(handle: &ConnHandle) -> Source {
        Source::Conn {
            reader: Arc::downgrade(&handle.reader),
            writer: Arc::downgrade(&handle.writer),
            closed: handle.closed.clone(),
            guard: None,
        }
    }
}

/// A body decoded from its framing, as a [`BodyStream`].
struct Framed {
    source: Source,
    framing: Framing,
    /// The most a wait for data may take.
    timeout: Duration,
    /// Sends `100 Continue` before the first read (a request that asked for it).
    continue_first: bool,
}

impl Framed {
    /// The source, locked (waiting for the connection's reader if need be).
    async fn reader(&mut self) -> Result<&mut (dyn AsyncBufRead + Send + Unpin), IoFailure> {
        match &mut self.source {
            Source::Owned(r) => Ok(r),
            Source::Conn { reader, closed, guard, .. } => {
                if closed.is_closed() {
                    return Err(net::closed());
                }
                if guard.is_none() {
                    let reader = reader.upgrade().ok_or_else(net::closed)?;
                    // The connection may be in use by another task.
                    let locked = closed.clone().guard(async move { Ok(reader.lock_owned().await) }).await?;
                    *guard = Some(locked);
                }
                Ok(&mut **guard.as_mut().expect("locked"))
            }
        }
    }

    /// The bytes buffered by the source, waiting for some (bounded by the
    /// timeout); empty at the end of the stream.
    async fn source_buf(&mut self) -> Result<usize, IoFailure> {
        let timeout = self.timeout;
        // A request's body stops when its connection is closed.
        let closed = match &self.source {
            Source::Conn { closed, .. } => Some(closed.clone()),
            Source::Owned(_) => None,
        };
        let r = self.reader().await?;
        let fill = async move {
            match tokio::time::timeout(timeout, r.fill_buf()).await {
                Ok(r) => Ok(r?.len()),
                Err(_) => Err(IoFailure::new("timeout", "timed out waiting for the body")),
            }
        };
        match closed {
            Some(c) => c.guard(fill).await,
            None => fill.await,
        }
    }

    /// Reads a line of the chunked encoding (at most `max` bytes).
    async fn chunk_line(&mut self, max: usize) -> Result<Vec<u8>, IoFailure> {
        let mut line = Vec::new();
        loop {
            if self.source_buf().await? == 0 {
                return Err(IoFailure::new("http", "connection closed in the middle of a chunked body"));
            }
            let r = self.reader().await?;
            let buf = r.fill_buf().await?;
            let (take, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            if line.len() + take > max {
                return Err(IoFailure::new("http", "chunk line too long"));
            }
            line.extend_from_slice(&buf[..take]);
            r.consume(take);
            if done {
                strip_eol(&mut line);
                return Ok(line);
            }
        }
    }

    /// Advances to the next body bytes: how many are available (0 at the end).
    async fn ready(&mut self) -> Result<usize, IoFailure> {
        if std::mem::take(&mut self.continue_first) {
            let Source::Conn { writer, .. } = &self.source else { unreachable!("only requests continue") };
            let writer = writer.upgrade().ok_or_else(net::closed)?;
            let mut w = writer.lock().await;
            w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
            w.flush().await?;
        }
        loop {
            match self.framing {
                Framing::Done | Framing::Length(0) => {
                    self.framing = Framing::Done;
                    return Ok(0);
                }
                Framing::Length(n) => {
                    let got = self.source_buf().await?;
                    if got == 0 {
                        return Err(IoFailure::new("io", "connection closed in the middle of the message body"));
                    }
                    return Ok(got.min(usize::try_from(n).unwrap_or(usize::MAX)));
                }
                Framing::UntilClose => {
                    let got = self.source_buf().await?;
                    if got == 0 {
                        self.framing = Framing::Done;
                    }
                    return Ok(got);
                }
                Framing::Chunked(Chunk::Data(n)) => {
                    let got = self.source_buf().await?;
                    if got == 0 {
                        return Err(IoFailure::new("http", "connection closed in the middle of a chunked body"));
                    }
                    return Ok(got.min(usize::try_from(n).unwrap_or(usize::MAX)));
                }
                Framing::Chunked(Chunk::DataEnd) => {
                    if !self.chunk_line(2).await?.is_empty() {
                        return Err(IoFailure::new("http", "missing CRLF after a chunk"));
                    }
                    self.framing = Framing::Chunked(Chunk::Size);
                }
                Framing::Chunked(Chunk::Size) => {
                    let line = self.chunk_line(1024).await?;
                    let text = String::from_utf8_lossy(&line);
                    let size = text.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size, 16).map_err(|_| IoFailure::new("http", format!("invalid chunk size {size:?}")))?;
                    if size > 0 {
                        self.framing = Framing::Chunked(Chunk::Data(size));
                        continue;
                    }
                    // Skip trailer fields up to the final blank line.
                    let mut budget = MAX_HEAD;
                    loop {
                        let line = self.chunk_line(budget).await?;
                        if line.is_empty() {
                            break;
                        }
                        budget = budget.saturating_sub(line.len() + 2);
                    }
                    self.framing = Framing::Done;
                    return Ok(0);
                }
            }
        }
    }
}

impl BodyStream for Framed {
    fn fill(&mut self) -> Filled<'_> {
        Box::pin(async move {
            let n = self.ready().await?;
            if n == 0 {
                return Ok(&[][..]);
            }
            // Already buffered by `ready`.
            let buf = self.reader().await?.fill_buf().await?;
            Ok(&buf[..n.min(buf.len())])
        })
    }

    fn release(&mut self) {
        if let Source::Conn { guard, .. } = &mut self.source {
            *guard = None;
        }
    }

    fn shutdown(&mut self) {
        if let Source::Owned(r) = &mut self.source {
            net::shutdown_now(r.get_mut());
        }
    }

    fn consume(&mut self, n: usize) {
        let r: &mut (dyn AsyncBufRead + Send + Unpin) = match &mut self.source {
            Source::Owned(r) => r,
            Source::Conn { guard: Some(g), .. } => &mut **g,
            Source::Conn { guard: None, .. } => return,
        };
        Pin::new(r).consume(n);
        let n = n as u64;
        self.framing = match self.framing {
            Framing::Length(left) => Framing::Length(left.saturating_sub(n)),
            Framing::Chunked(Chunk::Data(left)) if left <= n => Framing::Chunked(Chunk::DataEnd),
            Framing::Chunked(Chunk::Data(left)) => Framing::Chunked(Chunk::Data(left - n)),
            f => f,
        };
    }
}

// ----------------------------------------------------------------------
// Client

/// A response's head, with its connection.
struct Head {
    status: i64,
    headers: Vec<(String, String)>,
    stream: BufReader<Box<dyn AsyncStream>>,
    framing: Framing,
}

fn send(vm: &mut Vm, a: &[Value]) -> NResult {
    let (method, text) = (vm.str(a[0]), vm.str(a[1]));
    vm.charge_slow_bytes(method.len().saturating_add(text.len()))?;
    let url = match Url::parse(text) {
        Ok(u) => u,
        Err(e) => return Ok(vm.err_value(e.kind, &e.message)),
    };
    if !is_token(method) {
        return Ok(vm.err_value("invalid", &format!("invalid HTTP method {:?}", super::truncate(method, 64))));
    }
    let (method, text) = (method.to_string(), text.to_string());
    let headers = match header_pairs(vm, a[2])? {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    if let Some(denied) = check_permission(vm, url.capability(&method))? {
        return Ok(denied);
    }
    // The request is held outside the heap while it is sent.
    let mut res = super::or_return!(vm.reserve_io(vm.heap.bytes_of(a[3]).len()));
    // The connection counts as an open resource, with its buffers, until
    // its body has been read (or closed).
    let buffers = if url.scheme == "https" { net::CONN_BUFFER + net::TLS_BUFFERS } else { net::CONN_BUFFER };
    let slot = super::or_return!(vm.resource_slot_with(buffers, "a connection"));
    let body = vm.heap.bytes_of(a[3]);
    vm.charge_bytes(body.len())?;
    let request = encode_request(&method, &url, &headers, body);
    if let Err(e) = res.ensure(request.len()) {
        return Ok(vm.err_value("limit", &format!("cannot buffer the request: {e}")));
    }
    let timeout = Duration::from_nanos(a[4].as_int().max(0) as u64);
    let head_only = method.eq_ignore_ascii_case("HEAD");
    let max_body = vm.host.limits.max_http_body;
    let policy = vm.host.policy.clone();
    let tls = vm.host.tls.clone();
    suspend(
        async move {
            let mut head_res = res.sibling();
            let exchange = fetch(&policy, &tls, &url, &request, head_only, &mut head_res);
            let head = match tokio::time::timeout(timeout, exchange).await {
                Ok(r) => r?,
                Err(_) => return Err(IoFailure::new("timeout", format!("request to {text} timed out"))),
            };
            drop(res);
            Ok((head, head_res))
        },
        move |vm, (head, head_res): (Head, Reservation)| {
            drop(head_res);
            let Head { status, headers, stream, framing } = head;
            let headers = header_list(vm, headers)?;
            let reader = if framing == Framing::Done {
                // Nothing to read: the connection closes now.
                BodyReader::new(Box::new(MemoryBody::new(Vec::new())), None, max_body, None)
            } else {
                let framed = Framed { source: Source::Owned(stream), framing, timeout, continue_first: false };
                BodyReader::new(Box::new(framed), Some(slot), max_body, None)
            };
            let body = alloc_body(vm, reader);
            Ok(vm.heap.new_tuple(&[Value::Int(status), headers, body]))
        },
    )
}

fn encode_request(method: &str, url: &Url, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let sized = !body.is_empty() || ["POST", "PUT", "PATCH"].iter().any(|m| method.eq_ignore_ascii_case(m));
    let mut out = request_head(method, url, headers, sized.then_some(body.len() as u64)).into_bytes();
    out.extend_from_slice(body);
    out
}

/// A request's head; `length` is the body's `Content-Length`, if it has one.
fn request_head(method: &str, url: &Url, headers: &[(String, String)], length: Option<u64>) -> String {
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut head = format!("{method} {} HTTP/1.1\r\n", url.target);
    if !has("host") {
        head += &format!("Host: {}\r\n", url.authority());
    }
    push_headers(&mut head, headers);
    if !has("user-agent") {
        head += &format!("User-Agent: {USER_AGENT}\r\n");
    }
    end_head(head, length)
}

/// Appends `headers`, except those that frame the body or manage the
/// connection (which are ours).
fn push_headers(head: &mut String, headers: &[(String, String)]) {
    for (k, v) in headers {
        if !["content-length", "transfer-encoding", "connection"].iter().any(|h| k.eq_ignore_ascii_case(h)) {
            *head += &format!("{k}: {v}\r\n");
        }
    }
}

fn end_head(mut head: String, length: Option<u64>) -> String {
    if let Some(n) = length {
        head += &format!("Content-Length: {n}\r\n");
    }
    head + "Connection: close\r\n\r\n"
}

/// Connects to `url` (at an address the policy allows), sends the request
/// and reads the response's head.
async fn fetch(
    policy: &Policy,
    tls: &TlsSettings,
    url: &Url,
    request: &[u8],
    head_only: bool,
    res: &mut Reservation,
) -> Result<Head, IoFailure> {
    // The overall timeout is applied by the caller.
    let tcp = net::connect_checked(policy, &url.host, url.port).await?;
    let stream: Box<dyn AsyncStream> =
        if url.scheme == "https" { Box::new(net::tls::connect(tls, &url.host, tcp).await?) } else { Box::new(tcp) };
    let mut stream = BufReader::with_capacity(net::CONN_BUFFER, stream);
    stream.write_all(request).await?;
    stream.flush().await?;
    let (status, headers, framing) = read_response_head(&mut stream, head_only, res).await?;
    Ok(Head { status, headers, stream, framing })
}

/// Reads a response's head (skipping interim responses): its status,
/// headers and body framing.
async fn read_response_head<R: AsyncBufRead + Unpin>(
    stream: &mut R,
    head_only: bool,
    res: &mut Reservation,
) -> Result<(i64, Vec<(String, String)>, Framing), IoFailure> {
    loop {
        // Interim responses are discarded with their reservation.
        let mut head_res = res.sibling();
        let (start, headers) = read_head(&mut *stream, &mut head_res).await.map_err(|e| match e.kind {
            "closed" => IoFailure::new("io", "connection closed before a response was received"),
            _ => e,
        })?;
        let mut parts = start.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let status = parts.next().and_then(|s| s.parse::<i64>().ok()).filter(|s| (100..=999).contains(s));
        let Some(status) = status.filter(|_| version.starts_with("HTTP/")) else {
            return Err(IoFailure::new("http", format!("malformed status line {start:?}")));
        };
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        let no_body = head_only || status < 200 || status == 204 || status == 304;
        let framing = if no_body { Framing::Done } else { framing(&headers, true)? };
        res.absorb(head_res);
        return Ok((status, headers, framing));
    }
}

// ----------------------------------------------------------------------
// Bodies written as they go

/// The `http.Outgoing` type: the framing of a body that a script writes
/// (an upload's request, a reply's response), and the head still to send
/// before it.
struct Outgoing {
    head: Option<Vec<u8>>,
    /// The bytes left of a body with a `Content-Length`; `None` if chunked.
    left: Option<u64>,
    /// False when the body is not sent (a reply to `HEAD`, or a status
    /// without a body): writes are dropped.
    sent: bool,
    finished: bool,
}

impl NativeObject for Outgoing {
    fn type_name(&self) -> &'static str {
        "Outgoing"
    }
    fn size(&self) -> usize {
        64 + self.head.as_ref().map_or(0, Vec::capacity)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The `Content-Length` that `headers` give the body, if any.
fn given_length(headers: &[(String, String)]) -> Result<Option<u64>, String> {
    match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        Some((_, v)) => v.trim().parse().map(Some).map_err(|_| format!("invalid content-length {v:?}")),
        None => Ok(None),
    }
}

fn alloc_outgoing(vm: &mut Vm, head: String, left: Option<u64>, sent: bool) -> Value {
    vm.heap.alloc(Obj::Native(Box::new(Outgoing { head: Some(head.into_bytes()), left, sent, finished: false })))
}

/// `start_upload(method, url, headers, timeout)`: connects for a request
/// whose body is written as it goes: `(net.Conn, Outgoing)`.
fn start_upload(vm: &mut Vm, a: &[Value]) -> NResult {
    let (method, text) = (vm.str(a[0]).to_string(), vm.str(a[1]).to_string());
    vm.charge_slow_bytes(method.len() + text.len())?;
    let url = match Url::parse(&text) {
        Ok(u) => u,
        Err(e) => return Ok(vm.err_value(e.kind, &e.message)),
    };
    if !is_token(&method) {
        return Ok(vm.err_value("invalid", &format!("invalid HTTP method {:?}", super::truncate(&method, 64))));
    }
    let headers = match header_pairs(vm, a[2])? {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    let length = match given_length(&headers) {
        Ok(n) => n,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    if let Some(denied) = check_permission(vm, url.capability(&method))? {
        return Ok(denied);
    }
    let buffers = if url.scheme == "https" { net::CONN_BUFFER + net::TLS_BUFFERS } else { net::CONN_BUFFER };
    let slot = super::or_return!(vm.resource_slot_with(buffers, "a connection"));
    let mut head = request_head(&method, &url, &headers, length);
    if length.is_none() {
        head.insert_str(head.len() - "Connection: close\r\n\r\n".len(), "Transfer-Encoding: chunked\r\n");
    }
    let timeout = Duration::from_nanos(a[3].as_int().max(0) as u64);
    let (policy, tls) = (vm.host.policy.clone(), vm.host.tls.clone());
    let open = async move {
        let tcp = net::connect_checked(&policy, &url.host, url.port).await?;
        let peer = tcp.peer_addr()?.to_string();
        if url.scheme != "https" {
            let (r, w) = tcp.into_split();
            return Ok(net::Conn::new(Box::new(r), Box::new(w), peer));
        }
        let (r, w) = tokio::io::split(net::tls::connect(&tls, &url.host, tcp).await?);
        Ok(net::Conn::new(Box::new(r), Box::new(w), peer))
    };
    suspend(
        async move {
            match tokio::time::timeout(timeout, open).await {
                Ok(r) => r.map(|c| (c, slot)),
                Err(_) => Err(IoFailure::new("timeout", format!("connecting to {text} timed out"))),
            }
        },
        move |vm, (conn, slot)| {
            let conn = net::alloc_resource(vm, conn, slot);
            let body = alloc_outgoing(vm, head, length, true);
            Ok(vm.heap.new_tuple(&[conn, body]))
        },
    )
}

/// `reply_body(status, headers, include_body)`: the `Outgoing` of a
/// response whose body is written as it goes.
fn reply_body(vm: &mut Vm, a: &[Value]) -> NResult {
    let status = a[0].as_int();
    if !(100..=999).contains(&status) {
        return Ok(vm.err_value("invalid", &format!("invalid HTTP status {status}")));
    }
    let headers = match header_pairs(vm, a[1])? {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    let length = match given_length(&headers) {
        Ok(n) => n,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    let has_body = status >= 200 && status != 204 && status != 304;
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    push_headers(&mut head, &headers);
    if has_body && length.is_none() {
        head += "Transfer-Encoding: chunked\r\n";
    }
    let head = end_head(head, length.filter(|_| has_body));
    vm.charge_bytes(head.len())?;
    let body = alloc_outgoing(vm, head, length, has_body && a[2].as_bool());
    Ok(vm.ok(body))
}

/// `write_body(conn, body, data, last)`: writes `data` to the body (an
/// `Outgoing` on `conn`), then, if `last`, ends it.
fn write_body(vm: &mut Vm, a: &[Value]) -> NResult {
    let n = vm.heap.bytes_of(a[2]).len();
    let last = a[3].as_bool();
    let out = vm.heap.native::<Outgoing>(a[1]).ok_or_else(|| Ctl::panic("not an outgoing body"))?;
    if out.finished {
        return Ok(vm.err_value("closed", "the body has been finished"));
    }
    let left = out.left.map(|left| left.checked_sub(n as u64));
    match left {
        Some(None) => return Ok(vm.err_value("invalid", "the body is longer than its content-length")),
        Some(Some(more)) if last && more > 0 && out.sent => {
            return Ok(vm.err_value("invalid", &format!("the body is {more} bytes shorter than its content-length")));
        }
        _ => {}
    }
    let (chunked, sent) = (out.left.is_none(), out.sent);
    let len = out.head.as_ref().map_or(0, Vec::len) + if sent { n + 32 } else { 0 };
    vm.charge_bytes(len)?;
    let res = super::or_return!(vm.reserve_io(len));
    let out = vm.heap.native_mut::<Outgoing>(a[1]).expect("checked above");
    out.left = left.flatten();
    out.finished = last;
    let mut data = out.head.take().unwrap_or_default();
    if sent {
        let body = vm.heap.bytes_of(a[2]);
        if chunked && !body.is_empty() {
            data.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
            data.extend_from_slice(body);
            data.extend_from_slice(b"\r\n");
        } else if !chunked {
            data.extend_from_slice(body);
        }
        if chunked && last {
            data.extend_from_slice(b"0\r\n\r\n");
        }
    }
    if data.is_empty() {
        return Ok(vm.ok(Value::Unit));
    }
    net::write_bytes(vm, a[0], data, res)
}

/// `read_response(conn, head_only, timeout)`: reads the response to an
/// upload: `(status, headers, body)`, the body read from `conn`.
fn read_response(vm: &mut Vm, a: &[Value]) -> NResult {
    let h = match net::conn_handle(vm, a[0])? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    let head_only = a[1].as_bool();
    let timeout = Duration::from_nanos(a[2].as_int().max(0) as u64);
    let max_body = vm.host.limits.max_http_body;
    let mut res = Reservation::new(&vm.heap.io_memory);
    let reader = h.reader.clone();
    let read = async move {
        let mut reader = reader.lock().await;
        let head = read_response_head(&mut *reader, head_only, &mut res).await;
        drop(res);
        head
    };
    suspend(
        h.closed.clone().guard(async move {
            match tokio::time::timeout(timeout, read).await {
                Ok(r) => r,
                Err(_) => Err(IoFailure::new("timeout", "timed out waiting for the response")),
            }
        }),
        move |vm, (status, headers, framing)| {
            let headers = header_list(vm, headers)?;
            let reader = if framing == Framing::Done {
                BodyReader::new(Box::new(MemoryBody::new(Vec::new())), None, max_body, None)
            } else {
                let framed = Framed { source: Source::conn(&h), framing, timeout, continue_first: false };
                BodyReader::new(Box::new(framed), None, max_body, None)
            };
            let body = alloc_body(vm, reader);
            Ok(vm.heap.new_tuple(&[Value::Int(status), headers, body]))
        },
    )
}

// ----------------------------------------------------------------------
// Server

/// A request's head, read by the server: method, path, query, headers.
type RawHead = (String, String, String, Vec<(String, String)>);

/// A request whose head has been read, with its connection.
struct Received {
    head: RawHead,
    framing: Framing,
    expects_continue: bool,
    conn: net::Conn,
    /// The head's buffers, until they become values.
    res: Reservation,
    /// Counts the connection; its `net.Conn` object keeps it.
    slot: ResourceSlot,
}

/// Reading the head of one accepted connection: `None` if it did not
/// deliver one (it was answered with an error and closed, freeing its slot).
type Reading = Pin<Box<dyn Future<Output = Option<Received>> + Send>>;

/// A connection whose head is being read, and the flag that tells it to
/// give up (to make room for a newer one).
struct Pending {
    reading: Reading,
    evict: Arc<AtomicBool>,
}

#[derive(Default)]
struct PendingShared {
    /// Locked by the `next_request` call that drives the readings; oldest first.
    reads: tokio::sync::Mutex<Vec<Pending>>,
}

/// The `http.Server` type: a listening socket (counted by its slot while
/// open), and the connections accepted whose requests' heads are being
/// read (each with a slot of its own).
struct Server {
    listener: Option<Arc<TcpListener>>,
    slot: Option<ResourceSlot>,
    addr: String,
    closed: watch::Sender<bool>,
    pending: Arc<PendingShared>,
}

impl Server {
    fn new(socket: TcpListener) -> std::io::Result<Server> {
        let addr = socket.local_addr()?.to_string();
        let closed = watch::Sender::new(false);
        Ok(Server { listener: Some(Arc::new(socket)), slot: None, addr, closed, pending: Arc::default() })
    }

    /// Stops listening; the connections being read are closed with the last
    /// reference to them: now, or when a pending `next_request` ends.
    fn close(&mut self) {
        self.closed.send_replace(true);
        self.listener = None;
        self.slot = None;
        self.pending = Arc::default();
    }
}

impl net::Resource for Server {
    fn attach(&mut self, slot: ResourceSlot) {
        self.slot = Some(slot);
    }
}

impl NativeObject for Server {
    fn type_name(&self) -> &'static str {
        "Server"
    }
    fn is_resource(&self) -> bool {
        self.listener.is_some()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn server(vm: &Vm, v: Value) -> Result<&Server, Ctl> {
    vm.heap.native::<Server>(v).ok_or_else(|| Ctl::panic("not a server"))
}

fn serve(vm: &mut Vm, a: &[Value]) -> NResult {
    let addr = vm.str(a[0]).to_string();
    if let Some(denied) = check_permission(vm, Capability::NetListen(addr.clone()))? {
        return Ok(denied);
    }
    let slot = super::or_return!(vm.resource_slot());
    net::suspend_resource(slot, async move { Ok(Server::new(TcpListener::bind(addr).await?)?) })
}

/// `next_request(server)`: accepts connections and reads their requests'
/// heads concurrently (each within `HEAD_TIMEOUT`) until one is complete.
/// Each accepted connection takes an open-resource slot; while none is
/// free, connections wait in the backlog. At most half of
/// `max_open_resources` are read at once: beyond, the oldest is answered
/// with 408 and closed. Readings in progress when the call returns or is
/// cancelled stay in `pending` for the next call.
fn next_request(vm: &mut Vm, a: &[Value]) -> NResult {
    let srv = server(vm, a[0])?;
    let Some(socket) = srv.listener.clone() else { return Ok(vm.err_value("closed", "server closed")) };
    let closed = net::CloseSignal::of(&srv.closed);
    let shared = srv.pending.clone();
    let mem = vm.heap.io_memory.clone();
    let cap = (mem.max_resources() / 2).max(1);
    let serve = async move {
        let mut reads = shared.reads.lock().await;
        // Retries taking a slot while none is free.
        let mut retry: Option<Pin<Box<tokio::time::Sleep>>> = None;
        std::future::poll_fn(|cx| {
            loop {
                let mut progress = false;
                if retry.as_mut().is_none_or(|t| t.as_mut().poll(cx).is_ready()) {
                    retry = None;
                    match ResourceSlot::try_new(&mem) {
                        Some(slot) => match socket.poll_accept(cx) {
                            Poll::Ready(Ok((stream, _))) => {
                                let evict = Arc::new(AtomicBool::new(false));
                                let reading = Box::pin(read_connection(stream, mem.clone(), slot, evict.clone()));
                                reads.push(Pending { reading, evict });
                                // Beyond the cap, the oldest gives up.
                                let reading: Vec<&Pending> = reads.iter().filter(|p| !p.evict.load(Ordering::Relaxed)).collect();
                                if reading.len() > cap {
                                    reading[0].evict.store(true, Ordering::Relaxed);
                                }
                                progress = true;
                            }
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(IoFailure::from(e))),
                            Poll::Pending => {}
                        },
                        None => retry = Some(Box::pin(tokio::time::sleep(Duration::from_millis(50)))),
                    }
                }
                let mut i = 0;
                while i < reads.len() {
                    match reads[i].reading.as_mut().poll(cx) {
                        Poll::Ready(done) => {
                            drop(reads.remove(i));
                            if let Some(r) = done {
                                return Poll::Ready(Ok(r));
                            }
                            progress = true;
                        }
                        Poll::Pending => i += 1,
                    }
                }
                if !progress {
                    if let Some(t) = &mut retry
                        && t.as_mut().poll(cx).is_ready()
                    {
                        continue;
                    }
                    return Poll::Pending;
                }
            }
        })
        .await
    };
    let max_body = vm.host.limits.max_http_body;
    Err(Ctl::suspend(Box::pin(async move {
        finish(closed.guard(serve).await, move |vm, received: Received| {
            let Received { head: (method, path, query, headers), framing, expects_continue, conn, res, slot } = received;
            drop(res);
            let fields = [method, path, query].map(|s| vm.new_str(s));
            let headers = header_list(vm, headers)?;
            let handle = conn.handle().expect("a new connection is open");
            let body = if framing == Framing::Done || framing == Framing::Length(0) {
                BodyReader::new(Box::new(MemoryBody::new(Vec::new())), None, max_body, None)
            } else {
                let source = Source::conn(&handle);
                let framed = Framed { source, framing, timeout: SERVER_BODY_TIMEOUT, continue_first: expects_continue };
                BodyReader::new(Box::new(framed), None, max_body, None)
            };
            let body = alloc_body(vm, body);
            let conn = net::alloc_resource(vm, conn, slot);
            Ok(vm.heap.new_tuple(&[fields[0], fields[1], fields[2], headers, body, conn]))
        })
    })))
}

/// Answers a connection that does not deliver a valid request head with
/// `status` (best effort: a client that does not read cannot hold the slot).
async fn deny<W: AsyncWrite + Unpin>(w: &mut W, status: &str, message: &str) {
    let body = format!("{message}\n");
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let send = async {
        w.write_all(response.as_bytes()).await?;
        w.flush().await
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), send).await;
}

/// Reads the head of an accepted connection's request (see [`Reading`]).
async fn read_connection(stream: TcpStream, mem: Arc<IoMemory>, mut slot: ResourceSlot, evict: Arc<AtomicBool>) -> Option<Received> {
    let peer = stream.peer_addr().ok()?.to_string();
    let (r, mut w) = stream.into_split();
    if slot.reserve_buffers(net::CONN_BUFFER).is_err() {
        deny(&mut w, "503 Service Unavailable", "the server is out of memory").await;
        return None;
    }
    // Bytes buffered past the head stay in this reader for the body.
    let mut reader = net::read_half(Box::new(r));
    let mut res = Reservation::new(&mem);
    let evicted = std::future::poll_fn(|cx| {
        // Checked whenever the reading is polled (which the server does
        // right after setting the flag).
        if evict.load(Ordering::Relaxed) {
            return Poll::Ready(());
        }
        let _ = cx;
        Poll::Pending
    });
    let read = tokio::time::timeout(HEAD_TIMEOUT, read_request_head(&mut reader, &mut res));
    let result = tokio::select! {
        r = read => r.unwrap_or_else(|_| Err(IoFailure::new("timeout", "timed out reading the request"))),
        () = evicted => Err(IoFailure::new("timeout", "closed to serve newer connections")),
    };
    match result {
        Ok((head, framing, expects_continue)) => {
            let conn = net::Conn::with_reader(reader, Box::new(w), peer);
            Some(Received { head, framing, expects_continue, conn, res, slot })
        }
        Err(e) => {
            drop(res);
            match e.kind {
                "closed" => {}
                "timeout" => deny(&mut w, "408 Request Timeout", &e.message).await,
                _ => deny(&mut w, "400 Bad Request", &e.message).await,
            }
            None
        }
    }
}

/// Reads a request's head from `reader`: the head, how its body is framed
/// and whether the client expects `100 Continue` before sending it.
async fn read_request_head<R: AsyncBufRead + Unpin>(reader: &mut R, res: &mut Reservation) -> Result<(RawHead, Framing, bool), IoFailure> {
    let (start, headers) = read_head(&mut *reader, res).await?;
    let parts: Vec<&str> = start.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(IoFailure::new("http", format!("malformed request line {start:?}")));
    };
    if !is_token(method) || !version.starts_with("HTTP/1.") || target.is_empty() {
        return Err(IoFailure::new("http", format!("malformed request line {start:?}")));
    }
    // Absolute-form targets (`GET http://host/path`) carry the path after the authority.
    let absolute = (!target.starts_with('/')).then(|| super::uri::parse(target, true).ok()).flatten().filter(|p| p.authority.is_some());
    let (path, query) = match &absolute {
        Some(p) => (if p.path.is_empty() { "/" } else { p.path.as_str() }, p.query.as_deref().unwrap_or("")),
        None => target.split_once('?').unwrap_or((target, "")),
    };
    let expects_continue = values(&headers, "expect").any(|e| e.eq_ignore_ascii_case("100-continue"));
    let framing = framing(&headers, false)?;
    let head = (method.to_string(), path.to_string(), query.to_string(), headers);
    Ok((head, framing, expects_continue))
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

/// `write_response(conn, status, body, headers, include_body)`: writes the
/// head and (unless `include_body` is false) the body to `conn`.
fn write_response(vm: &mut Vm, a: &[Value]) -> NResult {
    let status = a[1].as_int();
    if !(100..=999).contains(&status) {
        return Ok(vm.err_value("invalid", &format!("invalid HTTP status {status}")));
    }
    let headers = match header_pairs(vm, a[3])? {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    for (k, v) in &headers {
        if !["content-length", "transfer-encoding", "connection"].iter().any(|h| k.eq_ignore_ascii_case(h)) {
            head += &format!("{k}: {v}\r\n");
        }
    }
    let has_body = status >= 200 && status != 204 && status != 304;
    let body = if has_body && a[4].as_bool() { vm.heap.bytes_of(a[2]) } else { &[] };
    if has_body {
        head += &format!("Content-Length: {}\r\n", vm.heap.bytes_of(a[2]).len());
    }
    head += "Connection: close\r\n\r\n";
    let len = head.len().saturating_add(body.len());
    vm.charge_bytes(len)?;
    let res = super::or_return!(vm.reserve_io(len));
    let body = if has_body && a[4].as_bool() { vm.heap.bytes_of(a[2]) } else { &[] };
    let mut data = head.into_bytes();
    data.extend_from_slice(body);
    net::write_bytes(vm, a[0], data, res)
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
        assert_eq!(Url::parse("http://user:pw@h/").unwrap().authority(), "h");
        assert_eq!(Url::parse("http://h/a b").unwrap_err().kind, "parse");
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
        assert_eq!(resolve(base, "../../../../x").unwrap(), "http://h:81/x");
        assert_eq!(resolve(base, "mailto:x@y").unwrap_err().kind, "invalid");
    }
}
