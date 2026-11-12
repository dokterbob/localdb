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

/// Which of the three cases [`resolve_client_addr`] landed in — the
/// middleware needs to tell them apart because only one is ever exempt from
/// the rate limit as a loopback caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAddr {
    /// The TCP peer itself, because it is not a configured trusted proxy.
    /// The loopback exemption applies to this case only — a direct
    /// loopback peer must never throttle its own user.
    Direct(IpAddr),
    /// An address `X-Forwarded-For` named, honored because the TCP peer is
    /// a configured trusted proxy. Never exempt, whatever the address is —
    /// loopback or otherwise — since nothing authenticates a header's
    /// content; a forwarded `127.0.0.1` is not the daemon's own loopback
    /// caller, it is whatever the proxy says, and must be rate-limited like
    /// any other claimed client.
    Forwarded(IpAddr),
    /// The TCP peer is a configured trusted proxy, but no usable client
    /// address could be resolved from its `X-Forwarded-For` (missing,
    /// empty, every hop malformed or non-UTF-8, or the chain is entirely
    /// other trusted proxies). Carries the peer's own address, which is
    /// what the request is limited on — one shared bucket for everything
    /// that proxy forwards without a usable header. Never exempt.
    TrustedProxyUnresolved(IpAddr),
}

impl ClientAddr {
    /// The address to key the rate limiter's bucket on, regardless of
    /// which case this is.
    pub fn key(self) -> IpAddr {
        match self {
            ClientAddr::Direct(ip)
            | ClientAddr::Forwarded(ip)
            | ClientAddr::TrustedProxyUnresolved(ip) => ip,
        }
    }

    /// Whether this resolution is exempt from the rate limit as a loopback
    /// caller. Only [`ClientAddr::Direct`] ever is — see the type's doc
    /// comment for why the other two cases must not be, even when the
    /// address they carry happens to be loopback.
    pub fn is_loopback_exempt(self) -> bool {
        matches!(self, ClientAddr::Direct(ip) if ip.is_loopback())
    }
}

/// Resolve the address `server::auth::rate_limit` should key/exempt a
/// request on.
///
/// `peer` is the TCP peer address; `trusted_proxies` the configured,
/// validated ranges (`server.abuse_limits.trusted_proxies`); `forwarded_for`
/// is `Some` of every `X-Forwarded-For` header line present on the request,
/// in the order they appear (multiple lines are concatenated in order, each
/// itself a comma-separated list — the standard shape for a repeated,
/// list-valued header), or `None` if the header was missing, or if any line
/// of it failed to decode as text (a non-UTF-8 header line makes resolution
/// fail exactly like a missing one — see
/// `rate_limit::forwarded_for_header_values`). RFC 7239's `Forwarded: for=`
/// is deliberately not accepted: supporting one header is enough to make a
/// trusted reverse proxy work, and every operator needing it controls which
/// header their own proxy sends.
///
/// If `peer` (after [`unmap_ipv4`]) is not inside any `trusted_proxies`
/// range, this returns [`ClientAddr::Direct`] with `peer` unmapped and
/// never looks at `forwarded_for` — a header is only ever honored from a
/// configured, trusted proxy, so an arbitrary caller can never spoof its
/// address by sending one.
///
/// Otherwise, walks the concatenated list **right to left** — the entry
/// nearest this daemon was appended most recently by the nearest proxy; the
/// leftmost entry is whatever the original caller claimed and must never be
/// trusted on its own — skipping entries that themselves parse as a trusted
/// proxy (a chain of proxies forwarding through each other) and returning
/// [`ClientAddr::Forwarded`] with the first entry that isn't.
///
/// Normalization is deliberately narrow: an entry must parse as a bare
/// `IpAddr` (`unmap_ipv4`-normalized before the trust check). No `ip:port`
/// or `[ipv6]:port` stripping is attempted — accepting that shape would
/// require deciding how to handle every other non-bare form a proxy might
/// emit, and this module would rather fail resolution than guess. So the
/// *first* hop reached in the right-to-left walk that doesn't parse as a
/// bare `IpAddr` — whether it's the nearest proxy's own append or some
/// earlier, attacker-controlled hop the walk reaches while skipping trusted
/// proxies — makes the whole resolution fail, the same as an
/// entirely-trusted chain, an empty/missing header, or a non-UTF-8 header
/// line: [`ClientAddr::TrustedProxyUnresolved`] with `peer`. This is a
/// deliberate behavior change from treating a malformed hop as merely
/// absent and continuing the walk past it: skipping over it would let an
/// attacker who controls an earlier, unparsable hop force the walk onto a
/// *different* uncontrolled hop further left, rather than onto the
/// uniform, un-spoofable "limited under the proxy's own address" fallback
/// every other unresolvable case already gets.
pub fn resolve_client_addr(
    peer: IpAddr,
    trusted_proxies: &[(IpAddr, u8)],
    forwarded_for: Option<&[String]>,
) -> ClientAddr {
    let peer = unmap_ipv4(peer);
    if !is_trusted(peer, trusted_proxies) {
        return ClientAddr::Direct(peer);
    }
    let Some(forwarded_for) = forwarded_for else {
        return ClientAddr::TrustedProxyUnresolved(peer);
    };
    let entries = forwarded_for
        .iter()
        .flat_map(|line| line.split(','))
        .map(|s| s.trim());
    for raw in entries.collect::<Vec<_>>().into_iter().rev() {
        let Ok(candidate) = raw.parse::<IpAddr>() else {
            // The first unparsable hop this walk reaches ends resolution
            // outright rather than being skipped — see the doc comment
            // above for why continuing past it would be unsafe.
            return ClientAddr::TrustedProxyUnresolved(peer);
        };
        let candidate = unmap_ipv4(candidate);
        if !is_trusted(candidate, trusted_proxies) {
            return ClientAddr::Forwarded(candidate);
        }
    }
    ClientAddr::TrustedProxyUnresolved(peer)
}

#[cfg(test)]
mod tests;
