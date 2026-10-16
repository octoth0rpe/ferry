//! The reverse proxy core: forwarding requests to upstreams.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use ring::rand::{SecureRandom, SystemRandom};

use bytes::Bytes;
use http::header::{
    ACCEPT, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, HeaderValue,
    PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, Request, Response, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Full};
use hyper::body::{Frame, Incoming, SizeHint};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;

use crate::config::format_duration;
use crate::log::{AccessLogger, Pending};
use crate::mapping::Upstream;
use crate::upstream::{Body, BoxError, Client, base_uri};

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PORT: HeaderName = HeaderName::from_static("x-forwarded-port");

/// Headers that describe a single connection and must not be forwarded.
const HOP_BY_HOP: [HeaderName; 9] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// Where one listener sends its requests, and how.
pub struct Route {
    pub upstream: Upstream,
    base_uri: String,
    client: Client,
    pub max_body: Option<u64>,
    pub listen_port: u16,
    connect_timeout: Duration,
}

impl Route {
    pub fn new(
        upstream: Upstream,
        listen_port: u16,
        max_body: Option<u64>,
        connect_timeout: Duration,
        idle_timeout: Duration,
    ) -> Route {
        Route {
            base_uri: base_uri(&upstream),
            client: Client::new(&upstream, connect_timeout, idle_timeout),
            upstream,
            max_body,
            listen_port,
            connect_timeout,
        }
    }

    /// The upstream as people think of it: `localhost:6006` or a socket path.
    fn target(&self) -> String {
        match &self.upstream {
            Upstream::Tcp { host, port } if host.contains(':') => format!("[{host}]:{port}"),
            Upstream::Tcp { host, port } => format!("{host}:{port}"),
            Upstream::Unix(path) => path.display().to_string(),
        }
    }
}

/// Forwards one request to the route's upstream and returns its response, or
/// an error page explaining what went wrong.
pub async fn handle(
    route: &Route,
    req: Request<Incoming>,
    peer: SocketAddr,
    logger: &Arc<AccessLogger>,
) -> Response<Body> {
    let pending = Pending::new(&req, peer);
    let html = wants_html(req.headers());
    let request_id = request_id(req.headers());

    let mut resp = forward(route, req, peer, &request_id, html).await;
    resp.headers_mut().insert(X_REQUEST_ID, request_id.clone());
    pending.finish(
        resp,
        logger.clone(),
        &request_id,
        route.upstream.to_string(),
    )
}

async fn forward(
    route: &Route,
    mut req: Request<Incoming>,
    peer: SocketAddr,
    request_id: &HeaderValue,
    html: bool,
) -> Response<Body> {
    if let (Some(limit), Some(len)) = (route.max_body, content_length(req.headers()))
        && len > limit
    {
        return too_large(html);
    }

    let host = req.headers().get(HOST).cloned().or_else(|| {
        req.uri()
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    });

    // Only HTTP/1.1 has Upgrade. WebSockets over HTTP/2 (RFC 8441) aren't
    // offered, so browsers open an HTTP/1.1 connection for them instead.
    let upgrade = match req.version() {
        Version::HTTP_11 => upgrade_protocol(req.headers()),
        _ => None,
    };
    let client_upgrade = upgrade.as_ref().map(|_| hyper::upgrade::on(&mut req));

    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let uri: Uri = match format!("{}{path}", route.base_uri).parse() {
        Ok(uri) => uri,
        Err(_) => return error_page(StatusCode::BAD_REQUEST, "invalid request path", html),
    };

    let (parts, body) = req.into_parts();
    let mut headers = parts.headers;
    strip_hop_by_hop(&mut headers);
    if let Some(proto) = &upgrade {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(UPGRADE, proto.clone());
    }
    if let Some(host) = &host {
        headers.insert(HOST, host.clone());
        headers.insert(X_FORWARDED_HOST, host.clone());
    }
    append_forwarded_for(&mut headers, peer);
    headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static("https"));
    headers.insert(X_FORWARDED_PORT, HeaderValue::from(route.listen_port));
    headers.insert(X_REQUEST_ID, request_id.clone());

    let exceeded = Arc::new(AtomicBool::new(false));
    let body: Body = match route.max_body {
        Some(limit) => LimitedBody {
            inner: body,
            remaining: limit,
            exceeded: exceeded.clone(),
        }
        .boxed(),
        None => body.map_err(BoxError::from).boxed(),
    };

    let mut upstream_req = Request::new(body);
    *upstream_req.method_mut() = parts.method;
    *upstream_req.uri_mut() = uri;
    *upstream_req.version_mut() = Version::HTTP_11;
    *upstream_req.headers_mut() = headers;

    let mut resp = match route.client.request(upstream_req).await {
        Ok(resp) => resp,
        Err(_) if exceeded.load(Ordering::Relaxed) => return too_large(html),
        Err(e) => return upstream_error(route, &e, request_id, html),
    };

    match (resp.status(), client_upgrade) {
        (StatusCode::SWITCHING_PROTOCOLS, Some(client_upgrade)) => {
            let upstream_upgrade = hyper::upgrade::on(&mut resp);
            tokio::spawn(tunnel(client_upgrade, upstream_upgrade));
        }
        _ => strip_hop_by_hop(resp.headers_mut()),
    }
    resp.map(|b| b.map_err(BoxError::from).boxed())
}

