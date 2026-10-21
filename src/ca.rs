//! ferry's local certificate authority, used when no cert/key is configured.
//!
//! The root is created once and kept in ferry's data directory. It's the
//! thing people trust, on this machine and on their phones. The server
//! certificate is issued from it on every start and reload, so it always
//! covers the machine's current names and addresses without anyone having to
//! trust anything again.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints,
};
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};

use crate::config::CertKey;
use crate::tls::{self, LoadedCert};
use crate::{date, der, dirs, net};

/// Replace the root, or reissue the server certificate, when it expires
/// sooner than this.
const RENEW_BEFORE_DAYS: i64 = 30;
/// Validity of the root. Apple's limits on certificate lifetimes don't apply
/// to roots people add themselves.
const ROOT_VALID_DAYS: i64 = 3650;
/// Validity of the server certificate. Apple platforms reject TLS certs valid
/// for more than 825 days, even locally trusted ones; 397 is the public limit.
const SERVER_VALID_DAYS: i64 = 397;

/// Where the local CA's files live.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The root certificate and its key.
    pub root: CertKey,
    /// The current server certificate (with the root after it) and its key.
    pub server: CertKey,
    /// Where ferry before the local CA kept its self-signed certificate.
    pub old_self_signed: PathBuf,
}

pub fn paths() -> Result<Paths, String> {
    let dir = dirs::data_dir()
        .ok_or("can't find a data directory for ferry's local CA (is HOME set?)")?
        .join("ferry");
    Ok(paths_in(&dir))
}

fn paths_in(dir: &Path) -> Paths {
    Paths {
        root: CertKey {
            cert: dir.join("ca.pem"),
            key: dir.join("ca-key.pem"),
        },
        server: CertKey {
            cert: dir.join("server.pem"),
            key: dir.join("server-key.pem"),
        },
        old_self_signed: dir.join("self-signed.pem"),
    }
}

/// The names the server certificate should cover on this machine, before
/// any the root's name constraints rule out.
pub fn wanted_names() -> BTreeSet<String> {
    let mut names: BTreeSet<String> = ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(String::from)
        .collect();
    let host = hostname();
    if let Some(host) = &host {
        if !host.contains('.') {
            names.insert(format!("{host}.local"));
        }
        names.insert(host.clone());
    }
    for ip in net::local_ips() {
        names.insert(ip.to_string());
    }
    names
}

fn hostname() -> Option<String> {
    let host = nix::unistd::gethostname().ok()?;
    let host = host.to_str()?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The local CA, and a server certificate just issued from it.
pub struct LocalCa {
    /// The server certificate, followed by the root.
    pub cert: LoadedCert,
    pub paths: Paths,
    /// The root certificate, DER-encoded.
    pub root_der: Vec<u8>,
    /// The root's common name, as devices list it.
    pub root_name: String,
    /// The root's SHA-256 fingerprint: hex bytes separated by colons.
    pub fingerprint: String,
    /// The names the server certificate was issued for, before any the
    /// root's name constraints ruled out. Compared with [`wanted_names`] to
    /// notice the machine's names changing.
    pub wanted: BTreeSet<String>,
    /// When the server certificate expires, as a Unix timestamp.
    pub not_after: i64,
    /// Why a new root was created, if one was.
    pub created: Option<String>,
    /// Names the server certificate doesn't cover, and why.
    pub warnings: Vec<String>,
}

impl LocalCa {
    /// Why the server certificate should be issued again, if it should: the
    /// machine's names or addresses have changed, or it's close to expiring.
    pub fn reissue_reason(wanted: &BTreeSet<String>, not_after: i64) -> Option<&'static str> {
        if *wanted != wanted_names() {
            Some("this machine's names or addresses changed")
        } else if not_after - date::now() < RENEW_BEFORE_DAYS * 86_400 {
            Some("the server certificate expires soon")
        } else {
            None
        }
    }
}

/// Loads the root, creating it if there isn't a usable one, and issues a
/// server certificate from it for this machine's names. A new root gets name
/// constraints if `constrain` is set.
pub fn load_or_create(constrain: bool) -> Result<LocalCa, String> {
    let paths = paths()?;
    let wanted = wanted_names();

    let (root, created) = match load_root(&paths.root) {
        Ok(root) => (root, None),
        Err(why) => {
            let constraints = constrain.then(name_constraints);
            let root = create_root(&paths.root, constraints).map_err(with_home_hint)?;
            (root, Some(why))
        }
    };
    let (cert, not_after, warnings) = issue_for(&root, &wanted, &paths).map_err(with_home_hint)?;
    Ok(LocalCa {
        cert,
        root_name: root.name,
        fingerprint: fingerprint(&root.der),
        root_der: root.der.to_vec(),
        paths,
        wanted,
        not_after,
        created,
        warnings,
    })
}

