//! OAuth client recognition and redirect-URI validation
//! (specs/05-surfaces.md §3.1).
//!
//! Domain logic per specs/01-architecture.md §1: client registry lookup and
//! redirect-uri validation policy live here (pure, I/O-free), not in
//! `server/src/auth/oauth.rs` (HTTP shape only) or `store-libsql`
//! (persistence only).
//!
//! `is_known_client`/`validate_redirect_uri` here recognize exactly one
//! client: the built-in `localdb-cli` public client, as pure recognition
//! logic — no `oauth_clients` DB row is required for it. Dynamically
//! registered clients (`POST /register`) are a separate, store-backed
//! lookup against the `oauth_clients` table, layered on top at
//! `AuthService::is_known_client`/`validate_client_redirect_uri`.

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
/// Recognizes exactly one: the built-in `localdb-cli` public client. A
/// dynamically registered client is recognized separately, via a
/// store-backed lookup against `oauth_clients` at
/// `AuthService::is_known_client`.
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
/// `client_id` is rejected here; a registered client (`POST /register`) is
/// validated separately, against its own stored `redirect_uris`, at
/// `AuthService::validate_client_redirect_uri`.
pub fn validate_redirect_uri(client_id: &str, redirect_uri: &str) -> bool {
    if client_id != LOCALDB_CLI_CLIENT_ID {
        return false;
    }
    redirect_uri == OOB_REDIRECT_URI || is_loopback_redirect(redirect_uri)
}

/// Validate a redirect_uri presented at Dynamic Client Registration time
/// (RFC 7591, specs/05-surfaces.md §3.1 work item 4).
///
/// Accepts exactly two shapes: an `https://` URL with a non-empty host, or
/// the same loopback pattern `localdb-cli` gets (`http://127.0.0.1[:port]/...`
/// / `http://localhost[:port]/...`) — reused via [`is_loopback_redirect`].
/// Everything else, including custom URI schemes (`myapp://...`) some native
/// apps use per RFC 8252 §7.1, is rejected.
///
/// Custom URI schemes are deliberately out of scope. RFC 8252's
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
/// `server::auth::register`'s DCR validation — this AS only ever supports
/// the authorization-code + refresh-token grants; a registrant asking for
/// anything else (e.g. `client_credentials`) would get a 201 advertising a
/// flow `/token` can't actually service.
pub const SUPPORTED_GRANT_TYPES: &[&str] = &["authorization_code", "refresh_token"];

/// Response types this authorization server supports (RFC 8414
/// `response_types_supported`; RFC 7591 §2 `response_types`) — only the
/// authorization-code flow (`code`); this server never issues an implicit-flow
/// token directly from `/authorize`.
pub const SUPPORTED_RESPONSE_TYPES: &[&str] = &["code"];

/// DCR bound policy (specs/05-surfaces.md §3.1's DCR note): a public,
/// unauthenticated `POST /register` must not let a single request
/// grow `oauth_clients` metadata without limit. These are minimal,
/// proportionate per-request caps — they bound how much a single
/// registration can store, not how often a caller may register. The
/// how-often concern is [`RegistrationPolicy`], enforced by
/// `AuthService::register_client` via `AuthStore::prune_and_create_oauth_client`.
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

/// How-often abuse-limit policy for `POST /register` (specs/05-surfaces.md
/// §3.1), applied by `AuthService::register_client` before any row is
/// persisted. Distinct from the per-request `MAX_REGISTRATION_*` caps
/// above: those bound a single request's payload size; this bounds how many
/// dynamically registered clients may accumulate over time. Sourced from
/// `server.abuse_limits` config (`core::config::schema::AbuseLimitsConfig`);
/// kept as a plain struct here rather than importing that type, so `core`
/// stays free of any dependency direction on `server`'s config module.
#[derive(Debug, Clone, Copy)]
pub struct RegistrationPolicy {
    /// Maximum number of dynamically registered clients kept at once. The
    /// built-in [`LOCALDB_CLI_CLIENT_ID`] client never counts toward this
    /// and is never pruned.
    pub max_dynamic_clients: u64,
    /// How long a dynamically registered client may sit unused — no
    /// authorization code ever issued to it — before it becomes eligible
    /// for pruning at the next registration attempt.
    pub unused_client_ttl_secs: i64,
}

