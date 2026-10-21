//! HTTP clients for talking to upstreams over TCP or Unix sockets.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Uri};
use http_body_util::combinators::BoxBody;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::{Client as LegacyClient, ResponseFuture};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::UnixStream;

use crate::mapping::Upstream;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BoxError>;

/// A pooled HTTP/1.1 client for one upstream.
#[derive(Clone)]
pub enum Client {
    Tcp(LegacyClient<HttpConnector, Body>),
    Unix(LegacyClient<UnixConnector, Body>),
}

impl Client {
    pub fn new(upstream: &Upstream, connect_timeout: Duration, idle_timeout: Duration) -> Client {
        let mut builder = LegacyClient::builder(TokioExecutor::new());
        builder
            .pool_idle_timeout(idle_timeout)
            .pool_max_idle_per_host(crate::server::POOL_SIZE)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new());
        match upstream {
            Upstream::Tcp { .. } => {
                // Resolves names like `localhost` to every address (::1 and
                // 127.0.0.1) and tries them in turn, so it doesn't matter which
                // one the app listens on.
                let mut http = HttpConnector::new();
                http.set_connect_timeout(Some(connect_timeout));
                http.set_nodelay(true);
                Client::Tcp(builder.build(http))
            }
            Upstream::Unix(path) => Client::Unix(builder.build(UnixConnector {
                path: Arc::new(path.clone()),
                timeout: connect_timeout,
            })),
        }
    }

    pub fn request(&self, req: Request<Body>) -> ResponseFuture {
        match self {
            Client::Tcp(c) => c.request(req),
            Client::Unix(c) => c.request(req),
        }
    }
}

/// The `scheme://authority` used in requests sent to an upstream. For Unix
/// sockets the authority is a placeholder; the connector ignores it.
pub fn base_uri(upstream: &Upstream) -> String {
    match upstream {
        Upstream::Tcp { host, port } if host.contains(':') => format!("http://[{host}]:{port}"),
        Upstream::Tcp { host, port } => format!("http://{host}:{port}"),
        Upstream::Unix(_) => "http://localhost".into(),
    }
}

/// Connects to a fixed Unix socket path, whatever the request URI.
#[derive(Clone)]
pub struct UnixConnector {
    path: Arc<PathBuf>,
    timeout: Duration,
}

impl tower_service::Service<Uri> for UnixConnector {
    type Response = TokioIo<UnixStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let path = self.path.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, UnixStream::connect(&*path)).await {
                Ok(stream) => stream.map(TokioIo::new),
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
            }
        })
    }
}
