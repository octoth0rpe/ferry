//! Access and error logging.
//!
//! The access log gets one line per request, written when the response body
//! has been fully sent (so the byte count and duration are real). The error
//! log is everything else, via `tracing`. Both can go to stdout/stderr or a
//! file, and both can be swapped or reopened while running.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, ready};
use std::time::Instant;

use arc_swap::ArcSwap;
use bytes::Bytes;
use http::header::{HOST, REFERER, USER_AGENT};
use http::{HeaderValue, Request, Response, Version};
use http_body_util::BodyExt;
use hyper::body::{Frame, SizeHint};

use crate::config::{AccessLog, Config, ErrorLog, LogFormat};
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
    time: jiff::Timestamp,
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
            time: jiff::Timestamp::now(),
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
            LogFormat::Json => serde_json::json!({
                "time": time,
                "remote_addr": ip.to_string(),
                "method": p.method,
                "path": p.path,
                "protocol": proto,
                "host": p.host,
                "status": self.status,
                "bytes": self.bytes,
                "duration_ms": (ms * 10.0).round() / 10.0,
                "request_id": self.request_id,
                "upstream": self.upstream,
                "referer": p.referer,
                "user_agent": p.user_agent,
            })
            .to_string(),
        }
    }
}

/// Formats a time the way ferry writes every timestamp: ISO 8601, UTC,
/// milliseconds. `2026-10-16T03:00:00.123Z`
pub fn timestamp(t: jiff::Timestamp) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Timestamps for the error log, in the same format as the access log.
struct IsoTime;

impl tracing_subscriber::fmt::time::FormatTime for IsoTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        w.write_str(&timestamp(jiff::Timestamp::now()))
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
fn error_sink() -> &'static ArcSwap<Sink> {
    static SINK: OnceLock<ArcSwap<Sink>> = OnceLock::new();
    SINK.get_or_init(|| ArcSwap::from_pointee(Sink::Stderr))
}

/// Points the error log at the configured destination.
pub fn set_error_log(target: &ErrorLog) -> io::Result<()> {
    let sink = match target {
        ErrorLog::Stderr => Sink::Stderr,
        ErrorLog::File(path) => Sink::open(path)?,
    };
    error_sink().store(Arc::new(sink));
    Ok(())
}

pub fn reopen_error_log() -> io::Result<()> {
    error_sink().load().reopen()
}

/// Installs the `tracing` subscriber that writes to the error log.
/// `FERRY_LOG` (e.g. `debug`) adjusts verbosity.
pub fn init_error_log() {
    let ansi = error_sink().load().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    tracing_subscriber::fmt()
        .with_writer(|| ErrorWriter(Vec::new()))
        .with_target(false)
        .with_timer(IsoTime)
        .with_ansi(ansi)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
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
            error_sink()
                .load()
                .write_line(&String::from_utf8_lossy(&self.0));
        }
    }
}

#[cfg(test)]
mod tests {
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
        pending.time = "2026-10-16T03:00:00.042Z".parse().unwrap();
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
        let t: jiff::Timestamp = "2026-10-15T23:00:00.5-04:00".parse().unwrap();
        assert_eq!(timestamp(t), "2026-10-16T03:00:00.500Z");
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(escape("a\nb\"c\\"), "a\\x0ab\\\"c\\\\");
    }
}
