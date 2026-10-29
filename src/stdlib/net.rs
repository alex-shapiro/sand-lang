//! The `net` module: TCP listeners, TCP connections, and TLS client
//! connections.
//!
//! Open outbound connections only through [`connect_checked`] so that the
//! policy checks every resolved IP address, and bound every wait on a peer
//! with a timeout.

use super::stream::{self, IoFailure, Suspended, line_read, read_line, read_to_end_reserved, read_upto, suspend};
use crate::check::ModKind;
use crate::native::{HostError, IntoValue, NativeModule, NativeObject, Val};
use crate::network::{self, Network, Stream};
use crate::policy::{Capability, Policy, format_host_port, is_localhost, is_valid_host, parse_port, split_port};
use crate::vm::iomem::{Reservation, ResourceSlot};
use crate::vm::value::{ValueType, to_usize_saturating};
use crate::vm::{Ctl, Vm};
use jiff::SignedDuration;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("net", ModKind::Std, include_str!("net.sand"));
    m.bind::<SocketAddr>();
    m.bind::<Listener>();
    m.bind::<Conn>();
    m.function("SocketAddr.ip", |addr: SocketAddr| addr.0.ip().to_string());
    m.function("SocketAddr.port", |addr: SocketAddr| i64::from(addr.0.port()));
    m.function("split_host_port", split_host_port);
    m.vm_function("listen", |vm: &mut Vm, ip: Val<String>, port: i64| listen_with(vm, ip, port, Listener::new));
    m.vm_function("connect", connect);
    m.vm_function("connect_tls", connect_tls);
    m.vm_function("Listener.accept", listener_accept);
    m.vm_function("Listener.addr", |vm: &mut Vm, l: Val<Listener>| -> Result<SocketAddr, Ctl> { Ok(l.get(&vm.heap)?.addr) });
    m.vm_function("Listener.close", |vm: &mut Vm, l: Val<Listener>| -> Result<_, Ctl> {
        l.get_mut(&mut vm.heap)?.close();
        Ok(())
    });
    m.vm_function("Conn.read", conn_read);
    m.vm_function("Conn.read_line", conn_read_line);
    m.vm_function("Conn.read_all", conn_read_all);
    m.vm_function("Conn.write", |vm: &mut Vm, c: Val<Conn>, data: Val<Vec<u8>>| conn_write(vm, c, |vm| data.bytes(&vm.heap)));
    m.vm_function("Conn.write_str", |vm: &mut Vm, c: Val<Conn>, text: Val<String>| conn_write(vm, c, |vm| text.str(&vm.heap).as_bytes()));
    m.vm_function("Conn.peer_addr", |vm: &mut Vm, c: Val<Conn>| -> Result<SocketAddr, Ctl> { Ok(c.get(&vm.heap)?.peer) });
    m.vm_function("Conn.close", |vm: &mut Vm, c: Val<Conn>| -> Result<_, Ctl> {
        c.get_mut(&mut vm.heap)?.close();
        Ok(())
    });
    m
}

// ----------------------------------------------------------------------
// Errors and results

pub(crate) fn closed() -> IoFailure {
    IoFailure::new("closed", "connection closed")
}

pub(crate) trait Resource: crate::native::ObjectType + IntoValue {
    /// Attaches the slot that counted the resource while it was being opened.
    fn attach(&mut self, slot: ResourceSlot);
}

