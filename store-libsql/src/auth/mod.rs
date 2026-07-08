//! libsql-backed `AuthStore` (D5): all auth *policy* lives in
//! `localdb_core::auth`; this module is persistence-only, mirroring
//! `SqliteBackend`/`TenantStore`'s conventions for parameter binding, error
//! mapping, and row decoding.

use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::auth::{
    AccessRequestRow, AccessRequestState, AuthCodeRow, AuthStore, AuthTokenRow, InviteRow,
    OAuthClientRow, Role, StoreGrantRow, UserRow,
};
use localdb_core::Error;

use crate::connection::LibsqlDb;

mod auth_codes;
mod clients;
mod grants;
mod invites;
mod sql;
mod tokens;
mod users;

#[cfg(test)]
mod tests;

/// The libsql implementation of `core::auth::AuthStore`.
///
/// Shares the same underlying `LibsqlDb` connection as `SqliteBackend`'s
/// retrieval tables (auth tables live in the same unified database file).
/// Constructed via `SqliteBackend::auth_store()` — `LibsqlDb` itself is
/// `pub(crate)`, so this can't be built directly from outside the crate.
pub struct LibsqlAuthStore {
    conn: Arc<LibsqlDb>,
}

impl LibsqlAuthStore {
    pub(crate) fn new(conn: Arc<LibsqlDb>) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl AuthStore for LibsqlAuthStore {
    async fn create_user(&self, user: &UserRow) -> Result<(), Error> {
        users::create_user(&self.conn, user).await
    }

    async fn get_user(&self, id: &str) -> Result<Option<UserRow>, Error> {
        users::get_user(&self.conn, id).await
    }

    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserRow>, Error> {
        users::get_user_by_name(&self.conn, name).await
    }

    async fn list_users(&self) -> Result<Vec<UserRow>, Error> {
        users::list_users(&self.conn).await
    }

    async fn update_user_role(&self, id: &str, role: Role) -> Result<(), Error> {
        users::update_user_role(&self.conn, id, role).await
    }

    async fn delete_user(&self, id: &str) -> Result<bool, Error> {
        users::delete_user(&self.conn, id).await
    }

    async fn count_users(&self) -> Result<u64, Error> {
        users::count_users(&self.conn).await
    }

    async fn insert_token(&self, token: &AuthTokenRow) -> Result<(), Error> {
        tokens::insert_token(&self.conn, token).await
    }

    async fn find_token_by_hash(&self, secret_hash: &str) -> Result<Option<AuthTokenRow>, Error> {
        tokens::find_token_by_hash(&self.conn, secret_hash).await
    }

    async fn find_token(&self, id: &str) -> Result<Option<AuthTokenRow>, Error> {
        tokens::find_token(&self.conn, id).await
    }

    async fn revoke_token(&self, id: &str) -> Result<bool, Error> {
        let now = localdb_core::ingestion::now_rfc3339();
        tokens::revoke_token(&self.conn, id, &now).await
    }

    async fn revoke_token_family(&self, family_id: &str) -> Result<u64, Error> {
        let now = localdb_core::ingestion::now_rfc3339();
        tokens::revoke_token_family(&self.conn, family_id, &now).await
    }

    async fn mark_token_used(&self, id: &str, used_at: &str) -> Result<(), Error> {
        tokens::mark_token_used(&self.conn, id, used_at).await
    }

    async fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<AuthTokenRow>, Error> {
        tokens::list_tokens_for_user(&self.conn, user_id).await
    }

    async fn create_auth_code(&self, code: &AuthCodeRow) -> Result<(), Error> {
        auth_codes::create_auth_code(&self.conn, code).await
    }

    async fn find_auth_code_by_hash(&self, code_hash: &str) -> Result<Option<AuthCodeRow>, Error> {
        auth_codes::find_auth_code_by_hash(&self.conn, code_hash).await
    }

    async fn consume_auth_code(&self, id: &str, consumed_at: &str) -> Result<bool, Error> {
        auth_codes::consume_auth_code(&self.conn, id, consumed_at).await
    }

    async fn create_oauth_client(&self, client: &OAuthClientRow) -> Result<(), Error> {
        clients::create_oauth_client(&self.conn, client).await
    }

    async fn find_oauth_client(&self, id: &str) -> Result<Option<OAuthClientRow>, Error> {
        clients::find_oauth_client(&self.conn, id).await
    }

    async fn grant_store(&self, grant: &StoreGrantRow) -> Result<(), Error> {
        grants::grant_store(&self.conn, grant).await
    }

    async fn revoke_store_grant(&self, store_name: &str, user_id: &str) -> Result<bool, Error> {
        grants::revoke_store_grant(&self.conn, store_name, user_id).await
    }

    async fn list_grants_for_user(&self, user_id: &str) -> Result<Vec<StoreGrantRow>, Error> {
        grants::list_grants_for_user(&self.conn, user_id).await
    }

    async fn list_grants_for_store(&self, store_name: &str) -> Result<Vec<StoreGrantRow>, Error> {
        grants::list_grants_for_store(&self.conn, store_name).await
    }

    async fn create_invite(&self, invite: &InviteRow) -> Result<(), Error> {
        invites::create_invite(&self.conn, invite).await
    }

    async fn find_invite_by_hash(&self, token_hash: &str) -> Result<Option<InviteRow>, Error> {
        invites::find_invite_by_hash(&self.conn, token_hash).await
    }

    async fn find_invite(&self, id: &str) -> Result<Option<InviteRow>, Error> {
        invites::find_invite(&self.conn, id).await
    }

    async fn list_invites(&self) -> Result<Vec<InviteRow>, Error> {
        invites::list_invites(&self.conn).await
    }

    async fn revoke_invite(&self, id: &str) -> Result<bool, Error> {
        let now = localdb_core::ingestion::now_rfc3339();
        invites::revoke_invite(&self.conn, id, &now).await
    }

    async fn try_consume_invite_use(&self, id: &str) -> Result<bool, Error> {
        invites::try_consume_invite_use(&self.conn, id).await
    }

    async fn release_invite_use(&self, id: &str) -> Result<(), Error> {
        invites::release_invite_use(&self.conn, id).await
    }

    async fn create_access_request(&self, req: &AccessRequestRow) -> Result<(), Error> {
        invites::create_access_request(&self.conn, req).await
    }

    async fn find_access_request(&self, id: &str) -> Result<Option<AccessRequestRow>, Error> {
        invites::find_access_request(&self.conn, id).await
    }

    async fn list_access_requests_for_invite(
        &self,
        invite_id: &str,
    ) -> Result<Vec<AccessRequestRow>, Error> {
        invites::list_access_requests_for_invite(&self.conn, invite_id).await
    }

    async fn list_access_requests(&self) -> Result<Vec<AccessRequestRow>, Error> {
        invites::list_access_requests(&self.conn).await
    }

    async fn update_access_request_state(
        &self,
        id: &str,
        state: AccessRequestState,
        resulting_user_id: Option<&str>,
        decided_at: &str,
    ) -> Result<(), Error> {
        invites::update_access_request_state(&self.conn, id, state, resulting_user_id, decided_at)
            .await
    }

    async fn mark_access_request_collected(
        &self,
        id: &str,
        collected_at: &str,
    ) -> Result<bool, Error> {
        invites::mark_access_request_collected(&self.conn, id, collected_at).await
    }
}
