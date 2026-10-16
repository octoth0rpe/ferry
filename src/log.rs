//! Access and error logging.
//!
//! The access log gets one line per request, written when the response body
//! has been fully sent (so the byte count and duration are real). The error
//! log is everything else, via `tracing`. Both can go to stdout/stderr or a
//! file, and both can be swapped or reopened while running.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::task::{Context, Poll, ready};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::{HOST, REFERER, USER_AGENT};
use http::{HeaderValue, Request, Response, Version};
use http_body_util::BodyExt;
use hyper::body::{Frame, SizeHint};

use crate::config::{AccessLog, Config, ErrorLog, LogFormat};
use crate::date;
use crate::upstream::{Body, BoxError};

/// Where log lines go.
pub enum Sink {
    Stdout,
    Stderr,
    File { path: PathBuf, file: Mutex<File> },
}

impl Sink {
    pub fn open(path: &Path) -> io::Result<Sink> {
        Ok(Sink::File {
            path: path.to_path_buf(),
            file: Mutex::new(open_append(path)?),
        })
    }

    /// Writes a whole line in one call, so lines from concurrent requests
    /// never interleave.
    pub fn write_line(&self, line: &str) {
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        let _ = match self {
            Sink::Stdout => io::stdout().lock().write_all(buf.as_bytes()),
            Sink::Stderr => io::stderr().lock().write_all(buf.as_bytes()),
            Sink::File { file, .. } => file.lock().unwrap().write_all(buf.as_bytes()),
        };
    }

    /// Reopens a file sink (after logrotate moved it). No-op otherwise.
    pub fn reopen(&self) -> io::Result<()> {
        if let Sink::File { path, file } = self {
            *file.lock().unwrap() = open_append(path)?;
        }
        Ok(())
    }

    pub fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        match self {
            Sink::Stdout => io::stdout().is_terminal(),
            Sink::Stderr => io::stderr().is_terminal(),
            Sink::File { .. } => false,
        }
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Writes access log lines in the configured format. `sink: None` means off.
pub struct AccessLogger {
    sink: Option<Sink>,
    format: LogFormat,
}

impl AccessLogger {
    pub fn from_config(config: &Config) -> io::Result<AccessLogger> {
        let sink = match &config.access_log {
            AccessLog::Off => None,
            AccessLog::Stdout if config.quiet => None,
            AccessLog::Stdout => Some(Sink::Stdout),
            AccessLog::File(path) => Some(Sink::open(path)?),
        };
        Ok(AccessLogger {
            sink,
            format: config.log_format,
        })
    }

    pub fn off() -> AccessLogger {
        AccessLogger {
            sink: None,
            format: LogFormat::Combined,
        }
    }

    pub fn reopen(&self) -> io::Result<()> {
        self.sink.as_ref().map_or(Ok(()), Sink::reopen)
    }

    fn log(&self, entry: &Entry) {
        if let Some(sink) = &self.sink {
            sink.write_line(&entry.format(self.format));
        }
    }
}

/// What's known about a request when it arrives.
pub struct Pending {
    start: Instant,
    time: SystemTime,
    peer: SocketAddr,
    method: String,
    path: String,
    version: Version,
    host: Option<String>,
    referer: Option<String>,
    user_agent: Option<String>,
}

impl Pending {
    pub fn new<B>(req: &Request<B>, peer: SocketAddr) -> Pending {
        let header = |name| {
            req.headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .map(String::from)
        };
        Pending {
            start: Instant::now(),
            time: SystemTime::now(),
            peer,
            method: req.method().to_string(),
            path: req
                .uri()
                .path_and_query()
                .map_or("/".into(), |pq| pq.to_string()),
            version: req.version(),
            host: header(HOST).or_else(|| req.uri().authority().map(|a| a.to_string())),
            referer: header(REFERER),
            user_agent: header(USER_AGENT),
        }
    }