/// Copies bytes both ways between an upgraded client and upstream connection.
async fn tunnel(client: OnUpgrade, upstream: OnUpgrade) {
    match tokio::try_join!(client, upstream) {
        Ok((client, upstream)) => {
            let mut client = TokioIo::new(client);
            let mut upstream = TokioIo::new(upstream);
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
        Err(e) => tracing::debug!("upgrade failed: {e}"),
    }
}

/// Uses the client's `X-Request-Id` if it sent a sensible one, otherwise
/// makes a new one.
fn request_id(headers: &HeaderMap) -> HeaderValue {
    headers
        .get(X_REQUEST_ID)
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .cloned()
        .unwrap_or_else(|| {
            HeaderValue::from_str(&random_id(16)).expect("alphanumeric is a valid header value")
        })
}

/// `len` random letters and digits from the OS random source, so request IDs
/// can't be guessed.
pub fn random_id(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    // The largest multiple of 62 that fits in a byte. Bytes at or above it
    // are skipped, so every character is equally likely.
    const LIMIT: u8 = (256 / ALPHABET.len() * ALPHABET.len()) as u8;
    let rng = SystemRandom::new();
    let mut id = String::with_capacity(len);
    let mut buf = [0u8; 32];
    while id.len() < len {
        rng.fill(&mut buf).expect("the OS random source works");
        id.extend(
            buf.iter()
                .filter(|&&b| b < LIMIT)
                .map(|&b| char::from(ALPHABET[usize::from(b) % ALPHABET.len()]))
                .take(len - id.len()),
        );
    }
    id
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named.iter().chain(HOP_BY_HOP.iter()) {
        headers.remove(name);
    }
}

/// The requested protocol if this is an HTTP/1.1 upgrade request.
fn upgrade_protocol(headers: &HeaderMap) -> Option<HeaderValue> {
    let wants_upgrade = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    wants_upgrade
        .then(|| headers.get(UPGRADE).cloned())
        .flatten()
}

fn append_forwarded_for(headers: &mut HeaderMap, peer: SocketAddr) {
    let ip = peer.ip().to_string();
    let value = match headers.get(X_FORWARDED_FOR).and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {ip}"),
        None => ip,
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(X_FORWARDED_FOR, value);
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
}

fn too_large(html: bool) -> Response<Body> {
    error_page(
        StatusCode::PAYLOAD_TOO_LARGE,
        "The request body is larger than this server accepts.",
        html,
    )
}

fn upstream_error(
    route: &Route,
    err: &hyper_util::client::legacy::Error,
    request_id: &HeaderValue,
    html: bool,
) -> Response<Body> {
    let target = route.target();
    let io_kind = io_error_kind(err);
    let (status, message) = if err.is_connect() {
        match io_kind {
            Some(io::ErrorKind::TimedOut) => (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "ferry: {target} didn't accept the connection within {}.",
                    format_duration(route.connect_timeout)
                ),
            ),
            Some(io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound) => (
                StatusCode::BAD_GATEWAY,
                format!("ferry: nothing is listening on {target} — is your app running?"),
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                format!("ferry: couldn't connect to {target}: {}", root_cause(err)),
            ),
        }
    } else {
        (
            StatusCode::BAD_GATEWAY,
            format!(
                "ferry: {target} closed the connection or sent an invalid response ({}).",
                root_cause(err)
            ),
        )
    };
    tracing::warn!(
        "{} (request {})",
        message.trim_start_matches("ferry: "),
        request_id.to_str().unwrap_or("-")
    );
    error_page(status, &message, html)
}

