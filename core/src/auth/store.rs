//! `AuthStore`: the persistence seam for all auth policy (D5).
//!
//! Mirrors `core::store::RetrievalStore`'s conventions: an object-safe async
//! trait, `Send + Sync + 'static`, with narrowly-scoped CRUD methods. The
//! concrete implementation lives in `store-libsql`; `core` itself does no
//! I/O.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::principal::Role;
use crate::Error;

/// A persisted user account. No passwords (D1) — identity is proven solely
/// by bearer secrets (`AuthTokenRow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRow {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub created_at: String,
}

/// The first of `base`, `base-2`, `base-3`, … for which `is_taken` reports
/// `false`. Used by `begin_bootstrap` to pick an admin name that doesn't
/// collide with an existing member — `users.name` is UNIQUE, so bootstrap
/// must never assume `base` itself is free.
pub fn first_unused_name(base: &str, is_taken: impl Fn(&str) -> bool) -> String {
    if !is_taken(base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        if !is_taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Which kind of bearer secret an `AuthTokenRow` represents.
///
/// All three share one table (D1) — an API key is simply a token with
/// `kind = ApiKey`, no default expiry, and `last_used_at` tracked instead of
/// TTL enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Access,
    Refresh,
    ApiKey,
}

/// Outcome of `AuthStore::try_demote_user_unless_last_admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoteOutcome {
    /// `id` is now (or already was) `Role::Member`.
    Done,
    /// `id` is the sole admin — refused to avoid a zero-admin instance.
    LastAdmin,
    /// `id` does not name a known user.
    NotFound,
}

/// Outcome of `AuthStore::rotate_tokens`.
///
/// `AuthService::rotate_refresh_token` already rejects a wrong-kind,
/// already-revoked (per the row it fetched), or expired token before ever
/// calling `rotate_tokens` — so in the ordinary path the store only sees a
/// genuinely eligible row and reports `Rotated`. A non-`Rotated` result
/// means the row changed between that check and this call: concurrently
/// rotated (reuse — the caller must revoke the family) or concurrently
/// expired (not reuse — no revocation). The split matters because the two
/// layers must agree on the expiry boundary: a token expiring in the exact
/// same second as "now" stays eligible, matching `core::auth::is_expired`'s
/// own boundary (`expires_at < now`); only a strictly earlier `expires_at`
/// is `Ineligible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateOutcome {
    /// The old refresh token was revoked and both replacements inserted.
    Rotated,
    /// `old_id` is unknown, not a refresh token, or expired — not evidence
    /// of reuse; the caller must not revoke anything.
    Ineligible,
    /// `old_id` names a refresh token that is not expired but is already
    /// revoked — reuse; the caller must revoke the whole family.
    AlreadyRotated,
}

/// A persisted bearer token/API key.
///
/// Only `secret_hash` (blake3) is ever stored — the plaintext secret is
/// shown once at mint time and never persisted (D1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokenRow {
    pub id: String,
    pub user_id: String,
    pub kind: TokenKind,
    pub secret_hash: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_at: String,
    /// Login-session family: the refresh chain plus every access token
    /// issued alongside it. Every refresh token minted from the same
    /// original login, and every access token minted together with one of
    /// them (`AuthService::issue_session`/`rotate_refresh_token`), shares a
    /// `family_id`; reuse of any revoked member revokes the whole family
    /// (D1).
    pub family_id: Option<String>,
    /// The token ID this one replaced via rotation, if any.
    pub rotated_from: Option<String>,
    /// For an API key: the session `family_id` active when it was minted,
    /// if any (`AuthService::issue_api_key_minted_by`). Lets reuse-detected
    /// family revocation also burn API keys minted while that session (or a
    /// key derived from it) was the caller's credential, closing the
    /// residual gap where a stolen session could mint a permanent key.
    /// `None` for keys minted with no session in scope (e.g.
    /// `issue_api_key`, invite redemption).
    pub minted_by_family: Option<String>,
}