    /// Arranges for the request to be logged once `resp`'s body is done.
    pub fn finish(
        self,
        resp: Response<Body>,
        logger: Arc<AccessLogger>,
        request_id: &HeaderValue,
        upstream: String,
    ) -> Response<Body> {
        if logger.sink.is_none() {
            return resp;
        }
        let entry = Entry {
            pending: self,
            status: resp.status().as_u16(),
            bytes: 0,
            request_id: request_id.to_str().unwrap_or("-").to_string(),
            upstream,
        };
        resp.map(|body| {
            LoggedBody {
                inner: body,
                entry: Some(entry),
                logger,
            }
            .boxed()
        })
    }
}

/// One access log line's worth of data.
struct Entry {
    pending: Pending,
    status: u16,
    bytes: u64,
    request_id: String,
    upstream: String,
}

impl Entry {
    fn format(&self, format: LogFormat) -> String {
        let p = &self.pending;
        let ms = p.start.elapsed().as_secs_f64() * 1000.0;
        let proto = match p.version {
            Version::HTTP_2 => "HTTP/2.0",
            Version::HTTP_10 => "HTTP/1.0",
            _ => "HTTP/1.1",
        };
        let ip = p.peer.ip();
        let bytes = match self.bytes {
            0 => "-".to_string(),
            n => n.to_string(),
        };
        let time = timestamp(p.time);
        let request = format!("{} {} {proto}", p.method, p.path);
        match format {
            LogFormat::Common => format!(
                "{ip} - - [{time}] \"{}\" {} {bytes}",
                escape(&request),
                self.status
            ),
            LogFormat::Combined => format!(
                "{ip} - - [{time}] \"{}\" {} {bytes} \"{}\" \"{}\" {ms:.1}ms {}",
                escape(&request),
                self.status,
                escape(p.referer.as_deref().unwrap_or("-")),
                escape(p.user_agent.as_deref().unwrap_or("-")),
                self.request_id,
            ),
            LogFormat::Json => {
                let mut o = JsonObject::new();
                o.string("time", &time);
                o.string("remote_addr", &ip.to_string());
                o.string("method", &p.method);
                o.string("path", &p.path);
                o.string("protocol", proto);
                o.option("host", p.host.as_deref());
                o.number("status", self.status);
                o.number("bytes", self.bytes);
                o.number("duration_ms", format_args!("{ms:.1}"));
                o.string("request_id", &self.request_id);
                o.string("upstream", &self.upstream);
                o.option("referer", p.referer.as_deref());
                o.option("user_agent", p.user_agent.as_deref());
                o.finish()
            }
        }
    }
}

/// Formats a time the way ferry writes every timestamp: ISO 8601, UTC,
/// milliseconds. `2026-10-16T03:00:00.123Z`
pub fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    date::timestamp(d.as_secs() as i64, d.subsec_millis())
}

/// Timestamps for the error log, in the same format as the access log.
struct IsoTime;

impl tracing_subscriber::fmt::time::FormatTime for IsoTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        w.write_str(&timestamp(SystemTime::now()))
    }
}

/// Escapes `"`, `\` and control characters so a line can't be forged.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Builds a one-line JSON object, with fields in the order they're added.
struct JsonObject(String);

impl JsonObject {
    fn new() -> JsonObject {
        JsonObject("{".into())
    }

    fn key(&mut self, key: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        json_string(&mut self.0, key);
        self.0.push(':');
    }

    fn string(&mut self, key: &str, value: &str) {
        self.key(key);
        json_string(&mut self.0, value);
    }

    fn option(&mut self, key: &str, value: Option<&str>) {
        match value {
            Some(v) => self.string(key, v),
            None => {
                self.key(key);
                self.0.push_str("null");
            }
        }
    }

    /// `value` must display as a valid JSON number.
    fn number(&mut self, key: &str, value: impl std::fmt::Display) {
        self.key(key);
        let _ = write!(self.0, "{value}");
    }

    fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// Appends `s` as a quoted JSON string.
fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A response body that counts bytes and writes the access log line when
/// it's finished or dropped (e.g. the client went away).
struct LoggedBody {
    inner: Body,
    entry: Option<Entry>,
    logger: Arc<AccessLogger>,
}

impl hyper::body::Body for LoggedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if let (Some(Ok(frame)), Some(entry)) = (&frame, &mut self.entry)
            && let Some(data) = frame.data_ref()
        {
            entry.bytes += data.len() as u64;
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.logger.log(&entry);
        }
    }
}

/// The error log's current destination, swappable at runtime.
fn error_sink() -> Arc<Sink> {
    error_sink_slot().read().unwrap().clone()
}

fn error_sink_slot() -> &'static RwLock<Arc<Sink>> {
    static SINK: OnceLock<RwLock<Arc<Sink>>> = OnceLock::new();
    SINK.get_or_init(|| RwLock::new(Arc::new(Sink::Stderr)))
}

/// Points the error log at the configured destination.
pub fn set_error_log(target: &ErrorLog) -> io::Result<()> {
    let sink = match target {
        ErrorLog::Stderr => Sink::Stderr,
        ErrorLog::File(path) => Sink::open(path)?,
    };
    *error_sink_slot().write().unwrap() = Arc::new(sink);
    Ok(())
}

pub fn reopen_error_log() -> io::Result<()> {
    error_sink().reopen()
}