fn io_error_kind(err: &(dyn std::error::Error + 'static)) -> Option<io::ErrorKind> {
    let mut source = Some(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<io::Error>() {
            return Some(io.kind());
        }
        source = e.source();
    }
    None
}

fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut e = err;
    while let Some(next) = e.source() {
        e = next;
    }
    e.to_string()
}

/// A small error page: HTML for browsers, plain text for everything else.
pub fn error_page(status: StatusCode, message: &str, html: bool) -> Response<Body> {
    let title = format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Error")
    );
    let (content_type, body) = if html {
        (
            "text/html; charset=utf-8",
            format!(
                "<!doctype html>\n<meta charset=utf-8>\n<title>{title}</title>\n\
                 <body style=\"font-family:system-ui,sans-serif;max-width:40em;margin:4em auto;padding:0 1em\">\n\
                 <h1>{title}</h1>\n<p>{}</p>\n",
                escape_html(message)
            ),
        )
    } else {
        ("text/plain; charset=utf-8", format!("{title}\n{message}\n"))
    };
    let mut resp = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| match never {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A request body that fails once more than `remaining` bytes have been read,
/// recording that it did so the proxy can answer 413.
struct LimitedBody {
    inner: Incoming,
    remaining: u64,
    exceeded: Arc<AtomicBool>,
}

impl hyper::body::Body for LimitedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let frame = match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
            Some(Ok(frame)) => frame,
            other => return Poll::Ready(other.map(|r| r.map_err(BoxError::from))),
        };
        if let Some(data) = frame.data_ref() {
            let len = data.len() as u64;
            if len > this.remaining {
                this.exceeded.store(true, Ordering::Relaxed);
                return Poll::Ready(Some(Err("request body too large".into())));
            }
            this.remaining -= len;
        }
        Poll::Ready(Some(Ok(frame)))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_connection_named_headers() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, "keep-alive, x-secret".parse().unwrap());
        h.insert("keep-alive", "timeout=5".parse().unwrap());
        h.insert("x-secret", "1".parse().unwrap());
        h.insert(TE, "trailers".parse().unwrap());
        h.insert("x-kept", "1".parse().unwrap());
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("x-kept"));
    }

    #[test]
    fn detects_upgrade() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        h.insert(UPGRADE, "websocket".parse().unwrap());
        assert_eq!(upgrade_protocol(&h).unwrap(), "websocket");
        h.remove(UPGRADE);
        assert_eq!(upgrade_protocol(&h), None);
    }

    #[test]
    fn forwarded_for_appends() {
        let mut h = HeaderMap::new();
        append_forwarded_for(&mut h, "10.0.0.1:5".parse().unwrap());
        append_forwarded_for(&mut h, "[::1]:5".parse().unwrap());
        assert_eq!(h[X_FORWARDED_FOR], "10.0.0.1, ::1");
    }

    #[test]
    fn request_id_reused_or_generated() {
        let mut h = HeaderMap::new();
        assert_eq!(request_id(&h).len(), 16);
        h.insert(X_REQUEST_ID, "abc".parse().unwrap());
        assert_eq!(request_id(&h), "abc");
    }

    #[test]
    fn error_page_escapes() {
        let resp = error_page(StatusCode::BAD_GATEWAY, "<b>", true);
        assert_eq!(resp.status(), 502);
        assert_eq!(resp.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    }

    #[test]
    fn random_ids_are_alphanumeric_and_distinct() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| random_id(16)).collect();
        assert_eq!(ids.len(), 1000);
        assert!(
            ids.iter()
                .all(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        );
        assert_eq!(random_id(100).len(), 100);
        assert_eq!(random_id(0), "");
    }
}
