//! The `net` module: TCP listeners and connections, and TLS client connections.
//!
//! A connection is split into a buffered read half and a write half, each
//! behind its own async mutex, so one task can read while another writes.
//! Closing a socket wakes every pending operation on it (they fail with kind
//! `"closed"`) and releases the OS socket once those operations have finished.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::{Capability, Decision, Policy, is_private_address};
use crate::vm::iomem::{MemoryLimit, Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, Vm};
use std::any::Any;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};

pub fn register(r: &mut Registry) {
    r.source("net", ModKind::Std, include_str!("net.sand"));
    r.add_builtin("net.listen", listen);
    r.add_builtin("net.connect", connect);
    r.add_builtin("net.connect_tls", connect_tls);
    r.add_builtin("net.Listener.accept", listener_accept);
    r.add_builtin("net.Listener.addr", |vm, a| {
        let addr = listener(vm, a[0])?.addr.clone();
        Ok(vm.new_str(addr))
    });
    r.add_builtin("net.Listener.close", |vm, a| {
        let l = vm.heap.native_mut::<Listener>(a[0]).ok_or_else(|| Ctl::Panic("not a listener".into()))?;
        if l.close() {
            vm.heap.resource_closed();
        }
        Ok(Value::Unit)
    });
    r.add_builtin("net.Conn.read", conn_read);
    r.add_builtin("net.Conn.read_line", conn_read_line);
    r.add_builtin("net.Conn.write", |vm, a| {
        let res = match vm.reserve_io(vm.heap.bytes_of(a[1]).len()) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };
        let data = vm.heap.bytes_of(a[1]).to_vec();
        conn_write(vm, a[0], data, res)
    });
    r.add_builtin("net.Conn.write_str", |vm, a| {
        let res = match vm.reserve_io(vm.str(a[1]).len()) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };
        let data = vm.str(a[1]).as_bytes().to_vec();
        conn_write(vm, a[0], data, res)
    });
    r.add_builtin("net.Conn.peer_addr", |vm, a| {
        let peer = conn(vm, a[0])?.peer.clone();
        Ok(vm.new_str(peer))
    });
    r.add_builtin("net.Conn.close", |vm, a| {
        let c = vm.heap.native_mut::<Conn>(a[0]).ok_or_else(|| Ctl::Panic("not a connection".into()))?;
        let before = c.size();
        if c.close() {
            let after = c.size();
            vm.heap.resource_closed();
            vm.heap.account(before, after);
        }
        Ok(Value::Unit)
    });
}

// ----------------------------------------------------------------------
// Errors and results

/// An error produced off the VM thread, turned into a Sand `Error` on it.
#[derive(Debug)]
pub(crate) struct NetError {
    pub kind: &'static str,
    pub message: String,
}

impl NetError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> NetError {
        NetError { kind, message: message.into() }
    }

    pub fn closed() -> NetError {
        NetError::new("closed", "connection closed")
    }

    /// The `Err(Error)` value for `self`.
    pub fn into_value(self, vm: &mut Vm) -> NResult {
        Ok(vm.err_value(self.kind, &self.message))
    }
}

impl From<MemoryLimit> for NetError {
    fn from(e: MemoryLimit) -> NetError {
        NetError::new("limit", e.to_string())
    }
}

impl From<std::io::Error> for NetError {
    fn from(e: std::io::Error) -> NetError {
        NetError::new(io_kind(&e), e.to_string())
    }
}

/// Builds a finisher that turns `r` into `Ok(..)` (via `ok`) or `Err(Error)`.
pub(crate) fn finish<T: Send + 'static>(r: Result<T, NetError>, ok: impl FnOnce(&mut Vm, T) -> NResult + Send + 'static) -> Finisher {
    Box::new(move |vm: &mut Vm| match r {
        Ok(v) => {
            let v = ok(vm, v)?;
            Ok(vm.ok(v))
        }
        Err(e) => e.into_value(vm),
    })
}

/// Suspends the calling task on `fut`, finishing with `ok` on success.
pub(crate) fn suspend<T: Send + 'static>(
    fut: impl Future<Output = Result<T, NetError>> + Send + 'static,
    ok: impl FnOnce(&mut Vm, T) -> NResult + Send + 'static,
) -> NResult {
    Err(Ctl::Async(Box::pin(async move { finish(fut.await, ok) })))
}

