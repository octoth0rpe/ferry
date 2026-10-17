//! Port mappings: which port to listen on and where to forward requests.
//!
//! Accepted forms (`LISTEN:UPSTREAM`):
//!
//! | Input                     | Listens on       | Forwards to                |
//! |---------------------------|------------------|----------------------------|
//! | `6005:6006`               | all interfaces   | `http://localhost:6006`    |
//! | `127.0.0.1:6005:6006`     | `127.0.0.1:6005` | `http://localhost:6006`    |
//! | `6005:10.0.0.5:6006`      | all interfaces   | `http://10.0.0.5:6006`     |
//! | `[::1]:6005:[::1]:6006`   | `[::1]:6005`     | `http://[::1]:6006`        |
//! | `6005:http://app:8080`    | all interfaces   | `http://app:8080`          |
//! | `6005:/run/app.sock`      | all interfaces   | Unix socket `/run/app.sock`|
//! | `80/443:6006`             | 443, and 80 redirecting to it | `http://localhost:6006` |
//!
//! IPv6 addresses must be written in brackets. `HTTP/HTTPS` on the listen
//! side also opens a plain HTTP port that only redirects to the HTTPS one.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::str::FromStr;

/// Host used when an upstream is given as a bare port.
pub const DEFAULT_UPSTREAM_HOST: &str = "localhost";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// Address to bind; `None` means all interfaces (IPv4 and IPv6).
    pub bind: Option<IpAddr>,
    pub port: u16,
    /// A plain HTTP port that redirects to `port` over HTTPS (`80/443`).
    pub redirect_from: Option<u16>,
    pub upstream: Upstream,
}

/// The listen side of a mapping: `PORT`, `ADDR:PORT`, or either with
/// `HTTP/HTTPS` ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listen {
    pub bind: Option<IpAddr>,
    pub port: u16,
    pub redirect_from: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// Plain HTTP over TCP. `host` is a hostname or an IP without brackets.
    Tcp { host: String, port: u16 },
    /// Plain HTTP over a Unix domain socket.
    Unix(PathBuf),
}

impl Mapping {
    /// A port both mappings would listen on with overlapping addresses, if
    /// there is one.
    pub fn conflict(&self, other: &Mapping) -> Option<u16> {
        let overlap = self.bind.is_none() || other.bind.is_none() || self.bind == other.bind;
        self.ports()
            .find(|port| overlap && other.ports().any(|p| p == *port))
    }

    /// Every port the mapping listens on.
    fn ports(&self) -> impl Iterator<Item = u16> {
        std::iter::once(self.port).chain(self.redirect_from)
    }

    /// The listen side as written by a user, e.g. `6005`, `127.0.0.1:6005`
    /// or `80/443`.
    pub fn listen_display(&self) -> String {
        match self.redirect_from {
            None => listen_display(self.bind, self.port),
            Some(from) => listen_display(self.bind, from) + &format!("/{}", self.port),
        }
    }
}

/// A listen address as written by a user, e.g. `6005` or `127.0.0.1:6005`.
pub fn listen_display(bind: Option<IpAddr>, port: u16) -> String {
    match bind {
        None => port.to_string(),
        Some(IpAddr::V4(ip)) => format!("{ip}:{port}"),
        Some(IpAddr::V6(ip)) => format!("[{ip}]:{port}"),
    }
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Upstream::Tcp { host, port } if host.contains(':') => {
                write!(f, "http://[{host}]:{port}")
            }
            Upstream::Tcp { host, port } => write!(f, "http://{host}:{port}"),
            Upstream::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingError {
    pub input: String,
    pub reason: String,
}

impl fmt::Display for MappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid mapping `{}`: {}", self.input, self.reason)
    }
}

impl std::error::Error for MappingError {}

impl FromStr for Mapping {
    type Err = MappingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse(s).map_err(|reason| MappingError {
            input: s.to_string(),
            reason,
        })
    }
}

fn parse(s: &str) -> Result<Mapping, String> {
    let (listen, upstream) = split_mapping(s)?;
    let Listen {
        bind,
        port,
        redirect_from,
    } = parse_listen(listen)?;
    Ok(Mapping {
        bind,
        port,
        redirect_from,
        upstream: parse_upstream(upstream)?,
    })
}

