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
