//! `ferry -t`: check a configuration without serving it.

use std::collections::HashSet;
use std::os::unix::fs::FileTypeExt;

use crate::config::Config;
use crate::mapping::Upstream;
use crate::{net, selfsigned, tls};

/// Warn about certificates expiring sooner than this.
const EXPIRY_WARNING_DAYS: i64 = 14;

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

/// Checks everything that can be checked without side effects: certificates
/// load and match their keys and aren't expired, upstream sockets exist, and
/// listen ports are free.
pub fn check(config: &Config) -> Report {
    let mut report = Report::default();
    let now = jiff::Timestamp::now().as_second();

    let mut seen = HashSet::new();
    for l in &config.listeners {
        let ck = l.cert_key(config);
        if !seen.insert(ck) {
            continue;
        }
        match ck {
            Some(ck) => match tls::load(ck) {
                Ok(cert) => {
                    report.warnings.extend(cert.warnings);
                    let date = jiff::Timestamp::from_second(cert.not_after)
                        .map(|t| t.strftime("%Y-%m-%d").to_string())
                        .unwrap_or_default();
                    let days = (cert.not_after - now) / 86_400;
                    if cert.not_after <= now {
                        report
                            .errors
                            .push(format!("{} expired on {date}", ck.cert.display()));
                    } else if days < EXPIRY_WARNING_DAYS {
                        report.warnings.push(format!(
                            "{} expires in {days} day(s), on {date}",
                            ck.cert.display()
                        ));
                    }
                }
                Err(e) => report.errors.push(e),
            },
            None => match selfsigned::paths() {
                Ok(paths) if paths.cert.exists() => report.notes.push(format!(
                    "no certificate configured; will use the self-signed one at {}",
                    paths.cert.display()
                )),
                Ok(paths) => report.notes.push(format!(
                    "no certificate configured; will generate a self-signed one at {}",
                    paths.cert.display()
                )),
                Err(e) => report.errors.push(e),
            },
        }
    }

    for l in &config.listeners {
        if let Upstream::Unix(path) = &l.mapping.upstream {
            match std::fs::metadata(path) {
                Ok(meta) if meta.file_type().is_socket() => {}
                Ok(_) => report
                    .warnings
                    .push(format!("upstream {} isn't a socket", path.display())),
                Err(_) => report.warnings.push(format!(
                    "nothing at {} yet; requests will fail until the app creates it",
                    path.display()
                )),
            }
        }
        if let Err(e) = net::bind(l.mapping.bind, l.mapping.port) {
            report.warnings.push(net::bind_error(
                &l.mapping.listen_display(),
                l.mapping.port,
                &e,
            ));
        }
    }

    report
}

impl Report {
    /// Prints the report: problems to stderr, the verdict to stdout.
    pub fn print(&self, config: &Config) -> bool {
        for e in &self.errors {
            eprintln!("error: {e}");
        }
        for w in &self.warnings {
            eprintln!("warning: {w}");
        }
        for n in &self.notes {
            eprintln!("note: {n}");
        }
        let what = match &config.config_path {
            Some(path) => path.display().to_string(),
            None => "configuration".into(),
        };
        let n = config.listeners.len();
        let listeners = if n == 1 { "listener" } else { "listeners" };
        if self.errors.is_empty() {
            println!("{what}: ok ({n} {listeners})");
            true
        } else {
            println!("{what}: {} error(s)", self.errors.len());
            false
        }
    }
}
