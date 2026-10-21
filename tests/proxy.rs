mod common;

use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn url(port: u16, path: &str) -> String {
    format!("https://localhost:{port}{path}")
}

#[tokio::test]
async fn proxies_http1_and_http2() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    for http2 in [false, true] {
        let resp = client(http2).get(url(port, "/")).send().await.unwrap();
        let expected = if http2 {
            http::Version::HTTP_2
        } else {
            http::Version::HTTP_11
        };
        assert_eq!(resp.version(), expected);
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().contains_key("x-request-id"));
        assert_eq!(resp.text().await.unwrap(), "hello from upstream");
    }
}

#[tokio::test]
async fn sets_forwarding_headers_and_strips_hop_by_hop() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let resp = client(false)
        .get(format!("https://127.0.0.1:{port}/headers?q=1"))
        .header("connection", "x-private")
        .header("x-private", "secret")
        .header("x-request-id", "my-id")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-request-id"], "my-id");
    let body = resp.text().await.unwrap();
    let has = |line: &str| body.lines().any(|l| l == line);
    assert!(has("uri: /headers?q=1"), "{body}");
    assert!(has(&format!("host: 127.0.0.1:{port}")), "{body}");
    assert!(has("x-forwarded-for: 127.0.0.1"), "{body}");
    assert!(has("x-forwarded-proto: https"), "{body}");
    assert!(
        has(&format!("x-forwarded-host: 127.0.0.1:{port}")),
        "{body}"
    );
    assert!(has(&format!("x-forwarded-port: {port}")), "{body}");
    assert!(has("x-request-id: my-id"), "{body}");
    assert!(!body.contains("x-private"), "{body}");
}

#[tokio::test]
async fn connection_cant_strip_end_to_end_headers() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let mut tls = tls_connect(port).await;
    tls.write_all(
        b"POST /headers HTTP/1.1\r\nHost: example.com\r\nConnection: host, content-length\r\n\
          Content-Length: 5\r\n\r\nhello",
    )
    .await
    .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&got).contains("x-request-id") {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "{}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&got);
    assert!(body.contains("\nhost: example.com\n"), "{body}");
    assert!(body.contains("\ncontent-length: 5\n"), "{body}");
}

#[tokio::test]
async fn http2_requests_get_a_host_header() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let body = client(true)
        .get(url(port, "/headers"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains(&format!("host: localhost:{port}\n")),
        "{body}"
    );
}

#[tokio::test]
async fn streams_large_bodies_both_ways() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let data: Vec<u8> = (0..5_000_000u32).map(|i| (i % 251) as u8).collect();
    for http2 in [false, true] {
        let resp = client(http2)
            .post(url(port, "/echo"))
            .body(data.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.bytes().await.unwrap() == data);
    }
}

#[tokio::test]
async fn explains_when_upstream_is_down() {
    let port = free_port();
    let dead = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{dead}")], port);

    let resp = client(false).get(url(port, "/")).send().await.unwrap();
    assert_eq!(resp.status(), 502);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains(&format!("nothing is listening on localhost:{dead}")),
        "{text}"
    );

    let resp = client(false)
        .get(url(port, "/"))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
    assert!(
        resp.text()
            .await
            .unwrap()
            .contains("<h1>502 Bad Gateway</h1>")
    );
}

#[tokio::test]
async fn localhost_upstream_works_on_either_loopback() {
    let v4 = tcp_upstream("127.0.0.1:0").await;
    let Ok(v6_listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
        eprintln!("skipping IPv6 half: no ::1");
        return;
    };
    let v6 = v6_listener.local_addr().unwrap().port();
    drop(v6_listener);
    let v6 = tcp_upstream(&format!("[::1]:{v6}")).await;

    let (p4, p6) = (free_port(), free_port());
    let _ferry = Ferry::start(&[&format!("{p4}:{v4}"), &format!("{p6}:{v6}")], p6);
    for port in [p4, p6] {
        let resp = client(false).get(url(port, "/")).send().await.unwrap();
        assert_eq!(resp.status(), 200, "port {port}");
    }
}

#[tokio::test]
async fn unix_socket_upstream() {
    let dir = TempDir::new();
    let sock = dir.path("app.sock");
    unix_upstream(&sock).await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{}", sock.display())], port);

    let resp = client(true).get(url(port, "/")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello from upstream");
    upgrade_roundtrip(port).await;
}

#[tokio::test]
async fn upgrade_passthrough() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);
    upgrade_roundtrip(port).await;
}

