mod common;

use std::path::Path;
use std::time::Duration;

use common::*;

async fn status(port: u16) -> Option<u16> {
    client(false)
        .get(format!("https://127.0.0.1:{port}/"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()
        .map(|r| r.status().as_u16())
}

/// Polls `f` until it's true or 5 seconds pass.
async fn eventually<F: Future<Output = bool>>(mut f: impl FnMut() -> F) -> bool {
    for _ in 0..100 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Saves the way editors do: write elsewhere, rename over.
fn save(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn proxy(listen: u16, upstream: u16) -> String {
    format!("[[proxy]]\nlisten = {listen}\nupstream = {upstream}\n")
}

#[tokio::test]
async fn config_changes_apply_without_restart() {
    let dir = TempDir::new();
    let path = dir.path("ferry.toml");
    let live = tcp_upstream("127.0.0.1:0").await;
    let dead = free_port();
    let (a, b) = (free_port(), free_port());
    std::fs::write(&path, proxy(a, dead)).unwrap();
    let ferry = Ferry::start(&["-c", path.to_str().unwrap()], a);
    assert_eq!(status(a).await, Some(502));

    // Point the listener at a live upstream and add a second one.
    save(&path, &(proxy(a, live) + &proxy(b, live)));
    assert!(
        eventually(|| async { status(a).await == Some(200) }).await,
        "{}",
        ferry.stderr()
    );
    assert!(eventually(|| async { status(b).await == Some(200) }).await);
    assert!(
        ferry.stderr().contains("reloaded configuration"),
        "{}",
        ferry.stderr()
    );

    // A broken config is rejected and the running one stays.
    save(&path, "[[proxy]]\nlisten = 99999\nupstream = 1\n");
    assert!(
        eventually(|| async { ferry.stderr().contains("not reloading") }).await,
        "{}",
        ferry.stderr()
    );
    assert!(
        ferry
            .stderr()
            .contains("ferry.toml:2: listen: port `99999` is out of range")
    );
    assert_eq!(status(a).await, Some(200));
    assert_eq!(status(b).await, Some(200));

    // Removing a listener closes its port.
    save(&path, &proxy(a, live));
    assert!(
        eventually(|| async { std::net::TcpStream::connect(("127.0.0.1", b)).is_err() }).await,
        "{}",
        ferry.stderr()
    );
    assert_eq!(status(a).await, Some(200));
}

#[tokio::test]
async fn sighup_reloads() {
    let dir = TempDir::new();
    let path = dir.path("ferry.toml");
    let live = tcp_upstream("127.0.0.1:0").await;
    let a = free_port();
    let dead = free_port();
    std::fs::write(&path, proxy(a, dead)).unwrap();
    let ferry = Ferry::start(&["-c", path.to_str().unwrap()], a);

    // Write in place without changing size/inode tricks; SIGHUP forces it.
    std::fs::write(&path, proxy(a, live)).unwrap();
    ferry.signal("HUP");
    assert!(
        eventually(|| async { status(a).await == Some(200) }).await,
        "{}",
        ferry.stderr()
    );
}

async fn served_cert(port: u16) -> Vec<u8> {
    let tls = tls_connect(port).await;
    tls.get_ref().1.peer_certificates().unwrap()[0].to_vec()
}

fn write_cert(dir: &TempDir, name: &str) -> (std::path::PathBuf, std::path::PathBuf, Vec<u8>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let (c, k) = (
        dir.path(&format!("{name}.pem")),
        dir.path(&format!("{name}.key")),
    );
    std::fs::write(&c, cert.cert.pem()).unwrap();
    std::fs::write(&k, cert.signing_key.serialize_pem()).unwrap();
    (c, k, cert.cert.der().to_vec())
}

#[tokio::test]
async fn renewed_certificates_are_picked_up() {
    let dir = TempDir::new();
    let (cert, key, first) = write_cert(&dir, "live");
    let port = free_port();
    let _ferry = Ferry::start(
        &[
            cert.to_str().unwrap(),
            key.to_str().unwrap(),
            &format!("{port}:1"),
        ],
        port,
    );
    assert_eq!(served_cert(port).await, first);

    // certbot-style renewal: new files renamed into place.
    let (new_cert, new_key, second) = write_cert(&dir, "new");
    std::fs::rename(&new_key, &key).unwrap();
    std::fs::rename(&new_cert, &cert).unwrap();
    assert!(eventually(|| async { served_cert(port).await == second }).await);
}

#[tokio::test]
async fn sigusr1_reopens_logs() {
    let dir = TempDir::new();
    let log = dir.path("access.log");
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let ferry = Ferry::start(
        &[
            "--access-log",
            log.to_str().unwrap(),
            &format!("{port}:{up}"),
        ],
        port,
    );
    assert_eq!(status(port).await, Some(200));
    assert!(
        eventually(|| async { std::fs::read_to_string(&log).is_ok_and(|s| s.contains(" 200 ")) })
            .await
    );

    std::fs::rename(&log, dir.path("access.log.1")).unwrap();
    ferry.signal("USR1");
    assert!(eventually(|| async { ferry.stderr().contains("reopened log files") }).await);
    assert_eq!(status(port).await, Some(200));
    assert!(
        eventually(|| async { std::fs::read_to_string(&log).is_ok_and(|s| s.contains(" 200 ")) })
            .await,
        "{}",
        ferry.stderr()
    );
}
