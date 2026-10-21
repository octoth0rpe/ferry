//! `ferry -t`: check a configuration without serving it.

use std::collections::HashSet;
use std::os::unix::fs::FileTypeExt;

use crate::config::Config;
use crate::mapping::Upstream;
use crate::{ca, date, net, tls};

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
    let now = date::now();

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
                    match tls::expiry(&ck.cert, cert.not_after, now) {
                        Err(e) => report.errors.push(e),
                        Ok(w) => report.warnings.extend(w),
                    }
                }
                Err(e) => report.errors.push(e),
            },
            None => match ca::paths() {
                Ok(paths) if ca::root_problem(&paths).is_some() => {
                    report.errors.extend(ca::root_problem(&paths));
                }
                Ok(paths) if paths.root.cert.exists() => report.notes.push(format!(
                    "no certificate configured; will issue one from ferry's local CA at {}",
                    paths.root.cert.display()
                )),
                Ok(paths) => report.notes.push(format!(
                    "no certificate configured; will create ferry's local CA at {}",
                    paths.root.cert.display()
                )),
                Err(e) => report.errors.push(e),
            },
        }
    }
    let public = ca::public_ips();
    if seen.contains(&None) && !public.is_empty() {
        report.warnings.push(ca::public_warning(&public));
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
        let m = &l.mapping;
        for port in std::iter::once(m.port).chain(m.redirect_from) {
            if let Err(e) = net::bind(m.bind, port) {
                report.warnings.push(net::bind_error(
                    &crate::mapping::listen_display(m.bind, port),
                    port,
                    &e,
                ));
            }
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