/// Splits `LISTEN:UPSTREAM` into its two halves.
fn split_mapping(s: &str) -> Result<(&str, &str), String> {
    if let Some(i) = s.find("://") {
        let colon = s[..i]
            .rfind(':')
            .ok_or("expected a listen port before the upstream URL, e.g. 6005:http://app:8080")?;
        return Ok((&s[..colon], &s[colon + 1..]));
    }
    if let Some(i) = find_unix_path(s) {
        return Ok((&s[..i], &s[i + 1..]));
    }
    let parts = split_colons(s)?;
    // The only ambiguous case is three parts: `port:host:port` vs
    // `bind:port:port`.
    let n_listen = match parts.len() {
        1 => {
            return Err(format!(
                "expected LISTEN:UPSTREAM, e.g. {s}:{}",
                next_port(s)
            ));
        }
        2 => 1,
        3 if is_ports(parts[0]) => 1,
        3 | 4 => 2,
        _ => return Err(too_many_parts()),
    };
    let listen_len = parts[..n_listen].iter().map(|p| p.len() + 1).sum::<usize>() - 1;
    Ok((&s[..listen_len], &s[listen_len + 1..]))
}

/// Parses the listen half of a mapping: `PORT` or `ADDR:PORT`, where
/// `PORT` can be `HTTP/HTTPS`.
pub fn parse_listen(s: &str) -> Result<Listen, String> {
    let (bind, ports) = match split_colons(s)?.as_slice() {
        [ports] => (None, *ports),
        [bind, ports] => (Some(parse_bind(bind)?), *ports),
        _ => return Err(too_many_parts()),
    };
    let (redirect_from, port) = match ports.split_once('/') {
        None => (None, parse_port(ports)?),
        Some((from, to)) => {
            let (from, to) = (parse_port(from)?, parse_port(to)?);
            if from == to {
                return Err(format!(
                    "the plain HTTP and HTTPS ports must differ, e.g. 80/443 (got {from}/{to})"
                ));
            }
            (Some(from), to)
        }
    };
    Ok(Listen {
        bind,
        port,
        redirect_from,
    })
}

/// Parses the upstream half of a mapping: `PORT`, `HOST:PORT`,
/// `http://HOST[:PORT]`, or a Unix socket path.
pub fn parse_upstream(s: &str) -> Result<Upstream, String> {
    if let Some((scheme, rest)) = s.split_once("://") {
        return match scheme {
            "http" => parse_http_upstream(rest),
            "https" => Err("https upstreams aren't supported yet".into()),
            other => Err(format!("unsupported upstream scheme `{other}`")),
        };
    }
    if is_unix_path(s) {
        return Ok(Upstream::Unix(PathBuf::from(s)));
    }
    match split_colons(s)?.as_slice() {
        [port] if is_digits(port) => Ok(Upstream::Tcp {
            host: DEFAULT_UPSTREAM_HOST.into(),
            port: parse_port(port)?,
        }),
        [host] => Err(format!("upstream `{host}` needs a port, e.g. {host}:8080")),
        [host, port] => Ok(Upstream::Tcp {
            host: parse_host(host)?,
            port: parse_port(port)?,
        }),
        _ => Err(too_many_parts()),
    }
}

fn is_unix_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("./") || s.starts_with("../")
}

/// Index of the `:` that starts a Unix socket path (`:/`, `:./`, `:../`).
fn find_unix_path(s: &str) -> Option<usize> {
    [":/", ":./", ":../"]
        .iter()
        .filter_map(|pat| s.find(pat))
        .min()
}

/// Parses the part of an `http://` URL after the scheme: `host[:port][/]`.
fn parse_http_upstream(rest: &str) -> Result<Upstream, String> {
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains('/') {
        return Err("upstream URLs can't include a path".into());
    }
    match split_colons(authority)?.as_slice() {
        [host] => Ok(Upstream::Tcp {
            host: parse_host(host)?,
            port: 80,
        }),
        [host, port] => Ok(Upstream::Tcp {
            host: parse_host(host)?,
            port: parse_port(port)?,
        }),
        _ => Err(too_many_parts()),
    }
}

/// Splits on `:`, treating anything inside `[...]` as a single part.
fn split_colons(s: &str) -> Result<Vec<&str>, String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_brackets = false;
    for (i, c) in s.char_indices() {
        match c {
            '[' if !in_brackets => in_brackets = true,
            ']' if in_brackets => in_brackets = false,
            ':' if !in_brackets => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if in_brackets {
        return Err("unclosed `[`".into());
    }
    parts.push(&s[start..]);
    if parts.iter().any(|p| p.is_empty()) {
        if parts.len() > 2 && s.contains("::") {
            return Err(too_many_parts());
        }
        return Err("empty part between `:` separators".into());
    }
    Ok(parts)
}

fn parse_port(s: &str) -> Result<u16, String> {
    if !is_digits(s) {
        return Err(format!("`{s}` is not a port number"));
    }
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("port `{s}` is out of range (1-65535)")),
        Ok(port) => Ok(port),
    }
}

