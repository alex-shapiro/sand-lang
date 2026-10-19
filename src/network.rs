//! Network abstraction for outgoing `net` and `http` connections.
//!
//! Hostnames are resolved and connections opened through a [`Network`].
//! The default implementation, [`OsNetwork`], uses the operating system's
//! resolver and TCP sockets. A custom implementation can be provided via
//! [`Config::network`](crate::Config::network), for example to simulate
//! networking in-memory.
//!
//! Network policies are enforced before address resolution, and connections
//! are opened only to allowed destinations. TLS is handled in-engine over
//! the streams opened by the network.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

/// Resolves hostnames and opens connections for an engine.
pub trait Network: Send + Sync {
    /// Resolves `host` and `port` to socket addresses.
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>>;

    /// Connects to the first reachable address in `addrs`, returning the stream and the connected address.
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
