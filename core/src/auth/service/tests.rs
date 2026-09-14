use super::*;
use crate::auth::client::LOCALDB_CLI_CLIENT_ID;
use crate::auth::store::FakeAuthStore;
use crate::auth::token::TOKEN_PREFIX;

fn service() -> AuthService<FakeAuthStore> {
    AuthService::new(Arc::new(FakeAuthStore::new()))
}

async fn make_open_invite(svc: &AuthService<FakeAuthStore>, max_uses: u32) -> IssuedInvite {
    svc.create_invite(
        InviteMode::Open,
        &[("docs".to_string(), StoreVisibility::Shared)],
        max_uses,
        None,
        "admin-1",
    )
    .await
    .unwrap()
}

async fn make_closed_invite(svc: &AuthService<FakeAuthStore>, max_uses: u32) -> IssuedInvite {
    svc.create_invite(
        InviteMode::Closed,
        &[("docs".to_string(), StoreVisibility::Shared)],
        max_uses,
        None,
        "admin-1",
    )
    .await
    .unwrap()
}

mod invites;
mod tokens;
mod users_and_grants;
