use std::net::{IpAddr, SocketAddr};

use if_addrs::get_if_addrs;

use crate::VIRTUAL_SUBNET;

/// LAN v4 + global v6 addresses of physical interfaces, excluding loopback,
/// link-local and our own virtual subnet. `port` is the shared STUN/WG port.
pub fn local_candidates(port: u16) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let Ok(ifaces) = get_if_addrs() else {
        return out;
    };
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        let ip = iface.ip();
        let keep = match ip {
            IpAddr::V4(v4) => {
                !v4.is_link_local() && !v4.is_unspecified() && !in_virtual_subnet(v4)
            }
            IpAddr::V6(v6) => {
                let o = v6.octets();
                !v6.is_loopback()
                    && !v6.is_unspecified()
                    && (o[0] & 0xe0) == 0x20 // 2000::/3 global unicast
                    && !(o[0] == 0xfe && (o[1] & 0xc0) == 0x80) // fe80::/10
            }
        };
        if keep {
            out.push(SocketAddr::new(ip, port));
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn in_virtual_subnet(v4: std::net::Ipv4Addr) -> bool {
    let o = VIRTUAL_SUBNET.octets();
    let a = v4.octets();
    a[0] == o[0] && a[1] == o[1] && a[2] == o[2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn virtual_subnet_filter() {
        assert!(in_virtual_subnet(Ipv4Addr::new(10, 99, 0, 5)));
        assert!(!in_virtual_subnet(Ipv4Addr::new(10, 99, 1, 5)));
        assert!(!in_virtual_subnet(Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn candidates_exclude_loopback_and_virtual() {
        for c in local_candidates(23947) {
            assert!(!c.ip().is_loopback());
            assert_eq!(c.port(), 23947);
            if let IpAddr::V4(v4) = c.ip() {
                assert!(!in_virtual_subnet(v4));
                assert!(!v4.is_link_local());
            }
        }
    }
}