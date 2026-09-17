use super::*;

// ---------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------

#[tokio::test]
async fn insert_and_find_token_by_hash_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let token = make_token("t1", "u1", TokenKind::ApiKey, "hash-1");
    store.insert_token(&token).await.unwrap();

    let found = store.find_token_by_hash("hash-1").await.unwrap().unwrap();
    assert_eq!(found, token);
    assert!(store.find_token_by_hash("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn revoke_token_sets_revoked_at_once() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let token = make_token("t1", "u1", TokenKind::ApiKey, "hash-1");
    store.insert_token(&token).await.unwrap();

    assert!(store.revoke_token("t1").await.unwrap());
    let found = store.find_token_by_hash("hash-1").await.unwrap().unwrap();
    assert!(found.revoked_at.is_some());

    // Revoking an already-revoked token reports no-op (false).
    assert!(!store.revoke_token("t1").await.unwrap());
}

#[tokio::test]
async fn revoke_token_missing_returns_false() {
    let (_dir, _backend, store) = make_store().await;
    assert!(!store.revoke_token("nope").await.unwrap());
}

#[tokio::test]
async fn revoke_token_family_revokes_all_members_only() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();

    let mut a = make_token("t1", "u1", TokenKind::Refresh, "hash-a");
    a.family_id = Some("fam-1".to_string());
    let mut b = make_token("t2", "u1", TokenKind::Refresh, "hash-b");
    b.family_id = Some("fam-1".to_string());
    let mut other = make_token("t3", "u1", TokenKind::Refresh, "hash-c");
    other.family_id = Some("fam-2".to_string());

    store.insert_token(&a).await.unwrap();
    store.insert_token(&b).await.unwrap();
    store.insert_token(&other).await.unwrap();

    let revoked_count = store.revoke_token_family("fam-1").await.unwrap();
    assert_eq!(revoked_count, 2);

    assert!(store
        .find_token_by_hash("hash-a")
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(store
        .find_token_by_hash("hash-b")
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(
        store
            .find_token_by_hash("hash-c")
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_none(),
        "a different family must not be touched"
    );
}

#[tokio::test]
async fn mark_token_used_sets_last_used_at() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let token = make_token("t1", "u1", TokenKind::ApiKey, "hash-1");
    store.insert_token(&token).await.unwrap();

    store
        .mark_token_used("t1", "2026-06-11T00:00:00Z")
        .await
        .unwrap();
    let found = store.find_token_by_hash("hash-1").await.unwrap().unwrap();
    assert_eq!(found.last_used_at.as_deref(), Some("2026-06-11T00:00:00Z"));
}

#[tokio::test]
async fn list_tokens_for_user_filters_correctly() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "bob", Role::Member))
        .await
        .unwrap();
    store
        .insert_token(&make_token("t1", "u1", TokenKind::ApiKey, "hash-1"))
        .await
        .unwrap();
    store
        .insert_token(&make_token("t2", "u1", TokenKind::Access, "hash-2"))
        .await
        .unwrap();
    store
        .insert_token(&make_token("t3", "u2", TokenKind::ApiKey, "hash-3"))
        .await
        .unwrap();

    let u1_tokens = store.list_tokens_for_user("u1").await.unwrap();
    assert_eq!(u1_tokens.len(), 2);
    let u2_tokens = store.list_tokens_for_user("u2").await.unwrap();
    assert_eq!(u2_tokens.len(), 1);
}
