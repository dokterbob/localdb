use super::*;

fn addr(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn range(s: &str) -> (IpAddr, u8) {
    localdb_core::config::schema::parse_ip_cidr(s).unwrap()
}

// -----------------------------------------------------------------
// `unmap_ipv4`
// -----------------------------------------------------------------

#[test]
fn unmap_ipv4_converts_a_mapped_address() {
    assert_eq!(unmap_ipv4(addr("::ffff:127.0.0.1")), addr("127.0.0.1"));
    assert_eq!(unmap_ipv4(addr("::ffff:203.0.113.9")), addr("203.0.113.9"));
}

#[test]
fn unmap_ipv4_leaves_a_genuine_ipv6_address_unchanged() {
    assert_eq!(unmap_ipv4(addr("::1")), addr("::1"));
    assert_eq!(unmap_ipv4(addr("2001:db8::1")), addr("2001:db8::1"));
}

#[test]
fn unmap_ipv4_leaves_ipv4_unchanged() {
    assert_eq!(unmap_ipv4(addr("203.0.113.9")), addr("203.0.113.9"));
}

// -----------------------------------------------------------------
// `limiter_key` — IPv6 keyed per /64 (item 3).
// -----------------------------------------------------------------

#[test]
fn limiter_key_masks_ipv6_to_its_64_prefix() {
    assert_eq!(
        limiter_key(addr("2001:db8:1:2:3:4:5:6")),
        addr("2001:db8:1:2::")
    );
}

#[test]
fn limiter_key_two_addresses_in_one_64_share_a_key() {
    assert_eq!(
        limiter_key(addr("2001:db8:1:2::aaaa")),
        limiter_key(addr("2001:db8:1:2::bbbb"))
    );
}

#[test]
fn limiter_key_two_different_64s_have_different_keys() {
    assert_ne!(
        limiter_key(addr("2001:db8:1:2::1")),
        limiter_key(addr("2001:db8:1:3::1"))
    );
}

#[test]
fn limiter_key_leaves_ipv4_unchanged() {
    assert_eq!(limiter_key(addr("203.0.113.9")), addr("203.0.113.9"));
}

#[test]
fn mapped_loopback_is_recognized_as_loopback_after_unmapping() {
    assert!(unmap_ipv4(addr("::ffff:127.0.0.1")).is_loopback());
}

// -----------------------------------------------------------------
// `is_trusted` / CIDR matching
// -----------------------------------------------------------------

#[test]
fn is_trusted_matches_an_exact_address() {
    let ranges = [range("127.0.0.1")];
    assert!(is_trusted(addr("127.0.0.1"), &ranges));
    assert!(!is_trusted(addr("127.0.0.2"), &ranges));
}

#[test]
fn is_trusted_matches_an_ipv4_cidr_range() {
    let ranges = [range("203.0.113.0/24")];
    assert!(is_trusted(addr("203.0.113.1"), &ranges));
    assert!(is_trusted(addr("203.0.113.255"), &ranges));
    assert!(!is_trusted(addr("203.0.114.1"), &ranges));
}

#[test]
fn is_trusted_matches_a_non_byte_aligned_prefix() {
    // 203.0.113.0/26 covers .0-.63 only.
    let ranges = [range("203.0.113.0/26")];
    assert!(is_trusted(addr("203.0.113.63"), &ranges));
    assert!(!is_trusted(addr("203.0.113.64"), &ranges));
}

#[test]
fn is_trusted_matches_an_ipv6_cidr_range() {
    let ranges = [range("2001:db8::/32")];
    assert!(is_trusted(addr("2001:db8:ffff::1"), &ranges));
    assert!(!is_trusted(addr("2001:db9::1"), &ranges));
}

#[test]
fn is_trusted_never_matches_across_families() {
    let ranges = [range("203.0.113.0/24")];
    assert!(!is_trusted(addr("2001:db8::1"), &ranges));
}

#[test]
fn is_trusted_unmaps_a_mapped_peer_before_matching_an_ipv4_range() {
    let ranges = [range("127.0.0.1")];
    assert!(is_trusted(addr("::ffff:127.0.0.1"), &ranges));
}

#[test]
fn is_trusted_with_an_empty_list_trusts_nothing() {
    assert!(!is_trusted(addr("127.0.0.1"), &[]));
}

// -----------------------------------------------------------------
// `resolve_client_addr` — table-driven, including spoofing attempts.
// -----------------------------------------------------------------

struct Case {
    name: &'static str,
    peer: &'static str,
    trusted: &'static [&'static str],
    forwarded_for: &'static [&'static str],
    want: &'static str,
}

#[test]
fn resolve_client_addr_cases() {
    let cases: &[Case] = &[
        Case {
            name: "untrusted peer with no header: peer is the client",
            peer: "203.0.113.7",
            trusted: &[],
            forwarded_for: &[],
            want: "203.0.113.7",
        },
        Case {
            name: "untrusted peer sending a spoofed header is ignored entirely",
            peer: "203.0.113.7",
            trusted: &[],
            forwarded_for: &["198.51.100.1"],
            want: "203.0.113.7",
        },
        Case {
            name: "trusted peer, single hop: the header value is the client",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: &["198.51.100.1"],
            want: "198.51.100.1",
        },
        Case {
            name: "trusted peer, chain of two proxies: skip the trusted hop",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: &["198.51.100.1, 10.0.0.1"],
            want: "198.51.100.1",
        },
        Case {
            name: "forged leftmost entry behind a trusted proxy: still walk right to left",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            // A malicious client could set its own X-Forwarded-For before
            // the request reaches the trusted proxy; the proxy appends the
            // real address. The leftmost (attacker-controlled) entry must
            // never be trusted blindly.
            forwarded_for: &["10.0.0.9, 198.51.100.1"],
            want: "198.51.100.1",
        },
        Case {
            name: "trusted peer, entirely-trusted chain: fall back to peer",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: &["10.0.0.1"],
            want: "127.0.0.1",
        },
        Case {
            name: "trusted peer, missing header: fall back to peer",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: &[],
            want: "127.0.0.1",
        },
        Case {
            name: "trusted peer, empty header value: fall back to peer",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: &[""],
            want: "127.0.0.1",
        },
        Case {
            name: "trusted peer, malformed trailing entry is skipped",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: &["198.51.100.1, not-an-address"],
            want: "198.51.100.1",
        },
        Case {
            name: "trusted peer, multiple header lines concatenated in order",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: &["198.51.100.1", "10.0.0.1"],
            want: "198.51.100.1",
        },
        Case {
            name: "trusted peer arriving as an IPv4-mapped IPv6 address",
            peer: "::ffff:127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: &["198.51.100.1"],
            want: "198.51.100.1",
        },
        Case {
            name: "IPv4-mapped IPv6 form in the header is unmapped before trust-checking",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: &["198.51.100.1, ::ffff:10.0.0.1"],
            want: "198.51.100.1",
        },
    ];

    for case in cases {
        let trusted: Vec<(IpAddr, u8)> = case
            .trusted
            .iter()
            .map(|s| localdb_core::config::schema::parse_ip_cidr(s).unwrap())
            .collect();
        let forwarded_for: Vec<String> = case.forwarded_for.iter().map(|s| s.to_string()).collect();
        let got = resolve_client_addr(case.peer.parse().unwrap(), &trusted, &forwarded_for);
        assert_eq!(
            got,
            case.want.parse::<IpAddr>().unwrap(),
            "case {:?} failed",
            case.name
        );
    }
}
