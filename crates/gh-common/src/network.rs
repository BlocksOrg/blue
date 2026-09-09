//! Network-address policy shared by package inspection and installation.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// A DNS result is acceptable only when it is non-empty and every address is
/// public. Rejecting the complete mixed set prevents connector fallback from
/// reaching an internal answer.
pub fn all_addresses_are_public(addresses: &[SocketAddr]) -> bool {
    !addresses.is_empty() && addresses.iter().all(|address| is_public_ip(address.ip()))
}

/// Return true only for globally routable unicast addresses.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public_v4(mapped);
            }
            // IPv4-compatible ::/96 encodings are deprecated and not globally
            // routable IPv6 addresses, regardless of the embedded IPv4 value.
            if u128::from(address) <= u128::from(u32::MAX) {
                return false;
            }
            is_public_v6(address)
        }
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    // These two anycast services are the globally reachable exceptions in
    // the otherwise special-purpose 192.0.0.0/24 block.
    if matches!(value, 0xc000_0009 | 0xc000_000a) {
        return true;
    }
    ![
        (0x0000_0000, 8),  // current network
        (0x0a00_0000, 8),  // private
        (0x6440_0000, 10), // carrier-grade NAT
        (0x7f00_0000, 8),  // loopback
        (0xa9fe_0000, 16), // link-local
        (0xac10_0000, 12), // private
        (0xc000_0000, 24), // IETF protocol assignments
        (0xc000_0200, 24), // TEST-NET-1
        (0xc058_6300, 24), // deprecated 6to4 relay anycast
        (0xc0a8_0000, 16), // private
        (0xc612_0000, 15), // benchmarking
        (0xc633_6400, 24), // TEST-NET-2
        (0xcb00_7100, 24), // TEST-NET-3
        (0xe000_0000, 4),  // multicast
        (0xf000_0000, 4),  // reserved/broadcast
    ]
    .iter()
    .any(|(network, prefix)| in_v4_prefix(value, *network, *prefix))
}

fn in_v4_prefix(value: u32, network: u32, prefix: u32) -> bool {
    let mask = u32::MAX << (32 - prefix);
    value & mask == network & mask
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let value = u128::from(address);
    // IANA records a handful of globally reachable assignments inside the
    // otherwise special-purpose 2001::/23 block. More-specific entries win.
    if [
        (0x2001_0001_0000_0000_0000_0000_0000_0001, 128), // PCP anycast
        (0x2001_0001_0000_0000_0000_0000_0000_0002, 128), // TURN anycast
        (0x2001_0001_0000_0000_0000_0000_0000_0003, 128), // DNS-SD anycast
        (0x2001_0003_0000_0000_0000_0000_0000_0000, 32),  // AMT
        (0x2001_0004_0112_0000_0000_0000_0000_0000, 48),  // AS112-v6
        (0x2001_0020_0000_0000_0000_0000_0000_0000, 28),  // ORCHIDv2
        (0x2001_0030_0000_0000_0000_0000_0000_0000, 28),  // DETs
    ]
    .iter()
    .any(|(network, prefix)| in_v6_prefix(value, *network, *prefix))
    {
        return true;
    }

    // Public IPv6 sources are deliberately limited to allocated global
    // unicast space. Translation/transition, reserved, local, and multicast
    // ranges outside 2000::/3 therefore fail closed automatically.
    in_v6_prefix(value, 0x2000_0000_0000_0000_0000_0000_0000_0000, 3)
        && ![
            (0x2001_0000_0000_0000_0000_0000_0000_0000, 23), // protocol assignments
            (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32), // documentation
            (0x2002_0000_0000_0000_0000_0000_0000_0000, 16), // 6to4
            (0x3fff_0000_0000_0000_0000_0000_0000_0000, 20), // documentation
        ]
        .iter()
        .any(|(network, prefix)| in_v6_prefix(value, *network, *prefix))
}

fn in_v6_prefix(value: u128, network: u128, prefix: u32) -> bool {
    let mask = u128::MAX << (128 - prefix);
    value & mask == network & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_special_addresses_and_accepts_public_examples() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::8.8.8.8",
            "64:ff9b::127.0.0.1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2002:0808:0808::1",
            "2001:2::1",
            "2001:1::4",
            "4000::1",
        ] {
            assert!(!is_public_ip(value.parse().unwrap()), "{value}");
        }
        for value in [
            "8.8.8.8",
            "1.1.1.1",
            "192.0.0.9",
            "192.0.0.10",
            "2606:4700:4700::1111",
            "2001:1::1",
            "2001:3::1",
            "2001:4:112::1",
            "2001:20::1",
            "2001:30::1",
        ] {
            assert!(is_public_ip(value.parse().unwrap()), "{value}");
        }
    }

    #[test]
    fn rejects_empty_and_mixed_dns_results() {
        assert!(!all_addresses_are_public(&[]));
        assert!(!all_addresses_are_public(&[
            "8.8.8.8:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ]));
        assert!(all_addresses_are_public(&[
            "8.8.8.8:443".parse().unwrap(),
            "[2606:4700:4700::1111]:443".parse().unwrap(),
        ]));
    }
}