/// A `POST /register` request that fails this server's DCR validation policy
/// — the single typed outcome [`validate_registration_request`] returns,
/// enforced by `AuthService::register_client`. `server::auth::register::
/// post_register` performs no validation of its own: it only maps each
/// variant onto its RFC 7591 §3.2.2 error code (`invalid_redirect_uri` for
/// [`Self::InvalidRedirectUri`], `invalid_client_metadata` for everything
/// else).
///
/// Variant order matches [`validate_registration_request`]'s check order —
/// the precedence a client observes when a request violates more than one
/// rule at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    /// `token_endpoint_auth_method` was present and not `"none"` — this
    /// server only ever registers public clients (there is nowhere to store
    /// or verify a `client_secret`).
    UnsupportedAuthMethod(String),
    /// `redirect_uris` was empty.
    EmptyRedirectUris,
    /// `redirect_uris` has more than [`MAX_REGISTRATION_REDIRECT_URIS`]
    /// entries, or one entry exceeds [`MAX_REGISTRATION_REDIRECT_URI_LEN`]
    /// characters.
    TooManyOrOverlongRedirectUris,
    /// `client_name` exceeds [`MAX_REGISTRATION_CLIENT_NAME_LEN`] characters.
    ClientNameTooLong,
    /// A `redirect_uris` entry is not an allowed shape — see
    /// [`validate_registration_redirect_uri`]'s doc comment for which shapes
    /// are allowed and why.
    InvalidRedirectUri(String),
    /// A `grant_types` entry this authorization server doesn't support (see
    /// [`SUPPORTED_GRANT_TYPES`]).
    UnsupportedGrantType(String),
    /// A `response_types` entry this authorization server doesn't support
    /// (see [`SUPPORTED_RESPONSE_TYPES`]).
    UnsupportedResponseType(String),
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAuthMethod(method) => write!(
                f,
                "token_endpoint_auth_method '{method}' is not supported; this server only \
                 registers public clients (token_endpoint_auth_method: 'none')"
            ),
            Self::EmptyRedirectUris => {
                write!(f, "redirect_uris is required and must not be empty")
            }
            Self::TooManyOrOverlongRedirectUris => write!(
                f,
                "redirect_uris must contain at most {MAX_REGISTRATION_REDIRECT_URIS} entries, \
                 each at most {MAX_REGISTRATION_REDIRECT_URI_LEN} characters"
            ),
            Self::ClientNameTooLong => write!(
                f,
                "client_name must be at most {MAX_REGISTRATION_CLIENT_NAME_LEN} characters"
            ),
            Self::InvalidRedirectUri(uri) => write!(
                f,
                "redirect_uri '{uri}' is not allowed: must be an https:// URL or a loopback \
                 http://127.0.0.1[:port]/... or http://localhost[:port]/... URL"
            ),
            Self::UnsupportedGrantType(entry) => write!(
                f,
                "grant_types entry '{entry}' is not supported; this server supports \
                 {SUPPORTED_GRANT_TYPES:?}"
            ),
            Self::UnsupportedResponseType(entry) => write!(
                f,
                "response_types entry '{entry}' is not supported; this server supports \
                 {SUPPORTED_RESPONSE_TYPES:?}"
            ),
        }
    }
}

/// Validate a `POST /register` request against this server's DCR policy
/// (RFC 7591), in order: `token_endpoint_auth_method` (if present, must be
/// `"none"`), non-empty `redirect_uris`, within the count/length caps
/// ([`registration_redirect_uris_within_bounds`]), `client_name` (if
/// present) within its length cap
/// ([`registration_client_name_within_bounds`]), each `redirect_uris` entry
/// an allowed shape ([`validate_registration_redirect_uri`]), then (if
/// present) every `grant_types`/`response_types` entry against
/// [`SUPPORTED_GRANT_TYPES`]/[`SUPPORTED_RESPONSE_TYPES`]. The single
/// validator `AuthService::register_client` enforces — `server::auth::
/// register::post_register` performs no validation of its own, only mapping
/// the returned [`RegistrationError`] onto its RFC 7591 §3.2.2 code.
pub fn validate_registration_request(
    redirect_uris: &[String],
    client_name: Option<&str>,
    token_endpoint_auth_method: Option<&str>,
    grant_types: Option<&[String]>,
    response_types: Option<&[String]>,
) -> Result<(), RegistrationError> {
    if let Some(method) = token_endpoint_auth_method {
        if method != "none" {
            return Err(RegistrationError::UnsupportedAuthMethod(method.to_string()));
        }
    }
    if redirect_uris.is_empty() {
        return Err(RegistrationError::EmptyRedirectUris);
    }
    if !registration_redirect_uris_within_bounds(redirect_uris) {
        return Err(RegistrationError::TooManyOrOverlongRedirectUris);
    }
    if !registration_client_name_within_bounds(client_name) {
        return Err(RegistrationError::ClientNameTooLong);
    }
    for uri in redirect_uris {
        if !validate_registration_redirect_uri(uri) {
            return Err(RegistrationError::InvalidRedirectUri(uri.clone()));
        }
    }
    if let Some(grant_types) = grant_types {
        if let Some(bad) = unsupported_entry(grant_types, SUPPORTED_GRANT_TYPES) {
            return Err(RegistrationError::UnsupportedGrantType(bad.to_string()));
        }
    }
    if let Some(response_types) = response_types {
        if let Some(bad) = unsupported_entry(response_types, SUPPORTED_RESPONSE_TYPES) {
            return Err(RegistrationError::UnsupportedResponseType(bad.to_string()));
        }
    }
    Ok(())
}

/// The first entry in `requested` that `supported` doesn't list — `None`
/// when every entry is supported. Shared by `grant_types`/`response_types`
/// validation in [`validate_registration_request`]: a registrant asking for
/// a flow this server doesn't actually support (e.g. `client_credentials`,
/// `token`) must be rejected rather than echoed back in the `201`, which
/// would otherwise be the only place a client learns which flows are
/// supported.
fn unsupported_entry<'a>(requested: &'a [String], supported: &[&str]) -> Option<&'a str> {
    requested
        .iter()
        .find(|entry| !supported.contains(&entry.as_str()))
        .map(|s| s.as_str())
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
mod tests;
