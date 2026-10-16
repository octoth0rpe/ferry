//! Command-line argument parsing.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, ValueHint};

use crate::config::{CertKey, LogFormat};
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
  ferry ./cert.pem ./key.pem 6005:6006 7001:7002
  ferry -c ferry.toml";

/// A very simple HTTPS reverse proxy.
#[derive(Debug, Parser)]
#[command(
    version,
    about,
    override_usage = "ferry [OPTIONS] [<CERT> <KEY>] <MAPPING>...\n       ferry -c <FILE> [OPTIONS]",
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Optional certificate and key files, followed by one or more mappings
    #[arg(value_name = "ARGS", value_hint = ValueHint::FilePath)]
    pub args: Vec<OsString>,

    /// Load settings from a TOML config file
    #[arg(short, long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub config: Option<PathBuf>,

    /// Check the configuration and exit
    #[arg(short, long)]
    pub test: bool,

    /// Print the effective configuration as a TOML config file and exit
    #[arg(long)]
    pub print_config: bool,

    /// Access log file; `-` for stdout (default), `off` to disable
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub access_log: Option<String>,

    /// Error log file; `-` for stderr (default)
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub error_log: Option<String>,

    /// Access log format [default: combined]
    #[arg(long, value_name = "FORMAT")]
    pub log_format: Option<LogFormat>,

    /// Don't print the startup banner or log requests to stdout
    #[arg(short, long)]
    pub quiet: bool,

    /// Reject request bodies larger than this (e.g. 10M) [default: unlimited]
    #[arg(long, value_name = "SIZE")]
    pub max_body: Option<String>,

    /// Don't redirect plain HTTP requests to HTTPS
    #[arg(long)]
    pub no_redirect: bool,

    /// Run in the background
    #[arg(short, long)]
    pub daemonize: bool,

    /// Write the process id to this file (and remove it on exit)
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub pid_file: Option<PathBuf>,

    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    #[arg(long, value_name = "SHELL", hide = true)]
    pub completions: Option<clap_complete::Shell>,
}

impl Cli {
    /// Writes the completion script for `shell` to stdout.
    pub fn print_completions(shell: clap_complete::Shell) {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        clap_complete::generate(shell, &mut cmd, "ferry", &mut std::io::stdout());
    }
}

/// The positional arguments, sorted into what they mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Positionals {
    /// `None` means no cert/key files were given.
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
        let mut mappings = Vec::new();

        for arg in &self.args {
            match arg.to_str().filter(|s| looks_like_mapping(s)) {
                Some(s) => mappings.push(s.parse::<Mapping>().map_err(|e| e.to_string())?),
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
    fn man_page_documents_every_flag() {
        let man = include_str!("../doc/ferry.1").replace("\\-", "-");
        for arg in Cli::command().get_arguments() {
            if let Some(long) = arg.get_long() {
                assert!(
                    man.contains(&format!("--{long}")),
                    "doc/ferry.1 is missing --{long}"
                );
            }
            if let Some(short) = arg.get_short() {
                assert!(
                    man.contains(&format!("-{short}")),
                    "doc/ferry.1 is missing -{short}"
                );
            }
        }
    }

    #[test]
    fn mappings_only() {
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
    fn bad_mapping_is_reported() {
        let e = positionals(&["6005:99999"]).unwrap_err();
        assert!(e.contains("invalid mapping `6005:99999`"), "{e}");
        let e = positionals(&["6005"]).unwrap_err();
        assert!(e.contains("e.g. 6005:6006"), "{e}");
    }
}
