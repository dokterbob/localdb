//! Daemon-side auth enforcement (T3).
//!
//! ALL auth *policy* (token validation, principal construction, grant
//! evaluation) lives in `localdb_core::auth` per the layering invariant
//! (specs/01-architecture.md §1). This module is only the HTTP surface
//! wiring: the resolved [`AuthMode`], the axum [`middleware`], and the
//! one-time setup-code bootstrap seam (D3b, redeemed by `/authorize` in T4).

pub mod base_url;
pub mod middleware;
pub mod oauth;
pub mod register;

use localdb_core::{auth::mint_secret, Error};

use crate::state::AppState;

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
/// pending-admin marker keeps onboarding recoverable. The caller presents the
/// plaintext once (currently stderr; native presentation can use the same result).
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