/// Adds a hint to `err` when HOME isn't ours, the likely reason ferry
/// couldn't write its files there.
fn with_home_hint(err: String) -> String {
    match dirs::home_hint() {
        Some(hint) => format!("{err}. {hint}"),
        None => err,
    }
}

/// Issues a server certificate for as many of `wanted` as the root permits.
/// A constrained root covers local names and private addresses only. A bare
/// or fully qualified hostname is left out quietly (`hostname.local` covers
/// the machine); an address it can't cover gets a warning, since someone
/// might be trying to reach ferry at it.
fn issue_for(
    root: &Root,
    wanted: &BTreeSet<String>,
    paths: &Paths,
) -> Result<(LoadedCert, i64, Vec<String>), String> {
    if !root.constrained {
        let (cert, not_after) = issue(root, wanted, paths)?;
        return Ok((cert, not_after, Vec::new()));
    }
    let constraints = name_constraints();
    let (names, left_out): (BTreeSet<String>, BTreeSet<String>) = wanted
        .iter()
        .cloned()
        .partition(|name| permits(&constraints, name));
    let warnings = left_out
        .iter()
        .filter(|name| name.parse::<IpAddr>().is_ok())
        .map(|ip| {
            format!(
                "the certificate from ferry's local CA doesn't cover {ip}: the CA only covers \
                 private addresses"
            )
        })
        .collect();
    let (cert, not_after) = issue(root, &names, paths)?;
    Ok((cert, not_after, warnings))
}

struct Root {
    der: CertificateDer<'static>,
    pem: String,
    name: String,
    not_after: i64,
    /// Whether it has name constraints.
    constrained: bool,
    issuer: Issuer<'static, KeyPair>,
}

/// Loads the stored root. The error says why a new one is needed.
fn load_root(paths: &CertKey) -> Result<Root, String> {
    if !paths.cert.exists() && !paths.key.exists() {
        return Err("none existed yet".into());
    }
    let unusable = |e: &dyn std::fmt::Display| format!("the stored one couldn't be loaded ({e})");
    let pem = fs::read_to_string(&paths.cert)
        .map_err(|e| unusable(&format!("{}: {e}", paths.cert.display())))?;
    let der = CertificateDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| unusable(&format!("{}: {e:?}", paths.cert.display())))?;
    let key = fs::read_to_string(&paths.key)
        .map_err(|e| unusable(&format!("{}: {e}", paths.key.display())))
        .and_then(|pem| {
            KeyPair::from_pem(&pem).map_err(|e| unusable(&format!("{}: {e}", paths.key.display())))
        })?;
    let parsed = der::parse(&der).ok_or_else(|| unusable(&"can't parse it"))?;
    let name = parsed
        .common_name
        .ok_or_else(|| unusable(&"it has no common name"))?;
    if parsed.not_after - date::now() < RENEW_BEFORE_DAYS * 86_400 {
        return Err("the stored one expires soon".into());
    }
    Ok(Root {
        issuer: Issuer::new(root_params(&name), key),
        der,
        pem,
        name,
        not_after: parsed.not_after,
        constrained: parsed.name_constraints,
    })
}

/// The root's parameters. Issuing only needs the subject name, key usages and
/// key identifier method from these, so they can be rebuilt from the stored
/// root's common name.
fn root_params(name: &str) -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// Creates and stores a new root. Name constraints limit it to local names
/// and private addresses, so if its key leaks it can't be used to
/// impersonate other sites to devices that trust it (Android ignores them).
fn create_root(paths: &CertKey, constraints: Option<NameConstraints>) -> Result<Root, String> {
    let err = |e: &dyn std::fmt::Display| format!("can't create ferry's local CA: {e}");
    // iOS leaves roots without a common name out of Certificate Trust
    // Settings, so they can't be trusted. Naming the machine tells several
    // ferry roots apart on a phone.
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "ferry".into());
    let host = hostname().unwrap_or_else(|| "localhost".into());
    let name = format!("ferry local CA ({user}@{host})");

    let mut params = root_params(&name);
    let now = date::now();
    set_validity(&mut params, now - 86_400, now + ROOT_VALID_DAYS * 86_400);
    let constrained = constraints.is_some();
    params.name_constraints = constraints;
    let key = KeyPair::generate().map_err(|e| err(&e))?;
    let cert = params.self_signed(&key).map_err(|e| err(&e))?;

    create_dir(paths)?;
    write_file(&paths.key, key.serialize_pem().as_bytes(), 0o600)?;
    write_file(&paths.cert, cert.pem().as_bytes(), 0o644)?;
    Ok(Root {
        der: cert.der().clone(),
        pem: cert.pem(),
        name: name.clone(),
        not_after: now + ROOT_VALID_DAYS * 86_400,
        constrained,
        issuer: Issuer::new(root_params(&name), key),
    })
}

