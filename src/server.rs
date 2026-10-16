//! Listeners and connections: accepting, TLS, and serving HTTP.
//!
//! Each listener's settings live behind an [`ArcSwap`] so a config reload can
//! replace them without dropping the socket. New connections pick up the new
//! settings; existing ones finish with the old.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use http::header::{HOST, LOCATION};
use http::{HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::proxy::{self, Route, error_page};
use crate::upstream::Body;
use crate::{net, selfsigned, tls};

/// Identifies a listening socket: bind address (`None` = all) and port.
pub type ListenKey = (Option<IpAddr>, u16);

/// How a listener behaves. Swapped atomically on reload.
pub struct ListenerState {
    tls: TlsAcceptor,
    route: Arc<Route>,
    redirect: bool,
    handshake_timeout: Duration,
    idle_timeout: Duration,
}

/// A listener ready to start: its key, how it's described to people, and its
/// state.
pub struct ListenerSpec {
    pub key: ListenKey,
    pub listen: String,
    pub state: ListenerState,
}

/// Everything [`build`] prepared, plus things worth telling the user.
pub struct Plan {
    pub listeners: Vec<ListenerSpec>,
    pub warnings: Vec<String>,
    /// Set when any listener uses the self-signed certificate.
    pub self_signed: Option<selfsigned::SelfSigned>,
}

/// Loads certificates and builds listener state for a config. Doesn't bind.
pub fn build(config: &Config) -> Result<Plan, Vec<String>> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut loaded: HashMap<Option<crate::config::CertKey>, Arc<rustls::ServerConfig>> =
        HashMap::new();
    let mut self_signed = None;
    let mut listeners = Vec::new();

    for l in &config.listeners {
        let ck = l.cert_key(config).cloned();
        let server_config = match loaded.get(&ck) {
            Some(sc) => Some(sc.clone()),
            None => {
                let cert = match &ck {
                    Some(ck) => tls::load(ck),
                    None => selfsigned::load_or_create().map(|ss| {
                        let cert = ss.cert.clone();
                        self_signed = Some(ss);
                        cert
                    }),
                };
                match cert {
                    Ok(cert) => {
                        warnings.extend(cert.warnings.iter().cloned());
                        let sc = tls::server_config(&cert);
                        loaded.insert(ck, sc.clone());
                        Some(sc)
                    }
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                }
            }
        };
        let Some(server_config) = server_config else {
            continue;
        };

        let adv = &config.advanced;
        let route = Route::new(
            l.mapping.upstream.clone(),
            l.mapping.port,
            l.max_body(config),
            adv.connect_timeout,
            adv.idle_timeout,
        );
        listeners.push(ListenerSpec {
            key: (l.mapping.bind, l.mapping.port),
            listen: l.mapping.listen_display(),
            state: ListenerState {
                tls: TlsAcceptor::from(server_config),
                route: Arc::new(route),
                redirect: config.redirect,
                handshake_timeout: adv.handshake_timeout,
                idle_timeout: adv.idle_timeout,
            },
        });
    }

    if errors.is_empty() {
        Ok(Plan {
            listeners,
            warnings,
            self_signed,
        })
    } else {
        errors.dedup();
        Err(errors)
    }
}

/// Binds sockets for every listener in `specs` whose key isn't in `skip`.
/// Binds all or none.
pub fn bind<'a>(
    specs: impl IntoIterator<Item = &'a ListenerSpec>,
    skip: impl Fn(&ListenKey) -> bool,
) -> Result<HashMap<ListenKey, std::net::TcpListener>, Vec<String>> {
    let mut sockets = HashMap::new();
    let mut errors = Vec::new();
    for spec in specs {
        if skip(&spec.key) {
            continue;
        }
        match net::bind(spec.key.0, spec.key.1) {
            Ok(s) => {
                sockets.insert(spec.key, s);
            }
            Err(e) => errors.push(net::bind_error(&spec.listen, spec.key.1, &e)),
        }
    }
    if errors.is_empty() {
        Ok(sockets)
    } else {
        Err(errors)
    }
}

struct Running {
    state: Arc<ArcSwap<ListenerState>>,
    stop: CancellationToken,
}

/// The set of running listeners.
pub struct Server {
    running: HashMap<ListenKey, Running>,
    shutdown: CancellationToken,
    tracker: TaskTracker,
}