/// Performs an HTTP/1.1 upgrade through ferry and checks bytes flow both ways.
async fn upgrade_roundtrip(port: u16) {
    let mut tls = tls_connect(port).await;
    tls.write_all(
        b"GET /upgrade HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: test-echo\r\n\r\n",
    )
    .await
    .unwrap();
    let mut buf = vec![0u8; 4096];
    let mut head = Vec::new();
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tls.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed during upgrade");
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head).to_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    assert!(head.contains("upgrade: test-echo"), "{head}");

    for msg in [&b"ping"[..], b"second message"] {
        tls.write_all(msg).await.unwrap();
        let mut got = vec![0u8; msg.len()];
        tokio::time::timeout(Duration::from_secs(5), tls.read_exact(&mut got))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, msg);
    }
}

#[tokio::test]
async fn plain_http_redirects_to_https_on_same_port() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let plain = plain_client();
    let resp = plain
        .get(format!("http://localhost:{port}/a/b?c=d"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 308);
    assert_eq!(
        resp.headers()["location"],
        format!("https://localhost:{port}/a/b?c=d").as_str()
    );
}

#[tokio::test]
async fn an_http_port_redirects_to_the_https_one() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let (http, https) = (free_port(), free_port());
    let ferry = Ferry::start(&[&format!("{http}/{https}:{up}")], https);
    ferry.wait_for_port(http);

    let resp = plain_client()
        .get(format!("http://localhost:{http}/a/b?c=d"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 308);
    assert_eq!(
        resp.headers()["location"],
        format!("https://localhost:{https}/a/b?c=d").as_str()
    );
    // The HTTP port only redirects: it never serves the app, even over TLS.
    assert!(client(false).get(url(http, "/")).send().await.is_err());
    let resp = client(false).get(url(https, "/")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello from upstream");
}

#[tokio::test]
async fn no_redirect_explains_instead() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&["--no-redirect", &format!("{port}:{up}")], port);

    let plain = plain_client();
    let resp = plain
        .get(format!("http://localhost:{port}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("only speaks HTTPS"));
}

#[tokio::test]
async fn max_body_rejects_large_requests() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&["--max-body", "1K", &format!("{port}:{up}")], port);
    let c = client(false);

    let ok = c
        .post(url(port, "/echo"))
        .body(vec![b'a'; 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    let big = c
        .post(url(port, "/echo"))
        .body(vec![b'a'; 1025])
        .send()
        .await
        .unwrap();
    assert_eq!(big.status(), 413);

    // Chunked: no Content-Length, so the limit is enforced while streaming.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        vec![Ok(vec![b'a'; 800]), Ok(vec![b'a'; 800])];
    let body = reqwest::Body::wrap_stream(futures_stream(chunks));
    let chunked = c.post(url(port, "/echo")).body(body).send().await.unwrap();
    assert_eq!(chunked.status(), 413);
}

fn futures_stream(
    items: Vec<Result<Vec<u8>, std::io::Error>>,
) -> impl futures_core::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + Sync + 'static {
    struct Iter(std::vec::IntoIter<Result<Vec<u8>, std::io::Error>>);
    impl futures_core::Stream for Iter {
        type Item = Result<Vec<u8>, std::io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.0.next())
        }
    }
    Iter(items.into_iter())
}

#[tokio::test]
async fn sigterm_shuts_down_cleanly() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let mut ferry = Ferry::start(&[&format!("{port}:{up}")], port);
    // Once a request is served, ferry is fully up.
    client(false).get(url(port, "/")).send().await.unwrap();
    ferry.signal("TERM");
    assert_eq!(
        ferry.wait(Duration::from_secs(5)),
        Some(0),
        "{}",
        ferry.stderr()
    );
}

#[tokio::test]
async fn shutdown_lets_in_flight_requests_finish() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let mut ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let request = tokio::spawn(client(false).get(url(port, "/slow")).send());
    tokio::time::sleep(Duration::from_millis(300)).await;
    ferry.signal("TERM");

    let resp = request.await.unwrap().unwrap();
    assert_eq!(resp.text().await.unwrap(), "slow");
    assert_eq!(ferry.wait(Duration::from_secs(5)), Some(0));
    // New connections are refused once shutdown starts.
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
}
