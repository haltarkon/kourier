//! The gate's allow-list: the address blocks TDLib's connections may go to, and the ports.
//! THE one file of the lane that may name Telegram's addresses (TG-1 plan §2.10,
//! `tests/source.rs`).
//!
//! VERIFIED 2026-11-01 sha256:3ecd072cedf72eceee6961b18a51e37f905d5ce4a656a5d24cf5fea63e6ec651 list-sha256:a1a5ac957468bf40a8033f61a37cc73d43d8458f4cbe7ca6d110439c6c58fcc0 https://core.telegram.org/resources/cidr.txt
//! Re-check at every TDLib pin bump and before every real run; a real run's non-zero
//! "refused by rule" count means the list is stale.
//!
//! Why blocks and not the compiled DC addresses: TDLib takes new DC addresses from the server's
//! config at run time (`help.getConfig`, `updateDcOptions`), which the compiled list does not know.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Telegram's IPv4 blocks (network, prefix length).
pub(crate) const TELEGRAM_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(91, 105, 192, 0), 23),
    (Ipv4Addr::new(91, 108, 4, 0), 22),
    (Ipv4Addr::new(91, 108, 8, 0), 22),
    (Ipv4Addr::new(91, 108, 12, 0), 22),
    (Ipv4Addr::new(91, 108, 16, 0), 22),
    (Ipv4Addr::new(91, 108, 20, 0), 22),
    (Ipv4Addr::new(91, 108, 56, 0), 22),
    // The published list omits it and the pin compiles it (DC 2, `ConnectionCreator.cpp:1302`).
    (Ipv4Addr::new(95, 161, 76, 100), 32),
    (Ipv4Addr::new(149, 154, 160, 0), 20),
    (Ipv4Addr::new(185, 76, 151, 0), 24),
];

/// Telegram's IPv6 blocks (network, prefix length).
pub(crate) const TELEGRAM_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::new(0x2001, 0xb28, 0xf23c, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 0xb28, 0xf23d, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 0xb28, 0xf23f, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 0x67c, 0x4e8, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2a0a, 0xf280, 0, 0, 0, 0, 0, 0), 32),
];

/// The ports TDLib walks for a data centre (spike §4): the only ports the gate forwards to.
pub(crate) const PORTS: [u16; 3] = [443, 80, 5222];

/// The mask of a prefix length over `bits` bits.
fn mask(len: u8, bits: u32) -> u128 {
    if len == 0 {
        0
    } else {
        (u128::MAX << (bits - u32::from(len))) & (u128::MAX >> (128 - bits))
    }
}

/// Whether `ip` lies in `(net, len)`.
fn v4_in(ip: Ipv4Addr, (net, len): (Ipv4Addr, u8)) -> bool {
    let m = mask(len, 32);
    u128::from(ip.to_bits()) & m == u128::from(net.to_bits()) & m
}

fn v6_in(ip: Ipv6Addr, (net, len): (Ipv6Addr, u8)) -> bool {
    let m = mask(len, 128);
    ip.to_bits() & m == net.to_bits() & m
}

/// Whether the gate may connect to `ip`. An IPv4-mapped IPv6 address is NOT looked up in the IPv4
/// list: TDLib sends IPv4 destinations as IPv4 (`Socks5.cpp:101-115`), so a mapped one is refused.
pub(crate) fn allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => TELEGRAM_V4.iter().any(|&r| v4_in(v4, r)),
        IpAddr::V6(v6) => TELEGRAM_V6.iter().any(|&r| v6_in(v6, r)),
    }
}

