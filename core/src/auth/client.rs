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
}