/// Checks `cap`; `Ok(Some(err))` is the `Err(permission_denied)` value to return.
pub(crate) fn check_permission(vm: &mut Vm, cap: Capability) -> Result<Option<Value>, Ctl> {
    if vm.permit(&cap)? { Ok(None) } else { Ok(Some(vm.err_value("permission_denied", &format!("permission denied: {cap}")))) }
}

/// Takes a slot for a socket about to be opened (or used) by a pending
/// operation, counted against `max_open_resources` together with the open
/// sockets on the heap and the slots of other pending operations. `Err` is
/// the `Err(Error { kind: "limit" })` value to return. The slot is released
/// when dropped: when the operation fails, finishes or is cancelled, or right
/// before its socket becomes a heap object ([`alloc_resource`]).
pub(crate) fn resource_slot(vm: &mut Vm) -> Result<ResourceSlot, Value> {
    let max = vm.host.limits.max_open_resources;
    let mem = vm.heap.io_memory.clone();
    if vm.heap.open_resources.saturating_add(mem.pending_resources()) >= max {
        return Err(vm.err_value("limit", &format!("too many open resources (limit {max})")));
    }
    Ok(ResourceSlot::new(&mem))
}

/// Like [`resource_slot`], for a connection: also reserves `buffers` bytes
/// for its fixed buffers ([`CONN_BUFFER`], ...), which its `Conn` object
/// counts once it exists.
pub(crate) fn conn_slot(vm: &mut Vm, buffers: usize) -> Result<ResourceSlot, Value> {
    let mut slot = resource_slot(vm)?;
    vm.heap.io_memory.publish_heap(vm.heap.used());
    match slot.reserve_buffers(buffers) {
        Ok(()) => Ok(slot),
        Err(e) => Err(vm.err_value("limit", &format!("cannot open a connection: its buffers ({buffers} bytes) do not fit: {e}"))),
    }
}

/// Allocates a socket object in place of the slot that counted it.
pub(crate) fn alloc_resource(vm: &mut Vm, obj: Box<dyn NativeObject>, slot: ResourceSlot) -> Value {
    drop(slot);
    vm.heap.alloc(Obj::Native(obj))
}

/// Like [`suspend`] for operations that produce a socket object, counted by
/// `slot` until then.
fn suspend_resource<T: NativeObject>(slot: ResourceSlot, fut: impl Future<Output = Result<T, NetError>> + Send + 'static) -> NResult {
    Err(Ctl::Async(Box::pin(async move {
        let r = fut.await;
        Box::new(move |vm: &mut Vm| match r {
            Ok(obj) => {
                let v = alloc_resource(vm, Box::new(obj), slot);
                Ok(vm.ok(v))
            }
            Err(e) => e.into_value(vm),
        }) as Finisher
    })))
}

// ----------------------------------------------------------------------
// Close signalling

/// Lets pending operations observe that their socket was closed.
#[derive(Clone)]
pub(crate) struct CloseSignal(watch::Receiver<bool>);

impl CloseSignal {
    /// Runs `op` unless (or until) the socket is closed.
    pub async fn guard<T>(mut self, op: impl Future<Output = Result<T, NetError>>) -> Result<T, NetError> {
        if *self.0.borrow() {
            return Err(NetError::closed());
        }
        tokio::select! {
            r = op => r,
            _ = self.0.wait_for(|closed| *closed) => Err(NetError::closed()),
        }
    }
}

// ----------------------------------------------------------------------
// Listener

pub(crate) struct Listener {
    socket: Option<Arc<TcpListener>>,
    addr: String,
    closed: watch::Sender<bool>,
}

impl Listener {
    fn new(socket: TcpListener) -> std::io::Result<Listener> {
        let addr = socket.local_addr()?.to_string();
        Ok(Listener { socket: Some(Arc::new(socket)), addr, closed: watch::Sender::new(false) })
    }

    /// Closes the listener; true if it was open.
    fn close(&mut self) -> bool {
        self.closed.send_replace(true);
        self.socket.take().is_some()
    }
}

