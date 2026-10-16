//! Binding listening sockets.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

use socket2::{Domain, Protocol, Socket, Type};

/// Binds a listening TCP socket. With no address, binds `[::]` accepting both
/// IPv4 and IPv6, falling back to `0.0.0.0` on hosts without IPv6.
pub fn bind(bind: Option<IpAddr>, port: u16) -> io::Result<TcpListener> {
    match bind {
        Some(ip) => bind_addr(SocketAddr::new(ip, port), None),
        None => {
            match bind_addr(
                SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port),
                Some(false),
            ) {
                Ok(l) => Ok(l),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    Err(e)
                }
                Err(e) => {
                    tracing::debug!("IPv6 unavailable ({e}); listening on IPv4 only");
                    bind_addr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port), None)
                }
            }
        }
    }
}

fn bind_addr(addr: SocketAddr, only_v6: Option<bool>) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if let Some(only_v6) = only_v6 {
        socket.set_only_v6(only_v6)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Describes a bind failure in terms a user can act on.
pub fn bind_error(listen: &str, port: u16, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::AddrInUse => {
            format!("can't listen on {listen}: port {port} is already in use")
        }
        io::ErrorKind::PermissionDenied if port < 1024 => format!(
            "can't listen on {listen}: permission denied (ports below 1024 need root or \
             CAP_NET_BIND_SERVICE on Linux)"
        ),
        io::ErrorKind::AddrNotAvailable => {
            format!("can't listen on {listen}: that address doesn't belong to this machine")
        }
        _ => format!("can't listen on {listen}: {e}"),
    }
}

/// This machine's IPv4 addresses that other machines could reach: not
/// loopback or link-local. (IPv6 addresses are left out; they are often
/// temporary privacy addresses that change regularly.)
pub fn local_ips() -> Vec<IpAddr> {
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    let mut ips: Vec<IpAddr> = addrs
        .filter(|ifa| ifa.flags.contains(nix::net::if_::InterfaceFlags::IFF_UP))
        .filter_map(|ifa| {
            ifa.address?
                .as_sockaddr_in()
                .map(|sin| IpAddr::V4(sin.ip()))
        })
        .filter(|ip| !ip.is_loopback() && !matches!(ip, IpAddr::V4(v4) if v4.is_link_local()))
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// Shows IPv4 clients of a dual-stack socket as `1.2.3.4`, not `::ffff:1.2.3.4`.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn default_bind_accepts_ipv4_and_ipv6() {
        let l = bind(None, 0).unwrap();
        let port = l.local_addr().unwrap().port();
        TcpStream::connect(("127.0.0.1", port)).unwrap();
        if TcpStream::connect(("::1", port)).is_err() {
            // IPv6 loopback can be missing in containers; only IPv4 is required.
            eprintln!("note: no IPv6 loopback");
        }
    }

    #[test]
    fn explicit_bind() {
        let l = bind(Some("127.0.0.1".parse().unwrap()), 0).unwrap();
        assert_eq!(
            l.local_addr().unwrap().ip(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn in_use_is_reported() {
        let l = bind(None, 0).unwrap();
        let port = l.local_addr().unwrap().port();
        let e = bind(None, port).unwrap_err();
        assert_eq!(
            bind_error("6005", port, &e),
            format!("can't listen on 6005: port {port} is already in use")
        );
    }

    #[test]
    fn canonical_unmaps_ipv4() {
        let a: SocketAddr = "[::ffff:10.0.0.1]:5".parse().unwrap();
        assert_eq!(canonical(a).to_string(), "10.0.0.1:5");
    }
}
