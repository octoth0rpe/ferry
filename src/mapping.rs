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
//!
//! IPv6 addresses must be written in brackets.

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
    pub upstream: Upstream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// Plain HTTP over TCP. `host` is a hostname or an IP without brackets.
    Tcp { host: String, port: u16 },
    /// Plain HTTP over a Unix domain socket.
    Unix(PathBuf),
}

impl Mapping {
    /// Whether two mappings would try to listen on the same socket.
    pub fn conflicts_with(&self, other: &Mapping) -> bool {
        self.port == other.port
            && (self.bind.is_none() || other.bind.is_none() || self.bind == other.bind)
    }

    /// The listen side as written by a user, e.g. `6005` or `127.0.0.1:6005`.
    pub fn listen_display(&self) -> String {
        match self.bind {
            None => self.port.to_string(),
            Some(IpAddr::V4(ip)) => format!("{ip}:{}", self.port),
            Some(IpAddr::V6(ip)) => format!("[{ip}]:{}", self.port),
        }
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
    let (listen, upstream) = if let Some(i) = s.find("://") {
        let (listen, scheme) = s[..i]
            .rsplit_once(':')
            .ok_or("expected a listen port before the upstream URL, e.g. 6005:http://app:8080")?;
        let upstream = match scheme {
            "http" => parse_http_upstream(&s[i + 3..])?,
            "https" => return Err("https upstreams aren't supported yet".into()),
            other => return Err(format!("unsupported upstream scheme `{other}`")),
        };
        (listen, upstream)
    } else if let Some(i) = find_unix_path(s) {
        let path = &s[i + 1..];
        (&s[..i], Upstream::Unix(PathBuf::from(path)))
    } else {
        let parts = split_colons(s)?;
        // Split the parts into listen and upstream halves. The only ambiguous
        // case is three parts: `port:host:port` vs `bind:port:port`.
        let n_listen = match parts.len() {
            1 => {
                return Err(format!(
                    "expected LISTEN:UPSTREAM, e.g. {s}:{}",
                    next_port(s)
                ));
            }
            2 => 1,
            3 if is_digits(parts[0]) => 1,
            3 => 2,
            4 => 2,
            _ => return Err(too_many_parts()),
        };
        let upstream = match &parts[n_listen..] {
            [port] => Upstream::Tcp {
                host: DEFAULT_UPSTREAM_HOST.into(),
                port: parse_port(port)?,
            },
            [host, port] => Upstream::Tcp {
                host: parse_host(host)?,
                port: parse_port(port)?,
            },
            _ => unreachable!(),
        };
        let (bind, port) = parse_listen(&parts[..n_listen])?;
        return Ok(Mapping {
            bind,
            port,
            upstream,
        });
    };

    let (bind, port) = parse_listen(&split_colons(listen)?)?;
    Ok(Mapping {
        bind,
        port,
        upstream,
    })
}

/// Index of the `:` that starts a Unix socket path (`:/`, `:./`, `:../`).
fn find_unix_path(s: &str) -> Option<usize> {
    [":/", ":./", ":../"]
        .iter()
        .filter_map(|pat| s.find(pat))
        .min()
}

fn parse_listen(parts: &[&str]) -> Result<(Option<IpAddr>, u16), String> {
    match parts {
        [port] => Ok((None, parse_port(port)?)),
        [bind, port] => Ok((Some(parse_bind(bind)?), parse_port(port)?)),
        _ => Err(too_many_parts()),
    }
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
    fn conflicts() {
        let a = ok("6005:6006");
        assert!(a.conflicts_with(&ok("6005:7000")));
        assert!(a.conflicts_with(&ok("127.0.0.1:6005:7000")));
        assert!(!a.conflicts_with(&ok("6006:7000")));
        assert!(!ok("127.0.0.1:6005:1").conflicts_with(&ok("10.0.0.1:6005:1")));
        assert!(ok("127.0.0.1:6005:1").conflicts_with(&ok("127.0.0.1:6005:2")));
    }

    #[test]
    fn listen_display() {
        assert_eq!(ok("6005:6006").listen_display(), "6005");
        assert_eq!(ok("127.0.0.1:6005:6006").listen_display(), "127.0.0.1:6005");
        assert_eq!(ok("[::1]:6005:6006").listen_display(), "[::1]:6005");
    }
}
