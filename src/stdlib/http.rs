//! The `http` module's client, on tokio TCP streams (and rustls for
//! `https`), and the message syntax and body framing it shares with the
//! server (`http_server.rs`).
//!
//! Redirects are followed in Sand (`http.sand`), so every hop goes through
//! `send` and its policy check. A response whose body is read to its end
//! leaves its connection in the run's [`Pool`].

use super::http_common::*;
use super::net::{self, ConnHandle, TlsSettings, check_permission, suspend};
use super::stream::{IoFailure, read_line_bytes, strip_eol};
use crate::check::ModKind;
use crate::engine::Registry;
use crate::network::{Network, Stream};
use crate::platform::{Clock, Instant};
use crate::policy::Policy;
use crate::vm::iomem::{Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, NResult, Vm};
use std::any::Any;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Waker};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Maximum size of a request or response head (start line and headers).
pub(super) const MAX_HEAD: usize = 64 << 10;
const USER_AGENT: &str = concat!("sand/", env!("CARGO_PKG_VERSION"));
/// How long an idle client connection is kept for another request.
const POOL_IDLE: Duration = Duration::from_secs(20);
/// The most idle client connections kept per origin.
const POOL_PER_ORIGIN: usize = 4;
/// The most idle client connections kept in all (and at most a quarter of
/// `max_open_resources`).
const POOL_MAX: usize = 16;

pub fn register(r: &mut Registry) {
    r.source("http", ModKind::Std, concat!(include_str!("http_os.sand"), include_str!("http.sand")));
    super::http_common::register(r);
    super::http_server::register(r);
    r.add_builtin("http.send", send);
    r.add_builtin("http.start_upload", start_upload);
    r.add_builtin("http.write_body", write_body);
    r.add_builtin("http.read_response", read_response);
}

// ----------------------------------------------------------------------
// Message heads

/// Reads a message head: the start line, and the headers in order with
/// lower-cased names. The lines are reserved in `res`.
pub(super) async fn read_head<R: AsyncBufRead + Unpin>(
    r: &mut R,
    res: &mut Reservation,
) -> Result<(String, Vec<(String, String)>), IoFailure> {
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
pub(super) fn values<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> {
    headers.iter().filter(move |(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Whether a `Connection` header lists `option`.
pub(super) fn connection_has(headers: &[(String, String)], option: &str) -> bool {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| v.split(','))
        .any(|o| o.trim().eq_ignore_ascii_case(option))
}

/// Whether a message of HTTP `version` with `headers` lets its connection
/// carry another exchange (RFC 9112 §9.3).
pub(super) fn persists(version: &str, headers: &[(String, String)]) -> bool {
    match version {
        "HTTP/1.0" => connection_has(headers, "keep-alive") && !connection_has(headers, "close"),
        v => v.starts_with("HTTP/1.") && !connection_has(headers, "close"),
    }
}

/// How a received message's body is delimited (RFC 9112 §6.3). Without
/// `Content-Length` or chunked encoding a body extends to the end of the
/// stream if `read_to_close` (responses), otherwise it is empty (requests).
pub(super) fn framing(headers: &[(String, String)], read_to_close: bool) -> Result<Framing, IoFailure> {
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
            Some(0) => Ok(Framing::Done),
            Some(n) => Ok(Framing::Length(n)),
            None => Err(IoFailure::new("http", "invalid content-length")),
        };
    }
    Ok(if read_to_close { Framing::UntilClose } else { Framing::Done })
}

/// How the body of a message being sent is delimited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Delimit {
    /// The message has no body.
    None,
    /// By `Content-Length`.
    Length(u64),
    Chunked,
    /// By closing the connection (a response to HTTP/1.0).
    Close,
}

/// Appends `headers`, except the framing and `Connection` headers, which
/// [`end_head`] adds.
pub(super) fn push_headers(head: &mut String, headers: &[(String, String)]) {
    for (k, v) in headers {
        if !["content-length", "transfer-encoding", "connection"].iter().any(|h| k.eq_ignore_ascii_case(h)) {
            *head += &format!("{k}: {v}\r\n");
        }
    }
}

