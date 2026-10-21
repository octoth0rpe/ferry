//! Listeners and connections: accepting, TLS, and serving HTTP.
//!
//! Each listener's settings live behind a `RwLock<Arc<_>>` so a config reload
//! can replace them without dropping the socket. New connections pick up the new
//! settings; existing ones finish with the old.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use http::header::{HOST, LOCATION};
use http::{HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{AccessLog, Config};
use crate::log::{AccessLogger, Pending};
use crate::proxy::{self, Route, error_page};
use crate::upstream::Body;
use crate::{ca, net, tls, trust};

/// Identifies a listening socket: bind address (`None` = all) and port.
pub type ListenKey = (Option<IpAddr>, u16);

/// How a listener behaves. Swapped atomically on reload.
pub struct ListenerState {
    /// `None` for the plain-HTTP listeners: the HTTP side of an `80/443`
    /// mapping, and the page for trusting the local CA.
    https: Option<Https>,
    log: Arc<AccessLogger>,
    redirect: bool,
    /// For the HTTP side of an `80/443` mapping: the HTTPS port to redirect
    /// to. `None` redirects to the same port.
    redirect_to: Option<u16>,
    /// Set on the listener that serves the page for trusting the local CA.
    trust_page: Option<Arc<trust::Page>>,
    handshake_timeout: Duration,
    idle_timeout: Duration,
}

struct Https {
    tls: TlsAcceptor,
    route: Arc<Route>,
}

/// What a listener is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// HTTPS for a mapping.
    App,
    /// The plain HTTP side of an `80/443` mapping, redirecting to HTTPS.
    Redirect,
    /// The page for trusting the local CA, over plain HTTP on a port the OS
    /// picks (port 0 in the key), so ferry never serves anything of its own
    /// at an app's address.
    Trust,
}

/// A listener ready to start: its key, how it's described to people, and its
/// state.
pub struct ListenerSpec {
    pub key: ListenKey,
    pub listen: String,
    pub state: ListenerState,
    /// Only the trust page is optional; if it can't be bound, ferry warns
    /// and carries on without it.
    pub kind: Kind,
}

/// Everything [`build`] prepared, plus things worth telling the user.
pub struct Plan {
    pub listeners: Vec<ListenerSpec>,
    /// The access log all listeners write to.
    pub log: Arc<AccessLogger>,
    pub warnings: Vec<String>,
    /// Set when any listener uses ferry's local CA.
    pub local_ca: Option<ca::LocalCa>,
    /// The port the page for trusting the local CA ended up on, once bound.
    pub trust_port: Option<u16>,
}

