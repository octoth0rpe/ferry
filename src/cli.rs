//! Command-line argument parsing.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

use crate::mapping::Mapping;

const AFTER_HELP: &str = "\
Mappings (LISTEN:UPSTREAM):
  6005:6006               https on port 6005 -> http://localhost:6006
  127.0.0.1:6005:6006     only listen on 127.0.0.1
  6005:10.0.0.5:6006      forward to another host
  6005:/run/app.sock      forward to a Unix socket
  [::1]:6005:6006         IPv6 addresses go in brackets

Without CERT and KEY, ferry uses a self-signed certificate.

Examples:
  ferry 6005:6006
  ferry ./cert.pem ./key.pem 6005:6006 7001:7002";

/// A very simple HTTPS reverse proxy.
#[derive(Debug, Parser)]
#[command(
    version,
    about,
    override_usage = "ferry [OPTIONS] [<CERT> <KEY>] <MAPPING>...",
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Optional certificate and key files, followed by one or more mappings
    #[arg(value_name = "ARGS")]
    pub args: Vec<OsString>,
}

/// A certificate chain and its private key, both PEM files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertKey {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// The positional arguments, sorted into what they mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Positionals {
    /// `None` means use the default self-signed certificate.
    pub cert_key: Option<CertKey>,
    pub mappings: Vec<Mapping>,
}

impl Cli {
    /// Sorts the positional arguments into cert/key files and mappings.
    ///
    /// Anything that looks like a mapping (contains `:` or is a bare number)
    /// is parsed as one; the rest must be exactly zero or two files.
    pub fn positionals(&self) -> Result<Positionals, String> {
        let mut files = Vec::new();
        let mut mappings: Vec<Mapping> = Vec::new();

        for arg in &self.args {
            match arg.to_str().filter(|s| looks_like_mapping(s)) {
                Some(s) => {
                    let mapping: Mapping = s.parse().map_err(|e| format!("{e}"))?;
                    if let Some(other) = mappings.iter().find(|m| m.conflicts_with(&mapping)) {
                        return Err(format!(
                            "mappings `{}` and `{}` both listen on port {}",
                            other.listen_display(),
                            mapping.listen_display(),
                            mapping.port
                        ));
                    }
                    mappings.push(mapping);
                }
                None => files.push(PathBuf::from(arg)),
            }
        }

        let cert_key = match <[PathBuf; 2]>::try_from(files) {
            Ok([cert, key]) => Some(CertKey { cert, key }),
            Err(files) if files.is_empty() => None,
            Err(files) if files.len() == 1 => {
                return Err(format!(
                    "got one file (`{}`); give both CERT and KEY, \
                     or neither to use a self-signed certificate",
                    files[0].display()
                ));
            }
            Err(files) => {
                let list: Vec<_> = files.iter().map(|f| format!("`{}`", f.display())).collect();
                return Err(format!(
                    "expected at most two files (CERT and KEY), got {}: {}",
                    files.len(),
                    list.join(", ")
                ));
            }
        };

        if mappings.is_empty() {
            return Err("nothing to proxy; give at least one mapping, e.g. 6005:6006".into());
        }

        Ok(Positionals { cert_key, mappings })
    }
}

fn looks_like_mapping(s: &str) -> bool {
    s.contains(':') || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn positionals(args: &[&str]) -> Result<Positionals, String> {
        let argv = std::iter::once("ferry").chain(args.iter().copied());
        Cli::try_parse_from(argv).unwrap().positionals()
    }

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn mappings_only_uses_self_signed() {
        let p = positionals(&["6005:6006", "7001:7002"]).unwrap();
        assert_eq!(p.cert_key, None);
        assert_eq!(p.mappings.len(), 2);
        assert_eq!(p.mappings[1].port, 7001);
    }

    #[test]
    fn cert_key_then_mappings() {
        let p = positionals(&["./cert.pem", "./key.pem", "6005:6006"]).unwrap();
        assert_eq!(
            p.cert_key,
            Some(CertKey {
                cert: "./cert.pem".into(),
                key: "./key.pem".into(),
            })
        );
        assert_eq!(p.mappings.len(), 1);
    }

    #[test]
    fn order_does_not_matter() {
        let p = positionals(&["6005:6006", "cert.pem", "key.pem"]).unwrap();
        assert_eq!(p.cert_key.unwrap().cert, PathBuf::from("cert.pem"));
    }

    #[test]
    fn one_file_is_an_error() {
        let e = positionals(&["cert.pem", "6005:6006"]).unwrap_err();
        assert!(e.contains("got one file (`cert.pem`)"), "{e}");
    }

    #[test]
    fn three_files_is_an_error() {
        let e = positionals(&["a", "b", "c", "6005:6006"]).unwrap_err();
        assert!(e.contains("got 3: `a`, `b`, `c`"), "{e}");
    }

    #[test]
    fn no_mappings_is_an_error() {
        assert!(positionals(&[]).unwrap_err().contains("nothing to proxy"));
        assert!(
            positionals(&["c", "k"])
                .unwrap_err()
                .contains("nothing to proxy")
        );
    }

    #[test]
    fn bad_mapping_is_reported() {
        let e = positionals(&["6005:99999"]).unwrap_err();
        assert!(e.contains("invalid mapping `6005:99999`"), "{e}");
        let e = positionals(&["6005"]).unwrap_err();
        assert!(e.contains("e.g. 6005:6006"), "{e}");
    }

    #[test]
    fn duplicate_listen_port_is_an_error() {
        let e = positionals(&["6005:6006", "127.0.0.1:6005:7000"]).unwrap_err();
        assert_eq!(
            e,
            "mappings `6005` and `127.0.0.1:6005` both listen on port 6005"
        );
    }
}
