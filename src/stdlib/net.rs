//! The `net` module: TCP listeners and connections, and TLS client connections.
//!
//! A connection is split into a buffered read half and a write half, each
//! behind its own async mutex, so one task can read while another writes.
//! Closing a socket wakes every pending operation on it (they fail with kind
//! `"closed"`) and releases the OS socket once those operations have finished.

use super::io_kind;
use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::Capability;
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, Vm};
use std::any::Any;
use std::future::Future;
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
        let data = vm.heap.bytes_of(a[1]).to_vec();
        conn_write(vm, a[0], data)
    });
    r.add_builtin("net.Conn.write_str", |vm, a| {
        let data = vm.str(a[1]).as_bytes().to_vec();
        conn_write(vm, a[0], data)
    });
    r.add_builtin("net.Conn.peer_addr", |vm, a| {
        let peer = conn(vm, a[0])?.peer.clone();
        Ok(vm.new_str(peer))
    });
    r.add_builtin("net.Conn.close", |vm, a| {
        let c = vm.heap.native_mut::<Conn>(a[0]).ok_or_else(|| Ctl::Panic("not a connection".into()))?;
        if c.close() {
            vm.heap.resource_closed();
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
        Err(e) => Ok(vm.err_value(e.kind, &e.message)),
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

fn resource_limit_error(vm: &mut Vm) -> Option<Value> {
    let max = vm.host.limits.max_open_resources;
    (vm.heap.open_resources >= max).then(|| vm.err_value("limit", &format!("too many open resources (limit {max})")))
}

/// Allocates a socket object, re-checking the open-resource limit (another
/// task may have opened sockets while this one was connecting).
fn alloc_resource(vm: &mut Vm, obj: Box<dyn NativeObject>) -> NResult {
    let max = vm.host.limits.max_open_resources;
    if vm.heap.open_resources >= max {
        // Dropping `obj` closes the socket.
        return Ok(vm.err_value("limit", &format!("too many open resources (limit {max})")));
    }
    let v = vm.heap.alloc(Obj::Native(obj));
    Ok(vm.ok(v))
}

/// Like [`suspend`] for operations that produce a socket object.
fn suspend_resource<T: NativeObject>(fut: impl Future<Output = Result<T, NetError>> + Send + 'static) -> NResult {
    Err(Ctl::Async(Box::pin(async move {
        let r = fut.await;
        Box::new(move |vm: &mut Vm| match r {
            Ok(obj) => alloc_resource(vm, Box::new(obj)),
            Err(e) => Ok(vm.err_value(e.kind, &e.message)),
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

fn listen(vm: &mut Vm, a: &[Value]) -> NResult {
    let addr = vm.str(a[0]).to_string();
    if let Some(denied) = check_permission(vm, Capability::NetListen(addr.clone()))? {
        return Ok(denied);
    }
    if let Some(e) = resource_limit_error(vm) {
        return Ok(e);
    }
    suspend_resource(async move { Ok(Listener::new(TcpListener::bind(addr).await?)?) })
}

fn listener_accept(vm: &mut Vm, a: &[Value]) -> NResult {
    let l = listener(vm, a[0])?;
    let Some(socket) = l.socket.clone() else { return Ok(vm.err_value("closed", "listener closed")) };
    let closed = CloseSignal(l.closed.subscribe());
    if let Some(e) = resource_limit_error(vm) {
        return Ok(e);
    }
    suspend_resource(closed.guard(async move {
        let (stream, _) = socket.accept().await?;
        Ok(Conn::from_tcp(stream)?)
    }))
}

// ----------------------------------------------------------------------
// Connections

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
}

impl Conn {
    pub fn new(reader: Box<dyn AsyncRead + Send + Unpin>, writer: WriteHalf, peer: String) -> Conn {
        Conn {
            halves: Some((Arc::new(Mutex::new(BufReader::new(reader))), Arc::new(Mutex::new(writer)))),
            peer,
            closed: watch::Sender::new(false),
        }
    }

    fn from_tcp(stream: TcpStream) -> std::io::Result<Conn> {
        let peer = stream.peer_addr()?.to_string();
        let (r, w) = stream.into_split();
        Ok(Conn::new(Box::new(r), Box::new(w), peer))
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

/// Opens a TCP connection, failing with kind `"timeout"` after `timeout`.
pub(crate) async fn tcp_connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, NetError> {
    match tokio::time::timeout(timeout, TcpStream::connect((host, port))).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(NetError::new("timeout", format!("connecting to {host}:{port} timed out"))),
    }
}

fn connect(vm: &mut Vm, a: &[Value]) -> NResult {
    let addr = vm.str(a[0]).to_string();
    let Some((host, port)) = split_host_port(&addr) else {
        return Ok(vm.err_value("invalid", &format!("invalid address {addr:?} (expected host:port)")));
    };
    if let Some(denied) = check_permission(vm, Capability::Net { host: host.clone(), port })? {
        return Ok(denied);
    }
    if let Some(e) = resource_limit_error(vm) {
        return Ok(e);
    }
    let timeout = Duration::from_nanos(a[1].as_int().max(0) as u64);
    suspend_resource(async move { Ok(Conn::from_tcp(tcp_connect(&host, port, timeout).await?)?) })
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
    if let Some(e) = resource_limit_error(vm) {
        return Ok(e);
    }
    suspend_resource(async move {
        let tcp = tcp_connect(&host, port, Duration::from_secs(30)).await?;
        let peer = tcp.peer_addr()?.to_string();
        let tls = tls::connect(&host, tcp).await?;
        let (r, w) = tokio::io::split(tls);
        Ok(Conn::new(Box::new(r), Box::new(w), peer))
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
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let mut buf = vec![0; max];
            let n = reader.read(&mut buf).await?;
            buf.truncate(n);
            Ok(buf)
        }),
        |vm, data| {
            vm.reserve(data.len())?;
            Ok(vm.heap.new_bytes(data))
        },
    )
}

/// Bytes a single line may occupy before reading fails: what is left of the
/// memory budget (lines are buffered outside the heap until complete).
pub(crate) fn line_budget(vm: &Vm) -> usize {
    match vm.heap.limit {
        Some(l) => l.saturating_sub(vm.heap.bytes),
        None => usize::MAX,
    }
}

/// Reads a line including its `\n` (if any); `None` at end of stream. Fails
/// with kind `"limit"` if the line exceeds `max` bytes.
pub(crate) async fn read_line_bytes<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> Result<Option<Vec<u8>>, NetError> {
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
        line.extend_from_slice(&buf[..take]);
        r.consume(take);
        if done {
            return Ok(Some(line));
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
    let max = line_budget(vm);
    let h = match conn_handle(vm, a[0])? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let Some(mut line) = read_line_bytes(&mut *reader, max).await? else { return Ok(None) };
            strip_eol(&mut line);
            String::from_utf8(line).map(Some).map_err(|_| NetError::new("invalid", "line is not valid UTF-8"))
        }),
        |vm, line| {
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

fn conn_write(vm: &mut Vm, c: Value, data: Vec<u8>) -> NResult {
    let h = match conn_handle(vm, c)? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    suspend(
        h.closed.guard(async move {
            let mut writer = h.writer.lock().await;
            writer.write_all(&data).await?;
            writer.flush().await?;
            Ok(())
        }),
        |_, ()| Ok(Value::Unit),
    )
}

// ----------------------------------------------------------------------
// TLS

#[cfg(feature = "tls")]
pub(crate) mod tls {
    use super::NetError;
    use rustls::pki_types::ServerName;
    use std::sync::{Arc, OnceLock};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::client::TlsStream;

    fn connector() -> Result<TlsConnector, NetError> {
        static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
        let config = CONFIG.get_or_init(|| {
            let roots: rustls::RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
                .map_err(|e| e.to_string())
        });
        match config {
            Ok(c) => Ok(TlsConnector::from(c.clone())),
            Err(e) => Err(NetError::new("io", format!("TLS setup failed: {e}"))),
        }
    }

    /// Performs a TLS handshake over `tcp`, verifying the certificate for `host`.
    pub async fn connect(host: &str, tcp: TcpStream) -> Result<TlsStream<TcpStream>, NetError> {
        let name =
            ServerName::try_from(host.to_string()).map_err(|_| NetError::new("invalid", format!("invalid TLS server name {host:?}")))?;
        Ok(connector()?.connect(name, tcp).await?)
    }
}

#[cfg(not(feature = "tls"))]
pub(crate) mod tls {
    use super::NetError;
    use tokio::net::TcpStream;

    /// TLS is unavailable without the `tls` feature.
    pub async fn connect(_host: &str, _tcp: TcpStream) -> Result<TcpStream, NetError> {
        Err(NetError::new("invalid", "TLS support is not enabled in this build"))
    }
}
