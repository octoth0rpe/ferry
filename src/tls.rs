//! TLS termination: certificate loading and the rustls server config.

use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::config::CertKey;
use crate::der;

/// A certificate and key loaded from disk and checked to belong together.
#[derive(Debug, Clone)]
pub struct LoadedCert {
    pub key: Arc<CertifiedKey>,
    /// Expiry of the leaf certificate, as a Unix timestamp.
    pub not_after: i64,
    /// Problems worth mentioning that don't stop ferry from serving.
    pub warnings: Vec<String>,
}

/// Warn about certificates expiring sooner than this.
pub const EXPIRY_WARNING_DAYS: i64 = 14;

/// Whether the certificate at `path`, expiring at `not_after`, has expired
/// (`Err`) or expires within [`EXPIRY_WARNING_DAYS`] (`Ok(Some)`), as of
/// `now`.
pub fn expiry(path: &Path, not_after: i64, now: i64) -> Result<Option<String>, String> {
    let date = crate::date::date(not_after);
    let days = (not_after - now) / 86_400;
    if not_after <= now {
        Err(format!("{} expired on {date}", path.display()))
    } else if days < EXPIRY_WARNING_DAYS {
        Ok(Some(format!(
            "{} expires in {days} day(s), on {date}",
            path.display()
        )))
    } else {
        Ok(None)
    }
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Loads a PEM certificate chain and private key.
pub fn load(ck: &CertKey) -> Result<LoadedCert, String> {
    let cert_name = ck.cert.display();
    let key_name = ck.key.display();

    let certs = CertificateDer::pem_file_iter(&ck.cert)
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("{cert_name}: {}", pem_error(&e)))?;
    if certs.is_empty() {
        return Err(format!("{cert_name}: no certificates found"));
    }

    let key = PrivateKeyDer::from_pem_file(&ck.key).map_err(|e| match e {
        rustls::pki_types::pem::Error::NoItemsFound if is_encrypted_key(&ck.key) => {
            format!("{key_name}: encrypted private keys aren't supported; decrypt it first")
        }
        rustls::pki_types::pem::Error::NoItemsFound => {
            format!("{key_name}: no private key found")
        }
        e => format!("{key_name}: {}", pem_error(&e)),
    })?;

    from_der(certs, key, &cert_name.to_string(), &key_name.to_string())
}

/// Builds a [`LoadedCert`] from DER data, checking the key matches the leaf.
pub fn from_der(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    cert_name: &str,
    key_name: &str,
) -> Result<LoadedCert, String> {
    let leaf =
        der::parse(&certs[0]).ok_or_else(|| format!("{cert_name}: can't parse certificate"))?;
    let not_after = leaf.not_after;
    let self_signed = leaf.self_signed;

    let mut warnings = Vec::new();
    if certs.len() == 1 && !self_signed {
        warnings.push(format!(
            "{cert_name} contains only the server certificate; if it was issued by an \
             intermediate CA, use the full chain (e.g. fullchain.pem) or some clients will \
             reject it"
        ));
    }

    let key = CertifiedKey::from_der(certs, key, &provider()).map_err(|e| match e {
        rustls::Error::InconsistentKeys(_) => {
            format!("{key_name} does not match the certificate in {cert_name}")
        }
        e => format!("{key_name}: {e}"),
    })?;

    Ok(LoadedCert {
        key: Arc::new(key),
        not_after,
        warnings,
    })
}

fn pem_error(e: &rustls::pki_types::pem::Error) -> String {
    match e {
        rustls::pki_types::pem::Error::Io(e) => e.to_string(),
        e => format!("invalid PEM: {e:?}"),
    }
}

fn is_encrypted_key(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| s.contains("ENCRYPTED"))
}

/// Builds the rustls server config for one listener, offering HTTP/2 and
/// HTTP/1.1 via ALPN.
pub fn server_config(cert: &LoadedCert) -> Arc<ServerConfig> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SingleCert(cert.key.clone())));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Serves one certificate regardless of SNI, including to clients that
/// connect by IP address and send no SNI at all.
#[derive(Debug)]
struct SingleCert(Arc<CertifiedKey>);

impl ResolvesServerCert for SingleCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Writes a fresh self-signed cert and key for `localhost` into `dir`.
    pub fn write_test_cert(dir: &Path, name: &str) -> CertKey {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let ck = CertKey {
            cert: dir.join(format!("{name}.pem")),
            key: dir.join(format!("{name}.key")),
        };
        std::fs::write(&ck.cert, cert.cert.pem()).unwrap();
        std::fs::write(&ck.key, cert.signing_key.serialize_pem()).unwrap();
        ck
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferry-tls-{}", crate::proxy::random_id(12)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_matching_pair() {
        let dir = tempdir();
        let ck = write_test_cert(&dir, "a");
        let loaded = load(&ck).unwrap();
        assert!(loaded.warnings.is_empty());
        assert!(loaded.not_after > 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn warns_about_a_leaf_without_its_chain() {
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Test CA");
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca = rcgen::Issuer::new(ca_params, ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["example.com".to_string()])
            .unwrap()
            .signed_by(&key, &ca)
            .unwrap();
        let load = |certs: Vec<CertificateDer<'static>>| {
            let key = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
            from_der(certs, key, "c.pem", "k.pem").unwrap()
        };

        let alone = load(vec![leaf.der().clone()]);
        assert_eq!(alone.warnings.len(), 1);
        assert!(alone.warnings[0].contains("only the server certificate"));
        let chain = load(vec![leaf.der().clone(), ca_cert.der().clone()]);
        assert!(chain.warnings.is_empty(), "{:?}", chain.warnings);
    }

    #[test]
    fn rejects_mismatched_key() {
        let dir = tempdir();
        let a = write_test_cert(&dir, "a");
        let b = write_test_cert(&dir, "b");
        let e = load(&CertKey {
            cert: a.cert,
            key: b.key,
        })
        .unwrap_err();
        assert!(e.contains("does not match the certificate"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_missing_and_empty_files() {
        let dir = tempdir();
        let a = write_test_cert(&dir, "a");
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "").unwrap();

        let e = load(&CertKey {
            cert: empty.clone(),
            key: a.key.clone(),
        })
        .unwrap_err();
        assert!(e.ends_with("no certificates found"), "{e}");

        let e = load(&CertKey {
            cert: a.cert.clone(),
            key: empty,
        })
        .unwrap_err();
        assert!(e.ends_with("no private key found"), "{e}");

        let e = load(&CertKey {
            cert: dir.join("missing.pem"),
            key: a.key,
        })
        .unwrap_err();
        assert!(e.contains("missing.pem"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
