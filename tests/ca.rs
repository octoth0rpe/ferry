mod common;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use common::*;

fn root_der(ferry: &Ferry) -> Vec<u8> {
    CertificateDer::from_pem_file(ferry.data_dir().join("ca.pem"))
        .unwrap()
        .to_vec()
}

fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[tokio::test]
async fn default_certificate_chains_to_the_local_ca() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:{up}")], port);

    let client = ca_client(&ferry.data_dir().join("ca.pem"));
    for host in ["localhost", "127.0.0.1"] {
        let resp = client
            .get(format!("https://{host}:{port}/"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{host}: {e:?}"));
        assert_eq!(resp.text().await.unwrap(), "hello from upstream");
    }
    let err = ferry.wait_for_stderr("browsers warn until you trust the CA");
    assert!(
        err.contains("created ferry's local CA (none existed yet)"),
        "{err}"
    );
    assert!(err.contains("cert: ferry local CA ("), "{err}");
}

/// The trust page's address on this machine, from the banner.
fn trust_url(ferry: &Ferry) -> String {
    let err = ferry.wait_for_stderr("browsers warn until you trust the CA");
    let start = err
        .find("see http://localhost:")
        .unwrap_or_else(|| panic!("{err}"))
        + 4;
    let url = &err[start..];
    url[..url.find(char::is_whitespace).unwrap()].to_string()
}

#[tokio::test]
async fn the_trust_page_has_a_port_of_its_own() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:{up}")], port);
    let der = root_der(&ferry);
    let fp = fingerprint(&der);
    let page = trust_url(&ferry);
    assert!(!page.contains(&format!(":{port}/")), "{page}");
    let http = plain_client();

    let resp = http.get(&page).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
    let html = resp.text().await.unwrap();
    assert!(
        html.contains(&fp[..47]) && html.contains(&fp[48..]),
        "{html}"
    );
    assert!(html.contains("Certificate Trust Settings"), "{html}");

    for (path, attachment) in [("ca.crt", false), ("ferry-ca.crt", true)] {
        let resp = http.get(format!("{page}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "application/x-x509-ca-cert");
        assert_eq!(
            resp.headers().contains_key("content-disposition"),
            attachment
        );
        assert_eq!(resp.bytes().await.unwrap().to_vec(), der);
    }
    let resp = http.get(format!("{page}other")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let resp = http.post(&page).send().await.unwrap();
    assert_eq!(resp.status(), 405);

    // The banner shows the same fingerprint.
    assert!(ferry.stderr().contains(&fp[..47]), "{}", ferry.stderr());
}

#[tokio::test]
async fn app_ports_are_left_to_the_app() {
    let up = tcp_upstream("127.0.0.1:0").await;
    let port = free_port();
    let _ferry = Ferry::start(&[&format!("{port}:{up}")], port);
    // Plain HTTP redirects, whatever the path; HTTPS goes to the app.
    let resp = plain_client()
        .get(format!("http://localhost:{port}/.ferry/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 308);
    let resp = client(false)
        .get(format!("https://localhost:{port}/.ferry/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello from upstream");
}

#[tokio::test]
async fn the_trust_page_keeps_its_port_across_reloads() {
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:1")], port);
    let page = trust_url(&ferry);
    ferry.signal("HUP");
    ferry.wait_for_stderr("reloaded configuration");
    let resp = plain_client().get(&page).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn no_trust_page_with_your_own_certificate() {
    let dir = TempDir::new();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let (c, k) = (dir.path("c.pem"), dir.path("k.pem"));
    std::fs::write(&c, cert.cert.pem()).unwrap();
    std::fs::write(&k, cert.signing_key.serialize_pem()).unwrap();
    let port = free_port();
    let ferry = Ferry::start(
        &[
            c.to_str().unwrap(),
            k.to_str().unwrap(),
            &format!("{port}:1"),
        ],
        port,
    );
    assert!(!ferry.data_dir().exists());
    let err = ferry.wait_for_stderr("cert:");
    assert!(err.contains("cert:") && !err.contains("local CA"), "{err}");
}

#[tokio::test]
async fn root_is_kept_and_server_certificate_reissued_across_restarts() {
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:1")], port);
    let root = root_der(&ferry);
    let server = std::fs::read(ferry.data_dir().join("server.pem")).unwrap();
    let home = ferry.stop();

    let ferry = Ferry::start_in(home, &[&format!("{port}:1")], port);
    assert_eq!(root_der(&ferry), root);
    assert_ne!(
        std::fs::read(ferry.data_dir().join("server.pem")).unwrap(),
        server
    );
    let err = ferry.wait_for_stderr("browsers warn until you trust the CA");
    assert!(!err.contains("created ferry's local CA"), "{err}");
    assert!(!err.contains("now uses a local CA"), "{err}");
}

#[tokio::test]
async fn upgrading_from_the_self_signed_certificate_says_so() {
    let home = TempDir::new();
    let dir = data_dir(&home);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("self-signed.pem"), "old").unwrap();

    let port = free_port();
    let ferry = Ferry::start_in(home, &[&format!("{port}:1")], port);
    let err = ferry.wait_for_stderr("browsers warn until you trust the CA");
    assert!(
        err.contains("ferry now uses a local CA instead of a self-signed certificate"),
        "{err}"
    );
}

/// Whether a DER certificate has a nameConstraints extension (OID 2.5.29.30).
fn has_name_constraints(der: &[u8]) -> bool {
    der.windows(5).any(|w| w == [0x06, 0x03, 0x55, 0x1d, 0x1e])
}

#[tokio::test]
async fn new_cas_have_name_constraints_unless_turned_off() {
    let port = free_port();
    let ferry = Ferry::start(&[&format!("{port}:1")], port);
    assert!(has_name_constraints(&root_der(&ferry)));
    drop(ferry);

    let dir = TempDir::new();
    let config = dir.path("ferry.toml");
    std::fs::write(
        &config,
        format!(
            "[[proxy]]\nlisten = {port}\nupstream = 1\n\n[advanced]\nca_name_constraints = false\n"
        ),
    )
    .unwrap();
    let ferry = Ferry::start(&["-c", config.to_str().unwrap()], port);
    assert!(!has_name_constraints(&root_der(&ferry)));
}

#[tokio::test]
async fn loopback_only_apps_keep_the_trust_page_local() {
    let port = free_port();
    let ferry = Ferry::start(&[&format!("127.0.0.1:{port}:1")], port);
    let page = trust_url(&ferry);
    let resp = plain_client().get(&page).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(!ferry.stderr().contains("on a phone"), "{}", ferry.stderr());
}
