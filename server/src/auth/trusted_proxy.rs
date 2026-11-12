//! Pure address-resolution logic behind `server::auth::rate_limit`'s
//! trusted-proxy support (specs/05-surfaces.md §3.1): deciding which
//! address a request should be rate-limited/exempted on, given the TCP
//! peer address, the configured `server.abuse_limits.trusted_proxies`
//! ranges, and the request's `X-Forwarded-For` header(s). This is transport
//! logic (not domain logic), so it lives here rather than in `core` — only
//! the trusted-proxy *string* parser (`localdb_core::config::schema::
//! parse_ip_cidr`) is shared with `core`'s config-load validation.
//!
//! Only `X-Forwarded-For` is accepted; RFC 7239's `Forwarded: for=` header
//! is not — see this module's doc on [`resolve_client_addr`].

use std::net::{IpAddr, Ipv6Addr};

/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) becomes its plain IPv4 form; every
/// other address (including a genuine IPv6 address) is returned unchanged.
/// Applied to every address this module compares — the peer address and
/// each `X-Forwarded-For` entry — so a dual-stack peer can't dodge an IPv4
/// `trusted_proxies` entry (or the loopback exemption) just by arriving
/// over its IPv6-mapped form.
pub fn unmap_ipv4(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Reduce a (already [`unmap_ipv4`]-normalized) address to the limiter's
/// bucket key: an IPv4 address is used as-is; a genuine IPv6 address is
/// masked to its /64 network prefix, since an ISP commonly hands a single
/// residential customer an entire /64 — keying on the full address would
/// let that customer spread requests across many buckets for free, while
/// keying on /64 treats the whole delegation as one caller, the same way
/// IPv4 NAT already pools many devices behind one bucket.
pub fn limiter_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

/// Whether `addr` falls inside any of `ranges` (each `(network, prefix_len)`
/// — already validated by `localdb_core::config::schema::parse_ip_cidr` at
/// config load time). `addr` is [`unmap_ipv4`]-normalized before matching,
/// so an IPv4-mapped peer is checked against IPv4 ranges correctly.
pub fn is_trusted(addr: IpAddr, ranges: &[(IpAddr, u8)]) -> bool {
    let addr = unmap_ipv4(addr);
    ranges.iter().any(|&(net, len)| in_network(addr, net, len))
}

fn in_network(addr: IpAddr, net: IpAddr, prefix_len: u8) -> bool {
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => prefix_matches(&a.octets(), &n.octets(), prefix_len),
        (IpAddr::V6(a), IpAddr::V6(n)) => prefix_matches(&a.octets(), &n.octets(), prefix_len),
        _ => false,
    }
}

/// Hand-written prefix compare on the address's 4- or 16-byte form — no
/// CIDR crate is needed for a check this small.
fn prefix_matches(addr: &[u8], net: &[u8], prefix_len: u8) -> bool {
    let full_bytes = (prefix_len / 8) as usize;
    let rem_bits = prefix_len % 8;
    if addr[..full_bytes] != net[..full_bytes] {
        return false;
    }
    if rem_bits == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - rem_bits);
    (addr[full_bytes] & mask) == (net[full_bytes] & mask)
}

/// Resolve the address `server::auth::rate_limit` should key/exempt a
/// request on.
///
/// `peer` is the TCP peer address; `trusted_proxies` the configured,
/// validated ranges (`server.abuse_limits.trusted_proxies`); `forwarded_for`
/// every `X-Forwarded-For` header line present on the request, in the order
/// they appear (multiple lines are concatenated in order, each itself a
/// comma-separated list — the standard shape for a repeated, list-valued
/// header). RFC 7239's `Forwarded: for=` is deliberately not accepted:
/// supporting one header is enough to make a trusted reverse proxy work,
/// and every operator needing it controls which header their own proxy
/// sends.
///
/// If `peer` (after [`unmap_ipv4`]) is not inside any `trusted_proxies`
/// range, this returns `peer` unmapped and never looks at `forwarded_for`
/// — a header is only ever honored from a configured, trusted proxy, so an
/// arbitrary caller can never spoof its address by sending one.
///
/// Otherwise, walks the concatenated list **right to left** — the entry
/// nearest this daemon was appended most recently by the nearest proxy; the
/// leftmost entry is whatever the original caller claimed and must never be
/// trusted on its own — skipping entries that themselves parse as a
/// trusted proxy (a chain of proxies forwarding through each other) and
/// returning the first entry that isn't. A malformed or empty entry is
/// skipped, not treated as the client. If the walk finds no usable,
/// untrusted entry at all (an entirely-trusted chain, every entry
/// malformed, or an empty/missing header), this falls back to `peer`.
pub fn resolve_client_addr(
    peer: IpAddr,
    trusted_proxies: &[(IpAddr, u8)],
    forwarded_for: &[String],
) -> IpAddr {
    let peer = unmap_ipv4(peer);
    if !is_trusted(peer, trusted_proxies) {
        return peer;
    }
    let entries = forwarded_for
        .iter()
        .flat_map(|line| line.split(','))
        .map(|s| s.trim());
    for raw in entries.collect::<Vec<_>>().into_iter().rev() {
        let Ok(candidate) = raw.parse::<IpAddr>() else {
            continue;
        };
        let candidate = unmap_ipv4(candidate);
        if !is_trusted(candidate, trusted_proxies) {
            return candidate;
        }
    }
    peer
}

#[cfg(test)]
mod tests;
