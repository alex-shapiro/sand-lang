//! The `net` module: TCP listeners and connections, and TLS client connections.
//!
//! A connection is split into a buffered read half and a write half, each
//! behind its own async mutex, so one task can read while another writes.
//! Closing a socket wakes every pending operation on it (they fail with kind
//! `"closed"`) and releases the OS socket once those operations have finished.

use super::stream::{IoFailure, read_line_bytes, read_to_end_reserved, strip_eol};
use crate::check::ModKind;
use crate::engine::Registry;
use crate::network::{Network, Stream};
use crate::policy::{Capability, Decision, Policy, is_private_address};
use crate::vm::iomem::{Reservation, ResourceSlot};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, Finisher, NResult, Vm};
use std::any::Any;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
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
    r.add_method("net.Listener.close", |vm, a| {
        vm.heap.native_mut::<Listener>(a[0]).ok_or_else(|| Ctl::internal("not a listener"))?.close();
        Ok(Value::Unit)
    });
    r.add_method("net.Conn.read", conn_read);
    r.add_method("net.Conn.read_line", conn_read_line);
    r.add_method("net.Conn.read_all", conn_read_all);
    r.add_method("net.Conn.write", |vm, a| conn_write(vm, a[0], a[1]));
    r.add_method("net.Conn.write_str", |vm, a| conn_write(vm, a[0], a[1]));
    r.add_builtin("net.Conn.peer_addr", |vm, a| {
        let peer = conn(vm, a[0])?.peer.clone();
        Ok(vm.new_str(peer))
    });
    r.add_method("net.Conn.close", |vm, a| {
        vm.heap.native_mut::<Conn>(a[0]).ok_or_else(|| Ctl::internal("not a connection"))?.close();
        Ok(Value::Unit)
    });
}

// ----------------------------------------------------------------------
// Errors and results

pub(crate) fn closed() -> IoFailure {
    IoFailure::new("closed", "connection closed")
}

/// Builds a finisher that turns `r` into `Ok(..)` (via `ok`) or `Err(Error)`.
pub(crate) fn finish<T: Send + 'static>(r: Result<T, IoFailure>, ok: impl FnOnce(&mut Vm, T) -> NResult + Send + 'static) -> Finisher {
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
    fut: impl Future<Output = Result<T, IoFailure>> + Send + 'static,
    ok: impl FnOnce(&mut Vm, T) -> NResult + Send + 'static,
) -> NResult {
    Err(Ctl::suspend(Box::pin(async move { finish(fut.await, ok) })))
}

/// Checks `cap`; `Ok(Some(err))` is the `Err(permission_denied)` value to return.
pub(crate) fn check_permission(vm: &mut Vm, cap: Capability) -> Result<Option<Value>, Ctl> {
    if vm.permit(&cap)? { Ok(None) } else { Ok(Some(vm.err_value("permission_denied", &format!("permission denied: {cap}")))) }
}

/// A native object holding an open resource, counted by its slot (see
/// `iomem::ResourceSlot`) until closed or collected.
pub(crate) trait Resource: NativeObject {
    /// Takes the slot that counted the resource while it was being opened.
    fn attach(&mut self, slot: ResourceSlot);
}

/// Allocates the object of a resource that `slot` counted while it was
/// being opened (the object keeps the slot until it is closed).
pub(crate) fn alloc_resource<T: Resource>(vm: &mut Vm, mut obj: T, slot: ResourceSlot) -> Value {
    obj.attach(slot);
    vm.heap.alloc(Obj::Native(Box::new(obj)))
}

