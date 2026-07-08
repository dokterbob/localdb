//! OAuth client recognition and redirect-URI validation (T4,
//! specs/05-surfaces.md §3.1).
//!
//! Domain logic per specs/01-architecture.md §1: client registry lookup and
//! redirect-uri validation policy live here (pure, I/O-free), not in
//! `server/src/auth/oauth.rs` (HTTP shape only) or `store-libsql`
//! (persistence only).
//!
//! T4 recognizes exactly one client: the built-in `localdb-cli` public
//! client, seeded here as pure recognition logic — no `oauth_clients` DB row
//! is required for it. The `oauth_clients` table exists in the schema
//! already (T1, D13) for T7's dynamic client registration (`POST
//! /register`), which will extend `is_known_client`/`validate_redirect_uri`
//! with a store-backed lookup for registered clients alongside this
//! built-in one.

/// The built-in CLI client ID (`localdb login`), always recognized.
pub const LOCALDB_CLI_CLIENT_ID: &str = "localdb-cli";

/// The `urn:ietf:wg:oauth:2.0:oob` "out of band" redirect sentinel: the
/// `localdb-cli` no-browser fallback (`localdb login --no-browser`) uses
/// this instead of a loopback URL when it cannot host a callback listener
/// story locally is preferred but a listener is unavailable, or the caller
/// explicitly asked to skip opening a browser. `server/src/auth/oauth.rs`
/// recognizes it specially: instead of a 302 redirect, the consent page
/// renders the resulting code for the user to copy/paste, which
/// `localdb login --no-browser` then reads back from stdin.
pub const OOB_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

/// Is `client_id` a client this server recognizes?
///
/// T4 recognizes exactly one: the built-in `localdb-cli` public client.
/// T7's dynamic client registration will extend this with a store-backed
/// lookup against `oauth_clients`.
pub fn is_known_client(client_id: &str) -> bool {
    client_id == LOCALDB_CLI_CLIENT_ID
}

/// Validate `redirect_uri` against `client_id`'s registered redirect policy
/// (R5, specs/05-surfaces.md §3.1).
///
/// `localdb-cli` gets the RFC 8252 §7.3 loopback exception: any
/// `http://127.0.0.1:<port>/<path>` or `http://localhost:<port>/<path>`
/// (port required, any value; path optional, any value) is accepted — the
/// CLI binds a fresh ephemeral port per login attempt, so the exact port
/// can't be pre-registered. It also accepts the literal
/// [`OOB_REDIRECT_URI`] sentinel for the no-browser fallback. Every other
/// `client_id` is rejected in this ticket — there is no registered-client
/// redirect-uri store yet (T7 adds one for `POST /register`-created
/// clients).
pub fn validate_redirect_uri(client_id: &str, redirect_uri: &str) -> bool {
    if client_id != LOCALDB_CLI_CLIENT_ID {
        return false;
    }
    redirect_uri == OOB_REDIRECT_URI || is_loopback_redirect(redirect_uri)
}

/// Validate a redirect_uri presented at Dynamic Client Registration time
/// (RFC 7591, T7, specs/05-surfaces.md §3.1 work item 4).
///
/// Accepts exactly two shapes: an `https://` URL with a non-empty host, or
/// the same loopback pattern `localdb-cli` gets (`http://127.0.0.1[:port]/...`
/// / `http://localhost[:port]/...`) — reused via [`is_loopback_redirect`].
/// Everything else, including custom URI schemes (`myapp://...`) some native
/// apps use per RFC 8252 §7.1, is rejected.
///
/// **Decision (T7):** custom schemes are deliberately out of scope. RFC 8252's
/// loopback-interface exception already covers the CLI/native-app redirect
/// case (and is what the built-in `localdb-cli` client already uses); there is
/// no evidence a stock MCP client needs a private-use URI scheme to complete
/// DCR against a localdb daemon, and accepting one would require registering
/// (and trusting the registrant's claim to) a scheme with no verification
/// that the calling process actually owns it. If an MCP ecosystem client
/// later needs one, extend this function then — the caller
/// (`AuthService::register_client`) validates every `redirect_uris` entry
/// against this single predicate, so relaxing it is a one-place change.
pub fn validate_registration_redirect_uri(redirect_uri: &str) -> bool {
    if let Some(rest) = redirect_uri.strip_prefix("https://") {
        if rest.is_empty() {
            return false;
        }
        return url::Url::parse(redirect_uri)
            .map(|u| u.scheme() == "https" && u.host_str().is_some())
            .unwrap_or(false);
    }
    is_loopback_redirect(redirect_uri)
}

/// Grant types this authorization server supports (RFC 8414
/// `grant_types_supported`; RFC 7591 §2 `grant_types`). Single source of
/// truth for both `server::handlers::discovery`'s `.well-known` document and
/// `server::auth::register`'s DCR validation (finding #3) — this AS only
/// ever supports the authorization-code + refresh-token grants (T4/T7); a
/// registrant asking for anything else (e.g. `client_credentials`) would get
/// a 201 advertising a flow `/token` can't actually service.
pub const SUPPORTED_GRANT_TYPES: &[&str] = &["authorization_code", "refresh_token"];

/// Response types this authorization server supports (RFC 8414
/// `response_types_supported`; RFC 7591 §2 `response_types`) — only the
/// authorization-code flow (`code`); this server never issues an implicit-flow
/// token directly from `/authorize`.
pub const SUPPORTED_RESPONSE_TYPES: &[&str] = &["code"];

