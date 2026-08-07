//! Destination policy for the public-only HTTP client (SSRF guard).
//!
//! Motivation: the feed connector is the first place in localdb where a URL
//! chosen by a *third party* (an entry `<link>` inside a feed document) is
//! handed to an HTTP client. Everything before it — `url` sources, the feed
//! URL itself — is operator-configured. That new trust boundary is what this
//! module guards: a feed publisher must not be able to make localdb fetch
//! `http://169.254.169.254/latest/meta-data/` (or a homelab admin panel) and
//! then index the response into a searchable store.
//!
//! Three layers, each load-bearing — none is defence-in-depth for another:
//!
//! 1. [`GuardedResolver`] (a [`reqwest::dns::Resolve`] impl) covers every
//!    *hostname*, on the initial request and on every redirect hop. It filters
//!    the resolved address list before reqwest ever connects, which is also
//!    what defeats DNS rebinding: reqwest connects to exactly the addresses
//!    the resolver returned, so there is no resolve-then-reresolve window.
//!    Never "resolve, validate, then let the default resolver run again" —
//!    there is no post-connect hook to validate against.
//! 2. A **preflight** check on the request URL's host, performed by the
//!    fetcher before `send()`. This is mandatory, not belt-and-braces:
//!    hyper-util's HTTP connector tries `SocketAddrs::try_parse(host, port)`
//!    *first* and only falls through to the custom resolver when the host is
//!    not an IP literal (`hyper-util/src/client/legacy/connect/http.rs`), so a
//!    literal like `http://127.0.0.1/` never reaches layer 1 at all.
//! 3. [`guarded_redirect_policy`] covers IP-literal *redirect targets* (name
//!    targets go through layer 1). Because `redirect::Policy::custom`
//!    *replaces* reqwest's default `Policy::limited(10)` rather than wrapping
//!    it, this policy re-implements the 10-hop cap itself.
//!
//! Both layers 2 and 3 inspect the host through [`reqwest::Url`], never a raw
//! string. That is deliberate: the `url` crate applies WHATWG host parsing, so
//! `http://2130706433/` and `http://0x7f.1/` both normalize to the host
//! `127.0.0.1`. A hand-rolled string check would miss exactly the obfuscated
//! literals this guard exists to catch (see `url_host_normalization` tests).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// A destination was refused because it is not globally routable.
///
/// Surfaces two ways: returned directly by the preflight check, or boxed into
/// a `reqwest::Error`'s source chain by [`GuardedResolver`] /
/// [`guarded_redirect_policy`]. [`is_blocked_error`] recovers the latter.
#[derive(Debug)]
pub(crate) struct BlockedDestinationError {
    host: String,
}

impl fmt::Display for BlockedDestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "destination '{}' is not globally routable; refusing to connect",
            self.host
        )
    }
}

impl std::error::Error for BlockedDestinationError {}

// ---------------------------------------------------------------------------
// The predicate
// ---------------------------------------------------------------------------

/// Whether `ip` must never be connected to by the public-only client.
///
/// Hand-rolled rather than pulled from `ipnet`: stdlib already answers most of
/// it (`is_loopback`, `is_private`, `is_link_local`, `is_documentation`,
/// `is_multicast`, `is_broadcast`, `is_unspecified`), leaving four IPv4 and
/// four IPv6 ranges that are one octet/segment comparison each. `IpAddr::
/// is_global` would collapse the whole thing, but it is still unstable on the
/// project's Rust 1.85 floor. For a security predicate, a small audited
/// surface with an exhaustive test table beats a new dependency.
pub(crate) fn is_blocked_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        // The critical branch: an IPv4-mapped v6 address (`::ffff:a.b.c.d`)
        // must be re-checked under the *v4* rules. Without this,
        // `::ffff:169.254.169.254` walks straight past every IPv4 range check
        // and is treated as an ordinary global v6 address.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => is_blocked_v4(mapped),
            None => is_blocked_v6(v6),
        },
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()          // 127.0.0.0/8
        || ip.is_private()    // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254.0.0/16 — the cloud metadata endpoint
        || ip.is_multicast()  // 224.0.0.0/4
        || ip.is_broadcast()  // 255.255.255.255
        || ip.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || o[0] == 0                     // 0.0.0.0/8 ("this network"), incl. unspecified
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || o[0] >= 240 // 240.0.0.0/4 reserved
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()                  // ff00::/8
        || (s[0] & 0xfe00) == 0xfc00          // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80          // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
        // ::/96 — the deprecated IPv4-compatible range. `to_ipv4_mapped`
        // returns `None` for these (it only recognises ::ffff:0:0/96), so
        // without this arm `::127.0.0.1` would be judged a global v6 address.
        || s[..6].iter().all(|seg| *seg == 0)
}

