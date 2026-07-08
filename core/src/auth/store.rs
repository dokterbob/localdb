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
    /// Refresh-token rotation family. Every refresh token minted from the
    /// same original login shares a `family_id`; reuse of any revoked
    /// member revokes the whole family (D1).
    pub family_id: Option<String>,
    /// The token ID this one replaced via rotation, if any.
    pub rotated_from: Option<String>,
}

/// A persisted OAuth2 authorization code (RFC 6749 §4.1, T4).
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
}

/// A dynamically registered OAuth2 client (RFC 7591, T7).
///
/// Public clients only (D1's "no passwords" ethos extends here: no
/// `client_secret` is ever minted or stored) — `token_endpoint_auth_method`
/// is always `"none"` and is not persisted, since it never varies.
/// `redirect_uris` are matched **exactly** at `/authorize` time (T7 decision,
/// specs/05-surfaces.md §3.1): registered clients get no loopback-any-port
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
/// The full redeem/approve state machine lands in T6; the table ships now
/// (D13) so a later ticket doesn't need another migration.
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
/// `secret_hash` (T6): the blake3 hash of the request secret minted at
/// redemption time (`AuthService::redeem_invite`) and shown once to the
/// requester then. On approval (`AuthService::approve_request`) that same
/// secret is promoted to the new user's live API key — this is deliberate:
/// it avoids ever holding a *second* plaintext credential in memory between
/// approval and the requester's next poll (see `AuthService::approve_request`
/// doc comment). `collected_at` (T6) guards the "handed out exactly once"
/// contract: `AuthStore::mark_access_request_collected` is the atomic
/// consume-once gate `poll_request` uses, mirroring `consume_auth_code`'s
/// convention for the OAuth2 authorization code.
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
    /// Total user count — used to decide whether to print the one-time
    /// setup code at daemon startup (specs/05-surfaces.md §3.1).
    async fn count_users(&self) -> Result<u64, Error>;
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
    /// convention. Returns `true` iff this call performed the demotion;
    /// `false` covers "unknown id", "already not an admin", and "would have
    /// left zero admins" — `AuthService::set_user_role` disambiguates via
    /// `is_last_admin` before falling back to the plain `update_user_role`
    /// for the idempotent non-admin cases.
    async fn try_demote_user_unless_last_admin(&self, id: &str) -> Result<bool, Error>;

    // ------------------------------------------------------------------
    // Tokens
    // ------------------------------------------------------------------
    async fn insert_token(&self, token: &AuthTokenRow) -> Result<(), Error>;
    async fn find_token_by_hash(&self, secret_hash: &str) -> Result<Option<AuthTokenRow>, Error>;
    /// Look up a token by its own ID (not its secret hash) — used by
    /// `DELETE /v1/keys/{id}` to resolve the owning user before checking
    /// "self or admin" (specs/05-surfaces.md §3.1), where the caller only
    /// has the token's ID, never its secret.
    async fn find_token(&self, id: &str) -> Result<Option<AuthTokenRow>, Error>;
    async fn revoke_token(&self, id: &str) -> Result<bool, Error>;
    /// Revoke every token sharing `family_id`. Called on rotated-refresh
    /// -token reuse detection (D1).
    async fn revoke_token_family(&self, family_id: &str) -> Result<u64, Error>;
    async fn mark_token_used(&self, id: &str, used_at: &str) -> Result<(), Error>;
    async fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<AuthTokenRow>, Error>;

    // ------------------------------------------------------------------
    // OAuth2 authorization codes (T4)
    // ------------------------------------------------------------------
    async fn create_auth_code(&self, code: &AuthCodeRow) -> Result<(), Error>;
    async fn find_auth_code_by_hash(&self, code_hash: &str) -> Result<Option<AuthCodeRow>, Error>;
    /// Mark the code consumed iff it is not already consumed. Returns
    /// `true` if this call consumed it, `false` if it was already consumed
    /// (or unknown) — this is the atomic "consume-once" guard against a
    /// concurrent-redemption race (see `AuthService::redeem_auth_code`).
    async fn consume_auth_code(&self, id: &str, consumed_at: &str) -> Result<bool, Error>;

    // ------------------------------------------------------------------
    // OAuth2 dynamic client registration (RFC 7591, T7)
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
    /// Atomically reserve one use against `max_uses` (T6, D9): the
    /// conditional update `UPDATE invites SET uses = uses + 1 WHERE id = ?
    /// AND uses < max_uses`. Returns `true` iff this call reserved a slot
    /// (mirrors `consume_auth_code`'s "consume iff eligible" convention);
    /// `false` means the invite has no remaining uses.
    ///
    /// `AuthService::redeem_invite` calls this to RESERVE a use *before*
    /// attempting the mint (user-create / access-request-file) — the atomic
    /// gate that caps concurrent redemptions at `max_uses` even when they
    /// race under distinct requested names. If the mint then fails, the
    /// caller must call `release_invite_use` to give the reserved slot back
    /// so a failed redemption never permanently burns a use.
    async fn try_consume_invite_use(&self, id: &str) -> Result<bool, Error>;
    /// Release a use previously reserved by `try_consume_invite_use` when
    /// the subsequent mint failed. Restores `uses` by one (never below
    /// zero).
    async fn release_invite_use(&self, id: &str) -> Result<(), Error>;

    async fn create_access_request(&self, req: &AccessRequestRow) -> Result<(), Error>;
    async fn find_access_request(&self, id: &str) -> Result<Option<AccessRequestRow>, Error>;
    async fn list_access_requests_for_invite(
        &self,
        invite_id: &str,
    ) -> Result<Vec<AccessRequestRow>, Error>;
    /// Every access request across every invite, newest-created last — backs
    /// `GET /v1/invites/requests` (T6). Small admin-facing surface (no
    /// pagination): the number of pending/decided join requests is expected
    /// to be tiny relative to, say, chunk counts.
    async fn list_access_requests(&self) -> Result<Vec<AccessRequestRow>, Error>;
    /// Atomically transition a pending access request to a terminal
    /// decision (`Approved` or `Denied`), iff it is currently `Pending`: a
    /// single conditional `UPDATE ... WHERE id = ? AND state = 'pending'`
    /// (same single-condition-in-the-WHERE-clause convention as
    /// `try_consume_invite_use`/`mark_access_request_collected`). Returns
    /// `true` iff this call performed the transition; `false` means the
    /// request was unknown or already decided (by a concurrent
    /// approve/deny), in which case the caller must not treat its own
    /// decision as having taken effect — see `AuthService::approve_request`
    /// and `AuthService::deny_request`.
    async fn try_decide_access_request(
        &self,
        id: &str,
        state: AccessRequestState,
        resulting_user_id: Option<&str>,
        decided_at: &str,
    ) -> Result<bool, Error>;
    /// Atomically mark an `Approved` access request's credential as
    /// collected, iff it hasn't been already (T6's "handed out exactly
    /// once" contract — mirrors `consume_auth_code`'s single-use guard).
    /// Returns `true` only if this call performed the transition; `false`
    /// if the request is unknown, not yet approved, or already collected —
    /// `AuthService::poll_request` treats every `false` outcome as
    /// `PollOutcome::AlreadyCollected` rather than distinguishing further,
    /// since by the time this is called the caller has already proven
    /// knowledge of the request secret.
    async fn mark_access_request_collected(
        &self,
        id: &str,
        collected_at: &str,
    ) -> Result<bool, Error>;
}