fn parse_bind(s: &str) -> Result<IpAddr, String> {
    if let Some(inner) = bracketed(s) {
        return inner
            .parse::<Ipv6Addr>()
            .map(IpAddr::V6)
            .map_err(|_| format!("`{s}` is not a valid IPv6 address"));
    }
    s.parse::<Ipv4Addr>()
        .map(IpAddr::V4)
        .map_err(|_| format!("listen address `{s}` must be an IP address"))
}

fn parse_host(s: &str) -> Result<String, String> {
    if let Some(inner) = bracketed(s) {
        return inner
            .parse::<Ipv6Addr>()
            .map(|ip| ip.to_string())
            .map_err(|_| format!("`{s}` is not a valid IPv6 address"));
    }
    let valid = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if !valid {
        return Err(format!("`{s}` is not a valid hostname or IP address"));
    }
    Ok(s.to_ascii_lowercase())
}

fn bracketed(s: &str) -> Option<&str> {
    s.strip_prefix('[')?.strip_suffix(']')
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `PORT` or `HTTP/HTTPS`.
fn is_ports(s: &str) -> bool {
    match s.split_once('/') {
        Some((a, b)) => is_digits(a) && is_digits(b),
        None => is_digits(s),
    }
}

fn too_many_parts() -> String {
    "too many `:`-separated parts (IPv6 addresses need brackets, e.g. [::1])".into()
}

/// A plausible upstream port to suggest in error messages.
fn next_port(s: &str) -> String {
    match s.parse::<u16>() {
        Ok(p) if p < u16::MAX => (p + 1).to_string(),
        _ => "6006".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(host: &str, port: u16) -> Upstream {
        Upstream::Tcp {
            host: host.into(),
            port,
        }
    }

    fn ok(s: &str) -> Mapping {
        s.parse().unwrap_or_else(|e| panic!("{e}"))
    }

    fn err(s: &str) -> String {
        s.parse::<Mapping>().unwrap_err().reason
    }

    #[test]
    fn port_to_port() {
        let m = ok("6005:6006");
        assert_eq!(m.bind, None);
        assert_eq!(m.port, 6005);
        assert_eq!(m.upstream, tcp("localhost", 6006));
    }

    #[test]
    fn bind_port_port() {
        let m = ok("127.0.0.1:6005:6006");
        assert_eq!(m.bind, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(m.port, 6005);
        assert_eq!(m.upstream, tcp("localhost", 6006));
    }

    #[test]
    fn port_host_port() {
        let m = ok("6005:10.0.0.5:6006");
        assert_eq!(m.bind, None);
        assert_eq!(m.upstream, tcp("10.0.0.5", 6006));

        let m = ok("6005:App.Internal:6006");
        assert_eq!(m.upstream, tcp("app.internal", 6006));
    }

    #[test]
    fn bind_port_host_port() {
        let m = ok("10.0.0.1:443:backend:8080");
        assert_eq!(m.bind, Some("10.0.0.1".parse().unwrap()));
        assert_eq!(m.port, 443);
        assert_eq!(m.upstream, tcp("backend", 8080));
    }

    #[test]
    fn ipv6_in_brackets() {
        let m = ok("[::1]:6005:6006");
        assert_eq!(m.bind, Some("::1".parse().unwrap()));
        assert_eq!(m.upstream, tcp("localhost", 6006));

        let m = ok("6005:[fe80::1]:6006");
        assert_eq!(m.upstream, tcp("fe80::1", 6006));
        assert_eq!(m.upstream.to_string(), "http://[fe80::1]:6006");

        let m = ok("[::]:6005:[::1]:6006");
        assert_eq!(m.bind, Some("::".parse().unwrap()));
        assert_eq!(m.upstream, tcp("::1", 6006));
    }

    #[test]
    fn http_url_upstream() {
        assert_eq!(ok("6005:http://app:8080").upstream, tcp("app", 8080));
        assert_eq!(ok("6005:http://app:8080/").upstream, tcp("app", 8080));
        assert_eq!(ok("6005:http://app").upstream, tcp("app", 80));
        assert_eq!(ok("6005:http://[::1]:81").upstream, tcp("::1", 81));

        let m = ok("127.0.0.1:6005:http://app:8080");
        assert_eq!(m.bind, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(m.port, 6005);
    }

    #[test]
    fn unix_socket_upstream() {
        let m = ok("6005:/run/app.sock");
        assert_eq!(m.port, 6005);
        assert_eq!(m.upstream, Upstream::Unix("/run/app.sock".into()));

        assert_eq!(
            ok("6005:./app.sock").upstream,
            Upstream::Unix("./app.sock".into())
        );
        assert_eq!(
            ok("6005:../tmp/app.sock").upstream,
            Upstream::Unix("../tmp/app.sock".into())
        );

        let m = ok("[::1]:6005:/tmp/a:b.sock");
        assert_eq!(m.bind, Some("::1".parse().unwrap()));
        assert_eq!(m.upstream, Upstream::Unix("/tmp/a:b.sock".into()));
    }

    #[test]
    fn errors() {
        assert!(err("6005").contains("expected LISTEN:UPSTREAM, e.g. 6005:6006"));
        assert!(err("6005:").contains("empty part"));
        assert!(err(":6006").contains("empty part"));
        assert!(err("6005:99999").contains("out of range"));
        assert!(err("0:6006").contains("out of range"));
        assert!(err("abc:6006").contains("not a port number"));
        assert!(err("localhost:6005:6006").contains("must be an IP address"));
        assert!(err("::1:6005:6006").contains("need brackets"));
        assert!(err("[::1:6005:6006").contains("unclosed"));
        assert!(err("[nope]:6005:6006").contains("not a valid IPv6"));
        assert!(err("6005:bad_host:6006").contains("not a valid hostname"));
        assert!(err("6005:https://app:8443").contains("aren't supported yet"));
        assert!(err("6005:ftp://app").contains("unsupported upstream scheme"));
        assert!(err("6005:http://app/path").contains("can't include a path"));
        assert!(err("1:2:3:4:5").contains("too many"));
    }

    #[test]
    fn halves() {
        let listen = |bind: Option<&str>, port, redirect_from| Listen {
            bind: bind.map(|b| b.parse().unwrap()),
            port,
            redirect_from,
        };
        assert_eq!(parse_listen("6005").unwrap(), listen(None, 6005, None));
        assert_eq!(
            parse_listen("[::1]:6005").unwrap(),
            listen(Some("::1"), 6005, None)
        );
        assert_eq!(parse_listen("80/443").unwrap(), listen(None, 443, Some(80)));
        assert_eq!(
            parse_listen("127.0.0.1:8080/8443").unwrap(),
            listen(Some("127.0.0.1"), 8443, Some(8080))
        );
        assert!(parse_listen("443/443").unwrap_err().contains("must differ"));
        assert!(
            parse_listen("80/")
                .unwrap_err()
                .contains("not a port number")
        );
        assert_eq!(parse_upstream("6006").unwrap(), tcp("localhost", 6006));
        assert_eq!(parse_upstream("app:80").unwrap(), tcp("app", 80));
        assert_eq!(
            parse_upstream("./a.sock").unwrap(),
            Upstream::Unix("./a.sock".into())
        );
        assert!(parse_upstream("app").unwrap_err().contains("needs a port"));
    }

    #[test]
    fn redirect_ports() {
        let m = ok("80/443:6006");
        assert_eq!((m.redirect_from, m.port), (Some(80), 443));
        assert_eq!(m.upstream, tcp("localhost", 6006));
        let m = ok("80/443:app:8080");
        assert_eq!((m.bind, m.redirect_from, m.port), (None, Some(80), 443));
        assert_eq!(m.upstream, tcp("app", 8080));
        let m = ok("[::1]:80/443:/run/app.sock");
        assert_eq!(m.redirect_from, Some(80));
        assert_eq!(m.upstream, Upstream::Unix("/run/app.sock".into()));
    }

    #[test]
    fn conflicts() {
        let a = ok("6005:6006");
        assert_eq!(a.conflict(&ok("6005:7000")), Some(6005));
        assert_eq!(a.conflict(&ok("127.0.0.1:6005:7000")), Some(6005));
        assert_eq!(a.conflict(&ok("6006:7000")), None);
        assert_eq!(
            ok("127.0.0.1:6005:1").conflict(&ok("10.0.0.1:6005:1")),
            None
        );
        assert_eq!(
            ok("127.0.0.1:6005:1").conflict(&ok("127.0.0.1:6005:2")),
            Some(6005)
        );
        // Redirect ports count too.
        assert_eq!(ok("80/443:1").conflict(&ok("80:2")), Some(80));
        assert_eq!(ok("80/443:1").conflict(&ok("80/8443:2")), Some(80));
        assert_eq!(ok("8080/443:1").conflict(&ok("80/8443:2")), None);
    }

    #[test]
    fn listen_display() {
        assert_eq!(ok("6005:6006").listen_display(), "6005");
        assert_eq!(ok("127.0.0.1:6005:6006").listen_display(), "127.0.0.1:6005");
        assert_eq!(ok("[::1]:6005:6006").listen_display(), "[::1]:6005");
        assert_eq!(ok("80/443:6006").listen_display(), "80/443");
        assert_eq!(
            ok("127.0.0.1:80/443:1").listen_display(),
            "127.0.0.1:80/443"
        );
    }
}