/// Loads certificates and builds listener state for a config. Doesn't bind.
pub fn build(config: &Config) -> Result<Plan, Vec<String>> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut loaded: HashMap<Option<crate::config::CertKey>, Arc<rustls::ServerConfig>> =
        HashMap::new();
    let mut local_ca = None;
    let mut trust_page = None;
    let mut listeners = Vec::new();
    let log = match AccessLogger::from_config(config) {
        Ok(log) => Arc::new(log),
        Err(e) => {
            let AccessLog::File(path) = &config.access_log else {
                unreachable!("only files can fail to open")
            };
            errors.push(format!("can't open access log {}: {e}", path.display()));
            Arc::new(AccessLogger::off())
        }
    };

    for l in &config.listeners {
        let ck = l.cert_key(config).cloned();
        let server_config = match loaded.get(&ck) {
            Some(sc) => Some(sc.clone()),
            None => {
                let cert = match &ck {
                    Some(ck) => tls::load(ck),
                    None => ca::load_or_create(config.advanced.ca_name_constraints).map(|ca| {
                        let cert = ca.cert.clone();
                        warnings.extend(ca.warnings.iter().cloned());
                        trust_page = Some(Arc::new(trust::Page::new(&ca)));
                        local_ca = Some(ca);
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
                https: Some(Https {
                    tls: TlsAcceptor::from(server_config),
                    route: Arc::new(route),
                }),
                log: log.clone(),
                redirect: config.redirect,
                redirect_to: None,
                trust_page: None,
                handshake_timeout: adv.handshake_timeout,
                idle_timeout: adv.idle_timeout,
            },
            kind: Kind::App,
        });
    }

    // `80/443` mappings: plain HTTP on the first port, redirecting to the
    // second.
    for l in &config.listeners {
        let m = &l.mapping;
        let Some(from) = m.redirect_from else {
            continue;
        };
        listeners.push(ListenerSpec {
            key: (m.bind, from),
            listen: crate::mapping::listen_display(m.bind, from),
            state: ListenerState {
                https: None,
                log: log.clone(),
                redirect: true,
                redirect_to: Some(m.port),
                trust_page: None,
                handshake_timeout: config.advanced.handshake_timeout,
                idle_timeout: config.advanced.idle_timeout,
            },
            kind: Kind::Redirect,
        });
    }

    // The trust page listens wherever phones can reach the apps: everywhere,
    // unless every app using the CA is on loopback only.
    if let Some(page) = trust_page {
        let local_only = config
            .listeners
            .iter()
            .filter(|l| l.cert_key(config).is_none())
            .all(|l| l.mapping.bind.is_some_and(|ip| ip.is_loopback()));
        let bind = local_only.then(|| IpAddr::from([127, 0, 0, 1]));
        listeners.push(ListenerSpec {
            key: (bind, 0),
            listen: "the page for trusting ferry's local CA".into(),
            state: ListenerState {
                https: None,
                log: log.clone(),
                redirect: false,
                redirect_to: None,
                trust_page: Some(page),
                handshake_timeout: config.advanced.handshake_timeout,
                idle_timeout: config.advanced.idle_timeout,
            },
            kind: Kind::Trust,
        });
    }

    if errors.is_empty() {
        Ok(Plan {
            listeners,
            log,
            warnings,
            local_ca,
            trust_port: None,
        })
    } else {
        errors.dedup();
        Err(errors)
    }
}

pub type Sockets = HashMap<ListenKey, std::net::TcpListener>;

/// Binds sockets for every listener in `specs` whose key isn't in `skip`.
/// Fails if any required listener can't be bound; optional ones that can't
/// are returned as warnings.
pub fn bind<'a>(
    specs: impl IntoIterator<Item = &'a ListenerSpec>,
    skip: impl Fn(&ListenKey) -> bool,
) -> Result<(Sockets, Vec<String>), Vec<String>> {
    let mut sockets = HashMap::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for spec in specs {
        if skip(&spec.key) {
            continue;
        }
        match net::bind(spec.key.0, spec.key.1) {
            Ok(s) => {
                sockets.insert(spec.key, s);
            }
            Err(e) if spec.kind == Kind::Trust => warnings.push(format!(
                "can't serve the page for trusting ferry's local CA: {e}"
            )),
            Err(e) => errors.push(net::bind_error(&spec.listen, spec.key.1, &e)),
        }
    }
    if errors.is_empty() {
        Ok((sockets, warnings))
    } else {
        Err(errors)
    }
}

struct Running {
    state: Arc<RwLock<Arc<ListenerState>>>,
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
                *running.state.write().unwrap() = state;
                continue;
            }
            let Some(socket) = sockets.remove(&spec.key) else {
                assert!(
                    spec.kind == Kind::Trust,
                    "a socket is bound for every required listener"
                );
                continue;
            };
            let listener = TcpListener::from_std(socket)?;
            let running = Running {
                state: Arc::new(RwLock::new(state)),
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
    state: Arc<RwLock<Arc<ListenerState>>>,
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
        let state = state.read().unwrap().clone();
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

    let Some(https) = &state.https else {
        serve_plain(stream, peer, &state).await;
        return;
    };
    if first[0] != TLS_HANDSHAKE {
        serve_plain(stream, peer, &state).await;
        return;
    }

    let tls = match tokio::time::timeout(state.handshake_timeout, https.tls.accept(stream)).await {
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

    let activity = Arc::new(Activity::new());
    let io = TokioIo::new(tls);
    let route = https.route.clone();
    let log = state.log.clone();
    let requests = activity.clone();
    let idle = state.idle_timeout;
    let service = service_fn(move |req: Request<Incoming>| {
        let route = route.clone();
        let log = log.clone();
        let progress = requests.start();
        async move { Ok::<_, Infallible>(proxy::handle(&route, req, peer, &log, progress, idle).await) }
    });

    let mut builder = auto::Builder::new(TokioExecutor::new());
    // hyper's header timer also runs while an HTTP/1.1 connection waits for
    // its next request, so it's the idle timeout there. The first request
    // gets `handshake_timeout`, below.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(idle);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(Duration::from_secs(30)));
    let conn = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(conn);

    // Close the connection if no request arrives in time, when it's been idle
    // too long, when a request stops making progress, or on shutdown. Idle
    // and shutdown closes are graceful: in-flight requests finish first.
    let mut closing = false;
    let mut hard_deadline: Option<Instant> = None;
    loop {
        let due = activity.due(state.handshake_timeout, idle);
        let wake = match due {
            Due::Idle(_) if closing => None,
            Due::FirstRequest(at) | Due::Idle(at) | Due::Stalled(at) => Some(at),
        };
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
            _ = sleep_until_ms(wake) => {
                match activity.due(state.handshake_timeout, idle) {
                    Due::FirstRequest(at) if now_ms() >= at => {
                        tracing::debug!("connection from {peer}: no request within the handshake timeout");
                        return;
                    }
                    Due::Stalled(at) if now_ms() >= at => {
                        tracing::debug!("connection from {peer}: a request made no progress; closing");
                        return;
                    }
                    Due::Idle(at) if now_ms() >= at && !closing => {
                        closing = true;
                        // A client that stops reading could hold a graceful
                        // close open forever.
                        hard_deadline = Some(Instant::now() + state.handshake_timeout);
                        conn.as_mut().graceful_shutdown();
                    }
                    _ => {}
                }
            }
            _ = activity.changed.notified() => {}
            _ = sleep_until(hard_deadline) => return,
        }
    }
}