/// Ends a head with the framing headers and an optional `Connection` header.
pub(super) fn end_head(mut head: String, body: Delimit, connection: Option<&str>) -> String {
    match body {
        Delimit::Length(n) => head += &format!("Content-Length: {n}\r\n"),
        Delimit::Chunked => head += "Transfer-Encoding: chunked\r\n",
        Delimit::None | Delimit::Close => {}
    }
    if let Some(c) = connection {
        head += &format!("Connection: {c}\r\n");
    }
    head + "\r\n"
}

// ----------------------------------------------------------------------
// Bodies

/// What remains of a received body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Framing {
    /// This many bytes (at least one).
    Length(u64),
    Chunked(Chunk),
    /// Everything up to the end of the stream.
    UntilClose,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Chunk {
    /// Next is a chunk-size line.
    Size,
    /// This many bytes of the chunk remain.
    Data(u64),
    /// Next is the CRLF after a chunk's data.
    DataEnd,
}

/// Where a body's bytes come from.
enum Source {
    /// A client connection, until the body ends. A body read to its end
    /// puts it in `reuse`.
    Owned { conn: Option<ClientConn>, reuse: Option<Weak<Pool>> },
    /// A `net.Conn`'s halves, held weakly so that closing the `net.Conn`
    /// closes the socket. `guard` holds the reader during a read only.
    Conn {
        reader: Weak<tokio::sync::Mutex<net::ReadHalf>>,
        writer: Weak<tokio::sync::Mutex<net::WriteHalf>>,
        closed: net::CloseSignal,
        guard: Option<tokio::sync::OwnedMutexGuard<net::ReadHalf>>,
    },
}

/// A body decoded from its framing, as a [`BodyStream`].
pub(super) struct Framed {
    source: Source,
    framing: Framing,
    /// The most a wait for data may take, on `clock`.
    timeout: Duration,
    clock: Arc<dyn Clock>,
    /// Sends `100 Continue` before the first read (a request that asked for it).
    continue_first: bool,
}

impl Framed {
    /// A body read from the `net.Conn` of `handle` (a server's request, or
    /// the response to an upload).
    pub(super) fn on_conn(handle: &ConnHandle, framing: Framing, timeout: Duration, clock: Arc<dyn Clock>, continue_first: bool) -> Framed {
        let source = Source::Conn {
            reader: Arc::downgrade(&handle.reader),
            writer: Arc::downgrade(&handle.writer),
            closed: handle.closed.clone(),
            guard: None,
        };
        Framed { source, framing, timeout, clock, continue_first }
    }