impl NativeObject for Listener {
    fn type_name(&self) -> &'static str {
        "Listener"
    }
    fn is_resource(&self) -> bool {
        self.socket.is_some()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn listener(vm: &Vm, v: Value) -> Result<&Listener, Ctl> {
    vm.heap.native::<Listener>(v).ok_or_else(|| Ctl::Panic("not a listener".into()))
}

/// The socket of the listener `v` and its close signal, for accepting
/// connections; `None` if the listener is closed.
pub(crate) fn listener_socket(vm: &Vm, v: Value) -> Result<Option<(Arc<TcpListener>, CloseSignal)>, Ctl> {
    let l = listener(vm, v)?;
    Ok(l.socket.clone().map(|s| (s, CloseSignal(l.closed.subscribe()))))
}

fn listen(vm: &mut Vm, a: &[Value]) -> NResult {
    let addr = vm.str(a[0]).to_string();
    if let Some(denied) = check_permission(vm, Capability::NetListen(addr.clone()))? {
        return Ok(denied);
    }
    let slot = match resource_slot(vm) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    suspend_resource(slot, async move { Ok(Listener::new(TcpListener::bind(addr).await?)?) })
}

fn listener_accept(vm: &mut Vm, a: &[Value]) -> NResult {
    let l = listener(vm, a[0])?;
    let Some(socket) = l.socket.clone() else { return Ok(vm.err_value("closed", "listener closed")) };
    let closed = CloseSignal(l.closed.subscribe());
    // A pending accept holds a slot for the connection it will produce.
    let slot = match conn_slot(vm, CONN_BUFFER) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    suspend_resource(
        slot,
        closed.guard(async move {
            let (stream, _) = socket.accept().await?;
            Ok(Conn::from_tcp(stream)?)
        }),
    )
}

// ----------------------------------------------------------------------
// Connections

/// The size of a connection's read buffer.
pub(crate) const CONN_BUFFER: usize = 8 << 10;

/// An estimate of the memory a TLS session holds besides the connection's
/// read buffer.
pub(crate) const TLS_BUFFERS: usize = 40 << 10;

pub(crate) type ReadHalf = BufReader<Box<dyn AsyncRead + Send + Unpin>>;
pub(crate) type WriteHalf = Box<dyn AsyncWrite + Send + Unpin>;
type SharedReader = Arc<Mutex<ReadHalf>>;
type SharedWriter = Arc<Mutex<WriteHalf>>;

/// The shareable parts of an open connection.
#[derive(Clone)]
pub(crate) struct ConnHandle {
    pub reader: SharedReader,
    pub writer: SharedWriter,
    pub closed: CloseSignal,
}

pub(crate) struct Conn {
    halves: Option<(SharedReader, SharedWriter)>,
    peer: String,
    closed: watch::Sender<bool>,
    /// Bytes of fixed buffers held while open.
    buffers: usize,
}

/// Makes end of stream final: once the peer has finished sending, later reads
/// report end of stream again instead of consulting the socket. (Some
/// platforms report a reset received after the peer's FIN — e.g. in answer
/// to data written after it closed — as an error on the next read.)
struct StickyEof<R> {
    inner: R,
    eof: bool,
}

impl<R: AsyncRead + Unpin> AsyncRead for StickyEof<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.eof || buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = r
            && buf.filled().len() == before
        {
            self.eof = true;
        }
        r
    }
}

impl Conn {
    /// A connection over `reader` and `writer`, which hold `extra` bytes of
    /// buffers besides the connection's own read buffer ([`CONN_BUFFER`]).
    /// Its slot has reserved them all.
    pub fn new(reader: Box<dyn AsyncRead + Send + Unpin>, writer: WriteHalf, peer: String, extra: usize) -> Conn {
        let reader: Box<dyn AsyncRead + Send + Unpin> = Box::new(StickyEof { inner: reader, eof: false });
        Conn {
            halves: Some((Arc::new(Mutex::new(BufReader::with_capacity(CONN_BUFFER, reader))), Arc::new(Mutex::new(writer)))),
            peer,
            closed: watch::Sender::new(false),
            buffers: CONN_BUFFER + extra,
        }
    }

    fn from_tcp(stream: TcpStream) -> std::io::Result<Conn> {
        let peer = stream.peer_addr()?.to_string();
        let (r, w) = stream.into_split();
        Ok(Conn::new(Box::new(r), Box::new(w), peer, 0))
    }

