//! Direct `AuthStore::consume_auth_code` cases.
//!
//! `AuthService::redeem_auth_code` already checks `consumed_at` itself
//! before ever calling `consume_auth_code`, so a second service-level
//! redemption attempt never reaches the store's own consume-once guard —
//! these cases call it directly.

use std::sync::Arc;

use crate::auth::{mint_secret, rfc3339_from_now, AuthCodeRow, AuthStore, Role, UserRow};
use crate::ids::new_ulid;

async fn seed_code<S: AuthStore>(store: &S) -> AuthCodeRow {
    let user = UserRow {
        id: new_ulid(),
        name: format!("auth-codes-store-{}", new_ulid()),
        role: Role::Member,
        created_at: rfc3339_from_now(0),
    };
    store.create_user(&user).await.unwrap();
    let minted = mint_secret();
    let row = AuthCodeRow {
        id: new_ulid(),
        client_id: crate::auth::LOCALDB_CLI_CLIENT_ID.to_string(),
        user_id: user.id,
        code_hash: minted.hash,
        code_challenge: "irrelevant-to-this-case".to_string(),
        code_challenge_method: "S256".to_string(),
        redirect_uri: "http://127.0.0.1:1/cb".to_string(),
        expires_at: rfc3339_from_now(600),
        consumed_at: None,
        created_at: rfc3339_from_now(0),
        minted_by_family: None,
    };
    store.create_auth_code(&row).await.unwrap();
    row
}

pub async fn consume_fresh_code_direct_succeeds<S: AuthStore>(store: Arc<S>) {
    let code = seed_code(&*store).await;
    let consumed = store
        .consume_auth_code(&code.id, &rfc3339_from_now(0))
        .await
        .unwrap();
    assert!(consumed);
    let reloaded = store
        .find_auth_code_by_hash(&code.code_hash)
        .await
        .unwrap()
        .unwrap();
    assert!(reloaded.consumed_at.is_some());
}

pub async fn consume_already_consumed_code_direct_fails<S: AuthStore>(store: Arc<S>) {
    let code = seed_code(&*store).await;
    let first_consumed_at = rfc3339_from_now(0);
    assert!(store
        .consume_auth_code(&code.id, &first_consumed_at)
        .await
        .unwrap());

    let second = store
        .consume_auth_code(&code.id, &rfc3339_from_now(0))
        .await
        .unwrap();
    assert!(
        !second,
        "a second consume of the same code must report false"
    );
    let reloaded = store
        .find_auth_code_by_hash(&code.code_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.consumed_at,
        Some(first_consumed_at),
        "a losing consume must not change consumed_at"
    );
}

pub async fn consume_unknown_id_direct_is_false<S: AuthStore>(store: Arc<S>) {
    let consumed = store
        .consume_auth_code("auth-codes-store-unknown-id", &rfc3339_from_now(0))
        .await
        .unwrap();
    assert!(!consumed);
}

/// Deterministic concurrency check: a burst of simultaneous consumes
/// against the SAME fresh code must yield exactly one `true`.
pub async fn concurrent_consume_same_code_yields_exactly_one_success<S: AuthStore>(store: Arc<S>) {
    const CONTENDERS: usize = 8;
    let code = seed_code(&*store).await;

    let barrier = Arc::new(tokio::sync::Barrier::new(CONTENDERS));
    let mut handles = Vec::new();
    for _ in 0..CONTENDERS {
        let store = store.clone();
        let id = code.id.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let consumed_at = rfc3339_from_now(0);
            barrier.wait().await;
            store.consume_auth_code(&id, &consumed_at).await
        }));
    }
    let mut successes = 0u32;
    for h in handles {
        if h.await.unwrap().unwrap() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1, "exactly one concurrent consume must succeed");
}
