//! The `http` module's server. [`next_request`] reads request heads from
//! many connections at once: newly accepted ones, and kept-alive ones handed
//! back after a response ([`end_response`]).

use super::http::{Delimit, Framed, Framing, Outgoing, end_head, framing, given_length, persists, push_headers, read_head, values};
use super::http_common::*;
use super::net::{self, Detached, finish};
use super::stream::IoFailure;
use crate::engine::Registry;
use crate::network::{Listener, NetworkFuture, Stream};
use crate::vm::iomem::{IoMemory, Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, NResult, Vm};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Poll, Waker};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// How long a request's head may take, from accept or from its first byte.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a kept-alive connection waits for its next request.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a server's body read waits for data.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn register(r: &mut Registry) {
    r.add_builtin("http.serve", serve);
    r.add_builtin("http.Server.addr", |vm, a| {
        let addr = server(vm, a[0])?.addr;
        Ok(vm.new_value(addr))
    });
    r.add_method("http.Server.close", |vm, a| {
        vm.heap.native_mut::<Server>(a[0]).ok_or_else(|| Ctl::internal("not a server"))?.close();
        Ok(Value::Unit)
    });
    r.add_builtin("http.next_request", next_request);
    r.add_builtin("http.write_response", write_response);
    r.add_builtin("http.reply_body", reply_body);
}

// ----------------------------------------------------------------------
// Server

/// Reads a request's head; `None` if the connection was answered with an
/// error or closed.
type Reading = Pin<Box<dyn Future<Output = Option<Received>> + Send>>;

/// A connection whose request's head is being read.
struct Pending {
    reading: Reading,
    state: Arc<ReadState>,
}

/// What a reading and the server that drives it share.
#[derive(Default)]
struct ReadState {
    /// Makes the reading give up.
    evict: AtomicBool,
    /// A kept-alive connection, until its next request starts to arrive.
    waiting: std::sync::Mutex<Option<Detached>>,
}

impl ReadState {
    fn evicted(&self) -> impl Future<Output = ()> + '_ {
        // Registers no waker: whoever sets `evict` polls the reading next.
        std::future::poll_fn(|_| if self.evict.load(Ordering::Relaxed) { Poll::Ready(()) } else { Poll::Pending })
    }

    fn take_waiting(&self) -> Option<Detached> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The connections a server reads requests from.
#[derive(Default)]
pub(super) struct Shared {
    /// Locked by the `next_request` call that drives the readings; oldest first.
    reads: tokio::sync::Mutex<Vec<Pending>>,
    /// Readings handed back but not yet in `reads`, and the driving call's waker.
    returned: std::sync::Mutex<(Vec<Pending>, Option<Waker>)>,
    /// The states of handed-back readings, for [`Shared::close_idle`].
    waiting: std::sync::Mutex<Vec<Weak<ReadState>>>,
}

impl Shared {
    fn returned(&self) -> std::sync::MutexGuard<'_, (Vec<Pending>, Option<Waker>)> {
        self.returned.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<Weak<ReadState>>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits for `conn`'s next request.
    fn hand_back(&self, conn: Detached, mem: Arc<IoMemory>) {
        let reader = conn.reader.clone();
        let state = Arc::new(ReadState { evict: AtomicBool::new(false), waiting: std::sync::Mutex::new(Some(conn)) });
        let idle_until = tokio::time::Instant::now() + IDLE_TIMEOUT;
        {
            let mut waiting = self.waiting();
            waiting.retain(|s| s.strong_count() > 0);
            waiting.push(Arc::downgrade(&state));
        }
        let reading = Box::pin(read_next(reader, mem, state.clone(), idle_until));
        let mut returned = self.returned();
        returned.0.push(Pending { reading, state });
        if let Some(w) = returned.1.take() {
            w.wake();
        }
    }