impl Server {
    pub fn new() -> Server {
        Server {
            running: HashMap::new(),
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Makes the running listeners match `specs`: starts new ones (using
    /// `sockets` already bound for them), updates existing ones in place, and
    /// stops ones no longer wanted. Must be called within a tokio runtime.
    pub fn apply(
        &mut self,
        specs: Vec<ListenerSpec>,
        mut sockets: HashMap<ListenKey, std::net::TcpListener>,
    ) -> io::Result<()> {
        let wanted: Vec<ListenKey> = specs.iter().map(|s| s.key).collect();
        for (key, running) in self.running.extract_if(|key, _| !wanted.contains(key)) {
            tracing::debug!("stopped listening on port {}", key.1);
            running.stop.cancel();
        }

        for spec in specs {
            let state = Arc::new(spec.state);
            if let Some(running) = self.running.get(&spec.key) {
                running.state.store(state);
                continue;
            }
            let socket = sockets
                .remove(&spec.key)
                .expect("a socket is bound for every new listener");
            let listener = TcpListener::from_std(socket)?;
            let running = Running {
                state: Arc::new(ArcSwap::new(state)),
                stop: CancellationToken::new(),
            };
            tokio::spawn(accept_loop(
                listener,
                running.state.clone(),
                running.stop.clone(),
                self.shutdown.clone(),
                self.tracker.clone(),
            ));
            self.running.insert(spec.key, running);
        }
        Ok(())
    }

    pub fn is_running(&self, key: &ListenKey) -> bool {
        self.running.contains_key(key)
    }

    /// Stops accepting connections and waits up to `grace` for in-flight
    /// requests to finish. Returns early if `force` resolves.
    pub async fn shutdown(self, grace: Duration, force: impl Future<Output = ()>) {
        self.shutdown.cancel();
        self.tracker.close();
        tokio::select! {
            _ = self.tracker.wait() => {}
            _ = tokio::time::sleep(grace) => {
                tracing::warn!("{} connection(s) still open after {grace:?}; closing them", self.tracker.len());
            }
            _ = force => {}
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<ArcSwap<ListenerState>>,
    stop: CancellationToken,
    shutdown: CancellationToken,
    tracker: TaskTracker,
) {
    loop {
        let (stream, peer) = tokio::select! {
            _ = stop.cancelled() => return,
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    // Usually running out of file descriptors; back off.
                    tracing::error!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let state = state.load_full();
        tracker.spawn(handle_connection(
            stream,
            net::canonical(peer),
            state,
            shutdown.clone(),
        ));
    }
}

/// TLS records start with this byte (a handshake record); plain HTTP never
/// does, since requests start with a method name.
const TLS_HANDSHAKE: u8 = 0x16;

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<ListenerState>,
    shutdown: CancellationToken,
) {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; 1];
    match tokio::time::timeout(state.handshake_timeout, stream.peek(&mut first)).await {
        Ok(Ok(1)) => {}
        _ => return,
    }

    if first[0] != TLS_HANDSHAKE {
        serve_plain(stream, state.redirect, state.handshake_timeout).await;
        return;
    }

    let tls = match tokio::time::timeout(state.handshake_timeout, state.tls.accept(stream)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            tracing::debug!("TLS handshake with {peer} failed: {e}");
            return;
        }
        Err(_) => {
            tracing::debug!("TLS handshake with {peer} timed out");
            return;
        }
    };

    let activity = Arc::new(AtomicU64::new(now_ms()));
    let io = TokioIo::new(Tracked {
        inner: tls,
        last: activity.clone(),
    });
    let route = state.route.clone();
    let service = service_fn(move |req: Request<Incoming>| {
        let route = route.clone();
        async move { Ok::<_, Infallible>(proxy::handle(&route, req, peer).await) }
    });

    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(state.handshake_timeout);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(Duration::from_secs(30)));
    let conn = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(conn);

    // Close the connection when it's been idle too long, or on shutdown.
    // Either way it's a graceful close: in-flight requests finish first.
    let idle = state.idle_timeout;
    let mut closing = false;
    loop {
        let idle_deadline = activity.load(Ordering::Relaxed) + idle.as_millis() as u64;
        let sleep_for = Duration::from_millis(idle_deadline.saturating_sub(now_ms()));
        tokio::select! {
            result = conn.as_mut() => {
                if let Err(e) = result {
                    tracing::debug!("connection from {peer}: {e}");
                }
                return;
            }
            _ = shutdown.cancelled(), if !closing => {
                closing = true;
                conn.as_mut().graceful_shutdown();
            }
            _ = tokio::time::sleep(sleep_for), if !closing => {
                if now_ms() >= activity.load(Ordering::Relaxed) + idle.as_millis() as u64 {
                    closing = true;
                    conn.as_mut().graceful_shutdown();
                }
            }
        }
    }
}

/// Answers plain HTTP sent to a TLS port, by redirecting to HTTPS or with a
/// short explanation.
async fn serve_plain(stream: TcpStream, redirect: bool, timeout: Duration) {
    let service = service_fn(move |req: Request<Incoming>| async move {
        Ok::<_, Infallible>(redirect_response(&req, redirect, false))
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(timeout)
        .keep_alive(false)
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// Redirects a plain HTTP request to the same URL over HTTPS. With
/// `default_port`, the redirect drops the port (for `:80` → `:443`).
pub fn redirect_response(
    req: &Request<Incoming>,
    enabled: bool,
    default_port: bool,
) -> Response<Body> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| match default_port {
            true => strip_port(h),
            false => h,
        });
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let html = req
        .headers()
        .get(http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));

    match host {
        Some(host) if enabled => {
            let location = format!("https://{host}{path}");
            match HeaderValue::from_str(&location) {
                Ok(location) => {
                    let mut resp = error_page(
                        StatusCode::PERMANENT_REDIRECT,
                        "This site is served over HTTPS.",
                        html,
                    );
                    resp.headers_mut().insert(LOCATION, location);
                    resp
                }
                Err(_) => error_page(StatusCode::BAD_REQUEST, "invalid Host header", html),
            }
        }
        Some(host) => error_page(
            StatusCode::BAD_REQUEST,
            &format!("This port only speaks HTTPS. Try https://{host}{path}"),
            html,
        ),
        None => error_page(
            StatusCode::BAD_REQUEST,
            "This port only speaks HTTPS.",
            html,
        ),
    }
}

/// `example.com:80` → `example.com`; `[::1]:80` → `[::1]`.
fn strip_port(host: &str) -> &str {
    match host.rfind(':') {
        Some(i) if !host[i..].contains(']') => &host[..i],
        _ => host,
    }
}

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Records the time of the last read or write on a stream.
struct Tracked<T> {
    inner: T,
    last: Arc<AtomicU64>,
}

impl<T: AsyncRead + Unpin> AsyncRead for Tracked<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if result.is_ready() {
            self.last.store(now_ms(), Ordering::Relaxed);
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tracked<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if result.is_ready() {
            self.last.store(now_ms(), Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if result.is_ready() {
            self.last.store(now_ms(), Ordering::Relaxed);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ports() {
        assert_eq!(strip_port("example.com:80"), "example.com");
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("[::1]:80"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
    }
}
