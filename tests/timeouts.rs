//! Connection and request timeouts: how long ferry waits for a first request,
//! keeps idle connections, and lets a request stand still.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Starts ferry in front of a test upstream with `advanced` settings.
async fn ferry_with(advanced: &str) -> (Ferry, u16) {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let dir = TempDir::new();
    let config = dir.path("ferry.toml");
    std::fs::write(
        &config,
        format!("[[proxy]]\nlisten = {port}\nupstream = {up}\n\n[advanced]\n{advanced}\n"),
    )
    .unwrap();
    let ferry = Ferry::start_in(dir, &["-c", config.to_str().unwrap()], port);
    (ferry, port)
}

/// Sends one HTTP/1.1 request on `tls` and reads the whole response.
async fn get(tls: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin), path: &str) -> String {
    tls.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed mid-response");
        got.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&got);
        if let Some(end) = text.find("\r\n\r\n") {
            let len: usize = text
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|v| v.parse().unwrap())
                })
                .unwrap_or(0);
            if got.len() >= end + 4 + len {
                return text.into_owned();
            }
        }
    }
}

/// Waits for the server to close `tls`; returns how long that took.
async fn closed_after(tls: &mut (impl AsyncReadExt + Unpin), limit: Duration) -> Duration {
    let start = Instant::now();
    let mut buf = [0u8; 1024];
    loop {
        match tokio::time::timeout(limit, tls.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return start.elapsed(),
            Ok(Ok(_)) => continue,
            Err(_) => panic!("still open after {limit:?}"),
        }
    }
}

#[tokio::test]
async fn a_connection_with_no_request_closes_after_the_handshake_timeout() {
    let (_ferry, port) = ferry_with("handshake_timeout = \"1s\"\nidle_timeout = \"30s\"").await;
    let mut tls = tls_connect(port).await;
    let took = closed_after(&mut tls, Duration::from_secs(10)).await;
    assert!(took < Duration::from_secs(3), "closed after {took:?}");
}

#[tokio::test]
async fn http1_keep_alive_lasts_the_idle_timeout_not_the_handshake_timeout() {
    let (_ferry, port) = ferry_with("handshake_timeout = \"1s\"\nidle_timeout = \"3s\"").await;
    let mut tls = tls_connect(port).await;
    assert!(get(&mut tls, "/").await.starts_with("HTTP/1.1 200"));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(get(&mut tls, "/").await.starts_with("HTTP/1.1 200"));
    let took = closed_after(&mut tls, Duration::from_secs(10)).await;
    assert!(
        took > Duration::from_millis(2500) && took < Duration::from_secs(5),
        "closed after {took:?}"
    );
}

#[tokio::test]
async fn idle_http2_connections_close_after_the_idle_timeout() {
    let (_ferry, port) = ferry_with("idle_timeout = \"2s\"").await;
    let tcp = tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config(&[b"h2"])));
    let tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    // hyper's client answers pings by itself.
    let (mut send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    let start = Instant::now();
    let conn = tokio::spawn(conn);
    let req = http::Request::get(format!("https://localhost:{port}/"))
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();
    let resp = send.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();

    tokio::time::timeout(Duration::from_secs(10), conn)
        .await
        .expect("the idle HTTP/2 connection is closed")
        .unwrap()
        .ok();
    let took = start.elapsed();
    assert!(took < Duration::from_secs(5), "closed after {took:?}");
}

#[tokio::test]
async fn an_upstream_that_doesnt_answer_gets_a_504() {
    // `/slow` takes a second to answer.
    let (_ferry, port) = ferry_with("idle_timeout = \"300ms\"").await;
    let resp = client(false)
        .get(format!("https://localhost:{port}/slow"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);
    let text = resp.text().await.unwrap();
    assert!(text.contains("didn't respond within 300ms"), "{text}");
}

#[tokio::test]
async fn a_request_that_stops_moving_is_cut_off() {
    let (_ferry, port) = ferry_with("handshake_timeout = \"1s\"\nidle_timeout = \"1s\"").await;
    let mut tls = tls_connect(port).await;
    // Promise a large upload, send a little of it, then go quiet.
    tls.write_all(
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000000\r\n\r\nstart",
    )
    .await
    .unwrap();
    let took = closed_after(&mut tls, Duration::from_secs(10)).await;
    assert!(took < Duration::from_secs(4), "closed after {took:?}");
}

#[tokio::test]
async fn slow_uploads_that_keep_moving_are_fine() {
    let (_ferry, port) = ferry_with("handshake_timeout = \"1s\"\nidle_timeout = \"1s\"").await;
    let mut tls = tls_connect(port).await;
    tls.write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8\r\n\r\n")
        .await
        .unwrap();
    // Four seconds in all, but never a second without a byte.
    for chunk in [b"ab", b"cd", b"ef", b"gh"] {
        tokio::time::sleep(Duration::from_millis(700)).await;
        tls.write_all(chunk).await.unwrap();
    }
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !String::from_utf8_lossy(&got).ends_with("abcdefgh") {
        let n = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
}