/// Like [`suspend`] for operations that produce a resource object,
/// counted by `slot` from the start (a socket being connected or accepted
/// is open already).
pub(crate) fn suspend_resource<T: Resource>(
    slot: ResourceSlot,
    fut: impl Future<Output = Result<T, IoFailure>> + Send + 'static,
) -> NResult {
    Err(Ctl::suspend(Box::pin(async move {
        let r = fut.await;
        Box::new(move |vm: &mut Vm| match r {
            Ok(obj) => {
                let v = alloc_resource(vm, obj, slot);
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
    /// The signal of `sender`'s socket.
    pub fn of(sender: &watch::Sender<bool>) -> CloseSignal {
        CloseSignal(sender.subscribe())
    }

    /// Whether the socket has been closed.
    pub fn is_closed(&self) -> bool {
        *self.0.borrow()
    }

    /// Runs `op` unless (or until) the socket is closed.
    pub async fn guard<T>(mut self, op: impl Future<Output = Result<T, IoFailure>>) -> Result<T, IoFailure> {
        if *self.0.borrow() {
            return Err(closed());
        }
        tokio::select! {
            r = op => r,
            _ = self.0.wait_for(|closed| *closed) => Err(closed()),
        }
    }
}

// ----------------------------------------------------------------------
// Listener

pub(crate) struct Listener {
    socket: Option<Arc<TcpListener>>,
    slot: Option<ResourceSlot>,
    addr: String,
    closed: watch::Sender<bool>,
}

impl Listener {
    fn new(socket: TcpListener) -> std::io::Result<Listener> {
        let addr = socket.local_addr()?.to_string();
        Ok(Listener { socket: Some(Arc::new(socket)), slot: None, addr, closed: watch::Sender::new(false) })
    }

    /// Closes the listener (the socket once pending accepts have ended).
    fn close(&mut self) {
        self.closed.send_replace(true);
        self.socket = None;
        self.slot = None;
    }
}

impl Resource for Listener {
    fn attach(&mut self, slot: ResourceSlot) {
        self.slot = Some(slot);
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
    vm.heap.native::<Listener>(v).ok_or_else(|| Ctl::internal("not a listener"))
}

fn listen(vm: &mut Vm, a: &[Value]) -> NResult {
    let addr = vm.str(a[0]).to_string();
    if let Some(denied) = check_permission(vm, Capability::NetListen(addr.clone()))? {
        return Ok(denied);
    }
    let slot = super::or_return!(vm.resource_slot());
    suspend_resource(slot, async move { Ok(Listener::new(TcpListener::bind(addr).await?)?) })
}

fn listener_accept(vm: &mut Vm, a: &[Value]) -> NResult {
    let l = listener(vm, a[0])?;
    let Some(socket) = l.socket.clone() else { return Ok(vm.err_value("closed", "listener closed")) };
    let closed = CloseSignal(l.closed.subscribe());
    // A pending accept holds a slot for the connection it will produce.
    let slot = super::or_return!(vm.resource_slot_with(CONN_BUFFER, "a connection"));
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

/// The buffered read half of a connection over `reader` (its buffer is
/// [`CONN_BUFFER`]).
pub(crate) fn read_half(reader: Box<dyn AsyncRead + Send + Unpin>) -> ReadHalf {
    BufReader::with_capacity(CONN_BUFFER, Box::new(StickyEof { inner: reader, eof: false }))
}
pub(crate) type WriteHalf = Box<dyn AsyncWrite + Send + Unpin>;
pub(crate) type SharedReader = Arc<Mutex<ReadHalf>>;
pub(crate) type SharedWriter = Arc<Mutex<WriteHalf>>;

/// An open connection outside of a `net.Conn` object, with its slot.
pub(crate) struct Detached {
    pub reader: SharedReader,
    pub writer: SharedWriter,
    pub peer: String,
    pub slot: ResourceSlot,
}

impl Detached {
    pub fn new(reader: ReadHalf, writer: WriteHalf, peer: String, slot: ResourceSlot) -> Detached {
        Detached { reader: Arc::new(Mutex::new(reader)), writer: Arc::new(Mutex::new(writer)), peer, slot }
    }

    /// Allocates a `net.Conn` object for the connection.
    pub fn alloc(self, vm: &mut Vm) -> Value {
        let conn = Conn { halves: Some((self.reader, self.writer)), slot: None, peer: self.peer, closed: watch::Sender::new(false) };
        alloc_resource(vm, conn, self.slot)
    }

    /// Starts closing the connection without waiting (see [`shutdown_now`]).
    pub fn close(self) {
        if let Ok(mut w) = self.writer.try_lock() {
            shutdown_now(&mut **w);
        }
    }
}

/// The shareable parts of an open connection.
#[derive(Clone)]
pub(crate) struct ConnHandle {
    pub reader: SharedReader,
    pub writer: SharedWriter,
    pub closed: CloseSignal,
}

pub(crate) struct Conn {
    halves: Option<(SharedReader, SharedWriter)>,
    /// Counts the connection, and reserves its fixed buffers, while open.
    slot: Option<ResourceSlot>,
    peer: String,
    closed: watch::Sender<bool>,
}

/// Makes end of stream final: once the peer has finished sending, later reads
/// report end of stream again: some platforms report a reset received after
/// the peer's FIN as an error on the next read.
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
    /// A connection over `reader` and `writer`; its slot reserves their buffers.
    pub fn new(reader: Box<dyn AsyncRead + Send + Unpin>, writer: WriteHalf, peer: String) -> Conn {
        Conn {
            halves: Some((Arc::new(Mutex::new(read_half(reader))), Arc::new(Mutex::new(writer)))),
            slot: None,
            peer,
            closed: watch::Sender::new(false),
        }
    }

    /// Creates a connection over `stream`, opened by a [`Network`].
    pub(crate) fn from_stream(stream: Box<dyn Stream>, peer: SocketAddr) -> Conn {
        let (r, w) = tokio::io::split(stream);
        Conn::new(Box::new(r), Box::new(w), peer.to_string())
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

    /// Takes the connection out of the object, which is then closed as far as
    /// the script can tell; `None` if it was closed.
    pub fn detach(&mut self) -> Option<Detached> {
        self.closed.send_replace(true);
        let (reader, writer) = self.halves.take()?;
        let slot = self.slot.take()?;
        Some(Detached { reader, writer, peer: self.peer.clone(), slot })
    }

    /// Closes the connection: the socket is closed once pending operations
    /// (which fail with kind `"closed"`) have ended.
    fn close(&mut self) {
        self.closed.send_replace(true);
        if let Some((_, w)) = self.halves.take()
            && let Ok(mut w) = w.try_lock()
        {
            shutdown_now(&mut **w);
        }
        self.slot = None;
    }
}

/// Starts closing `w` without waiting: a TLS stream sends its
/// `close_notify` and a TCP stream its FIN, if the socket takes them now.
pub(crate) fn shutdown_now<W: AsyncWrite + Unpin + ?Sized>(w: &mut W) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let _ = std::pin::Pin::new(w).poll_shutdown(&mut cx);
}

impl Resource for Conn {
    fn attach(&mut self, slot: ResourceSlot) {
        self.slot = Some(slot);
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
    vm.heap.native::<Conn>(v).ok_or_else(|| Ctl::internal("not a connection"))
}

/// Takes the connection out of the `net.Conn` object `v` (see
/// [`Conn::detach`]), or returns the `Err(closed)` value.
pub(crate) fn detach(vm: &mut Vm, v: Value) -> Result<Result<Detached, Value>, Ctl> {
    match vm.heap.native_mut::<Conn>(v).ok_or_else(|| Ctl::internal("not a connection"))?.detach() {
        Some(d) => Ok(Ok(d)),
        None => Ok(Err(vm.err_value("closed", "connection closed"))),
    }
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
pub(crate) async fn resolve_allowed(policy: &Policy, network: &dyn Network, host: &str, port: u16) -> Result<Vec<SocketAddr>, IoFailure> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => network.resolve(host, port).await?,
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
            Err(IoFailure::new(
                "permission_denied",
                format!("permission denied: connect to {host}:{port}: address {}{class} is not allowed", addr.ip()),
            ))
        }
        _ if allowed.is_empty() => Err(IoFailure::new("not_found", format!("no addresses found for {host}"))),
        _ => Ok(allowed),
    }
}

/// Opens a stream to an allowed host address.
pub(crate) async fn connect_checked(
    policy: &Policy,
    network: &dyn Network,
    host: &str,
    port: u16,
) -> Result<(Box<dyn Stream>, SocketAddr), IoFailure> {
    let addrs = resolve_allowed(policy, network, host, port).await?;
    // Connects to the checked addresses only (in order); never resolves again.
    Ok(network.connect(&addrs).await?)
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
    let slot = super::or_return!(vm.resource_slot_with(CONN_BUFFER, "a connection"));
    let timeout = Duration::from_nanos(a[1].as_int().max(0) as u64);
    let (policy, network, clock) = (vm.host.policy.clone(), vm.host.network.clone(), vm.host.clock.clone());
    suspend_resource(slot, async move {
        let open = connect_checked(&policy, &*network, &host, port);
        match crate::platform::timeout(&*clock, timeout, open).await {
            Some(r) => r.map(|(stream, peer)| Conn::from_stream(stream, peer)),
            None => Err(IoFailure::new("timeout", format!("connecting to {host}:{port} timed out"))),
        }
    })
}

fn connect_tls(vm: &mut Vm, a: &[Value]) -> NResult {
    if !cfg!(feature = "tls") {
        return Ok(vm.err_value("invalid", "TLS support is not enabled in this build"));
    }
    let addr = vm.str(a[0]);
    let Some((host, port)) = split_host_port(addr) else {
        return Ok(vm.err_value("invalid", &format!("invalid address {:?} (expected host:port)", super::truncate(addr, 200))));
    };
    if let Some(denied) = check_permission(vm, Capability::Net { host: host.clone(), port })? {
        return Ok(denied);
    }
    let slot = super::or_return!(vm.resource_slot_with(CONN_BUFFER + TLS_BUFFERS, "a connection"));
    let timeout = Duration::from_nanos(a[1].as_int().max(0) as u64);
    let (policy, network, clock) = (vm.host.policy.clone(), vm.host.network.clone(), vm.host.clock.clone());
    let settings = vm.host.tls.clone();
    // The timeout covers connecting and the handshake, so a peer that
    // accepts but never answers cannot hold the socket indefinitely.
    let target = format!("{host}:{port}");
    let open = async move {
        let (stream, peer) = connect_checked(&policy, &*network, &host, port).await?;
        let tls = tls::connect(&settings, &host, stream).await?;
        let (r, w) = tokio::io::split(tls);
        Ok(Conn::new(Box::new(r), Box::new(w), peer.to_string()))
    };
    suspend_resource(slot, async move {
        match crate::platform::timeout(&*clock, timeout, open).await {
            Some(r) => r,
            None => Err(IoFailure::new("timeout", format!("TLS connection to {target} timed out"))),
        }
    })
}

fn conn_read(vm: &mut Vm, a: &[Value]) -> NResult {
    let max = a[1].as_int();
    if max <= 0 {
        return Ok(vm.err_value("invalid", "read: max must be positive"));
    }
    // A read returns what the connection's buffer holds (filling it first if
    // it is empty), at most `CONN_BUFFER` bytes.
    let max = (max as usize).min(CONN_BUFFER);
    let h = match conn_handle(vm, a[0])? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    // The buffer is counted against the memory limit while the read is pending.
    let res = super::or_return!(vm.reserve_io(max));
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let available = reader.fill_buf().await?;
            let data = available[..available.len().min(max)].to_vec();
            reader.consume(data.len());
            Ok((data, res))
        }),
        |vm, (data, res)| {
            drop(res);
            vm.reserve(data.len())?;
            vm.charge_bytes(data.len())?;
            Ok(vm.heap.new_bytes(data))
        },
    )
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
            let line = String::from_utf8(line).map_err(|_| IoFailure::new("invalid", "line is not valid UTF-8"))?;
            Ok((Some(line), res))
        }),
        |vm, (line, res)| {
            drop(res);
            Ok(match line {
                Some(s) => {
                    vm.reserve(s.len())?;
                    vm.charge_slow_bytes(s.len())?;
                    let s = vm.new_str(s);
                    vm.some(s)
                }
                None => vm.none(),
            })
        },
    )
}

/// Reads until the peer has finished sending; the data is reserved as it
/// arrives.
fn conn_read_all(vm: &mut Vm, a: &[Value]) -> NResult {
    let h = match conn_handle(vm, a[0])? {
        Ok(h) => h,
        Err(e) => return Ok(e),
    };
    let mut res = vm.io_reservation();
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let mut data = Vec::new();
            read_to_end_reserved(&mut *reader, &mut data, &mut res, usize::MAX).await?;
            Ok((data, res))
        }),
        |vm, (data, res)| {
            drop(res);
            vm.reserve(data.len())?;
            vm.charge_bytes(data.len())?;
            Ok(vm.heap.new_bytes(data))
        },
    )
}