// ---------------------------------------------------------------------------
// FakeAuthStore — in-memory AuthStore for core unit tests.
// ---------------------------------------------------------------------------

/// An in-memory `AuthStore` for use in tests (mirrors `core::store::FakeStore`).
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeAuthStoreInner {
    users: Vec<UserRow>,
    tokens: Vec<AuthTokenRow>,
    auth_codes: Vec<AuthCodeRow>,
    oauth_clients: Vec<OAuthClientRow>,
    grants: Vec<StoreGrantRow>,
    invites: Vec<InviteRow>,
    access_requests: Vec<AccessRequestRow>,
    /// Test-only hook (see `FakeAuthStore::poison_next_revoke`): token IDs
    /// whose *next* `revoke_token` call should report "lost the race"
    /// (`false`) without touching `revoked_at`.
    poisoned_revokes: std::collections::HashSet<String>,
    /// Test-only hook (see `FakeAuthStore::poison_next_decide`): access
    /// request IDs whose *next* `try_decide_access_request` call should
    /// report "lost the race" (`false`) without touching `state`.
    poisoned_decides: std::collections::HashSet<String>,
    /// Test-only hook (see `FakeAuthStore::poison_next_collect`): access
    /// request IDs whose *next* `mark_access_request_collected` call should
    /// report "lost the race" (`false`) without touching `collected_at`.
    poisoned_collects: std::collections::HashSet<String>,
    /// Test-only hook (see `FakeAuthStore::poison_next_insert_token`): when
    /// `true`, the *next* `insert_token` call fails instead of persisting —
    /// simulates a transient store failure partway through a multi-step
    /// mint (T6 finding #6/#7 regression tests).
    poison_next_insert_token: bool,
}

