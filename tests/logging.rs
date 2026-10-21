mod common;

use std::time::Duration;

use common::*;

/// Waits for `f` to return true, for up to two seconds.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..80 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[tokio::test]
async fn access_log_goes_to_stdout_by_default() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let resp = client(false)
        .get(format!("https://127.0.0.1:{port}/hello?x=1"))
        .header("user-agent", "ferry-test")
        .send()
        .await
        .unwrap();
    let id = resp.headers()["x-request-id"].to_str().unwrap().to_string();
    resp.text().await.unwrap();

    assert!(
        eventually(|| ferry.stdout().contains(&id)),
        "{}",
        ferry.stdout()
    );
    let line = ferry.stdout();
    assert!(line.starts_with("127.0.0.1 - - ["), "{line}");
    assert!(
        line.contains("\"GET /hello?x=1 HTTP/1.1\" 200 19 \"-\" \"ferry-test\""),
        "{line}"
    );
}

#[tokio::test]
async fn quiet_silences_stdout() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let ferry = Ferry::start(&["-q", &format!("{port}:{up}")], port);
    client(false)
        .get(format!("https://localhost:{port}/"))
        .send()
        .await
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(ferry.stdout(), "");
}

#[tokio::test]
async fn logs_to_files_in_json() {
    let dir = TempDir::new();
    let access = dir.path("logs/access.log");
    let errors = dir.path("logs/error.log");
    let port = free_port();
    let dead = free_port();
    let ferry = Ferry::start(
        &[
            "--access-log",
            access.to_str().unwrap(),
            "--error-log",
            errors.to_str().unwrap(),
            "--log-format",
            "json",
            &format!("{port}:{dead}"),
        ],
        port,
    );

    let resp = client(true)
        .get(format!("https://localhost:{port}/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    resp.text().await.unwrap();

    let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
    assert!(
        eventually(|| read(&access).contains("\"status\":502")),
        "{}",
        read(&access)
    );
    let entry: serde_json::Value =
        serde_json::from_str(read(&access).lines().next().unwrap()).unwrap();
    assert_eq!(entry["protocol"], "HTTP/2.0");
    assert_eq!(entry["path"], "/x");

    assert!(
        eventually(|| read(&errors).contains("nothing is listening")),
        "{}",
        read(&errors)
    );
    assert_eq!(ferry.stdout(), "");
}

#[tokio::test]
async fn a_log_nobody_reads_doesnt_hold_up_requests() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let home = TempDir::new();
    // stdout is a pipe that's never read: it fills after 64K or so.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg(format!("{port}:{up}"))
        .env("HOME", &home.0)
        .env("XDG_DATA_HOME", home.path("share"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(std::time::Instant::now() < deadline, "ferry didn't start");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Long paths make long lines: a hundred requests fill the pipe.
    let client = client(true);
    let url = format!("https://localhost:{port}/{}", "x".repeat(1000));
    let all = async {
        for _ in 0..1000 {
            let resp = client.get(&url).send().await.unwrap();
            assert_eq!(resp.status(), 200);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .expect("requests stalled behind the access log");
    let _ = child.kill();
    let _ = child.wait();
}
