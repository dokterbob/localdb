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

#[tokio::test]
async fn issued_token_debug_redacts_the_secret() {
    let svc = service();
    let user = svc.create_user("redact-token", Role::Member).await.unwrap();
    let issued = svc.issue_api_key(&user.id).await.unwrap();
    let debugged = format!("{issued:?}");
    assert!(!debugged.contains(&issued.secret), "{debugged}");
    assert!(debugged.contains("<redacted>"), "{debugged}");
}

#[tokio::test]
async fn issued_invite_debug_redacts_the_secret() {
    let svc = service();
    let issued = make_open_invite(&svc, 1).await;
    let debugged = format!("{issued:?}");
    assert!(!debugged.contains(&issued.secret), "{debugged}");
    assert!(debugged.contains("<redacted>"), "{debugged}");
}

#[tokio::test]
async fn issued_auth_code_debug_redacts_the_secret() {
    let svc = service();
    let user = svc.create_user("redact-code", Role::Member).await.unwrap();
    let (_verifier, challenge) = crate::auth::generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            LOCALDB_CLI_CLIENT_ID,
            &user.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            None,
        )
        .await
        .unwrap();
    let debugged = format!("{issued:?}");
    assert!(!debugged.contains(&issued.secret), "{debugged}");
    assert!(debugged.contains("<redacted>"), "{debugged}");
}

mod bootstrap;
mod invites;
mod tokens;
mod users_and_grants;