#[cfg(any(test, feature = "test-support"))]
pub struct FakeAuthStore {
    inner: tokio::sync::RwLock<FakeAuthStoreInner>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeAuthStore {
    /// Deterministically simulate losing the atomic-revoke race in
    /// `AuthService::rotate_refresh_token`: the *next* `revoke_token(id)`
    /// call will return `false` (as if a concurrent caller's own
    /// conditional `UPDATE ... WHERE revoked_at IS NULL` had already run
    /// between this call's token fetch and its own revoke attempt) without
    /// marking the row revoked itself. True concurrent interleaving is hard
    /// to reproduce deterministically in a single-threaded unit test; this
    /// hook pins the resulting "revoke returned false" branch directly.
    pub async fn poison_next_revoke(&self, id: &str) {
        self.inner
            .write()
            .await
            .poisoned_revokes
            .insert(id.to_string());
    }

    /// Deterministically simulate losing the atomic decision race in
    /// `AuthService::approve_request`/`deny_request`: the *next*
    /// `try_decide_access_request(id, ..)` call will return `false` (as if a
    /// concurrent approve/deny had already transitioned the request out of
    /// `Pending`) without touching the row itself.
    pub async fn poison_next_decide(&self, id: &str) {
        self.inner
            .write()
            .await
            .poisoned_decides
            .insert(id.to_string());
    }

    /// Deterministically simulate losing the atomic collect race in
    /// `AuthService::poll_request`: the *next*
    /// `mark_access_request_collected(id, ..)` call will return `false` (as
    /// if a concurrent poll had already collected first) without touching
    /// `collected_at`.
    pub async fn poison_next_collect(&self, id: &str) {
        self.inner
            .write()
            .await
            .poisoned_collects
            .insert(id.to_string());
    }

    /// Deterministically simulate a transient store failure partway through
    /// a multi-step mint: the *next* `insert_token` call fails instead of
    /// persisting a row, so `AuthService::issue_api_key` (and anything built
    /// on it, e.g. `mint_open_invite_redemption`/`poll_request`) returns an
    /// error without having minted anything.
    pub async fn poison_next_insert_token(&self) {
        self.inner.write().await.poison_next_insert_token = true;
    }

    pub fn new() -> Self {
        Self {
            inner: tokio::sync::RwLock::new(FakeAuthStoreInner::default()),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for FakeAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Delete `id` from `inner.users` and cascade the cleanup the real schema
/// performs via FK constraints (`store-libsql/src/schema.rs`): tokens and
/// store grants are `ON DELETE CASCADE` (removed outright), and
/// `access_requests.resulting_user_id` is `ON DELETE SET NULL`. Shared by
/// the plain `delete_user` and the guarded `try_delete_user_unless_last_admin`
/// so both report the same cascaded state to callers relying on it (e.g. the
/// compensating deletes in `AuthService::approve_request`/
/// `mint_open_invite_redemption`).
#[cfg(any(test, feature = "test-support"))]
fn delete_user_and_cascade(inner: &mut FakeAuthStoreInner, id: &str) -> bool {
    let before = inner.users.len();
    inner.users.retain(|u| u.id != id);
    let deleted = inner.users.len() != before;
    if deleted {
        inner.tokens.retain(|t| t.user_id != id);
        inner.grants.retain(|g| g.user_id != id);
        for r in inner.access_requests.iter_mut() {
            if r.resulting_user_id.as_deref() == Some(id) {
                r.resulting_user_id = None;
            }
        }
    }
    deleted
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl AuthStore for FakeAuthStore {
    async fn create_user(&self, user: &UserRow) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        if inner.users.iter().any(|u| u.name == user.name) {
            return Err(Error::InvalidRequest {
                message: format!("user '{}' already exists", user.name),
            });
        }
        inner.users.push(user.clone());
        Ok(())
    }

    async fn get_user(&self, id: &str) -> Result<Option<UserRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .users
            .iter()
            .find(|u| u.id == id)
            .cloned())
    }

    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .users
            .iter()
            .find(|u| u.name == name)
            .cloned())
    }

