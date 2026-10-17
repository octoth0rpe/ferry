mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;

fn write_cert(dir: &TempDir, name: &str, days: i64) -> (String, String) {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let at = |secs: i64| rcgen::date_time_ymd(1970, 1, 1) + Duration::from_secs(secs as u64);
    params.not_before = at(now - 400 * 86_400);
    params.not_after = at(now + days * 86_400);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let (c, k) = (
        dir.path(&format!("{name}.pem")),
        dir.path(&format!("{name}.key")),
    );
    std::fs::write(&c, cert.pem()).unwrap();
    std::fs::write(&k, key.serialize_pem()).unwrap();
    (c.display().to_string(), k.display().to_string())
}

#[test]
fn valid_config_passes() {
    let dir = TempDir::new();
    let (c, k) = write_cert(&dir, "ok", 90);
    let (code, out, err) = run_ferry(&["-t", &c, &k, &format!("{}:6006", free_port())]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "configuration: ok (1 listener)\n");
}

#[test]
fn local_ca_is_noted_not_created() {
    let (code, _, err) = run_ferry(&["-t", &format!("{}:6006", free_port())]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("will create ferry's local CA at"), "{err}");
}

#[test]
fn expired_cert_fails_and_expiring_warns() {
    let dir = TempDir::new();
    let (c, k) = write_cert(&dir, "old", -1);
    let (code, out, err) = run_ferry(&["-t", &c, &k, &format!("{}:6006", free_port())]);
    assert_eq!(code, 1);
    assert!(err.contains("old.pem expired on"), "{err}");
    assert_eq!(out, "configuration: 1 error(s)\n");

    let (c, k) = write_cert(&dir, "soon", 5);
    let (code, _, err) = run_ferry(&["-t", &c, &k, &format!("{}:6006", free_port())]);
    assert_eq!(code, 0);
    assert!(
        err.contains("warning:") && err.contains("soon.pem expires in"),
        "{err}"
    );
}

#[test]
fn reports_all_config_errors() {
    let dir = TempDir::new();
    let path = dir.path("ferry.toml");
    std::fs::write(
        &path,
        "[[proxy]]\nlisten = 0\nupstream = 1\n\n[[proxy]]\nlisten = 2\nupstream = \"nope\"\n",
    )
    .unwrap();
    let (code, out, err) = run_ferry(&["-t", "-c", path.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert_eq!(out, "configuration: 2 error(s)\n");
    assert!(err.contains("ferry.toml:2: listen:"), "{err}");
    assert!(err.contains("ferry.toml:7: upstream:"), "{err}");
}

#[test]
fn warns_about_busy_ports_and_missing_sockets() {
    let busy = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let (code, _, err) = run_ferry(&["-t", &format!("{port}:/nonexistent/app.sock")]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("already in use"), "{err}");
    assert!(
        err.contains("nothing at /nonexistent/app.sock yet"),
        "{err}"
    );
}