    /// The handle for an operation, or `None` if the connection is closed.
    pub fn handle(&self) -> Option<ConnHandle> {
        let (reader, writer) = self.halves.clone()?;
        Some(ConnHandle { reader, writer, closed: CloseSignal(self.closed.subscribe()) })
    }

    /// Closes the connection; true if it was open.
    fn close(&mut self) -> bool {
        self.closed.send_replace(true);
        self.halves.take().is_some()
    }
}

impl NativeObject for Conn {
    fn type_name(&self) -> &'static str {
        "Conn"
    }
    fn size(&self) -> usize {
        64 + if self.halves.is_some() { self.buffers } else { 0 }
    }
    fn is_resource(&self) -> bool {
        self.halves.is_some()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn conn(vm: &Vm, v: Value) -> Result<&Conn, Ctl> {
    vm.heap.native::<Conn>(v).ok_or_else(|| Ctl::Panic("not a connection".into()))
}

/// The operation handle of the connection `v`, or the `Err(closed)` value.
pub(crate) fn conn_handle(vm: &mut Vm, v: Value) -> Result<Result<ConnHandle, Value>, Ctl> {
    match conn(vm, v)?.handle() {
        Some(h) => Ok(Ok(h)),
        None => Ok(Err(vm.err_value("closed", "connection closed"))),
    }
}

/// Splits `addr` (`host:port` or `[v6]:port`) into host and port.
fn split_host_port(addr: &str) -> Option<(String, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let host = match host.strip_prefix('[') {
        Some(h) => h.strip_suffix(']')?,
        None if host.contains(':') => return None,
        None => host,
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

/// Resolves `host` once and returns the addresses the policy allows
/// (`Capability::NetAddr`), so that a name cannot lead to an address the
/// policy forbids (e.g. through DNS rebinding). Fails with kind
/// `"permission_denied"` if every address is denied.
pub(crate) async fn resolve_allowed(policy: &Policy, host: &str, port: u16) -> Result<Vec<SocketAddr>, NetError> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port)).await?.collect(),
    };
    let mut denied = None;
    let mut allowed = Vec::with_capacity(addrs.len());
    for addr in addrs {
        match policy.check(&Capability::NetAddr { host: host.to_string(), addr }) {
            Decision::Deny => {
                denied.get_or_insert(addr);
            }
            Decision::Allow | Decision::Prompt => allowed.push(addr),
        }
    }
    match denied {
        Some(addr) if allowed.is_empty() => {
            let class = if is_private_address(addr.ip()) { " (private or internal)" } else { "" };
            Err(NetError::new(
                "permission_denied",
                format!("permission denied: connect to {host}:{port}: address {}{class} is not allowed", addr.ip()),
            ))
        }
        _ if allowed.is_empty() => Err(NetError::new("not_found", format!("no addresses found for {host}"))),
        _ => Ok(allowed),
    }
}

/// Opens a TCP connection to one of the addresses of `host` the policy allows.
pub(crate) async fn connect_checked(policy: &Policy, host: &str, port: u16) -> Result<TcpStream, NetError> {
    let addrs = resolve_allowed(policy, host, port).await?;
    // Connects to the checked addresses only (in order); never resolves again.
    Ok(TcpStream::connect(&addrs[..]).await?)
}

/// Like [`connect_checked`], failing with kind `"timeout"` after `timeout`.
pub(crate) async fn tcp_connect(policy: &Policy, host: &str, port: u16, timeout: Duration) -> Result<TcpStream, NetError> {
    match tokio::time::timeout(timeout, connect_checked(policy, host, port)).await {
        Ok(r) => r,
        Err(_) => Err(NetError::new("timeout", format!("connecting to {host}:{port} timed out"))),
    }
}

fn connect(vm: &mut Vm, a: &[Value]) -> NResult {
    let addr = vm.str(a[0]).to_string();
    let Some((host, port)) = split_host_port(&addr) else {
        return Ok(vm.err_value("invalid", &format!("invalid address {:?} (expected host:port)", super::truncate(&addr, 200))));
    };
    if let Some(denied) = check_permission(vm, Capability::Net { host: host.clone(), port })? {
        return Ok(denied);
    }
    // Counted from the start: a connection attempt holds a socket.
    let slot = match conn_slot(vm, CONN_BUFFER) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let timeout = Duration::from_nanos(a[1].as_int().max(0) as u64);
    let policy = vm.host.policy.clone();
    suspend_resource(slot, async move { Ok(Conn::from_tcp(tcp_connect(&policy, &host, port, timeout).await?)?) })
}

