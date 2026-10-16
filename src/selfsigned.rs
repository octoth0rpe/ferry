//! The default self-signed certificate, used when no cert/key is configured.
//!
//! It's generated once and kept in ferry's data directory, so a browser
//! exception or a trust-store entry made for it keeps working across runs.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use time::{Duration, OffsetDateTime};

use crate::config::CertKey;
use crate::net;
use crate::tls::{self, LoadedCert};

/// Regenerate when the cert expires sooner than this.
const RENEW_BEFORE_DAYS: i64 = 30;
/// Validity of a new cert. Apple platforms reject TLS certs valid for more
/// than 825 days, even locally trusted ones.
const VALID_DAYS: i64 = 397;

/// Where the self-signed certificate lives.
pub fn paths() -> Result<CertKey, String> {
    let dir = dirs::data_dir()
        .ok_or("can't find a data directory for the self-signed certificate (is HOME set?)")?
        .join("ferry");
    Ok(CertKey {
        cert: dir.join("self-signed.pem"),
        key: dir.join("self-signed-key.pem"),
    })
}

/// The names the certificate should cover on this machine.
pub fn wanted_names() -> BTreeSet<String> {
    let mut names: BTreeSet<String> = ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(String::from)
        .collect();
    if let Ok(host) = nix::unistd::gethostname()
        && let Some(host) = host.to_str()
        && !host.is_empty()
    {
        let host = host.to_ascii_lowercase();
        if !host.contains('.') {
            names.insert(format!("{host}.local"));
        }
        names.insert(host);
    }
    for ip in net::local_ips() {
        names.insert(ip.to_string());
    }
    names
}

/// Outcome of [`load_or_create`].
pub struct SelfSigned {
    pub cert: LoadedCert,
    pub paths: CertKey,
    /// Why a new certificate was generated, if one was.
    pub generated: Option<String>,
}

/// Loads the stored self-signed certificate, generating a new one if there
/// isn't one, it expires soon, or it doesn't cover this machine's names.
pub fn load_or_create() -> Result<SelfSigned, String> {
    let paths = paths()?;
    let wanted = wanted_names();
    let now = OffsetDateTime::now_utc().unix_timestamp();

    let reason = match (paths.cert.exists() && paths.key.exists()).then(|| tls::load(&paths)) {
        None => "none existed yet".to_string(),
        Some(Err(e)) => format!("the stored one couldn't be loaded ({e})"),
        Some(Ok(cert)) => {
            let have = cert_names(&paths.cert).unwrap_or_default();
            // Only missing hostnames force a new cert. IP addresses change as
            // a laptop moves between networks, and regenerating would undo
            // any trust given to the old cert.
            let missing: Vec<_> = wanted
                .difference(&have)
                .filter(|name| name.parse::<std::net::IpAddr>().is_err())
                .cloned()
                .collect();
            if cert.not_after - now < RENEW_BEFORE_DAYS * 86_400 {
                "the stored one expires soon".to_string()
            } else if !missing.is_empty() {
                format!("the stored one doesn't cover {}", missing.join(", "))
            } else {
                return Ok(SelfSigned {
                    cert,
                    paths,
                    generated: None,
                });
            }
        }
    };

    // Keep any names the old cert had, so addresses from other networks this
    // machine moves between stay covered.
    let mut names = cert_names(&paths.cert).unwrap_or_default();
    names.extend(wanted);
    generate(&paths, &names)?;
    let cert = tls::load(&paths)?;
    Ok(SelfSigned {
        cert,
        paths,
        generated: Some(reason),
    })
}

fn generate(paths: &CertKey, names: &BTreeSet<String>) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("can't generate self-signed certificate: {e}");
    let mut params =
        CertificateParams::new(names.iter().cloned().collect::<Vec<_>>()).map_err(|e| err(&e))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "ferry self-signed certificate");
    params.distinguished_name = dn;
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(VALID_DAYS);
    let key = KeyPair::generate().map_err(|e| err(&e))?;
    let cert = params.self_signed(&key).map_err(|e| err(&e))?;

    let dir = paths.cert.parent().expect("cert path has a parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    write_file(&paths.key, key.serialize_pem().as_bytes(), 0o600)?;
    write_file(&paths.cert, cert.pem().as_bytes(), 0o644)?;
    Ok(())
}

/// Writes via a temporary file and rename, so a crash never leaves a cert
/// that doesn't match its key.
fn write_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let err = |e: std::io::Error| format!("can't write {}: {e}", path.display());
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(err)?;
    f.write_all(contents).map_err(err)?;
    f.sync_all().map_err(err)?;
    fs::rename(&tmp, path).map_err(err)
}

/// DNS names and IP addresses in a certificate's subjectAltName.
fn cert_names(path: &PathBuf) -> Option<BTreeSet<String>> {
    use x509_parser::extensions::GeneralName;
    use x509_parser::pem::parse_x509_pem;

    let pem_bytes = fs::read(path).ok()?;
    let (_, pem) = parse_x509_pem(&pem_bytes).ok()?;
    let cert = pem.parse_x509().ok()?;
    let san = cert.subject_alternative_name().ok()??;
    let names = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(s) => Some(s.to_string()),
            GeneralName::IPAddress(b) => match b.len() {
                4 => Some(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(*b).ok()?).to_string()),
                16 => Some(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(*b).ok()?).to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_then_reuses() {
        let dir = std::env::temp_dir().join(format!("ferry-ss-{}", fastrand::u64(..)));
        let paths = CertKey {
            cert: dir.join("c.pem"),
            key: dir.join("k.pem"),
        };
        let names: BTreeSet<String> = ["localhost".into(), "127.0.0.1".into(), "::1".into()].into();
        generate(&paths, &names).unwrap();

        let loaded = tls::load(&paths).unwrap();
        assert!(loaded.not_after > OffsetDateTime::now_utc().unix_timestamp() + 300 * 86_400);
        assert_eq!(cert_names(&paths.cert).unwrap(), names);
        let mode = fs::metadata(&paths.key).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wanted_names_include_loopback() {
        let names = wanted_names();
        assert!(
            names.contains("localhost") && names.contains("127.0.0.1") && names.contains("::1")
        );
    }
}
