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

use localdb_core::{
    auth::{mint_secret, AuthStore as _},
    Error,
};

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

/// D3b bootstrap: when auth is enforced and no admin exists yet, mint a
/// one-time setup code. Its blake3 hash is held in `AppState` (the T4 seam:
/// `/authorize` will verify a presented code against
/// `AppState::setup_code_hash` and mint the first admin user); the plaintext
/// is returned so `start_daemon` can print it to stderr exactly once.
///
/// Finding #5: this keys off `AuthStore::admin_exists`, not "any user
/// exists" — a first user created without `--admin` (e.g. direct `localdb
/// user add bob`) is a `Role::Member`, and the old `count_users() > 0` check
/// would suppress the setup code in that case, starting the daemon
/// auth-enforced with zero admins and no way to create one via the API
/// (every admin-management route requires an admin principal already).
///
/// Nothing consumes the code in T3.
pub async fn generate_setup_code_if_needed(state: &AppState) -> Result<Option<String>, Error> {
    if state.auth_mode() != AuthMode::Enforced {
        return Ok(None);
    }
    if state.auth_store().admin_exists().await? {
        return Ok(None);
    }
    let minted = mint_secret();
    state.set_setup_code_hash(minted.hash);
    Ok(Some(minted.secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::auth::{hash_secret, Role};
    use localdb_core::config::schema::RawConfig;

    async fn make_state(auth_mode: AuthMode) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
            provider: "fake".to_string(),
            model: "default".to_string(),
        };
        let queue = crate::job_queue::JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir.path().to_path_buf(),
            queue.clone(),
            crate::scheduler::UrlRefreshScheduler::new(queue),
            auth_mode,
        )
        .await
        .unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn setup_code_generated_when_enforced_and_no_users() {
        let (_dir, state) = make_state(AuthMode::Enforced).await;

        let code = generate_setup_code_if_needed(&state).await.unwrap();

        let code = code.expect("enforced + zero users must yield a setup code");
        assert!(
            code.starts_with(localdb_core::auth::TOKEN_PREFIX),
            "setup code should be a minted ldb_ secret, got: {code}"
        );
        // The hash — and only the hash — is held in AppState for T4.
        assert_eq!(
            state.setup_code_hash().as_deref(),
            Some(hash_secret(&code).as_str()),
            "AppState must hold the blake3 hash of the printed plaintext"
        );
    }

    #[tokio::test]
    async fn setup_code_not_generated_when_an_admin_exists() {
        let (_dir, state) = make_state(AuthMode::Enforced).await;
        state
            .auth()
            .create_user("admin", Role::Admin)
            .await
            .unwrap();

        let code = generate_setup_code_if_needed(&state).await.unwrap();

        assert!(
            code.is_none(),
            "an existing admin must suppress the setup code"
        );
        assert!(state.setup_code_hash().is_none());
    }

    /// Finding #5 regression: a first user created *without* `--admin` (a
    /// plain `Role::Member`) must NOT suppress the setup code — the old
    /// "any user exists" check would otherwise leave the daemon
    /// auth-enforced with zero admins and no API-reachable way to create
    /// one.
    #[tokio::test]
    async fn setup_code_still_generated_when_only_member_users_exist() {
        let (_dir, state) = make_state(AuthMode::Enforced).await;
        state.auth().create_user("bob", Role::Member).await.unwrap();

        let code = generate_setup_code_if_needed(&state).await.unwrap();

        let code = code.expect("a member-only instance must still get a setup code");
        assert!(code.starts_with(localdb_core::auth::TOKEN_PREFIX));
        assert_eq!(
            state.setup_code_hash().as_deref(),
            Some(hash_secret(&code).as_str())
        );
    }

    #[tokio::test]
    async fn setup_code_not_generated_in_open_mode() {
        let (_dir, state) = make_state(AuthMode::Open).await;

        let code = generate_setup_code_if_needed(&state).await.unwrap();

        assert!(code.is_none(), "open mode must not mint a setup code");
        assert!(state.setup_code_hash().is_none());
    }
}
