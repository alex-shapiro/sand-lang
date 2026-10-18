//! The network that an engine's `net` and `http` modules connect through.
//!
//! An engine resolves names and opens connections through a [`Network`].
//! The default, [`OsNetwork`], uses the operating system's resolver and TCP
//! sockets. A host sets its own with [`Config::network`](crate::Config::network),
//! for example to serve connections from memory in a simulation.
//!
//! The engine checks the policy before it resolves a name, and connects only
//! to the resolved addresses that the policy allows. TLS runs in the engine,
//! over the streams that the network opens.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

/// Resolves names and opens connections for an engine.
pub trait Network: Send + Sync {
    /// The addresses of `host`, a name that is not an IP address.
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>>;

    /// Opens a stream to the first of `addrs` that accepts, and returns it
    /// with the address it reached.
    fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)>;
}

/// A byte stream to a peer.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// A future that a [`Network`] returns.
pub type NetworkFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// The operating system's resolver and TCP sockets.
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
}
