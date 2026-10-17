//! The startup banner: what ferry is serving, as clickable URLs.

use std::fmt::Write;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::path::Path;

use qrcodegen::{QrCode, QrCodeEcc};

use crate::ca::LocalCa;
use crate::config::{Config, ErrorLog};
use crate::mapping::Upstream;
use crate::server::Plan;
use crate::{dirs, net, trust};

/// Builds the banner text. `color` adds bold/dim ANSI styling; `qr` shows a
/// QR code for trusting the local CA on a phone.
pub fn render(config: &Config, plan: &Plan, color: bool, qr: bool) -> String {
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
            None => match &plan.local_ca {
                Some(ca) => format!("ferry local CA ({})", tilde(&ca.paths.root.cert)),
                None => "ferry local CA".into(),
            },
        })
        .collect();
    certs.dedup();
    for cert in certs {
        let _ = writeln!(out, "  {dim}cert: {cert}{reset}");
    }
    if let Some(ca) = &plan.local_ca {
        out.push_str(&trust_section(config, ca, color, qr));
    }
    out
}

/// How to trust the local CA: its fingerprint, the `/.ferry/` page for this
/// computer, and a QR code for phones.
fn trust_section(config: &Config, ca: &LocalCa, color: bool, qr: bool) -> String {
    let (bold, dim, reset) = match color {
        true => ("\x1b[1m", "\x1b[2m", "\x1b[0m"),
        false => ("", "", ""),
    };
    let mut out = String::new();
    if ca.created.is_some() && ca.paths.old_self_signed.exists() {
        let _ = writeln!(
            out,
            "  {bold}ferry now uses a local CA instead of a self-signed certificate. Trust it \
             once, below;{reset}\n  {bold}{} is no longer used.{reset}",
            tilde(&ca.paths.old_self_signed)
        );
    }
    let [fp1, fp2] = trust::fingerprint_lines(&ca.fingerprint);
    let _ = writeln!(out, "  {dim}CA fingerprint (SHA-256):{reset}");
    let _ = writeln!(out, "  {dim}  {fp1}{reset}");
    let _ = writeln!(out, "  {dim}  {fp2}{reset}");

    // Plain HTTP on a port that uses the CA serves the page; pick ports that
    // accept loopback and LAN connections respectively.
    let ca_ports = config
        .listeners
        .iter()
        .filter(|l| l.cert_key(config).is_none())
        .map(|l| (l.mapping.bind, l.mapping.port));
    let page = |host: &str, port: u16| {
        let port = match port {
            80 => String::new(),
            p => format!(":{p}"),
        };
        format!("http://{host}{port}{}", trust::PATH)
    };
    let all = |bind: Option<IpAddr>| bind.is_none_or(|ip| ip.is_unspecified());
    if let Some((_, port)) = ca_ports
        .clone()
        .find(|(bind, _)| all(*bind) || bind.is_some_and(|ip| ip.is_loopback()))
    {
        let _ = writeln!(
            out,
            "  {dim}browsers warn until you trust the CA; see {}{reset}",
            page("localhost", port)
        );
    }

    let lan_ips = net::lan_ips_best_first();
    let lan_pages: Vec<String> = lan_ips
        .iter()
        .filter(|ip| ip.is_ipv4())
        .filter_map(|ip| {
            let (_, port) = ca_ports
                .clone()
                .find(|(bind, _)| all(*bind) || *bind == Some(*ip))?;
            Some(page(&ip.to_string(), port))
        })
        .collect();
    if let Some((first, rest)) = lan_pages.split_first() {
        if qr {
            let _ = writeln!(
                out,
                "  on a phone, scan this to trust the CA (on an iPhone, open it in Safari):"
            );
            for line in qr_code(first, color) {
                let _ = writeln!(out, "  {line}");
            }
            let _ = writeln!(out, "  {first}");
        } else {
            let _ = writeln!(out, "  {dim}on a phone: {first}{reset}");
        }
        for page in rest {
            let _ = writeln!(out, "  {dim}  or {page}{reset}");
        }
    }
    out
}

/// A QR code drawn with half blocks, two rows of modules to a line. With
/// `color`, it's black on white whatever the terminal's colours; without, it
/// assumes light text on a dark background and draws the light modules.
fn qr_code(text: &str, color: bool) -> Vec<String> {
    let Ok(code) = QrCode::encode_text(text, QrCodeEcc::Low) else {
        return Vec::new();
    };
    // Scanners need a light border; two modules is enough in practice.
    let border = 2;
    let (start, end) = (-border, code.size() + border);
    let ink = |x, y| code.get_module(x, y) == color;
    (start..end)
        .step_by(2)
        .map(|y| {
            let row: String = (start..end)
                .map(|x| match (ink(x, y), y + 1 < end && ink(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            match color {
                true => format!("\x1b[30;107m{row}\x1b[0m"),
                false => row,
            }
        })
        .collect()
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
/// error log when it isn't the terminal (so log files record them too). The
/// QR code is only for people watching a terminal, not for background mode.
pub fn show(config: &Config, plan: &Plan, daemonize: bool) {
    let stderr_is_log = config.error_log == ErrorLog::Stderr;
    if !config.quiet {
        let tty = std::io::stderr().is_terminal();
        let color = tty && std::env::var_os("NO_COLOR").is_none();
        let qr = tty && !daemonize && config.advanced.qr_code;
        eprint!("{}", render(config, plan, color, qr));
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
