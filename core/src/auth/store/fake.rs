use super::*;
#[derive(Default)]
struct FakeAuthStoreInner {
    users: Vec<UserRow>,
    pending_bootstrap: Option<String>,
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

pub struct FakeAuthStore {
    inner: tokio::sync::RwLock<FakeAuthStoreInner>,
}

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
fn delete_user_and_cascade(inner: &mut FakeAuthStoreInner, id: &str) -> bool {
    let before = inner.users.len();
    inner.users.retain(|u| u.id != id);
    let deleted = inner.users.len() != before;
    if deleted {
        if inner.pending_bootstrap.as_deref() == Some(id) {
            inner.pending_bootstrap = None;
        }
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

#[async_trait]
impl AuthStore for FakeAuthStore {
    async fn pending_bootstrap_user(&self) -> Result<Option<UserRow>, Error> {
        let inner = self.inner.read().await;
        Ok(inner
            .users
            .iter()
            .find(|u| Some(&u.id) == inner.pending_bootstrap.as_ref() && u.role == Role::Admin)
            .cloned())
    }
    async fn begin_bootstrap(&self, user: &UserRow) -> Result<UserRow, Error> {
        let mut inner = self.inner.write().await;
        if let Some(pending) = inner
            .users
            .iter()
            .find(|u| Some(&u.id) == inner.pending_bootstrap.as_ref() && u.role == Role::Admin)
        {
            return Ok(pending.clone());
        }
        if inner.users.iter().any(|u| u.role == Role::Admin) {
            return Err(Error::Unauthorized {
                message: "setup is already complete".into(),
            });
        }
        if inner.users.iter().any(|u| u.name == user.name) {
            return Err(Error::InvalidRequest {
                message: format!("user '{}' already exists", user.name),
            });
        }
        inner.users.push(user.clone());
        inner.pending_bootstrap = Some(user.id.clone());
        Ok(user.clone())
    }
    async fn complete_bootstrap(&self, user_id: &str) -> Result<(), Error> {
        let mut inner = self.inner.write().await;
        if inner.pending_bootstrap.as_deref() == Some(user_id) {
            inner.pending_bootstrap = None;
        }
        Ok(())
    }
    async fn rotate_tokens(
        &self,
        old_id: &str,
        access: &AuthTokenRow,
        refresh: &AuthTokenRow,
    ) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        if inner.poisoned_revokes.remove(old_id) {
            return Ok(false);
        }
        let Some(index) = inner.tokens.iter().position(|t| {
            t.id == old_id
                && t.kind == TokenKind::Refresh
                && t.revoked_at.is_none()
                && !t.expires_at.as_deref().is_some_and(crate::auth::is_expired)
        }) else {
            return Ok(false);
        };
        if inner.poison_next_insert_token {
            inner.poison_next_insert_token = false;
            return Err(Error::Internal {
                message: "simulated token insert failure".into(),
                correlation_id: "fake_rotate_tokens".into(),
            });
        }
        inner.tokens[index].revoked_at = Some(crate::auth::rfc3339_from_now(0));
        inner.tokens.extend([refresh.clone(), access.clone()]);
        Ok(true)
    }
    async fn approve_access_request(
        &self,
        id: &str,
        user: &UserRow,
        grants: &[StoreGrantRow],
    ) -> Result<bool, Error> {
        let mut inner = self.inner.write().await;
        if inner.poisoned_decides.remove(id) {
            return Ok(false);
        }
        let Some(index) = inner
            .access_requests
            .iter()
            .position(|r| r.id == id && r.state == AccessRequestState::Pending)
        else {
            return Ok(false);
        };
        if inner.users.iter().any(|u| u.name == user.name) {
            return Err(Error::InvalidRequest {
                message: format!("user '{}' already exists", user.name),
            });
        }
        inner.users.push(user.clone());
        inner.grants.extend_from_slice(grants);
        let request = &mut inner.access_requests[index];
        request.state = AccessRequestState::Approved;
        request.resulting_user_id = Some(user.id.clone());
        request.decided_at = Some(user.created_at.clone());
        Ok(true)
    }

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

    async fn admin_exists(&self) -> Result<bool, Error> {
        Ok(self
            .inner
            .read()
            .await
            .users
            .iter()
            .any(|u| u.role == Role::Admin))
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
            Some(i)
                if i.uses < i.max_uses
                    && i.revoked_at.is_none()
                    && !i.expires_at.as_deref().is_some_and(crate::auth::is_expired) =>
            {
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
