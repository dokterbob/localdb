//! Behavioral contract shared by every `AuthStore` implementation (D5).
//!
//! Each function in the sibling modules is one case: it drives a freshly
//! constructed, empty store through `AuthService` (service-level rules) or
//! the `AuthStore` trait directly (store-level rules) and panics
//! (`assert!`/`assert_eq!`) on a violated expectation. [`FakeAuthStore`] and
//! `store-libsql`'s `LibsqlAuthStore` must both satisfy every case here —
//! that is the point of this module: a divergence between the two (an
//! expiry boundary, a missing cascade, an `Ok`-vs-`Err` mismatch) fails
//! here even though each store's own primitive-level tests pass.
//!
//! [`auth_store_contract_tests!`] turns the full case list into one
//! `#[tokio::test]` per case, so a crate instantiating the suite gets
//! individual test names in its output rather than one monolithic test.
//! Instantiated against [`FakeAuthStore`] in `core` itself
//! (`core/src/auth/store/contract_tests.rs`) and against `LibsqlAuthStore`
//! in `store-libsql` (`store-libsql/src/auth/contract_tests.rs`).
//!
//! [`FakeAuthStore`]: super::FakeAuthStore

pub mod auth_codes;
pub mod bootstrap;
pub mod clients;
pub mod grants;
pub mod invites_closed;
pub mod invites_open;
pub mod tokens;
pub mod users;

/// The store name every case that persists a real grant row uses.
/// `FakeAuthStore` never checks it; `LibsqlAuthStore`'s
/// `store_grants.store_name` has a `REFERENCES stores(name)` FK, so a
/// caller instantiating this suite against it must pre-provision a
/// `stores` row under this exact name before handing the store to
/// [`auth_store_contract_tests!`] (see
/// `store-libsql/src/auth/contract_tests.rs`).
pub const SHARED_STORE_NAME: &str = "docs";