/// A persisted OAuth2 authorization code (RFC 6749 §4.1).
///
/// Single-use, bound at issue time to `client_id` + `redirect_uri` +
/// `code_challenge` (PKCE S256) so a code minted for one exchange can't be
/// replayed against a different client/redirect/verifier combination — see
/// `AuthService::redeem_auth_code`. Only `code_hash` (blake3) is ever
/// stored; the plaintext code is shown once, in the `Location` redirect from
/// `POST /authorize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCodeRow {
    pub id: String,
    pub client_id: String,
    pub user_id: String,
    pub code_hash: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub redirect_uri: String,
    pub expires_at: String,
    pub consumed_at: Option<String>,
    pub created_at: String,
    /// The session family of the credential that authorized issuing this
    /// code (`AuthService::issue_auth_code`), i.e. its parent — `None` for
    /// the setup-code/bootstrap branch, which has no session ancestry.
    /// `AuthService::redeem_auth_code` hands this back so the session it
    /// creates (`AuthService::issue_session`) carries it forward as its own
    /// `minted_by_family`, letting reuse detection on the authorizing
    /// credential's family revoke this descendant session too.
    pub minted_by_family: Option<String>,
}

/// A dynamically registered OAuth2 client (RFC 7591).
///
/// Public clients only (no `client_secret` is ever minted or stored, the
/// same policy that keeps every other credential here as a hash rather than
/// a plaintext secret) — `token_endpoint_auth_method`
/// is always `"none"` and is not persisted, since it never varies.
/// `redirect_uris` are matched **exactly** at `/authorize` time
/// (specs/05-surfaces.md §3.1): registered clients get no loopback-any-port
/// exception the way the built-in `localdb-cli` client does — see
/// `core::auth::client::validate_registration_redirect_uri`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClientRow {
    pub id: String,
    pub client_name: Option<String>,
    /// The exact set of redirect URIs this client registered. Stored as a
    /// JSON array in the `oauth_clients.redirect_uris` column.
    pub redirect_uris: Vec<String>,
    pub created_at: String,
}

/// A store-name/user-id grant (D7). Normalized: one row per grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreGrantRow {
    pub store_name: String,
    pub user_id: String,
    pub granted_by: String,
    pub created_at: String,
}

/// Whether redeeming an invite requires admin approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InviteMode {
    /// Redemption immediately creates a user — no approval step.
    Open,
    /// Redemption creates a pending `AccessRequestRow`; an admin must approve.
    Closed,
}

/// A persisted invite.
///
/// The full redeem/approve state machine lives in `AuthService`; this row
/// shape covers both `open` and `closed` modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRow {
    pub id: String,
    pub token_hash: String,
    pub mode: InviteMode,
    /// Store names granted to the resulting user on redemption.
    pub store_grants: Vec<String>,
    pub max_uses: u32,
    pub uses: u32,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

/// State of a pending access request against a `closed`-mode invite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessRequestState {
    Pending,
    Approved,
    Denied,
}

/// A request to redeem a `closed`-mode invite, awaiting admin approval.
///
/// `secret_hash`: the blake3 hash of the request secret minted at
/// redemption time (`AuthService::redeem_invite`) and shown once to the
/// requester then. That secret never becomes a credential itself — it only
/// proves the requester's identity on later `poll_request` calls.
/// `AuthService::poll_request` mints a *fresh* API key for the resulting
/// user the first time a poll observes the `Approved` state, so approval
/// alone never hands out a usable credential. `collected_at` guards the
/// "handed out exactly once" contract: `AuthStore::collect_access_request`
/// is the atomic consume-once gate `poll_request` uses, mirroring
/// `consume_auth_code`'s convention for the OAuth2 authorization code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRequestRow {
    pub id: String,
    pub invite_id: String,
    pub requested_name: String,
    pub secret_hash: String,
    pub state: AccessRequestState,
    pub resulting_user_id: Option<String>,
    pub created_at: String,
    pub decided_at: Option<String>,
    pub collected_at: Option<String>,
}