/// Sleeps until `at` (milliseconds on the [`now_ms`] clock), or forever.
async fn sleep_until_ms(at: Option<u64>) {
    match at {
        Some(at) => tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms()))).await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// Answers plain HTTP: the page for trusting the local CA on its own port,
/// otherwise a redirect to HTTPS or a short explanation.
async fn serve_plain(stream: TcpStream, peer: SocketAddr, state: &ListenerState) {
    let redirect = state.redirect;
    let redirect_to = state.redirect_to;
    let log = state.log.clone();
    let trust_page = state.trust_page.clone();
    let service = service_fn(move |req: Request<Incoming>| {
        let log = log.clone();
        let trust_page = trust_page.clone();
        async move {
            let pending = Pending::new(&req, peer);
            let (resp, upstream) = match trust_page {
                Some(page) => (page.respond(&req), "ferry"),
                None => (redirect_response(&req, redirect, redirect_to), "redirect"),
            };
            let id = HeaderValue::from_static("-");
            Ok::<_, Infallible>(pending.finish(resp, log, &id, upstream.into()))
        }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(state.handshake_timeout)
        .keep_alive(false)
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// Redirects a plain HTTP request to the same URL over HTTPS: on the same
/// port, or with `to_port` on that one (`:80` → `:443`, which URLs leave out).
pub fn redirect_response<B>(
    req: &Request<B>,
    enabled: bool,
    to_port: Option<u16>,
) -> Response<Body> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| match to_port {
            None => h.to_string(),
            Some(443) => strip_port(h).to_string(),
            Some(port) => format!("{}:{port}", strip_port(h)),
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

/// Milliseconds since ferry started, for timeouts.
pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// What's happening on one client connection, for its timeouts. Activity is
/// counted per request (requests arriving, response headers, body bytes in
/// either direction), not per socket read or write, so HTTP/2 keep-alive
/// pings don't keep an idle connection open.
pub struct Activity {
    opened: u64,
    state: Mutex<ActivityState>,
    /// Woken when a request starts or finishes, which can bring the next
    /// deadline forward.
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct ActivityState {
    /// Requests that may still be in flight.
    requests: Vec<Weak<Progress>>,
    /// When the last request finished; `None` before the first.
    last_done: Option<u64>,
}

/// What a connection is waiting for, and when it's due (on the [`now_ms`]
/// clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    /// No request has arrived yet.
    FirstRequest(u64),
    /// No requests are in flight.
    Idle(u64),
    /// The longest a request in flight can go without progress.
    Stalled(u64),
}

impl Activity {
    fn new() -> Activity {
        Activity {
            opened: now_ms(),
            state: Mutex::new(ActivityState::default()),
            changed: tokio::sync::Notify::new(),
        }
    }

    /// Records a request starting. It's in flight until the returned value
    /// and every clone of it are dropped.
    fn start(self: &Arc<Self>) -> Arc<Progress> {
        let progress = Arc::new(Progress {
            last: AtomicU64::new(now_ms()),
            conn: self.clone(),
        });
        let mut state = self.state.lock().unwrap();
        state.requests.retain(|r| r.strong_count() > 0);
        state.requests.push(Arc::downgrade(&progress));
        drop(state);
        self.changed.notify_one();
        progress
    }

    fn due(&self, handshake: Duration, idle: Duration) -> Due {
        let (handshake, idle) = (millis(handshake), millis(idle));
        let mut state = self.state.lock().unwrap();
        state.requests.retain(|r| r.strong_count() > 0);
        let oldest = state
            .requests
            .iter()
            .filter_map(Weak::upgrade)
            .map(|p| p.last.load(Ordering::Relaxed))
            .min();
        match (oldest, state.last_done) {
            // A request waiting on its upstream gets `idle` before it's
            // answered 504; this leaves time for that.
            (Some(last), _) => Due::Stalled(last.saturating_add(idle).saturating_add(handshake)),
            (None, Some(done)) => Due::Idle(done.saturating_add(idle)),
            (None, None) => Due::FirstRequest(self.opened.saturating_add(handshake)),
        }
    }
}

/// One request's progress. Body frames and the response's arrival update it.
pub struct Progress {
    last: AtomicU64,
    conn: Arc<Activity>,
}

impl Progress {
    pub fn touch(&self) {
        self.last.store(now_ms(), Ordering::Relaxed);
    }

    /// When the request last made progress, on the [`now_ms`] clock.
    pub fn last(&self) -> u64 {
        self.last.load(Ordering::Relaxed)
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.conn.state.lock().unwrap().last_done = Some(now_ms());
        self.conn.changed.notify_one();
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn config(args: &[&str]) -> (std::path::PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("ferry-srv-{}", crate::proxy::random_id(12)));
        std::fs::create_dir_all(&dir).unwrap();
        let ck = crate::tls::tests::write_test_cert(&dir, "t");
        let mut argv = vec![
            "ferry".to_string(),
            ck.cert.display().to_string(),
            ck.key.display().to_string(),
        ];
        argv.extend(args.iter().map(|s| s.to_string()));
        let cli = crate::cli::Cli::try_parse_from(argv).unwrap();
        (dir, Config::load(&cli).unwrap())
    }

    fn keys(args: &[&str]) -> Vec<(ListenKey, bool)> {
        let (dir, config) = config(args);
        let plan = build(&config).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        plan.listeners
            .iter()
            .map(|l| (l.key, l.kind != Kind::App))
            .collect()
    }

    #[test]
    fn only_the_ports_named_are_opened() {
        assert_eq!(keys(&["443:3000"]), vec![((None, 443), false)]);
        assert_eq!(
            keys(&["80/443:3000"]),
            vec![((None, 443), false), ((None, 80), true)]
        );
        let lo = Some("127.0.0.1".parse().unwrap());
        assert_eq!(
            keys(&["127.0.0.1:8080/8443:3000"]),
            vec![((lo, 8443), false), ((lo, 8080), true)]
        );
        // --no-redirect is about plain HTTP on HTTPS ports; a redirect port
        // asked for by name still redirects.
        assert_eq!(
            keys(&["--no-redirect", "80/443:3000"]),
            vec![((None, 443), false), ((None, 80), true)]
        );
    }

    fn redirect(host: &str, path: &str, to_port: Option<u16>) -> (u16, Option<String>) {
        let req = Request::builder()
            .uri(path)
            .header(HOST, host)
            .body(())
            .unwrap();
        let resp = redirect_response(&req, true, to_port);
        let location = resp
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_string());
        (resp.status().as_u16(), location)
    }

    #[test]
    fn redirect_locations() {
        assert_eq!(
            redirect("localhost:6005", "/a?b", None),
            (308, Some("https://localhost:6005/a?b".into()))
        );
        assert_eq!(
            redirect("example.com", "/", Some(443)),
            (308, Some("https://example.com/".into()))
        );
        assert_eq!(
            redirect("example.com:80", "/x", Some(443)),
            (308, Some("https://example.com/x".into()))
        );
        assert_eq!(
            redirect("localhost:8080", "/x", Some(8443)),
            (308, Some("https://localhost:8443/x".into()))
        );
        assert_eq!(
            redirect("[::1]:8080", "/", Some(8443)),
            (308, Some("https://[::1]:8443/".into()))
        );
    }

    #[test]
    fn strips_ports() {
        assert_eq!(strip_port("example.com:80"), "example.com");
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("[::1]:80"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
    }
}
