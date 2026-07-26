//! Base-URL resolution for OAuth discovery (T7, RFC 9728 §3.1 / RFC 8414):
//! the issuer/resource identifier used by the `.well-known` endpoints and by
//! the `WWW-Authenticate: Bearer resource_metadata="..."` challenge on 401s.
//!
//! **Trust note:** the `Host` header is attacker-influencable on every route
//! this feeds (the `.well-known` endpoints and the 401 challenge are both
//! reachable without a bearer token, by design — specs/05-surfaces.md §3.1).
//! [`resolve_base_url`] therefore prefers the operator-configured
//! `server.public_url` when set (never derived from request input), and only
//! falls back to the request's own `Host` header — after strict sanitization
//! via [`sanitize_host_header`] — when no `public_url` is configured. A
//! hostile or malformed `Host` header (embedded scheme, path, userinfo,
//! whitespace, control characters) is rejected outright rather than echoed
//! back in any form; callers treat a `None` result as "cannot resolve a safe
//! base for this request" (a `400 invalid_request` for the `.well-known`
//! handlers, or "omit the challenge parameter" for the 401 case — see
//! `server::auth::middleware`).

/// Resolve the daemon's client-reachable base URL (no trailing slash).
///
/// Prefers `public_url` (trimmed of any trailing `/`) when configured —
/// this is the only correct choice behind a TLS-terminating reverse proxy,
/// since the proxy's externally-visible host/scheme has no other way to
/// reach this code (specs/03-config.md §1). Falls back to
/// `http://<sanitized Host header>` otherwise. Returns `None` if neither is
/// available (no `public_url` and a missing/invalid `Host` header) — the
/// caller decides how to fail in that case.
pub fn resolve_base_url(public_url: Option<&str>, host_header: Option<&str>) -> Option<String> {
    if let Some(configured) = public_url {
        let trimmed = configured.trim_end_matches('/');
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let host = host_header?;
    let sanitized = sanitize_host_header(host)?;
    Some(format!("http://{sanitized}"))
}

/// Validate that `raw` is exactly a `host[:port]` authority — no scheme, no
/// userinfo, no path, no query, no fragment, no whitespace/control
/// characters — and return it normalized (as `host` or `host:port`).
///
/// Implemented by probing `url::Url::parse("http://{raw}")` and rejecting
/// anything that doesn't round-trip to a bare authority; this reuses the
/// `url` crate's own authority parser rather than hand-rolling one, while
/// the upfront character checks below reject the cases a bare
/// `Url::parse` would otherwise silently "fix up" (e.g. stray whitespace).
pub fn sanitize_host_header(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
    {
        return None;
    }
    if raw.contains('?') || raw.contains('#') || raw.contains('@') {
        return None;
    }
    // Reject an embedded scheme (e.g. a header value of "http://evil.com" —
    // which would otherwise parse "cleanly" as a host named "http:").
    if raw.contains("://") {
        return None;
    }

    let probe = format!("http://{raw}");
    let parsed = url::Url::parse(&probe).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return None;
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return None;
    }
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_takes_priority_and_strips_trailing_slash() {
        assert_eq!(
            resolve_base_url(Some("https://localdb.example.com/"), Some("127.0.0.1:7700")),
            Some("https://localdb.example.com".to_string())
        );
    }

    #[test]
    fn public_url_without_trailing_slash_is_used_as_is() {
        assert_eq!(
            resolve_base_url(Some("https://localdb.example.com"), None),
            Some("https://localdb.example.com".to_string())
        );
    }

    #[test]
    fn falls_back_to_sanitized_host_header_when_no_public_url() {
        assert_eq!(
            resolve_base_url(None, Some("127.0.0.1:7700")),
            Some("http://127.0.0.1:7700".to_string())
        );
    }

    #[test]
    fn falls_back_to_host_header_without_port() {
        assert_eq!(
            resolve_base_url(None, Some("localdb.example.com")),
            Some("http://localdb.example.com".to_string())
        );
    }

    #[test]
    fn none_when_no_public_url_and_no_host_header() {
        assert_eq!(resolve_base_url(None, None), None);
    }

    #[test]
    fn none_when_no_public_url_and_hostile_host_header() {
        assert_eq!(resolve_base_url(None, Some("evil.com/path")), None);
    }

    #[test]
    fn public_url_configured_ignores_a_hostile_host_header() {
        // The Host header is attacker-influencable; when public_url is
        // configured it must never even be consulted.
        assert_eq!(
            resolve_base_url(
                Some("https://localdb.example.com"),
                Some("evil.com/path?x=1")
            ),
            Some("https://localdb.example.com".to_string())
        );
    }

    // --- sanitize_host_header ---

    #[test]
    fn sanitize_accepts_plain_host_and_port() {
        assert_eq!(
            sanitize_host_header("127.0.0.1:7700"),
            Some("127.0.0.1:7700".to_string())
        );
        assert_eq!(
            sanitize_host_header("localdb.example.com"),
            Some("localdb.example.com".to_string())
        );
    }

    #[test]
    fn sanitize_rejects_embedded_path() {
        assert_eq!(sanitize_host_header("evil.com/../../etc/passwd"), None);
        assert_eq!(sanitize_host_header("evil.com/x"), None);
    }

    #[test]
    fn sanitize_rejects_embedded_scheme() {
        assert_eq!(sanitize_host_header("http://evil.com"), None);
        assert_eq!(sanitize_host_header("https://evil.com"), None);
    }

    #[test]
    fn sanitize_rejects_userinfo() {
        assert_eq!(sanitize_host_header("user:pass@evil.com"), None);
        assert_eq!(sanitize_host_header("evil.com@127.0.0.1"), None);
    }

    #[test]
    fn sanitize_rejects_embedded_whitespace_and_control_chars() {
        // Leading/trailing OWS is already stripped by the HTTP layer before
        // this ever runs (and `.trim()` mirrors that here); what must be
        // rejected is whitespace/control characters *embedded* in the
        // header — e.g. a header-injection attempt.
        assert!(sanitize_host_header("evil.com").is_some());
        assert_eq!(sanitize_host_header("evil.com another"), None);
        assert_eq!(sanitize_host_header("evil.com\r\nX-Injected: 1"), None);
        assert_eq!(sanitize_host_header("evil\t.com"), None);
    }

    #[test]
    fn sanitize_rejects_query_and_fragment() {
        assert_eq!(sanitize_host_header("evil.com?x=1"), None);
        assert_eq!(sanitize_host_header("evil.com#frag"), None);
    }

    #[test]
    fn sanitize_rejects_empty_or_blank() {
        assert_eq!(sanitize_host_header(""), None);
        assert_eq!(sanitize_host_header("   "), None);
    }
}