/// DCR bound policy (finding #8, specs/05-surfaces.md §3.1's DCR note): a
/// public, unauthenticated `POST /register` must not let a single request
/// grow `oauth_clients` metadata without limit. These are minimal,
/// proportionate per-request caps, not a rate limiter — they bound how much
/// a single registration can store, not how often a caller may register.
/// A global registration-count cap / rate limit is a separate, out-of-scope
/// concern (tracked as a `// TODO` at the call site in
/// `server/src/auth/register.rs`).
///
/// At most this many `redirect_uris` entries per registration.
pub const MAX_REGISTRATION_REDIRECT_URIS: usize = 5;
/// Each `redirect_uris` entry must be at most this many characters.
pub const MAX_REGISTRATION_REDIRECT_URI_LEN: usize = 2048;
/// `client_name`, if present, must be at most this many characters.
pub const MAX_REGISTRATION_CLIENT_NAME_LEN: usize = 256;

/// Whether `redirect_uris` is within the DCR count and per-URI length caps
/// ([`MAX_REGISTRATION_REDIRECT_URIS`] / [`MAX_REGISTRATION_REDIRECT_URI_LEN`]).
/// Purely a size check — shape/scheme validity is
/// [`validate_registration_redirect_uri`]'s job.
pub fn registration_redirect_uris_within_bounds(redirect_uris: &[String]) -> bool {
    redirect_uris.len() <= MAX_REGISTRATION_REDIRECT_URIS
        && redirect_uris
            .iter()
            .all(|uri| uri.len() <= MAX_REGISTRATION_REDIRECT_URI_LEN)
}

/// Whether `client_name` (if present) is within the DCR length cap
/// ([`MAX_REGISTRATION_CLIENT_NAME_LEN`]).
pub fn registration_client_name_within_bounds(client_name: Option<&str>) -> bool {
    client_name
        .map(|name| name.len() <= MAX_REGISTRATION_CLIENT_NAME_LEN)
        .unwrap_or(true)
}

/// `http://127.0.0.1:<port>/<anything>` or `http://localhost:<port>/<anything>`,
/// port required and numeric, path optional.
fn is_loopback_redirect(redirect_uri: &str) -> bool {
    let Some(rest) = redirect_uri.strip_prefix("http://") else {
        return false;
    };
    let authority = match rest.find('/') {
        Some(idx) => &rest[..idx],
        None => rest,
    };
    let Some((host, port)) = authority.split_once(':') else {
        return false;
    };
    if host != "127.0.0.1" && host != "localhost" {
        return false;
    }
    !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_client_is_localdb_cli_only() {
        assert!(is_known_client(LOCALDB_CLI_CLIENT_ID));
        assert!(!is_known_client("some-other-client"));
        assert!(!is_known_client(""));
    }

    #[test]
    fn loopback_v4_with_path_is_valid_for_localdb_cli() {
        assert!(validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1:54321/callback"
        ));
    }

    #[test]
    fn loopback_localhost_with_path_is_valid_for_localdb_cli() {
        assert!(validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://localhost:8080/cb"
        ));
    }

    #[test]
    fn loopback_without_path_is_valid() {
        assert!(validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1:1234"
        ));
    }

    #[test]
    fn any_port_is_accepted() {
        assert!(validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1:1/x"
        ));
        assert!(validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1:65535/x"
        ));
    }

    #[test]
    fn oob_sentinel_is_valid_for_localdb_cli() {
        assert!(validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            OOB_REDIRECT_URI
        ));
    }

    #[test]
    fn non_loopback_host_is_rejected() {
        assert!(!validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://example.com:8080/callback"
        ));
        assert!(!validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://evil.com:80/callback"
        ));
    }

    #[test]
    fn https_loopback_is_rejected() {
        // The CLI's own ephemeral listener is plain HTTP; https loopback is
        // not part of the RFC 8252 exception this function implements.
        assert!(!validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "https://127.0.0.1:8080/callback"
        ));
    }

    #[test]
    fn missing_port_is_rejected() {
        assert!(!validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1/callback"
        ));
    }

    #[test]
    fn unknown_client_is_always_rejected_regardless_of_uri() {
        assert!(!validate_redirect_uri(
            "some-other-client",
            "http://127.0.0.1:1234/callback"
        ));
    }

    #[test]
    fn host_substring_lookalikes_are_rejected() {
        assert!(!validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1.evil.com:1234/callback"
        ));
        assert!(!validate_redirect_uri(
            LOCALDB_CLI_CLIENT_ID,
            "http://notlocalhost:1234/callback"
        ));
    }

    // -----------------------------------------------------------------
    // T7: DCR redirect_uri validation
    // -----------------------------------------------------------------

    #[test]
    fn registration_accepts_https_url() {
        assert!(validate_registration_redirect_uri(
            "https://app.example.com/oauth/callback"
        ));
    }

    #[test]
    fn registration_accepts_loopback_http() {
        assert!(validate_registration_redirect_uri(
            "http://127.0.0.1:51234/callback"
        ));
        assert!(validate_registration_redirect_uri(
            "http://localhost:8080/cb"
        ));
    }

    #[test]
    fn registration_rejects_plain_http_non_loopback() {
        assert!(!validate_registration_redirect_uri(
            "http://app.example.com/callback"
        ));
    }

    #[test]
    fn registration_rejects_custom_scheme() {
        assert!(!validate_registration_redirect_uri(
            "myapp://oauth/callback"
        ));
        assert!(!validate_registration_redirect_uri("cursor://callback"));
    }

    #[test]
    fn registration_rejects_empty_https_host() {
        assert!(!validate_registration_redirect_uri("https://"));
    }

    #[test]
    fn registration_rejects_oob_sentinel() {
        // The OOB sentinel is a localdb-cli-only fallback, not a valid
        // registration redirect for a general public client.
        assert!(!validate_registration_redirect_uri(OOB_REDIRECT_URI));
    }
}