/// Whether the gate may connect to `port`.
pub(crate) fn port_allowed(port: u16) -> bool {
    PORTS.contains(&port)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Space no Telegram block may touch: this host, private networks, link-local, shared and
    /// documentation space, multicast and the rest of the special blocks.
    const SPECIAL_V4: &[(Ipv4Addr, u8)] = &[
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ];
    const SPECIAL_V6: &[(Ipv6Addr, u8)] = &[
        (Ipv6Addr::UNSPECIFIED, 128),
        (Ipv6Addr::LOCALHOST, 128),
        (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
        (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
        (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
        (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
        (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    ];

    /// Two blocks overlap when the shorter one holds the other's network.
    fn v4_overlap(a: (Ipv4Addr, u8), b: (Ipv4Addr, u8)) -> bool {
        let short = a.1.min(b.1);
        v4_in(a.0, (b.0, short)) || v4_in(b.0, (a.0, short))
    }

    fn v6_overlap(a: (Ipv6Addr, u8), b: (Ipv6Addr, u8)) -> bool {
        let short = a.1.min(b.1);
        v6_in(a.0, (b.0, short)) || v6_in(b.0, (a.0, short))
    }

    /// T-g10: no block of the list touches private, loopback, link-local, ULA or other special
    /// space, so no mistake in the list can open the gate to this machine or its network.
    #[test]
    fn no_range_touches_private_or_loopback_space() {
        for &r in TELEGRAM_V4 {
            for &s in SPECIAL_V4 {
                assert!(!v4_overlap(r, s), "an IPv4 block touches special space");
            }
        }
        for &r in TELEGRAM_V6 {
            for &s in SPECIAL_V6 {
                assert!(!v6_overlap(r, s), "an IPv6 block touches special space");
            }
        }
        for ip in [
            IpAddr::from(Ipv4Addr::LOCALHOST),
            IpAddr::from(Ipv4Addr::UNSPECIFIED),
            IpAddr::from(Ipv6Addr::LOCALHOST),
            IpAddr::from(Ipv6Addr::UNSPECIFIED),
        ] {
            assert!(!allowed(ip), "a local address is allowed");
        }
    }

    /// Every block is written as its network (no host bits) with a sane length, so the list says
    /// what it looks like.
    #[test]
    fn every_block_is_a_network() {
        for &(net, len) in TELEGRAM_V4 {
            assert!((8..=32).contains(&len), "an IPv4 prefix length is odd");
            assert_eq!(
                u128::from(net.to_bits()) & !mask(len, 32) & u128::from(u32::MAX),
                0,
                "an IPv4 block has host bits"
            );
        }
        for &(net, len) in TELEGRAM_V6 {
            assert!((16..=128).contains(&len), "an IPv6 prefix length is odd");
            assert_eq!(
                net.to_bits() & !mask(len, 128),
                0,
                "an IPv6 block has host bits"
            );
        }
    }

    /// The data centres compiled into the pinned TDLib (`ConnectionCreator.cpp:1293-1311`;
    /// `:1259-1277` at a94b83d) are all inside the list: a start with an empty binlog reaches them
    /// through the gate.
    #[test]
    fn the_compiled_data_centres_are_allowed() {
        let v4 = [
            "149.154.175.10",
            "149.154.167.40",
            "149.154.175.117",
            "149.154.175.50",
            "149.154.167.51",
            "95.161.76.100",
            "149.154.175.100",
            "149.154.167.91",
            "149.154.171.5",
        ];
        let v6 = [
            "2001:b28:f23d:f001::e",
            "2001:67c:4e8:f002::e",
            "2001:b28:f23d:f003::e",
            "2001:b28:f23d:f001::a",
            "2001:67c:4e8:f002::a",
            "2001:b28:f23d:f003::a",
            "2001:67c:4e8:f004::a",
            "2001:b28:f23f:f005::a",
        ];
        for text in v4.iter().chain(v6.iter()) {
            let ip: IpAddr = text.parse().expect("an address");
            assert!(allowed(ip), "a compiled data centre is not in the list");
        }
    }

    /// The one block outside the published list is the one address the pin compiles: its neighbours
    /// and the rest of the /20 the list was seeded with are out.
    #[test]
    fn the_block_outside_the_published_list_is_one_address() {
        assert!(allowed(IpAddr::from(Ipv4Addr::new(95, 161, 76, 100))));
        for other in [
            Ipv4Addr::new(95, 161, 76, 99),
            Ipv4Addr::new(95, 161, 76, 101),
            Ipv4Addr::new(95, 161, 64, 0),
            Ipv4Addr::new(95, 161, 79, 255),
        ] {
            assert!(
                !allowed(IpAddr::from(other)),
                "an address of the seeded block is allowed"
            );
        }
    }

    /// The edges of a block: the first and last address are in, the next ones out; a mapped IPv6
    /// spelling of an allowed IPv4 address is out.
    #[test]
    fn a_block_has_exact_edges_and_mapped_addresses_are_out() {
        let (net, len) = TELEGRAM_V4[0];
        let size = 1u32 << (32 - u32::from(len));
        let first = net.to_bits();
        assert!(allowed(IpAddr::from(Ipv4Addr::from_bits(first))));
        assert!(allowed(IpAddr::from(Ipv4Addr::from_bits(first + size - 1))));
        assert!(!allowed(IpAddr::from(Ipv4Addr::from_bits(first - 1))));
        assert!(!allowed(IpAddr::from(Ipv4Addr::from_bits(first + size))));
        assert!(!allowed(IpAddr::from(net.to_ipv6_mapped())));
        for port in PORTS {
            assert!(port_allowed(port));
        }
        for port in [0, 22, 53, 1080, 8080, 8443] {
            assert!(!port_allowed(port), "a port outside the three is allowed");
        }
    }
}
