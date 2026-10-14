//! Which network addresses HighVi's tools may reach. The egress proxy applies
//! this to every resolved address before connecting.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Addresses of this machine's own interfaces. A public address assigned to
/// this host still leads to its local services, so the proxy refuses them
/// too. `None` when the interfaces cannot be listed; callers fail closed.
pub fn local_addresses() -> Option<Vec<IpAddr>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list we free below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut addresses = Vec::new();
    let mut current = list;
    while !current.is_null() {
        // SAFETY: `current` is a node of the list getifaddrs returned.
        let entry = unsafe { &*current };
        if !entry.ifa_addr.is_null() {
            // SAFETY: `ifa_addr` points to a sockaddr whose family tells
            // which concrete type it is.
            match i32::from(unsafe { (*entry.ifa_addr).sa_family }) {
                libc::AF_INET => {
                    let address = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                    addresses.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        address.sin_addr.s_addr,
                    ))));
                }
                libc::AF_INET6 => {
                    let address = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in6) };
                    addresses.push(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)));
                }
                _ => {}
            }
        }
        current = entry.ifa_next;
    }
    // SAFETY: frees the list returned by getifaddrs exactly once.
    unsafe { libc::freeifaddrs(list) };
    Some(addresses)
}

pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

/// Blocks every IANA special-purpose IPv4 range, not only the private ones.
fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

/// An allowlist: only global unicast (`2000::/3`) minus its special ranges,
/// plus NAT64 addresses that embed a public IPv4 address.
fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    // 64:ff9b::/96, the well-known NAT64 prefix that DNS64 resolvers return
    // on IPv6-only networks. The embedded IPv4 address decides.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [a, b] = segments[6].to_be_bytes();
        let [c, d] = segments[7].to_be_bytes();
        return is_public_ipv4(Ipv4Addr::new(a, b, c, d));
    }
    (segments[0] & 0xe000) == 0x2000
        // 2001::/23: IETF protocol assignments (Teredo, ORCHID, benchmarking…).
        && !(segments[0] == 0x2001 && segments[1] < 0x0200)
        // 2001:db8::/32 and 3fff::/20: documentation.
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && !(segments[0] == 0x3fff && segments[1] < 0x1000)
        // 2002::/16: 6to4, which can wrap any IPv4 address.
        && segments[0] != 0x2002
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_interfaces_include_loopback() {
        let local = local_addresses().unwrap();
        assert!(
            local.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "{local:?}"
        );
    }

    fn public(raw: &str) -> bool {
        is_public_ip(raw.parse().unwrap())
    }

    #[test]
    fn special_purpose_ipv4_is_blocked() {
        for raw in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.0.8",
            "192.0.0.170",
            "192.0.0.255",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!public(raw), "{raw}");
        }
    }

    #[test]
    fn ordinary_ipv4_is_allowed() {
        for raw in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "93.184.215.14",
        ] {
            assert!(public(raw), "{raw}");
        }
    }

    #[test]
    fn non_global_and_tunnelled_ipv6_is_blocked() {
        for raw in [
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "64:ff9b:1::808:808",
            "100::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:2::1",
            "2001:10::1",
            "2001:20::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "2002:808:808::1",
            "3fff::1",
            "3fff:fff::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "4000::1",
            "5f00::1",
        ] {
            assert!(!public(raw), "{raw}");
        }
    }

    #[test]
    fn global_unicast_ipv6_is_allowed() {
        for raw in [
            "2001:200::1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "3ffe::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(public(raw), "{raw}");
        }
    }
}
