//! Test harness: a configurable upstream server, a running `ferry` process,
//! and an HTTPS client that accepts ferry's certificates.

#![allow(dead_code)]

use std::convert::Infallible;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

type Body = BoxBody<Bytes, hyper::Error>;

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ferry-it-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A port that was free a moment ago.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The test upstream's routes:
/// - `/headers`: the request headers, one `name: value` per line
/// - `/echo`: the request body, streamed back
/// - `/slow`: waits a second, then answers `slow`
/// - `/upgrade`: 101 Switching Protocols, then echoes raw bytes
/// - anything else: `hello from upstream`
async fn upstream_service(mut req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let full = |s: String| Full::new(Bytes::from(s)).map_err(|n| match n {}).boxed();
    let resp = match req.uri().path() {
        "/headers" => {
            let mut lines = format!("method: {}\nuri: {}\n", req.method(), req.uri());
            for (k, v) in req.headers() {
                lines.push_str(&format!("{k}: {}\n", v.to_str().unwrap_or("?")));
            }
            Response::new(full(lines))
        }
        "/echo" => Response::new(req.into_body().boxed()),
        "/slow" => {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Response::new(full("slow".into()))
        }
        "/upgrade" => {
            let on_upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                let upgraded = on_upgrade.await.unwrap();
                let mut io = TokioIo::new(upgraded);
                let mut buf = [0u8; 1024];
                loop {
                    match io.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => io.write_all(&buf[..n]).await.unwrap(),
                    }
                }
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("connection", "upgrade")
                .header("upgrade", "test-echo")
                .body(full(String::new()))
                .unwrap()
        }
        _ => Response::new(full("hello from upstream".into())),
    };
    Ok(resp)
}

async fn serve<S>(stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service_fn(upstream_service))
        .with_upgrades()
        .await;
}

/// Starts the test upstream on a TCP address; returns its port.
pub async fn tcp_upstream(addr: &str) -> u16 {
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(stream));
        }
    });
    port
}

/// Starts the test upstream on a Unix socket.
pub async fn unix_upstream(path: &Path) {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(stream));
        }
    });
}

/// A running ferry process, killed on drop.
pub struct Ferry {
    pub child: Child,
    pub home: TempDir,
    pub stderr_path: PathBuf,
    pub stdout_path: PathBuf,
}

impl Ferry {
    /// Starts ferry with `args` and waits until `wait_port` accepts
    /// connections.
    pub fn start(args: &[&str], wait_port: u16) -> Ferry {
        let ferry = Ferry::spawn(args);
        ferry.wait_for_port(wait_port);
        ferry
    }

    pub fn spawn(args: &[&str]) -> Ferry {
        let home = TempDir::new();
        let stderr_path = home.path("stderr.log");
        let stdout_path = home.path("stdout.log");
        let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args(args)
            .env("HOME", &home.0)
            .env("XDG_DATA_HOME", home.path("share"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout_path).unwrap())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap();
        Ferry {
            child,
            home,
            stderr_path,
            stdout_path,
        }
    }

    pub fn wait_for_port(&self, port: u16) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("ferry didn't start listening on {port}:\n{}", self.stderr());
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    pub fn stdout(&self) -> String {
        std::fs::read_to_string(&self.stdout_path).unwrap_or_default()
    }

    pub fn signal(&self, sig: &str) {
        Command::new("kill")
            .args([&format!("-{sig}"), &self.child.id().to_string()])
            .status()
            .unwrap();
    }

    /// Waits for the process to exit; returns its exit code.
    pub fn wait(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }
}

impl Drop for Ferry {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs ferry to completion (for `-t`, `--print-config`, errors).
pub fn run_ferry(args: &[&str]) -> (i32, String, String) {
    let home = TempDir::new();
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(args)
        .env("HOME", &home.0)
        .env("XDG_DATA_HOME", home.path("share"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn tls_config(alpn: &[&[u8]]) -> rustls::ClientConfig {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAny))
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

pub fn client(http2: bool) -> reqwest::Client {
    let alpn: &[&[u8]] = if http2 { &[b"h2"] } else { &[b"http/1.1"] };
    let builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls_config(alpn))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    let builder = if http2 {
        builder.http2_prior_knowledge()
    } else {
        builder.http1_only()
    };
    builder.build().unwrap()
}

/// A client for plain HTTP requests.
pub fn plain_client() -> reqwest::Client {
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls_config(&[]))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

/// Opens a raw TLS connection (HTTP/1.1 ALPN) to ferry.
pub async fn tls_connect(port: u16) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let tcp = tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config(&[b"http/1.1"])));
    connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap()
}