/// Issues a server certificate for `names`, writes it out, and checks it
/// chains to the root the way a client would.
fn issue(
    root: &Root,
    names: &BTreeSet<String>,
    paths: &Paths,
) -> Result<(LoadedCert, i64), String> {
    let err = |e: &dyn std::fmt::Display| format!("can't issue a server certificate: {e}");
    let mut params =
        CertificateParams::new(names.iter().cloned().collect::<Vec<_>>()).map_err(|e| err(&e))?;
    // Clients only look at the subjectAltName, but OpenSSL can check a
    // hostname-like common name against the root's name constraints, so
    // use a name the constraints always permit.
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "localhost");
    params.distinguished_name = dn;
    // Apple requires serverAuth on every TLS server certificate.
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.use_authority_key_identifier_extension = true;
    let now = date::now();
    let not_after = (now + SERVER_VALID_DAYS * 86_400).min(root.not_after);
    set_validity(&mut params, now - 86_400, not_after);
    let key = KeyPair::generate().map_err(|e| err(&e))?;
    let cert = params.signed_by(&key, &root.issuer).map_err(|e| err(&e))?;

    verify(&root.der, cert.der()).map_err(|e| {
        format!(
            "ferry's local CA ({}) can't issue a certificate for this machine: {e}. \
             Delete it and {} to make a new one; devices that trusted the old one will \
             need to trust the new one.",
            paths.root.cert.display(),
            paths.root.key.display()
        )
    })?;

    let chain = format!("{}{}", cert.pem(), root.pem);
    create_dir(&paths.root)?;
    write_file(&paths.server.key, key.serialize_pem().as_bytes(), 0o600)?;
    write_file(&paths.server.cert, chain.as_bytes(), 0o644)?;

    let certs = vec![cert.der().clone(), root.der.clone()];
    let key = PrivateKeyDer::try_from(key.serialize_der()).map_err(|e| err(&e))?;
    let loaded = tls::from_der(
        certs,
        key,
        &paths.server.cert.display().to_string(),
        &paths.server.key.display().to_string(),
    )?;
    Ok((loaded, not_after))
}

/// Verifies `leaf` chains to `root` for `localhost`, as a TLS client would.
/// This catches a root key that doesn't match the root certificate, and a
/// root whose name constraints don't permit this machine's names.
fn verify(root: &CertificateDer<'static>, leaf: &CertificateDer<'_>) -> Result<(), String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root.clone()).map_err(|e| e.to_string())?;
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        tls::provider(),
    )
    .build()
    .map_err(|e| e.to_string())?;
    let name = ServerName::try_from("localhost").expect("localhost is a valid name");
    verifier
        .verify_server_cert(leaf, &[], &name, &[], UnixTime::now())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// What a constrained root may issue for: local names and private addresses.
///
/// The same for every root, so nothing about them goes stale. The hostname
/// is left out on purpose: it's often a fully qualified public name, which
/// would let the root vouch for that domain and everything under it.
fn name_constraints() -> NameConstraints {
    let mut permitted = vec![
        GeneralSubtree::DnsName("localhost".into()),
        GeneralSubtree::DnsName("local".into()),
    ];
    for cidr in PRIVATE_RANGES {
        let (ip, prefix) = cidr.split_once('/').expect("ranges have a prefix");
        let ip: IpAddr = ip.parse().expect("ranges are valid");
        permitted.push(GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(
            ip,
            prefix.parse().expect("ranges are valid"),
        )));
    }
    NameConstraints {
        permitted_subtrees: permitted,
        excluded_subtrees: Vec::new(),
    }
}

/// Loopback, private, link-local and carrier-grade NAT (Tailscale) ranges.
const PRIVATE_RANGES: &[&str] = &[
    "127.0.0.0/8",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "100.64.0.0/10",
    "::1/128",
    "fc00::/7",
    "fe80::/10",
];

