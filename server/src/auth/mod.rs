//! Daemon-side auth enforcement (T3).
//!
//! ALL auth *policy* (token validation, principal construction, grant
//! evaluation) lives in `localdb_core::auth` per the layering invariant
//! (specs/01-architecture.md §1). This module is only the HTTP surface
//! wiring: the resolved [`AuthMode`], the axum [`middleware`], and the
//! one-time setup-code bootstrap seam (D3b, redeemed by `/authorize` in T4).

pub mod middleware;

use localdb_core::{
    auth::{mint_secret, AuthStore as _},
    Error,
};

use crate::state::AppState;

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

/// D3b bootstrap: when auth is enforced and no users exist yet, mint a
/// one-time setup code. Its blake3 hash is held in `AppState` (the T4 seam:
/// `/authorize` will verify a presented code against
/// `AppState::setup_code_hash` and mint the first admin user); the plaintext
/// is returned so `start_daemon` can print it to stderr exactly once.
///
/// Nothing consumes the code in T3.
pub async fn generate_setup_code_if_needed(state: &AppState) -> Result<Option<String>, Error> {
    if state.auth_mode() != AuthMode::Enforced {
        return Ok(None);
    }
    if state.auth_store().count_users().await? > 0 {
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
    async fn setup_code_not_generated_when_users_exist() {
        let (_dir, state) = make_state(AuthMode::Enforced).await;
        state
            .auth()
            .create_user("admin", Role::Admin)
            .await
            .unwrap();

        let code = generate_setup_code_if_needed(&state).await.unwrap();

        assert!(
            code.is_none(),
            "existing users must suppress the setup code"
        );
        assert!(state.setup_code_hash().is_none());
    }

    #[tokio::test]
    async fn setup_code_not_generated_in_open_mode() {
        let (_dir, state) = make_state(AuthMode::Open).await;

        let code = generate_setup_code_if_needed(&state).await.unwrap();

        assert!(code.is_none(), "open mode must not mint a setup code");
        assert!(state.setup_code_hash().is_none());
    }
}
