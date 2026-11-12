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
//! Service-level cases (named through `AuthService`) prove the policy
//! layer's own rules; store-level cases (calling `AuthStore` methods
//! directly, usually suffixed `_direct` or living in a `*_store` module)
//! prove the guard that `AuthService` normally pre-checks before ever
//! reaching the store — a store that got that guard wrong would still pass
//! every service-level case, since the service's own pre-check rejects the
//! input first.
//!
//! [`auth_store_contract_tests!`] turns the full case list into one
//! `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` per case
//! (multi-threaded throughout, not just for the concurrency cases, so a
//! barrier-synchronized burst can actually run its tasks in parallel), so a
//! crate instantiating the suite gets individual test names in its output
//! rather than one monolithic test. Instantiated against [`FakeAuthStore`]
//! in `core` itself (`core/src/auth/store/contract_tests.rs`) and against
//! `LibsqlAuthStore` in `store-libsql`
//! (`store-libsql/src/auth/contract_tests.rs`).
//!
//! `$factory` is an `Fn() -> impl Future<Output = (Arc<S>, G)>` — the
//! second element is any guard value (e.g. a `TempDir`) that must outlive
//! the store itself for the duration of the case; `FakeAuthStore` has
//! nothing to guard and uses `()`. Each generated test destructures the
//! pair and passes only the store into the case function, so the guard
//! stays alive (bound as `_guard`, not bare `_`) until the test's own scope
//! ends — and because every case function takes `Arc<S>` by value and
//! drops its last reference before returning, the store (and the
//! connection it holds) is always gone before the guard's own drop (e.g.
//! deleting the tempdir backing it) ever runs.
//!
//! [`FakeAuthStore`]: super::FakeAuthStore
//! [`AuthStore`]: super::AuthStore

pub mod auth_codes;
pub mod auth_codes_store;
pub mod bootstrap;
pub mod bootstrap_store;
pub mod clients;
pub mod compromise;
pub mod grants;
pub mod grants_store;
pub mod invites_closed;
pub mod invites_closed_store;
pub mod invites_open;
pub mod invites_open_store;
pub mod tokens;
pub mod tokens_store;
pub mod users;
pub mod users_store;

/// The store name every case that persists a real grant row uses.
/// `FakeAuthStore` never checks it; `LibsqlAuthStore`'s
/// `store_grants.store_name` has a `REFERENCES stores(name)` FK, so a
/// caller instantiating this suite against it must pre-provision a
/// `stores` row under this exact name before handing the store to
/// [`auth_store_contract_tests!`] (see
/// `store-libsql/src/auth/contract_tests.rs`).
pub const SHARED_STORE_NAME: &str = "docs";