/// Whether `name` falls within `constraints`, by RFC 5280's rules: a DNS
/// constraint matches the name itself and any name ending in `.` plus it.
fn permits(constraints: &NameConstraints, name: &str) -> bool {
    let ip = name.parse::<IpAddr>().ok();
    constraints
        .permitted_subtrees
        .iter()
        .any(|tree| match (tree, ip) {
            (GeneralSubtree::DnsName(base), None) => {
                name == base || name.ends_with(&format!(".{base}"))
            }
            (GeneralSubtree::IpAddress(cidr), Some(ip)) => cidr_contains(cidr, ip),
            _ => false,
        })
}

fn cidr_contains(cidr: &CidrSubnet, ip: IpAddr) -> bool {
    let matches = |addr: &[u8], mask: &[u8], ip: &[u8]| {
        addr.iter()
            .zip(mask)
            .zip(ip)
            .all(|((a, m), i)| a & m == i & m)
    };
    match (cidr, ip) {
        (CidrSubnet::V4(addr, mask), IpAddr::V4(ip)) => matches(addr, mask, &ip.octets()),
        (CidrSubnet::V6(addr, mask), IpAddr::V6(ip)) => matches(addr, mask, &ip.octets()),
        _ => false,
    }
}

/// SHA-256 of a certificate, as colon-separated hex bytes.
pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Sets a certificate's validity period, from Unix timestamps.
fn set_validity(params: &mut CertificateParams, not_before: i64, not_after: i64) {
    let at = |secs: i64| rcgen::date_time_ymd(1970, 1, 1) + Duration::from_secs(secs as u64);
    params.not_before = at(not_before);
    params.not_after = at(not_after);
}

