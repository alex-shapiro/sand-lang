//! Network abstraction for connections and listeners.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

/// Resolves host names, opens network connections, and binds listeners.
pub trait Network: Send + Sync {
    /// Resolves a host name and port to socket addresses.
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>>;

    /// Connects to the first reachable address in `addrs`.
    fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)>;

    /// Binds a listener to `addr` (port `0` binds an ephemeral port). An IPv6
    /// listener must not accept IPv4 connections.
    fn listen(&self, addr: SocketAddr) -> NetworkFuture<'_, Box<dyn Listener>>;
}

/// A listening socket.
pub trait Listener: Send + Sync {
    /// Returns the bound local address.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Accepts the next incoming connection and returns the stream and peer
    /// address. Implementations must support concurrent calls and must be
    /// cancel-safe.
    fn accept(&self) -> NetworkFuture<'_, (Box<dyn Stream>, SocketAddr)>;
}

/// A bidirectional byte stream.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

pub type NetworkFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// Network implementation using operating system resolver and TCP sockets.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsNetwork;

impl Network for OsNetwork {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>> {
        Box::pin(async move { Ok(tokio::net::lookup_host((host, port)).await?.collect()) })
    }

    fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)> {
        Box::pin(async move {
            let tcp = tokio::net::TcpStream::connect(addrs).await?;
            let peer = tcp.peer_addr()?;
            Ok((Box::new(tcp) as Box<dyn Stream>, peer))
        })
    }

    fn listen(&self, addr: SocketAddr) -> NetworkFuture<'_, Box<dyn Listener>> {
        Box::pin(async move { Ok(Box::new(bind(addr)?) as Box<dyn Listener>) })
    }
}

/// Binds a nonblocking TCP listener with `IPV6_V6ONLY` enabled on IPv6 sockets.
fn bind(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // On Windows, `SO_REUSEADDR` allows another socket to hijack an active port.
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(socket.into())
}

impl Listener for tokio::net::TcpListener {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        tokio::net::TcpListener::local_addr(self)
    }

    fn accept(&self) -> NetworkFuture<'_, (Box<dyn Stream>, SocketAddr)> {
        Box::pin(async move {
            let (tcp, peer) = tokio::net::TcpListener::accept(self).await?;
            Ok((Box::new(tcp) as Box<dyn Stream>, peer))
        })
    }
}
