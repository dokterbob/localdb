use super::*;

// ---------------------------------------------------------------------
// OAuth2 authorization codes (T4)
// ---------------------------------------------------------------------

fn make_auth_code(id: &str, user_id: &str, code_hash: &str) -> AuthCodeRow {
    AuthCodeRow {
        id: id.to_string(),
        client_id: "localdb-cli".to_string(),
        user_id: user_id.to_string(),
        code_hash: code_hash.to_string(),
        code_challenge: "challenge-value".to_string(),
        code_challenge_method: "S256".to_string(),
        redirect_uri: "http://127.0.0.1:1234/callback".to_string(),
        expires_at: "2026-06-10T12:10:00Z".to_string(),
        consumed_at: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn create_and_find_auth_code_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let code = make_auth_code("c1", "u1", "code-hash-1");
    store.create_auth_code(&code).await.unwrap();

    let found = store
        .find_auth_code_by_hash("code-hash-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found, code);
    assert!(store
        .find_auth_code_by_hash("nope")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn consume_auth_code_marks_consumed_once() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let code = make_auth_code("c1", "u1", "code-hash-1");
    store.create_auth_code(&code).await.unwrap();

    assert!(store
        .consume_auth_code("c1", "2026-06-10T12:05:00Z")
        .await
        .unwrap());
    let found = store
        .find_auth_code_by_hash("code-hash-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.consumed_at.as_deref(), Some("2026-06-10T12:05:00Z"));

    // Second consume is a no-op (already consumed) — the atomic "consume
    // iff unconsumed" guard `AuthService::redeem_auth_code` relies on.
    assert!(!store
        .consume_auth_code("c1", "2026-06-10T12:06:00Z")
        .await
        .unwrap());
}

#[tokio::test]
async fn consume_auth_code_missing_returns_false() {
    let (_dir, _backend, store) = make_store().await;
    assert!(!store
        .consume_auth_code("nope", "2026-06-10T12:00:00Z")
        .await
        .unwrap());
}

#[tokio::test]
async fn auth_code_cascades_on_user_delete() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let code = make_auth_code("c1", "u1", "code-hash-1");
    store.create_auth_code(&code).await.unwrap();

    store.delete_user("u1").await.unwrap();

    assert!(
        store
            .find_auth_code_by_hash("code-hash-1")
            .await
            .unwrap()
            .is_none(),
        "auth codes must cascade-delete when their user is removed"
    );
}