/// Extract the host of `url` as an IP address, if it is an IP literal.
///
/// Reads `Url::host_str`, which is the *normalized* host the `url` crate
/// produced at parse time — `http://2130706433/` yields `"127.0.0.1"` here,
/// not `"2130706433"`. IPv6 literals are serialized bracketed (`"[::1]"`), so
/// the brackets are stripped before parsing. A domain name never parses as an
/// `IpAddr`, so `Some` ⟺ "this URL names an IP literal".
pub(crate) fn ip_literal_host(url: &reqwest::Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<IpAddr>().ok()
}

/// Drop every blocked address from a resolver result.
///
/// A mixed result (one public address, one private) keeps only the public
/// ones — that is the point of filtering rather than rejecting outright.
/// `Err` is returned only when *every* resolved address was blocked, which is
/// what a rebinding-style answer or a plain internal hostname looks like.
pub(crate) fn filter_allowed_addrs(
    addrs: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, BlockedDestinationError> {
    let mut allowed = Vec::new();
    let mut blocked: Vec<IpAddr> = Vec::new();
    for addr in addrs {
        if is_blocked_destination(addr.ip()) {
            blocked.push(addr.ip());
        } else {
            allowed.push(addr);
        }
    }
    if allowed.is_empty() && !blocked.is_empty() {
        return Err(BlockedDestinationError {
            host: blocked
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    Ok(allowed)
}

// ---------------------------------------------------------------------------
// Layer 1 — the resolver
// ---------------------------------------------------------------------------

/// A [`reqwest::dns::Resolve`] that resolves through `tokio::net::lookup_host`
/// and then drops blocked addresses.
///
/// `Resolve` has no "delegate to the default resolver" hook, so this impl must
/// do the lookup itself. Port `0` is passed to `lookup_host`; reqwest replaces
/// it with the URL's port (or the scheme's default) after resolution.
pub(crate) struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            let allowed = filter_allowed_addrs(resolved).map_err(
                |_| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(BlockedDestinationError { host })
                },
            )?;
            Ok(Box::new(allowed.into_iter()) as Addrs)
        }) as Pin<Box<_>>
    }
}

// ---------------------------------------------------------------------------
// Layer 3 — the redirect policy
// ---------------------------------------------------------------------------

/// Reqwest's default hop cap. `Policy::custom` replaces the default policy
/// outright, so the cap has to be restated here (see `Policy::redirect`'s
/// `Limit` arm: `previous` includes the originally requested URL, hence `>`).
const MAX_REDIRECTS: usize = 10;

/// Redirect policy that refuses hops to blocked IP literals and keeps
/// reqwest's default 10-hop cap.
///
/// Hostname hops need no check here — they go through [`GuardedResolver`]
/// when the connection is made.
pub(crate) fn guarded_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let blocked_host = ip_literal_host(attempt.url())
            .filter(|ip| is_blocked_destination(*ip))
            .map(|ip| ip.to_string());
        if let Some(host) = blocked_host {
            return attempt.error(BlockedDestinationError { host });
        }
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.stop();
        }
        attempt.follow()
    })
}

// ---------------------------------------------------------------------------
// Recovering the rejection from a reqwest error
// ---------------------------------------------------------------------------