    /// The source, locked (waiting for the connection's reader if need be).
    async fn reader(&mut self) -> Result<&mut (dyn AsyncBufRead + Send + Unpin), IoFailure> {
        match &mut self.source {
            Source::Owned { conn, .. } => Ok(&mut conn.as_mut().ok_or_else(net::closed)?.stream),
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
        let (timeout, clock) = (self.timeout, self.clock.clone());
        // A request's body stops when its connection is closed.
        let closed = match &self.source {
            Source::Conn { closed, .. } => Some(closed.clone()),
            Source::Owned { .. } => None,
        };
        let r = self.reader().await?;
        let fill = async move {
            match crate::platform::timeout(&*clock, timeout, r.fill_buf()).await {
                Some(r) => Ok(r?.len()),
                None => Err(IoFailure::new("timeout", "timed out waiting for the body")),
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
                Framing::Done => return Ok(0),
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

    fn consume(&mut self, n: usize) {
        let r: &mut (dyn AsyncBufRead + Send + Unpin) = match &mut self.source {
            Source::Owned { conn: Some(c), .. } => &mut c.stream,
            Source::Conn { guard: Some(g), .. } => &mut **g,
            Source::Owned { conn: None, .. } | Source::Conn { guard: None, .. } => return,
        };
        Pin::new(r).consume(n);
        let n = n as u64;
        self.framing = match self.framing {
            Framing::Length(left) if left <= n => Framing::Done,
            Framing::Length(left) => Framing::Length(left - n),
            Framing::Chunked(Chunk::Data(left)) if left <= n => Framing::Chunked(Chunk::DataEnd),
            Framing::Chunked(Chunk::Data(left)) => Framing::Chunked(Chunk::Data(left - n)),
            f => f,
        };
    }

    fn at_end(&self) -> bool {
        self.framing == Framing::Done
    }

    fn owns_conn(&self) -> bool {
        matches!(self.source, Source::Owned { .. })
    }

    fn release(&mut self) {
        if let Source::Conn { guard, .. } = &mut self.source {
            *guard = None;
        }
    }

    fn end(&mut self, complete: bool) {
        let Source::Owned { conn, reuse } = &mut self.source else { return };
        let Some(mut conn) = conn.take() else { return };
        match reuse.take().and_then(|p| p.upgrade()) {
            Some(pool) if complete && self.framing == Framing::Done => pool.put(conn),
            _ => net::shutdown_now(conn.stream.get_mut()),
        }
    }
}

// ----------------------------------------------------------------------
// Client connections

/// A client's connection to an origin, counted by its slot.
pub(crate) struct ClientConn {
    stream: BufReader<Box<dyn Stream>>,
    slot: ResourceSlot,
    /// `scheme://host[:port]`.
    origin: String,
}

impl ClientConn {
    /// Whether the server has neither closed the connection nor sent
    /// anything since the last response.
    fn is_ready(&mut self) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(&mut self.stream).poll_fill_buf(&mut cx).is_pending()
    }
}

/// A run's idle client connections, kept for later requests to their
/// origin. When a limit forces a collection, [`Pool::clear`] closes them and
/// the servers' kept-alive connections.
pub(crate) struct Pool {
    /// Oldest first, with when each became idle.
    idle: std::sync::Mutex<Vec<(ClientConn, Instant)>>,
    max: usize,
    servers: std::sync::Mutex<Vec<Weak<super::http_server::Shared>>>,
    clock: Arc<dyn crate::platform::Clock>,
}

impl Pool {
    pub fn new(max_open_resources: usize, clock: Arc<dyn crate::platform::Clock>) -> Arc<Pool> {
        let max = (max_open_resources / 4).min(POOL_MAX);
        Arc::new(Pool { idle: std::sync::Mutex::default(), max, servers: std::sync::Mutex::default(), clock })
    }

    /// Lets [`Pool::clear`] close the idle connections of `server`.
    pub(super) fn watch(&self, server: &Arc<super::http_server::Shared>) {
        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        servers.retain(|s| s.strong_count() > 0);
        servers.push(Arc::downgrade(server));
    }

    fn idle(&self) -> std::sync::MutexGuard<'_, Vec<(ClientConn, Instant)>> {
        self.idle.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The most recently used idle connection to `origin` that is ready for
    /// a request, after closing those idle for longer than [`POOL_IDLE`].
    fn take(&self, origin: &str) -> Option<ClientConn> {
        let now = self.clock.now();
        let mut idle = self.idle();
        idle.retain(|(_, since)| now.saturating_duration_since(*since) < POOL_IDLE);
        while let Some(i) = idle.iter().rposition(|(c, _)| c.origin == origin) {
            let (mut conn, _) = idle.remove(i);
            if conn.is_ready() {
                return Some(conn);
            }
        }
        None
    }

    /// Keeps `conn` idle, closing the oldest beyond the pool's bounds.
    fn put(&self, mut conn: ClientConn) {
        if self.max == 0 || !conn.stream.buffer().is_empty() || !conn.is_ready() {
            net::shutdown_now(conn.stream.get_mut());
            return;
        }
        let mut idle = self.idle();
        if idle.iter().filter(|(c, _)| c.origin == conn.origin).count() >= POOL_PER_ORIGIN {
            let oldest = idle.iter().position(|(c, _)| c.origin == conn.origin).expect("counted");
            idle.remove(oldest);
        }
        if idle.len() >= self.max {
            idle.remove(0);
        }
        let now = self.clock.now();
        idle.push((conn, now));
    }

    /// Closes every idle connection.
    pub fn clear(&self) {
        self.idle().clear();
        let servers = std::mem::take(&mut *self.servers.lock().unwrap_or_else(|e| e.into_inner()));
        for server in servers.iter().filter_map(Weak::upgrade) {
            server.close_idle();
            self.watch(&server);
        }
    }
}

/// Connects to `url` at an address the policy allows.
async fn connect(
    policy: &Policy,
    network: &dyn Network,
    tls: &TlsSettings,
    url: &Url,
    slot: ResourceSlot,
) -> Result<ClientConn, IoFailure> {
    let (tcp, _) = net::connect_checked(policy, network, &url.host, url.port).await?;
    let stream = if url.scheme == "https" { Box::new(net::tls::connect(tls, &url.host, tcp).await?) } else { tcp };
    Ok(ClientConn { stream: BufReader::with_capacity(net::CONN_BUFFER, stream), slot, origin: url.origin() })
}

/// A response's head.
struct ResponseHead {
    status: i64,
    headers: Vec<(String, String)>,
    framing: Framing,
    /// Whether the connection may carry another request after the body.
    persists: bool,
}

/// Sends `request` on `conn` and reads the response's head. A failure
/// before any of the response arrives returns the slot, to send the request
/// again on a new connection.
async fn exchange(
    mut conn: ClientConn,
    request: &[u8],
    head_only: bool,
    res: &mut Reservation,
) -> Result<(ClientConn, ResponseHead), (IoFailure, Option<ResourceSlot>)> {
    let sent = async {
        conn.stream.write_all(request).await?;
        conn.stream.flush().await?;
        if conn.stream.fill_buf().await?.is_empty() {
            return Err(IoFailure::new("io", "connection closed before a response was received"));
        }
        Ok(())
    };
    if let Err(e) = sent.await {
        return Err((e, Some(conn.slot)));
    }
    match read_response_head(&mut conn.stream, head_only, res).await {
        Ok(head) => Ok((conn, head)),
        Err(e) => Err((e, None)),
    }
}

/// Reads a response's head (skipping interim responses).
async fn read_response_head<R: AsyncBufRead + Unpin>(
    stream: &mut R,
    head_only: bool,
    res: &mut Reservation,
) -> Result<ResponseHead, IoFailure> {
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
        let persists = persists(version, &headers) && status != 101 && framing != Framing::UntilClose;
        res.absorb(head_res);
        return Ok(ResponseHead { status, headers, framing, persists });
    }
}

/// Methods that may be sent again when a reused connection closes before
/// responding (RFC 9110 §9.2.2).
const IDEMPOTENT: &[&str] = &["GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE"];

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
    let mut headers = match header_pairs(vm, a[2])? {
        Ok(h) => h,
        Err(e) => return Ok(vm.err_value("invalid", &e)),
    };
    if let Some(denied) = check_permission(vm, url.capability(&method))? {
        return Ok(denied);
    }
    if let Err(reason) = filter_request(vm, &url, &mut headers) {
        return Ok(vm.err_value("permission_denied", &reason));
    }
    // The request is held outside the heap while it is sent.
    let mut res = super::or_return!(vm.reserve_io(vm.heap.bytes_of(a[3]).len()));
    let pool = vm.host.http_pool.clone();
    let idle = pool.take(&url.origin());
    let slot = match idle {
        Some(_) => None,
        None => {
            let buffers = if url.scheme == "https" { net::CONN_BUFFER + net::TLS_BUFFERS } else { net::CONN_BUFFER };
            Some(super::or_return!(vm.resource_slot_with(buffers, "a connection")))
        }
    };
    let body = vm.heap.bytes_of(a[3]);
    vm.charge_bytes(body.len())?;
    let close = connection_has(&headers, "close");
    let sized = !body.is_empty() || ["POST", "PUT", "PATCH"].iter().any(|m| method.eq_ignore_ascii_case(m));
    let mut request =
        request_head(&method, &url, &headers, if sized { Delimit::Length(body.len() as u64) } else { Delimit::None }, close).into_bytes();
    request.extend_from_slice(body);
    if let Err(e) = res.ensure(request.len()) {
        return Ok(vm.err_value("limit", &format!("cannot buffer the request: {e}")));
    }
    let timeout = Duration::from_nanos(a[4].as_int().max(0) as u64);
    let head_only = method.eq_ignore_ascii_case("HEAD");
    let replayable = IDEMPOTENT.iter().any(|m| method.eq_ignore_ascii_case(m));
    let max_body = vm.host.limits.max_http_body;
    let (policy, network) = (vm.host.policy.clone(), vm.host.network.clone());
    let (tls, clock) = (vm.host.tls.clone(), vm.host.clock.clone());
    let body_clock = clock.clone();
    suspend(
        async move {
            let mut head_res = res.sibling();
            let run = async {
                let mut slot = slot;
                if let Some(conn) = idle {
                    match exchange(conn, &request, head_only, &mut head_res).await {
                        Ok(r) => return Ok(r),
                        Err((_, Some(s))) if replayable => slot = Some(s),
                        Err((e, _)) => return Err(e),
                    }
                }
                let conn = connect(&policy, &*network, &tls, &url, slot.expect("a slot for a new connection")).await?;
                exchange(conn, &request, head_only, &mut head_res).await.map_err(|(e, _)| e)
            };
            let r = match crate::platform::timeout(&*clock, timeout, run).await {
                Some(r) => r?,
                None => return Err(IoFailure::new("timeout", format!("request to {text} timed out"))),
            };
            drop(res);
            Ok((r, head_res))
        },
        move |vm, ((conn, head), head_res): ((ClientConn, ResponseHead), Reservation)| {
            drop(head_res);
            let ResponseHead { status, headers, framing, persists } = head;
            let headers = header_list(vm, headers)?;
            let reuse = (persists && !close).then(|| Arc::downgrade(&pool));
            let source = Source::Owned { conn: Some(conn), reuse };
            let mut framed = Framed { source, framing, timeout, clock: body_clock, continue_first: false };
            let reader = if framing == Framing::Done {
                framed.end(true);
                BodyReader::empty(max_body)
            } else {
                BodyReader::new(Box::new(framed), max_body, None)
            };
            let body = alloc_body(vm, reader);
            Ok(vm.heap.new_tuple(&[Value::Int(status), headers, body]))
        },
    )
}

/// Passes the headers of a request to `url` through the host's request filter, if it has one.
fn filter_request(vm: &Vm, url: &Url, headers: &mut Vec<(String, String)>) -> Result<(), String> {
    let Some(filter) = &vm.host.request_filter else { return Ok(()) };
    let target = crate::engine::RequestTarget { scheme: url.scheme.clone(), host: url.host.clone(), port: url.port };
    filter(&target, headers)
}

/// A request's head, with a `Connection: close` header if `close`.
fn request_head(method: &str, url: &Url, headers: &[(String, String)], body: Delimit, close: bool) -> String {
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut head = format!("{method} {} HTTP/1.1\r\n", url.target);
    if !has("host") {
        head += &format!("Host: {}\r\n", url.authority());
    }
    push_headers(&mut head, headers);
    if !has("user-agent") {
        head += &format!("User-Agent: {USER_AGENT}\r\n");
    }
    end_head(head, body, close.then_some("close"))
}

// ----------------------------------------------------------------------
// Bodies written as they go

/// The `http.Outgoing` type: a body that a script writes (an upload's or a
/// reply's), and the head still to send before it.
pub(super) struct Outgoing {
    pub head: Option<Vec<u8>>,
    /// `Delimit::None` drops writes (a reply to `HEAD`, or a status without
    /// a body).
    pub delimit: Delimit,
    /// The bytes left of a body with a `Content-Length`.
    pub left: Option<u64>,
    pub finished: bool,
    /// For a reply: what becomes of the connection after the body.
    pub after: Option<super::http_server::After>,
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
pub(super) fn given_length(headers: &[(String, String)]) -> Result<Option<u64>, String> {
    match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        Some((_, v)) => v.trim().parse().map(Some).map_err(|_| format!("invalid content-length {v:?}")),
        None => Ok(None),
    }
}

/// `start_upload(method, url, headers, timeout)`: connects for a streamed
/// request: `(net.Conn, Outgoing)`. The connection is not reused.
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
    let mut headers = match header_pairs(vm, a[2])? {
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
    if let Err(reason) = filter_request(vm, &url, &mut headers) {
        return Ok(vm.err_value("permission_denied", &reason));
    }
    let buffers = if url.scheme == "https" { net::CONN_BUFFER + net::TLS_BUFFERS } else { net::CONN_BUFFER };
    let slot = super::or_return!(vm.resource_slot_with(buffers, "a connection"));
    let delimit = length.map_or(Delimit::Chunked, Delimit::Length);
    let head = request_head(&method, &url, &headers, delimit, true);
    let timeout = Duration::from_nanos(a[3].as_int().max(0) as u64);
    let (policy, network, tls) = (vm.host.policy.clone(), vm.host.network.clone(), vm.host.tls.clone());
    let clock = vm.host.clock.clone();
    let open = async move {
        let (stream, peer) = net::connect_checked(&policy, &*network, &url.host, url.port).await?;
        if url.scheme != "https" {
            return Ok(net::Conn::from_stream(stream, peer));
        }
        let (r, w) = tokio::io::split(net::tls::connect(&tls, &url.host, stream).await?);
        Ok(net::Conn::new(Box::new(r), Box::new(w), peer.to_string()))
    };
    suspend(
        async move {
            match crate::platform::timeout(&*clock, timeout, open).await {
                Some(r) => r.map(|c| (c, slot)),
                None => Err(IoFailure::new("timeout", format!("connecting to {text} timed out"))),
            }
        },
        move |vm, (conn, slot)| {
            let conn = net::alloc_resource(vm, conn, slot);
            let out = Outgoing { head: Some(head.into_bytes()), delimit, left: length, finished: false, after: None };
            let body = vm.heap.alloc(Obj::Native(Box::new(out)));
            Ok(vm.heap.new_tuple(&[conn, body]))
        },
    )
}

/// `write_body(conn, body, data, last)`: writes `data` to the body (an
/// `Outgoing` on `conn`), then, if `last`, ends it.
fn write_body(vm: &mut Vm, a: &[Value]) -> NResult {
    let n = vm.heap.bytes_of(a[2]).len();
    let last = a[3].as_bool();
    let out = vm.heap.native::<Outgoing>(a[1]).ok_or_else(|| Ctl::internal("not an outgoing body"))?;
    if out.finished {
        return Ok(vm.err_value("closed", "the body has been finished"));
    }
    let left = out.left.map(|left| left.checked_sub(n as u64));
    let sent = out.delimit != Delimit::None;
    match left {
        Some(None) => return Ok(vm.err_value("invalid", "the body is longer than its content-length")),
        Some(Some(more)) if last && more > 0 && sent => {
            return Ok(vm.err_value("invalid", &format!("the body is {more} bytes shorter than its content-length")));
        }
        _ => {}
    }
    let len = out.head.as_ref().map_or(0, Vec::len) + if sent { n + 32 } else { 0 };
    vm.charge_bytes(len)?;
    let res = super::or_return!(vm.reserve_io(len));
    let out = vm.heap.native_mut::<Outgoing>(a[1]).expect("checked above");
    out.left = left.flatten();
    out.finished = last;
    let delimit = out.delimit;
    let after = if last { out.after.take() } else { None };
    let mut data = out.head.take().unwrap_or_default();
    let body = vm.heap.bytes_of(a[2]);
    match delimit {
        Delimit::Chunked => {
            if !body.is_empty() {
                data.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
                data.extend_from_slice(body);
                data.extend_from_slice(b"\r\n");
            }
            if last {
                data.extend_from_slice(b"0\r\n\r\n");
            }
        }
        Delimit::Length(_) | Delimit::Close => data.extend_from_slice(body),
        Delimit::None => {}
    }
    if let Some(after) = after {
        return super::http_server::end_response(vm, a[0], data, res, after);
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
    let clock = vm.host.clock.clone();
    let body_clock = clock.clone();
    let read = async move {
        let mut reader = reader.lock().await;
        let head = read_response_head(&mut *reader, head_only, &mut res).await;
        drop(res);
        head
    };
    suspend(
        h.closed.clone().guard(async move {
            match crate::platform::timeout(&*clock, timeout, read).await {
                Some(r) => r,
                None => Err(IoFailure::new("timeout", "timed out waiting for the response")),
            }
        }),
        move |vm, head: ResponseHead| {
            let headers = header_list(vm, head.headers)?;
            let reader = if head.framing == Framing::Done {
                BodyReader::empty(max_body)
            } else {
                BodyReader::new(Box::new(Framed::on_conn(&h, head.framing, timeout, body_clock, false)), max_body, None)
            };
            let body = alloc_body(vm, reader);
            Ok(vm.heap.new_tuple(&[Value::Int(head.status), headers, body]))
        },
    )
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
