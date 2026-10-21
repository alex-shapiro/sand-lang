//! Network abstraction for connections and listeners.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};

/// Resolves host names, opens network connections and binds listeners.
pub trait Network: Send + Sync {
    /// Resolves a host name and port to socket addresses.
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>>;

    /// Connects to the first reachable address in `addrs`.
    fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)>;

    /// Binds a listener to `addr` (port `0`: an ephemeral port).
    fn listen(&self, addr: SocketAddr) -> NetworkFuture<'_, Box<dyn Listener>>;
}

/// A listening socket.
pub trait Listener: Send + Sync {
    /// The address that the listener is bound to.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Polls for the next incoming connection and the address of its peer.
    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(Box<dyn Stream>, SocketAddr)>>;
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
        Box::pin(async move { Ok(Box::new(tokio::net::TcpListener::bind(addr).await?) as Box<dyn Listener>) })
    }
}

impl Listener for tokio::net::TcpListener {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        tokio::net::TcpListener::local_addr(self)
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(Box<dyn Stream>, SocketAddr)>> {
        tokio::net::TcpListener::poll_accept(self, cx).map_ok(|(tcp, peer)| (Box::new(tcp) as Box<dyn Stream>, peer))
    }
}