/// Expand to one `#[tokio::test]` per contract case.
///
/// `$factory` is an `Fn() -> impl Future<Output = Arc<S>>` (e.g. `||
/// async { Arc::new(FakeAuthStore::new()) }`), called once per generated
/// test to build a fresh, empty, fully provisioned store — see
/// [`SHARED_STORE_NAME`] for what "provisioned" means for a store that
/// enforces it.
#[macro_export]
macro_rules! auth_store_contract_tests {
    ($factory:expr) => {
        // -------------------------------------------------------------
        // Tokens
        // -------------------------------------------------------------
        #[tokio::test]
        async fn tokens_issue_and_authenticate_api_key() {
            $crate::auth::contract::tokens::issue_and_authenticate_api_key(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn tokens_issue_and_authenticate_access_token() {
            $crate::auth::contract::tokens::issue_and_authenticate_access_token(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn tokens_revoked_token_rejected() {
            $crate::auth::contract::tokens::revoked_token_rejected(($factory)().await).await;
        }
        #[tokio::test]
        async fn tokens_expired_token_rejected() {
            $crate::auth::contract::tokens::expired_token_rejected(($factory)().await).await;
        }
        #[tokio::test]
        async fn tokens_refresh_rotation_happy_path() {
            $crate::auth::contract::tokens::refresh_rotation_happy_path(($factory)().await).await;
        }
        #[tokio::test]
        async fn tokens_rotating_already_rotated_unexpired_token_revokes_family() {
            $crate::auth::contract::tokens::rotating_already_rotated_unexpired_token_revokes_family(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_rotating_unknown_token_fails_without_revocation() {
            $crate::auth::contract::tokens::rotating_unknown_token_fails_without_revocation(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_rotating_wrong_kind_token_fails_without_revocation() {
            $crate::auth::contract::tokens::rotating_wrong_kind_token_fails_without_revocation(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_rotating_expired_token_fails_without_revocation() {
            $crate::auth::contract::tokens::rotating_expired_token_fails_without_revocation(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_rotating_expired_and_rotated_token_fails_without_revocation() {
            $crate::auth::contract::tokens::rotating_expired_and_rotated_token_fails_without_revocation(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_insert_after_family_compromised_refused_identically() {
            $crate::auth::contract::tokens::insert_after_family_compromised_refused_identically(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_session_derived_key_revoked_with_session() {
            $crate::auth::contract::tokens::session_derived_key_revoked_with_session(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn tokens_logout_revokes_family_but_not_a_minted_key() {
            $crate::auth::contract::tokens::logout_revokes_family_but_not_a_minted_key(
                ($factory)().await,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Users
        // -------------------------------------------------------------
        #[tokio::test]
        async fn users_create_user() {
            $crate::auth::contract::users::create_user(($factory)().await).await;
        }
        #[tokio::test]
        async fn users_duplicate_name_refused() {
            $crate::auth::contract::users::duplicate_name_refused(($factory)().await).await;
        }
        #[tokio::test]
        async fn users_delete_cascades_tokens_grants_and_auth_codes() {
            $crate::auth::contract::users::delete_cascades_tokens_grants_and_auth_codes(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn users_last_admin_delete_refused() {
            $crate::auth::contract::users::last_admin_delete_refused(($factory)().await).await;
        }
        #[tokio::test]
        async fn users_last_admin_demotion_refused() {
            $crate::auth::contract::users::last_admin_demotion_refused(($factory)().await).await;
        }
        #[tokio::test]
        async fn users_demoting_a_member_is_idempotent_success() {
            $crate::auth::contract::users::demoting_a_member_is_idempotent_success(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn users_demoting_unknown_id_reports_not_found() {
            $crate::auth::contract::users::demoting_unknown_id_reports_not_found(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn users_two_admins_demote_and_delete_succeed() {
            $crate::auth::contract::users::two_admins_demote_and_delete_succeed(
                ($factory)().await,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Bootstrap
        // -------------------------------------------------------------
        #[tokio::test]
        async fn bootstrap_first_admin_gets_admin_name() {
            $crate::auth::contract::bootstrap::first_admin_gets_admin_name(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn bootstrap_admin_gets_next_free_name_when_admin_taken() {
            $crate::auth::contract::bootstrap::bootstrap_admin_gets_next_free_name_when_admin_taken(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn bootstrap_needed_before_and_after() {
            $crate::auth::contract::bootstrap::bootstrap_needed_before_and_after(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn bootstrap_complete_bootstrap_semantics() {
            $crate::auth::contract::bootstrap::complete_bootstrap_semantics(($factory)().await)
                .await;
        }

        // -------------------------------------------------------------
        // Grants
        // -------------------------------------------------------------
        #[tokio::test]
        async fn grants_grant_list_and_revoke() {
            $crate::auth::contract::grants::grant_list_and_revoke(($factory)().await).await;
        }
        #[tokio::test]
        async fn grants_grant_on_private_store_refused() {
            $crate::auth::contract::grants::grant_on_private_store_refused(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn grants_deleting_user_removes_grants() {
            $crate::auth::contract::grants::deleting_user_removes_grants(($factory)().await)
                .await;
        }

        // -------------------------------------------------------------
        // OAuth2 authorization codes
        // -------------------------------------------------------------
        #[tokio::test]
        async fn auth_codes_issue_and_redeem_once() {
            $crate::auth::contract::auth_codes::issue_and_redeem_once(($factory)().await).await;
        }
        #[tokio::test]
        async fn auth_codes_second_redemption_refused() {
            $crate::auth::contract::auth_codes::second_redemption_refused(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn auth_codes_wrong_pkce_verifier_refused_without_consuming() {
            $crate::auth::contract::auth_codes::wrong_pkce_verifier_refused_without_consuming(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn auth_codes_wrong_redirect_uri_refused_without_consuming() {
            $crate::auth::contract::auth_codes::wrong_redirect_uri_refused_without_consuming(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn auth_codes_wrong_client_refused_without_consuming() {
            $crate::auth::contract::auth_codes::wrong_client_refused_without_consuming(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn auth_codes_expired_code_refused() {
            $crate::auth::contract::auth_codes::expired_code_refused(($factory)().await).await;
        }

        // -------------------------------------------------------------
        // OAuth2 dynamic client registration
        // -------------------------------------------------------------
        #[tokio::test]
        async fn clients_register_and_lookup() {
            $crate::auth::contract::clients::register_and_lookup(($factory)().await).await;
        }
        #[tokio::test]
        async fn clients_unknown_client_is_not_known() {
            $crate::auth::contract::clients::unknown_client_is_not_known(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn clients_builtin_client_known_with_loopback_any_port() {
            $crate::auth::contract::clients::builtin_client_known_with_loopback_any_port(
                ($factory)().await,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Invites — open mode
        // -------------------------------------------------------------
        #[tokio::test]
        async fn invites_open_redeem_creates_user_grants_and_key_atomically() {
            $crate::auth::contract::invites_open::redeem_creates_user_grants_and_key_atomically(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_open_max_uses_enforced_sequentially() {
            $crate::auth::contract::invites_open::max_uses_enforced_sequentially(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_open_max_uses_enforced_under_concurrent_burst() {
            $crate::auth::contract::invites_open::max_uses_enforced_under_concurrent_burst(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_open_revoked_invite_refused() {
            $crate::auth::contract::invites_open::revoked_invite_refused(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn invites_open_expired_invite_refused() {
            $crate::auth::contract::invites_open::expired_invite_refused(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn invites_open_duplicate_requested_name_refused_without_consuming_a_use() {
            $crate::auth::contract::invites_open::duplicate_requested_name_refused_without_consuming_a_use(
                ($factory)().await,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Invites — closed mode
        // -------------------------------------------------------------
        #[tokio::test]
        async fn invites_closed_redeem_files_pending_request_and_consumes_a_use() {
            $crate::auth::contract::invites_closed::redeem_files_pending_request_and_consumes_a_use(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_closed_approve_creates_user_and_grants() {
            $crate::auth::contract::invites_closed::approve_creates_user_and_grants(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_closed_approve_with_taken_name_fails_and_leaves_request_pending() {
            $crate::auth::contract::invites_closed::approve_with_taken_name_fails_and_leaves_request_pending(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_closed_deny_sets_denied_outcome() {
            $crate::auth::contract::invites_closed::deny_sets_denied_outcome(($factory)().await)
                .await;
        }
        #[tokio::test]
        async fn invites_closed_poll_before_decision_is_pending() {
            $crate::auth::contract::invites_closed::poll_before_decision_is_pending(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_closed_first_poll_after_approval_returns_key_once_then_already_collected()
        {
            $crate::auth::contract::invites_closed::first_poll_after_approval_returns_key_once_then_already_collected(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_closed_wrong_secret_and_unknown_id_are_indistinguishable() {
            $crate::auth::contract::invites_closed::wrong_secret_and_unknown_id_are_indistinguishable(
                ($factory)().await,
            )
            .await;
        }
        #[tokio::test]
        async fn invites_closed_concurrent_polls_after_approval_yield_exactly_one_key() {
            $crate::auth::contract::invites_closed::concurrent_polls_after_approval_yield_exactly_one_key(
                ($factory)().await,
            )
            .await;
        }
    };
}