/// Expand to one `#[tokio::test(flavor = "multi_thread", worker_threads =
/// 4)]` per contract case — see this module's doc comment for `$factory`'s
/// shape and the store-vs-guard drop ordering it relies on.
#[macro_export]
macro_rules! auth_store_contract_tests {
    ($factory:expr) => {
        // -------------------------------------------------------------
        // Tokens
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_issue_and_authenticate_api_key() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::issue_and_authenticate_api_key(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_issue_and_authenticate_access_token() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::issue_and_authenticate_access_token(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_revoked_token_rejected() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::revoked_token_rejected(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_expired_token_rejected() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::expired_token_rejected(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_refresh_rotation_happy_path() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::refresh_rotation_happy_path(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_rotating_already_rotated_unexpired_token_revokes_family() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::rotating_already_rotated_unexpired_token_revokes_family(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_rotating_unknown_token_fails_without_revocation() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::rotating_unknown_token_fails_without_revocation(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_rotating_wrong_kind_token_fails_without_revocation() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::rotating_wrong_kind_token_fails_without_revocation(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_rotating_expired_token_fails_without_revocation() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::rotating_expired_token_fails_without_revocation(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_rotating_expired_and_rotated_token_fails_without_revocation() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::rotating_expired_and_rotated_token_fails_without_revocation(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_insert_after_family_compromised_refused_identically() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::insert_after_family_compromised_refused_identically(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_session_derived_key_revoked_with_session() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::session_derived_key_revoked_with_session(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_logout_revokes_family_but_not_a_minted_key() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens::logout_revokes_family_but_not_a_minted_key(store)
                .await;
        }

        // -------------------------------------------------------------
        // Tokens — direct store-level cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_rotate_live_refresh_token_direct() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::rotate_live_refresh_token_direct(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_rotate_already_rotated_refresh_token_direct() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::rotate_already_rotated_refresh_token_direct(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_rotate_expired_refresh_token_direct() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::rotate_expired_refresh_token_direct(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_rotate_wrong_kind_access_token_with_family_direct() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::rotate_wrong_kind_access_token_with_family_direct(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_rotate_wrong_kind_api_key_direct() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::rotate_wrong_kind_api_key_direct(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_rotate_unknown_id_direct() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::rotate_unknown_id_direct(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_concurrent_rotate_same_refresh_token_yields_one_rotation() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::concurrent_rotate_same_refresh_token_yields_one_rotation(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_mark_token_used_sets_last_used_at() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::mark_token_used_sets_last_used_at(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_list_tokens_for_user_returns_only_that_users_tokens() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::list_tokens_for_user_returns_only_that_users_tokens(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn tokens_store_revoke_token_family_revokes_every_member() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::tokens_store::revoke_token_family_revokes_every_member(store)
                .await;
        }

        // -------------------------------------------------------------
        // Tokens — transitive compromise closure
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn compromise_reuse_on_root_revokes_two_levels_of_descendants() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::compromise::reuse_on_root_revokes_two_levels_of_descendants(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn compromise_revoke_compromised_family_direct_reaches_closure_regardless_of_row_order(
        ) {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::compromise::revoke_compromised_family_direct_reaches_closure_regardless_of_row_order(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Users
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_create_user() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::create_user(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_duplicate_name_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::duplicate_name_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_delete_cascades_tokens_grants_and_auth_codes() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::delete_cascades_tokens_grants_and_auth_codes(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_last_admin_delete_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::last_admin_delete_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_last_admin_demotion_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::last_admin_demotion_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_demoting_a_member_is_idempotent_success() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::demoting_a_member_is_idempotent_success(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_demoting_unknown_id_reports_not_found() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::demoting_unknown_id_reports_not_found(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_two_admins_demote_and_delete_succeed() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users::two_admins_demote_and_delete_succeed(store).await;
        }

        // -------------------------------------------------------------
        // Users — direct store-level cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_demote_sole_admin_direct_is_last_admin() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::demote_sole_admin_direct_is_last_admin(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_demote_unknown_id_direct_is_not_found() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::demote_unknown_id_direct_is_not_found(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_demote_member_direct_is_done() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::demote_member_direct_is_done(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_demote_one_of_two_admins_direct_is_done() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::demote_one_of_two_admins_direct_is_done(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_delete_sole_admin_direct_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::delete_sole_admin_direct_is_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_delete_unknown_id_direct_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::delete_unknown_id_direct_is_false(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_delete_one_of_two_admins_direct_leaves_one() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::delete_one_of_two_admins_direct_leaves_one(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_update_role_promotes_member_to_admin() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::update_user_role_promotes_member_to_admin(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_delete_user_raw_removes_the_user() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::delete_user_raw_removes_the_user(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_delete_user_raw_unknown_id_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::delete_user_raw_unknown_id_is_false(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_count_users_reflects_every_insert() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::count_users_reflects_every_insert(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_admin_exists_is_false_with_only_members() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::admin_exists_is_false_with_only_members(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn users_store_concurrent_demote_of_two_last_admins_never_reaches_zero() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::users_store::concurrent_demote_of_two_last_admins_never_reaches_zero(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Bootstrap
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_first_admin_gets_admin_name() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap::first_admin_gets_admin_name(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_admin_gets_next_free_name_when_admin_taken() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap::bootstrap_admin_gets_next_free_name_when_admin_taken(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_needed_before_and_after() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap::bootstrap_needed_before_and_after(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_complete_bootstrap_semantics() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap::complete_bootstrap_semantics(store).await;
        }

        // -------------------------------------------------------------
        // Bootstrap — direct store-level cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_store_begin_bootstrap_reuses_existing_pending_admin() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap_store::begin_bootstrap_reuses_existing_pending_admin(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_store_begin_bootstrap_refused_once_an_admin_exists() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap_store::begin_bootstrap_refused_once_an_admin_exists(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn bootstrap_store_begin_bootstrap_skips_several_occupied_suffixes() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::bootstrap_store::begin_bootstrap_skips_several_occupied_suffixes(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Grants
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn grants_grant_list_and_revoke() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::grants::grant_list_and_revoke(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn grants_grant_on_private_store_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::grants::grant_on_private_store_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn grants_deleting_user_removes_grants() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::grants::deleting_user_removes_grants(store).await;
        }

        // -------------------------------------------------------------
        // Grants — direct/coverage cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn grants_store_list_grants_for_store_returns_every_grantee() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::grants_store::list_grants_for_store_returns_every_grantee(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn grants_store_revoke_store_grant_then_reauthorized_principal_loses_access() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::grants_store::revoke_store_grant_then_reauthorized_principal_loses_access(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // OAuth2 authorization codes
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_issue_and_redeem_once() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes::issue_and_redeem_once(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_second_redemption_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes::second_redemption_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_wrong_pkce_verifier_refused_without_consuming() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes::wrong_pkce_verifier_refused_without_consuming(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_wrong_redirect_uri_refused_without_consuming() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes::wrong_redirect_uri_refused_without_consuming(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_wrong_client_refused_without_consuming() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes::wrong_client_refused_without_consuming(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_expired_code_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes::expired_code_refused(store).await;
        }

        // -------------------------------------------------------------
        // OAuth2 authorization codes — direct store-level cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_store_consume_fresh_code_direct_succeeds() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes_store::consume_fresh_code_direct_succeeds(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_store_consume_already_consumed_code_direct_fails() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes_store::consume_already_consumed_code_direct_fails(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_store_consume_unknown_id_direct_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes_store::consume_unknown_id_direct_is_false(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn auth_codes_store_concurrent_consume_same_code_yields_exactly_one_success() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::auth_codes_store::concurrent_consume_same_code_yields_exactly_one_success(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // OAuth2 dynamic client registration
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn clients_register_and_lookup() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::clients::register_and_lookup(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn clients_unknown_client_is_not_known() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::clients::unknown_client_is_not_known(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn clients_builtin_client_known_with_loopback_any_port() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::clients::builtin_client_known_with_loopback_any_port(store)
                .await;
        }

        // -------------------------------------------------------------
        // Invites — open mode
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_redeem_creates_user_grants_and_key_atomically() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open::redeem_creates_user_grants_and_key_atomically(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_max_uses_enforced_sequentially() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open::max_uses_enforced_sequentially(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_max_uses_enforced_under_concurrent_burst() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open::max_uses_enforced_under_concurrent_burst(store)
                .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_revoked_invite_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open::revoked_invite_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_expired_invite_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open::expired_invite_refused(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_duplicate_requested_name_refused_without_consuming_a_use() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open::duplicate_requested_name_refused_without_consuming_a_use(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Invites — open mode, direct store-level cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_store_redeem_direct_on_revoked_invite_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open_store::redeem_open_invite_direct_on_revoked_invite_is_refused(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_store_redeem_direct_on_expired_invite_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open_store::redeem_open_invite_direct_on_expired_invite_is_refused(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_store_redeem_direct_on_exhausted_invite_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open_store::redeem_open_invite_direct_on_exhausted_invite_is_refused(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_open_store_list_invites_returns_every_invite() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_open_store::list_invites_returns_every_invite(store)
                .await;
        }

        // -------------------------------------------------------------
        // Invites — closed mode
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_redeem_files_pending_request_and_consumes_a_use() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::redeem_files_pending_request_and_consumes_a_use(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_approve_creates_user_and_grants() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::approve_creates_user_and_grants(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_approve_with_taken_name_fails_and_leaves_request_pending() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::approve_with_taken_name_fails_and_leaves_request_pending(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_deny_sets_denied_outcome() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::deny_sets_denied_outcome(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_poll_before_decision_is_pending() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::poll_before_decision_is_pending(store).await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_first_poll_after_approval_returns_key_once_then_already_collected()
        {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::first_poll_after_approval_returns_key_once_then_already_collected(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_wrong_secret_and_unknown_id_are_indistinguishable() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::wrong_secret_and_unknown_id_are_indistinguishable(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_concurrent_polls_after_approval_yield_exactly_one_key() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed::concurrent_polls_after_approval_yield_exactly_one_key(
                store,
            )
            .await;
        }

        // -------------------------------------------------------------
        // Invites — closed mode, direct store-level and coverage cases
        // -------------------------------------------------------------
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_redeem_direct_on_revoked_invite_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::redeem_closed_invite_direct_on_revoked_invite_is_refused(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_redeem_direct_on_expired_invite_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::redeem_closed_invite_direct_on_expired_invite_is_refused(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_redeem_direct_on_exhausted_invite_is_refused() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::redeem_closed_invite_direct_on_exhausted_invite_is_refused(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_create_access_request_direct_persists_it() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::create_access_request_direct_persists_it(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_list_access_requests_for_invite_filters_by_invite() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::list_access_requests_for_invite_filters_by_invite(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_list_access_requests_orders_newest_last() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::list_access_requests_orders_newest_last(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_try_decide_access_request_unknown_id_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::try_decide_access_request_unknown_id_is_false(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_try_decide_access_request_already_decided_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::try_decide_access_request_already_decided_is_false(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_approve_access_request_direct_on_non_pending_request_is_false(
        ) {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::approve_access_request_direct_on_non_pending_request_is_false(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_collect_access_request_on_pending_request_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::collect_access_request_on_pending_request_is_false(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_collect_access_request_on_denied_request_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::collect_access_request_on_denied_request_is_false(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_collect_access_request_unknown_id_is_false() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::collect_access_request_unknown_id_is_false(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_concurrent_closed_redemption_enforces_max_uses() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::concurrent_closed_redemption_enforces_max_uses(
                store,
            )
            .await;
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn invites_closed_store_concurrent_approve_and_deny_yields_exactly_one_decision() {
            let (store, _guard) = ($factory)().await;
            $crate::auth::contract::invites_closed_store::concurrent_approve_and_deny_yields_exactly_one_decision(
                store,
            )
            .await;
        }
    };
}
