//! The startup banner: what ferry is serving, as clickable URLs.

use std::fmt::Write;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::path::Path;

use crate::config::{Config, ErrorLog};
use crate::mapping::Upstream;
use crate::net;
use crate::server::Plan;

/// Builds the banner text. `color` adds bold/dim ANSI styling.
pub fn render(config: &Config, plan: &Plan, color: bool) -> String {
    let (bold, dim, reset) = match color {
        true => ("\x1b[1m", "\x1b[2m", "\x1b[0m"),
        false => ("", "", ""),
    };
    let lan_ips = net::local_ips();
    let mut out = format!("{bold}ferry {}{reset}\n", env!("CARGO_PKG_VERSION"));

    for l in &config.listeners {
        let m = &l.mapping;
        let upstream = match &m.upstream {
            Upstream::Unix(path) => tilde(path),
            tcp => tcp.to_string(),
        };
        let urls = urls(m.bind, m.port, &lan_ips);
        let _ = writeln!(out, "  {bold}{}{reset}  →  {upstream}", urls[0]);
        for url in &urls[1..] {
            let _ = writeln!(out, "  {dim}{url}{reset}");
        }
    }
    for spec in plan.listeners.iter().filter(|s| s.optional) {
        let _ = writeln!(
            out,
            "  {dim}http://*:{} redirects to https{reset}",
            spec.key.1
        );
    }

    let mut certs: Vec<String> = config
        .listeners
        .iter()
        .map(|l| match l.cert_key(config) {
            Some(ck) => tilde(&ck.cert),
            None => match &plan.self_signed {
                Some(ss) => format!("self-signed ({})", tilde(&ss.paths.cert)),
                None => "self-signed".into(),
            },
        })
        .collect();
    certs.dedup();
    for cert in certs {
        let _ = writeln!(out, "  {dim}cert: {cert}{reset}");
    }
    if plan.self_signed.is_some() {
        let _ = writeln!(
            out,
            "  {dim}browsers will warn about the self-signed cert until you trust it{reset}"
        );
    }
    out
}

/// The URLs a listener can be reached at, best first.
fn urls(bind: Option<IpAddr>, port: u16, lan_ips: &[IpAddr]) -> Vec<String> {
    let port_part = match port {
        443 => String::new(),
        p => format!(":{p}"),
    };
    let host = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    };
    match bind {
        None => std::iter::once("localhost".to_string())
            .chain(lan_ips.iter().map(|ip| host(*ip)))
            .map(|h| format!("https://{h}{port_part}"))
            .collect(),
        Some(ip) if ip.is_unspecified() => urls(None, port, lan_ips),
        Some(ip) => vec![format!("https://{}{port_part}", host(ip))],
    }
}

/// Shows paths under the home directory as `~/...`.
fn tilde(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Prints the banner to stderr unless `-q`, and logs the listeners to the
/// error log when it isn't the terminal (so log files record them too).
pub fn show(config: &Config, plan: &Plan) {
    let stderr_is_log = config.error_log == ErrorLog::Stderr;
    if !config.quiet {
        let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        eprint!("{}", render(config, plan, color));
    }
    if config.quiet || !stderr_is_log {
        for l in &config.listeners {
            tracing::info!(
                "listening on {} -> {}",
                l.mapping.listen_display(),
                l.mapping.upstream
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_for_each_kind_of_bind() {
        let lan: Vec<IpAddr> = vec!["192.168.1.20".parse().unwrap()];
        assert_eq!(
            urls(None, 6005, &lan),
            vec!["https://localhost:6005", "https://192.168.1.20:6005"]
        );
        assert_eq!(urls(None, 443, &[]), vec!["https://localhost"]);
        assert_eq!(
            urls(Some("127.0.0.1".parse().unwrap()), 6005, &lan),
            vec!["https://127.0.0.1:6005"]
        );
        assert_eq!(
            urls(Some("::1".parse().unwrap()), 6005, &lan),
            vec!["https://[::1]:6005"]
        );
        assert_eq!(
            urls(Some("::".parse().unwrap()), 6005, &[]),
            vec!["https://localhost:6005"]
        );
    }
}