    async fn list_users(&self) -> Result<Vec<UserRow>, Error> {
        Ok(self.inner.read().await.users.clone())
    }

    async fn update_user_role(&self, id: &str, role: Role) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        match inner.users.iter_mut().find(|u| u.id == id) {
            Some(u) => {
                u.role = role;
                Ok(())
            }
            None => Err(Error::InvalidRequest {
                message: format!("user '{id}' not found"),
            }),
        }
    }

    async fn delete_user(&self, id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        let deleted = delete_user_and_cascade(&mut inner, id);
        Ok(deleted)
    }

    async fn count_users(&self) -> Result<u64, Error> {
        Ok(self.inner.read().await.users.len() as u64)
    }

    async fn try_delete_user_unless_last_admin(&self, id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        let target_is_admin = inner
            .users
            .iter()
            .any(|u| u.id == id && u.role == Role::Admin);
        if target_is_admin {
            let admin_count = inner.users.iter().filter(|u| u.role == Role::Admin).count();
            if admin_count <= 1 {
                // Would drop admins to zero — refuse, matching the atomic
                // libsql guard `DELETE ... WHERE role <> 'admin' OR (admin
                // count) > 1`.
                return Ok(false);
            }
        }
        Ok(delete_user_and_cascade(&mut inner, id))
    }

    async fn try_demote_user_unless_last_admin(&self, id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        let admin_count = inner.users.iter().filter(|u| u.role == Role::Admin).count();
        match inner.users.iter_mut().find(|u| u.id == id) {
            Some(u) if u.role == Role::Admin && admin_count > 1 => {
                u.role = Role::Member;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn insert_token(&self, token: &AuthTokenRow) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        if inner.poison_next_insert_token {
            inner.poison_next_insert_token = false;
            return Err(Error::Internal {
                message: "poisoned insert_token: simulated store failure".to_string(),
                correlation_id: "fake_auth_store_poison_insert_token".to_string(),
            });
        }
        inner.tokens.push(token.clone());
        Ok(())
    }

    async fn find_token_by_hash(&self, secret_hash: &str) -> Result<Option<AuthTokenRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .tokens
            .iter()
            .find(|t| t.secret_hash == secret_hash)
            .cloned())
    }

    async fn find_token(&self, id: &str) -> Result<Option<AuthTokenRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .tokens
            .iter()
            .find(|t| t.id == id)
            .cloned())
    }

    async fn revoke_token(&self, id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        if inner.poisoned_revokes.remove(id) {
            // See `FakeAuthStore::poison_next_revoke`.
            return Ok(false);
        }
        let now = crate::ingestion::now_rfc3339();
        match inner.tokens.iter_mut().find(|t| t.id == id) {
            Some(t) if t.revoked_at.is_none() => {
                t.revoked_at = Some(now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn revoke_token_family(&self, family_id: &str) -> Result<u64, Error> {
        let mut inner = self.inner.write().await;
        let now = crate::ingestion::now_rfc3339();
        let mut count = 0u64;
        for t in inner.tokens.iter_mut() {
            if t.family_id.as_deref() == Some(family_id) && t.revoked_at.is_none() {
                t.revoked_at = Some(now.clone());
                count += 1;
            }
        }
        Ok(count)
    }

    async fn mark_token_used(&self, id: &str, used_at: &str) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        if let Some(t) = inner.tokens.iter_mut().find(|t| t.id == id) {
            t.last_used_at = Some(used_at.to_string());
        }
        Ok(())
    }

    async fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<AuthTokenRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .tokens
            .iter()
            .filter(|t| t.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn create_auth_code(&self, code: &AuthCodeRow) -> Result<(), Error> {
        self.inner.write().await.auth_codes.push(code.clone());
        Ok(())
    }

    async fn find_auth_code_by_hash(&self, code_hash: &str) -> Result<Option<AuthCodeRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .auth_codes
            .iter()
            .find(|c| c.code_hash == code_hash)
            .cloned())
    }

    async fn consume_auth_code(&self, id: &str, consumed_at: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        match inner.auth_codes.iter_mut().find(|c| c.id == id) {
            Some(c) if c.consumed_at.is_none() => {
                c.consumed_at = Some(consumed_at.to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn create_oauth_client(&self, client: &OAuthClientRow) -> Result<(), Error> {
        self.inner.write().await.oauth_clients.push(client.clone());
        Ok(())
    }

    async fn find_oauth_client(&self, id: &str) -> Result<Option<OAuthClientRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .oauth_clients
            .iter()
            .find(|c| c.id == id)
            .cloned())
    }

    async fn grant_store(&self, grant: &StoreGrantRow) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        inner
            .grants
            .retain(|g| !(g.store_name == grant.store_name && g.user_id == grant.user_id));
        inner.grants.push(grant.clone());
        Ok(())
    }

    async fn revoke_store_grant(&self, store_name: &str, user_id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        let before = inner.grants.len();
        inner
            .grants
            .retain(|g| !(g.store_name == store_name && g.user_id == user_id));
        Ok(inner.grants.len() != before)
    }

    async fn list_grants_for_user(&self, user_id: &str) -> Result<Vec<StoreGrantRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .grants
            .iter()
            .filter(|g| g.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_grants_for_store(&self, store_name: &str) -> Result<Vec<StoreGrantRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .grants
            .iter()
            .filter(|g| g.store_name == store_name)
            .cloned()
            .collect())
    }

    async fn create_invite(&self, invite: &InviteRow) -> Result<(), Error> {
        self.inner.write().await.invites.push(invite.clone());
        Ok(())
    }

    async fn find_invite_by_hash(&self, token_hash: &str) -> Result<Option<InviteRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .invites
            .iter()
            .find(|i| i.token_hash == token_hash)
            .cloned())
    }

    async fn find_invite(&self, id: &str) -> Result<Option<InviteRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .invites
            .iter()
            .find(|i| i.id == id)
            .cloned())
    }

    async fn list_invites(&self) -> Result<Vec<InviteRow>, Error> {
        Ok(self.inner.read().await.invites.clone())
    }

    async fn revoke_invite(&self, id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        let now = crate::ingestion::now_rfc3339();
        match inner.invites.iter_mut().find(|i| i.id == id) {
            Some(i) if i.revoked_at.is_none() => {
                i.revoked_at = Some(now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn try_consume_invite_use(&self, id: &str) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        match inner.invites.iter_mut().find(|i| i.id == id) {
            Some(i) if i.uses < i.max_uses => {
                i.uses += 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release_invite_use(&self, id: &str) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        if let Some(i) = inner.invites.iter_mut().find(|i| i.id == id) {
            i.uses = i.uses.saturating_sub(1);
        }
        Ok(())
    }

    async fn create_access_request(&self, req: &AccessRequestRow) -> Result<(), Error> {
        self.inner.write().await.access_requests.push(req.clone());
        Ok(())
    }

    async fn find_access_request(&self, id: &str) -> Result<Option<AccessRequestRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .access_requests
            .iter()
            .find(|r| r.id == id)
            .cloned())
    }

    async fn list_access_requests_for_invite(
        &self,
        invite_id: &str,
    ) -> Result<Vec<AccessRequestRow>, Error> {
        Ok(self
            .inner
            .read()
            .await
            .access_requests
            .iter()
            .filter(|r| r.invite_id == invite_id)
            .cloned()
            .collect())
    }

    async fn list_access_requests(&self) -> Result<Vec<AccessRequestRow>, Error> {
        Ok(self.inner.read().await.access_requests.clone())
    }

    async fn try_decide_access_request(
        &self,
        id: &str,
        state: AccessRequestState,
        resulting_user_id: Option<&str>,
        decided_at: &str,
    ) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        if inner.poisoned_decides.remove(id) {
            // See `FakeAuthStore::poison_next_decide`.
            return Ok(false);
        }
        match inner.access_requests.iter_mut().find(|r| r.id == id) {
            Some(r) if r.state == AccessRequestState::Pending => {
                r.state = state;
                r.resulting_user_id = resulting_user_id.map(|s| s.to_string());
                r.decided_at = Some(decided_at.to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn mark_access_request_collected(
        &self,
        id: &str,
        collected_at: &str,
    ) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        if inner.poisoned_collects.remove(id) {
            // See `FakeAuthStore::poison_next_collect`.
            return Ok(false);
        }
        match inner.access_requests.iter_mut().find(|r| r.id == id) {
            Some(r) if r.state == AccessRequestState::Approved && r.collected_at.is_none() => {
                r.collected_at = Some(collected_at.to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, name: &str) -> UserRow {
        UserRow {
            id: id.to_string(),
            name: name.to_string(),
            role: Role::Member,
            created_at: "2026-06-10T12:00:00Z".to_string(),
        }
    }

    fn admin(id: &str, name: &str) -> UserRow {
        UserRow {
            role: Role::Admin,
            ..user(id, name)
        }
    }

    #[tokio::test]
    async fn create_user_rejects_duplicate_name() {
        let store = FakeAuthStore::new();
        store.create_user(&user("u1", "alice")).await.unwrap();
        let err = store.create_user(&user("u2", "alice")).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn poison_next_revoke_makes_revoke_token_report_false_once() {
        let store = FakeAuthStore::new();
        let token = AuthTokenRow {
            id: "t1".to_string(),
            user_id: "u1".to_string(),
            kind: super::TokenKind::Refresh,
            secret_hash: "hash-1".to_string(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            family_id: None,
            rotated_from: None,
        };
        store.insert_token(&token).await.unwrap();

        store.poison_next_revoke("t1").await;
        assert!(!store.revoke_token("t1").await.unwrap());
        // `revoked_at` itself must remain untouched by the poisoned call.
        let stored = store.find_token("t1").await.unwrap().unwrap();
        assert!(stored.revoked_at.is_none());

        // The poison is single-shot: the next call behaves normally.
        assert!(store.revoke_token("t1").await.unwrap());
    }

    // -----------------------------------------------------------------
    // T5/finding #5: atomic last-admin guard at the store level
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn try_delete_user_unless_last_admin_refuses_the_sole_admin() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "only-admin")).await.unwrap();

        assert!(!store.try_delete_user_unless_last_admin("a1").await.unwrap());
        assert!(store.get_user("a1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn try_delete_user_unless_last_admin_succeeds_with_another_admin() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "admin1")).await.unwrap();
        store.create_user(&admin("a2", "admin2")).await.unwrap();

        assert!(store.try_delete_user_unless_last_admin("a1").await.unwrap());
        assert!(store.get_user("a1").await.unwrap().is_none());
        assert!(store.get_user("a2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn try_delete_user_unless_last_admin_allows_deleting_a_member() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "solo-admin")).await.unwrap();
        store.create_user(&user("m1", "some-member")).await.unwrap();

        // The guard only ever blocks dropping the *admin* count to zero —
        // deleting a member is unaffected even with only one admin present.
        assert!(store.try_delete_user_unless_last_admin("m1").await.unwrap());
        assert!(store.get_user("m1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn try_delete_user_unless_last_admin_cascades_tokens_and_grants() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "admin1")).await.unwrap();
        store.create_user(&admin("a2", "admin2")).await.unwrap();
        store
            .insert_token(&AuthTokenRow {
                id: "t1".to_string(),
                user_id: "a1".to_string(),
                kind: super::TokenKind::ApiKey,
                secret_hash: "hash-1".to_string(),
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
                created_at: "2026-06-10T12:00:00Z".to_string(),
                family_id: None,
                rotated_from: None,
            })
            .await
            .unwrap();
        store
            .grant_store(&StoreGrantRow {
                store_name: "docs".to_string(),
                user_id: "a1".to_string(),
                granted_by: "a2".to_string(),
                created_at: "2026-06-10T12:00:00Z".to_string(),
            })
            .await
            .unwrap();

        assert!(store.try_delete_user_unless_last_admin("a1").await.unwrap());
        assert!(store.list_tokens_for_user("a1").await.unwrap().is_empty());
        assert!(store.list_grants_for_user("a1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn try_demote_user_unless_last_admin_refuses_the_sole_admin() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "only-admin")).await.unwrap();

        assert!(!store.try_demote_user_unless_last_admin("a1").await.unwrap());
        let reloaded = store.get_user("a1").await.unwrap().unwrap();
        assert_eq!(reloaded.role, Role::Admin);
    }

    #[tokio::test]
    async fn try_demote_user_unless_last_admin_succeeds_with_another_admin() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "admin1")).await.unwrap();
        store.create_user(&admin("a2", "admin2")).await.unwrap();

        assert!(store.try_demote_user_unless_last_admin("a1").await.unwrap());
        let reloaded = store.get_user("a1").await.unwrap().unwrap();
        assert_eq!(reloaded.role, Role::Member);
        // The remaining admin is unaffected.
        let a2 = store.get_user("a2").await.unwrap().unwrap();
        assert_eq!(a2.role, Role::Admin);
    }

    #[tokio::test]
    async fn try_demote_user_unless_last_admin_is_a_noop_on_a_member() {
        let store = FakeAuthStore::new();
        store.create_user(&admin("a1", "solo-admin")).await.unwrap();
        store.create_user(&user("m1", "some-member")).await.unwrap();

        // `m1` isn't an admin at all — the guard's WHERE clause never
        // matches, so this reports `false` (nothing to demote), not a
        // last-admin refusal.
        assert!(!store.try_demote_user_unless_last_admin("m1").await.unwrap());
    }

    #[tokio::test]
    async fn count_users_reflects_inserts() {
        let store = FakeAuthStore::new();
        assert_eq!(store.count_users().await.unwrap(), 0);
        store.create_user(&user("u1", "alice")).await.unwrap();
        assert_eq!(store.count_users().await.unwrap(), 1);
    }

    fn access_request(id: &str, invite_id: &str) -> AccessRequestRow {
        AccessRequestRow {
            id: id.to_string(),
            invite_id: invite_id.to_string(),
            requested_name: "someone".to_string(),
            secret_hash: "req-hash".to_string(),
            state: AccessRequestState::Pending,
            resulting_user_id: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            decided_at: None,
            collected_at: None,
        }
    }

    #[tokio::test]
    async fn try_decide_access_request_transitions_pending_once() {
        let store = FakeAuthStore::new();
        store
            .create_access_request(&access_request("ar1", "i1"))
            .await
            .unwrap();

        assert!(store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Approved,
                Some("u1"),
                "2026-06-11T00:00:00Z",
            )
            .await
            .unwrap());
        let found = store.find_access_request("ar1").await.unwrap().unwrap();
        assert_eq!(found.state, AccessRequestState::Approved);
        assert_eq!(found.resulting_user_id.as_deref(), Some("u1"));

        // Already decided: a second decision must not overwrite it.
        assert!(!store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Denied,
                None,
                "2026-06-11T00:00:01Z",
            )
            .await
            .unwrap());
        let unchanged = store.find_access_request("ar1").await.unwrap().unwrap();
        assert_eq!(unchanged.state, AccessRequestState::Approved);
    }

    #[tokio::test]
    async fn poison_next_decide_makes_try_decide_report_false_once() {
        let store = FakeAuthStore::new();
        store
            .create_access_request(&access_request("ar1", "i1"))
            .await
            .unwrap();

        store.poison_next_decide("ar1").await;
        assert!(!store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Approved,
                Some("u1"),
                "2026-06-11T00:00:00Z",
            )
            .await
            .unwrap());
        let untouched = store.find_access_request("ar1").await.unwrap().unwrap();
        assert_eq!(untouched.state, AccessRequestState::Pending);

        // Single-shot: the next call behaves normally.
        assert!(store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Approved,
                Some("u1"),
                "2026-06-11T00:00:01Z",
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn poison_next_collect_makes_mark_collected_report_false_once() {
        let store = FakeAuthStore::new();
        store
            .create_access_request(&access_request("ar1", "i1"))
            .await
            .unwrap();
        store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Approved,
                Some("u1"),
                "2026-06-11T00:00:00Z",
            )
            .await
            .unwrap();

        store.poison_next_collect("ar1").await;
        assert!(!store
            .mark_access_request_collected("ar1", "2026-06-11T00:00:01Z")
            .await
            .unwrap());
        let untouched = store.find_access_request("ar1").await.unwrap().unwrap();
        assert!(untouched.collected_at.is_none());

        // Single-shot: the next call behaves normally.
        assert!(store
            .mark_access_request_collected("ar1", "2026-06-11T00:00:02Z")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn poison_next_insert_token_fails_the_next_insert_only() {
        let store = FakeAuthStore::new();
        let token = AuthTokenRow {
            id: "t1".to_string(),
            user_id: "u1".to_string(),
            kind: super::TokenKind::ApiKey,
            secret_hash: "hash-1".to_string(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            family_id: None,
            rotated_from: None,
        };

        store.poison_next_insert_token().await;
        assert!(store.insert_token(&token).await.is_err());
        assert!(store.find_token("t1").await.unwrap().is_none());

        // Single-shot: the next call behaves normally.
        store.insert_token(&token).await.unwrap();
        assert!(store.find_token("t1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn revoke_store_grant_returns_false_when_absent() {
        let store = FakeAuthStore::new();
        assert!(!store.revoke_store_grant("docs", "u1").await.unwrap());
    }

    #[tokio::test]
    async fn grant_store_is_idempotent_per_store_user_pair() {
        let store = FakeAuthStore::new();
        let grant = StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        };
        store.grant_store(&grant).await.unwrap();
        store.grant_store(&grant).await.unwrap();
        let grants = store.list_grants_for_user("u1").await.unwrap();
        assert_eq!(grants.len(), 1, "re-granting must not duplicate the row");
    }
}