/// Whether `err` was caused by this module refusing a destination.
///
/// Layers 1 and 3 can only signal through an opaque boxed error that reqwest
/// wraps, so the only way back is to walk `source()` looking for our marker
/// type. If reqwest ever stops preserving the chain this returns `false` and
/// the caller degrades to its ordinary transient-error path — the connection
/// still never happens, only the "fall back to the feed entry's own summary"
/// nicety is lost.
pub(crate) fn is_blocked_error(err: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.downcast_ref::<BlockedDestinationError>().is_some() {
            return true;
        }
        source = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn blocked(s: &str) -> bool {
        is_blocked_destination(IpAddr::from_str(s).expect("test address must parse"))
    }

    #[test]
    fn blocked_ipv4_ranges() {
        for addr in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata — the canonical SSRF target
            "100.64.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(blocked(addr), "{addr} must be blocked");
        }
    }

    #[test]
    fn blocked_ipv6_ranges() {
        for addr in ["::", "::1", "fc00::1", "fe80::1", "2001:db8::1", "ff02::1"] {
            assert!(blocked(addr), "{addr} must be blocked");
        }
    }

    #[test]
    fn allowed_addresses_prove_ranges_are_not_over_wide() {
        for addr in [
            "100.63.255.255", // just below CGNAT 100.64/10
            "100.128.0.1",    // just above CGNAT 100.64/10
            "198.17.255.255", // just below benchmarking 198.18/15
            "198.20.0.1",     // just above benchmarking 198.18/15
            "8.8.8.8",
            "2606:4700:4700::1111",
        ] {
            assert!(!blocked(addr), "{addr} must be allowed");
        }
    }

    #[test]
    fn ipv4_mapped_v6_is_checked_under_v4_rules() {
        assert!(blocked("::ffff:127.0.0.1"));
        assert!(
            blocked("::ffff:169.254.169.254"),
            "the mapped-v6 form of the metadata endpoint must not bypass the v4 checks"
        );
        assert!(!blocked("::ffff:8.8.8.8"));
    }

    #[test]
    fn ipv4_compatible_v6_is_blocked() {
        // `::a.b.c.d` is not recognised by `to_ipv4_mapped`, so it needs its
        // own arm rather than falling through as "some global v6 address".
        assert!(blocked("::127.0.0.1"));
        assert!(blocked("::8.8.8.8"));
    }

    // -----------------------------------------------------------------------
    // URL normalization — the assumption layers 2 and 3 rest on
    // -----------------------------------------------------------------------

    #[test]
    fn url_host_normalization_defeats_obfuscated_literals() {
        for raw in ["http://2130706433/", "http://0x7f.1/", "http://127.1/"] {
            let url = reqwest::Url::parse(raw).expect("must parse");
            assert_eq!(
                url.host_str(),
                Some("127.0.0.1"),
                "{raw} must normalize to 127.0.0.1"
            );
            let ip = ip_literal_host(&url).expect("must be recognised as an IP literal");
            assert!(is_blocked_destination(ip), "{raw} must be blocked");
        }
    }

    #[test]
    fn ip_literal_host_unwraps_bracketed_ipv6() {
        let url = reqwest::Url::parse("http://[::1]:8080/x").expect("must parse");
        assert_eq!(
            ip_literal_host(&url),
            Some(IpAddr::from_str("::1").unwrap())
        );
    }

    #[test]
    fn ip_literal_host_is_none_for_domains() {
        let url = reqwest::Url::parse("https://example.com/x").expect("must parse");
        assert!(ip_literal_host(&url).is_none());
    }

    // -----------------------------------------------------------------------
    // filter_allowed_addrs
    // -----------------------------------------------------------------------

    #[test]
    fn filter_keeps_public_addresses_from_a_mixed_answer() {
        let addrs = vec![
            SocketAddr::from(([127, 0, 0, 1], 80)),
            SocketAddr::from(([8, 8, 8, 8], 80)),
        ];
        let kept = filter_allowed_addrs(addrs).expect("a public address survives");
        assert_eq!(kept, vec![SocketAddr::from(([8, 8, 8, 8], 80))]);
    }

    #[test]
    fn filter_errors_when_every_address_is_blocked() {
        let addrs = vec![SocketAddr::from(([169, 254, 169, 254], 80))];
        assert!(filter_allowed_addrs(addrs).is_err());
    }

    #[test]
    fn filter_passes_an_empty_answer_through() {
        // NXDOMAIN-shaped: nothing was blocked, so this is not our error to
        // report — hyper will fail with "no addresses" on its own.
        assert_eq!(filter_allowed_addrs(Vec::new()).unwrap(), Vec::new());
    }

    #[test]
    fn blocked_destination_error_is_recovered_from_a_wrapped_source_chain() {
        #[derive(Debug)]
        struct Wrapper(Box<dyn std::error::Error + Send + Sync>);
        impl fmt::Display for Wrapper {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "wrapper")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self.0.as_ref())
            }
        }

        let inner = BlockedDestinationError {
            host: "127.0.0.1".to_string(),
        };
        let wrapped = Wrapper(Box::new(inner));
        // Same walk `is_blocked_error` performs, exercised without needing a
        // real `reqwest::Error` (which cannot be constructed outside reqwest).
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&wrapped);
        let mut found = false;
        while let Some(e) = source {
            if e.downcast_ref::<BlockedDestinationError>().is_some() {
                found = true;
                break;
            }
            source = e.source();
        }
        assert!(found);
    }
}
