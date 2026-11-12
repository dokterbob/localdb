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
// `ClientAddr` — which cases are loopback-exempt (item 1).
// -----------------------------------------------------------------

#[test]
fn only_direct_is_loopback_exempt() {
    assert!(ClientAddr::Direct(addr("127.0.0.1")).is_loopback_exempt());
    assert!(!ClientAddr::Direct(addr("203.0.113.7")).is_loopback_exempt());
    assert!(
        !ClientAddr::Forwarded(addr("127.0.0.1")).is_loopback_exempt(),
        "a forwarded address must never be exempt, even when it is loopback"
    );
    assert!(
        !ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")).is_loopback_exempt(),
        "the proxy's own unresolved-fallback address must never be exempt, even when it is \
         loopback"
    );
}

#[test]
fn key_unwraps_every_case() {
    assert_eq!(
        ClientAddr::Direct(addr("203.0.113.1")).key(),
        addr("203.0.113.1")
    );
    assert_eq!(
        ClientAddr::Forwarded(addr("203.0.113.2")).key(),
        addr("203.0.113.2")
    );
    assert_eq!(
        ClientAddr::TrustedProxyUnresolved(addr("203.0.113.3")).key(),
        addr("203.0.113.3")
    );
}

// -----------------------------------------------------------------
// `resolve_client_addr` — table-driven, including spoofing attempts.
// -----------------------------------------------------------------

struct Case {
    name: &'static str,
    peer: &'static str,
    trusted: &'static [&'static str],
    /// `None` means no `X-Forwarded-For` header at all; `Some(&[])` is not
    /// a meaningful distinction at this layer and isn't exercised here —
    /// `rate_limit::forwarded_for_header_values` never returns `Some(&[])`.
    forwarded_for: Option<&'static [&'static str]>,
    want: ClientAddr,
}

#[test]
fn resolve_client_addr_cases() {
    let cases: &[Case] = &[
        Case {
            name: "untrusted peer with no header: peer is the client, directly",
            peer: "203.0.113.7",
            trusted: &[],
            forwarded_for: None,
            want: ClientAddr::Direct(addr("203.0.113.7")),
        },
        Case {
            name: "untrusted peer sending a spoofed header is ignored entirely",
            peer: "203.0.113.7",
            trusted: &[],
            forwarded_for: Some(&["198.51.100.1"]),
            want: ClientAddr::Direct(addr("203.0.113.7")),
        },
        Case {
            name: "trusted peer, single hop: the header value is the client, forwarded",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["198.51.100.1"]),
            want: ClientAddr::Forwarded(addr("198.51.100.1")),
        },
        Case {
            name: "trusted peer, chain of two proxies: skip the trusted hop",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: Some(&["198.51.100.1, 10.0.0.1"]),
            want: ClientAddr::Forwarded(addr("198.51.100.1")),
        },
        Case {
            name: "forged leftmost entry behind a trusted proxy: still walk right to left",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            // A malicious client could set its own X-Forwarded-For before
            // the request reaches the trusted proxy; the proxy appends the
            // real address. The leftmost (attacker-controlled) entry must
            // never be trusted blindly.
            forwarded_for: Some(&["10.0.0.9, 198.51.100.1"]),
            want: ClientAddr::Forwarded(addr("198.51.100.1")),
        },
        Case {
            name: "trusted peer, entirely-trusted chain: fall back to the proxy, unresolved",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: Some(&["10.0.0.1"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, missing header: fall back to the proxy, unresolved",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: None,
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, empty header value: fall back to the proxy, unresolved",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&[""]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, malformed trailing entry fails resolution outright",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            // The nearest proxy's own appended hop is the first (rightmost)
            // one this walk reaches; it is malformed, so resolution fails
            // before the leftmost, attacker-controlled entry is ever
            // consulted — it must never become the client.
            forwarded_for: Some(&["198.51.100.1, not-an-address"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, bracketed ipv6 entry is unparsable (no bracket stripping)",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["[2001:db8::1]"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, ip:port entry is unparsable (no port stripping)",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["203.0.113.7:5678"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, a zone id on an ipv6 entry is unparsable",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["fe80::1%eth0"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, the literal token 'unknown' is unparsable",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["unknown"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, an obfuscated identifier is unparsable",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["_hidden"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, an empty element between commas is unparsable",
            peer: "127.0.0.1",
            // "10.0.0.1" must be trusted here so the walk actually reaches
            // the empty element instead of stopping on it as the client.
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: Some(&["198.51.100.1,,10.0.0.1"]),
            want: ClientAddr::TrustedProxyUnresolved(addr("127.0.0.1")),
        },
        Case {
            name: "trusted peer, multiple header lines concatenated in order",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: Some(&["198.51.100.1", "10.0.0.1"]),
            want: ClientAddr::Forwarded(addr("198.51.100.1")),
        },
        Case {
            name: "trusted peer arriving as an IPv4-mapped IPv6 address",
            peer: "::ffff:127.0.0.1",
            trusted: &["127.0.0.1"],
            forwarded_for: Some(&["198.51.100.1"]),
            want: ClientAddr::Forwarded(addr("198.51.100.1")),
        },
        Case {
            name: "IPv4-mapped IPv6 form in the header is unmapped before trust-checking",
            peer: "127.0.0.1",
            trusted: &["127.0.0.1", "10.0.0.1"],
            forwarded_for: Some(&["198.51.100.1, ::ffff:10.0.0.1"]),
            want: ClientAddr::Forwarded(addr("198.51.100.1")),
        },
    ];

    for case in cases {
        let trusted: Vec<(IpAddr, u8)> = case
            .trusted
            .iter()
            .map(|s| localdb_core::config::schema::parse_ip_cidr(s).unwrap())
            .collect();
        let forwarded_for: Option<Vec<String>> = case
            .forwarded_for
            .map(|lines| lines.iter().map(|s| s.to_string()).collect());
        let got = resolve_client_addr(
            case.peer.parse().unwrap(),
            &trusted,
            forwarded_for.as_deref(),
        );
        assert_eq!(got, case.want, "case {:?} failed", case.name);
    }
}