/// Like [`suspend`] for operations that produce a resource object,
/// counted by `slot` from the start (a socket being connected or accepted
/// is open already).
fn suspend_resource<T: Resource>(slot: ResourceSlot, fut: impl Future<Output = Result<T, IoFailure>> + Send + 'static) -> Suspended<T> {
    suspend(fut, move |_, mut obj: T| {
        obj.attach(slot);
        Ok(obj)
    })
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
// Addresses

/// An IP address and port, formatted as `127.0.0.1:8080` or `[::1]:8080`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, crate::SandValue)]
#[sand(module = "net")]
pub(crate) struct SocketAddr(std::net::SocketAddr);

impl From<std::net::SocketAddr> for SocketAddr {
    fn from(mut addr: std::net::SocketAddr) -> SocketAddr {
        // `SocketAddrV6` omits `flowinfo` in `Display` and `FromStr`.
        if let std::net::SocketAddr::V6(v6) = &mut addr {
            v6.set_flowinfo(0);
        }
        SocketAddr(addr)
    }
}

impl fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ValueType for SocketAddr {
    const ORDER: Option<fn(&Self, &Self) -> std::cmp::Ordering> = Some(Ord::cmp);
    fn parse(text: &str) -> Result<Self, String> {
        let addr: std::net::SocketAddr = text.parse().map_err(|_| format!("invalid socket address {:?}", super::truncate(text, 200)))?;
        Ok(addr.into())
    }
}

fn split_host_port(text: &str) -> Result<(String, i64), HostError> {
    let parts = split_port(text).and_then(|(host, port)| Some((host, parse_port(port?)?))).filter(|&(host, _)| is_valid_host(host));
    let Some((host, port)) = parts else {
        let msg = format!("invalid address {:?} (expected host:port or [ipv6]:port)", super::truncate(text, 200));
        return Err(HostError::new("parse", msg));
    };
    Ok((host.to_string(), port.into()))
}

fn check_port(port: i64) -> Result<u16, HostError> {
    u16::try_from(port).map_err(|_| HostError::new("invalid", format!("invalid port {port} (expected 0 to 65535)")))
}

fn check_endpoint(vm: &Vm, host: Val<String>, port: i64) -> Result<(String, u16), HostError> {
    let host = host.str(&vm.heap);
    if !is_valid_host(host) {
        let msg = format!("invalid host {:?} (expected a host name or an IP address without brackets)", super::truncate(host, 200));
        return Err(HostError::new("invalid", msg));
    }
    Ok((host.to_string(), check_port(port)?))
}

/// Checks `NetListen` for `ip` and `port`, binds a listener, and wraps it with `open`.
pub(crate) fn listen_with<T: Resource>(
    vm: &mut Vm,
    ip: Val<String>,
    port: i64,
    open: impl FnOnce(Box<dyn network::Listener>) -> std::io::Result<T> + Send + 'static,
) -> Suspended<T> {
    let ip = ip.str(&vm.heap);
    let Ok(ip) = ip.parse::<IpAddr>().map(|ip| ip.to_canonical()) else {
        let msg = format!("invalid IP address {:?} (expected an address such as 127.0.0.1 or ::1)", super::truncate(ip, 200));
        return Err(HostError::new("invalid", msg));
    };
    let addr = std::net::SocketAddr::new(ip, check_port(port)?);
    vm.require(&Capability::NetListen(addr))?;
    let slot = vm.resource_slot()?;
    let network = vm.host.network.clone();
    suspend_resource(slot, async move { Ok(open(network.listen(addr).await?)?) })
}

// ----------------------------------------------------------------------
// Listener

/// A listening TCP socket returned by `listen`.
#[derive(crate::SandObject)]
#[sand(module = "net")]
pub(crate) struct Listener {
    socket: Option<Arc<dyn network::Listener>>,
    slot: Option<ResourceSlot>,
    addr: SocketAddr,
    closed: watch::Sender<bool>,
}

impl Listener {
    fn new(socket: Box<dyn network::Listener>) -> std::io::Result<Listener> {
        let addr = socket.local_addr()?.into();
        Ok(Listener { socket: Some(socket.into()), slot: None, addr, closed: watch::Sender::new(false) })
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
    fn is_resource(&self) -> bool {
        self.socket.is_some()
    }
}

fn listener_accept(vm: &mut Vm, l: Val<Listener>) -> Suspended<Conn> {
    let l = l.get(&vm.heap)?;
    let Some(socket) = l.socket.clone() else { return Err(HostError::new("closed", "listener closed")) };
    let closed = CloseSignal(l.closed.subscribe());
    // A pending accept holds a slot for the connection it will produce.
    let slot = vm.resource_slot_with(CONN_BUFFER, "a connection")?;
    suspend_resource(
        slot,
        closed.guard(async move {
            let (stream, peer) = socket.accept().await?;
            Ok(Conn::from_stream(stream, peer))
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
    pub peer: SocketAddr,
    pub slot: ResourceSlot,
}

impl Detached {
    pub fn new(reader: ReadHalf, writer: WriteHalf, peer: SocketAddr, slot: ResourceSlot) -> Detached {
        Detached { reader: Arc::new(Mutex::new(reader)), writer: Arc::new(Mutex::new(writer)), peer, slot }
    }

    pub fn into_conn(self) -> Conn {
        Conn { halves: Some((self.reader, self.writer)), slot: Some(self.slot), peer: self.peer, closed: watch::Sender::new(false) }
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

/// An open TCP or TLS stream connection.
///
/// The read and write halves use separate async mutexes so that a task can read
/// while another task writes. Closing the connection cancels all pending
/// operations with kind `"closed"` and drops the OS socket after they finish.
#[derive(crate::SandObject)]
#[sand(module = "net")]
pub(crate) struct Conn {
    halves: Option<(SharedReader, SharedWriter)>,
    /// Counts the connection, and reserves its fixed buffers, while open.
    slot: Option<ResourceSlot>,
    peer: SocketAddr,
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
    pub fn new(reader: Box<dyn AsyncRead + Send + Unpin>, writer: WriteHalf, peer: SocketAddr) -> Conn {
        Conn {
            halves: Some((Arc::new(Mutex::new(read_half(reader))), Arc::new(Mutex::new(writer)))),
            slot: None,
            peer,
            closed: watch::Sender::new(false),
        }
    }

    /// Creates a connection wrapping a network stream.
    pub(crate) fn from_stream(stream: Box<dyn Stream>, peer: std::net::SocketAddr) -> Conn {
        let (r, w) = tokio::io::split(stream);
        Conn::new(Box::new(r), Box::new(w), peer.into())
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
        Some(Detached { reader, writer, peer: self.peer, slot })
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
    fn is_resource(&self) -> bool {
        self.halves.is_some()
    }
}

/// Detaches the connection from `c` (see [`Conn::detach`]), or fails with kind `"closed"`.
pub(crate) fn detach(vm: &mut Vm, c: Val<Conn>) -> Result<Detached, HostError> {
    c.get_mut(&mut vm.heap)?.detach().ok_or_else(|| closed().into())
}

/// Returns the active handle for connection `c`, or fails with kind `"closed"`.
pub(crate) fn conn_handle(vm: &Vm, c: Val<Conn>) -> Result<ConnHandle, HostError> {
    c.get(&vm.heap)?.handle().ok_or_else(|| closed().into())
}

/// Resolves the host of `request` (a `Net` or `Http` request that the
/// policy allowed) once and returns the addresses the policy allows
/// ([`Policy::check_resolved`]), so that a name cannot lead to an address the
/// policy forbids (e.g. through DNS rebinding). `localhost` and the names
/// under it resolve to the loopback addresses (RFC 6761) without the
/// resolver. Fails with kind `"permission_denied"` if every address is
/// denied.
pub(crate) async fn resolve_allowed(
    policy: &Policy,
    network: &dyn Network,
    request: &Capability,
) -> Result<Vec<std::net::SocketAddr>, IoFailure> {
    let (host, port) = match request {
        Capability::Net { host, port, .. } | Capability::Http { host, port, .. } => (host.as_str(), *port),
        _ => unreachable!("a connection request"),
    };
    let addrs: Vec<std::net::SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
        Err(_) if is_localhost(host) => [Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()].map(|ip: IpAddr| (ip, port).into()).into(),
        Err(_) => network.resolve(host, port).await?,
    };
    let mut denied = None;
    let mut allowed = Vec::with_capacity(addrs.len());
    for addr in addrs {
        match policy.check_resolved(request, addr) {
            Err(reason) => {
                denied.get_or_insert((addr, reason));
            }
            Ok(()) => allowed.push(addr),
        }
    }
    match denied {
        Some((addr, reason)) if allowed.is_empty() => Err(IoFailure::new(
            "permission_denied",
            format!("permission denied: connect to {}: address {} {reason}", format_host_port(host, port), addr.ip()),
        )),
        _ if allowed.is_empty() => Err(IoFailure::new("not_found", format!("no addresses found for {host}"))),
        _ => Ok(allowed),
    }
}

/// Resolves the host of `request` through `network`, checks each resolved
/// address against `policy`, and connects to the first allowed address.
pub(crate) async fn connect_checked(
    policy: &Policy,
    network: &dyn Network,
    request: &Capability,
) -> Result<(Box<dyn Stream>, std::net::SocketAddr), IoFailure> {
    let addrs = resolve_allowed(policy, network, request).await?;
    Ok(network.connect(&addrs).await?)
}

fn connect(vm: &mut Vm, host: Val<String>, port: i64, timeout: SignedDuration) -> Suspended<Conn> {
    let (host, port) = check_endpoint(vm, host, port)?;
    let request = Capability::Net { host: host.clone(), port, tls: false };
    vm.require(&request)?;
    // Counted from the start: a connection attempt holds a socket.
    let slot = vm.resource_slot_with(CONN_BUFFER, "a connection")?;
    let timeout = stream::timeout(timeout);
    let (policy, network, clock) = (vm.host.policy.clone(), vm.host.network.clone(), vm.host.clock.clone());
    suspend_resource(slot, async move {
        let open = connect_checked(&policy, &*network, &request);
        match crate::platform::timeout(&*clock, timeout, open).await {
            Some(r) => r.map(|(stream, peer)| Conn::from_stream(stream, peer)),
            None => Err(IoFailure::new("timeout", format!("connecting to {} timed out", format_host_port(&host, port)))),
        }
    })
}

fn connect_tls(vm: &mut Vm, host: Val<String>, port: i64, timeout: SignedDuration) -> Suspended<Conn> {
    if !cfg!(feature = "tls") {
        return Err(HostError::new("invalid", "TLS support is not enabled in this build"));
    }
    let (host, port) = check_endpoint(vm, host, port)?;
    let request = Capability::Net { host: host.clone(), port, tls: true };
    vm.require(&request)?;
    let slot = vm.resource_slot_with(CONN_BUFFER + TLS_BUFFERS, "a connection")?;
    let timeout = stream::timeout(timeout);
    let (policy, network, clock) = (vm.host.policy.clone(), vm.host.network.clone(), vm.host.clock.clone());
    let settings = vm.host.tls.clone();
    // The timeout covers connecting and the handshake, so a peer that
    // accepts but never answers cannot hold the socket indefinitely.
    let target = format_host_port(&host, port);
    let open = async move {
        let (stream, peer) = connect_checked(&policy, &*network, &request).await?;
        let tls = tls::connect(&settings, &host, stream).await?;
        let (r, w) = tokio::io::split(tls);
        Ok(Conn::new(Box::new(r), Box::new(w), peer.into()))
    };
    suspend_resource(slot, async move {
        match crate::platform::timeout(&*clock, timeout, open).await {
            Some(r) => r,
            None => Err(IoFailure::new("timeout", format!("TLS connection to {target} timed out"))),
        }
    })
}

fn conn_read(vm: &mut Vm, c: Val<Conn>, max: i64) -> Suspended<Vec<u8>> {
    if max <= 0 {
        return Err(HostError::new("invalid", "read: max must be positive"));
    }
    // A read returns what the connection's buffer holds (filling it first if
    // it is empty), at most `CONN_BUFFER` bytes.
    let max = to_usize_saturating(max).min(CONN_BUFFER);
    let h = conn_handle(vm, c)?;
    // The buffer is counted against the memory limit while the read is pending.
    let res = vm.reserve_io(max)?;
    suspend(
        h.closed.guard(async move {
            let data = read_upto(&mut *h.reader.lock().await, max).await?;
            Ok((data, res))
        }),
        |_, (data, res)| {
            drop(res);
            Ok(data)
        },
    )
}

fn conn_read_line(vm: &mut Vm, c: Val<Conn>) -> Suspended<Option<String>> {
    let h = conn_handle(vm, c)?;
    // The line may grow until it exhausts the memory limit.
    let mut res = vm.io_reservation();
    suspend(
        h.closed.guard(async move {
            let line = read_line(&mut *h.reader.lock().await, &mut res).await?;
            Ok((line, res))
        }),
        |vm, (line, res)| {
            drop(res);
            line_read(vm, line, "line is not valid UTF-8")
        },
    )
}

/// Reads until the peer has finished sending; the data is reserved as it
/// arrives.
fn conn_read_all(vm: &mut Vm, c: Val<Conn>) -> Suspended<Vec<u8>> {
    let h = conn_handle(vm, c)?;
    let mut res = vm.io_reservation();
    suspend(
        h.closed.guard(async move {
            let mut reader = h.reader.lock().await;
            let mut data = Vec::new();
            read_to_end_reserved(&mut *reader, &mut data, &mut res, usize::MAX).await?;
            Ok((data, res))
        }),
        |_, (data, res)| {
            drop(res);
            Ok(data)
        },
    )
}

/// Copies the bytes returned by `bytes` into an I/O reservation before writing,
/// because the heap value can move or be collected while the write is pending.
fn conn_write(vm: &mut Vm, c: Val<Conn>, bytes: impl Fn(&Vm) -> &[u8]) -> Suspended<()> {
    let len = bytes(vm).len();
    vm.charge_bytes(len)?;
    let res = vm.reserve_io(len)?;
    let data = bytes(vm).to_vec();
    write_bytes(vm, c, data, res)
}

/// Writes `data` to connection `c`; `res` reserves `data` until the write is done.
pub(crate) fn write_bytes(vm: &mut Vm, c: Val<Conn>, data: Vec<u8>, res: Reservation) -> Suspended<()> {
    let h = conn_handle(vm, c)?;
    suspend(
        h.closed.guard(async move {
            let mut writer = h.writer.lock().await;
            writer.write_all(&data).await?;
            writer.flush().await?;
            drop(res);
            Ok(())
        }),
        |_, ()| Ok(()),
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