fn create_dir(paths: &CertKey) -> Result<(), String> {
    let dir = paths.cert.parent().expect("cert path has a parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("can't create {}: {e}", dir.display()))
}

/// Writes via a temporary file and rename, so a crash never leaves a cert
/// that doesn't match its key. The temporary name includes the process id,
/// so two ferries starting at once don't write over each other's.
fn write_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        std::env::temp_dir().join(format!("ferry-ca-{}", crate::proxy::random_id(12)))
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn creates_a_root_then_reuses_it() {
        let dir = tempdir();
        let paths = paths_in(&dir);
        assert_eq!(load_root(&paths.root).err().unwrap(), "none existed yet");

        let root = create_root(&paths.root, None).unwrap();
        let wanted = names(&["localhost", "127.0.0.1", "::1", "host.local"]);
        let (_, not_after) = issue(&root, &wanted, &paths).unwrap();
        assert!(not_after > date::now() + 300 * 86_400);

        let mode = |p: &Path| {
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(p).unwrap().permissions()) & 0o777
        };
        assert_eq!(mode(&paths.root.key), 0o600);
        assert_eq!(mode(&paths.server.key), 0o600);
        assert_eq!(mode(&dir), 0o700);

        let root_cert = der::parse(&root.der).unwrap();
        assert!(root_cert.self_signed);
        assert!(root.name.starts_with("ferry local CA ("), "{}", root.name);

        // The stored server certificate is the new leaf, then the root.
        let chain: Vec<CertificateDer> = CertificateDer::pem_file_iter(&paths.server.cert)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1], root.der);
        let leaf = der::parse(&chain[0]).unwrap();
        assert_eq!(leaf.names.into_iter().collect::<BTreeSet<_>>(), wanted);
        assert!(!leaf.self_signed);

        // Loading it back gives the same root, which can still issue.
        let again = load_root(&paths.root).unwrap();
        assert_eq!(again.der, root.der);
        assert_eq!(again.name, root.name);
        issue(&again, &wanted, &paths).unwrap();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mismatched_root_key_is_reported() {
        let (a, b) = (tempdir(), tempdir());
        let (pa, pb) = (paths_in(&a), paths_in(&b));
        create_root(&pa.root, None).unwrap();
        create_root(&pb.root, None).unwrap();
        fs::copy(&pb.root.key, &pa.root.key).unwrap();

        let root = load_root(&pa.root).unwrap();
        let e = issue(&root, &names(&["localhost"]), &pa).err().unwrap();
        assert!(
            e.contains("can't issue a certificate for this machine"),
            "{e}"
        );
        assert!(e.contains("Delete it and"), "{e}");
        let _ = fs::remove_dir_all(a);
        let _ = fs::remove_dir_all(b);
    }

    #[test]
    fn a_damaged_root_is_replaced() {
        let dir = tempdir();
        let paths = paths_in(&dir);
        create_root(&paths.root, None).unwrap();
        fs::write(&paths.root.cert, "junk").unwrap();
        let e = load_root(&paths.root).err().unwrap();
        assert!(e.starts_with("the stored one couldn't be loaded"), "{e}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn name_constraints_limit_what_the_root_can_issue() {
        let dir = tempdir();
        let paths = paths_in(&dir);
        let root = create_root(&paths.root, Some(name_constraints())).unwrap();
        let local = names(&[
            "localhost",
            "127.0.0.1",
            "::1",
            "mbp.local",
            "192.168.1.20",
            "100.101.102.103",
        ]);
        issue(&root, &local, &paths).unwrap();
        let e = issue(&root, &names(&["localhost", "example.com"]), &paths)
            .err()
            .unwrap();
        assert!(
            e.contains("can't issue a certificate for this machine"),
            "{e}"
        );
        let e = issue(&root, &names(&["localhost", "mbp"]), &paths)
            .err()
            .unwrap();
        assert!(
            e.contains("can't issue a certificate for this machine"),
            "{e}"
        );
        let e = issue(&root, &names(&["localhost", "8.8.8.8"]), &paths)
            .err()
            .unwrap();
        assert!(
            e.contains("can't issue a certificate for this machine"),
            "{e}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn constrained_roots_leave_out_public_addresses() {
        let dir = tempdir();
        let paths = paths_in(&dir);
        let root = create_root(&paths.root, Some(name_constraints())).unwrap();
        assert!(load_root(&paths.root).unwrap().constrained);
        let wanted = names(&[
            "localhost",
            "mbp",
            "mbp.local",
            "192.168.1.20",
            "203.0.113.7",
        ]);
        let (_, _, warnings) = issue_for(&root, &wanted, &paths).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("doesn't cover 203.0.113.7"),
            "{warnings:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn constrained_roots_leave_out_hostnames_quietly() {
        let dir = tempdir();
        let paths = paths_in(&dir);
        let root = create_root(&paths.root, Some(name_constraints())).unwrap();
        let wanted = names(&["localhost", "mbp", "mbp.local", "build1.example.com"]);
        let (_, _, warnings) = issue_for(&root, &wanted, &paths).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let chain: Vec<CertificateDer> = CertificateDer::pem_file_iter(&paths.server.cert)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let covered: BTreeSet<String> = der::parse(&chain[0]).unwrap().names.into_iter().collect();
        assert_eq!(covered, names(&["localhost", "mbp.local"]));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unconstrained_roots_cover_everything() {
        let dir = tempdir();
        let paths = paths_in(&dir);
        let root = create_root(&paths.root, None).unwrap();
        assert!(!load_root(&paths.root).unwrap().constrained);
        let wanted = names(&["localhost", "example.com", "203.0.113.7"]);
        let (_, _, warnings) = issue_for(&root, &wanted, &paths).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn permitted_names() {
        let c = name_constraints();
        for ok in [
            "localhost",
            "mbp.local",
            "a.b.local",
            "10.1.2.3",
            "172.31.0.1",
            "192.168.0.9",
            "127.0.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "100.64.0.1",
        ] {
            assert!(permits(&c, ok), "{ok}");
        }
        for bad in [
            "example.com",
            "mbp",
            "build1.example.com",
            "notlocal",
            "mbp.example",
            "8.8.8.8",
            "172.32.0.1",
            "2001:db8::1",
            "100.128.0.1",
        ] {
            assert!(!permits(&c, bad), "{bad}");
        }
    }

    #[test]
    fn reissues_when_names_change() {
        let now = date::now();
        let wanted = wanted_names();
        assert_eq!(LocalCa::reissue_reason(&wanted, now + 300 * 86_400), None);
        assert!(
            LocalCa::reissue_reason(&names(&["localhost"]), now + 300 * 86_400)
                .unwrap()
                .contains("addresses changed")
        );
        assert!(
            LocalCa::reissue_reason(&wanted, now + 86_400)
                .unwrap()
                .contains("expires soon")
        );
    }

    #[test]
    fn wanted_names_include_loopback() {
        let names = wanted_names();
        assert!(
            names.contains("localhost") && names.contains("127.0.0.1") && names.contains("::1")
        );
    }

    #[test]
    fn fingerprints_are_colon_separated_sha256() {
        let fp = fingerprint(b"");
        assert!(fp.starts_with("E3:B0:C4:42:98:FC:1C:14"), "{fp}");
        assert_eq!(fp.len(), 32 * 3 - 1);
    }
}