    /// Closes the connections waiting for their next request.
    pub(super) fn close_idle(&self) {
        for state in self.waiting().drain(..).filter_map(|s| s.upgrade()) {
            if state.take_waiting().is_some() {
                state.evict.store(true, Ordering::Relaxed);
            }
        }
        if let Some(w) = self.returned().1.take() {
            w.wake();
        }
    }
}

/// The `http.Server` type.
struct Server {
    listener: Option<Arc<dyn Listener>>,
    slot: Option<ResourceSlot>,
    addr: net::SocketAddr,
    closed: watch::Sender<bool>,
    shared: Arc<Shared>,
}

impl Server {
    fn new(socket: Box<dyn Listener>, shared: Arc<Shared>) -> std::io::Result<Server> {
        let addr = socket.local_addr()?.into();
        let closed = watch::Sender::new(false);
        Ok(Server { listener: Some(socket.into()), slot: None, addr, closed, shared })
    }

    /// Stops listening and closes the connections being read (or, if a
    /// `next_request` is pending, when it ends).
    fn close(&mut self) {
        self.closed.send_replace(true);
        self.listener = None;
        self.slot = None;
        self.shared = Arc::default();
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
    vm.heap.native::<Server>(v).ok_or_else(|| Ctl::internal("not a server"))
}

fn serve(vm: &mut Vm, a: &[Value]) -> NResult {
    let shared = Arc::default();
    vm.host.http_pool.watch(&shared);
    net::listen_with(vm, a[0], a[1], move |socket| Server::new(socket, shared))
}

/// Tells the oldest readings beyond the newest `cap` to give up.
fn evict_beyond(reads: &[Pending], cap: usize) {
    let reading: Vec<&Pending> = reads.iter().filter(|p| !p.state.evict.load(Ordering::Relaxed)).collect();
    for p in &reading[..reading.len().saturating_sub(cap)] {
        p.state.evict.store(true, Ordering::Relaxed);
    }
}

/// Accepts connections and reads request heads until a complete request is available.
fn next_request(vm: &mut Vm, a: &[Value]) -> NResult {
    let srv = server(vm, a[0])?;
    let Some(socket) = srv.listener.clone() else { return Err(Ctl::native_err("closed", "server closed")) };
    let closed = net::CloseSignal::of(&srv.closed);
    let shared = srv.shared.clone();
    let weak = Arc::downgrade(&shared);
    let mem = vm.heap.io_memory.clone();
    let cap = (mem.max_resources() / 2).max(1);
    let serve = async move {
        let mut reads = shared.reads.lock().await;
        let mut retry: Option<Pin<Box<tokio::time::Sleep>>> = None;
        let mut accepting: Option<NetworkFuture<'static, (Box<dyn Stream>, std::net::SocketAddr)>> = None;
        std::future::poll_fn(|cx| {
            loop {
                let mut progress = false;
                {
                    let mut returned = shared.returned();
                    returned.1 = Some(cx.waker().clone());
                    if !returned.0.is_empty() {
                        reads.append(&mut returned.0);
                        evict_beyond(&reads, cap);
                        progress = true;
                    }
                }
                if retry.as_mut().is_none_or(|t| t.as_mut().poll(cx).is_ready()) {
                    retry = None;
                    match ResourceSlot::try_new(&mem) {
                        Some(slot) => match accepting.get_or_insert_with(|| accept(socket.clone())).as_mut().poll(cx) {
                            Poll::Ready(Ok((stream, peer))) => {
                                accepting = None;
                                let state = Arc::new(ReadState::default());
                                let reading = Box::pin(read_connection(stream, peer, mem.clone(), slot, state.clone()));
                                reads.push(Pending { reading, state });
                                evict_beyond(&reads, cap);
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
    let clock = vm.host.clock.clone();
    Err(Ctl::suspend(Box::pin(async move {
        finish(closed.guard(serve).await, move |vm, received: Received| {
            let Received { head: (method, path, query, headers), framing, expects_continue, persists, http10, conn, res } = received;
            drop(res);
            let fields = [method, path, query].map(|s| vm.new_str(s));
            let headers = header_list(vm, headers)?;
            let conn = conn.alloc(vm);
            let handle = vm.heap.native::<net::Conn>(conn).and_then(net::Conn::handle).expect("a new connection is open");
            let body = if framing == Framing::Done {
                BodyReader::empty(max_body)
            } else {
                BodyReader::new(Box::new(Framed::on_conn(&handle, framing, BODY_TIMEOUT, clock, expects_continue)), max_body, None)
            };
            let body = alloc_body(vm, body);
            let exchange = vm.heap.alloc(Obj::Native(Box::new(Exchange { persists, http10, server: weak })));
            Ok(vm.heap.new_tuple(&[fields[0], fields[1], fields[2], headers, body, conn, exchange]))
        })
    })))
}

/// A request's head, read by the server: method, path, query, headers.
type RawHead = (String, String, String, Vec<(String, String)>);

/// A request whose head has been read, with its connection.
struct Received {
    head: RawHead,
    framing: Framing,
    expects_continue: bool,
    /// True if the client connection can be reused.
    persists: bool,
    http10: bool,
    conn: Detached,
    /// The head's buffers, until they become values.
    res: Reservation,
}

/// Sends an error response, giving up after 5 seconds so that a client that
/// does not read cannot hold the slot.
async fn deny(conn: &Detached, status: &str, message: &str) {
    let body = format!("{message}\n");
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let send = async {
        let mut w = conn.writer.lock().await;
        w.write_all(response.as_bytes()).await?;
        w.flush().await
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), send).await;
}

fn accept(socket: Arc<dyn Listener>) -> NetworkFuture<'static, (Box<dyn Stream>, std::net::SocketAddr)> {
    Box::pin(async move { socket.accept().await })
}

/// Reads the request of a newly accepted connection (see [`read_request`]).
async fn read_connection(
    stream: Box<dyn Stream>,
    peer: std::net::SocketAddr,
    mem: Arc<IoMemory>,
    mut slot: ResourceSlot,
    state: Arc<ReadState>,
) -> Option<Received> {
    let (r, w) = tokio::io::split(stream);
    let enough = slot.reserve_buffers(net::CONN_BUFFER).is_ok();
    let conn = Detached::new(net::read_half(Box::new(r)), Box::new(w), peer.into(), slot);
    if !enough {
        deny(&conn, "503 Service Unavailable", "the server is out of memory").await;
        return None;
    }
    read_request(conn, mem, state).await
}

/// Reads the next request of the kept-alive connection in `state`, whose
/// read half is `reader`. Closes it if nothing arrives by `idle_until` or
/// the reading is evicted.
async fn read_next(
    reader: net::SharedReader,
    mem: Arc<IoMemory>,
    state: Arc<ReadState>,
    idle_until: tokio::time::Instant,
) -> Option<Received> {
    let first = async {
        let mut r = reader.lock().await;
        matches!(tokio::time::timeout_at(idle_until, r.fill_buf()).await, Ok(Ok(buf)) if !buf.is_empty())
    };
    let arrived = tokio::select! {
        arrived = first => arrived,
        () = state.evicted() => false,
    };
    drop(reader);
    let conn = state.take_waiting()?;
    if !arrived {
        conn.close();
        return None;
    }
    read_request(conn, mem, state).await
}

/// Reads the head of the next request on `conn`. A connection that does
/// not deliver it within [`HEAD_TIMEOUT`], or gives up for a newer one, is
/// answered with `408 Request Timeout`.
async fn read_request(conn: Detached, mem: Arc<IoMemory>, state: Arc<ReadState>) -> Option<Received> {
    let mut res = Reservation::new(&mem);
    let read = async {
        // Bytes buffered past the head stay in this reader for the body.
        let mut reader = conn.reader.lock().await;
        match tokio::time::timeout(HEAD_TIMEOUT, read_request_head(&mut *reader, &mut res)).await {
            Ok(r) => r,
            Err(_) => Err(IoFailure::new("timeout", "timed out reading the request")),
        }
    };
    let result = tokio::select! {
        r = read => r,
        () = state.evicted() => Err(IoFailure::new("timeout", "closed to serve newer connections")),
    };
    match result {
        Ok((head, framing, expects_continue, version)) => {
            let http10 = version == "HTTP/1.0";
            let persists = persists(&version, &head.3);
            Some(Received { head, framing, expects_continue, persists, http10, conn, res })
        }
        Err(e) => {
            drop(res);
            match e.kind {
                "closed" => {}
                "timeout" => deny(&conn, "408 Request Timeout", &e.message).await,
                _ => deny(&conn, "400 Bad Request", &e.message).await,
            }
            conn.close();
            None
        }
    }
}

/// Reads a request's head from `reader`: the head, how its body is framed,
/// whether the client expects `100 Continue` before sending it, and the
/// HTTP version.
async fn read_request_head<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    res: &mut Reservation,
) -> Result<(RawHead, Framing, bool, String), IoFailure> {
    let (start, headers) = read_head(&mut *reader, res).await?;
    let parts: Vec<&str> = start.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(IoFailure::new("http", format!("malformed request line {start:?}")));
    };
    if !is_token(method) || !version.starts_with("HTTP/1.") || target.is_empty() {
        return Err(IoFailure::new("http", format!("malformed request line {start:?}")));
    }
    // Absolute-form targets (`GET http://host/path`) contain the path after the authority.
    let absolute = (!target.starts_with('/')).then(|| super::uri::parse(target, true).ok()).flatten().filter(|p| p.authority.is_some());
    let (path, query) = match &absolute {
        Some(p) => (if p.path.is_empty() { "/" } else { p.path.as_str() }, p.query.as_deref().unwrap_or("")),
        None => target.split_once('?').unwrap_or((target, "")),
    };
    let expects_continue = values(&headers, "expect").any(|e| e.eq_ignore_ascii_case("100-continue"));
    let framing = framing(&headers, false)?;
    let version = version.to_string();
    let head = (method.to_string(), path.to_string(), query.to_string(), headers);
    Ok((head, framing, expects_continue, version))
}

// ----------------------------------------------------------------------
// Responses

/// The `http.Exchange` type: what the response to a request needs to know
/// about its connection.
struct Exchange {
    /// True if the client connection can be reused.
    persists: bool,
    /// Whether the client speaks HTTP/1.0, which has no chunked encoding.
    http10: bool,
    server: Weak<Shared>,
}

impl NativeObject for Exchange {
    fn type_name(&self) -> &'static str {
        "Exchange"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// What becomes of a connection once a response has been written to it.
pub(super) struct After {
    /// Whether it waits for the client's next request (otherwise it is closed).
    persists: bool,
    server: Weak<Shared>,
}

/// True if the connection is reused for subsequent requests.
fn after(vm: &Vm, exchange: Value, body: Value, delimit: Delimit) -> Result<(After, Option<&'static str>), Ctl> {
    let ex = vm.heap.native::<Exchange>(exchange).ok_or_else(|| Ctl::internal("not an exchange"))?;
    let complete = vm.heap.native::<Body>(body).ok_or_else(|| Ctl::internal("not a body"))?.is_complete();
    let persists = ex.persists && complete && delimit != Delimit::Close && ex.server.strong_count() > 0;
    let header = match (persists, ex.http10) {
        (false, _) => Some("close"),
        (true, true) => Some("keep-alive"),
        (true, false) => None,
    };
    Ok((After { persists, server: ex.server.clone() }, header))
}

/// Takes the connection out of `net.Conn` `c`, writes `data` (the end of a
/// response) to it, then hands it back to the server or closes it.
pub(super) fn end_response(vm: &mut Vm, c: Value, data: Vec<u8>, res: Reservation, after: After) -> NResult {
    let conn = net::detach(vm, c)?;
    let mem = vm.heap.io_memory.clone();
    net::suspend(
        async move {
            let written = async {
                let mut w = conn.writer.lock().await;
                w.write_all(&data).await?;
                w.flush().await
            };
            let r = written.await;
            drop(res);
            match after.server.upgrade() {
                Some(server) if after.persists && r.is_ok() => server.hand_back(conn, mem),
                _ => conn.close(),
            }
            Ok(r?)
        },
        |_, ()| Ok(Value::Unit),
    )
}

/// A response's status and headers.
struct Parts {
    status: i64,
    headers: Vec<(String, String)>,
}

fn validate_response(vm: &Vm, status: Value, headers: Value) -> Result<Parts, Ctl> {
    let status = status.as_int();
    if !(100..=999).contains(&status) {
        return Err(Ctl::native_err("invalid", format!("invalid HTTP status {status}")));
    }
    Ok(Parts { status, headers: header_pairs(vm, headers)? })
}

/// A response's start line and headers, without framing headers.
fn status_head(status: i64, headers: &[(String, String)]) -> String {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    push_headers(&mut head, headers);
    head
}

/// Whether a response with `status` has a body (RFC 9112 §6.3).
fn has_body(status: i64) -> bool {
    status >= 200 && status != 204 && status != 304
}

/// `write_response(conn, exchange, request_body, status, body, headers,
/// include_body)`: writes a response with a `content-length` to `conn`.
fn write_response(vm: &mut Vm, a: &[Value]) -> NResult {
    let Parts { status, headers } = validate_response(vm, a[3], a[5])?;
    let has_body = has_body(status);
    let delimit = if has_body { Delimit::Length(vm.heap.bytes_of(a[4]).len() as u64) } else { Delimit::None };
    let (after, connection) = after(vm, a[1], a[2], delimit)?;
    let head = end_head(status_head(status, &headers), delimit, connection);
    let body = if has_body && a[6].as_bool() { vm.heap.bytes_of(a[4]) } else { &[] };
    let len = head.len().saturating_add(body.len());
    vm.charge_bytes(len)?;
    let res = vm.reserve_io(len)?;
    let body = if has_body && a[6].as_bool() { vm.heap.bytes_of(a[4]) } else { &[] };
    let mut data = head.into_bytes();
    data.extend_from_slice(body);
    end_response(vm, a[0], data, res, after)
}

/// `reply_body(exchange, request_body, status, headers, include_body)`: the
/// `Outgoing` of a streamed response.
fn reply_body(vm: &mut Vm, a: &[Value]) -> NResult {
    let Parts { status, headers } = validate_response(vm, a[2], a[3])?;
    let length = match given_length(&headers) {
        Ok(n) => n,
        Err(e) => return Err(Ctl::native_err("invalid", e)),
    };
    let http10 = vm.heap.native::<Exchange>(a[0]).ok_or_else(|| Ctl::internal("not an exchange"))?.http10;
    let advertised = match (has_body(status), length) {
        (false, _) => Delimit::None,
        (true, Some(n)) => Delimit::Length(n),
        (true, None) if http10 => Delimit::Close,
        (true, None) => Delimit::Chunked,
    };
    let (after, connection) = after(vm, a[0], a[1], advertised)?;
    let head = end_head(status_head(status, &headers), advertised, connection);
    vm.charge_bytes(head.len())?;
    let delimit = if a[4].as_bool() { advertised } else { Delimit::None };
    let out = Outgoing { head: Some(head.into_bytes()), delimit, left: length, finished: false, after: Some(after) };
    let out = vm.heap.alloc(Obj::Native(Box::new(out)));
    Ok(vm.ok(out))
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
