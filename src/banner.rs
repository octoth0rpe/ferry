//! The startup banner: what ferry is serving, as clickable URLs.

use std::fmt::Write;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::path::Path;

use qrcodegen::{QrCode, QrCodeEcc};

use crate::ca::LocalCa;
use crate::config::{Config, ErrorLog};
use crate::mapping::Upstream;
use crate::server::{Kind, Plan};
use crate::{dirs, net, trust};

/// Builds the banner text. `color` adds bold/dim ANSI styling; `qr` shows QR
/// codes for phones: one for trusting the local CA, one for each app.
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
    for m in config.listeners.iter().map(|l| &l.mapping) {
        if let Some(from) = m.redirect_from {
            let _ = writeln!(
                out,
                "  {dim}http on {} redirects to https on {}{reset}",
                crate::mapping::listen_display(m.bind, from),
                m.port
            );
        }
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
        out.push_str(&trust_section(plan, ca, color));
    }
    out.push_str(&phone_section(config, plan, color, qr));
    out
}

/// The local CA's fingerprint, and where to trust it on this computer.
fn trust_section(plan: &Plan, ca: &LocalCa, color: bool) -> String {
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
    if let Some((port, _)) = trust_listener(plan) {
        let _ = writeln!(
            out,
            "  {dim}NOTE: Browsers will warn until you trust the CA; see {}{reset}",
            trust_page("localhost", port)
        );
    }
    out
}