/// Persistence seam for all auth policy state.
///
/// Implemented over libsql in `store-libsql`; `core` itself does no I/O
/// (D5). Object-safe so it can be boxed/`Arc`-shared across async tasks,
/// matching `RetrievalStore`'s conventions.
#[async_trait]
pub trait AuthStore: Send + Sync + 'static {
    // ------------------------------------------------------------------
    // Users
    // ------------------------------------------------------------------
    async fn create_user(&self, user: &UserRow) -> Result<(), Error>;
    async fn get_user(&self, id: &str) -> Result<Option<UserRow>, Error>;
    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserRow>, Error>;
    async fn list_users(&self) -> Result<Vec<UserRow>, Error>;
    async fn update_user_role(&self, id: &str, role: Role) -> Result<(), Error>;
    async fn delete_user(&self, id: &str) -> Result<bool, Error>;
    /// Total user count — used by `AuthService`/callers that need a plain
    /// user census (specs/05-surfaces.md §3.1).
    async fn count_users(&self) -> Result<u64, Error>;
    /// Whether at least one `Role::Admin` user exists.
    ///
    /// The one-time setup-code bootstrap
    /// (`server::auth::generate_setup_code_if_needed`) must key off this, not
    /// `count_users() > 0` — a first user created without `--admin` (e.g.
    /// direct `localdb user add bob`) is a `Role::Member`, and suppressing
    /// the setup code on "any user exists" would leave the instance
    /// auth-enforced with zero admins and no way to create one via the API
    /// (every admin-management route requires an admin principal already).
    async fn admin_exists(&self) -> Result<bool, Error>;
    /// Pending first-admin setup; absence never implies an established admin needs recovery.
    async fn pending_bootstrap_user(&self) -> Result<Option<UserRow>, Error>;
    /// Atomically reuse the pending admin or create the first admin and its recovery marker.
    /// Reject when an established admin already exists. `user.name` is the *desired* name
    /// (`"admin"`); if a member already holds it, the stored user gets the first unused
    /// `"admin-2"`, `"admin-3"`, … variant instead (`first_unused_name`) — `users.name` is
    /// UNIQUE, so insisting on the literal name would make setup unable to complete.
    async fn begin_bootstrap(&self, user: &UserRow) -> Result<UserRow, Error>;
    async fn complete_bootstrap(&self, user_id: &str) -> Result<(), Error>;
    /// Atomically delete `id` unless it is currently the *sole* admin (D7's
    /// last-admin lockout guard): a single conditional `DELETE ... WHERE id
    /// = ? AND (role <> 'admin' OR (admin count) > 1)`, so two concurrent
    /// guarded deletes/demotes racing the same two-admin instance can never
    /// both succeed and leave zero admins. Returns `true` iff this call
    /// deleted the row; `false` covers both "unknown id" and "would have
    /// left zero admins" — `AuthService::delete_user` disambiguates the two
    /// for its error message via `is_last_admin`.
    async fn try_delete_user_unless_last_admin(&self, id: &str) -> Result<bool, Error>;
    /// Atomically demote `id` to `Role::Member` unless it is currently the
    /// sole admin — the demotion counterpart to
    /// `try_delete_user_unless_last_admin`, same conditional-UPDATE
    /// convention, but folding the idempotent "already a member" case into
    /// the same guarded statement too (`role = 'member' OR (admin count) >
    /// 1`) so every outcome comes from one write — no second, unguarded
    /// `update_user_role` call that a concurrent promote/demote could race
    /// into leaving zero admins. See [`DemoteOutcome`].
    async fn try_demote_user_unless_last_admin(&self, id: &str) -> Result<DemoteOutcome, Error>;

    // ------------------------------------------------------------------
    // Tokens
    // ------------------------------------------------------------------
    /// Insert `token`. Refuses with `Error::Unauthorized` — persisting
    /// nothing — if `token.minted_by_family` is `Some` and that family has
    /// already been recorded as compromised (by a concurrent or prior
    /// `revoke_compromised_family` call): the atomic guard against minting a
    /// key or session from a credential whose session is revoked between an
    /// OAuth authorization and the token exchange that redeems it, or
    /// concurrently with a reuse-detected revocation racing the same insert.
    /// A `None` `minted_by_family` is never refused.
    async fn insert_token(&self, token: &AuthTokenRow) -> Result<(), Error>;
    /// Consume the old refresh token and insert both replacements atomically;
    /// errors leave all three rows unchanged. See [`RotateOutcome`] for what
    /// each non-`Rotated` outcome means and which one calls for revoking the
    /// family.
    async fn rotate_tokens(
        &self,
        old_id: &str,
        access: &AuthTokenRow,
        refresh: &AuthTokenRow,
    ) -> Result<RotateOutcome, Error>;
    async fn find_token_by_hash(&self, secret_hash: &str) -> Result<Option<AuthTokenRow>, Error>;
    /// Look up a token by its own ID (not its secret hash) — used by
    /// `DELETE /v1/keys/{id}` to resolve the owning user before checking
    /// "self or admin" (specs/05-surfaces.md §3.1), where the caller only
    /// has the token's ID, never its secret.
    async fn find_token(&self, id: &str) -> Result<Option<AuthTokenRow>, Error>;
    async fn revoke_token(&self, id: &str) -> Result<bool, Error>;
    /// Revoke every token sharing `family_id`. Called on explicit
    /// revocation (`AuthService::revoke_by_secret`'s refresh-token branch,
    /// RFC 7009 logout) — deliberately narrower than
    /// `revoke_compromised_family`: logout ends the session's own access
    /// and refresh tokens, never a key the user minted on purpose.
    async fn revoke_token_family(&self, family_id: &str) -> Result<u64, Error>;
    /// Revoke `family_id`'s entire *compromise closure*: `family_id`
    /// itself, plus the family of every token minted by a credential
    /// already in the closure, transitively — a chain of sessions created
    /// by authorizing one with another's credential, and the API keys
    /// minted under any of them, are all burned together. Every closure
    /// member is also durably recorded (so a token insert racing this call,
    /// or arriving later via an in-flight OAuth exchange, is refused by
    /// `insert_token` rather than slipping in after the sweep). Called on
    /// rotated-refresh-token reuse detection (D1); deliberately includes
    /// keys minted for other users by an admin whose own session was
    /// stolen, since the session itself is compromised either way.
    async fn revoke_compromised_family(&self, family_id: &str) -> Result<u64, Error>;
    async fn mark_token_used(&self, id: &str, used_at: &str) -> Result<(), Error>;
    async fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<AuthTokenRow>, Error>;

    // ------------------------------------------------------------------
    // OAuth2 authorization codes
    // ------------------------------------------------------------------
    async fn create_auth_code(&self, code: &AuthCodeRow) -> Result<(), Error>;
    async fn find_auth_code_by_hash(&self, code_hash: &str) -> Result<Option<AuthCodeRow>, Error>;
    /// Mark the code consumed iff it is not already consumed. Returns
    /// `true` if this call consumed it, `false` if it was already consumed
    /// (or unknown) — this is the atomic "consume-once" guard against a
    /// concurrent-redemption race (see `AuthService::redeem_auth_code`).
    async fn consume_auth_code(&self, id: &str, consumed_at: &str) -> Result<bool, Error>;

    // ------------------------------------------------------------------
    // OAuth2 dynamic client registration (RFC 7591)
    // ------------------------------------------------------------------
    async fn create_oauth_client(&self, client: &OAuthClientRow) -> Result<(), Error>;
    async fn find_oauth_client(&self, id: &str) -> Result<Option<OAuthClientRow>, Error>;

    // ------------------------------------------------------------------
    // Grants
    // ------------------------------------------------------------------
    async fn grant_store(&self, grant: &StoreGrantRow) -> Result<(), Error>;
    async fn revoke_store_grant(&self, store_name: &str, user_id: &str) -> Result<bool, Error>;
    async fn list_grants_for_user(&self, user_id: &str) -> Result<Vec<StoreGrantRow>, Error>;
    async fn list_grants_for_store(&self, store_name: &str) -> Result<Vec<StoreGrantRow>, Error>;

    // ------------------------------------------------------------------
    // Invites + access requests
    // ------------------------------------------------------------------
    async fn create_invite(&self, invite: &InviteRow) -> Result<(), Error>;
    async fn find_invite_by_hash(&self, token_hash: &str) -> Result<Option<InviteRow>, Error>;
    async fn find_invite(&self, id: &str) -> Result<Option<InviteRow>, Error>;
    async fn list_invites(&self) -> Result<Vec<InviteRow>, Error>;
    async fn revoke_invite(&self, id: &str) -> Result<bool, Error>;
    /// Atomically reserve one use against `max_uses` (D9) and commit an
    /// `open`-mode redemption's entire mint — creating `user`, applying its
    /// `grants`, and inserting the freshly minted API-key `credential` — as
    /// one transaction, modelled on `approve_access_request`. Returns
    /// `false` if the invite is no longer eligible (exhausted, revoked, or
    /// expired as of this call); any other failure (most commonly `user`'s
    /// name colliding with `create_user`'s UNIQUE constraint) rolls the
    /// whole transaction back, so a redemption that doesn't commit never
    /// burns a use either way — there is nothing to release.
    async fn redeem_open_invite(
        &self,
        invite_id: &str,
        user: &UserRow,
        grants: &[StoreGrantRow],
        credential: &AuthTokenRow,
    ) -> Result<bool, Error>;
    /// The `closed`-mode counterpart to `redeem_open_invite`: atomically
    /// reserves a use and files `request` together. Returns `false` under
    /// the same eligibility conditions.
    async fn redeem_closed_invite(
        &self,
        invite_id: &str,
        request: &AccessRequestRow,
    ) -> Result<bool, Error>;

    /// Create the user, apply grants, and approve a still-pending request in one transaction.
    async fn approve_access_request(
        &self,
        id: &str,
        user: &UserRow,
        grants: &[StoreGrantRow],
    ) -> Result<bool, Error>;
    async fn create_access_request(&self, req: &AccessRequestRow) -> Result<(), Error>;
    async fn find_access_request(&self, id: &str) -> Result<Option<AccessRequestRow>, Error>;
    async fn list_access_requests_for_invite(
        &self,
        invite_id: &str,
    ) -> Result<Vec<AccessRequestRow>, Error>;
    /// Every access request across every invite, newest-created last — backs
    /// `GET /v1/invites/requests`. Small admin-facing surface (no
    /// pagination): the number of pending/decided join requests is expected
    /// to be tiny relative to, say, chunk counts.
    async fn list_access_requests(&self) -> Result<Vec<AccessRequestRow>, Error>;
    /// Atomically transition a pending access request to a terminal
    /// decision (`Approved` or `Denied`), iff it is currently `Pending`: a
    /// single conditional `UPDATE ... WHERE id = ? AND state = 'pending'`
    /// (same single-condition-in-the-WHERE-clause convention as
    /// `collect_access_request`). Returns `true` iff this call performed the
    /// transition; `false` means the request was unknown or already decided
    /// (by a concurrent approve/deny), in which case the caller must not
    /// treat its own decision as having taken effect — see
    /// `AuthService::approve_request` and `AuthService::deny_request`.
    async fn try_decide_access_request(
        &self,
        id: &str,
        state: AccessRequestState,
        resulting_user_id: Option<&str>,
        decided_at: &str,
    ) -> Result<bool, Error>;
    /// Atomically mark an `Approved` access request's credential as
    /// collected, iff it hasn't been already (the "handed out exactly once"
    /// contract — mirrors `consume_auth_code`'s single-use guard), and
    /// insert the freshly minted `credential` together as one transaction —
    /// modelled on `approve_access_request`. Returns `true` only if this
    /// call performed the transition and the insert; `false` if the request
    /// is unknown, not yet approved, or already collected, in which case
    /// `credential` was never persisted — `AuthService::poll_request` treats
    /// every `false` outcome as `PollOutcome::AlreadyCollected` rather than
    /// distinguishing further, since by the time this is called the caller
    /// has already proven knowledge of the request secret.
    async fn collect_access_request(
        &self,
        id: &str,
        collected_at: &str,
        credential: &AuthTokenRow,
    ) -> Result<bool, Error>;
}

// ---------------------------------------------------------------------------
// FakeAuthStore — in-memory AuthStore for core unit tests.
// ---------------------------------------------------------------------------

/// An in-memory `AuthStore` for use in tests (mirrors `core::store::FakeStore`).
#[cfg(any(test, feature = "test-support"))]
mod fake;
#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeAuthStore;

#[cfg(test)]
mod tests;

/// Runs the shared `AuthStore` contract suite (`crate::auth::contract`)
/// against `FakeAuthStore` — see that module's doc comment.
#[cfg(test)]
mod contract_tests;
