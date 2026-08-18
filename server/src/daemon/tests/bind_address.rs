//! `warn_if_unspecified`/`client_base_url`/`mcp_allowed_hosts` tests.

use std::net::SocketAddr;

use crate::daemon::{bind_tcp_listener, client_base_url, mcp_allowed_hosts, warn_if_unspecified};

// --- bind address warning ---

#[test]
fn warn_if_unspecified_does_not_panic_for_any_input() {
    warn_if_unspecified("127.0.0.1:7700".parse().unwrap());
    warn_if_unspecified("192.168.1.1:7700".parse().unwrap());
    warn_if_unspecified("0.0.0.0:7700".parse().unwrap());
    warn_if_unspecified("[::]:7700".parse().unwrap());
    warn_if_unspecified("[::1]:7700".parse().unwrap());
}

/// Pins the actual OS-resolution behavior the wildcard-alias fix depends on
/// (Codex review comment: string checks like `bind == "0.0.0.0"` miss aliases
/// such as `"0"`, `"[::]"`, `"000.000.000.000"` that the OS still resolves to
/// the unspecified address). Binding on the *actually returned* `SocketAddr`
/// rather than the config string is only a real fix if these forms truly
/// resolve to unspecified on the platforms we run on — this test binds each
/// one for real and checks `local_addr().ip().is_unspecified()`, instead of
/// just asserting the (already-known-correct) canonical `"0.0.0.0"` case.
#[tokio::test]
async fn wildcard_aliases_resolve_to_unspecified_when_actually_bound() {
    for alias in ["0", "[::]", "000.000.000.000"] {
        let (_listener, bound_addr) = bind_tcp_listener(alias, 0)
            .await
            .unwrap_or_else(|e| panic!("bind({alias:?}) should succeed: {e:?}"));
        assert!(
            bound_addr.ip().is_unspecified(),
            "bind alias {alias:?} resolved to {bound_addr}, expected an unspecified address"
        );
    }
}

// --- client_base_url ---

#[test]
fn client_base_url_substitutes_loopback_for_unspecified_v4() {
    let addr: SocketAddr = "0.0.0.0:7700".parse().unwrap();
    assert_eq!(client_base_url(addr), "http://127.0.0.1:7700");
}

#[test]
fn client_base_url_substitutes_loopback_for_unspecified_v6() {
    let addr: SocketAddr = "[::]:7700".parse().unwrap();
    assert_eq!(client_base_url(addr), "http://[::1]:7700");
}

#[test]
fn client_base_url_passes_through_specific_addresses() {
    assert_eq!(
        client_base_url("127.0.0.1:7700".parse().unwrap()),
        "http://127.0.0.1:7700"
    );
    assert_eq!(
        client_base_url("192.168.1.5:7700".parse().unwrap()),
        "http://192.168.1.5:7700"
    );
    assert_eq!(
        client_base_url("[::1]:7700".parse().unwrap()),
        "http://[::1]:7700"
    );
}

// --- mcp_allowed_hosts ---
//
// These pin the actual bug fix: rmcp's Streamable HTTP transport enforces
// its own DNS-rebinding `Host`-header allowlist, defaulting to
// localhost/127.0.0.1/::1 only — independent of, and narrower than, the
// daemon's own non-loopback-bind trust decision (PR #135). Without this
// function's fix, a deliberately-bound LAN/Tailscale address 403s every
// `/mcp` request with "Host header is not allowed", which MCP clients
// (e.g. Claude Code) surface as a spurious "needs authentication".

#[test]
fn mcp_allowed_hosts_includes_localhost_defaults_for_loopback_bind() {
    let hosts = mcp_allowed_hosts("127.0.0.1:7700".parse().unwrap());
    assert!(hosts.contains(&"localhost".to_string()));
    assert!(hosts.contains(&"127.0.0.1".to_string()));
    assert!(hosts.contains(&"::1".to_string()));
}

/// The actual bug: before this fix, only rmcp's localhost-only default
/// applied, so a deliberately-bound non-loopback address (e.g. a
/// Tailscale/LAN IP, here a TEST-NET-1 address per RFC 5737 — guaranteed
/// non-routable, so safe to use as a plain `SocketAddr` without binding
/// to it) would 403 on `/mcp` despite working on every other route.
#[test]
fn mcp_allowed_hosts_includes_the_specific_bind_address() {
    let hosts = mcp_allowed_hosts("192.0.2.1:7700".parse().unwrap());
    assert!(
        hosts.contains(&"192.0.2.1".to_string()),
        "expected the bind address itself to be allow-listed, got: {hosts:?}"
    );
    // Local access must keep working too — `with_allowed_hosts` replaces
    // rmcp's default list rather than extending it, so the defaults must
    // still be present alongside the bind-specific host.
    assert!(hosts.contains(&"localhost".to_string()));
    assert!(hosts.contains(&"127.0.0.1".to_string()));
    assert!(hosts.contains(&"::1".to_string()));
}

#[test]
fn mcp_allowed_hosts_disables_the_check_for_wildcard_binds() {
    assert_eq!(
        mcp_allowed_hosts("0.0.0.0:7700".parse().unwrap()),
        Vec::<String>::new()
    );
    assert_eq!(
        mcp_allowed_hosts("[::]:7700".parse().unwrap()),
        Vec::<String>::new()
    );
}