/// What a phone on the same network needs: the page for trusting
/// the local CA, with the steps in brief, and each app's address. With `qr`,
/// as QR codes side by side.
fn phone_section(config: &Config, plan: &Plan, color: bool, qr: bool) -> String {
    let (dim, reset) = match color {
        true => ("\x1b[2m", "\x1b[0m"),
        false => ("", ""),
    };
    let Some(ip) = net::lan_ips_best_first()
        .into_iter()
        .find(|ip| ip.is_ipv4())
    else {
        return String::new();
    };
    let reachable = |bind: Option<IpAddr>| all_interfaces(bind) || bind == Some(ip);
    let trust = trust_listener(plan)
        .filter(|(_, lan)| *lan)
        .map(|(port, _)| trust_page(&ip.to_string(), port));

    let mut out = String::new();
    if !qr {
        if let Some(page) = &trust {
            let _ = writeln!(out, "  {dim}On a phone, trust the CA at {page}{reset}");
            for line in &PHONE_STEPS[1..] {
                let _ = writeln!(out, "  {dim}  {line}{reset}");
            }
        }
        return out;
    }

    // The CA first, in a row of its own with the steps beside it, since a
    // phone has to trust it before the apps will load.
    if let Some(page) = &trust {
        // The steps start a line down, level with the top of the code.
        let steps: Panel = std::iter::once((String::new(), 0))
            .chain(
                PHONE_STEPS
                    .iter()
                    .map(|line| (format!("{dim}{line}{reset}"), line.chars().count())),
            )
            .collect();
        let qr = panel("On a phone, scan this first:", page, color);
        for line in rows(&[qr, steps]).concat() {
            let _ = writeln!(out, "  {line}");
        }
    }
    let mut ports: Vec<u16> = config
        .listeners
        .iter()
        .filter(|l| reachable(l.mapping.bind))
        .map(|l| l.mapping.port)
        .collect();
    ports.dedup();
    let apps: Vec<Panel> = ports
        .into_iter()
        .map(|port| {
            let url = match port {
                443 => format!("https://{ip}"),
                p => format!("https://{ip}:{p}"),
            };
            app_panel(&url, color)
        })
        .collect();
    if trust.is_some() && !apps.is_empty() {
        let _ = writeln!(out, "  {dim}{}{reset}", "─".repeat(76));
    }
    for line in rows(&apps).concat() {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// An app's QR code with its URL centred above it.
fn app_panel(url: &str, color: bool) -> Panel {
    let code = qr_code(url, color);
    let url_width = url.chars().count();
    let width = code
        .iter()
        .map(|(_, w)| *w)
        .max()
        .unwrap_or(0)
        .max(url_width);
    let centred = |(text, w): (String, usize)| {
        let pad = (width - w) / 2;
        (format!("{}{text}", " ".repeat(pad)), w + pad)
    };
    std::iter::once((url.to_string(), url_width))
        .chain(code)
        .map(centred)
        .collect()
}

/// One labelled QR code: its lines, each with its width on screen.
type Panel = Vec<(String, usize)>;

fn panel(label: &str, url: &str, color: bool) -> Panel {
    let mut lines = vec![(label.to_string(), label.chars().count())];
    lines.extend(qr_code(url, color));
    lines.push((url.to_string(), url.chars().count()));
    lines
}

/// Lays panels out two to a row where they fit in 80 columns, one otherwise.
fn rows(panels: &[Panel]) -> Vec<Vec<String>> {
    const GAP: usize = 4;
    const WIDTH: usize = 78;
    let width = |p: &Panel| p.iter().map(|(_, w)| *w).max().unwrap_or(0);
    let mut rows = Vec::new();
    let mut i = 0;
    while i < panels.len() {
        let pair = panels
            .get(i + 1)
            .filter(|next| width(&panels[i]) + GAP + width(next) <= WIDTH);
        let row = match pair {
            Some(next) => {
                let w = width(&panels[i]);
                let height = panels[i].len().max(next.len());
                (0..height)
                    .map(|n| {
                        let (left, lw) = panels[i].get(n).cloned().unwrap_or_default();
                        let right = next.get(n).map(|(s, _)| s.as_str()).unwrap_or("");
                        format!("{left}{}{right}", " ".repeat(w - lw + GAP))
                            .trim_end()
                            .to_string()
                    })
                    .collect()
            }
            None => panels[i].iter().map(|(s, _)| s.clone()).collect(),
        };
        rows.push(row);
        i += if pair.is_some() { 2 } else { 1 };
    }
    rows
}

fn all_interfaces(bind: Option<IpAddr>) -> bool {
    bind.is_none_or(|ip| ip.is_unspecified())
}

/// Where the page for trusting the local CA is, if it's being served: its
/// port, and whether other devices can reach it.
fn trust_listener(plan: &Plan) -> Option<(u16, bool)> {
    let spec = plan.listeners.iter().find(|s| s.kind == Kind::Trust)?;
    Some((plan.trust_port?, all_interfaces(spec.key.0)))
}

fn trust_page(host: &str, port: u16) -> String {
    format!("http://{host}:{port}/")
}

/// The trust page's steps in brief, for people looking at the terminal
/// rather than the phone. Narrow, to fit beside the QR code.
const PHONE_STEPS: [&str; 13] = [
    "Trust ferry's CA, once per phone:",
    "iPhone",
    " • Open the page in Safari and",
    "   download the certificate.",
    " • Settings → General → VPN & Device",
    "   Management → ferry local CA →",
    "   Install (within 8 minutes).",
    " • General → About → Certificate",
    "   Trust Settings → ferry local CA.",
    "Android",
    " • Download the certificate.",
    " • Settings → search \"CA certificate\"",
    "   → Install anyway → ferry-ca.crt.",
];

/// A QR code drawn with half blocks, two rows of modules to a line. With
/// `color`, it's black on white whatever the terminal's colours; without, it
/// assumes light text on a dark background and draws the light modules.
fn qr_code(text: &str, color: bool) -> Panel {
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
            let width = row.chars().count();
            match color {
                true => (format!("\x1b[30;107m{row}\x1b[0m"), width),
                false => (row, width),
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
    if let Some(banner) = text(config, plan, daemonize) {
        eprint!("{banner}");
    }
    log_listeners(config);
}

/// The banner as [`show`] would print it, or `None` with `-q`.
pub fn text(config: &Config, plan: &Plan, daemonize: bool) -> Option<String> {
    if config.quiet {
        return None;
    }
    let tty = std::io::stderr().is_terminal();
    let color = tty && std::env::var_os("NO_COLOR").is_none();
    let qr = tty && !daemonize && config.advanced.qr_code;
    Some(render(config, plan, color, qr))
}

/// Logs the listeners to the error log when the banner doesn't reach it.
pub fn log_listeners(config: &Config) {
    let stderr_is_log = config.error_log == ErrorLog::Stderr;
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
    fn panels_go_two_to_a_row_when_they_fit() {
        let p = |label: &str, url: &str| panel(label, url, false);
        let short = p("a", "https://192.168.1.20:6005/");
        let width = short.iter().map(|(_, w)| *w).max().unwrap();
        let rows_of = |panels: &[Panel]| rows(panels).iter().map(Vec::len).collect::<Vec<_>>();

        // Three panels: a pair, then one on its own.
        assert_eq!(
            rows_of(&[short.clone(), short.clone(), short.clone()]),
            vec![short.len(), short.len()]
        );
        let pair = &rows(&[short.clone(), short.clone()])[0];
        assert!(pair[1].starts_with(&short[1].0), "{pair:?}");
        assert_eq!(
            pair.last().unwrap(),
            &format!(
                "{}{}{}",
                short.last().unwrap().0,
                " ".repeat(width - short.last().unwrap().1 + 4),
                short.last().unwrap().0
            )
        );

        // Too wide to pair: stacked.
        let wide = p(&"x".repeat(60), "https://192.168.1.20:6005/");
        assert_eq!(
            rows_of(&[wide.clone(), short.clone()]),
            vec![wide.len(), short.len()]
        );
    }

    #[test]
    fn app_panels_centre_the_url_over_the_code() {
        let url = "https://192.168.1.20:6005";
        let panel = app_panel(url, false);
        let qr_width = panel[1].1;
        assert!(qr_width > url.len());
        let pad = (qr_width - url.len()) / 2;
        assert_eq!(panel[0].0, format!("{}{url}", " ".repeat(pad)));
        assert_eq!(panel[0].1, url.len() + pad);
        assert!(panel[1..].iter().all(|(_, w)| *w == qr_width));
    }

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