fn connect_tls(vm: &mut Vm, a: &[Value]) -> NResult {
    if !cfg!(feature = "tls") {
        return Ok(vm.err_value("invalid", "TLS support is not enabled in this build"));
    }
    let host = vm.str(a[0]).trim_start_matches('[').trim_end_matches(']').to_string();
    let Ok(port) = u16::try_from(a[1].as_int()) else {
        return Ok(vm.err_value("invalid", &format!("invalid port {}", a[1].as_int())));
    };
    if let Some(denied) = check_permission(vm, Capability::Net { host: host.clone(), port })? {
        return Ok(denied);
    }
    let slot = match conn_slot(vm, CONN_BUFFER + TLS_BUFFERS) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let timeout = Duration::from_nanos(a[2].as_int().max(0) as u64);
    let policy = vm.host.policy.clone();
    let settings = vm.host.tls.clone();
    // The timeout covers connecting and the handshake, so a peer that
    // accepts but never answers cannot hold the socket indefinitely.
    let target = format!("{host}:{port}");
    let open = async move {
        let tcp = connect_checked(&policy, &host, port).await?;
        let peer = tcp.peer_addr()?.to_string();
        let tls = tls::connect(&settings, &host, tcp).await?;
        let (r, w) = tokio::io::split(tls);
        Ok(Conn::new(Box::new(r), Box::new(w), peer, TLS_BUFFERS))
    };
    suspend_resource(slot, async move {
        match tokio::time::timeout(timeout, open).await {
            Ok(r) => r,
            Err(_) => Err(NetError::new("timeout", format!("TLS connection to {target} timed out"))),
        }
    })
}

fn conn_read(vm: &mut Vm, a: &[Value]) -> NResult {
    let max = a[1].as_int();
    if max <= 0 {
        return Ok(vm.err_value("invalid", "read: max must be positive"));
    }
    // Reads return whatever is available, so a large `max` need not be allocated up front.
    let max = max.min(1 << 20) as usize;
    let h = match conn_handle(vm, a[0])? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    // The buffer is counted against the memory limit while the read is pending.
    let res = match vm.reserve_io(max) {
        Ok(r) => r,
        Err(e) => return Ok(e),
    };
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let mut buf = vec![0; max];
            let n = reader.read(&mut buf).await?;
            buf.truncate(n);
            Ok((buf, res))
        }),
        |vm, (data, res)| {
            drop(res);
            vm.reserve(data.len())?;
            Ok(vm.heap.new_bytes(data))
        },
    )
}

/// Reads a line including its `\n` (if any); `None` at end of stream. Fails
/// with kind `"limit"` if the line exceeds `max` bytes. The line's buffer is
/// reserved in `res`.
pub(crate) async fn read_line_bytes<R: AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
    res: &mut Reservation,
) -> Result<Option<Vec<u8>>, NetError> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        if line.len() + take > max {
            return Err(NetError::new("limit", format!("line longer than {max} bytes")));
        }
        res.reserve_buf_upto(&mut line, take, max)?;
        line.extend_from_slice(&buf[..take]);
        r.consume(take);
        if done {
            return Ok(Some(line));
        }
    }
}

/// Appends everything up to the end of `r` to `data`, whose buffer is
/// reserved in `res` as data arrives and grows to at most `cap` bytes (`r`
/// must end by then, e.g. through `take`).
pub(crate) async fn read_to_end_reserved<R: AsyncRead + Unpin>(
    r: &mut R,
    data: &mut Vec<u8>,
    res: &mut Reservation,
    cap: usize,
) -> Result<(), NetError> {
    loop {
        res.reserve_buf_upto(data, (16 << 10).min(cap.saturating_sub(data.len())), cap)?;
        if r.read_buf(data).await? == 0 {
            return Ok(());
        }
    }
}

/// Removes a trailing `\n` or `\r\n`.
pub(crate) fn strip_eol(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
}