/// Installs the `tracing` subscriber that writes to the error log.
/// `FERRY_LOG` (e.g. `debug`) sets the level.
pub fn init_error_log() {
    let ansi = error_sink().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let value = std::env::var("FERRY_LOG").ok();
    let (level, bad) = log_level(value.as_deref());
    tracing_subscriber::fmt()
        .with_writer(|| ErrorWriter(Vec::new()))
        .with_target(false)
        .with_timer(IsoTime)
        .with_ansi(ansi)
        .with_max_level(level)
        .init();
    if let Some(bad) = bad {
        tracing::warn!(
            "FERRY_LOG={bad:?} isn't a level; expected error, warn, info, debug or trace. Using info."
        );
    }
}

/// The error log level for a `FERRY_LOG` value, and the value itself if it
/// isn't a level.
fn log_level(value: Option<&str>) -> (tracing::Level, Option<&str>) {
    match value.map(str::trim) {
        None | Some("") => (tracing::Level::INFO, None),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "error" => (tracing::Level::ERROR, None),
            "warn" => (tracing::Level::WARN, None),
            "info" => (tracing::Level::INFO, None),
            "debug" => (tracing::Level::DEBUG, None),
            "trace" => (tracing::Level::TRACE, None),
            _ => (tracing::Level::INFO, Some(v)),
        },
    }
}

/// Buffers one event, then writes it to the current error sink as one line.
struct ErrorWriter(Vec<u8>);

impl Write for ErrorWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ErrorWriter {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            error_sink().write_line(&String::from_utf8_lossy(&self.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn entry() -> Entry {
        let req = Request::builder()
            .method("GET")
            .uri("/a?b=c")
            .header(HOST, "example.com")
            .header(USER_AGENT, "curl/8 \"x\"")
            .body(())
            .unwrap();
        let mut pending = Pending::new(&req, "10.0.0.1:5000".parse().unwrap());
        // 2026-10-16T03:00:00.042Z
        pending.time = UNIX_EPOCH + Duration::from_millis(1_792_119_600_042);
        Entry {
            pending,
            status: 200,
            bytes: 1234,
            request_id: "abc123".into(),
            upstream: "http://localhost:6006".into(),
        }
    }

    #[test]
    fn common_format() {
        assert_eq!(
            entry().format(LogFormat::Common),
            r#"10.0.0.1 - - [2026-10-16T03:00:00.042Z] "GET /a?b=c HTTP/1.1" 200 1234"#
        );
    }

    #[test]
    fn combined_format() {
        let line = entry().format(LogFormat::Combined);
        let prefix = r#"10.0.0.1 - - [2026-10-16T03:00:00.042Z] "GET /a?b=c HTTP/1.1" 200 1234 "-" "curl/8 \"x\"" "#;
        assert!(line.starts_with(prefix), "{line}");
        assert!(line.ends_with("ms abc123"), "{line}");
    }

    #[test]
    fn json_format() {
        let v: serde_json::Value = serde_json::from_str(&entry().format(LogFormat::Json)).unwrap();
        assert_eq!(v["status"], 200);
        assert_eq!(v["path"], "/a?b=c");
        assert_eq!(v["user_agent"], "curl/8 \"x\"");
        assert_eq!(v["host"], "example.com");
        assert_eq!(v["request_id"], "abc123");
        assert_eq!(v["upstream"], "http://localhost:6006");
        assert_eq!(v["referer"], serde_json::Value::Null);
        assert_eq!(v["time"], "2026-10-16T03:00:00.042Z");
    }

    #[test]
    fn timestamps_are_iso8601_utc_with_milliseconds() {
        let t = UNIX_EPOCH + Duration::from_millis(1_792_119_600_500);
        assert_eq!(timestamp(t), "2026-10-16T03:00:00.500Z");
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn log_levels() {
        use tracing::Level;
        assert_eq!(log_level(None), (Level::INFO, None));
        assert_eq!(log_level(Some("")), (Level::INFO, None));
        assert_eq!(log_level(Some("debug")), (Level::DEBUG, None));
        assert_eq!(log_level(Some(" WARN ")), (Level::WARN, None));
        assert_eq!(
            log_level(Some("ferry=debug")),
            (Level::INFO, Some("ferry=debug"))
        );
    }

    #[test]
    fn json_escapes_awkward_strings() {
        let mut e = entry();
        e.pending.path = "/q?a=\"b\"&c=\\d".into();
        e.pending.user_agent = Some("line\nbreak\ttab \u{7f}\u{85} café 日本 🚢".into());
        e.pending.host = Some("\u{1}".into());
        let line = e.format(LogFormat::Json);
        assert!(!line.contains('\n'), "{line}");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["path"], "/q?a=\"b\"&c=\\d");
        assert_eq!(
            v["user_agent"],
            "line\nbreak\ttab \u{7f}\u{85} café 日本 🚢"
        );
        assert_eq!(v["host"], "\u{1}");
        assert!(v["duration_ms"].is_f64());
        assert_eq!(v["bytes"], 1234);
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(escape("a\nb\"c\\"), "a\\x0ab\\\"c\\\\");
    }
}
