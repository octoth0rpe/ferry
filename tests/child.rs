mod common;

use std::time::{Duration, Instant};

use common::*;

#[tokio::test]
async fn holds_requests_and_the_banner_until_the_app_listens() {
    let up = free_port();
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:{up}"), "--", "sleep", "30"], port);

    let started = Instant::now();
    let request = tokio::spawn(async move {
        client(false)
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !ferry.stderr().contains("https://localhost"),
        "{}",
        ferry.stderr()
    );

    // The "app" starts listening; the held request goes through.
    tcp_upstream(&format!("127.0.0.1:{up}")).await;
    assert_eq!(request.await.unwrap(), "hello from upstream");
    assert!(started.elapsed() >= Duration::from_millis(700));
    let err = ferry.wait_for_stderr(&format!("https://localhost:{port}"));
    assert!(err.contains(&format!("https://localhost:{port}")), "{err}");
}

#[test]
fn exits_with_the_commands_exit_code() {
    let port = free_port();
    let mut ferry = Ferry::spawn(&[&format!("{port}:1"), "--", "sh", "-c", "exit 3"]);
    assert_eq!(
        ferry.wait(Duration::from_secs(10)),
        Some(3),
        "{}",
        ferry.stderr()
    );
    assert!(
        ferry.stderr().contains("`sh -c exit 3` exited"),
        "{}",
        ferry.stderr()
    );
}

#[test]
fn passes_termination_signals_on() {
    for (sig, code) in [("TERM", 7), ("INT", 5)] {
        let port = free_port();
        let script = format!("trap 'exit {code}' {sig}; while :; do sleep 0.05; done");
        let mut ferry = Ferry::start(&[&format!("{port}:1"), "--", "sh", "-c", &script], port);
        // Give sh a moment to install its trap.
        std::thread::sleep(Duration::from_millis(300));
        ferry.signal(sig);
        assert_eq!(
            ferry.wait(Duration::from_secs(10)),
            Some(code),
            "{sig}: {}",
            ferry.stderr()
        );
    }
}

#[test]
fn the_commands_output_passes_through() {
    let port = free_port();
    let ferry = Ferry::start(
        &[
            &format!("{port}:1"),
            "--",
            "sh",
            "-c",
            "echo out-from-app; echo err-from-app >&2; sleep 30",
        ],
        port,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(ferry.stdout().contains("out-from-app") && ferry.stderr().contains("err-from-app")) {
        assert!(
            Instant::now() < deadline,
            "{}\n{}",
            ferry.stdout(),
            ferry.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn explains_a_missing_command() {
    let (code, _, err) = run_ferry(&[&format!("{}:1", free_port()), "--", "no-such-command-ferry"]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("can't run `no-such-command-ferry`: command not found"),
        "{err}"
    );
}

#[test]
fn a_command_needs_the_foreground() {
    let (code, _, err) = run_ferry(&["-d", &format!("{}:1", free_port()), "--", "true"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("needs ferry in the foreground"), "{err}");
}
