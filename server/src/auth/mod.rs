//! Daemon-side auth enforcement (T3).
//!
//! ALL auth *policy* (token validation, principal construction, grant
//! evaluation) lives in `localdb_core::auth` per the layering invariant
//! (specs/01-architecture.md §1). This module is only the HTTP surface
//! wiring: the resolved [`AuthMode`], the axum [`middleware`], and the
//! one-time setup-code bootstrap seam (D3b, redeemed by `/authorize` in T4).

pub mod base_url;
pub mod join;
pub mod middleware;
pub mod oauth;
pub mod register;

use axum::response::Html;

use localdb_core::{auth::mint_secret, Error};

use crate::state::AppState;

/// Shared inline CSS for every rendered auth-flow HTML page: the OAuth
/// consent page, its OOB (`--no-browser`) success page, and the standalone
/// `/join` page (plus their waiting/joined variants) — one page style so the
/// small set of elements they use (forms, buttons, inline `<code>` secrets,
/// store lists) looks consistent regardless of which page renders them.
const PAGE_STYLE: &str = r#"
body { font-family: -apple-system, system-ui, sans-serif; max-width: 420px; margin: 48px auto; padding: 0 16px; color: #1a1a1a; }
h1 { font-size: 1.25rem; }
label { display: block; margin-top: 16px; font-size: 0.9rem; }
input[type="text"], input[type="password"] { width: 100%; padding: 8px; margin-top: 4px; box-sizing: border-box; font-size: 1rem; }
button { margin-top: 20px; padding: 8px 20px; font-size: 1rem; }
.error { color: #b00020; }
.hint { color: #666; font-size: 0.85rem; }
ul { padding-left: 20px; }
code, #authorization-code { font-size: 1.05rem; user-select: all; background: #f0f0f0; padding: 4px 8px; display: inline-block; margin-top: 8px; word-break: break-all; }
"#;

/// Wrap `body` in the shared minimal (dependency-free, inline-CSS) HTML
/// document every auth-flow page uses — see [`PAGE_STYLE`]. The emitted
/// `<body>` tag carries no attributes, so a caller that needs its own (the
/// waiting pages' `data-*` attributes) can add them with a string replace,
/// the technique `oauth::render_request_submitted_page` and
/// `join::render_join_wait_page` both use.
pub(crate) fn page_shell(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
{PAGE_STYLE}
</style>
</head>
<body>
{body}
</body>
</html>"#
    ))
}

/// Whether `err` is an internal/server-side fault — a bug, a write-lock or
/// store failure, an unreachable or misconfigured dependency — rather than
/// genuine client input. Mirrors `server::error::http_status_for`'s
/// classification: `RuntimeStateLocked`/`DaemonRunning`/`IndexInProgress`
/// (lock/conflict), `DaemonUnreachable`/`ProviderUnavailable` (upstream
/// unavailable), `ModelMissing` (unavailable dependency), and `Internal` (bug)
/// all describe something wrong with the server or its dependencies, never a
/// malformed or invalid *request* — so a public auth-surface caller (`/token`,
/// `/register`) must not be told a client-input-shaped error for them (RFC
/// 6749 §5.2's token-error registry / RFC 7591 §3.2.2's DCR-error registry
/// both describe client-input failures only) and the underlying message must
/// never be echoed back (it can carry internal detail, e.g. a raw SQL error
/// via `Error::Internal`).
///
/// Shared by `oauth.rs` (token issuance/rotation/auth-code redemption,
/// finding #1) and `register.rs` (DCR store-persistence failures, finding
/// #4) — a single classification so the two surfaces can never drift.
pub(crate) fn is_internal_class_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Internal { .. }
            | Error::RuntimeStateLocked
            | Error::DaemonRunning
            | Error::IndexInProgress
            | Error::DaemonUnreachable
            | Error::ProviderUnavailable { .. }
            | Error::ModelMissing { .. }
    )
}

/// The daemon's resolved auth enforcement state, computed once at startup by
/// `daemon::resolve_auth_mode` from `server.auth` + the actually-bound
/// address (specs/05-surfaces.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Every protected route requires a valid bearer token.
    Enforced,
    /// No auth: every request runs as `Principal::local_trust()` — the same
    /// trust boundary as the daemonless CLI.
    Open,
}

/// The type-erasure-free auth service the daemon uses: `AuthService` over
/// the libsql `AuthStore` sharing the unified on-disk database
/// (`<data_dir>/localdb.db`) — the same file the CLI's `AppDb` opens, so
/// users/keys survive restarts and break-glass CLI writes are visible to a
/// (re)started daemon.
pub type ServerAuthService = localdb_core::auth::AuthService<store_libsql::LibsqlAuthStore>;

/// Issue a setup code for first-admin creation or recovery of pending setup.
/// Only its hash lives in AppState. Restart rotates the code while the persisted
/// pending-admin marker keeps onboarding recoverable. The plaintext is returned
/// to the caller to present however its surface sees fit — `server::start_daemon`
/// hands it back on `DaemonHandle::setup_code` rather than printing it itself, so
/// the CLI's `serve` command decides whether and how to show it (stdout, in both
/// human and `--json` output).
pub async fn generate_setup_code_if_needed(state: &AppState) -> Result<Option<String>, Error> {
    if state.auth_mode() != AuthMode::Enforced {
        return Ok(None);
    }
    if !state.auth().bootstrap_needed().await? {
        return Ok(None);
    }
    let minted = mint_secret();
    state.set_setup_code_hash(minted.hash);
    Ok(Some(minted.secret))
}

#[cfg(test)]
mod tests;
