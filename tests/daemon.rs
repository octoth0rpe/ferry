mod common;

use std::process::Command;
use std::time::Duration;

use common::*;

fn ferry_in(dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(args)
        .current_dir(&dir.0)
        .env("HOME", &dir.0)
        .env("XDG_DATA_HOME", dir.path("share"))
        .output()
        .unwrap()
}

fn alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success()
}

async fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn daemonizes_writes_pid_file_and_keeps_relative_paths() {
    let dir = TempDir::new();
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    std::fs::write(
        dir.path("ferry.toml"),
        format!("access_log = \"logs/access.log\"\nerror_log = \"logs/error.log\"\npid_file = \"run/ferry.pid\"\n[[proxy]]\nlisten = {port}\nupstream = {}\n", free_port()),
    )
    .unwrap();

    let out = ferry_in(&dir, &["-d", "-c", "ferry.toml"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("https://localhost:{port}")),
        "banner: {stderr}"
    );

    let pid: i32 = std::fs::read_to_string(dir.path("run/ferry.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid));

    let get = || async {
        client(false)
            .get(format!("https://127.0.0.1:{port}/"))
            .send()
            .await
            .map(|r| r.status().as_u16())
            .ok()
    };
    assert_eq!(get().await, Some(502));

    // Reload after the daemon changed directory still resolves paths.
    std::fs::write(
        dir.path("ferry.toml"),
        format!("access_log = \"logs/access.log\"\nerror_log = \"logs/error.log\"\npid_file = \"run/ferry.pid\"\n[[proxy]]\nlisten = {port}\nupstream = {up}\n"),
    )
    .unwrap();
    Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .unwrap();
    let mut ok = false;
    for _ in 0..100 {
        if get().await == Some(200) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let errors = std::fs::read_to_string(dir.path("logs/error.log")).unwrap_or_default();
    assert!(ok, "{errors}");
    assert!(
        std::fs::read_to_string(dir.path("logs/access.log"))
            .unwrap()
            .contains(" 200 ")
    );

    // A second instance with the same pid file refuses to start.
    let out = ferry_in(
        &dir,
        &[
            "-d",
            "--pid-file",
            "run/ferry.pid",
            &format!("{}:1", free_port()),
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("already running (pid {pid}")));

    Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(wait_until(|| !alive(pid)).await);
    assert!(wait_until(|| !dir.path("run/ferry.pid").exists()).await);
}

#[test]
fn startup_errors_reach_the_terminal() {
    let dir = TempDir::new();
    let busy = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let out = ferry_in(&dir, &["-d", &format!("{port}:1")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already in use"));
}

#[test]
fn warns_that_stdout_logs_are_discarded() {
    let dir = TempDir::new();
    let port = free_port();
    let out = ferry_in(&dir, &["-d", "--pid-file", "p", &format!("{port}:1")]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("discards the access log"), "{stderr}");
    assert!(stderr.contains("discards the error log"), "{stderr}");
    let pid = std::fs::read_to_string(dir.path("p")).unwrap();
    Command::new("kill").arg(pid.trim()).status().unwrap();
}

#[test]
fn the_pid_file_is_locked_while_ferry_runs() {
    let home = TempDir::new();
    let pid_file = home.path("ferry.pid");
    let port = free_port();
    let args = [
        "--pid-file",
        pid_file.to_str().unwrap(),
        &format!("{port}:1"),
    ];
    let first = Ferry::start_in(home, &args, port);
    // ferry listens before it writes the pid file.
    let pid = first.child.id().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&pid_file).map_or(true, |s| s.trim() != pid) {
        assert!(std::time::Instant::now() < deadline, "no pid file");
        std::thread::sleep(Duration::from_millis(25));
    }
    // Even if the pid in it is unreadable, a second ferry can't take it.
    std::fs::write(&pid_file, "junk").unwrap();
    let mut second = Ferry::spawn(&[
        "--pid-file",
        pid_file.to_str().unwrap(),
        &format!("{}:1", free_port()),
    ]);
    assert_eq!(second.wait(Duration::from_secs(10)), Some(1));
    let err = second.stderr();
    assert!(err.contains("ferry is already running (it holds"), "{err}");
    drop(first);
}

#[test]
fn a_symlinked_pid_file_is_refused() {
    let dir = TempDir::new();
    let victim = dir.path("victim");
    std::fs::write(&victim, "keep me").unwrap();
    std::os::unix::fs::symlink(&victim, dir.path("ferry.pid")).unwrap();
    let out = ferry_in(
        &dir,
        &["--pid-file", "ferry.pid", &format!("{}:1", free_port())],
    );
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("it's a symbolic link"), "{err}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
}