/// Writes `data` (a `str` or `bytes`), from a copy that is reserved until
/// the write is done (the heap value may change or go meanwhile).
fn conn_write(vm: &mut Vm, c: Value, v: Value) -> NResult {
    fn bytes(vm: &Vm, v: Value) -> &[u8] {
        match vm.heap.get(v) {
            Obj::Str(s) => s.as_bytes(),
            _ => vm.heap.bytes_of(v),
        }
    }
    let len = bytes(vm, v).len();
    vm.charge_bytes(len)?;
    let res = super::or_return!(vm.reserve_io(len));
    let data = bytes(vm, v).to_vec();
    write_bytes(vm, c, data, res)
}

/// Writes `data` to connection `c`; `res` reserves `data` until the write is done.
pub(crate) fn write_bytes(vm: &mut Vm, c: Value, data: Vec<u8>, res: Reservation) -> NResult {
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
    use super::{IoFailure, TlsSettings};
    use crate::network::Stream;
    use rustls::pki_types::{CertificateDer, ServerName};
    use std::sync::{Arc, OnceLock};
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

    fn connector(settings: &TlsSettings) -> Result<TlsConnector, IoFailure> {
        // Engines without extra roots share one configuration.
        static DEFAULT: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
        let config = if settings.roots.is_empty() {
            DEFAULT.get_or_init(|| build(&[]))
        } else {
            settings.config.get_or_init(|| build(&settings.roots))
        };
        match config {
            Ok(c) => Ok(TlsConnector::from(c.clone())),
            Err(e) => Err(IoFailure::new("io", format!("TLS setup failed: {e}"))),
        }
    }

    /// Performs a TLS handshake over a stream and verifies the host certificate.
    pub async fn connect(settings: &TlsSettings, host: &str, stream: Box<dyn Stream>) -> Result<TlsStream<Box<dyn Stream>>, IoFailure> {
        let name = ServerName::try_from(host.to_string())
            .map_err(|_| IoFailure::new("invalid", format!("invalid TLS server name {:?}", crate::stdlib::truncate(host, 200))))?;
        connector(settings)?.connect(name, stream).await.map_err(|e| {
            let e = IoFailure::from(e);
            IoFailure::new(e.kind, format!("TLS handshake with {host} failed: {}", e.message))
        })
    }
}

#[cfg(not(feature = "tls"))]
pub(crate) mod tls {
    use super::{IoFailure, TlsSettings};
    use crate::network::Stream;

    /// TLS is unavailable without the `tls` feature.
    pub async fn connect(_settings: &TlsSettings, _host: &str, _stream: Box<dyn Stream>) -> Result<Box<dyn Stream>, IoFailure> {
        Err(IoFailure::new("invalid", "TLS support is not enabled in this build"))
    }
}