fn conn_read_line(vm: &mut Vm, a: &[Value]) -> NResult {
    let h = match conn_handle(vm, a[0])? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    // The line may grow until it exhausts the memory limit.
    let mut res = vm.io_reservation();
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let Some(mut line) = read_line_bytes(&mut *reader, usize::MAX, &mut res).await? else { return Ok((None, res)) };
            strip_eol(&mut line);
            let line = String::from_utf8(line).map_err(|_| NetError::new("invalid", "line is not valid UTF-8"))?;
            Ok((Some(line), res))
        }),
        |vm, (line, res)| {
            drop(res);
            Ok(match line {
                Some(s) => {
                    vm.reserve(s.len())?;
                    let s = vm.new_str(s);
                    vm.some(s)
                }
                None => vm.none(),
            })
        },
    )
}

/// Writes `data` (whose copy is reserved in `res` until the write is done).
fn conn_write(vm: &mut Vm, c: Value, data: Vec<u8>, res: Reservation) -> NResult {
    let h = match conn_handle(vm, c)? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    suspend(
        h.closed.guard(async move {
            let mut writer = h.writer.lock().await;
            writer.write_all(&data).await?;
            writer.flush().await?;
            drop(res);
            Ok(())
        }),
        |_, ()| Ok(Value::Unit),
    )
}

// ----------------------------------------------------------------------
// TLS

/// Per-engine TLS client settings: extra trusted root certificates (DER)
/// and the client configuration built from them on first use.
#[derive(Clone, Default)]
pub(crate) struct TlsSettings {
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    roots: Arc<Vec<Vec<u8>>>,
    #[cfg(feature = "tls")]
    config: Arc<std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, String>>>,
}

impl TlsSettings {
    /// Settings trusting `extra` in addition to the current roots.
    pub fn with_roots(&self, extra: Vec<Vec<u8>>) -> TlsSettings {
        let mut roots = (*self.roots).clone();
        roots.extend(extra);
        TlsSettings {
            roots: Arc::new(roots),
            #[cfg(feature = "tls")]
            config: Default::default(),
        }
    }
}

#[cfg(feature = "tls")]
pub(crate) mod tls {
    use super::{NetError, TlsSettings};
    use rustls::pki_types::{CertificateDer, ServerName};
    use std::sync::{Arc, OnceLock};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::client::TlsStream;

    /// The bundled web PKI roots plus `extra`.
    fn build(extra: &[Vec<u8>]) -> Result<Arc<rustls::ClientConfig>, String> {
        let mut roots: rustls::RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
        for (i, der) in extra.iter().enumerate() {
            roots.add(CertificateDer::from(der.clone())).map_err(|e| format!("invalid root certificate #{}: {e}", i + 1))?;
        }
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
            .map_err(|e| e.to_string())
    }

    fn connector(settings: &TlsSettings) -> Result<TlsConnector, NetError> {
        // Engines without extra roots share one configuration.
        static DEFAULT: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
        let config = if settings.roots.is_empty() {
            DEFAULT.get_or_init(|| build(&[]))
        } else {
            settings.config.get_or_init(|| build(&settings.roots))
        };
        match config {
            Ok(c) => Ok(TlsConnector::from(c.clone())),
            Err(e) => Err(NetError::new("io", format!("TLS setup failed: {e}"))),
        }
    }

    /// Performs a TLS handshake over `tcp`, verifying the certificate for `host`.
    pub async fn connect(settings: &TlsSettings, host: &str, tcp: TcpStream) -> Result<TlsStream<TcpStream>, NetError> {
        let name = ServerName::try_from(host.to_string())
            .map_err(|_| NetError::new("invalid", format!("invalid TLS server name {:?}", crate::stdlib::truncate(host, 200))))?;
        connector(settings)?.connect(name, tcp).await.map_err(|e| {
            let e = NetError::from(e);
            NetError::new(e.kind, format!("TLS handshake with {host} failed: {}", e.message))
        })
    }
}

#[cfg(not(feature = "tls"))]
pub(crate) mod tls {
    use super::{NetError, TlsSettings};
    use tokio::net::TcpStream;

    /// TLS is unavailable without the `tls` feature.
    pub async fn connect(_settings: &TlsSettings, _host: &str, _tcp: TcpStream) -> Result<TcpStream, NetError> {
        Err(NetError::new("invalid", "TLS support is not enabled in this build"))
    }
}
