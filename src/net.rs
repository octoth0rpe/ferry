//! Binding listening sockets.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

use socket2::{Domain, Protocol, Socket, Type};

/// Binds a listening TCP socket. With no address, binds `[::]` accepting both
/// IPv4 and IPv6, falling back to `0.0.0.0` on hosts without IPv6.
pub fn bind(bind: Option<IpAddr>, port: u16) -> io::Result<TcpListener> {
    let v4_any = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
    let Some(ip) = bind else {
        // macOS lets a dual-stack socket bind even when another process has
        // 0.0.0.0 on the same port, and then IPv4 clients reach the other
        // process. Check the IPv4 side is free first.
        if port != 0 {
            drop(bind_addr(v4_any, None)?);
        }
        return match bind_addr(
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
                bind_addr(v4_any, None)
            }
        };
    };
    bind_addr(SocketAddr::new(ip, port), None)
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

/// Whether `ip` is on this machine or a private network: loopback, private,
/// link-local, unique local or carrier-grade NAT (Tailscale), the same
/// ranges the local CA's name constraints allow.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && b & 0xc0 == 64)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
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

/// This machine's LAN addresses, the one on the default route first: that's
/// the one other devices on the same network can most likely reach.
pub fn lan_ips_best_first() -> Vec<IpAddr> {
    let mut ips = local_ips();
    if let Some(best) = default_route_ip()
        && let Some(i) = ips.iter().position(|ip| *ip == best)
    {
        let best = ips.remove(i);
        ips.insert(0, best);
    }
    ips
}

/// The local address of the default route. Connecting a UDP socket picks it
/// without sending anything.
fn default_route_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // TEST-NET-1: never a real host, but routed like one.
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Shows IPv4 clients of a dual-stack socket as `1.2.3.4`, not `::ffff:1.2.3.4`.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// Raises the open-file limit as far as this process may (the soft limit to
/// the hard one), since busy sites need far more than the usual 256 or 1024.
pub fn raise_fd_limit() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else {
        return;
    };
    // macOS refuses more than OPEN_MAX, even when the hard limit is unlimited.
    for want in [hard, 10240] {
        if want <= soft {
            return;
        }
        if setrlimit(Resource::RLIMIT_NOFILE, want, hard).is_ok() {
            return;
        }
    }
}

/// The open-file limit, capped at a million.
pub fn fd_limit() -> u64 {
    use nix::sys::resource::{Resource, getrlimit};
    getrlimit(Resource::RLIMIT_NOFILE).map_or(1024, |(soft, _)| soft.min(1 << 20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.5",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
            "2001:db8::1",
            "2606:4700::1",
        ] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }
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
    fn default_bind_notices_ipv4_conflicts() {
        let other = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = other.local_addr().unwrap().port();
        assert_eq!(
            bind(None, port).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
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
